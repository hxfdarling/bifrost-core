mod plugin;
use plugin::traffic_stats::TrafficStats;

use plugin::{PluginManager, DataDirection};
use std::sync::Arc;
use hyper::{Request, Response, Method};
use hyper::body::{Incoming, Body};
use hyper_util::rt::TokioExecutor;
use tokio::net::TcpListener;
use hyper::service::service_fn;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::select;
use hyper_tls::HttpsConnector;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};
use http_body_util::{Empty, Full, BodyExt};
use hyper_util::client::legacy::Client;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper_util::rt::TokioIo;

struct ProxyServer {
    plugin_manager: Arc<PluginManager>,
}

impl ProxyServer {
    pub fn new() -> Self {
        let mut plugin_manager = PluginManager::new();
        
        // 注册流量统计插件
        let traffic_stats = Arc::new(TrafficStats::new());
        plugin_manager.register_plugin(traffic_stats.clone());

        // 启动统计信息打印任务
        tokio::spawn({
            let traffic_stats = traffic_stats.clone();
            async move {
                loop {
                    let (bytes_in, bytes_out, total_reqs, current_reqs, qps, speed_in, speed_out, max_speed_in, max_speed_out, max_qps) = traffic_stats.get_stats();
                    println!(
                        "流量统计 - 入站: {:.2} MB ({:.2} MB/s, 峰值: {:.2} MB/s), 出站: {:.2} MB ({:.2} MB/s, 峰值: {:.2} MB/s), 请求: {} (总量: {}), QPS: {} (峰值: {})",
                        bytes_in as f64 / 1_048_576.0,
                        speed_in as f64 / 1_048_576.0,
                        max_speed_in as f64 / 1_048_576.0,
                        bytes_out as f64 / 1_048_576.0,
                        speed_out as f64 / 1_048_576.0,
                        max_speed_out as f64 / 1_048_576.0,
                        current_reqs,
                        total_reqs,
                        qps,
                        max_qps
                    );
                    sleep(Duration::from_secs(1)).await;
                }
            }
        });

        Self {
            plugin_manager: Arc::new(plugin_manager),
        }
    }

    async fn handle_connect(&self, req: Request<Incoming>) -> Result<Response<Empty<Bytes>>, hyper::Error> {
        if let Some(auth) = req.uri().authority() {
            let addr = if !auth.port().is_some() {
                format!("{}:443", auth)
            } else {
                auth.to_string()
            };
            
            println!("正在建立到 {} 的 HTTPS 隧道 [Host: {}]", addr, auth.host());
            
            // 通知插件系统 CONNECT 请求
            if let Err(e) = self.plugin_manager.handle_connect(&addr).await {
                println!("插件处理 CONNECT 请求失败: {}", e);
                return Ok(Response::builder()
                    .status(500)
                    .body(Empty::new())
                    .unwrap());
            }

            match TcpStream::connect(&addr).await {
                Ok(mut target_stream) => {
                    println!("成功连接到目标服务器 [Host: {}]", auth.host());
                    
                    let resp = Response::builder()
                        .status(200)
                        .header("Connection", "keep-alive")
                        .body(Empty::new())
                        .unwrap();

                    tokio::spawn({
                        let plugin_manager = self.plugin_manager.clone();  // 克隆 plugin_manager
                        let host = auth.host().to_string(); // 保存 host 信息供后续使用
                        async move {
                            match hyper::upgrade::on(req).await {
                                Ok(upgraded) => {
                                    println!("隧道连接已建立 [Host: {}]", host);
                                    let mut client_stream = TokioIo::new(upgraded);
                                    let mut client_buf = [0u8; 8192];
                                    let mut server_buf = [0u8; 8192];
                                    
                                    loop {
                                        tokio::select! {
                                            result = client_stream.read(&mut client_buf) => {
                                                match result {
                                                    Ok(0) => {
                                                        println!("客户端关闭连接 [Host: {}]", host);
                                                        break;
                                                    }
                                                    Ok(n) => {
                                                        // 统计上行流量
                                                        if let Err(e) = plugin_manager.handle_data(DataDirection::Upstream, &client_buf[..n]).await {
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
                                                        println!("服务器关闭连接");
                                                        break;
                                                    }
                                                    Ok(n) => {
                                                        // 统计下行流量
                                                        if let Err(e) = plugin_manager.handle_data(DataDirection::Downstream, &server_buf[..n]).await {
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
                                    if let Err(e) = plugin_manager.handle_connect_close(&host).await {
                                        println!("处理连接关闭失败 [Host: {}]: {}", host, e);
                                    }
                                    println!("HTTPS 隧道关闭 [Host: {}]", host);
                                }
                                Err(e) => println!("连接升级失败 [Host: {}]: {}", host, e),
                            }
                        }
                    });

                    Ok(resp)
                }
                Err(e) => {
                    println!("连接目标服务器失败: {}", e);
                    Ok(Response::builder()
                        .status(502)
                        .body(Empty::new())
                        .unwrap())
                }
            }
        } else {
            println!("无效的 CONNECT 请求");
            Ok(Response::builder()
                .status(400)
                .body(Empty::new())
                .unwrap())
        }
    }

    async fn proxy_handler(&self, mut req: Request<Incoming>) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        // 首先处理请求开始的统计
        if let Err(e) = self.plugin_manager.handle_request(&mut req).await {
            println!("插件处理请求失败: {}", e);
        }

        match *req.method() {
            Method::CONNECT => {
                let connect_response = self.handle_connect(req).await.unwrap_or_else(|_| {
                    Response::builder()
                        .status(500)
                        .body(Empty::new())
                        .unwrap()
                });
                Ok(connect_response.map(|_| Empty::new().map_err(|never| match never {}).boxed()))
            }
            _ => {
                let https = HttpsConnector::new();
                let client = Client::builder(TokioExecutor::new())
                    .build::<_, Incoming>(https);

                match client.request(req).await {
                    Ok(mut response) => {
                      // 处理响应
                        if let Err(e) = self.plugin_manager.handle_response(&mut response).await {
                            println!("插件处理响应失败: {}", e);
                            let body = Full::from("Internal Server Error").map_err(|never| match never {}).boxed();
                            return Ok(Response::builder()
                                .status(500)
                                .body(body)
                                .unwrap());
                        }else {
                            Ok(response.map(|b| b.boxed()))
                        }
                    }
                    Err(_) => {
                        let body = Full::from("Internal Server Error").map_err(|never| match never {}).boxed();
                        Ok(Response::builder()
                            .status(500)
                            .body(body)
                            .unwrap())
                    }
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let proxy_server = Arc::new(ProxyServer::new());
    
    let listener = TcpListener::bind(addr).await.unwrap();
    println!("代理服务器运行在 http://{}", addr);

    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let proxy_server = proxy_server.clone();
        
        let service = service_fn(move |req| {
            let proxy_server = proxy_server.clone();
            async move {
                proxy_server.proxy_handler(req).await
            }
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