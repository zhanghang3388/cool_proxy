//! Claude token 刷新：调用 Anthropic OAuth token 端点换新 access_token，写回 DB。
//! 后台周期扫号池刷即将过期的 token，并复用 [`InFlight`] 做 single-flight 去重，避免
//! 后台扫描与 401 触发的按需刷新并发命中同一账号、用同一个旧 refresh_token 互相刷失败。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use tracing::{debug, info, warn};

use crate::auth::claude::refresh_access_token;
use crate::auth::{InFlight, InFlightGuard};
use crate::config::Config;
use crate::pool::claude::ClaudePool;
use crate::proxy::ProxiedClients;
use crate::store::claude_accounts::{ClaudeAccountRow, ClaudeTokenUpdate};

pub struct ClaudeRefresher {
    pub clients: Arc<ProxiedClients>,
    in_flight: InFlight,
}

impl ClaudeRefresher {
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
    pub async fn refresh(&self, acc: &ClaudeAccountRow) -> Result<ClaudeTokenUpdate> {
        if acc.refresh_token.trim().is_empty() {
            anyhow::bail!("账号缺少 refresh_token，无法刷新");
        }
        let http = self.clients.get(&acc.proxy_url)?;
        let data = refresh_access_token(&http, &acc.refresh_token).await?;
        Ok(ClaudeTokenUpdate {
            access_token: data.access_token,
            refresh_token: data.refresh_token,
            expires_at: data.expires_at,
            raw_auth_token: data.raw_auth_token,
        })
    }
}

/// 后台任务：周期扫 Claude 号池，刷新即将过期的 token。
pub async fn run_claude_refresh_loop(
    cfg: Arc<Config>,
    pool: Arc<ClaudePool>,
    refresher: Arc<ClaudeRefresher>,
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
        debug!("claude refresh scan: {} candidate(s)", candidates.len());
        let now = Utc::now();
        let cutoff =
            now + chrono::Duration::seconds(cfg.token_refresh.refresh_before_expire_seconds);
        for snap in candidates {
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
            if let Some(exp) = acc.expires_at {
                if exp > cutoff {
                    continue;
                }
            }
            match refresher.refresh(&acc).await {
                Ok(update) => {
                    pool.update_after_refresh(&acc.id, &update);
                    info!(account = %acc.id, email = %acc.email, "claude token refreshed");
                }
                Err(e) => {
                    let msg = e.to_string();
                    warn!(account = %acc.id, "claude refresh failed: {msg}");
                    pool.mark_refresh_failed(&acc.id, &msg);
                }
            }
        }
    }
}
