//! 透明上下文压缩：在发往 Kiro 上游之前，把过大的 Anthropic 请求压回上游上下文上限内。
//!
//! 背景：Kiro / CodeWhisperer 的 `generateAssistantResponse` 对单次输入有**服务端硬上限**，
//! 超限直接被拒（"Input is too long"）。该上限改不了，所以只能在代理侧减少实际上送的内容。
//!
//! 策略（按「性价比从低到高」依次施加，够了就停）：
//!  1. **截断超大 `tool_result` 块**——读大文件那种，单块最大、冗余最高，先砍这里通常就够；
//!  2. **丢弃最旧的历史消息**——保留 system / tools（顶层，必要且稳定）+ 最近若干轮 + 本轮。
//!
//! 重要约束：
//!  - 只在**估算超过阈值**时才动手；未超限的请求逐字节不变（不影响合成缓存命中）；
//!  - 有损：模型看不到被截断/丢弃的旧上下文，长会话回答质量会下降——这是「不报错」的代价；
//!  - 丢消息时跳过会让 `tool_result` 失去其 `tool_use` 配对的「孤儿」前缀，避免破坏对话结构；
//!  - system / tools 不动（动了会改变模型行为）；若它们本身就超限，压缩无能为力，
//!    交由上层的「输入超长 → 干净 400」止血逻辑兜底。

use serde_json::{json, Value};

/// 估算 token 用的字节/词元比（与项目其它估算口径一致：约 3 字节/token）。
const BYTES_PER_TOKEN: usize = 3;

/// 压缩参数（由 `KiroConfig` 映射而来）。
#[derive(Debug, Clone, Copy)]
pub struct CompactConfig {
    pub enabled: bool,
    /// 触发压缩的输入 token 阈值（估算）。
    pub threshold_tokens: u32,
    /// 单个 tool_result 块允许的最大 token，超出截断保留头部。
    pub tool_result_max_tokens: u32,
    /// 至少保留最近多少轮对话（1 轮≈2 条消息）。
    pub keep_recent_turns: u32,
}

/// 压缩结果（诊断用，写日志）。
#[derive(Debug, Clone, Copy, Default)]
pub struct CompactOutcome {
    /// 是否实际改动了请求。
    pub applied: bool,
    pub before_tokens: u32,
    pub after_tokens: u32,
    /// 被截断的 tool_result 块数。
    pub truncated_tool_results: usize,
    /// 被丢弃的历史消息条数。
    pub dropped_messages: usize,
}

/// 估算整个请求（system + tools + messages 的文本）token 数。诊断 / 阈值判断用。
pub fn estimate_request_tokens(raw: &Value) -> u32 {
    let mut total = 0usize;
    // system：字符串或块数组
    match raw.get("system") {
        Some(Value::String(s)) => total += s.len(),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                total += b.get("text").and_then(|v| v.as_str()).map_or(0, str::len);
            }
        }
        _ => {}
    }
    // tools：name + description + schema
    if let Some(tools) = raw.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            total += t.get("name").and_then(|v| v.as_str()).map_or(0, str::len);
            total += t
                .get("description")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            total += t.get("input_schema").map_or(0, |v| v.to_string().len());
        }
    }
    // messages：逐块文本
    if let Some(msgs) = raw.get("messages").and_then(|v| v.as_array()) {
        for m in msgs {
            total += message_text_len(m);
        }
    }
    (total / BYTES_PER_TOKEN) as u32
}

/// 一条 message 的文本字节量（用于估算）。
fn message_text_len(m: &Value) -> usize {
    match m.get("content") {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(blocks)) => blocks.iter().map(block_text_len).sum(),
        _ => 0,
    }
}

/// 一个 content 块的文本字节量。
fn block_text_len(b: &Value) -> usize {
    match b.get("type").and_then(|v| v.as_str()) {
        Some("text") => b.get("text").and_then(|v| v.as_str()).map_or(0, str::len),
        Some("tool_result") => tool_result_text(b).len(),
        Some("tool_use") => b.get("input").map_or(0, |v| v.to_string().len()),
        _ => 0,
    }
}

