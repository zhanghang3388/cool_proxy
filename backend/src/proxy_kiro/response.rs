//! Kiro 事件 → Anthropic Messages 输出。
//!
//! 上游 event-stream 解析出的帧（[`EventFrame`]）先经 [`KiroEventProcessor`] 归一成
//! 一串 [`KiroEvent`]（文本增量 / thinking / tool_use / usage），再由：
//! - [`ClaudeSseEncoder`]：转成 Anthropic SSE 事件流（流式）；
//! - [`aggregate`]：聚合成单个 Anthropic message JSON（非流式）。

use serde_json::{json, Value};
use uuid::Uuid;

use super::eventstream::EventFrame;

/// 归一化后的语义事件。
#[derive(Debug, Clone)]
pub enum KiroEvent {
    /// 普通文本增量。
    Text(String),
    /// thinking（reasoning）增量。
    Thinking { text: String, signature: Option<String> },
    /// 加密的 thinking 块。
    RedactedThinking(String),
    /// 一次完整的工具调用（input 已解析）。
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

/// token 用量累积。
#[derive(Debug, Clone, Default)]
pub struct KiroUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

struct PendingToolUse {
    id: String,
    name: String,
    input_buffer: String,
}

/// 消费 event-stream 帧，吐出归一事件，并累积 usage。
#[derive(Default)]
pub struct KiroEventProcessor {
    current_tool: Option<PendingToolUse>,
    processed_ids: std::collections::HashSet<String>,
    pub usage: KiroUsage,
    /// 累积输出文本字符数（上游不给 outputTokens 时兜底估算）。
    output_chars: usize,
}

impl KiroEventProcessor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 处理一帧，返回该帧产生的归一事件（可能为空）。
    pub fn process(&mut self, frame: &EventFrame) -> Vec<KiroEvent> {
        let mut out = Vec::new();
        let event: Value = match serde_json::from_slice(&frame.payload) {
            Ok(v) => v,
            Err(_) => return out,
        };
        let et = frame.event_type.as_str();

        // assistantResponseEvent / codeEvent：文本增量
        if et == "assistantResponseEvent" || event.get("assistantResponseEvent").is_some() {
            let resp = event.get("assistantResponseEvent").unwrap_or(&event);
            if let Some(c) = resp.get("content").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    self.output_chars += c.len();
                    out.push(KiroEvent::Text(c.to_string()));
                }
            }
        }
        if et == "codeEvent" || event.get("codeEvent").is_some() {
            let resp = event.get("codeEvent").unwrap_or(&event);
            if let Some(c) = resp.get("content").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    self.output_chars += c.len();
                    out.push(KiroEvent::Text(c.to_string()));
                }
            }
        }

        // reasoningContentEvent：thinking
        if et == "reasoningContentEvent" || event.get("reasoningContentEvent").is_some() {
            let r = event.get("reasoningContentEvent").unwrap_or(&event);
            if let Some(t) = r.get("text").and_then(|v| v.as_str()) {
                if !t.is_empty() {
                    let sig = r
                        .get("signature")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    out.push(KiroEvent::Thinking {
                        text: t.to_string(),
                        signature: sig,
                    });
                }
            }
            if let Some(red) = r.get("redactedContent").and_then(|v| v.as_str()) {
                if !red.is_empty() {
                    out.push(KiroEvent::RedactedThinking(red.to_string()));
                }
            }
        }

        // toolUseEvent：可能分片，累积到 stop
        if et == "toolUseEvent" || event.get("toolUseEvent").is_some() {
            let t = event.get("toolUseEvent").unwrap_or(&event);
            self.handle_tool_use(t, &mut out);
        }

        // messageMetadataEvent / metadataEvent：token 用量
        if et == "messageMetadataEvent"
            || et == "metadataEvent"
            || event.get("messageMetadataEvent").is_some()
            || event.get("metadataEvent").is_some()
        {
            let m = event
                .get("messageMetadataEvent")
                .or_else(|| event.get("metadataEvent"))
                .unwrap_or(&event);
            if let Some(tu) = m.get("tokenUsage") {
                let uncached = tu.get("uncachedInputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let cache_read = tu.get("cacheReadInputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let cache_write = tu.get("cacheWriteInputTokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let calc_input = uncached + cache_read + cache_write;
                if calc_input > 0 {
                    self.usage.input_tokens = calc_input;
                }
                if let Some(o) = tu.get("outputTokens").and_then(|v| v.as_i64()) {
                    self.usage.output_tokens = o;
                }
                self.usage.cache_read_tokens = cache_read;
                self.usage.cache_write_tokens = cache_write;
            }
        }

        out
    }

    fn handle_tool_use(&mut self, t: &Value, out: &mut Vec<KiroEvent>) {
        let tool_id = t.get("toolUseId").and_then(|v| v.as_str()).unwrap_or("");
        let tool_name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let is_stop = t.get("stop").and_then(|v| v.as_bool()).unwrap_or(false);

        // input 可能是字符串片段或完整对象
        let mut fragment = String::new();
        let mut obj_input: Option<String> = None;
        match t.get("input") {
            Some(Value::String(s)) => fragment = s.clone(),
            Some(v) if v.is_object() => obj_input = Some(v.to_string()),
            _ => {}
        }

        // 新工具开始
        if !tool_id.is_empty() && !tool_name.is_empty() {
            if let Some(cur) = &self.current_tool {
                if cur.id != tool_id && !self.processed_ids.contains(&cur.id) {
                    // 前一个被打断，先收尾
                    if let Some(ev) = self.finish_current() {
                        out.push(ev);
                    }
                }
            }
            if self.current_tool.is_none() && !self.processed_ids.contains(tool_id) {
                self.current_tool = Some(PendingToolUse {
                    id: tool_id.to_string(),
                    name: tool_name.to_string(),
                    input_buffer: String::new(),
                });
            }
        }

        if let Some(cur) = &mut self.current_tool {
            if !fragment.is_empty() {
                cur.input_buffer.push_str(&fragment);
            }
            if let Some(o) = obj_input {
                cur.input_buffer = o;
            }
        }

        if is_stop && self.current_tool.is_some() {
            if let Some(ev) = self.finish_current() {
                out.push(ev);
            }
        }
    }

    fn finish_current(&mut self) -> Option<KiroEvent> {
        let cur = self.current_tool.take()?;
        if self.processed_ids.contains(&cur.id) {
            return None;
        }
        let input = if cur.input_buffer.trim().is_empty() {
            json!({})
        } else {
            match serde_json::from_str::<Value>(&cur.input_buffer) {
                Ok(v) => v,
                Err(_) => json!({
                    "_error": "Tool input truncated by Kiro API (output token limit exceeded)",
                    "_partialInput": cur.input_buffer.chars().take(500).collect::<String>(),
                }),
            }
        };
        self.output_chars += cur.name.len() + cur.input_buffer.len();
        self.processed_ids.insert(cur.id.clone());
        Some(KiroEvent::ToolUse {
            id: cur.id,
            name: cur.name,
            input,
        })
    }

    /// 流结束时调用：收掉未 stop 的工具，并兜底估算 output_tokens。
    pub fn finalize(&mut self) -> Vec<KiroEvent> {
        let mut out = Vec::new();
        if let Some(ev) = self.finish_current() {
            out.push(ev);
        }
        if self.usage.output_tokens == 0 && self.output_chars > 0 {
            self.usage.output_tokens = ((self.output_chars as f64) / 3.5).ceil() as i64;
        }
        out
    }
}

