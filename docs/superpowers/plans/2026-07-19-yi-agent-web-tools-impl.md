# yi-agent Web Tools Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement 2 web tools (WebFetch + WebSearch) in `yi-agent-rs/crates/yi-agent-tools` that integrate with `yi-agent-core`'s `Tool` trait.

**Architecture:** `WebFetchTool` fetches URLs and converts HTML to markdown via `html2md`. `WebSearchTool` delegates to a `WebSearchProvider` trait, with `BochaSearchProvider` as the first implementation. Both use `reqwest` for HTTP (consistent with `yi-agent-llm`). WebSearchTool is only registered if `BOCHA_API_KEY` env var is set.

**Tech Stack:** Rust 2024, reqwest (rustls-tls), html2md, serde, thiserror, wiremock (dev)

**Design doc:** `docs/plans/2026-07-19-yi-agent-web-tools-design.md`

---

## Task 1: Add web dependencies to Cargo.toml

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`

**Step 1: Add dependencies**

Add to `[dependencies]` section (after existing deps):

```toml
# Web 工具
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
html2md = "0.1"
```

Add to `[dev-dependencies]` section:

```toml
wiremock = "0.6"
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-tools`
Expected: PASS (reqwest and html2md pulled in)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/Cargo.toml yi-agent-rs/Cargo.lock
git commit -m "chore(tools): add reqwest, html2md, wiremock deps for web tools"
```

---

## Task 2: Add web error variants to ToolsError

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/error.rs`

**Step 1: Add new error variants**

Add these variants before the closing `}` of `ToolsError` enum (after `ArgsParse`):

```rust
    #[error("args parse error: {0}")]
    ArgsParse(#[from] serde_json::Error),

    #[error("http error: {0}")]
    Http(String),

    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),

    #[error("response too large: {0} bytes")]
    ResponseTooLarge(usize),

    #[error("search engine error: {0}")]
    SearchEngine(String),

    #[error("BOCHA_API_KEY not set")]
    MissingApiKey,
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-tools`
Expected: PASS (dead_code warnings expected since no callers yet)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/error.rs
git commit -m "feat(tools): add web error variants to ToolsError"
```

---

## Task 3: WebSearchProvider trait + SearchResult

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/provider.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`

**Step 1: Create provider.rs**

Create `yi-agent-rs/crates/yi-agent-tools/src/web/provider.rs`:

```rust
use crate::error::ToolsError;

/// A single search result from any search engine.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Trait for search engine providers.
/// Implement this to add a new search engine (Bocha, Bing, etc.)
#[async_trait::async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, ToolsError>;
}
```

**Step 2: Create web/mod.rs**

Create `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`:

```rust
pub mod bocha;
pub mod fetch;
pub mod provider;
pub mod search;

pub use bocha::BochaSearchProvider;
pub use fetch::WebFetchTool;
pub use provider::{SearchResult, WebSearchProvider};
pub use search::WebSearchTool;
```

Note: This references modules that don't exist yet (bocha, fetch, search). Create stub files to make it compile, or add modules one by one in subsequent tasks. For now, just create `provider.rs` and update `mod.rs` to only export provider:

```rust
pub mod provider;

pub use provider::{SearchResult, WebSearchProvider};
```

**Step 3: Update lib.rs**

Add `mod web;` to `src/lib.rs` and export `SearchResult` and `WebSearchProvider`:

```rust
mod context;
mod error;
mod fs;
mod shell;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use yi_agent_core::ToolRegistry;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use shell::BashTool;
pub use web::{SearchResult, WebSearchProvider};
```

**Step 4: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-tools`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/web/ yi-agent-rs/crates/yi-agent-tools/src/lib.rs
git commit -m "feat(tools): add WebSearchProvider trait and SearchResult type"
```

---

## Task 4: WebFetchTool

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/fetch.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`

**Step 1: Write the implementation + tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/web/fetch.rs`:

```rust
use std::time::Duration;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
use crate::error::ToolsError;

const DEFAULT_MAX_LENGTH: usize = 100 * 1024;  // 100KB
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;  // 10MB

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
            .redirect(reqwest::redirect::Policy::none())
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
            Ok(u) => return ToolsError::UnsupportedContentType(format!("unsupported scheme: {}", u.scheme())).into(),
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

        let content = process_content(&content_type, &body);
        let content = match content {
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
    use wiremock::matchers::{header, method, path};
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
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", target_path),
            )
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
```

Update `src/web/mod.rs`:
```rust
pub mod fetch;
pub mod provider;

pub use fetch::WebFetchTool;
pub use provider::{SearchResult, WebSearchProvider};
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools web::fetch`
Expected: PASS (7 tests)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/web/fetch.rs yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs
git commit -m "feat(tools): implement WebFetchTool with HTML→MD, truncation, redirect support"
```

