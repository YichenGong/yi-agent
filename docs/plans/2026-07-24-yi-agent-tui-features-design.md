# yi-agent TUI 功能扩展设计

**日期**: 2026-07-24
**状态**: 已确认，待实现
**范围**: `yi-agent-rs/crates/yi-agent` 和 `yi-agent-rs/crates/yi-agent-core`

---

## 1. 目标

补全三个核心能力：

1. **Conversation Compact** — 上下文超限时自动摘要旧消息，支持手动触发
2. **@path 文件引用** — 用户输入中用 `@path/to/file` 语法引用文件，内容自动拼入 prompt
3. **Slash 命令扩展** — 新增 `/model`、`/cost`、`/compact`、`/config`

---

## 2. Compact 机制

### 2.1 触发方式

- **手动**：用户输入 `/compact`
- **自动**：每轮 LLM 调用后累计 `input_tokens`，下一轮用户输入前若超过阈值则自动触发

### 2.2 Token 计数

用 `AgentEvent::Usage(TokenUsage)` 事件的 `input_tokens` 累计。这是 API 返回的真实 token 数，零依赖。缺点是事后才知道，但可在每轮结束后检查，下一轮开始前触发。

### 2.3 阈值设计

`AgentConfig` 新增字段：

```rust
pub compact_threshold: Option<u32>,  // 默认 100,000 tokens
pub compact_keep_turns: Option<u32>, // 默认 4 轮（8 条消息）
```

- threshold 默认 100K（Claude 200K 上下文窗口的 50%，留够空间给摘要请求）
- keep_turns 默认 4 轮，保留最近工具调用的上下文

### 2.4 摘要策略：结构化摘要 + 保留最近 N 轮

**流程**：
1. 取 `Session.messages`，分为 `[旧消息]` + `[最近 keep_turns*2 条]`
2. 用当前 provider 发摘要请求（结构化提示词）
3. 用摘要消息替换旧消息，保留最近 N 轮
4. 更新 session，重置 token 计数

**摘要提示词**：

```
请将以下对话历史总结为结构化摘要，用于后续对话的上下文。

请包含以下部分：
1. **用户意图**：用户的核心目标和需求
2. **关键决策**：已确定的方向、方案选择
3. **工具调用要点**：读取/修改的文件路径、执行的关键命令及其结果
4. **当前状态**：已完成的任务、未完成的任务、待解决的问题

请保持简洁，只保留对后续任务有帮助的信息。

对话历史：
{旧消息内容}
```

### 2.5 自动触发时机

在 `UserCommand::Prompt(text)` 分支中，**发送前**检查：

```rust
if self.session_token_count > threshold {
    self.compact_session().await?;
}
```

token 计数来自上一轮 Usage 事件，在"下一轮用户输入前"触发。

### 2.6 compact_session 实现

```rust
async fn compact_session(&mut self) -> Result<(), AgentError> {
    let session = self.agent.session();
    let keep_msgs = keep_turns * 2;  // 每轮 user+assistant
    let (old_msgs, recent_msgs) = split_at_keep(&session.messages(), keep_msgs);

    let summary = self.call_summary_llm(old_msgs).await?;

    let mut new_session = Session::new();
    new_session.push(Message::user(summary_text_with_header));
    new_session.push(recent_msgs...);

    self.agent = Agent::new(...).with_session(new_session);
    self.session_token_count = 0;
}
```

---

## 3. @path 文件引用

### 3.1 语法

- `@path/to/file.rs` — 相对路径，基于 workdir 解析
- `@"path with spaces.rs"` — 带空格的路径用引号
- `@/abs/path` — 绝对路径，必须在 workdir 内
- `@` 前面必须是空白或行首，避免误识别邮箱 `user@host`

### 3.2 替换格式

```
用户输入文本
--- @src/main.rs ---
<file content with line numbers>
--- end ---
其他文本
```

### 3.3 限制

- 最大 5000 行 / 50KB，超过则拒绝：`"文件过大(XXXX 行)，请让 agent 用 read 工具分段读取"`
- 路径必须在 workdir 内（与 ReadTool 相同约束）
- 不支持目录引用（指向目录时报错）

### 3.4 集成位置

在 `app.rs` 的 `UserCommand::Prompt(text)` 分支中，`self.agent.run(text)` 之前调用 `expand_file_refs`。解析失败时通过 `renderer.render_error` 显示错误，不发送给 agent。

```rust
let expanded = match expand_file_refs(&text, &self.config.workdir) {
    Ok(text) => text,
    Err(e) => {
        self.renderer.render_error(&e.into());
        continue;
    }
};
self.agent.run(expanded).await
```

---

## 4. Slash 命令扩展

### 4.1 新增命令

| 命令 | 行为 |
|------|------|
| `/model <name>` | 热切换模型，保留会话 |
| `/cost` | 显示累计 token 用量和估算费用 |
| `/compact` | 手动触发 compact |
| `/config` | 显示当前配置信息 |

### 4.2 /model 热切换

```rust
UserCommand::Model(name) => {
    self.config.model = name.clone();
    let session = self.agent.session();
    self.agent = Agent::new(
        Arc::clone(&self.provider),
        Arc::clone(&self.tools),
        self.config.clone(),
    ).with_session(session);
    self.renderer.render_system(&format!("模型已切换为 {name}"));
}
```

### 4.3 /cost 用量统计

App 新增字段：

```rust
total_input_tokens: u32,
total_output_tokens: u32,
```

在 `AgentEvent::Usage(usage)` 事件处累计。`/cost` 显示：

```
· 累计用量：input 12,345 tokens / output 6,789 tokens
· 估算费用：$0.XX
```

### 4.4 /compact 手动触发

复用 `compact_session()` 方法。手动触发时额外渲染压缩前后对比：

```
· 压缩前：20 条消息 / ~85,000 tokens
· 压缩后：9 条消息 / ~15,000 tokens
```

### 4.5 /config 显示

```
· 模型: claude-sonnet-4-20250514
· 工作目录: /path/to/workdir
· 最大轮数: 20
· Compact 阈值: 100,000 tokens
· Compact 保留轮数: 4
```

### 4.6 UserCommand 枚举变更

```rust
pub enum UserCommand {
    Prompt(String),
    Quit,
    Clear,
    Help,
    Model(String),    // 新增
    Cost,             // 新增
    Compact,          // 新增
    Config,           // 新增
}
```

---

## 5. 架构与改动范围

### 5.1 改动文件清单

| 文件 | 改动 |
|------|------|
| `yi-agent-core/src/agent.rs` | `AgentConfig` 加 `compact_threshold`、`compact_keep_turns` |
| `yi-agent/src/app.rs` | compact 逻辑、token 累计、新命令处理、`@path` 集成、模型热切换 |
| `yi-agent/src/input.rs` | `UserCommand` 加 `Model`/`Cost`/`Compact`/`Config`；解析逻辑更新 |
| `yi-agent/src/file_ref.rs` | **新文件**：`@path` 解析与文件读取 |
| `yi-agent/src/compact.rs` | **新文件**：摘要请求构建、session 压缩逻辑 |
| `yi-agent/src/config.rs` | 加 `compact_threshold`、`compact_keep_turns` CLI 参数和配置 |
| `yi-agent/src/render/inline.rs` | 渲染 `/cost`、`/config`、compact 进度消息 |

### 5.2 不改的部分

- `yi-agent-core` 的 `Provider` trait — compact 复用 `provider.call`
- `yi-agent-tools` — ReadTool 已存在
- `yi-agent-llm` — 不改
- `yi-agent-store` — 不涉及持久化
