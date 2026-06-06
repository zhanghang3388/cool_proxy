//! Kiro token 刷新：严格按 provider 分派，与 KAM 的 `refresh_token_by_provider` 完全一致。
//!
//!  - **Google / Github（social）** → POST `https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken`，
//!    body `{ "refreshToken": "..." }`。machineId 写进 user-agent。
//!  - **BuilderId / Enterprise（IdC）** → POST `https://oidc.{region}.amazonaws.com/token`，
//!    JSON body `{ clientId, clientSecret, grantType: "refresh_token", refreshToken }`。
//!
//! 与之前实现的关键区别：
//!  - 不再有"试 IdC 失败回退 social"的猜测路径——provider 决定刷新流程，错就直接报。
//!  - IdC 路径不再传旧 refresh_token 占位 region/client_id 等元数据（以后参考字段已落库）。
//!  - 401 + AWS 明确拒绝 → 走 `mark_token_invalid`：把账号标记为 invalid 并禁用，避免反复尝试。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::auth::kiro::{
    auth_method_for_provider, normalize_non_empty, parse_timestamp, pick_number, pick_string,
    KIRO_AUTH_METHOD_IDC, KIRO_PROVIDER_BUILDER_ID,
};
use crate::auth::{InFlight, InFlightGuard};
use crate::config::Config;
use crate::pool::kiro::KiroPool;
use crate::proxy::ProxiedClients;
use crate::store::kiro_accounts::{KiroAccountRow, KiroTokenUpdate};

/// Kiro Desktop（social 登录）刷新 endpoint。
const KIRO_DESKTOP_AUTH_API: &str = "https://prod.us-east-1.auth.desktop.kiro.dev";
/// IdC OAuth/OIDC 刷新 endpoint 模板。
const KIRO_AWS_OIDC_TOKEN_ENDPOINT_FMT: &str = "https://oidc.{region}.amazonaws.com/token";
/// KAM 默认 KiroIDE 版本号（拼进 desktop API user-agent）。
const KIRO_IDE_VERSION: &str = "0.6.18";

/// 鉴权类错误前缀（与 KAM 一致）。命中这个前缀视为"AWS 明确拒绝刷新"，应标 invalid 而不是冷却。
const AUTH_ERROR_PREFIX: &str = "AUTH_ERROR:";

pub struct KiroRefresher {
    pub clients: Arc<ProxiedClients>,
    in_flight: InFlight,
}

impl KiroRefresher {
    pub fn new(clients: Arc<ProxiedClients>) -> Self {
        Self {
            clients,
            in_flight: InFlight::default(),
        }
    }

    /// 占用某账号的刷新槽位（single-flight 去重）。返回 `None` 表示已有刷新在进行。
    pub fn begin_refresh(&self, id: &str) -> Option<InFlightGuard> {
        self.in_flight.try_acquire(id)
    }

    /// 刷新一个账号的 token，返回写回 DB 用的更新集合。
    pub async fn refresh(&self, acc: &KiroAccountRow) -> Result<KiroTokenUpdate> {
        let refresh_token = normalize_non_empty(Some(acc.refresh_token.as_str()))
            .ok_or_else(|| anyhow::anyhow!("账号缺少 refresh_token，无法刷新"))?;

        // 解析 provider —— 缺失/未知时按 auth_method 兜底（旧账号兼容）。
        let provider = if acc.provider.is_empty() {
            if acc.auth_method.eq_ignore_ascii_case(KIRO_AUTH_METHOD_IDC) {
                KIRO_PROVIDER_BUILDER_ID
            } else {
                "Google"
            }
            .to_string()
        } else {
            acc.provider.clone()
        };

        let auth_method = auth_method_for_provider(&provider);

        let token = if auth_method == KIRO_AUTH_METHOD_IDC {
            self.refresh_via_idc(&refresh_token, &provider, acc).await?
        } else {
            self.refresh_via_desktop(&refresh_token, acc).await?
        };

        Ok(build_token_update(token, &refresh_token, acc))
    }

