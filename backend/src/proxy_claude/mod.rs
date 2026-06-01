//! Claude 反代：对外提供 Anthropic Messages API（`/claude/v1/messages`），带着账号池里的
//! OAuth 令牌转发到 `api.anthropic.com`。请求做 OAuth 伪装（system 身份标识 + 工具名升级），
//! 响应把工具名还原回客户端原名。流式 SSE 与非流式 JSON 都按客户端 `stream` 字段走透传。

pub mod cloak;

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::pool::claude::ClaudePoolError;
use crate::proxy::verify_client_key;
use crate::state::AppState;

const UPSTREAM_BASE: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_BETA: &str =
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,fine-grained-tool-streaming-2025-05-14";
const CLAUDE_USER_AGENT: &str = "claude-cli/2.0.1 (external, cli)";
/// 列模型时只带 OAuth beta（其余 messages 专用 beta 不发，降低被上游 400 的概率）。
const ANTHROPIC_BETA_OAUTH: &str = "oauth-2025-04-20";

/// 模型列表动态拉取的缓存 TTL：成功结果缓存 6 小时；失败后 5 分钟内不再打上游。
const MODELS_POS_TTL: Duration = Duration::from_secs(6 * 3600);
const MODELS_NEG_TTL: Duration = Duration::from_secs(300);

/// Anthropic 风格错误体。
fn anthropic_error(status: StatusCode, message: &str) -> Response {
    let body = json!({
        "type": "error",
        "error": {
            "type": if status.is_server_error() { "api_error" } else { "invalid_request_error" },
            "message": message,
        }
    });
    (status, axum::Json(body)).into_response()
}

/// 静态兜底模型清单：动态拉取失败 / 无可用账号时返回。
const FALLBACK_MODEL_IDS: &[&str] = &[
    "claude-opus-4-5",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
    "claude-opus-4-1",
    "claude-sonnet-4-0",
    "claude-3-5-haiku-latest",
];

/// 把模型 id 列表包成 Anthropic `/v1/models` 风格响应体。
fn models_payload(ids: &[String]) -> Value {
    let now = 1_700_000_000u64;
    let data: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "type": "model",
                "object": "model",
                "created": now,
                "owned_by": "anthropic",
                "display_name": id,
            })
        })
        .collect();
    json!({"object": "list", "data": data})
}

/// 模型列表缓存：动态拉取成功缓存 6h，失败缓存 5min（期间走兜底、不再打上游）。
#[derive(Default)]
pub struct ModelsCache {
    inner: RwLock<Option<(Instant, Vec<String>)>>,
}

impl ModelsCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取未过期缓存（命中返回 id 列表）。
    fn get(&self) -> Option<Vec<String>> {
        let g = self.inner.read().unwrap();
        let (at, ids) = g.as_ref()?;
        // 空列表代表“上游拉取失败”的负缓存：用更短的 TTL。
        let ttl = if ids.is_empty() {
            MODELS_NEG_TTL
        } else {
            MODELS_POS_TTL
        };
        if at.elapsed() < ttl {
            Some(ids.clone())
        } else {
            None
        }
    }

    fn put(&self, ids: Vec<String>) {
        *self.inner.write().unwrap() = Some((Instant::now(), ids));
    }
}

/// `GET /claude/v1/models`：用账号池里的 OAuth 令牌实时向 `api.anthropic.com/v1/models`
/// 拉取该账号可见的模型；带缓存与静态兜底，拉取失败不影响接口可用。
pub async fn models_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }

    // 1) 缓存命中：正缓存直接返回；负缓存（空）走兜底。
    if let Some(ids) = app.claude_models_cache.get() {
        if ids.is_empty() {
            return axum::Json(models_payload(&fallback_ids())).into_response();
        }
        return axum::Json(models_payload(&ids)).into_response();
    }

    // 2) 选个账号，实时拉取。无账号 / 全不可用：返回兜底，但不写负缓存（账号随时可能加进来）。
    let selected = match app.claude_pool.pick() {
        Ok(s) => s,
        Err(_) => return axum::Json(models_payload(&fallback_ids())).into_response(),
    };

    match fetch_upstream_models(&app.clients, &selected.proxy_url, &selected.access_token).await {
        Ok(ids) if !ids.is_empty() => {
            app.claude_pool.report_success_for(&selected.id);
            app.claude_models_cache.put(ids.clone());
            axum::Json(models_payload(&ids)).into_response()
        }
        Ok(_) => {
            // 上游返回空列表：当作失败处理，写负缓存避免反复打。
            warn!(account = %selected.id, "claude /v1/models returned empty list, using fallback");
            app.claude_models_cache.put(Vec::new());
            axum::Json(models_payload(&fallback_ids())).into_response()
        }
        Err(e) => {
            warn!(account = %selected.id, "claude /v1/models fetch failed: {e:#}; using fallback");
            app.claude_models_cache.put(Vec::new());
            axum::Json(models_payload(&fallback_ids())).into_response()
        }
    }
}

