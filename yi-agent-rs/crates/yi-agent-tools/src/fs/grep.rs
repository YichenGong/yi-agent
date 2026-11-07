use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::walk;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

const MAX_OUTPUT_ENTRIES: usize = 200;
const MAX_OUTPUT_BYTES: usize = 32 * 1024;
const TRUNCATION_NOTICE: &str =
    "[truncated: narrow path, glob, or pattern; use files_with_matches or count]\n";
const DEADLINE_NOTICE: &str =
    "[scan stopped after 10s: narrow path or glob, some matches were not reached]\n";

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
                "path": { "type": "string", "description": "File or directory, default root (the session working directory)" },
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

        // 绝对路径按原样解释（可落在根集任一根内）；相对路径一律拼**主根**。
        let base = match &args.path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
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

        // The walk is synchronous and can touch thousands of files. Running it
        // on a tokio worker would park that thread for the whole scan and slow
        // down every other tool call in the same turn.
        let base_for_scan = base.clone();
        let roots = self.ctx.roots().to_vec();
        let glob_for_scan = glob_pattern.clone();

        let joined = tokio::task::spawn_blocking(move || {
            // Design §4.9: the 10 s bound must be taken *and* enforced inside
            // this closure. Taking it before `spawn_blocking` would charge the
            // queueing delay to the scan budget, so a saturated blocking pool
            // could report truncation after less than 10 s of real work. Do not
            // hoist this back out of the closure.
            let deadline = walk::deadline_from_now();
            grep_search(
                &base_for_scan,
                &re,
                glob_for_scan.as_ref(),
                mode,
                context,
                &roots,
                Some(deadline),
            )
        })
        .await;

        match joined {
            Ok(Ok((output, _timed_out))) => {
                if output.is_empty() {
                    ToolResult::text("no matches")
                } else {
                    ToolResult::text(output)
                }
            }
            Ok(Err(e)) => e.into(),
            Err(e) => ToolResult::error(format!("grep worker failed: {e}")),
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

/// Appends one rendered result line only when both context-safety budgets allow it.
fn append_entry(output: &mut String, entry_count: &mut usize, entry: String) -> bool {
    let content_budget = MAX_OUTPUT_BYTES - TRUNCATION_NOTICE.len();
    if *entry_count >= MAX_OUTPUT_ENTRIES || output.len() + entry.len() > content_budget {
        return false;
    }

    output.push_str(&entry);
    *entry_count += 1;
    true
}

fn grep_search(
    base: &Path,
    re: &regex::Regex,
    glob_filter: Option<&glob::Pattern>,
    mode: OutputMode,
    context: usize,
    roots: &[PathBuf],
    deadline: Option<Instant>,
) -> Result<(String, bool), ToolsError> {
    let mut output = String::new();
    let mut found_any = false;
    let mut entry_count = 0;
    let mut truncated = false;

    let walked = walk::walk_with_deadline(base, walk::FILES, deadline);
    let timed_out = matches!(walked.truncated, walk::Truncated::Deadline);

    'walk: for path in walked.paths.iter() {
        let path = path.as_path();

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

        // 命中哪个根就相对哪个根报，避免副根的匹配被报成主根下的路径；任一含它的
        // 根都没有时退回绝对路径（例如 `path` 直接传了根集之外的目录）。
        let rel = crate::context::relative_to_roots(roots, path)
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let shown = rel.as_str();

        match mode {
            OutputMode::Content => {
                for (i, line) in &file_matches {
                    if !append_entry(
                        &mut output,
                        &mut entry_count,
                        format!("{}:{}:{}\n", shown, i + 1, line),
                    ) {
                        truncated = true;
                        break 'walk;
                    }
                    if context > 0 {
                        let start = i.saturating_sub(context);
                        let end = (i + context + 1).min(lines.len());
                        for (j, context_line) in lines.iter().enumerate().take(end).skip(start) {
                            if j != *i
                                && !append_entry(
                                    &mut output,
                                    &mut entry_count,
                                    format!("{}-{}:{}\n", shown, j + 1, context_line),
                                )
                            {
                                truncated = true;
                                break 'walk;
                            }
                        }
                    }
                }
            }
            OutputMode::FilesWithMatches => {
                if !append_entry(&mut output, &mut entry_count, format!("{}\n", shown)) {
                    truncated = true;
                    break;
                }
            }
            OutputMode::Count => {
                if !append_entry(
                    &mut output,
                    &mut entry_count,
                    format!("{}:{}\n", shown, file_matches.len()),
                ) {
                    truncated = true;
                    break;
                }
            }
        }
    }

    if truncated {
        output.push_str(TRUNCATION_NOTICE);
    }
    if timed_out {
        output.push_str(DEADLINE_NOTICE);
    }

    if !found_any {
        // 即使一条都没找到，只要扫描被 deadline 截断，也必须说明，
        // 否则 "no matches" 会被误读为「确实没有」。
        Ok(if timed_out {
            (DEADLINE_NOTICE.to_string(), true)
        } else {
            (String::new(), false)
        })
    } else {
        Ok((output, timed_out))
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

    fn text_result(result: &ToolResult) -> &str {
        match &result.content[0] {
            yi_agent_core::ContentBlock::Text(text) => text,
            _ => panic!("expected a text result"),
        }
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

    #[tokio::test]
    async fn grep_reports_relative_path_against_the_matching_root() {
        let primary = TempDir::new().unwrap();
        let secondary = TempDir::new().unwrap();
        fs::write(secondary.path().join("sib.rs"), "fn main() {}\n").unwrap();
        let ctx = Arc::new(ToolsContext::with_roots(vec![
            primary.path().to_path_buf(),
            secondary.path().to_path_buf(),
        ]));
        let tool = GrepTool::new(ctx);
        let result = tool
            .call(serde_json::json!({
                "pattern": "fn main",
                "path": secondary.path().to_string_lossy(),
                "output_mode": "files_with_matches",
            }))
            .await;
        assert!(!result.is_error);
        // 必须相对**命中的那个根**（secondary）报路径，而不是绝对路径、
        // 也不是相对主根。等值断言才拦得住"退回旧行为"的回归。
        assert_eq!(text_result(&result), "sib.rs\n");
    }

    #[tokio::test]
    async fn grep_files_mode_truncates_many_matching_files() {
        let tmp = TempDir::new().unwrap();
        for index in 0..201 {
            fs::write(tmp.path().join(format!("match-{index}.txt")), "needle\n").unwrap();
        }
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({
                "pattern": "needle",
                "glob": "*.txt",
                "output_mode": "files_with_matches"
            }))
            .await;

        let text = text_result(&result);
        assert!(text.contains("[truncated:"));
        assert!(text.len() <= MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn grep_content_mode_truncates_large_matching_output() {
        let tmp = TempDir::new().unwrap();
        let large_line = format!("needle {}", "x".repeat(20_000));
        fs::write(
            tmp.path().join("large.txt"),
            format!("{large_line}\n{large_line}\n"),
        )
        .unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "needle"})).await;

        let text = text_result(&result);
        assert!(text.contains("[truncated:"));
        assert!(text.len() <= MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    async fn grep_skips_gitignored_build_output() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/generated.rs"), "needle\n").unwrap();
        fs::write(tmp.path().join("src.rs"), "needle\n").unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}))
            .await;

        let text = text_result(&result);
        assert!(
            text.contains("src.rs"),
            "the real source file must still match, got: {text}"
        );
        assert!(
            !text.contains("target/generated.rs"),
            "gitignored build output must be skipped, got: {text}"
        );
    }

    #[tokio::test]
    async fn grep_finds_matches_inside_dot_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        fs::write(
            tmp.path().join(".github/workflows/ci.yml"),
            "run: cargo test\n",
        )
        .unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"pattern": "cargo test", "output_mode": "files_with_matches"}))
            .await;

        let text = text_result(&result);
        assert!(
            text.contains(".github/workflows/ci.yml"),
            "hidden(false) keeps dot directories searchable, got: {text}"
        );
    }

    #[tokio::test]
    async fn grep_reports_deadline_truncation() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let tool = make_tool(&tmp);

        // 正常 deadline 下不得 panic、不得误报截断。
        // 真正的「过期 deadline」路径由下面的同步测试精确覆盖。
        let result = tool
            .call(serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}))
            .await;
        let text = text_result(&result);
        assert!(!result.is_error);
        assert!(
            !text.contains("[scan stopped after 10s:"),
            "a 200-file walk must finish well inside 10s, got: {text}"
        );
    }

    #[test]
    fn grep_reports_deadline_truncation_with_expired_deadline() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let re = regex::Regex::new("needle").unwrap();

        let (output, timed_out) = grep_search(
            tmp.path(),
            &re,
            None,
            OutputMode::FilesWithMatches,
            0,
            &[tmp.path().to_path_buf()],
            // `Instant` 由模块顶部的 `use std::time::Instant` 带入；
            // `Duration` 这里用全路径，避免测试模块再加一行 import。
            Some(
                Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .expect("clock is at least 1s past boot"),
            ),
        )
        .unwrap();

        assert!(
            timed_out,
            "an expired deadline must be reported to the caller"
        );
        assert!(
            output.contains("[scan stopped after 10s:"),
            "the user must be told the scan was cut short, got: {output}"
        );
    }
}
