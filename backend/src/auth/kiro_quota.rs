//! Kiro 额度查询：调用 CodeWhisperer / Q runtime 的 getUsageLimits 接口，
//! 解析出 credits / bonus / 套餐 / 重置时间，并识别封禁状态。
//! 解析路径从 cockpit-tools 的 `extract_usage_payload` 移植。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::kiro::{
    get_path_value, normalize_non_empty, parse_profile_arn_region, parse_timestamp, pick_number,
    pick_string, runtime_endpoint_for_region,
};
use crate::proxy::ProxiedClients;

/// 一次额度查询的结构化结果。
#[derive(Debug, Clone, Default)]
pub struct KiroUsageSnapshot {
    pub plan_name: Option<String>,
    pub plan_tier: Option<String>,
    pub credits_total: Option<f64>,
    pub credits_used: Option<f64>,
    pub bonus_total: Option<f64>,
    pub bonus_used: Option<f64>,
    pub usage_reset_at: Option<DateTime<Utc>>,
    pub bonus_expire_days: Option<i64>,
    /// 原始响应体，落库备查。
    pub raw: Value,
}

/// 拉取账号额度。`profile_arn` 用于定位 region + 鉴权，无 ARN 时直接报错。
/// 上游返回 403 / 带封禁原因时，返回 `Err("BANNED:<reason>")`，调用方据此标记账号。
pub async fn fetch_kiro_usage(
    clients: &Arc<ProxiedClients>,
    access_token: &str,
    profile_arn: &str,
    proxy_url: &str,
) -> Result<KiroUsageSnapshot> {
    if access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }
    if profile_arn.trim().is_empty() {
        anyhow::bail!("missing profile_arn（无法定位 Kiro runtime endpoint）");
    }

    let region = parse_profile_arn_region(profile_arn);
    let endpoint = runtime_endpoint_for_region(region.as_deref());
    let url = format!("{}/getUsageLimits", endpoint.trim_end_matches('/'));

    let http = clients.get(proxy_url)?;
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .query(&[
            ("origin", "AI_EDITOR"),
            ("profileArn", profile_arn),
            ("resourceType", "AGENTIC_REQUEST"),
            ("isEmailRequired", "true"),
        ])
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .send()
        .await
        .with_context(|| "kiro usage request")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        if let Some(reason) = parse_runtime_error_reason(&body)
            .or_else(|| (status == reqwest::StatusCode::FORBIDDEN).then(|| body.clone()))
        {
            anyhow::bail!("BANNED:{}", reason);
        }
        anyhow::bail!(
            "kiro usage request failed: {} {}",
            status,
            body.chars().take(512).collect::<String>()
        );
    }

    let usage: Value = serde_json::from_str(&body).with_context(|| "parse kiro usage response")?;
    Ok(parse_usage_snapshot(usage))
}

fn parse_usage_snapshot(usage: Value) -> KiroUsageSnapshot {
    let (
        plan_name,
        plan_tier,
        credits_total,
        credits_used,
        bonus_total,
        bonus_used,
        usage_reset_at,
        bonus_expire_days,
    ) = extract_usage_payload(Some(&usage));

    KiroUsageSnapshot {
        plan_name,
        plan_tier,
        credits_total,
        credits_used,
        bonus_total,
        bonus_used,
        usage_reset_at: usage_reset_at.and_then(|s| DateTime::<Utc>::from_timestamp(s, 0)),
        bonus_expire_days,
        raw: usage,
    }
}

/// 从上游错误体里抠出可读的封禁 / 错误原因。
fn parse_runtime_error_reason(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok()?;
    let direct = pick_string(
        Some(&parsed),
        &[
            &["reason"],
            &["message"],
            &["errorMessage"],
            &["error", "message"],
            &["error", "reason"],
            &["detail"],
            &["details"],
        ],
    );
    if let Some(reason) = direct.and_then(|raw| normalize_non_empty(Some(raw.as_str()))) {
        return Some(reason);
    }
    pick_string(
        Some(&parsed),
        &[&["error"], &["code"], &["errorCode"], &["error", "code"]],
    )
    .and_then(|raw| normalize_non_empty(Some(raw.as_str())))
}

