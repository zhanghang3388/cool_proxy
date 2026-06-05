//! Kiro / CodeWhisperer 上游调用：构造 `generateAssistantResponse` 请求，
//! 附带 AWS SDK 风格的 headers，走账号绑定的代理。

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Response;
use uuid::Uuid;

use super::payload::KiroPayload;
use crate::proxy::ProxiedClients;

const KIRO_VERSION: &str = "0.12.155";
const AWS_SDK_VERSION: &str = "1.0.34";

/// 上游端点。两个 `generateAssistantResponse`，失败时按顺序 fallback。
pub struct KiroEndpoint {
    pub url: &'static str,
    pub name: &'static str,
}

pub const KIRO_ENDPOINTS: &[KiroEndpoint] = &[
    KiroEndpoint {
        url: "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse",
        name: "CodeWhisperer",
    },
    KiroEndpoint {
        url: "https://q.us-east-1.amazonaws.com/generateAssistantResponse",
        name: "AmazonQ",
    },
];

/// IDE 模式（social 账号）。
const AGENT_MODE_SPEC: &str = "spec";
/// CLI 模式（IdC 账号）。
const AGENT_MODE_VIBE: &str = "vibe";

fn kiro_user_agent() -> String {
    format!(
        "aws-sdk-js/{AWS_SDK_VERSION} ua/2.1 os/win32#10.0.19045 lang/js md/nodejs#22.22.0 \
         api/codewhispererstreaming#{AWS_SDK_VERSION} m/E KiroIDE-{KIRO_VERSION}"
    )
}

fn kiro_amz_user_agent() -> String {
    format!("aws-sdk-js/{AWS_SDK_VERSION} KiroIDE-{KIRO_VERSION}")
}

/// IdC / 企业账号在 generateAssistantResponse 上标识为 “Amazon Q for CLI”（aws-sdk-rust），
/// 与 Kiro 官方 / Kiro-account-manager 一致——IdC token 用 KiroIDE 标识可能被上游判为无效。
const KIRO_CLI_USER_AGENT: &str = "aws-sdk-rust/1.3.9 os/windows lang/rust/1.87.0";
const KIRO_CLI_AMZ_USER_AGENT: &str =
    "aws-sdk-rust/1.3.9 ua/2.1 api/ssooidc/1.88.0 os/windows lang/rust/1.87.0 m/E app/AmazonQ-For-CLI";

/// 向某个端点发起流式请求，返回原始 reqwest::Response（body 是 event-stream）。
pub async fn call_kiro(
    clients: &ProxiedClients,
    endpoint: &KiroEndpoint,
    payload: &KiroPayload,
    access_token: &str,
    auth_method: &str,
    proxy_url: &str,
) -> Result<Response> {
    let is_idc = auth_method.eq_ignore_ascii_case("idc");
    let agent_mode = if is_idc { AGENT_MODE_VIBE } else { AGENT_MODE_SPEC };
    // IdC/企业账号用 Amazon Q for CLI 标识，social 用 KiroIDE 标识。
    let (user_agent, amz_user_agent) = if is_idc {
        (
            KIRO_CLI_USER_AGENT.to_string(),
            KIRO_CLI_AMZ_USER_AGENT.to_string(),
        )
    } else {
        (kiro_user_agent(), kiro_amz_user_agent())
    };

    let body = serde_json::to_vec(payload).context("serialize kiro payload")?;
    let http = clients.get(proxy_url)?;

    let resp = http
        .post(endpoint.url)
        .timeout(Duration::from_secs(600))
        .header("content-type", "application/json")
        .header("x-amzn-kiro-agent-mode", agent_mode)
        .header("x-amz-user-agent", amz_user_agent)
        .header("user-agent", user_agent)
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=3")
        .header("Authorization", format!("Bearer {access_token}"))
        .body(body)
        .send()
        .await
        .with_context(|| format!("kiro request to {}", endpoint.name))?;

    Ok(resp)
}
