use crate::context::ToolsContext;
use crate::error::ToolsError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use walkdir::WalkDir;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

pub struct GrepTool {
    ctx: Arc<ToolsContext>,
}

impl GrepTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    output_mode: Option<OutputMode>,
    #[serde(default)]
    context: Option<usize>,
}

#[derive(Deserialize, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with regex. Modes: content, files_with_matches, count."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern" },
                "path": { "type": "string", "description": "File or directory, default root" },
                "glob": { "type": "string", "description": "Filter files by glob, e.g. *.rs" },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "default": "content"
                },
                "context": { "type": "integer", "description": "Lines of context around matches, default 0" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: GrepArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let re = match regex::Regex::new(&args.pattern) {
            Ok(r) => r,
            Err(e) => return ToolsError::Regex(e).into(),
        };

        let base = match &args.path {
            Some(p) => self.ctx.root().join(p),
            None => self.ctx.root().to_path_buf(),
        };

        let glob_pattern = match args.glob.as_ref().map(|g| glob::Pattern::new(g)) {
            Some(Ok(p)) => Some(p),
            Some(Err(e)) => return ToolsError::Glob(e).into(),
            None => None,
        };

        let mode = args.output_mode.unwrap_or_default();
        let context = args.context.unwrap_or(0);

        match grep_search(
            &base,
            &re,
            glob_pattern.as_ref(),
            mode,
            context,
            self.ctx.root(),
        ) {
            Ok(output) => {
                if output.is_empty() {
                    ToolResult::text("no matches")
                } else {
                    ToolResult::text(output)
                }
            }
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

fn is_binary(path: &std::path::Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut buf = [0u8; 1024];
    let n = file.read(&mut buf).unwrap_or(0);
    buf[..n].contains(&0)
}

fn grep_search(
    base: &std::path::Path,
    re: &regex::Regex,
    glob_filter: Option<&glob::Pattern>,
    mode: OutputMode,
    context: usize,
    root: &std::path::Path,
) -> Result<String, ToolsError> {
    let mut output = String::new();
    let mut found_any = false;

    for entry in WalkDir::new(base).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        // Apply glob filter (matches file name).
        if let Some(pat) = glob_filter {
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !pat.matches(file_name) {
                continue;
            }
        }

        if is_binary(path) {
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();
        let mut file_matches: Vec<(usize, &str)> = Vec::new();

        for (i, line) in lines.iter().enumerate() {
            if re.is_match(line) {
                file_matches.push((i, *line));
            }
        }

        if file_matches.is_empty() {
            continue;
        }
        found_any = true;

        let rel = path.strip_prefix(root).unwrap_or(path);

        match mode {
            OutputMode::Content => {
                for (i, line) in &file_matches {
                    output.push_str(&format!("{}:{}:{}\n", rel.display(), i + 1, line));
                    if context > 0 {
                        let start = i.saturating_sub(context);
                        let end = (i + context + 1).min(lines.len());
                        for (j, context_line) in lines.iter().enumerate().take(end).skip(start) {
                            if j != *i {
                                output.push_str(&format!(
                                    "{}-{}:{}\n",
                                    rel.display(),
                                    j + 1,
                                    context_line
                                ));
                            }
                        }
                    }
                }
            }
            OutputMode::FilesWithMatches => {
                output.push_str(&format!("{}\n", rel.display()));
            }
            OutputMode::Count => {
                output.push_str(&format!("{}:{}\n", rel.display(), file_matches.len()));
            }
        }
    }

    if !found_any {
        Ok(String::new())
    } else {
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GrepTool {
        GrepTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    fn setup_sample_tree(tmp: &TempDir) {
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
        fs::write(tmp.path().join("src/b.rs"), "fn foo() {}\nfn baz() {}\n").unwrap();
        fs::write(tmp.path().join("README.md"), "# foo\nhello\n").unwrap();
    }

    #[tokio::test]
    async fn grep_content_mode() {
        let tmp = TempDir::new().unwrap();
        setup_sample_tree(&tmp);
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"pattern": "fn foo"})).await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("src/a.rs:1:fn foo"));
            assert!(s.contains("src/b.rs:1:fn foo"));
        }
    }

    #[tokio::test]
    async fn grep_files_mode() {
        let tmp = TempDir::new().unwrap();
        setup_sample_tree(&tmp);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({
                "pattern": "fn foo",
                "output_mode": "files_with_matches"
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("src/a.rs"));
            assert!(s.contains("src/b.rs"));
            assert!(!s.contains("README.md"));
        }
    }

    #[tokio::test]
    async fn grep_count_mode() {
        let tmp = TempDir::new().unwrap();
        setup_sample_tree(&tmp);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({
                "pattern": "fn foo",
                "output_mode": "count"
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            // a.rs has 1 match, b.rs has 1 match
            assert!(s.contains("src/a.rs:1"));
            assert!(s.contains("src/b.rs:1"));
        }
    }

    #[tokio::test]
    async fn grep_glob_filter() {
        let tmp = TempDir::new().unwrap();
        setup_sample_tree(&tmp);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({
                "pattern": "foo",
                "glob": "*.rs"
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("src/a.rs"));
            assert!(s.contains("src/b.rs"));
            assert!(!s.contains("README.md"));
        }
    }

    #[tokio::test]
    async fn grep_no_matches() {
        let tmp = TempDir::new().unwrap();
        setup_sample_tree(&tmp);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"pattern": "nonexistent_pattern"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert_eq!(s, "no matches");
        }
    }
}
