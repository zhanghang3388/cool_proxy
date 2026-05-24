use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::{dt_to_ms, ms_to_dt, SqlitePool};
use crate::auth::CodexTokenStorage;

/// DB 里一行账号的完整结构。和原来的 CodexAccount 字段一一对应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountRow {
    pub id: String,
    pub email: String,
    pub account_id: String,
    pub plan: Option<String>,
    pub enabled: bool,
    #[serde(skip)]
    pub access_token: String,
    #[serde(skip)]
    pub refresh_token: String,
    #[serde(skip)]
    pub id_token: String,
    pub expire_at: Option<DateTime<Utc>>,
    pub last_refresh_at: Option<DateTime<Utc>>,
    pub failure_count: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub proxy_url: String,
    pub quota_5h_used_percent: Option<f64>,
    pub quota_5h_reset_at: Option<DateTime<Utc>>,
    pub quota_week_used_percent: Option<f64>,
    pub quota_week_reset_at: Option<DateTime<Utc>>,
    pub quota_checked_at: Option<DateTime<Utc>>,
    pub quota_error: Option<String>,
    #[serde(skip)]
    pub raw_extra: serde_json::Map<String, serde_json::Value>,
}

impl AccountRow {
    pub fn from_storage(id: String, storage: &CodexTokenStorage) -> Self {
        let plan = storage
            .extra
            .get("plan_type")
            .or_else(|| storage.extra.get("plan"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Self {
            id,
            email: storage.email.clone(),
            account_id: storage.account_id.clone(),
            plan,
            enabled: true,
            access_token: storage.access_token.clone(),
            refresh_token: storage.refresh_token.clone(),
            id_token: storage.id_token.clone(),
            expire_at: storage.expire_at(),
            last_refresh_at: DateTime::parse_from_rfc3339(&storage.last_refresh)
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            failure_count: 0,
            cooldown_until: None,
            last_error: None,
            last_used_at: None,
            total_requests: 0,
            total_failures: 0,
            proxy_url: storage.proxy_url.clone(),
            quota_5h_used_percent: None,
            quota_5h_reset_at: None,
            quota_week_used_percent: None,
            quota_week_reset_at: None,
            quota_checked_at: None,
            quota_error: None,
            raw_extra: storage.extra.clone(),
        }
    }

    pub fn to_storage(&self) -> CodexTokenStorage {
        CodexTokenStorage {
            id_token: self.id_token.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            account_id: self.account_id.clone(),
            last_refresh: self
                .last_refresh_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            email: self.email.clone(),
            kind: "codex".to_string(),
            expire: self.expire_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
            proxy_url: self.proxy_url.clone(),
            extra: self.raw_extra.clone(),
        }
    }

    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(c) = self.cooldown_until {
            if c > now {
                return false;
            }
        }
        if self.access_token.is_empty() {
            return false;
        }
        true
    }
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
    let raw_extra_str: String = r.get("raw_extra")?;
    let raw_extra: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&raw_extra_str).unwrap_or_default();
    Ok(AccountRow {
        id: r.get("id")?,
        email: r.get("email")?,
        account_id: r.get("account_id")?,
        plan: r.get("plan")?,
        enabled: r.get::<_, i64>("enabled")? != 0,
        access_token: r.get("access_token")?,
        refresh_token: r.get("refresh_token")?,
        id_token: r.get("id_token")?,
        expire_at: ms_to_dt(r.get("expire_at")?),
        last_refresh_at: ms_to_dt(r.get("last_refresh_at")?),
        failure_count: r.get::<_, i64>("failure_count")? as u32,
        cooldown_until: ms_to_dt(r.get("cooldown_until")?),
        last_error: r.get("last_error")?,
        last_used_at: ms_to_dt(r.get("last_used_at")?),
        total_requests: r.get::<_, i64>("total_requests")? as u64,
        total_failures: r.get::<_, i64>("total_failures")? as u64,
        proxy_url: r.get("proxy_url")?,
        quota_5h_used_percent: r.get("quota_5h_used_percent")?,
        quota_5h_reset_at: ms_to_dt(r.get("quota_5h_reset_at")?),
        quota_week_used_percent: r.get("quota_week_used_percent")?,
        quota_week_reset_at: ms_to_dt(r.get("quota_week_reset_at")?),
        quota_checked_at: ms_to_dt(r.get("quota_checked_at")?),
        quota_error: r.get("quota_error")?,
        raw_extra,
    })
}

