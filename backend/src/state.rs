use std::sync::Arc;

use crate::auth::refresher::Refresher;
use crate::config::Config;
use crate::pool::AccountPool;
use crate::proxy::{ProxiedClients, RequestLog};
use crate::proxy_pool::{legacy_pool_path, ProxyPool};
use crate::store::{default_db_path, open as open_db, SqlitePool};

/// 全局共享状态。
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub pool: Arc<AccountPool>,
    pub proxy_pool: Arc<ProxyPool>,
    pub clients: Arc<ProxiedClients>,
    pub refresher: Arc<Refresher>,
    pub request_log: Arc<RequestLog>,
}

impl AppState {
    pub fn new(config: Arc<Config>) -> anyhow::Result<Self> {
        let db_path = default_db_path(&config.auth_dir);
        let db = open_db(&db_path)?;

        let pool = Arc::new(AccountPool::new(config.clone(), db.clone()));
        pool.load_from_disk()?;

        let proxy_pool = Arc::new(ProxyPool::new(db.clone()));
        proxy_pool.import_legacy_if_empty(&legacy_pool_path(&config.auth_dir))?;

        let clients = Arc::new(ProxiedClients::new());
        let refresher = Arc::new(Refresher::new(clients.clone()));
        let request_log = Arc::new(RequestLog::new(db.clone()));

        Ok(Self {
            config,
            db,
            pool,
            proxy_pool,
            clients,
            refresher,
            request_log,
        })
    }
}
