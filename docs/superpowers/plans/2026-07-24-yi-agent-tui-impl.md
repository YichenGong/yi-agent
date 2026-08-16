# yi-agent TUI (内联 CLI) 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 为 yi-agent 实现内联流式 CLI，支持流式渲染 agent 输出、可中断并发交互、reedline 输入。

**Architecture:** `Renderer` trait 解耦渲染层与 agent 核心；App 循环用 `tokio::select!` 并发协调 reedline 输入（`spawn_blocking`）和 `AgentEvent` 流；Ctrl+C 中断（ESC 留作 TODO）。

**Tech Stack:** Rust 2024, tokio, reedline 0.49, clap 4, crossterm 0.28, tokio-util 0.7, anyhow, futures

**Design doc:** `docs/plans/2026-07-24-yi-agent-tui-design.md`

---

## 关键 API 速查

### 构建 Agent

```rust
use yi_agent_core::agent::{Agent, AgentConfig};
use yi_agent_core::tool::ToolRegistry;
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};
use yi_agent_tools::register_builtin_tools;

let provider = AnthropicProvider::new(AnthropicProviderOpts {
    base_url: Some(url), api_key: Some(key), ..Default::default()
})?;
let mut tools = ToolRegistry::new();
register_builtin_tools(&mut tools, workdir);
let config = AgentConfig { model, system_prompt, max_turns, ..Default::default() };
let agent = Agent::new(Arc::new(provider), Arc::new(tools), config);
```

### AgentEvent (yi-agent-core/src/agent.rs:78-94)

```rust
pub enum AgentEvent {
    Start, AssistantText(String),
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, result: ToolResult },
    Done { reason: DoneReason }, Error(AgentError),
}
```

### ToolResult (yi-agent-core/src/tool.rs:12-42)

```rust
pub struct ToolResult { pub content: Vec<ContentBlock>, pub is_error: bool }
```

提取文本：遍历 `result.content`，匹配 `ContentBlock::Text(s)` 即可。

### Agent::run 签名

```rust
pub async fn run(&mut self, user_prompt: String) -> Result<BoxStream<'static, AgentEvent>, AgentError>
```

注意：`Agent::run` 需要 `&mut self`。中断靠 drop stream。当前不接受 `CancellationToken`（留后续）。

---

## Task 1: 添加依赖

**Files:** Modify `yi-agent-rs/Cargo.toml`, `yi-agent-rs/crates/yi-agent/Cargo.toml`

**Step 1:** 编辑 `yi-agent-rs/Cargo.toml` 的 `[workspace.dependencies]`，替换注释掉的依赖为：

```toml
[workspace.dependencies]
yi-agent-core = { path = "crates/yi-agent-core" }
yi-agent-llm = { path = "crates/yi-agent-llm" }
yi-agent-tools = { path = "crates/yi-agent-tools" }
yi-agent-mcp = { path = "crates/yi-agent-mcp" }
yi-agent-store = { path = "crates/yi-agent-store" }

tokio = { version = "1", features = ["full"] }
anyhow = "1"
futures = "0.3"
clap = { version = "4", features = ["derive"] }
reedline = "0.49"
crossterm = "0.28"
tokio-util = "0.7"
```

**Step 2:** 编辑 `yi-agent-rs/crates/yi-agent/Cargo.toml`，在 `[dependencies]` 追加：

```toml
tokio = { workspace = true }
anyhow = { workspace = true }
futures = { workspace = true }
clap = { workspace = true }
reedline = { workspace = true }
crossterm = { workspace = true }
tokio-util = { workspace = true }
```

**Step 3:** Run `cd yi-agent-rs && cargo check -p yi-agent`
Expected: 编译通过

**Step 4:** Run `cd yi-agent-rs && cargo fmt --all -- --check && cargo clippy -p yi-agent -- -D warnings`
Expected: 通过

**Step 5:** Commit:
```bash
git add Cargo.toml crates/yi-agent/Cargo.toml
git commit -m "build(yi-agent): add TUI dependencies"
```

---

## Task 2: Config 模块

**Files:** Create `yi-agent-rs/crates/yi-agent/src/config.rs`

**Step 1:** 写 `config.rs`：

