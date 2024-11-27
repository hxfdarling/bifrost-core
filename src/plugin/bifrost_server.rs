use super::DataDirection;
use crate::plugin::Plugin;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
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
}

#[async_trait]
impl Plugin for BifrostServerPlugin {
    async fn handle_request(
        &self,
        _req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        // 检查主机地址
        if _req.uri().host().is_none() {
            println!("No host in request URI, responding directly");

            // 创建响应体
            let body: BoxBody<Bytes, hyper::Error> = Full::from(Bytes::from("Bifrost Working"))
                .map_err(|never| match never {})
                .boxed();

            // 构建响应
            let response = Response::builder()
                .status(200)
                .header("Content-Type", "text/plain")
                .body(body)
                .map_err(|e| {
                    let err = format!("Failed to build response: {}", e);
                    eprintln!("Error: {}", err);
                    err
                })?;

            return Ok((false, Some(response)));
        }

        // 其他情况交给后续模块处理
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
