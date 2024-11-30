use crate::context::Context;
use crate::plugin::{DataDirection, Plugin, PluginManager};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::Version;
use hyper::{Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioIo;
use log::{debug, error, info, warn};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::RwLock;
use tokio::time::sleep;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::{BasicConstraints, SubjectAlternativeName};
use openssl::x509::{X509Builder, X509NameBuilder};
use rustls::internal;
use rustls::ClientConfig;
use rustls::RootCertStore;
use rustls_pemfile;
use sha1::{Digest, Sha1};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{connect_async, tungstenite::Message, WebSocketStream};
#[derive(Clone)]
struct CachedCert {
    cert: Arc<CertificateDer<'static>>,
    key: Arc<PrivateKeyDer<'static>>,
    expiry: SystemTime,
}

pub struct HttpsInterceptor {
    cert_cache: Arc<RwLock<HashMap<String, CachedCert>>>,
    cache_duration: Duration,
}

impl HttpsInterceptor {
    pub fn new() -> Self {
        Self {
            cert_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(3600),
        }
    }
    // 生成证书
    async fn generate_cert(&self, domain: &str) -> Result<CachedCert> {
        let config = Context::global().get_config().await;
        let cert_path = &config.cert_path;
        let key_path = &config.key_path;

        // 读取根证书和私钥
        let root_cert_data = tokio::fs::read(cert_path)
            .await
            .map_err(|e| format!("无法读取证书文件 {}: {}", cert_path.display(), e))?;
        let root_key_data = tokio::fs::read(key_path)
            .await
            .map_err(|e| format!("无法读取私钥文件 {}: {}", key_path.display(), e))?;

        // 解析根证书和私钥
        let root_cert = openssl::x509::X509::from_pem(&root_cert_data)
            .map_err(|e| format!("无法解析根证书: {}", e))?;
        let root_key = PKey::private_key_from_pem(&root_key_data)
            .map_err(|e| format!("无法解析根私钥: {}", e))?;

        // 创建新的证书
        let mut builder = X509Builder::new().map_err(|e| format!("创建证书构建器失败: {}", e))?;

        // 设置证书版本
        builder
            .set_version(2)
            .map_err(|e| format!("设置证书版本失败: {}", e))?;

        // 设置序列号
        let serial = openssl::bn::BigNum::from_u32(rand::random::<u32>())
            .and_then(|bn| bn.to_asn1_integer())
            .map_err(|e| format!("生成序列号失败: {}", e))?;
        builder
            .set_serial_number(&serial)
            .map_err(|e| format!("设置序列号失败: {}", e))?;

        // 设置证书有效期
        let not_before =
            Asn1Time::days_from_now(0).map_err(|e| format!("设置起始时间失败: {}", e))?;
        let not_after =
            Asn1Time::days_from_now(365).map_err(|e| format!("设置过期时间失败: {}", e))?;
        builder
            .set_not_before(&not_before)
            .map_err(|e| format!("设置起始时间失败: {}", e))?;
        builder
            .set_not_after(&not_after)
            .map_err(|e| format!("设置过期时间失败: {}", e))?;

        // 设置证书主体信息时处理长域名
        let mut name_builder =
            X509NameBuilder::new().map_err(|e| format!("创建名称构建器失败: {}", e))?;

        // 如果域名超过64个字符，则截取前64个字符
        let cn_value = if domain.len() > 64 {
            &domain[..64]
        } else {
            domain
        };

        name_builder
            .append_entry_by_text("CN", cn_value)
            .map_err(|e| format!("设置通用名称失败: {}", e))?;

        let name = name_builder.build();
        builder
            .set_subject_name(&name)
            .map_err(|e| format!("设置主体名称失败: {}", e))?;
        builder
            .set_issuer_name(root_cert.subject_name())
            .map_err(|e| format!("设置颁发者名称失败: {}", e))?;

        // 添加 SAN 扩展
        let san = SubjectAlternativeName::new()
            .dns(domain)
            .build(&builder.x509v3_context(Some(&root_cert), None))
            .map_err(|e| format!("创建SAN扩展失败: {}", e))?;
        builder
            .append_extension(san)
            .map_err(|e| format!("添加SAN扩展失败: {}", e))?;

        // 创建新的密钥对
        let pkey = PKey::from_rsa(
            openssl::rsa::Rsa::generate(2048).map_err(|e| format!("生成RSA密钥对失败: {}", e))?,
        )
        .map_err(|e| format!("创建密钥对失败: {}", e))?;

        // 设置公钥
        builder
            .set_pubkey(&pkey)
            .map_err(|e| format!("设置公钥失败: {}", e))?;

        // 使用根私钥签名证书
        builder
            .sign(&root_key, MessageDigest::sha256())
            .map_err(|e| format!("签名证书失败: {}", e))?;

        // 获取生成的证书
        let cert = builder.build();

        // 转换为 DER 格式
        let cert_der = cert
            .to_der()
            .map_err(|e| format!("转换证书为DER格式失败: {}", e))?;
        let key_der = pkey
            .private_key_to_pkcs8()
            .map_err(|e| format!("转换私钥为PKCS8格式失败: {}", e))?;

        Ok(CachedCert {
            cert: Arc::new(CertificateDer::from(cert_der)),
            key: Arc::new(PrivateKeyDer::from(
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_der),
            )),
            expiry: SystemTime::now() + self.cache_duration,
        })
    }
    // 获取或生成证书,如果缓存中没有则生成
    async fn get_or_generate_cert(&self, domain: &str) -> Result<CachedCert> {
        let cache = self.cert_cache.read().await;
        if let Some(cached) = cache.get(domain) {
            if cached.expiry > SystemTime::now() {
                return Ok(cached.clone());
            }
        }
        drop(cache);

        let cert = self.generate_cert(domain).await?;
        let mut cache = self.cert_cache.write().await;
        cache.insert(domain.to_string(), cert.clone());
        Ok(cert)
    }
    // 构建一个502错误，支持传入错误信息
    fn build_502_error(error_msg: &str) -> Response<BoxBody<Bytes, hyper::Error>> {
        Response::builder()
            .status(502)
            .body(
                Full::from(Bytes::from(error_msg.to_string()))
                    .map_err(|never| match never {})
                    .boxed(),
            )
            .unwrap()
    }
    // 隧道代理处
    async fn handle_tunnel(
        request_id: u64,
        upgraded: hyper::upgrade::Upgraded,
        req: Request<()>,
        mut target_stream: TcpStream,
        host: String,
    ) {
        let mut client_stream = TokioIo::new(upgraded);
        let mut client_buf = vec![0u8; 32 * 1024]; // 32KB buffer
        let mut server_buf = vec![0u8; 32 * 1024]; // 32KB buffer

        // 增加超时时间到5分钟
        const TIMEOUT_DURATION: Duration = Duration::from_secs(300);
        let mut timeout = Box::pin(sleep(TIMEOUT_DURATION));
        let plugin_manager = PluginManager::global();
        // 添加错误计数器
        let mut error_count = 0;
        const MAX_ERRORS: u32 = 3;
        match plugin_manager.handle_connect(request_id, &req).await {
            Ok((true, _response)) => {
                loop {
                    select! {
                        result = client_stream.read(&mut client_buf) => {
                            match result {
                                Ok(0) => {
                                    info!("客户端正常关闭连接 [RequestID: {}, Host: {}]", request_id, host);
                                    break;
                                }
                                Ok(n) => {
                                    error_count = 0; // 重置错误计数
                                    timeout.as_mut().reset(tokio::time::Instant::now() + TIMEOUT_DURATION);

                                    if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Upstream, &client_buf[..n]).await {
                                        error!("统计上行流量失败: {}", e);
                                    }
                                    if let Err(e) = target_stream.write_all(&client_buf[..n]).await {
                                        error!("写入目标服务器失败: {}", e);
                                        error_count += 1;
                                        if error_count >= MAX_ERRORS {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("从客户端读取失败: {}", e);
                                    error_count += 1;
                                    if error_count >= MAX_ERRORS {
                                        break;
                                    }
                                }
                            }
                        }
                        result = target_stream.read(&mut server_buf) => {
                            match result {
                                Ok(0) => {
                                    info!("服务正常关闭连接 [RequestID: {}]", request_id);
                                    break;
                                }
                                Ok(n) => {
                                    error_count = 0; // 重置错误计数
                                    timeout.as_mut().reset(tokio::time::Instant::now() + TIMEOUT_DURATION);

                                    if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Downstream, &server_buf[..n]).await {
                                        warn!("统计下行流量失败: {}", e);
                                    }
                                    if let Err(e) = client_stream.write_all(&server_buf[..n]).await {
                                        error!("写入客户端失败: {}", e);
                                        error_count += 1;
                                        if error_count >= MAX_ERRORS {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("从服务器读取失败: {}", e);
                                    error_count += 1;
                                    if error_count >= MAX_ERRORS {
                                        break;
                                    }
                                }
                            }
                        }
                        _ = &mut timeout => {
                            error!("连接超时（{}秒无数据传输），闭隧道 [RequestID: {}, Host: {}]",
                                TIMEOUT_DURATION.as_secs(), request_id, host);
                            break;
                        }
                    }
                }
            }
            Ok((false, response)) => {
                // 直接响应请求
                if let Some(response) = response {
                    let response_bytes = format!("HTTP/1.1 {}\r\n\r\n", response.status());
                    client_stream
                        .write_all(response_bytes.as_bytes())
                        .await
                        .unwrap();
                }
            }
            Err(e) => {
                error!("处理连接失败: {}", e);
            }
        };

        // 在隧道关闭时调用插件的 on_connect_close
        if let Err(e) = plugin_manager.handle_connect_close(request_id, &host).await {
            error!(
                "处理连接关闭失败 [RequestID: {}, Host: {}]: {}",
                request_id, host, e
            );
        }
        info!("HTTPS 隧道关闭 [RequestID: {}, Host: {}]", request_id, host);
    }
    // 隧道代理预处理
    async fn handle_tunnel_proxy(
        request_id: u64,
        req: Request<Incoming>,
        addr: String,
        host: String,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        match TcpStream::connect(&addr).await {
            Ok(target_stream) => {
                tokio::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            info!("正在建立到 {} 的 HTTPS 隧道 [Host: {}]", addr, host);
                            let cloned_req = Request::builder()
                                .method(hyper::Method::CONNECT)
                                .uri(format!("https://{}", host))
                                .body(())
                                .unwrap();
                            HttpsInterceptor::handle_tunnel(
                                request_id,
                                upgraded,
                                cloned_req,
                                target_stream,
                                host,
                            )
                            .await;
                        }
                        Err(e) => error!("连接升级失败: {}", e),
                    }
                });

                Ok(Response::builder()
                    .status(200)
                    .header("Connection", "keep-alive")
                    .header("Proxy-Connection", "keep-alive")
                    .header("Proxy-Agent", "bifrost")
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap())
            }
            Err(e) => {
                error!("连接目标服务器失败: {}", e);
                Ok(Self::build_502_error(&format!("连接目标服务器失败: {}", e)))
            }
        }
    }
    // 判断是否为WebSocket升级请求
    fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
        let is_upgrade = req
            .headers()
            .get(hyper::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_lowercase().contains("websocket"))
            .unwrap_or(false)
            && req
                .headers()
                .get(hyper::header::CONNECTION)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_lowercase().contains("upgrade"))
                .unwrap_or(false);

        is_upgrade
    }

    // WebSocket升处理
    async fn handle_websocket_connection(
        req: Request<Incoming>,
        host: &String,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        info!("处理 WebSocket 升级请求 [Host: {}]", host);

        // 构建目标 URI
        let target_uri = if req.uri().scheme().is_none() {
            format!(
                "wss://{}{}",
                req.headers()
                    .get(hyper::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or(host),
                req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
            )
        } else {
            req.uri().to_string().replace("http", "ws")
        };

        info!("WebSocket 目标 URI: {}", target_uri);

        // 构建新的请求,保留原始请求的所有头部
        let mut request_builder = Request::builder().uri(target_uri.clone()).method("GET");

        // 复制所有原始请求头
        for (name, value) in req.headers() {
            request_builder = request_builder.header(name, value);
        }
        // 连接到目标服务器
        let request = request_builder
            .body(())
            .map_err(|e| format!("构建请求失败: {}", e))?;

        // 使用构建好的请求发起连接
        match connect_async(request).await {
            Ok((_, response)) => {
                info!("获取目标服务器响应头成功 [Host: {}]", host);

                // 构建升级响应
                let mut response_builder = Response::builder().status(response.status());
                // 复制所有响应头
                for (name, value) in response.headers() {
                    response_builder = response_builder.header(name, value);
                }

                let host = host.to_string();
                tokio::spawn(async move {
                    handle_websocket_upgrade(req, target_uri, host).await;
                });
                Ok(response_builder
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap())
            }
            Err(e) => {
                error!("连接 WebSocket 服务器失败: {}", e);
                Ok(Self::build_502_error(&format!(
                    "连接 WebSocket 服务器失败: {}",
                    e
                )))
            }
        }
    }

    pub async fn handle_connect(
        &self,
        request_id: u64,
        req: Request<Incoming>,
    ) -> Result<Option<Response<BoxBody<Bytes, hyper::Error>>>> {
        let config = Context::global().get_config().await;
        // 先获取所有需要的信息
        let auth_str = req
            .uri()
            .authority()
            .ok_or("Invalid CONNECT request")?
            .to_string();

        let addr = if !auth_str.contains(':') {
            format!("{}:443", auth_str)
        } else {
            auth_str.clone()
        };

        // 获取目标服务器的host
        let host = auth_str.split(':').next().unwrap_or(&auth_str).to_string();
        // 是否启用HTTPS 拦截
        let enable_https = config.enable_https;
        // 是否启用HTTP/2
        let enable_h2 = config.enable_h2;

        let acceptor = if enable_https {
            let cert = self.get_or_generate_cert(&host).await?;
            let mut server_config = ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.cert.as_ref().clone()], (*cert.key).clone_key())?;

            // 设置支持的协议版本
            if enable_h2 {
                server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            } else {
                server_config.alpn_protocols = vec![b"http/1.1".to_vec()];
            }

            let acceptor = TlsAcceptor::from(Arc::new(server_config));
            Some(acceptor)
        } else {
            None
        };
        // HTTPS 拦截模
        if let Some(acceptor) = acceptor {
            // 先检查目标服务器是否可达
            if let Err(e) = TcpStream::connect(&addr).await {
                error!("目标服务器不可达 [Host: {}]: {}", auth_str, e);
                return Ok(Some(Self::build_502_error(&format!(
                    "无法连接到目标服务器: {}",
                    e
                ))));
            }
            tokio::spawn(async move {
                match hyper::upgrade::on(req).await {
                    Ok(upgraded) => {
                        info!("开始TLS握手 [Host: {}]", host);
                        // 1. 先进行TLS握手
                        match acceptor.accept(TokioIo::new(upgraded)).await {
                            Ok(tls_stream) => {
                                info!("TLS握手成功 [Host: {}]", host);
                                let io = TokioIo::new(tls_stream);
                                let service = service_fn(|req| async move {
                                    // 从请求头中获取 host
                                    let new_host = req
                                        .headers()
                                        .get(hyper::header::HOST)
                                        .and_then(|h| h.to_str().ok())
                                        .unwrap_or_default()
                                        .to_string();

                                    let is_websocket = Self::is_websocket_upgrade(&req);
                                    match if is_websocket {
                                        Self::handle_websocket_connection(req, &new_host).await
                                    } else {
                                        HttpsInterceptor::handle_http_request(req, &new_host).await
                                    } {
                                        Ok(response) => Ok::<_, hyper::Error>(response),
                                        Err(e) => Ok(Self::build_502_error(&format!(
                                            "{} Bad Gateway: {}",
                                            new_host,
                                            e.to_string()
                                        ))),
                                    }
                                });

                                if let Err(e) = hyper::server::conn::http1::Builder::new()
                                    .preserve_header_case(true)
                                    .title_case_headers(true)
                                    .serve_connection(io, service)
                                    .with_upgrades()
                                    .await
                                {
                                    error!("HTTP连接处理失败 [Host: {}]: {:?}", host, e);
                                }
                            }
                            Err(e) => error!("TLS握手失败 [Host: {}]: {:?}", host, e),
                        }
                    }
                    Err(e) => error!("连接升级失败 [Host: {}]: {:?}", host, e),
                }
            });
            // 返回200, 表示连接成功, 并保持连接, 允许客户端继续发送数据
            Ok(Some(
                Response::builder()
                    .status(200)
                    .header("Connection", "keep-alive")
                    .header("Proxy-Connection", "keep-alive")
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap(),
            ))
        } else {
            // 直接转发模式，不进行TLS握手
            Ok(Some(
                Self::handle_tunnel_proxy(request_id, req, addr, host).await?,
            ))
        }
    }
    async fn handle_http_request(
        req: Request<Incoming>,
        host: &String,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        let https = HttpsConnector::new();
        let client = Client::builder(TokioExecutor::new())
            .http1_title_case_headers(true)
            .http1_preserve_header_case(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(20))
            .http2_adaptive_window(true)
            .set_host(true)
            .build::<_, Incoming>(https);

        // 构建新的URI，确保使用绝对路径
        let uri_string = if req.uri().scheme().is_none() {
            // 从Host头获取主机名
            let host = req
                .headers()
                .get(hyper::header::HOST)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("localhost");

            // 构建完整的URL
            format!(
                "https://{}{}",
                host,
                req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
            )
        } else {
            req.uri().to_string()
        };

        // 构建新请求
        let mut builder = Request::builder().method(req.method()).uri(uri_string);

        // 复制所有请求头
        for (name, value) in req.headers() {
            builder = builder.header(name, value);
        }

        let new_req = builder
            .body(req.into_body())
            .map_err(|e| format!("构建请求失败: {}", e))?;

        match client.request(new_req).await {
            Ok(response) => {
                info!("请求转发成功 [Host: {}]", host);
                Ok(response.map(|b| b.boxed()))
            }
            Err(e) => {
                error!("请求转发失败 [Host: {}] ,错误: {}", host, e);
                Ok(Response::builder()
                    .status(502)
                    .body(
                        Full::new(Bytes::from(format!("Bad Gateway: {}", e)))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap())
            }
        }
    }
}

