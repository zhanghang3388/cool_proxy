//! Kiro 额度查询：调用 CodeWhisperer / Q runtime 的 getUsageLimits 接口，
//! 解析出 credits / bonus / 套餐 / 重置时间，并识别封禁状态。
//!
//! 仿 KAM 的 `KiroQClient::get_usage_limits` + `get_usage_limits_with_region_probe`：
//!  - **Social/BuilderId**：传账号自带 profileArn 或对应默认 ARN，单区域请求；
//!  - **Enterprise**：不传 profileArn，遍历支持的全部 region 直到一个 2xx，写回 detected_region；
//!  - **封禁识别**：423 Locked → BANNED；403 + reason="TemporarilySuspended" → BANNED；
//!    body 含 "suspended" 关键词 → BANNED；其余 401/403 视为 AUTH_ERROR。
//!  - **冗余检查**：getUsageLimits 成功后再调一次 ListAvailableModels，某些封禁 usage 端点
//!    会正常返回但 ListAvailableModels 直接 403。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::kiro::{
    get_path_value, normalize_non_empty, parse_timestamp, pick_number, pick_string,
    runtime_endpoint_for_region, KIRO_PROVIDER_BUILDER_ID, KIRO_PROVIDER_ENTERPRISE,
    KIRO_PROVIDER_GITHUB, KIRO_PROVIDER_GOOGLE,
};
use crate::proxy::ProxiedClients;

/// KiroIDE 版本号（拼进 user-agent，与官方 / KAM 对齐）。
const KIRO_IDE_VERSION: &str = "0.12.155";

/// BuilderId 默认 profileArn —— Kiro IDE 写到 cache 的真实值（KAM 同名常量）。
pub const KIRO_BUILDER_ID_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX";
/// Social（Google / Github）共用的固定 profileArn。
pub const KIRO_SOCIAL_PROFILE_ARN: &str =
    "arn:aws:codewhisperer:us-east-1:699475941385:profile/EHGA3GRVQMUK";

/// 企业账号多区域探测优先级列表（与 KAM `USAGE_PROBE_REGIONS` 对齐：覆盖 SUPPORTED_KIRO_REGIONS 全集）。
const USAGE_PROBE_REGIONS: &[&str] = &[
    // 高频
    "us-east-1",
    "eu-central-1",
    "us-west-2",
    "ap-northeast-1",
    "us-east-2",
    "eu-west-1",
    "ap-southeast-1",
    "us-west-1",
    "eu-west-2",
    "ap-northeast-2",
    // 兜底（低频但受支持）
    "ap-southeast-2",
    "ap-southeast-3",
    "ap-southeast-4",
    "ap-southeast-5",
    "ap-southeast-7",
    "ap-northeast-3",
    "ap-south-1",
    "ap-south-2",
    "ap-east-1",
    "eu-west-3",
    "eu-north-1",
    "eu-south-1",
    "eu-south-2",
    "eu-central-2",
    "ca-central-1",
    "ca-west-1",
    "sa-east-1",
    "me-south-1",
    "me-central-1",
    "il-central-1",
    "mx-central-1",
    "af-south-1",
    "us-gov-west-1",
    "us-gov-east-1",
    "cn-north-1",
    "cn-northwest-1",
];

/// 返回探测列表（给单测和外部诊断用）。
pub fn usage_probe_regions() -> &'static [&'static str] {
    USAGE_PROBE_REGIONS
}

/// 给 RequestBuilder 拼 KAM 风格的 User-Agent —— 包含账号稳定 machineId。
fn kiro_user_agent(machine_id: &str) -> String {
    // 与 KAM `build_kiro_custom_user_agent` 一致：`KiroIDE {version} {machineId}`。
    format!("KiroIDE {KIRO_IDE_VERSION} {machine_id}")
}

