# 真实 LLM 测试系统设计

Date: 2026-07-26

## 背景

当前 yi-agent 的 LLM 测试全部基于 `wiremock` mock HTTP 服务器,没有任何测试调用真实
LLM API。这导致:

1. 真实 API 的 SSE 格式变更、新模型适配、tool calling 行为无法回归验证
2. agent loop + 真实工具 + 真实模型的端到端行为无法验证
3. 当前 CLI 没有非交互式入口,无法从命令行直接跑真实 LLM 场景

## 目标

1. **Provider 层冒烟测试**:验证 `AnthropicProvider` / `OpenaiProvider` 对真实 API 的
   SSE 流解析、错误映射、鉴权
2. **Agent loop 端到端测试**:验证 think→act→observe 完整循环,含真实工具调用
3. **不破坏现有 CI**:`cargo test` / `just ci` 仍只跑 mock 测试,不调真实 API
4. **新增非交互式 CLI 入口** `yi-agent run`:既能用于测试,也有脚本化使用价值

## 设计决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 测试范围 | provider 层 + 端到端 | 覆盖完整链路,分层隔离 |
| Gate 机制 | `#[ignore]` + `--ignored` | Rust 生态标准做法,默认 `cargo test` 跳过,语义明确 |
| 端到端载体 | `yi-agent run` CLI 子命令 | 有产品价值(脚本化),测试通过 `std::process::Command` 调用 |
| 输入 | CLI 参数 + stdin 双支持 | 灵活,`echo "..." \| yi-agent run` 和 `yi-agent run "..."` 都可 |
| 输出格式 | 默认人读 + `--json` flag 切 JSONL | 默认友好,测试用 `--json` 断言 |
| 测试组织 | justfile recipe | `just test-real-*`,无 key 时 exit 0 跳过 |

## 架构

### 1. Provider 层冒烟测试

位置:`yi-agent-rs/crates/yi-agent-llm/tests/real_integration.rs`

全部用 `#[ignore]` 标记,默认 `cargo test` 跳过。

测试列表:

| 测试 | 验证点 |
|---|---|
| `real_anthropic_text_stream` | 真实 API 返回文本流,SSE 解析正确,文本可拼接 |
| `real_anthropic_tool_use` | 真实 API 返回 tool_use,`ToolUseStart/Delta/End` 事件正确 |
| `real_anthropic_call_accumulate` | `Provider::call()` 端到端,`ProviderResponse.content` 正确 |
| `real_anthropic_env_auth` | 无 `ANTHROPIC_API_KEY` 时返回 `Auth` 错误 |
| `real_openai_text_stream` | OpenAI 对应项 |
| `real_openai_tool_use` | OpenAI 对应项 |
| `real_openai_call_accumulate` | OpenAI 对应项 |
| `real_openai_env_auth` | OpenAI 对应项 |

每个测试开头双保险:
```rust
if std::env::var("ANTHROPIC_API_KEY").is_err() {
    eprintln!("skip: no ANTHROPIC_API_KEY");
    return;
}
```

跑法:
```bash
ANTHROPIC_API_KEY=sk-... cargo test -p yi-agent-llm --test real_integration -- --ignored
```

### 2. `yi-agent run` CLI 子命令

#### CLI 定义 (`yi-agent/src/config.rs`)

```rust
pub enum Command {
    /// Run a prompt non-interactively and exit (headless mode).
    Run {
        /// Prompt text. If omitted, reads from stdin.
        prompt: Option<String>,
        /// Output events as JSONL (one AgentEvent per line).
        #[arg(long)]
        json: bool,
        /// Read prompt from stdin even if prompt arg is given.
        #[arg(long)]
        stdin: bool,
    },
    Web { host: String, port: u16 },
}
```

#### main.rs 分支

新增 `Some(Command::Run { .. }) => run_headless(cli)`。复用现有 `config::load(&cli)`
拿 provider/tools/permission setup,只替换最后的 `run_tui_agent()` 为 headless drain。

#### Headless drain 逻辑

构造 `Agent`,调 `agent.run(prompt).await`,拿到 `BoxStream<AgentEvent>`,按事件类型输出:

