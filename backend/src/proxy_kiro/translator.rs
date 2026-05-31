//! Anthropic Messages API 请求 → Kiro `generateAssistantResponse` payload 翻译。
//!
//! 关键约束（来自 Kiro 上游 schema）：
//! - history 必须 user / assistant 严格交替，且以 user 消息开头；
//! - system prompt 以 Human/AI pair 注入到 history 头部；
//! - tools 只放最后一条（currentMessage）的 context；
//! - assistant 的 thinking/reasoning 不回传到 history（会触发 400）。

use serde_json::Value;

use super::payload::{
    build_payload, map_model_id, BuildArgs, KiroAssistantResponseMessage, KiroHistoryMessage,
    KiroImage, KiroImageSource, KiroInputSchema, KiroToolResult, KiroToolResultContent,
    KiroToolSpecification, KiroToolUse, KiroToolWrapper, KiroUserInputMessage,
    KiroUserInputMessageContext,
};
use crate::proxy::translator::tool_names::build_short_name_map;

const ORIGIN: &str = "AI_EDITOR";
const MAX_TOOL_DESC_LEN: usize = 10_000;

/// 翻译结果：payload + tool 名映射（Kiro名→原名），后者用于把响应里的 tool 名还原。
pub struct Translated {
    pub payload: super::payload::KiroPayload,
    /// Kiro 工具名 → 客户端原始工具名。
    pub tool_name_restore: std::collections::HashMap<String, String>,
    pub model_id: String,
    pub stream: bool,
}