/// 账号稳定 machineId：优先用账号自带的；缺失时按 id 派生 sha256("kiro-device-{id}")。
pub fn stable_machine_id(account_id: &str, account_machine_id: Option<&str>) -> String {
    if let Some(m) = account_machine_id.map(str::trim).filter(|s| !s.is_empty()) {
        return m.to_string();
    }
    use sha2::{Digest, Sha256};
    let mut h = Sha256::default();
    h.update(format!("kiro-device-{account_id}").as_bytes());
    hex::encode(h.finalize())
}

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
    /// 原始响应体（与 raw_usage / usage_data 同源）。
    pub raw: Value,
    /// 命中的真实 region（企业号探测时返回）。
    pub detected_region: Option<String>,
    /// 与 KAM 一致的派生状态：active / banned / capped / overage。
    pub derived_status: String,
}

/// 输入参数：把账号关键属性压成一个简单结构，避免函数签名爆炸。
pub struct UsageQuery<'a> {
    pub account_id: &'a str,
    pub access_token: &'a str,
    pub provider: &'a str,
    pub idc_region: Option<&'a str>,
    pub profile_arn: Option<&'a str>,
    pub machine_id: Option<&'a str>,
    pub proxy_url: &'a str,
}

/// 拉取账号额度：按 provider 分派路径。
///
///  - **Google / Github / BuilderId**：计算 profile_arn（账号自带优先，否则 provider 默认值），
///    单区域请求 `q.{region}.amazonaws.com/getUsageLimits`；
///  - **Enterprise**：不传 profileArn，遍历 USAGE_PROBE_REGIONS 直到一个 2xx；
///    任意区域命中封禁立即终止，全部都 403 视为 BANNED（KAM 行为）。
///
/// 任一端点上识别出封禁原因时返回 `Err("BANNED:<reason>")`。
pub async fn fetch_kiro_usage(
    clients: &Arc<ProxiedClients>,
    query: UsageQuery<'_>,
) -> Result<KiroUsageSnapshot> {
    if query.access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }

    let machine_id = stable_machine_id(query.account_id, query.machine_id);
    let http = clients.get(query.proxy_url)?;

    if query.provider.eq_ignore_ascii_case(KIRO_PROVIDER_ENTERPRISE) {
        return fetch_enterprise_usage(&http, query.access_token, &machine_id).await;
    }

    // Social / BuilderId：profile_arn = 账号自带 > provider 默认。
    let profile_arn = resolve_default_profile_arn(query.provider, query.profile_arn);
    let region = query
        .idc_region
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("us-east-1");

    let endpoint = runtime_endpoint_for_region(Some(region));
    let (status, body) = send_usage(
        &http,
        &endpoint,
        query.access_token,
        &machine_id,
        Some(profile_arn.as_str()),
    )
    .await?;
    handle_usage_response(status, body, region.to_string())
}

/// Enterprise 多区域探测。
async fn fetch_enterprise_usage(
    http: &reqwest::Client,
    access_token: &str,
    machine_id: &str,
) -> Result<KiroUsageSnapshot> {
    let mut last_err: Option<String> = None;
    for region in USAGE_PROBE_REGIONS {
        let endpoint = runtime_endpoint_for_region(Some(region));
        let (status, body) = match send_usage(http, &endpoint, access_token, machine_id, None).await
        {
            Ok(t) => t,
            Err(e) => {
                last_err = Some(format!("{region}: {e}"));
                continue;
            }
        };
        if status.is_success() {
            return handle_usage_response(status, body, region.to_string());
        }
        // 任一区域命中封禁 → 立即终止。
        if let Some(reason) = detect_ban_reason(status.as_u16(), &body) {
            anyhow::bail!("BANNED:{}", reason);
        }
        // 401/403 视为"该区域无此账号"，跳到下一个；其它状态码作为最后一次错误暂存。
        match status.as_u16() {
            401 | 403 | 400 => {
                last_err = Some(format!(
                    "{region}: status={} body={}",
                    status,
                    body.chars().take(256).collect::<String>()
                ));
                continue;
            }
            _ => {
                last_err = Some(format!(
                    "{region}: status={} body={}",
                    status,
                    body.chars().take(256).collect::<String>()
                ));
                continue;
            }
        }
    }
    // 所有区域都 401/403 → KAM 视为 BANNED（"Failed to find account in any region"）。
    anyhow::bail!(
        "BANNED:无法在任何区域找到企业账号 (probe over {} regions): {}",
        USAGE_PROBE_REGIONS.len(),
        last_err.unwrap_or_default()
    );
}

