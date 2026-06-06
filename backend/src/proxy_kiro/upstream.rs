//! Kiro / CodeWhisperer 上游调用：构造 `generateAssistantResponse` 请求头与请求体。
//!
//! Header 形态严格对齐 KAM `with_kiro_upstream_headers` —— 这点至关重要：用错 UA 或
//! agent-mode 会让 BuilderId / Enterprise 账号被上游判为「your subscription does not
//! support this application」（403），因为 AWS 会按 UA + agent-mode 反查订阅是否覆盖
//! 当前调用的应用身份。
//!
//! 关键约束：
//!  - 所有账号类型（social / IdC）都用 `KiroIDE {ver} {machineId}` 形态的 UA；
//!  - 所有账号类型都带 `x-amzn-kiro-agent-mode: q-developer-converse`；
//!  - 所有账号类型都带 `x-amzn-codewhisperer-optout: true`；
//!  - `TokenType` header 一律不发（KAM 移除原因：会触发 403）；
//!  - URL 用 `q.{region}.amazonaws.com/generateAssistantResponse`，region 由账号自身
//!    的 profileArn / idc_region 解析，不再硬编码 us-east-1（企业号 region 错位 = 403）。

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Response;
use uuid::Uuid;

use super::payload::KiroPayload;
use crate::proxy::ProxiedClients;

/// KiroIDE 版本号 —— 与 KAM `build_kiro_custom_user_agent` 输出一致：`KiroIDE x.y.z {machineId}`。
const KIRO_VERSION: &str = "0.12.155";
/// `x-amzn-kiro-agent-mode` 唯一可用值（KAM `DEFAULT_AGENT_MODE`）。其它值会让上游 403。
const KIRO_AGENT_MODE: &str = "q-developer-converse";

/// 上游端点描述。仅用于日志展示，URL 由账号 region 动态拼出。
pub struct KiroEndpoint {
    pub name: &'static str,
    pub region: &'static str,
}

/// 候选 region 列表 —— 当账号自带的 region 解析不出来或被上游 403 时按顺序兜底。
/// 与 KAM `USAGE_PROBE_REGIONS` 高频段对齐。
pub const KIRO_FALLBACK_ENDPOINTS: &[KiroEndpoint] = &[
    KiroEndpoint {
        name: "us-east-1",
        region: "us-east-1",
    },
    KiroEndpoint {
        name: "eu-central-1",
        region: "eu-central-1",
    },
    KiroEndpoint {
        name: "us-west-2",
        region: "us-west-2",
    },
    KiroEndpoint {
        name: "ap-northeast-1",
        region: "ap-northeast-1",
    },
];

/// 拼出某个 region 的 generateAssistantResponse URL。
fn build_url(region: &str) -> String {
    format!("https://q.{region}.amazonaws.com/generateAssistantResponse")
}

/// 解析账号优先 region：profile_arn > idc_region > 默认 us-east-1。
pub fn resolve_account_region(
    profile_arn: Option<&str>,
    idc_region: Option<&str>,
) -> String {
    if let Some(arn) = profile_arn {
        if let Some(r) = crate::auth::kiro::parse_profile_arn_region(arn) {
            return r;
        }
    }
    idc_region
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "us-east-1".to_string())
}

/// `KiroIDE {version} {machineId}` —— 与 KAM 完全一致。machineId 缺失时退到无机器码版本，
/// 但 KAM 实测下应避免出现这种情况（pool 添加账号时会自动分配 uuid machineId）。
fn kiro_user_agent(machine_id: &str) -> String {
    let mid = machine_id.trim();
    if mid.is_empty() {
        format!("KiroIDE {KIRO_VERSION}")
    } else {
        format!("KiroIDE {KIRO_VERSION} {mid}")
    }
}

/// 向某个 region 的 generateAssistantResponse 端点发起流式请求。
///
/// `auth_method` 当前仅用于诊断日志；实际 header 集合所有账号都一样。
pub async fn call_kiro(
    clients: &ProxiedClients,
    region: &str,
    payload: &KiroPayload,
    access_token: &str,
    _auth_method: &str,
    machine_id: Option<&str>,
    proxy_url: &str,
) -> Result<Response> {
    let machine_id = machine_id.unwrap_or("").trim();
    let user_agent = kiro_user_agent(machine_id);
    let url = build_url(region);

    let body = serde_json::to_vec(payload).context("serialize kiro payload")?;
    let http = clients.get(proxy_url)?;

    let resp = http
        .post(&url)
        .timeout(Duration::from_secs(600))
        .header("content-type", "application/json")
        // event-stream Accept：与 KAM 一致，触发 chunked event-stream 响应。
        .header("accept", "application/vnd.amazon.eventstream")
        .header("authorization", format!("Bearer {access_token}"))
        .header("user-agent", user_agent.clone())
        .header("x-amz-user-agent", user_agent)
        .header("x-amzn-codewhisperer-optout", "true")
        .header("x-amzn-kiro-agent-mode", KIRO_AGENT_MODE)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .body(body)
        .send()
        .await
        .with_context(|| format!("kiro request to q.{region}.amazonaws.com"))?;

    Ok(resp)
}
