use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::json;

use crate::state::AppState;

#[derive(Serialize)]
pub struct StatsView {
    pub total_accounts: usize,
    pub enabled_accounts: usize,
    pub cooling_down: usize,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}

pub async fn overview(State(app): State<Arc<AppState>>) -> Response {
    match app.pool.stats_overview() {
        Ok(s) => Json(StatsView {
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

pub async fn current_config(State(app): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "host": app.config.host,
        "port": app.config.port,
        "auth_dir": app.config.auth_dir,
        "upstream": app.config.upstream,
        "retry": app.config.retry,
        "token_refresh": app.config.token_refresh,
        "api_keys_count": app.config.api_keys.len(),
    }))
}
