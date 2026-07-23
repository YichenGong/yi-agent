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
