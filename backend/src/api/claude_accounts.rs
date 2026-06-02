//! Claude 账号池管理面板接口。镜像 `api/kiro_accounts.rs`，去掉额度 / 上传 / 粘贴导入，
//! 改成两步 OAuth 登录（拿授权链接 → 回填授权码）。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::auth::claude::exchange_code;
use crate::auth::claude_quota::fetch_claude_quota;
use crate::state::AppState;
use crate::store::claude_accounts::{ClaudeAccountQuotaUpdate, ClaudeAccountRow};

// ===== 视图模型 =====

/// 单个额度窗口（5h / week）。`remaining_percent = 100 - used_percent`。
#[derive(Serialize)]
pub struct QuotaWindowView {
    pub used_percent: Option<f64>,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<String>,
}

/// 账号额度视图：5 小时 + 7 天窗口 + 查询时刻 / 错误。
#[derive(Serialize)]
pub struct ClaudeQuotaView {
    pub five_hour: Option<QuotaWindowView>,
    pub week: Option<QuotaWindowView>,
    pub checked_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ClaudeAccountView {
    pub id: String,
    pub email: String,
    pub org_name: Option<String>,
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
    pub quota: ClaudeQuotaView,
}

fn quota_window_view(
    used: Option<f64>,
    reset_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Option<QuotaWindowView> {
    if used.is_none() && reset_at.is_none() {
        return None;
    }
    Some(QuotaWindowView {
        used_percent: used,
        remaining_percent: used.map(|u| (100.0 - u).clamp(0.0, 100.0)),
        reset_at: reset_at.map(|t| t.to_rfc3339()),
    })
}

fn quota_view(a: &ClaudeAccountRow) -> ClaudeQuotaView {
    ClaudeQuotaView {
        five_hour: quota_window_view(a.quota_5h_used_percent, a.quota_5h_reset_at),
        week: quota_window_view(a.quota_week_used_percent, a.quota_week_reset_at),
        checked_at: a.quota_checked_at.map(|t| t.to_rfc3339()),
        error: a.quota_error.clone(),
    }
}

fn account_view(
    app: &Arc<AppState>,
    a: ClaudeAccountRow,
    now: chrono::DateTime<chrono::Utc>,
) -> ClaudeAccountView {
    let quota = quota_view(&a);
    ClaudeAccountView {
        expired: a.expires_at.map(|t| t <= now).unwrap_or(true),
        proxy_id: app.proxy_pool.id_by_url(&a.proxy_url),
        id: a.id,
        email: a.email,
        org_name: a.org_name,
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
        quota,
    }
}

#[derive(Serialize)]
pub struct ClaudeAccountListResp {
    pub total: i64,
    pub items: Vec<ClaudeAccountView>,
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

pub async fn list(State(app): State<Arc<AppState>>, Query(q): Query<ListQuery>) -> Response {
    let now = chrono::Utc::now();
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let search = q.q.as_deref().filter(|s| !s.is_empty());

    let total = match app.claude_pool.count(search) {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = match app.claude_pool.list_page(limit, offset, search) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let items = rows
        .into_iter()
        .map(|a| account_view(&app, a, now))
        .collect();
    Json(ClaudeAccountListResp {
        total,
        items,
        limit,
        offset,
    })
    .into_response()
}

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
        if !app.claude_pool.set_enabled(&id, enabled) {
            return (StatusCode::NOT_FOUND, "account not found").into_response();
        }
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if app.claude_pool.remove(&id).is_none() {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    }
    info!(account = %id, "claude account removed");
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
    if let Err(e) = app.claude_pool.set_proxy(&id, url) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn reset_cooldown(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    if app.claude_pool.reset_cooldown(&id) {
        Json(json!({"ok": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, "account not found").into_response()
    }
}

pub async fn manual_refresh(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let Some(acc) = app.claude_pool.get(&id) else {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    };
    if acc.refresh_token.is_empty() {
        return (StatusCode::BAD_REQUEST, "no refresh_token on file").into_response();
    }
    match app.claude_refresher.refresh(&acc).await {
        Ok(update) => {
            app.claude_pool.update_after_refresh(&id, &update);
            app.claude_pool.report_success(&id);
            let expire = update.expires_at.map(|t| t.to_rfc3339());
            Json(json!({"ok": true, "expire_at": expire})).into_response()
        }
        Err(e) => {
            let msg = format!("{e:#}");
            app.claude_pool.mark_refresh_failed(&id, &msg);
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

// ===== 额度查询（仅手动；/api/oauth/usage 会激进 429，绝不后台轮询）=====

#[derive(Serialize)]
pub struct QuotaRefreshItem {
    pub id: String,
    pub ok: bool,
    pub quota: Option<ClaudeQuotaView>,
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

async fn refresh_one_quota(app: Arc<AppState>, id: String) -> QuotaRefreshItem {
    let Some(acc) = app.claude_pool.get(&id) else {
        return QuotaRefreshItem {
            id,
            ok: false,
            quota: None,
            error: Some("account not found".to_string()),
        };
    };
    if acc.access_token.is_empty() {
        let msg = "缺少 access_token，无法查询额度".to_string();
        app.claude_pool.update_quota_error(&id, &msg);
        let quota = app.claude_pool.get(&id).map(|a| quota_view(&a));
        return QuotaRefreshItem {
            id,
            ok: false,
            quota,
            error: Some(msg),
        };
    }

    let result = fetch_claude_quota(&app.clients, &acc.access_token, &acc.proxy_url).await;
    let (ok, error) = match result {
        Ok(snapshot) => {
            let update = ClaudeAccountQuotaUpdate {
                quota_5h_used_percent: snapshot.five_hour.as_ref().and_then(|w| w.used_percent),
                quota_5h_reset_at: snapshot.five_hour.as_ref().and_then(|w| w.reset_at),
                quota_week_used_percent: snapshot.week.as_ref().and_then(|w| w.used_percent),
                quota_week_reset_at: snapshot.week.as_ref().and_then(|w| w.reset_at),
                quota_error: None,
            };
            app.claude_pool.update_quota(&id, &update);
            (true, None)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            app.claude_pool.update_quota_error(&id, &msg);
            (false, Some(msg))
        }
    };

    let quota = app.claude_pool.get(&id).map(|a| quota_view(&a));
    QuotaRefreshItem {
        id,
        ok,
        quota,
        error,
    }
}

pub async fn refresh_quota(State(app): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    let item = refresh_one_quota(app, id).await;
    if item.quota.is_none() && item.error.as_deref() == Some("account not found") {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    }
    Json(item).into_response()
}

pub async fn refresh_quotas(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<QuotaRefreshPayload>,
) -> Response {
    let ids = if payload.ids.is_empty() {
        app.claude_pool.all_ids_sorted()
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

// ===== OAuth 登录（两步）=====

#[derive(Serialize)]
pub struct LoginStartResp {
    pub auth_url: String,
    pub state: String,
}

/// 第一步：生成授权链接 + state。
pub async fn login_start(State(app): State<Arc<AppState>>) -> Response {
    match app.claude_login.start() {
        Ok((auth_url, state)) => Json(LoginStartResp { auth_url, state }).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct LoginFinishPayload {
    pub state: String,
    pub code: String,
    /// 可选：登录及后续请求走的代理（id 优先，其次 url）。
    #[serde(default)]
    pub proxy_id: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// 第二步：回填授权码，换 token 并入库。
pub async fn login_finish(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<LoginFinishPayload>,
) -> Response {
    // peek 不消费：换 token 失败时保留 verifier，用户改正授权码后可直接重试。
    let Some(verifier) = app.claude_login.peek(&payload.state) else {
        return (
            StatusCode::BAD_REQUEST,
            "登录已过期或 state 无效，请重新获取授权链接",
        )
            .into_response();
    };

    // 解析代理（用于本次 token 交换 + 绑定到账号）。
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

    let data = match exchange_code(&http, &payload.code, &payload.state, &verifier).await {
        Ok(d) => d,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("{e:#}")).into_response(),
    };
    // 换 token 成功，消费掉这次登录的 state。
    app.claude_login.remove(&payload.state);

    let acc = match app.claude_pool.add_or_replace(&data) {
        Ok(a) => a,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    // 绑定代理：显式选了就用它，否则自动分配一个。
    if !proxy_url.trim().is_empty() {
        let _ = app.claude_pool.set_proxy(&acc.id, proxy_url);
    } else if acc.proxy_url.trim().is_empty() {
        if let Some((_, url)) = app.proxy_pool.next_assignment() {
            let _ = app.claude_pool.set_proxy(&acc.id, url);
        }
    }

    info!(account = %acc.id, email = %acc.email, "claude account added via oauth");
    let now = chrono::Utc::now();
    let view = account_view(&app, app.claude_pool.get(&acc.id).unwrap_or(acc), now);
    Json(json!({"ok": true, "account": view})).into_response()
}

// ===== 重新分配代理（与 codex 一致：从共享代理池 round-robin，可复用、不独占）=====

#[derive(Deserialize)]
pub struct RebalancePayload {
    /// only_unassigned=true 仅给当前没绑代理的账号分配；
    /// false 表示把所有 Claude 账号重新轮询分配（覆盖现有绑定）。
    #[serde(default)]
    pub only_unassigned: bool,
}

#[derive(Serialize)]
pub struct RebalanceResult {
    pub assigned: usize,
    pub skipped_no_proxies: bool,
    pub failed: Vec<String>,
}

pub async fn rebalance(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<RebalancePayload>,
) -> Response {
    let proxies = app.proxy_pool.list();
    if proxies.is_empty() {
        return Json(RebalanceResult {
            assigned: 0,
            skipped_no_proxies: true,
            failed: vec![],
        })
        .into_response();
    }

    let ids = if payload.only_unassigned {
        app.claude_pool.unassigned_ids()
    } else {
        app.claude_pool.all_ids_sorted()
    };

    let mut assigned = 0;
    let mut failed = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let url = proxies[i % proxies.len()].url.clone();
        match app.claude_pool.set_proxy(id, url) {
            Ok(()) => assigned += 1,
            Err(e) => failed.push(format!("{id}: {e}")),
        }
    }
    info!(
        assigned,
        only_unassigned = payload.only_unassigned,
        "claude rebalance done"
    );
    Json(RebalanceResult {
        assigned,
        skipped_no_proxies: false,
        failed,
    })
    .into_response()
}

// ===== 统计 =====

#[derive(Serialize)]
pub struct ClaudeStatsView {
    pub total_accounts: usize,
    pub enabled_accounts: usize,
    pub cooling_down: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}

pub async fn stats(State(app): State<Arc<AppState>>) -> Response {
    match app.claude_pool.stats_overview() {
        Ok(s) => Json(ClaudeStatsView {
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
