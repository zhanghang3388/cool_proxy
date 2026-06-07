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

/// 请求里**每个块边界**的累积前缀：用于 lookup 时按「最长前缀」命中历史缓存，
/// 与本次是否在该位置打了 cache_control 无关（对齐 Anthropic「命中最长缓存前缀」语义）。
#[derive(Debug, Clone)]
struct Checkpoint {
    /// 到此块（含）为止的累积 SHA256 十六进制指纹。
    prefix_digest: String,
    /// 到此块（含）为止的累积可缓存 token 估算数。
    cumulative_tokens: u32,
}

/// 解析请求得到的缓存计划。
///
/// - `checkpoints`：**每个块边界**的累积前缀（按出现顺序，token 递增），lookup 用；
/// - `breakpoints`：带 `cache_control` 的断点（checkpoints 的子集），record（写入/续期）用。
///
/// `is_empty()` 以 breakpoints 为准：没打任何 cache_control 即视为「不参与合成缓存」。
#[derive(Debug, Clone, Default)]
pub struct CachePlan {
    checkpoints: Vec<Checkpoint>,
    breakpoints: Vec<Breakpoint>,
}

impl CachePlan {
    pub fn is_empty(&self) -> bool {
        self.breakpoints.is_empty()
    }

    /// 本次请求声明的 cache_control 断点数（诊断用）。
    pub fn breakpoint_count(&self) -> usize {
        self.breakpoints.len()
    }

    /// 可缓存前缀总量 = 到最后一个 cache_control 断点为止的累积 token（诊断用）。
    pub fn cacheable_tokens(&self) -> u32 {
        self.breakpoints
            .last()
            .map(|b| b.cumulative_tokens)
            .unwrap_or(0)
    }

    /// 块边界（checkpoint）数（诊断用；旧版无此字段，可借此确认新代码已部署）。
    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// 整个请求的累积可缓存 token 估算（最后一个块边界），= split 时的缩放分母。
    pub fn total_estimated_tokens(&self) -> u32 {
        self.checkpoints
            .last()
            .map(|c| c.cumulative_tokens)
            .unwrap_or(0)
    }

    /// 系统前缀（第一个块边界）的指纹前 12 位 + 累积 token（诊断用）。
    /// 跨轮对比这个值即可判断「被缓存前缀是否逐字节稳定」。
    pub fn first_checkpoint(&self) -> String {
        match self.checkpoints.first() {
            Some(c) => format!("{}:{}", short(&c.prefix_digest), c.cumulative_tokens),
            None => "none".to_string(),
        }
    }

    /// 本次声明的各 cache_control 断点指纹前 12 位 + 累积 token（诊断用）。
    pub fn breakpoint_digests(&self) -> Vec<String> {
        self.breakpoints
            .iter()
            .map(|b| format!("{}:{}", short(&b.prefix_digest), b.cumulative_tokens))
            .collect()
    }
}

