//! Claude（Anthropic）OAuth2 + PKCE 登录 / 刷新核心。移植自 CLIProxyAPI
//! `internal/auth/claude/anthropic_auth.go`，保持端点 / client_id / scope 一致，
//! 这样换出来的 `sk-ant-oat...` 令牌与 Claude Code 官方登录态等价。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// OAuth 端点与客户端常量（与 Claude Code 官方一致）。
pub const AUTH_URL: &str = "https://claude.ai/oauth/authorize";
pub const TOKEN_URL: &str = "https://api.anthropic.com/v1/oauth/token";
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub const REDIRECT_URI: &str = "http://localhost:54545/callback";
pub const SCOPE: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 待完成登录的有效期：超过则要求重新生成授权链接。
const LOGIN_TTL: Duration = Duration::from_secs(15 * 60);

/// 解析后的 Claude OAuth 令牌数据。
#[derive(Debug, Clone)]
pub struct ClaudeTokenData {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub email: String,
    pub org_name: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    /// 归一化后的原始令牌快照，落库备查。
    pub raw_auth_token: Value,
}

/// PKCE 校验码对。
#[derive(Debug, Clone)]
pub struct PkceCodes {
    pub verifier: String,
    pub challenge: String,
}

/// 生成 PKCE：verifier 取 32 字节随机（两个 v4 UUID 拼接）base64url，challenge 为其 SHA256。
pub fn gen_pkce() -> PkceCodes {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    PkceCodes {
        verifier,
        challenge,
    }
}

/// 构造 claude.ai 授权链接（带 PKCE challenge + state）。
pub fn build_authorize_url(state: &str, pkce: &PkceCodes) -> Result<String> {
    let url = reqwest::Url::parse_with_params(
        AUTH_URL,
        &[
            ("code", "true"),
            ("client_id", CLIENT_ID),
            ("response_type", "code"),
            ("redirect_uri", REDIRECT_URI),
            ("scope", SCOPE),
            ("code_challenge", pkce.challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("state", state),
        ],
    )
    .with_context(|| "build claude authorize url")?;
    Ok(url.to_string())
}

/// 根据账号推导稳定 id（DB 主键 + 列表展示用）。
pub fn derive_claude_account_id(data: &ClaudeTokenData) -> String {
    let email = data.email.trim();
    if !email.is_empty() {
        return format!("claude-{}", email.replace('/', "_"));
    }
    let mut h = Sha256::default();
    h.update(data.access_token.as_bytes());
    if let Some(rt) = data.refresh_token.as_deref() {
        h.update(rt.as_bytes());
    }
    let digest = h.finalize();
    format!("claude-acc-{}", &hex::encode(digest)[..12])
}

/// 从用户回填的内容里抽出授权码与 state。兼容四种粘贴形式：
///  1. 整段回调 URL：`http://localhost:54545/callback?code=XXX&state=YYY`
///  2. 查询串：`?code=XXX&state=YYY` 或 `code=XXX&state=YYY`
///  3. 手动 console 流：`code#state`
///  4. 纯 code
fn split_code_and_state(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();

    // 形式 1/2：能解析出 code 查询参数就优先用它（顺带拿 state）。
    let as_url = if raw.starts_with("http://") || raw.starts_with("https://") {
        reqwest::Url::parse(raw).ok()
    } else if raw.contains("code=") {
        reqwest::Url::parse(&format!(
            "http://localhost/?{}",
            raw.trim_start_matches('?')
        ))
        .ok()
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
                    state
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
        }
    }

    // 形式 3/4：code#state 或纯 code。
    let mut parts = raw.splitn(2, '#');
    let code = parts.next().unwrap_or("").trim().to_string();
    let state = parts
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    (code, state)
}

#[cfg(test)]
mod code_parse_tests {
    use super::split_code_and_state;

    #[test]
    fn parses_full_callback_url() {
        let (code, state) =
            split_code_and_state("http://localhost:54545/callback?code=mdo3CExM&state=f992a25e");
        assert_eq!(code, "mdo3CExM");
        assert_eq!(state.as_deref(), Some("f992a25e"));
    }

