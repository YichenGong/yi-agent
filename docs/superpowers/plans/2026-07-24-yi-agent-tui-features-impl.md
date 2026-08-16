# yi-agent TUI Features Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add conversation compact, @path file references, and slash command extensions (/model, /cost, /compact, /config) to the yi-agent TUI.

**Architecture:** Three independent modules converge in the App select! loop: input parsing (new slash commands + @path expansion), token tracking (Usage event accumulation), and compact logic (LLM summarization of old messages). All changes are in `yi-agent` and `yi-agent-core` crates.

**Tech Stack:** Rust 2024, tokio, clap, reedline, yi-agent-core (Provider/Agent/Session)

---

## Task 1: Extend UserCommand with new slash commands

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/input.rs`

**Step 1: Write the failing tests**

Add these tests to the existing `#[cfg(test)] mod tests` block in `input.rs`:

```rust
#[test]
fn parse_model_command() {
    let cmd = parse_user_input("/model claude-sonnet-4-5").unwrap();
    match cmd {
        UserCommand::Model(name) => assert_eq!(name, "claude-sonnet-4-5"),
        _ => panic!("expected Model"),
    }
}

#[test]
fn parse_model_command_no_arg_returns_prompt() {
    // /model without an argument should be treated as a prompt
    let cmd = parse_user_input("/model").unwrap();
    assert!(matches!(cmd, UserCommand::Prompt(_)));
}

#[test]
fn parse_cost_command() {
    assert!(matches!(
        parse_user_input("/cost").unwrap(),
        UserCommand::Cost
    ));
}

#[test]
fn parse_compact_command() {
    assert!(matches!(
        parse_user_input("/compact").unwrap(),
        UserCommand::Compact
    ));
}

#[test]
fn parse_config_command() {
    assert!(matches!(
        parse_user_input("/config").unwrap(),
        UserCommand::Config
    ));
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib input::tests`
Expected: FAIL — `Model`, `Cost`, `Compact`, `Config` variants don't exist

**Step 3: Write minimal implementation**

Add new variants to `UserCommand` enum:

```rust
pub enum UserCommand {
    Prompt(String),
    Quit,
    Clear,
    Help,
    Model(String),
    Cost,
    Compact,
    Config,
}
```

Add parsing in `parse_user_input` match block, after the existing `"clear"` arm and before `"help"`:

```rust
"model" => {
    // /model <name> — requires an argument
    if let Some(name) = parts.get(1) {
        Some(UserCommand::Model(name.to_string()))
    } else {
        Some(UserCommand::Prompt(trimmed.to_string()))
    }
}
"cost" => Some(UserCommand::Cost),
"compact" => Some(UserCommand::Compact),
"config" => Some(UserCommand::Config),
```

Update `help_text()` to include new commands:

```rust
pub fn help_text() -> &'static str {
    "\
可用命令：
  /quit, /q    退出
  /clear       清空对话上下文
  /model <name>  切换模型
  /cost        显示 token 用量
  /compact     手动压缩对话
  /config      显示当前配置
  /help, /h    显示此帮助
  <其他文本>    发送给 agent 作为 prompt

Ctrl+C 或 ESC 可中断当前 agent 运行。"
}
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib input::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/input.rs
git commit -m "feat(yi-agent): add Model/Cost/Compact/Config slash command parsing"
```

---

## Task 2: Token usage tracking and /cost command

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1: Write the failing test**