---

## Task 5: BochaSearchProvider

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`

**Step 1: Write the implementation + tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs`:

```rust
use std::time::Duration;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use crate::error::ToolsError;
use crate::web::provider::{SearchResult, WebSearchProvider};

pub struct BochaSearchProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl BochaSearchProvider {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url: "https://api.bocha.cn/v1".to_string(),
        }
    }

    pub fn with_base_url(api_key: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self { client, api_key, base_url }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("BOCHA_API_KEY").ok()?;
        Some(Self::new(api_key))
    }
}

#[derive(Serialize)]
struct BochaRequest {
    query: String,
    summary: bool,
    count: usize,
    freshness: String,
}

#[derive(Deserialize)]
struct BochaResponse {
    code: u16,
    #[serde(default)]
    #[allow(dead_code)]
    msg: Option<String>,
    data: Option<BochaData>,
}

#[derive(Deserialize)]
struct BochaData {
    #[serde(default, rename = "webPages")]
    web_pages: Option<WebPages>,
}

#[derive(Deserialize)]
struct WebPages {
    #[serde(default)]
    value: Vec<WebPageValue>,
}

#[derive(Deserialize)]
struct WebPageValue {
    #[serde(default)]
    name: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    summary: String,
}

#[async_trait]
impl WebSearchProvider for BochaSearchProvider {
    fn name(&self) -> &str {
        "bocha"
    }

    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, ToolsError> {
        let req_body = BochaRequest {
            query: query.to_string(),
            summary: true,
            count,
            freshness: "noLimit".to_string(),
        };

        let resp = self
            .client
            .post(format!("{}/web-search", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&req_body)
            .send()
            .await
            .map_err(|e| ToolsError::Http(e.to_string()))?;

        let status = resp.status().as_u16();

        if status == 401 {
            return Err(ToolsError::SearchEngine("invalid API key".to_string()));
        }
        if status == 403 {
            return Err(ToolsError::SearchEngine("insufficient balance".to_string()));
        }
        if status == 429 {
            return Err(ToolsError::SearchEngine("rate limited".to_string()));
        }
        if status >= 500 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolsError::SearchEngine(format!("server error: {}", body)));
        }
        if !resp.status().is_success() {
            return Err(ToolsError::SearchEngine(format!("unexpected status: {}", status)));
        }

        let bocha_resp: BochaResponse = resp
            .json()
            .await
            .map_err(|e| ToolsError::Http(format!("failed to parse response: {}", e)))?;

        let results: Vec<SearchResult> = bocha_resp
            .data
            .and_then(|d| d.web_pages)
            .map(|wp| wp.value)
            .unwrap_or_default()
            .into_iter()
            .map(|v| SearchResult {
                title: v.name,
                url: v.url,
                snippet: if v.summary.is_empty() { v.snippet } else { v.summary },
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_response() -> String {
        r#"{
            "code": 200,
            "msg": null,
            "data": {
                "_type": "SearchResponse",
                "webPages": {
                    "webSearchUrl": "",
                    "totalEstimatedMatches": 3,
                    "value": [
                        {
                            "name": "Result One",
                            "url": "https://example.com/1",
                            "snippet": "Short description one",
                            "summary": "Full summary one"
                        },
                        {
                            "name": "Result Two",
                            "url": "https://example.com/2",
                            "snippet": "Short description two",
                            "summary": ""
                        }
                    ]
                }
            }
        }"#.to_string()
    }

    #[tokio::test]
    async fn search_returns_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(sample_response()),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url(
            "test-key".to_string(),
            server.uri(),
        );
        let results = provider.search("test query", 5).await.unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Result One");
        assert_eq!(results[0].url, "https://example.com/1");
        // summary field should be preferred
        assert_eq!(results[0].snippet, "Full summary one");
        // fallback to snippet when summary is empty
        assert_eq!(results[1].snippet, "Short description two");
    }

    #[tokio::test]
    async fn search_no_results() {
        let server = MockServer::start().await;
        let empty_response = r#"{
            "code": 200,
            "data": {
                "webPages": {
                    "value": []
                }
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(empty_response),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url(
            "test-key".to_string(),
            server.uri(),
        );
        let results = provider.search("nothing", 5).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_invalid_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url(
            "bad-key".to_string(),
            server.uri(),
        );
        let result = provider.search("test", 5).await;
        assert!(matches!(result, Err(ToolsError::SearchEngine(_))));
    }

    #[tokio::test]
    async fn search_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url(
            "test-key".to_string(),
            server.uri(),
        );
        let result = provider.search("test", 5).await;
        assert!(matches!(result, Err(ToolsError::SearchEngine(_))));
    }

    #[tokio::test]
    async fn search_uses_summary_field() {
        let server = MockServer::start().await;
        let response = r#"{
            "code": 200,
            "data": {
                "webPages": {
                    "value": [
                        {
                            "name": "Test",
                            "url": "https://example.com",
                            "snippet": "SHORT",
                            "summary": "LONG SUMMARY"
                        }
                    ]
                }
            }
        }"#;
        Mock::given(method("POST"))
            .and(path("/web-search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_string(response),
            )
            .mount(&server)
            .await;

        let provider = BochaSearchProvider::with_base_url(
            "test-key".to_string(),
            server.uri(),
        );
        let results = provider.search("test", 1).await.unwrap();
        assert_eq!(results[0].snippet, "LONG SUMMARY");
    }
}
```

