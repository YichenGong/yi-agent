//! 首轮结束后生成 thread 标题：材料拼接、标题清洗，以及调用 provider 生成。
//!
//! 与 `compact.rs` 同风格：纯逻辑（材料拼接 / 清洗）与"调用 provider 生成标题"分开放，
//! 便于单测覆盖清洗规则；生成失败由调用方静默回退到截断标题。

use std::sync::Arc;

use crate::{
    agent::{AgentConfig, AgentError},
    message::Message,
    provider::{Provider, ProviderRequest},
};

/// 标题字符数上限，与 `thread_store::title_from` 的截断上限保持一致。
pub const TITLE_MAX_CHARS: usize = 30;

/// 材料每一侧（提问 / 回复）截断后的最大字符数。
pub const TITLE_MATERIAL_SIDE_CHARS: usize = 2000;

/// 把首轮的两侧文本拼成标题模型的输入材料。
///
/// `assistant_text` 为 `None`（首轮无文本回复，例如纯工具调用）时省略该段。
pub fn build_title_material(user_text: &str, assistant_text: Option<&str>) -> String {
    let truncate = |s: &str| -> String { s.chars().take(TITLE_MATERIAL_SIDE_CHARS).collect() };
    let mut out = format!("用户提问：{}", truncate(user_text));
    // 全空白的助手侧等同缺失：否则会渲染一个悬空的"助手回复："标签，这非但没提供
    // 信息，反而用一段空结构误导标题模型。
    if let Some(a) = assistant_text.filter(|a| !a.trim().is_empty()) {
        out.push_str("\n\n助手回复：");
        out.push_str(&truncate(a));
    }
    out
}

/// 把模型返回的原始文本清洗成一个可用标题。
///
/// 步骤：去首尾空白 → 取首个非空行 → 反复（去包裹引号 → 去 `标题:` / `Title:` 前缀）
/// 直到稳定 → 截断至 [`TITLE_MAX_CHARS`]。结果为空返回 `None`（调用方据此回退兜底标题）。
///
/// 引号与前缀的清洗必须交替循环：模型可能写成 `标题："x"` 或 `"标题：x"`，只做一轮
/// 固定顺序会留下另一侧的包裹字符，故循环到不再变化为止。
pub fn sanitize_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut text = first_line;
    loop {
        let mut next = text.trim_matches(|c| matches!(c, '"' | '\'' | '「' | '」' | '“' | '”'));
        for prefix in ["标题:", "标题：", "Title:", "title:"] {
            if let Some(rest) = next.strip_prefix(prefix) {
                next = rest.trim();
                break;
            }
        }
        if next == text {
            break;
        }
        text = next;
    }
    let capped: String = text.chars().take(TITLE_MAX_CHARS).collect();
    let capped = capped.trim().to_string();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}

/// 标题生成系统提示：约束模型只吐一个短标题。
pub const TITLE_INSTRUCTIONS: &str = "\
你会得到一个会话的开头（用户提问，可能附带助手回复）。请为它生成一个简短的标题。
要求：
- 只输出标题本身，不要换行、不要引号、不要“标题：”之类前缀、不要任何解释。
- 语言跟随用户提问：中文提问用中文标题，英文提问用英文标题。
- 不超过 20 个汉字（或约 40 个英文字符）。";

