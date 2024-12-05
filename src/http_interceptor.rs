use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use log::{error, info};
use std::convert::Infallible;
use std::error::Error;

use std::time::{Duration, Instant};

use hyper_util::rt::TokioExecutor;

use crate::plugin::PluginManager;
use crate::store::REQUEST_ID_COUNTER;
use crate::websocket_interceptor::Websocket;
use std::sync::atomic::Ordering;

type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;
#[derive(Clone)]
pub struct HttpInterceptor {}

impl HttpInterceptor {
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
    pub async fn handle_http(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        // 检查是否为 WebSocket 升级请求
        if Websocket::is_websocket_upgrade(&req) {
            return match Websocket::handle_websocket(req).await {
                Ok(response) => Ok(response),
                Err(e) => {
                    error!("WebSocket Upgrade Failed: {}", e);
                    let body = Full::from(Bytes::from("WebSocket upgrade failed"))
                        .map_err(|never| match never {})
                        .boxed();
                    Ok(Response::builder().status(400).body(body).unwrap())
                }
            };
        }
        match HttpInterceptor::http_request(req).await {
            Ok(response) => Ok(response),
            Err(e) => {
                error!("HTTP Request Failed: {}", e);
                let body = Full::from(Bytes::from("Internal Server Error"))
                    .map_err(|never| match never {})
                    .boxed();
                Ok(Response::builder().status(500).body(body).unwrap())
            }
        }
    }

    pub async fn http_request(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        // 从请求头中获取 host，HTTP/2 中 :authority 伪头部字段替代了 Host 头
        let host = if req.version() == hyper::Version::HTTP_2 {
            // 对于 HTTP/2，首先尝试从 :authority 伪头部获取
            req.uri()
                .authority()
                .map(|a| a.to_string())
                .or_else(|| {
                    req.headers()
                        .get(":authority")
                        .and_then(|h| h.to_str().ok())
                        .map(|s| s.to_string())
                })
                .unwrap_or("localhost".to_string())
        } else {
            // HTTP/1.1 继续使用 Host 头
            req.headers()
                .get(hyper::header::HOST)
                .and_then(|h| h.to_str().ok())
                .unwrap_or("localhost")
                .to_string()
        };

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
            // 使用之前获取的 host
            format!(
                "https://{}{}",
                host,
                req.uri().path_and_query().map(|x| x.as_str()).unwrap_or("")
            )
        } else {
            req.uri().to_string()
        };

        // 构建新请求
        let mut builder = Request::builder()
            .method(req.method())
            .uri(uri_string.clone());

        // 复制所有请求头
        for (name, value) in req.headers() {
            builder = builder.header(name, value);
        }
        let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::SeqCst);

        let new_req = builder
            .body(req.into_body())
            .map_err(|e| format!("Build Request Failed: {}", e))?;
        // 处理请求，如果插件返回 false，表示不继续处理
        let mut new_req = new_req;
        match PluginManager::global()
            .handle_request(request_id, &mut new_req)
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
                error!("Plugin Handle Request Failed: {}", e);
                let body = Full::from(Bytes::from("Internal Server Error"))
                    .map_err(|never| match never {})
                    .boxed();
                return Ok(Response::builder().status(500).body(body).unwrap());
            }
        }
        let start_time = Instant::now();
        match client.request(new_req).await {
            Ok(response) => {
                info!(
                    "HTTP Request Success [Protocol: {}], [Time: {:?}], Host: {}",
                    uri_string.split("://").next().unwrap_or(""),
                    start_time.elapsed(),
                    host
                );
                Ok(response.map(|b| b.boxed()))
            }
            Err(e) => {
                error!(
                    "HTTP Request Failed [URI: {}], [Time: {:?}]: {}",
                    uri_string,
                    start_time.elapsed(),
                    e
                );
                Ok(Self::build_502_error(&format!(
                    "Service Unavailable. Can't Connect to Target Server [{}], [Time: {:?}]",
                    host,
                    start_time.elapsed()
                )))
            }
        }
    }
}