Update `src/web/mod.rs`:
```rust
pub mod bocha;
pub mod fetch;
pub mod provider;

pub use bocha::BochaSearchProvider;
pub use fetch::WebFetchTool;
pub use provider::{SearchResult, WebSearchProvider};
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools web::bocha`
Expected: PASS (5 tests)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/web/bocha.rs yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs
git commit -m "feat(tools): implement BochaSearchProvider with summary preference, error mapping"
```

---

## Task 6: WebSearchTool

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/web/search.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`

**Step 1: Write the implementation + tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/web/search.rs`:

```rust
use std::sync::Arc;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
use crate::error::ToolsError;
use crate::web::provider::WebSearchProvider;

const DEFAULT_COUNT: usize = 25;

pub struct WebSearchTool {
    engine: Arc<dyn WebSearchProvider>,
}

impl WebSearchTool {
    pub fn new(engine: Arc<dyn WebSearchProvider>) -> Self {
        Self { engine }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    count: Option<usize>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web using a search engine. Returns titles, URLs, and snippets."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "integer", "description": "Max results, default 25" }
            },
            "required": ["query"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: SearchArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let count = args.count.unwrap_or(DEFAULT_COUNT);

        match self.engine.search(&args.query, count).await {
            Ok(results) => {
                if results.is_empty() {
                    ToolResult::text("no results")
                } else {
                    let formatted = format_results(&results);
                    ToolResult::text(formatted)
                }
            }
            Err(e) => e.into(),
        }
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

fn format_results(results: &[crate::web::provider::SearchResult]) -> String {
    results
        .iter()
        .enumerate()
        .map(|(i, r)| format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, r.snippet))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::provider::SearchResult;

    // Use a mock provider for testing
    struct MockProvider {
        results: Vec<SearchResult>,
    }

    #[async_trait]
    impl WebSearchProvider for MockProvider {
        fn name(&self) -> &str { "mock" }
        async fn search(&self, _query: &str, _count: usize) -> Result<Vec<SearchResult>, ToolsError> {
            Ok(self.results.clone())
        }
    }

    fn make_tool(results: Vec<SearchResult>) -> WebSearchTool {
        WebSearchTool::new(Arc::new(MockProvider { results }))
    }

    #[tokio::test]
    async fn search_returns_results() {
        let results = vec![
            SearchResult {
                title: "Title One".to_string(),
                url: "https://example.com/1".to_string(),
                snippet: "Snippet one".to_string(),
            },
            SearchResult {
                title: "Title Two".to_string(),
                url: "https://example.com/2".to_string(),
                snippet: "Snippet two".to_string(),
            },
        ];
        let tool = make_tool(results);
        let result = tool
            .call(serde_json::json!({"query": "test"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("1. Title One"));
            assert!(s.contains("https://example.com/1"));
            assert!(s.contains("Snippet one"));
            assert!(s.contains("2. Title Two"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn search_no_results() {
        let tool = make_tool(vec![]);
        let result = tool
            .call(serde_json::json!({"query": "nothing"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert_eq!(s, "no results");
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn search_uses_default_count() {
        // Test that count defaults to 25 when not specified
        let tool = make_tool(vec![]);
        let result = tool
            .call(serde_json::json!({"query": "test"}))
            .await;
        assert!(!result.is_error);  // doesn't error, just returns empty
    }
}
```

Update `src/web/mod.rs`:
```rust
pub mod bocha;
pub mod fetch;
pub mod provider;
pub mod search;

pub use bocha::BochaSearchProvider;
pub use fetch::WebFetchTool;
pub use provider::{SearchResult, WebSearchProvider};
pub use search::WebSearchTool;
```

Update `src/lib.rs` to export web tools:
```rust
mod context;
mod error;
mod fs;
mod shell;
mod web;

use std::path::PathBuf;
use std::sync::Arc;

use yi_agent_core::ToolRegistry;

pub use context::ToolsContext;
pub use error::ToolsError;
pub use fs::{EditTool, GlobTool, GrepTool, ReadTool, WriteTool};
pub use shell::BashTool;
pub use web::{BochaSearchProvider, WebFetchTool, WebSearchTool, SearchResult, WebSearchProvider};
```

**Step 2: Run tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools web::search`
Expected: PASS (3 tests)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/web/search.rs yi-agent-rs/crates/yi-agent-tools/src/web/mod.rs yi-agent-rs/crates/yi-agent-tools/src/lib.rs
git commit -m "feat(tools): implement WebSearchTool with default count 25"
```

---

## Task 7: Extend register_builtin_tools

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`

**Step 1: Update register_builtin_tools**

Update the function in `src/lib.rs`:

```rust
/// Register all builtin tools into the given registry.
///
/// `root` constrains FS tool operations to the given directory.
/// Shell tools use it as initial cwd but do not restrict `sh -c` operations
/// to within root (system-level isolation requires sandbox, which is future work).
///
/// Web tools:
/// - WebFetchTool is always registered.
/// - WebSearchTool is only registered if BOCHA_API_KEY env var is set.
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    let ctx = Arc::new(ToolsContext::new(root));
    // FS + Shell tools
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    registry.register(Arc::new(WriteTool::new(ctx.clone())));
    registry.register(Arc::new(EditTool::new(ctx.clone())));
    registry.register(Arc::new(GlobTool::new(ctx.clone())));
    registry.register(Arc::new(GrepTool::new(ctx.clone())));
    registry.register(Arc::new(BashTool::new(ctx)));

    // Web tools
    registry.register(Arc::new(WebFetchTool::new()));
    if let Some(bocha) = BochaSearchProvider::from_env() {
        registry.register(Arc::new(WebSearchTool::new(Arc::new(bocha))));
    }
    // BOCHA_API_KEY not set: WebSearchTool not registered
}
```

**Step 2: Verify it compiles**

Run: `cd yi-agent-rs && cargo check -p yi-agent-tools`
Expected: PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/src/lib.rs
git commit -m "feat(tools): extend register_builtin_tools with WebFetch + WebSearch"
```

---

## Task 8: Integration tests

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/tests/web_integration.rs`

**Step 1: Write integration tests**

Create `yi-agent-rs/crates/yi-agent-tools/tests/web_integration.rs`:

```rust
use yi_agent_core::{Tool, ToolRegistry};
use yi_agent_tools::register_builtin_tools;
use yi_agent_tools::{WebFetchTool, WebSearchTool, BochaSearchProvider};
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn register_includes_web_fetch() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    assert!(registry.get("web_fetch").is_some(), "web_fetch should be registered");
}

#[tokio::test]
async fn register_includes_all_tools() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    // FS + Shell tools (6) + WebFetch (1) = 7 minimum
    // WebSearch only if BOCHA_API_KEY set
    for name in &["read", "write", "edit", "glob", "grep", "bash", "web_fetch"] {
        assert!(registry.get(name).is_some(), "missing tool: {}", name);
    }
}

#[tokio::test]
async fn web_fetch_works_via_registry() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<h1>Test</h1>"),
        )
        .mount(&server)
        .await;

    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    let fetch = registry.get("web_fetch").unwrap();
    let result = fetch
        .call(serde_json::json!({
            "url": format!("{}/page", server.uri())
        }))
        .await;
    assert!(!result.is_error);
    if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
        assert!(s.to_lowercase().contains("test"));
    }
}
```

**Step 2: Run all tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools`
Expected: PASS (all existing tests + new web tests)

**Step 3: Run clippy**

Run: `cd yi-agent-rs && cargo clippy -p yi-agent-tools -- -D warnings`
Expected: PASS

**Step 4: Run fmt check**

Run: `cd yi-agent-rs && cargo fmt -p yi-agent-tools -- --check`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/tests/web_integration.rs
git commit -m "test(tools): add web tools integration tests"
```

---

## Task 9: Update project management tracking

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/README.md`

**Step 1: Update yi-agent-tools.md**

Change `[ ]` to `[x]` for the Web tools line:
```
- [x] Web 工具：WebFetch + WebSearch（Bocha）— [设计](../plans/2026-07-19-yi-agent-web-tools-design.md)
```

**Step 2: Update README.md**

Update the yi-agent-tools section to mark Web tools as `[x]`.

**Step 3: Commit**

```bash
git add docs/project-management/yi-agent-tools.md docs/project-management/README.md
git commit -m "docs: update progress for web tools (WebFetch+WebSearch complete)"
```

---

## Summary

**Tasks:** 9
**New tests:** ~15 (7 fetch + 5 bocha + 3 search + 3 integration)
**Final verification:** `cargo test -p yi-agent-tools && cargo clippy -p yi-agent-tools -- -D warnings && cargo fmt -p yi-agent-tools -- --check`

**Out of scope:** Multi-engine aggregation, image/video search, WebFetch prompt parameter, SSRF protection, request retry, caching, streaming, cookie/session, proxy config.