fn fallback_ids() -> Vec<String> {
    FALLBACK_MODEL_IDS.iter().map(|s| s.to_string()).collect()
}

/// 向 `api.anthropic.com/v1/models` 拉一页模型，抽出全部 `data[].id`。
/// 自动翻页（最多 5 页），失败 / 非 2xx 返回 Err。
async fn fetch_upstream_models(
    clients: &crate::proxy::ProxiedClients,
    proxy_url: &str,
    access_token: &str,
) -> anyhow::Result<Vec<String>> {
    let http = clients.get(proxy_url)?;
    let mut ids: Vec<String> = Vec::new();
    let mut after: Option<String> = None;

    for _ in 0..5 {
        let mut url = format!("{UPSTREAM_BASE}/v1/models?limit=100");
        if let Some(cursor) = &after {
            url.push_str("&after_id=");
            url.push_str(cursor);
        }
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(30))
            .header("Authorization", format!("Bearer {access_token}"))
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", ANTHROPIC_BETA_OAUTH)
            .header("x-app", "cli")
            .header("User-Agent", CLAUDE_USER_AGENT)
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "upstream {} {}",
                status.as_u16(),
                body.chars().take(200).collect::<String>()
            );
        }
        let v: Value = serde_json::from_str(&body)?;
        if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
            for m in arr {
                if let Some(id) = m.get("id").and_then(|x| x.as_str()) {
                    ids.push(id.to_string());
                }
            }
        }
        let has_more = v.get("has_more").and_then(|x| x.as_bool()).unwrap_or(false);
        let last_id = v
            .get("last_id")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        match (has_more, last_id) {
            (true, Some(cursor)) => after = Some(cursor),
            _ => break,
        }
    }
    Ok(ids)
}

/// `POST /claude/v1/messages`：Anthropic Messages 反代主入口。
pub async fn messages_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return anthropic_error(StatusCode::BAD_REQUEST, &format!("read request body: {e}"))
        }
    };
    let raw: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
    };

    let model = raw
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return anthropic_error(StatusCode::BAD_REQUEST, "missing required field: model");
    }
    let client_wants_stream = raw.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // 伪装一次（与账号无关），所有重试复用同一份请求体。
    let mut cloaked = raw;
    let reverse_map = cloak::apply_oauth_cloaking(&mut cloaked);
    let cloaked_bytes = Bytes::from(serde_json::to_vec(&cloaked).unwrap_or_default());

    let max_attempts = app.config.retry.max_retries.max(1);
    let started = Instant::now();
    let log_path = "/claude/v1/messages";
    let mut last_account: Option<String> = None;
    let mut last_error: Option<(StatusCode, String)> = None;

    for attempt in 0..max_attempts {
        let selected = match app.claude_pool.pick() {
            Ok(s) => s,
            Err(ClaudePoolError::Empty) => {
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no claude accounts configured",
                );
            }
            Err(ClaudePoolError::AllUnavailable) => {
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "all claude accounts cooling down or disabled",
                );
            }
        };
        last_account = Some(selected.id.clone());

        debug!(
            attempt,
            account = %selected.id,
            model = %model,
            stream = client_wants_stream,
            "claude forwarding"
        );

        let res = forward_claude(
            &app.clients,
            &selected.proxy_url,
            &cloaked_bytes,
            &selected.access_token,
            client_wants_stream,
        )
        .await;

        let resp = match res {
            Ok(r) => r,
            Err(e) => {
                let msg = format!("network: {e}");
                app.claude_pool.report_failure_for(&selected.id, &msg);
                warn!(account = %selected.id, "claude forward error: {msg}");
                last_error = Some((
                    StatusCode::BAD_GATEWAY,
                    format!("upstream network error: {e}"),
                ));
                continue;
            }
        };

        let status = resp.status();
        if status.is_success() {
            if client_wants_stream {
                return stream_response(
                    app.clone(),
                    resp,
                    selected.id.clone(),
                    model.clone(),
                    reverse_map,
                    parts.method.clone(),
                    attempt + 1,
                    started,
                );
            } else {
                return aggregate_response(
                    app.clone(),
                    resp,
                    selected.id.clone(),
                    model.clone(),
                    reverse_map,
                    parts.method.clone(),
                    attempt + 1,
                    started,
                )
                .await;
            }
        }

        // 失败：读片段，按状态码决定重试 / 刷新 / 直接返回。
        let code = status.as_u16();
        let snippet = read_snippet(resp).await;

        // 客户端类错误：是请求本身的问题，直接回传，不换号重试。
        if matches!(code, 400 | 404 | 405 | 413 | 422) {
            app.request_log.push(
                &parts.method,
                log_path,
                Some(selected.id.clone()),
                Some(model.clone()),
                code,
                started.elapsed().as_millis() as u64,
                attempt + 1,
                None,
                None,
                None,
                Some(snippet.clone()),
            );
            return anthropic_error(status, &snippet);
        }

        if code == 401 || code == 403 {
            // 鉴权失效：后台刷新（single-flight），换号继续。
            spawn_claude_refresh(app.clone(), selected.id.clone());
            warn!(account = %selected.id, status = %status, "claude auth error, refresh spawned, switching account");
        } else {
            // 429 / 5xx / 其它：记一次失败便于冷却。
            app.claude_pool
                .report_failure_for(&selected.id, &format!("upstream {code}: {snippet}"));
            warn!(account = %selected.id, status = %status, "claude upstream error, will retry");
        }
        last_error = Some((status, format!("upstream {code}: {snippet}")));
    }

    let (status, msg) =
        last_error.unwrap_or((StatusCode::BAD_GATEWAY, "all retries failed".to_string()));
    info!("claude giving up: {status} {msg}");
    app.request_log.push(
        &parts.method,
        log_path,
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
    anthropic_error(status, &msg)
}

