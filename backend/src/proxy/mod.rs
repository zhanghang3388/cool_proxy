use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::pool::PoolError;
use crate::state::AppState;

pub mod clients;
pub mod log;
pub use clients::ProxiedClients;
pub use log::{LogEntry, RequestLog};

const CODEX_USER_AGENT: &str = "codex_cli_rs/0.118.0 (Mac OS 26.3.1; arm64) iTerm.app/3.6.9";
const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// 不应透传到上游或下游的 hop-by-hop / 内部 header
const STRIP_REQ_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "connection",
    "proxy-connection",
    "proxy-authorization",
    "transfer-encoding",
    "upgrade",
    "te",
    "keep-alive",
    "trailer",
    "content-length",
    "expect",
    "openai-organization",
    "chatgpt-account-id",
];

const STRIP_RESP_HEADERS: &[&str] = &[
    "connection",
    "transfer-encoding",
    "upgrade",
    "keep-alive",
    "trailer",
    "te",
    "proxy-connection",
];

/// 校验下游客户端带的 api key（兼容 Bearer / x-api-key 两种）
pub fn verify_client_key(headers: &HeaderMap, allowed: &[String]) -> bool {
    let candidates = [
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim_start_matches("Bearer ").trim().to_string()),
        headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string()),
    ];
    for c in candidates.into_iter().flatten() {
        if c.is_empty() {
            continue;
        }
        for k in allowed {
            if constant_time_eq(c.as_bytes(), k.as_bytes()) {
                return true;
            }
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.ct_eq(b).into()
}

/// 主入口：把任意 /v1/* 请求转发到 chatgpt.com/backend-api/codex/<rest>
pub async fn proxy_handler(
    State(app): State<Arc<AppState>>,
    req: Request,
) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return (StatusCode::UNAUTHORIZED, "missing or invalid api key").into_response();
    }

    let upstream_path = match extract_upstream_path(req.uri()) {
        Some(p) => p,
        None => {
            return (StatusCode::BAD_REQUEST, "invalid path").into_response();
        }
    };

    // 读取请求体（一次性）。Codex 请求体一般不大，简单实现先这样。
    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("read request body: {e}"),
            )
                .into_response();
        }
    };

    let max_attempts = app.config.retry.max_retries.max(1);
    let mut last_error: Option<(StatusCode, String)> = None;
    let mut last_account: Option<String> = None;
    let started = Instant::now();

    for attempt in 0..max_attempts {
        let selected = match app.pool.pick() {
            Ok(s) => s,
            Err(PoolError::Empty) => {
                app.request_log.push(
                    &parts.method,
                    &upstream_path,
                    None,
                    503,
                    started.elapsed().as_millis() as u64,
                    attempt + 1,
                    Some("no accounts".into()),
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no codex accounts configured",
                )
                    .into_response();
            }
            Err(PoolError::AllUnavailable) => {
                app.request_log.push(
                    &parts.method,
                    &upstream_path,
                    None,
                    503,
                    started.elapsed().as_millis() as u64,
                    attempt + 1,
                    Some("all unavailable".into()),
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "all accounts cooling down or disabled",
                )
                    .into_response();
            }
        };
        last_account = Some(selected.id.clone());

        debug!(
            attempt,
            account = %selected.id,
            method = %parts.method,
            path = %upstream_path,
            "forwarding"
        );

        let res = forward_once(
            &app.clients,
            &app.config.upstream.base_url,
            &upstream_path,
            &parts.method,
            &parts.headers,
            &body_bytes,
            &selected.access_token,
            &selected.account_id,
            &selected.proxy_url,
        )
        .await;

        match res {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status == StatusCode::NOT_MODIFIED {
                    app.pool.report_success(&selected.id);
                    app.request_log.push(
                        &parts.method,
                        &upstream_path,
                        Some(selected.id.clone()),
                        status.as_u16(),
                        started.elapsed().as_millis() as u64,
                        attempt + 1,
                        None,
                    );
                    return resp;
                }
                if should_retry(status) {
                    let snippet = "(see upstream body)";
                    warn!(
                        account = %selected.id,
                        status = %status,
                        "request failed, will retry with another account"
                    );
                    app.pool.report_failure(&selected.id, status.as_u16(), snippet);
                    last_error = Some((status, format!("upstream {status}")));
                    continue;
                }
                // 4xx 客户端错误（不是账号问题）直接返给调用方
                app.request_log.push(
                    &parts.method,
                    &upstream_path,
                    Some(selected.id.clone()),
                    status.as_u16(),
                    started.elapsed().as_millis() as u64,
                    attempt + 1,
                    None,
                );
                return resp;
            }
            Err(e) => {
                error!(account = %selected.id, "forward error: {e:?}");
                app.pool
                    .report_failure(&selected.id, 0, &format!("network: {e}"));
                last_error = Some((
                    StatusCode::BAD_GATEWAY,
                    format!("upstream network error: {e}"),
                ));
                continue;
            }
        }
    }

    let (status, msg) = last_error.unwrap_or((
        StatusCode::BAD_GATEWAY,
        "all retries failed".to_string(),
    ));
    info!("proxy giving up: {status} {msg}");
    app.request_log.push(
        &parts.method,
        &upstream_path,
        last_account,
        status.as_u16(),
        started.elapsed().as_millis() as u64,
        max_attempts,
        Some(msg.clone()),
    );
    (status, msg).into_response()
}

