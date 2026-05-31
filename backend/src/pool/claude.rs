//! Claude 账号池。镜像 `KiroPool` 的"DB 主 + 内存 ID 索引 + round-robin"模式。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use chrono::Utc;
use serde::Serialize;
use tracing::info;

use crate::auth::claude::{derive_claude_account_id, ClaudeTokenData};
use crate::config::Config;
use crate::store::claude_accounts as store_claude;
use crate::store::claude_accounts::{ClaudeAccountRow, ClaudeTokenUpdate};
use crate::store::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum ClaudePoolError {
    #[error("no claude accounts available")]
    Empty,
    #[error("all claude accounts cooling down or disabled")]
    AllUnavailable,
}

/// 选中的账号（给反代用）。
#[derive(Debug, Clone)]
pub struct SelectedClaudeAccount {
    pub id: String,
    pub access_token: String,
    pub proxy_url: String,
}

pub struct ClaudePool {
    db: SqlitePool,
    cfg: Arc<Config>,
    ids: RwLock<Vec<String>>,
    cursor: AtomicUsize,
}

impl ClaudePool {
    pub fn new(cfg: Arc<Config>, db: SqlitePool) -> Self {
        Self {
            db,
            cfg,
            ids: RwLock::new(Vec::new()),
            cursor: AtomicUsize::new(0),
        }
    }

    /// 启动时刷新内存 ID 索引。
    pub fn load(&self) -> anyhow::Result<usize> {
        let ids = store_claude::all_ids_sorted(&self.db)?;
        let n = ids.len();
        *self.ids.write().unwrap() = ids;
        info!("claude pool: {} account(s) indexed", n);
        Ok(n)
    }

    fn refresh_ids(&self) -> anyhow::Result<()> {
        let ids = store_claude::all_ids_sorted(&self.db)?;
        *self.ids.write().unwrap() = ids;
        Ok(())
    }

    pub fn list_page(
        &self,
        limit: i64,
        offset: i64,
        q: Option<&str>,
    ) -> anyhow::Result<Vec<ClaudeAccountRow>> {
        store_claude::list_page(&self.db, limit, offset, q)
    }

    pub fn count(&self, q: Option<&str>) -> anyhow::Result<i64> {
        store_claude::count(&self.db, q)
    }

    pub fn get(&self, id: &str) -> Option<ClaudeAccountRow> {
        store_claude::get(&self.db, id).ok().flatten()
    }

    pub fn all_ids_sorted(&self) -> Vec<String> {
        self.ids.read().unwrap().clone()
    }

    /// round-robin 选一个可用账号（账号级 cooldown / enabled 过滤）。
    pub fn pick(&self) -> Result<SelectedClaudeAccount, ClaudePoolError> {
        let now = Utc::now();
        let ids = self.ids.read().unwrap();
        if ids.is_empty() {
            return Err(ClaudePoolError::Empty);
        }
        let n = ids.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            let id = &ids[idx];
            let Some(a) = store_claude::get(&self.db, id).ok().flatten() else {
                continue;
            };
            if !a.is_available(now) {
                continue;
            }
            let _ = store_claude::mark_used(&self.db, id);
            return Ok(SelectedClaudeAccount {
                id: a.id,
                access_token: a.access_token,
                proxy_url: a.proxy_url,
            });
        }
        Err(ClaudePoolError::AllUnavailable)
    }

    pub fn report_success_for(&self, id: &str) {
        let _ = store_claude::report_success(&self.db, id);
    }

    /// 失败上报：按配置决定是否冷却。
    pub fn report_failure_for(&self, id: &str, msg: &str) {
        if self.cfg.retry.disable_cooldown {
            let _ = store_claude::mark_refresh_failed(&self.db, id, msg);
            return;
        }
        let _ = store_claude::report_failure(
            &self.db,
            id,
            msg,
            self.cfg.retry.cooldown_seconds as i64,
            self.cfg.retry.long_cooldown_seconds as i64,
            self.cfg.retry.failure_threshold,
        );
    }

    /// 登录 / 替换一个账号。自动派生稳定 id。
    pub fn add_or_replace(&self, data: &ClaudeTokenData) -> anyhow::Result<ClaudeAccountRow> {
        let id = derive_claude_account_id(data);
        let row = ClaudeAccountRow::from_token_data(id.clone(), data);
        store_claude::upsert(&self.db, &row)?;
        self.refresh_ids()?;
        store_claude::get(&self.db, &id)?
            .ok_or_else(|| anyhow::anyhow!("claude account vanished after upsert"))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        store_claude::set_enabled(&self.db, id, enabled).unwrap_or(false)
    }

    pub fn remove(&self, id: &str) -> Option<()> {
        let ok = store_claude::delete(&self.db, id).unwrap_or(false);
        if ok {
            let _ = self.refresh_ids();
            Some(())
        } else {
            None
        }
    }

    pub fn set_proxy(&self, id: &str, proxy_url: String) -> anyhow::Result<()> {
        let proxy_url = crate::proxy_pool::validate_proxy_url(&proxy_url)?;
        if !store_claude::set_proxy(&self.db, id, &proxy_url)? {
            anyhow::bail!("account not found");
        }
        Ok(())
    }

    pub fn reset_cooldown(&self, id: &str) -> bool {
        store_claude::reset_cooldown(&self.db, id).unwrap_or(false)
    }

    pub fn report_success(&self, id: &str) {
        let _ = store_claude::report_success(&self.db, id);
    }

    pub fn snapshot_for_refresh(&self, threshold_seconds: i64) -> Vec<ClaudeAccountRow> {
        store_claude::snapshot_for_refresh(&self.db, threshold_seconds).unwrap_or_default()
    }

    pub fn update_after_refresh(&self, id: &str, u: &ClaudeTokenUpdate) {
        let _ = store_claude::update_after_refresh(&self.db, id, u);
    }

    pub fn mark_refresh_failed(&self, id: &str, msg: &str) {
        let _ = store_claude::mark_refresh_failed(&self.db, id, msg);
    }

    pub fn stats_overview(&self) -> anyhow::Result<ClaudeStatsCounts> {
        let (total, enabled, cooling, expired, total_req, total_fail) =
            store_claude::stats_overview(&self.db)?;
        Ok(ClaudeStatsCounts {
            total,
            enabled,
            cooling,
            expired,
            total_requests: total_req,
            total_failures: total_fail,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct ClaudeStatsCounts {
    pub total: usize,
    pub enabled: usize,
    pub cooling: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}