```rust
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "yi-agent", version, about = "Interactive AI agent CLI")]
pub struct Cli {
    #[arg(long)] pub api_url: Option<String>,
    #[arg(long)] pub api_key: Option<String>,
    #[arg(long)] pub model: Option<String>,
    #[arg(long)] pub max_turns: Option<usize>,
    #[arg(long)] pub workdir: Option<PathBuf>,
    #[arg(long)] pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_url: String,
    pub api_key: String,
    pub model: String,
    pub max_turns: usize,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
}

const DEFAULT_MODEL: &str = "claude-sonnet-4-20250514";
const DEFAULT_MAX_TURNS: usize = 20;

impl Config {
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let api_key = cli.api_key.clone()
            .or_else(|| std::env::var("MODEL_API_KEY").ok())
            .context("API key required: set MODEL_API_KEY or use --api-key")?;
        let api_url = cli.api_url.clone()
            .or_else(|| std::env::var("MODEL_API_URL").ok())
            .context("API URL required: set MODEL_API_URL or use --api-url")?;
        let model = cli.model.clone()
            .or_else(|| std::env::var("YI_AGENT_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let max_turns = cli.max_turns
            .or_else(|| std::env::var("YI_AGENT_MAX_TURNS").ok().and_then(|s| s.parse().ok()))
            .unwrap_or(DEFAULT_MAX_TURNS);
        let workdir = cli.workdir.clone()
            .or_else(|| std::env::var("YI_AGENT_WORKDIR").ok().map(PathBuf::from))
            .unwrap_or_else(|| std::env::current_dir().context("get current dir")?);
        let system_prompt = cli.system_prompt.clone()
            .or_else(|| std::env::var("YI_AGENT_SYSTEM_PROMPT").ok());
        if max_turns == 0 { bail!("max_turns must be > 0"); }
        Ok(Self { api_url, api_key, model, max_turns, workdir, system_prompt })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_with(url: &str, key: &str) -> Cli {
        Cli { api_url: Some(url.into()), api_key: Some(key.into()),
              model: None, max_turns: None, workdir: None, system_prompt: None }
    }

    #[test]
    fn config_uses_cli_values() {
        let c = Config::from_cli(&cli_with("https://x.com", "k")).unwrap();
        assert_eq!(c.api_url, "https://x.com");
        assert_eq!(c.model, DEFAULT_MODEL);
        assert_eq!(c.max_turns, DEFAULT_MAX_TURNS);
    }

    #[test]
    fn config_cli_overrides_env() {
        std::env::set_var("MODEL_API_KEY", "env-k");
        std::env::set_var("MODEL_API_URL", "https://env.com");
        let c = Config::from_cli(&cli_with("https://cli.com", "cli-k")).unwrap();
        assert_eq!(c.api_key, "cli-k");
        assert_eq!(c.api_url, "https://cli.com");
        std::env::remove_var("MODEL_API_KEY");
        std::env::remove_var("MODEL_API_URL");
    }

    #[test]
    fn config_falls_back_to_env() {
        std::env::set_var("MODEL_API_KEY", "env-k");
        std::env::set_var("MODEL_API_URL", "https://env.com");
        let c = Config::from_cli(&Cli { api_url: None, api_key: None,
            model: None, max_turns: None, workdir: None, system_prompt: None }).unwrap();
        assert_eq!(c.api_key, "env-k");
        std::env::remove_var("MODEL_API_KEY");
        std::env::remove_var("MODEL_API_URL");
    }

    #[test]
    fn config_errors_without_api_key() {
        std::env::remove_var("MODEL_API_KEY");
        std::env::remove_var("MODEL_API_URL");
        let cli = Cli { api_url: Some("https://x.com".into()), api_key: None,
            model: None, max_turns: None, workdir: None, system_prompt: None };
        assert!(Config::from_cli(&cli).is_err());
    }

    #[test]
    fn config_rejects_zero_max_turns() {
        let mut cli = cli_with("https://x.com", "k");
        cli.max_turns = Some(0);
        assert!(Config::from_cli(&cli).is_err());
    }
}
```

**Step 2:** 在 `main.rs` 添加 `mod config;`

**Step 3:** Run `cd yi-agent-rs && cargo test -p yi-agent config::`
Expected: 5 tests pass

**Step 4:** Run `cd yi-agent-rs && cargo fmt --all -- --check && cargo clippy -p yi-agent -- -D warnings`

**Step 5:** Commit:
```bash
git add crates/yi-agent/src/config.rs crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add Config module (env + CLI args)"
```

---

## Task 3: Renderer trait + InlineRenderer

**Files:** Create `yi-agent-rs/crates/yi-agent/src/render/mod.rs`, `render/inline.rs`

**Step 1:** 写 `render/mod.rs`：

```rust
pub mod inline;

use yi_agent_core::agent::{AgentError, AgentEvent};

pub trait Renderer {
    fn render_user_input(&mut self, text: &str);
    fn render_agent_event(&mut self, event: &AgentEvent);
    fn render_error(&mut self, err: &AgentError);
    fn render_system(&mut self, msg: &str);
}
```

