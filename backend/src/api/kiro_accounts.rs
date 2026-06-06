//! Kiro 账号池的管理面板接口。镜像 `api/accounts.rs`，字段集与 KAM 对齐。
//!
//! 与之前接口的差异：
//!  - 列表 view 多出 `provider`（显式 Google/Github/BuilderId/Enterprise）、
//!    `client_id_hash`、`start_url`、`machine_id`、`disabled_reason`，前端可直接展示；
//!  - 查额度走 KAM 风格：social/BuilderId 传默认 profileArn、Enterprise 跨区域探测；
//!  - SSO 登录：在企业 Start URL 之外，新增 BuilderId（默认 view.awsapps.com/start）入口。

use std::sync::Arc;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::info;

use crate::auth::kiro::{
    auth_method_for_provider, KiroTokenData, KIRO_PROVIDER_BUILDER_ID, KIRO_PROVIDER_ENTERPRISE,
};
use crate::auth::kiro_models::{
    build_cache_entry, fetch_all_available_models, read_models_cache, ListModelsQuery,
};
use crate::auth::kiro_quota::{
    banned_reason, fetch_kiro_usage, is_auth_error_message as is_quota_auth_error,
    KiroUsageSnapshot, UsageQuery,
};
use crate::auth::kiro_refresh::is_auth_error_message as is_refresh_auth_error;
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
    /// 显式 provider：Google / Github / BuilderId / Enterprise。
    pub provider: String,
    /// 兼容旧字段（与 provider 同值，前端逐步迁移）。
    pub login_provider: Option<String>,
    pub auth_method: String,
    pub enabled: bool,
    pub expire_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_used_at: Option<String>,
    pub failure_count: u32,
    pub success_count: u64,
    pub cooldown_until: Option<String>,
    pub last_error: Option<String>,
    pub disabled_reason: Option<String>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub expired: bool,
    pub proxy_url: String,
    pub proxy_id: Option<String>,
    pub status: Option<String>,
    pub status_reason: Option<String>,
    pub start_url: Option<String>,
    pub client_id_hash: Option<String>,
    pub machine_id: Option<String>,
    pub region: Option<String>,
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