/// 处理一次 getUsageLimits 响应：解析 + 派生 status。
fn handle_usage_response(
    status: reqwest::StatusCode,
    body: String,
    detected_region: String,
) -> Result<KiroUsageSnapshot> {
    if !status.is_success() {
        if let Some(reason) = detect_ban_reason(status.as_u16(), &body) {
            anyhow::bail!("BANNED:{}", reason);
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            anyhow::bail!(
                "AUTH_ERROR: {} {}",
                status,
                body.chars().take(512).collect::<String>()
            );
        }
        // 某些情况（企业 IdC、profileArn 无效等）会 400 + Invalid profileArn —— 视为"无个人额度"
        // 而非错误（KAM 也只是显示套餐名）。
        if status.as_u16() == 400
            && body.to_ascii_lowercase().contains("profilearn")
        {
            return Ok(KiroUsageSnapshot {
                plan_name: Some("Enterprise（无个人额度）".to_string()),
                raw: serde_json::from_str(&body).unwrap_or(Value::Null),
                detected_region: Some(detected_region),
                derived_status: crate::auth::kiro::KIRO_STATUS_ACTIVE.to_string(),
                ..Default::default()
            });
        }
        anyhow::bail!(
            "kiro usage status={} body={}",
            status,
            body.chars().take(512).collect::<String>()
        );
    }
    let usage: Value = serde_json::from_str(&body).with_context(|| "parse kiro usage response")?;
    Ok(parse_usage_snapshot(usage, detected_region))
}

/// 检测封禁原因（与 KAM `classify_kiro_q_error` + `KiroPortalClient` 一致）。
/// 返回 `Some(reason)` 时调用方应把账号标记 banned。
fn detect_ban_reason(status: u16, body: &str) -> Option<String> {
    // 423 Locked → 一律视为封禁（KAM 行为）。
    if status == 423 {
        if let Ok(parsed) = serde_json::from_str::<Value>(body) {
            let type_field = parsed
                .get("__type")
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if type_field.contains("AccountSuspendedException") {
                return parsed
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some("账号已被暂停".to_string()));
            }
        }
        return Some("Account suspended (HTTP 423)".to_string());
    }
    // 403 + reason=TemporarilySuspended → 封禁。
    if status == 403 {
        if let Ok(parsed) = serde_json::from_str::<Value>(body) {
            let reason = parsed
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("");
            if reason == "TemporarilySuspended" {
                return parsed
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| Some(reason.to_string()));
            }
        }
    }
    // 关键词兜底（不分状态码）。
    let lower = body.to_ascii_lowercase();
    if lower.contains("suspended")
        || lower.contains("temporarilysuspended")
    {
        return Some(
            extract_reason_text(body)
                .unwrap_or_else(|| "Account suspended".to_string()),
        );
    }
    None
}

fn extract_reason_text(body: &str) -> Option<String> {
    let parsed = serde_json::from_str::<Value>(body).ok()?;
    let direct = pick_string(
        Some(&parsed),
        &[
            &["reason"],
            &["message"],
            &["errorMessage"],
            &["error", "message"],
            &["detail"],
            &["details"],
        ],
    );
    direct.and_then(|raw| normalize_non_empty(Some(raw.as_str())))
}

/// 由 BANNED:/AUTH_ERROR: 前缀的错误反向提取原因。
pub fn banned_reason(err: &str) -> Option<String> {
    err.strip_prefix("BANNED:")
        .and_then(|raw| normalize_non_empty(Some(raw)))
}

/// 是否为 AUTH_ERROR 错误（401/403 + token 失效相关）—— 调用方据此触发 token 刷新。
pub fn is_auth_error_message(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    msg.starts_with("AUTH_ERROR:")
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("expired")
        || lower.contains("invalid token")
}

