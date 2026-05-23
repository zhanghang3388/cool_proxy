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

use crate::pool::{PoolError, ReportContext};
use crate::state::AppState;

pub mod clients;
pub mod error_class;
pub mod log;
pub use clients::ProxiedClients;
pub use error_class::{classify, quota_backoff, ErrorKind};
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

    // 从请求体里提一下 model 字段，便于日志归类（解析失败就为 None）
    let model: Option<String> = serde_json::from_slice::<serde_json::Value>(&body_bytes)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(String::from));

    for attempt in 0..max_attempts {
        let selected = match app.pool.pick_for(model.as_deref().unwrap_or("")) {
            Ok(s) => s,
            Err(PoolError::Empty) => {
                app.request_log.push(
                    &parts.method,
                    &upstream_path,
                    None,
                    model.clone(),
                    503,
                    started.elapsed().as_millis() as u64,
                    attempt + 1,
                    None,
                    None,
                    None,
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
                    model.clone(),
                    503,
                    started.elapsed().as_millis() as u64,
                    attempt + 1,
                    None,
                    None,
                    None,
                    Some("all unavailable".into()),
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "all accounts cooling down or disabled",
                )
                    .into_response();
            }
            Err(PoolError::Db(e)) => {
                error!("pick from db failed: {e:?}");
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("db error: {e}"))
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
            Ok((mut resp, retry_after)) => {
                let status = resp.status();
                if status.is_success() || status == StatusCode::NOT_MODIFIED {
                    let model_for_state = model.clone().unwrap_or_default();
                    app.pool.report_success_for(&selected.id, &model_for_state);
                    let original_body = std::mem::replace(resp.body_mut(), Body::empty());
                    let log = app.request_log.clone();
                    let method = parts.method.clone();
                    let path_log = upstream_path.clone();
                    let acct_id = selected.id.clone();
                    let model_l = model.clone();
                    let status_code = status.as_u16();
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let attempts_n = attempt + 1;
                    let teed = tee_usage(original_body, move |tail| {
                        let (input, output, total) = parse_usage(&tail);
                        log.push(
                            &method,
                            &path_log,
                            Some(acct_id),
                            model_l,
                            status_code,
                            elapsed_ms,
                            attempts_n,
                            input,
                            output,
                            total,
                            None,
                        );
                    });
                    *resp.body_mut() = teed;
                    return resp;
                }

                // 失败响应：先吃掉前几百字节做 last_error，再分类处理
                let body = std::mem::replace(resp.body_mut(), Body::empty());
                let snippet = match axum::body::to_bytes(body, 4 * 1024).await {
                    Ok(b) if !b.is_empty() => {
                        let s = String::from_utf8_lossy(&b).into_owned();
                        truncate_snippet(&s, 300)
                    }
                    _ => format!("upstream {status}"),
                };
                let model_str = model.clone().unwrap_or_default();
                let kind = app.pool.report(ReportContext {
                    id: &selected.id,
                    model: &model_str,
                    status: Some(status.as_u16()),
                    retry_after,
                    message: &snippet,
                });

                match kind {
                    crate::proxy::ErrorKind::Auth => {
                        // access_token 失效：后台异步 refresh，本次换号继续
                        spawn_refresh(app.clone(), selected.id.clone());
                        warn!(
                            account = %selected.id,
                            status = %status,
                            "auth error, refresh spawned, switching account"
                        );
                        last_error = Some((status, format!("upstream {status}: {snippet}")));
                        continue;
                    }
                    crate::proxy::ErrorKind::Client => {
                        // 客户端错误：直接返给调用方
                        app.request_log.push(
                            &parts.method,
                            &upstream_path,
                            Some(selected.id.clone()),
                            model.clone(),
                            status.as_u16(),
                            started.elapsed().as_millis() as u64,
                            attempt + 1,
                            None,
                            None,
                            None,
                            None,
                        );
                        // resp.body 已经被消耗，重新组一个携带 snippet 的响应
                        return (status, snippet).into_response();
                    }
                    crate::proxy::ErrorKind::Quota
                    | crate::proxy::ErrorKind::NotFound
                    | crate::proxy::ErrorKind::Transient
                    | crate::proxy::ErrorKind::Network => {
                        warn!(
                            account = %selected.id,
                            status = %status,
                            kind = kind.label(),
                            "upstream error, will retry with another account"
                        );
                        last_error = Some((status, format!("upstream {status}: {snippet}")));
                        continue;
                    }
                }
            }
            Err(e) => {
                error!(account = %selected.id, "forward error: {e:?}");
                let model_str = model.clone().unwrap_or_default();
                let msg = format!("network: {e}");
                app.pool.report(ReportContext {
                    id: &selected.id,
                    model: &model_str,
                    status: None,
                    retry_after: None,
                    message: &msg,
                });
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
        model,
        status.as_u16(),
        started.elapsed().as_millis() as u64,
        max_attempts,
        None,
        None,
        None,
        Some(msg.clone()),
    );
    (status, msg).into_response()
}

fn extract_upstream_path(uri: &Uri) -> Option<String> {
    // 入站路径形如 "/v1/chat/completions?x=y"，原样转给上游
    let pq = uri.path_and_query()?;
    Some(pq.as_str().to_string())
}

/// 解析上游响应里的 Retry-After 头。支持秒数和 HTTP-date 两种形式（这里只识别秒数）。
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get("retry-after")?;
    let s = v.to_str().ok()?.trim();
    s.parse::<u64>().ok().map(Duration::from_secs)
}

