//! Kiro 额度查询：调用 CodeWhisperer / Q runtime 的 getUsageLimits 接口，
//! 解析出 credits / bonus / 套餐 / 重置时间，并识别封禁状态。
//! 解析路径从 cockpit-tools 的 `extract_usage_payload` 移植。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::auth::kiro::{
    get_path_value, normalize_non_empty, parse_timestamp, pick_number, pick_string,
    runtime_endpoint_for_region,
};
use crate::proxy::ProxiedClients;

/// KiroIDE 版本号（拼进 user-agent，与官方 / Kiro-account-manager 对齐）。
const KIRO_IDE_VERSION: &str = "0.12.155";

/// 额度查询客户端标识，逐字对齐 Kiro-account-manager 的 getUsageLimits：`User-Agent` /
/// `x-amz-user-agent` 都带账号稳定 machineId。CodeWhisperer 边缘要求可识别的 aws-sdk
/// 客户端，缺失这些头会让 token 被判 "bearer token invalid"。
fn kiro_usage_user_agent(machine_id: &str) -> String {
    format!(
        "aws-sdk-js/1.0.18 ua/2.1 os/windows lang/js md/nodejs#20.16.0 \
         api/codewhispererstreaming#1.0.18 m/E KiroIDE-{KIRO_IDE_VERSION}-{machine_id}"
    )
}

fn kiro_usage_amz_user_agent(machine_id: &str) -> String {
    format!("aws-sdk-js/1.0.18 KiroIDE {KIRO_IDE_VERSION} {machine_id}")
}

/// 账号稳定 machineId：sha256("kiro-device-{id}")，与 Kiro-account-manager 一致。
fn stable_machine_id(account_id: &str) -> String {
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
    /// 原始响应体，落库备查。
    pub raw: Value,
}

/// 拉取账号额度。按账号 `idc_region` 推断的端点优先，再补 q.us-east-1 / q.eu-central-1，
/// **遍历**候选端点、接受第一个 2xx——企业 IdC 账号的 Q profile 只托管在某一个固定区域，
/// 打错区域上游可能回 400（不是 403），单端点 + 仅 403 回退会漏掉真正托管 profile 的区域
/// （对齐 Kiro-account-manager verifyAlive 的双端点遍历）。
/// 任一端点上识别出封禁原因时返回 `Err("BANNED:<reason>")`，调用方据此标记账号。
pub async fn fetch_kiro_usage(
    clients: &Arc<ProxiedClients>,
    account_id: &str,
    access_token: &str,
    idc_region: Option<&str>,
    proxy_url: &str,
) -> Result<KiroUsageSnapshot> {
    if access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }

    // 候选端点：先按账号自身 region（idc_region）推断的端点（覆盖 gov/iso 专用端点），
    // 再补两个标准区域端点 q.us-east-1 / q.eu-central-1（去重）。
    let region = idc_region.filter(|s| !s.trim().is_empty());
    let mut endpoints = vec![runtime_endpoint_for_region(region)];
    for std_ep in [
        "https://q.us-east-1.amazonaws.com",
        "https://q.eu-central-1.amazonaws.com",
    ] {
        if !endpoints.iter().any(|e| e == std_ep) {
            endpoints.push(std_ep.to_string());
        }
    }

    let machine_id = stable_machine_id(account_id);
    let http = clients.get(proxy_url)?;

    // 遍历候选端点：第一个 2xx 即解析返回；任一端点上命中封禁原因立即终止；
    // 否则记录最后一次非 2xx，留待循环后统一判定（无个人额度 / 报错）。
    let mut last: Option<(reqwest::StatusCode, String)> = None;
    for endpoint in &endpoints {
        let (status, body) = send_usage(&http, endpoint, access_token, &machine_id).await?;
        if status.is_success() {
            let usage: Value =
                serde_json::from_str(&body).with_context(|| "parse kiro usage response")?;
            return Ok(parse_usage_snapshot(usage));
        }
        // 真封禁/停用：任何端点上命中都立即终止，不必再试其它区域。
        if let Some(r) = parse_runtime_error_reason(&body).as_deref() {
            if looks_like_ban(r) {
                anyhow::bail!("BANNED:{}", r);
            }
        }
        last = Some((status, body));
    }

    // 所有候选端点都非 2xx——到这里才允许降级或报错。
    let (status, body) = last.expect("至少尝试过一个端点");
    let reason = parse_runtime_error_reason(&body);

    // 组织/企业托管账号没有可查的个人额度：所有区域端点都回 400 "Invalid profileArn"
    // （额度按组织池子算、不按人头）。只有在**所有端点都试过**后仍是 400，才按"企业版、
    // 无个人额度"优雅返回——账号正常可用、不报错、不禁用（对齐 Kiro-account-manager 企业号
    // 只显示套餐名）。匹配放宽：reason 含 "profilearn"，或 400 但拿不到可读原因（AWS 可能把
    // 校验消息放在 __type 等 parse_runtime_error_reason 不读的字段）都按无个人额度处理；
    // 只有带明确"非 profilearn"原因的 400 才作为真错误暴露。
    let no_personal_quota = status == reqwest::StatusCode::BAD_REQUEST
        && reason
            .as_deref()
            .map(|r| r.to_ascii_lowercase().contains("profilearn"))
            .unwrap_or(true);
    if no_personal_quota {
        return Ok(KiroUsageSnapshot {
            plan_name: Some("Enterprise（无个人额度）".to_string()),
            raw: serde_json::from_str(&body).unwrap_or(Value::Null),
            ..Default::default()
        });
    }

    let detail = reason.unwrap_or_else(|| body.chars().take(512).collect::<String>());
    anyhow::bail!("kiro usage request failed: {} {}", status, detail);
}

