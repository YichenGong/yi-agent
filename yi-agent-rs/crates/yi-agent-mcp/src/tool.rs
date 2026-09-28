//! `McpTool`: adapts a remote MCP tool to `yi_agent_core::Tool`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::manager::McpManager;
use crate::naming::qualified_name;

/// 把 MCP `annotations.readOnlyHint` 映射成 `ToolMetadata`。
///
/// 只有显式标记为只读的工具才不需要确认,其余一律需要确认。
pub(crate) fn metadata_from_annotations(
    server: &str,
    read_only_hint: Option<bool>,
) -> ToolMetadata {
    let read_only = read_only_hint.unwrap_or(false);
    ToolMetadata {
        source: ToolSource::Mcp {
            server_name: server.to_string(),
        },
        requires_confirmation: !read_only,
        read_only,
        version: None,
    }
}

/// 远端 MCP 工具到 `yi-agent-core::Tool` 的适配器。
///
/// 调用会转发给 `McpManager::call_tool`,由后者负责懒连接与超时。
pub struct McpTool {
    server: String,
    remote_name: String,
    qualified: String,
    description: String,
    input_schema: Value,
    read_only: bool,
    manager: Arc<McpManager>,
}

impl McpTool {
    pub(crate) fn new(
        server: String,
        remote_name: String,
        description: String,
        input_schema: Value,
        read_only: bool,
        manager: Arc<McpManager>,
    ) -> Self {
        let qualified = qualified_name(&server, &remote_name);
        Self {
            server,
            remote_name,
            qualified,
            description,
            input_schema,
            read_only,
            manager,
        }
    }

    /// Construct without a live manager — used by unit tests for name/metadata.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        server: &str,
        remote_name: &str,
        description: &str,
        input_schema: Value,
        read_only: bool,
    ) -> Self {
        let qualified = qualified_name(server, remote_name);
        // A manager is required by the struct; tests never call `call`.
        let manager = McpManager::empty_for_test();
        Self {
            server: server.into(),
            remote_name: remote_name.into(),
            qualified,
            description: description.into(),
            input_schema,
            read_only,
            manager,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }

    fn schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn metadata(&self) -> ToolMetadata {
        metadata_from_annotations(&self.server, Some(self.read_only))
    }

    async fn call(&self, args: Value) -> ToolResult {
        self.manager
            .call_tool(&self.server, &self.remote_name, args)
            .await
    }
}

#[cfg(test)]
mod meta_tests {
    use super::metadata_from_annotations;
    use yi_agent_core::ToolSource;

    #[test]
    fn read_only_hint_true_makes_tool_read_only_and_no_confirmation() {
        let meta = metadata_from_annotations("fs", Some(true));
        assert!(meta.read_only);
        assert!(!meta.requires_confirmation);
        assert_eq!(
            meta.source,
            ToolSource::Mcp {
                server_name: "fs".into()
            }
        );
    }

    #[test]
    fn read_only_hint_false_requires_confirmation() {
        let meta = metadata_from_annotations("fs", Some(false));
        assert!(!meta.read_only);
        assert!(meta.requires_confirmation);
    }

    #[test]
    fn absent_annotations_default_to_confirming() {
        let meta = metadata_from_annotations("fs", None);
        assert!(!meta.read_only);
        assert!(meta.requires_confirmation);
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    #[test]
    fn exposes_qualified_name_description_and_schema() {
        let tool = McpTool::new_for_test(
            "fs",
            "read_file",
            "Reads a file",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            true,
        );
        assert_eq!(tool.name(), "mcp__fs__read_file");
        assert_eq!(tool.description(), "Reads a file");
        assert_eq!(tool.schema()["type"], "object");
        let meta = tool.metadata();
        assert!(meta.read_only);
        assert!(!meta.requires_confirmation);
    }
}