/// 401/auth 类失败：后台异步刷一次 token；失败则在账号上写 30min 冷却避免风暴。
fn spawn_refresh(app: Arc<AppState>, id: String) {
    tokio::spawn(async move {
        let Some(acc) = app.pool.get(&id) else {
            return;
        };
        if acc.refresh_token.is_empty() {
            app.pool.mark_auth_dead(&id, "no refresh_token on file");
            return;
        }
        let storage = acc.to_storage();
        match app.refresher.refresh(&storage, &acc.proxy_url).await {
            Ok(new_storage) => {
                app.pool.update_after_refresh(&id, &new_storage);
                app.pool.report_success(&id);
                info!(account = %id, "token refreshed (auth-triggered)");
            }
            Err(e) => {
                let msg = e.to_string();
                warn!(account = %id, "auth-triggered refresh failed: {msg}");
                app.pool.mark_auth_dead(&id, &msg);
            }
        }
    });
}

/// 把上游错误体做成可读的简短 snippet，去掉换行、限制长度，避免日志/UI 里塞进 KB 级内容。
fn truncate_snippet(raw: &str, max: usize) -> String {
    let collapsed: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max).collect();
    out.push_str("...");
    out
}

/// SSE / JSON 响应体里挖 usage：保留尾部 32KB（够覆盖 response.completed），
/// stream 结束时把尾部交给回调。回调里只允许做轻量 JSON 解析 + DB 写入。
fn tee_usage<F>(body: Body, on_done: F) -> Body
where
    F: FnOnce(bytes::BytesMut) + Send + 'static,
{
    use futures_util::stream::StreamExt;
    const TAIL_CAP: usize = 32 * 1024;
    let mut tail = bytes::BytesMut::with_capacity(TAIL_CAP);
    let mut stream = body.into_data_stream();
    let mut callback = Some(on_done);
    let s = async_stream::stream! {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    // 累积尾部，超过 cap 就丢掉前面的（usage 一定在最后一个 SSE 事件里）
                    if tail.len() + b.len() > TAIL_CAP {
                        let drop = tail.len() + b.len() - TAIL_CAP;
                        let drop = drop.min(tail.len());
                        let _ = tail.split_to(drop);
                    }
                    if b.len() >= TAIL_CAP {
                        tail.clear();
                        let start = b.len() - TAIL_CAP;
                        tail.extend_from_slice(&b[start..]);
                    } else {
                        tail.extend_from_slice(&b);
                    }
                    yield Ok::<_, std::io::Error>(b);
                }
                Err(e) => {
                    yield Err(std::io::Error::new(std::io::ErrorKind::Other, e));
                    break;
                }
            }
        }
        if let Some(cb) = callback.take() {
            cb(tail);
        }
    };
    Body::from_stream(s)
}

/// 从尾部 bytes 里解析 codex `/responses` 的 usage 字段。
/// 流式：寻找最后一个 `data: {...}` 里的 `response.usage` 或 `usage`；
/// 非流式 JSON：直接当 JSON 解析根级。
/// 返回 (input_tokens, output_tokens, total_tokens)，任何字段缺失则该字段为 None。
fn parse_usage(tail: &[u8]) -> (Option<i64>, Option<i64>, Option<i64>) {
    let s = std::str::from_utf8(tail).unwrap_or("");
    if s.is_empty() {
        return (None, None, None);
    }
    // 1) 尝试整体 JSON（非流式 / 完整短响应）
    if let Some(u) = extract_usage_from_json_str(s) {
        return u;
    }
    // 2) SSE：从后往前找最近的 data: 行
    for line in s.lines().rev() {
        let line = line.trim_start();
        let payload = line.strip_prefix("data:").map(|p| p.trim_start());
        if let Some(p) = payload {
            if p.starts_with('{') {
                if let Some(u) = extract_usage_from_json_str(p) {
                    return u;
                }
            }
        }
    }
    (None, None, None)
}

fn extract_usage_from_json_str(s: &str) -> Option<(Option<i64>, Option<i64>, Option<i64>)> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    // codex 流式 response.completed: {"response": {"usage": {...}}}
    let usage = v
        .get("response")
        .and_then(|r| r.get("usage"))
        .or_else(|| v.get("usage"))?;
    let input = usage
        .get("input_tokens")
        .and_then(|x| x.as_i64())
        .or_else(|| usage.get("prompt_tokens").and_then(|x| x.as_i64()));
    let output = usage
        .get("output_tokens")
        .and_then(|x| x.as_i64())
        .or_else(|| usage.get("completion_tokens").and_then(|x| x.as_i64()));
    let total = usage
        .get("total_tokens")
        .and_then(|x| x.as_i64())
        .or_else(|| match (input, output) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        });
    Some((input, output, total))
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
) -> anyhow::Result<(Response, Option<Duration>)> {
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
    let retry_after = parse_retry_after(upstream.headers());
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
    Ok((resp, retry_after))
}
