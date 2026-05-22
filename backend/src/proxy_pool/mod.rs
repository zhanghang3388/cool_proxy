use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyEntry {
    pub id: String,
    pub url: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyPoolFile {
    #[serde(default)]
    pub proxies: Vec<ProxyEntry>,
    /// 累计分配过的次数，用于决定下一个新账号绑定哪个代理。
    /// 永远递增，不随删除回退，避免删除后再加导致分布偏斜。
    #[serde(default)]
    pub assign_counter: u64,
}

pub struct ProxyPool {
    path: PathBuf,
    inner: RwLock<ProxyPoolFile>,
}

impl ProxyPool {
    pub fn load(path: PathBuf) -> Result<Self> {
        let inner = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read proxy pool {:?}", path))?;
            serde_json::from_str(&raw).with_context(|| "parse proxy pool")?
        } else {
            ProxyPoolFile::default()
        };
        Ok(Self {
            path,
            inner: RwLock::new(inner),
        })
    }

    fn save_locked(&self, file: &ProxyPoolFile) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(file)?;
        std::fs::write(&tmp, &data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn list(&self) -> Vec<ProxyEntry> {
        self.inner.read().unwrap().proxies.clone()
    }

    pub fn url_by_id(&self, id: &str) -> Option<String> {
        self.inner
            .read()
            .unwrap()
            .proxies
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.url.clone())
    }

    pub fn id_by_url(&self, url: &str) -> Option<String> {
        let url = url.trim();
        if url.is_empty() {
            return None;
        }
        self.inner
            .read()
            .unwrap()
            .proxies
            .iter()
            .find(|p| p.url == url)
            .map(|p| p.id.clone())
    }

    pub fn add(&self, url: String, label: String) -> Result<ProxyEntry> {
        let url = url.trim().to_string();
        if url.is_empty() {
            anyhow::bail!("url must not be empty");
        }
        // 简单校验：必须看起来像一个 URL
        let _ = reqwest::Url::parse(&url).with_context(|| format!("invalid url: {url}"))?;

        let mut g = self.inner.write().unwrap();
        if g.proxies.iter().any(|p| p.url == url) {
            anyhow::bail!("proxy already exists");
        }
        let entry = ProxyEntry {
            id: format!("px_{}", &Uuid::new_v4().simple().to_string()[..12]),
            url,
            label,
            created_at: Some(Utc::now()),
        };
        g.proxies.push(entry.clone());
        self.save_locked(&g)?;
        Ok(entry)
    }

    pub fn update(&self, id: &str, url: Option<String>, label: Option<String>) -> Result<()> {
        let mut g = self.inner.write().unwrap();
        let Some(p) = g.proxies.iter_mut().find(|p| p.id == id) else {
            anyhow::bail!("proxy not found");
        };
        if let Some(u) = url {
            let u = u.trim().to_string();
            if u.is_empty() {
                anyhow::bail!("url must not be empty");
            }
            let _ = reqwest::Url::parse(&u).with_context(|| format!("invalid url: {u}"))?;
            p.url = u;
        }
        if let Some(l) = label {
            p.label = l;
        }
        self.save_locked(&g)
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        let mut g = self.inner.write().unwrap();
        let len_before = g.proxies.len();
        g.proxies.retain(|p| p.id != id);
        let removed = g.proxies.len() != len_before;
        if removed {
            self.save_locked(&g)?;
        }
        Ok(removed)
    }

    /// round-robin 拿下一个代理 URL，返回 (id, url)。空池返回 None。
    pub fn next_assignment(&self) -> Option<(String, String)> {
        let mut g = self.inner.write().unwrap();
        if g.proxies.is_empty() {
            return None;
        }
        let idx = (g.assign_counter as usize) % g.proxies.len();
        g.assign_counter = g.assign_counter.wrapping_add(1);
        let p = &g.proxies[idx];
        let pair = (p.id.clone(), p.url.clone());
        let _ = self.save_locked(&g);
        Some(pair)
    }
}

/// 默认的代理池文件路径：放在 auth_dir 下，方便和认证文件一起打包/迁移
pub fn default_pool_path(auth_dir: &Path) -> PathBuf {
    auth_dir.join("proxies.json")
}