/// 主入口：解析 Anthropic Messages 请求 JSON。
pub fn translate(raw: &Value, profile_arn: &str) -> Result<Translated, String> {
    let model = raw
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if model.is_empty() {
        return Err("missing required field: model".to_string());
    }
    let model_id = map_model_id(&model);
    let stream = raw.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

    // ===== 工具名映射 =====
    let mut tool_name_restore = std::collections::HashMap::new();
    let mut orig_to_kiro: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    if let Some(tools) = raw.get("tools").and_then(|v| v.as_array()) {
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        let short = build_short_name_map(&names);
        for (orig, kiro) in short.iter() {
            orig_to_kiro.insert(orig.clone(), kiro.clone());
            tool_name_restore.insert(kiro.clone(), orig.clone());
        }
    }
    let to_kiro_name = |orig: &str| -> String {
        orig_to_kiro
            .get(orig)
            .cloned()
            .unwrap_or_else(|| orig.to_string())
    };

    // ===== system prompt =====
    let mut system_prompt = extract_system(raw.get("system"));
    let timestamp = chrono::Utc::now().to_rfc3339();
    system_prompt = format!("[Context: Current time is {timestamp}]\n\n{system_prompt}");

    // ===== messages → history + currentMessage =====
    let messages = raw
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut history: Vec<KiroHistoryMessage> = Vec::new();
    let mut current_content = String::new();
    let mut current_images: Vec<KiroImage> = Vec::new();
    let mut current_tool_results: Vec<KiroToolResult> = Vec::new();

    // pending user 内容（用于在遇到 assistant 前合并连续 user 消息）
    let mut pending_user = String::new();
    let mut pending_images: Vec<KiroImage> = Vec::new();
    let mut pending_tool_results: Vec<KiroToolResult> = Vec::new();

    let n = messages.len();
    for (i, msg) in messages.iter().enumerate() {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let is_last = i == n - 1;

        if role == "user" {
            let uc = extract_user_content(msg)?;
            if is_last {
                current_content = join_nonempty(&pending_user, &uc.content);
                current_images.extend(pending_images.drain(..));
                current_images.extend(uc.images);
                current_tool_results.extend(pending_tool_results.drain(..));
                current_tool_results.extend(uc.tool_results);
                pending_user.clear();
            } else {
                let next_is_assistant = messages
                    .get(i + 1)
                    .and_then(|m| m.get("role"))
                    .and_then(|v| v.as_str())
                    == Some("assistant");
                if next_is_assistant {
                    let content = join_nonempty(&pending_user, &uc.content);
                    let mut imgs = std::mem::take(&mut pending_images);
                    imgs.extend(uc.images);
                    let mut trs = std::mem::take(&mut pending_tool_results);
                    trs.extend(uc.tool_results);
                    push_user_history(&mut history, content, imgs, trs, &model_id);
                    pending_user.clear();
                } else {
                    pending_user = join_nonempty(&pending_user, &uc.content);
                    pending_images.extend(uc.images);
                    pending_tool_results.extend(uc.tool_results);
                }
            }
        } else if role == "assistant" {
            // 先把还没落地的 pending user 落到 history（保证交替）
            if !pending_user.trim().is_empty()
                || !pending_images.is_empty()
                || !pending_tool_results.is_empty()
            {
                let content = std::mem::take(&mut pending_user);
                let imgs = std::mem::take(&mut pending_images);
                let trs = std::mem::take(&mut pending_tool_results);
                push_user_history(&mut history, content, imgs, trs, &model_id);
            }
            let ac = extract_assistant_content(msg, &to_kiro_name)?;
            history.push(KiroHistoryMessage {
                user_input_message: None,
                assistant_response_message: Some(KiroAssistantResponseMessage {
                    content: ac.content,
                    tool_uses: if ac.tool_uses.is_empty() {
                        None
                    } else {
                        Some(ac.tool_uses)
                    },
                }),
            });
        }
    }

    // 收尾：剩余 pending user 并入 currentMessage
    if !pending_user.trim().is_empty()
        || !pending_images.is_empty()
        || !pending_tool_results.is_empty()
    {
        current_content = join_nonempty(&pending_user, &current_content);
        let mut imgs = std::mem::take(&mut pending_images);
        imgs.extend(current_images.drain(..));
        current_images = imgs;
        let mut trs = std::mem::take(&mut pending_tool_results);
        trs.extend(current_tool_results.drain(..));
        current_tool_results = trs;
    }

    // history 必须以 user 开头：若首条是 assistant，前插一条占位 user
    if history
        .first()
        .map(|h| h.assistant_response_message.is_some())
        .unwrap_or(false)
    {
        history.insert(
            0,
            KiroHistoryMessage {
                user_input_message: Some(KiroUserInputMessage {
                    content: "Begin conversation".to_string(),
                    model_id: Some(model_id.clone()),
                    origin: ORIGIN.to_string(),
                    images: None,
                    context: None,
                }),
                assistant_response_message: None,
            },
        );
    }

    // system prompt 注入为 history 头部的 Human/AI pair
    if !system_prompt.trim().is_empty() {
        let pair = vec![
            KiroHistoryMessage {
                user_input_message: Some(KiroUserInputMessage {
                    content: system_prompt,
                    model_id: None,
                    origin: ORIGIN.to_string(),
                    images: None,
                    context: Some(KiroUserInputMessageContext::default()),
                }),
                assistant_response_message: None,
            },
            KiroHistoryMessage {
                user_input_message: None,
                assistant_response_message: Some(KiroAssistantResponseMessage {
                    content: "I will follow these instructions.".to_string(),
                    tool_uses: None,
                }),
            },
        ];
        let mut new_history = pair;
        new_history.extend(history.drain(..));
        history = new_history;
    }

    // ===== tools =====
    let tools = convert_tools(raw.get("tools"), &to_kiro_name);

    // ===== thinking =====
    let thinking = raw
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s != "disabled")
        .unwrap_or(false);

    let max_tokens = raw.get("max_tokens").and_then(|v| v.as_i64());
    let temperature = raw.get("temperature").and_then(|v| v.as_f64());
    let top_p = raw.get("top_p").and_then(|v| v.as_f64());

    let payload = build_payload(BuildArgs {
        content: current_content,
        model_id: model_id.clone(),
        origin: ORIGIN.to_string(),
        history,
        tools,
        tool_results: current_tool_results,
        images: current_images,
        profile_arn: profile_arn.to_string(),
        max_tokens,
        temperature,
        top_p,
        thinking,
    });

    Ok(Translated {
        payload,
        tool_name_restore,
        model_id,
        stream,
    })
}

fn join_nonempty(a: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}\n{b}"),
    }
}