/// 取指纹前 12 位，便于日志里跨轮肉眼对比。
fn short(digest: &str) -> &str {
    &digest[..digest.len().min(12)]
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

    /// 核心：对一个 CachePlan 计算「命中多少前缀」，并把当前所有 cache_control 断点写入/续期。
    ///
    /// 命中规则（对齐 Anthropic「命中最长缓存前缀」）：在本次请求的**每个块边界**
    /// （checkpoints）里，找一个仍存活于 store、且不超过本次可缓存区域（最后一个 cache_control
    /// 断点）的、token 最大者。**与本次是否在该位置重新打 cache_control 无关**——这才能让
    /// 「断点随对话前移」的多轮场景在第 2 轮起读到上一轮缓存的前缀。
    ///
    /// 写入：只在带 cache_control 的断点处登记/续期（写语义不变）。
    pub fn lookup_and_record(&self, plan: &CachePlan) -> u32 {
        if plan.breakpoints.is_empty() {
            return 0;
        }
        let now = Instant::now();
        let mut map = self.inner.lock().unwrap();

        // 先清过期项（顺手控制规模）。
        map.retain(|_, e| e.expires_at > now);

        // 可缓存区域上界 = 最后一个 cache_control 断点的累积 token。
        let cacheable_tokens = plan.cacheable_tokens();

        // 命中判定：在「本次请求的所有块边界」里，从最长前缀往短找，第一个仍存活于表中
        // 且 ≤ cacheable 的边界即命中（checkpoints 按 token 递增，rev 即从长到短）。
        let mut hit_tokens = 0u32;
        for cp in plan.checkpoints.iter().rev() {
            if cp.cumulative_tokens > cacheable_tokens {
                continue; // 超出本次可缓存区域，不计入 read
            }
            if let Some(entry) = map.get(&cp.prefix_digest) {
                if entry.expires_at > now {
                    // 取较小者防御估算漂移；同一前缀两次估算本应相等。
                    hit_tokens = entry.tokens.min(cp.cumulative_tokens);
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
/// 详见函数体注释：先在「估算口径」算出三段比例，再等比缩放到真实 `total_input`，
/// 避免估算偏大把本轮 creation 额度吃掉（多轮 creation 恒 0 的根因）。
pub fn split_usage(total_input: i64, hit_tokens: u32, plan: &CachePlan) -> CacheSplit {
    if total_input <= 0 || plan.breakpoints.is_empty() {
        return CacheSplit {
            cache_read: 0,
            cache_creation: 0,
            fresh_input: total_input.max(0),
        };
    }
    // read/creation/fresh 的比例先在「估算口径」里算出来（三段同一套 3 字节/token 估算，
    // 内部自洽），再整体等比缩放到真实 total_input。
    //
    // 为什么不能直接跨口径 min/相减：估算（在未过滤的原始请求上、按 3 字节/token）系统性比
    // 上游真实分词偏大。旧实现拿「估算口径的 hit」去 min/减「真实口径的 total」，虚高的 hit 把
    // 本属于「本轮新增」的 creation 额度整个吃掉 —— 多轮里 creation 恒为 0、read 恒等于全部，
    // 命中率诡异地接近满。等比缩放保留了「本轮 delta」这一段，creation 如实反映每轮新增。
    //
    // 记 e_total=全请求累积估算，e_cacheable=最后断点累积估算，e_hit=命中估算
    // （lookup 保证 e_hit ≤ e_cacheable ≤ e_total）：
    //   cache_read     = total_input * e_hit       / e_total
    //   cacheable      = total_input * e_cacheable / e_total
    //   cache_creation = cacheable - cache_read
    //   fresh_input    = total_input - cacheable
    // 三者之和恒等于 total_input（末项吸收整除余数），且都 ≥ 0。
    let e_total = plan.total_estimated_tokens() as i128;
    if e_total <= 0 {
        return CacheSplit {
            cache_read: 0,
            cache_creation: 0,
            fresh_input: total_input,
        };
    }
    let e_cacheable = (plan.cacheable_tokens() as i128).min(e_total);
    // hit 钳到 cacheable 上界，防御估算漂移（正常 lookup 已保证 ≤）。
    let e_hit = (hit_tokens as i128).min(e_cacheable);
    let total = total_input as i128;

    // 同一缩放因子 total/e_total 作用于 hit 与 cacheable，保证比例不被破坏。
    let cache_read = (total * e_hit / e_total) as i64;
    let cacheable = (total * e_cacheable / e_total) as i64;
    let cache_creation = (cacheable - cache_read).max(0);
    let fresh_input = (total_input - cache_read - cache_creation).max(0);
    CacheSplit {
        cache_read,
        cache_creation,
        fresh_input,
    }
}

/// 从 Anthropic Messages 请求 JSON 解析缓存计划。
///
/// 按出现顺序（system → tools → messages）逐块累积前缀指纹与 token 估算：
///  - **每个块**都落一个 checkpoint（用于 lookup 时按最长前缀命中历史缓存）；
///  - 带 `cache_control` 的块**额外**落一个 Breakpoint（用于 record），最多 MAX_BREAKPOINTS 个，
///    其 ttl 取自 `cache_control.ttl`（"1h" → 1 小时，其余 → 5 分钟）。
pub fn build_plan(raw: &Value) -> CachePlan {
    let mut hasher = Sha256::new();
    let mut cumulative_tokens: u32 = 0;
    let mut checkpoints: Vec<Checkpoint> = Vec::new();
    let mut breakpoints: Vec<Breakpoint> = Vec::new();

    // 累加一个「块」：喂指纹、累加 token，每块都落一个 checkpoint（lookup 用）；
    // 若该块带 cache_control 则**额外**落一个 Breakpoint（record 用）。
    let feed = |hasher: &mut Sha256,
                    cumulative_tokens: &mut u32,
                    checkpoints: &mut Vec<Checkpoint>,
                    breakpoints: &mut Vec<Breakpoint>,
                    text: &str,
                    cache_control: Option<&Value>| {
        // 规范化后再喂指纹：剥掉头部逐轮变化的 git/日期/环境噪音，让稳定前缀逐字节一致。
        // token 也按规范化后文本估算，保证「同一前缀两轮估算相等」（防御性 min 不会无故缩水）。
        let canon = canonicalize_for_fingerprint(text);
        hasher.update(canon.as_bytes());
        hasher.update(b"\x1f"); // 块分隔符，避免相邻块拼接歧义
        *cumulative_tokens += estimate_tokens(&canon);
        let digest = format!("{:x}", hasher.clone().finalize());
        checkpoints.push(Checkpoint {
            prefix_digest: digest.clone(),
            cumulative_tokens: *cumulative_tokens,
        });
        if let Some(cc) = cache_control {
            if breakpoints.len() < MAX_BREAKPOINTS {
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
        Some(Value::String(s)) => feed(
            &mut hasher,
            &mut cumulative_tokens,
            &mut checkpoints,
            &mut breakpoints,
            s,
            None,
        ),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                let text = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                feed(
                    &mut hasher,
                    &mut cumulative_tokens,
                    &mut checkpoints,
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
                &mut checkpoints,
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
                Some(Value::String(s)) => feed(
                    &mut hasher,
                    &mut cumulative_tokens,
                    &mut checkpoints,
                    &mut breakpoints,
                    s,
                    None,
                ),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        let text = block_text(b);
                        feed(
                            &mut hasher,
                            &mut cumulative_tokens,
                            &mut checkpoints,
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

    CachePlan {
        checkpoints,
        breakpoints,
    }
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

/// 仅用于「计费指纹」的规范化：剥掉 Claude Code / cool_api 在请求头部夹带的、
/// 同一会话相邻两轮之间会变化的噪音（git 状态、最近提交、当前日期、环境段等），
/// 让「稳定前缀」逐字节一致，从而跨轮命中合成缓存。
///
/// 重要：本函数**只影响指纹与 token 估算**，绝不改动发往上游的 payload
/// （上游 system 过滤在 translator / prompt_filter，是另一条独立路径）。
/// 它比 prompt_filter 的上游去噪更激进：上游漏删只是多几行噪音（无害），
/// 指纹漏删却会直接打穿缓存命中，故这里宁可多删——确定性删除不影响同一会话的稳定性，
/// 只在「两个不同对话恰好仅在被删行上不同」这种极罕见情形下可能误并（代价仅是多报一点 read）。
pub(crate) fn canonicalize_for_fingerprint(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out: Vec<&str> = Vec::new();
    let mut skip_section = false; // `# Environment` / `# auto memory` 整段
    let mut in_commits = false; // `Recent commits:` 之后的逐条提交行
    let mut in_status = false; // git `Status:` 之后的文件清单
    for line in text.lines() {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();

        // 任何新标题/新标记都终结正在跳过的 commits / status 自由文本块（防止它们吞掉后续正文）。
        if trimmed.starts_with("# ")
            || trimmed.starts_with("Recent commits:")
            || trimmed == "Status:"
            || is_volatile_line(trimmed, &lower)
        {
            in_commits = false;
            in_status = false;
        }

        // `# Environment` / `# auto memory` 整段跳过（到下一个 "# " 标题为止）。
        if trimmed == "# Environment" || trimmed == "# auto memory" {
            skip_section = true;
            continue;
        }
        if skip_section {
            if trimmed.starts_with("# ") {
                skip_section = false; // 保留这个新标题
            } else {
                continue;
            }
        }

        // git `Status:` 标题及其后的文件清单（到空行为止）整体跳过——文件状态随编辑而变。
        if trimmed == "Status:" {
            in_status = true;
            continue;
        }
        if in_status {
            if trimmed.is_empty() {
                in_status = false;
            }
            continue;
        }

        // `Recent commits:` 标题及其后逐条提交行（`<7-40 hex> <msg>`）整体跳过。
        if trimmed.starts_with("Recent commits:") {
            in_commits = true;
            continue;
        }
        if in_commits {
            if trimmed.is_empty() || looks_like_commit_line(trimmed) {
                continue;
            }
            in_commits = false; // 提交块结束，落下去正常处理本行
        }

        if is_volatile_line(trimmed, &lower) {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// 判断一行是否为「同一会话相邻两轮间会变」的噪音行（与 prompt_filter 对齐并扩展）。
fn is_volatile_line(trimmed: &str, lower: &str) -> bool {
    const VOLATILE_PREFIXES: &[&str] = &[
        "gitStatus:",
        "Current branch:",
        "Main branch",
        "Git user:",
        "Today's date is",
        "# currentDate",
        "Assistant knowledge cutoff",
        "x-anthropic-billing-header:",
        "<fast_mode_info>",
        "</fast_mode_info>",
    ];
    const VOLATILE_CONTAINS: &[&str] = &[
        ".claude/projects/",
        "git status at the start of the conversation",
        "has been invoked in the following environment",
        "powered by the model named",
    ];
    if VOLATILE_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return true;
    }
    if VOLATILE_CONTAINS.iter().any(|p| trimmed.contains(p)) {
        return true;
    }
    lower.contains("you are claude code")
}

/// 形如 `<7-40 位十六进制> <消息>` 的 git 提交行（`Recent commits:` 之后逐条提交）。
fn looks_like_commit_line(trimmed: &str) -> bool {
    let mut it = trimmed.splitn(2, char::is_whitespace);
    let first = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("");
    !rest.is_empty()
        && (7..=40).contains(&first.len())
        && first.bytes().all(|b| b.is_ascii_hexdigit())
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

    // ===== 多轮缓存命中场景的辅助构造 =====
    fn user_msg(text: &str, cc: bool) -> Value {
        let mut block = json!({"type":"text","text":text});
        if cc {
            block["cache_control"] = json!({"type":"ephemeral","ttl":"5m"});
        }
        json!({"role":"user","content":[block]})
    }
    fn asst_msg(text: &str) -> Value {
        json!({"role":"assistant","content":[{"type":"text","text":text}]})
    }
    fn tool() -> Value {
        json!({"name":"Read","description":"reads a file","input_schema":{"type":"object"}})
    }

    /// 忠实复现真实多轮 Claude Code：system / tools **不带** cache_control，
    /// 只有「最后一条 user」带 cache_control，断点随对话往前滑动。
    /// 这正是 cool_api 把断点收敛成单个滑动断点后的样子。
    #[test]
    fn repro_single_moving_breakpoint() {
        let store = PromptCacheStore::new(256);
        let s = "S".repeat(900); // 稳定 system，但**不带** cache_control
        let total = 100_000i64;

        // 构造第 n 轮：messages 累积，仅最后一条 user 带 cc；system/tools 不带 cc。
        let turn = |msgs: Vec<Value>| {
            json!({
                "model":"claude-opus-4-8",
                "system":[ json!({"type":"text","text": s}) ],
                "tools":[ tool() ],
                "messages": msgs,
            })
        };

        let p1 = build_plan(&turn(vec![user_msg("hello-1", true)]));
        let h1 = store.lookup_and_record(&p1);
        let sp1 = split_usage(total, h1, &p1);
        // 首轮无历史 → 全 creation、无 read。
        assert_eq!(sp1.cache_read, 0, "turn1 should have no read");
        assert!(sp1.cache_creation > 0, "turn1 should write cache");

        let p2 = build_plan(&turn(vec![
            user_msg("hello-1", false),
            asst_msg("hi-1"),
            user_msg("hello-2", true),
        ]));
        let h2 = store.lookup_and_record(&p2);
        let sp2 = split_usage(total, h2, &p2);
        // 修复关键：第 2 轮即便只有滑动断点，也应读到上一轮缓存过的前缀（到 hello-1 为止）。
        assert!(
            sp2.cache_read > 0,
            "turn2 must read the prefix cached on turn1, got read={}",
            sp2.cache_read
        );

        let p3 = build_plan(&turn(vec![
            user_msg("hello-1", false),
            asst_msg("hi-1"),
            user_msg("hello-2", false),
            asst_msg("hi-2"),
            user_msg("hello-3", true),
        ]));
        let h3 = store.lookup_and_record(&p3);
        let sp3 = split_usage(total, h3, &p3);
        // 读量随对话增长（命中到上一轮 hello-2 为止的更长前缀）。
        assert!(
            sp3.cache_read >= sp2.cache_read,
            "turn3 read should not shrink: turn2={} turn3={}",
            sp2.cache_read,
            sp3.cache_read
        );
        // 三者之和恒等于上游真实总量。
        assert_eq!(sp3.cache_read + sp3.cache_creation + sp3.fresh_input, total);
    }

    /// 根因回归：system 第一块头部每轮变化（git 状态 / commit 列表 / 当前日期），
    /// 但稳定正文与历史问答不变。规范化后，第 2 轮起应命中上一轮缓存的前缀。
    /// 改造前（指纹喂原始 raw）此用例必然 read=0；改造后应 read>0。
    #[test]
    fn volatile_header_still_hits() {
        let store = PromptCacheStore::new(256);
        let stable = "S".repeat(900); // 稳定 system 正文
        let total = 100_000i64;

        // 头部噪音逐轮变：commit 短哈希、文件状态、当前日期都不同。
        let sys = |commit: &str, date: &str, status: &str| {
            format!(
                "You are Claude Code, Anthropic's official CLI for Claude.\n\
                 Current branch: main\n\
                 Status:\n{status}\n\
                 Recent commits:\n{commit} kiro: tweak\n\
                 # currentDate\nToday's date is {date}\n\n{stable}"
            )
        };
        let turn = |sys_text: String, msgs: Vec<Value>| {
            json!({
                "model":"claude-opus-4-8",
                "system":[ json!({"type":"text","text": sys_text}) ],
                "tools":[ tool() ],
                "messages": msgs,
            })
        };

        let p1 = build_plan(&turn(
            sys("aaaaaaa", "2026/06/06", " M src/a.rs"),
            vec![user_msg("hello-1", true)],
        ));
        let h1 = store.lookup_and_record(&p1);
        let sp1 = split_usage(total, h1, &p1);
        assert_eq!(sp1.cache_read, 0, "首轮无历史应全 creation");
        assert!(sp1.cache_creation > 0);

        // 第 2 轮：头部三处全变，但稳定正文 + hello-1 不变。
        let p2 = build_plan(&turn(
            sys("bbbbbbb", "2026/06/07", " M src/b.rs\n M src/c.rs"),
            vec![
                user_msg("hello-1", false),
                asst_msg("hi-1"),
                user_msg("hello-2", true),
            ],
        ));
        let h2 = store.lookup_and_record(&p2);
        let sp2 = split_usage(total, h2, &p2);
        assert!(
            sp2.cache_read > 0,
            "规范化后第 2 轮应命中上一轮缓存前缀, got read={}",
            sp2.cache_read
        );
        assert_eq!(sp2.cache_read + sp2.cache_creation + sp2.fresh_input, total);
    }

    /// 根因回归：缓存前缀的**估算**远大于上游**真实** total_input 时（CC 大 system + 3 字节/token
    /// 高估 + 上游过滤），旧实现会把第 2 轮的 cache_creation 挤成 0、read 顶满。等比缩放后，
    /// 第 2 轮应如实把「本轮新增 delta」记成 creation（>0），而非全部当 read。
    #[test]
    fn delta_is_billed_as_creation_not_swallowed() {
        let store = PromptCacheStore::new(256);
        // 大而稳定的 system（不带 cc），断点滑动在最后一条 user 上。
        let sys = "S".repeat(9000); // ~3000 估算 token
        // 上游真实总量远小于估算（模拟过滤 + 高估）。
        let total = 2_000i64;

        let turn = |msgs: Vec<Value>| {
            json!({
                "model":"claude-opus-4-8",
                "system":[ json!({"type":"text","text": sys}) ],
                "messages": msgs,
            })
        };

        // 第 1 轮：仅 user1 带 cc。
        let p1 = build_plan(&turn(vec![user_msg(&"U".repeat(3000), true)]));
        let h1 = store.lookup_and_record(&p1);
        let sp1 = split_usage(total, h1, &p1);
        assert_eq!(sp1.cache_read, 0, "首轮无历史应全 creation");
        assert!(sp1.cache_creation > 0);
        assert_eq!(sp1.cache_read + sp1.cache_creation + sp1.fresh_input, total);

        // 第 2 轮：user1 转为历史（无 cc），新增 assistant + user2（cc 滑到 user2）。
        let p2 = build_plan(&turn(vec![
            user_msg(&"U".repeat(3000), false),
            asst_msg(&"A".repeat(3000)),
            user_msg(&"U2".repeat(1500), true),
        ]));
        let h2 = store.lookup_and_record(&p2);
        let sp2 = split_usage(total, h2, &p2);
        // 关键断言：旧实现这里 creation==0；新实现应 >0（本轮新增的 assistant+user2 计入 creation）。
        assert!(
            sp2.cache_creation > 0,
            "本轮 delta 应记成 creation，不应被高估的 read 吃掉，got creation={}",
            sp2.cache_creation
        );
        // 既要读到上一轮缓存的前缀，也要保持总量守恒。
        assert!(sp2.cache_read > 0, "应读到上一轮缓存前缀");
        assert_eq!(sp2.cache_read + sp2.cache_creation + sp2.fresh_input, total);
    }

    /// 规范化对「仅头部噪音不同」的两段文本应产出**逐字节相同**的结果，且保留正文。
    #[test]
    fn canonicalize_strips_volatile_and_is_deterministic() {
        let a = canonicalize_for_fingerprint(
            "keep this\n\
             gitStatus: dirty\n\
             Current branch: main\n\
             Status:\n M foo.rs\n M bar.rs\n\
             Recent commits:\n225926a kiro: x\n09e1358 kiro: y\n\
             # currentDate\nToday's date is 2026/06/07\n\
             also keep",
        );
        let b = canonicalize_for_fingerprint(
            "keep this\n\
             gitStatus: clean\n\
             Current branch: feature/z\n\
             Status:\n(clean)\n\
             Recent commits:\ndeadbee kiro: p\n\
             # currentDate\nToday's date is 2026/06/08\n\
             also keep",
        );
        assert_eq!(a, b, "仅头部噪音不同 → 规范化后应一致");
        assert!(a.contains("keep this") && a.contains("also keep"));
        assert!(!a.contains("gitStatus"));
        assert!(!a.contains("Current branch"));
        assert!(!a.contains("Today's date"));
        assert!(!a.contains("foo.rs") && !a.contains("225926a"));
    }
}
