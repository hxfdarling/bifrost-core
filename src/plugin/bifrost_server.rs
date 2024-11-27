use super::DataDirection;
use crate::context::Context;
use crate::plugin::Plugin;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use serde_json::json;
use std::error::Error;

pub struct BifrostServerPlugin {
    // 添加需要的字段
}

impl BifrostServerPlugin {
    pub fn new() -> Self {
        Self {
            // 初始化字段
        }
    }

    async fn host_server(
        &self,
        req: &Request<Incoming>,
    ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, Box<dyn Error + Send + Sync>> {
        let path = req.uri().path();

        match path {
            "/config" => {
                let port = Context::global().get_port().await;
                let response_json = json!({
                    "code": 0,
                    "data": {
                        "port": port,
                        // 可以在这里添加更多配置信息
                    }
                });

                let body = BoxBody::new(
                    Full::from(Bytes::from(response_json.to_string()))
                        .map_err(|never| match never {}),
                );

                Response::builder()
                    .status(200)
                    .header("Content-Type", "application/json")
                    .body(body)
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
            }
            _ => {
                let body = BoxBody::new(
                    Full::from(Bytes::from("Bifrost Working")).map_err(|never| match never {}),
                );

                Response::builder()
                    .status(200)
                    .header("Content-Type", "text/plain")
                    .body(body)
                    .map_err(|e| Box::new(e) as Box<dyn Error + Send + Sync>)
            }
        }
    }
}

#[async_trait]
impl Plugin for BifrostServerPlugin {
    async fn handle_request(
        &self,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        let port = Context::global().get_port().await;
        println!("当前服务运行在端口: {}", port);

        // 如果请求中没有主机头，表示直接访问Bifrost Server，非代理流量
        if req.uri().host().is_none() {
            println!("No host in request URI, responding directly");
            let response = self.host_server(req).await?;
            return Ok((false, Some(response)));
        }
        println!("Request forwarded to next handler");
        Ok((true, None))
    }

    async fn handle_response(
        &self,
        resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // 实现响应处理逻辑
        Ok(false)
    }

    async fn handle_connect(&self, addr: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 实现连接处理逻辑
        Ok(())
    }

    async fn handle_connect_close(&self, addr: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 实现连接关闭处理逻辑
        Ok(())
    }

    async fn handle_data(
        &self,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 实现数据处理逻辑
        Ok(())
    }
}
