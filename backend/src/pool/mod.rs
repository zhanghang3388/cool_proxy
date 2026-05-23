use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use chrono::Utc;
use serde::Serialize;
use tracing::{info, warn};

use crate::auth::codex::{scan_codex_files, CodexTokenStorage};
use crate::config::Config;
use crate::store::accounts as store_accounts;
use crate::store::accounts::AccountRow;
use crate::store::SqlitePool;

/// 兼容老接口：之前 list 返回 CodexAccount，现在直接返回 AccountRow
/// （字段一一对应、token 字段被 serde(skip)，不会泄露给前端）。
pub use crate::store::accounts::AccountRow as CodexAccount;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no accounts available")]
    Empty,
    #[error("all accounts cooling down or disabled")]
    AllUnavailable,
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

#[derive(Debug, Clone)]
pub struct SelectedAccount {
    pub id: String,
    pub access_token: String,
    pub account_id: String,
    pub proxy_url: String,
}

/// "DB 主 + 内存索引"模式：DB 是真相源，内存只缓存 ID 列表用于 round-robin。
/// 任何对账号的状态变更都直接落 DB；ID 列表在 reload / upsert / delete 时增量维护。
pub struct AccountPool {
    db: SqlitePool,
    cfg: Arc<Config>,
    /// 全部账号 ID（按主键排序），用于 pick 的 round-robin 起点
    ids: RwLock<Vec<String>>,
    cursor: AtomicUsize,
}

