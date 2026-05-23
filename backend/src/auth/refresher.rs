use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::auth::CodexTokenStorage;
use crate::config::Config;
use crate::pool::AccountPool;
use crate::proxy::ProxiedClients;

const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Debug, Deserialize)]
struct RefreshResp {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    expires_in: i64,
}

pub struct Refresher {
    pub clients: Arc<ProxiedClients>,
}

impl Refresher {
    pub fn new(clients: Arc<ProxiedClients>) -> Self {
        Self { clients }
    }

    /// 用 refresh_token 拿新的 access_token；通过 `proxy_url` 走对应的代理。
    pub async fn refresh(
        &self,
        storage: &CodexTokenStorage,
        proxy_url: &str,
    ) -> Result<CodexTokenStorage> {
        let params = [
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", storage.refresh_token.as_str()),
            ("scope", "openid profile email"),
        ];

        let http = self.clients.get(proxy_url)?;
        let resp = http
            .post(TOKEN_URL)
            .header("Accept", "application/json")
            .timeout(Duration::from_secs(60))
            .form(&params)
            .send()
            .await
            .with_context(|| "refresh request")?;

        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("refresh failed: {} {}", status, body);
        }
        let parsed: RefreshResp = serde_json::from_str(&body)
            .with_context(|| format!("parse refresh response: {body}"))?;

        let new_expire = Utc::now() + chrono::Duration::seconds(parsed.expires_in);
        let mut updated = storage.clone();
        updated.access_token = parsed.access_token;
        updated.refresh_token = parsed.refresh_token;
        if !parsed.id_token.is_empty() {
            updated.id_token = parsed.id_token;
        }
        updated.expire = new_expire.to_rfc3339();
        updated.last_refresh = Utc::now().to_rfc3339();
        updated.kind = "codex".to_string();
        Ok(updated)
    }
}

/// 后台任务：周期扫号池，刷新即将过期的 token。直接写 DB（pool.update_after_refresh），不再落文件。
pub async fn run_refresh_loop(
    cfg: Arc<Config>,
    pool: Arc<AccountPool>,
    refresher: Arc<Refresher>,
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
        debug!("refresh scan: {} candidate(s)", candidates.len());
        for (id, storage, _path, proxy_url) in candidates {
            match refresher.refresh(&storage, &proxy_url).await {
                Ok(new_storage) => {
                    pool.update_after_refresh(&id, &new_storage);
                    info!(account = %id, email = %new_storage.email, "token refreshed");
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(account = %id, "refresh failed: {msg}");
                    pool.mark_refresh_failed(&id, &msg);
                }
            }
        }
    }
}