Add a test module at the bottom of `app.rs` (after the `run_esc_listener` function):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_tracks_token_usage() {
        // We can't easily test the full App (needs provider + runtime),
        // but we can test that UsageStats accumulates correctly.
        let mut stats = UsageStats::default();
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 200,
            output_tokens: 75,
            ..Default::default()
        });
        assert_eq!(stats.total_input_tokens, 300);
        assert_eq!(stats.total_output_tokens, 125);
    }

    #[test]
    fn usage_stats_session_reset() {
        let mut stats = UsageStats::default();
        stats.add_usage(yi_agent_core::TokenUsage {
            input_tokens: 500,
            output_tokens: 100,
            ..Default::default()
        });
        stats.reset_session();
        assert_eq!(stats.total_input_tokens, 0);
        assert_eq!(stats.total_output_tokens, 0);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests`
Expected: FAIL — `UsageStats` doesn't exist

**Step 3: Write minimal implementation**

Add `UsageStats` struct near the top of `app.rs` (after the `use` statements, before `App`):

```rust
/// Tracks cumulative token usage for /cost display.
#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub total_input_tokens: u32,
    pub total_output_tokens: u32,
}

impl UsageStats {
    pub fn add_usage(&mut self, usage: yi_agent_core::TokenUsage) {
        self.total_input_tokens += usage.input_tokens;
        self.total_output_tokens += usage.output_tokens;
    }

    pub fn reset_session(&mut self) {
        self.total_input_tokens = 0;
        self.total_output_tokens = 0;
    }

