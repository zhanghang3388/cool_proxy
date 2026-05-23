use anyhow::Result;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyRow {
    pub id: String,
    pub url: String,
    pub label: String,
    pub created_at: i64, // unix ms
}

fn row_to_proxy(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProxyRow> {
    Ok(ProxyRow {
        id: r.get("id")?,
        url: r.get("url")?,
        label: r.get("label")?,
        created_at: r.get("created_at")?,
    })
}

pub fn list(pool: &SqlitePool) -> Result<Vec<ProxyRow>> {
    let conn = pool.get()?;
    let mut stmt = conn.prepare("SELECT * FROM proxies ORDER BY created_at ASC")?;
    let it = stmt.query_map([], row_to_proxy)?;
    Ok(it.filter_map(|r| r.ok()).collect())
}

pub fn url_by_id(pool: &SqlitePool, id: &str) -> Result<Option<String>> {
    let conn = pool.get()?;
    Ok(conn
        .query_row("SELECT url FROM proxies WHERE id = ?1", params![id], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

pub fn id_by_url(pool: &SqlitePool, url: &str) -> Result<Option<String>> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(None);
    }
    let conn = pool.get()?;
    Ok(conn
        .query_row("SELECT id FROM proxies WHERE url = ?1", params![url], |r| {
            r.get::<_, String>(0)
        })
        .optional()?)
}

pub fn add(pool: &SqlitePool, url: String, label: String) -> Result<ProxyRow> {
    let conn = pool.get()?;
    let id = format!("px_{}", &Uuid::new_v4().simple().to_string()[..12]);
    let created_at = Utc::now().timestamp_millis();
    let n = conn.execute(
        "INSERT INTO proxies(id,url,label,created_at) VALUES(?1,?2,?3,?4)
         ON CONFLICT(url) DO NOTHING",
        params![id, url, label, created_at],
    )?;
    if n == 0 {
        anyhow::bail!("proxy already exists");
    }
    Ok(ProxyRow {
        id,
        url,
        label,
        created_at,
    })
}

pub fn update(
    pool: &SqlitePool,
    id: &str,
    url: Option<String>,
    label: Option<String>,
) -> Result<()> {
    let conn = pool.get()?;
    if let Some(u) = url {
        conn.execute(
            "UPDATE proxies SET url = ?2 WHERE id = ?1",
            params![id, u],
        )?;
    }
    if let Some(l) = label {
        conn.execute(
            "UPDATE proxies SET label = ?2 WHERE id = ?1",
            params![id, l],
        )?;
    }
    Ok(())
}

pub fn delete(pool: &SqlitePool, id: &str) -> Result<bool> {
    let conn = pool.get()?;
    let n = conn.execute("DELETE FROM proxies WHERE id = ?1", params![id])?;
    Ok(n > 0)
}

/// 给"上传新账号自动分配代理"用的 round-robin。
/// counter 存在 kv 表里 (key = "proxy_assign_counter")，保证重启不偏。
pub fn next_assignment(pool: &SqlitePool) -> Result<Option<(String, String)>> {
    let mut conn = pool.get()?;
    let tx = conn.transaction()?;
    let proxies: Vec<(String, String)> = {
        let mut stmt = tx.prepare("SELECT id, url FROM proxies ORDER BY created_at ASC")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    if proxies.is_empty() {
        return Ok(None);
    }
    let counter: i64 = tx
        .query_row(
            "SELECT v FROM kv WHERE k = 'proxy_assign_counter'",
            [],
            |r| {
                let s: String = r.get(0)?;
                Ok(s.parse::<i64>().unwrap_or(0))
            },
        )
        .optionally()
        .unwrap_or(0);
    let idx = (counter as usize) % proxies.len();
    let next = counter.wrapping_add(1);
    tx.execute(
        "INSERT INTO kv(k,v) VALUES('proxy_assign_counter',?1)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![next.to_string()],
    )?;
    let pair = proxies[idx].clone();
    tx.commit()?;
    Ok(Some(pair))
}

trait OptionallyExt<T> {
    fn optionally(self) -> Option<T>;
}

impl<T> OptionallyExt<T> for rusqlite::Result<T> {
    fn optionally(self) -> Option<T> {
        match self {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(_) => None,
        }
    }
}
