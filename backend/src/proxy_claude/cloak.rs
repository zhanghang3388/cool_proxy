//! OAuth"伪装"：让发往 Anthropic 的请求看起来像 Claude Code，否则 `sk-ant-oat` 令牌
//! 会被判定为三方客户端（额外计费）甚至直接 401。两件事：
//!  1. system[] 第一块必须是 Claude Code 身份标识，Anthropic 据此放行 `user:sessions:claude_code` scope；
//!  2. 把第三方小写工具名改成 Claude Code 的首字母大写名（bash→Bash…），响应再还原回去。
//!
//! 注意：这里**只**注入身份标识那一块，不强行塞入 Claude Code 的整套行为提示——后者会把
//! 模型行为绑死成"编码 CLI"，对通用用途有害。用户自己的 system 原样保留在身份标识之后。

use std::collections::HashMap;

use serde_json::{json, Value};

/// OAuth 令牌放行所需的 system 首块文本（必须逐字一致）。
pub const AGENT_IDENTIFIER: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// 第三方（小写）工具名 → Claude Code（首字母大写）工具名。
const TOOL_RENAME: &[(&str, &str)] = &[
    ("bash", "Bash"),
    ("read", "Read"),
    ("write", "Write"),
    ("edit", "Edit"),
    ("glob", "Glob"),
    ("grep", "Grep"),
    ("task", "Task"),
    ("webfetch", "WebFetch"),
    ("websearch", "WebSearch"),
    ("todowrite", "TodoWrite"),
    ("todoread", "TodoRead"),
    ("question", "Question"),
    ("skill", "Skill"),
    ("ls", "LS"),
    ("notebookedit", "NotebookEdit"),
];

fn rename_to(name: &str) -> Option<&'static str> {
    TOOL_RENAME
        .iter()
        .find(|(from, _)| *from == name)
        .map(|(_, to)| *to)
}

/// 对请求体做 OAuth 伪装（原地修改）。返回 `升级名 -> 原名` 的反向映射，
/// 用于把响应里的工具名还原成客户端原本发来的名字。
pub fn apply_oauth_cloaking(body: &mut Value) -> HashMap<String, String> {
    inject_system_identifier(body);
    rename_tool_names(body)
}

/// 在 system[] 头部插入 Claude Code 身份标识；保留用户原本的 system 内容。
fn inject_system_identifier(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    // 已经以身份标识开头则不重复注入（客户端本就是 Claude Code）。
    let already = match obj.get("system") {
        Some(Value::Array(arr)) => {
            arr.first()
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                == Some(AGENT_IDENTIFIER)
        }
        Some(Value::String(s)) => s.starts_with(AGENT_IDENTIFIER),
        _ => false,
    };
    if already {
        return;
    }

    let mut new_system: Vec<Value> = vec![json!({"type": "text", "text": AGENT_IDENTIFIER})];
    match obj.get("system") {
        Some(Value::String(s)) => {
            if !s.trim().is_empty() {
                new_system.push(json!({"type": "text", "text": s}));
            }
        }
        Some(Value::Array(arr)) => {
            for block in arr {
                new_system.push(block.clone());
            }
        }
        _ => {}
    }
    obj.insert("system".to_string(), Value::Array(new_system));
}

/// 改写 tools[] / tool_choice / messages 里的工具名，返回反向映射。
fn rename_tool_names(body: &mut Value) -> HashMap<String, String> {
    let mut reverse: HashMap<String, String> = HashMap::new();
    let Some(obj) = body.as_object_mut() else {
        return reverse;
    };

    // tools[]：跳过带 type 的内置工具（web_search / code_execution 等）。
    if let Some(Value::Array(tools)) = obj.get_mut("tools") {
        for tool in tools.iter_mut() {
            let is_builtin = tool
                .get("type")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            if is_builtin {
                continue;
            }
            if let Some(t) = tool.as_object_mut() {
                rename_obj_field(t, "name", &mut reverse);
            }
        }
    }

    // tool_choice.name（仅当 type=="tool"）
    if let Some(tc) = obj.get_mut("tool_choice").and_then(|v| v.as_object_mut()) {
        if tc.get("type").and_then(|v| v.as_str()) == Some("tool") {
            rename_obj_field(tc, "name", &mut reverse);
        }
    }

    // messages[].content[].tool_use.name
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for msg in messages.iter_mut() {
            let Some(Value::Array(content)) = msg.get_mut("content") else {
                continue;
            };
            for part in content.iter_mut() {
                let Some(p) = part.as_object_mut() else {
                    continue;
                };
                if p.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                    rename_obj_field(p, "name", &mut reverse);
                }
            }
        }
    }

    reverse
}

