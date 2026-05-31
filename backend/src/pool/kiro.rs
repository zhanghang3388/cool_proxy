//! Kiro 账号池。镜像 `AccountPool` 的"DB 主 + 内存 ID 索引"模式，
//! 去掉 codex 专有的 model_states，保留 enabled / cooldown / 统计 / 代理绑定。
//! 本期只做账号池管理，pick 逻辑预留给后续反代 API 用。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use chrono::Utc;
use serde::Serialize;
use tracing::info;

use crate::auth::kiro::{derive_kiro_account_id, KiroTokenData};
use crate::config::Config;
use crate::store::kiro_accounts as store_kiro;
use crate::store::kiro_accounts::{KiroAccountRow, KiroQuotaUpdate, KiroTokenUpdate};
use crate::store::SqlitePool;

#[derive(Debug, thiserror::Error)]
pub enum KiroPoolError {
    #[error("no kiro accounts available")]
    Empty,
    #[error("all kiro accounts cooling down or disabled")]
    AllUnavailable,
}

/// 选中的账号（给后续反代用）。
#[derive(Debug, Clone)]
pub struct SelectedKiroAccount {
    pub id: String,
    pub access_token: String,
    pub profile_arn: String,
    pub auth_method: String,
    pub login_provider: Option<String>,
    pub proxy_url: String,
}

/// social 登录（Google/Github）兜底 profileArn。
const KIRO_SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";
/// Builder-ID / IdC 兜底 profileArn。
const KIRO_BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";

/// 解析账号最终用于上游的 profileArn：账号自带优先，否则按登录方式兜底。
pub fn resolve_profile_arn(acc: &KiroAccountRow) -> String {
    if let Some(arn) = acc.profile_arn.as_deref() {
        if !arn.trim().is_empty() {
            return arn.to_string();
        }
    }
    let provider = acc.login_provider.as_deref().unwrap_or("").to_ascii_lowercase();
    if provider == "github" || provider == "google" {
        KIRO_SOCIAL_PROFILE_ARN.to_string()
    } else if acc.auth_method.eq_ignore_ascii_case("idc") {
        KIRO_BUILDER_ID_PROFILE_ARN.to_string()
    } else {
        // 默认按 social 兜底（大多数导入账号是社交登录）
        KIRO_SOCIAL_PROFILE_ARN.to_string()
    }
}

pub struct KiroPool {
    db: SqlitePool,
    cfg: Arc<Config>,
    ids: RwLock<Vec<String>>,
    cursor: AtomicUsize,
}

