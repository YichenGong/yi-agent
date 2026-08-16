# yi-agent TUI (内联 CLI) 设计文档

**日期**: 2026-07-24
**状态**: 已确认，待实现
**范围**: `yi-agent-rs/crates/yi-agent` 的 TUI 架构、Renderer 抽象、并发模型、配置与渲染样式

---

## 1. 目标与定位

`yi-agent` 的 TUI 层是用户与 agent 交互的终端界面。起步版采用**内联流式 CLI** 风格（类似 Claude Code CLI），而非全屏 TUI（ratatui）。

**起步版目标**：
- 流式渲染 agent 输出（文本 + 工具调用 + 工具结果）
- 可中断的并发交互（agent 思考时用户可输入/中断）
- 通过 `Renderer` trait 解耦渲染层，保留未来切换全屏 TUI 的能力

**非目标（留给后续迭代）**：
- Markdown 渲染 / 语法高亮
- 可折叠工具调用块
- 全屏 TUI 布局 / 侧边栏 / 模态框
- Spinner / 进度条
- 配置文件（TOML）

**设计原则**：
- 渲染层与 agent 核心通过 `Renderer` trait 解耦，渲染层可替换
- 起步简单但不锁死未来选项：trait 抽象成本很低，但保留了切全屏 TUI 的可能
- 运行时错误不致命：agent 出错后用户可继续对话

---

## 2. 整体架构

```
┌─────────────────────────────────────────────────────┐
│  yi-agent (binary crate)                            │
│                                                     │
│  main.rs                                            │
│   ├─ parse args (clap)                              │
│   ├─ load config (env + CLI)                        │
│   ├─ build Agent (provider + tools)                 │
│   └─ run App                                        │
│                                                     │
│  app.rs ── App 循环:协调输入/输出/中断               │
│   ├─ InputTask:  reedline → 用户输入                │
│   ├─ AgentTask:  Agent::run() → AgentEvent stream   │
│   └─ Renderer:   trait,消费事件并渲染               │
│                                                     │
│  render/                                            │
│   ├─ mod.rs    ── Renderer trait 定义               │
│   └─ inline.rs ── InlineRenderer (起步实现)         │
│                                                     │
│  config.rs ── env + CLI 解析                        │
│  input.rs   ── reedline 封装 + slash 命令解析       │
│  agent_builder.rs ── 从 Config 构建 Agent           │
└─────────────────────────────────────────────────────┘
```

**数据流**：
```
用户输入 → reedline → UserCommand → App 循环
                                        ↓
                              Agent::run() → BoxStream<AgentEvent>
                                        ↓
                              Renderer::render_agent_event()
                                        ↓
                                     stdout
```

---

## 3. 核心决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| TUI 风格 | 内联流式 CLI | 最快跑通端到端，与现有事件流天然契合 |
| 渲染层抽象 | `Renderer` trait | 解耦渲染与 agent 核心，保留切换全屏 TUI 的能力 |
| 交互模式 | 可中断并发 | 体验最好，agent 思考时用户可随时输入/中断 |
| 输入库 | reedline | 成熟的多行编辑、历史、自定义 keybinding |
| 配置源 | 环境变量 + CLI 参数 | 起步最小集，不引入配置文件 |
| API 配置 | `MODEL_API_URL` + `MODEL_API_KEY` | provider-agnostic，可指向任意兼容端点 |
| 中断机制 | Ctrl+C + ESC，drop stream + CancellationToken | 双键分工：ESC 只在 agent 运行时生效避免与 reedline 冲突；优雅取消 agent 任务 |
| 输入输出混排 | 允许 | 与 Claude Code 体验一致 |

---

## 4. `Renderer` Trait

核心解耦点。定义在 `yi-agent/src/render/mod.rs`：

