//! OAuth"伪装"：让发往 Anthropic 的请求看起来像 Claude Code，否则 `sk-ant-oat` 令牌
//! 会被判定为三方客户端（额外计费）甚至直接 401。几件事：
//!  1. system[] 第一块必须是 Claude Code 身份标识，Anthropic 据此放行 `user:sessions:claude_code` scope；
//!  2. system[0] 再前置一个 `x-anthropic-billing-header:` 块，让后端按"一方 Claude Code"归类计费；
//!  3. metadata.user_id 注入 Claude Code 格式的假 ID，避免请求关联到真实账号触发 per-user 限额；
//!  4. 把第三方小写工具名改成 Claude Code 的首字母大写名（bash→Bash…），响应再还原回去。
//!
//! 注意：这里**只**注入身份标识那一块，不强行塞入 Claude Code 的整套行为提示——后者会把
//! 模型行为绑死成"编码 CLI"，对通用用途有害。用户自己的 system 原样保留在身份标识之后。

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// OAuth 令牌放行所需的 system 首块文本（必须逐字一致）。
pub const AGENT_IDENTIFIER: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// 伪装使用的 Claude Code 版本号；需与出站 `User-Agent`（mod.rs `CLAUDE_USER_AGENT`）保持一致。
/// billing header 里的 `cc_version` 由它 + 构建指纹拼成。
pub const CLAUDE_CODE_VERSION: &str = "2.1.63";

/// 计算构建指纹用的盐（照搬参考 `claude_executor.go:1615 fingerprintSalt`）。
const FINGERPRINT_SALT: &str = "59cf53e54c78";

/// billing header 块的前缀，用于幂等判断。
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

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
///
/// `account_id` 用于按账号缓存假 user_id，保持同账号会话连续性。
pub fn apply_oauth_cloaking(body: &mut Value, account_id: &str) -> HashMap<String, String> {
    // 指纹用的"原始首块 system 文本"必须在注入身份标识之前抓取。
    let fingerprint_src = first_system_text(body);
    inject_fake_user_id(body, account_id);
    // 幂等门：system[0] 已是 billing header，说明请求已被伪装过（或客户端本就是完整的
    // Claude Code，自带 billing + 身份块），此时跳过身份与计费注入，避免重复堆叠。
    if !first_system_text(body).starts_with(BILLING_PREFIX) {
        inject_system_identifier(body);
        inject_billing_header(body, &fingerprint_src);
    }
    rename_tool_names(body)
}

