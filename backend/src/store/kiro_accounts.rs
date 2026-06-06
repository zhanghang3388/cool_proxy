//! Kiro 账号的 SQLite 存取层。结构对齐 `D:/java_projects/kiro-account-manager` 的
//! `Account` 模型：
//!  - 显式 provider（Google / Github / BuilderId / Enterprise）+ auth_method 派生；
//!  - 显式 start_url / client_id_hash（IdC 切号 / 文件名定位必需）；
//!  - id_token / sso_session_id / machine_id 显式存储；
//!  - usage_data 与 raw_auth_token 都落库，用于派生 capped/overage/封禁状态。

use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{dt_to_ms, ms_to_dt, SqlitePool};
use crate::auth::kiro::{auth_method_for_provider, KiroTokenData};

/// DB 里一行 Kiro 账号。token 字段 serde(skip)，可整体序列化给前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroAccountRow {
    pub id: String,
    pub email: String,
    pub user_id: Option<String>,
    /// 显式 provider：Google / Github / BuilderId / Enterprise。
    pub provider: String,
    /// social 或 IdC（与 provider 派生）。
    pub auth_method: String,
    /// 兼容字段：旧版本读取 login_provider 的页面。统一与 provider 同值。
    pub login_provider: Option<String>,
    pub enabled: bool,
    #[serde(skip)]
    pub access_token: String,
    #[serde(skip)]
    pub refresh_token: String,
    pub token_type: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,

    #[serde(skip)]
    pub idc_region: Option<String>,
    #[serde(skip)]
    pub issuer_url: Option<String>,
    #[serde(skip)]
    pub client_id: Option<String>,
    #[serde(skip)]
    pub client_secret: Option<String>,
    /// IdC 客户端身份指纹（{hash}.json 文件名 + 切号定位）。
    pub client_id_hash: Option<String>,
    /// 企业 SSO 的 Start URL。
    pub start_url: Option<String>,
    #[serde(skip)]
    pub scopes: Option<String>,
    pub login_hint: Option<String>,
    pub profile_arn: Option<String>,
    #[serde(skip)]
    pub id_token: Option<String>,
    #[serde(skip)]
    pub sso_session_id: Option<String>,
    /// 账号绑定的稳定 machineId（用于 user-agent + 上游身份识别）。
    pub machine_id: Option<String>,

    pub plan_name: Option<String>,
    pub plan_tier: Option<String>,
    pub credits_total: Option<f64>,
    pub credits_used: Option<f64>,
    pub bonus_total: Option<f64>,
    pub bonus_used: Option<f64>,
    pub usage_reset_at: Option<DateTime<Utc>>,
    pub bonus_expire_days: Option<i64>,

    pub last_refresh_at: Option<DateTime<Utc>>,
    pub failure_count: u32,
    pub success_count: u64,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub proxy_url: String,

    pub status: Option<String>,
    pub status_reason: Option<String>,
    pub disabled_reason: Option<String>,
    pub quota_checked_at: Option<DateTime<Utc>>,
    pub quota_error: Option<String>,

    #[serde(skip)]
    pub raw_auth_token: Value,
    #[serde(skip)]
    pub raw_usage: Value,
    /// usage_data：与 raw_usage 同义，但 KAM 沿用这个字段名。getUsageLimits 原始体。
    #[serde(skip)]
    pub usage_data: Value,
    /// ListAvailableModels 缓存：`{ cached_at, response, model_provider }`。30 分钟 TTL。
    #[serde(skip)]
    pub models_cache: Value,
}

