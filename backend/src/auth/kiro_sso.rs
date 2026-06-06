//! Kiro SSO（AWS IAM Identity Center / Builder ID）登录：OAuth2 Authorization Code + PKCE。
//!
//! 仿 KAM `IdcProvider::login`，但做成与 Claude OAuth 一致的"展示授权链接 → 用户在浏览器
//! 完成授权 → 手动回填授权码"两步式，便于在没有本地浏览器的服务端使用。
//!
//! 与之前实现的差异：
//!  - 同时支持 BuilderId（Start URL 默认 `view.awsapps.com/start`）与 Enterprise
//!    （组织的 d-xxx 域名）—— 由调用方传 provider 决定；
//!  - register 时 `redirectUris` **不带端口** 走 `http://127.0.0.1/oauth/callback`，
//!    KAM v1.7.4+ 已确认这样才能拿到带 REFRESH_TOKEN grant 的 client（带端口注册的
//!    client 一过期就拒刷）；authorize/token 仍用真实带端口的 redirect_uri；
//!  - pending 上下文新增 provider 字段，为 token 交换后入池决定 provider。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::auth::claude::gen_pkce;
use crate::auth::kiro::{KIRO_PROVIDER_BUILDER_ID, KIRO_PROVIDER_ENTERPRISE};

/// AWS OIDC 端点基址模板（client/register、authorize、token 共用）。
const OIDC_BASE_FMT: &str = "https://oidc.{region}.amazonaws.com";
/// 真实回调地址：authorize 返回时浏览器会跳到这里（端口固定，方便用户复制粘贴）。
const REDIRECT_URI: &str = "http://127.0.0.1:54546/oauth/callback";
/// register 时传给 AWS 的 redirect_uri：必须**不带端口**，才能拿到带 REFRESH_TOKEN grant
/// 的 public client。带端口注册出来的 client `enabledGrants.REFRESH_TOKEN` 会缺失，
/// IDE/KAM 一过期就刷不动。AWS 对 loopback 地址按 RFC 8252 允许任意端口匹配。
const REGISTER_REDIRECT_URI: &str = "http://127.0.0.1/oauth/callback";
/// CodeWhisperer / Q 所需 scope（与 Kiro IDE 一致）。
pub const SCOPES: &[&str] = &[
    "codewhisperer:completions",
    "codewhisperer:analysis",
    "codewhisperer:conversations",
    "codewhisperer:transformations",
    "codewhisperer:taskassist",
];
/// 待完成登录有效期：超过则需重新获取授权链接。
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

/// 一次进行中的 SSO 登录的暂存上下文（拿链接时写入，回填授权码时取出）。
#[derive(Clone)]
pub struct PendingKiroSso {
    pub verifier: String,
    pub client_id: String,
    pub client_secret: String,
    pub region: String,
    pub start_url: String,
    pub proxy_url: String,
    pub email_hint: Option<String>,
    /// "BuilderId" 或 "Enterprise" —— token 交换后用于决定入池 provider。
    pub provider: String,
    created: Instant,
}

/// 暂存 `state -> PendingKiroSso`，支持两步登录（拿链接 → 回填授权码）。
#[derive(Default)]
pub struct KiroSsoLoginStore {
    pending: Mutex<HashMap<String, PendingKiroSso>>,
}

impl KiroSsoLoginStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 第一步：注册 OIDC 客户端 + 生成 PKCE/state + 构造授权链接，存下上下文。
    /// 返回 `(auth_url, state)`。需要一个（可走代理的）HTTP 客户端来调 register。
    pub async fn start(
        &self,
        http: &reqwest::Client,
        provider: &str,
        start_url: &str,
        region: &str,
        proxy_url: &str,
        email_hint: Option<String>,
    ) -> Result<(String, String)> {
        let provider = match provider {
            KIRO_PROVIDER_BUILDER_ID => KIRO_PROVIDER_BUILDER_ID,
            KIRO_PROVIDER_ENTERPRISE => KIRO_PROVIDER_ENTERPRISE,
            other => anyhow::bail!("unsupported sso provider: {other}"),
        };
        let oidc_base = OIDC_BASE_FMT.replace("{region}", region);
        let (client_id, client_secret) = register_client(http, &oidc_base, start_url).await?;

        let pkce = gen_pkce();
        let state = Uuid::new_v4().simple().to_string();
        let auth_url = build_authorize_url(&oidc_base, &client_id, &state, &pkce.challenge);

        let mut map = self.pending.lock().unwrap();
        prune(&mut map);
        map.insert(
            state.clone(),
            PendingKiroSso {
                verifier: pkce.verifier,
                client_id,
                client_secret,
                region: region.to_string(),
                start_url: start_url.to_string(),
                proxy_url: proxy_url.to_string(),
                email_hint,
                provider: provider.to_string(),
                created: Instant::now(),
            },
        );
        Ok((auth_url, state))
    }

    /// 读取（不消费）某 state 的上下文：换 token 失败时保留，便于改正授权码后重试。
    pub fn peek(&self, state: &str) -> Option<PendingKiroSso> {
        let mut map = self.pending.lock().unwrap();
        prune(&mut map);
        map.get(state).cloned()
    }

    /// 登录成功后消费掉该 state。
    pub fn remove(&self, state: &str) {
        let mut map = self.pending.lock().unwrap();
        map.remove(state);
    }
}

fn prune(map: &mut HashMap<String, PendingKiroSso>) {
    map.retain(|_, p| p.created.elapsed() < LOGIN_TTL);
}

