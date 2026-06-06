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

/// `GET /kiro/v1/models`：返回 Kiro 当前账号池里某个可用账号支持的模型清单。
///
/// 行为与 KAM `list_available_models` 对齐：
///  - 先 round-robin 选一个可用账号；
///  - 该账号有 fresh 缓存（30 分钟）且不要求强刷，直接返回缓存；
///  - 否则去打 `q.{region}.amazonaws.com/ListAvailableModels` 翻页聚合，
///    成功时写回缓存；401 自动 refresh + 重试一次；403 + suspended 标 banned；
///  - 输出形态走 OpenAI / Anthropic 通用 `{ "object": "list", "data": [...] }`，
///    以便客户端把它当 `/v1/models` 用。客户端可加 `?force=1` 强制刷新。
pub async fn models_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }

    let force_refresh = req
        .uri()
        .query()
        .map(|q| {
            q.split('&').any(|kv| {
                let mut it = kv.splitn(2, '=');
                matches!(it.next(), Some("force") | Some("force_refresh"))
                    && matches!(it.next().unwrap_or(""), "1" | "true" | "yes")
            })
        })
        .unwrap_or(false);

    // 客户端可以选 modelProvider（KAM 单页参数）。默认 None = 让上游返回全集。
    let model_provider = req.uri().query().and_then(|q| {
        q.split('&').find_map(|kv| {
            let mut it = kv.splitn(2, '=');
            if matches!(it.next(), Some("model_provider") | Some("modelProvider")) {
                it.next().map(|v| v.to_string())
            } else {
                None
            }
        })
    });

    match fetch_models_for_pool(&app, model_provider.as_deref(), force_refresh).await {
        Ok(models) => {
            let now = chrono::Utc::now().timestamp() as u64;
            let data: Vec<Value> = models
                .available_models
                .iter()
                .map(|m| {
                    json!({
                        "id": m.model_id,
                        "type": "model",
                        "object": "model",
                        "created": now,
                        "owned_by": m.provider.as_deref().unwrap_or("kiro"),
                        "display_name": if m.model_name.is_empty() { &m.model_id } else { &m.model_name },
                        "description": m.description,
                        "is_default": m.is_default.unwrap_or(false),
                        "context_window": m.context_window,
                        "rate_multiplier": m.rate_multiplier,
                        "rate_unit": m.rate_unit,
                        "supported_input_types": m.supported_input_types,
                        "token_limits": m.token_limits,
                        "prompt_caching": m.prompt_caching,
                        "capabilities": m.capabilities,
                    })
                })
                .collect();
            axum::Json(json!({
                "object": "list",
                "data": data,
                "default_model": models.default_model.as_ref().map(|m| &m.model_id),
            }))
            .into_response()
        }
        Err((status, msg)) => anthropic_error(status, &msg),
    }
}

