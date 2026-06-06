//! Kiro / CodeWhisperer `ListAvailableModels` 客户端：列出某账号当前可用的模型清单。
//!
//! 与 KAM `commands/account_models.rs` 行为一致：
//!  - URL：`https://q.{region}.amazonaws.com/ListAvailableModels`，
//!    query 参数：`origin=AI_EDITOR&maxResults=50&profileArn=...&modelProvider=...&nextToken=...`
//!  - Header：与 generateAssistantResponse 同形（`KiroIDE {ver} {machineId}` UA），
//!    但**不带** `x-amzn-kiro-agent-mode` / `x-amzn-codewhisperer-optout`（KAM 单测明确禁掉），
//!    也**不带** `TokenType`（KAM 注释：会触发 403）；
//!  - 翻页：`nextToken` 非空就接着拉，最终聚合 + `default_model` 置顶。
//!
//! 缓存策略（账号级，30 分钟 TTL）：把响应体连同时间戳序列化成 JSON 存到
//! `kiro_accounts.models_cache` 列；`/v1/models` 每次先查缓存，命中且 fresh 直接返回。

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::proxy::ProxiedClients;
use crate::proxy_kiro::upstream::resolve_account_region;

/// 缓存 TTL（秒）。与 KAM `AVAILABLE_MODELS_CACHE_TTL_SECONDS` 一致：30 分钟。
pub const MODELS_CACHE_TTL_SECONDS: i64 = 30 * 60;

/// KiroIDE 版本号 —— 与 upstream.rs 同步。
const KIRO_VERSION: &str = "0.12.155";

/// 完整 ListAvailableModels 响应：聚合所有翻页 + default_model。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableModelsResponse {
    /// 兼容 AWS API 返回的 `models` 字段和缓存里写的 `availableModels` 字段。
    #[serde(default, alias = "models")]
    pub available_models: Vec<AvailableModel>,
    pub next_token: Option<String>,
    pub default_model: Option<AvailableModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModel {
    pub model_id: String,
    #[serde(default)]
    pub model_name: String,
    #[serde(default)]
    pub description: String,
    pub provider: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub context_window: Option<i64>,
    pub is_default: Option<bool>,
    pub rate_multiplier: Option<f64>,
    pub rate_unit: Option<String>,
    pub prompt_caching: Option<AvailableModelPromptCaching>,
    #[serde(default)]
    pub supported_input_types: Vec<String>,
    pub token_limits: Option<AvailableModelTokenLimits>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelTokenLimits {
    pub max_input_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AvailableModelPromptCaching {
    pub maximum_cache_checkpoints_per_request: Option<i64>,
    pub minimum_tokens_per_cache_checkpoint: Option<i64>,
    pub supports_prompt_caching: Option<bool>,
}

/// 缓存条目：落进 `kiro_accounts.models_cache` 列的 JSON。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsCacheEntry {
    pub cached_at: i64,
    pub response: ListAvailableModelsResponse,
    pub model_provider: Option<String>,
}

/// 输入参数。machineId 缺失会用 region 兜底但 KAM 实测应避免。
pub struct ListModelsQuery<'a> {
    pub access_token: &'a str,
    pub provider: &'a str,
    pub idc_region: Option<&'a str>,
    pub profile_arn: Option<&'a str>,
    pub machine_id: Option<&'a str>,
    pub model_provider: Option<&'a str>,
    pub proxy_url: &'a str,
}