impl AccountPool {
    pub fn new(cfg: Arc<Config>, db: SqlitePool) -> Self {
        Self {
            db,
            cfg,
            ids: RwLock::new(Vec::new()),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 启动时调用：刷新内存 ID 索引。第一次启动如果 DB 是空的，先从 auth_dir 一次性导入。
    pub fn load_from_disk(&self) -> anyhow::Result<usize> {
        // DB 空的才走文件迁移
        if store_accounts::all_ids_sorted(&self.db)?.is_empty() {
            self.import_legacy_files()?;
        }
        let ids = store_accounts::all_ids_sorted(&self.db)?;
        let n = ids.len();
        *self.ids.write().unwrap() = ids;
        info!("account pool: {} account(s) indexed", n);
        Ok(n)
    }

    /// 从 auth_dir 扫描 codex-*.json，导入到 DB。仅在 DB 为空时调用。
    fn import_legacy_files(&self) -> anyhow::Result<()> {
        let files = scan_codex_files(&self.cfg.auth_dir)?;
        if files.is_empty() {
            return Ok(());
        }
        info!("first run: importing {} legacy codex file(s)", files.len());
        for path in files {
            match CodexTokenStorage::load(&path) {
                Ok(storage) => {
                    let id = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    let row = AccountRow::from_storage(id, &storage);
                    if let Err(e) = store_accounts::upsert(&self.db, &row) {
                        warn!("import {:?} failed: {e:?}", path);
                    }
                }
                Err(e) => warn!("skip invalid token file {:?}: {e:?}", path),
            }
        }
        Ok(())
    }

    /// 刷新内存 ID 索引。新增/删除账号后调用。
    fn refresh_ids(&self) -> anyhow::Result<()> {
        let ids = store_accounts::all_ids_sorted(&self.db)?;
        *self.ids.write().unwrap() = ids;
        Ok(())
    }

    pub fn list_page(
        &self,
        limit: i64,
        offset: i64,
        q: Option<&str>,
    ) -> anyhow::Result<Vec<AccountRow>> {
        store_accounts::list_page(&self.db, limit, offset, q)
    }

    pub fn count(&self, q: Option<&str>) -> anyhow::Result<i64> {
        store_accounts::count(&self.db, q)
    }

    pub fn get(&self, id: &str) -> Option<AccountRow> {
        store_accounts::get(&self.db, id).ok().flatten()
    }

    /// 轮询挑一个可用账号。先从内存 ID 索引取候选，再按 ID 查 DB 行做可用性判断。
    /// 内存只读锁极快；写状态时单条 UPDATE，几千个号也不会有锁竞争。
    pub fn pick(&self) -> Result<SelectedAccount, PoolError> {
        let now = Utc::now();
        let ids = self.ids.read().unwrap();
        if ids.is_empty() {
            return Err(PoolError::Empty);
        }
        let n = ids.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            let id = &ids[idx];
            // 取一行做可用性检查（DB 是真相源）
            let Some(a) = store_accounts::get(&self.db, id).ok().flatten() else {
                continue;
            };
            if !a.is_available(now) {
                continue;
            }
            // 标记 last_used / total_requests 自增
            let _ = store_accounts::mark_used(&self.db, id);
            return Ok(SelectedAccount {
                id: a.id,
                access_token: a.access_token,
                account_id: a.account_id,
                proxy_url: a.proxy_url,
            });
        }
        Err(PoolError::AllUnavailable)
    }

    pub fn report_success(&self, id: &str) {
        let _ = store_accounts::report_success(&self.db, id);
    }

    pub fn report_failure(&self, id: &str, status: u16, msg: &str) {
        let line = format!("HTTP {status}: {msg}");
        let _ = store_accounts::report_failure(
            &self.db,
            id,
            &line,
            self.cfg.retry.cooldown_seconds as i64,
            self.cfg.retry.long_cooldown_seconds as i64,
            self.cfg.retry.failure_threshold,
        );
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        store_accounts::set_enabled(&self.db, id, enabled).unwrap_or(false)
    }

    pub fn remove(&self, id: &str) -> Option<()> {
        let ok = store_accounts::delete(&self.db, id).unwrap_or(false);
        if ok {
            let _ = self.refresh_ids();
            Some(())
        } else {
            None
        }
    }

    pub fn add_or_replace_from_storage(
        &self,
        id: String,
        storage: &CodexTokenStorage,
    ) -> anyhow::Result<AccountRow> {
        let row = AccountRow::from_storage(id.clone(), storage);
        store_accounts::upsert(&self.db, &row)?;
        self.refresh_ids()?;
        Ok(store_accounts::get(&self.db, &id)?
            .ok_or_else(|| anyhow::anyhow!("account vanished after upsert"))?)
    }

    pub fn set_proxy(&self, id: &str, proxy_url: String) -> anyhow::Result<()> {
        let proxy_url = crate::proxy_pool::validate_proxy_url(&proxy_url)?;
        if !store_accounts::set_proxy(&self.db, id, &proxy_url)? {
            anyhow::bail!("account not found");
        }
        Ok(())
    }

    pub fn unassigned_ids(&self) -> Vec<String> {
        store_accounts::unassigned_ids(&self.db).unwrap_or_default()
    }

    pub fn all_ids_sorted(&self) -> Vec<String> {
        self.ids.read().unwrap().clone()
    }

    pub fn snapshot_for_refresh(
        &self,
        threshold_seconds: i64,
    ) -> Vec<(String, CodexTokenStorage, PathBuf, String)> {
        // PathBuf 字段保留是为了不动 refresher 接口。"DB 主"模式下不需要文件路径，
        // 这里返回一个空 PathBuf 占位，refresher 不再写文件。
        store_accounts::snapshot_for_refresh(&self.db, threshold_seconds)
            .unwrap_or_default()
            .into_iter()
            .map(|(id, st, proxy)| (id, st, PathBuf::new(), proxy))
            .collect()
    }

    pub fn update_after_refresh(&self, id: &str, storage: &CodexTokenStorage) {
        let _ = store_accounts::update_after_refresh(&self.db, id, storage);
    }

    pub fn mark_refresh_failed(&self, id: &str, msg: &str) {
        let _ = store_accounts::mark_refresh_failed(&self.db, id, msg);
    }

    pub fn reset_cooldown(&self, id: &str) -> bool {
        store_accounts::reset_cooldown(&self.db, id).unwrap_or(false)
    }

    pub fn stats_overview(&self) -> anyhow::Result<StatsCounts> {
        let (total, enabled, cooling, expired, total_req, total_fail) =
            store_accounts::stats_overview(&self.db)?;
        Ok(StatsCounts {
            total,
            enabled,
            cooling,
            expired,
            total_requests: total_req,
            total_failures: total_fail,
        })
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }
}

#[derive(Debug, Serialize)]
pub struct StatsCounts {
    pub total: usize,
    pub enabled: usize,
    pub cooling: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}

/// 帮助函数：根据 storage 推导一个稳定的 account id（用作 DB 主键、也用作导出文件名）。
pub fn derive_account_id(storage: &CodexTokenStorage) -> String {
    let email = storage.email.trim();
    let plan = storage
        .extra
        .get("plan_type")
        .or_else(|| storage.extra.get("plan"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let safe_email = if email.is_empty() {
        let mut h = sha2::Sha256::default();
        use sha2::Digest;
        h.update(storage.account_id.as_bytes());
        h.update(storage.access_token.as_bytes());
        let digest = h.finalize();
        format!("acc-{}", &hex::encode(digest)[..12])
    } else {
        email.replace('/', "_")
    };

    if plan.is_empty() {
        format!("codex-{safe_email}")
    } else {
        format!("codex-{safe_email}-{plan}")
    }
}

/// 兼容老导出名（带 .json 后缀），导出到文件时用。
pub fn derive_file_name(storage: &CodexTokenStorage) -> String {
    format!("{}.json", derive_account_id(storage))
}
