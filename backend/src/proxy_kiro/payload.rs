//! Kiro / CodeWhisperer `generateAssistantResponse` 请求体的数据结构 + 构造逻辑，
//! 以及模型 id 映射。结构与字段名严格对齐上游 schema（camelCase）。

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// 默认模型（未知 model 兜底）。
pub const DEFAULT_MODEL_ID: &str = "claude-sonnet-4.5";

/// 把客户端传来的 model 名映射成 Kiro 接受的 model id。
///
/// 步骤（对齐 KAM `get_internal_model_id` + `normalize_claude_model_format`）：
///  1. CW 内部 ID（全大写带下划线）原样透传；
///  2. 轻量规整：小写、去 `-thinking` 后缀、去 `-YYYYMMDD` 日期后缀；
///  3. 显式别名（简写 / 3.x 旧版 / GPT / 开源 / auto）命中即返回；
///  4. 版本号横杠转点号：`claude-opus-4-8` → `claude-opus-4.8`；
///  5. 命中 Kiro 支持的格式才透传，否则兜底默认 —— 避免把未知 modelId
///     （如 Claude Code 默认发的 `claude-sonnet-4-5-20250929`）丢给上游直接 400。
pub fn map_model_id(model: &str) -> String {
    let m = model.trim();
    if m.is_empty() {
        return DEFAULT_MODEL_ID.to_string();
    }
    // 1. CW 内部 ID 原样透传
    if is_codewhisperer_model_id(m) {
        return m.to_string();
    }
    // 2. 规整
    let canon = strip_variant_suffixes(&m.to_ascii_lowercase());
    // 3. 显式别名
    if let Some(v) = explicit_model_alias(&canon) {
        return v.to_string();
    }
    // 4. 横杠版本号转点号
    let normalized = dash_version_to_dot(&canon);
    // 5. 校验 + 兜底
    if is_kiro_supported_model_format(&normalized) {
        normalized
    } else {
        DEFAULT_MODEL_ID.to_string()
    }
}

/// 去掉 `-thinking` 后缀与 `-YYYYMMDD`（8 位、以 20 开头）日期后缀。
/// thinking 由 `thinking` 参数 / `additionalModelRequestFields` 控制，modelId 不应带它。
fn strip_variant_suffixes(model: &str) -> String {
    let mut s = model;
    if let Some(stripped) = s.strip_suffix("-thinking") {
        s = stripped;
    }
    // `-20xxxxxx`：'-' + 8 位数字，且以 "20" 打头
    if s.len() > 9 {
        let tail = &s[s.len() - 9..];
        if let Some(digits) = tail.strip_prefix('-') {
            if digits.len() == 8
                && digits.starts_with("20")
                && digits.bytes().all(|b| b.is_ascii_digit())
            {
                s = &s[..s.len() - 9];
            }
        }
    }
    s.to_string()
}

/// 显式别名表（已规整为小写、无 thinking/日期后缀）。
fn explicit_model_alias(model: &str) -> Option<&'static str> {
    Some(match model {
        "auto" | "default" => "auto",
        "opus" => "claude-opus-4.5",
        "sonnet" => "claude-sonnet-4.5",
        "haiku" => "claude-haiku-4.5",
        "claude-sonnet-4-5" | "claude-sonnet-4.5" | "claude-sonnet-latest" => "claude-sonnet-4.5",
        "claude-haiku-4-5" | "claude-haiku-4.5" => "claude-haiku-4.5",
        "claude-opus-4-5" | "claude-opus-4.5" => "claude-opus-4.5",
        "claude-sonnet-4" => "claude-sonnet-4",
        "claude-3-5-sonnet" | "claude-3-5-sonnet-latest" | "claude-3-opus" | "claude-3-opus-latest" => {
            "claude-sonnet-4.5"
        }
        "claude-3-sonnet" => "claude-sonnet-4",
        "claude-3-haiku" | "claude-3-5-haiku" => "claude-haiku-4.5",
        "gpt-4" | "gpt-4o" | "gpt-4-turbo" | "gpt-3.5-turbo" | "gpt-4o-mini" => "claude-sonnet-4.5",
        "deepseek" | "deepseek-3-2" | "deepseek-3.2" => "deepseek-3.2",
        "minimax" | "minimax-m2-5" | "minimax-m2.5" => "minimax-m2.5",
        "minimax-m2-1" | "minimax-m2.1" => "minimax-m2.1",
        "glm-5" | "glm5" => "glm-5",
        "qwen" | "qwen3" | "qwen3-coder" | "qwen3-coder-next" => "qwen3-coder-next",
        _ => return None,
    })
}