/// 拉取某账号当前可用的模型列表（自动翻页 + 排序 + default 置顶）。
///
/// 错误约定（与 KAM 一致）：
///  - `AUTH_ERROR: ...`：401 / 403 + 非封禁原因，调用方应触发 token 刷新；
///  - `BANNED: ...`：403 + suspended 关键词 / 423，调用方应标账号 banned。
pub async fn fetch_all_available_models(
    clients: &ProxiedClients,
    query: ListModelsQuery<'_>,
) -> Result<ListAvailableModelsResponse> {
    if query.access_token.trim().is_empty() {
        anyhow::bail!("missing access_token");
    }
    let region = resolve_account_region(query.profile_arn, query.idc_region);
    // Enterprise 账号绝不要传 profileArn（账号自身没绑定固定 ARN，传错就 403）。
    let profile_arn = if query
        .provider
        .eq_ignore_ascii_case(crate::auth::kiro::KIRO_PROVIDER_ENTERPRISE)
    {
        None
    } else {
        query
            .profile_arn
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let machine_id = crate::auth::kiro_quota::stable_machine_id("models", query.machine_id);
    let user_agent = kiro_user_agent(&machine_id);
    let http = clients.get(query.proxy_url)?;

    let mut aggregated = ListAvailableModelsResponse::default();
    let mut next_token: Option<String> = None;
    let mut pages = 0u32;

    // 防御：上游分页逻辑出错时不要无限循环。50 页 * 50 条 = 2500 个模型，足够。
    while pages < 50 {
        pages += 1;
        let url = build_list_url(
            &region,
            profile_arn,
            query.model_provider,
            next_token.as_deref(),
        );
        tracing::debug!("kiro ListAvailableModels GET {url}");
        let resp = http
            .get(&url)
            .timeout(Duration::from_secs(30))
            .header("authorization", format!("Bearer {}", query.access_token))
            .header("accept", "application/json")
            .header("user-agent", user_agent.clone())
            .header("x-amz-user-agent", user_agent.clone())
            .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .send()
            .await
            .with_context(|| "kiro ListAvailableModels request")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            classify_error(status.as_u16(), &body)?;
            unreachable!();
        }

        let mut page: ListAvailableModelsResponse = serde_json::from_str(&body)
            .with_context(|| format!("parse ListAvailableModels response: {body}"))?;

        // 第一页带回的 default_model 留下来；后续页保留首次出现的值。
        if aggregated.default_model.is_none() {
            aggregated.default_model = page.default_model.clone();
        }
        let default_id = aggregated
            .default_model
            .as_ref()
            .map(|m| m.model_id.as_str());
        mark_default_model(&mut page.available_models, default_id);
        if let Some(d) = aggregated.default_model.as_mut() {
            d.is_default = Some(true);
        }

        aggregated.available_models.extend(page.available_models);
        next_token = page.next_token;
        if next_token.is_none() {
            break;
        }
    }

    ensure_default_model_present(&mut aggregated);
    sort_models_default_first(&mut aggregated.available_models);
    aggregated.next_token = None;
    Ok(aggregated)
}

/// 把上游错误状态码 + body 映射成 anyhow::Error，前缀 AUTH_ERROR / BANNED 与 KAM 对齐。
fn classify_error(status: u16, body: &str) -> Result<()> {
    if status == 401 {
        anyhow::bail!("AUTH_ERROR: ListAvailableModels 401: {body}");
    }
    if status == 403 {
        let lower = body.to_ascii_lowercase();
        if (body.contains("AccessDeniedException") && body.contains("TemporarilySuspended"))
            || lower.contains("suspended")
        {
            anyhow::bail!("BANNED: ListAvailableModels: {body}");
        }
        anyhow::bail!("AUTH_ERROR: ListAvailableModels 403: {body}");
    }
    if status == 423 {
        anyhow::bail!("BANNED: Account suspended (HTTP 423)");
    }
    anyhow::bail!("ListAvailableModels failed - HTTP {status}: {body}");
}

fn build_list_url(
    region: &str,
    profile_arn: Option<&str>,
    model_provider: Option<&str>,
    next_token: Option<&str>,
) -> String {
    let mut url = format!(
        "https://q.{region}.amazonaws.com/ListAvailableModels?origin=AI_EDITOR&maxResults=50"
    );
    if let Some(arn) = profile_arn.filter(|s| !s.trim().is_empty()) {
        url.push_str("&profileArn=");
        url.push_str(&urlencode(arn));
    }
    if let Some(mp) = model_provider.filter(|s| !s.trim().is_empty()) {
        url.push_str("&modelProvider=");
        url.push_str(&urlencode(mp));
    }
    if let Some(token) = next_token.filter(|s| !s.trim().is_empty()) {
        url.push_str("&nextToken=");
        url.push_str(&urlencode(token));
    }
    url
}

