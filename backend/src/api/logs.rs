use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::proxy::LogEntry;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
}

fn default_limit() -> usize {
    50
}

#[derive(Serialize)]
pub struct LogsResp {
    pub total: i64,
    pub items: Vec<LogEntry>,
    pub limit: usize,
    pub offset: usize,
}

pub async fn list(State(app): State<Arc<AppState>>, Query(q): Query<LogsQuery>) -> Json<LogsResp> {
    let limit = q.limit.clamp(1, 1000);
    let offset = q.offset;
    let items = app.request_log.snapshot(limit, offset);
    let total = app.request_log.count();
    Json(LogsResp {
        total,
        items,
        limit,
        offset,
    })
}

pub async fn clear(State(app): State<Arc<AppState>>) -> Response {
    app.request_log.clear();
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}
