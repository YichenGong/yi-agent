//! effort 能力探测的编排：基线 + 7 档串行、预算控制、结果封装。
//!
//! 只负责「问一轮、收一份结果」；落盘与防覆盖由调用方（app-server）按条目
//! 指纹决定。

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use yi_agent_llm::probe::{self, EffortProbeTarget, ProbeChannel, ProbeVerdict};

use crate::models::{ChannelCapability, ModelProvider, REASONING_TIERS};

/// 单个探测请求的超时。
pub const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 整轮探测（基线 + 7 档）的总预算。
pub const PROBE_TOTAL_BUDGET: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    /// `"chat"` / `"responses"` / `"anthropic"`。
    pub channel: String,
    pub capability: ChannelCapability,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn channel_key(channel: ProbeChannel) -> &'static str {
    match channel {
        ProbeChannel::Chat => "chat",
        ProbeChannel::Responses => "responses",
        ProbeChannel::Anthropic => "anthropic",
    }
}

fn failed(channel: ProbeChannel, error: String) -> ProbeOutcome {
    ProbeOutcome {
        channel: channel_key(channel).to_string(),
        capability: ChannelCapability {
            supported: Vec::new(),
            probed_at: now_secs(),
            error: Some(error),
        },
    }
}

/// 对一条模型条目跑一轮探测。
///
/// `api_style` 只对 `openai` 有意义：`chat` / `responses` 直接定通道，`None`
/// （= auto）先判定网关通道再只测那一条。
pub async fn probe_entry(
    provider: ModelProvider,
    api_url: &str,
    api_key: &str,
    model: &str,
    api_style: Option<&str>,
) -> ProbeOutcome {
    let channel = match provider {
        ModelProvider::Anthropic => ProbeChannel::Anthropic,
        ModelProvider::Openai => match api_style {
            Some("chat") => ProbeChannel::Chat,
            Some("responses") => ProbeChannel::Responses,
            _ => match probe::detect_openai_channel(api_url, api_key, model, PROBE_REQUEST_TIMEOUT)
                .await
            {
                Ok(ch) => ch,
                // 判不出通道时记在 chat 槽位上：它是 `auto` 的默认落点，
                // UI 读到 `error` 只会显示「未探测」，不会误报支持集合。
                Err(error) => return failed(ProbeChannel::Chat, error),
            },
        },
    };

    let started = Instant::now();
    let target = EffortProbeTarget {
        base_url: api_url,
        api_key,
        model,
        channel,
    };

    // 基线：不带 effort。它把「鉴权/模型不存在/网络」与「档位不支持」分开，
    // 否则 401 会被记成「7 档都不支持」。
    match probe::probe_effort(&target, None, PROBE_REQUEST_TIMEOUT).await {
        ProbeVerdict::Supported => {}
        ProbeVerdict::Unsupported | ProbeVerdict::Indeterminate(_) => {
            return failed(channel, "baseline request was rejected".to_string());
        }
        ProbeVerdict::Fatal(error) => return failed(channel, error),
    }

    let mut supported: Vec<String> = Vec::new();
    let mut fatal: Option<String> = None;
    for tier in REASONING_TIERS {
        if started.elapsed() >= PROBE_TOTAL_BUDGET {
            // 预算耗尽：保留已得结果，剩余档位按未知处理（不算不支持）。
            break;
        }
        match probe::probe_effort(&target, Some(tier), PROBE_REQUEST_TIMEOUT).await {
            ProbeVerdict::Supported => supported.push(tier.to_string()),
            ProbeVerdict::Unsupported => {}
            // 无法归因：不写进 supported，也不中止其余档位。
            ProbeVerdict::Indeterminate(_) => {}
            ProbeVerdict::Fatal(error) => {
                fatal = Some(error);
                break;
            }
        }
    }

    ProbeOutcome {
        channel: channel_key(channel).to_string(),
        capability: ChannelCapability {
            supported,
            probed_at: now_secs(),
            error: fatal,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// 基线 + 指定档位的响应：命中 `reasoning_effort` 时按 `status` 回。
    async fn server_with(tiers: &[(&str, u16)]) -> MockServer {
        let server = MockServer::start().await;
        for (tier, status) in tiers {
            let body = if *status == 200 {
                "data: [DONE]\n\n".to_string()
            } else {
                r#"{"error":{"message":"Unsupported value","param":"reasoning_effort"}}"#
                    .to_string()
            };
            Mock::given(method("POST"))
                .and(path("/chat/completions"))
                .and(body_string_contains(format!(
                    "\"reasoning_effort\":\"{tier}\""
                )))
                .respond_with(ResponseTemplate::new(*status).set_body_string(body))
                .mount(&server)
                .await;
        }
        // 基线（不带 reasoning_effort）与未命中档位的兜底：都回 200。
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn only_the_accepted_tiers_are_recorded() {
        // 七档全部显式声明，才能断言精确列表（否则兜底 200 会把未声明的
        // 档位也算成支持，测试就失去判别力）。
        let server = server_with(&[
            ("none", 400),
            ("minimal", 400),
            ("low", 200),
            ("medium", 400),
            ("high", 200),
            ("xhigh", 400),
            ("max", 400),
        ])
        .await;
        let out = probe_entry(
            ModelProvider::Openai,
            &server.uri(),
            "sk-test",
            "m",
            Some("chat"),
        )
        .await;
        assert_eq!(out.channel, "chat");
        assert_eq!(out.capability.supported, vec!["low", "high"]);
        assert!(out.capability.error.is_none());
        assert!(out.capability.probed_at > 0);
    }

    #[tokio::test]
    async fn an_anthropic_entry_probes_the_anthropic_channel_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("event: done\n\n"))
            .mount(&server)
            .await;
        let out = probe_entry(
            ModelProvider::Anthropic,
            &server.uri(),
            "sk-test",
            "claude",
            None,
        )
        .await;
        assert_eq!(out.channel, "anthropic");
        assert_eq!(out.capability.supported.len(), 7);
    }

    #[tokio::test]
    async fn a_failing_baseline_stops_immediately_and_records_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(401).set_body_string("nope"))
            .mount(&server)
            .await;
        let out = probe_entry(
            ModelProvider::Openai,
            &server.uri(),
            "sk-test",
            "m",
            Some("chat"),
        )
        .await;
        assert!(out.capability.supported.is_empty());
        assert!(out.capability.error.is_some());
        // 只发了基线那一次。
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_style_probes_only_the_detected_channel() {
        let server = MockServer::start().await;
        // /responses 缺失 → 判定为 chat-only。
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(404).set_body_string(""))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("data: [DONE]\n\n"))
            .mount(&server)
            .await;
        let out = probe_entry(ModelProvider::Openai, &server.uri(), "sk-test", "m", None).await;
        assert_eq!(out.channel, "chat");
        assert_eq!(out.capability.supported.len(), 7);
    }
}