fn push_user_history(
    history: &mut Vec<KiroHistoryMessage>,
    content: String,
    images: Vec<KiroImage>,
    tool_results: Vec<KiroToolResult>,
    model_id: &str,
) {
    if content.trim().is_empty() && images.is_empty() && tool_results.is_empty() {
        return;
    }
    let content = if content.trim().is_empty() {
        if !tool_results.is_empty() {
            "Tool results provided.".to_string()
        } else {
            "Continue".to_string()
        }
    } else {
        content
    };
    let context = if tool_results.is_empty() {
        None
    } else {
        Some(KiroUserInputMessageContext {
            tool_results: Some(tool_results),
            tools: None,
        })
    };
    history.push(KiroHistoryMessage {
        user_input_message: Some(KiroUserInputMessage {
            content,
            model_id: Some(model_id.to_string()),
            origin: ORIGIN.to_string(),
            images: if images.is_empty() { None } else { Some(images) },
            context,
        }),
        assistant_response_message: None,
    });
}

fn extract_system(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

struct UserContent {
    content: String,
    images: Vec<KiroImage>,
    tool_results: Vec<KiroToolResult>,
}

fn extract_user_content(msg: &Value) -> Result<UserContent, String> {
    let mut content = String::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    match msg.get("content") {
        Some(Value::String(s)) => content = s.clone(),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            content.push_str(t);
                        }
                    }
                    "image" => {
                        if let Some(src) = block.get("source") {
                            if src.get("type").and_then(|v| v.as_str()) == Some("base64") {
                                let media = src
                                    .get("media_type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let fmt = media.split('/').nth(1).unwrap_or("");
                                if media.starts_with("image/") && !fmt.is_empty() {
                                    let data = src
                                        .get("data")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    images.push(KiroImage {
                                        format: normalize_image_format(fmt),
                                        source: KiroImageSource { bytes: data },
                                    });
                                }
                            }
                        }
                    }
                    "tool_result" => {
                        if let Some(tid) = block.get("tool_use_id").and_then(|v| v.as_str()) {
                            let text = extract_tool_result_text(block.get("content"));
                            tool_results.push(KiroToolResult {
                                tool_use_id: tid.to_string(),
                                content: vec![KiroToolResultContent { text }],
                                status: "success".to_string(),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    Ok(UserContent {
        content,
        images,
        tool_results,
    })
}

fn extract_tool_result_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => {
            if s.is_empty() {
                "(empty)".to_string()
            } else {
                s.clone()
            }
        }
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if parts.is_empty() {
                "(no text output)".to_string()
            } else {
                parts.join("")
            }
        }
        Some(Value::Null) | None => "(no output)".to_string(),
        Some(other) => other.to_string(),
    }
}

struct AssistantContent {
    content: String,
    tool_uses: Vec<KiroToolUse>,
}

fn extract_assistant_content(
    msg: &Value,
    to_kiro_name: &impl Fn(&str) -> String,
) -> Result<AssistantContent, String> {
    let mut content = String::new();
    let mut tool_uses = Vec::new();

    match msg.get("content") {
        Some(Value::String(s)) => content = s.clone(),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                let btype = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                            content.push_str(t);
                        }
                    }
                    // 故意忽略 thinking / redacted_thinking：history 不回传 reasoning
                    "tool_use" => {
                        let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        if id.is_empty() || name.is_empty() {
                            continue;
                        }
                        let input = block.get("input").cloned().unwrap_or(Value::Object(
                            serde_json::Map::new(),
                        ));
                        if !input.is_object() {
                            return Err(format!("tool_use requires object input: {name}"));
                        }
                        tool_uses.push(KiroToolUse {
                            tool_use_id: id.to_string(),
                            name: to_kiro_name(name),
                            input,
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    // Kiro 要求 assistant content 非空
    if content.trim().is_empty() && !tool_uses.is_empty() {
        content = " ".to_string();
    }

    Ok(AssistantContent { content, tool_uses })
}

fn convert_tools(tools: Option<&Value>, to_kiro_name: &impl Fn(&str) -> String) -> Vec<KiroToolWrapper> {
    let Some(arr) = tools.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for tool in arr {
        let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let mut description = tool
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("Tool: {name}"));
        if description.len() > MAX_TOOL_DESC_LEN {
            description.truncate(MAX_TOOL_DESC_LEN);
            description.push_str("...");
        }
        let schema = tool
            .get("input_schema")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({"type": "object"}));
        out.push(KiroToolWrapper {
            tool_specification: KiroToolSpecification {
                name: to_kiro_name(name),
                description,
                input_schema: KiroInputSchema { json: schema },
            },
        });
    }
    out
}

fn normalize_image_format(fmt: &str) -> String {
    match fmt.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg".to_string(),
        other => other.to_string(),
    }
}
