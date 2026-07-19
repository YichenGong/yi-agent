//! AnthropicProvider and Provider trait implementation.

use std::env;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason};

use crate::anthropic::error::map_status_error;
use crate::anthropic::stream::AnthropicStream;
use crate::anthropic::types::AnthropicRequest;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Configuration for constructing an [`AnthropicProvider`].
///
/// All fields optional — resolved with the following priority:
/// 1. Explicit value here
/// 2. Environment variable (`ANTHROPIC_BASE_URL`, `ANTHROPIC_API_KEY`)
/// 3. Built-in default
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
    /// `api_key` resolution: `opts.api_key` > `ANTHROPIC_API_KEY` env var > error.
    pub fn new(opts: AnthropicProviderOpts) -> Result<Self, ProviderError> {
        let api_key = opts
            .api_key
            .or_else(|| env::var("ANTHROPIC_API_KEY").ok())
            .ok_or_else(|| {
                ProviderError::Auth("ANTHROPIC_API_KEY not set and no api_key provided".into())
            })?;

        let base_url = opts
            .base_url
            .or_else(|| env::var("ANTHROPIC_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let api_version = opts
            .api_version
            .unwrap_or_else(|| DEFAULT_API_VERSION.to_string());

        let timeout = opts.timeout.unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let client = reqwest::Client::builder()
            .timeout(timeout)
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
        let body: AnthropicRequest = req.into();

        let resp = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.api_version)
            .header("content-type", "application/json")
            .json(&body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| ProviderError::Network(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(map_status_error(resp).await);
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = AnthropicStream::new(byte_stream);
        // Map Result<ProviderEvent, ProviderError> → ProviderEvent.
        // Mid-stream errors become a terminal Stop event with the error message.
        //
        // `scan` carries a `Some(())` token that is consumed when the first
        // `Stop` event is seen; once consumed (state becomes `None`), the
        // stream yields `None` forever. This guarantees the stream terminates
        // after the first Stop — whether from `message_delta` or from a
        // converted mid-stream error — preventing spurious events from
        // arriving after an error or natural termination.
        let mapped = event_stream
            .map(|item| match item {
                Ok(event) => event,
                Err(e) => ProviderEvent::Stop {
                    reason: StopReason::Other(format!("stream error: {e}")),
                },
            })
            .scan(Some(()), |state, event| {
                let yield_event = state.is_some();
                if matches!(event, ProviderEvent::Stop { .. }) {
                    *state = None;
                }
                std::future::ready(if yield_event { Some(event) } else { None })
            });
        Ok(mapped.boxed())
    }
}