impl KiroPool {
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
        let ids = store_kiro::all_ids_sorted(&self.db)?;
        let n = ids.len();
        *self.ids.write().unwrap() = ids;
        info!("kiro pool: {} account(s) indexed", n);
        Ok(n)
    }

    fn refresh_ids(&self) -> anyhow::Result<()> {
        let ids = store_kiro::all_ids_sorted(&self.db)?;
        *self.ids.write().unwrap() = ids;
        Ok(())
    }

    pub fn list_page(
        &self,
        limit: i64,
        offset: i64,
        q: Option<&str>,
    ) -> anyhow::Result<Vec<KiroAccountRow>> {
        store_kiro::list_page(&self.db, limit, offset, q)
    }

    pub fn count(&self, q: Option<&str>) -> anyhow::Result<i64> {
        store_kiro::count(&self.db, q)
    }

    pub fn get(&self, id: &str) -> Option<KiroAccountRow> {
        store_kiro::get(&self.db, id).ok().flatten()
    }

    pub fn all_ids_sorted(&self) -> Vec<String> {
        self.ids.read().unwrap().clone()
    }

    /// round-robin 选一个可用账号（账号级 cooldown / enabled 过滤）。给反代用。
    pub fn pick(&self) -> Result<SelectedKiroAccount, KiroPoolError> {
        let now = Utc::now();
        let ids = self.ids.read().unwrap();
        if ids.is_empty() {
            return Err(KiroPoolError::Empty);
        }
        let n = ids.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            let id = &ids[idx];
            let Some(a) = store_kiro::get(&self.db, id).ok().flatten() else {
                continue;
            };
            if !a.is_available(now) {
                continue;
            }
            let _ = store_kiro::mark_used(&self.db, id);
            let profile_arn = resolve_profile_arn(&a);
            return Ok(SelectedKiroAccount {
                id: a.id,
                access_token: a.access_token,
                profile_arn,
                auth_method: a.auth_method,
                login_provider: a.login_provider,
                proxy_url: a.proxy_url,
            });
        }
        Err(KiroPoolError::AllUnavailable)
    }

    /// 成功上报：清失败计数与冷却。
    pub fn report_success_for(&self, id: &str) {
        let _ = store_kiro::report_success(&self.db, id);
    }

    /// 失败上报：按配置决定是否冷却。返回是否进入了冷却（仅用于日志）。
    pub fn report_failure_for(&self, id: &str, msg: &str) {
        if self.cfg.retry.disable_cooldown {
            let _ = store_kiro::mark_refresh_failed(&self.db, id, msg);
            return;
        }
        let _ = store_kiro::report_failure(
            &self.db,
            id,
            msg,
            self.cfg.retry.cooldown_seconds as i64,
            self.cfg.retry.long_cooldown_seconds as i64,
            self.cfg.retry.failure_threshold,
        );
    }

    /// 标记账号被封禁（403 + SUSPENDED 之类），写 status + 禁用避免反复命中。
    pub fn mark_banned(&self, id: &str, reason: &str) {
        let _ = store_kiro::set_enabled(&self.db, id, false);
        let q = crate::store::kiro_accounts::KiroQuotaUpdate {
            status: Some(crate::auth::kiro::KIRO_STATUS_BANNED.to_string()),
            status_reason: Some(reason.to_string()),
            quota_error: Some(reason.to_string()),
            raw_usage: None,
            ..Default::default()
        };
        let _ = store_kiro::update_quota(&self.db, id, &q);
    }

    /// 导入 / 替换一个账号。自动派生稳定 id。
    pub fn add_or_replace(&self, data: &KiroTokenData) -> anyhow::Result<KiroAccountRow> {
        let id = derive_kiro_account_id(data);
        let row = KiroAccountRow::from_token_data(id.clone(), data);
        store_kiro::upsert(&self.db, &row)?;
        self.refresh_ids()?;
        store_kiro::get(&self.db, &id)?
            .ok_or_else(|| anyhow::anyhow!("kiro account vanished after upsert"))
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        store_kiro::set_enabled(&self.db, id, enabled).unwrap_or(false)
    }

    pub fn remove(&self, id: &str) -> Option<()> {
        let ok = store_kiro::delete(&self.db, id).unwrap_or(false);
        if ok {
            let _ = self.refresh_ids();
            Some(())
        } else {
            None
        }
    }

    pub fn set_proxy(&self, id: &str, proxy_url: String) -> anyhow::Result<()> {
        let proxy_url = crate::proxy_pool::validate_proxy_url(&proxy_url)?;
        if !store_kiro::set_proxy(&self.db, id, &proxy_url)? {
            anyhow::bail!("account not found");
        }
        Ok(())
    }

    pub fn reset_cooldown(&self, id: &str) -> bool {
        store_kiro::reset_cooldown(&self.db, id).unwrap_or(false)
    }

    pub fn report_success(&self, id: &str) {
        let _ = store_kiro::report_success(&self.db, id);
    }

    pub fn snapshot_for_refresh(&self, threshold_seconds: i64) -> Vec<KiroAccountRow> {
        store_kiro::snapshot_for_refresh(&self.db, threshold_seconds).unwrap_or_default()
    }

    pub fn update_after_refresh(&self, id: &str, u: &KiroTokenUpdate) {
        let _ = store_kiro::update_after_refresh(&self.db, id, u);
    }

    pub fn mark_refresh_failed(&self, id: &str, msg: &str) {
        let _ = store_kiro::mark_refresh_failed(&self.db, id, msg);
    }

    pub fn update_quota(&self, id: &str, q: &KiroQuotaUpdate) -> bool {
        store_kiro::update_quota(&self.db, id, q).unwrap_or(false)
    }

    pub fn update_quota_error(&self, id: &str, msg: &str) -> bool {
        store_kiro::update_quota_error(&self.db, id, msg).unwrap_or(false)
    }

    pub fn stats_overview(&self) -> anyhow::Result<KiroStatsCounts> {
        let (total, enabled, cooling, expired, total_req, total_fail) =
            store_kiro::stats_overview(&self.db)?;
        Ok(KiroStatsCounts {
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
pub struct KiroStatsCounts {
    pub total: usize,
    pub enabled: usize,
    pub cooling: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}
