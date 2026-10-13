//! OpenaiProvider and Provider trait implementation.

use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use yi_agent_core::{
    ApiStyle, NoticeLevel, Provider, ProviderError, ProviderEvent, ProviderRequest,
};

use crate::openai::channel::{Channel, ChannelCache, looks_like_missing_route};
use crate::openai::error::{is_effort_rejection, map_status_error_from};
use crate::openai::responses::stream::ResponsesStream;
use crate::openai::responses::types::responses_request_from;
use crate::openai::stream::OpenaiStream;
use crate::openai::types::OpenaiRequest;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_TIMEOUT_SECS: u64 = 300;

#[cfg(test)]
mod tests {
    use super::DEFAULT_TIMEOUT_SECS;

    #[test]
    fn default_stream_timeout_is_five_minutes() {
        assert_eq!(DEFAULT_TIMEOUT_SECS, 300);
    }
}

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
    /// `auto`(默认) | `responses` | `chat`。非法值解析为 `auto`。
    pub api_style: Option<String>,
    /// 推理努力档位;`None` = 不发该字段(零回归默认)。
    pub reasoning_effort: Option<String>,
}

pub struct OpenaiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    timeout: Duration,
    /// 端点风格:显式指定直接定通道,`Auto` 走探测/回退。
    api_style: ApiStyle,
    /// resolve 后的权威 effort:请求未显式指定时由它补入。
    reasoning_effort: Option<String>,
    /// 网关能力判定缓存(`base_url` → 实际通道),进程内共享。
    channel_cache: ChannelCache,
    /// effort 拒绝缓存(`base_url|model|channel`),命中后不再补入 effort。
    effort_reject_cache: Arc<Mutex<HashSet<String>>>,
}

impl std::fmt::Debug for OpenaiProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenaiProvider")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("api_style", &self.api_style)
            .field("reasoning_effort", &self.reasoning_effort)
            .finish_non_exhaustive()
    }
}

/// A non-success POST outcome. The status/body are kept (rather than mapped
/// eagerly) so the `auto` path can inspect them before deciding to fall back.
enum PostError {
    /// Transport-level failure (DNS, connection, timeout) — always terminal.
    Transport(ProviderError),
    /// Non-2xx HTTP status with the body already read for classification.
    Status(u16, String),
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

        let api_style = opts
            .api_style
            .as_deref()
            .map(ApiStyle::parse)
            .unwrap_or(ApiStyle::Auto);

        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            client,
            base_url,
            api_key,
            timeout,
            api_style,
            reasoning_effort: opts.reasoning_effort,
            channel_cache: Arc::new(Mutex::new(HashMap::new())),
            effort_reject_cache: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Decide which endpoint this request should target.
    ///
    /// `Auto` has no side effects here: an unknown gateway defaults to
    /// `Responses`, and the missing-route fallback in `call_stream` caches the
    /// negative result so later requests skip the wasted POST.
    fn resolve_channel(&self) -> Channel {
        match self.api_style {
            ApiStyle::Chat => Channel::Chat,
            ApiStyle::Responses => Channel::Responses,
            ApiStyle::Auto => self
                .channel_cache
                .lock()
                .ok()
                .and_then(|cache| cache.get(&self.base_url).copied())
                .unwrap_or(Channel::Responses),
        }
    }

    fn cache_channel(&self, channel: Channel) {
        if let Ok(mut cache) = self.channel_cache.lock() {
            cache.insert(self.base_url.clone(), channel);
        }
    }

    /// 记住 (base_url, model, channel) 拒绝过 effort,后续请求不再补入。
    fn cache_effort_reject(&self, key: String) {
        if let Ok(mut cache) = self.effort_reject_cache.lock() {
            cache.insert(key);
        }
    }

    /// POST a serialized body to `channel` and map the streamed response into
    /// provider events. Shared by the normal path and the chat fallback.
    async fn post_channel(
        &self,
        channel: Channel,
        body: &serde_json::Value,
    ) -> Result<BoxStream<'static, ProviderEvent>, PostError> {
        let path = match channel {
            Channel::Chat => "/v1/chat/completions",
            Channel::Responses => "/v1/responses",
        };