**Step 2:** 写 `render/inline.rs`：

```rust
use std::io::{self, Write};

use yi_agent_core::agent::{AgentError, AgentEvent, DoneReason};
use yi_agent_core::message::ContentBlock;
use yi_agent_core::tool::ToolResult;

use super::Renderer;

pub struct InlineRenderer {
    streaming_text: bool,
}

impl InlineRenderer {
    pub fn new() -> Self { Self { streaming_text: false } }

    fn ensure_newline(&mut self) {
        if self.streaming_text {
            println!();
            self.streaming_text = false;
        }
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max { s.to_string() }
        else { format!("{}...", s.chars().take(max).collect::<String>()) }
    }

    fn tool_result_summary(result: &ToolResult) -> String {
        let mut out = String::new();
        for block in &result.content {
            match block {
                ContentBlock::Text(t) => out.push_str(t),
                _ => out.push_str("[non-text]"),
            }
        }
        Self::truncate(&out, 80)
    }
}

impl Default for InlineRenderer {
    fn default() -> Self { Self::new() }
}

impl Renderer for InlineRenderer {
    fn render_user_input(&mut self, text: &str) {
        self.ensure_newline();
        println!("\x1b[48;5;240m 你: {} \x1b[0m", text);
    }

    fn render_agent_event(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::Start => {}
            AgentEvent::AssistantText(text) => {
                print!("{}", text);
                io::stdout().flush().ok();
                self.streaming_text = true;
            }
            AgentEvent::ToolCall { name, input, .. } => {
                self.ensure_newline();
                println!("  \x1b[33m⚙ {}({})\x1b[0m", name, Self::truncate(&input.to_string(), 80));
            }
            AgentEvent::ToolResult { result, .. } => {
                self.ensure_newline();
                let summary = Self::tool_result_summary(result);
                if result.is_error {
                    println!("  \x1b[31m↳ {}\x1b[0m", summary);
                } else {
                    println!("  \x1b[2;32m↳ {}\x1b[0m", summary);
                }
            }
            AgentEvent::Done { reason } => {
                self.ensure_newline();
                if let DoneReason::MaxTurns = reason {
                    println!("\x1b[2m· 达到最大轮数限制\x1b[0m");
                }
            }
            AgentEvent::Error(err) => self.render_error(err),
        }
    }

    fn render_error(&mut self, err: &AgentError) {
        self.ensure_newline();
        println!("\x1b[1;31m✗ {}\x1b[0m", err);
    }

    fn render_system(&mut self, msg: &str) {
        self.ensure_newline();
        println!("\x1b[2m· {}\x1b[0m", msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short() { assert_eq!(InlineRenderer::truncate("hi", 10), "hi"); }

    #[test]
    fn truncate_long() {
        let result = InlineRenderer::truncate(&"a".repeat(100), 80);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn tool_result_summary_text() {
        assert_eq!(InlineRenderer::tool_result_summary(&ToolResult::text("hello")), "hello");
    }

    #[test]
    fn tool_result_summary_error() {
        let s = InlineRenderer::tool_result_summary(&ToolResult::error("boom"));
        assert!(s.contains("error: boom"));
    }
}
```

**Step 3:** 在 `main.rs` 添加 `mod render;`

**Step 4:** Run `cd yi-agent-rs && cargo test -p yi-agent render::`
Expected: 4 tests pass

**Step 5:** Run `cd yi-agent-rs && cargo fmt --all -- --check && cargo clippy -p yi-agent -- -D warnings`

**Step 6:** Commit:
```bash
git add crates/yi-agent/src/render/ crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add Renderer trait and InlineRenderer"
```

---

## Task 4: UserCommand + input 模块

**Files:** Create `yi-agent-rs/crates/yi-agent/src/input.rs`

**Step 1:** 写 `input.rs`：