/// 末尾 `-{major}-{minor}`（两个单数字）转 `-{major}.{minor}`。
/// 例：`claude-opus-4-8` → `claude-opus-4.8`，`claude-sonnet-4-6` → `claude-sonnet-4.6`。
fn dash_version_to_dot(model: &str) -> String {
    if let Some(last_dash) = model.rfind('-') {
        let after = &model[last_dash + 1..];
        if after.len() == 1 && after.bytes().all(|b| b.is_ascii_digit()) {
            let prefix = &model[..last_dash];
            if let Some(second_last) = prefix.rfind('-') {
                let between = &prefix[second_last + 1..];
                if between.len() == 1 && between.bytes().all(|b| b.is_ascii_digit()) {
                    return format!("{}.{}", &model[..last_dash], after);
                }
            }
        }
    }
    model.to_string()
}

/// 是否符合 Kiro 接受的 modelId 格式（对齐 KAM `is_kiro_supported_model_format`）。
fn is_kiro_supported_model_format(model: &str) -> bool {
    if model == "auto" {
        return true;
    }
    if model.starts_with("claude-sonnet-")
        || model.starts_with("claude-haiku-")
        || model.starts_with("claude-opus-")
    {
        return true;
    }
    matches!(
        model,
        "deepseek-3.2" | "minimax-m2.5" | "minimax-m2.1" | "glm-5" | "qwen3-coder-next"
    )
}

