use std::collections::HashMap;

use serde_json::{json, Value};

use super::tool_names::build_reverse_map_from_openai;

/// 流式翻译器。每收到一行 codex SSE（已剥掉 `data: ` 前缀的 JSON）调用一次 `push`，
/// 返回 0 或多个待写给客户端的 OpenAI ChatCompletion chunk JSON（不含 `data: ` / `\n\n` 包装，
/// 调用方自己加 SSE framing）。
pub struct StreamTranslator {
    response_id: String,
    created_at: i64,
    model: String,
    function_call_index: i64,
    has_received_args_delta: bool,
    short_name_to_orig: HashMap<String, String>,
    /// 累积的 usage（codex 在多个事件里都可能带 response.usage，我们以最后一次为准）
    last_usage: Option<Value>,
}

impl StreamTranslator {
    pub fn new(default_model: &str, original_openai_body: &[u8]) -> Self {
        Self {
            response_id: String::new(),
            created_at: 0,
            model: default_model.to_string(),
            function_call_index: -1,
            has_received_args_delta: false,
            short_name_to_orig: build_reverse_map_from_openai(original_openai_body),
            last_usage: None,
        }
    }

    #[allow(dead_code)]
    pub fn response_id(&self) -> &str {
        &self.response_id
    }

    #[allow(dead_code)]
    pub fn last_usage(&self) -> Option<&Value> {
        self.last_usage.as_ref()
    }

    /// 喂一条 codex 事件 JSON。返回需要发给客户端的 chunk（已是 OpenAI chat.completion.chunk 形状）。
    pub fn push(&mut self, codex_event: &Value) -> Vec<Value> {
        let event_type = codex_event
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // response.created：仅记录，不输出
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
            return Vec::new();
        }

        // 累积 usage（在事件 root 或 response.usage 里）
        if let Some(u) = codex_event
            .get("response")
            .and_then(|r| r.get("usage"))
            .or_else(|| codex_event.get("usage"))
        {
            self.last_usage = Some(u.clone());
        }

        // 取/更新 model（流中部分事件会带 model 字段）
        if let Some(m) = codex_event.get("model").and_then(|v| v.as_str()) {
            self.model = m.to_string();
        }

        match event_type {
            "response.output_text.delta" => {
                let delta = codex_event
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut chunk = self.empty_chunk();
                let choices = chunk["choices"].as_array_mut().unwrap();
                choices[0]["delta"] = json!({
                    "role": "assistant",
                    "content": delta,
                });
                vec![chunk]
            }
            "response.reasoning_summary_text.delta" => {
                let delta = codex_event
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({
                    "role": "assistant",
                    "reasoning_content": delta,
                });
                vec![chunk]
            }
            "response.reasoning_summary_text.done" => {
                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({
                    "role": "assistant",
                    "reasoning_content": "\n\n",
                });
                vec![chunk]
            }
            "response.output_item.added" => {
                let item = codex_event.get("item");
                let is_fc = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    == Some("function_call");
                if !is_fc {
                    return Vec::new();
                }
                self.function_call_index += 1;
                self.has_received_args_delta = false;

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

                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({
                    "role": "assistant",
                    "tool_calls": [{
                        "index": self.function_call_index,
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name_restored,
                            "arguments": "",
                        }
                    }]
                });
                vec![chunk]
            }
            "response.function_call_arguments.delta" => {
                self.has_received_args_delta = true;
                let delta = codex_event
                    .get("delta")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({
                    "tool_calls": [{
                        "index": self.function_call_index,
                        "function": { "arguments": delta },
                    }]
                });
                vec![chunk]
            }
            "response.function_call_arguments.done" => {
                if self.has_received_args_delta {
                    return Vec::new();
                }
                let full = codex_event
                    .get("arguments")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({
                    "tool_calls": [{
                        "index": self.function_call_index,
                        "function": { "arguments": full },
                    }]
                });
                vec![chunk]
            }
            "response.completed" => {
                let finish = if self.function_call_index != -1 {
                    "tool_calls"
                } else {
                    "stop"
                };
                let mut chunk = self.empty_chunk();
                chunk["choices"][0]["delta"] = json!({});
                chunk["choices"][0]["finish_reason"] = Value::String(finish.into());
                chunk["choices"][0]["native_finish_reason"] = Value::String(finish.into());
                if let Some(u) = self.last_usage.clone() {
                    chunk["usage"] = mapped_usage(&u);
                }
                vec![chunk]
            }
            _ => Vec::new(),
        }
    }

    fn empty_chunk(&self) -> Value {
        json!({
            "id": self.response_id,
            "object": "chat.completion.chunk",
            "created": self.created_at,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": null,
                "native_finish_reason": null,
            }]
        })
    }
}

/// 把 codex 风格 usage 映射成 OpenAI 风格。
pub fn mapped_usage(codex_usage: &Value) -> Value {
    let mut out = serde_json::Map::new();
    if let Some(v) = codex_usage.get("input_tokens").and_then(|x| x.as_i64()) {
        out.insert("prompt_tokens".into(), json!(v));
    }
    if let Some(v) = codex_usage.get("output_tokens").and_then(|x| x.as_i64()) {
        out.insert("completion_tokens".into(), json!(v));
    }
    if let Some(v) = codex_usage.get("total_tokens").and_then(|x| x.as_i64()) {
        out.insert("total_tokens".into(), json!(v));
    }
    if let Some(v) = codex_usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|x| x.as_i64())
    {
        out.insert(
            "prompt_tokens_details".into(),
            json!({ "cached_tokens": v }),
        );
    }
    if let Some(v) = codex_usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|x| x.as_i64())
    {
        out.insert(
            "completion_tokens_details".into(),
            json!({ "reasoning_tokens": v }),
        );
    }
    Value::Object(out)
}