```rust
#[derive(Debug, Clone)]
pub enum UserCommand {
    Prompt(String),
    Interrupt,
    Quit,
    Clear,
    Help,
}

pub fn parse_user_input(line: &str) -> UserCommand {
    match line.trim() {
        "/quit" | "/q" => UserCommand::Quit,
        "/clear" => UserCommand::Clear,
        "/help" => UserCommand::Help,
        _ => UserCommand::Prompt(line.to_string()),
    }
}

pub const HELP_TEXT: &str = "\
可用命令:
  /quit, /q   退出
  /clear      清空当前对话上下文
  /help       显示此帮助
  Ctrl+C      中断当前 agent 运行";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_quit() {
        assert!(matches!(parse_user_input("/quit"), UserCommand::Quit));
        assert!(matches!(parse_user_input("/q"), UserCommand::Quit));
    }

    #[test]
    fn parse_clear() { assert!(matches!(parse_user_input("/clear"), UserCommand::Clear)); }

    #[test]
    fn parse_help() { assert!(matches!(parse_user_input("/help"), UserCommand::Help)); }

    #[test]
    fn parse_prompt() {
        match parse_user_input("hello") { UserCommand::Prompt(s) => assert_eq!(s, "hello"), _ => panic!() }
    }

    #[test]
    fn parse_empty_is_prompt() {
        match parse_user_input("") { UserCommand::Prompt(s) => assert_eq!(s, ""), _ => panic!() }
    }

    #[test]
    fn parse_trims() { assert!(matches!(parse_user_input("  /quit  "), UserCommand::Quit)); }

    #[test]
    fn parse_unknown_slash_is_prompt() {
        match parse_user_input("/foo") { UserCommand::Prompt(s) => assert_eq!(s, "/foo"), _ => panic!() }
    }
}
```

**Step 2:** 在 `main.rs` 添加 `mod input;`

**Step 3:** Run `cd yi-agent-rs && cargo test -p yi-agent input::`
Expected: 7 tests pass

**Step 4:** Run fmt + clippy

**Step 5:** Commit:
```bash
git add crates/yi-agent/src/input.rs crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add UserCommand and input parsing"
```

---

## Task 5: agent_builder 模块

**Files:** Create `yi-agent-rs/crates/yi-agent/src/agent_builder.rs`

**Step 1:** 写 `agent_builder.rs`：

```rust
use std::sync::Arc;

use anyhow::Result;

use yi_agent_core::agent::{Agent, AgentConfig};
use yi_agent_core::tool::ToolRegistry;
use yi_agent_llm::{AnthropicProvider, AnthropicProviderOpts};
use yi_agent_tools::register_builtin_tools;

use crate::config::Config;

pub fn build_agent(config: &Config) -> Result<Agent> {
    let provider = AnthropicProvider::new(AnthropicProviderOpts {
        base_url: Some(config.api_url.clone()),
        api_key: Some(config.api_key.clone()),
        ..Default::default()
    })?;
    let mut tools = ToolRegistry::new();
    register_builtin_tools(&mut tools, config.workdir.clone());
    let agent_config = AgentConfig {
        model: config.model.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: Some(config.max_turns as u32),
        ..Default::default()
    };
    Ok(Agent::new(Arc::new(provider), Arc::new(tools), agent_config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn build_agent_succeeds() {
        let config = Config {
            api_url: "https://api.anthropic.com".into(),
            api_key: "test-key".into(),
            model: "claude-sonnet-4-20250514".into(),
            max_turns: 20,
            workdir: PathBuf::from("."),
            system_prompt: None,
        };
        assert!(build_agent(&config).is_ok());
    }
}
```

**Step 2:** 在 `main.rs` 添加 `mod agent_builder;`

**Step 3:** Run `cd yi-agent-rs && cargo test -p yi-agent agent_builder::`
Expected: 1 test pass

**Step 4:** Run fmt + clippy

**Step 5:** Commit:
```bash
git add crates/yi-agent/src/agent_builder.rs crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add agent_builder module"
```

---

## Task 6: App 循环

**Files:** Create `yi-agent-rs/crates/yi-agent/src/app.rs`

**Step 1:** 写 `app.rs`：