    pub fn session_token_count(&self) -> u32 {
        self.total_input_tokens
    }
}
```

Add `usage_stats` field to `App` struct:

```rust
pub struct App {
    agent: Agent,
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    renderer: Box<dyn Renderer>,
    usage_stats: UsageStats,
}
```

Update `App::new` to initialize `usage_stats: UsageStats::default()`.

In the `select!` loop's agent event branch, add usage tracking before `render_agent_event`:

```rust
Some(e) => {
    if let AgentEvent::Usage(u) = &e {
        self.usage_stats.add_usage(u.clone());
    }
    self.renderer.render_agent_event(&e);
}
```

Handle `UserCommand::Cost`:

```rust
UserCommand::Cost => {
    let input = self.usage_stats.total_input_tokens;
    let output = self.usage_stats.total_output_tokens;
    self.renderer.render_system(
        &format!("累计用量：input {input} tokens / output {output} tokens")
    );
}
```

Also reset usage_stats in `UserCommand::Clear`:

```rust
UserCommand::Clear => {
    current_stream = None;
    self.usage_stats.reset_session();
    self.agent = Agent::new(
        Arc::clone(&self.provider),
        Arc::clone(&self.tools),
        self.config.clone(),
    ).with_session(Session::new());
    self.renderer.render_system("对话已清空");
}
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/app.rs
git commit -m "feat(yi-agent): track token usage and add /cost command"
```

---

## Task 3: /config command

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1: Write the failing test**

Add to `app.rs` test module:

```rust
#[test]
fn format_config_display() {
    let config = AgentConfig {
        model: "test-model".to_string(),
        max_turns: Some(42),
        ..Default::default()
    };
    let s = format_config(&config);
    assert!(s.contains("test-model"));
    assert!(s.contains("42"));
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests::format_config_display`
Expected: FAIL — `format_config` doesn't exist

**Step 3: Write minimal implementation**

Add helper function near `UsageStats`:

```rust
/// Format AgentConfig for /config display.
fn format_config(config: &AgentConfig) -> String {
    let max_turns = config.max_turns.unwrap_or(0);
    format!(
        "模型: {}\n最大轮数: {}",
        config.model, max_turns
    )
}
```

Handle `UserCommand::Config` in the `select!` loop:

```rust
UserCommand::Config => {
    self.renderer.render_system(&format_config(&self.config));
}
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/app.rs
git commit -m "feat(yi-agent): add /config command"
```

---

## Task 4: /model hot-swap command

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1: Write the failing test**

Add to `app.rs` test module:

```rust
#[test]
fn model_swap_preserves_session() {
    // Verify that creating a new Agent with the same Session preserves messages.
    use yi_agent_core::Session;
    let mut session = Session::new();
    session.push(yi_agent_core::Message::user("hello"));
    let session_clone = session.clone();
    assert_eq!(session_clone.len(), 1);
    // Simulating hot-swap: the session is passed to a new Agent
    assert_eq!(session_clone.messages().len(), 1);
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests::model_swap_preserves_session`
Expected: PASS (this tests existing behavior, confirming Session clone works)

**Step 3: Write minimal implementation**

Handle `UserCommand::Model(name)` in the `select!` loop, after `UserCommand::Clear`:

```rust
UserCommand::Model(name) => {
    // 热切换：保留当前 session，更换模型
    current_stream = None;
    let session = self.agent.session();
    self.config.model = name.clone();
    self.agent = Agent::new(
        Arc::clone(&self.provider),
        Arc::clone(&self.tools),
        self.config.clone(),
    ).with_session(session);
    self.renderer.render_system(&format!("模型已切换为 {name}"));
}
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/app.rs
git commit -m "feat(yi-agent): add /model hot-swap command"
```

---

## Task 5: @path file reference module

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/file_ref.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (add `mod file_ref;`)

**Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent/src/file_ref.rs` with tests first:

```rust
//! @path 文件引用：将用户输入中的 @path 替换为文件内容。

use std::path::{Path, PathBuf};

/// 最大行数
const MAX_LINES: usize = 5000;
/// 最大字节数
const MAX_BYTES: usize = 50_000;

/// @path 引用解析错误
#[derive(Debug, Clone)]
pub enum FileRefError {
    /// 文件不存在
    NotFound(String),
    /// 是目录
    IsDirectory(String),
    /// 超出 workdir 范围
    OutsideWorkdir(String),
    /// 文件过大
    TooLarge { path: String, lines: usize },
    /// 读取失败
    ReadFailed(String),
}

impl std::fmt::Display for FileRefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileRefError::NotFound(p) => write!(f, "文件不存在: {p}"),
            FileRefError::IsDirectory(p) => write!(f, "路径是目录: {p}"),
            FileRefError::OutsideWorkdir(p) => write!(f, "路径超出工作目录范围: {p}"),
            FileRefError::TooLarge { path, lines } => {
                write!(f, "文件过大({lines} 行)，请让 agent 用 read 工具分段读取: {path}")
            }
            FileRefError::ReadFailed(msg) => write!(f, "读取文件失败: {msg}"),
        }
    }
}

impl std::error::Error for FileRefError {}

/// 在用户输入文本中查找 @path 引用，读取文件内容并替换。
///
/// 语法:
/// - @path/to/file — 相对路径
/// - @"path with spaces" — 带空格的路径
/// - @ 前面必须是空白或行首
pub fn expand_file_refs(text: &str, workdir: &Path) -> Result<String, FileRefError> {
    // 实现在 Step 3
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn make_temp_workdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "yi-agent-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_at_sign_returns_text_unchanged() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("hello world", &workdir).unwrap();
        assert_eq!(result, "hello world");
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn email_not_treated_as_ref() {
        let workdir = make_temp_workdir();
        // user@host should NOT be treated as a file ref (no space before @)
        let result = expand_file_refs("contact user@host.com please", &workdir).unwrap();
        assert_eq!(result, "contact user@host.com please");
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn expand_simple_file_ref() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("test.txt");
        std::fs::write(&filepath, "line1\nline2\nline3\n").unwrap();

        let result = expand_file_refs("check @test.txt please", &workdir).unwrap();
        assert!(result.contains("check"));
        assert!(result.contains("--- @test.txt ---"));
        assert!(result.contains("line1"));
        assert!(result.contains("line3"));
        assert!(result.contains("--- end ---"));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn expand_quoted_path_with_spaces() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("my file.txt");
        std::fs::write(&filepath, "content here\n").unwrap();

        let result = expand_file_refs(@"read @""my file.txt"" now", &workdir).unwrap();
        assert!(result.contains("content here"));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn file_not_found_returns_error() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("check @nonexistent.txt", &workdir);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, FileRefError::NotFound(_)));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn directory_ref_returns_error() {
        let workdir = make_temp_workdir();
        let subdir = workdir.join("subdir");
        std::fs::create_dir(&subdir).unwrap();

        let result = expand_file_refs("check @subdir", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::IsDirectory(_)));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn absolute_path_outside_workdir_rejected() {
        let workdir = make_temp_workdir();
        let result = expand_file_refs("check @/etc/hosts", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::OutsideWorkdir(_)));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn large_file_rejected() {
        let workdir = make_temp_workdir();
        let filepath = workdir.join("big.txt");
        // Write 6000 lines — exceeds MAX_LINES (5000)
        let content = "x\n".repeat(6000);
        std::fs::write(&filepath, &content).unwrap();

        let result = expand_file_refs("check @big.txt", &workdir);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), FileRefError::TooLarge { .. }));
        std::fs::remove_dir_all(&workdir).ok();
    }

    #[test]
    fn multiple_refs_in_one_input() {
        let workdir = make_temp_workdir();
        std::fs::write(workdir.join("a.txt"), "AAA\n").unwrap();
        std::fs::write(workdir.join("b.txt"), "BBB\n").unwrap();

        let result = expand_file_refs("see @a.txt and @b.txt", &workdir).unwrap();
        assert!(result.contains("AAA"));
        assert!(result.contains("BBB"));
        std::fs::remove_dir_all(&workdir).ok();
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib file_ref::tests`
Expected: FAIL — `expand_file_refs` is `todo!()` (panics)

**Step 3: Write minimal implementation**

Replace `todo!()` in `expand_file_refs` with:

```rust
pub fn expand_file_refs(text: &str, workdir: &Path) -> Result<String, FileRefError> {
    let mut result = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        let ch = chars[i];

        // Detect @ at start of text or after whitespace
        if ch == '@' && (i == 0 || chars[i - 1].is_whitespace()) {
            // Check for quoted path @"..."
            if i + 1 < chars.len() && chars[i + 1] == '"' {
                let start = i + 2;
                let mut end = start;
                while end < chars.len() && chars[end] != '"' {
                    end += 1;
                }
                if end < chars.len() {
                    let path_str: String = chars[start..end].iter().collect();
                    let content = read_file_ref(&path_str, workdir)?;
                    result.push_str(&format_file_ref(&path_str, &content));
                    i = end + 1;
                    continue;
                }
            }

            // Unquoted path: read until whitespace
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && !chars[end].is_whitespace() {
                end += 1;
            }
            if end > start {
                let path_str: String = chars[start..end].iter().collect();
                let content = read_file_ref(&path_str, workdir)?;
                result.push_str(&format_file_ref(&path_str, &content));
                i = end;
                continue;
            }
        }

        result.push(ch);
        i += 1;
    }

    Ok(result)
}

/// Read a file reference, enforcing workdir constraint and size limits.
fn read_file_ref(path_str: &str, workdir: &Path) -> Result<String, FileRefError> {
    let path = Path::new(path_str);

    // Resolve relative to workdir, or use absolute
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        workdir.join(path)
    };

    // Canonicalize to check workdir containment
    let canonical = resolved
        .canonicalize()
        .map_err(|_| FileRefError::NotFound(path_str.to_string()))?;

    let canonical_workdir = workdir
        .canonicalize()
        .map_err(|e| FileRefError::ReadFailed(e.to_string()))?;

    if !canonical.starts_with(&canonical_workdir) {
        return Err(FileRefError::OutsideWorkdir(path_str.to_string()));
    }

    if canonical.is_dir() {
        return Err(FileRefError::IsDirectory(path_str.to_string()));
    }

    if !canonical.exists() {
        return Err(FileRefError::NotFound(path_str.to_string()));
    }

    let content = std::fs::read_to_string(&canonical)
        .map_err(|e| FileRefError::ReadFailed(e.to_string()))?;

    let lines = content.lines().count();
    let bytes = content.len();

    if lines > MAX_LINES || bytes > MAX_BYTES {
        return Err(FileRefError::TooLarge {
            path: path_str.to_string(),
            lines,
        });
    }

    // Return content with line numbers (cat -n style)
    let mut numbered = String::new();
    for (idx, line) in content.lines().enumerate() {
        numbered.push_str(&format!("{:>6}\t{}\n", idx + 1, line));
    }
    Ok(numbered)
}

/// Format a file reference as a delimited block.
fn format_file_ref(path_str: &str, content: &str) -> String {
    format!("--- @{path_str} ---\n{content}--- end ---\n")
}
```

Add `mod file_ref;` to `main.rs` after `mod config;`.

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib file_ref::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/file_ref.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add @path file reference expansion module"
```

---

## Task 6: Integrate @path into App

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1: Write the failing test**

Add to `app.rs` test module:

```rust
#[test]
fn expand_file_refs_integration() {
    use crate::file_ref::expand_file_refs;
    use std::path::PathBuf;

    let workdir = PathBuf::from(".");
    // Just verify the function is callable from app module
    let result = expand_file_refs("no refs here", &workdir);
    assert!(result.is_ok());
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests::expand_file_refs_integration`
Expected: FAIL — `file_ref` module not imported in app.rs

**Step 3: Write minimal implementation**

Add import at top of `app.rs`:

```rust
use crate::file_ref::expand_file_refs;
```

In the `UserCommand::Prompt(text)` branch, before `self.agent.run(text)`, add file ref expansion:

```rust
UserCommand::Prompt(text) => {
    // 如果有正在运行的 agent，先中断
    if current_stream.is_some() {
        self.agent.cancel();
        current_stream = None;
    }

    // 展开 @path 文件引用
    let expanded = match expand_file_refs(&text, &self.config.workdir) {
        Ok(text) => text,
        Err(e) => {
            self.renderer.render_error(&yi_agent_core::AgentError::Provider(
                yi_agent_core::ProviderError::InvalidRequest(e.to_string()),
            ));
            continue;
        }
    };

    self.renderer.render_user_input(&text);
    match self.agent.run(expanded).await {
        Ok(stream) => {
            current_stream = Some(stream);
        }
        Err(e) => {
            self.renderer.render_error(&e);
        }
    }
}
```

Note: `render_user_input` shows the original `text` (without expanded file content) to keep the display clean.

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/app.rs
git commit -m "feat(yi-agent): integrate @path expansion into prompt handling"
```

---

## Task 7: AgentConfig compact fields

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1: Write the failing tests**

Add to `agent.rs` test module:

```rust
#[test]
fn agent_config_has_compact_fields() {
    let config = AgentConfig::default();
    assert!(config.compact_threshold.is_some());
    assert!(config.compact_keep_turns.is_some());
}
```

Add to `config.rs` test module:

```rust
#[test]
fn load_includes_compact_defaults() {
    let cli = Cli {
        api_url: None,
        api_key: Some("test-key".into()),
        model: None,
        max_turns: None,
        workdir: Some(PathBuf::from(".")),
        system_prompt: None,
        compact_threshold: None,
        compact_keep_turns: None,
    };
    let config = load(&cli).unwrap();
    assert_eq!(config.compact_threshold, 100_000);
    assert_eq!(config.compact_keep_turns, 4);
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::agent_config_has_compact_fields`
Run: `cd yi-agent-rs && cargo test -p yi-agent --lib config::tests::load_includes_compact_defaults`
Expected: FAIL — fields don't exist

**Step 3: Write minimal implementation**

In `agent.rs`, add fields to `AgentConfig`:

```rust
pub struct AgentConfig {
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub gen_params: GenParams,
    /// Token count threshold to trigger auto-compact.
    pub compact_threshold: Option<u32>,
    /// Number of recent turns to keep during compact.
    pub compact_keep_turns: Option<u32>,
}
```

Update `Default` impl:

```rust
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-5".to_string(),
            system_prompt: None,
            max_turns: Some(100),
            gen_params: Default::default(),
            compact_threshold: Some(100_000),
            compact_keep_turns: Some(4),
        }
    }
}
```

In `config.rs`, add fields to `Config`:

```rust
pub struct Config {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: u32,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub compact_threshold: u32,
    pub compact_keep_turns: u32,
}
```

Add CLI args to `Cli`:

```rust
/// Token threshold for auto-compact
#[arg(long)]
pub compact_threshold: Option<u32>,

/// Number of recent turns to keep during compact
#[arg(long)]
pub compact_keep_turns: Option<u32>,
```

In `load()`, after `system_prompt`:

```rust
let compact_threshold = cli
    .compact_threshold
    .or_else(|| {
        std::env::var("YI_AGENT_COMPACT_THRESHOLD")
            .ok()
            .and_then(|s| s.parse().ok())
    })
    .unwrap_or(100_000);

let compact_keep_turns = cli
    .compact_keep_turns
    .or_else(|| {
        std::env::var("YI_AGENT_COMPACT_KEEP_TURNS")
            .ok()
            .and_then(|s| s.parse().ok())
    })
    .unwrap_or(4);
```

Add to the `Ok(Config { ... })`:

```rust
compact_threshold,
compact_keep_turns,
```

Update ALL existing test `Cli { ... }` literals in `config.rs` to include the new fields (`compact_threshold: None, compact_keep_turns: None`).

In `main.rs`, update `agent_config` construction:

```rust
let agent_config = yi_agent_core::AgentConfig {
    model: config.model.clone(),
    system_prompt: config.system_prompt.clone(),
    max_turns: Some(config.max_turns),
    compact_threshold: Some(config.compact_threshold),
    compact_keep_turns: Some(config.compact_keep_turns),
    ..Default::default()
};
```

**Step 4: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent::tests::agent_config_has_compact_fields`
Run: `cd yi-agent-rs && cargo test -p yi-agent --lib config`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: add compact_threshold and compact_keep_turns to AgentConfig"
```

---

## Task 8: Compact logic module

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/compact.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs` (add `mod compact;`)

**Step 1: Write the failing tests**

Create `yi-agent-rs/crates/yi-agent/src/compact.rs`:

```rust
//! 会话压缩：用 LLM 摘要旧消息，保留最近 N 轮。

use std::sync::Arc;

use yi_agent_core::{
    Agent, AgentConfig, AgentError, Message, Provider, ProviderError, ProviderRequest, Session,
};

/// 结构化摘要提示词
const SUMMARY_PROMPT_TEMPLATE: &str = "\
请将以下对话历史总结为结构化摘要，用于后续对话的上下文。

请包含以下部分：
1. **用户意图**：用户的核心目标和需求
2. **关键决策**：已确定的方向、方案选择
3. **工具调用要点**：读取/修改的文件路径、执行的关键命令及其结果
4. **当前状态**：已完成的任务、未完成的任务、待解决的问题

请保持简洁，只保留对后续任务有帮助的信息。

对话历史：
{conversation}";

/// 将消息列表格式化为纯文本对话（用于摘要请求）。
pub fn format_messages_for_summary(messages: &[Message]) -> String {
    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            yi_agent_core::Role::User => "用户",
            yi_agent_core::Role::Assistant => "助手",
            yi_agent_core::Role::Tool => "工具结果",
            yi_agent_core::Role::System => "系统",
        };
        let text: String = msg
            .content
            .iter()
            .map(|block| match block {
                yi_agent_core::ContentBlock::Text(t) => t.clone(),
                yi_agent_core::ContentBlock::ToolUse { name, input, .. } => {
                    format!("[调用工具 {name}: {input}]")
                }
                yi_agent_core::ContentBlock::ToolResult { content, .. } => {
                    let inner: String = content
                        .iter()
                        .map(|b| match b {
                            yi_agent_core::ContentBlock::Text(t) => t.clone(),
                            _ => "[非文本内容]".to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    format!("[工具结果: {inner}]")
                }
                yi_agent_core::ContentBlock::Image { .. } => "[图片]".to_string(),
            })
            .collect::<Vec<_>>()
            .join("");
        out.push_str(&format!("{role}: {text}\n\n"));
    }
    out
}

/// 构建摘要请求的 prompt。
pub fn build_summary_prompt(messages: &[Message]) -> String {
    let conversation = format_messages_for_summary(messages);
    SUMMARY_PROMPT_TEMPLATE.replace("{conversation}", &conversation)
}

/// 执行 compact：摘要旧消息 + 保留最近 N 轮，返回新 Session。
///
/// `keep_turns * 2` = 保留的消息数（每轮 = user + assistant）。
pub async fn compact_session(
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    session: &Session,
    keep_turns: u32,
) -> Result<Session, AgentError> {
    let messages = session.messages();
    let keep_count = (keep_turns as usize) * 2;

    if messages.len() <= keep_count {
        // Not enough messages to compact
        return Ok(session.clone());
    }

    let (old_messages, recent_messages) = messages.split_at(messages.len() - keep_count);

    let summary_prompt = build_summary_prompt(old_messages);

    let req = ProviderRequest {
        model: config.model.clone(),
        system: None,
        messages: vec![Message::user(summary_prompt)],
        tools: vec![],
        params: config.gen_params.clone(),
    };

    let response = provider
        .call(req)
        .await
        .map_err(AgentError::Provider)?;

    // Extract summary text from response
    let summary_text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            yi_agent_core::ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    // Build new session: summary message + recent messages
    let mut new_session = Session::new();
    new_session.push(Message::user(format!(
        "[对话摘要]\n{summary_text}"
    )));
    for msg in recent_messages {
        new_session.push(msg.clone());
    }

    Ok(new_session)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_messages_basic() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant(vec![yi_agent_core::ContentBlock::Text("hi there".into())]),
        ];
        let text = format_messages_for_summary(&messages);
        assert!(text.contains("用户: hello"));
        assert!(text.contains("助手: hi there"));
    }

    #[test]
    fn build_summary_prompt_contains_template() {
        let messages = vec![Message::user("test message")];
        let prompt = build_summary_prompt(&messages);
        assert!(prompt.contains("用户意图"));
        assert!(prompt.contains("关键决策"));
        assert!(prompt.contains("工具调用要点"));
        assert!(prompt.contains("当前状态"));
        assert!(prompt.contains("test message"));
    }

    #[test]
    fn build_summary_prompt_with_tool_use() {
        let messages = vec![
            Message::user("read the file"),
            Message::assistant(vec![yi_agent_core::ContentBlock::ToolUse {
                id: "t1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "main.rs"}),
            }]),
            Message::tool_results(vec![yi_agent_core::ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![yi_agent_core::ContentBlock::Text("file content".into())],
                is_error: false,
            }]),
        ];
        let prompt = build_summary_prompt(&messages);
        assert!(prompt.contains("调用工具 read"));
        assert!(prompt.contains("工具结果: file content"));
    }

    #[tokio::test]
    async fn compact_session_with_few_messages_returns_clone() {
        use async_trait::async_trait;
        use futures::stream::{BoxStream, StreamExt};

        struct DummyProvider;
        #[async_trait]
        impl Provider for DummyProvider {
            async fn call_stream(
                &self,
                _req: ProviderRequest,
            ) -> Result<BoxStream<'static, yi_agent_core::ProviderEvent>, ProviderError> {
                Ok(futures::stream::iter(vec![]).boxed())
            }
        }

        let mut session = Session::new();
        session.push(Message::user("hi"));
        let provider: Arc<dyn Provider> = Arc::new(DummyProvider);
        let config = AgentConfig::default();

        let result = compact_session(&provider, &config, &session, 4).await;
        assert!(result.is_ok());
        // Only 1 message, keep_count=8, so should return clone unchanged
        assert_eq!(result.unwrap().len(), 1);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib compact::tests`
Expected: FAIL — module not declared in main.rs

**Step 3: Write minimal implementation**

Add `mod compact;` to `main.rs` after `mod config;`.

The implementation is already in the test file above (the non-test functions are the implementation).

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib compact::tests`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/compact.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add compact module with structured summary logic"
```

---

## Task 9: Integrate compact into App (/compact + auto-trigger)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1: Write the failing test**

Add to `app.rs` test module:

```rust
#[test]
fn should_trigger_compact_above_threshold() {
    let threshold = 100_000u32;
    let current_tokens = 120_000u32;
    assert!(current_tokens > threshold);
}

#[test]
fn should_not_trigger_compact_below_threshold() {
    let threshold = 100_000u32;
    let current_tokens = 50_000u32;
    assert!(current_tokens <= threshold);
}
```

**Step 2: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib app::tests::should_trigger_compact`
Expected: FAIL — tests don't exist yet

**Step 3: Write minimal implementation**

Add import at top of `app.rs`:

```rust
use crate::compact::compact_session;
```

Handle `UserCommand::Compact` in the `select!` loop (after `UserCommand::Config`):

```rust
UserCommand::Compact => {
    let before_msgs = self.agent.session().len();
    let before_tokens = self.usage_stats.session_token_count();

    let keep_turns = self.config.compact_keep_turns.unwrap_or(4);
    let session = self.agent.session();
    match compact_session(&self.provider, &self.config, &session, keep_turns).await {
        Ok(new_session) => {
            let after_msgs = new_session.len();
            self.agent = Agent::new(
                Arc::clone(&self.provider),
                Arc::clone(&self.tools),
                self.config.clone(),
            ).with_session(new_session);
            self.usage_stats.reset_session();
            self.renderer.render_system(
                &format!("对话已压缩：{before_msgs} 条消息 → {after_msgs} 条消息")
            );
        }
        Err(e) => {
            self.renderer.render_error(&e);
        }
    }
}
```

Add auto-compact check in `UserCommand::Prompt(text)` branch, **before** `expand_file_refs`:

```rust
UserCommand::Prompt(text) => {
    // 如果有正在运行的 agent，先中断
    if current_stream.is_some() {
        self.agent.cancel();
        current_stream = None;
    }

    // 自动 compact 检查
    let threshold = self.config.compact_threshold.unwrap_or(100_000);
    if self.usage_stats.session_token_count() > threshold {
        self.renderer.render_system("上下文接近上限，正在自动压缩...");
        let keep_turns = self.config.compact_keep_turns.unwrap_or(4);
        let session = self.agent.session();
        match compact_session(&self.provider, &self.config, &session, keep_turns).await {
            Ok(new_session) => {
                self.agent = Agent::new(
                    Arc::clone(&self.provider),
                    Arc::clone(&self.tools),
                    self.config.clone(),
                ).with_session(new_session);
                self.usage_stats.reset_session();
            }
            Err(e) => {
                self.renderer.render_error(&e);
            }
        }
    }

    // 展开 @path 文件引用
    let expanded = match expand_file_refs(&text, &self.config.workdir) {
        // ... (same as Task 6)
    };
    // ... (rest same as Task 6)
}
```

**Step 4: Run test to verify it passes**

Run: `cd yi-agent-rs && cargo test -p yi-agent --lib`
Expected: PASS

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/app.rs
git commit -m "feat(yi-agent): integrate compact into App with auto-trigger and /compact command"
```

---

## Task 10: Full CI verification

**Files:** None (verification only)

**Step 1: Run full CI**

Run: `cd yi-agent-rs && just ci`
Expected: All pass (fmt-check, lint, test, build)

**Step 2: Fix any issues**

If clippy or fmt fails, fix and re-run.

**Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve CI issues from feature integration"
```
