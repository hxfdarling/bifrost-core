use crate::plugin::net_storage::NetworkRecord;
use once_cell::sync::OnceCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::RwLock;

static GLOBAL_CONTEXT: OnceCell<Store> = OnceCell::new();
// 添加请求ID计数器
pub static REQUEST_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
#[derive(Debug)]
pub struct TrafficStatsData {
    pub bytes_in: AtomicU64,
    pub bytes_out: AtomicU64,
    pub last_bytes_in: AtomicU64,        // 新增：上一秒的入站字节数
    pub last_bytes_out: AtomicU64,       // 新增：上一秒的出站字节数
    pub current_speed_in: AtomicU64,     // 新增：当前入站速度
    pub current_speed_out: AtomicU64,    // 新增：当前出站速度
    pub last_update: AtomicU64,          // 新增：上次更新时间
    pub total_requests: AtomicU64,       // 累计请求数量
    pub current_requests: AtomicU64,     // 当前请求数量
    pub last_second: AtomicU64,          // 上一秒的时间戳
    pub requests_this_second: AtomicU64, // 当前秒内的请求数
    pub last_qps: AtomicU64,             // 上一次计算的 QPS
    pub max_speed_in: AtomicU64,         // 新增：历史最高入站速度
    pub max_speed_out: AtomicU64,        // 新增：历史最高出站速度
    pub max_qps: AtomicU64,              // 新增：峰值 QPS
}

impl TrafficStatsData {
    pub fn new() -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        Self {
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
            last_bytes_in: AtomicU64::new(0),
            last_bytes_out: AtomicU64::new(0),
            current_speed_in: AtomicU64::new(0),
            current_speed_out: AtomicU64::new(0),
            last_update: AtomicU64::new(now),
            total_requests: AtomicU64::new(0),
            current_requests: AtomicU64::new(0),
            last_second: AtomicU64::new(now),
            requests_this_second: AtomicU64::new(0),
            last_qps: AtomicU64::new(0),
            max_speed_in: AtomicU64::new(0),
            max_speed_out: AtomicU64::new(0),
            max_qps: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    config: Arc<RwLock<Config>>,
    network_records: Arc<RwLock<VecDeque<NetworkRecord>>>,
    traffic_stats: Arc<TrafficStatsData>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub enable_https: bool,
    pub enable_h2: bool,
    pub max_network_records: usize,
}

impl Store {
    pub fn init(
        port: u16,
        cert_path: PathBuf,
        key_path: PathBuf,
        enable_https: bool,
        enable_h2: bool,
        max_network_records: Option<usize>,
    ) {
        let _ = GLOBAL_CONTEXT.set(Self {
            config: Arc::new(RwLock::new(Config {
                port,
                cert_path,
                key_path,
                enable_https,
                enable_h2,
                max_network_records: max_network_records.unwrap_or(10000),
            })),
            network_records: Arc::new(RwLock::new(VecDeque::new())),
            traffic_stats: Arc::new(TrafficStatsData::new()),
        });
    }

    pub fn global() -> &'static Store {
        GLOBAL_CONTEXT.get().expect("Context not initialized")
    }

    pub async fn get_config(&self) -> Config {
        self.config.read().await.clone()
    }

    pub async fn add_network_record(&self, record: NetworkRecord) {
        let mut records = self.network_records.write().await;
        let max_records = self.config.read().await.max_network_records;

        if records.len() >= max_records {
            records.pop_front();
        }
        records.push_back(record);
    }

    pub async fn get_network_records(&self) -> Vec<NetworkRecord> {
        self.network_records.read().await.iter().cloned().collect()
    }

    pub async fn update_network_record_by_id<F>(
        &self,
        request_id: u64,
        update_fn: F,
    ) -> Result<(), &'static str>
    where
        F: FnOnce(&mut NetworkRecord),
    {
        let mut records = self.network_records.write().await;
        if let Some(record) = records.iter_mut().find(|r| r.id == request_id) {
            update_fn(record);
            Ok(())
        } else {
            Err("Record not found")
        }
    }

    pub fn get_traffic_stats(&self) -> Arc<TrafficStatsData> {
        self.traffic_stats.clone()
    }
}
