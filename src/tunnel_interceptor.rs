use crate::plugin::{DataDirection, PluginManager};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use log::{error, info, warn};
use std::error::Error;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::select;
use tokio::time::sleep;
type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
#[derive(Clone)]

pub struct TunnelInterceptor {}

impl TunnelInterceptor {
    // 隧道代理处
    pub async fn handle_tunnel(
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
                                    info!("Client Connection Closed [RequestID: {}, Host: {}]", request_id, host);
                                    break;
                                }
                                Ok(n) => {
                                    error_count = 0; // 重置错误计数
                                    timeout.as_mut().reset(tokio::time::Instant::now() + TIMEOUT_DURATION);

                                    if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Upstream, &client_buf[..n]).await {
                                        error!("Upstream Traffic Statistics Failed: {}", e);
                                    }
                                    if let Err(e) = target_stream.write_all(&client_buf[..n]).await {
                                        error!("Write to Target Server Failed: {}", e);
                                        error_count += 1;
                                        if error_count >= MAX_ERRORS {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Read from Client Failed: {}", e);
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
                                    info!("Target Server Connection Closed [RequestID: {}]", request_id);
                                    break;
                                }
                                Ok(n) => {
                                    error_count = 0; // 重置错误计数
                                    timeout.as_mut().reset(tokio::time::Instant::now() + TIMEOUT_DURATION);

                                    if let Err(e) = plugin_manager.handle_data(request_id, DataDirection::Downstream, &server_buf[..n]).await {
                                        warn!("Downstream Traffic Statistics Failed: {}", e);
                                    }
                                    if let Err(e) = client_stream.write_all(&server_buf[..n]).await {
                                        error!("Write to Client Failed: {}", e);
                                        error_count += 1;
                                        if error_count >= MAX_ERRORS {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Read from Target Server Failed: {}", e);
                                    error_count += 1;
                                    if error_count >= MAX_ERRORS {
                                        break;
                                    }
                                }
                            }
                        }
                        _ = &mut timeout => {
                            error!("Connection Timeout ({} seconds without data transmission), Close Tunnel [RequestID: {}, Host: {}]",
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
                error!("Handle Connection Failed: {}", e);
            }
        };

        // 在隧道关闭时调用插件的 on_connect_close
        if let Err(e) = plugin_manager.handle_connect_close(request_id, &host).await {
            error!(
                "Handle Connection Close Failed [RequestID: {}, Host: {}]: {}",
                request_id, host, e
            );
        }
        info!(
            "HTTPS Tunnel Closed [RequestID: {}, Host: {}]",
            request_id, host
        );
    }
    // 隧道代理预处理
    pub async fn handle_tunnel_proxy(
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
                            info!("Establishing HTTPS Tunnel to {} [Host: {}]", addr, host);
                            let cloned_req = Request::builder()
                                .method(hyper::Method::CONNECT)
                                .uri(format!("https://{}", host))
                                .body(())
                                .unwrap();
                            TunnelInterceptor::handle_tunnel(
                                request_id,
                                upgraded,
                                cloned_req,
                                target_stream,
                                host,
                            )
                            .await;
                        }
                        Err(e) => error!("Upgrade Connection Failed: {}", e),
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
                error!("Connect to Target Server Failed: {}", e);
                Ok(Response::builder()
                    .status(502)
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap())
            }
        }
    }
}
