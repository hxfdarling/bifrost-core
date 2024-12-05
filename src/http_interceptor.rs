use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use log::{error, info};
use std::convert::Infallible;
use std::error::Error;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hyper_util::rt::TokioExecutor;

use crate::plugin::PluginManager;
use crate::store::REQUEST_ID_COUNTER;
use crate::websocket_interceptor::Websocket;
use dashmap::DashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;

// DNS 缓存, 全局共享
static DNS_CACHE: LazyLock<Arc<DashMap<String, (Vec<std::net::SocketAddr>, u64)>>> =
    LazyLock::new(|| Arc::new(DashMap::new()));
const CACHE_DURATION: u64 = 300; // 5分钟缓存时间（秒）

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
    // 新增的 DNS 查询函数
    async fn dns_lookup(host: &str) -> Result<Vec<std::net::SocketAddr>> {
        let start_time = Instant::now();
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        // 检查缓存
        if let Some(entry) = DNS_CACHE.get(host) {
            let (addrs, timestamp) = entry.value();
            if current_time - timestamp < CACHE_DURATION {
                info!(
                    "DNS Cache Hit [host: {}], [Time: {:?}]",
                    host,
                    start_time.elapsed()
                );
                return Ok(addrs.clone());
            }
        }

        let port = host.split(':').nth(1).unwrap_or("443");
        let addr = (
            host.split(':').next().unwrap(),
            port.parse::<u16>().unwrap(),
        );
        info!("DNS Lookup [addr: {:?}]", addr);

        match tokio::net::lookup_host(addr).await {
            Ok(addrs) => {
                let addrs = addrs.collect::<Vec<_>>();
                // 更新缓存
                DNS_CACHE.insert(host.to_string(), (addrs.clone(), current_time));
                info!(
                    "DNS Lookup Success [addr: {:?}], [Time: {:?}]",
                    addr,
                    start_time.elapsed()
                );
                Ok(addrs)
            }
            Err(e) => {
                error!(
                    "DNS Lookup Failed: {}, [Time: {:?}]",
                    e,
                    start_time.elapsed()
                );
                Err(e.into())
            }
        }
    }
    pub async fn handle_http(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Infallible> {
        let host = HttpInterceptor::get_host(&req);

        if let Err(_) = HttpInterceptor::dns_lookup(&host).await {
            return Ok(HttpInterceptor::build_502_error(&format!(
                "DNS Lookup Failed: [{}]",
                &host
            )));
        }

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
    fn get_host(req: &Request<Incoming>) -> String {
        if req.version() == hyper::Version::HTTP_2 {
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
        }
    }

    pub async fn http_request(
        req: Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>> {
        let host = HttpInterceptor::get_host(&req);
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
        info!("HTTP Request [URI: {}]", uri_string);

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
