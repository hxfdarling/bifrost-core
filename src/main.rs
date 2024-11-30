mod context;
mod https_interceptor;
mod plugin;

use bytes::Bytes;
use clap::Parser;
use env_logger::{Builder, Env};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use https_interceptor::HttpsInterceptor;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use log::{error, info};

use plugin::PluginManager;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;

/// 命令行参数结构
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 监听端口
    #[arg(short = 'p', long = "port", default_value_t = 8080)]
    port: u16,

    /// 是否启用HTTPS流量劫持
    #[arg(long = "https", help = "启用HTTPS流量劫持", default_value_t = true)]
    enable_https: bool,

    /// 是否启用 HTTP/2 支持
    #[arg(long = "h2", help = "启用 HTTP/2 支持")]
    enable_h2: bool,

    /// 最大网络记录数量
    #[arg(long = "max-records", default_value_t = 1000)]
    max_network_records: usize,
}

// 添加请求ID计数器
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

struct ProxyServer {
    https_interceptor: HttpsInterceptor,
}

impl ProxyServer {
    pub fn new() -> Self {
        Self {
            https_interceptor: HttpsInterceptor::new(),
        }
    }

    // 重构后的 handle_connect 函数
    async fn handle_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let error_response = Response::builder()
            .status(500)
            .body(Empty::new().map_err(|never| match never {}).boxed())
            .unwrap();
        // 添加错误日志
        match self.https_interceptor.handle_connect(request_id, req).await {
            Ok(response_option) => Ok(response_option.unwrap_or_else(|| {
                error!("CONNECT请求处理失败: handle_connect 返回 None");
                error_response
            })),
            Err(e) => {
                error!("CONNECT请求处理失败: {}", e);
                Ok(error_response)
            }
        }
    }

    // 新增 handle_http 函数
    async fn handle_http(
        &self,
        mut req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);

        // 检查是否为 WebSocket 升级请求
        let is_ws_upgrade = req
            .headers()
            .get(hyper::header::UPGRADE)
            .and_then(|h| h.to_str().ok())
            .map(|h| h.to_lowercase().contains("websocket"))
            .unwrap_or(false);

        // 处理请求，如果插件返回 false，表示不继续处理
        match PluginManager::global()
            .handle_request(request_id, &mut req)
            .await
        {
            Ok((false, Some(response))) => {
                return Ok(response);
            }
            Ok((false, None)) => {
                let body = Full::from(Bytes::from("Bad Plugin Response"))
                    .map_err(|never| match never {})
                    .boxed();
                return Ok(Response::builder().status(400).body(body).unwrap());
            }
            Ok((true, _)) => (), // 继续处理
            Err(e) => {
                error!("插件处理请求失败: {}", e);
                let body = Full::from(Bytes::from("Internal Server Error"))
                    .map_err(|never| match never {})
                    .boxed();
                return Ok(Response::builder().status(500).body(body).unwrap());
            }
        }

        let https = HttpsConnector::new();
        let client = Client::builder(TokioExecutor::new())
            .http1_preserve_header_case(true)
            .http1_title_case_headers(true)
            .build::<_, Incoming>(https);

        // 保存请求的必要信息
        let req_method = req.method().clone();
        let req_uri = req.uri().clone();
        let req_headers = req.headers().clone();
        let mut req_clone = Request::builder()
            .uri(req_uri)
            .method(req_method)
            .body(())
            .unwrap();
        for (key, value) in req_headers.iter() {
            req_clone.headers_mut().insert(key, value.clone());
        }

        match client.request(req).await {
            Ok(mut response) => {
                // 如果是 WebSocket 升级响应，直接返回
                if is_ws_upgrade && response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS {
                    return Ok(response.map(|b| b.boxed()));
                }

                if let Err(e) = PluginManager::global()
                    .handle_response(request_id, &req_clone, &mut response)
                    .await
                {
                    error!("插件处理响应失败: {}", e);
                    let body = Full::from(format!("Plugin response error: {}", e))
                        .map_err(|never| match never {})
                        .boxed();
                    return Ok(Response::builder().status(500).body(body).unwrap());
                }
                Ok(response.map(|b| b.boxed()))
            }
            Err(e) => {
                error!("请求目标服务器失败: {}", e);
                let body = Full::from(format!("Request failed: {}", e))
                    .map_err(|never| match never {})
                    .boxed();
                Ok(Response::builder().status(502).body(body).unwrap())
            }
        }
    }

    // 修改后的 proxy_handler 函数
    async fn proxy_handler(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        match *req.method() {
            Method::CONNECT => self.handle_connect(req).await,
            _ => self.handle_http(req).await,
        }
    }
}

#[tokio::main]
async fn main() {
    // 初始化 env_logger
    Builder::from_env(Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let args = Args::parse();

    let cert_path = PathBuf::from("files/root.crt");
    let key_path = PathBuf::from("files/root.key");

    // 检查证书文件
    if !cert_path.exists() || !key_path.exists() {
        error!(
            "根证书或密钥文件不存在，请检查路径: {:?}, {:?}",
            cert_path, key_path
        );
        std::process::exit(1);
    }

    // 初始化全局 Context
    context::Context::init(
        args.port,
        cert_path.clone(),
        key_path.clone(),
        args.enable_https,
        args.enable_h2,
        Some(args.max_network_records),
    );

    // 如果启用 HTTPS 劫持，加载证书和密钥
    let (root_cert, root_key) = if args.enable_https {
        info!("启用HTTPS流量劫持");
        if args.enable_h2 {
            info!("启用 HTTP/2 支持");
        }

        let cert = fs::read(&cert_path).expect("无法读取根证书");
        let key = fs::read(&key_path).expect("无法读取密钥文件");

        let root_cert = CertificateDer::from(cert);
        let root_key = PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(key));

        (Some(root_cert), Some(root_key))
    } else {
        (None, None)
    };

    PluginManager::init();

    // 启动统计信息打印任务
    // TrafficStatsPlugin::start_stats_printer();
    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let proxy_server = Arc::new(ProxyServer::new());

    let listener = TcpListener::bind(addr).await.unwrap();
    info!("代理服务器运行在 http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let proxy_server = proxy_server.clone();

        let service = service_fn(move |req| {
            let proxy_server = proxy_server.clone();
            async move { proxy_server.proxy_handler(req).await }
        });

        tokio::task::spawn(async move {
            if let Err(err) = hyper::server::conn::http1::Builder::new()
                .preserve_header_case(true)
                .title_case_headers(true)
                .serve_connection(io, service)
                .with_upgrades()
                .await
            {
                error!("服务器连接错误: {:?}, 错误类型: {}", err, err.to_string());
            }
        });
    }
}
