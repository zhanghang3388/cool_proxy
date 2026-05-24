use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde_json::Value;

use crate::proxy::ProxiedClients;

const WHAM_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const CODEX_USER_AGENT: &str = "codex_cli_rs/0.118.0 (Mac OS 26.3.1; arm64) iTerm.app/3.6.9";

#[derive(Debug, Clone)]
pub struct QuotaWindow {
    pub used_percent: Option<f64>,
    pub reset_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct CodexQuotaSnapshot {
    pub five_hour: Option<QuotaWindow>,
    pub week: Option<QuotaWindow>,
}

pub async fn fetch_codex_quota(
    clients: &Arc<ProxiedClients>,
    access_token: &str,
    account_id: &str,
    proxy_url: &str,
) -> Result<CodexQuotaSnapshot> {
    if access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }

    let http = clients.get(proxy_url)?;
    let mut req = http
        .get(WHAM_USAGE_URL)
        .timeout(Duration::from_secs(30))
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {access_token}"))
        .header("User-Agent", CODEX_USER_AGENT);

    if !account_id.trim().is_empty() {
        req = req.header("Chatgpt-Account-Id", account_id);
    }

    let resp = req.send().await.with_context(|| "quota request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "quota request failed: {} {}",
            status,
            compact_body(status, &body)
        );
    }

    parse_quota_snapshot(&body)
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

fn parse_quota_snapshot(body: &str) -> Result<CodexQuotaSnapshot> {
    let root: Value = serde_json::from_str(body).with_context(|| "parse quota response")?;
    let five_hour = find_window(
        &root,
        &["primary_window", "primaryWindow", "five_hour", "fiveHour"],
    );
    let week = find_window(
        &root,
        &["secondary_window", "secondaryWindow", "weekly", "week"],
    );

    if five_hour.is_none() && week.is_none() {
        anyhow::bail!("quota response missing primary/secondary windows");
    }

    Ok(CodexQuotaSnapshot { five_hour, week })
}

fn find_window(root: &Value, keys: &[&str]) -> Option<QuotaWindow> {
    for key in keys {
        if let Some(v) = find_key_recursive(root, key) {
            let window = parse_window(v);
            if window.used_percent.is_some() || window.reset_at.is_some() {
                return Some(window);
            }
        }
    }
    None
}

fn find_key_recursive<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(map) => {
            if let Some(v) = map.get(key) {
                return Some(v);
            }
            for v in map.values() {
                if let Some(found) = find_key_recursive(v, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => arr.iter().find_map(|v| find_key_recursive(v, key)),
        _ => None,
    }
}

fn parse_window(v: &Value) -> QuotaWindow {
    QuotaWindow {
        used_percent: first_f64(
            v,
            &[
                "used_percent",
                "usedPercent",
                "usage_percent",
                "usagePercent",
                "used",
            ],
        )
        .map(|n| n.clamp(0.0, 100.0)),
        reset_at: first_time(
            v,
            &[
                "reset_at",
                "resetAt",
                "resets_at",
                "resetsAt",
                "next_reset_at",
                "nextResetAt",
            ],
        ),
    }
}

fn first_f64(v: &Value, keys: &[&str]) -> Option<f64> {
    let obj = v.as_object()?;
    for key in keys {
        if let Some(raw) = obj.get(*key) {
            if let Some(n) = raw.as_f64() {
                return Some(n);
            }
            if let Some(s) = raw.as_str() {
                if let Ok(n) = s.trim().parse::<f64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn first_time(v: &Value, keys: &[&str]) -> Option<DateTime<Utc>> {
    let obj = v.as_object()?;
    for key in keys {
        let Some(raw) = obj.get(*key) else {
            continue;
        };
        if let Some(t) = parse_time(raw) {
            return Some(t);
        }
    }
    None
}

fn parse_time(v: &Value) -> Option<DateTime<Utc>> {
    if let Some(n) = v.as_i64() {
        return unix_to_time(n);
    }
    let s = v.as_str()?.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(n) = s.parse::<i64>() {
        return unix_to_time(n);
    }
    None
}

fn unix_to_time(raw: i64) -> Option<DateTime<Utc>> {
    if raw <= 0 {
        return None;
    }
    if raw > 1_000_000_000_000 {
        DateTime::<Utc>::from_timestamp_millis(raw)
    } else {
        DateTime::<Utc>::from_timestamp(raw, 0)
    }
}
