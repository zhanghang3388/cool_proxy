//! Kiro token 刷新：两条流。
//!  - social（Google/GitHub）：POST https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken
//!  - IdC / Builder-ID：POST https://oidc.{region}.amazonaws.com/token（form 编码）
//!
//! 优先级按账号 auth_method 决定；IdC 失败可回退 social，反之亦然。
//! 所有请求走账号绑定的代理。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::auth::kiro::{normalize_non_empty, parse_timestamp, pick_number, pick_string};
use crate::config::Config;
use crate::pool::kiro::KiroPool;
use crate::proxy::ProxiedClients;
use crate::store::kiro_accounts::{KiroAccountRow, KiroTokenUpdate};

const KIRO_REFRESH_ENDPOINT: &str = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";
const KIRO_AWS_OIDC_TOKEN_ENDPOINT_FMT: &str = "https://oidc.{region}.amazonaws.com/token";

pub struct KiroRefresher {
    pub clients: Arc<ProxiedClients>,
}

impl KiroRefresher {
    pub fn new(clients: Arc<ProxiedClients>) -> Self {
        Self { clients }
    }

    /// 刷新一个账号的 token，返回写回 DB 用的更新集合。
    pub async fn refresh(&self, acc: &KiroAccountRow) -> Result<KiroTokenUpdate> {
        let refresh_token = normalize_non_empty(Some(acc.refresh_token.as_str()))
            .ok_or_else(|| anyhow::anyhow!("账号缺少 refresh_token，无法刷新"))?;

        let prefer_idc = acc.auth_method.eq_ignore_ascii_case("idc")
            || (acc.idc_region.is_some()
                && acc.client_id.is_some()
                && acc.client_secret.is_some());

        let mut errors: Vec<String> = Vec::new();
        let mut token: Option<Value> = None;

        if prefer_idc {
            match self.refresh_via_idc(&refresh_token, acc).await {
                Ok(t) => token = Some(t),
                Err(e) => {
                    warn!(account = %acc.id, "IdC OIDC 刷新失败: {e}");
                    errors.push(format!("IdC OIDC 刷新失败: {e}"));
                }
            }
        }

        if token.is_none() {
            match self.refresh_via_remote(&refresh_token, &acc.proxy_url).await {
                Ok(t) => token = Some(t),
                Err(e) => {
                    warn!(account = %acc.id, "refreshToken 接口刷新失败: {e}");
                    errors.push(format!("refreshToken 接口刷新失败: {e}"));
                }
            }
        }

        // social 优先但失败时，最后再试一次 IdC（凭据齐全的话）
        if token.is_none() && !prefer_idc && acc.client_secret.is_some() {
            if let Ok(t) = self.refresh_via_idc(&refresh_token, acc).await {
                token = Some(t);
            }
        }

        let Some(token) = token else {
            anyhow::bail!("刷新 Kiro 登录态失败: {}", errors.join("；"));
        };

        Ok(build_token_update(token, &refresh_token, acc))
    }

    /// social：POST /refreshToken，body `{ "refreshToken": "..." }`。
    async fn refresh_via_remote(&self, refresh_token: &str, proxy_url: &str) -> Result<Value> {
        let http = self.clients.get(proxy_url)?;
        let resp = http
            .post(KIRO_REFRESH_ENDPOINT)
            .timeout(Duration::from_secs(60))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&json!({ "refreshToken": refresh_token }))
            .send()
            .await
            .with_context(|| "kiro refreshToken request")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "status={} body={}",
                status,
                body.chars().take(512).collect::<String>()
            );
        }
        let parsed: Value =
            serde_json::from_str(&body).with_context(|| format!("parse refresh response: {body}"))?;
        Ok(unwrap_token_response(parsed))
    }

    /// IdC：POST oidc.{region}.amazonaws.com/token，form 编码。
    async fn refresh_via_idc(&self, refresh_token: &str, acc: &KiroAccountRow) -> Result<Value> {
        let region = acc
            .idc_region
            .clone()
            .or_else(|| {
                acc.profile_arn
                    .as_deref()
                    .and_then(crate::auth::kiro::parse_profile_arn_region)
            })
            .ok_or_else(|| anyhow::anyhow!("缺少 idc_region"))?;
        let client_id = acc
            .client_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("缺少 client_id"))?;
        let client_secret = acc
            .client_secret
            .clone()
            .ok_or_else(|| anyhow::anyhow!("缺少 client_secret"))?;

        let endpoint = KIRO_AWS_OIDC_TOKEN_ENDPOINT_FMT.replace("{region}", region.as_str());
        let http = self.clients.get(&acc.proxy_url)?;
        let resp = http
            .post(&endpoint)
            .timeout(Duration::from_secs(60))
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
            ])
            .send()
            .await
            .with_context(|| "kiro idc oidc request")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "status={} body={}",
                status,
                body.chars().take(512).collect::<String>()
            );
        }
        let mut token = unwrap_token_response(
            serde_json::from_str::<Value>(&body)
                .with_context(|| format!("parse idc response: {body}"))?,
        );
        // 保留 IdC 元数据，便于下次刷新
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
                .or_insert_with(|| Value::String("IdC".to_string()));
        }
        Ok(token)
    }
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
            &["idToken"],
            &["id_token"],
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
        for acc in candidates {
            match refresher.refresh(&acc).await {
                Ok(update) => {
                    pool.update_after_refresh(&acc.id, &update);
                    info!(account = %acc.id, email = %acc.email, "kiro token refreshed");
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(account = %acc.id, "kiro refresh failed: {msg}");
                    pool.mark_refresh_failed(&acc.id, &msg);
                }
            }
        }
    }
}
