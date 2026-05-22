use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::RwLock;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tracing::{info, warn};

use crate::auth::codex::{scan_codex_files, CodexTokenStorage};
use crate::config::Config;

/// 单个账号在号池里的运行时状态。
#[derive(Debug, Clone, Serialize)]
pub struct CodexAccount {
    pub id: String,
    pub file_path: PathBuf,
    pub email: String,
    pub account_id: String,
    pub plan: Option<String>,
    pub enabled: bool,

    pub expire_at: Option<DateTime<Utc>>,
    pub last_refresh_at: Option<DateTime<Utc>>,

    pub failure_count: u32,
    pub cooldown_until: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
    pub last_used_at: Option<DateTime<Utc>>,

    pub total_requests: u64,
    pub total_failures: u64,

    /// 绑定的代理 URL（空字符串表示直连）
    pub proxy_url: String,

    /// 不暴露给前端：token 实际值
    #[serde(skip)]
    pub access_token: String,
    #[serde(skip)]
    pub refresh_token: String,
    #[serde(skip)]
    pub id_token: String,
    #[serde(skip)]
    pub raw_extra: serde_json::Map<String, serde_json::Value>,
}

impl CodexAccount {
    fn from_storage(path: PathBuf, storage: &CodexTokenStorage) -> Self {
        // 用文件名做稳定 ID（去掉扩展名），方便前端定位
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();

        let plan = storage
            .extra
            .get("plan_type")
            .or_else(|| storage.extra.get("plan"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Self {
            id,
            file_path: path,
            email: storage.email.clone(),
            account_id: storage.account_id.clone(),
            plan,
            enabled: true,
            expire_at: storage.expire_at(),
            last_refresh_at: DateTime::parse_from_rfc3339(&storage.last_refresh)
                .ok()
                .map(|dt| dt.with_timezone(&Utc)),
            failure_count: 0,
            cooldown_until: None,
            last_error: None,
            last_used_at: None,
            total_requests: 0,
            total_failures: 0,
            proxy_url: storage.proxy_url.clone(),
            access_token: storage.access_token.clone(),
            refresh_token: storage.refresh_token.clone(),
            id_token: storage.id_token.clone(),
            raw_extra: storage.extra.clone(),
        }
    }

    fn to_storage(&self) -> CodexTokenStorage {
        CodexTokenStorage {
            id_token: self.id_token.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            account_id: self.account_id.clone(),
            last_refresh: self
                .last_refresh_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            email: self.email.clone(),
            kind: "codex".to_string(),
            expire: self
                .expire_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            proxy_url: self.proxy_url.clone(),
            extra: self.raw_extra.clone(),
        }
    }

    pub fn is_available(&self, now: DateTime<Utc>) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(c) = self.cooldown_until {
            if c > now {
                return false;
            }
        }
        if self.access_token.is_empty() {
            return false;
        }
        true
    }
}

/// 选号失败的原因，便于反代层做不同响应
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("no accounts available")]
    Empty,
    #[error("all accounts cooling down or disabled")]
    AllUnavailable,
}

/// 一次成功被选中的号 + 用于失败回报的句柄
#[derive(Debug, Clone)]
pub struct SelectedAccount {
    pub id: String,
    pub access_token: String,
    pub account_id: String,
    pub proxy_url: String,
}

pub struct AccountPool {
    accounts: RwLock<Vec<CodexAccount>>,
    cursor: AtomicUsize,
    cfg: std::sync::Arc<Config>,
}

impl AccountPool {
    pub fn new(cfg: std::sync::Arc<Config>) -> Self {
        Self {
            accounts: RwLock::new(Vec::new()),
            cursor: AtomicUsize::new(0),
            cfg,
        }
    }