/// 把 Kiro tool 名还原成客户端原始名。
fn restore_name(name: &str, restore: &std::collections::HashMap<String, String>) -> String {
    restore.get(name).cloned().unwrap_or_else(|| name.to_string())
}

// ===== 流式：Anthropic SSE 编码器 =====

/// 维护 content block 索引与开闭状态，逐事件产出 Anthropic SSE 字符串。
pub struct ClaudeSseEncoder {
    message_id: String,
    model: String,
    restore: std::collections::HashMap<String, String>,
    block_index: i64,
    text_open: bool,
    thinking_open: bool,
    started: bool,
    has_tool: bool,
}

impl ClaudeSseEncoder {
    pub fn new(model: String, restore: std::collections::HashMap<String, String>) -> Self {
        Self {
            message_id: format!("msg_{}", Uuid::new_v4().simple()),
            model,
            restore,
            block_index: 0,
            text_open: false,
            thinking_open: false,
            started: false,
            has_tool: false,
        }
    }

    fn sse(event: &str, data: &Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// message_start。
    pub fn start(&mut self, input_tokens_est: i64) -> String {
        self.started = true;
        let data = json!({
            "type": "message_start",
            "message": {
                "id": self.message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": { "input_tokens": input_tokens_est.max(1), "output_tokens": 0 }
            }
        });
        Self::sse("message_start", &data)
    }

    fn close_text(&mut self, out: &mut String) {
        if self.text_open {
            out.push_str(&Self::sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.block_index}),
            ));
            self.block_index += 1;
            self.text_open = false;
        }
    }

    fn close_thinking(&mut self, out: &mut String) {
        if self.thinking_open {
            out.push_str(&Self::sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": self.block_index}),
            ));
            self.block_index += 1;
            self.thinking_open = false;
        }
    }

    /// 处理一个归一事件，返回要发给客户端的 SSE 片段。
    pub fn encode(&mut self, ev: &KiroEvent) -> String {
        let mut out = String::new();
        match ev {
            KiroEvent::Text(t) => {
                self.close_thinking(&mut out);
                if !self.text_open {
                    out.push_str(&Self::sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    ));
                    self.text_open = true;
                }
                out.push_str(&Self::sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "text_delta", "text": t}
                    }),
                ));
            }
            KiroEvent::Thinking { text, signature } => {
                self.close_text(&mut out);
                if !self.thinking_open {
                    out.push_str(&Self::sse(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": self.block_index,
                            "content_block": {"type": "thinking", "thinking": ""}
                        }),
                    ));
                    self.thinking_open = true;
                }
                out.push_str(&Self::sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "thinking_delta", "thinking": text}
                    }),
                ));
                if let Some(sig) = signature {
                    out.push_str(&Self::sse(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": self.block_index,
                            "delta": {"type": "signature_delta", "signature": sig}
                        }),
                    ));
                }
            }
            KiroEvent::RedactedThinking(data) => {
                self.close_text(&mut out);
                self.close_thinking(&mut out);
                out.push_str(&Self::sse(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {"type": "redacted_thinking", "data": data}
                    }),
                ));
                out.push_str(&Self::sse(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": self.block_index}),
                ));
                self.block_index += 1;
            }
            KiroEvent::ToolUse { id, name, input } => {
                self.close_text(&mut out);
                self.close_thinking(&mut out);
                self.has_tool = true;
                let restored = restore_name(name, &self.restore);
                out.push_str(&Self::sse(
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": self.block_index,
                        "content_block": {"type": "tool_use", "id": id, "name": restored, "input": {}}
                    }),
                ));
                out.push_str(&Self::sse(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": self.block_index,
                        "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
                    }),
                ));
                out.push_str(&Self::sse(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": self.block_index}),
                ));
                self.block_index += 1;
            }
        }
        out
    }

    /// 收尾：关闭未闭合的块，发 message_delta + message_stop。
    pub fn finish(&mut self, usage: &KiroUsage) -> String {
        let mut out = String::new();
        self.close_text(&mut out);
        self.close_thinking(&mut out);
        let stop_reason = if self.has_tool { "tool_use" } else { "end_turn" };
        let mut usage_obj = json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
        });
        if usage.cache_read_tokens > 0 {
            usage_obj["cache_read_input_tokens"] = json!(usage.cache_read_tokens);
        }
        if usage.cache_write_tokens > 0 {
            usage_obj["cache_creation_input_tokens"] = json!(usage.cache_write_tokens);
        }
        out.push_str(&Self::sse(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": usage_obj
            }),
        ));
        out.push_str(&Self::sse("message_stop", &json!({"type": "message_stop"})));
        out
    }

    pub fn error(status: u16, message: &str) -> String {
        Self::sse(
            "error",
            &json!({
                "type": "error",
                "error": {"type": "api_error", "message": message, "status": status}
            }),
        )
    }
}