impl KiroAccountRow {
    /// 从解析好的 token 数据建一行（首次导入 / 替换用）。运行时状态归零。
    pub fn from_token_data(id: String, data: &KiroTokenData) -> Self {
        let auth_method = if data.auth_method.is_empty() {
            auth_method_for_provider(&data.provider).to_string()
        } else {
            data.auth_method.clone()
        };
        Self {
            id,
            email: data.email.clone(),
            user_id: data.user_id.clone(),
            provider: data.provider.clone(),
            auth_method,
            login_provider: Some(data.provider.clone()),
            enabled: true,
            access_token: data.access_token.clone(),
            refresh_token: data.refresh_token.clone().unwrap_or_default(),
            token_type: data.token_type.clone(),
            expires_at: data.expires_at,
            idc_region: data.idc_region.clone(),
            issuer_url: data.issuer_url.clone(),
            client_id: data.client_id.clone(),
            client_secret: data.client_secret.clone(),
            client_id_hash: data.client_id_hash.clone(),
            start_url: data.start_url.clone(),
            scopes: data.scopes.clone(),
            login_hint: data.login_hint.clone(),
            profile_arn: data.profile_arn.clone(),
            id_token: data.id_token.clone(),
            sso_session_id: data.sso_session_id.clone(),
            machine_id: data.machine_id.clone(),
            plan_name: None,
            plan_tier: None,
            credits_total: None,
            credits_used: None,
            bonus_total: None,
            bonus_used: None,
            usage_reset_at: None,
            bonus_expire_days: None,
            last_refresh_at: None,
            failure_count: 0,
            success_count: 0,
            cooldown_until: None,
            last_error: None,
            last_used_at: None,
            total_requests: 0,
            total_failures: 0,
            proxy_url: String::new(),
            status: None,
            status_reason: None,
            disabled_reason: None,
            quota_checked_at: None,
            quota_error: None,
            raw_auth_token: data.raw_auth_token.clone(),
            raw_usage: data.raw_usage.clone(),
            usage_data: data.raw_usage.clone(),
            models_cache: Value::Object(Default::default()),
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
        if matches!(
            self.status.as_deref(),
            Some(crate::auth::kiro::KIRO_STATUS_BANNED)
                | Some(crate::auth::kiro::KIRO_STATUS_INVALID)
                | Some(crate::auth::kiro::KIRO_STATUS_CAPPED)
        ) {
            return false;
        }
        true
    }
}

fn json_from_str(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn col_string(r: &rusqlite::Row<'_>, name: &str) -> rusqlite::Result<String> {
    r.get::<_, Option<String>>(name).map(|o| o.unwrap_or_default())
}

fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<KiroAccountRow> {
    let raw_auth_token = json_from_str(&col_string(r, "raw_auth_token")?);
    let raw_usage = json_from_str(&col_string(r, "raw_usage")?);
    let usage_data = json_from_str(&col_string(r, "usage_data")?);
    let models_cache = json_from_str(&col_string(r, "models_cache")?);
    // provider 列在新 schema 才有；旧库 / 旧行可能为 NULL，由调用方负责 backfill。
    let provider = r
        .get::<_, Option<String>>("provider")?
        .unwrap_or_default();
    let auth_method = r.get::<_, String>("auth_method").unwrap_or_default();
    Ok(KiroAccountRow {
        id: r.get("id")?,
        email: r.get("email")?,
        user_id: r.get("user_id")?,
        provider,
        auth_method,
        login_provider: r.get("login_provider")?,
        enabled: r.get::<_, i64>("enabled")? != 0,
        access_token: r.get("access_token")?,
        refresh_token: r.get("refresh_token")?,
        token_type: r.get("token_type")?,
        expires_at: ms_to_dt(r.get("expires_at")?),
        idc_region: r.get("idc_region")?,
        issuer_url: r.get("issuer_url")?,
        client_id: r.get("client_id")?,
        client_secret: r.get("client_secret")?,
        client_id_hash: r.get("client_id_hash")?,
        start_url: r.get("start_url")?,
        scopes: r.get("scopes")?,
        login_hint: r.get("login_hint")?,
        profile_arn: r.get("profile_arn")?,
        id_token: r.get("id_token")?,
        sso_session_id: r.get("sso_session_id")?,
        machine_id: r.get("machine_id")?,
        plan_name: r.get("plan_name")?,
        plan_tier: r.get("plan_tier")?,
        credits_total: r.get("credits_total")?,
        credits_used: r.get("credits_used")?,
        bonus_total: r.get("bonus_total")?,
        bonus_used: r.get("bonus_used")?,
        usage_reset_at: ms_to_dt(r.get("usage_reset_at")?),
        bonus_expire_days: r.get("bonus_expire_days")?,
        last_refresh_at: ms_to_dt(r.get("last_refresh_at")?),
        failure_count: r.get::<_, i64>("failure_count")? as u32,
        success_count: r.get::<_, Option<i64>>("success_count")?.unwrap_or(0) as u64,
        cooldown_until: ms_to_dt(r.get("cooldown_until")?),
        last_error: r.get("last_error")?,
        last_used_at: ms_to_dt(r.get("last_used_at")?),
        total_requests: r.get::<_, i64>("total_requests")? as u64,
        total_failures: r.get::<_, i64>("total_failures")? as u64,
        proxy_url: r.get("proxy_url")?,
        status: r.get("status")?,
        status_reason: r.get("status_reason")?,
        disabled_reason: r.get("disabled_reason")?,
        quota_checked_at: ms_to_dt(r.get("quota_checked_at")?),
        quota_error: r.get("quota_error")?,
        raw_auth_token,
        raw_usage,
        usage_data,
        models_cache,
    })
}

/// 刷新后写回的 token 字段集合。
#[derive(Debug, Clone)]
pub struct KiroTokenUpdate {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub raw_auth_token: Value,
    /// 刷新有时返回新的 id_token / sso_session_id / profile_arn，优先覆盖。
    pub id_token: Option<String>,
    pub sso_session_id: Option<String>,
    pub profile_arn: Option<String>,
}

/// 额度查询结果写回。
#[derive(Debug, Clone, Default)]
pub struct KiroQuotaUpdate {
    pub plan_name: Option<String>,
    pub plan_tier: Option<String>,
    pub credits_total: Option<f64>,
    pub credits_used: Option<f64>,
    pub bonus_total: Option<f64>,
    pub bonus_used: Option<f64>,
    pub usage_reset_at: Option<DateTime<Utc>>,
    pub bonus_expire_days: Option<i64>,
    pub status: Option<String>,
    pub status_reason: Option<String>,
    pub raw_usage: Option<Value>,
    /// 完整 usage_data（getUsageLimits 原始体），落到 usage_data 列。
    pub usage_data: Option<Value>,
    pub quota_error: Option<String>,
    /// 刷新查额度时探测到的 region —— 写回 idc_region 加速下次。
    pub detected_region: Option<String>,
}

/// 全字段 upsert。运行时状态（enabled/failure/cooldown/统计/proxy）在冲突时保留旧值。
pub fn upsert(pool: &SqlitePool, a: &KiroAccountRow) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO kiro_accounts(
            id,email,user_id,provider,login_provider,auth_method,enabled,
            access_token,refresh_token,token_type,expires_at,
            idc_region,issuer_url,client_id,client_secret,client_id_hash,start_url,
            scopes,login_hint,profile_arn,id_token,sso_session_id,machine_id,
            last_refresh_at,failure_count,cooldown_until,last_error,
            last_used_at,total_requests,total_failures,proxy_url,
            raw_auth_token,raw_usage,usage_data,success_count
         ) VALUES (
            ?1,?2,?3,?4,?5,?6,?7,
            ?8,?9,?10,?11,
            ?12,?13,?14,?15,?16,?17,
            ?18,?19,?20,?21,?22,?23,
            ?24,?25,?26,?27,
            ?28,?29,?30,?31,
            ?32,?33,?34,?35
         )
         ON CONFLICT(id) DO UPDATE SET
            email = excluded.email,
            user_id = excluded.user_id,
            provider = excluded.provider,
            login_provider = excluded.login_provider,
            auth_method = excluded.auth_method,
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            token_type = excluded.token_type,
            expires_at = excluded.expires_at,
            idc_region = COALESCE(excluded.idc_region, kiro_accounts.idc_region),
            issuer_url = COALESCE(excluded.issuer_url, kiro_accounts.issuer_url),
            client_id = COALESCE(excluded.client_id, kiro_accounts.client_id),
            client_secret = COALESCE(excluded.client_secret, kiro_accounts.client_secret),
            client_id_hash = COALESCE(excluded.client_id_hash, kiro_accounts.client_id_hash),
            start_url = COALESCE(excluded.start_url, kiro_accounts.start_url),
            scopes = COALESCE(excluded.scopes, kiro_accounts.scopes),
            login_hint = COALESCE(excluded.login_hint, kiro_accounts.login_hint),
            profile_arn = COALESCE(excluded.profile_arn, kiro_accounts.profile_arn),
            id_token = COALESCE(excluded.id_token, kiro_accounts.id_token),
            sso_session_id = COALESCE(excluded.sso_session_id, kiro_accounts.sso_session_id),
            -- machine_id：保留旧值（账号一旦绑定 machineId，后续重导入不应改变 IdE 身份指纹）
            machine_id = COALESCE(kiro_accounts.machine_id, excluded.machine_id),
            last_refresh_at = excluded.last_refresh_at,
            -- proxy_url：仅当传入非空才覆盖（保留旧绑定）
            proxy_url = CASE WHEN excluded.proxy_url <> '' THEN excluded.proxy_url ELSE kiro_accounts.proxy_url END,
            raw_auth_token = excluded.raw_auth_token,
            -- usage_data 仅在新值非空对象时覆盖（避免 reimport 时清掉旧 usage 快照）
            usage_data = CASE
                WHEN excluded.usage_data <> '{}' THEN excluded.usage_data
                ELSE kiro_accounts.usage_data
            END
        ",
        params![
            a.id,
            a.email,
            a.user_id,
            a.provider,
            a.login_provider,
            a.auth_method,
            a.enabled as i64,
            a.access_token,
            a.refresh_token,
            a.token_type,
            dt_to_ms(a.expires_at),
            a.idc_region,
            a.issuer_url,
            a.client_id,
            a.client_secret,
            a.client_id_hash,
            a.start_url,
            a.scopes,
            a.login_hint,
            a.profile_arn,
            a.id_token,
            a.sso_session_id,
            a.machine_id,
            dt_to_ms(a.last_refresh_at),
            a.failure_count as i64,
            dt_to_ms(a.cooldown_until),
            a.last_error,
            dt_to_ms(a.last_used_at),
            a.total_requests as i64,
            a.total_failures as i64,
            a.proxy_url,
            serde_json::to_string(&a.raw_auth_token).unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&a.raw_usage).unwrap_or_else(|_| "{}".into()),
            serde_json::to_string(&a.usage_data).unwrap_or_else(|_| "{}".into()),
            a.success_count as i64,
        ],
    )?;
    Ok(())
}