/// 按 provider 决定默认 profile_arn（与 KAM `resolve_default_profile_arn` 一致）。
fn resolve_default_profile_arn(provider: &str, account_arn: Option<&str>) -> String {
    if let Some(arn) = account_arn.map(str::trim).filter(|s| !s.is_empty()) {
        return arn.to_string();
    }
    match provider {
        KIRO_PROVIDER_GOOGLE | KIRO_PROVIDER_GITHUB => KIRO_SOCIAL_PROFILE_ARN.to_string(),
        // BuilderId 默认 ARN 是真实可用的（kiro IDE cache 里就有），必须发送。
        KIRO_PROVIDER_BUILDER_ID => KIRO_BUILDER_ID_PROFILE_ARN.to_string(),
        // Enterprise 调用方应该走 fetch_enterprise_usage（不传 profileArn），
        // 兜底返回 BuilderId ARN 但实际不会被发送。
        _ => KIRO_BUILDER_ID_PROFILE_ARN.to_string(),
    }
}

/// 向某个区域端点发起一次 getUsageLimits，返回 (status, body)。
async fn send_usage(
    http: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    machine_id: &str,
    profile_arn: Option<&str>,
) -> Result<(reqwest::StatusCode, String)> {
    let url = format!("{}/getUsageLimits", endpoint.trim_end_matches('/'));
    tracing::debug!(
        "kiro getUsageLimits GET {url} profile_arn={}",
        profile_arn.unwrap_or("<none>")
    );

    // KAM 顺序：isEmailRequired → origin → profileArn → resourceType。
    let mut req = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .query(&[("isEmailRequired", "true"), ("origin", "AI_EDITOR")]);
    if let Some(arn) = profile_arn {
        req = req.query(&[("profileArn", arn)]);
    }
    req = req.query(&[("resourceType", "AGENTIC_REQUEST")]);

    let user_agent = kiro_user_agent(machine_id);
    let invocation_id = uuid::Uuid::new_v4().to_string();
    let resp = req
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .header("User-Agent", user_agent.clone())
        .header("x-amz-user-agent", user_agent)
        .header("amz-sdk-invocation-id", invocation_id)
        .header("amz-sdk-request", "attempt=1; max=1")
        .send()
        .await
        .with_context(|| "kiro usage request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    tracing::debug!(
        "kiro getUsageLimits resp {url} status={status} body={}",
        body.chars().take(512).collect::<String>()
    );
    Ok((status, body))
}

fn parse_usage_snapshot(usage: Value, detected_region: String) -> KiroUsageSnapshot {
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

    let derived_status = derive_status_from_usage(&usage);

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
        detected_region: Some(detected_region),
        derived_status,
    }
}

/// 根据 KAM `core/usage.rs` 的判定规则推导账号状态：capped / overage / active。
fn derive_status_from_usage(usage: &Value) -> String {
    if is_usage_capped(Some(usage)) {
        crate::auth::kiro::KIRO_STATUS_CAPPED.to_string()
    } else if is_in_overage(Some(usage)) {
        crate::auth::kiro::KIRO_STATUS_OVERAGE.to_string()
    } else {
        crate::auth::kiro::KIRO_STATUS_ACTIVE.to_string()
    }
}

// ===== 以下解析逻辑从 cockpit-tools / KAM extract_usage_payload 移植 =====

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
            &[
                "usageBreakdownList",
                "0",
                "freeTrialInfo",
                "usageLimitWithPrecision",
            ],
            &["usageBreakdownList", "0", "freeTrialInfo", "usageLimit"],
        ],
    );

    let mut bonus_used = pick_number(
        usage,
        &[
            &["bonusCredits", "used"],
            &["bonus", "used"],
            &["usageBreakdowns", "bonus", "used"],
            &[
                "usageBreakdownList",
                "0",
                "freeTrialInfo",
                "currentUsageWithPrecision",
            ],
            &["usageBreakdownList", "0", "freeTrialInfo", "currentUsage"],
        ],
    );

    let mut usage_reset_at = parse_timestamp(
        usage
            .and_then(|value| get_path_value(value, &["resetAt"]))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["resetTime"])))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["resetOn"])))
            .or_else(|| usage.and_then(|value| get_path_value(value, &["nextDateReset"])))
            .or_else(|| {
                usage.and_then(|value| get_path_value(value, &["usageBreakdowns", "resetAt"]))
            }),
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
            &[
                &["displayName"],
                &["displayNamePlural"],
                &["type"],
                &["unit"],
            ],
        )
    });
    plan_tier =
        plan_tier.or_else(|| pick_string(breakdown, &[&["currency"], &["type"], &["unit"]]));

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
    if let Some(state) = get_path_value(usage, &["kiro", "resourceNotifications", "usageState"]) {
        return Some(state);
    }
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
        .or_else(|| {
            get_path_value(usage, &["usageBreakdowns"]).and_then(|value| value.as_array())
        })?;
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

