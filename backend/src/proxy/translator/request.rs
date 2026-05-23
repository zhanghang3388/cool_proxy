use std::collections::HashMap;

use serde_json::{json, Map, Value};

use super::tool_names::{build_short_name_map, shorten_name_if_needed};

/// 把 OpenAI ChatCompletion 形状的请求体翻译成 codex `/responses` 形状。
/// `model` 由调用方传入（覆盖请求体里的值，方便上层选号后再决定模型）。
/// `stream` 决定 codex 端是否流式（cool_proxy 内部一律 true）。
pub fn translate_request(model: &str, openai_body: &[u8], stream: bool) -> Value {
    let raw: Value =
        serde_json::from_slice(openai_body).unwrap_or_else(|_| Value::Object(Map::new()));

    let mut out = Map::new();
    out.insert("model".into(), Value::String(model.to_string()));
    out.insert("instructions".into(), Value::String(String::new()));
    out.insert("stream".into(), Value::Bool(stream));
    out.insert(
        "include".into(),
        json!(["reasoning.encrypted_content"]),
    );
    out.insert("parallel_tool_calls".into(), Value::Bool(true));
    out.insert("store".into(), Value::Bool(false));

    // reasoning.effort + reasoning.summary
    let mut reasoning = Map::new();
    let effort = raw
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .unwrap_or("medium")
        .to_string();
    reasoning.insert("effort".into(), Value::String(effort));
    reasoning.insert("summary".into(), Value::String("auto".into()));
    out.insert("reasoning".into(), Value::Object(reasoning));

    // tools 短名映射 + tools 数组
    let original_tool_names: Vec<String> = raw
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|t| t.get("type").and_then(|x| x.as_str()) == Some("function"))
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(|s| s.to_string())
                })
                .collect()
        })
        .unwrap_or_default();

    let short_map: HashMap<String, String> = if original_tool_names.is_empty() {
        HashMap::new()
    } else {
        let refs: Vec<&str> = original_tool_names.iter().map(String::as_str).collect();
        build_short_name_map(&refs)
    };

    if let Some(arr) = raw.get("tools").and_then(|v| v.as_array()) {
        let mut translated_tools = Vec::with_capacity(arr.len());
        for t in arr {
            let tool_type = t.get("type").and_then(|x| x.as_str()).unwrap_or("");
            // 内置工具（web_search 等）原样透传
            if !tool_type.is_empty() && tool_type != "function" && t.is_object() {
                translated_tools.push(t.clone());
                continue;
            }
            if tool_type != "function" {
                continue;
            }
            let mut item = Map::new();
            item.insert("type".into(), Value::String("function".into()));
            if let Some(fn_obj) = t.get("function").and_then(|f| f.as_object()) {
                if let Some(name) = fn_obj.get("name").and_then(|v| v.as_str()) {
                    let mapped = short_map
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| shorten_name_if_needed(name));
                    item.insert("name".into(), Value::String(mapped));
                }
                if let Some(desc) = fn_obj.get("description") {
                    item.insert("description".into(), desc.clone());
                }
                if let Some(params) = fn_obj.get("parameters") {
                    item.insert("parameters".into(), params.clone());
                }
                if let Some(strict) = fn_obj.get("strict") {
                    item.insert("strict".into(), strict.clone());
                }
            }
            translated_tools.push(Value::Object(item));
        }
        out.insert("tools".into(), Value::Array(translated_tools));
    }

    // tool_choice：string 直通；对象形式 {type:function, function:{name}} 拍平 + 缩名
    if let Some(tc) = raw.get("tool_choice") {
        match tc {
            Value::String(s) => {
                out.insert("tool_choice".into(), Value::String(s.clone()));
            }
            Value::Object(obj) => {
                let tc_type = obj.get("type").and_then(|x| x.as_str()).unwrap_or("");
                if tc_type == "function" {
                    let name = obj
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mapped = if name.is_empty() {
                        String::new()
                    } else {
                        short_map
                            .get(&name)
                            .cloned()
                            .unwrap_or_else(|| shorten_name_if_needed(&name))
                    };
                    let mut choice = Map::new();
                    choice.insert("type".into(), Value::String("function".into()));
                    if !mapped.is_empty() {
                        choice.insert("name".into(), Value::String(mapped));
                    }
                    out.insert("tool_choice".into(), Value::Object(choice));
                } else if !tc_type.is_empty() {
                    out.insert("tool_choice".into(), Value::Object(obj.clone()));
                }
            }
            _ => {}
        }
    }

    // 转换 messages → input
    let mut input: Vec<Value> = Vec::new();
    if let Some(messages) = raw.get("messages").and_then(|v| v.as_array()) {
        for m in messages {
            let role = m.get("role").and_then(|v| v.as_str()).unwrap_or("");
            match role {
                "tool" => {
                    let call_id = m
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let mut item = Map::new();
                    item.insert("type".into(), Value::String("function_call_output".into()));
                    item.insert("call_id".into(), Value::String(call_id));
                    let output_text = stringify_tool_output(m.get("content"));
                    item.insert("output".into(), Value::String(output_text));
                    input.push(Value::Object(item));
                }
                _ => {
                    let mut msg = Map::new();
                    msg.insert("type".into(), Value::String("message".into()));
                    let mapped_role = if role == "system" {
                        "developer"
                    } else {
                        role
                    };
                    msg.insert("role".into(), Value::String(mapped_role.to_string()));

                    let mut content_arr: Vec<Value> = Vec::new();
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    match m.get("content") {
                        Some(Value::String(s)) if !s.is_empty() => {
                            let mut part = Map::new();
                            part.insert("type".into(), Value::String(part_type.into()));
                            part.insert("text".into(), Value::String(s.clone()));
                            content_arr.push(Value::Object(part));
                        }
                        Some(Value::Array(items)) => {
                            for it in items {
                                let t = it.get("type").and_then(|x| x.as_str()).unwrap_or("");
                                if t == "text" {
                                    let text = it
                                        .get("text")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let mut part = Map::new();
                                    part.insert("type".into(), Value::String(part_type.into()));
                                    part.insert("text".into(), Value::String(text));
                                    content_arr.push(Value::Object(part));
                                }
                                // image_url / file 不在本期支持范围内，跳过
                            }
                        }
                        _ => {}
                    }
                    msg.insert("content".into(), Value::Array(content_arr.clone()));

                    // assistant 仅有 tool_calls 没有正文时，跳过 message 项（codex 直接收 function_call）
                    if role != "assistant" || !content_arr.is_empty() {
                        input.push(Value::Object(msg));
                    }

                    // assistant 的 tool_calls 拆成顶级 function_call 项
                    if role == "assistant" {
                        if let Some(tcs) = m.get("tool_calls").and_then(|v| v.as_array()) {
                            for tc in tcs {
                                if tc.get("type").and_then(|x| x.as_str()) != Some("function") {
                                    continue;
                                }
                                let call_id = tc
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let name_orig = tc
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let mapped_name = short_map
                                    .get(&name_orig)
                                    .cloned()
                                    .unwrap_or_else(|| shorten_name_if_needed(&name_orig));
                                let arguments = tc
                                    .get("function")
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(|a| a.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let mut fc = Map::new();
                                fc.insert("type".into(), Value::String("function_call".into()));
                                fc.insert("call_id".into(), Value::String(call_id));
                                fc.insert("name".into(), Value::String(mapped_name));
                                fc.insert("arguments".into(), Value::String(arguments));
                                input.push(Value::Object(fc));
                            }
                        }
                    }
                }
            }
        }
    }
    out.insert("input".into(), Value::Array(input));

    Value::Object(out)
}

/// tool 消息的 content 可能是 string / array / object。codex 期望 `output` 是简单字符串，
/// 这里用最稳的策略：能直接当 string 就用，否则把整个 JSON 序列化成 string。
fn stringify_tool_output(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => {
            let mut out = String::new();
            for it in items {
                if it.get("type").and_then(|x| x.as_str()) == Some("text") {
                    if let Some(t) = it.get("text").and_then(|v| v.as_str()) {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(t);
                    }
                }
            }
            out
        }
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    }
}
