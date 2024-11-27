use crate::plugin::DataDirection;
use crate::plugin::Plugin;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response};
use std::error::Error;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct TrafficStats {
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    last_bytes_in: AtomicU64,        // 新增：上一秒的入站字节数
    last_bytes_out: AtomicU64,       // 新增：上一秒的出站字节数
    current_speed_in: AtomicU64,     // 新增：当前入站速度
    current_speed_out: AtomicU64,    // 新增：当前出站速度
    last_update: AtomicU64,          // 新增：上次更新时间
    total_requests: AtomicU64,       // 累计请求数量
    current_requests: AtomicU64,     // 当前请求数量
    last_second: AtomicU64,          // 上一秒的时间戳
    requests_this_second: AtomicU64, // 当前秒内的请求数
    last_qps: AtomicU64,             // 上一次计算的 QPS
    max_speed_in: AtomicU64,         // 新增：历史最高入站速度
    max_speed_out: AtomicU64,        // 新增：历史最高出站速度
    max_qps: AtomicU64,              // 新增：峰值 QPS
}

impl TrafficStats {
    pub fn new() -> Self {
        Self {
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            last_bytes_in: AtomicU64::new(0),
            last_bytes_out: AtomicU64::new(0),
            current_speed_in: AtomicU64::new(0),
            current_speed_out: AtomicU64::new(0),
            last_update: AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
            total_requests: AtomicU64::new(0),
            current_requests: AtomicU64::new(0),
            last_second: AtomicU64::new(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
            requests_this_second: AtomicU64::new(0),
            last_qps: AtomicU64::new(0),
            max_speed_in: AtomicU64::new(0),
            max_speed_out: AtomicU64::new(0),
            max_qps: AtomicU64::new(0), // 初始化峰值 QPS
        }
    }

    pub fn update_speeds(&self) {
        let current_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let last_time = self.last_update.load(Ordering::Relaxed);
        let time_diff = current_time - last_time;

        if time_diff >= 1 {
            let total_in = self.bytes_in.load(Ordering::Relaxed);
            let total_out = self.bytes_out.load(Ordering::Relaxed);
            let last_in = self.last_bytes_in.load(Ordering::Relaxed);
            let last_out = self.last_bytes_out.load(Ordering::Relaxed);

            // 计算速度（字节/秒）
            let speed_in = (total_in - last_in) / time_diff;
            let speed_out = (total_out - last_out) / time_diff;

            // 更新当前速度
            self.current_speed_in.store(speed_in, Ordering::Relaxed);
            self.current_speed_out.store(speed_out, Ordering::Relaxed);

            // 更新最高速度
            loop {
                let current_max_in = self.max_speed_in.load(Ordering::Relaxed);
                if speed_in <= current_max_in
                    || self
                        .max_speed_in
                        .compare_exchange(
                            current_max_in,
                            speed_in,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    break;
                }
            }

            loop {
                let current_max_out = self.max_speed_out.load(Ordering::Relaxed);
                if speed_out <= current_max_out
                    || self
                        .max_speed_out
                        .compare_exchange(
                            current_max_out,
                            speed_out,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    break;
                }
            }

            // 更新其他状态
            self.last_bytes_in.store(total_in, Ordering::Relaxed);
            self.last_bytes_out.store(total_out, Ordering::Relaxed);
            self.last_update.store(current_time, Ordering::Relaxed);
        }
    }

    pub fn get_stats(&self) -> (u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) {
        self.update_speeds();
        let current_second = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let last_second = self.last_second.load(Ordering::Relaxed);

        if current_second > last_second {
            let current_qps = self.requests_this_second.load(Ordering::Relaxed);
            self.last_qps.store(current_qps, Ordering::Relaxed);

            // 更新峰值 QPS
            loop {
                let current_max_qps = self.max_qps.load(Ordering::Relaxed);
                if current_qps <= current_max_qps
                    || self
                        .max_qps
                        .compare_exchange(
                            current_max_qps,
                            current_qps,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                {
                    break;
                }
            }

            self.requests_this_second.store(0, Ordering::Relaxed);
            self.last_second.store(current_second, Ordering::Relaxed);
        }

        (
            self.bytes_in.load(Ordering::Relaxed),
            self.bytes_out.load(Ordering::Relaxed),
            self.total_requests.load(Ordering::Relaxed),
            self.current_requests.load(Ordering::Relaxed),
            self.last_qps.load(Ordering::Relaxed),
            self.current_speed_in.load(Ordering::Relaxed),
            self.current_speed_out.load(Ordering::Relaxed),
            self.max_speed_in.load(Ordering::Relaxed),
            self.max_speed_out.load(Ordering::Relaxed),
            self.max_qps.load(Ordering::Relaxed), // 返回峰值 QPS
        )
    }
}

#[async_trait]
impl Plugin for TrafficStats {
    async fn handle_request(
        &self,
        _req: &mut Request<Incoming>,
    ) -> Result<(bool, Option<Response<BoxBody<Bytes, hyper::Error>>>), Box<dyn Error + Send + Sync>>
    {
        // 统计 HTTP 请求
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.current_requests.fetch_add(1, Ordering::Relaxed);
        self.requests_this_second.fetch_add(1, Ordering::Relaxed);
        Ok((true, None)) // 返回 true 和 None，表示继续处理
    }

    async fn handle_response(
        &self,
        _resp: &mut Response<Incoming>,
    ) -> Result<bool, Box<dyn Error + Send + Sync>> {
        // 统计 HTTP 响应
        self.current_requests.fetch_sub(1, Ordering::Relaxed);
        Ok(true)
    }

    async fn handle_connect(&self, _target: &str) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 统计隧道代理连接
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.current_requests.fetch_add(1, Ordering::Relaxed);
        self.requests_this_second.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn handle_data(
        &self,
        direction: DataDirection,
        data: &[u8],
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        match direction {
            DataDirection::Upstream => {
                self.bytes_out
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
            DataDirection::Downstream => {
                self.bytes_in
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn handle_connect_close(
        &self,
        _target: &str,
    ) -> Result<(), Box<dyn Error + Send + Sync>> {
        // 统计隧道代理连接关闭
        self.current_requests.fetch_sub(1, Ordering::Relaxed);
        Ok(())
    }
}
