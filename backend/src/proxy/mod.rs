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
pub mod models_catalog;
pub mod translator;
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

    // /v1/models 是 OpenAI 兼容的"列出模型"接口。codex 上游没有这个端点，
    // 这里直接返回项目支持的几个 model id，方便 SDK 拉一次列表能起来。
    if req.method() == Method::GET {
        let path_only = req.uri().path();
        if path_only == "/v1/models" {
            let is_codex_client = req
                .uri()
                .query()
                .map(|q| q.split('&').any(|p| p.starts_with("client_version=")))
                .unwrap_or(false);
            return models_list_response(&app, is_codex_client);
        }
        if let Some(id) = path_only.strip_prefix("/v1/models/") {
            return models_get_response(&app, id);
        }
    }

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

/// 项目"内置可服务"的 model 兜底列表：用于号池里没拿到任何提示信息时的 fallback。
/// 这里只列 codex 客户端实测可用的；catalog 里有更多 slug，但不是所有都能跑。
const FALLBACK_MODELS: &[&str] = &[
    "gpt-5-codex",
    "gpt-5-codex-mini",
    "gpt-5",
    "gpt-5-mini",
    "gpt-4.1",
];

/// 当前可服务的 model 列表：取号池里所有 enabled 且未死的账号 → 收集它们最近成功跑过的 model
/// （记录在 account_model_states 里 last_kind=success 的行）→ union FALLBACK_MODELS。
/// 没有任何号 → 空列表（让客户端知道 0 模型可用，比硬编一份骗它好）。
fn available_model_ids(app: &AppState) -> Vec<String> {
    let mut has_any = false;
    let mut from_logs: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for id in app.pool.all_ids_sorted() {
        let Some(acc) = app.pool.get(&id) else {
            continue;
        };
        if !acc.enabled || acc.access_token.is_empty() {
            continue;
        }
        has_any = true;
        // 从该号的 model_states 提取曾经被请求过的 model_key
        for s in app.pool.list_model_states(&acc.id) {
            if !s.model_key.is_empty() {
                from_logs.insert(s.model_key);
            }
        }
    }
    if !has_any {
        return Vec::new();
    }
    let mut out: Vec<String> = FALLBACK_MODELS.iter().map(|s| s.to_string()).collect();
    for m in from_logs {
        if !out.contains(&m) {
            out.push(m);
        }
    }
    out
}

fn models_list_response(app: &AppState, codex_client: bool) -> Response {
    let ids = available_model_ids(app);
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let body = if codex_client {
        models_catalog::build_codex_client_response(&id_refs)
    } else {
        models_catalog::build_simple_list(&id_refs)
    };
    (StatusCode::OK, axum::Json(body)).into_response()
}