/// 401/403 触发的后台刷新：single-flight 去重；无 refresh_token 直接禁用。
fn spawn_claude_refresh(app: Arc<AppState>, id: String) {
    tokio::spawn(async move {
        let Some(_guard) = app.claude_refresher.begin_refresh(&id) else {
            return;
        };
        let Some(acc) = app.claude_pool.get(&id) else {
            return;
        };
        if acc.refresh_token.is_empty() {
            warn!(account = %id, "claude auth error, no refresh_token, disabling");
            app.claude_pool.set_enabled(&id, false);
            return;
        }
        match app.claude_refresher.refresh(&acc).await {
            Ok(update) => {
                app.claude_pool.update_after_refresh(&id, &update);
                app.claude_pool.report_success(&id);
                info!(account = %id, "claude token refreshed (auth-triggered)");
            }
            Err(e) => {
                warn!(account = %id, "claude auth-triggered refresh failed: {e}");
                app.claude_pool.mark_refresh_failed(&id, &e.to_string());
            }
        }
    });
}

/// 构造并发送到 Anthropic 上游的请求。`stream` 决定 Accept / Accept-Encoding。
async fn forward_claude(
    clients: &crate::proxy::ProxiedClients,
    proxy_url: &str,
    body: &Bytes,
    access_token: &str,
    stream: bool,
) -> anyhow::Result<reqwest::Response> {
    let http = clients.get(proxy_url)?;
    let url = format!("{UPSTREAM_BASE}/v1/messages?beta=true");
    let mut req = http
        .post(&url)
        .timeout(std::time::Duration::from_secs(600))
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("anthropic-beta", ANTHROPIC_BETA)
        .header("x-app", "cli")
        .header("User-Agent", CLAUDE_USER_AGENT)
        .header("x-stainless-lang", "js")
        .header("x-stainless-runtime", "node")
        .header("x-stainless-retry-count", "0")
        .body(body.clone());
    if stream {
        // SSE 必须不压缩，否则按行扫描读不了。
        req = req
            .header("Accept", "text/event-stream")
            .header("Accept-Encoding", "identity");
    } else {
        // 非流式让 reqwest 自动解压（已开启 gzip/brotli feature），不要手动设 Accept-Encoding。
        req = req.header("Accept", "application/json");
    }
    Ok(req.send().await?)
}

async fn read_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    let collapsed: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    collapsed.trim().chars().take(400).collect()
}

/// 从一条 SSE `data:` 行里抽 usage（message_start 给 input，message_delta 给 output）。
fn parse_stream_usage(line: &str, input: &mut Option<i64>, output: &mut Option<i64>) {
    let Some(payload) = line.strip_prefix("data:").map(|s| s.trim_start()) else {
        return;
    };
    if !payload.starts_with('{') {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
        return;
    };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("message_start") => {
            if let Some(n) = v
                .get("message")
                .and_then(|m| m.get("usage"))
                .and_then(|u| u.get("input_tokens"))
                .and_then(|x| x.as_i64())
            {
                *input = Some(n);
            }
        }
        Some("message_delta") => {
            if let Some(n) = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|x| x.as_i64())
            {
                *output = Some(n);
            }
        }
        _ => {}
    }
}