pub fn get(pool: &SqlitePool, id: &str) -> Result<Option<KiroAccountRow>> {
    let conn = pool.get()?;
    Ok(conn
        .query_row(
            "SELECT * FROM kiro_accounts WHERE id = ?1",
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
            "SELECT COUNT(*) FROM kiro_accounts WHERE email LIKE ?1 OR id LIKE ?1 OR provider LIKE ?1",
            params![like],
            |r| r.get(0),
        )?
    } else {
        conn.query_row("SELECT COUNT(*) FROM kiro_accounts", [], |r| r.get(0))?
    };
    Ok(n)
}

pub fn list_page(
    pool: &SqlitePool,
    limit: i64,
    offset: i64,
    q: Option<&str>,
) -> Result<Vec<KiroAccountRow>> {
    let conn = pool.get()?;
    let mut rows: Vec<KiroAccountRow> = Vec::new();
    if let Some(qs) = q {
        let like = format!("%{}%", qs);
        let mut stmt = conn.prepare(
            "SELECT * FROM kiro_accounts WHERE email LIKE ?1 OR id LIKE ?1 OR provider LIKE ?1
             ORDER BY id LIMIT ?2 OFFSET ?3",
        )?;
        let it = stmt.query_map(params![like, limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    } else {
        let mut stmt =
            conn.prepare("SELECT * FROM kiro_accounts ORDER BY id LIMIT ?1 OFFSET ?2")?;
        let it = stmt.query_map(params![limit, offset], row_to_account)?;
        for r in it {
            rows.push(r?);
        }
    }
    Ok(rows)
}

pub fn all_ids_sorted(pool: &SqlitePool) -> Result<Vec<String>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT id FROM kiro_accounts ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

pub fn delete(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute("DELETE FROM kiro_accounts WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

pub fn set_enabled(pool: &SqlitePool, id: &str, enabled: bool) -> Result<bool> {
    let conn = pool.get()?;
    let n = if enabled {
        conn.execute(
            "UPDATE kiro_accounts SET enabled = 1, failure_count = 0, cooldown_until = NULL,
             disabled_reason = NULL WHERE id = ?1",
            params![id],
        )?
    } else {
        conn.execute(
            "UPDATE kiro_accounts SET enabled = 0 WHERE id = ?1",
            params![id],
        )?
    };
    Ok(n > 0)
}

pub fn set_disabled_with_reason(pool: &SqlitePool, id: &str, reason: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET enabled = 0, disabled_reason = ?2 WHERE id = ?1",
        params![id, reason],
    )?;
    Ok(n > 0)
}

pub fn set_proxy(pool: &SqlitePool, id: &str, proxy_url: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET proxy_url = ?2 WHERE id = ?1",
        params![id, proxy_url],
    )?;
    Ok(n > 0)
}

pub fn set_machine_id(pool: &SqlitePool, id: &str, machine_id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET machine_id = ?2 WHERE id = ?1 AND (machine_id IS NULL OR machine_id = '')",
        params![id, machine_id],
    )?;
    Ok(n > 0)
}

pub fn reset_cooldown(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET failure_count = 0, cooldown_until = NULL, last_error = NULL WHERE id = ?1",
        params![id],
    )?;
    Ok(n > 0)
}

