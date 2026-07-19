//! yi-agent-llm: LLM provider implementations.
//!
//! 依赖 `yi-agent-core` 的 `Provider` trait,初期实现 Anthropic Claude provider,
//! 架构上预留多 provider 扩展能力。

pub mod anthropic;

pub use anthropic::client::AnthropicProvider;
pub use anthropic::client::AnthropicProviderOpts;
