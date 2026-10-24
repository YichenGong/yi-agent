//! 无 key 时的占位 provider。
//!
//! 为什么不是「干脆不注册工具」：那样模型看不到 `web_search`，用户也无从判断
//! 是"没配置"还是"功能坏了"。注册一个占位实现后，调用会拿到一条明确原因，
//! 模型能原样讲给用户听。

use async_trait::async_trait;

use crate::error::ToolsError;
use crate::web::provider::{SearchResult, WebSearchProvider};

pub struct UnconfiguredProvider;

#[async_trait]
impl WebSearchProvider for UnconfiguredProvider {
    fn name(&self) -> &str {
        "unconfigured"
    }

    async fn search(&self, _query: &str, _count: usize) -> Result<Vec<SearchResult>, ToolsError> {
        Err(ToolsError::SearchEngine(
            "web search is not configured: set an API key in Settings > 网页搜索".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn searching_without_a_key_says_so_instead_of_pretending() {
        let err = UnconfiguredProvider.search("q", 5).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("not configured"), "got: {msg}");
        // 必须指出去哪儿配，否则用户只看到「不可用」。
        assert!(msg.contains("网页搜索"), "got: {msg}");
    }

    #[test]
    fn its_name_is_not_a_real_engine() {
        assert_eq!(UnconfiguredProvider.name(), "unconfigured");
    }
}