    /// social：POST /refreshToken，body `{ "refreshToken": "..." }`。
    async fn refresh_via_desktop(
        &self,
        refresh_token: &str,
        acc: &KiroAccountRow,
    ) -> Result<Value> {
        let http = self.clients.get(&acc.proxy_url)?;
        let machine_id = acc
            .machine_id
            .as_deref()
            .unwrap_or("")
            .trim();
        let user_agent = if machine_id.is_empty() {
            format!("KiroIDE-{KIRO_IDE_VERSION}")
        } else {
            format!("KiroIDE-{KIRO_IDE_VERSION}-{machine_id}")
        };

        let mut last_err: Option<String> = None;
        for attempt in 0u32..3 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            let resp = http
                .post(format!("{KIRO_DESKTOP_AUTH_API}/refreshToken"))
                .timeout(Duration::from_secs(60))
                .header("Content-Type", "application/json")
                .header("Accept", "application/json")
                .header("user-agent", user_agent.clone())
                .json(&json!({ "refreshToken": refresh_token }))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last_err = Some(format!("network: {e}"));
                    continue;
                }
            };
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.is_success() {
                let parsed: Value = serde_json::from_str(&body)
                    .with_context(|| format!("parse refresh response: {body}"))?;
                return Ok(unwrap_token_response(parsed));
            }
            // 401 → AWS 明确拒绝，立即标记为 AUTH_ERROR，不继续重试。
            if status.as_u16() == 401 {
                anyhow::bail!(
                    "{AUTH_ERROR_PREFIX} desktop refresh 401: {}",
                    body.chars().take(512).collect::<String>()
                );
            }
            last_err = Some(format!(
                "status={} body={}",
                status,
                body.chars().take(512).collect::<String>()
            ));
        }
        anyhow::bail!(
            "desktop refresh failed: {}",
            last_err.unwrap_or_else(|| "unknown error".to_string())
        )
    }

    /// IdC：POST oidc.{region}.amazonaws.com/token，restJson1 协议（JSON + camelCase）。
    async fn refresh_via_idc(
        &self,
        refresh_token: &str,
        provider: &str,
        acc: &KiroAccountRow,
    ) -> Result<Value> {
        let region = acc
            .idc_region
            .clone()
            .or_else(|| {
                acc.profile_arn
                    .as_deref()
                    .and_then(crate::auth::kiro::parse_profile_arn_region)
            })
            .unwrap_or_else(|| "us-east-1".to_string());
        let client_id = acc
            .client_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("IdC 账号缺少 client_id"))?;
        let client_secret = acc
            .client_secret
            .clone()
            .ok_or_else(|| anyhow::anyhow!("IdC 账号缺少 client_secret"))?;

        let endpoint = KIRO_AWS_OIDC_TOKEN_ENDPOINT_FMT.replace("{region}", region.as_str());
        let http = self.clients.get(&acc.proxy_url)?;
        let body = json!({
            "clientId": client_id,
            "clientSecret": client_secret,
            "refreshToken": refresh_token,
            "grantType": "refresh_token",
        });
        let resp = http
            .post(&endpoint)
            .timeout(Duration::from_secs(60))
            .header("Content-Type", "application/json")
            .header("x-amz-user-agent", "aws-sdk-js/3.980.0 KiroIDE")
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=4")
            .json(&body)
            .send()
            .await
            .with_context(|| "kiro idc oidc request")?;
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            // AWS 对 invalid_grant / 失效 refresh_token 返回 400 + invalid。
            if status.as_u16() == 400
                && body_text.to_ascii_lowercase().contains("invalid")
                && body_text.to_ascii_lowercase().contains("refresh")
            {
                anyhow::bail!("{AUTH_ERROR_PREFIX} RefreshToken 已失效");
            }
            if status.as_u16() == 401 {
                anyhow::bail!(
                    "{AUTH_ERROR_PREFIX} idc refresh 401: {}",
                    body_text.chars().take(512).collect::<String>()
                );
            }
            anyhow::bail!(
                "idc refresh status={} body={}",
                status,
                body_text.chars().take(512).collect::<String>()
            );
        }
        let mut token = unwrap_token_response(
            serde_json::from_str::<Value>(&body_text)
                .with_context(|| format!("parse idc response: {body_text}"))?,
        );
        // 保留 IdC 元数据，便于下次刷新；若上游没回 refreshToken 就沿用旧的。
        if let Some(obj) = token.as_object_mut() {
            obj.entry("refreshToken")
                .or_insert_with(|| Value::String(refresh_token.to_string()));
            obj.entry("idc_region")
                .or_insert_with(|| Value::String(region.clone()));
            obj.entry("client_id")
                .or_insert_with(|| Value::String(client_id.clone()));
            obj.entry("client_secret")
                .or_insert_with(|| Value::String(client_secret.clone()));
            obj.entry("authMethod")
                .or_insert_with(|| Value::String(KIRO_AUTH_METHOD_IDC.to_string()));
            obj.entry("provider")
                .or_insert_with(|| Value::String(provider.to_string()));
        }
        debug!(account = %acc.id, provider, region, "kiro idc token refreshed");
        Ok(token)
    }
}

/// 判断错误是否为 AUTH_ERROR（AWS 明确拒绝刷新，需标 invalid + 禁用）。
pub fn is_auth_error_message(msg: &str) -> bool {
    msg.contains(AUTH_ERROR_PREFIX) || msg.contains("invalid_grant")
}