fn rename_obj_field(
    obj: &mut serde_json::Map<String, Value>,
    key: &str,
    reverse: &mut HashMap<String, String>,
) {
    let Some(name) = obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string()) else {
        return;
    };
    if let Some(new_name) = rename_to(&name) {
        if new_name != name {
            reverse.insert(new_name.to_string(), name.clone());
            obj.insert(key.to_string(), json!(new_name));
        }
    }
}

/// 还原非流式响应里的工具名（content[] 里的 tool_use.name）。
pub fn restore_tool_names_in_json(body: &mut Value, reverse: &HashMap<String, String>) {
    if reverse.is_empty() {
        return;
    }
    let Some(Value::Array(content)) = body.get_mut("content") else {
        return;
    };
    for part in content.iter_mut() {
        let Some(p) = part.as_object_mut() else {
            continue;
        };
        if p.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
            continue;
        }
        if let Some(name) = p
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            if let Some(orig) = reverse.get(&name) {
                p.insert("name".to_string(), json!(orig));
            }
        }
    }
}

/// 还原流式 SSE 行里的工具名（content_block_start 的 tool_use.name）。
/// 改写了返回 `Some(新行)`（不含结尾换行），未改写返回 `None`。
pub fn restore_tool_names_in_sse_line(
    line: &str,
    reverse: &HashMap<String, String>,
) -> Option<String> {
    if reverse.is_empty() {
        return None;
    }
    let payload = line.strip_prefix("data:")?.trim_start();
    if !payload.starts_with('{') {
        return None;
    }
    let mut v: Value = serde_json::from_str(payload).ok()?;
    let cb = v.get_mut("content_block")?.as_object_mut()?;
    if cb.get("type").and_then(|x| x.as_str()) != Some("tool_use") {
        return None;
    }
    let name = cb.get("name").and_then(|x| x.as_str())?.to_string();
    let orig = reverse.get(&name)?;
    cb.insert("name".to_string(), json!(orig));
    Some(format!("data: {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_identifier_as_first_system_block() {
        let mut body = json!({"system": "be helpful", "messages": []});
        apply_oauth_cloaking(&mut body);
        let sys = body.get("system").unwrap().as_array().unwrap();
        assert_eq!(sys[0]["text"], AGENT_IDENTIFIER);
        assert_eq!(sys[1]["text"], "be helpful");
    }

    #[test]
    fn does_not_double_inject() {
        let mut body = json!({"system": [{"type": "text", "text": AGENT_IDENTIFIER}]});
        apply_oauth_cloaking(&mut body);
        let sys = body.get("system").unwrap().as_array().unwrap();
        assert_eq!(sys.len(), 1);
    }

    #[test]
    fn renames_tools_and_restores_response() {
        let mut body = json!({
            "tools": [{"name": "bash"}, {"type": "web_search_20250305", "name": "web_search"}],
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "name": "edit", "id": "x", "input": {}}]}]
        });
        let reverse = apply_oauth_cloaking(&mut body);
        assert_eq!(body["tools"][0]["name"], "Bash");
        // 内置工具不动
        assert_eq!(body["tools"][1]["name"], "web_search");
        assert_eq!(body["messages"][0]["content"][0]["name"], "Edit");

        let mut resp =
            json!({"content": [{"type": "tool_use", "name": "Bash", "id": "y", "input": {}}]});
        restore_tool_names_in_json(&mut resp, &reverse);
        assert_eq!(resp["content"][0]["name"], "bash");
    }
}