/// 注册一个 public OIDC 客户端（authorization_code + refresh_token），返回 `(clientId, clientSecret)`。
async fn register_client(
    http: &reqwest::Client,
    oidc_base: &str,
    start_url: &str,
) -> Result<(String, String)> {
    let body = json!({
        "clientName": "Kiro IDE",
        "clientType": "public",
        "scopes": SCOPES,
        "grantTypes": ["authorization_code", "refresh_token"],
        // 注册必须用不带端口的 redirect_uri，才能拿到带 REFRESH_TOKEN grant 的 client。
        "redirectUris": [REGISTER_REDIRECT_URI],
        "issuerUrl": start_url,
    });
    let resp = http
        .post(format!("{oidc_base}/client/register"))
        .timeout(Duration::from_secs(60))
        .header("Content-Type", "application/json")
        .header("x-amz-user-agent", "aws-sdk-js/3.980.0 KiroIDE")
        .json(&body)
        .send()
        .await
        .with_context(|| "kiro oidc client/register request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        if status.as_u16() == 400 && text.to_ascii_lowercase().contains("invalid start url provided") {
            anyhow::bail!(
                "Start URL 无效。请检查 IAM Identity Center Start URL 是否正确。示例：https://d-1234567890.awsapps.com/start"
            );
        }
        anyhow::bail!(
            "client/register 失败 status={status} body={}",
            text.chars().take(512).collect::<String>()
        );
    }
    let v: Value =
        serde_json::from_str(&text).with_context(|| format!("parse register response: {text}"))?;
    let client_id = v
        .get("clientId")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("register 响应缺少 clientId"))?
        .to_string();
    let client_secret = v
        .get("clientSecret")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("register 响应缺少 clientSecret"))?
        .to_string();
    Ok((client_id, client_secret))
}

/// 构造 authorize 链接（scopes 用逗号连接，对齐 Kiro IDE）。
fn build_authorize_url(oidc_base: &str, client_id: &str, state: &str, challenge: &str) -> String {
    let scope_str = SCOPES.join(",");
    let endpoint = format!("{oidc_base}/authorize");
    match reqwest::Url::parse_with_params(
        &endpoint,
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("scopes", scope_str.as_str()),
            ("state", state),
            ("code_challenge", challenge),
            ("code_challenge_method", "S256"),
        ],
    ) {
        Ok(u) => u.to_string(),
        Err(_) => format!(
            "{endpoint}?response_type=code&client_id={client_id}&redirect_uri={REDIRECT_URI}\
             &scopes={scope_str}&state={state}&code_challenge={challenge}&code_challenge_method=S256"
        ),
    }
}

/// 第二步：用授权码换 token，返回原始 token JSON（`{accessToken, refreshToken, expiresIn, ...}`）。
pub async fn exchange_code(
    http: &reqwest::Client,
    pending: &PendingKiroSso,
    raw_code: &str,
) -> Result<Value> {
    let (code, _state) = parse_code_and_state(raw_code);
    if code.is_empty() {
        anyhow::bail!("未能从回填内容里解析出授权码");
    }
    let oidc_base = OIDC_BASE_FMT.replace("{region}", &pending.region);
    let body = json!({
        "clientId": pending.client_id,
        "clientSecret": pending.client_secret,
        "grantType": "authorization_code",
        "redirectUri": REDIRECT_URI,
        "code": code,
        "codeVerifier": pending.verifier,
    });
    let resp = http
        .post(format!("{oidc_base}/token"))
        .timeout(Duration::from_secs(60))
        .header("Content-Type", "application/json")
        .header("x-amz-user-agent", "aws-sdk-js/3.980.0 KiroIDE")
        .header("amz-sdk-invocation-id", Uuid::new_v4().to_string())
        .header("amz-sdk-request", "attempt=1; max=4")
        .json(&body)
        .send()
        .await
        .with_context(|| "kiro oidc token exchange request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "token 交换失败 status={status} body={}",
            text.chars().take(512).collect::<String>()
        );
    }
    serde_json::from_str(&text).with_context(|| format!("parse token response: {text}"))
}

/// 从用户回填内容里抽出授权码与 state（兼容整段回调 URL / 查询串 / `code#state` / 纯 code）。
fn parse_code_and_state(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    let as_url = if raw.starts_with("http://") || raw.starts_with("https://") {
        reqwest::Url::parse(raw).ok()
    } else if raw.contains("code=") {
        reqwest::Url::parse(&format!("http://127.0.0.1/?{}", raw.trim_start_matches('?'))).ok()
    } else {
        None
    };
    if let Some(url) = as_url {
        let mut code = None;
        let mut state = None;
        for (k, v) in url.query_pairs() {
            match k.as_ref() {
                "code" => code = Some(v.into_owned()),
                "state" => state = Some(v.into_owned()),
                _ => {}
            }
        }
        if let Some(c) = code {
            let c = c.trim().to_string();
            if !c.is_empty() {
                return (
                    c,
                    state.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
                );
            }
        }
    }
    let mut parts = raw.splitn(2, '#');
    let code = parts.next().unwrap_or("").trim().to_string();
    let state = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (code, state)
}

#[cfg(test)]
mod tests {
    use super::{build_authorize_url, parse_code_and_state};

    #[test]
    fn authorize_url_has_required_params() {
        let url = build_authorize_url(
            "https://oidc.us-east-1.amazonaws.com",
            "client-abc",
            "state-xyz",
            "challenge-123",
        );
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=client-abc"));
        assert!(url.contains("code_challenge=challenge-123"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-xyz"));
    }

    #[test]
    fn parses_full_callback_url() {
        let (code, state) =
            parse_code_and_state("http://127.0.0.1:54546/oauth/callback?code=abc123&state=s1");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("s1"));
    }

    #[test]
    fn parses_bare_code() {
        let (code, state) = parse_code_and_state("  rawcode  ");
        assert_eq!(code, "rawcode");
        assert_eq!(state, None);
    }
}