fn is_codewhisperer_model_id(model: &str) -> bool {
    model.contains('_')
        && model
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// thinking 参数只对 Claude 4+ 系列生效（其它模型 schema 没这字段，传了会 400）。
pub fn model_supports_thinking(model_id: &str) -> bool {
    let l = model_id.to_ascii_lowercase();
    l.contains("claude-sonnet-4") || l.contains("claude-opus-4") || l.contains("claude-haiku-4")
}

// ===== Kiro payload 结构 =====

#[derive(Debug, Clone, Serialize)]
pub struct KiroImageSource {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroImage {
    pub format: String,
    pub source: KiroImageSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroToolResultContent {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroToolResult {
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    pub content: Vec<KiroToolResultContent>,
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroToolSpecification {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: KiroInputSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroInputSchema {
    pub json: Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroToolWrapper {
    #[serde(rename = "toolSpecification")]
    pub tool_specification: KiroToolSpecification,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroToolUse {
    #[serde(rename = "toolUseId")]
    pub tool_use_id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KiroUserInputMessageContext {
    #[serde(rename = "toolResults", skip_serializing_if = "Option::is_none")]
    pub tool_results: Option<Vec<KiroToolResult>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<KiroToolWrapper>>,
}

impl KiroUserInputMessageContext {
    fn is_empty(&self) -> bool {
        self.tool_results.is_none() && self.tools.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroUserInputMessage {
    pub content: String,
    #[serde(rename = "modelId", skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub origin: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<KiroImage>>,
    #[serde(rename = "userInputMessageContext", skip_serializing_if = "Option::is_none")]
    pub context: Option<KiroUserInputMessageContext>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroAssistantResponseMessage {
    pub content: String,
    #[serde(rename = "toolUses", skip_serializing_if = "Option::is_none")]
    pub tool_uses: Option<Vec<KiroToolUse>>,
}

/// history 里的一条：要么 user 要么 assistant（互斥）。
#[derive(Debug, Clone, Serialize)]
pub struct KiroHistoryMessage {
    #[serde(rename = "userInputMessage", skip_serializing_if = "Option::is_none")]
    pub user_input_message: Option<KiroUserInputMessage>,
    #[serde(rename = "assistantResponseMessage", skip_serializing_if = "Option::is_none")]
    pub assistant_response_message: Option<KiroAssistantResponseMessage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroCurrentMessage {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroConversationState {
    #[serde(rename = "agentContinuationId")]
    pub agent_continuation_id: String,
    #[serde(rename = "agentTaskType")]
    pub agent_task_type: String,
    #[serde(rename = "chatTriggerType")]
    pub chat_trigger_type: String,
    #[serde(rename = "conversationId")]
    pub conversation_id: String,
    #[serde(rename = "currentMessage")]
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<KiroHistoryMessage>>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct KiroInferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
}

impl KiroInferenceConfig {
    fn is_empty(&self) -> bool {
        self.max_tokens.is_none() && self.temperature.is_none() && self.top_p.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct KiroPayload {
    #[serde(rename = "conversationState")]
    pub conversation_state: KiroConversationState,
    #[serde(rename = "profileArn", skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
    #[serde(rename = "additionalModelRequestFields", skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<Value>,
}

/// 构造 payload 的入参集合（由 translator 填好）。
pub struct BuildArgs {
    pub content: String,
    pub model_id: String,
    pub origin: String,
    pub history: Vec<KiroHistoryMessage>,
    pub tools: Vec<KiroToolWrapper>,
    pub tool_results: Vec<KiroToolResult>,
    pub images: Vec<KiroImage>,
    pub profile_arn: String,
    pub max_tokens: Option<i64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub thinking: bool,
}

/// 组装最终 KiroPayload。history 已由 translator 保证 user/assistant 交替且以 user 开头。
pub fn build_payload(args: BuildArgs) -> KiroPayload {
    let final_content = if args.content.trim().is_empty() {
        if args.tool_results.is_empty() {
            "Continue".to_string()
        } else {
            "Tool results provided.".to_string()
        }
    } else {
        args.content
    };

    let mut ctx = KiroUserInputMessageContext::default();
    if !args.tool_results.is_empty() {
        ctx.tool_results = Some(args.tool_results);
    }
    if !args.tools.is_empty() {
        ctx.tools = Some(args.tools);
    }

    let current = KiroUserInputMessage {
        content: final_content,
        model_id: Some(args.model_id.clone()),
        origin: args.origin.clone(),
        images: if args.images.is_empty() {
            None
        } else {
            Some(args.images)
        },
        context: if ctx.is_empty() { None } else { Some(ctx) },
    };

    let conversation_id = Uuid::new_v4().to_string();

    let inference = KiroInferenceConfig {
        max_tokens: args.max_tokens,
        temperature: args.temperature,
        top_p: args.top_p,
    };

    let additional = if args.thinking && model_supports_thinking(&args.model_id) {
        Some(serde_json::json!({ "thinking": { "type": "adaptive" } }))
    } else {
        None
    };

    KiroPayload {
        conversation_state: KiroConversationState {
            agent_continuation_id: Uuid::new_v4().to_string(),
            agent_task_type: "vibe".to_string(),
            chat_trigger_type: "MANUAL".to_string(),
            conversation_id,
            current_message: KiroCurrentMessage {
                user_input_message: current,
            },
            history: if args.history.is_empty() {
                None
            } else {
                Some(args.history)
            },
        },
        profile_arn: if args.profile_arn.is_empty() {
            None
        } else {
            Some(args.profile_arn)
        },
        inference_config: if inference.is_empty() {
            None
        } else {
            Some(inference)
        },
        additional_model_request_fields: additional,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_claude_code_default_dated_models_to_dotted_ids() {
        // Claude Code 默认会发带日期后缀的横杠名 —— 旧实现原样透传给 Kiro 导致 400。
        assert_eq!(map_model_id("claude-sonnet-4-5-20250929"), "claude-sonnet-4.5");
        assert_eq!(map_model_id("claude-3-5-haiku-20241022"), "claude-haiku-4.5");
    }

    #[test]
    fn dash_versions_convert_to_dot_passthrough() {
        // 4.6 / 4.7 / 4.8 不在别名表里，靠归一化 + 校验透传（横杠转点号）。
        assert_eq!(map_model_id("claude-opus-4-8"), "claude-opus-4.8");
        assert_eq!(map_model_id("claude-sonnet-4-6"), "claude-sonnet-4.6");
        assert_eq!(map_model_id("claude-opus-4-7"), "claude-opus-4.7");
    }

    #[test]
    fn strips_thinking_suffix_from_model_id() {
        assert_eq!(map_model_id("claude-opus-4-8-thinking"), "claude-opus-4.8");
        assert_eq!(map_model_id("claude-sonnet-4.5-thinking"), "claude-sonnet-4.5");
    }

    #[test]
    fn dotted_and_alias_forms() {
        assert_eq!(map_model_id("claude-sonnet-4.5"), "claude-sonnet-4.5");
        assert_eq!(map_model_id("claude-sonnet-4-5"), "claude-sonnet-4.5");
        assert_eq!(map_model_id("sonnet"), "claude-sonnet-4.5");
        assert_eq!(map_model_id("opus"), "claude-opus-4.5");
        assert_eq!(map_model_id("haiku"), "claude-haiku-4.5");
        assert_eq!(map_model_id("auto"), "auto");
    }

    #[test]
    fn legacy_and_gpt_fall_back_sensibly() {
        assert_eq!(map_model_id("claude-3-5-haiku"), "claude-haiku-4.5");
        assert_eq!(map_model_id("claude-3-opus"), "claude-sonnet-4.5");
        assert_eq!(map_model_id("gpt-4o"), "claude-sonnet-4.5");
    }

    #[test]
    fn open_source_aliases() {
        assert_eq!(map_model_id("deepseek-3.2"), "deepseek-3.2");
        assert_eq!(map_model_id("glm-5"), "glm-5");
        assert_eq!(map_model_id("qwen3-coder"), "qwen3-coder-next");
    }

    #[test]
    fn cw_internal_id_passes_through() {
        assert_eq!(
            map_model_id("CLAUDE_SONNET_4_5_V1_0"),
            "CLAUDE_SONNET_4_5_V1_0"
        );
    }

    #[test]
    fn unknown_model_falls_back_to_default() {
        assert_eq!(map_model_id("totally-made-up"), DEFAULT_MODEL_ID);
        assert_eq!(map_model_id(""), DEFAULT_MODEL_ID);
    }
}
