use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_and_check;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

pub struct ReadTool {
    ctx: Arc<ToolsContext>,
}

impl ReadTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 2000;

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file from the workspace. Returns content with line numbers (cat -n style)."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path within root"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start from (1-based), default 1"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to read, default 2000"
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ReadArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let resolved = match resolve_and_check(self.ctx.root(), &args.path) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        match read_file(&resolved, args.offset, args.limit) {
            Ok(output) => ToolResult::text(output),
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

fn read_file(
    path: &std::path::Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolsError> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolsError::NotFound(path.to_path_buf())
        } else {
            ToolsError::Io(e)
        }
    })?;

    if metadata.is_dir() {
        return Err(ToolsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "is a directory",
        )));
    }

    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    let offset = offset.unwrap_or(1).saturating_sub(1);
    let limit = limit.unwrap_or(DEFAULT_LIMIT);

    // Clamp offset to total to prevent slice panic when offset > total
    let offset = offset.min(total);
    let end = (offset + limit).min(total);
    let shown: Vec<String> = lines[offset..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>6}\t{}", offset + i + 1, line))
        .collect();

    let mut output = shown.join("\n");
    if end < total {
        output.push_str(&format!(
            "\n[truncated: showed {} of {} lines]",
            end - offset,
            total
        ));
    }

    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> ReadTool {
        ReadTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[tokio::test]
    async fn read_file_basic() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "line1\nline2\nline3\n").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "file.txt"})).await;
        assert!(!result.is_error);
        let text = &result.content[0];
        if let yi_agent_core::ContentBlock::Text(s) = text {
            assert!(s.contains("1\tline1"));
            assert!(s.contains("2\tline2"));
            assert!(s.contains("3\tline3"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn read_not_found() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "missing.txt"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn read_directory_errors() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "sub"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn read_truncates_long_file() {
        let tmp = TempDir::new().unwrap();
        let content: Vec<String> = (0..3000).map(|i| format!("line{}", i)).collect();
        fs::write(tmp.path().join("big.txt"), content.join("\n")).unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "big.txt"})).await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("[truncated:"));
            assert!(s.contains("of 3000 lines"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn read_with_offset_and_limit() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "file.txt", "offset": 2, "limit": 2}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("2\tl2"));
            assert!(s.contains("3\tl3"));
            assert!(!s.contains("l4"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn read_offset_beyond_end() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "l1\nl2\nl3\n").unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "file.txt", "offset": 100}))
            .await;
        // Should not panic; should return success with empty content
        assert!(!result.is_error);
    }
}