pub fn mark_used(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE kiro_accounts SET last_used_at = ?2, total_requests = total_requests + 1 WHERE id = ?1",
        params![id, now],
    )?;
    Ok(())
}

pub fn report_success(pool: &SqlitePool, id: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE kiro_accounts SET failure_count = 0, success_count = success_count + 1,
         cooldown_until = NULL, last_error = NULL WHERE id = ?1",
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
            "SELECT failure_count, total_failures FROM kiro_accounts WHERE id = ?1",
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
        "UPDATE kiro_accounts SET
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

pub fn update_after_refresh(pool: &SqlitePool, id: &str, u: &KiroTokenUpdate) -> Result<()> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    conn.execute(
        "UPDATE kiro_accounts SET
            access_token = ?2,
            refresh_token = CASE WHEN ?3 <> '' THEN ?3 ELSE refresh_token END,
            token_type = COALESCE(?4, token_type),
            expires_at = ?5,
            last_refresh_at = ?6,
            raw_auth_token = ?7,
            id_token = COALESCE(?8, id_token),
            sso_session_id = COALESCE(?9, sso_session_id),
            profile_arn = COALESCE(?10, profile_arn),
            -- 刷新成功 → 清掉 'invalid' 状态，恢复 active
            status = CASE WHEN status = 'invalid' THEN 'active' ELSE status END,
            disabled_reason = CASE WHEN status = 'invalid' THEN NULL ELSE disabled_reason END
         WHERE id = ?1",
        params![
            id,
            u.access_token,
            u.refresh_token.clone().unwrap_or_default(),
            u.token_type,
            dt_to_ms(u.expires_at),
            now,
            serde_json::to_string(&u.raw_auth_token).unwrap_or_else(|_| "{}".into()),
            u.id_token,
            u.sso_session_id,
            u.profile_arn,
        ],
    )?;
    Ok(())
}

