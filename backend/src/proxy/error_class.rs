use std::time::Duration;

/// 上游错误的语义分类。决定 pool 怎么记账、handler 是否触发 refresh / 重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// access_token 失效。需要 refresh，本次换号。
    Auth,
    /// 限流。尊重 Retry-After，否则指数 backoff。
    Quota,
    /// 上游不支持该 model。长冷却（12h）model 维度。
    NotFound,
    /// 5xx / 408 / 425：临时抖动。连续 N 次才短冷却。
    Transient,
    /// 网络层失败（DNS / TLS / 连不上 / 超时）。同 Transient，但写到账号级。
    Network,
    /// 其他 4xx：客户端自己的错，直接返给调用方，不计入失败。
    Client,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Auth => "auth",
            ErrorKind::Quota => "quota",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Transient => "transient",
            ErrorKind::Network => "network",
            ErrorKind::Client => "client",
        }
    }
}

/// `status = None` 表示请求层失败（reqwest::Error），归为 Network。
pub fn classify(status: Option<u16>) -> ErrorKind {
    match status {
        Some(401) | Some(402) | Some(403) => ErrorKind::Auth,
        Some(404) => ErrorKind::NotFound,
        Some(429) => ErrorKind::Quota,
        Some(408) | Some(425) | Some(500) | Some(502) | Some(503) | Some(504) => {
            ErrorKind::Transient
        }
        Some(_) => ErrorKind::Client,
        None => ErrorKind::Network,
    }
}

/// quota backoff：base = 1s，每升一级翻倍，封顶 30min。
/// 返回 (cooldown, next_level)。
pub fn quota_backoff(prev_level: i64) -> (Duration, i64) {
    let lv = prev_level.max(0);
    let base = 1u64;
    let cap_secs = 30 * 60;
    let shift = lv.min(20) as u32;
    let raw = base.checked_shl(shift).unwrap_or(u64::MAX);
    let secs = raw.min(cap_secs);
    let next = if secs >= cap_secs { lv } else { lv + 1 };
    (Duration::from_secs(secs), next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_codes() {
        assert_eq!(classify(Some(200)), ErrorKind::Client); // 不会被这个函数命中，但分类是 client；handler 自己处理 2xx
        assert_eq!(classify(Some(401)), ErrorKind::Auth);
        assert_eq!(classify(Some(402)), ErrorKind::Auth);
        assert_eq!(classify(Some(403)), ErrorKind::Auth);
        assert_eq!(classify(Some(404)), ErrorKind::NotFound);
        assert_eq!(classify(Some(408)), ErrorKind::Transient);
        assert_eq!(classify(Some(425)), ErrorKind::Transient);
        assert_eq!(classify(Some(429)), ErrorKind::Quota);
        assert_eq!(classify(Some(500)), ErrorKind::Transient);
        assert_eq!(classify(Some(502)), ErrorKind::Transient);
        assert_eq!(classify(Some(503)), ErrorKind::Transient);
        assert_eq!(classify(Some(504)), ErrorKind::Transient);
        assert_eq!(classify(Some(400)), ErrorKind::Client);
        assert_eq!(classify(Some(422)), ErrorKind::Client);
        assert_eq!(classify(None), ErrorKind::Network);
    }

    #[test]
    fn quota_backoff_progression() {
        let (d0, l1) = quota_backoff(0);
        assert_eq!(d0, Duration::from_secs(1));
        assert_eq!(l1, 1);
        let (d1, l2) = quota_backoff(l1);
        assert_eq!(d1, Duration::from_secs(2));
        assert_eq!(l2, 2);
        let (d2, l3) = quota_backoff(l2);
        assert_eq!(d2, Duration::from_secs(4));
        assert_eq!(l3, 3);
    }

    #[test]
    fn quota_backoff_caps_at_30min() {
        let (d, lv_next) = quota_backoff(60);
        assert_eq!(d, Duration::from_secs(30 * 60));
        // 已封顶后不再加级
        assert_eq!(lv_next, 60);
    }
}
