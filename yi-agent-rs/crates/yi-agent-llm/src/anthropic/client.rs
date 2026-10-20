//! AnthropicProvider and Provider trait implementation.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest};

use crate::anthropic::error::map_status_error;
use crate::anthropic::stream::AnthropicStream;
use crate::anthropic::types::AnthropicRequest;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[cfg(test)]
mod tests {
    use super::DEFAULT_TIMEOUT_SECS;

    #[test]
    fn default_stream_timeout_is_five_minutes() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 300);
    }
}

/// Configuration for constructing an [`AnthropicProvider`].
///
/// All fields optional — resolved with the following priority:
/// 1. Explicit value here
/// 2. Built-in default
///
/// The process environment is never consulted: model configuration has exactly
/// one source of truth (`~/.yi-agent/models.json` resolved through
/// `RuntimeConfig`).
#[derive(Default)]
pub struct AnthropicProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub api_version: Option<String>,
    pub timeout: Option<Duration>,
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    api_version: String,
    timeout: Duration,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicProvider")
            .field("base_url", &self.base_url)
            .field("api_version", &self.api_version)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl AnthropicProvider {
    /// Construct a provider.
    ///
    /// `api_key` comes only from `opts.api_key`; when absent the call fails with
    /// [`ProviderError::Auth`]. The process environment is never consulted.
    pub fn new(opts: AnthropicProviderOpts) -> Result<Self, ProviderError> {
        let api_key = opts.api_key.ok_or_else(|| {
            ProviderError::Auth("no api key provided: configure a model entry in settings".into())
        })?;

        let base_url = opts
            .base_url
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let api_version = opts
            .api_version
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());

        let timeout = opts
            .timeout
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        // 连接要有时限，但**响应体不能有总期限**：`.timeout()` 是「从开始连接
        // 一直到响应体读完」的总期限，它会把一条正常输出的长回答在 300 秒处
        // 硬砍断（真实表现：长回答/长思考到点被截断成 stream error）。
        // 这里要的是「没有输出才计时」：`read_timeout` 按每次读操作计时，读到
        // 数据就重置——服务器一直在流，流就一直跑；真的静默了才判超时。
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            api_key,
            api_version,
            timeout,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let model = req.model.clone();
        let msg_count = req.messages.len();
        tracing::info!(provider = "anthropic", model = %model, msg_count, "sending request");

        let body: AnthropicRequest = req.into();

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(provider = "anthropic", error = %e, "request failed");
                ProviderError::Network(e.to_string())
            })?;

        let status = resp.status();
        tracing::info!(provider = "anthropic", status = %status, "response received");

        if !resp.status().is_success() {
            tracing::warn!(provider = "anthropic", status = %status, "error response");
            return Err(map_status_error(resp).await);
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = AnthropicStream::new(byte_stream);
        // Map Result<ProviderEvent, ProviderError> → ProviderEvent.
        // A mid-stream error keeps its classification as a terminal
        // `StreamError`, so the agent loop can tell a retryable timeout from a
        // non-retryable break without parsing text.
        //
        // `scan` carries a `Some(())` token that is consumed when the first
        // terminal event is seen; once consumed (state becomes `None`), the
        // stream yields `None` forever. This guarantees the stream terminates
        // after the first terminal event — whether from `message_delta` or from
        // a converted mid-stream error — preventing spurious events from
        // arriving after an error or natural termination.
        let mapped = event_stream
            .map(|item| match item {
                Ok(event) => event,
                Err(e) => ProviderEvent::StreamError(e),
            })
            .scan(Some(()), |state, event| {
                let yield_event = state.is_some();
                // Both a natural stop and a transport failure are terminal:
                // either must latch the guard, or a failed stream would keep
                // yielding until the server closes (defeating the deadline).
                if matches!(
                    event,
                    ProviderEvent::Stop { .. } | ProviderEvent::StreamError(_)
                ) {
                    *state = None;
                }
                std::future::ready(if yield_event { Some(event) } else { None })
            });
        Ok(mapped.boxed())
    }
}
