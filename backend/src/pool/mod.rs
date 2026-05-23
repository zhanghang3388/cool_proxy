use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use tracing::{info, warn};

use crate::auth::codex::{scan_codex_files, CodexTokenStorage};
use crate::config::Config;
use crate::proxy::{classify, quota_backoff, ErrorKind};
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

/// 错误分类化上报的入参。`status = None` 表示请求层失败（reqwest::Error）。
#[derive(Debug, Clone)]
pub struct ReportContext<'a> {
    pub id: &'a str,
    pub model: &'a str,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    pub message: &'a str,
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

    /// 兼容老调用方：等价于 `pick_for("")`。新代码请用 `pick_for(model)`。
    #[allow(dead_code)]
    pub fn pick(&self) -> Result<SelectedAccount, PoolError> {
        self.pick_for("")
    }

    /// `pick_for(model)`：在内存 round-robin 的基础上，过滤掉
    ///   - 该 (account, model) 处在冷却的；
    ///   - 该 account 的全局冷却（model_key="" 行）；
    ///   - 老的 accounts.cooldown_until（兼容字段，仍然尊重）。
    /// model 传 "" 等价于老 `pick`，仅看账号级状态。
    pub fn pick_for(&self, model: &str) -> Result<SelectedAccount, PoolError> {
        let now = Utc::now();
        let now_ms = now.timestamp_millis();
        let ids = self.ids.read().unwrap();
        if ids.is_empty() {
            return Err(PoolError::Empty);
        }

        // 当前正在冷却的 (account_id, model_key)；用 set 做 O(1) 判定
        let cooling = store_accounts::currently_cooling(&self.db, now_ms).unwrap_or_default();
        let mut blocked_global: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut blocked_model: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (aid, mk) in cooling.iter() {
            if mk.is_empty() {
                blocked_global.insert(aid.as_str());
            } else if mk == model && !model.is_empty() {
                blocked_model.insert(aid.as_str());
            }
        }

        let n = ids.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            let id = &ids[idx];
            if blocked_global.contains(id.as_str()) || blocked_model.contains(id.as_str()) {
                continue;
            }
            let Some(a) = store_accounts::get(&self.db, id).ok().flatten() else {
                continue;
            };
            if !a.is_available(now) {
                continue;
            }
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
        // 也清掉 model 状态表里这个号的"全局冷却"行，避免老数据残留
        let _ = store_accounts::clear_model_state(&self.db, id, "");
    }

    /// 单个 (account, model) 维度的成功上报：清掉该 model 状态 + 全局 transient 行。
    pub fn report_success_for(&self, id: &str, model: &str) {
        let _ = store_accounts::report_success(&self.db, id);
        let _ = store_accounts::clear_model_state(&self.db, id, "");
        if !model.is_empty() {
            let _ = store_accounts::clear_model_state(&self.db, id, model);
        }
    }

    #[allow(dead_code)]
    pub fn report_failure(&self, id: &str, status: u16, msg: &str) {
        // 兼容旧调用路径（如果还有人调）。如果开了 disable_cooldown 就只记错误。
        if self.cfg.retry.disable_cooldown {
            let _ = store_accounts::mark_refresh_failed(&self.db, id, msg);
            return;
        }
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

    /// 错误分类化上报。返回这次错误的分类，调用方根据 kind 决定要不要 spawn refresh / 是否继续重试。
    pub fn report(&self, ctx: ReportContext<'_>) -> ErrorKind {
        let kind = classify(ctx.status);
        let now_ms = Utc::now().timestamp_millis();
        let disable = self.cfg.retry.disable_cooldown;
        let status_i = ctx.status.map(|c| c as i64);
        let label = kind.label();
        let model_for_state = match kind {
            // 网络错误归账号级（model_key = ""），其他写到具体 model 上
            ErrorKind::Network => "",
            _ => ctx.model,
        };

        match kind {
            ErrorKind::Auth => {
                // 401/402/403：不写 next_retry_after。只记录最近错误，等 refresh_one 来兜底。
                let _ = store_accounts::upsert_model_state(
                    &self.db,
                    ctx.id,
                    model_for_state,
                    None,
                    0,
                    0,
                    status_i,
                    Some(ctx.message),
                    Some(label),
                );
            }
            ErrorKind::NotFound => {
                let next = if disable {
                    None
                } else {
                    Some(now_ms + 12 * 60 * 60 * 1000)
                };
                let _ = store_accounts::upsert_model_state(
                    &self.db,
                    ctx.id,
                    model_for_state,
                    next,
                    0,
                    0,
                    status_i,
                    Some(ctx.message),
                    Some(label),
                );
            }
            ErrorKind::Quota => {
                if disable {
                    let _ = store_accounts::upsert_model_state(
                        &self.db,
                        ctx.id,
                        model_for_state,
                        None,
                        0,
                        0,
                        status_i,
                        Some(ctx.message),
                        Some(label),
                    );
                } else {
                    let prev_lv = store_accounts::get_model_state(&self.db, ctx.id, model_for_state)
                        .ok()
                        .flatten()
                        .map(|s| s.quota_backoff_lv)
                        .unwrap_or(0);
                    let (cooldown, next_lv) = match ctx.retry_after {
                        Some(d) if !d.is_zero() => (d, prev_lv),
                        _ => quota_backoff(prev_lv),
                    };
                    let next = now_ms + cooldown.as_millis() as i64;
                    let _ = store_accounts::upsert_model_state(
                        &self.db,
                        ctx.id,
                        model_for_state,
                        Some(next),
                        next_lv,
                        0,
                        status_i,
                        Some(ctx.message),
                        Some(label),
                    );
                }
            }
            ErrorKind::Transient | ErrorKind::Network => {
                // 累加 transient_fails；满阈值才写 next_retry_after
                let prev = store_accounts::get_model_state(&self.db, ctx.id, model_for_state)
                    .ok()
                    .flatten();
                let new_fails = prev.as_ref().map(|s| s.transient_fails).unwrap_or(0) + 1;
                let threshold = self.cfg.retry.transient_threshold.max(1) as i64;
                let next = if !disable && new_fails >= threshold {
                    Some(now_ms + (self.cfg.retry.cooldown_seconds as i64) * 1000)
                } else {
                    prev.and_then(|s| s.next_retry_after)
                        .map(|t| t.timestamp_millis())
                };
                let _ = store_accounts::upsert_model_state(
                    &self.db,
                    ctx.id,
                    model_for_state,
                    next,
                    0,
                    new_fails,
                    status_i,
                    Some(ctx.message),
                    Some(label),
                );
            }
            ErrorKind::Client => {
                // 客户端错误：不冷却、不计失败，仅记最近错误
                let _ = store_accounts::upsert_model_state(
                    &self.db,
                    ctx.id,
                    model_for_state,
                    None,
                    0,
                    0,
                    status_i,
                    Some(ctx.message),
                    Some(label),
                );
            }
        }
        kind
    }

    /// refresh 失败后的兜底：把账号级 (model_key="") 行写一个 30min 冷却，
    /// 避免后续每次请求都触发重复 refresh 风暴。
    pub fn mark_auth_dead(&self, id: &str, msg: &str) {
        if self.cfg.retry.disable_cooldown {
            let _ = store_accounts::mark_refresh_failed(&self.db, id, msg);
            return;
        }
        let now_ms = Utc::now().timestamp_millis();
        let next = now_ms + 30 * 60 * 1000;
        let _ = store_accounts::upsert_model_state(
            &self.db,
            id,
            "",
            Some(next),
            0,
            0,
            Some(401),
            Some(msg),
            Some("auth_dead"),
        );
        let _ = store_accounts::mark_refresh_failed(&self.db, id, msg);
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

    pub fn list_model_states(&self, id: &str) -> Vec<store_accounts::ModelStateRow> {
        store_accounts::list_model_states(&self.db, id).unwrap_or_default()
    }

    pub fn cooling_account_count(&self) -> i64 {
        let now_ms = Utc::now().timestamp_millis();
        store_accounts::cooling_account_count(&self.db, now_ms).unwrap_or(0)
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