/// 刷新接口可能把 token 包在 `data` 里，统一拆出来。
fn unwrap_token_response(value: Value) -> Value {
    if let Some(data) = value.get("data") {
        if data.is_object() {
            return data.clone();
        }
    }
    value
}

/// 把刷新返回体规整成写回 DB 的更新集合，合并旧 raw_auth_token 里缺的键。
fn build_token_update(token: Value, old_refresh: &str, acc: &KiroAccountRow) -> KiroTokenUpdate {
    let access_token = pick_string(
        Some(&token),
        &[
            &["accessToken"],
            &["access_token"],
            &["token"],
            &["accessTokenJwt"],
        ],
    )
    .unwrap_or_default();

    let refresh_token = pick_string(
        Some(&token),
        &[&["refreshToken"], &["refresh_token"], &["refreshTokenJwt"]],
    )
    .or_else(|| normalize_non_empty(Some(old_refresh)));

    let token_type = pick_string(
        Some(&token),
        &[&["tokenType"], &["token_type"], &["authType"]],
    );

    let expires_at = parse_timestamp(
        token
            .get("expiresAt")
            .or_else(|| token.get("expires_at"))
            .or_else(|| token.get("expiry"))
            .or_else(|| token.get("expiration")),
    )
    .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
    .or_else(|| {
        pick_number(Some(&token), &[&["expiresIn"], &["expires_in"]])
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs.round() as i64))
    });

    let id_token = pick_string(Some(&token), &[&["idToken"], &["id_token"]]);
    let sso_session_id = pick_string(
        Some(&token),
        &[&["aws_sso_app_session_id"], &["ssoSessionId"], &["sso_session_id"]],
    );
    let profile_arn = pick_string(Some(&token), &[&["profileArn"], &["profile_arn"]]);

    // 合并：新 token 字段覆盖旧 raw，其余键保留。
    let mut raw_auth_token = acc.raw_auth_token.clone();
    if !raw_auth_token.is_object() {
        raw_auth_token = json!({});
    }
    if let (Some(target), Some(src)) = (raw_auth_token.as_object_mut(), token.as_object()) {
        for (k, v) in src {
            target.insert(k.clone(), v.clone());
        }
    }

    KiroTokenUpdate {
        access_token: if access_token.is_empty() {
            acc.access_token.clone()
        } else {
            access_token
        },
        refresh_token,
        token_type,
        expires_at,
        raw_auth_token,
        id_token,
        sso_session_id,
        profile_arn,
    }
}

/// 后台任务：周期扫 Kiro 号池，刷新即将过期的 token。
pub async fn run_kiro_refresh_loop(
    cfg: Arc<Config>,
    pool: Arc<KiroPool>,
    refresher: Arc<KiroRefresher>,
) {
    let interval = Duration::from_secs(cfg.token_refresh.scan_interval_seconds.max(10));
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let candidates = pool.snapshot_for_refresh(cfg.token_refresh.refresh_before_expire_seconds);
        if candidates.is_empty() {
            continue;
        }
        debug!("kiro refresh scan: {} candidate(s)", candidates.len());
        let now = Utc::now();
        let cutoff =
            now + chrono::Duration::seconds(cfg.token_refresh.refresh_before_expire_seconds);
        for snap in candidates {
            // single-flight：跳过已有刷新在进行的账号（如 401 触发的按需刷新）。
            let Some(_guard) = refresher.begin_refresh(&snap.id) else {
                continue;
            };
            // 重新取最新账号，避免用 snapshot 里可能已被刷新过的旧 refresh_token。
            let Some(acc) = pool.get(&snap.id) else {
                continue;
            };
            if acc.refresh_token.is_empty() {
                continue;
            }
            // 取到锁后再确认一次是否仍需刷新（可能在等待期间已被按需刷新过）。
            if let Some(exp) = acc.expires_at {
                if exp > cutoff {
                    continue;
                }
            }
            match refresher.refresh(&acc).await {
                Ok(update) => {
                    pool.update_after_refresh(&acc.id, &update);
                    info!(account = %acc.id, email = %acc.email, "kiro token refreshed");
                }
                Err(e) => {
                    let msg = e.to_string();
                    // AUTH_ERROR + token 已过期 → KAM 行为：标 invalid + 禁用。
                    let token_dead = acc
                        .expires_at
                        .map(|t| t <= now)
                        .unwrap_or(true);
                    if is_auth_error_message(&msg) && token_dead {
                        warn!(account = %acc.id, "kiro token invalid (auth-error + expired): {msg}");
                        pool.mark_token_invalid(&acc.id, &msg);
                    } else {
                        warn!(account = %acc.id, "kiro refresh failed: {msg}");
                        pool.mark_refresh_failed(&acc.id, &msg);
                    }
                }
            }
        }
    }
}
