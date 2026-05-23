use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use super::SqlitePool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRow {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub duration_ms: u64,
    pub attempts: u32,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct InsertRequest<'a> {
    pub at_ms: i64,
    pub account_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub method: &'a str,
    pub path: &'a str,
    pub status: u16,
    pub duration_ms: u64,
    pub attempts: u32,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub error: Option<&'a str>,
}

pub fn insert(pool: &SqlitePool, r: &InsertRequest<'_>) -> Result<i64> {
    let conn = pool.get()?;
    conn.execute(
        "INSERT INTO requests(
            at, account_id, model, method, path, status, duration_ms, attempts,
            input_tokens, output_tokens, total_tokens, error
         ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            r.at_ms,
            r.account_id,
            r.model,
            r.method,
            r.path,
            r.status as i64,
            r.duration_ms as i64,
            r.attempts as i64,
            r.input_tokens,
            r.output_tokens,
            r.total_tokens,
            r.error,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn row_to_request(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestRow> {
    let at_ms: i64 = r.get("at")?;
    Ok(RequestRow {
        id: r.get("id")?,
        at: DateTime::<Utc>::from_timestamp_millis(at_ms).unwrap_or_default(),
        account_id: r.get("account_id")?,
        model: r.get("model")?,
        method: r.get("method")?,
        path: r.get("path")?,
        status: r.get::<_, i64>("status")? as u16,
        duration_ms: r.get::<_, i64>("duration_ms")? as u64,
        attempts: r.get::<_, i64>("attempts")? as u32,
        input_tokens: r.get("input_tokens")?,
        output_tokens: r.get("output_tokens")?,
        total_tokens: r.get("total_tokens")?,
        error: r.get("error")?,
    })
}

/// 倒序拿最近 limit 条；before_id 用于"加载更多"分页（< before_id）。
pub fn list_recent(
    pool: &SqlitePool,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<RequestRow>> {
    let conn = pool.get()?;
    let mut rows = Vec::new();
    if let Some(bid) = before_id {
        let mut stmt = conn.prepare(
            "SELECT * FROM requests WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        for r in stmt.query_map(params![bid, limit], row_to_request)? {
            rows.push(r?);
        }
    } else {
        let mut stmt = conn.prepare("SELECT * FROM requests ORDER BY id DESC LIMIT ?1")?;
        for r in stmt.query_map(params![limit], row_to_request)? {
            rows.push(r?);
        }
    }
    Ok(rows)
}

pub fn clear_before(pool: &SqlitePool, before_ms: Option<i64>) -> Result<usize> {
    let conn = pool.get()?;
    let n = if let Some(t) = before_ms {
        conn.execute("DELETE FROM requests WHERE at < ?1", params![t])?
    } else {
        conn.execute("DELETE FROM requests", [])?
    };
    Ok(n)
}

#[derive(Debug, Serialize)]
pub struct UsageBucket {
    pub key: String,
    pub count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct UsageReport {
    pub total_count: i64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_total_tokens: i64,
    pub by_model: Vec<UsageBucket>,
    pub by_account: Vec<UsageBucket>,
}

pub fn usage(
    pool: &SqlitePool,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
) -> Result<UsageReport> {
    let conn = pool.get()?;
    let from = from_ms.unwrap_or(0);
    let to = to_ms.unwrap_or(i64::MAX);

    let (count, input_tok, output_tok, total_tok): (i64, i64, i64, i64) = conn.query_row(
        "SELECT
            COUNT(*),
            COALESCE(SUM(input_tokens),0),
            COALESCE(SUM(output_tokens),0),
            COALESCE(SUM(total_tokens),0)
         FROM requests WHERE at >= ?1 AND at <= ?2",
        params![from, to],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    let mut by_model = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(model,'(unknown)'), COUNT(*),
                    COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(total_tokens),0)
             FROM requests WHERE at >= ?1 AND at <= ?2
             GROUP BY COALESCE(model,'(unknown)')
             ORDER BY SUM(total_tokens) DESC",
        )?;
        for r in stmt.query_map(params![from, to], |r| {
            Ok(UsageBucket {
                key: r.get::<_, String>(0)?,
                count: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                total_tokens: r.get(4)?,
            })
        })? {
            by_model.push(r?);
        }
    }

    let mut by_account = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(account_id,'(unknown)'), COUNT(*),
                    COALESCE(SUM(input_tokens),0),
                    COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(total_tokens),0)
             FROM requests WHERE at >= ?1 AND at <= ?2
             GROUP BY COALESCE(account_id,'(unknown)')
             ORDER BY SUM(total_tokens) DESC
             LIMIT 200",
        )?;
        for r in stmt.query_map(params![from, to], |r| {
            Ok(UsageBucket {
                key: r.get::<_, String>(0)?,
                count: r.get(1)?,
                input_tokens: r.get(2)?,
                output_tokens: r.get(3)?,
                total_tokens: r.get(4)?,
            })
        })? {
            by_account.push(r?);
        }
    }

    Ok(UsageReport {
        total_count: count,
        total_input_tokens: input_tok,
        total_output_tokens: output_tok,
        total_total_tokens: total_tok,
        by_model,
        by_account,
    })
}

#[allow(dead_code)]
pub fn count(pool: &SqlitePool) -> Result<i64> {
    let conn = pool.get()?;
    Ok(conn.query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))?)
}

#[allow(dead_code)]
pub fn get(pool: &SqlitePool, id: i64) -> Result<Option<RequestRow>> {
    let conn = pool.get()?;
    Ok(conn
        .query_row("SELECT * FROM requests WHERE id = ?1", params![id], row_to_request)
        .optional()?)
}
