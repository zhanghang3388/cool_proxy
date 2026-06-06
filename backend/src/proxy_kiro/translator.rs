//! Anthropic Messages API 请求 → Kiro `generateAssistantResponse` payload 翻译。
//!
//! 关键约束（来自 Kiro 上游 schema）：
//! - history 必须 user / assistant 严格交替，且以 user 消息开头；
//! - system prompt 以 Human/AI pair 注入到 history 头部；
//! - tools 只放最后一条（currentMessage）的 context；
//! - assistant 的 thinking/reasoning 不回传到 history（会触发 400）。

use std::collections::HashSet;

use serde_json::Value;

use super::payload::{
    build_payload, map_model_id, BuildArgs, KiroAssistantResponseMessage, KiroHistoryMessage,
    KiroImage, KiroImageSource, KiroInputSchema, KiroToolResult, KiroToolResultContent,
    KiroToolSpecification, KiroToolUse, KiroToolWrapper, KiroUserInputMessage,
    KiroUserInputMessageContext,
};
use super::prompt_filter::{self, PromptFilterOptions};
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
pub fn translate(
    raw: &Value,
    profile_arn: &str,
    filter: PromptFilterOptions,
) -> Result<Translated, String> {
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
    // 先过滤掉 Claude Code 的身份/环境噪音（对齐 KAM），再前置时间戳。
    let mut system_prompt = prompt_filter::apply(filter, &extract_system(raw.get("system")));
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

    // ===== tool_use / tool_result 配对校验（对齐 kiro.rs convert_request 第 8~10 步）=====
    // Kiro 上游的硬约束（不满足直接 400，curl 单轮碰不到、Claude Code 多轮必中）：
    //  ① 每个 assistant 的 tool_use 必须有配对的 tool_result，否则 400；
    //  ② 历史里引用过的工具名必须在 currentMessage.tools 里有定义，否则 400。
    // 先过滤当前消息里重复 / 孤立的 tool_result，拿到"历史里无任何 result 配对"的孤立
    // tool_use，再把它们从历史里摘掉；最后收集历史用到的工具名，留待补占位。
    let orphaned_tool_use_ids = validate_tool_pairing(&history, &mut current_tool_results);
    remove_orphaned_tool_uses(&mut history, &orphaned_tool_use_ids);
    let history_tool_names = collect_history_tool_names(&history);

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
    let mut tools = convert_tools(raw.get("tools"), &to_kiro_name);
    // ②：给历史里用过、但当前 tools 列表里没有的工具补占位定义（Kiro 否则 400）。
    append_placeholder_tools(&mut tools, &history_tool_names);

    // ===== thinking =====
    // 来源一：显式 thinking 参数（type != "disabled"）；
    // 来源二：model 名带 `-thinking` 后缀（归一化时已从 modelId 剥掉，这里据原名判定）。
    let thinking_param = raw
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        .map(|s| s != "disabled")
        .unwrap_or(false);
    let thinking = thinking_param || model.to_ascii_lowercase().ends_with("-thinking");

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
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
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
                                let media =
                                    src.get("media_type").and_then(|v| v.as_str()).unwrap_or("");
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
                        let input = block
                            .get("input")
                            .cloned()
                            .unwrap_or(Value::Object(serde_json::Map::new()));
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

fn convert_tools(
    tools: Option<&Value>,
    to_kiro_name: &impl Fn(&str) -> String,
) -> Vec<KiroToolWrapper> {
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
            // 按字符边界截断：MAX_TOOL_DESC_LEN 是字节上限，直接 String::truncate 到字节
            // 位置会在多字节 UTF-8 字符（中文 / emoji 等）中间 panic。
            let mut end = MAX_TOOL_DESC_LEN;
            while end > 0 && !description.is_char_boundary(end) {
                end -= 1;
            }
            description.truncate(end);
            description.push_str("...");
        }
        // Claude Code / MCP 的 input_schema 经常带脏字段（required:null、properties:null、
        // 缺 type 等），原样透传会被 Kiro 上游判为 400 "Improperly formed request"。
        // 这里统一规整成合法 schema（对齐 kiro.rs normalize_json_schema）。
        let schema = normalize_json_schema(tool.get("input_schema").cloned().unwrap_or(Value::Null));
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

/// 规范化工具的 JSON Schema，修复 Claude Code / MCP 工具定义里常见的脏字段。
///
/// 对齐 kiro.rs `normalize_json_schema`：
///  - 非 object（含 null / 缺失）→ 直接兜底成最小合法 object schema；
///  - `type` 不是非空字符串 → 设为 "object"；
///  - `properties` 不是 object → 设为空 object；
///  - `required` 不是字符串数组 → 设为空数组（过滤掉非字符串项）；
///  - `additionalProperties` 不是 bool/object → 设为 true。
fn normalize_json_schema(schema: Value) -> Value {
    let Value::Object(mut obj) = schema else {
        return serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": true
        });
    };

    if !obj
        .get("type")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty())
    {
        obj.insert("type".to_string(), Value::String("object".to_string()));
    }

    if !matches!(obj.get("properties"), Some(Value::Object(_))) {
        obj.insert(
            "properties".to_string(),
            Value::Object(serde_json::Map::new()),
        );
    }

    let required = match obj.remove("required") {
        Some(Value::Array(arr)) => Value::Array(
            arr.into_iter()
                .filter_map(|v| v.as_str().map(|s| Value::String(s.to_string())))
                .collect(),
        ),
        _ => Value::Array(Vec::new()),
    };
    obj.insert("required".to_string(), required);

    if !matches!(
        obj.get("additionalProperties"),
        Some(Value::Bool(_)) | Some(Value::Object(_))
    ) {
        obj.insert("additionalProperties".to_string(), Value::Bool(true));
    }

    Value::Object(obj)
}

