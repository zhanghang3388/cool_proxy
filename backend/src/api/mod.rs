pub mod accounts;
pub mod auth;
pub mod kiro_accounts;
pub mod logs;
pub mod proxies;
pub mod stats;
pub mod usage;

use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post, put};
use axum::Router;

use crate::state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/accounts", get(accounts::list).post(accounts::upload))
        .route("/accounts/import", post(accounts::import_json))
        .route("/accounts/quota/refresh", post(accounts::refresh_quotas))
        .route(
            "/accounts/:id",
            delete(accounts::delete_one).patch(accounts::patch_one),
        )
        .route("/accounts/:id/refresh", post(accounts::manual_refresh))
        .route("/accounts/:id/quota", post(accounts::refresh_quota))
        .route(
            "/accounts/:id/reset-cooldown",
            post(accounts::reset_cooldown),
        )
        .route("/accounts/:id/proxy", put(accounts::set_proxy))
        .route("/accounts/reload", post(accounts::reload))
        .route("/accounts/export", post(accounts::export_to_files))
        .route("/proxies", get(proxies::list).post(proxies::create))
        .route(
            "/proxies/:id",
            delete(proxies::delete_one).patch(proxies::update),
        )
        .route("/proxies/rebalance", post(proxies::rebalance))
        .route("/proxies/:id/test", post(proxies::test_one))
        .route("/stats", get(stats::overview))
        .route("/usage", get(usage::report))
        .route("/config", get(stats::current_config))
        .route("/logs", get(logs::list).delete(logs::clear))
        // ===== Kiro 账号池 =====
        .route(
            "/kiro/accounts",
            get(kiro_accounts::list).post(kiro_accounts::upload),
        )
        .route("/kiro/accounts/import", post(kiro_accounts::import_json))
        .route(
            "/kiro/accounts/quota/refresh",
            post(kiro_accounts::refresh_quotas),
        )
        .route(
            "/kiro/accounts/:id",
            delete(kiro_accounts::delete_one).patch(kiro_accounts::patch_one),
        )
        .route(
            "/kiro/accounts/:id/refresh",
            post(kiro_accounts::manual_refresh),
        )
        .route(
            "/kiro/accounts/:id/quota",
            post(kiro_accounts::refresh_quota),
        )
        .route(
            "/kiro/accounts/:id/reset-cooldown",
            post(kiro_accounts::reset_cooldown),
        )
        .route("/kiro/accounts/:id/proxy", put(kiro_accounts::set_proxy))
        .route("/kiro/stats", get(kiro_accounts::stats))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::admin_guard,
        ))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}
