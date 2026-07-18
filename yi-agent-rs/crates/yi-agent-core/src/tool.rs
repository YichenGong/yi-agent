//! Tool trait and registry.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::message::ContentBlock;

/// Result of tool execution, fed back to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
}

impl ToolResult {
    /// Success: single text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(text.into())],
            is_error: false,
        }
    }

    /// Error: text with "error: " prefix + is_error=true.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            content: vec![ContentBlock::Text(format!("error: {}", text.into()))],
            is_error: true,
        }
    }

    /// Multiple content blocks, not an error.
    pub fn with_content(content: Vec<ContentBlock>) -> Self {
        Self { content, is_error: false }
    }
}

/// Metadata describing a tool's non-behavioral properties.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolMetadata {
    pub source: ToolSource,
    pub requires_confirmation: bool,
    pub read_only: bool,
    pub version: Option<String>,
}

/// Where a tool comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ToolSource {
    #[default]
    Builtin,
    Mcp { server_name: String },
    Plugin { name: String },
}

/// Tool schema passed to the LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// All tools (builtin, MCP, plugins) implement this.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn schema(&self) -> Value;
    fn description(&self) -> &str;
    async fn call(&self, args: Value) -> ToolResult;

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata::default()
    }
}

/// Registry of tools keyed by name.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .values()
            .map(|t| ToolSchema {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.schema(),
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str { "echo" }
        fn schema(&self) -> Value {
            serde_json::json!({"type": "object", "properties": {"msg": {"type": "string"}}})
        }
        fn description(&self) -> &str { "Echoes input" }
        async fn call(&self, args: Value) -> ToolResult {
            ToolResult::text(args.to_string())
        }
    }

    #[test]
    fn tool_result_text_constructor() {
        let r = ToolResult::text("hello");
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 1);
    }

    #[test]
    fn tool_result_error_constructor() {
        let r = ToolResult::error("boom");
        assert!(r.is_error);
        match &r.content[0] {
            ContentBlock::Text(s) => assert!(s.starts_with("error:")),
            _ => panic!("expected Text"),
        }
    }

    #[test]
    fn tool_result_with_content() {
        let blocks = vec![ContentBlock::Text("a".into()), ContentBlock::Text("b".into())];
        let r = ToolResult::with_content(blocks);
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 2);
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        assert_eq!(reg.len(), 1);
        assert!(reg.get("echo").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn registry_schemas() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(EchoTool));
        let schemas = reg.schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "echo");
        assert_eq!(schemas[0].description, "Echoes input");
    }

    #[tokio::test]
    async fn tool_call_returns_result() {
        let tool = EchoTool;
        let result = tool.call(serde_json::json!({"msg": "hi"})).await;
        assert!(!result.is_error);
    }

    #[test]
    fn tool_metadata_default() {
        let tool = EchoTool;
        let meta = tool.metadata();
        assert_eq!(meta.source, ToolSource::Builtin);
        assert!(!meta.requires_confirmation);
        assert!(!meta.read_only);
        assert!(meta.version.is_none());
    }
}