fn account_view(
    app: &Arc<AppState>,
    a: KiroAccountRow,
    now: chrono::DateTime<chrono::Utc>,
) -> KiroAccountView {
    let usage = usage_view(&a);
    KiroAccountView {
        expired: a.expires_at.map(|t| t <= now).unwrap_or(true),
        proxy_id: app.proxy_pool.id_by_url(&a.proxy_url),
        id: a.id,
        email: a.email,
        user_id: a.user_id,
        login_provider: Some(a.provider.clone()),
        provider: a.provider,
        auth_method: a.auth_method,
        enabled: a.enabled,
        expire_at: a.expires_at.map(|t| t.to_rfc3339()),
        last_refresh_at: a.last_refresh_at.map(|t| t.to_rfc3339()),
        last_used_at: a.last_used_at.map(|t| t.to_rfc3339()),
        failure_count: a.failure_count,
        success_count: a.success_count,
        cooldown_until: a.cooldown_until.map(|t| t.to_rfc3339()),
        last_error: a.last_error,
        disabled_reason: a.disabled_reason,
        total_requests: a.total_requests,
        total_failures: a.total_failures,
        proxy_url: a.proxy_url,
        status: a.status,
        status_reason: a.status_reason,
        start_url: a.start_url,
        client_id_hash: a.client_id_hash,
        machine_id: a.machine_id,
        region: a.idc_region,
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
            // AUTH_ERROR + token 已过期 → 标记 invalid 而不仅仅记错误
            let now = chrono::Utc::now();
            let token_dead = acc.expires_at.map(|t| t <= now).unwrap_or(true);
            if is_refresh_auth_error(&msg) && token_dead {
                app.kiro_pool.mark_token_invalid(&id, &msg);
            } else {
                app.kiro_pool.mark_refresh_failed(&id, &msg);
            }
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
    let raw_value = snapshot.raw.clone();
    KiroQuotaUpdate {
        plan_name: snapshot.plan_name,
        plan_tier: snapshot.plan_tier,
        credits_total: snapshot.credits_total,
        credits_used: snapshot.credits_used,
        bonus_total: snapshot.bonus_total,
        bonus_used: snapshot.bonus_used,
        usage_reset_at: snapshot.usage_reset_at,
        bonus_expire_days: snapshot.bonus_expire_days,
        status: Some(snapshot.derived_status),
        status_reason: None,
        raw_usage: Some(raw_value.clone()),
        usage_data: Some(raw_value),
        quota_error: None,
        detected_region: snapshot.detected_region,
    }
}

async fn refresh_one_quota(app: Arc<AppState>, id: String) -> QuotaRefreshItem {
    let Some(mut acc) = app.kiro_pool.get(&id) else {
        return QuotaRefreshItem {
            id,
            ok: false,
            usage: None,
            error: Some("account not found".to_string()),
        };
    };

    // 查额度前先确保 access_token 新鲜：过期/将过期的 token 会被上游判为
    // "bearer token invalid"（与 KAM 一致 —— 用前先刷新）。
    let stale = acc
        .expires_at
        .map(|e| e <= chrono::Utc::now() + chrono::Duration::seconds(60))
        .unwrap_or(true);
    if stale && !acc.refresh_token.is_empty() {
        if let Some(_guard) = app.kiro_refresher.begin_refresh(&id) {
            match app.kiro_refresher.refresh(&acc).await {
                Ok(update) => {
                    app.kiro_pool.update_after_refresh(&id, &update);
                    if let Some(fresh) = app.kiro_pool.get(&id) {
                        acc = fresh;
                    }
                }
                Err(e) => {
                    let msg = format!("token 刷新失败（查额度前）: {e:#}");
                    tracing::warn!(account = %id, "{msg}");
                    let token_dead = acc
                        .expires_at
                        .map(|t| t <= chrono::Utc::now())
                        .unwrap_or(true);
                    if is_refresh_auth_error(&msg) && token_dead {
                        app.kiro_pool.mark_token_invalid(&id, &msg);
                    } else {
                        app.kiro_pool.update_quota_error(&id, &msg);
                    }
                    let usage = app.kiro_pool.get(&id).map(|a| usage_view(&a));
                    return QuotaRefreshItem {
                        id,
                        ok: false,
                        usage,
                        error: Some(msg),
                    };
                }
            }
        }
    }

    // 按 provider 派发查询路径（Enterprise 多区域探测，其它单区域 + 默认 ARN）。
    let result = fetch_kiro_usage(
        &app.clients,
        UsageQuery {
            account_id: &acc.id,
            access_token: &acc.access_token,
            provider: &acc.provider,
            idc_region: acc.idc_region.as_deref(),
            profile_arn: acc.profile_arn.as_deref(),
            machine_id: acc.machine_id.as_deref(),
            proxy_url: &acc.proxy_url,
        },
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
                    usage_data: None,
                    ..Default::default()
                };
                app.kiro_pool.update_quota(&id, &banned);
                (false, Some(format!("BANNED: {reason}")))
            } else if is_quota_auth_error(&msg) {
                // 401 后再 refresh 一次重试
                if let Some(_guard) = app.kiro_refresher.begin_refresh(&id) {
                    if let Ok(update) = app.kiro_refresher.refresh(&acc).await {
                        app.kiro_pool.update_after_refresh(&id, &update);
                        if let Some(fresh) = app.kiro_pool.get(&id) {
                            acc = fresh;
                        }
                        if let Ok(snapshot) = fetch_kiro_usage(
                            &app.clients,
                            UsageQuery {
                                account_id: &acc.id,
                                access_token: &acc.access_token,
                                provider: &acc.provider,
                                idc_region: acc.idc_region.as_deref(),
                                profile_arn: acc.profile_arn.as_deref(),
                                machine_id: acc.machine_id.as_deref(),
                                proxy_url: &acc.proxy_url,
                            },
                        )
                        .await
                        {
                            app.kiro_pool
                                .update_quota(&id, &quota_update_from_snapshot(snapshot));
                            let usage = app.kiro_pool.get(&id).map(|a| usage_view(&a));
                            return QuotaRefreshItem {
                                id,
                                ok: true,
                                usage,
                                error: None,
                            };
                        }
                    }
                }
                app.kiro_pool.update_quota_error(&id, &msg);
                (false, Some(msg))
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

// ===== 账号实际可用模型（诊断用，调用方为管理面板）=====
//
// 客户端 GET /kiro/v1/models 永远返回 Kiro 官方维护的全集（21 个模型，与 KAM 对齐），
// 不打上游 —— 高频访问、所有账号都一样、订阅差异由 messages 调用时上游自然拒绝处理。
// 这里的 /api/kiro/accounts/<id>/models 是**诊断接口**：去打上游 ListAvailableModels
// 看某个具体账号当前订阅实际开了哪些模型，结果 30 分钟缓存到该账号上。

#[derive(Deserialize, Default)]
pub struct AccountModelsQuery {
    /// 强制重新拉取上游，绕过 30 分钟缓存。
    #[serde(default)]
    pub force: Option<bool>,
    /// 按 modelProvider 过滤（KAM 单页参数：anthropic / openai / ...）。
    #[serde(default)]
    pub model_provider: Option<String>,
}

#[derive(Serialize)]
pub struct AccountModelsResp {
    pub id: String,
    pub provider: String,
    pub region: Option<String>,
    pub from_cache: bool,
    pub cached_at: Option<String>,
    pub default_model_id: Option<String>,
    pub models: Vec<Value>,
}

pub async fn account_models(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<AccountModelsQuery>,
) -> Response {
    let Some(mut acc) = app.kiro_pool.get(&id) else {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    };

    let force = q.force.unwrap_or(false);
    let model_provider = q
        .model_provider
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // 1) 缓存命中且非强刷 —— 直接返回。
    if !force {
        if let Some(resp) = read_models_cache(&acc.models_cache, model_provider, false) {
            // 缓存里只存了 response 主体，cached_at 单独从 models_cache JSON 里读。
            let cached_at = acc
                .models_cache
                .get("cachedAt")
                .or_else(|| acc.models_cache.get("cached_at"))
                .and_then(|v| v.as_i64())
                .and_then(|s| chrono::DateTime::<chrono::Utc>::from_timestamp(s, 0))
                .map(|t| t.to_rfc3339());
            return Json(AccountModelsResp {
                id,
                provider: acc.provider,
                region: acc.idc_region,
                from_cache: true,
                cached_at,
                default_model_id: resp.default_model.as_ref().map(|m| m.model_id.clone()),
                models: resp
                    .available_models
                    .iter()
                    .map(model_to_view)
                    .collect(),
            })
            .into_response();
        }
    }

    // 2) 查上游前先确保 token 新鲜（与查额度路径一致）。
    let stale = acc
        .expires_at
        .map(|e| e <= chrono::Utc::now() + chrono::Duration::seconds(60))
        .unwrap_or(true);
    if stale && !acc.refresh_token.is_empty() {
        if let Some(_guard) = app.kiro_refresher.begin_refresh(&id) {
            match app.kiro_refresher.refresh(&acc).await {
                Ok(update) => {
                    app.kiro_pool.update_after_refresh(&id, &update);
                    if let Some(fresh) = app.kiro_pool.get(&id) {
                        acc = fresh;
                    }
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        format!("token refresh failed: {e:#}"),
                    )
                        .into_response();
                }
            }
        }
    }

    // 3) 真去打 ListAvailableModels。401 自动 refresh + 重试一次；403 + suspended 标 banned。
    let mut access_token = acc.access_token.clone();
    let mut profile_arn = acc.profile_arn.clone();
    let mut idc_region = acc.idc_region.clone();
    let mut machine_id = acc.machine_id.clone();
    let mut last_err: Option<String> = None;

    for attempt in 0..2u32 {
        let res = fetch_all_available_models(
            &app.clients,
            ListModelsQuery {
                access_token: &access_token,
                provider: &acc.provider,
                idc_region: idc_region.as_deref(),
                profile_arn: profile_arn.as_deref(),
                machine_id: machine_id.as_deref(),
                model_provider,
                proxy_url: &acc.proxy_url,
            },
        )
        .await;

        match res {
            Ok(resp) => {
                let entry = build_cache_entry(&resp, model_provider);
                app.kiro_pool.update_models_cache(&id, &entry);
                return Json(AccountModelsResp {
                    id: acc.id.clone(),
                    provider: acc.provider.clone(),
                    region: idc_region.clone(),
                    from_cache: false,
                    cached_at: Some(chrono::Utc::now().to_rfc3339()),
                    default_model_id: resp.default_model.as_ref().map(|m| m.model_id.clone()),
                    models: resp.available_models.iter().map(model_to_view).collect(),
                })
                .into_response();
            }
            Err(e) => {
                let msg = format!("{e:#}");
                last_err = Some(msg.clone());
                if attempt == 0 && msg.contains("AUTH_ERROR:") {
                    if let Some(_guard) = app.kiro_refresher.begin_refresh(&id) {
                        if let Ok(update) = app.kiro_refresher.refresh(&acc).await {
                            app.kiro_pool.update_after_refresh(&id, &update);
                            if let Some(fresh) = app.kiro_pool.get(&id) {
                                acc = fresh;
                                access_token = acc.access_token.clone();
                                profile_arn = acc.profile_arn.clone();
                                idc_region = acc.idc_region.clone();
                                machine_id = acc.machine_id.clone();
                            }
                            continue;
                        }
                    }
                }
                if msg.contains("BANNED:") {
                    let reason = msg
                        .split("BANNED:")
                        .nth(1)
                        .map(str::trim)
                        .unwrap_or("suspended")
                        .to_string();
                    app.kiro_pool.mark_banned(&id, &reason);
                }
                break;
            }
        }
    }

    (
        StatusCode::BAD_GATEWAY,
        last_err.unwrap_or_else(|| "ListAvailableModels failed".to_string()),
    )
        .into_response()
}

/// 把单个 AvailableModel 转成 OpenAI 风格的扁平 JSON（前端可直接渲染）。
fn model_to_view(m: &crate::auth::kiro_models::AvailableModel) -> Value {
    json!({
        "id": m.model_id,
        "name": if m.model_name.is_empty() { &m.model_id } else { &m.model_name },
        "description": m.description,
        "provider": m.provider,
        "is_default": m.is_default.unwrap_or(false),
        "context_window": m.context_window,
        "rate_multiplier": m.rate_multiplier,
        "rate_unit": m.rate_unit,
        "capabilities": m.capabilities,
        "supported_input_types": m.supported_input_types,
        "token_limits": m.token_limits,
        "prompt_caching": m.prompt_caching,
    })
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

// ===== SSO（AWS IAM Identity Center / Builder ID）登录 =====
//
// 步骤一：调用 `oidc.{region}.amazonaws.com/client/register` 注册 OIDC 客户端，
// 生成 PKCE + state，构造授权链接返回前端展示。
// 步骤二：用户在浏览器自行完成授权，把回调 URL（含 `code=`）整段粘回，后端调用
// `/token` 换出 access/refresh token，按 KAM 规则推断 provider（BuilderId vs Enterprise），
// 自动算 client_id_hash、绑定代理、入池。

#[derive(Deserialize)]
pub struct SsoLoginStartPayload {
    /// 组织 SSO Start URL，例如 https://your-org.awsapps.com/start。留空走 BuilderId 默认值。
    #[serde(default)]
    pub start_url: Option<String>,
    /// 想要登录的 provider —— 默认按 start_url 推断。
    #[serde(default)]
    pub provider: Option<String>,
    /// AWS 区域，缺省 us-east-1。
    #[serde(default)]
    pub region: Option<String>,
    /// 可选：登录及后续请求走的代理（id 优先，其次 url）。
    #[serde(default)]
    pub proxy_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// 可选：邮箱/备注，用于生成稳定账号 id。
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Serialize)]
pub struct SsoLoginStartResp {
    pub auth_url: String,
    pub state: String,
}

/// 第一步：注册 OIDC 客户端并返回授权链接。
pub async fn sso_login_start(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<SsoLoginStartPayload>,
) -> Response {
    // 推断 provider + start_url：用户给了 start_url 就遵守它（落到 BuilderId 默认 url
    // 视为 BuilderId 登录）；没给 url 但显式选了 BuilderId，则用 BuilderId 默认 url。
    let normalized_provider = payload
        .provider
        .as_deref()
        .and_then(crate::auth::kiro::normalize_provider_name);
    let explicit_url = payload
        .start_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let (provider, start_url) = match (normalized_provider.as_deref(), explicit_url) {
        (Some(KIRO_PROVIDER_BUILDER_ID), None) => (
            KIRO_PROVIDER_BUILDER_ID.to_string(),
            crate::auth::kiro::KIRO_BUILDER_ID_START_URL.to_string(),
        ),
        (Some(p), Some(url)) => (p.to_string(), url.to_string()),
        (Some(KIRO_PROVIDER_ENTERPRISE), None) => {
            return (StatusCode::BAD_REQUEST, "Enterprise 登录必须填写 Start URL").into_response();
        }
        (None, Some(url)) => {
            // 推断 provider
            let p = if crate::auth::kiro::is_builder_id_start_url(url) {
                KIRO_PROVIDER_BUILDER_ID.to_string()
            } else {
                KIRO_PROVIDER_ENTERPRISE.to_string()
            };
            (p, url.to_string())
        }
        (None, None) => (
            KIRO_PROVIDER_BUILDER_ID.to_string(),
            crate::auth::kiro::KIRO_BUILDER_ID_START_URL.to_string(),
        ),
        (Some(_other), None) => {
            return (
                StatusCode::BAD_REQUEST,
                "Social 登录暂不支持 SSO 流程，请走『粘贴 JSON』导入",
            )
                .into_response();
        }
    };

    if !start_url.starts_with("https://") {
        return (StatusCode::BAD_REQUEST, "Start URL 必须以 https:// 开头").into_response();
    }
    let region = payload
        .region
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("us-east-1")
        .to_string();

    let proxy_url = if let Some(pid) = payload.proxy_id.as_deref().filter(|s| !s.is_empty()) {
        match app.proxy_pool.url_by_id(pid) {
            Some(u) => u,
            None => return (StatusCode::NOT_FOUND, "proxy not found").into_response(),
        }
    } else {
        payload.url.clone().unwrap_or_default()
    };

    let http = match app.clients.get(&proxy_url) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("proxy error: {e}")).into_response(),
    };

    let email_hint = payload
        .email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    match app
        .kiro_login
        .start(
            &http,
            &provider,
            &start_url,
            &region,
            &proxy_url,
            email_hint,
        )
        .await
    {
        Ok((auth_url, state)) => Json(SsoLoginStartResp { auth_url, state }).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    }
}

