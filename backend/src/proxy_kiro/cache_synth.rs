//! 合成 prompt-cache 计费（仅 usage 计费层面，不是真正的算力缓存）。
//!
//! 背景：Kiro / CodeWhisperer 上游不支持 Anthropic 的 prompt caching，响应里不会回
//! `cacheReadInputTokens` / `cacheWriteInputTokens`，所以下游（cool_api 等）看到的缓存
//! 命中永远是 0、按全量 input 计费。但 Kiro 是「按月请求次数」限额、**不按 token 计费**，
//! 给下游报缓存命中不会让我们对上游多花钱。
//!
//! 本模块据此为 kiro 反代「合成」缓存计费：读 Claude Code 自己在请求里打的 `cache_control`
//! 断点（CC 本来就发，且带 `ttl: "5m"` / `"1h"`），按**前缀重叠**把上游回报的**真实** input
//! token 总数拆成 `cache_read` / `cache_creation` / fresh 三份 —— 总数不变、内部自洽，
//! 不是凭空编数字。下游计费表现与 claude 渠道一致。
//!
//! 重要约束：
//!  - 只「拆分」上游给的真实总量，绝不放大总 input；
//!  - CC 没打 `cache_control` 时退化为「全部 fresh」，不伪造缓存；
//!  - 命中判定基于**累积前缀指纹**（SHA256），与 Anthropic「前缀必须逐字节相同才命中」一致；
//!  - 每个断点按其自带 ttl（5m / 1h）独立计时，过期即不再命中。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

/// Anthropic 允许的最大缓存断点数。
const MAX_BREAKPOINTS: usize = 4;
/// 估算 token 用的字节/词元比（与本项目其它估算口径一致：约 3 字节/token）。
const BYTES_PER_TOKEN: usize = 3;
/// 5m / 1h 之外的兜底 TTL（CC 不给 ttl 时按 5m）。
const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
const TTL_1H: Duration = Duration::from_secs(60 * 60);
const TTL_5M: Duration = Duration::from_secs(5 * 60);

/// 一个缓存断点：到此为止的累积前缀指纹 + 累积可缓存 token 估算 + 该断点的 ttl。
#[derive(Debug, Clone)]
struct Breakpoint {
    /// 到此块（含）为止，整个可缓存前缀的累积 SHA256 十六进制指纹。
    prefix_digest: String,
    /// 到此块（含）为止，累积的可缓存 token 估算数。
    cumulative_tokens: u32,
    /// 该断点的存活时长（来自 cache_control.ttl）。
    ttl: Duration,
}

/// 解析请求得到的缓存计划：一串断点（按出现顺序）。空表示请求没打任何 cache_control。
#[derive(Debug, Clone, Default)]
pub struct CachePlan {
    breakpoints: Vec<Breakpoint>,
}

impl CachePlan {
    pub fn is_empty(&self) -> bool {
        self.breakpoints.is_empty()
    }
}

/// 缓存计费拆分结果：三者之和应等于上游真实总 input。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheSplit {
    /// 命中已存前缀、按 cache 读取计费的 token。
    pub cache_read: i64,
    /// 本次新写入缓存（creation）的 token。
    pub cache_creation: i64,
    /// 未缓存的普通 input（最后一轮 user 等）。
    pub fresh_input: i64,
}

/// 进程内、带 TTL 的前缀指纹表。key = 前缀指纹，value = (累积 token, 过期时刻)。
///
/// 每个前缀按其断点 ttl 存活；命中即「续期」（刷新过期时刻），贴合 Anthropic「每次命中
/// 都会把缓存寿命续上」的语义。容量上限做简单 LRU 式裁剪，避免无限增长。
pub struct PromptCacheStore {
    inner: Mutex<HashMap<String, Entry>>,
    capacity: usize,
}

#[derive(Clone)]
struct Entry {
    tokens: u32,
    expires_at: Instant,
}

impl Default for PromptCacheStore {
    fn default() -> Self {
        Self::new(4096)
    }
}

