use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::state::AppState;
use crate::store::requests as store_requests;

#[derive(Deserialize)]
pub struct UsageQuery {
    /// 起始时间（unix ms），不传 = 全量
    #[serde(default)]
    pub from_ms: Option<i64>,
    /// 截止时间（unix ms）
    #[serde(default)]
    pub to_ms: Option<i64>,
}

pub async fn report(
    State(app): State<Arc<AppState>>,
    Query(q): Query<UsageQuery>,
) -> Response {
    match store_requests::usage(&app.db, q.from_ms, q.to_ms) {
        Ok(r) => Json(r).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
