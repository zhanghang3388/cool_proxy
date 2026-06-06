//! Kiro 账号池。镜像 `AccountPool` 的"DB 主 + 内存 ID 索引"模式。
//!
//! 与 KAM 一致点：
//!  - `pick` 选号时跳过 banned/invalid/capped 状态（is_available 已统一逻辑）。
//!  - SelectedKiroAccount 带 provider/machine_id —— 反代调用 generateAssistantResponse
//!    时按 provider 决定 user-agent + agent-mode（IdC = vibe，social = spec）。
//!  - resolve_profile_arn：social/BuilderId 用真实可用的默认 ARN，Enterprise 不发 ARN。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;

use chrono::Utc;
use serde::Serialize;
use tracing::info;

use crate::auth::kiro::{
    derive_kiro_account_id, KiroTokenData, KIRO_PROVIDER_BUILDER_ID, KIRO_PROVIDER_ENTERPRISE,
    KIRO_PROVIDER_GITHUB, KIRO_PROVIDER_GOOGLE,
};
use crate::auth::kiro_quota::{KIRO_BUILDER_ID_PROFILE_ARN, KIRO_SOCIAL_PROFILE_ARN};
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
    /// 发往上游的 profile_arn。Enterprise 为 None；其它为已规整的真实 ARN。
    pub profile_arn: Option<String>,
    pub provider: String,
    pub auth_method: String,
    pub machine_id: Option<String>,
    pub proxy_url: String,
}

/// 解析 generateAssistantResponse 调用要发的 profile_arn。
///
///  - **Google / Github（social）**：账号自带 ARN > Social 默认 ARN。永远要发。
///  - **BuilderId（IdC）**：账号自带 ARN > BuilderId 默认 ARN。永远要发（这是真实可用值）。
///  - **Enterprise（IdC）**：返回 None —— Enterprise 账号的 profile 由上游按 token 推断，
///    传别的 ARN 会让 token 被判 invalid。
pub fn resolve_profile_arn_for_upstream(acc: &KiroAccountRow) -> Option<String> {
    let account_arn = acc
        .profile_arn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match acc.provider.as_str() {
        KIRO_PROVIDER_GOOGLE | KIRO_PROVIDER_GITHUB => Some(
            account_arn
                .map(str::to_string)
                .unwrap_or_else(|| KIRO_SOCIAL_PROFILE_ARN.to_string()),
        ),
        KIRO_PROVIDER_BUILDER_ID => Some(
            account_arn
                .map(str::to_string)
                .unwrap_or_else(|| KIRO_BUILDER_ID_PROFILE_ARN.to_string()),
        ),
        KIRO_PROVIDER_ENTERPRISE => None,
        // 兜底：旧账号没 provider 时按 social
        _ => Some(
            account_arn
                .map(str::to_string)
                .unwrap_or_else(|| KIRO_SOCIAL_PROFILE_ARN.to_string()),
        ),
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

    /// 启动时刷新内存 ID 索引；同时一次性回填旧库的 provider 字段。
    pub fn load(&self) -> anyhow::Result<usize> {
        match store_kiro::backfill_provider(&self.db) {
            Ok(n) if n > 0 => info!("kiro pool: backfilled provider for {n} legacy account(s)"),
            Ok(_) => {}
            Err(e) => tracing::warn!("kiro provider backfill failed: {e:?}"),
        }
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

    /// round-robin 选一个可用账号。给反代用。
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
            let profile_arn = resolve_profile_arn_for_upstream(&a);
            return Ok(SelectedKiroAccount {
                id: a.id,
                access_token: a.access_token,
                profile_arn,
                provider: a.provider,
                auth_method: a.auth_method,
                machine_id: a.machine_id,
                proxy_url: a.proxy_url,
            });
        }
        Err(KiroPoolError::AllUnavailable)
    }

    /// 成功上报：清失败计数与冷却。
    pub fn report_success_for(&self, id: &str) {
        let _ = store_kiro::report_success(&self.db, id);
    }

    /// 失败上报：按配置决定是否冷却。
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

    /// 标记账号被封禁（403/423 + SUSPENDED 之类）。
    pub fn mark_banned(&self, id: &str, reason: &str) {
        let q = KiroQuotaUpdate {
            status: Some(crate::auth::kiro::KIRO_STATUS_BANNED.to_string()),
            status_reason: Some(reason.to_string()),
            quota_error: Some(reason.to_string()),
            raw_usage: None,
            usage_data: None,
            ..Default::default()
        };
        let _ = store_kiro::update_quota(&self.db, id, &q);
        // update_quota 已经按 status='banned' 自动禁用账号 —— 不再额外调 set_enabled。
    }

    /// 标记 token invalid（refresh_token 失效，无法自救）。
    pub fn mark_token_invalid(&self, id: &str, reason: &str) {
        let _ = store_kiro::mark_token_invalid(&self.db, id, reason);
    }

    /// 导入 / 替换一个账号。自动派生稳定 id；缺 machine_id 时生成 uuid。
    pub fn add_or_replace(&self, data: &KiroTokenData) -> anyhow::Result<KiroAccountRow> {
        let id = derive_kiro_account_id(data);
        let mut row = KiroAccountRow::from_token_data(id.clone(), data);
        // 没绑过 machine_id 就生成一个稳定 uuid，与 KAM 行为一致。
        if row.machine_id.as_deref().map(str::is_empty).unwrap_or(true) {
            row.machine_id = Some(uuid::Uuid::new_v4().to_string().to_lowercase());
        }
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