    pub fn load_from_disk(&self) -> anyhow::Result<usize> {
        let files = scan_codex_files(&self.cfg.auth_dir)?;
        let mut existing: HashMap<String, CodexAccount> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .map(|a| (a.id.clone(), a.clone()))
            .collect();

        let mut new_list = Vec::with_capacity(files.len());
        for path in files {
            match CodexTokenStorage::load(&path) {
                Ok(storage) => {
                    let mut acc = CodexAccount::from_storage(path.clone(), &storage);
                    // 保留运行时状态（启用/禁用、计数、冷却）
                    if let Some(prev) = existing.remove(&acc.id) {
                        acc.enabled = prev.enabled;
                        acc.failure_count = prev.failure_count;
                        acc.cooldown_until = prev.cooldown_until;
                        acc.last_error = prev.last_error;
                        acc.last_used_at = prev.last_used_at;
                        acc.total_requests = prev.total_requests;
                        acc.total_failures = prev.total_failures;
                    }
                    new_list.push(acc);
                }
                Err(e) => warn!("skip invalid token file {:?}: {e:?}", path),
            }
        }
        let count = new_list.len();
        *self.accounts.write().unwrap() = new_list;
        info!("loaded {} codex account(s) from {:?}", count, self.cfg.auth_dir);
        Ok(count)
    }

    pub fn list(&self) -> Vec<CodexAccount> {
        self.accounts.read().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.accounts.read().unwrap().len()
    }

    pub fn get(&self, id: &str) -> Option<CodexAccount> {
        self.accounts
            .read()
            .unwrap()
            .iter()
            .find(|a| a.id == id)
            .cloned()
    }

