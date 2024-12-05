use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};

use hyper_util::rt::TokioIo;
use log::{error, info};

use std::error::Error;
type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
use futures_util::{SinkExt, StreamExt};

use tokio_tungstenite::connect_async;
pub struct Websocket {}

impl Websocket {
    // 判断是否为WebSocket升级请求
    pub fn is_websocket_upgrade(req: &Request<Incoming>) -> bool {
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
    pub async fn handle_websocket(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        // 从请求头中获取 host
        let host = req
            .headers()
            .get(hyper::header::HOST)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default()
            .to_string();

        // 构建目标 URI
        let target_uri = if req.uri().scheme().is_none() {
            format!(
                "wss://{}{}",
                req.headers()
                    .get(hyper::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or(&host),
                req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
            )
        } else {
            req.uri().to_string().replace("http", "ws")
        };
        info!(
            "WebSocket request [Protocol: {}] [Host: {}]",
            target_uri.split("://").next().unwrap_or(""),
            host
        );

        // 逐个尝试每个子协议

        let mut request_builder = Request::builder().uri(target_uri.clone()).method("GET");

        // 复制其他请求头
        for (name, value) in req.headers() {
            request_builder = request_builder.header(name, value);
        }
        // connect to target service
        match connect_async(request_builder.body(()).unwrap()).await {
            Ok((target_ws_stream, response)) => {
                info!(
                    "WebSocket connection to target service success, [Host: {}]",
                    host
                );
                // upgrade to websocket
                tokio::spawn(async move {
                    match hyper::upgrade::on(req).await {
                        Ok(upgraded) => {
                            info!("WebSocket upgrade success [Host: {}]", host);
                            let ws_config =
                                tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default(
                                );

                            // 创建客户端 WebSocket 流
                            let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                                TokioIo::new(upgraded),
                                tokio_tungstenite::tungstenite::protocol::Role::Server,
                                Some(ws_config),
                            )
                            .await;

                            let host = host.to_string();
                            let (mut client_write, mut client_read) = ws_stream.split();
                            let (mut target_write, mut target_read) = target_ws_stream.split();

                            info!("WebSocket Connection Success [Host: {}]", host);

                            // 创建两个转发任务
                            let client_to_server = async {
                                while let Some(msg) = client_read.next().await {
                                    match msg {
                                        Ok(msg) => {
                                            if let Err(e) = target_write.send(msg).await {
                                                error!(
                                                    "Send message to target service failed, [Host: {}]: {}",
                                                    host, e
                                                );
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Read message from client failed, [Host: {}]: {}",
                                                host, e
                                            );
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
                                                    "Send message to client failed, [Host: {}]: {}",
                                                    host, e
                                                );
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            error!(
                                                "Read message from target service failed, [Host: {}]: {}",
                                                host, e
                                            );
                                            break;
                                        }
                                    }
                                }
                            };

                            // 同时运行两个转发任务
                            tokio::select! {
                                _ = client_to_server => {
                                    info!("Client to target service forwarding task ended, [Host: {}]", host);
                                    // 发送关闭帧
                                    if let Err(e) = client_write.close().await {
                                        error!(
                                            "Close client connection failed, [Host: {}]: {}",
                                            host, e
                                        );
                                    }
                                    if let Err(e) = target_write.close().await {
                                        error!(
                                            "Close target service connection failed, [Host: {}]: {}",
                                            host, e
                                        );
                                    }
                                },
                                _ = server_to_client => {
                                    info!(
                                        "Target service to client forwarding task ended, [Host: {}]",
                                        host
                                    );
                                    // 发送关闭帧
                                    if let Err(e) = client_write.close().await {
                                        error!(
                                            "Close client connection failed, [Host: {}]: {}",
                                            host, e
                                        );
                                    }
                                    if let Err(e) = target_write.close().await {
                                        error!(
                                            "Close target service connection failed, [Host: {}]: {}",
                                            host, e
                                        );
                                    }
                                },
                            }
                        }
                        Err(e) => {
                            error!("WebSocket upgrade failed [Host: {}]: {}", host, e);
                        }
                    }
                });

                // 成功连接，直接返回
                let mut response_builder = Response::builder().status(response.status());
                for (name, value) in response.headers() {
                    response_builder = response_builder.header(name, value);
                }
                // return empty response
                return Ok(response_builder
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap());
            }
            Err(e) => {
                info!(
                    "WebSocket connection to target service failed, [Host: {}]: {}",
                    host, e
                );
                Ok(Response::builder()
                    .status(502)
                    .body(
                        Full::from(Bytes::from(format!("Bad Gateway: {}", e)))
                            .map_err(|never| match never {})
                            .boxed(),
                    )
                    .unwrap())
            }
        }
    }
}
