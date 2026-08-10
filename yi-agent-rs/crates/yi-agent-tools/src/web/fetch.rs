use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::error::ToolsError;

const DEFAULT_MAX_LENGTH: usize = 100 * 1024; // 100KB
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024; // 10MB

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::default())
            .user_agent("yi-agent/0.1.1")
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }

    #[cfg(test)]
    fn new_for_test() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::default())
            .user_agent("yi-agent/0.1.1")
            .no_proxy()
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    #[serde(default)]
    max_length: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and return content as markdown (HTML→MD) or plain text."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "URL to fetch (http or https)" },
                "max_length": { "type": "integer", "description": "Max bytes to return, default 100KB" }
            },
            "required": ["url"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: FetchArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        tracing::info!(tool = "web_fetch", url = %args.url, "fetch start");

        // Validate URL scheme
        let url = match reqwest::Url::parse(&args.url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
            Ok(u) => {
                tracing::warn!(
                    tool = "web_fetch",
                    scheme = u.scheme(),
                    "unsupported scheme"
                );
                return ToolsError::UnsupportedContentType(format!(
                    "unsupported scheme: {}",
                    u.scheme()
                ))
                .into();
            }
            Err(e) => {
                tracing::warn!(tool = "web_fetch", error = %e, "invalid URL");
                return ToolsError::Http(format!("invalid URL: {}", e)).into();
            }
        };

        let resp = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(tool = "web_fetch", error = %e, "request failed");
                return ToolsError::Http(e.to_string()).into();
            }
        };

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let status = resp.status();
        tracing::info!(tool = "web_fetch", status = %status, content_type = %content_type, "response received");

        // Check body size before reading
        if let Some(len) = resp.content_length() {
            if len > MAX_BODY_SIZE as u64 {
                tracing::warn!(tool = "web_fetch", size = len, "response too large");
                return ToolsError::ResponseTooLarge(len as usize).into();
            }
        }

        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(tool = "web_fetch", error = %e, "body read failed");
                return ToolsError::Http(e.to_string()).into();
            }
        };

        if body.len() > MAX_BODY_SIZE {
            tracing::warn!(tool = "web_fetch", size = body.len(), "body too large");
            return ToolsError::ResponseTooLarge(body.len()).into();
        }

        let content = match process_content(&content_type, &body) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(tool = "web_fetch", error = %e, "content processing failed");
                return e.into();
            }
        };

        let max_length = args.max_length.unwrap_or(DEFAULT_MAX_LENGTH);
        let content = truncate_content(&content, max_length);

        tracing::info!(
            tool = "web_fetch",
            content_len = content.len(),
            "fetch done"
        );

        ToolResult::text(content)
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

fn process_content(content_type: &str, body: &[u8]) -> Result<String, ToolsError> {
    let ct = content_type.to_lowercase();
    if ct.contains("text/html") {
        let html = std::str::from_utf8(body)
            .map_err(|e| ToolsError::Http(format!("invalid UTF-8 in HTML: {}", e)))?;
        // html2md 0.1.1 遇到 HTML 注释会走 `println!("<!-- ... -->")` 把注释
        // 直接写到 stdout，在 TUI 的 raw mode + alternate screen 下会污染
        // 屏幕渲染。先在此剥掉注释再交给 html2md。
        let html = strip_html_comments(html);
        Ok(html2md::parse_html(&html))
    } else if ct.contains("text/plain")
        || ct.contains("application/json")
        || ct.contains("application/xml")
        || ct.contains("text/xml")
    {
        Ok(String::from_utf8_lossy(body).to_string())
    } else {
        Err(ToolsError::UnsupportedContentType(content_type.to_string()))
    }
}

