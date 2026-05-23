use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 与 CLIProxyAPI 的 CodexTokenStorage JSON 文件兼容。
/// 用 `serde(default)` 容忍历史文件缺字段；保留 `extra` 让我们写回时不丢失元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokenStorage {
    #[serde(default)]
    pub id_token: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default)]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default = "default_type")]
    #[serde(rename = "type")]
    pub kind: String,
    /// CLIProxyAPI 写出的字段名是 `expired`（值是 RFC3339 时间戳，不是 bool）
    #[serde(default, rename = "expired")]
    pub expire: String,

    /// 绑定到这个账号的代理 URL（cool_proxy 自有字段，CLIProxyAPI 不认）
    #[serde(default)]
    pub proxy_url: String,

    /// 兜底所有未识别字段，写回时一并保留
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn default_type() -> String {
    "codex".to_string()
}

impl CodexTokenStorage {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("read token file {:?}", path))?;
        let storage: CodexTokenStorage =
            serde_json::from_str(&raw).with_context(|| format!("parse token file {:?}", path))?;
        Ok(storage)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create dir {:?}", dir))?;
        }
        let tmp = path.with_extension("json.tmp");
        let data = serde_json::to_vec_pretty(self)?;
        std::fs::write(&tmp, &data).with_context(|| format!("write tmp file {:?}", tmp))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("rename {:?} -> {:?}", tmp, path))?;
        Ok(())
    }

    pub fn expire_at(&self) -> Option<DateTime<Utc>> {
        if self.expire.trim().is_empty() {
            return None;
        }
        DateTime::parse_from_rfc3339(&self.expire)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }

    pub fn is_expired(&self) -> bool {
        match self.expire_at() {
            Some(t) => t <= Utc::now(),
            None => true,
        }
    }

    pub fn expires_within(&self, seconds: i64) -> bool {
        match self.expire_at() {
            Some(t) => (t - Utc::now()).num_seconds() <= seconds,
            None => true,
        }
    }
}

/// 扫描目录下所有 codex-*.json 文件
pub fn scan_codex_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create auth dir {:?}", dir))?;
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("read dir {:?}", dir))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if name.starts_with("codex") && name.ends_with(".json") {
            out.push(path);
        } else {
            // 兼容用户直接丢进来的 OAuth 文件——尝试看看里面是不是 codex 类型
            if let Ok(s) = CodexTokenStorage::load(&path) {
                if s.kind == "codex" || (!s.access_token.is_empty() && !s.refresh_token.is_empty())
                {
                    out.push(path);
                }
            }
        }
    }
    Ok(out)
}