pub fn mark_refresh_failed(pool: &SqlitePool, id: &str, msg: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE kiro_accounts SET last_error = ?2 WHERE id = ?1",
        params![id, format!("refresh failed: {msg}")],
    )?;
    Ok(())
}

/// 把刷新失败 + token 已过期视为 token 失效：写 status=invalid + 禁用账号。
/// 与 KAM `tasks/token_refresh.rs` 的兜底分支一致。
pub fn mark_token_invalid(pool: &SqlitePool, id: &str, reason: &str) -> Result<()> {
    let conn = pool.get()?;
    conn.execute(
        "UPDATE kiro_accounts SET
            status = 'invalid',
            status_reason = ?2,
            disabled_reason = ?2,
            enabled = 0,
            last_error = ?3
         WHERE id = ?1",
        params![id, reason, format!("token invalid: {reason}")],
    )?;
    Ok(())
}

pub fn update_quota(pool: &SqlitePool, id: &str, q: &KiroQuotaUpdate) -> Result<bool> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    let raw_usage_str = q
        .raw_usage
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()));
    let usage_data_str = q
        .usage_data
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()));
    let n = conn.execute(
        "UPDATE kiro_accounts SET
            plan_name = COALESCE(?2, plan_name),
            plan_tier = COALESCE(?3, plan_tier),
            credits_total = COALESCE(?4, credits_total),
            credits_used = COALESCE(?5, credits_used),
            bonus_total = COALESCE(?6, bonus_total),
            bonus_used = COALESCE(?7, bonus_used),
            usage_reset_at = COALESCE(?8, usage_reset_at),
            bonus_expire_days = COALESCE(?9, bonus_expire_days),
            status = ?10,
            status_reason = ?11,
            quota_checked_at = ?12,
            quota_error = ?13,
            raw_usage = COALESCE(?14, raw_usage),
            usage_data = COALESCE(?15, usage_data),
            idc_region = COALESCE(?16, idc_region),
            -- 状态变 banned/capped/invalid 自动禁用，恢复 active 重新启用
            enabled = CASE
                WHEN ?10 IN ('banned','invalid','capped') THEN 0
                WHEN ?10 = 'active' THEN 1
                ELSE enabled
            END,
            disabled_reason = CASE
                WHEN ?10 IN ('banned','invalid','capped') THEN ?11
                WHEN ?10 = 'active' THEN NULL
                ELSE disabled_reason
            END
         WHERE id = ?1",
        params![
            id,
            q.plan_name,
            q.plan_tier,
            q.credits_total,
            q.credits_used,
            q.bonus_total,
            q.bonus_used,
            dt_to_ms(q.usage_reset_at),
            q.bonus_expire_days,
            q.status,
            q.status_reason,
            now,
            q.quota_error,
            raw_usage_str,
            usage_data_str,
            q.detected_region,
        ],
    )?;
    Ok(n > 0)
}

