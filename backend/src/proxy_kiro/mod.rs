//! Kiro 反代：对外提供 Anthropic Messages API（`/kiro/v1/messages`），
//! 翻译成 Kiro `generateAssistantResponse` 转发到上游，把上游 event-stream 反向翻译成
//! Anthropic SSE（流式）或单个 message JSON（非流式）。复用账号池 / 代理池 / 请求日志。

pub mod eventstream;
pub mod payload;
pub mod response;
pub mod translator;
pub mod upstream;

use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::pool::kiro::KiroPoolError;
use crate::proxy::verify_client_key;
use crate::state::AppState;

use eventstream::EventStreamParser;
use response::{aggregate, ClaudeSseEncoder, KiroEventProcessor};

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

/// `GET /kiro/v1/models`：返回 Kiro 支持的模型列表（Anthropic/OpenAI 通用形状）。
pub async fn models_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }
    let now = 1_700_000_000u64;
    let ids = [
        "claude-sonnet-4.5",
        "claude-sonnet-4",
        "claude-haiku-4.5",
        "claude-opus-4.5",
    ];
    let data: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({
                "id": id,
                "type": "model",
                "object": "model",
                "created": now,
                "owned_by": "kiro",
                "display_name": id,
            })
        })
        .collect();
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

/// `POST /kiro/v1/messages`：Anthropic Messages 反代主入口。
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

    let max_attempts = app.config.retry.max_retries.max(1);
    let started = Instant::now();
    let log_path = "/kiro/v1/messages";
    let mut last_account: Option<String> = None;
    let mut last_error: Option<(StatusCode, String)> = None;

    for attempt in 0..max_attempts {
        let selected = match app.kiro_pool.pick() {
            Ok(s) => s,
            Err(KiroPoolError::Empty) => {
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no kiro accounts configured",
                );
            }
            Err(KiroPoolError::AllUnavailable) => {
                return anthropic_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "all kiro accounts cooling down or disabled",
                );
            }
        };
        last_account = Some(selected.id.clone());

        // 翻译请求（每次重试都重建，conversationId/continuationId 会刷新，没问题）
        let translated = match translator::translate(&raw, &selected.profile_arn) {
            Ok(t) => t,
            Err(e) => return anthropic_error(StatusCode::BAD_REQUEST, &e),
        };

        // 估算输入 token（payload 字节数 / 3），给 message_start 用
        let input_est = (serde_json::to_vec(&translated.payload)
            .map(|v| v.len())
            .unwrap_or(0)
            / 3) as i64;

        debug!(
            attempt,
            account = %selected.id,
            model = %model,
            stream = client_wants_stream,
            "kiro forwarding"
        );

        // 依次尝试端点
        let mut endpoint_resp = None;
        let mut endpoint_err: Option<String> = None;
        let mut auth_failed = false;
        for endpoint in upstream::KIRO_ENDPOINTS {
            match upstream::call_kiro(
                &app.clients,
                endpoint,
                &translated.payload,
                &selected.access_token,
                &selected.auth_method,
                &selected.proxy_url,
            )
            .await
            {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        endpoint_resp = Some(resp);
                        break;
                    }
                    let code = status.as_u16();
                    let snippet = read_snippet(resp).await;
                    // 401/403：同账号换端点也没用，直接按账号级错误处理后跳出
                    if code == 401 || code == 403 {
                        handle_auth_error(&app, &selected.id, code, &snippet);
                        auth_failed = true;
                        endpoint_err = Some(format!("upstream {code} on {}: {snippet}", endpoint.name));
                        break;
                    }
                    // 429 / 其它：记下来换下一个端点再试
                    endpoint_err = Some(format!("upstream {code} on {}: {snippet}", endpoint.name));
                    continue;
                }
                Err(e) => {
                    endpoint_err = Some(format!("network on {}: {e}", endpoint.name));
                    continue;
                }
            }
        }

        let Some(resp) = endpoint_resp else {
            let msg = endpoint_err.unwrap_or_else(|| "all kiro endpoints failed".to_string());
            if !auth_failed {
                // 非鉴权失败（网络/429/5xx）：记一次失败，便于冷却
                app.kiro_pool.report_failure_for(&selected.id, &msg);
            }
            warn!(account = %selected.id, "kiro attempt failed: {msg}");
            last_error = Some((StatusCode::BAD_GATEWAY, msg));
            continue;
        };

        // 成功：流式翻译 or 聚合
        app.kiro_pool.report_success_for(&selected.id);
        if client_wants_stream {
            return stream_response(
                app.clone(),
                resp,
                selected.id.clone(),
                model.clone(),
                translated.tool_name_restore,
                input_est,
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
                translated.tool_name_restore,
                parts.method.clone(),
                attempt + 1,
                started,
            )
            .await;
        }
    }

    let (status, msg) =
        last_error.unwrap_or((StatusCode::BAD_GATEWAY, "all retries failed".to_string()));
    info!("kiro giving up: {status} {msg}");
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

