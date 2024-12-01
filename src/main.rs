mod http_interceptor;
mod https_interceptor;
mod plugin;
mod store;
mod tunnel_interceptor;
mod websocket_interceptor;

use bytes::Bytes;
use clap::Parser;
use env_logger::{Builder, Env};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use http_interceptor::HttpInterceptor;
use https_interceptor::HttpsInterceptor;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use log::{error, info};

use crate::websocket_interceptor::Websocket;
use plugin::PluginManager;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
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
    #[arg(long = "h2", help = "启用 HTTP/2 支持", default_value_t = false)]
    enable_h2: bool,

    /// 最大网络记录数量
    #[arg(long = "max-records", default_value_t = 1000)]
    max_network_records: usize,
}

struct ProxyServer {
    https_interceptor: HttpsInterceptor,
}

impl ProxyServer {
    pub fn new() -> Self {
        Self {
            https_interceptor: HttpsInterceptor::new(),
        }
    }
    async fn handle_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        let error_response = Response::builder()
            .status(500)
            .body(Empty::new().map_err(|never| match never {}).boxed())
            .unwrap();
        match self.https_interceptor.handle_connect(req).await {
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

    // 修改后的 proxy_handler 函数
    async fn proxy_handler(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        match *req.method() {
            Method::CONNECT => self.handle_connect(req).await,
            _ => HttpInterceptor::handle_http(req).await,
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

    // 初始化全局 Store
    store::Store::init(
        args.port,
        cert_path.clone(),
        key_path.clone(),
        args.enable_https,
        args.enable_h2,
        Some(args.max_network_records),
    );

    // 如果启用 HTTPS 劫持，加载证书和密钥
    if args.enable_https {
        info!("启用HTTPS流量劫持");
        if args.enable_h2 {
            info!("启用 HTTP/2 支持");
        }

        let cert = fs::read(&cert_path).expect("无法读取根证书");
        let key = fs::read(&key_path).expect("无法读取密钥文件");

        let _cert = CertificateDer::from(cert);
        let _key = PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(key));
    }

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
