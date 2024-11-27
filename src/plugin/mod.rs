use hyper::{Request, Response};
use async_trait::async_trait;
use std::sync::Arc;
use hyper::body::{Body, Incoming};
use std::error::Error;

pub mod traffic_stats;

#[async_trait]
pub trait Plugin: Send + Sync {
    async fn on_request(&self, req: &mut Request<Incoming>) -> Result<(), Box<dyn Error>>;
    async fn on_response(&self, resp: &mut Response<Incoming>) -> Result<(), Box<dyn Error>>;
    async fn on_connect(&self, target: &str) -> Result<(), Box<dyn Error>>;
    async fn on_connect_close(&self, target: &str) -> Result<(), Box<dyn Error>>;
    async fn on_data(&self, direction: DataDirection, data: &[u8]) -> Result<(), Box<dyn Error>>;
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

    pub async fn handle_request(&self, req: &mut Request<Incoming>) -> Result<(), Box<dyn Error>> {
        for plugin in &self.plugins {
            plugin.on_request(req).await?;
        }
        Ok(())
    }

    pub async fn handle_response(&self, resp: &mut Response<Incoming>) -> Result<(), Box<dyn Error>> {
        for plugin in &self.plugins {
            plugin.on_response(resp).await?;
        }
        Ok(())
    }

    pub async fn handle_connect(&self, target: &str) -> Result<(), Box<dyn Error>> {
        for plugin in &self.plugins {
            plugin.on_connect(target).await?;
        }
        Ok(())
    }

    pub async fn handle_connect_close(&self, target: &str) -> Result<(), Box<dyn Error>> {
        for plugin in &self.plugins {
            plugin.on_connect_close(target).await?;
        }
        Ok(())
    }

    pub async fn handle_data(&self, direction: DataDirection, data: &[u8]) -> Result<(), Box<dyn Error>> {
        for plugin in &self.plugins {
            plugin.on_data(direction.clone(), data).await?;
        }
        Ok(())
    }
} 