/// 处理 401/403 鉴权类错误：401 刷 token；403 检测封禁，封禁则禁用，否则也尝试刷新。
fn handle_auth_error(app: &Arc<AppState>, id: &str, status: u16, snippet: &str) {
    if status == 403 {
        let lower = snippet.to_ascii_lowercase();
        if lower.contains("suspended") || lower.contains("banned") || lower.contains("forbidden") {
            app.kiro_pool.mark_banned(id, snippet);
            return;
        }
    }
    spawn_kiro_refresh(app.clone(), id.to_string());
}

/// 401/403 触发的后台刷新：无 refresh_token 直接禁用，避免反复命中。
fn spawn_kiro_refresh(app: Arc<AppState>, id: String) {
    tokio::spawn(async move {
        let Some(acc) = app.kiro_pool.get(&id) else {
            return;
        };
        if acc.refresh_token.is_empty() {
            warn!(account = %id, "kiro auth error, no refresh_token, disabling");
            app.kiro_pool.set_enabled(&id, false);
            return;
        }
        match app.kiro_refresher.refresh(&acc).await {
            Ok(update) => {
                app.kiro_pool.update_after_refresh(&id, &update);
                app.kiro_pool.report_success_for(&id);
                info!(account = %id, "kiro token refreshed (auth-triggered)");
            }
            Err(e) => {
                warn!(account = %id, "kiro auth-triggered refresh failed: {e}");
                app.kiro_pool.mark_refresh_failed(&id, &e.to_string());
            }
        }
    });
}

async fn read_snippet(resp: reqwest::Response) -> String {
    let body = resp.text().await.unwrap_or_default();
    let collapsed: String = body
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    collapsed.trim().chars().take(400).collect()
}

/// 流式：边收上游 event-stream，边发 Anthropic SSE。
#[allow(clippy::too_many_arguments)]
fn stream_response(
    app: Arc<AppState>,
    resp: reqwest::Response,
    account_id: String,
    model: String,
    restore: std::collections::HashMap<String, String>,
    input_est: i64,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    use futures_util::StreamExt;

    let log = app.request_log.clone();
    let s = async_stream::stream! {
        let mut parser = EventStreamParser::new();
        let mut proc = KiroEventProcessor::new();
        let mut encoder = ClaudeSseEncoder::new(model.clone(), restore);
        let mut upstream = resp.bytes_stream();

        yield Ok::<_, std::io::Error>(bytes::Bytes::from(encoder.start(input_est)));

        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(b) => {
                    for frame in parser.feed(&b) {
                        for ev in proc.process(&frame) {
                            let sse = encoder.encode(&ev);
                            if !sse.is_empty() {
                                yield Ok(bytes::Bytes::from(sse));
                            }
                        }
                    }
                }
                Err(e) => {
                    let line = ClaudeSseEncoder::error(502, &format!("upstream stream error: {e}"));
                    yield Ok(bytes::Bytes::from(line));
                    break;
                }
            }
        }
        // 收尾未完成的工具 + 估算 usage
        for ev in proc.finalize() {
            let sse = encoder.encode(&ev);
            if !sse.is_empty() {
                yield Ok(bytes::Bytes::from(sse));
            }
        }
        let usage = proc.usage.clone();
        yield Ok(bytes::Bytes::from(encoder.finish(&usage)));

        // 写请求日志
        let total = usage.input_tokens + usage.output_tokens;
        log.push(
            &method,
            "/kiro/v1/messages",
            Some(account_id),
            Some(model),
            200,
            started.elapsed().as_millis() as u64,
            attempt_count,
            Some(usage.input_tokens),
            Some(usage.output_tokens),
            Some(total),
            None,
        );
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

/// 非流式：收完上游 event-stream，聚合成单个 Anthropic message JSON。
#[allow(clippy::too_many_arguments)]
async fn aggregate_response(
    app: Arc<AppState>,
    resp: reqwest::Response,
    account_id: String,
    model: String,
    restore: std::collections::HashMap<String, String>,
    method: Method,
    attempt_count: u32,
    started: Instant,
) -> Response {
    use futures_util::StreamExt;

    let mut parser = EventStreamParser::new();
    let mut proc = KiroEventProcessor::new();
    let mut events = Vec::new();
    let mut upstream = resp.bytes_stream();

    while let Some(chunk) = upstream.next().await {
        match chunk {
            Ok(b) => {
                for frame in parser.feed(&b) {
                    events.extend(proc.process(&frame));
                }
            }
            Err(e) => {
                return anthropic_error(
                    StatusCode::BAD_GATEWAY,
                    &format!("upstream stream error: {e}"),
                );
            }
        }
    }
    events.extend(proc.finalize());
    let usage = proc.usage.clone();

    let total = usage.input_tokens + usage.output_tokens;
    app.request_log.push(
        &method,
        "/kiro/v1/messages",
        Some(account_id),
        Some(model.clone()),
        200,
        started.elapsed().as_millis() as u64,
        attempt_count,
        Some(usage.input_tokens),
        Some(usage.output_tokens),
        Some(total),
        None,
    );

    let obj = aggregate(&events, &usage, &model, &restore);
    (StatusCode::OK, axum::Json(obj)).into_response()
}