/// 若 `Err` 是 `BANNED:` 前缀，返回封禁原因。
pub fn banned_reason(err: &str) -> Option<String> {
    err.strip_prefix("BANNED:")
        .and_then(|raw| normalize_non_empty(Some(raw)))
}

// ===== 以下解析逻辑从 cockpit-tools extract_usage_payload 移植 =====

type UsageTuple = (
    Option<String>, // plan_name
    Option<String>, // plan_tier
    Option<f64>,    // credits_total
    Option<f64>,    // credits_used
    Option<f64>,    // bonus_total
    Option<f64>,    // bonus_used
    Option<i64>,    // usage_reset_at (unix secs)
    Option<i64>,    // bonus_expire_days
);

fn extract_usage_payload(usage: Option<&Value>) -> UsageTuple {
    let usage = resolve_usage_root(usage);

    let mut plan_name = pick_string(
        usage,
        &[
            &["planName"],
            &["currentPlanName"],
            &["subscriptionInfo", "subscriptionName"],
            &["subscriptionInfo", "subscriptionTitle"],
            &["usageBreakdowns", "planName"],
            &["freeTrialUsage", "planName"],
            &["plan", "name"],
        ],
    );

    let mut plan_tier = pick_string(
        usage,
        &[
            &["planTier"],
            &["tier"],
            &["subscriptionInfo", "type"],
            &["usageBreakdowns", "tier"],
            &["plan", "tier"],
        ],
    );

    let mut credits_total = pick_number(
        usage,
        &[
            &["estimatedUsage", "total"],
            &["estimatedUsage", "creditsTotal"],
            &["usageBreakdowns", "plan", "totalCredits"],
            &["usageBreakdowns", "covered", "total"],
            &["usageBreakdownList", "0", "usageLimitWithPrecision"],
            &["usageBreakdownList", "0", "usageLimit"],
            &["credits", "total"],
            &["totalCredits"],
        ],
    );

    let mut credits_used = pick_number(
        usage,
        &[
            &["estimatedUsage", "used"],
            &["estimatedUsage", "creditsUsed"],
            &["usageBreakdowns", "plan", "usedCredits"],
            &["usageBreakdowns", "covered", "used"],
            &["usageBreakdownList", "0", "currentUsageWithPrecision"],
            &["usageBreakdownList", "0", "currentUsage"],
            &["credits", "used"],
            &["usedCredits"],
        ],
    );

    let mut bonus_total = pick_number(
        usage,
        &[
            &["bonusCredits", "total"],
            &["bonus", "total"],
            &["usageBreakdowns", "bonus", "total"],
            &["usageBreakdownList", "0", "freeTrialInfo", "usageLimitWithPrecision"],
            &["usageBreakdownList", "0", "freeTrialInfo", "usageLimit"],
        ],
    );

    let mut bonus_used = pick_number(
        usage,
        &[
            &["bonusCredits", "used"],
            &["bonus", "used"],
            &["usageBreakdowns", "bonus", "used"],
            &["usageBreakdownList", "0", "freeTrialInfo", "currentUsageWithPrecision"],
            &["usageBreakdownList", "0", "freeTrialInfo", "currentUsage"],
        ],
    );

    let mut usage_reset_at = parse_timestamp(
        usage
            .and_then(|value| get_path_value(value, &["resetAt"]))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["resetTime"])))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["resetOn"])))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["nextDateReset"])))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["usageBreakdowns", "resetAt"]))),
    );

    let mut bonus_expire_days = pick_number(
        usage,
        &[
            &["bonusCredits", "expiryDays"],
            &["bonusCredits", "expireDays"],
            &["bonus", "expiryDays"],
            &["usageBreakdownList", "0", "freeTrialInfo", "daysRemaining"],
        ],
    )
    .map(|value| value.round() as i64);

    let breakdown = pick_usage_breakdown(usage);
    let free_trial = breakdown.and_then(|value| {
        get_path_value(value, &["freeTrialUsage"])
            .or_else(|| get_path_value(value, &["freeTrialInfo"]))
    });

    plan_name = plan_name.or_else(|| {
        pick_string(
            breakdown,
            &[&["displayName"], &["displayNamePlural"], &["type"], &["unit"]],
        )
    });
    plan_tier = plan_tier.or_else(|| pick_string(breakdown, &[&["currency"], &["type"], &["unit"]]));

    if credits_total.is_none() {
        credits_total = pick_number(
            breakdown,
            &[
                &["usageLimitWithPrecision"],
                &["usageLimit"],
                &["limit"],
                &["total"],
                &["totalCredits"],
            ],
        );
    }
    if credits_used.is_none() {
        credits_used = pick_number(
            breakdown,
            &[
                &["currentUsageWithPrecision"],
                &["currentUsage"],
                &["used"],
                &["usedCredits"],
            ],
        );
    }
    if bonus_total.is_none() {
        bonus_total = pick_number(
            free_trial,
            &[
                &["usageLimitWithPrecision"],
                &["usageLimit"],
                &["limit"],
                &["total"],
                &["totalCredits"],
            ],
        );
    }
    if bonus_used.is_none() {
        bonus_used = pick_number(
            free_trial,
            &[
                &["currentUsageWithPrecision"],
                &["currentUsage"],
                &["used"],
                &["usedCredits"],
            ],
        );
    }
    if usage_reset_at.is_none() {
        usage_reset_at = parse_timestamp(
            breakdown
                .and_then(|value| get_path_value(value, &["resetDate"]))
                .or_else(|| breakdown.and_then(|value| get_path_value(value, &["resetAt"]))),
        );
    }
    if bonus_expire_days.is_none() {
        bonus_expire_days = pick_number(
            free_trial,
            &[&["daysRemaining"], &["expiryDays"], &["expireDays"]],
        )
        .map(|value| value.round() as i64)
        .or_else(|| {
            days_until(parse_timestamp(
                free_trial.and_then(|value| get_path_value(value, &["expiryDate"])),
            ))
        })
        .or_else(|| {
            days_until(parse_timestamp(
                free_trial.and_then(|value| get_path_value(value, &["freeTrialExpiry"])),
            ))
        });
    }

    (
        plan_name,
        plan_tier,
        credits_total,
        credits_used,
        bonus_total,
        bonus_used,
        usage_reset_at,
        bonus_expire_days,
    )
}