pub fn update_quota_error(pool: &SqlitePool, id: &str, msg: &str) -> Result<bool> {
    let conn = pool.get()?;
    let now = Utc::now().timestamp_millis();
    let n = conn.execute(
        "UPDATE kiro_accounts SET quota_checked_at = ?2, quota_error = ?3, status = ?4, status_reason = ?3 WHERE id = ?1",
        params![id, now, msg, crate::auth::kiro::KIRO_STATUS_ERROR],
    )?;
    Ok(n > 0)
}

/// 写入 ListAvailableModels 缓存（30 分钟 TTL，调用方负责打时间戳进 value）。
pub fn update_models_cache(pool: &SqlitePool, id: &str, cache: &Value) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET models_cache = ?2 WHERE id = ?1",
        params![id, serde_json::to_string(cache).unwrap_or_else(|_| "{}".into())],
    )?;
    Ok(n > 0)
}

/// 清空 ListAvailableModels 缓存（账号刷过 token 后顺手做一次，避免缓存里 access_token 失配）。
pub fn clear_models_cache(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute(
        "UPDATE kiro_accounts SET models_cache = '{}' WHERE id = ?1",
        params![id],
    )?;
    Ok(n > 0)
}

/// 后台 refresher 用：所有 enabled 且 refresh_token 非空、且即将（threshold 秒内）过期的账号。
pub fn snapshot_for_refresh(
    pool: &SqlitePool,
    threshold_seconds: i64,
) -> Result<Vec<KiroAccountRow>> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let cutoff = now_ms + threshold_seconds * 1000;
    // 未知过期时间（expires_at IS NULL）的账号不再每轮都刷：仅当从未刷新过、或距上次成功
    // 刷新已超过一个 threshold 窗口时才纳入候选，避免每个扫描周期都对其打一次上游。
    let stale_before = now_ms - threshold_seconds * 1000;
    let mut stmt = conn.prepare(
        "SELECT * FROM kiro_accounts
         WHERE enabled = 1 AND refresh_token <> ''
           AND (status IS NULL OR status NOT IN ('banned','invalid'))
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

/// 全表统计（给 stats 接口）。返回 (total, enabled, cooling, expired, sum_req, sum_fail)。
pub fn stats_overview(pool: &SqlitePool) -> Result<(usize, usize, usize, usize, u64, u64)> {
    let conn = pool.get()?;
    let now_ms = Utc::now().timestamp_millis();
    let total: i64 = conn.query_row("SELECT COUNT(*) FROM kiro_accounts", [], |r| r.get(0))?;
    let enabled: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kiro_accounts WHERE enabled = 1",
        [],
        |r| r.get(0),
    )?;
    let cooling: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kiro_accounts WHERE cooldown_until IS NOT NULL AND cooldown_until > ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let expired: i64 = conn.query_row(
        "SELECT COUNT(*) FROM kiro_accounts WHERE expires_at IS NULL OR expires_at <= ?1",
        params![now_ms],
        |r| r.get(0),
    )?;
    let (sum_req, sum_fail): (i64, i64) = conn.query_row(
        "SELECT COALESCE(SUM(total_requests),0), COALESCE(SUM(total_failures),0) FROM kiro_accounts",
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

/// 一次性回填：把旧库的 provider/auth_method 字段补齐到新 schema。
/// 仅对 `provider IS NULL OR provider = ''` 的行生效，已经有显式 provider 的行不动。
/// 推断规则与 [`KiroTokenData::from_value`] 一致：login_provider → start_url → 默认 BuilderId/Google。
pub fn backfill_provider(pool: &SqlitePool) -> Result<usize> {
    use crate::auth::kiro::{
        auth_method_for_provider, is_builder_id_start_url, normalize_provider_name,
        KIRO_PROVIDER_BUILDER_ID, KIRO_PROVIDER_ENTERPRISE, KIRO_PROVIDER_GOOGLE,
    };

    let conn = pool.get()?;
    let mut stmt = conn.prepare(
        "SELECT id, login_provider, auth_method, start_url, client_secret, email, client_id, client_id_hash
         FROM kiro_accounts
         WHERE provider IS NULL OR provider = ''",
    )?;
    let rows: Vec<(
        String,         // id
        Option<String>, // login_provider
        Option<String>, // auth_method
        Option<String>, // start_url
        Option<String>, // client_secret
        String,         // email
        Option<String>, // client_id
        Option<String>, // client_id_hash
    )> = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    drop(stmt);

    let mut updated = 0usize;
    for (id, login_provider, auth_method, start_url, client_secret, email, client_id, _hash) in
        rows
    {
        // 推断 provider
        let mut provider = login_provider
            .as_deref()
            .and_then(normalize_provider_name);
        if provider.is_none() {
            // 显式 IdC 凭据 + start_url
            if let Some(url) = start_url.as_deref() {
                if is_builder_id_start_url(url) {
                    provider = Some(KIRO_PROVIDER_BUILDER_ID.to_string());
                } else if !url.trim().is_empty() && url.contains("awsapps.com") {
                    provider = Some(KIRO_PROVIDER_ENTERPRISE.to_string());
                }
            }
        }
        if provider.is_none() {
            // 从 clientSecret 解 startUrl
            if let Some(secret) = client_secret.as_deref() {
                if let Some(url) = crate::auth::kiro::extract_start_url_from_client_secret(secret) {
                    if is_builder_id_start_url(&url) {
                        provider = Some(KIRO_PROVIDER_BUILDER_ID.to_string());
                    } else {
                        provider = Some(KIRO_PROVIDER_ENTERPRISE.to_string());
                    }
                }
            }
        }
        if provider.is_none() && client_id.is_some() {
            provider = Some(KIRO_PROVIDER_BUILDER_ID.to_string());
        }
        if provider.is_none() {
            provider = crate::auth::kiro::normalize_provider_name(&email)
                .or_else(|| {
                    let lower = email.to_ascii_lowercase();
                    if lower.contains("@gmail") {
                        Some("Google".to_string())
                    } else if lower.contains("github") {
                        Some("Github".to_string())
                    } else {
                        None
                    }
                });
        }
        let provider = provider.unwrap_or_else(|| {
            // auth_method 是 idc 但啥都没 → BuilderId 兜底
            if matches!(auth_method.as_deref(), Some("idc") | Some("IdC")) {
                KIRO_PROVIDER_BUILDER_ID.to_string()
            } else {
                KIRO_PROVIDER_GOOGLE.to_string()
            }
        });
        let am = auth_method_for_provider(&provider);

        let n = conn.execute(
            "UPDATE kiro_accounts SET provider = ?2, login_provider = ?2, auth_method = ?3 WHERE id = ?1",
            params![id, provider, am],
        )?;
        if n > 0 {
            updated += 1;
        }
    }
    Ok(updated)
}