    /// 轮询挑一个可用账号
    pub fn pick(&self) -> Result<SelectedAccount, PoolError> {
        let now = Utc::now();
        let mut accounts = self.accounts.write().unwrap();
        if accounts.is_empty() {
            return Err(PoolError::Empty);
        }
        let n = accounts.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            let idx = (start + i) % n;
            if accounts[idx].is_available(now) {
                accounts[idx].last_used_at = Some(now);
                accounts[idx].total_requests += 1;
                return Ok(SelectedAccount {
                    id: accounts[idx].id.clone(),
                    access_token: accounts[idx].access_token.clone(),
                    account_id: accounts[idx].account_id.clone(),
                    proxy_url: accounts[idx].proxy_url.clone(),
                });
            }
        }
        Err(PoolError::AllUnavailable)
    }

    pub fn report_success(&self, id: &str) {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
            a.failure_count = 0;
            a.cooldown_until = None;
            a.last_error = None;
        }
    }

    pub fn report_failure(&self, id: &str, status: u16, msg: &str) {
        let mut accounts = self.accounts.write().unwrap();
        let Some(a) = accounts.iter_mut().find(|a| a.id == id) else {
            return;
        };
        a.failure_count = a.failure_count.saturating_add(1);
        a.total_failures = a.total_failures.saturating_add(1);
        a.last_error = Some(format!("HTTP {status}: {msg}"));
        let now = Utc::now();
        let cooldown = if a.failure_count >= self.cfg.retry.failure_threshold {
            self.cfg.retry.long_cooldown_seconds
        } else {
            self.cfg.retry.cooldown_seconds
        };
        a.cooldown_until = Some(now + chrono::Duration::seconds(cooldown as i64));
        warn!(
            account = %id,
            "marked cooldown for {}s (count={})",
            cooldown,
            a.failure_count
        );
    }

    pub fn set_enabled(&self, id: &str, enabled: bool) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
            a.enabled = enabled;
            if enabled {
                a.failure_count = 0;
                a.cooldown_until = None;
            }
            true
        } else {
            false
        }
    }

    pub fn remove(&self, id: &str) -> Option<PathBuf> {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(pos) = accounts.iter().position(|a| a.id == id) {
            let removed = accounts.remove(pos);
            return Some(removed.file_path);
        }
        None
    }

    pub fn add_or_replace_from_storage(
        &self,
        path: PathBuf,
        storage: &CodexTokenStorage,
    ) -> CodexAccount {
        let new_acc = CodexAccount::from_storage(path, storage);
        let mut accounts = self.accounts.write().unwrap();
        if let Some(existing) = accounts.iter_mut().find(|a| a.id == new_acc.id) {
            existing.access_token = new_acc.access_token.clone();
            existing.refresh_token = new_acc.refresh_token.clone();
            existing.id_token = new_acc.id_token.clone();
            existing.email = new_acc.email.clone();
            existing.account_id = new_acc.account_id.clone();
            existing.expire_at = new_acc.expire_at;
            existing.last_refresh_at = new_acc.last_refresh_at;
            existing.raw_extra = new_acc.raw_extra.clone();
            // 文件里如果带了 proxy_url 就更新；否则保留运行时已绑定的代理
            if !new_acc.proxy_url.is_empty() {
                existing.proxy_url = new_acc.proxy_url.clone();
            }
            existing.failure_count = 0;
            existing.cooldown_until = None;
            existing.last_error = None;
            existing.clone()
        } else {
            accounts.push(new_acc.clone());
            new_acc
        }
    }

    /// 修改某账号绑定的代理 URL，并把更新后的 storage 同步写回磁盘。
    /// 传空字符串表示清除代理（直连）。
    pub fn set_proxy(&self, id: &str, proxy_url: String) -> anyhow::Result<()> {
        let (path, storage) = {
            let mut accounts = self.accounts.write().unwrap();
            let Some(a) = accounts.iter_mut().find(|a| a.id == id) else {
                anyhow::bail!("account not found");
            };
            a.proxy_url = proxy_url.trim().to_string();
            (a.file_path.clone(), a.to_storage())
        };
        storage.save(&path)?;
        Ok(())
    }

    /// 列出所有当前没有绑定代理的账号 id。
    pub fn unassigned_ids(&self) -> Vec<String> {
        self.accounts
            .read()
            .unwrap()
            .iter()
            .filter(|a| a.proxy_url.is_empty())
            .map(|a| a.id.clone())
            .collect()
    }

    /// 列出所有账号 id（按 id 排序，便于全局重新分配的稳定性）。
    pub fn all_ids_sorted(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .accounts
            .read()
            .unwrap()
            .iter()
            .map(|a| a.id.clone())
            .collect();
        v.sort();
        v
    }

    /// 给 token refresher 用：返回需要刷新的快照（id, storage, file_path, proxy_url）
    pub fn snapshot_for_refresh(
        &self,
        threshold_seconds: i64,
    ) -> Vec<(String, CodexTokenStorage, PathBuf, String)> {
        let accounts = self.accounts.read().unwrap();
        accounts
            .iter()
            .filter(|a| {
                a.enabled
                    && !a.refresh_token.is_empty()
                    && match a.expire_at {
                        Some(t) => (t - Utc::now()).num_seconds() <= threshold_seconds,
                        None => true,
                    }
            })
            .map(|a| {
                (
                    a.id.clone(),
                    a.to_storage(),
                    a.file_path.clone(),
                    a.proxy_url.clone(),
                )
            })
            .collect()
    }

    pub fn update_after_refresh(&self, id: &str, storage: &CodexTokenStorage) {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
            a.access_token = storage.access_token.clone();
            a.refresh_token = storage.refresh_token.clone();
            if !storage.id_token.is_empty() {
                a.id_token = storage.id_token.clone();
            }
            a.expire_at = storage.expire_at();
            a.last_refresh_at = Some(Utc::now());
            a.raw_extra = storage.extra.clone();
        }
    }

    pub fn mark_refresh_failed(&self, id: &str, msg: &str) {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
            a.last_error = Some(format!("refresh failed: {msg}"));
        }
    }

    pub fn reset_cooldown(&self, id: &str) -> bool {
        let mut accounts = self.accounts.write().unwrap();
        if let Some(a) = accounts.iter_mut().find(|a| a.id == id) {
            a.failure_count = 0;
            a.cooldown_until = None;
            a.last_error = None;
            true
        } else {
            false
        }
    }
}

/// 帮助函数：根据 storage 推导一个稳定的文件名
pub fn derive_file_name(storage: &CodexTokenStorage) -> String {
    let email = storage.email.trim();
    let plan = storage
        .extra
        .get("plan_type")
        .or_else(|| storage.extra.get("plan"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_lowercase();

    let safe_email = if email.is_empty() {
        let mut h = sha2::Sha256::default();
        use sha2::Digest;
        h.update(storage.account_id.as_bytes());
        h.update(storage.access_token.as_bytes());
        let digest = h.finalize();
        format!("acc-{}", &hex::encode(digest)[..12])
    } else {
        email.replace('/', "_")
    };

    if plan.is_empty() {
        format!("codex-{safe_email}.json")
    } else {
        format!("codex-{safe_email}-{plan}.json")
    }
}

#[allow(dead_code)]
pub fn _path_helper(base: &Path, name: &str) -> PathBuf {
    base.join(name)
}