/// 向某个区域端点发起一次 getUsageLimits，返回 (status, body)。逐字对齐 Kiro-account-manager
/// 的 `fetchRestApi`：只带 Accept / Authorization / UA 两件套，**不传 profileArn**
/// （KAM 所有 getUsageLimits 调用点都不传，由上游按 token 身份推断 profile）。
async fn send_usage(
    http: &reqwest::Client,
    endpoint: &str,
    access_token: &str,
    machine_id: &str,
) -> Result<(reqwest::StatusCode, String)> {
    let url = format!("{}/getUsageLimits", endpoint.trim_end_matches('/'));
    // 诊断用：明确记录实际发出的 getUsageLimits（不带 profileArn）。
    tracing::info!("kiro getUsageLimits GET {url} (origin=AI_EDITOR, 无 profileArn)");
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(30))
        .query(&[
            ("origin", "AI_EDITOR"),
            ("resourceType", "AGENTIC_REQUEST"),
            ("isEmailRequired", "true"),
        ])
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {}", access_token.trim()))
        .header("User-Agent", kiro_usage_user_agent(machine_id))
        .header("x-amz-user-agent", kiro_usage_amz_user_agent(machine_id))
        .send()
        .await
        .with_context(|| "kiro usage request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    // 诊断用：记录真实响应 status + body 前 512 字符——确认企业号查额度命中的端点、
    // 实际状态码（是否 400 / 是否含 "Invalid profileArn"）与降级路径。
    tracing::info!(
        "kiro getUsageLimits resp {} status={} body={}",
        url,
        status,
        body.chars().take(512).collect::<String>()
    );
    Ok((status, body))
}

/// 判断上游错误原因是否为封禁/停用类（区分真封号 vs 认证/限流等可恢复错误）。
fn looks_like_ban(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    [
        "suspend",
        "banned",
        "improper",
        "violat",
        "abuse",
        "fraud",
        "disabled",
        "terminated",
    ]
    .iter()
    .any(|k| l.contains(k))
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
    // 正确的嵌套遍历：kiro -> resourceNotifications -> usageState。
    // （get_path_value 是逐元素 obj.get(key)，单个带点字符串只会去找名字真带点的扁平 key，
    // 不会逐层下钻，因此这里必须拆成多段。）
    if let Some(state) = get_path_value(usage, &["kiro", "resourceNotifications", "usageState"]) {
        return Some(state);
    }
    // 兜底兼容：万一上游真用扁平点号 key。
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
