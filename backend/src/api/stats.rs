use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::state::{AppState, KIRO_CFG_KEY};
use crate::store::Kv;

#[derive(Serialize)]
pub struct StatsView {
    pub total_accounts: usize,
    pub enabled_accounts: usize,
    pub cooling_down: usize,
    pub model_cooling_down: i64,
    pub expired: usize,
    pub total_requests: u64,
    pub total_failures: u64,
}

pub async fn overview(State(app): State<Arc<AppState>>) -> Response {
    match app.pool.stats_overview() {
        Ok(s) => Json(StatsView {
            total_accounts: s.total,
            enabled_accounts: s.enabled,
            cooling_down: s.cooling,
            model_cooling_down: app.pool.cooling_account_count(),
            expired: s.expired,
            total_requests: s.total_requests,
            total_failures: s.total_failures,
        })
        .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
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

/// GET /config/kiro —— 返回当前运行期 kiro 配置（前端表单据此回填）。
pub async fn get_kiro_config(State(app): State<Arc<AppState>>) -> Response {
    match serde_json::to_value(app.kiro_cfg()) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// 部分更新负载：所有字段可选，只覆盖传入的项。
#[derive(Deserialize, Default)]
pub struct KiroConfigPatch {
    #[serde(default)]
    pub compact: Option<bool>,
    #[serde(default)]
    pub compact_threshold_tokens: Option<u32>,
    #[serde(default)]
    pub tool_result_max_tokens: Option<u32>,
    #[serde(default)]
    pub keep_recent_turns: Option<u32>,
    #[serde(default)]
    pub synth_cache: Option<bool>,
    #[serde(default)]
    pub filter_claude_code: Option<bool>,
    #[serde(default)]
    pub strip_boundaries: Option<bool>,
    #[serde(default)]
    pub env_noise: Option<bool>,
}

/// PUT /config/kiro —— 校验后写入运行期 holder（即时生效）+ 持久化到 DB（重启不丢），回新值。
pub async fn put_kiro_config(
    State(app): State<Arc<AppState>>,
    Json(patch): Json<KiroConfigPatch>,
) -> Response {
    let mut cfg = app.kiro_cfg();
    if let Some(v) = patch.compact {
        cfg.compact = v;
    }
    if let Some(v) = patch.compact_threshold_tokens {
        cfg.compact_threshold_tokens = v;
    }
    if let Some(v) = patch.tool_result_max_tokens {
        cfg.tool_result_max_tokens = v;
    }
    if let Some(v) = patch.keep_recent_turns {
        cfg.keep_recent_turns = v;
    }
    if let Some(v) = patch.synth_cache {
        cfg.synth_cache = v;
    }
    if let Some(v) = patch.filter_claude_code {
        cfg.filter_claude_code = v;
    }
    if let Some(v) = patch.strip_boundaries {
        cfg.strip_boundaries = v;
    }
    if let Some(v) = patch.env_noise {
        cfg.env_noise = v;
    }

    // 校验：阈值/上限须合理，避免压缩逻辑被设成无意义值。
    if cfg.compact_threshold_tokens < 1000 {
        return (StatusCode::BAD_REQUEST, "compact_threshold_tokens 至少 1000").into_response();
    }
    if cfg.tool_result_max_tokens < 100 {
        return (StatusCode::BAD_REQUEST, "tool_result_max_tokens 至少 100").into_response();
    }
    if cfg.keep_recent_turns < 1 {
        return (StatusCode::BAD_REQUEST, "keep_recent_turns 至少 1").into_response();
    }

    // 持久化（先落盘再写内存：落盘失败就不改运行期状态，避免内存/DB 不一致）。
    match persist_kiro_cfg(&app, &cfg) {
        Ok(()) => {}
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
    *app.kiro_runtime.write().unwrap() = cfg.clone();

    match serde_json::to_value(cfg) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// DELETE /config/kiro —— 清除 DB 覆盖、运行期重置为 config.yaml 的 kiro 值，回新值。
pub async fn delete_kiro_config(State(app): State<Arc<AppState>>) -> Response {
    let conn = match app.db.get() {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };
    if let Err(e) = conn.execute("DELETE FROM kv WHERE k = ?1", [KIRO_CFG_KEY]) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response();
    }
    let file_default = app.config.kiro.clone();
    *app.kiro_runtime.write().unwrap() = file_default.clone();
    match serde_json::to_value(file_default) {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// 把运行期 kiro 配置序列化为 JSON 写入 kv 表。
fn persist_kiro_cfg(app: &Arc<AppState>, cfg: &crate::config::KiroConfig) -> anyhow::Result<()> {
    let conn = app.db.get()?;
    let json = serde_json::to_string(cfg)?;
    Kv::set(&conn, KIRO_CFG_KEY, &json)?;
    Ok(())
}