```rust
use futures::stream::{BoxStream, StreamExt};
use tokio::sync::mpsc;

use yi_agent_core::agent::{Agent, AgentEvent};

use crate::input::{parse_user_input, HELP_TEXT, UserCommand};
use crate::render::Renderer;

pub struct App {
    agent: Agent,
    renderer: Box<dyn Renderer>,
}

impl App {
    pub fn new(agent: Agent, renderer: Box<dyn Renderer>) -> Self {
        Self { agent, renderer }
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<UserCommand>(16);

        // 输入循环 (reedline 同步阻塞 → spawn_blocking)
        let input_tx = cmd_tx.clone();
        tokio::task::spawn_blocking(move || run_input_loop(input_tx));

        // Ctrl+C 监听
        let ctrl_c_tx = cmd_tx.clone();
        tokio::spawn(async move {
            loop {
                if tokio::signal::ctrl_c().await.is_err() { break; }
                let _ = ctrl_c_tx.send(UserCommand::Interrupt).await;
            }
        });

        // TODO: ESC 中断需要与 reedline 的 raw mode 协调,留到后续迭代

        let mut current_stream: Option<BoxStream<'static, AgentEvent>> = None;

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    match cmd {
                        UserCommand::Prompt(text) => {
                            if current_stream.is_some() {
                                current_stream = None;
                                self.renderer.render_system("已中断");
                            }
                            if text.trim().is_empty() { continue; }
                            match self.agent.run(text.clone()).await {
                                Ok(stream) => {
                                    self.renderer.render_user_input(&text);
                                    current_stream = Some(stream);
                                }
                                Err(e) => self.renderer.render_error(&e),
                            }
                        }
                        UserCommand::Interrupt => {
                            if current_stream.is_some() {
                                current_stream = None;
                                self.renderer.render_system("已中断");
                            }
                        }
                        UserCommand::Quit => {
                            self.renderer.render_system("再见");
                            break;
                        }
                        UserCommand::Clear => {
                            // TODO: 需要 Agent 支持 clear_session
                            self.renderer.render_system("对话已清空");
                        }
                        UserCommand::Help => {
                            self.renderer.render_system(HELP_TEXT);
                        }
                    }
                }
                Some(event) = async {
                    match &mut current_stream {
                        Some(s) => s.next().await,
                        None => None,
                    }
                }, if current_stream.is_some() => {
                    match event {
                        Some(AgentEvent::Done { .. }) | None => current_stream = None,
                        Some(e) => self.renderer.render_agent_event(&e),
                    }
                }
            }
        }
        Ok(())
    }
}

fn run_input_loop(cmd_tx: mpsc::Sender<UserCommand>) {
    use reedline::{DefaultPrompt, DefaultPromptSegment, Reedline, Signal};

    let mut line_editor = Reedline::create();
    let prompt = DefaultPrompt::new(DefaultPromptSegment::Empty, DefaultPromptSegment::Empty);

    loop {
        match line_editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                if cmd_tx.blocking_send(parse_user_input(&line)).is_err() { break; }
            }
            Ok(Signal::CtrlD) => {
                let _ = cmd_tx.blocking_send(UserCommand::Quit);
                break;
            }
            Err(_) => continue,
        }
    }
}
```

**Step 2:** 在 `main.rs` 添加 `mod app;`

**Step 3:** Run `cd yi-agent-rs && cargo check -p yi-agent && cargo clippy -p yi-agent -- -D warnings`

**Step 4:** Commit:
```bash
git add crates/yi-agent/src/app.rs crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): add App loop with concurrent input/agent streams"
```

---

## Task 7: main.rs 入口

**Files:** Modify `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1:** 替换 `main.rs` 全部内容：

```rust
mod agent_builder;
mod app;
mod config;
mod input;
mod render;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = config::Cli::parse();

    let config = match config::Config::from_cli(&cli) {
        Ok(c) => c,
        Err(e) => { eprintln!("配置错误: {e:#}"); return ExitCode::FAILURE; }
    };

    let agent = match agent_builder::build_agent(&config) {
        Ok(a) => a,
        Err(e) => { eprintln!("初始化失败: {e:#}"); return ExitCode::FAILURE; }
    };

    let renderer = Box::new(render::inline::InlineRenderer::new());
    let app = app::App::new(agent, renderer);

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => { eprintln!("无法创建 runtime: {e}"); return ExitCode::FAILURE; }
    };

    match rt.block_on(app.run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("运行时错误: {e:#}"); ExitCode::FAILURE }
    }
}
```

**Step 2:** Run `cd yi-agent-rs && cargo build -p yi-agent`
Expected: 构建成功

**Step 3:** 冒烟测试：
- `./target/debug/yi-agent --help` → 显示帮助
- `MODEL_API_KEY="" MODEL_API_URL="" ./target/debug/yi-agent` → 打印配置错误

**Step 4:** Commit:
```bash
git add crates/yi-agent/src/main.rs
git commit -m "feat(yi-agent): wire up main entry point"
```

---

## Task 8: 完整 CI 验证

**Step 1:** Run `cd yi-agent-rs && just ci`
Expected: fmt-check + lint + test + build 全部通过

**Step 2:** 修复任何失败后重新运行

**Step 3:** `git status` 确认 clean

---

## 已知简化（后续迭代）

1. **ESC 中断未实现** — 需与 reedline raw mode 协调
2. **CancellationToken 未引入** — 中断靠 drop stream
3. **`/clear` 未完全实现** — 需要 Agent 支持 clear_session
4. **无 spinner / markdown 渲染** — 留给"富文本渲染"迭代