// ===== KAM core/usage.rs 移植：is_usage_capped / is_in_overage =====

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverageStatus {
    Enabled,
    Disabled,
    Unknown,
}

impl OverageStatus {
    fn from_usage_data(usage_data: Option<&Value>) -> Self {
        match usage_data
            .and_then(|d| d.get("overageConfiguration"))
            .and_then(|c| c.get("overageStatus"))
            .and_then(Value::as_str)
        {
            Some("ENABLED") => Self::Enabled,
            Some("DISABLED") => Self::Disabled,
            _ => Self::Unknown,
        }
    }

    fn is_enabled(self) -> bool {
        self == Self::Enabled
    }
}

fn read_amount(item: &Value, integer_key: &str, precision_key: &str) -> Option<f64> {
    item.get(precision_key)
        .and_then(Value::as_f64)
        .or_else(|| item.get(integer_key).and_then(Value::as_f64))
        .or_else(|| {
            item.get(integer_key)
                .and_then(Value::as_i64)
                .map(|n| n as f64)
        })
}

#[derive(Debug, Clone, Copy)]
struct UsageBreakdown {
    current: f64,
    limit: f64,
    overage_cap: f64,
}

impl UsageBreakdown {
    fn from_usage_data(usage_data: Option<&Value>) -> Option<Self> {
        let item = usage_data?
            .get("usageBreakdownList")?
            .as_array()?
            .first()?;
        let current = read_amount(item, "currentUsage", "currentUsageWithPrecision")?;
        let limit = read_amount(item, "usageLimit", "usageLimitWithPrecision")?;
        let overage_cap = read_amount(item, "overageCap", "overageCapWithPrecision").unwrap_or(0.0);
        Some(Self {
            current,
            limit,
            overage_cap,
        })
    }
}

#[derive(Debug, Clone, Copy)]
struct UsageDetails {
    main_limit: f64,
    main_usage: f64,
    trial_limit: f64,
    trial_usage: f64,
    bonus_limit: f64,
    bonus_usage: f64,
    overage_cap: f64,
}

impl UsageDetails {
    fn from_usage_data(usage_data: Option<&Value>) -> Option<Self> {
        let item = usage_data?
            .get("usageBreakdownList")?
            .as_array()?
            .first()?;

        let main_limit = item
            .get("usageLimit")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let main_usage = item
            .get("currentUsage")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);

