use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::store::proxies as store_proxies;
use crate::store::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEntry {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl From<store_proxies::ProxyRow> for ProxyEntry {
    fn from(r: store_proxies::ProxyRow) -> Self {
        Self {
            id: r.id,
            url: r.url,
            label: r.label,
            created_at: Utc.timestamp_millis_opt(r.created_at).single(),
        }
    }
}

/// 代理池：DB 主，handler 接口保持原状。
pub struct ProxyPool {
    db: SqlitePool,
}

impl ProxyPool {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    /// 启动时若 DB 为空且老 proxies.json 存在，做一次性导入。
    pub fn import_legacy_if_empty(&self, legacy_path: &Path) -> Result<()> {
        let existing = store_proxies::list(&self.db)?;
        if !existing.is_empty() {
            return Ok(());
        }
        if !legacy_path.exists() {
            return Ok(());
        }
        let raw = std::fs::read_to_string(legacy_path)
            .with_context(|| format!("read legacy proxies {:?}", legacy_path))?;
        #[derive(Deserialize)]
        struct LegacyFile {
            #[serde(default)]
            proxies: Vec<LegacyEntry>,
        }
        #[derive(Deserialize)]
        struct LegacyEntry {
            url: String,
            #[serde(default)]
            label: String,
        }
        let parsed: LegacyFile =
            serde_json::from_str(&raw).with_context(|| "parse legacy proxies.json")?;
        for e in parsed.proxies {
            // 忽略重复，老文件可能本来就乱
            let _ = store_proxies::add(&self.db, e.url, e.label);
        }
        tracing::info!("imported legacy proxies.json");
        Ok(())
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        store_proxies::list(&self.db)
            .unwrap_or_default()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    pub fn url_by_id(&self, id: &str) -> Option<String> {
        store_proxies::url_by_id(&self.db, id).ok().flatten()
    }

    pub fn id_by_url(&self, url: &str) -> Option<String> {
        store_proxies::id_by_url(&self.db, url).ok().flatten()
    }

    pub fn add(&self, url: String, label: String) -> Result<ProxyEntry> {
        let url = validate_proxy_url(&url)?;
        if url.is_empty() {
            anyhow::bail!("url must not be empty");
        }
        let row = store_proxies::add(&self.db, url, label)?;
        Ok(row.into())
    }

    pub fn update(&self, id: &str, url: Option<String>, label: Option<String>) -> Result<()> {
        let url = match url {
            Some(u) => {
                let v = validate_proxy_url(&u)?;
                if v.is_empty() {
                    anyhow::bail!("url must not be empty");
                }
                Some(v)
            }
            None => None,
        };
        store_proxies::update(&self.db, id, url, label)?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        store_proxies::delete(&self.db, id)
    }

    /// round-robin 给新账号分配代理。
    pub fn next_assignment(&self) -> Option<(String, String)> {
        store_proxies::next_assignment(&self.db).ok().flatten()
    }
}

/// 校验代理 URL：空字符串视为"直连"，非空必须能被 reqwest 解析，且 scheme 限定为
/// http / https / socks5 / socks5h。fragment（`#xxx` 备注）会被剥掉。
pub fn validate_proxy_url(url: &str) -> Result<String> {
    let mut trimmed = url.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    // 订阅工具常会在末尾加 `#中文备注`，HTTP fragment 不传输，保险起见剥掉
    if let Some(idx) = trimmed.find('#') {
        trimmed = &trimmed[..idx];
    }
    let trimmed = trimmed.trim_end();
    let parsed = reqwest::Url::parse(trimmed)
        .with_context(|| format!("invalid proxy url: {trimmed}"))?;
    match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => anyhow::bail!(
            "unsupported proxy scheme: {other} (only http/https/socks5/socks5h are allowed)"
        ),
    }
    Ok(trimmed.to_string())
}

/// 保留这个路径，用于一次性把老 proxies.json 导入 DB。导入完不再写。
pub fn legacy_pool_path(auth_dir: &Path) -> PathBuf {
    auth_dir.join("proxies.json")
}