```rust
pub trait Renderer {
    /// 渲染用户输入的 prompt（回显）
    fn render_user_input(&mut self, text: &str);
    /// 渲染 agent 事件流中的一个事件
    fn render_agent_event(&mut self, event: &AgentEvent);
    /// 渲染错误
    fn render_error(&mut self, err: &AgentError);
    /// 渲染系统消息（如中断提示、状态）
    fn render_system(&mut self, msg: &str);
}
```

**关键约束**：trait 只负责"渲染"，不持有 agent 状态、不驱动 agent。这样：
- 渲染层可替换（`InlineRenderer` → 将来的 `TuiRenderer`）
- 测试时可用 `MockRenderer` 验证 App 逻辑

---

## 5. App 循环与并发模型

这是整个设计最核心的部分——如何让用户在 agent 思考时能随时输入（包括中断）。

### 并发模型

App 运行两个并发任务，通过 mpsc channel 通信：

```rust
// app.rs 核心循环（简化）
pub async fn run(self) -> Result<()> {
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<UserCommand>(16);

    // Task 1: 输入循环（同步阻塞的 reedline 放到 spawn_blocking）
    let cmd_tx_clone = cmd_tx.clone();
    tokio::task::spawn_blocking(move || {
        let mut line_editor = Reedline::create();
        loop {
            let line = line_editor.read_line(&Prompt).unwrap();
            let cmd = parse_user_input(&line);  // /quit, /clear, 或普通 prompt
            if cmd_tx_clone.blocking_send(cmd).is_err() { break; }
        }
    });

    // Task 2: 主循环（异步，消费命令 + 驱动 agent）
    let mut current_agent_stream: Option<BoxStream<AgentEvent>> = None;
    loop {
        tokio::select! {
            // 用户输入了新命令
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    UserCommand::Prompt(text) => {
                        let stream = agent.run(text.clone()).await?;
                        self.renderer.render_user_input(&text);
                        current_agent_stream = Some(stream);
                    }
                    UserCommand::Interrupt => { /* 取消当前 stream */ }
                    UserCommand::Quit => break,
                    // ...
                }
            }
            // agent 事件流有新事件
            Some(event) = async {
                match &mut current_agent_stream {
                    Some(s) => s.next().await,
                    None => None,
                }
            }, if current_agent_stream.is_some() => {
                match event {
                    Some(AgentEvent::Done { .. }) => current_agent_stream = None,
                    Some(e) => self.renderer.render_agent_event(&e),
                    None => current_agent_stream = None,
                }
            }
        }
    }
}
```

### 中断机制

支持两种中断键，分工如下：

| 键 | agent 空闲时 | agent 运行时 |
|----|-------------|-------------|
| Ctrl+C | 退出程序 | 中断 agent |
| ESC | reedline 内部键（多行编辑/emacs 模式切换） | 中断 agent |

- **Ctrl+C** 通过 `tokio::signal::ctrl_c()` 监听，触发 `UserCommand::Interrupt`
- **ESC** 通过独立 task 监听 stdin raw 字节流（crossterm `event::poll`），仅在 `current_agent_stream.is_some()` 时触发 `UserCommand::Interrupt`，避免与 reedline 的 ESC 键冲突
- 中断后丢弃当前 `current_agent_stream`（drop 即取消 spawned task），打印 `· 已中断`，回到输入态
- Agent 的 `run()` 内部用 `tokio::spawn`，drop stream 不会立即杀任务，但事件无处可去——需要让 `run()` 接受一个 `CancellationToken` 以优雅停止

### 关键设计点

1. **reedline 是同步阻塞的**，必须 `spawn_blocking`，否则会卡住 tokio runtime
2. **输入与输出不互斥**：agent 输出流式打印时，用户可以打字（终端会混排，但这正是 Claude Code 的体验）
3. **单 agent 流**：同一时刻只有一个活跃 agent stream，新 prompt 会等待上一个完成或被中断

---

## 6. InlineRenderer 渲染样式

### 渲染样式

起步版用 ANSI 颜色 + 简单符号区分角色，不引入 markdown 渲染库：

