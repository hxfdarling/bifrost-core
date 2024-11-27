use crate::context::Context;
use crate::plugin::{DataDirection, Plugin};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::server::conn::http2;
use hyper::Version;
use hyper::{Request, Response};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use log::{debug, error, info, warn};
use rcgen::Certificate;
use rcgen::{CertificateParams, DistinguishedName, DnType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

type Result<T, E = Box<dyn Error + Send + Sync>> = std::result::Result<T, E>;

#[derive(Clone)]
struct CachedCert {
    cert: Arc<CertificateDer<'static>>,
    key: Arc<PrivateKeyDer<'static>>,
    expiry: SystemTime,
}

pub struct HttpsInterceptorPlugin {
    cert_cache: Arc<RwLock<HashMap<String, CachedCert>>>,
    cache_duration: Duration,
    root_cert: Option<(CertificateDer<'static>, PrivateKeyDer<'static>)>,
}

impl HttpsInterceptorPlugin {
    pub fn new(cache_duration: Option<Duration>) -> Self {
        Self {
            cert_cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: cache_duration.unwrap_or(Duration::from_secs(3600)),
            root_cert: None,
        }
    }
}

#[async_trait]
impl Plugin for HttpsInterceptorPlugin {
    async fn handle_request(
        &self,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>)> {
        Ok((true, None))
    }

    async fn handle_response(&self, resp: &mut Response<Incoming>) -> Result<bool> {
        Ok(true)
    }

    async fn handle_connect(&self, target: &str) -> Result<()> {
        // 检查是否启用 HTTPS 拦截
        let config = Context::global().get_config().await;
        if !config.enable_https {
            return Ok(());
        }

        debug!("HTTPS拦截插件处理CONNECT请求: {}", target);
        Ok(())
    }

    async fn handle_connect_close(&self, addr: &str) -> Result<()> {
        // 实现连接关闭处理逻辑
        Ok(())
    }

    async fn handle_data(&self, direction: DataDirection, data: &[u8]) -> Result<()> {
        // 实现数据处理逻辑
        Ok(())
    }
}