/// 取 tool_result 的可读文本（content 可能是字符串或块数组）。
fn tool_result_text(b: &Value) -> String {
    match b.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|x| x.get("text").and_then(|t| t.as_str()).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// 主入口：必要时就地压缩 `raw`，返回做了什么（用于日志）。
pub fn compact_request(raw: &mut Value, cfg: &CompactConfig) -> CompactOutcome {
    let before = estimate_request_tokens(raw);
    let mut out = CompactOutcome {
        before_tokens: before,
        after_tokens: before,
        ..Default::default()
    };
    if !cfg.enabled || before <= cfg.threshold_tokens {
        return out;
    }

    // ① 截断超大 tool_result。
    let result_cap_bytes = cfg.tool_result_max_tokens as usize * BYTES_PER_TOKEN;
    out.truncated_tool_results = truncate_tool_results(raw, result_cap_bytes);
    out.after_tokens = estimate_request_tokens(raw);
    if out.after_tokens <= cfg.threshold_tokens {
        out.applied = out.truncated_tool_results > 0;
        return out;
    }

    // ② 丢弃最旧的历史消息（保留最近 keep_recent_turns*2 条 + 始终保留最后一条）。
    out.dropped_messages = drop_oldest_messages(raw, cfg.threshold_tokens, cfg.keep_recent_turns);
    out.after_tokens = estimate_request_tokens(raw);

    out.applied = out.truncated_tool_results > 0 || out.dropped_messages > 0;
    out
}

/// 截断所有超过 `cap_bytes` 的 tool_result 块（保留头部 + 截断标记）。返回被截断的块数。
fn truncate_tool_results(raw: &mut Value, cap_bytes: usize) -> usize {
    let mut count = 0;
    let Some(msgs) = raw.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return 0;
    };
    for m in msgs.iter_mut() {
        let Some(blocks) = m.get_mut("content").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for b in blocks.iter_mut() {
            if b.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            let text = tool_result_text(b);
            if text.len() <= cap_bytes {
                continue;
            }
            let head = truncate_on_char_boundary(&text, cap_bytes);
            let dropped = text.len() - head.len();
            let marker =
                format!("\n…[cool_proxy 截断 {dropped} 字节以适配 Kiro 上下文上限]");
            // 统一替换成单个 text 块，避免再保留原 content 数组里的巨量数据。
            b["content"] = json!([{ "type": "text", "text": format!("{head}{marker}") }]);
            count += 1;
        }
    }
    count
}

/// 丢弃最旧的历史消息，直到估算回到阈值内或只剩需保留的尾部。返回丢弃条数。
///
/// 保留规则：始终保留**最后一条**（本轮 user）；尽量保留最近 `keep_recent_turns*2` 条；
/// 从最前面开始丢，且丢完后若开头是「孤儿 tool_result / assistant」则继续丢，
/// 直到开头是正常的 user 消息，避免破坏 tool_use↔tool_result 配对。
fn drop_oldest_messages(raw: &mut Value, threshold: u32, keep_recent_turns: u32) -> usize {
    let Some(msgs) = raw.get("messages").and_then(|v| v.as_array()) else {
        return 0;
    };
    let n = msgs.len();
    if n <= 1 {
        return 0;
    }
    let keep_tail = (keep_recent_turns as usize * 2).max(1).min(n - 1);
    // 可丢弃的上界：前 n - keep_tail 条（至少给尾部留 keep_tail 条）。
    let max_drop = n - keep_tail;

    // 用 suffix-sum O(n) 算「丢前 skip 条后的体量」，避免每次试丢都 clone 整个请求。
    let base_bytes = system_tools_bytes(raw); // system + tools 始终计入
    let msg_bytes: Vec<usize> = msgs.iter().map(message_text_len).collect();
    let mut suffix = vec![0usize; n + 1]; // suffix[i] = sum(msg_bytes[i..])
    for i in (0..n).rev() {
        suffix[i] = suffix[i + 1] + msg_bytes[i];
    }
    let est_after_skip =
        |skip: usize| -> u32 { ((base_bytes + suffix[skip]) / BYTES_PER_TOKEN) as u32 };

    // 找到达标所需的最小 drop（达不到则取 max_drop）。
    let mut drop = max_drop;
    for skip in 1..=max_drop {
        if est_after_skip(skip) <= threshold {
            drop = skip;
            break;
        }
    }

    // 跳过开头的孤儿块（首条不应是 tool_result，也尽量让首条是 user）。
    while drop < max_drop && starts_with_orphan(&msgs[drop..]) {
        drop += 1;
    }
    if drop == 0 {
        return 0;
    }

    // 真正执行丢弃。
    if let Some(arr) = raw.get_mut("messages").and_then(|v| v.as_array_mut()) {
        arr.drain(0..drop);
    }
    drop
}

/// system + tools 的文本字节量（丢历史时这部分始终保留，须计入体量）。
fn system_tools_bytes(raw: &Value) -> usize {
    let mut total = 0usize;
    match raw.get("system") {
        Some(Value::String(s)) => total += s.len(),
        Some(Value::Array(blocks)) => {
            for b in blocks {
                total += b.get("text").and_then(|v| v.as_str()).map_or(0, str::len);
            }
        }
        _ => {}
    }
    if let Some(tools) = raw.get("tools").and_then(|v| v.as_array()) {
        for t in tools {
            total += t.get("name").and_then(|v| v.as_str()).map_or(0, str::len);
            total += t
                .get("description")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            total += t.get("input_schema").map_or(0, |v| v.to_string().len());
        }
    }
    total
}

/// 切片开头是否为「孤儿」：首条是 tool_result 块开头（其 tool_use 已被丢掉）或 assistant 回合。
fn starts_with_orphan(msgs: &[Value]) -> bool {
    let Some(first) = msgs.first() else {
        return false;
    };
    let role = first.get("role").and_then(|v| v.as_str()).unwrap_or("");
    if role == "assistant" {
        return true;
    }
    // user 消息但内容以 tool_result 开头 → 其配对 tool_use 在已丢弃的 assistant 里，是孤儿。
    if let Some(Value::Array(blocks)) = first.get("content") {
        if let Some(b0) = blocks.first() {
            return b0.get("type").and_then(|v| v.as_str()) == Some("tool_result");
        }
    }
    false
}