/// 流式：边读上游 SSE 边回写客户端，逐行还原工具名并解析 usage。
#[allow(clippy::too_many_arguments)]
fn stream_response(
    app: Arc<AppState>,
    resp: reqwest::Response,
    account_id: String,
    model: String,
    reverse_map: std::collections::HashMap<String, String>,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    use futures_util::StreamExt;

    let log = app.request_log.clone();
    let pool = app.claude_pool.clone();
    let report_acct = account_id.clone();
    let s = async_stream::stream! {
        let mut upstream = resp.bytes_stream();
        // 用字节缓冲按 '\n' 切行：一条完整的 SSE 行一定是合法 UTF-8，
        // 避免按 chunk 边界做 lossy 解码把跨 chunk 的多字节字符（中文 / emoji）切坏。
        let mut buf: Vec<u8> = Vec::new();
        let mut input_tokens: Option<i64> = None;
        let mut output_tokens: Option<i64> = None;
        let mut stream_err: Option<String> = None;

        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => {
                    buf.extend_from_slice(&b);
                    // 按行处理：保留最后一段不完整的行在 buf 里。
                    while let Some(nl) = buf.iter().position(|&c| c == b'\n') {
                        let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                        let line = String::from_utf8_lossy(&line_bytes);
                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        parse_stream_usage(trimmed, &mut input_tokens, &mut output_tokens);
                        match cloak::restore_tool_names_in_sse_line(trimmed, &reverse_map) {
                            Some(replaced) => {
                                yield Ok::<_, std::io::Error>(Bytes::from(format!("{replaced}\n")));
                            }
                            None => {
                                yield Ok(Bytes::from(line_bytes));
                            }
                        }
                    }
                }
                Err(e) => {
                    stream_err = Some(format!("upstream stream error: {e}"));
                    break;
                }
            }
        }
        // flush 残留（一般是末尾没换行的情况）
        if !buf.is_empty() {
            yield Ok(Bytes::from(std::mem::take(&mut buf)));
        }

        let (status_code, err_msg) = match &stream_err {
            Some(e) => (502u16, Some(e.clone())),
            None => (200u16, None),
        };
        if stream_err.is_none() {
            pool.report_success_for(&report_acct);
        } else {
            pool.report_failure_for(&report_acct, stream_err.as_deref().unwrap_or("stream error"));
        }
        let total = match (input_tokens, output_tokens) {
            (Some(i), Some(o)) => Some(i + o),
            _ => None,
        };
        log.push(
            &method,
            "/claude/v1/messages",
            Some(account_id),
            Some(model),
            status_code,
            started.elapsed().as_millis() as u64,
            attempt_count,
            input_tokens,
            output_tokens,
            total,
            err_msg,
        );
    };

    let body = Body::from_stream(s);
    let mut out = Response::builder()
        .status(StatusCode::OK)
        .body(body)
        .expect("build sse response");
    let h = out.headers_mut();
    h.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("connection", HeaderValue::from_static("keep-alive"));
    out
}

/// 非流式：读完整 JSON，还原工具名，解析 usage，回传。
#[allow(clippy::too_many_arguments)]
async fn aggregate_response(
    app: Arc<AppState>,
    resp: reqwest::Response,
    account_id: String,
    model: String,
    reverse_map: std::collections::HashMap<String, String>,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("read upstream body: {e}");
            app.claude_pool.report_failure_for(&account_id, &msg);
            app.request_log.push(
                &method,
                "/claude/v1/messages",
                Some(account_id),
                Some(model),
                502,
                started.elapsed().as_millis() as u64,
                attempt_count,
                None,
                None,
                None,
                Some(msg.clone()),
            );
            return anthropic_error(StatusCode::BAD_GATEWAY, &msg);
        }
    };

    let mut obj: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            let msg = format!("upstream returned non-JSON: {e}");
            app.claude_pool.report_failure_for(&account_id, &msg);
            return anthropic_error(StatusCode::BAD_GATEWAY, &msg);
        }
    };

    cloak::restore_tool_names_in_json(&mut obj, &reverse_map);
    let input = obj
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(|x| x.as_i64());
    let output = obj
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(|x| x.as_i64());
    let total = match (input, output) {
        (Some(i), Some(o)) => Some(i + o),
        _ => None,
    };

    app.claude_pool.report_success_for(&account_id);
    app.request_log.push(
        &method,
        "/claude/v1/messages",
        Some(account_id),
        Some(model),
        200,
        started.elapsed().as_millis() as u64,
        attempt_count,
        input,
        output,
        total,
        None,
    );

    (StatusCode::OK, axum::Json(obj)).into_response()
}
