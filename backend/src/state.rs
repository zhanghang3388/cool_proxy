use std::sync::Arc;

use crate::auth::claude::ClaudeLoginStore;
use crate::auth::claude_refresh::ClaudeRefresher;
use crate::auth::kiro_refresh::KiroRefresher;
use crate::auth::kiro_sso::KiroSsoLoginStore;
use crate::auth::refresher::Refresher;
use crate::config::{Config, KiroConfig};
use crate::pool::claude::ClaudePool;
use crate::pool::kiro::KiroPool;
use crate::pool::AccountPool;
use crate::proxy::{ProxiedClients, RequestLog};
use crate::proxy_pool::{legacy_pool_path, ProxyPool};
use crate::store::{default_db_path, open as open_db, Kv, SqlitePool};

/// 运行期可改的 kiro 配置在 kv 表里的持久化 key。
pub const KIRO_CFG_KEY: &str = "kiro_config_overrides";

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
    /// kiro 反代合成 prompt-cache 计费的前缀指纹表（带 TTL）。
    pub kiro_prompt_cache: Arc<crate::proxy_kiro::cache_synth::PromptCacheStore>,
    /// 运行期可改的 kiro 配置：启动时用 config.yaml 初始化、再被 DB 持久化值覆盖；
    /// 前端 /config/kiro 改它即时生效（无需重启）。
    pub kiro_runtime: Arc<std::sync::RwLock<KiroConfig>>,
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
        let kiro_prompt_cache =
            Arc::new(crate::proxy_kiro::cache_synth::PromptCacheStore::default());

        // 运行期 kiro 配置：先取 config.yaml 值，再用 DB 里持久化的覆盖（解析失败仅 warn）。
        let kiro_runtime = Arc::new(std::sync::RwLock::new(load_runtime_kiro(&db, &config.kiro)));

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
            kiro_prompt_cache,
            kiro_runtime,
        })
    }

    /// 取当前运行期 kiro 配置的快照（克隆，避免把锁守卫带进请求处理）。KiroConfig 很小。
    pub fn kiro_cfg(&self) -> KiroConfig {
        self.kiro_runtime.read().unwrap().clone()
    }
}

/// 启动时加载运行期 kiro 配置：DB 有持久化覆盖且能解析则用它，否则回退 config.yaml 值。
fn load_runtime_kiro(db: &SqlitePool, file_default: &KiroConfig) -> KiroConfig {
    let conn = match db.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("kiro 运行期配置：取 DB 连接失败，用 config.yaml 值: {e}");
            return file_default.clone();
        }
    };
    match Kv::get(&conn, KIRO_CFG_KEY) {
        Ok(Some(json)) => match serde_json::from_str::<KiroConfig>(&json) {
            Ok(cfg) => {
                tracing::info!("kiro 运行期配置：已从 DB 加载持久化覆盖");
                cfg
            }
            Err(e) => {
                tracing::warn!("kiro 运行期配置：DB 值解析失败，用 config.yaml 值: {e}");
                file_default.clone()
            }
        },
        Ok(None) => file_default.clone(),
        Err(e) => {
            tracing::warn!("kiro 运行期配置：读 DB 失败，用 config.yaml 值: {e}");
            file_default.clone()
        }
    }
}
