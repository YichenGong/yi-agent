//! yi-agent-llm: LLM provider implementations.
//!
//! 依赖 `yi-agent-core` 的 `Provider` trait,实现 Anthropic Claude 和 OpenAI provider,
//! 架构上预留多 provider 扩展能力。

pub mod anthropic;
pub mod openai;

pub use anthropic::client::AnthropicProvider;
pub use anthropic::client::AnthropicProviderOpts;