/// 在不超过 `max_bytes` 的前提下，按 UTF-8 字符边界截断（避免切碎多字节字符）。
fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user(text: &str) -> Value {
        json!({"role":"user","content":[{"type":"text","text":text}]})
    }
    fn asst(text: &str) -> Value {
        json!({"role":"assistant","content":[{"type":"text","text":text}]})
    }
    fn tool_result(id: &str, text: &str) -> Value {
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":id,"content":[{"type":"text","text":text}]}
        ]})
    }

    fn cfg() -> CompactConfig {
        CompactConfig {
            enabled: true,
            threshold_tokens: 1000,
            tool_result_max_tokens: 100, // 300 字节
            keep_recent_turns: 2,
        }
    }

    #[test]
    fn under_threshold_is_untouched() {
        let mut raw = json!({
            "model":"m",
            "system":"sys",
            "messages":[ user("hello"), asst("hi") ]
        });
        let before = raw.clone();
        let out = compact_request(&mut raw, &cfg());
        assert!(!out.applied);
        assert_eq!(raw, before, "未超阈值应逐字节不变");
    }

    #[test]
    fn truncates_oversized_tool_result() {
        let big = "X".repeat(9000); // 远超 300 字节上限
        let mut raw = json!({
            "model":"m",
            "system":"s",
            "messages":[ user("q"), tool_result("t1", &big), user("again") ]
        });
        let out = compact_request(&mut raw, &cfg());
        assert!(out.applied);
        assert_eq!(out.truncated_tool_results, 1);
        // 截断后整体应明显变小
        assert!(out.after_tokens < out.before_tokens);
        // tool_result 文本应带截断标记
        let tr = &raw["messages"][1]["content"][0]["content"][0]["text"];
        assert!(tr.as_str().unwrap().contains("cool_proxy 截断"));
    }

    #[test]
    fn drops_oldest_when_still_too_big() {
        // 构造很多轮，每轮文本不大但累计超阈值；tool_result 截断不足以达标 → 丢旧消息。
        let mut msgs = Vec::new();
        for i in 0..40 {
            msgs.push(user(&format!("user-{i}-{}", "u".repeat(200))));
            msgs.push(asst(&format!("asst-{i}-{}", "a".repeat(200))));
        }
        let mut raw = json!({"model":"m","system":"s","messages": msgs});
        let before = estimate_request_tokens(&raw);
        assert!(before > 1000, "前置条件：应超阈值");
        let out = compact_request(&mut raw, &cfg());
        assert!(out.applied);
        assert!(out.dropped_messages > 0, "应丢弃最旧消息");
        assert!(out.after_tokens <= 1000 || raw["messages"].as_array().unwrap().len() <= 4);
        // 始终保留最后一条（本例最后一条是 asst-39）
        let last = raw["messages"].as_array().unwrap().last().unwrap();
        assert!(last["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("asst-39"));
    }

    #[test]
    fn disabled_does_nothing() {
        let big = "X".repeat(9000);
        let mut raw = json!({
            "model":"m","system":"s",
            "messages":[ tool_result("t1", &big) ]
        });
        let before = raw.clone();
        let mut c = cfg();
        c.enabled = false;
        let out = compact_request(&mut raw, &c);
        assert!(!out.applied);
        assert_eq!(raw, before);
    }

    #[test]
    fn does_not_leave_orphan_tool_result_at_front() {
        // well-formed 多轮：user(text) → assistant(tool_use) → user(tool_result)。
        // 丢弃后开头应落在「干净的 user(text)」边界，而非孤儿 tool_result / assistant。
        let asst_tooluse = |i: usize| {
            json!({"role":"assistant","content":[
                {"type":"text","text":format!("a-{i}-{}", "a".repeat(150))},
                {"type":"tool_use","id":format!("t{i}"),"name":"Read","input":{"p":"x"}}
            ]})
        };
        let mut msgs = Vec::new();
        for i in 0..30 {
            msgs.push(user(&format!("u-{i}-{}", "u".repeat(150))));
            msgs.push(asst_tooluse(i));
            msgs.push(tool_result(&format!("t{i}"), &format!("r-{i}-{}", "r".repeat(150))));
        }
        msgs.push(user("final"));
        let mut raw = json!({"model":"m","system":"s","messages": msgs});
        let out = compact_request(&mut raw, &cfg());
        assert!(out.dropped_messages > 0);
        let arr = raw["messages"].as_array().unwrap();
        let first = &arr[0];
        let role = first["role"].as_str().unwrap();
        let t0 = first["content"][0]["type"].as_str().unwrap_or("");
        let clean = role == "user" && t0 != "tool_result";
        // 开头要么是干净 user，要么已压到尾部保持下界（keep_recent_turns*2=4）。
        assert!(
            clean || arr.len() <= 4,
            "开头应是干净 user 或已到下界；role={role} t0={t0} len={}",
            arr.len()
        );
    }
}