fn models_get_response(app: &AppState, id: &str) -> Response {
    let ids = available_model_ids(app);
    if ids.iter().any(|m| m == id) {
        let body = serde_json::json!({
            "id": id,
            "object": "model",
            "created": 1_700_000_000u64,
            "owned_by": "openai",
        });
        (StatusCode::OK, axum::Json(body)).into_response()
    } else {
        let body = serde_json::json!({
            "error": {
                "message": format!("model '{id}' not found"),
                "type": "invalid_request_error",
                "code": "model_not_found",
            }
        });
        (StatusCode::NOT_FOUND, axum::Json(body)).into_response()
    }
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
            // 无 refresh_token 的号上游 401 后无法自救：直接禁用 + 标注原因，
            // 避免每次请求都被选中再触发一轮无意义的 spawn_refresh。
            warn!(
                account = %id,
                "auth error and no refresh_token on file, disabling account"
            );
            app.pool
                .disable_account(&id, "no refresh_token; account disabled");
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

/// OpenAI 风格的错误体；4xx/5xx 失败时统一这个形状返给客户端。
fn openai_error_response(status: StatusCode, message: &str) -> Response {
    let kind = if status.is_server_error() {
        "server_error"
    } else {
        "invalid_request_error"
    };
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": kind,
            "code": serde_json::Value::Null,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// 流式状态码 + 错误片段 → 一条 SSE 错误事件。客户端会收到 `event: error\ndata: {...}\n\n`。
fn sse_error_event(status: StatusCode, message: &str) -> String {
    let body = serde_json::json!({
        "error": {
            "message": message,
            "type": if status.is_server_error() { "server_error" } else { "invalid_request_error" },
            "code": serde_json::Value::Null,
            "status": status.as_u16(),
        }
    });
    format!("event: error\ndata: {}\n\n", body)
}

fn sse_data_line(payload: &serde_json::Value) -> String {
    format!("data: {}\n\n", payload)
}

/// `/v1/chat/completions`：把 OpenAI ChatCompletion 形状的请求翻译成 codex `/responses`，
/// 转发到上游，把 codex SSE 流反向翻译成 chat.completion.chunk 流（或聚合成非流式响应）。
pub async fn chat_completions_handler(
    State(app): State<Arc<AppState>>,
    req: Request,
) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return openai_error_response(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("read request body: {e}"),
            );
        }
    };

    let raw: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => {
            return openai_error_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid JSON body: {e}"),
            );
        }
    };
    let model = raw
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return openai_error_response(
            StatusCode::BAD_REQUEST,
            "missing required field: model",
        );
    }
    let client_wants_stream = raw
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // 翻译成 codex /responses 形状。cool_proxy 内部一律 stream=true 上游。
    let codex_body = translator::translate_request(&model, &body_bytes, true);
    let codex_body_bytes = Bytes::from(codex_body.to_string().into_bytes());

    let upstream_path = "/responses".to_string();
    let max_attempts = app.config.retry.max_retries.max(1);
    let started = Instant::now();
    let mut last_account: Option<String> = None;
    let mut last_error: Option<(StatusCode, String)> = None;

    for attempt in 0..max_attempts {
        let selected = match app.pool.pick_for(&model) {
            Ok(s) => s,
            Err(PoolError::Empty) => {
                return openai_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no codex accounts configured",
                );
            }
            Err(PoolError::AllUnavailable) => {
                return openai_error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "all accounts cooling down or disabled",
                );
            }
            Err(PoolError::Db(e)) => {
                error!("pick from db failed: {e:?}");
                return openai_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("db error: {e}"),
                );
            }
        };
        last_account = Some(selected.id.clone());

        debug!(
            attempt,
            account = %selected.id,
            model = %model,
            stream = client_wants_stream,
            "chat-completions forwarding"
        );

        let res = forward_once(
            &app.clients,
            &app.config.upstream.base_url,
            &upstream_path,
            &Method::POST,
            &parts.headers,
            &codex_body_bytes,
            &selected.access_token,
            &selected.account_id,
            &selected.proxy_url,
        )
        .await;

        match res {
            Ok((mut resp, retry_after)) => {
                let status = resp.status();
                if status.is_success() {
                    // 成功：根据 client_wants_stream 选择翻译路径
                    if client_wants_stream {
                        return stream_translate_response(
                            app.clone(),
                            resp,
                            selected.id.clone(),
                            model.clone(),
                            body_bytes.clone(),
                            parts.method.clone(),
                            attempt + 1,
                            started,
                        );
                    } else {
                        return aggregate_translate_response(
                            app.clone(),
                            resp,
                            selected.id.clone(),
                            model.clone(),
                            body_bytes.clone(),
                            parts.method.clone(),
                            attempt + 1,
                            started,
                        )
                        .await;
                    }
                }

                // 失败：吃掉前几百字节做 last_error，然后分类
                let body = std::mem::replace(resp.body_mut(), Body::empty());
                let snippet = match axum::body::to_bytes(body, 4 * 1024).await {
                    Ok(b) if !b.is_empty() => {
                        let s = String::from_utf8_lossy(&b).into_owned();
                        truncate_snippet(&s, 300)
                    }
                    _ => format!("upstream {status}"),
                };
                let kind = app.pool.report(ReportContext {
                    id: &selected.id,
                    model: &model,
                    status: Some(status.as_u16()),
                    retry_after,
                    message: &snippet,
                });

                match kind {
                    ErrorKind::Auth => {
                        spawn_refresh(app.clone(), selected.id.clone());
                        warn!(
                            account = %selected.id,
                            status = %status,
                            "chat: auth error, refresh spawned, switching account"
                        );
                        last_error = Some((status, format!("upstream {status}: {snippet}")));
                        continue;
                    }
                    ErrorKind::Client => {
                        app.request_log.push(
                            &parts.method,
                            &upstream_path,
                            Some(selected.id.clone()),
                            Some(model.clone()),
                            status.as_u16(),
                            started.elapsed().as_millis() as u64,
                            attempt + 1,
                            None,
                            None,
                            None,
                            None,
                        );
                        return openai_error_response(status, &snippet);
                    }
                    ErrorKind::Quota
                    | ErrorKind::NotFound
                    | ErrorKind::Transient
                    | ErrorKind::Network => {
                        warn!(
                            account = %selected.id,
                            status = %status,
                            kind = kind.label(),
                            "chat: upstream error, will retry"
                        );
                        last_error = Some((status, format!("upstream {status}: {snippet}")));
                        continue;
                    }
                }
            }
            Err(e) => {
                error!(account = %selected.id, "chat: forward error: {e:?}");
                let msg = format!("network: {e}");
                app.pool.report(ReportContext {
                    id: &selected.id,
                    model: &model,
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
    info!("chat: giving up: {status} {msg}");
    app.request_log.push(
        &parts.method,
        &upstream_path,
        last_account,
        Some(model),
        status.as_u16(),
        started.elapsed().as_millis() as u64,
        max_attempts,
        None,
        None,
        None,
        Some(msg.clone()),
    );
    openai_error_response(status, &msg)
}

/// 把上游 codex SSE 流翻译成 OpenAI chat.completion.chunk 流，边收边回写客户端。
/// 同时 tee 一份给 RequestLog 的 usage 解析（保留原有计费逻辑）。
fn stream_translate_response(
    app: Arc<AppState>,
    mut resp: Response,
    account_id: String,
    model: String,
    original_request: Bytes,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    let upstream_body = std::mem::replace(resp.body_mut(), Body::empty());

    let log = app.request_log.clone();
    let log_method = method.clone();
    let log_path = "/responses".to_string();
    let log_acct = account_id.clone();
    let log_model = Some(model.clone());
    let log_attempts = attempt_count;
    let pool = app.pool.clone();
    let success_acct = account_id.clone();
    let success_model = model.clone();

    // 启动 codex SSE → OpenAI chunk 翻译流
    let original_for_translator = original_request.clone();
    let model_for_translator = model.clone();

    let s = async_stream::stream! {
        use futures_util::StreamExt as _;
        let mut translator = translator::StreamTranslator::new(
            &model_for_translator,
            &original_for_translator,
        );
        let mut up = upstream_body.into_data_stream();
        let mut buf = bytes::BytesMut::new();
        let mut tail = bytes::BytesMut::with_capacity(32 * 1024);
        let mut completed = false;

        while let Some(chunk) = up.next().await {
            match chunk {
                Ok(b) => {
                    // tee：尾部 32KB 给 parse_usage 用
                    if tail.len() + b.len() > 32 * 1024 {
                        let drop = (tail.len() + b.len()).saturating_sub(32 * 1024).min(tail.len());
                        let _ = tail.split_to(drop);
                    }
                    if b.len() >= 32 * 1024 {
                        tail.clear();
                        let start = b.len() - 32 * 1024;
                        tail.extend_from_slice(&b[start..]);
                    } else {
                        tail.extend_from_slice(&b);
                    }

                    buf.extend_from_slice(&b);
                    // SSE 按双换行分事件，行内按 \n 分。这里只取以 "data: " 开头的行，逐行解析。
                    while let Some(idx) = find_double_newline(&buf) {
                        let event_block = buf.split_to(idx + 2); // 含分隔
                        let event_slice: &[u8] = &event_block;
                        for line in event_slice.split(|&c| c == b'\n') {
                            let line = trim_eol(line);
                            if !line.starts_with(b"data:") {
                                continue;
                            }
                            let payload = &line[5..];
                            let payload = if payload.first() == Some(&b' ') {
                                &payload[1..]
                            } else {
                                payload
                            };
                            if payload == b"[DONE]" {
                                continue;
                            }
                            let Ok(ev) = serde_json::from_slice::<serde_json::Value>(payload) else {
                                continue;
                            };
                            let chunks = translator.push(&ev);
                            for chunk in chunks {
                                let line = sse_data_line(&chunk);
                                yield Ok::<_, std::io::Error>(bytes::Bytes::from(line));
                            }
                            if ev.get("type").and_then(|v| v.as_str())
                                == Some("response.completed")
                            {
                                completed = true;
                            }
                        }
                    }
                }
                Err(e) => {
                    let line = sse_error_event(
                        StatusCode::BAD_GATEWAY,
                        &format!("upstream stream error: {e}"),
                    );
                    yield Ok(bytes::Bytes::from(line));
                    break;
                }
            }
        }

        // 流结束：发 [DONE]
        yield Ok(bytes::Bytes::from_static(b"data: [DONE]\n\n"));

        // 写 RequestLog（usage 从 tail 解析），并标记成功
        let (input, output, total) = parse_usage(&tail);
        log.push(
            &log_method,
            &log_path,
            Some(log_acct),
            log_model,
            200,
            started.elapsed().as_millis() as u64,
            log_attempts,
            input,
            output,
            total,
            None,
        );
        if completed {
            pool.report_success_for(&success_acct, &success_model);
        }
    };

    let body = Body::from_stream(s);
    let mut out = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .expect("build sse response");
    let h = out.headers_mut();
    h.insert("content-type", HeaderValue::from_static("text/event-stream"));
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("connection", HeaderValue::from_static("keep-alive"));
    out
}

/// 非流式：等所有 codex SSE 收完，构建一个 ChatCompletion JSON 返回。
async fn aggregate_translate_response(
    app: Arc<AppState>,
    mut resp: Response,
    account_id: String,
    model: String,
    original_request: Bytes,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    let upstream_body = std::mem::replace(resp.body_mut(), Body::empty());

    let mut up = upstream_body.into_data_stream();
    let mut buf = bytes::BytesMut::new();
    let mut tail = bytes::BytesMut::with_capacity(32 * 1024);
    let mut agg = translator::Aggregator::new(&model, &original_request);

    while let Some(chunk) = up.next().await {
        match chunk {
            Ok(b) => {
                if tail.len() + b.len() > 32 * 1024 {
                    let drop = (tail.len() + b.len()).saturating_sub(32 * 1024).min(tail.len());
                    let _ = tail.split_to(drop);
                }
                if b.len() >= 32 * 1024 {
                    tail.clear();
                    let start = b.len() - 32 * 1024;
                    tail.extend_from_slice(&b[start..]);
                } else {
                    tail.extend_from_slice(&b);
                }

                buf.extend_from_slice(&b);
                while let Some(idx) = find_double_newline(&buf) {
                    let event_block = buf.split_to(idx + 2);
                    let event_slice: &[u8] = &event_block;
                    for line in event_slice.split(|&c| c == b'\n') {
                        let line = trim_eol(line);
                        if !line.starts_with(b"data:") {
                            continue;
                        }
                        let payload = &line[5..];
                        let payload = if payload.first() == Some(&b' ') {
                            &payload[1..]
                        } else {
                            payload
                        };
                        if payload == b"[DONE]" {
                            continue;
                        }
                        let Ok(ev) = serde_json::from_slice::<serde_json::Value>(payload) else {
                            continue;
                        };
                        agg.push(&ev);
                    }
                }
            }
            Err(e) => {
                return openai_error_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream stream error: {e}"),
                );
            }
        }
    }

    let completed = agg.is_completed();
    let (input, output, total) = parse_usage(&tail);
    app.request_log.push(
        &method,
        "/responses",
        Some(account_id.clone()),
        Some(model.clone()),
        200,
        started.elapsed().as_millis() as u64,
        attempt_count,
        input,
        output,
        total,
        None,
    );
    if completed {
        app.pool.report_success_for(&account_id, &model);
    }
    let final_obj = agg.finalize();
    (StatusCode::OK, axum::Json(final_obj)).into_response()
}

fn find_double_newline(buf: &[u8]) -> Option<usize> {
    // 兼容 \n\n 和 \r\n\r\n
    let mut i = 0;
    while i + 1 < buf.len() {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            return Some(i + 2);
        }
        i += 1;
    }
    None
}

fn trim_eol(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 {
        let c = line[end - 1];
        if c == b'\r' || c == b'\n' {
            end -= 1;
        } else {
            break;
        }
    }
    &line[..end]
}