// 处理 WebSocket 连接升级和消息转发
async fn handle_websocket_upgrade(req: Request<Incoming>, target_uri: String, host: String) {
    match hyper::upgrade::on(req).await {
        Ok(upgraded) => {
            let ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();

            // 创建客户端 WebSocket 流
            let mut ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                Some(ws_config),
            )
            .await;

            // 设置连接超时
            let connect_timeout =
                tokio::time::timeout(Duration::from_secs(60), connect_async(&target_uri));

            match connect_timeout.await {
                Ok(connect_result) => {
                    match connect_result {
                        Ok((target_ws_stream, _)) => {
                            let (mut client_write, mut client_read) = ws_stream.split();
                            let (mut target_write, mut target_read) = target_ws_stream.split();

                            info!("WebSocket 双向流建立成功 [Host: {}]", host);

                            // 创建两个转发任务
                            let client_to_server = async {
                                while let Some(msg) = client_read.next().await {
                                    match msg {
                                        Ok(msg) => {
                                            if let Err(e) = target_write.send(msg).await {
                                                error!(
                                                    "发送消息到服务器失败 [Host: {}]: {}",
                                                    host, e
                                                );
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            error!("从客户端读取消息失败 [Host: {}]: {}", host, e);
                                            break;
                                        }
                                    }
                                }
                            };

                            let server_to_client = async {
                                while let Some(msg) = target_read.next().await {
                                    match msg {
                                        Ok(msg) => {
                                            if let Err(e) = client_write.send(msg).await {
                                                error!(
                                                    "发送消息到客户端失败 [Host: {}]: {}",
                                                    host, e
                                                );
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            error!("从服务器读取消息失败 [Host: {}]: {}", host, e);
                                            break;
                                        }
                                    }
                                }
                            };

                            // 同时运行两个转发任务
                            tokio::select! {
                                _ = client_to_server => {},
                                _ = server_to_client => {},
                            }
                        }
                        Err(e) => {
                            error!("连接目标 WebSocket 服务器失败 [Host: {}]: {}", host, e);
                            // 发送关闭帧并等待关闭完成
                            if let Err(e) = ws_stream
                                .close(Some(CloseFrame {
                                    code: CloseCode::Error,
                                    reason: "连接目标服务器失败".into(),
                                }))
                                .await
                            {
                                error!("关闭 WebSocket 连接失败 [Host: {}]: {}", host, e);
                            }
                        }
                    }
                }
                Err(_) => {
                    error!("连接目标 WebSocket 服务器超时 [Host: {}]", host);
                    // 发送关闭帧并等待关闭完成
                    if let Err(e) = ws_stream
                        .close(Some(CloseFrame {
                            code: CloseCode::Normal,
                            reason: "连接目标服务器超时".into(),
                        }))
                        .await
                    {
                        error!("关闭 WebSocket 连接失败 [Host: {}]: {}", host, e);
                    }
                }
            }
        }
        Err(e) => error!("WebSocket 升级失败 [Host: {}]: {}", host, e),
    }
}