/// 统一的模型拉取流程：池子选账号 → 缓存命中？→ 上游拉 → 缓存写回 → 401 refresh 重试 1 次。
async fn fetch_models_for_pool(
    app: &Arc<AppState>,
    model_provider: Option<&str>,
    force_refresh: bool,
) -> Result<crate::auth::kiro_models::ListAvailableModelsResponse, (StatusCode, String)> {
    use crate::auth::kiro_models::{
        build_cache_entry, fetch_all_available_models, read_models_cache, ListModelsQuery,
    };

    // 选一个账号。/v1/models 是只读探活，没必要做多账号轮替——选到第一个能用的就够。
    let selected = match app.kiro_pool.pick() {
        Ok(s) => s,
        Err(KiroPoolError::Empty) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "no kiro accounts configured".to_string(),
            ));
        }
        Err(KiroPoolError::AllUnavailable) => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "all kiro accounts cooling down or disabled".to_string(),
            ));
        }
    };

    // 1) 缓存命中直接返回
    if !force_refresh {
        if let Some(acc) = app.kiro_pool.get(&selected.id) {
            if let Some(cached) = read_models_cache(&acc.models_cache, model_provider, false) {
                return Ok(cached);
            }
        }
    }

    // 2) 拉上游 + 一次性 401 重试
    let mut access_token = selected.access_token.clone();
    let mut profile_arn = selected.profile_arn.clone();
    let mut idc_region = selected.idc_region.clone();
    let mut machine_id = selected.machine_id.clone();

    let mut last_err: Option<String> = None;
    for attempt in 0..2u32 {
        let result = fetch_all_available_models(
            &app.clients,
            ListModelsQuery {
                access_token: &access_token,
                provider: &selected.provider,
                idc_region: idc_region.as_deref(),
                profile_arn: profile_arn.as_deref(),
                machine_id: machine_id.as_deref(),
                model_provider,
                proxy_url: &selected.proxy_url,
            },
        )
        .await;

        match result {
            Ok(resp) => {
                let entry = build_cache_entry(&resp, model_provider);
                app.kiro_pool.update_models_cache(&selected.id, &entry);
                return Ok(resp);
            }
            Err(e) => {
                let msg = format!("{e:#}");
                last_err = Some(msg.clone());
                // 第一次失败若是 AUTH_ERROR，尝试 refresh 一次再重试。
                if attempt == 0 && msg.contains("AUTH_ERROR:") {
                    if let Some(_guard) = app.kiro_refresher.begin_refresh(&selected.id) {
                        if let Some(acc) = app.kiro_pool.get(&selected.id) {
                            match app.kiro_refresher.refresh(&acc).await {
                                Ok(update) => {
                                    app.kiro_pool.update_after_refresh(&selected.id, &update);
                                    if let Some(fresh) = app.kiro_pool.get(&selected.id) {
                                        access_token = fresh.access_token.clone();
                                        profile_arn = crate::pool::kiro::resolve_profile_arn_for_upstream(&fresh);
                                        idc_region = fresh.idc_region.clone();
                                        machine_id = fresh.machine_id.clone();
                                    }
                                    continue;
                                }
                                Err(refresh_err) => {
                                    return Err((
                                        StatusCode::BAD_GATEWAY,
                                        format!("token refresh failed: {refresh_err:#}"),
                                    ));
                                }
                            }
                        }
                    }
                }
                // BANNED / 不可恢复错误：标账号
                if msg.contains("BANNED:") {
                    let reason = msg
                        .split("BANNED:")
                        .nth(1)
                        .map(str::trim)
                        .unwrap_or("suspended")
                        .to_string();
                    app.kiro_pool.mark_banned(&selected.id, &reason);
                }
                break;
            }
        }
    }

    Err((
        StatusCode::BAD_GATEWAY,
        last_err.unwrap_or_else(|| "ListAvailableModels failed".to_string()),
    ))
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
        let translated = match translator::translate(&raw, selected.profile_arn.as_deref().unwrap_or("")) {
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

        // 依次尝试端点：先打账号自身 region，然后兜底列表（去重）。
        // 之前硬编码 us-east-1 的 CodeWhisperer/AmazonQ 双端点会让企业号 region 错位时
        // 持续 403 —— 与 KAM 一致：URL 由账号 region 决定。
        let primary_region = upstream::resolve_account_region(
            selected.profile_arn.as_deref(),
            selected.idc_region.as_deref(),
        );
        let mut endpoints_tried: Vec<String> = Vec::with_capacity(5);
        endpoints_tried.push(primary_region.clone());
        for ep in upstream::KIRO_FALLBACK_ENDPOINTS {
            if !endpoints_tried.iter().any(|r| r == ep.region) {
                endpoints_tried.push(ep.region.to_string());
            }
        }

        let mut endpoint_resp = None;
        let mut endpoint_err: Option<String> = None;
        let mut auth_failed = false;
        for region in &endpoints_tried {
            match upstream::call_kiro(
                &app.clients,
                region,
                &translated.payload,
                &selected.access_token,
                &selected.auth_method,
                selected.machine_id.as_deref(),
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
                        endpoint_err =
                            Some(format!("upstream {code} on q.{region}.amazonaws.com: {snippet}"));
                        break;
                    }
                    // 429 / 其它：记下来换下一个端点再试
                    endpoint_err =
                        Some(format!("upstream {code} on q.{region}.amazonaws.com: {snippet}"));
                    continue;
                }
                Err(e) => {
                    endpoint_err = Some(format!("network on q.{region}.amazonaws.com: {e}"));
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

        // 上游已返回 200。成功 / 失败的最终上报与请求日志改到流处理结束后按实际结果落，
        // 避免"HTTP 200 但流中途断开"被错误地记成成功 + 200。
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
        // single-flight：同一账号同一时刻只允许一次刷新在飞，避免并发请求用同一个旧
        // refresh_token 互相刷失败。
        let Some(_guard) = app.kiro_refresher.begin_refresh(&id) else {
            return;
        };
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
    let pool = app.kiro_pool.clone();
    let report_acct = account_id.clone();
    let s = async_stream::stream! {
        let mut parser = EventStreamParser::new();
        let mut proc = KiroEventProcessor::new();
        let mut encoder = ClaudeSseEncoder::new(model.clone(), restore);
        let mut upstream = resp.bytes_stream();
        let mut stream_err: Option<String> = None;

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
                    let msg = format!("upstream stream error: {e}");
                    let line = ClaudeSseEncoder::error(502, &msg);
                    yield Ok(bytes::Bytes::from(line));
                    stream_err = Some(msg);
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

        // 按实际结果上报账号状态 + 写请求日志
        let (status_code, err_msg) = match &stream_err {
            Some(e) => (502u16, Some(e.clone())),
            None => (200u16, None),
        };
        if stream_err.is_none() {
            pool.report_success_for(&report_acct);
        } else {
            pool.report_failure_for(&report_acct, stream_err.as_deref().unwrap_or("stream error"));
        }
        let total = usage.input_tokens + usage.output_tokens;
        log.push(
            &method,
            "/kiro/v1/messages",
            Some(account_id),
            Some(model),
            status_code,
            started.elapsed().as_millis() as u64,
            attempt_count,
            Some(usage.input_tokens),
            Some(usage.output_tokens),
            Some(total),
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
                // 流中途断开：按失败上报 + 记日志（之前这里既不上报也不记日志）。
                let msg = format!("upstream stream error: {e}");
                app.kiro_pool.report_failure_for(&account_id, &msg);
                app.request_log.push(
                    &method,
                    "/kiro/v1/messages",
                    Some(account_id.clone()),
                    Some(model.clone()),
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
        }
    }
    events.extend(proc.finalize());
    let usage = proc.usage.clone();

    app.kiro_pool.report_success_for(&account_id);
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
