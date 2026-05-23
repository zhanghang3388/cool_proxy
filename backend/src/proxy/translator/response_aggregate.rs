use std::collections::HashMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::response_stream::{mapped_usage, mime_from_codex_format};
use super::tool_names::build_reverse_map_from_openai;

/// 非流式聚合器：把 codex 的 SSE 流喂进来，结束后调用 `finalize` 拿到一个完整的
/// `chat.completion` 对象（OpenAI 非流式形状）。
pub struct Aggregator {
    response_id: String,
    created_at: i64,
    model: String,
    text: String,
    reasoning: String,
    /// 当前正在累积 arguments 的 tool_call 索引 → (call_id, name_restored, arguments_acc)
    tool_calls: Vec<ToolCallAcc>,
    cur_function_call_index: i64,
    /// 缩名 → 原名
    short_name_to_orig: HashMap<String, String>,
    last_usage: Option<Value>,
    completed: bool,
    seen_images: HashMap<String, [u8; 32]>,
    images: Vec<Value>,
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    arguments: String,
    has_delta: bool,
}

impl Aggregator {
    pub fn new(default_model: &str, original_openai_body: &[u8]) -> Self {
        Self {
            response_id: String::new(),
            created_at: 0,
            model: default_model.to_string(),
            text: String::new(),
            reasoning: String::new(),
            tool_calls: Vec::new(),
            cur_function_call_index: -1,
            short_name_to_orig: build_reverse_map_from_openai(original_openai_body),
            last_usage: None,
            completed: false,
            seen_images: HashMap::new(),
            images: Vec::new(),
        }
    }

    pub fn push(&mut self, codex_event: &Value) {
        let event_type = codex_event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if event_type == "response.created" {
            if let Some(resp) = codex_event.get("response") {
                if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                    self.response_id = id.to_string();
                }
                if let Some(t) = resp.get("created_at").and_then(|v| v.as_i64()) {
                    self.created_at = t;
                }
                if let Some(m) = resp.get("model").and_then(|v| v.as_str()) {
                    self.model = m.to_string();
                }
            }
            return;
        }

        if let Some(u) = codex_event
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| codex_event.get("usage"))
        {
            self.last_usage = Some(u.clone());
        }

        if let Some(m) = codex_event.get("model").and_then(|v| v.as_str()) {
            self.model = m.to_string();
        }

        match event_type {
            "response.output_text.delta" => {
                if let Some(d) = codex_event.get("delta").and_then(|v| v.as_str()) {
                    self.text.push_str(d);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some(d) = codex_event.get("delta").and_then(|v| v.as_str()) {
                    self.reasoning.push_str(d);
                }
            }
            "response.reasoning_summary_text.done" => {
                self.reasoning.push_str("\n\n");
            }
            "response.output_item.added" => {
                let is_fc = codex_event
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("function_call");
                if !is_fc {
                    return;
                }
                self.cur_function_call_index += 1;
                let item = codex_event.get("item");
                let call_id = item
                    .and_then(|i| i.get("call_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name_short = item
                    .and_then(|i| i.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name_restored = self
                    .short_name_to_orig
                    .get(&name_short)
                    .cloned()
                    .unwrap_or(name_short);
                self.tool_calls.push(ToolCallAcc {
                    id: call_id,
                    name: name_restored,
                    arguments: String::new(),
                    has_delta: false,
                });
            }
            "response.function_call_arguments.delta" => {
                if let Some(d) = codex_event.get("delta").and_then(|v| v.as_str()) {
                    if let Some(idx) = self.cur_index() {
                        let acc = &mut self.tool_calls[idx];
                        acc.arguments.push_str(d);
                        acc.has_delta = true;
                    }
                }
            }
            "response.function_call_arguments.done" => {
                if let Some(idx) = self.cur_index() {
                    let acc = &mut self.tool_calls[idx];
                    if !acc.has_delta {
                        if let Some(args) = codex_event.get("arguments").and_then(|v| v.as_str()) {
                            acc.arguments.push_str(args);
                        }
                    }
                }
            }
            "response.image_generation_call.partial_image" => {
                let item_id = codex_event
                    .get("item_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let b64 = codex_event
                    .get("partial_image_b64")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if b64.is_empty() {
                    return;
                }
                if !item_id.is_empty() && self.image_seen(&item_id, b64) {
                    return;
                }
                let mime = mime_from_codex_format(
                    codex_event
                        .get("output_format")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                );
                self.images.push(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime};base64,{b64}")},
                }));
            }
            "response.output_item.done" => {
                let item = codex_event.get("item");
                let item_type = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if item_type != "image_generation_call" {
                    return;
                }
                let item_id = item
                    .and_then(|i| i.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let b64 = item
                    .and_then(|i| i.get("result"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if b64.is_empty() {
                    return;
                }
                if !item_id.is_empty() && self.image_seen(&item_id, b64) {
                    return;
                }
                let mime = mime_from_codex_format(
                    item.and_then(|i| i.get("output_format"))
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                );
                self.images.push(json!({
                    "type": "image_url",
                    "image_url": {"url": format!("data:{mime};base64,{b64}")},
                }));
            }
            "response.completed" => {
                self.completed = true;
            }
            _ => {}
        }
    }

    fn image_seen(&mut self, item_id: &str, b64: &str) -> bool {
        let mut h = Sha256::new();
        h.update(b64.as_bytes());
        let digest: [u8; 32] = h.finalize().into();
        if let Some(prev) = self.seen_images.get(item_id) {
            if *prev == digest {
                return true;
            }
        }
        self.seen_images.insert(item_id.to_string(), digest);
        false
    }

    fn cur_index(&self) -> Option<usize> {
        if self.cur_function_call_index < 0 {
            None
        } else {
            Some(self.cur_function_call_index as usize)
        }
    }

    /// 即使没收到 `response.completed` 也能调（流被截断时降级）。
    pub fn finalize(self) -> Value {
        let finish_reason = if !self.tool_calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        };

        let content = if self.text.is_empty() && !self.tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(self.text)
        };
        let reasoning_content = if self.reasoning.is_empty() {
            Value::Null
        } else {
            Value::String(self.reasoning)
        };
        let tool_calls = if self.tool_calls.is_empty() {
            Value::Null
        } else {
            let arr: Vec<Value> = self
                .tool_calls
                .into_iter()
                .map(|tc| {
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                })
                .collect();
            Value::Array(arr)
        };
        let images = if self.images.is_empty() {
            Value::Null
        } else {
            Value::Array(self.images)
        };

        let mut out = json!({
            "id": self.response_id,
            "object": "chat.completion",
            "created": self.created_at,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content,
                    "reasoning_content": reasoning_content,
                    "tool_calls": tool_calls,
                    "images": images,
                },
                "finish_reason": finish_reason,
                "native_finish_reason": finish_reason,
            }],
        });
        if let Some(u) = self.last_usage.as_ref() {
            out["usage"] = mapped_usage(u);
        }
        out
    }

    pub fn is_completed(&self) -> bool {
        self.completed
    }
}