#[derive(Debug, Clone)]
pub struct AccountQuotaUpdate {
    pub quota_5h_used_percent: Option<f64>,
    pub quota_5h_reset_at: Option<DateTime<Utc>>,
    pub quota_week_used_percent: Option<f64>,
    pub quota_week_reset_at: Option<DateTime<Utc>>,
    pub quota_error: Option<String>,
}

/// 全字段 upsert（导入 / 上传 / 替换都用它）。
pub fn upsert(pool: &SqlitePool, a: &AccountRow) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO accounts(
            id,email,account_id,plan,enabled,access_token,refresh_token,id_token,
            expire_at,last_refresh_at,failure_count,cooldown_until,last_error,
            last_used_at,total_requests,total_failures,proxy_url,raw_extra
         ) VALUES (
            ?1,?2,?3,?4,?5,?6,?7,?8,
            ?9,?10,?11,?12,?13,
            ?14,?15,?16,?17,?18
         )
         ON CONFLICT(id) DO UPDATE SET
            email = excluded.email,
            account_id = excluded.account_id,
            plan = excluded.plan,
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            id_token = excluded.id_token,
            expire_at = excluded.expire_at,
            last_refresh_at = excluded.last_refresh_at,
            -- 保留运行时状态：enabled/failure_count/cooldown_until/last_used_at/total_*/last_error
            -- proxy_url：仅当传入非空才覆盖（保留旧绑定）
            proxy_url = CASE WHEN excluded.proxy_url <> '' THEN excluded.proxy_url ELSE accounts.proxy_url END,
            raw_extra = excluded.raw_extra
        ",
        params![
            a.id,
            a.email,
            a.account_id,
            a.plan,
            a.enabled as i64,
            a.access_token,
            a.refresh_token,
            a.id_token,
            dt_to_ms(a.expire_at),
            dt_to_ms(a.last_refresh_at),
            a.failure_count as i64,
            dt_to_ms(a.cooldown_until),
            a.last_error,
            dt_to_ms(a.last_used_at),
            a.total_requests as i64,
            a.total_failures as i64,
            a.proxy_url,
            serde_json::to_string(&a.raw_extra).unwrap_or_else(|_| "{}".into()),
        ],
    )?;
    Ok(())
}

pub fn get(pool: &SqlitePool, id: &str) -> Result<Option<AccountRow>> {
    let conn = pool.get()?;
    let row = conn
        .query_row(
            "SELECT * FROM accounts WHERE id = ?1",
            params![id],
            row_to_account,
        )
        .optional()?;
    Ok(row)
}

pub fn count(pool: &SqlitePool, q: Option<&str>) -> Result<i64> {
    let conn = pool.get()?;
    let n: i64 = if let Some(qs) = q {
        let like = format!("%{}%", qs);
        conn.query_row(
            "SELECT COUNT(*) FROM accounts WHERE email LIKE ?1 OR id LIKE ?1",
            params![like],
            |r| r.get(0),
        )?
    } else {
        conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?
    };
    Ok(n)
}

