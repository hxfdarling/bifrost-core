use crate::plugin::{DataDirection, Plugin};
use crate::store::Store;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::{body::Incoming, Request, Response};
use log::info;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::net::SocketAddr;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime};

const MAX_BODY_SIZE: usize = 2 * 1024 * 1024; // 2MB

static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RequestState {
    Connecting,
    Sending,
    Receiving,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkRecord {
    // 请求ID
    pub id: u64,
    // 网络基本信息
    pub client_ip: String,
    pub client_port: u16,
    pub server_ip: String,
    pub server_port: u16,
    pub host: String,
    pub url: String,
    pub path: String,
    pub protocol: String,
    pub method: String,

    // 请求/响应信息
    pub request_headers: Vec<(String, String)>,
    pub response_headers: Vec<(String, String)>,
    pub request_body: Option<Vec<u8>>,
    pub response_body: Option<Vec<u8>>,

    // 性能数据
    pub request_size: usize,
    pub response_size: usize,
    pub start_time: SystemTime,
    pub end_time: Option<SystemTime>,
    pub dns_duration: Option<Duration>,
    pub tcp_connect_duration: Option<Duration>,
    pub ttfb_duration: Option<Duration>, // Time to First Byte
    pub download_duration: Option<Duration>,

    // 状态码
    pub status_code: Option<u16>,
    pub status_message: Option<String>,

    // 状态相关字段
    pub state: RequestState,
}

pub struct NetStorage;

impl NetStorage {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Plugin for NetStorage {
    async fn handle_request(
        &self,
        request_id: u64,
        req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        let remote_addr = req
            .extensions()
            .get::<SocketAddr>()
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let (client_ip, client_port) = if let Some(idx) = remote_addr.rfind(':') {
            (
                remote_addr[..idx].to_string(),
                remote_addr[idx + 1..].parse().unwrap_or(0),
            )
        } else {
            (remote_addr, 0)
        };

        // 计算请求头大小
        let mut request_size = 0;
        // 计算请求行大小 (Method + Path + Version)
        request_size += req.method().as_str().len() + 1; // +1 for space
        request_size += req.uri().path().len() + 1; // +1 for space
        request_size += format!("{:?}", req.version()).len() + 2; // +2 for \r\n

        // 计算请求头大小
        for (name, value) in req.headers() {
            request_size += name.as_str().len() + 2; // +2 for ": "
            request_size += value.len() + 2; // +2 for \r\n
        }
        request_size += 2; // 请求头结束的空行 \r\n

        let record = NetworkRecord {
            id: request_id,
            client_ip,
            client_port,
            server_ip: req.uri().host().unwrap_or("unknown").to_string(),
            server_port: req.uri().port_u16().unwrap_or(80),
            host: req.uri().host().unwrap_or("unknown").to_string(),
            url: req.uri().to_string(),
            path: req.uri().path().to_string(),
            protocol: format!("{:?}", req.version()),
            method: req.method().to_string(),
            request_headers: req
                .headers()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                .collect(),
            response_headers: vec![],
            request_body: None,
            response_body: None,
            request_size, // 更新请求大小初始值
            response_size: 0,
            start_time: SystemTime::now(),
            end_time: None,
            dns_duration: None,
            tcp_connect_duration: None,
            ttfb_duration: None,
            download_duration: None,
            status_code: None,
            status_message: None,
            state: RequestState::Connecting,
        };

        Store::global().add_network_record(record).await;

        // 更新状态为发送中
        Store::global()
            .update_network_record_by_id(request_id, |record| {
                record.state = RequestState::Sending;
            })
            .await
            .ok();

        Ok((true, None))
    }

    async fn handle_response(
        &self,
        request_id: u64,
        req: &Request<()>,

        resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // 打印日志
        info!("handle_response: {}", request_id);
        info!("response headers: {:?}", resp.headers());

        // 计算响应头大小
        let mut response_size = 0;
        // 计算状态行大小
        response_size += format!("{:?}", resp.version()).len() + 1; // +1 for space
        response_size += resp.status().as_str().len() + 1; // +1 for space
        response_size += resp.status().canonical_reason().unwrap_or("").len() + 2; // +2 for \r\n

        // 计算响应头大小
        for (name, value) in resp.headers() {
            response_size += name.as_str().len() + 2; // +2 for ": "
            response_size += value.len() + 2; // +2 for \r\n
        }
        response_size += 2; // 响应头结束的空行 \r\n

        Store::global()
            .update_network_record_by_id(request_id, |record| {
                record.state = RequestState::Receiving;
                record.status_code = Some(resp.status().as_u16());
                record.status_message = Some(
                    resp.status()
                        .canonical_reason()
                        .unwrap_or("Unknown")
                        .to_string(),
                );
                record.response_headers = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
                    .collect();
                record.end_time = Some(SystemTime::now());
                record.response_size = response_size; // 设置响应头大小

                if let Ok(duration) = record.end_time.unwrap().duration_since(record.start_time) {
                    record.download_duration = Some(duration);
                }
            })
            .await
            .ok();

        Ok(true)
    }

    async fn handle_connect(
        &self,
        request_id: u64,
        req: &Request<()>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        Ok((true, None))
    }

    async fn handle_connect_close(
        &self,
        request_id: u64,
        addr: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // 连接关闭时，如果状态不是 Completed，则标记为 Cancelled
        Store::global()
            .update_network_record_by_id(request_id, |record| {
                if !matches!(record.state, RequestState::Completed) {
                    record.state = RequestState::Cancelled;
                }
            })
            .await
            .ok();
        Ok(true)
    }

    async fn handle_data(
        &self,
        request_id: u64,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        Store::global()
            .update_network_record_by_id(request_id, |record| {
                match direction {
                    DataDirection::Upstream => {
                        record.request_size += data.len();
                        if record.request_size <= MAX_BODY_SIZE {
                            if record.request_body.is_none() {
                                record.request_body = Some(Vec::new());
                            }
                            if let Some(body) = &mut record.request_body {
                                body.extend_from_slice(data);
                            }
                        } else {
                            record.request_body = None;
                        }
                    }
                    DataDirection::Downstream => {
                        record.response_size += data.len();
                        if record.response_size <= MAX_BODY_SIZE {
                            if record.response_body.is_none() {
                                record.response_body = Some(Vec::new());
                            }
                            if let Some(body) = &mut record.response_body {
                                body.extend_from_slice(data);
                            }
                        } else {
                            record.response_body = None;
                        }
                        // 当收到完整响应数据时，将状态更新为完成
                        record.state = RequestState::Completed;
                    }
                }
            })
            .await
            .ok();

        Ok(())
    }
}