/// 历史里所有 assistant 的 tool_use_id。
fn collect_history_tool_use_ids(history: &[KiroHistoryMessage]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for h in history {
        if let Some(a) = &h.assistant_response_message {
            if let Some(tus) = &a.tool_uses {
                for tu in tus {
                    ids.insert(tu.tool_use_id.clone());
                }
            }
        }
    }
    ids
}

/// 历史 user 消息 context 里已经携带的 tool_result 对应的 tool_use_id。
fn collect_history_tool_result_ids(history: &[KiroHistoryMessage]) -> HashSet<String> {
    let mut ids = HashSet::new();
    for h in history {
        if let Some(u) = &h.user_input_message {
            if let Some(ctx) = &u.context {
                if let Some(trs) = &ctx.tool_results {
                    for tr in trs {
                        ids.insert(tr.tool_use_id.clone());
                    }
                }
            }
        }
    }
    ids
}

/// 历史里 assistant 用过的工具名（去重、保持出现顺序）。
fn collect_history_tool_names(history: &[KiroHistoryMessage]) -> Vec<String> {
    let mut names = Vec::new();
    for h in history {
        if let Some(a) = &h.assistant_response_message {
            if let Some(tus) = &a.tool_uses {
                for tu in tus {
                    if !names.contains(&tu.name) {
                        names.push(tu.name.clone());
                    }
                }
            }
        }
    }
    names
}

/// 校验当前消息的 tool_results 配对、就地过滤掉重复 / 孤立项，返回历史里"完全没有
/// 任何 tool_result 配对"的孤立 tool_use_id 集合（对齐 kiro.rs `validate_tool_pairing`）。
fn validate_tool_pairing(
    history: &[KiroHistoryMessage],
    current_tool_results: &mut Vec<KiroToolResult>,
) -> HashSet<String> {
    let all_tool_use_ids = collect_history_tool_use_ids(history);
    let history_result_ids = collect_history_tool_result_ids(history);
    // 历史里尚未被任何 result 配对的 tool_use
    let mut unpaired: HashSet<String> = all_tool_use_ids
        .difference(&history_result_ids)
        .cloned()
        .collect();

    let mut filtered = Vec::with_capacity(current_tool_results.len());
    for tr in std::mem::take(current_tool_results) {
        if unpaired.remove(&tr.tool_use_id) {
            // 与历史里某个未配对的 tool_use 成功配对
            filtered.push(tr);
        } else if all_tool_use_ids.contains(&tr.tool_use_id) {
            // 已在历史里配对过：重复 tool_result，丢弃
        } else {
            // 找不到对应 tool_use：孤立 tool_result，丢弃
        }
    }
    *current_tool_results = filtered;

    // 剩下的 unpaired 就是真正孤立的 tool_use（无任何 result），需从历史摘除
    unpaired
}

/// 从历史 assistant 消息里移除孤立的 tool_use（Kiro 要求 tool_use 必须有配对 result）。
fn remove_orphaned_tool_uses(history: &mut [KiroHistoryMessage], orphaned_ids: &HashSet<String>) {
    if orphaned_ids.is_empty() {
        return;
    }
    for h in history.iter_mut() {
        if let Some(a) = &mut h.assistant_response_message {
            if let Some(tus) = &mut a.tool_uses {
                tus.retain(|tu| !orphaned_ids.contains(&tu.tool_use_id));
                if tus.is_empty() {
                    a.tool_uses = None;
                }
            }
        }
    }
}

/// 给历史里用过、但当前 tools 列表里没有的工具补占位定义（工具名大小写不敏感比较，
/// 对齐 kiro.rs：Kiro 匹配工具名忽略大小写）。
fn append_placeholder_tools(tools: &mut Vec<KiroToolWrapper>, history_tool_names: &[String]) {
    if history_tool_names.is_empty() {
        return;
    }
    let existing: HashSet<String> = tools
        .iter()
        .map(|t| t.tool_specification.name.to_ascii_lowercase())
        .collect();
    for name in history_tool_names {
        if name.is_empty() || existing.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        tools.push(KiroToolWrapper {
            tool_specification: KiroToolSpecification {
                name: name.clone(),
                description: "Tool used in conversation history".to_string(),
                input_schema: KiroInputSchema {
                    json: serde_json::json!({
                        "type": "object",
                        "properties": {},
                        "required": [],
                        "additionalProperties": true
                    }),
                },
            },
        });
    }
}