/// 剥掉 HTML 注释 `<!-- ... -->`（含跨行）。
/// 用状态机扫描而非正则，避免引入 `regex` 依赖；同时正确处理注释内部的
/// 嵌套 `--` 与伪注释边界（HTML5 规范：注释到第一个 `-->` 结束）。
fn strip_html_comments(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < bytes.len() {
        // 检测 `<!--` 开头
        if i + 4 <= bytes.len() && &bytes[i..i + 4] == b"<!--" {
            // 找到第一个 `-->` 结束注释
            let mut j = i + 4;
            let mut found = false;
            while j + 3 <= bytes.len() {
                if &bytes[j..j + 3] == b"-->" {
                    i = j + 3;
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                // 没有匹配的 `-->`：把剩余内容当作注释丢弃
                return out;
            }
        } else {
            // 非 ASCII 安全：按 char 边界推进，避免把多字节字符拆开
            let ch = html[i..].chars().next().expect("non-empty slice");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn truncate_content(content: &str, max_length: usize) -> String {
    if content.len() <= max_length {
        return content.to_string();
    }
    let mut end = max_length;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &content[..end];
    format!(
        "[truncated: showed {} of {} bytes]\n{}",
        end,
        content.len(),
        truncated
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn setup_mock_server() -> MockServer {
        MockServer::start().await
    }

    #[tokio::test]
    async fn fetch_html_returns_markdown() {
        let server = setup_mock_server().await;
        let html = "<html><body><h1>Hello</h1><p>World</p></body></html>";
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html; charset=utf-8")
                    .set_body_string(html),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/page", server.uri())
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.to_lowercase().contains("hello"));
            assert!(s.to_lowercase().contains("world"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn fetch_plain_text() {
        let server = setup_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/text"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("just plain text"),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/text", server.uri())
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("just plain text"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn fetch_json_content() {
        let server = setup_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/api"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(r#"{"key": "value"}"#),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/api", server.uri())
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("value"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn fetch_unsupported_content_type() {
        let server = setup_mock_server().await;
        Mock::given(method("GET"))
            .and(path("/img"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "image/png")
                    .set_body_raw([0u8; 100], "image/png"),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/img", server.uri())
            }))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn fetch_truncates_large_response() {
        let server = setup_mock_server().await;
        let large_html = format!("<p>{}</p>", "x".repeat(200_000));
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/html")
                    .set_body_string(&large_html),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/big", server.uri())
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("[truncated:"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn fetch_invalid_scheme() {
        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": "ftp://example.com/file"
            }))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn fetch_follows_redirect() {
        let server = setup_mock_server().await;
        let target_path = "/target";
        let redirect_path = "/redirect";

        Mock::given(method("GET"))
            .and(path(redirect_path))
            .respond_with(ResponseTemplate::new(302).insert_header("location", target_path))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path(target_path))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/plain")
                    .set_body_string("redirected content"),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}{}", server.uri(), redirect_path)
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("redirected content"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn fetch_truncates_multibyte_safely() {
        // Create content with multi-byte UTF-8 characters that exceeds max_length
        // The key is that the truncation point falls in the middle of a multi-byte char
        let content = "你好世界".repeat(1000); // Each char is 3 bytes in UTF-8
        let result = truncate_content(&content, 100); // 100 bytes, likely mid-char
        assert!(result.contains("[truncated:"));
        assert!(!result.is_empty());
    }

    #[test]
    fn strip_html_comments_removes_simple_comment() {
        let html = "<p>before</p><!-- a comment --><p>after</p>";
        let stripped = strip_html_comments(html);
        assert!(
            !stripped.contains("<!--") && !stripped.contains("-->"),
            "comments should be gone: {stripped}"
        );
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
    }

    #[test]
    fn strip_html_comments_removes_multiline_comment() {
        let html = "<!-- line one\nline two\nline three --><p>x</p>";
        let stripped = strip_html_comments(html);
        assert!(!stripped.contains("<!--"));
        assert!(stripped.contains("<p>x</p>"));
    }

    #[test]
    fn strip_html_comments_removes_multiple_comments() {
        let html = "<!-- a --><p>1</p><!-- b --><p>2</p><!-- c -->";
        let stripped = strip_html_comments(html);
        assert!(!stripped.contains("<!--"));
        assert!(stripped.contains("<p>1</p>"));
        assert!(stripped.contains("<p>2</p>"));
    }

    #[test]
    fn strip_html_comments_preserves_text_with_dash_dash() {
        // 文本中合法的 `--` 不应被误判为注释边界
        let html = "<p>a -- b</p>";
        let stripped = strip_html_comments(html);
        assert_eq!(stripped, "<p>a -- b</p>");
    }

    #[test]
    fn strip_html_comments_handles_unclosed_comment() {
        // 没有匹配 `-->` 的 `<!--`：剩余内容当作注释丢弃
        let html = "<p>ok</p><!-- never closed";
        let stripped = strip_html_comments(html);
        assert!(stripped.contains("<p>ok</p>"));
        assert!(!stripped.contains("<!--"));
    }

    #[test]
    fn strip_html_comments_handles_cjk_content() {
        let html = "<!-- 注释 --><p>你好世界</p>";
        let stripped = strip_html_comments(html);
        assert!(!stripped.contains("<!--"));
        assert!(stripped.contains("你好世界"));
    }

    #[tokio::test]
    async fn fetch_html_strips_comments_no_stdout_leak() {
        // 回归测试：html2md 0.1.1 对 HTML 注释会走 println!，污染 TUI。
        // 这里通过 mock server 返回带注释的 HTML，验证 fetch 结果里
        // 不包含 `<!--` 且正文内容保留。
        // 注意：wiremock 的 `set_body_string` 会把 mime 设为 text/plain，
        // 且最终渲染时 mime 会覆盖 insert_header 设置的 content-type。
        // 所以这里用 `set_body_raw` 同时设置 body 和 mime。
        let server = setup_mock_server().await;
        let html = "<html><body><!-- analytics tracker --><h1>Title</h1></body></html>";
        Mock::given(method("GET"))
            .and(path("/with_comments"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(html.as_bytes().to_vec(), "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;

        let tool = WebFetchTool::new_for_test();
        let result = tool
            .call(serde_json::json!({
                "url": format!("{}/with_comments", server.uri())
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(
                !s.contains("<!--") && !s.contains("-->"),
                "fetch result should not contain HTML comment markers: {s}"
            );
            assert!(
                s.to_lowercase().contains("title"),
                "real content should be preserved: {s}"
            );
        } else {
            panic!("expected text block");
        }
    }
}