/// 分页 + 可选搜索。返回的 access_token / refresh_token / id_token 字段被 serde 跳过，
/// 调用方可以放心整体序列化给前端。
pub fn list_page(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
    q: Option<&str>,
) -> Result<Vec<AccountRow>> {
    let conn = pool.get()?;
    let mut rows: Vec<AccountRow> = Vec::new();
    if let Some(qs) = q {
        let like = format!("%{}%", qs);
        let mut stmt = conn.prepare(
            "SELECT * FROM accounts WHERE email LIKE ?1 OR id LIKE ?1
             ORDER BY id LIMIT ?2 OFFSET ?3",
        )?;
        let it = stmt.query_map(params![like, limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    } else {
        let mut stmt = conn.prepare("SELECT * FROM accounts ORDER BY id LIMIT ?1 OFFSET ?2")?;
        let it = stmt.query_map(params![limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    }
    Ok(rows)
}

/// 全表 ID 列表，按主键排序。给 pool 内存索引用，几千个 ID 是 KB 级，没压力。
pub fn all_ids_sorted(pool: &SqlitePool) -> Result<Vec<String>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT id FROM accounts ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

pub fn unassigned_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT id FROM accounts WHERE proxy_url = '' ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

pub fn delete(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn set_enabled(pool: &SqlitePool, id: &str, enabled: bool) -> Result<bool> {
    let conn = pool.get()?;
    let n = if enabled {
        conn.execute(
            "UPDATE accounts SET enabled = 1, failure_count = 0, cooldown_until = NULL WHERE id = ?1",
            params![id],
        )?
    } else {
        conn.execute("UPDATE accounts SET enabled = 0 WHERE id = ?1", params![id])?
    };
    Ok(n > 0)
}

pub fn set_proxy(pool: &SqlitePool, id: &str, proxy_url: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE accounts SET proxy_url = ?2 WHERE id = ?1",
        params![id, proxy_url],
    )?;
    Ok(n > 0)
}

pub fn reset_cooldown(pool: &SqlitePool, id: &str) -> Result<bool> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    let n = tx.execute(
        "UPDATE accounts SET failure_count = 0, cooldown_until = NULL, last_error = NULL WHERE id = ?1",
        params![id],
    )?;
    tx.execute(
        "DELETE FROM account_model_states WHERE account_id = ?1",
        params![id],
    )?;
    tx.commit()?;
    Ok(n > 0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStateRow {
    pub model_key: String,
    pub next_retry_after: Option<DateTime<Utc>>,
    pub quota_backoff_lv: i64,
    pub transient_fails: i64,
    pub last_status: Option<i64>,
    pub last_error: Option<String>,
    pub last_kind: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
}

fn row_to_model_state(r: &rusqlite::Row<'_>) -> rusqlite::Result<ModelStateRow> {
    Ok(ModelStateRow {
        model_key: r.get("model_key")?,
        next_retry_after: ms_to_dt(r.get("next_retry_after")?),
        quota_backoff_lv: r.get("quota_backoff_lv")?,
        transient_fails: r.get("transient_fails")?,
        last_status: r.get("last_status")?,
        last_error: r.get("last_error")?,
        last_kind: r.get("last_kind")?,
        updated_at: ms_to_dt(r.get("updated_at")?),
    })
}

pub fn get_model_state(
    pool: &SqlitePool,
    account_id: &str,
    model_key: &str,
) -> Result<Option<ModelStateRow>> {
    let conn = pool.get()?;
    Ok(conn
        .query_row(
            "SELECT * FROM account_model_states WHERE account_id = ?1 AND model_key = ?2",
            params![account_id, model_key],
            row_to_model_state,
        )
        .optional()?)
}

pub fn list_model_states(pool: &SqlitePool, account_id: &str) -> Result<Vec<ModelStateRow>> {
    let conn = pool.get()?;
    let mut stmt = conn
        .prepare("SELECT * FROM account_model_states WHERE account_id = ?1 ORDER BY model_key")?;
    let it = stmt.query_map(params![account_id], row_to_model_state)?;
    Ok(it.filter_map(|r| r.ok()).collect())
}

/// 候选过滤用：返回所有"当前还在冷却"的 (account_id, model_key) 对。
/// 调用方在 pick_for 阶段把对应账号过滤掉。返回的 model_key 含空串，表示账号级冷却。
pub fn currently_cooling(pool: &SqlitePool, now_ms: i64) -> Result<Vec<(String, String)>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT account_id, model_key FROM account_model_states
         WHERE next_retry_after IS NOT NULL AND next_retry_after > ?1",
    )?;
    let it = stmt.query_map(params![now_ms], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    Ok(it.filter_map(|r| r.ok()).collect())
}

/// distinct 账号数：当前至少有一个 model_state 还在冷却的账号。
pub fn cooling_account_count(pool: &SqlitePool, now_ms: i64) -> Result<i64> {
    let conn = pool.get()?;
    let n: i64 = conn.query_row(
        "SELECT COUNT(DISTINCT account_id) FROM account_model_states
         WHERE next_retry_after IS NOT NULL AND next_retry_after > ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    Ok(n)
}

#[allow(clippy::too_many_arguments)]
pub fn upsert_model_state(
    pool: &SqlitePool,
    account_id: &str,
    model_key: &str,
    next_retry_after_ms: Option<i64>,
    quota_backoff_lv: i64,
    transient_fails: i64,
    last_status: Option<i64>,
    last_error: Option<&str>,
    last_kind: Option<&str>,
) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO account_model_states(
            account_id, model_key, next_retry_after, quota_backoff_lv, transient_fails,
            last_status, last_error, last_kind, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(account_id, model_key) DO UPDATE SET
            next_retry_after = excluded.next_retry_after,
            quota_backoff_lv = excluded.quota_backoff_lv,
            transient_fails  = excluded.transient_fails,
            last_status      = excluded.last_status,
            last_error       = excluded.last_error,
            last_kind        = excluded.last_kind,
            updated_at       = excluded.updated_at",
        params![
            account_id,
            model_key,
            next_retry_after_ms,
            quota_backoff_lv,
            transient_fails,
            last_status,
            last_error,
            last_kind,
            now,
        ],
    )?;
    Ok(())
}

pub fn clear_model_state(pool: &SqlitePool, account_id: &str, model_key: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "DELETE FROM account_model_states WHERE account_id = ?1 AND model_key = ?2",
        params![account_id, model_key],
    )?;
    Ok(())
}

/// pick 用：原子 update 一行的 last_used_at + total_requests，并取出选中的 access/account/proxy。
pub fn mark_used(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE accounts SET last_used_at = ?2, total_requests = total_requests + 1 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn report_success(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE accounts SET failure_count = 0, cooldown_until = NULL, last_error = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

#[allow(dead_code)]
pub fn report_failure(
    pool: &SqlitePool,
    id: &str,
    msg: &str,
    cooldown_short_secs: i64,
    cooldown_long_secs: i64,
    failure_threshold: u32,
) -> Result<()> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    // 读当前 failure_count + 1
    let cur: Option<(i64, i64)> = conn
        .query_row(
            "SELECT failure_count, total_failures FROM accounts WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((failure_count, total_failures)) = cur else {
        return Ok(());
    };
    let new_fail = failure_count.saturating_add(1);
    let cooldown_secs = if (new_fail as u32) >= failure_threshold {
        cooldown_long_secs
    } else {
        cooldown_short_secs
    };
    let cooldown_until = now_ms + cooldown_secs * 1000;
    conn.execute(
        "UPDATE accounts SET
            failure_count = ?2,
            total_failures = ?3,
            last_error = ?4,
            cooldown_until = ?5
         WHERE id = ?1",
        params![
            id,
            new_fail,
            total_failures.saturating_add(1),
            msg,
            cooldown_until
        ],
    )?;
    Ok(())
}

pub fn update_after_refresh(
    pool: &SqlitePool,
    id: &str,
    storage: &CodexTokenStorage,
) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE accounts SET
            access_token = ?2,
            refresh_token = ?3,
            id_token = CASE WHEN ?4 <> '' THEN ?4 ELSE id_token END,
            expire_at = ?5,
            last_refresh_at = ?6,
            raw_extra = ?7
         WHERE id = ?1",
        params![
            id,
            storage.access_token,
            storage.refresh_token,
            storage.id_token,
            dt_to_ms(storage.expire_at()),
            now,
            serde_json::to_string(&storage.extra).unwrap_or_else(|_| "{}".into()),
        ],
    )?;
    Ok(())
}

pub fn mark_refresh_failed(pool: &SqlitePool, id: &str, msg: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE accounts SET last_error = ?2 WHERE id = ?1",
        params![id, format!("refresh failed: {msg}")],
    )?;
    Ok(())
}

pub fn update_quota(pool: &SqlitePool, id: &str, q: &AccountQuotaUpdate) -> Result<bool> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    let n = conn.execute(
        "UPDATE accounts SET
            quota_5h_used_percent = ?2,
            quota_5h_reset_at = ?3,
            quota_week_used_percent = ?4,
            quota_week_reset_at = ?5,
            quota_checked_at = ?6,
            quota_error = ?7
         WHERE id = ?1",
        params![
            id,
            q.quota_5h_used_percent,
            dt_to_ms(q.quota_5h_reset_at),
            q.quota_week_used_percent,
            dt_to_ms(q.quota_week_reset_at),
            now,
            q.quota_error,
        ],
    )?;
    Ok(n > 0)
}

