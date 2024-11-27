use once_cell::sync::OnceCell;
use std::sync::Arc;
use tokio::sync::RwLock;

static GLOBAL_CONTEXT: OnceCell<Context> = OnceCell::new();

#[derive(Clone, Debug)]
pub struct Context {
    // 服务器配置信息
    config: Arc<RwLock<Config>>,
}

#[derive(Clone, Debug)]
pub struct Config {
    // 服务器端口
    pub port: u16,
}

impl Context {
    // 初始化全局 Context
    pub fn init(port: u16) {
        let _ = GLOBAL_CONTEXT.set(Self {
            config: Arc::new(RwLock::new(Config { port })),
        });
    }

    // 获取全局 Context 实例
    pub fn global() -> &'static Context {
        GLOBAL_CONTEXT.get().expect("Context not initialized")
    }

    // 获取端口号
    pub async fn get_port(&self) -> u16 {
        self.config.read().await.port
    }

    // 设置端口号
    pub async fn set_port(&self, port: u16) {
        self.config.write().await.port = port;
    }
}