impl PromptCacheStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            capacity: capacity.max(64),
        }
    }

    /// 核心：对一个 CachePlan 计算「命中多少前缀」，并把当前所有断点写入/续期。
    ///
    /// 返回命中的 token 数（即可计入 cache_read 的部分）。命中规则：在 plan 的断点里，
    /// 从**最长**前缀往短找，第一个仍存活于表中的断点即命中，其 cumulative_tokens 即命中量。
    pub fn lookup_and_record(&self, plan: &CachePlan) -> u32 {
        if plan.breakpoints.is_empty() {
            return 0;
        }
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();

        // 先清过期项（顺手控制规模）。
        map.retain(|_, e| e.expires_at > now);

        // 命中判定：最长前缀优先。
        let mut hit_tokens = 0u32;
        for bp in plan.breakpoints.iter().rev() {
            if let Some(entry) = map.get(&bp.prefix_digest) {
                if entry.expires_at > now {
                    hit_tokens = entry.tokens;
                    break;
                }
            }
        }

        // 写入 / 续期当前所有断点。
        for bp in &plan.breakpoints {
            let expires_at = now + bp.ttl;
            map.entry(bp.prefix_digest.clone())
                .and_modify(|e| {
                    // 续期取更晚者，token 取更大者（前缀只会增长）。
                    if expires_at > e.expires_at {
                        e.expires_at = expires_at;
                    }
                    if bp.cumulative_tokens > e.tokens {
                        e.tokens = bp.cumulative_tokens;
                    }
                })
                .or_insert(Entry {
                    tokens: bp.cumulative_tokens,
                    expires_at,
                });
        }

        // 规模控制：超容量时丢弃最早过期的若干项。
        if map.len() > self.capacity {
            let mut items: Vec<(String, Instant)> =
                map.iter().map(|(k, e)| (k.clone(), e.expires_at)).collect();
            items.sort_by_key(|(_, exp)| *exp);
            let remove = map.len() - self.capacity;
            for (k, _) in items.into_iter().take(remove) {
                map.remove(&k);
            }
        }

        hit_tokens
    }
}

/// 把上游回报的真实总 input，依据 plan 命中情况拆成 read / creation / fresh。
///
/// - `total_input`：上游 messageMetadataEvent 给的真实 input 总量（绝不超过它）；
/// - `hit_tokens`：store 命中的前缀 token 数（来自 lookup_and_record）；
/// - `plan`：本次请求的缓存计划，用其「最后一个断点的累积 token」作为「本应可缓存的前缀总量」。
///
/// 规则：
///   cache_read     = min(hit_tokens, total_input)
///   cacheable_total= min(最后断点累积 token, total_input)
///   cache_creation = max(0, cacheable_total - cache_read)
///   fresh_input    = total_input - cache_read - cache_creation
pub fn split_usage(total_input: i64, hit_tokens: u32, plan: &CachePlan) -> CacheSplit {
    if total_input <= 0 || plan.breakpoints.is_empty() {
        return CacheSplit {
            cache_read: 0,
            cache_creation: 0,
            fresh_input: total_input.max(0),
        };
    }
    let cacheable_total = plan
        .breakpoints
        .last()
        .map(|b| b.cumulative_tokens as i64)
        .unwrap_or(0)
        .min(total_input);
    let cache_read = (hit_tokens as i64).min(cacheable_total).min(total_input);
    let cache_creation = (cacheable_total - cache_read).max(0);
    let fresh_input = (total_input - cache_read - cache_creation).max(0);
    CacheSplit {
        cache_read,
        cache_creation,
        fresh_input,
    }
}

