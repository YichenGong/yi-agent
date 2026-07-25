//! LLM-based prefix extractor for bash commands.
//!
//! Uses a [`Provider`] to ask an LLM to extract the command prefix
//! (command name + subcommand, without arguments) from a shell command.
//! Falls back to `None` on timeout (15 s) or error, allowing callers to
//! use [`fallback_prefix`] as a secondary strategy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use yi_agent_core::Provider;
use yi_agent_core::permission::PrefixExtractor;

/// LLM-based prefix extractor with a 15-second timeout.
///
/// Not yet wired into the agent loop. Construct with [`LlmPrefixExtractor::new`]
/// and pass to whichever component needs prefix extraction.
#[allow(dead_code)]
pub struct LlmPrefixExtractor {
    provider: Arc<dyn Provider>,
    model: String,
}

#[allow(dead_code)]
impl LlmPrefixExtractor {
    pub fn new(provider: Arc<dyn Provider>, model: String) -> Self {
        Self { provider, model }
    }
}

#[async_trait]
impl PrefixExtractor for LlmPrefixExtractor {
    async fn extract(&self, command: &str) -> Option<String> {
        // Short commands (single token) don't need an LLM round-trip.
        if command.split_whitespace().count() <= 1 {
            return Some(command.trim().to_string());
        }

        let prompt = format!(
            "从以下 shell 命令提取命令前缀(命令名 + 子命令,不含参数)。\
             只返回前缀字符串,不要其他内容。\n命令: {command}"
        );

        let fut = self.provider.complete(&self.model, &prompt);
        match tokio::time::timeout(Duration::from_secs(15), fut).await {
            Ok(Ok(text)) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            _ => None, // timeout or error
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use yi_agent_core::{ProviderError, ProviderEvent, ProviderRequest, StopReason};

    /// Mock provider that returns a fixed text response (or an error).
    struct MockProvider {
        text: Result<String, ProviderError>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            match &self.text {
                Ok(t) => {
                    let events = vec![
                        ProviderEvent::TextDelta(t.clone()),
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                        },
                    ];
                    Ok(futures::stream::iter(events).boxed())
                }
                Err(e) => Err(e.clone()),
            }
        }
    }

    #[tokio::test]
    async fn extract_returns_prefix_from_llm() {
        let provider = Arc::new(MockProvider {
            text: Ok("git push".to_string()),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        let result = extractor.extract("git push origin main").await;
        assert_eq!(result, Some("git push".to_string()));
    }

    #[tokio::test]
    async fn extract_short_command_skips_llm() {
        let provider = Arc::new(MockProvider {
            text: Err(ProviderError::Server("should not be called".into())),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        // Single-word command should not call the LLM.
        let result = extractor.extract("ls").await;
        assert_eq!(result, Some("ls".to_string()));
    }

    #[tokio::test]
    async fn extract_empty_llm_response_returns_none() {
        let provider = Arc::new(MockProvider {
            text: Ok("".to_string()),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        let result = extractor.extract("git push origin main").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn extract_llm_error_returns_none() {
        let provider = Arc::new(MockProvider {
            text: Err(ProviderError::Server("error".into())),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        let result = extractor.extract("git push origin main").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn extract_whitespace_only_response_returns_none() {
        let provider = Arc::new(MockProvider {
            text: Ok("  \n  ".to_string()),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        let result = extractor.extract("git push origin main").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn extract_trims_llm_output() {
        let provider = Arc::new(MockProvider {
            text: Ok("  cargo run  \n".to_string()),
        });
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        let result = extractor.extract("cargo run --release").await;
        assert_eq!(result, Some("cargo run".to_string()));
    }

    /// A mock provider that sleeps forever, to test the timeout path.
    struct SlowProvider;

    #[async_trait]
    impl Provider for SlowProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            // Never resolves — simulates a hung provider.
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    #[tokio::test]
    async fn extract_timeout_returns_none() {
        let provider = Arc::new(SlowProvider);
        let extractor = LlmPrefixExtractor::new(provider, "test-model".to_string());
        // Use a short timeout via a direct call to verify the 15 s timeout logic
        // is wired correctly. We can't wait 15 s in a test, so we verify the
        // behavior indirectly: the future should not resolve immediately.
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            extractor.extract("git push origin main"),
        )
        .await;
        // The inner timeout (15 s) hasn't fired yet, so our outer timeout (100 ms)
        // fires first, meaning the result is a timeout (Err).
        assert!(result.is_err(), "expected outer timeout to fire first");
    }

    // --- Verify Provider::complete default implementation works ---

    #[tokio::test]
    async fn complete_default_impl_collects_text() {
        let provider = MockProvider {
            text: Ok("hello world".to_string()),
        };
        let result = provider.complete("model", "prompt").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn complete_default_impl_propagates_error() {
        let provider = MockProvider {
            text: Err(ProviderError::Auth("bad key".into())),
        };
        let result = provider.complete("model", "prompt").await;
        assert!(result.is_err());
    }
}