fn extract_upstream_path(uri: &Uri) -> Option<String> {
    // 入站路径形如 "/v1/chat/completions?x=y"，原样转给上游
    let pq = uri.path_and_query()?;
    Some(pq.as_str().to_string())
}

fn should_retry(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        401 | 403 | 408 | 425 | 429 | 500 | 502 | 503 | 504
    )
}

#[allow(clippy::too_many_arguments)]
async fn forward_once(
    clients: &ProxiedClients,
    base_url: &str,
    upstream_path: &str,
    method: &Method,
    in_headers: &HeaderMap,
    body: &Bytes,
    access_token: &str,
    account_id: &str,
    proxy_url: &str,
) -> anyhow::Result<Response> {
    let url = format!("{}{}", base_url.trim_end_matches('/'), upstream_path);
    let http = clients.get(proxy_url)?;

    let mut req = http
        .request(method.clone(), &url)
        .timeout(Duration::from_secs(600));

    // 复制下游 header（去掉黑名单）
    for (k, v) in in_headers.iter() {
        let name = k.as_str().to_ascii_lowercase();
        if STRIP_REQ_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if name.starts_with("x-cool-") {
            continue;
        }
        req = req.header(k.as_str(), v);
    }

    req = req
        .header("Authorization", format!("Bearer {access_token}"))
        .header("OpenAI-Beta", "responses=v1")
        .header("Originator", CODEX_ORIGINATOR)
        .header("User-Agent", CODEX_USER_AGENT)
        .header("Connection", "Keep-Alive");

    // CLIProxyAPI 在 UA 含 "Mac OS" 时为每个请求生成一个 Session_id
    if !in_headers
        .get("session_id")
        .or_else(|| in_headers.get("Session_id"))
        .is_some()
    {
        req = req.header("Session_id", Uuid::new_v4().to_string());
    }

    if !account_id.is_empty() {
        req = req.header("Chatgpt-Account-Id", account_id);
    }

    if !body.is_empty() {
        req = req.body(body.clone());
    }

    let upstream = req.send().await?;
    let status = upstream.status();
    let mut headers = HeaderMap::new();
    for (k, v) in upstream.headers().iter() {
        let name_lower = k.as_str().to_ascii_lowercase();
        if STRIP_RESP_HEADERS.contains(&name_lower.as_str()) {
            continue;
        }
        if let (Ok(n), Ok(val)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            headers.insert(n, val);
        }
    }

    let stream = upstream.bytes_stream().map(|res| {
        res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    });

    let body = Body::from_stream(stream);
    let mut resp = Response::builder()
        .status(status)
        .body(body)
        .map_err(|e| anyhow::anyhow!("build response: {e}"))?;
    *resp.headers_mut() = headers;
    Ok(resp)
}