        let resp = self
            .client
            .post(format!("{}{}", self.base_url, path))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(provider = "openai", error = %e, "request failed");
                PostError::Transport(ProviderError::Network(e.to_string()))
            })?;

        let status = resp.status();
        tracing::info!(provider = "openai", %status, ?channel, "response received");

        if !status.is_success() {
            tracing::warn!(provider = "openai", %status, ?channel, "error response");
            let status_u16 = status.as_u16();
            let body_text = resp.text().await.unwrap_or_default();
            return Err(PostError::Status(status_u16, body_text));
        }

        let byte_stream = resp.bytes_stream();
        let event_stream: BoxStream<'static, ProviderEvent> = match channel {
            Channel::Chat => OpenaiStream::new(byte_stream)
                .map(|item| item.unwrap_or_else(ProviderEvent::StreamError))
                .boxed(),
            Channel::Responses => ResponsesStream::new(byte_stream)
                .map(|item| item.unwrap_or_else(ProviderEvent::StreamError))
                .boxed(),
        };
        Ok(guard_terminal(event_stream))
    }

    /// Resolve the channel, build the body, POST it, and apply the missing-route
    /// fallback. This is today's `call_stream` body minus the effort promotion —
    /// the strip-retry path re-enters *here* so a stripped retry never re-adds
    /// the rejected effort.
    async fn dispatch(
        &self,
        req: &ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, PostError> {
        let model = &req.model;
        let msg_count = req.messages.len();
        let channel = self.resolve_channel();
        tracing::info!(provider = "openai", model = %model, msg_count, ?channel, "sending request");

        let body = channel_body(channel, req).map_err(PostError::Transport)?;
        let (status, body_text) = match self.post_channel(channel, &body).await {
            Ok(stream) => return Ok(stream),
            Err(PostError::Transport(e)) => return Err(PostError::Transport(e)),
            Err(PostError::Status(status, body_text)) => (status, body_text),
        };

        let missing_route = channel == Channel::Responses
            && self.api_style == ApiStyle::Auto
            && looks_like_missing_route(status, &body_text);
        if !missing_route {
            return Err(PostError::Status(status, body_text));
        }

        // 网关无 /v1/responses：缓存 chat，并用同一逻辑请求重投一次。
        tracing::info!(
            provider = "openai",
            status,
            "responses route missing; caching Chat and retrying as chat"
        );
        self.cache_channel(Channel::Chat);

        let chat_body = channel_body(Channel::Chat, req).map_err(PostError::Transport)?;
        let chat_stream = self.post_channel(Channel::Chat, &chat_body).await?;

        let notice = ProviderEvent::Notice {
            level: NoticeLevel::Info,
            text: "该地址不支持 Responses，已自动改用 Chat".into(),
        };
        Ok(futures::stream::once(std::future::ready(notice))
            .chain(chat_stream)
            .boxed())
    }
}

/// 拒绝缓存的键:`base_url|model|channel`。
fn effort_reject_key(base_url: &str, model: &str, channel: Channel) -> String {
    format!("{base_url}|{model}|{channel:?}")
}

/// Serialize the request body for `channel`; both builders read the same
/// normalized `ProviderRequest`, so `reasoning_effort` promotion applies to both.
fn channel_body(
    channel: Channel,
    req: &ProviderRequest,
) -> Result<serde_json::Value, ProviderError> {
    let serialized = match channel {
        Channel::Chat => serde_json::to_value(OpenaiRequest::from(req.clone())),
        Channel::Responses => serde_json::to_value(responses_request_from(req.clone())),
    };
    serialized
        .map_err(|e| ProviderError::InvalidRequest(format!("failed to serialize request: {e}")))
}

/// Latch a terminal guard: after `Stop`/`StreamError` nothing more is yielded
/// (except a trailing `Usage`, which OpenAI sends in a final chunk).
fn guard_terminal(stream: BoxStream<'static, ProviderEvent>) -> BoxStream<'static, ProviderEvent> {
    stream
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
            // Usage can arrive after Stop (OpenAI sends it in a final chunk).
            // Always yield it even after Stop so token counts are captured.
            let yield_event = yield_event || matches!(event, ProviderEvent::Usage(_));
            std::future::ready(if yield_event { Some(event) } else { None })
        })
        .boxed()
}

#[async_trait]
impl Provider for OpenaiProvider {
    async fn call_stream(
        &self,
        mut req: ProviderRequest,
    ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
        // provider 级 effort 是 resolve 后的权威值:请求未显式指定时补入。
        // 该 (base_url,model,channel) 曾拒绝 effort 时跳过补入,（Task 6）。
        let channel = self.resolve_channel();
        let eff_key = effort_reject_key(&self.base_url, &req.model, channel);
        let already_rejected = self
            .effort_reject_cache
            .lock()
            .map(|s| s.contains(&eff_key))
            .unwrap_or(false);
        if !already_rejected
            && self.reasoning_effort.is_some()
            && req.params.reasoning_effort.is_none()
        {
            req.params.reasoning_effort = self.reasoning_effort.clone();
        }
        let had_effort = req.params.reasoning_effort.is_some();

        match self.dispatch(&req).await {
            // 网关拒绝所配 effort:剥离该字段重投一次,并低声告知。
            // 重投走 `dispatch`(而非本函数),因此不会再次补入 effort。
            Err(PostError::Status(status, body))
                if had_effort && is_effort_rejection(status, &body) =>
            {
                self.cache_effort_reject(eff_key);
                tracing::info!(
                    provider = "openai",
                    status,
                    "reasoning effort rejected; retrying without it"
                );
                let mut retry = req.clone();
                retry.params.reasoning_effort = None;
                let stream = match self.dispatch(&retry).await {
                    Ok(s) => s,
                    Err(PostError::Transport(e)) => return Err(e),
                    Err(PostError::Status(status, body)) => {
                        return Err(map_status_error_from(status, &body));
                    }
                };
                let notice = ProviderEvent::Notice {
                    level: NoticeLevel::Info,
                    text: "该模型不支持所配推理档，已忽略".into(),
                };
                Ok(futures::stream::once(std::future::ready(notice))
                    .chain(stream)
                    .boxed())
            }
            Ok(stream) => Ok(stream),
            Err(PostError::Transport(e)) => Err(e),
            Err(PostError::Status(status, body)) => Err(map_status_error_from(status, &body)),
        }
    }
}
