//! Claude（Anthropic OAuth）额度查询。镜像 `auth/quota.rs`（Codex 的 `fetch_codex_quota`）。
//!
//! 额度来自社区逆向的未公开端点 `GET https://api.anthropic.com/api/oauth/usage`，返回
//! `five_hour` / `seven_day` 两个窗口，各含 `utilization`(0-100 已用百分比) + `resets_at`(ISO8601)。
//! 结构与 Codex 额度的 5h/week 完全一致，故直接复用 [`crate::auth::quota::QuotaWindow`]。
//!
//! 注意：该端点对频繁轮询会激进返回 429（且无 Retry-After），所以只在用户**手动**点查额度时
//! 调用，绝不后台轮询；结果缓存进 DB。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::StatusCode;
use serde_json::Value;

use crate::auth::quota::QuotaWindow;
use crate::proxy::ProxiedClients;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
/// 必带 `oauth-2025-04-20` beta，否则 401；UA 用 claude-cli 与 messages 路径一致，避免命中
/// 更严格的限流桶。
const ANTHROPIC_BETA_OAUTH: &str = "oauth-2025-04-20";
const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.63 (external, cli)";

/// Claude 额度快照：5 小时窗口 + 7 天窗口。
#[derive(Debug, Clone)]
pub struct ClaudeQuotaSnapshot {
    pub five_hour: Option<QuotaWindow>,
    pub week: Option<QuotaWindow>,
}

/// 向 `/api/oauth/usage` 拉一次额度。非 2xx（尤其 429）/ 两个窗口都缺，返回 Err。
pub async fn fetch_claude_quota(
    clients: &Arc<ProxiedClients>,
    access_token: &str,
    proxy_url: &str,
) -> Result<ClaudeQuotaSnapshot> {
    if access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }

    let http = clients.get(proxy_url)?;
    let resp = http
        .get(USAGE_URL)
        .timeout(Duration::from_secs(30))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("anthropic-beta", ANTHROPIC_BETA_OAUTH)
        .header("User-Agent", CLAUDE_USER_AGENT)
        .send()
        .await
        .with_context(|| "claude quota request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "quota request failed: {} {}",
            status,
            compact_body(status, &body)
        );
    }

    parse_snapshot(&body)
}

fn compact_body(status: StatusCode, body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return status
            .canonical_reason()
            .unwrap_or("upstream error")
            .to_string();
    }
    trimmed.chars().take(512).collect()
}

fn parse_snapshot(body: &str) -> Result<ClaudeQuotaSnapshot> {
    let root: Value = serde_json::from_str(body).with_context(|| "parse quota response")?;
    let five_hour = parse_window(root.get("five_hour"));
    let week = parse_window(root.get("seven_day"));
    if five_hour.is_none() && week.is_none() {
        anyhow::bail!("quota response missing five_hour/seven_day windows");
    }
    Ok(ClaudeQuotaSnapshot { five_hour, week })
}

/// 解析单个窗口对象：`utilization` → used_percent，`resets_at` → reset_at。
/// 两者都缺则返回 None（视为该窗口无数据）。
fn parse_window(v: Option<&Value>) -> Option<QuotaWindow> {
    let obj = v?.as_object()?;
    let used_percent = obj
        .get("utilization")
        .and_then(value_to_f64)
        .map(|n| n.clamp(0.0, 100.0));
    let reset_at = obj
        .get("resets_at")
        .and_then(|r| r.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    if used_percent.is_none() && reset_at.is_none() {
        return None;
    }
    Some(QuotaWindow {
        used_percent,
        reset_at,
    })
}

fn value_to_f64(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.trim().parse::<f64>().ok())
}
