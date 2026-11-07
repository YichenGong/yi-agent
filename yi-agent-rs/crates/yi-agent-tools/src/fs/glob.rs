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

const UNBOUNDED_ROOT_GLOB_WARNING: &str = "[warning: unbounded recursive glob at repository root] \
prefer rg --files, a targeted directory, or a file-type constrained pattern; avoid heavy directories such as .git/, target/, node_modules/, and .worktrees/.";

const DEADLINE_NOTICE: &str =
    "[scan stopped after 10s: narrow path or glob, some matches were not reached]";

/// Matching options that reproduce `glob::glob()`'s component-by-component
/// expansion.
///
/// `require_literal_separator: true` is load-bearing: with the default
/// (`false`), `*` crosses `/`, so `{"pattern": "*.rs"}` would return every
/// `.rs` in the tree instead of the root-level ones.
///
/// `require_literal_leading_dot: false` keeps the previous behaviour, where
/// `*` also matched dot-files such as `.gitignore`.
pub(crate) const MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

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
                "path": { "type": "string", "description": "Base directory, default root (the session working directory)" }
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
        // 绝对路径按原样解释（可落在根集任一根内）；相对路径一律拼**主根**。
        let base = match &args.path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.root().join(p),
            None => self.ctx.root().to_path_buf(),
        };

        // `Pattern::join` semantics: an absolute pattern replaces the base
        // entirely, so a pattern outside `base` is legal. We therefore walk
        // from the pattern's static prefix when it is absolute, and from
        // `base` otherwise.
        let pattern = match glob::Pattern::new(&args.pattern) {
            Ok(p) => p,
            Err(e) => return ToolsError::Glob(e).into(),
        };
        // `glob::Pattern` has no `is_absolute()`; the raw pattern is the only
        // reliable source for this bit.
        let pattern_is_absolute = Path::new(&args.pattern).is_absolute();

        let walk_base = if pattern_is_absolute {
            static_prefix(&args.pattern).unwrap_or_else(|| base.clone())
        } else {
            base.clone()
        };

        let roots = self.ctx.roots().to_vec();
        let walk_base_for_scan = walk_base.clone();
        let pattern_for_scan = pattern.clone();

        let joined = tokio::task::spawn_blocking(move || {
            // Design §4.9: the 10 s bound must be taken *and* enforced inside
            // this closure. Taking it before `spawn_blocking` would charge the
            // queueing delay to the scan budget, so a saturated blocking pool
            // could report truncation after less than 10 s of real work. Do not
            // hoist this back out of the closure.
            let deadline = walk::deadline_from_now();
            let (paths, timed_out) = scan(&walk_base_for_scan, &pattern_for_scan, Some(deadline));
            let shown: Vec<String> = paths
                .iter()
                .map(|path| {
                    crate::context::relative_to_roots(&roots, path)
                        .unwrap_or_else(|| path.to_string_lossy().to_string())
                })
                .collect();
            (shown, timed_out)
        })
        .await;

        let (mut matches, timed_out) = match joined {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("glob worker failed: {e}")),
        };
        matches.sort();

        render_result(warn_unbounded_root_glob, &matches, timed_out)
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

