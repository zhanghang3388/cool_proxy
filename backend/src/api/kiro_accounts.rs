//! Kiro 账号池的管理面板接口。镜像 `api/accounts.rs`，字段换成 Kiro 集合。
//! 本期只做账号池管理：列表 / 导入 / 上传 / 启停 / 删除 / 绑代理 / 刷新 token / 查额度。

use std::sync::Arc;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::info;

use crate::auth::kiro::KiroTokenData;
use crate::auth::kiro_quota::{banned_reason, fetch_kiro_usage, KiroUsageSnapshot};
use crate::state::AppState;
use crate::store::kiro_accounts::{KiroAccountRow, KiroQuotaUpdate};

// ===== 视图模型 =====

#[derive(Serialize)]
pub struct KiroUsageView {
    pub plan_name: Option<String>,
    pub plan_tier: Option<String>,
    pub credits_total: Option<f64>,
    pub credits_used: Option<f64>,
    pub credits_remaining: Option<f64>,
    pub bonus_total: Option<f64>,
    pub bonus_used: Option<f64>,
    pub bonus_remaining: Option<f64>,
    pub usage_reset_at: Option<String>,
    pub bonus_expire_days: Option<i64>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct KiroAccountView {
    pub id: String,
    pub email: String,
    pub user_id: Option<String>,
    pub login_provider: Option<String>,
    pub auth_method: String,
    pub enabled: bool,
    pub expire_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_used_at: Option<String>,
    pub failure_count: u32,
    pub cooldown_until: Option<String>,
    pub last_error: Option<String>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub expired: bool,
    pub proxy_url: String,
    pub proxy_id: Option<String>,
    pub status: Option<String>,
    pub status_reason: Option<String>,
    pub usage: KiroUsageView,
}

#[derive(Serialize)]
pub struct KiroAccountListResp {
    pub total: i64,
    pub items: Vec<KiroAccountView>,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default = "default_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub q: Option<String>,
}

fn default_limit() -> i64 {
    50
}

fn remaining(total: Option<f64>, used: Option<f64>) -> Option<f64> {
    match (total, used) {
        (Some(t), Some(u)) => Some((t - u).max(0.0)),
        (Some(t), None) => Some(t),
        _ => None,
    }
}

fn usage_view(a: &KiroAccountRow) -> KiroUsageView {
    KiroUsageView {
        plan_name: a.plan_name.clone(),
        plan_tier: a.plan_tier.clone(),
        credits_total: a.credits_total,
        credits_used: a.credits_used,
        credits_remaining: remaining(a.credits_total, a.credits_used),
        bonus_total: a.bonus_total,
        bonus_used: a.bonus_used,
        bonus_remaining: remaining(a.bonus_total, a.bonus_used),
        usage_reset_at: a.usage_reset_at.map(|t| t.to_rfc3339()),
        bonus_expire_days: a.bonus_expire_days,
        checked_at: a.quota_checked_at.map(|t| t.to_rfc3339()),
        error: a.quota_error.clone(),
    }
}

fn account_view(app: &Arc<AppState>, a: KiroAccountRow, now: chrono::DateTime<chrono::Utc>) -> KiroAccountView {
    let usage = usage_view(&a);
    KiroAccountView {
        expired: a.expires_at.map(|t| t <= now).unwrap_or(true),
        proxy_id: app.proxy_pool.id_by_url(&a.proxy_url),
        id: a.id,
        email: a.email,
        user_id: a.user_id,
        login_provider: a.login_provider,
        auth_method: a.auth_method,
        enabled: a.enabled,
        expire_at: a.expires_at.map(|t| t.to_rfc3339()),
        last_refresh_at: a.last_refresh_at.map(|t| t.to_rfc3339()),
        last_used_at: a.last_used_at.map(|t| t.to_rfc3339()),
        failure_count: a.failure_count,
        cooldown_until: a.cooldown_until.map(|t| t.to_rfc3339()),
        last_error: a.last_error,
        total_requests: a.total_requests,
        total_failures: a.total_failures,
        proxy_url: a.proxy_url,
        status: a.status,
        status_reason: a.status_reason,
        usage,
    }
}

// ===== 列表 =====

pub async fn list(State(app): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    let now = chrono::Utc::now();
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let search = q.q.as_deref().filter(|s| !s.is_empty());

    let total = match app.kiro_pool.count(search) {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = match app.kiro_pool.list_page(limit, offset, search) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let items = rows
        .into_iter()
        .map(|a| account_view(&app, a, now))
        .collect();

    Json(KiroAccountListResp {
        total,
        items,
        limit,
        offset,
    })
    .into_response()
}

// ===== 启停 / 删除 / 绑代理 =====

#[derive(Deserialize)]
pub struct PatchPayload {
    pub enabled: Option<bool>,
}

pub async fn patch_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<PatchPayload>,
) -> Response {
    if let Some(enabled) = payload.enabled {
        if !app.kiro_pool.set_enabled(&id, enabled) {
            return (StatusCode::NOT_FOUND, "account not found").into_response();
        }
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if app.kiro_pool.remove(&id).is_none() {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    }
    info!(account = %id, "kiro account removed");
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
pub struct SetProxyPayload {
    #[serde(default)]
    pub proxy_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

pub async fn set_proxy(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<SetProxyPayload>,
) -> Response {
    let url = if let Some(pid) = payload.proxy_id.as_deref() {
        if pid.is_empty() {
            String::new()
        } else {
            match app.proxy_pool.url_by_id(pid) {
                Some(u) => u,
                None => return (StatusCode::NOT_FOUND, "proxy not found").into_response(),
            }
        }
    } else {
        payload.url.unwrap_or_default()
    };

    if let Err(e) = app.kiro_pool.set_proxy(&id, url) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn reset_cooldown(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if app.kiro_pool.reset_cooldown(&id) {
        Json(json!({"ok": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, "account not found").into_response()
    }
}

// ===== 刷新 token =====

pub async fn manual_refresh(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(acc) = app.kiro_pool.get(&id) else {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    };
    if acc.refresh_token.is_empty() {
        return (StatusCode::BAD_REQUEST, "no refresh_token on file").into_response();
    }
    match app.kiro_refresher.refresh(&acc).await {
        Ok(update) => {
            app.kiro_pool.update_after_refresh(&id, &update);
            app.kiro_pool.report_success(&id);
            let expire = update.expires_at.map(|t| t.to_rfc3339());
            Json(json!({"ok": true, "expire_at": expire})).into_response()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            app.kiro_pool.mark_refresh_failed(&id, &msg);
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

// ===== 额度查询 =====

#[derive(Serialize)]
pub struct QuotaRefreshItem {
    pub id: String,
    pub ok: bool,
    pub usage: Option<KiroUsageView>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct QuotaRefreshPayload {
    #[serde(default)]
    pub ids: Vec<String>,
}

#[derive(Serialize)]
pub struct QuotaRefreshResp {
    pub items: Vec<QuotaRefreshItem>,
}

fn quota_update_from_snapshot(snapshot: KiroUsageSnapshot) -> KiroQuotaUpdate {
    KiroQuotaUpdate {
        plan_name: snapshot.plan_name,
        plan_tier: snapshot.plan_tier,
        credits_total: snapshot.credits_total,
        credits_used: snapshot.credits_used,
        bonus_total: snapshot.bonus_total,
        bonus_used: snapshot.bonus_used,
        usage_reset_at: snapshot.usage_reset_at,
        bonus_expire_days: snapshot.bonus_expire_days,
        status: Some(crate::auth::kiro::KIRO_STATUS_NORMAL.to_string()),
        status_reason: None,
        raw_usage: Some(snapshot.raw),
        quota_error: None,
    }
}

async fn refresh_one_quota(app: Arc<AppState>, id: String) -> QuotaRefreshItem {
    let Some(acc) = app.kiro_pool.get(&id) else {
        return QuotaRefreshItem {
            id,
            ok: false,
            usage: None,
            error: Some("account not found".to_string()),
        };
    };

    let Some(profile_arn) = acc.profile_arn.clone() else {
        let msg = "缺少 profile_arn，无法查询额度".to_string();
        app.kiro_pool.update_quota_error(&id, &msg);
        let usage = app.kiro_pool.get(&id).map(|a| usage_view(&a));
        return QuotaRefreshItem {
            id,
            ok: false,
            usage,
            error: Some(msg),
        };
    };

    let result = fetch_kiro_usage(
        &app.clients,
        &acc.access_token,
        &profile_arn,
        &acc.proxy_url,
    )
    .await;

    let (ok, error) = match result {
        Ok(snapshot) => {
            let update = quota_update_from_snapshot(snapshot);
            app.kiro_pool.update_quota(&id, &update);
            (true, None)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            // 识别封禁，写到 status 而不仅仅是错误
            if let Some(reason) = banned_reason(&msg) {
                let banned = KiroQuotaUpdate {
                    status: Some(crate::auth::kiro::KIRO_STATUS_BANNED.to_string()),
                    status_reason: Some(reason.clone()),
                    quota_error: Some(reason.clone()),
                    raw_usage: None,
                    ..Default::default()
                };
                app.kiro_pool.update_quota(&id, &banned);
                (false, Some(format!("BANNED: {reason}")))
            } else {
                app.kiro_pool.update_quota_error(&id, &msg);
                (false, Some(msg))
            }
        }
    };

    let usage = app.kiro_pool.get(&id).map(|a| usage_view(&a));
    QuotaRefreshItem {
        id,
        ok,
        usage,
        error,
    }
}

pub async fn refresh_quota(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let item = refresh_one_quota(app, id).await;
    if item.usage.is_none() && item.error.as_deref() == Some("account not found") {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    }
    Json(item).into_response()
}

pub async fn refresh_quotas(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<QuotaRefreshPayload>,
) -> Response {
    let ids = if payload.ids.is_empty() {
        app.kiro_pool.all_ids_sorted()
    } else {
        payload.ids
    };
    let items = stream::iter(ids)
        .map(|id| refresh_one_quota(app.clone(), id))
        .buffer_unordered(6)
        .collect::<Vec<_>>()
        .await;
    Json(QuotaRefreshResp { items }).into_response()
}

// ===== 导入 =====

#[derive(Deserialize)]
pub struct ImportPayload {
    /// `tokens` 数组 / `token` 单对象 / `text` 文本（JSON / JSONL / 数组）三选一。
    #[serde(default)]
    pub tokens: Option<Vec<Value>>,
    #[serde(default)]
    pub token: Option<Value>,
    #[serde(default)]
    pub text: Option<String>,
}

/// 把文本切成多个 JSON 值：整体当数组 -> 单对象 -> 逐行 JSONL。
fn parse_text_to_values(text: &str) -> Vec<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if let Ok(arr) = serde_json::from_str::<Vec<Value>>(trimmed) {
        return arr;
    }
    if let Ok(one) = serde_json::from_str::<Value>(trimmed) {
        if one.is_object() {
            return vec![one];
        }
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if v.is_object() {
                out.push(v);
            }
        }
    }
    out
}

/// 解析一个 JSON 值并入库（自动分配代理）。返回 account id 或错误说明。
fn import_one_value(app: &Arc<AppState>, value: &Value, label: &str) -> Result<String, String> {
    let data = KiroTokenData::from_value(value).map_err(|e| format!("{label}: {e}"))?;
    if data.access_token.is_empty() {
        return Err(format!("{label}: missing access_token"));
    }
    if data.refresh_token.is_none() {
        tracing::warn!("{label}: imported without refresh_token, will not be auto-refreshed");
    }

    let acc = app
        .kiro_pool
        .add_or_replace(&data)
        .map_err(|e| format!("{label}: {e}"))?;

    // 文件本身没带代理且这条还没绑过时，自动分配一个
    if acc.proxy_url.trim().is_empty() {
        if let Some((_, url)) = app.proxy_pool.next_assignment() {
            let _ = app.kiro_pool.set_proxy(&acc.id, url);
        }
    }
    Ok(acc.id)
}

pub async fn import_json(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<ImportPayload>,
) -> Response {
    let mut values: Vec<Value> = Vec::new();
    if let Some(arr) = payload.tokens {
        values.extend(arr);
    }
    if let Some(one) = payload.token {
        values.push(one);
    }
    if let Some(text) = payload.text {
        values.extend(parse_text_to_values(&text));
    }

    if values.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            "no parseable JSON tokens; expect `tokens`/`token`/`text` field",
        )
            .into_response();
    }

    let mut imported: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for (idx, v) in values.iter().enumerate() {
        let label = format!("#{}", idx + 1);
        match import_one_value(&app, v, &label) {
            Ok(id) => imported.push(id),
            Err(e) => errors.push(e),
        }
    }
    let body = json!({ "imported": imported, "errors": errors });
    if imported.is_empty() && !errors.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }
    Json(body).into_response()
}

/// 上传 Kiro 认证 JSON 文件，写入 DB。
pub async fn upload(State(app): State<Arc<AppState>>, mut multipart: Multipart) -> Response {
    let mut imported: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("multipart error: {e}")).into_response();
        }
    } {
        let original_name = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "<unnamed>".to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("{original_name}: read field failed: {e}"));
                continue;
            }
        };
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("{original_name}: invalid json ({e})"));
                continue;
            }
        };
        match import_one_value(&app, &value, &original_name) {
            Ok(id) => imported.push(id),
            Err(e) => errors.push(e),
        }
    }

    let body = json!({ "imported": imported, "errors": errors });
    if imported.is_empty() && !errors.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }
    Json(body).into_response()
}

// ===== 统计 =====

#[derive(Serialize)]
pub struct KiroStatsView {
    pub total_accounts: usize,
    pub enabled_accounts: usize,
    pub cooling_down: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}

pub async fn stats(State(app): State<Arc<AppState>>) -> Response {
    match app.kiro_pool.stats_overview() {
        Ok(s) => Json(KiroStatsView {
            total_accounts: s.total,
            enabled_accounts: s.enabled,
            cooling_down: s.cooling,
            expired: s.expired,
            total_requests: s.total_requests,
            total_failures: s.total_failures,
        })
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
