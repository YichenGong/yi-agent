use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_for_write;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

pub struct WriteTool {
    ctx: Arc<ToolsContext>,
}

impl WriteTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Create or overwrite a file in the workspace. Parent directories are created automatically."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: WriteArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let resolved = match resolve_for_write(self.ctx.root(), &args.path) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        // Create parent dirs if needed.
        if let Some(parent) = resolved.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolsError::Io(e).into();
            }
        }

        let bytes = args.content.as_bytes();
        if let Err(e) = std::fs::write(&resolved, bytes) {
            return ToolsError::Io(e).into();
        }

        ToolResult::text(format!("wrote {} bytes to {}", bytes.len(), args.path))
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> WriteTool {
        WriteTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[tokio::test]
    async fn write_new_file() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "out.txt", "content": "hello"}))
            .await;
        assert!(!result.is_error);
        let written = fs::read_to_string(tmp.path().join("out.txt")).unwrap();
        assert_eq!(written, "hello");
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({
                "path": "sub/dir/file.txt",
                "content": "nested"
            }))
            .await;
        assert!(!result.is_error);
        let written = fs::read_to_string(tmp.path().join("sub/dir/file.txt")).unwrap();
        assert_eq!(written, "nested");
    }

    #[tokio::test]
    async fn write_overwrites_existing() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "old").unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "file.txt", "content": "new"}))
            .await;
        assert!(!result.is_error);
        let written = fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(written, "new");
    }
}