fn resolve_usage_root(usage: Option<&Value>) -> Option<&Value> {
    let usage = usage?;
    if let Some(state) = get_path_value(usage, &["kiro.resourceNotifications.usageState"]) {
        return Some(state);
    }
    if let Some(state) = get_path_value(usage, &["usageState"]) {
        return Some(state);
    }
    Some(usage)
}

fn pick_usage_breakdown(usage: Option<&Value>) -> Option<&Value> {
    let usage = usage?;
    let list = get_path_value(usage, &["usageBreakdownList"])
        .and_then(|value| value.as_array())
        .or_else(|| get_path_value(usage, &["usageBreakdowns"]).and_then(|value| value.as_array()))?;
    if list.is_empty() {
        return None;
    }
    list.iter()
        .find(|item| {
            item.as_object()
                .and_then(|obj| obj.get("type"))
                .and_then(|value| value.as_str())
                .map(|value| value.eq_ignore_ascii_case("credit"))
                .unwrap_or(false)
        })
        .or_else(|| list.first())
}

fn days_until(timestamp: Option<i64>) -> Option<i64> {
    let ts = timestamp?;
    let now = Utc::now().timestamp();
    if ts <= now {
        return Some(0);
    }
    Some(((ts - now) as f64 / 86_400.0).ceil() as i64)
}
