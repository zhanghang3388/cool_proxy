use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub auth_dir: PathBuf,
    pub api_keys: Vec<String>,
    pub admin_token: String,
    pub upstream: UpstreamConfig,
    pub retry: RetryConfig,
    pub token_refresh: TokenRefreshConfig,
    #[serde(default)]
    pub log: LogConfig,
    #[serde(default)]
    pub kiro: KiroConfig,
}

/// Kiro 反代专属配置（system prompt 过滤，对齐 KAM gateway/prompt_filter.rs）。
/// 全部可选：老配置文件不写也能跑（serde default）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct KiroConfig {
    /// 检测到 Claude Code 系统提示（≥2 个特征标记）时，整体替换成精简后端提示。
    /// 较激进（会丢掉 CC 的工具说明等），默认关。仍命中身份类 403 时可打开。
    #[serde(default)]
    pub filter_claude_code: bool,
    /// 去掉 `--- SYSTEM PROMPT ---` 之类边界标记。安全，默认开。
    #[serde(default = "default_true")]
    pub strip_boundaries: bool,
    /// 去掉环境/身份噪音行（gitStatus / Recent commits / knowledge cutoff /
    /// "you are claude code" / # Environment 段等）。默认开。
    #[serde(default = "default_true")]
    pub env_noise: bool,
    /// 合成 prompt-cache 计费：Kiro 上游不支持缓存，但按月请求数限额（不按 token 计费），
    /// 故按 Claude Code 自带的 cache_control 断点，把上游真实 input 拆成 cache_read /
    /// cache_creation / fresh 写入 usage，让下游（cool_api）像 claude 渠道一样按缓存计费。
    /// 只「拆分」真实总量、不放大，默认开。
    #[serde(default = "default_true")]
    pub synth_cache: bool,
    /// 透明上下文压缩：Kiro 上游输入有服务端硬上限，超限会被拒（"Input is too long"）。
    /// 开启后发送前估算输入 token，超过 `compact_threshold_tokens` 时自动：① 截断超大
    /// tool_result 块；② 丢弃最旧历史轮次，压回限内再上送。有损（模型看不到被丢弃的旧
    /// 上下文），但避免「数据过长」报错。默认开。
    #[serde(default = "default_true")]
    pub compact: bool,
    /// 触发压缩的输入 token 阈值（估算，约 3 字节/token）。应设为略低于 Kiro 实测上限。
    /// 默认 120000；可借日志里的 `est_tokens` 跨请求标定后调整。
    #[serde(default = "default_compact_threshold")]
    pub compact_threshold_tokens: u32,
    /// 单个 tool_result 块允许的最大 token，超出截断保留头部。默认 4000。
    #[serde(default = "default_tool_result_max")]
    pub tool_result_max_tokens: u32,
    /// 压缩丢弃历史时至少保留最近多少轮对话（1 轮≈user+assistant 两条）。默认 8。
    #[serde(default = "default_keep_recent_turns")]
    pub keep_recent_turns: u32,
}

fn default_compact_threshold() -> u32 {
    120_000
}

fn default_tool_result_max() -> u32 {
    4_000
}

fn default_keep_recent_turns() -> u32 {
    8
}

fn default_true() -> bool {
    true
}

impl Default for KiroConfig {
    fn default() -> Self {
        Self {
            filter_claude_code: false,
            strip_boundaries: true,
            env_noise: true,
            synth_cache: true,
            compact: true,
            compact_threshold_tokens: default_compact_threshold(),
            tool_result_max_tokens: default_tool_result_max(),
            keep_recent_turns: default_keep_recent_turns(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UpstreamConfig {
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    pub max_retries: u32,
    pub cooldown_seconds: u64,
    pub long_cooldown_seconds: u64,
    pub failure_threshold: u32,
    /// 连续 N 次 transient/网络失败后才进短冷却。默认 3。
    #[serde(default = "default_transient_threshold")]
    pub transient_threshold: u32,
    /// 紧急逃生开关：true 时所有冷却写入都跳过，号永远可用，方便定位上游真实错误。
    #[serde(default)]
    pub disable_cooldown: bool,
}

fn default_transient_threshold() -> u32 {
    3
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TokenRefreshConfig {
    pub scan_interval_seconds: u64,
    pub refresh_before_expire_seconds: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
        }
    }
}

impl Config {
    pub fn load(path: &PathBuf) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("read config file {:?}", path))?;
        let cfg: Config = serde_yaml::from_str(&content).with_context(|| "parse yaml config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.api_keys.is_empty() {
            anyhow::bail!("api_keys must not be empty");
        }
        if self.admin_token.trim().is_empty() {
            anyhow::bail!("admin_token must not be empty");
        }
        if self.retry.failure_threshold == 0 {
            anyhow::bail!("retry.failure_threshold must be > 0");
        }
        Ok(())
    }

    pub fn bind_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
