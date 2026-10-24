use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::error::ToolsError;
use crate::web::provider::{SearchResult, WebSearchProvider};

const DEFAULT_COUNT: usize = 25;

pub struct WebSearchTool {
    engine: Arc<dyn WebSearchProvider>,
    /// 调用方未显式给 `count` 时用它。来自 `websearch.json`，可被设置页改动。
    default_count: usize,
}

impl WebSearchTool {
    pub fn new(engine: Arc<dyn WebSearchProvider>) -> Self {
        Self::with_default_count(engine, DEFAULT_COUNT)
    }

    pub fn with_default_count(engine: Arc<dyn WebSearchProvider>, default_count: usize) -> Self {
        Self {
            engine,
            default_count: default_count.clamp(1, 50),
        }
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

        let count = args.count.unwrap_or(self.default_count);
        tracing::info!(tool = "web_search", query = %args.query, count, "search start");

        match self.engine.search(&args.query, count).await {
            Ok(results) => {
                tracing::info!(
                    tool = "web_search",
                    result_count = results.len(),
                    "search done"
                );
                if results.is_empty() {
                    ToolResult::text("no results")
                } else {
                    let formatted = format_results(&results);
                    ToolResult::text(formatted)
                }
            }
            Err(e) => {
                tracing::warn!(tool = "web_search", error = %e, "search failed");
                e.into()
            }
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

fn format_results(results: &[SearchResult]) -> String {
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

    struct MockProvider {
        results: Vec<SearchResult>,
        last_count: std::sync::Mutex<Option<usize>>,
    }

    #[async_trait]
    impl WebSearchProvider for MockProvider {
        fn name(&self) -> &str {
            "mock"
        }
        async fn search(
            &self,
            _query: &str,
            count: usize,
        ) -> Result<Vec<SearchResult>, ToolsError> {
            *self.last_count.lock().unwrap() = Some(count);
            Ok(self.results.clone())
        }
    }

    fn make_tool(results: Vec<SearchResult>) -> (WebSearchTool, std::sync::Arc<MockProvider>) {
        let provider = std::sync::Arc::new(MockProvider {
            results,
            last_count: std::sync::Mutex::new(None),
        });
        let tool = WebSearchTool::new(provider.clone());
        (tool, provider)
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
        let (tool, _provider) = make_tool(results);
        let result = tool.call(serde_json::json!({"query": "test"})).await;
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
        let (tool, _provider) = make_tool(vec![]);
        let result = tool.call(serde_json::json!({"query": "nothing"})).await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert_eq!(s, "no results");
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn a_configured_default_count_replaces_the_hardcoded_25() {
        let provider = std::sync::Arc::new(MockProvider {
            results: vec![],
            last_count: std::sync::Mutex::new(None),
        });
        let tool = WebSearchTool::with_default_count(provider.clone(), 7);
        tool.call(serde_json::json!({"query": "test"})).await;
        assert_eq!(*provider.last_count.lock().unwrap(), Some(7));
    }

    #[tokio::test]
    async fn an_explicit_count_still_wins_over_the_default() {
        let provider = std::sync::Arc::new(MockProvider {
            results: vec![],
            last_count: std::sync::Mutex::new(None),
        });
        let tool = WebSearchTool::with_default_count(provider.clone(), 7);
        tool.call(serde_json::json!({"query": "test", "count": 3}))
            .await;
        assert_eq!(*provider.last_count.lock().unwrap(), Some(3));
    }

    #[tokio::test]
    async fn search_uses_default_count() {
        let (tool, provider) = make_tool(vec![]);
        tool.call(serde_json::json!({"query": "test"})).await;
        let captured = provider.last_count.lock().unwrap();
        assert_eq!(*captured, Some(25), "default count should be 25");
    }
}