/// 只走 reqwest 已经依赖的 url 库做 percent encoding（避免新引 `urlencoding` crate）。
fn urlencode(s: &str) -> String {
    // reqwest::Url 没暴露纯字符串编码 API，自己手写一次 percent-encode (RFC 3986 unreserved)。
    let mut out = String::with_capacity(s.len());
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn kiro_user_agent(machine_id: &str) -> String {
    let mid = machine_id.trim();
    if mid.is_empty() {
        format!("KiroIDE {KIRO_VERSION}")
    } else {
        format!("KiroIDE {KIRO_VERSION} {mid}")
    }
}

fn mark_default_model(models: &mut [AvailableModel], default_id: Option<&str>) {
    if let Some(id) = default_id {
        for m in models {
            if m.model_id == id && m.is_default.is_none() {
                m.is_default = Some(true);
            }
        }
    }
}

fn ensure_default_model_present(resp: &mut ListAvailableModelsResponse) {
    if let Some(default_model) = resp.default_model.clone() {
        if resp
            .available_models
            .iter()
            .all(|m| m.model_id != default_model.model_id)
        {
            resp.available_models.insert(0, default_model);
        }
    }
}

fn sort_models_default_first(models: &mut [AvailableModel]) {
    models.sort_by_key(|m| !m.is_default.unwrap_or(false));
}

/// 缓存读：fresh 才返回；force_refresh / model_provider 不匹配 / 解析失败都回 None。
pub fn read_models_cache(
    cache_value: &Value,
    model_provider: Option<&str>,
    force_refresh: bool,
) -> Option<ListAvailableModelsResponse> {
    if force_refresh {
        return None;
    }
    let entry: ModelsCacheEntry = serde_json::from_value(cache_value.clone()).ok()?;
    let now = Utc::now().timestamp();
    if now.saturating_sub(entry.cached_at) > MODELS_CACHE_TTL_SECONDS {
        return None;
    }
    if entry.model_provider.as_deref() != model_provider {
        return None;
    }
    Some(entry.response)
}

/// 缓存写：把响应连同时间戳序列化成 JSON。
pub fn build_cache_entry(
    response: &ListAvailableModelsResponse,
    model_provider: Option<&str>,
) -> Value {
    let entry = ModelsCacheEntry {
        cached_at: Utc::now().timestamp(),
        response: response.clone(),
        model_provider: model_provider.map(str::to_string),
    };
    serde_json::to_value(entry).unwrap_or(Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_includes_origin_and_max_results() {
        let url = build_list_url("us-east-1", None, None, None);
        assert!(url.contains("origin=AI_EDITOR"));
        assert!(url.contains("maxResults=50"));
        assert!(url.starts_with("https://q.us-east-1.amazonaws.com/ListAvailableModels?"));
    }

    #[test]
    fn url_appends_profile_and_provider_when_present() {
        let url = build_list_url(
            "eu-central-1",
            Some("arn:aws:codewhisperer:::profile/test"),
            Some("anthropic"),
            Some("page-2"),
        );
        assert!(url.contains("profileArn=arn%3Aaws%3Acodewhisperer%3A%3A%3Aprofile%2Ftest"));
        assert!(url.contains("modelProvider=anthropic"));
        assert!(url.contains("nextToken=page-2"));
    }

    #[test]
    fn deserialize_supports_aws_api_format() {
        let r: ListAvailableModelsResponse = serde_json::from_value(serde_json::json!({
            "models": [
                {"modelId": "claude-sonnet-4.5", "modelName": "Claude Sonnet 4.5"}
            ],
            "nextToken": "p2",
        }))
        .unwrap();
        assert_eq!(r.available_models.len(), 1);
        assert_eq!(r.available_models[0].model_id, "claude-sonnet-4.5");
        assert_eq!(r.next_token.as_deref(), Some("p2"));
    }

    #[test]
    fn deserialize_supports_cached_format_with_available_models_alias() {
        let r: ListAvailableModelsResponse = serde_json::from_value(serde_json::json!({
            "availableModels": [
                {"modelId": "auto", "modelName": "Auto"}
            ]
        }))
        .unwrap();
        assert_eq!(r.available_models.len(), 1);
    }

    #[test]
    fn default_model_is_inserted_and_sorted_first() {
        let mut r = ListAvailableModelsResponse {
            available_models: vec![
                AvailableModel {
                    model_id: "claude-sonnet-4.5".to_string(),
                    ..serde_json::from_value(serde_json::json!({"modelId":"claude-sonnet-4.5"})).unwrap()
                },
            ],
            default_model: Some(
                serde_json::from_value(serde_json::json!({"modelId":"auto","modelName":"Auto"}))
                    .unwrap(),
            ),
            next_token: None,
        };
        ensure_default_model_present(&mut r);
        sort_models_default_first(&mut r.available_models);
        assert_eq!(r.available_models[0].model_id, "auto");
    }

    #[test]
    fn cache_round_trip() {
        let resp = ListAvailableModelsResponse {
            available_models: vec![],
            default_model: Some(
                serde_json::from_value(serde_json::json!({"modelId":"auto","modelName":"Auto"}))
                    .unwrap(),
            ),
            next_token: None,
        };
        let value = build_cache_entry(&resp, Some("anthropic"));
        let cached = read_models_cache(&value, Some("anthropic"), false).unwrap();
        assert_eq!(
            cached.default_model.as_ref().unwrap().model_id,
            "auto"
        );
        // 不匹配的 model_provider 应 miss
        assert!(read_models_cache(&value, Some("openai"), false).is_none());
        // force_refresh 直接 miss
        assert!(read_models_cache(&value, Some("anthropic"), true).is_none());
    }

    #[test]
    fn cache_expires_after_ttl() {
        // 手工伪造一个 cached_at = 0 的旧条目
        let v = serde_json::json!({
            "cachedAt": 0,
            "response": { "availableModels": [] },
            "modelProvider": null,
        });
        assert!(read_models_cache(&v, None, false).is_none());
    }
}