/// The literal directory prefix of `pattern`, up to the first component that
/// contains a wildcard. `None` when the pattern is not absolute or the first
/// component already has a wildcard.
fn static_prefix(pattern: &str) -> Option<PathBuf> {
    let path = Path::new(pattern);
    if !path.is_absolute() {
        return None;
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let as_str = component.as_os_str().to_string_lossy();
        if as_str.contains(['*', '?', '[']) {
            break;
        }
        prefix.push(component);
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Match `pattern` against every entry under `base`.
///
/// Returns the matched paths **as the walker produced them** (absolute), plus
/// whether the deadline cut the walk short. Turning them into display paths is
/// the caller's job: only it has the root set.
///
/// Uses `MATCH_TARGETS`, not `FILES`: the `glob()` this replaces returned
/// directories as matches and crossed symlinks. Both are load-bearing.
fn scan(base: &Path, pattern: &glob::Pattern, deadline: Option<Instant>) -> (Vec<PathBuf>, bool) {
    let walked = walk::walk_with_deadline(base, walk::MATCH_TARGETS, deadline);
    let timed_out = matches!(walked.truncated, walk::Truncated::Deadline);
    // `glob::Pattern` has no `is_absolute()`, but it does keep the original
    // text, which is enough to recover the bit.
    let pattern_is_absolute = Path::new(pattern.as_str()).is_absolute();

    let mut matches: Vec<PathBuf> = Vec::new();
    for path in walked.paths {
        // Absolute patterns match the absolute path; relative ones are matched
        // component-by-component against the path relative to `base`.
        let matched = if pattern_is_absolute {
            pattern.matches_path_with(&path, MATCH)
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            pattern.matches_path_with(rel, MATCH)
        };
        if matched {
            matches.push(path);
        }
    }
    (matches, timed_out)
}

/// Assemble the tool output.
///
/// A deadline-truncated scan must never fall through to "no matches": that
/// would present an incomplete scan as a definitive empty answer.
fn render_result(
    warn_unbounded_root_glob: bool,
    matches: &[String],
    timed_out: bool,
) -> ToolResult {
    let mut lines = Vec::new();
    if warn_unbounded_root_glob {
        lines.push(UNBOUNDED_ROOT_GLOB_WARNING.to_string());
    }
    if timed_out {
        lines.push(DEADLINE_NOTICE.to_string());
    }

    if matches.is_empty() {
        if !timed_out {
            lines.push("no matches".to_string());
        }
    } else {
        lines.extend(matches.iter().cloned());
    }

    ToolResult::text(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> GlobTool {
        GlobTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    fn text_result(result: &ToolResult) -> &str {
        match &result.content[0] {
            yi_agent_core::ContentBlock::Text(text) => text,
            _ => panic!("expected a text result"),
        }
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

    #[tokio::test]
    async fn glob_reports_relative_path_against_the_matching_root() {
        let primary = TempDir::new().unwrap();
        let secondary = TempDir::new().unwrap();
        fs::write(secondary.path().join("sib.rs"), "fn main() {}").unwrap();
        let ctx = Arc::new(ToolsContext::with_roots(vec![
            primary.path().to_path_buf(),
            secondary.path().to_path_buf(),
        ]));
        let tool = GlobTool::new(ctx);
        let result = tool
            .call(serde_json::json!({
                "pattern": "sib.rs",
                "path": secondary.path().to_string_lossy(),
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            // 必须相对**命中的那个根**（secondary）报路径，而不是绝对路径、
            // 也不是相对主根。等值断言才拦得住"退回旧行为"的回归。
            assert_eq!(s, "sib.rs");
        }
    }

    #[tokio::test]
    async fn glob_single_star_does_not_cross_directories() {
        // 与 glob() 的逐段展开一致：`*.rs` 只匹配根级。
        // 若 require_literal_separator 误设 false，这里会多出 src/main.rs。
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(tmp.path().join("top.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "*.rs"})).await;

        let text = text_result(&result);
        assert!(
            text.contains("top.rs"),
            "root-level file must match, got: {text}"
        );
        assert!(
            !text.contains("src/main.rs"),
            "`*` must not cross `/`, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_double_star_matches_at_any_depth() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("a/b/c/Cargo.toml"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"pattern": "**/Cargo.toml"}))
            .await;

        let text = text_result(&result);
        // Line equality, not `contains`: `contains("Cargo.toml")` is already
        // satisfied by the deep `a/b/c/Cargo.toml`, so it would keep passing
        // even if zero-depth matching broke.
        assert!(
            text.lines().any(|line| line == "Cargo.toml"),
            "zero depth must match, got: {text}"
        );
        assert!(
            text.contains("a/b/c/Cargo.toml"),
            "deep match must appear, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_still_matches_dot_files() {
        // require_literal_leading_dot: false 保持现行为：* 能命中 .gitignore。
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "*"})).await;

        let text = text_result(&result);
        assert!(
            text.contains(".gitignore"),
            "dot files must still match, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_skips_gitignored_build_output() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/out.rs"), "").unwrap();
        fs::write(tmp.path().join("src.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/*.rs"})).await;

        let text = text_result(&result);
        assert!(text.contains("src.rs"), "got: {text}");
        assert!(!text.contains("target/out.rs"), "got: {text}");
    }

    #[tokio::test]
    async fn glob_finds_matches_inside_dot_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        fs::write(tmp.path().join(".github/workflows/ci.yml"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/*.yml"})).await;

        let text = text_result(&result);
        assert!(
            text.contains(".github/workflows/ci.yml"),
            "hidden(false) keeps dot directories searchable, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_resolves_absolute_patterns() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let pattern = format!("{}/src/*.rs", tmp.path().to_string_lossy());
        let result = tool.call(serde_json::json!({"pattern": pattern})).await;

        let text = text_result(&result);
        assert!(
            text.contains("src/main.rs"),
            "absolute patterns must work, got: {text}"
        );
    }

    /// `glob()` 也把目录当匹配返回；若有人把 walker 换成 `walk::FILES`，
    /// `src` 就会静默消失。这条断言专门钉住 `MATCH_TARGETS`。
    #[tokio::test]
    async fn glob_matches_directories_as_well_as_files() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/*"})).await;

        let text = text_result(&result);
        assert!(text.contains("src/main.rs"), "got: {text}");
        assert!(
            text.lines().any(|line| line == "src"),
            "directories are matches too (MATCH_TARGETS), got: {text}"
        );
    }

    /// 截断时的输出契约：既要告诉用户「扫到 10 s 就停了」，也不能再说
    /// 「no matches」——那会让一次被提前终止的扫描伪装成「确实没有」。
    #[test]
    fn glob_output_reports_truncation_without_claiming_no_matches() {
        let empty_and_cut_short = render_result(false, &[], true);
        let text = text_result(&empty_and_cut_short);
        assert!(
            text.contains("scan stopped after 10s"),
            "truncation must be reported, got: {text}"
        );
        assert!(
            !text.contains("no matches"),
            "a cut-short scan must not claim there were no matches, got: {text}"
        );

        let truly_empty = text_result(&render_result(false, &[], false)).to_string();
        assert_eq!(truly_empty, "no matches");

        let with_hits = render_result(false, &["a.rs".to_string()], true);
        let text = text_result(&with_hits);
        assert!(
            text.contains("a.rs"),
            "partial results must still be shown, got: {text}"
        );
        assert!(
            text.contains("scan stopped after 10s"),
            "partial results must carry the truncation notice, got: {text}"
        );
    }

    #[test]
    fn glob_reports_deadline_truncation() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.rs")), "").unwrap();
        }
        let pattern = glob::Pattern::new("**/*.rs").unwrap();

        let (matches, timed_out) = scan(
            tmp.path(),
            &pattern,
            // `Instant` 由模块顶部的 `use std::time::Instant` 带入；
            // `Duration` 这里用全路径，避免测试模块再加一行 import。
            Some(
                Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .expect("clock is at least 1s past boot"),
            ),
        );

        assert!(timed_out, "expired deadline must be reported");
        assert!(
            matches.is_empty(),
            "expired deadline must not collect paths, got {matches:?}"
        );
    }
}
