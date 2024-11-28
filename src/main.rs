mod context;
mod error;
mod plugin;

use bytes::Bytes;
use clap::Parser;
use env_logger;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Body, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use hyper_util::rt::TokioIo;
use plugin::bifrost_server::BifrostServerPlugin;
use plugin::https_interceptor::HttpsInterceptorPlugin;
use plugin::net_storage::NetStorage;
use plugin::traffic_stats::TrafficStatsPlugin;
use plugin::{DataDirection, PluginManager};
use rcgen::{Certificate, CertificateParams, DistinguishedName};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::convert::Infallible;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::select;
use tokio::time::{sleep, Duration};

/// 命令行参数结构
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// 监听端口
    #[arg(short = 'p', long = "port", default_value_t = 8080)]
    port: u16,

    /// 是否启用HTTPS流量劫持
    #[arg(long = "https")]
    enable_https: bool,

    /// 是否启用 HTTP/2 支持
    #[arg(long = "h2", help = "启用 HTTP/2 支持")]
    enable_h2: bool,

    /// 是否仅使用 HTTP/2
    #[arg(long = "h2-only", help = "仅使用 HTTP/2 协议")]
    h2_only: bool,

    /// 最大网络记录数量
    #[arg(long = "max-records", default_value_t = 1000)]
    max_network_records: usize,
}

// 添加请求ID计数器
static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

struct ProxyServer {
    plugin_manager: Arc<PluginManager>,
}

impl ProxyServer {
    pub fn new() -> Self {
        let mut plugin_manager = PluginManager::new();
        // 注册Bifrost Server插件
        plugin_manager.register_plugin(Arc::new(BifrostServerPlugin::new()));

        // 注册流量统计插件
        let traffic_stats_plugin = Arc::new(TrafficStatsPlugin::new());
        plugin_manager.register_plugin(traffic_stats_plugin);

        // 注册 HTTPS 拦截器插件
        plugin_manager.register_plugin(Arc::new(HttpsInterceptorPlugin::new(Some(
            Duration::from_secs(3600),
        ))));

        // 注册 NetStorage 插件
        plugin_manager.register_plugin(Arc::new(NetStorage::new()));

        // 启动统计信息打印任务
        TrafficStatsPlugin::start_stats_printer();

        Self {
            plugin_manager: Arc::new(plugin_manager),
        }
    }

