//! 裸 HTTP 的 reasoning effort 探测。
//!
//! **刻意不复用 `Provider::call_stream`**：那条路径在 effort 被网关拒绝时会
//! 剥离字段重投并返回成功，在 `/responses` 缺失时会回退 chat 并返回成功。用
//! 它探测只会得到「全部支持」的假象。这里只看状态码与错误体，不读流内容。

use std::time::Duration;

use yi_agent_core::{GenParams, Message, ProviderRequest, ToolSchema};

use crate::openai::channel::looks_like_missing_route;
use crate::openai::error::is_effort_rejection as openai_effort_rejection;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeChannel {
    Chat,
    Responses,
    Anthropic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// 2xx：该档可用。
    Supported,
    /// 400/422 且错误体点名 effort：该档被明确拒绝。
    Unsupported,
    /// 400/422 但没点名 effort：无法归因，按未知处理。
    Indeterminate(String),
    /// 鉴权/网络/服务端错误：整轮探测应中止。
    Fatal(String),
}

pub struct EffortProbeTarget<'a> {
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub channel: ProbeChannel,
}

/// 探测专用 dummy tool：真实 agent 请求永远带 tools，Luna/Sol 的 400 正是
/// 「tools + effort」组合触发，不带 tool 测出来的结论在真实对话里不成立。
pub fn probe_tools() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "noop".into(),
        description: "noop".into(),
        input_schema: serde_json::json!({"type": "object", "properties": {}}),
    }]
}

fn probe_request(model: &str, effort: Option<&str>) -> ProviderRequest {
    ProviderRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: probe_tools(),
        params: GenParams {
            max_tokens: Some(64),
            reasoning_effort: effort.map(str::to_string),
            ..Default::default()
        },
    }
}

/// 发一次最小请求，只看状态码与错误体。`effort = None` 即基线（不带该字段）。
pub async fn probe_effort(
    target: &EffortProbeTarget<'_>,
    effort: Option<&str>,
    timeout: Duration,
) -> ProbeVerdict {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return ProbeVerdict::Fatal(format!("failed to build HTTP client: {e}")),
    };
    let request = probe_request(target.model, effort);
    let base = target.base_url.trim_end_matches('/');

    let (url, body): (String, serde_json::Value) = match target.channel {
        ProbeChannel::Chat => (
            format!("{base}/chat/completions"),
            serde_json::to_value(crate::openai::types::OpenaiRequest::from(request))
                .unwrap_or_default(),
        ),
        ProbeChannel::Responses => (
            format!("{base}/responses"),
            serde_json::to_value(crate::openai::responses::types::responses_request_from(
                request,
            ))
            .unwrap_or_default(),
        ),
        ProbeChannel::Anthropic => (
            format!("{base}/v1/messages"),
            serde_json::to_value(crate::anthropic::types::AnthropicRequest::from(request))
                .unwrap_or_default(),
        ),
    };

    let mut builder = client
        .post(url)
        .header("content-type", "application/json")
        .json(&body);
    builder = match target.channel {
        ProbeChannel::Chat | ProbeChannel::Responses => {
            builder.header("Authorization", format!("Bearer {}", target.api_key))
        }
        ProbeChannel::Anthropic => builder
            .header("x-api-key", target.api_key)
            .header("anthropic-version", "2023-06-01"),
    };

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => return ProbeVerdict::Fatal(format!("network error: {e}")),
    };

    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        // 不读流：拿到 2xx 就丢弃连接，省 token。
        drop(resp);
        return ProbeVerdict::Supported;
    }
    let body = resp.text().await.unwrap_or_default();
    classify(target.channel, status, &body)
}

