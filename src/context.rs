use once_cell::sync::OnceCell;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

static GLOBAL_CONTEXT: OnceCell<Context> = OnceCell::new();

#[derive(Clone, Debug)]
pub struct Context {
    config: Arc<RwLock<Config>>,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub enable_https: bool,
    pub enable_h2: bool,
    pub h2_only: bool,
}

impl Context {
    pub fn init(
        port: u16,
        cert_path: PathBuf,
        key_path: PathBuf,
        enable_https: bool,
        enable_h2: bool,
        h2_only: bool,
    ) {
        let _ = GLOBAL_CONTEXT.set(Self {
            config: Arc::new(RwLock::new(Config {
                port,
                cert_path,
                key_path,
                enable_https,
                enable_h2,
                h2_only,
            })),
        });
    }

    pub fn global() -> &'static Context {
        GLOBAL_CONTEXT.get().expect("Context not initialized")
    }

    pub async fn get_config(&self) -> Config {
        self.config.read().await.clone()
    }
}