/// 用当前模型生成一个 thread 标题。
///
/// `assistant_text` 为 `None` 时只用用户提问。返回 `Ok(None)` 表示模型产出
/// 无法作为标题（空/全空白）；provider 报错时返回 `Err`，由调用方决定回退。
pub async fn generate_title(
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    user_text: &str,
    assistant_text: Option<&str>,
) -> Result<Option<String>, AgentError> {
    let mut params = config.gen_params.clone();
    // 标题极短：限死输出上限，避免个别模型长篇大论。
    params.max_tokens = Some(64);
    let response = provider
        .call(ProviderRequest {
            model: config.model.clone(),
            system: Some(TITLE_INSTRUCTIONS.to_string()),
            messages: vec![Message::user(build_title_material(
                user_text,
                assistant_text,
            ))],
            tools: vec![],
            params,
        })
        .await
        .map_err(AgentError::Provider)?;
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            crate::message::ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    Ok(sanitize_title(&text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::{AgentConfig, AgentError},
        provider::{Provider, ProviderError, ProviderEvent, ProviderRequest, StopReason},
    };
    use async_trait::async_trait;
    use futures::stream::{BoxStream, StreamExt};

    /// 固定返回一段文本的 provider。
    struct FixedProvider(String);

    #[async_trait]
    impl Provider for FixedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let events = vec![
                ProviderEvent::TextDelta(self.0.clone()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    /// 每次调用都失败的 provider。
    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Err(ProviderError::Network("boom".into()))
        }
    }

    /// 记录收到的请求、并返回固定文本的 provider。
    ///
    /// 用于对 `generate_title` 构造的 `ProviderRequest` 做"请求形状"断言：
    /// `generate_title` 的约束（`max_tokens`、`system`、`tools`、`model`、保留
    /// `gen_params`）都只能从实际发出的请求上观察。
    struct CapturingProvider {
        seen: std::sync::Arc<std::sync::Mutex<Option<ProviderRequest>>>,
        reply: String,
    }

    #[async_trait]
    impl Provider for CapturingProvider {
        async fn call_stream(
            &self,
            req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            *self.seen.lock().unwrap() = Some(req);
            let events = vec![
                ProviderEvent::TextDelta(self.reply.clone()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    fn core_config() -> AgentConfig {
        AgentConfig::default()
    }

    #[tokio::test]
    async fn generate_title_returns_sanitized_title() {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(FixedProvider("\"修复登录报错\"".into()));
        let out = generate_title(&provider, &core_config(), "登录页报错", Some("好"))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("修复登录报错"));
    }

    #[tokio::test]
    async fn generate_title_blank_response_is_none() {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(FixedProvider("   ".into()));
        let out = generate_title(&provider, &core_config(), "hi", None)
            .await
            .unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn generate_title_propagates_provider_error() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(FailingProvider);
        let err = generate_title(&provider, &core_config(), "hi", None).await;
        assert!(matches!(err, Err(AgentError::Provider(_))));
    }

    /// `generate_title` 必须把调用绑定在既定请求形状上：限死 `max_tokens`、带
    /// `TITLE_INSTRUCTIONS` 系统提示、不挂工具、沿用 config 的 model 与非默认 gen_params。
    #[tokio::test]
    async fn generate_title_request_shape_is_bound() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(CapturingProvider {
            seen: seen.clone(),
            reply: "标题".into(),
        });
        let mut config = core_config();
        config.model = "capture-model".into();
        // 非默认字段：证明 generate_title 是"基于 config.gen_params 覆盖 max_tokens"，
        // 而不是整体重置成默认值。
        config.gen_params.temperature = Some(0.3);

        let out = generate_title(&provider, &config, "hi", None)
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("标题"));

        let req = seen
            .lock()
            .unwrap()
            .take()
            .expect("provider must be called");
        assert_eq!(req.params.max_tokens, Some(64), "标题调用必须限死输出上限");
        assert_eq!(
            req.system.as_deref(),
            Some(TITLE_INSTRUCTIONS),
            "必须带上标题专用系统提示"
        );
        assert!(req.tools.is_empty(), "标题调用不得挂任何工具");
        assert_eq!(req.model, config.model, "必须沿用会话当前模型");
        assert_eq!(
            req.params.temperature,
            Some(0.3),
            "config.gen_params 的非默认字段必须被保留"
        );
    }

    #[test]
    fn material_joins_both_sides_with_labels() {
        let m = build_title_material("帮我修一下登录页的报错", Some("好的，我看下 AuthForm。"));
        assert!(m.starts_with("用户提问：帮我修一下登录页的报错"));
        assert!(m.contains("助手回复：好的，我看下 AuthForm。"));
    }

    #[test]
    fn material_omits_assistant_when_absent() {
        let m = build_title_material("你好", None);
        assert!(m.starts_with("用户提问：你好"));
        assert!(!m.contains("助手回复"), "absent assistant must be omitted");
    }

    /// 全空白的助手侧等同缺失：不得留下悬空的"助手回复："标签。
    #[test]
    fn material_omits_blank_assistant() {
        let m = build_title_material("hi", Some("   "));
        assert!(m.starts_with("用户提问：hi"));
        assert!(
            !m.contains("助手回复"),
            "blank assistant must render no assistant section: {m:?}"
        );
    }

    #[test]
    fn material_truncates_each_side() {
        let long = "字".repeat(TITLE_MATERIAL_SIDE_CHARS + 500);
        let m = build_title_material(&long, Some(&long));
        // 每侧最多 2000 字符：总字符数上界 = 两侧材料 + 两个标签 + 一个空行。
        assert!(
            m.chars().count()
                <= TITLE_MATERIAL_SIDE_CHARS * 2 + "用户提问：助手回复：\n\n".chars().count()
        );
    }

    #[test]
    fn sanitize_trims_and_takes_first_nonempty_line() {
        assert_eq!(
            sanitize_title("  修复登录报错  \n解释一下").as_deref(),
            Some("修复登录报错")
        );
    }

    #[test]
    fn sanitize_strips_wrapping_quotes_and_prefix() {
        assert_eq!(
            sanitize_title("\"修复登录报错\"").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("「修复登录报错」").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("标题：修复登录报错").as_deref(),
            Some("修复登录报错")
        );
        assert_eq!(
            sanitize_title("Title: fix login").as_deref(),
            Some("fix login")
        );
    }

    /// 前缀与引号的清洗必须能交替进行：`标题："x"` 先去前缀再去引号才算干净。
    #[test]
    fn sanitize_strips_quotes_after_prefix() {
        assert_eq!(sanitize_title("标题：\"修复\"").as_deref(), Some("修复"));
        assert_eq!(sanitize_title("\"标题：修复\"").as_deref(), Some("修复"));
    }

    /// 其余的包裹引号变体与 `\r\n` 行尾：`lines()` 会吃掉 `\r`，首行照常取用。
    #[test]
    fn sanitize_handles_quote_variants_and_crlf() {
        assert_eq!(sanitize_title("'修复'").as_deref(), Some("修复"));
        assert_eq!(sanitize_title("“修复”").as_deref(), Some("修复"));
        assert_eq!(sanitize_title("修复\r\n解释一下").as_deref(), Some("修复"));
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "字".repeat(100);
        let out = sanitize_title(&long).unwrap();
        assert_eq!(out.chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn sanitize_blank_returns_none() {
        assert_eq!(sanitize_title("   \n  "), None);
    }
}
