use std::sync::Arc;

use axum::extract::{Multipart, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::auth::CodexTokenStorage;
use crate::pool::derive_file_name;
use crate::state::AppState;

#[derive(Serialize)]
pub struct AccountView {
    pub id: String,
    pub email: String,
    pub account_id: String,
    pub plan: Option<String>,
    pub enabled: bool,
    pub expire_at: Option<String>,
    pub last_refresh_at: Option<String>,
    pub last_used_at: Option<String>,
    pub failure_count: u32,
    pub cooldown_until: Option<String>,
    pub last_error: Option<String>,
    pub total_requests: u64,
    pub total_failures: u64,
    pub file_path: String,
    pub expired: bool,
    pub proxy_url: String,
    pub proxy_id: Option<String>,
}

pub async fn list(State(app): State<Arc<AppState>>) -> Json<Vec<AccountView>> {
    let now = chrono::Utc::now();
    let accounts = app.pool.list();
    let view: Vec<AccountView> = accounts
        .into_iter()
        .map(|a| AccountView {
            expired: a.expire_at.map(|t| t <= now).unwrap_or(true),
            id: a.id,
            email: a.email,
            account_id: a.account_id,
            plan: a.plan,
            enabled: a.enabled,
            expire_at: a.expire_at.map(|t| t.to_rfc3339()),
            last_refresh_at: a.last_refresh_at.map(|t| t.to_rfc3339()),
            last_used_at: a.last_used_at.map(|t| t.to_rfc3339()),
            failure_count: a.failure_count,
            cooldown_until: a.cooldown_until.map(|t| t.to_rfc3339()),
            last_error: a.last_error,
            total_requests: a.total_requests,
            total_failures: a.total_failures,
            file_path: a.file_path.to_string_lossy().to_string(),
            proxy_id: app.proxy_pool.id_by_url(&a.proxy_url),
            proxy_url: a.proxy_url,
        })
        .collect();
    Json(view)
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
        if !app.pool.set_enabled(&id, enabled) {
            return (StatusCode::NOT_FOUND, "account not found").into_response();
        }
    }
    Json(json!({"ok": true})).into_response()
}

#[derive(Deserialize)]
pub struct SetProxyPayload {
    /// 用 proxy_id 指定代理池里的代理；为空字符串表示直连。
    /// 与 url 二选一。
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

    if let Err(e) = app.pool.set_proxy(&id, url) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(file_path) = app.pool.remove(&id) else {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    };
    if file_path.exists() {
        if let Err(e) = std::fs::remove_file(&file_path) {
            warn!("delete file {:?} failed: {e:?}", file_path);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("delete file failed: {e}"),
            )
                .into_response();
        }
    }
    info!(account = %id, "account removed");
    Json(json!({"ok": true})).into_response()
}

pub async fn manual_refresh(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(acc) = app.pool.get(&id) else {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
    };
    if acc.refresh_token.is_empty() {
        return (StatusCode::BAD_REQUEST, "no refresh_token on file").into_response();
    }
    let storage = CodexTokenStorage {
        id_token: acc.id_token.clone(),
        access_token: acc.access_token.clone(),
        refresh_token: acc.refresh_token.clone(),
        account_id: acc.account_id.clone(),
        last_refresh: acc
            .last_refresh_at
            .map(|t| t.to_rfc3339())
            .unwrap_or_default(),
        email: acc.email.clone(),
        kind: "codex".to_string(),
        expire: acc.expire_at.map(|t| t.to_rfc3339()).unwrap_or_default(),
        proxy_url: acc.proxy_url.clone(),
        extra: acc.raw_extra.clone(),
    };
    match app.refresher.refresh(&storage, &acc.proxy_url).await {
        Ok(new_storage) => {
            if let Err(e) = new_storage.save(&acc.file_path) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("save failed: {e}"),
                )
                    .into_response();
            }
            app.pool.update_after_refresh(&id, &new_storage);
            app.pool.report_success(&id);
            Json(json!({"ok": true, "expire_at": new_storage.expire})).into_response()
        }
        Err(e) => {
            let msg = e.to_string();
            app.pool.mark_refresh_failed(&id, &msg);
            (StatusCode::BAD_GATEWAY, msg).into_response()
        }
    }
}

pub async fn reset_cooldown(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if app.pool.reset_cooldown(&id) {
        Json(json!({"ok": true})).into_response()
    } else {
        (StatusCode::NOT_FOUND, "account not found").into_response()
    }
}

pub async fn reload(State(app): State<Arc<AppState>>) -> Response {
    match app.pool.load_from_disk() {
        Ok(n) => Json(json!({"ok": true, "count": n})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("reload failed: {e}"),
        )
            .into_response(),
    }
}

/// 上传一个或多个 codex-*.json 文件。multipart/form-data，字段名任意。
pub async fn upload(
    State(app): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> Response {
    let mut imported: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    while let Some(field) = match multipart.next_field().await {
        Ok(f) => f,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("multipart error: {e}"))
                .into_response();
        }
    } {
        let original_name = field.file_name().map(|s| s.to_string());
        let bytes = match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                errors.push(format!("read field failed: {e}"));
                continue;
            }
        };

        let storage: CodexTokenStorage = match serde_json::from_slice(&bytes) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!(
                    "{}: invalid json ({e})",
                    original_name.unwrap_or_else(|| "<unnamed>".into())
                ));
                continue;
            }
        };
        let mut storage = storage;

        if storage.access_token.is_empty() || storage.refresh_token.is_empty() {
            errors.push(format!(
                "{}: missing access_token / refresh_token",
                original_name.clone().unwrap_or_else(|| "<unnamed>".into())
            ));
            continue;
        }

        let target_name = match original_name.as_deref() {
            Some(n)
                if n.to_ascii_lowercase().ends_with(".json")
                    && (n.starts_with("codex") || !n.contains('/')) =>
            {
                n.to_string()
            }
            _ => derive_file_name(&storage),
        };
        let target_path = app.config.auth_dir.join(target_name);

        // 自动分配代理：仅当文件本身没带 proxy_url 且不是覆盖一个已有代理的账号时
        if storage.proxy_url.trim().is_empty() {
            let stem = target_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let already_assigned = app
                .pool
                .get(stem)
                .map(|a| !a.proxy_url.is_empty())
                .unwrap_or(false);
            if !already_assigned {
                if let Some((_, url)) = app.proxy_pool.next_assignment() {
                    storage.proxy_url = url;
                }
            }
        }

        if let Err(e) = storage.save(&target_path) {
            errors.push(format!("save {:?}: {e}", target_path));
            continue;
        }
        let acc = app.pool.add_or_replace_from_storage(target_path, &storage);
        imported.push(acc.id);
    }

    let body = json!({
        "imported": imported,
        "errors": errors,
    });
    if imported.is_empty() && !errors.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }
    Json(body).into_response()
}
