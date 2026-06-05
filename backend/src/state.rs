use std::sync::Arc;

use crate::auth::claude::ClaudeLoginStore;
use crate::auth::claude_refresh::ClaudeRefresher;
use crate::auth::kiro_refresh::KiroRefresher;
use crate::auth::kiro_sso::KiroSsoLoginStore;
use crate::auth::refresher::Refresher;
use crate::config::Config;
use crate::pool::claude::ClaudePool;
use crate::pool::kiro::KiroPool;
use crate::pool::AccountPool;
use crate::proxy::{ProxiedClients, RequestLog};
use crate::proxy_pool::{legacy_pool_path, ProxyPool};
use crate::store::{default_db_path, open as open_db, SqlitePool};

/// 全局共享状态。
pub struct AppState {
    pub config: Arc<Config>,
    pub db: SqlitePool,
    pub pool: Arc<AccountPool>,
    pub kiro_pool: Arc<KiroPool>,
    pub claude_pool: Arc<ClaudePool>,
    pub proxy_pool: Arc<ProxyPool>,
    pub clients: Arc<ProxiedClients>,
    pub refresher: Arc<Refresher>,
    pub kiro_refresher: Arc<KiroRefresher>,
    pub claude_refresher: Arc<ClaudeRefresher>,
    pub claude_login: Arc<ClaudeLoginStore>,
    pub kiro_login: Arc<KiroSsoLoginStore>,
    pub claude_models_cache: Arc<crate::proxy_claude::ModelsCache>,
    pub request_log: Arc<RequestLog>,
}

impl AppState {
    pub fn new(config: Arc<Config>) -> anyhow::Result<Self> {
        let db_path = default_db_path(&config.auth_dir);
        let db = open_db(&db_path)?;

        let pool = Arc::new(AccountPool::new(config.clone(), db.clone()));
        pool.load_from_disk()?;

        let kiro_pool = Arc::new(KiroPool::new(config.clone(), db.clone()));
        kiro_pool.load()?;

        let claude_pool = Arc::new(ClaudePool::new(config.clone(), db.clone()));
        claude_pool.load()?;

        let proxy_pool = Arc::new(ProxyPool::new(db.clone()));
        proxy_pool.import_legacy_if_empty(&legacy_pool_path(&config.auth_dir))?;

        let clients = Arc::new(ProxiedClients::new());
        let refresher = Arc::new(Refresher::new(clients.clone()));
        let kiro_refresher = Arc::new(KiroRefresher::new(clients.clone()));
        let claude_refresher = Arc::new(ClaudeRefresher::new(clients.clone()));
        let claude_login = Arc::new(ClaudeLoginStore::new());
        let kiro_login = Arc::new(KiroSsoLoginStore::new());
        let claude_models_cache = Arc::new(crate::proxy_claude::ModelsCache::new());
        let request_log = Arc::new(RequestLog::new(db.clone()));

        Ok(Self {
            config,
            db,
            pool,
            kiro_pool,
            claude_pool,
            proxy_pool,
            clients,
            refresher,
            kiro_refresher,
            claude_refresher,
            claude_login,
            kiro_login,
            claude_models_cache,
            request_log,
        })
    }
}
