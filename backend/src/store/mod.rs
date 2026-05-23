use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod proxies;
pub mod requests;

pub type SqlitePool = Pool<SqliteConnectionManager>;

/// SQLite 文件路径：放在 auth_dir 下，方便备份。
pub fn default_db_path(auth_dir: &Path) -> PathBuf {
    auth_dir.join("cool_proxy.db")
}

/// 建库 + 迁移 + 启用合理的 PRAGMA。
pub fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create db dir {:?}", dir))?;
    }
    let manager = SqliteConnectionManager::file(path).with_init(|c| {
        c.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
        )
    });
    let pool = Pool::builder()
        .max_size(8)
        .build(manager)
        .with_context(|| format!("open sqlite {:?}", path))?;
    {
        let conn = pool.get()?;
        migrate(&conn)?;
    }
    Ok(pool)
}

fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS accounts (
            id              TEXT PRIMARY KEY,
            email           TEXT NOT NULL DEFAULT '',
            account_id      TEXT NOT NULL DEFAULT '',
            plan            TEXT,
            enabled         INTEGER NOT NULL DEFAULT 1,
            access_token    TEXT NOT NULL DEFAULT '',
            refresh_token   TEXT NOT NULL DEFAULT '',
            id_token        TEXT NOT NULL DEFAULT '',
            expire_at       INTEGER,           -- unix ms
            last_refresh_at INTEGER,
            failure_count   INTEGER NOT NULL DEFAULT 0,
            cooldown_until  INTEGER,
            last_error      TEXT,
            last_used_at    INTEGER,
            total_requests  INTEGER NOT NULL DEFAULT 0,
            total_failures  INTEGER NOT NULL DEFAULT 0,
            proxy_url       TEXT NOT NULL DEFAULT '',
            raw_extra       TEXT NOT NULL DEFAULT '{}',
            created_at      INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER) * 1000)
        );
        CREATE INDEX IF NOT EXISTS idx_accounts_enabled ON accounts(enabled);
        CREATE INDEX IF NOT EXISTS idx_accounts_email ON accounts(email);
        CREATE INDEX IF NOT EXISTS idx_accounts_proxy ON accounts(proxy_url);

        CREATE TABLE IF NOT EXISTS proxies (
            id          TEXT PRIMARY KEY,
            url         TEXT NOT NULL UNIQUE,
            label       TEXT NOT NULL DEFAULT '',
            created_at  INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS kv (
            k TEXT PRIMARY KEY,
            v TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS requests (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            at              INTEGER NOT NULL,           -- unix ms
            account_id      TEXT,
            model           TEXT,
            method          TEXT NOT NULL,
            path            TEXT NOT NULL,
            status          INTEGER NOT NULL,
            duration_ms     INTEGER NOT NULL,
            attempts        INTEGER NOT NULL,
            input_tokens    INTEGER,
            output_tokens   INTEGER,
            total_tokens    INTEGER,
            error           TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_requests_at ON requests(at DESC);
        CREATE INDEX IF NOT EXISTS idx_requests_model ON requests(model);
        CREATE INDEX IF NOT EXISTS idx_requests_account ON requests(account_id);
        ",
    )?;
    Ok(())
}

/// 把 `Option<DateTime<Utc>>` 转成 unix ms，方便 sqlite 存储 + 索引。
pub fn dt_to_ms(t: Option<DateTime<Utc>>) -> Option<i64> {
    t.map(|x| x.timestamp_millis())
}

pub fn ms_to_dt(ms: Option<i64>) -> Option<DateTime<Utc>> {
    ms.and_then(|m| DateTime::<Utc>::from_timestamp_millis(m))
}

/// 一次性 KV，用于记录"是否已经从 auths/ 导入过"这种迁移标记。
#[derive(Debug, Serialize, Deserialize)]
pub struct Kv;

impl Kv {
    pub fn get(conn: &rusqlite::Connection, k: &str) -> Result<Option<String>> {
        Ok(conn
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| r.get::<_, String>(0))
            .optional()?)
    }

    pub fn set(conn: &rusqlite::Connection, k: &str, v: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO kv(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![k, v],
        )?;
        Ok(())
    }
}
