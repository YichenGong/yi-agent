# Tracing 记录 LLM 对话内容

## 背景

当前 tracing 只记录流程性信息(turn 号、消息条数、model、provider、HTTP status、tool input/is_error),完全不记录发给 LLM 的消息内容和 LLM 返回的内容,无法用于调试对话链路。

## 目标

按需记录"发给大模型的内容"和"大模型返回的内容",采用增量策略避免重复,通过 `--debug` flag 开关。

## 设计

### 触发方式

- `yi-agent` 新增 `--debug` 命令行 flag(布尔,默认 false)
- `tracing_init::init(debug: bool)` 接收 debug 参数:
  - `debug=false`:文件 filter = `info`(现状不变)
  - `debug=true`:文件 filter = `info,yi_agent_core=debug,yi_agent_llm=debug`
- stderr 层 `YI_LOG` 逻辑不变,仍由环境变量独立控制

### 记录策略:增量

每轮 `messages` 会把之前所有历史带上去,完整记录会产生 O(N²) 重复。采用增量:

- 维护游标 `last_logged`(初始 0)
- turn 1:记录 `messages[0..]`(全部,即初始 user message)+ system prompt
- turn N(N≥2):只记录 `messages[last_logged..]`(上一轮新增的 assistant message + tool results)
- 每轮记录后 `last_logged = messages.len()`

### 记录内容(全部用 `debug!`)

1. **request 增量**(agent.rs run_loop,`call_stream` 之前):
   - `debug!(turn, system = ?, new_msgs = ?&messages[last_logged..], "think: request delta")`
   - 用 `log_enabled!(target:"yi_agent_core", Level::DEBUG)` 预检,避免非 debug 模式下序列化开销
2. **response**(agent.rs run_loop,`accumulate_stream` 返回后):
   - `debug!(turn, content = ?content, "think: response")`
   - 每次都记(天然不重复)
3. **tool result 内容**:不单独记,靠下一轮 request 增量里的 ToolResult message 体现

### 改动文件清单

1. `crates/yi-agent/src/tracing_init.rs`:`init()` → `init(debug: bool)`,按 debug 切换文件 filter
2. `crates/yi-agent/src/main.rs`:clap 加 `--debug` flag,传入 `tracing_init::init`
3. `crates/yi-agent-core/src/agent.rs`(run_loop):
   - 顶部 `let mut last_logged = 0usize;`
   - `call_stream` 前记录 request 增量并更新游标
   - `accumulate_stream` 返回后记录 response

### 不改动

- provider client(anthropic/openai)不动,避免两处重复记录
- tool 执行日志保持现状(只记 input + is_error)
- `compact.rs`、`app.rs` 等不动

### 验证

- `cargo build` 编译通过
- `yi-agent --debug "hello"` 确认文件里有 request delta 和 response
- 不带 `--debug` 跑,确认文件里只有 info 级别日志,没有对话内容