#[derive(Deserialize)]
pub struct SsoLoginFinishPayload {
    pub state: String,
    pub code: String,
}

/// 第二步：回填授权码，换 token 并入池。
pub async fn sso_login_finish(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<SsoLoginFinishPayload>,
) -> Response {
    let Some(pending) = app.kiro_login.peek(&payload.state) else {
        return (
            StatusCode::BAD_REQUEST,
            "登录已过期或 state 无效，请重新获取授权链接",
        )
            .into_response();
    };

    let http = match app.clients.get(&pending.proxy_url) {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("proxy error: {e}")).into_response(),
    };

    let token = match crate::auth::kiro_sso::exchange_code(&http, &pending, &payload.code).await {
        Ok(t) => t,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    app.kiro_login.remove(&payload.state);

    // 推断 provider：以 pending 里记录的为主（start_url 在 KIM 阶段就分类好了）。
    let provider = pending.provider.clone();
    let auth_method = auth_method_for_provider(&provider);
    let normalized_url = crate::auth::kiro::normalize_start_url(&pending.start_url);
    let client_id_hash = crate::auth::kiro::calculate_client_id_hash(&normalized_url);

    let merged = json!({
        "accessToken": token.get("accessToken").or_else(|| token.get("access_token")),
        "refreshToken": token.get("refreshToken").or_else(|| token.get("refresh_token")),
        "expiresIn": token.get("expiresIn").or_else(|| token.get("expires_in")),
        "idToken": token.get("idToken").or_else(|| token.get("id_token")),
        "aws_sso_app_session_id": token.get("aws_sso_app_session_id"),
        "clientId": pending.client_id,
        "clientSecret": pending.client_secret,
        "idcRegion": pending.region,
        "region": pending.region,
        "issuerUrl": pending.start_url,
        "startUrl": normalized_url,
        "client_id_hash": client_id_hash,
        "authMethod": auth_method,
        "provider": provider,
        "email": pending.email_hint,
    });

    let data = match KiroTokenData::from_value(&merged) {
        Ok(d) => d,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("解析 token 失败: {e}")).into_response()
        }
    };
    if data.access_token.is_empty() {
        return (StatusCode::BAD_GATEWAY, "token 交换未返回 accessToken").into_response();
    }

    let acc = match app.kiro_pool.add_or_replace(&data) {
        Ok(a) => a,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    if !pending.proxy_url.trim().is_empty() {
        let _ = app.kiro_pool.set_proxy(&acc.id, pending.proxy_url.clone());
    } else if acc.proxy_url.trim().is_empty() {
        if let Some((_, url)) = app.proxy_pool.next_assignment() {
            let _ = app.kiro_pool.set_proxy(&acc.id, url);
        }
    }

    info!(account = %acc.id, email = %acc.email, provider = %acc.provider, "kiro account added via sso");
    let now = chrono::Utc::now();
    let fresh = app.kiro_pool.get(&acc.id).unwrap_or(acc);
    let view = account_view(&app, fresh, now);
    Json(json!({"ok": true, "account": view})).into_response()
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
