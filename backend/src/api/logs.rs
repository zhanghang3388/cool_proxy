use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::proxy::LogEntry;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// 加载更多：传上一页最小 id，返回 id < before_id 的记录
    #[serde(default)]
    pub before_id: Option<i64>,
}

fn default_limit() -> usize {
    100
}

pub async fn list(
    State(app): State<Arc<AppState>>,
    Query(q): Query<LogsQuery>,
) -> Json<Vec<LogEntry>> {
    Json(app.request_log.snapshot(q.limit.clamp(1, 1000), q.before_id))
}

pub async fn clear(State(app): State<Arc<AppState>>) -> Response {
    app.request_log.clear();
    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}