/// 从 Anthropic Messages 请求 JSON 解析缓存计划。
///
/// 扫描带 `cache_control` 的块，按出现顺序（system → tools → messages）累积前缀指纹与
/// token 估算，每遇到一个 cache_control 断点就记一条 Breakpoint，最多 MAX_BREAKPOINTS 个。
/// 块的 ttl 取自 `cache_control.ttl`（"1h" → 1 小时，其余 → 5 分钟）。
pub fn build_plan(raw: &Value) -> CachePlan {
    let mut hasher = Sha256::new();
    let mut cumulative_tokens: u32 = 0;
    let mut breakpoints: Vec<Breakpoint> = Vec::new();

    // 累加一个「块」：把其文本喂进指纹、累加 token；若带 cache_control 则落一个断点。
    let mut feed = |hasher: &mut Sha256,
                    cumulative_tokens: &mut u32,
                    breakpoints: &mut Vec<Breakpoint>,
                    text: &str,
                    cache_control: Option<&Value>| {
        hasher.update(text.as_bytes());
        hasher.update(b"\x1f"); // 块分隔符，避免相邻块拼接歧义
        *cumulative_tokens += estimate_tokens(text);
        if let Some(cc) = cache_control {
            if breakpoints.len() < MAX_BREAKPOINTS {
                let digest = format!("{:x}", hasher.clone().finalize());
                breakpoints.push(Breakpoint {
                    prefix_digest: digest,
                    cumulative_tokens: *cumulative_tokens,
                    ttl: parse_ttl(cc),
                });
            }
        }
    };

    // 1) system：字符串或块数组
    match raw.get("system") {
        Some(Value::String(s)) => {
            feed(&mut hasher, &mut cumulative_tokens, &mut breakpoints, s, None)
        }
        Some(Value::Array(blocks)) => {
            for b in blocks {
                let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                feed(
                    &mut hasher,
                    &mut cumulative_tokens,
                    &mut breakpoints,
                    text,
                    b.get("cache_control"),
                );
            }
        }
        _ => {}
    }

    // 2) tools：每个工具的名字+描述+schema 作为文本，cache_control 在工具对象上
    if let Some(tools) = raw.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let desc = t.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let schema = t
                .get("input_schema")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let text = format!("{name}\n{desc}\n{schema}");
            feed(
                &mut hasher,
                &mut cumulative_tokens,
                &mut breakpoints,
                &text,
                t.get("cache_control"),
            );
        }
    }

    // 3) messages：按出现顺序，文本块累加；cache_control 可能在块上
    if let Some(messages) = raw.get("messages").and_then(|v| v.as_array()) {
        for m in messages {
            match m.get("content") {
                Some(Value::String(s)) => {
                    feed(&mut hasher, &mut cumulative_tokens, &mut breakpoints, s, None)
                }
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        let text = block_text(b);
                        feed(
                            &mut hasher,
                            &mut cumulative_tokens,
                            &mut breakpoints,
                            &text,
                            b.get("cache_control"),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    CachePlan { breakpoints }
}

/// 取一个 content 块的代表文本（用于指纹 + token 估算）。
fn block_text(b: &Value) -> String {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => b.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        Some("tool_result") => b.get("content").map(value_to_text).unwrap_or_default(),
        Some("tool_use") => b.get("input").map(|v| v.to_string()).unwrap_or_default(),
        // image 等非文本块：用其 type 占位，保证前缀指纹仍随之变化
        _ => b.get("type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}

fn value_to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(|x| x.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(""),
        other => other.to_string(),
    }
}

/// `cache_control.ttl`："1h" → 1 小时；"5m" 或缺省/其它 → 5 分钟。
fn parse_ttl(cache_control: &Value) -> Duration {
    match cache_control.get("ttl").and_then(|v| v.as_str()) {
        Some("1h") => TTL_1H,
        Some("5m") => TTL_5M,
        _ => DEFAULT_TTL,
    }
}

/// 文本 token 估算（与项目其它处一致：约 3 字节/token，至少 1）。
fn estimate_tokens(text: &str) -> u32 {
    if text.is_empty() {
        0
    } else {
        ((text.len() / BYTES_PER_TOKEN) as u32).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sys_with_cc(text: &str, ttl: &str) -> Value {
        json!({
            "model": "claude-sonnet-4-5",
            "system": [
                {"type": "text", "text": text, "cache_control": {"type": "ephemeral", "ttl": ttl}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        })
    }

    #[test]
    fn no_cache_control_means_empty_plan() {
        let raw = json!({
            "model": "claude-sonnet-4-5",
            "system": "plain system",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let plan = build_plan(&raw);
        assert!(plan.is_empty());
        // 空 plan → 全 fresh
        let split = split_usage(100, 0, &plan);
        assert_eq!(split.fresh_input, 100);
        assert_eq!(split.cache_read, 0);
        assert_eq!(split.cache_creation, 0);
    }

    #[test]
    fn first_request_is_all_creation_then_read() {
        let store = PromptCacheStore::new(128);
        let raw = sys_with_cc(&"x".repeat(300), "5m"); // ~100 tokens 前缀
        let plan = build_plan(&raw);
        assert_eq!(plan.breakpoints.len(), 1);

        // 第一次：未命中 → 全 creation
        let hit1 = store.lookup_and_record(&plan);
        assert_eq!(hit1, 0);
        let split1 = split_usage(150, hit1, &plan);
        assert_eq!(split1.cache_read, 0);
        assert!(split1.cache_creation > 0);
        assert_eq!(
            split1.cache_read + split1.cache_creation + split1.fresh_input,
            150
        );

        // 第二次：同前缀 → 命中，read 等于前缀 token
        let hit2 = store.lookup_and_record(&plan);
        assert!(hit2 > 0);
        let split2 = split_usage(150, hit2, &plan);
        assert!(split2.cache_read > 0);
        assert_eq!(
            split2.cache_read + split2.cache_creation + split2.fresh_input,
            150
        );
    }

    #[test]
    fn split_never_exceeds_total() {
        let store = PromptCacheStore::new(128);
        let raw = sys_with_cc(&"y".repeat(3000), "1h"); // 前缀 ~1000 tokens
        let plan = build_plan(&raw);
        let _ = store.lookup_and_record(&plan);
        let hit = store.lookup_and_record(&plan);
        // 上游真实总量只有 50，远小于前缀估算 → 拆分后三者和仍 = 50，且都 >= 0
        let split = split_usage(50, hit, &plan);
        assert_eq!(
            split.cache_read + split.cache_creation + split.fresh_input,
            50
        );
        assert!(split.cache_read >= 0 && split.cache_creation >= 0 && split.fresh_input >= 0);
        assert!(split.cache_read <= 50);
    }

    #[test]
    fn ttl_1h_parsed() {
        let raw = sys_with_cc("abc", "1h");
        let plan = build_plan(&raw);
        assert_eq!(plan.breakpoints[0].ttl, TTL_1H);
        let raw5 = sys_with_cc("abc", "5m");
        let plan5 = build_plan(&raw5);
        assert_eq!(plan5.breakpoints[0].ttl, TTL_5M);
    }

    #[test]
    fn different_prefix_does_not_hit() {
        let store = PromptCacheStore::new(128);
        let a = build_plan(&sys_with_cc(&"a".repeat(300), "5m"));
        let b = build_plan(&sys_with_cc(&"b".repeat(300), "5m"));
        let _ = store.lookup_and_record(&a);
        // 不同前缀 → 不命中
        assert_eq!(store.lookup_and_record(&b), 0);
    }

    #[test]
    fn caps_breakpoints_at_four() {
        // 造 6 个带 cache_control 的 system 块，只应保留前 4 个断点
        let blocks: Vec<Value> = (0..6)
            .map(|i| {
                json!({"type":"text","text":format!("block{i}"),
                       "cache_control":{"type":"ephemeral"}})
            })
            .collect();
        let raw = json!({
            "model": "claude-sonnet-4-5",
            "system": blocks,
            "messages": [{"role":"user","content":"hi"}]
        });
        let plan = build_plan(&raw);
        assert_eq!(plan.breakpoints.len(), MAX_BREAKPOINTS);
    }
}
