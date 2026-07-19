use std::sync::Arc;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_and_check;

pub struct EditTool {
    ctx: Arc<ToolsContext>,
}

impl EditTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file by replacing a unique old_string with new_string. Fails if old_string matches 0 or 2+ times."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string", "description": "Unique text to find" },
                "new_string": { "type": "string", "description": "Text to replace with" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: EditArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        if args.old_string.is_empty() {
            return ToolsError::EditFailed { reason: "old_string is empty".into() }.into();
        }
        if args.old_string == args.new_string {
            return ToolsError::EditFailed { reason: "old_string equals new_string".into() }.into();
        }

        let resolved = match resolve_and_check(self.ctx.root(), &args.path) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        match edit_file(&resolved, &args.old_string, &args.new_string) {
            Ok(()) => ToolResult::text(format!("edited {}: replaced 1 occurrence", args.path)),
            Err(e) => e.into(),
        }
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

fn edit_file(path: &std::path::Path, old_string: &str, new_string: &str) -> Result<(), ToolsError> {
    if !path.exists() {
        return Err(ToolsError::NotFound(path.to_path_buf()));
    }

    let content = std::fs::read_to_string(path)?;

    let count = content.matches(old_string).count();
    match count {
        0 => Err(ToolsError::EditFailed { reason: "old_string not found".into() }),
        1 => {
            let new_content = content.replacen(old_string, new_string, 1);
            std::fs::write(path, new_content)?;
            Ok(())
        }
        n => Err(ToolsError::EditFailed {
            reason: format!("old_string matched {} times, must be unique", n),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> EditTool {
        EditTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[tokio::test]
    async fn edit_unique_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "hello world").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({
            "path": "file.txt",
            "old_string": "hello",
            "new_string": "goodbye"
        })).await;
        assert!(!result.is_error);
        let written = fs::read_to_string(tmp.path().join("file.txt")).unwrap();
        assert_eq!(written, "goodbye world");
    }

    #[tokio::test]
    async fn edit_no_match() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "hello world").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({
            "path": "file.txt",
            "old_string": "missing",
            "new_string": "x"
        })).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn edit_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "foo foo foo").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({
            "path": "file.txt",
            "old_string": "foo",
            "new_string": "bar"
        })).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn edit_empty_old_string() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "hi").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({
            "path": "file.txt",
            "old_string": "",
            "new_string": "x"
        })).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn edit_same_strings() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "hi").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({
            "path": "file.txt",
            "old_string": "hi",
            "new_string": "hi"
        })).await;
        assert!(result.is_error);
    }
}
