use crate::plugin::DataDirection;
use crate::plugin::Plugin;
use crate::store::Store;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::{Request, Response};
use log::info;
use std::error::Error;
use std::sync::atomic::Ordering;
use tokio;

#[derive(Debug)]
pub struct TrafficStatsPlugin {}

impl TrafficStatsPlugin {
    pub fn new() -> Self {
        // 启动统计计算任务
        tokio::spawn(async move {
            let mut last_bytes_in: u64 = 0;
            let mut last_bytes_out: u64 = 0;

            loop {
                let traffic_stats = Store::global().get_traffic_stats();

                // 计算网速
                let current_bytes_in = traffic_stats.bytes_in.load(Ordering::Relaxed);
                let current_bytes_out = traffic_stats.bytes_out.load(Ordering::Relaxed);

                let speed_in = current_bytes_in.saturating_sub(last_bytes_in);
                let speed_out = current_bytes_out.saturating_sub(last_bytes_out);

                traffic_stats
                    .current_speed_in
                    .store(speed_in, Ordering::Relaxed);
                traffic_stats
                    .current_speed_out
                    .store(speed_out, Ordering::Relaxed);

                // 更新峰值速度
                let current_max_in = traffic_stats.max_speed_in.load(Ordering::Relaxed);
                if speed_in > current_max_in {
                    traffic_stats
                        .max_speed_in
                        .store(speed_in, Ordering::Relaxed);
                }

                let current_max_out = traffic_stats.max_speed_out.load(Ordering::Relaxed);
                if speed_out > current_max_out {
                    traffic_stats
                        .max_speed_out
                        .store(speed_out, Ordering::Relaxed);
                }

                // 计算并更新 QPS
                let current_qps = traffic_stats.requests_this_second.load(Ordering::Relaxed);
                traffic_stats.last_qps.store(current_qps, Ordering::Relaxed);
                traffic_stats
                    .requests_this_second
                    .store(0, Ordering::Relaxed);

                let current_max_qps = traffic_stats.max_qps.load(Ordering::Relaxed);
                if current_qps > current_max_qps {
                    traffic_stats.max_qps.store(current_qps, Ordering::Relaxed);
                }

                // 更新上一秒的数据
                last_bytes_in = current_bytes_in;
                last_bytes_out = current_bytes_out;

                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        });

        Self {}
    }

    pub fn start_stats_printer() {
        tokio::spawn(async move {
            loop {
                let traffic_stats = Store::global().get_traffic_stats();
                let stats = traffic_stats.as_ref();
                info!(
                    "Inbound: {:.2} MB ({:.2} MB/s, Peak: {:.2} MB/s), Outbound: {:.2} MB ({:.2} MB/s, Peak: {:.2} MB/s), Requests: {} (Total: {}), QPS: {} (Peak: {})",
                    stats.bytes_in.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.current_speed_in.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.max_speed_in.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.bytes_out.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.current_speed_out.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.max_speed_out.load(Ordering::Relaxed) as f64 / 1_048_576.0,
                    stats.current_requests.load(Ordering::Relaxed),
                    stats.total_requests.load(Ordering::Relaxed),
                    stats.last_qps.load(Ordering::Relaxed),
                    stats.max_qps.load(Ordering::Relaxed)
                );
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        });
    }
}

#[async_trait]
impl Plugin for TrafficStatsPlugin {
    async fn handle_request(
        &self,
        request_id: u64,
        _req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        let stats = Store::global().get_traffic_stats();
        stats.total_requests.fetch_add(1, Ordering::Relaxed);
        stats.current_requests.fetch_add(1, Ordering::Relaxed);
        stats.requests_this_second.fetch_add(1, Ordering::Relaxed);
        Ok((true, None))
    }

    async fn handle_response(
        &self,
        request_id: u64,
        req: &Request<()>,

        _resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let stats = Store::global().get_traffic_stats();
        stats.current_requests.fetch_sub(1, Ordering::Relaxed);
        Ok(true)
    }

    async fn handle_connect(
        &self,
        request_id: u64,
        req: &Request<()>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        let stats = Store::global().get_traffic_stats();
        stats.total_requests.fetch_add(1, Ordering::Relaxed);
        stats.current_requests.fetch_add(1, Ordering::Relaxed);
        stats.requests_this_second.fetch_add(1, Ordering::Relaxed);
        Ok((true, None))
    }

    async fn handle_data(
        &self,
        request_id: u64,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        let stats = Store::global().get_traffic_stats();
        match direction {
            DataDirection::Upstream => {
                stats
                    .bytes_out
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            DataDirection::Downstream => {
                stats
                    .bytes_in
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn handle_connect_close(
        &self,
        request_id: u64,
        _target: &str,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        let stats = Store::global().get_traffic_stats();
        stats.current_requests.fetch_sub(1, Ordering::Relaxed);
        Ok(true)
    }
}