| 事件 | 渲染样式 |
|------|----------|
| 用户输入 | 整行背景色 + 前缀：`\x1b[48;5;240m 你: {text} \x1b[0m`（浅灰背景，dim 白字） |
| 助手文本 | `{text}`（默认色，流式追加，无前缀） |
| 工具调用 | `  ⚙ {name}({input_summary})`（yellow，2空格缩进） |
| 工具结果 | `  ↳ {result_summary}`（green=成功 / red=失败，dim） |
| 错误 | `✗ {error}`（red, bold） |
| 系统消息 | `· {msg}`（dim） |
| Done | 不打印（或极简的换行） |

- 用户输入背景色用 256 色中的浅灰（`48;5;240`），在深色/浅色终端上都可读，左右各留一个空格让背景色不贴边
- `input_summary` / `result_summary`：起步版用截断（如前 80 字符 + `...`），不做 JSON 美化

### 流式文本处理

`AgentEvent::AssistantText(String)` 是增量文本块。InlineRenderer 维护一个"当前行是否正在流式输出"的状态：

- 收到第一个 chunk 时不加前缀直接打印
- 后续 chunk 用 `print!` 追加（不换行），`flush`
- 收到非 `AssistantText` 事件时，如果上一行未换行，先补一个 `\n`

这样助手文本看起来是自然流式生长的，和 Claude Code 体验一致。

### 工具调用的渲染时机

`AgentEvent::ToolCall` 在 LLM 决定调用工具时立即发出，但此时工具还没执行完。渲染策略：

- `ToolCall` → 打印 `⚙ name(input_summary)`
- `ToolResult` → 打印 `↳ result_summary`

起步版**不加 spinner**，直接两行打印。spinner 留给后续"富文本渲染"迭代。

---

## 7. 配置与 CLI 参数

### 配置项

| 配置项 | 环境变量 | CLI 参数 | 默认值 |
|--------|----------|----------|--------|
| API URL | `MODEL_API_URL` | `--api-url` | 无（必填） |
| API Key | `MODEL_API_KEY` | `--api-key` | 无（必填） |
| 模型 | `YI_AGENT_MODEL` | `--model` | `claude-sonnet-4-20250514` |
| 最大轮数 | `YI_AGENT_MAX_TURNS` | `--max-turns` | `20` |
| 工作目录 | `YI_AGENT_WORKDIR` | `--workdir` | 当前目录 |
| 系统提示 | `YI_AGENT_SYSTEM_PROMPT` | `--system-prompt` | 内置默认 |

**优先级**：CLI 参数 > 环境变量 > 默认值。

`MODEL_API_URL` + `MODEL_API_KEY` 让用户可以指向任意兼容 Anthropic Messages API 的端点（官方、第三方代理、本地网关），不用改代码。

### CLI 参数（clap derive）

```rust
#[derive(Parser)]
#[command(name = "yi-agent", version, about = "Interactive AI agent CLI")]
struct Cli {
    /// API endpoint URL (overrides MODEL_API_URL)
    #[arg(long)]
    api_url: Option<String>,

    /// API key (overrides MODEL_API_KEY)
    #[arg(long)]
    api_key: Option<String>,

    /// Model to use
    #[arg(long)]
    model: Option<String>,

    /// Max agent turns per conversation
    #[arg(long)]
    max_turns: Option<usize>,

    /// Working directory for file system tools
    #[arg(long)]
    workdir: Option<PathBuf>,

    /// Custom system prompt
    #[arg(long)]
    system_prompt: Option<String>,
}
```

### Slash 命令（运行时）

在 reedline 输入中识别 `/` 开头的命令：

| 命令 | 作用 | 起步版实现 |
|------|------|------------|
| `/quit` `/q` | 退出 | ✅ |
| `/clear` | 清空当前对话上下文 | ✅ |
| `/help` | 显示帮助 | ✅ |
| `/model <name>` | 切换模型（下一轮生效） | 后续 |
| `/maxturns <n>` | 调整最大轮数 | 后续 |

