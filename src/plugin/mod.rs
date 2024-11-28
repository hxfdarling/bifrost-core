use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Incoming};
use hyper::{Request, Response};
use std::error::Error;
use std::sync::Arc;

pub mod bifrost_server;
pub mod https_interceptor;
pub mod net_storage;
pub mod traffic_stats;

#[async_trait]
pub trait Plugin: Send + Sync {
    // HTTP 请求处理
    async fn handle_request(
        &self,
        request_id: u64,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>;
    // HTTP 响应处理
    async fn handle_response(
        &self,
        request_id: u64,
        req: &Request<()>,
        resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>>;
    // 连接处理
    async fn handle_connect(
        &self,
        request_id: u64,
        addr: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;
    // 连接关闭处理
    async fn handle_connect_close(
        &self,
        request_id: u64,
        addr: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>>;
    // 数据处理
    async fn handle_data(
        &self,
        request_id: u64,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>>;
}

#[derive(Debug, Clone, Copy)]
pub enum DataDirection {
    Upstream,
    Downstream,
}

pub struct PluginManager {
    plugins: Vec<Arc<dyn Plugin>>,
}

impl PluginManager {
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
        }
    }

    pub fn register_plugin(&mut self, plugin: Arc<dyn Plugin>) {
        self.plugins.push(plugin);
    }

    pub async fn handle_request(
        &self,
        request_id: u64,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        for plugin in &self.plugins {
            let (continue_processing, response) = plugin.handle_request(request_id, req).await?;
            if !continue_processing {
                return Ok((false, response));
            }
        }
        Ok((true, None))
    }
    // 返回的bool表示是否继续后续流程，也包括中断后续插件执行
    pub async fn handle_response(
        &self,
        request_id: u64,
        req: &Request<()>,
        resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            let continue_processing = plugin.handle_response(request_id, req, resp).await?;
            if !continue_processing {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn handle_connect(
        &self,
        request_id: u64,
        target: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_connect(request_id, target).await?;
        }
        Ok(())
    }

    pub async fn handle_connect_close(
        &self,
        request_id: u64,
        target: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_connect_close(request_id, target).await?;
        }
        Ok(())
    }

    pub async fn handle_data(
        &self,
        request_id: u64,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_data(request_id, direction, data).await?;
        }
        Ok(())
    }
}
