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

        // Validate URL scheme
        let url = match reqwest::Url::parse(&args.url) {
            Ok(u) if u.scheme() == "http" || u.scheme() == "https" => u,
            Ok(u) => {
                return ToolsError::UnsupportedContentType(format!("unsupported scheme: {}", u.scheme()))
                    .into();
            }
            Err(e) => return ToolsError::Http(format!("invalid URL: {}", e)).into(),
        };

        let resp = match self.client.get(url).send().await {
            Ok(r) => r,
            Err(e) => return ToolsError::Http(e.to_string()).into(),
        };

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // Check body size before reading
        if let Some(len) = resp.content_length() {
            if len > MAX_BODY_SIZE as u64 {
                return ToolsError::ResponseTooLarge(len as usize).into();
            }
        }

        let body = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return ToolsError::Http(e.to_string()).into(),
        };

        if body.len() > MAX_BODY_SIZE {
            return ToolsError::ResponseTooLarge(body.len()).into();
        }

        let content = match process_content(&content_type, &body) {
            Ok(c) => c,
            Err(e) => return e.into(),
        };

        let max_length = args.max_length.unwrap_or(DEFAULT_MAX_LENGTH);
        let content = truncate_content(&content, max_length);

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
        Ok(html2md::parse_html(html))
    } else if ct.contains("text/plain") {
        Ok(String::from_utf8_lossy(body).to_string())
    } else if ct.contains("application/json") {
        Ok(String::from_utf8_lossy(body).to_string())
    } else if ct.contains("application/xml") || ct.contains("text/xml") {
        Ok(String::from_utf8_lossy(body).to_string())
    } else {
        Err(ToolsError::UnsupportedContentType(content_type.to_string()))
    }
}

fn truncate_content(content: &str, max_length: usize) -> String {
    if content.len() <= max_length {
        content.to_string()
    } else {
        let truncated = &content[..max_length];
        format!(
            "[truncated: showed {} of {} bytes]\n{}",
            max_length,
            content.len(),
            truncated
        )
    }
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

        let tool = WebFetchTool::new();
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

        let tool = WebFetchTool::new();
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

        let tool = WebFetchTool::new();
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

        let tool = WebFetchTool::new();
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

        let tool = WebFetchTool::new();
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
        let tool = WebFetchTool::new();
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

        let tool = WebFetchTool::new();
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
}
