use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};

use hyper_util::rt::TokioIo;
use log::{error, info};

use std::error::Error;
use std::time::Duration;
type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
use futures_util::{SinkExt, StreamExt};

use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
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
                info!("WebSocket Connection Success [Host: {}]", host);

                // 构建升级响应
                let mut response_builder = Response::builder().status(response.status());
                // 复制所有响应头
                for (name, value) in response.headers() {
                    response_builder = response_builder.header(name, value);
                }

                let host = host.to_string();
                tokio::spawn(async move {
                    Websocket::handle_websocket_upgrade(req, target_uri, host).await;
                });
                Ok(response_builder
                    .body(Empty::new().map_err(|never| match never {}).boxed())
                    .unwrap())
            }
            Err(e) => {
                error!("WebSocket Connection Failed [Target URI: {}]", target_uri);
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
    // 处理 WebSocket 连接升级和消息转发
    async fn handle_websocket_upgrade(req: Request<Incoming>, target_uri: String, host: String) {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let ws_config =
                    tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();

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

                                info!("WebSocket Connection Success [Host: {}]", host);

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
                                                error!(
                                                    "从客户端读取消息失败 [Host: {}]: {}",
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
                                                        "发送消息到客户端失败 [Host: {}]: {}",
                                                        host, e
                                                    );
                                                    break;
                                                }
                                            }
                                            Err(e) => {
                                                error!(
                                                    "从服务器读取消息失败 [Host: {}]: {}",
                                                    host, e
                                                );
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
}