    #[test]
    fn parses_bare_query_string() {
        let (code, state) = split_code_and_state("code=abc123&state=xyz");
        assert_eq!(code, "abc123");
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parses_hash_form() {
        let (code, state) = split_code_and_state("abc#xyz");
        assert_eq!(code, "abc");
        assert_eq!(state.as_deref(), Some("xyz"));
    }

    #[test]
    fn parses_bare_code() {
        let (code, state) = split_code_and_state("just-a-code");
        assert_eq!(code, "just-a-code");
        assert_eq!(state, None);
    }
}

/// 把 token 接口返回体规整成 [`ClaudeTokenData`]。
fn parse_token_response(body: &Value) -> ClaudeTokenData {
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let refresh_token = body
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let email = body
        .get("account")
        .and_then(|a| a.get("email_address"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let org_name = body
        .get("organization")
        .and_then(|o| o.get("name"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let expires_at = body
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs));

    let raw_auth_token = json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "email": email,
        "org_name": org_name,
        "expires_at": expires_at.map(|t| t.to_rfc3339()),
    });

    ClaudeTokenData {
        access_token,
        refresh_token,
        email,
        org_name,
        expires_at,
        raw_auth_token,
    }
}

/// 用授权码换 token（PKCE）。`http` 应是带/不带代理的 reqwest 客户端。
pub async fn exchange_code(
    http: &reqwest::Client,
    code: &str,
    fallback_state: &str,
    verifier: &str,
) -> Result<ClaudeTokenData> {
    let (parsed_code, state_in_code) = split_code_and_state(code);
    if parsed_code.is_empty() {
        anyhow::bail!("授权码为空");
    }
    let state = state_in_code.unwrap_or_else(|| fallback_state.to_string());
    let req_body = json!({
        "code": parsed_code,
        "state": state,
        "grant_type": "authorization_code",
        "client_id": CLIENT_ID,
        "redirect_uri": REDIRECT_URI,
        "code_verifier": verifier,
    });

    let resp = http
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(60))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&req_body)
        .send()
        .await
        .with_context(|| "claude token exchange request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "token exchange failed: {} {}",
            status,
            body.chars().take(512).collect::<String>()
        );
    }
    let parsed: Value =
        serde_json::from_str(&body).with_context(|| format!("parse token response: {body}"))?;
    let data = parse_token_response(&parsed);
    if data.access_token.is_empty() {
        anyhow::bail!("token response missing access_token");
    }
    Ok(data)
}

/// 用 refresh_token 换新 access_token。
pub async fn refresh_access_token(
    http: &reqwest::Client,
    refresh_token: &str,
) -> Result<ClaudeTokenData> {
    if refresh_token.trim().is_empty() {
        anyhow::bail!("refresh token is required");
    }
    let req_body = json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    let resp = http
        .post(TOKEN_URL)
        .timeout(Duration::from_secs(60))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&req_body)
        .send()
        .await
        .with_context(|| "claude token refresh request")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "token refresh failed: {} {}",
            status,
            body.chars().take(512).collect::<String>()
        );
    }
    let parsed: Value =
        serde_json::from_str(&body).with_context(|| format!("parse refresh response: {body}"))?;
    let mut data = parse_token_response(&parsed);
    // 刷新返回可能不带 refresh_token：沿用旧的。
    if data.refresh_token.is_none() {
        data.refresh_token = Some(refresh_token.to_string());
    }
    if data.access_token.is_empty() {
        anyhow::bail!("refresh response missing access_token");
    }
    Ok(data)
}

// ===== 待完成登录的内存暂存 =====

struct PendingLogin {
    verifier: String,
    created: Instant,
}

/// 暂存 `state -> code_verifier`，供两步登录（拿链接 → 回填授权码）使用。
#[derive(Default)]
pub struct ClaudeLoginStore {
    pending: Mutex<HashMap<String, PendingLogin>>,
}

impl ClaudeLoginStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开始一次登录：生成 PKCE + state，存下 verifier，返回授权链接与 state。
    pub fn start(&self) -> Result<(String, String)> {
        let pkce = gen_pkce();
        let state = Uuid::new_v4().simple().to_string();
        let auth_url = build_authorize_url(&state, &pkce)?;
        let mut map = self.pending.lock().unwrap();
        prune(&mut map);
        map.insert(
            state.clone(),
            PendingLogin {
                verifier: pkce.verifier,
                created: Instant::now(),
            },
        );
        Ok((auth_url, state))
    }

    /// 读取（不消费）某 state 对应的 verifier。换 token 失败时保留，便于改正授权码后重试。
    pub fn peek(&self, state: &str) -> Option<String> {
        let mut map = self.pending.lock().unwrap();
        prune(&mut map);
        map.get(state).map(|p| p.verifier.clone())
    }

    /// 登录成功后消费掉该 state。
    pub fn remove(&self, state: &str) {
        let mut map = self.pending.lock().unwrap();
        map.remove(state);
    }
}

fn prune(map: &mut HashMap<String, PendingLogin>) {
    map.retain(|_, p| p.created.elapsed() < LOGIN_TTL);
}
