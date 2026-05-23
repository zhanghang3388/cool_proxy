//! OpenAI ChatCompletion ↔ codex /responses 翻译层。
//!
//! 入口：
//! - [`translate_request`]：把 `/v1/chat/completions` 形状翻译成 codex `/responses`。
//! - [`StreamTranslator`]：流式响应翻译器（codex SSE → OpenAI chat.completion.chunk）。
//! - [`Aggregator`]：非流式聚合器（codex SSE → 单个 chat.completion 对象）。

pub mod request;
pub mod response_aggregate;
pub mod response_stream;
pub mod tool_names;

pub use request::translate_request;
pub use response_aggregate::Aggregator;
pub use response_stream::StreamTranslator;

#[cfg(test)]
mod tests;