    // 新增隧道代理处理函数
    async fn handle_tunnel(
        plugin_manager: &Arc<PluginManager>,
        request_id: u64,
        upgraded: hyper::upgrade::Upgraded,
        mut target_stream: TcpStream,
        host: String,
    ) {
        println!("隧道连接已建立 [RequestID: {}, Host: {}]", request_id, host);
        let mut client_stream = TokioIo::new(upgraded);
        let mut client_buf = [0u8; 8192];
        let mut server_buf = [0u8; 8192];

        loop {
            select! {
                result = client_stream.read(&mut client_buf) => {
                    match result {
                        Ok(0) => {
                            println!("客户端关闭连接 [RequestID: {}, Host: {}]", request_id, host);
                            break;
                        }
                        Ok(n) => {
                            // 统计上行流量
                            if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Upstream, &client_buf[..n]).await {
                                println!("统计上行流量失败: {}", e);
                            }
                            if let Err(e) = target_stream.write_all(&client_buf[..n]).await {
                                println!("写入目标服务器失败: {}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            println!("从客户端读取失败: {}", e);
                            break;
                        }
                    }
                }
                result = target_stream.read(&mut server_buf) => {
                    match result {
                        Ok(0) => {
                            println!("服务器关闭连接 [RequestID: {}]", request_id);
                            break;
                        }
                        Ok(n) => {
                            // 统计下行流量
                            if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Downstream, &server_buf[..n]).await {
                                println!("统计下行流量失败: {}", e);
                            }
                            if let Err(e) = client_stream.write_all(&server_buf[..n]).await {
                                println!("写入客户端失败: {}", e);
                                break;
                            }
                        }
                        Err(e) => {
                            println!("从服务器读取失败: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        // 在隧道关闭时调用插件的 on_connect_close
        if let Err(e) = plugin_manager.handle_connect_close(request_id, &host).await {
            println!(
                "处理连接关闭失败 [RequestID: {}, Host: {}]: {}",
                request_id, host, e
            );
        }
        println!("HTTPS 隧道关闭 [RequestID: {}, Host: {}]", request_id, host);
    }

    // 重构后的 handle_connect 函数
    async fn handle_connect(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Empty<Bytes>>, hyper::Error> {
        let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
        let auth = match req.uri().authority() {
            Some(auth) => auth,
            None => {
                println!("无效的 CONNECT 请求");
                return Ok(Response::builder().status(400).body(Empty::new()).unwrap());
            }
        };

        let addr = if !auth.port().is_some() {
            format!("{}:443", auth)
        } else {
            auth.to_string()
        };

        println!("正在建立到 {} 的 HTTPS 隧道 [Host: {}]", addr, auth.host());

        // 通知插件系统 CONNECT 请求
        if let Err(e) = self.plugin_manager.handle_connect(request_id, &addr).await {
            println!("插件处理 CONNECT 请求失败: {}", e);
            return Ok(Response::builder().status(500).body(Empty::new()).unwrap());
        }

        match TcpStream::connect(&addr).await {
            Ok(target_stream) => {
                println!("成功连接到目标服务器 [Host: {}]", auth.host());

                let resp = Response::builder()
                    .status(200)
                    .header("Connection", "keep-alive")
                    .body(Empty::new())
                    .unwrap();

                let plugin_manager = self.plugin_manager.clone();
                let host = auth.host().to_string();

                tokio::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            ProxyServer::handle_tunnel(
                                &plugin_manager,
                                request_id,
                                upgraded,
                                target_stream,
                                host,
                            )
                            .await;
                        }
                        Err(e) => println!("连接升级失败 [Host: {}]: {}", host, e),
                    }
                });

                Ok(resp)
            }
            Err(e) => {
                println!("连接目标服务失败: {}", e);
                Ok(Response::builder().status(502).body(Empty::new()).unwrap())
            }
        }
    }

    async fn proxy_handler(
        &self,
        mut req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        match *req.method() {
            Method::CONNECT => {
                let connect_response = self.handle_connect(req).await.unwrap_or_else(|e| {
                    println!("处理 CONNECT 请求失败: {}", e);
                    Response::builder().status(500).body(Empty::new()).unwrap()
                });
                Ok(connect_response.map(|_| Empty::new().map_err(|never| match never {}).boxed()))
            }
            _ => {
                let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
                // 处理请求，如果插件返回 false，表示不继续处理
                match self
                    .plugin_manager
                    .handle_request(request_id, &mut req)
                    .await
                {
                    Ok((false, Some(response))) => {
                        return Ok(response);
                    }
                    Ok((false, None)) => {
                        // 如果插件要求停止处理但没有提供响应，返回 400 错误
                        let body = Full::from(Bytes::from("Bad Plugin Response"))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::builder().status(400).body(body).unwrap());
                    }
                    Ok((true, _)) => (), // 继续处理
                    Err(e) => {
                        println!("插件处理请求失败: {}", e);
                        let body = Full::from(Bytes::from("Internal Server Error"))
                            .map_err(|never| match never {})
                            .boxed();
                        return Ok(Response::builder().status(500).body(body).unwrap());
                    }
                }

                let https = HttpsConnector::new();
                let client = Client::builder(TokioExecutor::new()).build::<_, Incoming>(https);

                // 保存请求的必要信息
                let req_method = req.method().clone();
                let req_uri = req.uri().clone();
                let req_headers = req.headers().clone();
                let mut req_clone = Request::builder()
                    .uri(req_uri)
                    .method(req_method)
                    .body(())
                    .unwrap();
                // 将req_headers 复制到req_clone上面，使用循环遍历，
                for (key, value) in req_headers.iter() {
                    req_clone.headers_mut().insert(key, value.clone());
                }

                match client.request(req).await {
                    Ok(mut response) => {
                        if let Err(e) = self
                            .plugin_manager
                            .handle_response(request_id, &req_clone, &mut response)
                            .await
                        {
                            println!("插件处理响应失败: {}", e);
                            let body = Full::from(format!("Plugin response error: {}", e))
                                .map_err(|never| match never {})
                                .boxed();
                            return Ok(Response::builder().status(500).body(body).unwrap());
                        }
                        Ok(response.map(|b| b.boxed()))
                    }
                    Err(e) => {
                        println!("请求目标服务器失败: {}", e);
                        let body = Full::from(format!("Request failed: {}", e))
                            .map_err(|never| match never {})
                            .boxed();
                        Ok(Response::builder().status(502).body(body).unwrap())
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // 初始化日志
    env_logger::init();

    let args = Args::parse();

    let cert_path = PathBuf::from("files/root.crt");
    let key_path = PathBuf::from("files/root.key");

    // 检查证书文件
    if !cert_path.exists() || !key_path.exists() {
        eprintln!(
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
        args.h2_only,
        Some(args.max_network_records),
    );

    // 如果启用 HTTPS 劫持，加载证书和密钥
    let (root_cert, root_key) = if args.enable_https {
        println!("启用HTTPS流量劫持");
        if args.enable_h2 {
            println!("启用 HTTP/2 支持");
            if args.h2_only {
                println!("仅使用 HTTP/2 协议");
            }
        }

        let cert = fs::read(&cert_path).expect("无法读取根证书");
        let key = fs::read(&key_path).expect("无法读取密钥文件");

        let root_cert = CertificateDer::from(cert);
        let root_key = PrivateKeyDer::from(rustls::pki_types::PrivatePkcs8KeyDer::from(key));

        (Some(root_cert), Some(root_key))
    } else {
        (None, None)
    };

    let addr = SocketAddr::from(([0, 0, 0, 0], args.port));
    let proxy_server = Arc::new(ProxyServer::new());

    let listener = TcpListener::bind(addr).await.unwrap();
    println!("代理服务器运行在 http://{}", addr);

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
                println!("服器错误: {}", err);
            }
        });
    }
}
