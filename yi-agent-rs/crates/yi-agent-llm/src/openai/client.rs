//! OpenaiProvider and Provider trait implementation.

use std::env;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use yi_agent_core::{Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason};

use crate::openai::error::map_status_error;
use crate::openai::stream::OpenaiStream;
use crate::openai::types::OpenaiRequest;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Configuration for constructing an [`OpenaiProvider`].
///
/// All fields optional — resolved with the following priority:
/// 1. Explicit value here
/// 2. Environment variable (`OPENAI_BASE_URL`, `OPENAI_API_KEY`)
/// 3. Built-in default
#[derive(Default)]
pub struct OpenaiProviderOpts {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout: Option<Duration>,
}

pub struct OpenaiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl std::fmt::Debug for OpenaiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenaiProvider")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl OpenaiProvider {
    /// Construct a provider.
    ///
    /// `api_key` resolution: `opts.api_key` > `OPENAI_API_KEY` env var > error.
    pub fn new(opts: OpenaiProviderOpts) -> Result<Self, ProviderError> {
        let api_key = opts
            .api_key
            .or_else(|| env::var("OPENAI_API_KEY").ok())
            .ok_or_else(|| {
                ProviderError::Auth("OPENAI_API_KEY not set and no api_key provided".into())
            })?;

        let base_url = opts
            .base_url
            .or_else(|| env::var("OPENAI_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let timeout = opts
            .timeout
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_TIMEOUT_SECS));

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            api_key,
            timeout,
        })
    }
}

#[async_trait]
impl Provider for OpenaiProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        let model = req.model.clone();
        let msg_count = req.messages.len();
        tracing::info!(provider = "openai", model = %model, msg_count, "sending request");

        let body: OpenaiRequest = req.into();

        let resp = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(provider = "openai", error = %e, "request failed");
                ProviderError::Network(e.to_string())
            })?;

        let status = resp.status();
        tracing::info!(provider = "openai", status = %status, "response received");

        if !resp.status().is_success() {
            tracing::warn!(provider = "openai", status = %status, "error response");
            return Err(map_status_error(resp).await);
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = OpenaiStream::new(byte_stream);

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
