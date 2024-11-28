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
use log::{error, info, warn};
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
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

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

        // 设置证书主体信息
        let mut name_builder =
            X509NameBuilder::new().map_err(|e| format!("创建名称构建器失败: {}", e))?;
        name_builder
            .append_entry_by_text("CN", domain)
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
    // 隧道代理处理
    async fn handle_tunnel(
        request_id: u64,
        upgraded: hyper::upgrade::Upgraded,
        req: Request<()>,
        mut target_stream: TcpStream,
        host: String,
    ) {
        let mut client_stream = TokioIo::new(upgraded);
        let mut client_buf = [0u8; 8192];
        let mut server_buf = [0u8; 8192];

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
                                    println!("客户端正常关闭连接 [RequestID: {}, Host: {}]", request_id, host);
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
                                    println!("服务器正常关闭连接 [RequestID: {}]", request_id);
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
        println!("HTTPS 隧道关闭 [RequestID: {}, Host: {}]", request_id, host);
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
                            println!("正在建立到 {} 的 HTTPS 隧道 [Host: {}]", addr, host);
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

        // 在 upgrade 之前取所需的信息
        let host = auth_str.split(':').next().unwrap_or(&auth_str).to_string();
        let enable_https = config.enable_https;

        // 从 context 获取 HTTP/2 配置
        let config = Context::global().get_config().await;
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
        // HTTPS 拦截模式
        if let Some(acceptor) = acceptor {
            // 连接到目标服务器
            if let Ok(target_stream) = TcpStream::connect(&addr).await {
                tokio::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            println!("开始TLS握手 [Host: {}]", host);
                            // 接受 TLS 连接
                            match acceptor.accept(TokioIo::new(upgraded)).await {
                                Ok(tls_stream) => {
                                    println!("TLS 握手成功 [Host: {}]", host);
                                    let io = TokioIo::new(tls_stream);
                                    let service = service_fn(move |req| {
                                        HttpsInterceptor::handle_request(req)
                                    });

                                    if enable_h2 {
                                        println!("使用 HTTP/2 服务器处理连接,{}", host);
                                        // 使用 HTTP/2 服务器处理连接
                                        if let Ok(_) = http2::Builder::new(TokioExecutor::new())
                                            .serve_connection(io, service)
                                            .await
                                        {
                                            println!(
                                                "HTTP/2 连接已关闭 [RequestID: {}, Host: {}]",
                                                request_id, host
                                            );
                                        } else {
                                            error!("HTTP/2 连接失败,{}", host);
                                        }
                                    } else {
                                        // 使用 HTTP/1.1 服务器处理连接
                                        if let Ok(_) = hyper::server::conn::http1::Builder::new()
                                            .serve_connection(io, service)
                                            .await
                                        {
                                            println!(
                                                "HTTP/1.1 连接已关闭 [RequestID: {}, Host: {}]",
                                                request_id, host
                                            );
                                        } else {
                                            error!("HTTP/1.1 连接失败,{}", host);
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("TLS 握手失败: {}", e);
                                }
                            }
                        }
                        Err(e) => {
                            error!("连接升级失败: {}", e);
                        }
                    }
                });
                // 首先返回200 Connection Established
                Ok(Some(
                    Response::builder()
                        .status(200)
                        .header("Connection", "keep-alive")
                        .header("Proxy-Connection", "keep-alive")
                        .body(Empty::new().map_err(|never| match never {}).boxed())
                        .unwrap(),
                ))
            } else {
                error!("无法连接到目标服务器 [Host: {}]", host);
                Ok(Some(Self::build_502_error(&format!(
                    "无法连接到目标服务器: {}",
                    host
                ))))
            }
        } else {
            // 直接转发模式
            Ok(Some(
                Self::handle_tunnel_proxy(request_id, req, addr, host).await?,
            ))
        }
    }
    async fn handle_request(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        // 检查是否为 WebSocket 升级请求
        if is_websocket_upgrade(&req) {
            println!("WebSocket 升级请求 {}", req.uri());
            return handle_websocket_upgrade(req).await;
        }

        let https = HttpsConnector::new();
        let client = Client::builder(TokioExecutor::new())
            .http1_title_case_headers(true)
            .http1_preserve_header_case(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_only(false)
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
        // .version(req.version());

        // 复制所有请求头
        for (name, value) in req.headers() {
            builder = builder.header(name, value);
        }

        let new_req = builder
            .body(req.into_body())
            .map_err(|e| format!("构建请求失败: {}", e))?;

        match client.request(new_req).await {
            Ok(response) => {
                println!("与远程服务器使用的协议版本: {:?}", response.version());
                Ok(response.map(|b| b.boxed()))
            }
            Err(e) => {
                println!("请求转发失败: {}", e);
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

// 新增辅助函数
fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
    req.headers()
        .get(hyper::header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase().contains("websocket"))
        .unwrap_or(false)
        && req
            .headers()
            .get(hyper::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_lowercase().contains("upgrade"))
            .unwrap_or(false)
}

async fn handle_websocket_upgrade(
    req: Request<Incoming>,
) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
    let https = HttpsConnector::new();
    let client = Client::builder(TokioExecutor::new())
        .http1_title_case_headers(true)
        .http1_preserve_header_case(true)
        .build::<_, Incoming>(https);

    // 构建目标 WebSocket URL
    let uri_string = if req.uri().scheme().is_none() {
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("localhost");
        format!(
            "wss://{}{}",
            host,
            req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
        )
    } else {
        // 将 https 替换为 wss
        req.uri().to_string().replace("https://", "wss://")
    };

    println!("WebSocket 升级请求: {}", uri_string);

    // 构建新的 WebSocket 升级请求
    let mut builder = Request::builder()
        .method(req.method())
        .uri(uri_string)
        .version(req.version());

    // 复制所有请求头
    for (name, value) in req.headers() {
        builder = builder.header(name, value);
    }

    let new_req = builder
        .body(req.into_body())
        .map_err(|e| format!("构建 WebSocket 请求失败: {}", e))?;

    // 发送请求并处理响应
    match client.request(new_req).await {
        Ok(response) => {
            if response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
                // 构建升级响应，确保包含所有必要的升级头
                let mut builder = Response::builder()
                    .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
                    .header(hyper::header::CONNECTION, "upgrade")
                    .header(hyper::header::UPGRADE, "websocket");

                // 复制所有 WebSocket 相关的响应头
                for (name, value) in response.headers() {
                    if name == hyper::header::SEC_WEBSOCKET_ACCEPT
                        || name == hyper::header::SEC_WEBSOCKET_PROTOCOL
                        || name == hyper::header::SEC_WEBSOCKET_EXTENSIONS
                        || name == hyper::header::SEC_WEBSOCKET_VERSION
                    {
                        builder = builder.header(name, value);
                    }
                }

                println!("WebSocket 升级成功，返回升级响应");

                Ok(builder
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap())
            } else {
                // 如果不是升级响应，则直接转发原始响应
                println!("WebSocket 升级失败，状态码: {}", response.status());
                Ok(response.map(|b| b.boxed()))
            }
        }
        Err(e) => {
            error!("WebSocket 请求转发失败: {}", e);
            Ok(Response::builder()
                .status(502)
                .body(
                    Full::new(Bytes::from(format!("WebSocket Gateway Error: {}", e)))
                        .map_err(|never| match never {})
                        .boxed(),
                )
                .unwrap())
        }
    }
}
