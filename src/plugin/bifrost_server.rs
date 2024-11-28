use super::DataDirection;
use crate::context::Context;
use crate::plugin::Plugin;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use serde_json::{json, Value};
use std::error::Error;

type HandlerResult = Result<Value, Box<dyn Error + Send + Sync>>;

pub struct BifrostServerPlugin {}

impl BifrostServerPlugin {
    pub fn new() -> Self {
        Self {}
    }

    async fn host_server(
        &self,
        req: &Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn Error + Send + Sync>> {
        if req.method() == hyper::Method::OPTIONS {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Access-Control-Allow-Origin", "*")
                .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                .header("Access-Control-Allow-Headers", "Content-Type")
                .body(BoxBody::new(
                    Full::new(Bytes::new()).map_err(|never| match never {}),
                ))
                .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>);
        }

        let response = match req.uri().path() {
            "/" => Self::wrap_response(Ok(json!("Bifrost is working"))).await,
            "/config" => Self::wrap_response(Self::handle_config(req).await).await,
            "/get_record" => Self::wrap_response(Self::handle_get_record(req).await).await,
            _ => Self::handle_not_found(),
        }?;

        Ok(response)
    }

    async fn wrap_response(
        result: HandlerResult,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn Error + Send + Sync>> {
        match result {
            Ok(data) => {
                let response_json = json!({
                    "code": 0,
                    "data": data
                });

                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/json")
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(BoxBody::new(
                        Full::from(Bytes::from(response_json.to_string()))
                            .map_err(|never| match never {}),
                    ))
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
            }
            Err(e) => {
                let response_json = json!({
                    "code": 1,
                    "message": e.to_string()
                });

                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header("Content-Type", "application/json")
                    .header("Access-Control-Allow-Origin", "*")
                    .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                    .header("Access-Control-Allow-Headers", "Content-Type")
                    .body(BoxBody::new(
                        Full::from(Bytes::from(response_json.to_string()))
                            .map_err(|never| match never {}),
                    ))
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
            }
        }
    }

    async fn handle_config(_req: &Request<Incoming>) -> HandlerResult {
        let config = Context::global().get_config().await;
        Ok(json!({
            "port": config.port,
            "cert_path": config.cert_path.to_string_lossy(),
            "key_path": config.key_path.to_string_lossy(),
            "enable_https": config.enable_https,
        }))
    }

    async fn handle_get_record(_req: &Request<Incoming>) -> HandlerResult {
        let context = Context::global();
        let records = context.get_network_records().await;

        // 获取最新的10条记录并过滤body内容
        let filtered_records: Vec<Value> = records
            .iter()
            .rev()
            .take(10)
            .map(|record| {
                let mut record_json = serde_json::to_value(record)?;
                if let Value::Object(ref mut map) = record_json {
                    map.remove("request_body");
                    map.remove("response_body");
                }
                Ok(record_json)
            })
            .collect::<Result<Vec<_>, Box<dyn Error + Send + Sync>>>()?;

        Ok(Value::Array(filtered_records))
    }

    fn handle_not_found(
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn Error + Send + Sync>> {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header("Content-Type", "application/json")
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
            .header("Access-Control-Allow-Headers", "Content-Type")
            .body(BoxBody::new(
                Full::from(Bytes::from(
                    json!({
                        "code": 1,
                        "message": "Route not found"
                    })
                    .to_string(),
                ))
                .map_err(|never| match never {}),
            ))
            .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
    }
}

#[async_trait]
impl Plugin for BifrostServerPlugin {
    async fn handle_request(
        &self,
        request_id: u64,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        let port = Context::global().get_config().await.port;

        // 获取原始目标地址的几种方式：
        let target_host = if req.method() == hyper::Method::CONNECT {
            // 1. 对于 CONNECT 请求（HTTPS），目标地址在 URI 中
            // 格式为: "CONNECT example.com:443 HTTP/1.1"
            req.uri().to_string()
        } else {
            // 2. 对于普通 HTTP 请求，需要检查完整的 URL
            // 格式为: "http://example.com/path"
            match req.uri().scheme_str() {
                Some(scheme) => {
                    // 如果是绝对路径 URL
                    format!("{}://{}", scheme, req.uri().authority().unwrap())
                }
                None => {
                    // 如果是相对路径，从 Host header 获取
                    req.headers()
                        .get(hyper::header::HOST)
                        .map(|h| h.to_str().unwrap_or(""))
                        .unwrap_or("")
                        .to_string()
                }
            }
        };

        println!("Original target: {}", target_host);

        // 判断是否是访问 Bifrost 服务器的请求
        if target_host.contains(&format!("127.0.0.1:{}", port))
            || target_host.contains(&format!("localhost:{}", port))
        {
            println!("Request handled by Bifrost Server");
            let response = self.host_server(req).await?;
            return Ok((false, Some(response)));
        }

        println!("Request forwarded to proxy target: {}", target_host);
        Ok((true, None))
    }

    async fn handle_response(
        &self,
        request_id: u64,
        resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        Ok(true)
    }

    async fn handle_connect(
        &self,
        request_id: u64,
        addr: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn handle_connect_close(
        &self,
        request_id: u64,
        addr: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }

    async fn handle_data(
        &self,
        request_id: u64,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        Ok(())
    }
}
