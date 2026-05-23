use axum::http::Method;
use chrono::Utc;

use crate::store::requests as store_requests;
pub use crate::store::requests::RequestRow as LogEntry;
use crate::store::SqlitePool;

/// 请求日志：DB 主，重启不丢。
pub struct RequestLog {
    db: SqlitePool,
}

impl RequestLog {
    pub fn new(db: SqlitePool) -> Self {
        Self { db }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn push(
        &self,
        method: &Method,
        path: &str,
        account_id: Option<String>,
        model: Option<String>,
        status: u16,
        duration_ms: u64,
        attempts: u32,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        total_tokens: Option<i64>,
        error: Option<String>,
    ) {
        let now = Utc::now().timestamp_millis();
        let r = store_requests::InsertRequest {
            at_ms: now,
            account_id: account_id.as_deref(),
            model: model.as_deref(),
            method: method.as_str(),
            path,
            status,
            duration_ms,
            attempts,
            input_tokens,
            output_tokens,
            total_tokens,
            error: error.as_deref(),
        };
        if let Err(e) = store_requests::insert(&self.db, &r) {
            tracing::warn!("request log insert failed: {e:?}");
        }
    }

    pub fn snapshot(&self, limit: usize, before_id: Option<i64>) -> Vec<LogEntry> {
        store_requests::list_recent(&self.db, limit as i64, before_id).unwrap_or_default()
    }

    pub fn clear(&self) {
        let _ = store_requests::clear_before(&self.db, None);
    }
}
