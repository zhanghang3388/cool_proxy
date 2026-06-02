//! Claude token 刷新：调用 Anthropic OAuth token 端点换新 access_token，写回 DB。
//! 后台周期扫号池刷即将过期的 token，并复用 [`InFlight`] 做 single-flight 去重，避免
//! 后台扫描与 401 触发的按需刷新并发命中同一账号、用同一个旧 refresh_token 互相刷失败。
//!
//! 失败分类（[`RefreshError`]）决定退避：429 按 Retry-After 阻断该 refresh_token 一段时间，
//! 阻断期内本轮直接跳过，不再反复打 token 端点；4xx 永久失败；5xx / 网络可下轮重试。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use tracing::{debug, info, warn};

use crate::auth::claude::{refresh_access_token, RefreshError};
use crate::auth::{InFlight, InFlightGuard};
use crate::config::Config;
use crate::pool::claude::ClaudePool;
use crate::proxy::ProxiedClients;
use crate::store::claude_accounts::{ClaudeAccountRow, ClaudeTokenUpdate};

pub struct ClaudeRefresher {
    pub clients: Arc<ProxiedClients>,
    in_flight: InFlight,
    /// 按 refresh_token 的阻断表：429 命中后在此记录解除时刻，期内跳过刷新。
    block_until: Mutex<HashMap<String, Instant>>,
}

impl ClaudeRefresher {
    pub fn new(clients: Arc<ProxiedClients>) -> Self {
        Self {
            clients,
            in_flight: InFlight::default(),
            block_until: Mutex::new(HashMap::new()),
        }
    }

    /// 占用某账号的刷新槽位（single-flight 去重）。返回 `None` 表示已有刷新在进行。
    pub fn begin_refresh(&self, id: &str) -> Option<InFlightGuard> {
        self.in_flight.try_acquire(id)
    }

    /// 该 refresh_token 是否仍在 429 阻断期内。
    pub fn is_blocked(&self, refresh_token: &str) -> bool {
        let mut map = self.block_until.lock().unwrap();
        match map.get(refresh_token) {
            Some(until) if *until > Instant::now() => true,
            Some(_) => {
                // 已过期：顺手清掉。
                map.remove(refresh_token);
                false
            }
            None => false,
        }
    }

    fn set_blocked(&self, refresh_token: &str, dur: Duration) {
        self.block_until
            .lock()
            .unwrap()
            .insert(refresh_token.to_string(), Instant::now() + dur);
    }

    fn clear_blocked(&self, refresh_token: &str) {
        self.block_until.lock().unwrap().remove(refresh_token);
    }

    /// 刷新一个账号的 token，返回写回 DB 用的更新集合。失败返回分类错误。
    pub async fn refresh(
        &self,
        acc: &ClaudeAccountRow,
    ) -> std::result::Result<ClaudeTokenUpdate, RefreshError> {
        if acc.refresh_token.trim().is_empty() {
            return Err(RefreshError::Permanent(
                "账号缺少 refresh_token，无法刷新".into(),
            ));
        }
        // 阻断期内直接拒绝，避免反复打 token 端点。
        if self.is_blocked(&acc.refresh_token) {
            return Err(RefreshError::RateLimited {
                retry_after: Duration::from_secs(0),
            });
        }
        let http = self
            .clients
            .get(&acc.proxy_url)
            .map_err(|e| RefreshError::Transient(format!("get http client: {e}")))?;
        match refresh_access_token(&http, &acc.refresh_token).await {
            Ok(data) => {
                // 成功：解除可能存在的阻断。
                self.clear_blocked(&acc.refresh_token);
                Ok(ClaudeTokenUpdate {
                    access_token: data.access_token,
                    refresh_token: data.refresh_token,
                    expires_at: data.expires_at,
                    raw_auth_token: data.raw_auth_token,
                })
            }
            Err(RefreshError::RateLimited { retry_after }) => {
                self.set_blocked(&acc.refresh_token, retry_after);
                Err(RefreshError::RateLimited { retry_after })
            }
            Err(other) => Err(other),
        }
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
            // 429 阻断期内：本轮跳过，等下一 tick（或阻断到期）再试。
            if refresher.is_blocked(&acc.refresh_token) {
                debug!(account = %acc.id, "claude refresh skipped: rate-limit backoff active");
                continue;
            }
            match refresher.refresh(&acc).await {
                Ok(update) => {
                    pool.update_after_refresh(&acc.id, &update);
                    info!(account = %acc.id, email = %acc.email, "claude token refreshed");
                }
                Err(e) => {
                    let msg = e.to_string();
                    match &e {
                        RefreshError::RateLimited { retry_after } => {
                            warn!(account = %acc.id, "claude refresh rate-limited, backing off {}s", retry_after.as_secs());
                        }
                        _ => warn!(account = %acc.id, "claude refresh failed: {msg}"),
                    }
                    pool.mark_refresh_failed(&acc.id, &msg);
                }
            }
        }
    }
}
