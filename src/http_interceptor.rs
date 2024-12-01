use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::Client;
use log::{error, info};
use std::error::Error;

use std::time::Duration;

use hyper_util::rt::TokioExecutor;

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

    pub async fn handle_http_request(
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
                Ok(Self::build_502_error(&format!(
                    "{} Bad Gateway: {}",
                    host,
                    e.to_string()
                )))
            }
        }
    }
}
