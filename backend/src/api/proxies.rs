use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use crate::proxy_pool::ProxyEntry;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct CreatePayload {
    pub url: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Deserialize)]
pub struct UpdatePayload {
    pub url: Option<String>,
    pub label: Option<String>,
}

pub async fn list(State(app): State<Arc<AppState>>) -> Json<Vec<ProxyEntry>> {
    Json(app.proxy_pool.list())
}

pub async fn create(
    State(app): State<Arc<AppState>>,
    Json(payload): Json<CreatePayload>,
) -> Response {
    match app.proxy_pool.add(payload.url, payload.label) {
        Ok(entry) => {
            info!(proxy_id = %entry.id, "proxy added");
            (StatusCode::CREATED, Json(entry)).into_response()
        }
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub async fn update(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(payload): Json<UpdatePayload>,
) -> Response {
    // 编辑前先拿到旧 url，编辑后让连接池失效，避免脏连接复用
    let old_url = app
        .proxy_pool
        .list()
        .into_iter()
        .find(|p| p.id == id)
        .map(|p| p.url);
    if let Err(e) = app.proxy_pool.update(&id, payload.url, payload.label) {
        return (StatusCode::BAD_REQUEST, e.to_string()).into_response();
    }
    if let Some(u) = old_url {
        app.clients.invalidate(&u);
    }
    Json(json!({"ok": true})).into_response()
}

pub async fn delete_one(
    State(app): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let url = app.proxy_pool.url_by_id(&id);
    match app.proxy_pool.remove(&id) {
        Ok(true) => {
            if let Some(u) = url {
                app.clients.invalidate(&u);
            }
            Json(json!({"ok": true})).into_response()
        }
        Ok(false) => (StatusCode::NOT_FOUND, "proxy not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct RebalancePayload {
    /// only_unassigned=true 仅给当前没绑代理的账号分配，
    /// 默认 false 表示把所有账号重新轮询分配（破坏现有绑定）。
    #[serde(default)]
    pub only_unassigned: bool,
}

#[derive(serde::Serialize)]
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
        app.pool.unassigned_ids()
    } else {
        app.pool.all_ids_sorted()
    };

    let mut assigned = 0;
    let mut failed = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let url = proxies[i % proxies.len()].url.clone();
        match app.pool.set_proxy(id, url) {
            Ok(()) => assigned += 1,
            Err(e) => failed.push(format!("{id}: {e}")),
        }
    }
    info!(
        assigned,
        only_unassigned = payload.only_unassigned,
        "rebalance done"
    );
    Json(RebalanceResult {
        assigned,
        skipped_no_proxies: false,
        failed,
    })
    .into_response()
}