fn normalize_image_format(fmt: &str) -> String {
    match fmt.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "jpeg".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 回归：超长且全多字节字符的工具描述不应 panic（旧实现按字节 truncate 会在
    /// 多字节字符中间切断而 panic），且应被安全截断。
    #[test]
    fn long_multibyte_tool_description_does_not_panic() {
        let desc = "中".repeat(5000); // 每个 '中' 3 字节 => 15000 字节，远超 MAX_TOOL_DESC_LEN
        let raw = json!({
            "model": "claude-sonnet-4.5",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "do_thing",
                "description": desc,
                "input_schema": {"type": "object"}
            }]
        });
        let t = translate(
            &raw,
            "arn:aws:codewhisperer:us-east-1:123456789012:profile/X",
            PromptFilterOptions::default(),
        )
        .expect("translate should not fail");
        let payload_json = serde_json::to_string(&t.payload).expect("payload serializes");
        // 被截断：出现省略号、且不再包含完整原始串
        assert!(
            payload_json.contains("..."),
            "description should be truncated"
        );
        assert!(
            !payload_json.contains(&desc),
            "full description must not survive"
        );
    }

    #[test]
    fn normalizes_dirty_tool_schema() {
        // required:null / properties:null / 缺 type → 修成合法 schema（否则 Kiro 400）
        let fixed = normalize_json_schema(json!({"properties": null, "required": null}));
        assert_eq!(fixed["type"], json!("object"));
        assert_eq!(fixed["properties"], json!({}));
        assert_eq!(fixed["required"], json!([]));
        assert_eq!(fixed["additionalProperties"], json!(true));
    }

    #[test]
    fn non_object_schema_falls_back_to_minimal() {
        for v in [json!("nonsense"), json!(123), Value::Null, json!([1, 2])] {
            let fixed = normalize_json_schema(v);
            assert_eq!(fixed["type"], json!("object"));
            assert!(fixed["properties"].is_object());
            assert_eq!(fixed["required"], json!([]));
        }
    }

    #[test]
    fn keeps_valid_schema_fields() {
        let good = json!({
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"],
            "additionalProperties": false
        });
        let fixed = normalize_json_schema(good);
        assert_eq!(fixed["properties"]["q"]["type"], json!("string"));
        assert_eq!(fixed["required"], json!(["q"]));
        assert_eq!(fixed["additionalProperties"], json!(false));
    }

    #[test]
    fn required_filters_non_string_entries() {
        let fixed = normalize_json_schema(json!({"type": "object", "required": ["a", 1, null, "b"]}));
        assert_eq!(fixed["required"], json!(["a", "b"]));
    }

    #[test]
    fn orphaned_tool_use_removed_and_placeholder_added() {
        // assistant 用了 search(tu_1) + missing_tool(tu_2)，但只有 tu_1 有 tool_result。
        // tu_2 无配对 → 应从历史摘除；search 历史用过但当前 tools 为空 → 应补占位。
        let raw = json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                {"role": "user", "content": "do X"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "search", "input": {"q": "a"}},
                    {"type": "tool_use", "id": "tu_2", "name": "missing_tool", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "tu_1", "content": "ok"}
                ]}
            ]
        });
        let t = translate(
            &raw,
            "arn:aws:codewhisperer:us-east-1:1:profile/X",
            PromptFilterOptions::default(),
        )
        .expect("translate ok");
        let s = serde_json::to_string(&t.payload).expect("serialize");
        assert!(s.contains("tu_1"), "paired tool_use kept");
        assert!(!s.contains("tu_2"), "orphaned tool_use must be removed");
        assert!(
            !s.contains("missing_tool"),
            "orphaned tool must not become a placeholder"
        );
        assert!(s.contains("search"), "history-used tool gets a placeholder");
        assert!(
            s.contains("Tool used in conversation history"),
            "placeholder description present"
        );
    }

    #[test]
    fn orphaned_tool_result_dropped() {
        // 当前消息带一个找不到对应 tool_use 的 tool_result → 应被丢弃，不下发给上游。
        let raw = json!({
            "model": "claude-sonnet-4.5",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "ghost_id", "content": "x"}
                ]}
            ]
        });
        let t = translate(
            &raw,
            "arn:aws:codewhisperer:us-east-1:1:profile/X",
            PromptFilterOptions::default(),
        )
        .expect("translate ok");
        let s = serde_json::to_string(&t.payload).expect("serialize");
        assert!(!s.contains("ghost_id"), "orphaned tool_result must be dropped");
    }
}
