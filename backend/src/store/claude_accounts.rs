//! Claude（Anthropic OAuth / Claude Code）账号的 SQLite 存取层。
//! 镜像 `store/kiro_accounts.rs` 的"DB 主"模式，去掉 Kiro 的额度 / profileArn / IdC 字段，
//! 字段集对应 Anthropic OAuth 登录态（access_token / refresh_token / expires_at + 组织信息）。

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{dt_to_ms, ms_to_dt, SqlitePool};
use crate::auth::claude::ClaudeTokenData;

/// DB 里一行 Claude 账号。token 字段 serde(skip)，可整体序列化给前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeAccountRow {
    pub id: String,
    pub email: String,
    pub org_name: Option<String>,
    pub enabled: bool,
    #[serde(skip)]
    pub access_token: String,
    #[serde(skip)]
    pub refresh_token: String,
    pub expires_at: Option<DateTime<Utc>>,

    pub last_refresh_at: Option<DateTime<Utc>>,
    pub failure_count: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub proxy_url: String,

    #[serde(skip)]
    pub raw_auth_token: Value,
}

impl ClaudeAccountRow {
    /// 从解析好的 token 数据建一行（首次登录 / 替换用）。运行时状态归零。
    pub fn from_token_data(id: String, data: &ClaudeTokenData) -> Self {
        Self {
            id,
            email: data.email.clone(),
            org_name: data.org_name.clone(),
            enabled: true,
            access_token: data.access_token.clone(),
            refresh_token: data.refresh_token.clone().unwrap_or_default(),
            expires_at: data.expires_at,
            last_refresh_at: None,
            failure_count: 0,
            cooldown_until: None,
            last_error: None,
            last_used_at: None,
            total_requests: 0,
            total_failures: 0,
            proxy_url: String::new(),
            raw_auth_token: data.raw_auth_token.clone(),
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

fn json_from_str(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClaudeAccountRow> {
    let raw_auth_token: String = r.get("raw_auth_token")?;
    Ok(ClaudeAccountRow {
        id: r.get("id")?,
        email: r.get("email")?,
        org_name: r.get("org_name")?,
        enabled: r.get::<_, i64>("enabled")? != 0,
        access_token: r.get("access_token")?,
        refresh_token: r.get("refresh_token")?,
        expires_at: ms_to_dt(r.get("expires_at")?),
        last_refresh_at: ms_to_dt(r.get("last_refresh_at")?),
        failure_count: r.get::<_, i64>("failure_count")? as u32,
        cooldown_until: ms_to_dt(r.get("cooldown_until")?),
        last_error: r.get("last_error")?,
        last_used_at: ms_to_dt(r.get("last_used_at")?),
        total_requests: r.get::<_, i64>("total_requests")? as u64,
        total_failures: r.get::<_, i64>("total_failures")? as u64,
        proxy_url: r.get("proxy_url")?,
        raw_auth_token: json_from_str(&raw_auth_token),
    })
}

/// 刷新后写回的 token 字段集合。
#[derive(Debug, Clone)]
pub struct ClaudeTokenUpdate {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub raw_auth_token: Value,
}

/// 全字段 upsert。运行时状态（enabled/failure/cooldown/统计/proxy）在冲突时保留旧值。
pub fn upsert(pool: &SqlitePool, a: &ClaudeAccountRow) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO claude_accounts(
            id,email,org_name,enabled,
            access_token,refresh_token,expires_at,
            last_refresh_at,failure_count,cooldown_until,last_error,
            last_used_at,total_requests,total_failures,proxy_url,
            raw_auth_token
         ) VALUES (
            ?1,?2,?3,?4,
            ?5,?6,?7,
            ?8,?9,?10,?11,
            ?12,?13,?14,?15,
            ?16
         )
         ON CONFLICT(id) DO UPDATE SET
            email = excluded.email,
            org_name = excluded.org_name,
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            expires_at = excluded.expires_at,
            last_refresh_at = excluded.last_refresh_at,
            -- proxy_url：仅当传入非空才覆盖（保留旧绑定）
            proxy_url = CASE WHEN excluded.proxy_url <> '' THEN excluded.proxy_url ELSE claude_accounts.proxy_url END,
            raw_auth_token = excluded.raw_auth_token
        ",
        params![
            a.id,
            a.email,
            a.org_name,
            a.enabled as i64,
            a.access_token,
            a.refresh_token,
            dt_to_ms(a.expires_at),
            dt_to_ms(a.last_refresh_at),
            a.failure_count as i64,
            dt_to_ms(a.cooldown_until),
            a.last_error,
            dt_to_ms(a.last_used_at),
            a.total_requests as i64,
            a.total_failures as i64,
            a.proxy_url,
            serde_json::to_string(&a.raw_auth_token).unwrap_or_else(|_| "{}".into()),
        ],
    )?;
    Ok(())
}

pub fn get(pool: &SqlitePool, id: &str) -> Result<Option<ClaudeAccountRow>> {
    let conn = pool.get()?;
    Ok(conn
        .query_row(
            "SELECT * FROM claude_accounts WHERE id = ?1",
            params![id],
            row_to_account,
        )
        .optional()?)
}

pub fn count(pool: &SqlitePool, q: Option<&str>) -> Result<i64> {
    let conn = pool.get()?;
    let n: i64 = if let Some(qs) = q {
        let like = format!("%{}%", qs);
        conn.query_row(
            "SELECT COUNT(*) FROM claude_accounts WHERE email LIKE ?1 OR id LIKE ?1",
            params![like],
            |r| r.get(0),
        )?
    } else {
        conn.query_row("SELECT COUNT(*) FROM claude_accounts", [], |r| r.get(0))?
    };
    Ok(n)
}

pub fn list_page(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
    q: Option<&str>,
) -> Result<Vec<ClaudeAccountRow>> {
    let conn = pool.get()?;
    let mut rows: Vec<ClaudeAccountRow> = Vec::new();
    if let Some(qs) = q {
        let like = format!("%{}%", qs);
        let mut stmt = conn.prepare(
            "SELECT * FROM claude_accounts WHERE email LIKE ?1 OR id LIKE ?1
             ORDER BY id LIMIT ?2 OFFSET ?3",
        )?;
        let it = stmt.query_map(params![like, limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    } else {
        let mut stmt =
            conn.prepare("SELECT * FROM claude_accounts ORDER BY id LIMIT ?1 OFFSET ?2")?;
        let it = stmt.query_map(params![limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    }
    Ok(rows)
}

pub fn all_ids_sorted(pool: &SqlitePool) -> Result<Vec<String>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT id FROM claude_accounts ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

/// 当前没绑代理（proxy_url = ''）的账号 id。给"仅未分配"的重新分配用。
pub fn unassigned_ids(pool: &SqlitePool) -> Result<Vec<String>> {
    let conn = pool.get()?;
    let mut stmt =
        conn.prepare("SELECT id FROM claude_accounts WHERE proxy_url = '' ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

pub fn delete(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute("DELETE FROM claude_accounts WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn set_enabled(pool: &SqlitePool, id: &str, enabled: bool) -> Result<bool> {
    let conn = pool.get()?;
    let n = if enabled {
        conn.execute(
            "UPDATE claude_accounts SET enabled = 1, failure_count = 0, cooldown_until = NULL WHERE id = ?1",
            params![id],
        )?
    } else {
        conn.execute(
            "UPDATE claude_accounts SET enabled = 0 WHERE id = ?1",
            params![id],
        )?
    };
    Ok(n > 0)
}

pub fn set_proxy(pool: &SqlitePool, id: &str, proxy_url: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE claude_accounts SET proxy_url = ?2 WHERE id = ?1",
        params![id, proxy_url],
    )?;
    Ok(n > 0)
}

pub fn reset_cooldown(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE claude_accounts SET failure_count = 0, cooldown_until = NULL, last_error = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(n > 0)
}

pub fn mark_used(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE claude_accounts SET last_used_at = ?2, total_requests = total_requests + 1 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn report_success(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE claude_accounts SET failure_count = 0, cooldown_until = NULL, last_error = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

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
    let cur: Option<(i64, i64)> = conn
        .query_row(
            "SELECT failure_count, total_failures FROM claude_accounts WHERE id = ?1",
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
        "UPDATE claude_accounts SET
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

pub fn update_after_refresh(pool: &SqlitePool, id: &str, u: &ClaudeTokenUpdate) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE claude_accounts SET
            access_token = ?2,
            refresh_token = CASE WHEN ?3 <> '' THEN ?3 ELSE refresh_token END,
            expires_at = ?4,
            last_refresh_at = ?5,
            raw_auth_token = ?6
         WHERE id = ?1",
        params![
            id,
            u.access_token,
            u.refresh_token.clone().unwrap_or_default(),
            dt_to_ms(u.expires_at),
            now,
            serde_json::to_string(&u.raw_auth_token).unwrap_or_else(|_| "{}".into()),
        ],
    )?;
    Ok(())
}

pub fn mark_refresh_failed(pool: &SqlitePool, id: &str, msg: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE claude_accounts SET last_error = ?2 WHERE id = ?1",
        params![id, format!("refresh failed: {msg}")],
    )?;
    Ok(())
}

/// 后台 refresher 用：enabled 且 refresh_token 非空、且即将（threshold 秒内）过期的账号。
/// 未知过期时间（expires_at IS NULL）的账号按 last_refresh_at 节流，避免每轮都刷。
pub fn snapshot_for_refresh(
    pool: &SqlitePool,
    threshold_seconds: i64,
) -> Result<Vec<ClaudeAccountRow>> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let cutoff = now_ms + threshold_seconds * 1000;
    let stale_before = now_ms - threshold_seconds * 1000;
    let mut stmt = conn.prepare(
        "SELECT * FROM claude_accounts
         WHERE enabled = 1 AND refresh_token <> ''
           AND (
             expires_at <= ?1
             OR (expires_at IS NULL AND (last_refresh_at IS NULL OR last_refresh_at <= ?2))
           )",
    )?;
    let it = stmt.query_map(params![cutoff, stale_before], row_to_account)?;
    let mut out = Vec::new();
    for r in it {
        out.push(r?);
    }
    Ok(out)
}

/// 全表统计。返回 (total, enabled, cooling, expired, sum_req, sum_fail)。
pub fn stats_overview(pool: &SqlitePool) -> Result<(usize, usize, usize, usize, u64, u64)> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM claude_accounts", [], |r| r.get(0))?;
    let enabled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM claude_accounts WHERE enabled = 1",
        [],
        |r| r.get(0),
    )?;
    let cooling: i64 = conn.query_row(
        "SELECT COUNT(*) FROM claude_accounts WHERE cooldown_until IS NOT NULL AND cooldown_until > ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let expired: i64 = conn.query_row(
        "SELECT COUNT(*) FROM claude_accounts WHERE expires_at IS NULL OR expires_at <= ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let (sum_req, sum_fail): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(total_requests),0), COALESCE(SUM(total_failures),0) FROM claude_accounts",
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
