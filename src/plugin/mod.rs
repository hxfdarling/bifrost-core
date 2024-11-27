use hyper::{Request, Response};
use async_trait::async_trait;
use std::sync::Arc;
use hyper::body::{Body, Incoming};
use std::error::Error;
use http_body_util::combinators::BoxBody;
use bytes::Bytes;

pub mod traffic_stats;
pub mod bifrost_server;

#[async_trait]
pub trait Plugin: Send + Sync {
   // HTTP 请求处理 
    async fn handle_request(&self, req: &mut Request<Incoming>) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>;
    // HTTP 响应处理
    async fn handle_response(&self, resp: &mut Response<Incoming>) -> Result<bool, Box<dyn Error + Send + Sync>>;
    // 连接处理
    async fn handle_connect(&self, addr: &str) -> Result<(), Box<dyn Error + Send + Sync>>;
    // 连接关闭处理
    async fn handle_connect_close(&self, addr: &str) -> Result<(), Box<dyn Error + Send + Sync>>;
    // 数据处理
    async fn handle_data(&self, direction: DataDirection, data: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>>;
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

    pub async fn handle_request(&self, req: &mut Request<Incoming>) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            let (continue_processing, response) = plugin.handle_request(req).await?;
            if !continue_processing {
                return Ok((false, response));
            }
        }
        Ok((true, None))
    }

    pub async fn handle_response(&self, resp: &mut Response<Incoming>) -> Result<bool, Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            if plugin.handle_response(resp).await? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn handle_connect(&self, target: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_connect(target).await?;
        }
        Ok(())
    }

    pub async fn handle_connect_close(&self, target: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_connect_close(target).await?;
        }
        Ok(())
    }

    pub async fn handle_data(&self, direction: DataDirection, data: &[u8]) -> Result<(), Box<dyn Error + Send + Sync>> {
        for plugin in &self.plugins {
            plugin.handle_data(direction, data).await?;
        }
        Ok(())
    }
} 