---

## 8. 错误处理

**错误分层**：

1. **配置错误**（启动时）：API key/URL 缺失、参数非法 → 直接 `anyhow::bail!` 打印错误信息并退出，不进入 App 循环
2. **Agent 运行时错误**：Provider 连接失败、流解析错误 → 通过 `AgentEvent::Error(AgentError)` 流式返回，Renderer 渲染为红色 `✗`，**不退出**，用户可以继续输入下一个 prompt
3. **输入/IO 错误**：reedline 读取出错 → 打印警告，尝试恢复，连续失败才退出
4. **中断处理**：Ctrl+C 或 ESC → 取消当前 agent stream，打印 `· 已中断`，回到输入态，**不退出**

**关键原则**：除配置错误外，运行时错误不致命。Agent 出错后用户应该能继续对话，而不是被迫重启。

---

## 9. 测试策略

`Renderer` trait 的可测试性是核心。因为渲染逻辑和 App 逻辑解耦，可以分别测试：

1. **Renderer 单元测试**：实现一个 `MockRenderer`（或直接测 `InlineRenderer` 输出到 buffer），喂入构造的 `AgentEvent` 序列，断言输出包含预期的字符串/ANSI 序列
2. **App 逻辑测试**：用 `MockRenderer` + mock provider，验证：
   - 用户输入 prompt → 触发 `agent.run()`
   - 中断 → stream 被取消
   - `/quit` → 循环退出
   - `/clear` → 上下文重置
3. **集成测试**（少量）：真实启动 binary，通过 stdin/stdout pipe 交互，验证端到端流程。起步版只做 1-2 个 smoke test

**不测的**：reedline 的行为（第三方库）、真实 LLM API 调用（需要 mock）。

---

## 10. 依赖与模块清单

### 新增依赖（`yi-agent/Cargo.toml`）

| 依赖 | 用途 |
|------|------|
| `reedline` | 多行输入编辑、历史 |
| `clap` (derive) | CLI 参数解析 |
| `crossterm` | ANSI 颜色（轻量，不开 raw mode） |
| `tokio-util` | `CancellationToken` |
| `anyhow` | 错误处理 |
| `futures` | `BoxStream` / `StreamExt` |

**不引入**：`ratatui`（留给未来 `TuiRenderer`）、`syntect`（语法高亮留给后续）、`indicatif`（spinner 留给后续）。

### 模块清单

```
yi-agent-rs/crates/yi-agent/
├── Cargo.toml          # 新增上述依赖
├── src/
│   ├── main.rs         # 入口：parse cli → load config → build agent → run app
│   ├── config.rs       # Config struct + load(cli) 函数
│   ├── app.rs          # App 循环：并发协调输入/输出/中断
│   ├── input.rs        # reedline 封装 + slash 命令解析（UserCommand enum）
│   ├── render/
│   │   ├── mod.rs      # Renderer trait
│   │   └── inline.rs   # InlineRenderer 实现
│   └── agent_builder.rs # 从 Config 构建 Agent（provider + tools 注册）
```

---

## 11. 后续扩展路径

`Renderer` trait 是扩展的关键锚点。后续迭代按优先级：

1. **富文本渲染**：Markdown 渲染、代码块语法高亮、彩色输出、spinner（`syntect` + `indicatif`）
2. **输入增强**：自动补全、vim mode、slash 命令补全（reedline 原生支持）
3. **工具调用可视化**：可折叠块、完整 input/output JSON 展开
4. **复杂布局/面板**：实现 `TuiRenderer`（ratatui + crossterm），支持持久侧边栏、标签页、模态框

所有这些扩展都是**增量添加**，不需要改动 agent 核心或 App 循环——只需实现 `Renderer` trait 的新版本或增强现有 `InlineRenderer`。
