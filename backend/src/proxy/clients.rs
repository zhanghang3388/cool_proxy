use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::{Context, Result};

/// 按代理 URL 缓存 reqwest::Client，保证同一代理的连接池能复用。
/// `proxy_url` 空字符串表示直连（key="" -> 默认 Client）。
pub struct ProxiedClients {
    inner: RwLock<HashMap<String, reqwest::Client>>,
}

impl ProxiedClients {
    pub fn new() -> Self {
        let mut m = HashMap::new();
        m.insert(
            String::new(),
            build_client(None).expect("build default client"),
        );
        Self {
            inner: RwLock::new(m),
        }
    }

    pub fn get(&self, proxy_url: &str) -> Result<reqwest::Client> {
        let key = proxy_url.trim();
        {
            let g = self.inner.read().unwrap();
            if let Some(c) = g.get(key) {
                return Ok(c.clone());
            }
        }
        let client = build_client(if key.is_empty() { None } else { Some(key) })?;
        let mut g = self.inner.write().unwrap();
        // 并发场景下别人可能已经先插入了，统一返回 map 里实际存的那个
        Ok(g.entry(key.to_string()).or_insert(client).clone())
    }

    /// 删除某条代理对应的连接池缓存（在用户删除/编辑代理时调用，避免脏连接）。
    pub fn invalidate(&self, proxy_url: &str) {
        let key = proxy_url.trim();
        if key.is_empty() {
            return;
        }
        self.inner.write().unwrap().remove(key);
    }
}

fn build_client(proxy_url: Option<&str>) -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder()
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .http2_keep_alive_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(600));
    if let Some(url) = proxy_url {
        let proxy = reqwest::Proxy::all(url)
            .with_context(|| format!("parse proxy url: {url}"))?;
        b = b.proxy(proxy);
    } else {
        // 没指定代理 = 直连，强制忽略环境里的 HTTP(S)_PROXY，避免误走系统代理
        b = b.no_proxy();
    }
    Ok(b.build()?)
}