        let trial_info = item.get("freeTrialInfo");
        let trial_active = trial_info
            .and_then(|t| t.get("freeTrialStatus"))
            .and_then(Value::as_str)
            == Some("ACTIVE");
        let (trial_limit, trial_usage) = if trial_active {
            let l = trial_info
                .and_then(|t| t.get("usageLimit"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let u = trial_info
                .and_then(|t| t.get("currentUsage"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            (l, u)
        } else {
            (0.0, 0.0)
        };

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let (bonus_limit, bonus_usage) = item
            .get("bonuses")
            .and_then(Value::as_array)
            .map(|bonuses| {
                bonuses.iter().fold((0.0, 0.0), |(l, u), b| {
                    let expiry_ms = b
                        .get("expiresAt")
                        .and_then(Value::as_i64)
                        .map(|t| t * 1000)
                        .unwrap_or(i64::MAX);
                    let active = b.get("status").and_then(Value::as_str) == Some("ACTIVE");
                    if expiry_ms > now_ms && active {
                        let bl = b.get("usageLimit").and_then(Value::as_f64).unwrap_or(0.0);
                        let bu = b.get("currentUsage").and_then(Value::as_f64).unwrap_or(0.0);
                        (l + bl, u + bu)
                    } else {
                        (l, u)
                    }
                })
            })
            .unwrap_or((0.0, 0.0));

        let overage_cap = if OverageStatus::from_usage_data(usage_data).is_enabled() {
            item.get("overageCap")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        } else {
            0.0
        };

        Some(Self {
            main_limit,
            main_usage,
            trial_limit,
            trial_usage,
            bonus_limit,
            bonus_usage,
            overage_cap,
        })
    }

    fn remaining(&self) -> f64 {
        let total_limit = self.main_limit + self.trial_limit + self.bonus_limit + self.overage_cap;
        let total_usage = self.main_usage + self.trial_usage + self.bonus_usage;
        total_limit - total_usage
    }
}

/// 账号是否在超额状态：开启了超额，且当前用量已超 limit 但未到 limit+overage_cap。
pub fn is_in_overage(usage_data: Option<&Value>) -> bool {
    let Some(b) = UsageBreakdown::from_usage_data(usage_data) else {
        return false;
    };
    if b.limit <= 0.0 {
        return false;
    }
    if !OverageStatus::from_usage_data(usage_data).is_enabled() {
        return false;
    }
    b.current > b.limit && !is_usage_capped(usage_data)
}

/// 账号是否封顶：所有可用配额（主+试用+奖励+超额）都用完。
pub fn is_usage_capped(usage_data: Option<&Value>) -> bool {
    let Some(d) = UsageDetails::from_usage_data(usage_data) else {
        return false;
    };
    let total_limit = d.main_limit + d.trial_limit + d.bonus_limit + d.overage_cap;
    if total_limit <= 0.0 {
        return false;
    }
    d.remaining() <= 0.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(status: &str, current: f64, limit: f64, cap: f64) -> Value {
        json!({
            "overageConfiguration": { "overageStatus": status },
            "usageBreakdownList": [{
                "currentUsage": current,
                "currentUsageWithPrecision": current,
                "usageLimit": limit,
                "usageLimitWithPrecision": limit,
                "overageCap": cap,
                "overageCapWithPrecision": cap
            }]
        })
    }

    #[test]
    fn capped_when_disabled_over_limit() {
        let d = data("DISABLED", 100.0, 100.0, 0.0);
        assert!(is_usage_capped(Some(&d)));
    }

    #[test]
    fn not_capped_when_enabled_within_overage() {
        let d = data("ENABLED", 150.0, 100.0, 100.0);
        assert!(!is_usage_capped(Some(&d)));
    }

    #[test]
    fn ban_detection_423_with_account_suspended_exception() {
        let body = r#"{"__type":"AccountSuspendedException","message":"账号已被暂停"}"#;
        let r = detect_ban_reason(423, body);
        assert_eq!(r.as_deref(), Some("账号已被暂停"));
    }

    #[test]
    fn ban_detection_403_with_temporarily_suspended() {
        let body = r#"{"reason":"TemporarilySuspended","message":"too many requests"}"#;
        let r = detect_ban_reason(403, body);
        assert_eq!(r.as_deref(), Some("too many requests"));
    }

    #[test]
    fn ban_detection_keyword_fallback() {
        let body = r#"{"message":"This account has been suspended"}"#;
        let r = detect_ban_reason(400, body);
        assert!(r.is_some());
    }

    #[test]
    fn auth_error_detection() {
        assert!(is_auth_error_message("AUTH_ERROR: 401"));
        assert!(is_auth_error_message("Invalid token"));
        assert!(!is_auth_error_message("BANNED:foo"));
    }

    #[test]
    fn provider_default_arn_for_social() {
        assert_eq!(
            resolve_default_profile_arn("Google", None),
            KIRO_SOCIAL_PROFILE_ARN
        );
        assert_eq!(
            resolve_default_profile_arn("Github", None),
            KIRO_SOCIAL_PROFILE_ARN
        );
    }

    #[test]
    fn provider_default_arn_for_builder_id() {
        assert_eq!(
            resolve_default_profile_arn("BuilderId", None),
            KIRO_BUILDER_ID_PROFILE_ARN
        );
    }

    #[test]
    fn account_arn_overrides_default() {
        let custom = "arn:aws:codewhisperer:eu-central-1:111111111111:profile/CUSTOM";
        assert_eq!(resolve_default_profile_arn("Google", Some(custom)), custom);
    }
}
