use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// 简单的 admin token 校验中间件。Header: `Authorization: Bearer <admin_token>`
/// 或 `x-admin-token: <admin_token>`。
pub async fn admin_guard(
    State(app): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let token_opt = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").trim().to_string())
        .or_else(|| {
            req.headers()
                .get("x-admin-token")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });

    let Some(token) = token_opt else {
        return (StatusCode::UNAUTHORIZED, "missing admin token").into_response();
    };

    use subtle::ConstantTimeEq;
    let expected = app.config.admin_token.as_bytes();
    let actual = token.as_bytes();
    if expected.len() != actual.len() || !bool::from(expected.ct_eq(actual)) {
        return (StatusCode::UNAUTHORIZED, "invalid admin token").into_response();
    }

    next.run(req).await
}