// ===== 非流式：聚合成单个 Anthropic message =====

/// 把全部归一事件聚合成一个 Anthropic message JSON。
pub fn aggregate(
    events: &[KiroEvent],
    usage: &KiroUsage,
    model: &str,
    restore: &std::collections::HashMap<String, String>,
) -> Value {
    let mut content_blocks: Vec<Value> = Vec::new();
    let mut text_acc = String::new();
    let mut has_tool = false;

    let flush_text = |acc: &mut String, blocks: &mut Vec<Value>| {
        if !acc.trim().is_empty() {
            blocks.push(json!({"type": "text", "text": acc.clone()}));
        }
        acc.clear();
    };

    for ev in events {
        match ev {
            KiroEvent::Text(t) => text_acc.push_str(t),
            KiroEvent::Thinking { text, signature } => {
                flush_text(&mut text_acc, &mut content_blocks);
                let mut block = json!({"type": "thinking", "thinking": text});
                if let Some(sig) = signature {
                    block["signature"] = json!(sig);
                }
                content_blocks.push(block);
            }
            KiroEvent::RedactedThinking(data) => {
                flush_text(&mut text_acc, &mut content_blocks);
                content_blocks.push(json!({"type": "redacted_thinking", "data": data}));
            }
            KiroEvent::ToolUse { id, name, input } => {
                flush_text(&mut text_acc, &mut content_blocks);
                has_tool = true;
                content_blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": restore_name(name, restore),
                    "input": input,
                }));
            }
        }
    }
    flush_text(&mut text_acc, &mut content_blocks);

    let stop_reason = if has_tool { "tool_use" } else { "end_turn" };
    let mut usage_obj = json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
    });
    if usage.cache_read_tokens > 0 {
        usage_obj["cache_read_input_tokens"] = json!(usage.cache_read_tokens);
    }
    if usage.cache_write_tokens > 0 {
        usage_obj["cache_creation_input_tokens"] = json!(usage.cache_write_tokens);
    }

    json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content_blocks,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": usage_obj,
    })
}
