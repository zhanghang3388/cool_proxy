//! Kiro 反代：对外提供 Anthropic Messages API（`/kiro/v1/messages`），
//! 翻译成 Kiro `generateAssistantResponse` 转发到上游，把上游 event-stream 反向翻译成
//! Anthropic SSE（流式）或单个 message JSON（非流式）。复用账号池 / 代理池 / 请求日志。

pub mod cache_synth;
pub mod eventstream;
pub mod payload;
pub mod prompt_filter;
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

/// `GET /kiro/v1/models`：返回 Kiro 官方维护的模型清单（与 KAM `get_available_models`
/// 同步），不打上游 `ListAvailableModels`。
///
/// 原因（与 KAM 一致）：
///  - `/v1/models` 高频被客户端拉，打上游会被限流；
///  - Kiro 官方维护的全量清单对所有账号都一样，订阅差异由 `/v1/messages` 那边的上游
///    自然拒绝处理（不在反代层提前过滤）。
///  - 想拿"某账号实际支持的模型"，请走管理面板的 `/api/kiro/accounts/<id>/models`
///    （会去打 `ListAvailableModels`），那是诊断接口，不是给客户端用的。
///
/// 客户端可以加 `?force=1` —— 当前没缓存，仅留作未来扩展（接 ListAvailableModels）的占位。
pub async fn models_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }
    let now = chrono::Utc::now().timestamp() as u64;
    // 仅返回 Claude 系列，且 id 用「横杠」形式（claude-opus-4-8，而非 4.8）。
    //
    // 原因：本清单主要给上游网关（如 cool_api）做「获取模型 / allowed_models 登记」用。
    // Claude Code 默认就发横杠形式的模型名（claude-opus-4-8 / claude-sonnet-4-5-20250929
    // / claude-3-5-haiku-20241022），网关按精确字符串匹配，清单与 CC 实发名一致才不会
    // 被网关在转发前就 400 掉。cool_proxy 自己的 /v1/messages 对横杠/点号都认（map_model_id
    // 会归一），所以这里统一横杠不影响实际对话。
    //
    // 末尾特意带上 CC 的后台「小模型」claude-3-5-haiku-20241022 —— 它不在 Kiro 官方清单里，
    // 但 CC 会用它发标题/摘要类请求；列进来，网关侧才能一次把主模型 + 小模型都登记全。
    // 不再返回 auto / 开源模型（deepseek / minimax / glm / qwen）—— 按需仅暴露 Claude。
    const MODEL_IDS: &[&str] = &[
        "claude-opus-4-8",
        "claude-opus-4-8-thinking",
        "claude-opus-4-7",
        "claude-opus-4-7-thinking",
        "claude-opus-4-6",
        "claude-opus-4-6-thinking",
        "claude-sonnet-4-6",
        "claude-sonnet-4-6-thinking",
        "claude-opus-4-5",
        "claude-opus-4-5-thinking",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-thinking",
        "claude-haiku-4-5",
        "claude-haiku-4-5-thinking",
        "claude-sonnet-4",
        "claude-sonnet-4-thinking",
        // Claude Code 后台小模型（不在 Kiro 清单内，列出便于网关登记；
        // 实际转发时 map_model_id 会归一到 claude-haiku-4.5）。
        "claude-3-5-haiku-20241022",
    ];
    let data: Vec<Value> = MODEL_IDS
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
    axum::Json(json!({"object": "list", "data": data})).into_response()
}

