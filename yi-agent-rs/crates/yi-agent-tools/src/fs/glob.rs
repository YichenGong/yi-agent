use crate::context::ToolsContext;
use crate::error::ToolsError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

const UNBOUNDED_ROOT_GLOB_WARNING: &str = "[warning: unbounded recursive glob at repository root] \
prefer rg --files, a targeted directory, or a file-type constrained pattern; avoid heavy directories such as .git/, target/, node_modules/, and .worktrees/.";

pub struct GlobTool {
    ctx: Arc<ToolsContext>,
}

impl GlobTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern (supports ** for recursive). Returns paths relative to root."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, supports ** for recursive" },
                "path": { "type": "string", "description": "Base directory, default root" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: GlobArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let warn_unbounded_root_glob =
            is_unbounded_root_recursive_scan(args.path.as_deref(), &args.pattern);
        let base = match &args.path {
            Some(p) => self.ctx.root().join(p),
            None => self.ctx.root().to_path_buf(),
        };

        let full_pattern = base.join(&args.pattern);
        let pattern_str = match full_pattern.to_str() {
            Some(s) => s.to_string(),
            None => return ToolResult::error("path contains invalid UTF-8"),
        };

        // Handle errors from glob::glob() which returns Result<Paths, PatternError>
        let paths_iter = match glob::glob(&pattern_str) {
            Ok(it) => it,
            Err(e) => return ToolsError::Glob(e).into(),
        };

        let root = self.ctx.root();
        let mut matches: Vec<String> = Vec::new();
        for entry in paths_iter {
            match entry {
                Ok(path) => {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    matches.push(rel.to_string_lossy().to_string());
                }
                Err(_) => continue,
            }
        }

        let mut lines = Vec::new();
        if warn_unbounded_root_glob {
            lines.push(UNBOUNDED_ROOT_GLOB_WARNING.to_string());
        }

        if matches.is_empty() {
            lines.push("no matches".to_string());
        } else {
            lines.extend(matches);
        }

        ToolResult::text(lines.join("\n"))
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

fn is_unbounded_root_recursive_scan(path: Option<&str>, pattern: &str) -> bool {
    let root_path = match path.map(str::trim).filter(|p| !p.is_empty()) {
        None => true,
        Some(".") | Some("./") => true,
        Some(_) => false,
    };

    root_path && pattern.trim() == "**/*"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GlobTool {
        GlobTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[tokio::test]
    async fn glob_recursive_rs_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(tmp.path().join("src/lib.rs"), "").unwrap();
        fs::write(tmp.path().join("README.md"), "#").unwrap();

        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"pattern": "**/*.rs"})).await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("src/main.rs"));
            assert!(s.contains("src/lib.rs"));
            assert!(!s.contains("README.md"));
        }
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"pattern": "**/*.py"})).await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert_eq!(s, "no matches");
        }
    }

    #[tokio::test]
    async fn glob_warns_on_unbounded_root_recursive_scan() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("README.md"), "#").unwrap();

        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": ".", "pattern": "**/*"}))
            .await;

        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("[warning: unbounded recursive glob at repository root]"));
            assert!(s.contains("prefer rg --files"));
            assert!(s.contains("README.md"));
        }
    }
}
