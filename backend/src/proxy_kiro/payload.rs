//! Kiro / CodeWhisperer `generateAssistantResponse` 请求体的数据结构 + 构造逻辑，
//! 以及模型 id 映射。结构与字段名严格对齐上游 schema（camelCase）。

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// 默认模型（未知 model 兜底）。
pub const DEFAULT_MODEL_ID: &str = "claude-sonnet-4.5";

/// 把客户端传来的 model 名映射成 Kiro 接受的 model id。
/// 规则：CW 内部 ID（全大写带下划线）原样透传；显式 alias 命中映射表；
/// 看似 `claude-{sonnet|haiku|opus}-*` 的新模型原样透传；否则兜底默认。
pub fn map_model_id(model: &str) -> String {
    let m = model.trim();
    if m.is_empty() {
        return DEFAULT_MODEL_ID.to_string();
    }
    if is_codewhisperer_model_id(m) {
        return m.to_string();
    }
    let lower = m.to_ascii_lowercase();
    let mapped = match lower.as_str() {
        "claude-sonnet-4-5" | "claude-sonnet-4.5" => Some("claude-sonnet-4.5"),
        "claude-haiku-4-5" | "claude-haiku-4.5" => Some("claude-haiku-4.5"),
        "claude-opus-4-5" | "claude-opus-4.5" => Some("claude-opus-4.5"),
        "claude-sonnet-4" | "claude-sonnet-4-20250514" => Some("claude-sonnet-4"),
        "claude-3-5-sonnet" | "claude-3-opus" => Some("claude-sonnet-4.5"),
        "claude-3-sonnet" => Some("claude-sonnet-4"),
        "claude-3-haiku" => Some("claude-haiku-4.5"),
        "gpt-4" | "gpt-4o" | "gpt-4-turbo" | "gpt-3.5-turbo" => Some("claude-sonnet-4.5"),
        _ => None,
    };
    if let Some(v) = mapped {
        return v.to_string();
    }
    // 看似 Kiro 支持的新 Claude 模型：原样透传
    if lower.starts_with("claude-sonnet-")
        || lower.starts_with("claude-haiku-")
        || lower.starts_with("claude-opus-")
    {
        return m.to_string();
    }
    DEFAULT_MODEL_ID.to_string()
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