/// 取 system 的首块文本（数组取第一个 text 块，字符串直接取），用于构建指纹。
fn first_system_text(body: &Value) -> String {
    match body.get("system") {
        Some(Value::Array(arr)) => arr
            .iter()
            .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .and_then(|b| b.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// 构建 3 字符构建指纹：`SHA256(salt + msgText[4] + msgText[7] + msgText[20] + version)[:3]`。
/// 越界的字符位用 '0' 兜底（照搬参考 `computeFingerprint`，按 Unicode 标量计位）。
fn compute_fingerprint(message_text: &str, version: &str) -> String {
    let chars: Vec<char> = message_text.chars().collect();
    let mut picked = String::new();
    for idx in [4usize, 7, 20] {
        picked.push(chars.get(idx).copied().unwrap_or('0'));
    }
    let input = format!("{FINGERPRINT_SALT}{picked}{version}");
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(digest)[..3].to_string()
}

/// 在 system[0] 前置 `x-anthropic-billing-header:` 块。已存在则跳过。
/// `cch` 取当前请求体序列化后的 SHA256 前 5 位（与参考一致：覆盖 system+messages+tools）。
fn inject_billing_header(body: &mut Value, fingerprint_src: &str) {
    // 幂等：首块已是 billing header 就不重复注入。
    if first_system_text(body).starts_with(BILLING_PREFIX) {
        return;
    }
    let build_hash = compute_fingerprint(fingerprint_src, CLAUDE_CODE_VERSION);
    let cch = {
        let bytes = serde_json::to_vec(body).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        hex::encode(digest)[..5].to_string()
    };
    let billing_text = format!(
        "{BILLING_PREFIX} cc_version={CLAUDE_CODE_VERSION}.{build_hash}; cc_entrypoint=cli; cch={cch};"
    );
    let block = json!({"type": "text", "text": billing_text});

    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match obj.get_mut("system") {
        Some(Value::Array(arr)) => arr.insert(0, block),
        // 走到这里时身份标识注入已把 system 规整成数组；字符串/缺失只兜底处理。
        Some(Value::String(s)) => {
            let prev = json!({"type": "text", "text": s.clone()});
            obj.insert("system".to_string(), Value::Array(vec![block, prev]));
        }
        _ => {
            obj.insert("system".to_string(), Value::Array(vec![block]));
        }
    }
}

/// Anthropic Messages API `max_tokens` 缺失时的兜底值（必填字段，缺了直接 400）。
const DEFAULT_MAX_TOKENS: i64 = 1024;

/// 在转发前对请求体做一遍规范化，修掉若干会被 Anthropic 直接打回 400 的字段组合。
/// 与 OAuth 伪装解耦：伪装只管"看起来像 Claude Code"，这里只管"请求本身合法"。
/// 客户端若是真实 Claude Code，自己发的组合本就合法，这里基本是空操作；但 cool_proxy
/// 也服务通用客户端，它们可能发出非法组合（thinking + 强制工具、thinking + 非 1 温度、
/// budget_tokens >= max_tokens、漏发 max_tokens），这里统一兜住。
///
/// 规则均照搬参考实现（CLIProxyAPI）：
///  - `disableThinkingIfToolChoiceForced`（claude_executor.go:790）
///  - `normalizeClaudeTemperatureForThinking`（claude_executor.go:809）
///  - `normalizeClaudeBudget`（thinking/provider/claude/apply.go:167）
pub fn normalize_request_body(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };

    // max_tokens 必填：缺失则注入兜底，顺带成为后面 budget 比较的基准。
    ensure_max_tokens(obj);

    // tool_choice 强制工具（any / tool）时，Anthropic 拒绝任何 thinking 配置。
    disable_thinking_if_tool_choice_forced(obj);

    // thinking 开启（enabled / adaptive / auto）时，temperature 只能是 1（或不传）。
    normalize_temperature_for_thinking(obj);

    // budget_tokens 必须严格小于 max_tokens。
    normalize_thinking_budget(obj);
}

/// `max_tokens` 缺失或非正数时写入兜底值。
fn ensure_max_tokens(obj: &mut serde_json::Map<String, Value>) {
    let valid = obj
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .map(|n| n > 0)
        .unwrap_or(false);
    if !valid {
        obj.insert("max_tokens".to_string(), json!(DEFAULT_MAX_TOKENS));
    }
}

/// `tool_choice.type` 为 `any` / `tool` 时删除 thinking 与 output_config.effort。
fn disable_thinking_if_tool_choice_forced(obj: &mut serde_json::Map<String, Value>) {
    let forced = obj
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| t == "any" || t == "tool")
        .unwrap_or(false);
    if !forced {
        return;
    }
    obj.remove("thinking");
    // adaptive thinking 可能把控制项放在 output_config.effort，一并清掉；清空则删整个对象。
    if let Some(oc) = obj.get_mut("output_config").and_then(|v| v.as_object_mut()) {
        oc.remove("effort");
        if oc.is_empty() {
            obj.remove("output_config");
        }
    }
}

/// thinking 开启时把非 1 的 temperature 归一到 1。
fn normalize_temperature_for_thinking(obj: &mut serde_json::Map<String, Value>) {
    // 客户端没传 temperature 就不用管（不传等价于默认 1）。
    if !obj.contains_key("temperature") {
        return;
    }
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .map(|t| {
            let t = t.trim().to_ascii_lowercase();
            matches!(t.as_str(), "enabled" | "adaptive" | "auto")
        })
        .unwrap_or(false);
    if !thinking_on {
        return;
    }
    let already_one = obj
        .get("temperature")
        .and_then(|v| v.as_f64())
        .map(|f| f == 1.0)
        .unwrap_or(false);
    if !already_one {
        obj.insert("temperature".to_string(), json!(1));
    }
}

/// `thinking.budget_tokens >= max_tokens` 时把 budget 压到 `max_tokens - 1`。
fn normalize_thinking_budget(obj: &mut serde_json::Map<String, Value>) {
    let Some(budget) = obj
        .get("thinking")
        .and_then(|t| t.get("budget_tokens"))
        .and_then(|v| v.as_i64())
    else {
        return;
    };
    if budget <= 0 {
        return;
    }
    // ensure_max_tokens 已保证 max_tokens 存在且为正。
    let max = obj
        .get("max_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(DEFAULT_MAX_TOKENS);
    if budget >= max {
        let adjusted = max - 1;
        if let Some(t) = obj.get_mut("thinking").and_then(|v| v.as_object_mut()) {
            t.insert("budget_tokens".to_string(), json!(adjusted));
        }
    }
}

/// 按账号缓存的假 user_id：同一账号在进程生命周期内复用同一个 ID，
/// 让 Anthropic 后端看到的是连续会话而非每请求换人。
static USER_ID_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// 生成 Claude Code 格式的假 user_id：`user_<64hex>_account_<uuid>_session_<uuid>`。
fn generate_fake_user_id() -> String {
    let mut bytes = [0u8; 32];
    // 用两个 v4 UUID 凑 32 字节随机源（避免额外引 rand 依赖）。
    bytes[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let hex_part = hex::encode(bytes);
    let account = uuid::Uuid::new_v4();
    let session = uuid::Uuid::new_v4();
    format!("user_{hex_part}_account_{account}_session_{session}")
}

/// 取某账号缓存的假 user_id，没有则生成并缓存。
fn cached_user_id(account_id: &str) -> String {
    let mut guard = USER_ID_CACHE.lock().unwrap();
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(existing) = map.get(account_id) {
        return existing.clone();
    }
    let id = generate_fake_user_id();
    map.insert(account_id.to_string(), id.clone());
    id
}

/// 校验是否为 Claude Code 格式的 user_id（照搬参考 `cloak_utils.go` 的正则，手写避免引 regex）。
/// 形如 `user_<64 hex>_account_<uuid>_session_<uuid>`。
fn is_valid_user_id(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("user_") else {
        return false;
    };
    let Some((hex_part, rest)) = rest.split_once("_account_") else {
        return false;
    };
    if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let Some((account, session)) = rest.split_once("_session_") else {
        return false;
    };
    is_uuid(account) && is_uuid(session)
}

/// 校验标准小写 UUID（8-4-4-4-12，hex）。
fn is_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != groups.len() {
        return false;
    }
    parts
        .iter()
        .zip(groups.iter())
        .all(|(p, &len)| p.len() == len && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// 注入/修正 `metadata.user_id`：缺失或格式不合法时写入按账号缓存的假 ID。
fn inject_fake_user_id(body: &mut Value, account_id: &str) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let valid = obj
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
        .map(is_valid_user_id)
        .unwrap_or(false);
    if valid {
        return;
    }
    let id = cached_user_id(account_id);
    match obj.get_mut("metadata") {
        Some(Value::Object(m)) => {
            m.insert("user_id".to_string(), json!(id));
        }
        _ => {
            obj.insert("metadata".to_string(), json!({"user_id": id}));
        }
    }
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

    // messages[].content[]：tool_use.name / tool_reference.tool_name，
    // 以及 tool_result.content[] 里嵌套的 tool_reference.tool_name。
    if let Some(Value::Array(messages)) = obj.get_mut("messages") {
        for msg in messages.iter_mut() {
            let Some(Value::Array(content)) = msg.get_mut("content") else {
                continue;
            };
            for part in content.iter_mut() {
                rename_content_part(part, &mut reverse);
            }
        }
    }

    reverse
}

/// 改写单个 content 块里的工具名：
///  - `tool_use` → 改 `name`
///  - `tool_reference` → 改 `tool_name`
///  - `tool_result` → 递归改其 `content[]` 里嵌套的 `tool_reference.tool_name`
fn rename_content_part(part: &mut Value, reverse: &mut HashMap<String, String>) {
    let Some(p) = part.as_object_mut() else {
        return;
    };
    match p.get("type").and_then(|v| v.as_str()) {
        Some("tool_use") => rename_obj_field(p, "name", reverse),
        Some("tool_reference") => rename_obj_field(p, "tool_name", reverse),
        Some("tool_result") => {
            if let Some(Value::Array(nested)) = p.get_mut("content") {
                for n in nested.iter_mut() {
                    let Some(np) = n.as_object_mut() else {
                        continue;
                    };
                    if np.get("type").and_then(|v| v.as_str()) == Some("tool_reference") {
                        rename_obj_field(np, "tool_name", reverse);
                    }
                }
            }
        }
        _ => {}
    }
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

/// 还原非流式响应里的工具名（content[] 里的 tool_use.name / tool_reference.tool_name）。
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
        let key = match p.get("type").and_then(|v| v.as_str()) {
            Some("tool_use") => "name",
            Some("tool_reference") => "tool_name",
            _ => continue,
        };
        if let Some(name) = p.get(key).and_then(|v| v.as_str()).map(|s| s.to_string()) {
            if let Some(orig) = reverse.get(&name) {
                p.insert(key.to_string(), json!(orig));
            }
        }
    }
}

/// 还原流式 SSE 行里的工具名（content_block_start 的 tool_use.name / tool_reference.tool_name）。
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
    let key = match cb.get("type").and_then(|x| x.as_str()) {
        Some("tool_use") => "name",
        Some("tool_reference") => "tool_name",
        _ => return None,
    };
    let name = cb.get(key).and_then(|x| x.as_str())?.to_string();
    let orig = reverse.get(&name)?;
    cb.insert(key.to_string(), json!(orig));
    Some(format!("data: {v}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injects_identifier_and_billing_blocks() {
        let mut body = json!({"system": "be helpful", "messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        let sys = body.get("system").unwrap().as_array().unwrap();
        // system[0] = billing header, system[1] = 身份标识, system[2] = 用户原 system。
        assert!(sys[0]["text"].as_str().unwrap().starts_with(BILLING_PREFIX));
        assert_eq!(sys[1]["text"], AGENT_IDENTIFIER);
        assert_eq!(sys[2]["text"], "be helpful");
    }

    #[test]
    fn billing_header_has_expected_shape() {
        let mut body = json!({"system": "hello world prompt text here", "messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        let billing = body["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains(&format!("cc_version={CLAUDE_CODE_VERSION}.")));
        assert!(billing.contains("cc_entrypoint=cli;"));
        assert!(billing.contains("cch="));
    }

    #[test]
    fn billing_header_not_double_injected() {
        let mut body = json!({"system": "be helpful", "messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        let len_after_first = body["system"].as_array().unwrap().len();
        // 再跑一次：已带 billing 前缀，不应重复注入身份/计费块。
        apply_oauth_cloaking(&mut body, "acc-1");
        assert_eq!(body["system"].as_array().unwrap().len(), len_after_first);
    }

    #[test]
    fn injects_fake_user_id_when_missing() {
        let mut body = json!({"messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        let uid = body["metadata"]["user_id"].as_str().unwrap();
        assert!(is_valid_user_id(uid), "generated id should be valid: {uid}");
    }

    #[test]
    fn keeps_valid_user_id() {
        let valid = "user_".to_string()
            + &"a".repeat(64)
            + "_account_00000000-0000-0000-0000-000000000000"
            + "_session_11111111-1111-1111-1111-111111111111";
        let mut body = json!({"metadata": {"user_id": valid.clone()}, "messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        assert_eq!(body["metadata"]["user_id"], json!(valid));
    }

    #[test]
    fn replaces_invalid_user_id() {
        let mut body = json!({"metadata": {"user_id": "garbage"}, "messages": []});
        apply_oauth_cloaking(&mut body, "acc-1");
        let uid = body["metadata"]["user_id"].as_str().unwrap();
        assert!(is_valid_user_id(uid));
    }

    #[test]
    fn cached_user_id_stable_per_account() {
        let a1 = cached_user_id("acc-stable");
        let a2 = cached_user_id("acc-stable");
        assert_eq!(a1, a2);
        let b = cached_user_id("acc-other");
        assert_ne!(a1, b);
    }

    #[test]
    fn user_id_validation_rejects_malformed() {
        assert!(!is_valid_user_id("user_short_account_x_session_y"));
        assert!(!is_valid_user_id("nope"));
        assert!(!is_valid_user_id(&("user_".to_string() + &"a".repeat(63))));
    }

    #[test]
    fn ensure_max_tokens_injects_when_missing() {
        let mut body = json!({"messages": []});
        normalize_request_body(&mut body);
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn ensure_max_tokens_keeps_valid_value() {
        let mut body = json!({"max_tokens": 4096, "messages": []});
        normalize_request_body(&mut body);
        assert_eq!(body["max_tokens"], json!(4096));
    }

    #[test]
    fn ensure_max_tokens_replaces_non_positive() {
        let mut body = json!({"max_tokens": 0, "messages": []});
        normalize_request_body(&mut body);
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
    }

    #[test]
    fn forced_tool_choice_strips_thinking() {
        let mut body = json!({
            "max_tokens": 4096,
            "tool_choice": {"type": "any"},
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "output_config": {"effort": "high"},
        });
        normalize_request_body(&mut body);
        assert!(body.get("thinking").is_none());
        // output_config 只有 effort，清掉后整个对象删除。
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn forced_tool_choice_keeps_other_output_config() {
        let mut body = json!({
            "max_tokens": 4096,
            "tool_choice": {"type": "tool", "name": "bash"},
            "thinking": {"type": "enabled"},
            "output_config": {"effort": "high", "other": "keep"},
        });
        normalize_request_body(&mut body);
        assert!(body.get("thinking").is_none());
        assert!(body["output_config"].get("effort").is_none());
        assert_eq!(body["output_config"]["other"], "keep");
    }

    #[test]
    fn auto_tool_choice_keeps_thinking() {
        let mut body = json!({
            "max_tokens": 4096,
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "enabled", "budget_tokens": 2000},
        });
        normalize_request_body(&mut body);
        // auto 允许 thinking 共存，不应删除。
        assert!(body.get("thinking").is_some());
    }

    #[test]
    fn thinking_forces_temperature_to_one() {
        let mut body = json!({
            "max_tokens": 4096,
            "temperature": 0.7,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
        });
        normalize_request_body(&mut body);
        assert_eq!(body["temperature"], json!(1));
    }

    #[test]
    fn no_temperature_means_no_injection() {
        let mut body = json!({
            "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
        });
        normalize_request_body(&mut body);
        // 客户端没传 temperature 就不应凭空塞一个。
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn temperature_untouched_without_thinking() {
        let mut body = json!({"max_tokens": 4096, "temperature": 0.5});
        normalize_request_body(&mut body);
        assert_eq!(body["temperature"], json!(0.5));
    }

    #[test]
    fn budget_clamped_below_max_tokens() {
        let mut body = json!({
            "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 5000},
        });
        normalize_request_body(&mut body);
        assert_eq!(body["thinking"]["budget_tokens"], json!(999));
    }

    #[test]
    fn budget_clamped_against_injected_default_max() {
        // max_tokens 缺失 → 注入 1024 → budget 1024 应被压到 1023。
        let mut body = json!({
            "thinking": {"type": "enabled", "budget_tokens": 1024},
        });
        normalize_request_body(&mut body);
        assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
        assert_eq!(body["thinking"]["budget_tokens"], json!(1023));
    }

    #[test]
    fn budget_left_alone_when_below_max() {
        let mut body = json!({
            "max_tokens": 8000,
            "thinking": {"type": "enabled", "budget_tokens": 2000},
        });
        normalize_request_body(&mut body);
        assert_eq!(body["thinking"]["budget_tokens"], json!(2000));
    }

    #[test]
    fn does_not_double_inject_identifier() {
        let mut body = json!({"system": [{"type": "text", "text": AGENT_IDENTIFIER}]});
        apply_oauth_cloaking(&mut body, "acc-1");
        let sys = body.get("system").unwrap().as_array().unwrap();
        // 身份标识不重复（仍只有一块），但 billing 会前置成 system[0]。
        assert!(sys[0]["text"].as_str().unwrap().starts_with(BILLING_PREFIX));
        assert_eq!(sys[1]["text"], AGENT_IDENTIFIER);
        let id_count = sys
            .iter()
            .filter(|b| b["text"] == json!(AGENT_IDENTIFIER))
            .count();
        assert_eq!(id_count, 1);
    }

    #[test]
    fn renames_tools_and_restores_response() {
        let mut body = json!({
            "tools": [{"name": "bash"}, {"type": "web_search_20250305", "name": "web_search"}],
            "messages": [{"role": "assistant", "content": [{"type": "tool_use", "name": "edit", "id": "x", "input": {}}]}]
        });
        let reverse = apply_oauth_cloaking(&mut body, "acc-1");
        assert_eq!(body["tools"][0]["name"], "Bash");
        // 内置工具不动
        assert_eq!(body["tools"][1]["name"], "web_search");
        assert_eq!(body["messages"][0]["content"][0]["name"], "Edit");

        let mut resp =
            json!({"content": [{"type": "tool_use", "name": "Bash", "id": "y", "input": {}}]});
        restore_tool_names_in_json(&mut resp, &reverse);
        assert_eq!(resp["content"][0]["name"], "bash");
    }

    #[test]
    fn renames_and_restores_tool_reference() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_reference", "tool_name": "bash"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "tool_reference", "tool_name": "edit"}
                    ]}
                ]
            }]
        });
        let reverse = apply_oauth_cloaking(&mut body, "acc-1");
        assert_eq!(body["messages"][0]["content"][0]["tool_name"], "Bash");
        assert_eq!(
            body["messages"][0]["content"][1]["content"][0]["tool_name"],
            "Edit"
        );

        // 非流式还原
        let mut resp = json!({"content": [{"type": "tool_reference", "tool_name": "Bash"}]});
        restore_tool_names_in_json(&mut resp, &reverse);
        assert_eq!(resp["content"][0]["tool_name"], "bash");

        // 流式还原
        let line = r#"data: {"type":"content_block_start","content_block":{"type":"tool_reference","tool_name":"Edit"}}"#;
        let restored = restore_tool_names_in_sse_line(line, &reverse).unwrap();
        assert!(restored.contains("\"tool_name\":\"edit\""));
    }
}
