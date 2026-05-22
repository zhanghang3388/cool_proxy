use std::collections::VecDeque;
use std::sync::RwLock;

use axum::http::Method;
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub id: u64,
    pub at: DateTime<Utc>,
    pub method: String,
    pub path: String,
    pub account_id: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
    pub attempts: u32,
    pub error: Option<String>,
}

pub struct RequestLog {
    capacity: usize,
    inner: RwLock<Inner>,
}

struct Inner {
    entries: VecDeque<LogEntry>,
    next_id: u64,
}

impl RequestLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            inner: RwLock::new(Inner {
                entries: VecDeque::with_capacity(capacity.max(1)),
                next_id: 1,
            }),
        }
    }

    pub fn push(
        &self,
        method: &Method,
        path: &str,
        account_id: Option<String>,
        status: u16,
        duration_ms: u64,
        attempts: u32,
        error: Option<String>,
    ) {
        let mut g = self.inner.write().unwrap();
        let id = g.next_id;
        g.next_id = g.next_id.wrapping_add(1);
        let entry = LogEntry {
            id,
            at: Utc::now(),
            method: method.as_str().to_string(),
            path: path.to_string(),
            account_id,
            status,
            duration_ms,
            attempts,
            error,
        };
        if g.entries.len() == self.capacity {
            g.entries.pop_front();
        }
        g.entries.push_back(entry);
    }

    pub fn snapshot(&self, limit: usize) -> Vec<LogEntry> {
        let g = self.inner.read().unwrap();
        g.entries
            .iter()
            .rev()
            .take(limit.min(g.entries.len()))
            .cloned()
            .collect()
    }

    pub fn clear(&self) {
        self.inner.write().unwrap().entries.clear();
    }
}
