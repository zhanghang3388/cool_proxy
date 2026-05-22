use std::sync::Arc;

use axum::extract::State;
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

pub async fn overview(State(app): State<Arc<AppState>>) -> Json<StatsView> {
    let now = chrono::Utc::now();
    let accounts = app.pool.list();
    let mut total_requests = 0u64;
    let mut total_failures = 0u64;
    let mut enabled = 0usize;
    let mut cooling = 0usize;
    let mut expired = 0usize;
    for a in &accounts {
        total_requests = total_requests.saturating_add(a.total_requests);
        total_failures = total_failures.saturating_add(a.total_failures);
        if a.enabled {
            enabled += 1;
        }
        if let Some(t) = a.cooldown_until {
            if t > now {
                cooling += 1;
            }
        }
        if a.expire_at.map(|t| t <= now).unwrap_or(true) {
            expired += 1;
        }
    }
    Json(StatsView {
        total_accounts: accounts.len(),
        enabled_accounts: enabled,
        cooling_down: cooling,
        expired,
        total_requests,
        total_failures,
    })
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
