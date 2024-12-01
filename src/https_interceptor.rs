use crate::context::Context;
use crate::tunnel_interceptor::TunnelInterceptor;
use crate::websocket_interceptor::Websocket;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioIo;
use log::{error, info, warn};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_rustls::TlsAcceptor;
type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
use hyper::service::service_fn;
use hyper_util::rt::TokioExecutor;
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::x509::extension::SubjectAlternativeName;
use openssl::x509::{X509Builder, X509NameBuilder};
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

                                    let is_websocket = Websocket::is_websocket_upgrade(&req);
                                    match if is_websocket {
                                        Websocket::handle_websocket_connection(req, &new_host).await
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
                TunnelInterceptor::handle_tunnel_proxy(request_id, req, addr, host).await?,
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
