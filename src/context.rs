use crate::plugin::net_storage::NetworkRecord;
use once_cell::sync::OnceCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

static GLOBAL_CONTEXT: OnceCell<Context> = OnceCell::new();

#[derive(Clone, Debug)]
pub struct Context {
    config: Arc<RwLock<Config>>,
    network_records: Arc<RwLock<VecDeque<NetworkRecord>>>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub enable_https: bool,
    pub enable_h2: bool,
    pub h2_only: bool,
    pub max_network_records: usize,
}

impl Context {
    pub fn init(
        port: u16,
        cert_path: PathBuf,
        key_path: PathBuf,
        enable_https: bool,
        enable_h2: bool,
        h2_only: bool,
        max_network_records: Option<usize>,
    ) {
        let _ = GLOBAL_CONTEXT.set(Self {
            config: Arc::new(RwLock::new(Config {
                port,
                cert_path,
                key_path,
                enable_https,
                enable_h2,
                h2_only,
                max_network_records: max_network_records.unwrap_or(10000),
            })),
            network_records: Arc::new(RwLock::new(VecDeque::new())),
        });
    }

    pub fn global() -> &'static Context {
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
}