/// `POST /kiro/v1/messages/count_tokens`（及根路径别名）。
///
/// Claude Code 在发正式消息前/中会打这个端点估算上下文大小。Kiro 上游没有对应接口，
/// 这里本地粗略估算即可（客户端只用它判断是否要压缩上下文，估算值容差很大）。
/// 之前没有这个路由 → CC 预检拿到 404 → 报错 / 体验异常。
pub async fn count_tokens_handler(State(app): State<Arc<AppState>>, req: Request) -> Response {
    if !verify_client_key(req.headers(), &app.config.api_keys) {
        return anthropic_error(StatusCode::UNAUTHORIZED, "missing or invalid api key");
    }
    let body_bytes = match axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(e) => {
            return anthropic_error(StatusCode::BAD_REQUEST, &format!("read request body: {e}"))
        }
    };
    // 粗略估算：请求体字节数 / 4（偏保守；至少为 1）。
    let estimate = (body_bytes.len() / 4).max(1);
    axum::Json(json!({ "input_tokens": estimate })).into_response()
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

    // 合成 prompt-cache 计费：按 CC 自带的 cache_control 断点算前缀命中（只算一次，避免
    // 重试时重复登记）。后面在拿到上游真实 input 总量后，据此把它拆成 read/creation/fresh。
    let synth_cache: Option<(cache_synth::CachePlan, u32)> = if app.config.kiro.synth_cache {
        let plan = cache_synth::build_plan(&raw);
        if plan.is_empty() {
            debug!("kiro synth-cache: 请求未带任何 cache_control → 不参与合成缓存（全 fresh）");
            None
        } else {
            let hit = app.kiro_prompt_cache.lookup_and_record(&plan);
            // 诊断：sys_ckpt 跨轮应稳定；hit>0 表示命中历史前缀。checkpoints 字段仅新代码有。
            let (sys_blocks, sys_cc, tools_n, tools_cc, msgs_n, msg_cc) = cache_request_shape(&raw);
            debug!(
                hit,
                breakpoints = plan.breakpoint_count(),
                checkpoints = plan.checkpoint_count(),
                cacheable = plan.cacheable_tokens(),
                sys_ckpt = %plan.first_checkpoint(),
                bp_digests = ?plan.breakpoint_digests(),
                sys_blocks,
                sys_cc,
                tools_n,
                tools_cc,
                msgs_n,
                msg_cc,
                "kiro synth-cache (hit=0=未命中历史前缀; sys_ckpt 跨轮应一致)"
            );
            // 临时诊断：打印第一个系统块开头 200 字符，定位「等长却每轮变」的易变前缀。
            // 第一个系统块是 CC 开场白区，不含用户代码/对话内容。
            debug!(sys0 = %first_system_snippet(&raw, 200), "kiro synth-cache sys0 (定位易变前缀)");
            Some((plan, hit))
        }
    } else {
        None
    };

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
        let filter = prompt_filter::PromptFilterOptions::from_config(&app.config.kiro);
        let translated = match translator::translate(
            &raw,
            selected.profile_arn.as_deref().unwrap_or(""),
            filter,
        ) {
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

        // 与 KAM 一致：URL 由账号 region 决定，不做额外的 region fallback。
        // 之前我加了 fallback 列表会掩盖真实错误（同账号换 region 通常一样 403），
        // 还会让"实际命中的 region"和"账号绑定的 region"不一致，调试困难。
        let primary_region = upstream::resolve_account_region(
            selected.profile_arn.as_deref(),
            selected.idc_region.as_deref(),
        );

        // 诊断日志：把实际用到的 region / profile_arn / provider 全打出来，
        // 方便定位 "subscription does not support" / "User is not authorized" 这类
        // AWS 按订阅 + profileArn + region 反查时拒绝的错误。
        info!(
            attempt,
            account = %selected.id,
            provider = %selected.provider,
            auth_method = %selected.auth_method,
            region = %primary_region,
            profile_arn = ?selected.profile_arn,
            machine_id = ?selected.machine_id,
            model = %model,
            stream = client_wants_stream,
            "kiro forwarding to q.{}.amazonaws.com",
            primary_region
        );

        let mut endpoint_resp = None;
        let mut endpoint_err: Option<String> = None;
        let mut auth_failed = false;
        match upstream::call_kiro(
            &app.clients,
            &primary_region,
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
                } else {
                    let code = status.as_u16();
                    let snippet = read_snippet(resp).await;
                    if code == 401 || code == 403 {
                        handle_auth_error(&app, &selected.id, code, &snippet);
                        auth_failed = true;
                    }
                    endpoint_err = Some(format!(
                        "upstream {code} on q.{primary_region}.amazonaws.com: {snippet}"
                    ));
                }
            }
            Err(e) => {
                endpoint_err =
                    Some(format!("network on q.{primary_region}.amazonaws.com: {e}"));
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
                synth_cache.clone(),
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
                synth_cache.clone(),
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

/// 诊断：统计请求里 system / tools / messages 各有多少块、其中多少块带 cache_control。
/// 帮助判断「上游（CC/cool_api）到底有没有把 cache_control 打在稳定前缀（system/tools）上」。
fn cache_request_shape(raw: &Value) -> (usize, usize, usize, usize, usize, usize) {
    let (mut sys_blocks, mut sys_cc) = (0usize, 0usize);
    match raw.get("system") {
        Some(Value::String(_)) => sys_blocks = 1,
        Some(Value::Array(a)) => {
            sys_blocks = a.len();
            sys_cc = a.iter().filter(|b| b.get("cache_control").is_some()).count();
        }
        _ => {}
    }
    let (mut tools_n, mut tools_cc) = (0usize, 0usize);
    if let Some(t) = raw.get("tools").and_then(|v| v.as_array()) {
        tools_n = t.len();
        tools_cc = t.iter().filter(|x| x.get("cache_control").is_some()).count();
    }
    let (mut msgs_n, mut msg_cc) = (0usize, 0usize);
    if let Some(m) = raw.get("messages").and_then(|v| v.as_array()) {
        msgs_n = m.len();
        for msg in m {
            if let Some(blocks) = msg.get("content").and_then(|v| v.as_array()) {
                msg_cc += blocks
                    .iter()
                    .filter(|b| b.get("cache_control").is_some())
                    .count();
            }
        }
    }
    (sys_blocks, sys_cc, tools_n, tools_cc, msgs_n, msg_cc)
}

/// 临时诊断：取第一个系统块文本前 `n` 个字符（CC 开场白区，便于跨轮肉眼对比易变前缀）。
fn first_system_snippet(raw: &Value, n: usize) -> String {
    let text = match raw.get("system") {
        Some(Value::String(s)) => s.as_str(),
        Some(Value::Array(a)) => a
            .first()
            .and_then(|b| b.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or(""),
        _ => "",
    };
    let snip: String = text.chars().take(n).collect();
    // 把换行折叠成 ⏎ 方便单行查看
    snip.replace('\n', "⏎")
}

/// 用合成缓存计划把 `total_input` 拆成 `(fresh_input, cache_read, cache_creation)`。
/// 未启用 / 无断点 / 无 input 时返回 `(total_input, 0, 0)`（即不合成，全 fresh）。
fn synth_split(
    total_input: i64,
    synth: &Option<(cache_synth::CachePlan, u32)>,
) -> (i64, i64, i64) {
    match synth {
        Some((plan, hit)) if !plan.is_empty() && total_input > 0 => {
            let s = cache_synth::split_usage(total_input, *hit, plan);
            (s.fresh_input, s.cache_read, s.cache_creation)
        }
        _ => (total_input, 0, 0),
    }
}

/// 把合成缓存拆分写回 KiroUsage（input_tokens 变成 fresh 部分，cache 字段填上）。
/// 返回拆分前的真实 input 总量，供请求日志记录真实总数。
fn apply_synth_split(
    usage: &mut response::KiroUsage,
    synth: &Option<(cache_synth::CachePlan, u32)>,
) -> i64 {
    let total = usage.input_tokens;
    let (fresh, read, create) = synth_split(total, synth);
    usage.input_tokens = fresh;
    usage.cache_read_tokens = read;
    usage.cache_write_tokens = create;
    total
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
    synth: Option<(cache_synth::CachePlan, u32)>,
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

        // message_start 的 usage：合成缓存按字节估算先拆一份（cool_api 从 message_start
        // 读取 input/cache 计费），真实总量到流末再在 message_delta 里以真值重拆。
        let (start_input, start_read, start_create) = synth_split(input_est, &synth);
        yield Ok::<_, std::io::Error>(bytes::Bytes::from(
            encoder.start(start_input, start_read, start_create),
        ));

        // 每 20s 发一个 ping 保活。Claude Code 的首条请求（大 system prompt + 一堆工具）
        // 上游首 token 可能很久，期间若长时间无字节，CC 会把连接当作 stalled 而断开。
        // 对齐 kiro.rs：流式期间周期性发 `event: ping`。
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(20));
        ping.tick().await; // 吃掉 interval 立即触发的第一拍

        loop {
            tokio::select! {
                // biased：优先处理上游数据，ping 只在确实空闲时发，避免抢占数据。
                biased;
                chunk = upstream.next() => {
                    match chunk {
                        Some(Ok(b)) => {
                            for frame in parser.feed(&b) {
                                for ev in proc.process(&frame) {
                                    let sse = encoder.encode(&ev);
                                    if !sse.is_empty() {
                                        yield Ok(bytes::Bytes::from(sse));
                                    }
                                }
                            }
                            // 上游在 200 之后于流内报错：发一条 Anthropic error SSE 收尾，
                            // 别让 CC 拿到一个"正常但空"的响应（旧实现会静默丢弃错误帧）。
                            if let Some(err) = proc.take_error() {
                                let line = ClaudeSseEncoder::error(502, &err);
                                yield Ok(bytes::Bytes::from(line));
                                stream_err = Some(err);
                                break;
                            }
                        }
                        Some(Err(e)) => {
                            let msg = format!("upstream stream error: {e}");
                            let line = ClaudeSseEncoder::error(502, &msg);
                            yield Ok(bytes::Bytes::from(line));
                            stream_err = Some(msg);
                            break;
                        }
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    yield Ok(bytes::Bytes::from_static(
                        b"event: ping\ndata: {\"type\": \"ping\"}\n\n",
                    ));
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
        let mut usage = proc.usage.clone();
        // 用上游真实 input 总量重算合成缓存拆分，写进 message_delta 的 usage。
        let real_input = apply_synth_split(&mut usage, &synth);
        let stop_override = proc.stop_reason_override();
        yield Ok(bytes::Bytes::from(encoder.finish(&usage, stop_override)));

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
        // 日志记真实总 input（合成拆分只影响计费字段，不改变真实消耗）。
        let total = real_input + usage.output_tokens;
        log.push(
            &method,
            "/kiro/v1/messages",
            Some(account_id),
            Some(model),
            status_code,
            started.elapsed().as_millis() as u64,
            attempt_count,
            Some(real_input),
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
    synth: Option<(cache_synth::CachePlan, u32)>,
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
    let mut usage = proc.usage.clone();
    let stop_override = proc.stop_reason_override();

    // 上游在 200 之后于流内报错：按失败上报 + 记日志 + 回 502，
    // 而不是把一个"正常但空"的 message 交给客户端。
    if let Some(err) = proc.take_error() {
        app.kiro_pool.report_failure_for(&account_id, &err);
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
            Some(err.clone()),
        );
        return anthropic_error(StatusCode::BAD_GATEWAY, &err);
    }

    // 非流式有真实总量，直接以真值做合成缓存拆分。
    let real_input = apply_synth_split(&mut usage, &synth);

    app.kiro_pool.report_success_for(&account_id);
    let total = real_input + usage.output_tokens;
    app.request_log.push(
        &method,
        "/kiro/v1/messages",
        Some(account_id),
        Some(model.clone()),
        200,
        started.elapsed().as_millis() as u64,
        attempt_count,
        Some(real_input),
        Some(usage.output_tokens),
        Some(total),
        None,
    );

    let obj = aggregate(&events, &usage, &model, &restore, stop_override);
    (StatusCode::OK, axum::Json(obj)).into_response()
}