fn classify(channel: ProbeChannel, status: u16, body: &str) -> ProbeVerdict {
    if status == 400 || status == 422 {
        let effort_named = match channel {
            ProbeChannel::Chat | ProbeChannel::Responses => openai_effort_rejection(status, body),
            ProbeChannel::Anthropic => crate::anthropic::error::is_effort_rejection(status, body),
        };
        if effort_named {
            return ProbeVerdict::Unsupported;
        }
        return ProbeVerdict::Indeterminate(truncate(body, 160));
    }
    ProbeVerdict::Fatal(format!("HTTP {status}: {}", truncate(body, 160)))
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// `auto` 的通道判定：只发一次 `/responses`，按「404/405 且 body 为空」
/// 判定网关无该路由。判不出来（鉴权/网络）时返回可读原因。
pub async fn detect_openai_channel(
    base_url: &str,
    api_key: &str,
    model: &str,
    timeout: Duration,
) -> Result<ProbeChannel, String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let base = base_url.trim_end_matches('/');
    let body = serde_json::to_value(crate::openai::responses::types::responses_request_from(
        probe_request(model, None),
    ))
    .unwrap_or_default();
    let resp = client
        .post(format!("{base}/responses"))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("network error: {e}"))?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        drop(resp);
        return Ok(ProbeChannel::Responses);
    }
    let text = resp.text().await.unwrap_or_default();
    if looks_like_missing_route(status, &text) {
        return Ok(ProbeChannel::Chat);
    }
    Err(format!("HTTP {status}: {}", truncate(&text, 160)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn target<'a>(base: &'a str, channel: ProbeChannel) -> EffortProbeTarget<'a> {
        EffortProbeTarget {
            base_url: base,
            api_key: "sk-test",
            model: "m",
            channel,
        }
    }

    #[tokio::test]
    async fn a_2xx_means_supported_and_the_request_carries_a_tool() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("\"reasoning_effort\":\"high\""))
            .and(body_string_contains("\"noop\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("high"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    #[tokio::test]
    async fn an_effort_naming_400_means_unsupported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#,
            ))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("minimal"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(v, ProbeVerdict::Unsupported);
    }

    #[tokio::test]
    async fn a_400_not_naming_effort_is_indeterminate() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_string(r#"{"error":{"message":"tools[0].parameters is invalid"}}"#),
            )
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("high"),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(v, ProbeVerdict::Indeterminate(_)));
    }

    #[tokio::test]
    async fn a_401_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("high"),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(v, ProbeVerdict::Fatal(_)));
    }

    #[tokio::test]
    async fn a_timeout_is_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("high"),
            Duration::from_millis(100),
        )
        .await;
        assert!(matches!(v, ProbeVerdict::Fatal(_)));
    }

    #[tokio::test]
    async fn responses_channel_posts_to_the_responses_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_string_contains("\"effort\":\"low\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Responses),
            Some("low"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    #[tokio::test]
    async fn anthropic_channel_posts_to_v1_messages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_string_contains("\"effort\":\"medium\""))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let v = probe_effort(
            &target(&server.uri(), ProbeChannel::Anthropic),
            Some("medium"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(v, ProbeVerdict::Supported);
    }

    /// 判别性回归：走 `Provider::call_stream` 会把「档位被拒」掩盖成成功
    /// （它剥离字段重投），因此探测**必须**走裸 HTTP。这条测试钉住这个事实：
    /// 同一个 mock 下，provider 返回 Ok，而 probe 判 Unsupported。
    #[tokio::test]
    async fn the_provider_path_would_hide_an_effort_rejection_but_the_probe_does_not() {
        use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};

        let server = MockServer::start().await;
        // 带 effort 的请求被拒；不带 effort 的重投成功。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_string_contains("\"reasoning_effort\""))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;

        let provider =
            crate::openai::client::OpenaiProvider::new(crate::openai::client::OpenaiProviderOpts {
                base_url: Some(server.uri()),
                api_key: Some("sk-test".into()),
                api_style: Some("chat".into()),
                ..Default::default()
            })
            .unwrap();
        let ok = provider
            .call_stream(ProviderRequest {
                model: "m".into(),
                system: None,
                messages: vec![Message::user("ping")],
                tools: probe_tools(),
                params: GenParams {
                    max_tokens: Some(64),
                    reasoning_effort: Some("high".into()),
                    ..Default::default()
                },
            })
            .await;
        assert!(
            ok.is_ok(),
            "the provider strips and retries, hiding the rejection"
        );

        let verdict = probe_effort(
            &target(&server.uri(), ProbeChannel::Chat),
            Some("high"),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(verdict, ProbeVerdict::Unsupported);
    }

    #[tokio::test]
    async fn auto_detection_reads_a_missing_responses_route_as_chat_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        let ch = detect_openai_channel(&server.uri(), "sk-test", "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ch, ProbeChannel::Chat);
    }

    #[tokio::test]
    async fn auto_detection_reads_a_live_responses_route_as_responses() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let ch = detect_openai_channel(&server.uri(), "sk-test", "m", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(ch, ProbeChannel::Responses);
    }
}
