use std::sync::Arc;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::auth::CodexTokenStorage;
use crate::pool::{derive_account_id, derive_file_name};
use crate::state::AppState;

/// 上传文件名清洗：拒绝任何带路径分隔符 / 父目录引用 / 隐藏文件 的输入，
/// 只接受看起来像 `codex-*.json` 的纯文件名。返回 None 表示让调用方 fallback。
fn sanitize_upload_filename(raw: Option<&str>) -> Option<String> {
    let name = raw?.trim();
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return None;
    }
    if name == "." || name == ".." || name.starts_with('.') {
        return None;
    }
    let pb = std::path::PathBuf::from(name);
    let file = pb.file_name().and_then(|s| s.to_str())?;
    if file != name {
        return None;
    }
    let lower = file.to_ascii_lowercase();
    if !lower.ends_with(".json") {
        return None;
    }
    if !lower.starts_with("codex") {
        return None;
    }
    Some(file.to_string())
}

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
    pub expired: bool,
    pub proxy_url: String,
    pub proxy_id: Option<String>,
}

#[derive(Serialize)]
pub struct AccountListResp {
    pub total: i64,
    pub items: Vec<AccountView>,
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

pub async fn list(
    State(app): State<Arc<AppState>>,
    Query(q): Query<ListQuery>,
) -> Response {
    let now = chrono::Utc::now();
    let limit = q.limit.clamp(1, 500);
    let offset = q.offset.max(0);
    let search = q.q.as_deref().filter(|s| !s.is_empty());

    let total = match app.pool.count(search) {
        Ok(n) => n,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    let rows = match app.pool.list_page(limit, offset, search) {
        Ok(r) => r,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let items = rows
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
            proxy_id: app.proxy_pool.id_by_url(&a.proxy_url),
            proxy_url: a.proxy_url,
        })
        .collect();

    Json(AccountListResp {
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
        if !app.pool.set_enabled(&id, enabled) {
            return (StatusCode::NOT_FOUND, "account not found").into_response();
        }
    }
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

    if let Err(e) = app.pool.set_proxy(&id, url) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    if app.pool.remove(&id).is_none() {
        return (StatusCode::NOT_FOUND, "account not found").into_response();
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
    let storage = acc.to_storage();
    match app.refresher.refresh(&storage, &acc.proxy_url).await {
        Ok(new_storage) => {
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

/// 把 DB 里所有账号导出成 codex-*.json，写到 auth_dir。供 CLIProxyAPI 互通用。
pub async fn export_to_files(State(app): State<Arc<AppState>>) -> Response {
    let dir = app.config.auth_dir.clone();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("create dir failed: {e}"),
        )
            .into_response();
    }
    // 一次性拿全部 ID（DB 主键索引很快）
    let ids = app.pool.all_ids_sorted();
    let mut written = 0usize;
    let mut errors: Vec<String> = Vec::new();
    for id in ids {
        let Some(acc) = app.pool.get(&id) else {
            continue;
        };
        let storage = acc.to_storage();
        let filename = derive_file_name(&storage);
        let target = dir.join(filename);
        if let Err(e) = storage.save(&target) {
            errors.push(format!("{id}: {e}"));
            continue;
        }
        written += 1;
    }
    Json(json!({"ok": true, "written": written, "errors": errors})).into_response()
}

/// 上传 codex-*.json，写入 DB（不再落 auths/ 目录；要互通调用 export 接口）。
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

        // ID 优先用上传文件名（去掉 .json 后缀），否则按 derive_account_id 生成
        let id_from_name = sanitize_upload_filename(original_name.as_deref())
            .map(|n| n.trim_end_matches(".json").to_string());
        let id = id_from_name.unwrap_or_else(|| derive_account_id(&storage));

        // 文件本身没带 proxy_url 且 DB 里这条 ID 还没绑过代理时，自动分配
        if storage.proxy_url.trim().is_empty() {
            let already_assigned = app
                .pool
                .get(&id)
                .map(|a| !a.proxy_url.is_empty())
                .unwrap_or(false);
            if !already_assigned {
                if let Some((_, url)) = app.proxy_pool.next_assignment() {
                    storage.proxy_url = url;
                }
            }
        }

        match app.pool.add_or_replace_from_storage(id.clone(), &storage) {
            Ok(acc) => imported.push(acc.id),
            Err(e) => errors.push(format!("{id}: {e}")),
        }
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
