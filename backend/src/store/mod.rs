use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

pub mod accounts;
pub mod kiro_accounts;
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

        CREATE TABLE IF NOT EXISTS account_model_states (
            account_id        TEXT NOT NULL,
            model_key         TEXT NOT NULL,
            next_retry_after  INTEGER,
            quota_backoff_lv  INTEGER NOT NULL DEFAULT 0,
            transient_fails   INTEGER NOT NULL DEFAULT 0,
            last_status       INTEGER,
            last_error        TEXT,
            last_kind         TEXT,
            updated_at        INTEGER NOT NULL,
            PRIMARY KEY(account_id, model_key),
            FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_ams_next_retry ON account_model_states(next_retry_after);

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

        -- Kiro 账号池：和 codex accounts 平行的一张表。
        CREATE TABLE IF NOT EXISTS kiro_accounts (
            id              TEXT PRIMARY KEY,
            email           TEXT NOT NULL DEFAULT '',
            user_id         TEXT,
            login_provider  TEXT,
            auth_method     TEXT NOT NULL DEFAULT 'social',  -- social | idc
            enabled         INTEGER NOT NULL DEFAULT 1,
            access_token    TEXT NOT NULL DEFAULT '',
            refresh_token   TEXT NOT NULL DEFAULT '',
            token_type      TEXT,
            expires_at      INTEGER,           -- unix ms
            -- IdC / Builder-ID 刷新所需
            idc_region      TEXT,
            issuer_url      TEXT,
            client_id       TEXT,
            client_secret   TEXT,
            scopes          TEXT,
            login_hint      TEXT,
            profile_arn     TEXT,
            -- 套餐 / 额度
            plan_name       TEXT,
            plan_tier       TEXT,
            credits_total   REAL,
            credits_used    REAL,
            bonus_total     REAL,
            bonus_used      REAL,
            usage_reset_at  INTEGER,           -- unix ms
            bonus_expire_days INTEGER,
            -- 运行时状态
            last_refresh_at INTEGER,
            failure_count   INTEGER NOT NULL DEFAULT 0,
            cooldown_until  INTEGER,
            last_error      TEXT,
            last_used_at    INTEGER,
            total_requests  INTEGER NOT NULL DEFAULT 0,
            total_failures  INTEGER NOT NULL DEFAULT 0,
            proxy_url       TEXT NOT NULL DEFAULT '',
            -- 封禁 / 额度查询状态
            status          TEXT,
            status_reason   TEXT,
            quota_checked_at INTEGER,
            quota_error     TEXT,
            -- 原始快照，刷新 / 调试时保留
            raw_auth_token  TEXT NOT NULL DEFAULT '{}',
            raw_usage       TEXT NOT NULL DEFAULT '{}',
            created_at      INTEGER NOT NULL DEFAULT (CAST(strftime('%s','now') AS INTEGER) * 1000)
        );
        CREATE INDEX IF NOT EXISTS idx_kiro_enabled ON kiro_accounts(enabled);
        CREATE INDEX IF NOT EXISTS idx_kiro_email ON kiro_accounts(email);
        CREATE INDEX IF NOT EXISTS idx_kiro_proxy ON kiro_accounts(proxy_url);
        ",
    )?;
    ensure_column(conn, "accounts", "quota_5h_used_percent", "REAL")?;
    ensure_column(conn, "accounts", "quota_5h_reset_at", "INTEGER")?;
    ensure_column(conn, "accounts", "quota_week_used_percent", "REAL")?;
    ensure_column(conn, "accounts", "quota_week_reset_at", "INTEGER")?;
    ensure_column(conn, "accounts", "quota_checked_at", "INTEGER")?;
    ensure_column(conn, "accounts", "quota_error", "TEXT")?;
    Ok(())
}

fn ensure_column(conn: &rusqlite::Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for c in cols {
        if c? == column {
            return Ok(());
        }
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
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
            .query_row("SELECT v FROM kv WHERE k = ?1", params![k], |r| {
                r.get::<_, String>(0)
            })
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