pub fn update_quota_error(pool: &SqlitePool, id: &str, msg: &str) -> Result<bool> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    let n = conn.execute(
        "UPDATE accounts SET quota_checked_at = ?2, quota_error = ?3 WHERE id = ?1",
        params![id, now, msg],
    )?;
    Ok(n > 0)
}

/// 直接把 reason 写到 last_error，不加任何前缀。给 disable_account 等明确事件用。
pub fn set_last_error(pool: &SqlitePool, id: &str, msg: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE accounts SET last_error = ?2 WHERE id = ?1",
        params![id, msg],
    )?;
    Ok(())
}

/// 给后台 refresher 用：所有 enabled 且 refresh_token 非空、且 (无 expire_at 或将在 threshold 秒内过期) 的账号。
pub fn snapshot_for_refresh(
    pool: &SqlitePool,
    threshold_seconds: i64,
) -> Result<Vec<(String, CodexTokenStorage, String)>> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let cutoff = now_ms + threshold_seconds * 1000;
    let mut stmt = conn.prepare(
        "SELECT * FROM accounts
         WHERE enabled = 1 AND refresh_token <> ''
           AND (expire_at IS NULL OR expire_at <= ?1)",
    )?;
    let it = stmt.query_map(params![cutoff], row_to_account)?;
    let mut out = Vec::new();
    for r in it {
        let a = r?;
        let storage = a.to_storage();
        out.push((a.id, storage, a.proxy_url));
    }
    Ok(out)
}

/// 全表统计（给 stats 接口 + cooling 计数）。
pub fn stats_overview(pool: &SqlitePool) -> Result<(usize, usize, usize, usize, u64, u64)> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM accounts", [], |r| r.get(0))?;
    let enabled: i64 =
        conn.query_row("SELECT COUNT(*) FROM accounts WHERE enabled = 1", [], |r| {
            r.get(0)
        })?;
    let cooling: i64 = conn.query_row(
        "SELECT COUNT(*) FROM accounts WHERE cooldown_until IS NOT NULL AND cooldown_until > ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let expired: i64 = conn.query_row(
        "SELECT COUNT(*) FROM accounts WHERE expire_at IS NULL OR expire_at <= ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let (sum_req, sum_fail): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(total_requests),0), COALESCE(SUM(total_failures),0) FROM accounts",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((
        total as usize,
        enabled as usize,
        cooling as usize,
        expired as usize,
        sum_req as u64,
        sum_fail as u64,
    ))
}
