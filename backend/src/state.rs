use std::path::PathBuf;
use std::sync::Arc;

use crate::auth::refresher::Refresher;
use crate::config::Config;
use crate::pool::AccountPool;
use crate::proxy::{ProxiedClients, RequestLog};
use crate::proxy_pool::{default_pool_path, ProxyPool};

/// 全局共享状态——通过 axum 的 State 注入到 handler。
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: Arc<AccountPool>,
    pub proxy_pool: Arc<ProxyPool>,
    pub clients: Arc<ProxiedClients>,
    pub refresher: Arc<Refresher>,
    pub request_log: Arc<RequestLog>,
}

impl AppState {
    pub fn new(config: Arc<Config>, pool: Arc<AccountPool>) -> anyhow::Result<Self> {
        let proxy_pool_path: PathBuf = default_pool_path(&config.auth_dir);
        let proxy_pool = Arc::new(ProxyPool::load(proxy_pool_path)?);
        let clients = Arc::new(ProxiedClients::new());
        let refresher = Arc::new(Refresher::new(clients.clone()));
        let request_log = Arc::new(RequestLog::new(500));
        Ok(Self {
            config,
            pool,
            proxy_pool,
            clients,
            refresher,
            request_log,
        })
    }
}
