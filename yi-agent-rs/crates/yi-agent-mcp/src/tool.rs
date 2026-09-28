//! `McpTool`: adapts a remote MCP tool to `yi_agent_core::Tool`.

use yi_agent_core::{ToolMetadata, ToolSource};

/// 把 MCP `annotations.readOnlyHint` 映射成 `ToolMetadata`。
///
/// 只有显式标记为只读的工具才不需要确认,其余一律需要确认。
// TODO(Task 6): McpTool 适配器接入后移除 `allow(dead_code)`。
#[allow(dead_code)]
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

/// 远端 MCP 工具到 `yi-agent-core::Tool` 的适配器(占位,后续任务实现)。
pub struct McpTool {}

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