**人读模式**(默认):
- `AgentEvent::AssistantText(t)` → `println!("{t}")` (stdout)
- `AgentEvent::ToolCall { name, input }` → `eprintln!("[tool:{name}] {input}")` (stderr)
- `AgentEvent::ToolResult { id, result }` → `eprintln!("[result:{id}] {result}")` (stderr)
- `AgentEvent::Done` / `Cancelled` / `Error` → `eprintln!("[{kind}]")` 后退出

**JSON 模式**(`--json`):
- 每个 `AgentEvent` 序列化为一行 JSON 输出到 stdout,`Done` 后退出
- 格式:JSONL (newline-delimited JSON),每行一个 `AgentEvent`

**退出码**:
- 成功 `EndTurn` → 0
- `Error` → 1
- `Cancelled` → 130

**Permission 处理**:headless 模式默认 `--yolo` 行为(自动允许非黑名单工具),避免
无人交互时卡在权限请求。黑名单命令直接 Deny,结果作为 tool_result 喂回模型。

### 3. Agent loop 端到端测试

位置:`yi-agent-rs/crates/yi-agent/tests/e2e_real.rs`

全部用 `#[ignore]` 标记。用 `std::process::Command::new("target/debug/yi-agent")` 调
`yi-agent run --json`,parse JSONL 用 `serde_json::Deserializer::from_reader` 断言。

测试列表:

| 测试 | 验证点 |
|---|---|
| `e2e_simple_text_response` | `yi-agent run --json "hi"` 返回 JSONL,含 `AssistantText` + `Done{EndTurn}` |
| `e2e_tool_use_read` | prompt 让 agent 读文件,JSONL 含 `ToolCall{read}` + `ToolResult` + 后续 `AssistantText` |
| `e2e_tool_use_bash` | prompt 让 agent 跑 bash,JSONL 含 `ToolCall{bash}` + `ToolResult` + 后续回复 |
| `e2e_error_no_api_key` | 无 key 时 `yi-agent run` 退出码 1,stderr 含 auth 错误信息 |

每个测试独立 `TempDir` 作 workdir,避免污染。

跑法:
```bash
ANTHROPIC_API_KEY=sk-... cargo test -p yi-agent --test e2e_real -- --ignored
```

### 4. justfile recipe

新增到 `yi-agent-rs/justfile`:

```makefile
# 跑真实 LLM provider 层测试(需 ANTHROPIC_API_KEY / OPENAI_API_KEY)
test-real-llm:
    #!/usr/bin/env bash
    set -e
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ]; then
        echo "skip: no API key set (ANTHROPIC_API_KEY / OPENAI_API_KEY)"
        exit 0
    fi
    cargo test -p yi-agent-llm --test real_integration -- --ignored

# 跑真实 LLM 端到端测试(需 API key)
test-real-e2e:
    #!/usr/bin/env bash
    set -e
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ]; then
        echo "skip: no API key set"
        exit 0
    fi
    cargo test -p yi-agent --test e2e_real -- --ignored

# 跑所有真实 LLM 测试
test-real-all: test-real-llm test-real-e2e
    @echo "Real LLM tests passed"
```

无 key 时 **exit 0 跳过**,不报失败 — 适合本地随意跑。

### 5. CI 不变

`just ci` 仍只跑 `fmt-check lint test build`,`test` = `cargo test --all-features
--workspace`,只跑 mock 测试。真实 LLM 测试不在 CI 跑(避免成本/密钥泄露)。

## 非目标 (YAGNI)

- 不引入 `serial_test` crate 替换现有手写 `ENV_LOCK`(现有方案虽不优雅但可工作)
- 不把 `Agent::session` 的 `std::sync::Mutex` 改为 `tokio::sync::Mutex`(当前未死锁,改造风险大于收益)
- 不在 CI 跑真实 LLM 测试
- 不做真实 LLM 的性能/延迟基准测试
- 不自动适配新模型(新模型手动加测试)

## CLAUDE.md 补充

在现有 `## cargo test 执行` 章节后追加 `## 真实 LLM 测试` 小节。
