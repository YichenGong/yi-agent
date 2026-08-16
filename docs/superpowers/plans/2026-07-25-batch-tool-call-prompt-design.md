# 鼓励批量 Tool Call 的 Prompt 引导设计

日期: 2026-07-25

## 背景与目标

当前项目中,agent loop 已通过 `futures::future::join_all` 支持并行执行多个 tool call
(`crates/yi-agent-core/src/agent.rs:299-353`),流式响应累积也能收集任意数量的
`ContentBlock::ToolUse`(`crates/yi-agent-core/src/provider.rs:86-136`)。

但是 `AgentConfig.system_prompt` 默认为 `None`(`agent.rs:64-74`),模型完全没有
关于"如何使用工具"的引导,纯靠模型自身的 function calling 直觉。这导致模型倾向
于一轮只返回一个 tool call,增加了不必要的 LLM 调用轮次。

目标: 通过 prompt 层面的引导(不引入新工具、不改 agent loop),鼓励模型:
- 独立操作时一轮返回多个 tool call(利用已有的并行执行能力)
- 依赖串行操作时用 `&&` 合并成一个 bash 调用(减少跨轮次等待)

## 方案选择

考虑过三种方案:

| 方案 | 思路 | 取舍 |
|------|------|------|
| **A(采用)** | 改 system_prompt + bash description | 改动最小,YAGNI;直接命中两类场景 |
| B | 引入 `batch`/`plan` 新工具 | 语义清晰但重复造轮子(bash 已是批量执行器),违背 YAGNI |
| C | 检测连续单工具模式动态注入提示 | 需状态跟踪+检测逻辑,误判风险高 |

选 A:最小改动,先看效果,后续不够再加机制。

## 设计

### 改动范围

总共改三个文件,不引入新工具、不改 agent loop:

**文件 A: `crates/yi-agent-tools/src/shell/bash.rs`**
- 改 `description()` 方法(第 41-43 行),加一句 `&&` 合并引导

**文件 B: `crates/yi-agent-core/src/agent.rs`**
- 新增 `AgentConfig::default_system_prompt()` 公开方法,返回内置文案
- 改 `AgentConfig::default()`,把 `system_prompt: None` 改成
  `Some(Self::default_system_prompt())`

**文件 C: `crates/yi-agent/src/main.rs`**
- 新增 `resolve_system_prompt()` 辅助函数:用户未传 `system_prompt` 时回退到
  `default_system_prompt()`
- `main.rs` 在构造 `AgentConfig` 时显式设置 `system_prompt` 字段,会覆盖
  `..Default::default()` 的值。因此仅改 `AgentConfig::default()` 不够——当用户
  未传 `--system-prompt` 时,`config.system_prompt` 为 `None`,会反过来把默认值
  覆盖成 `None`。`resolve_system_prompt` 修复了这个回退路径
- 用户一旦通过 `--system-prompt` 或 `YI_AGENT_SYSTEM_PROMPT` 传了自定义值,
  `resolve_system_prompt` 直接用用户的,不拼接——符合"仅默认场景注入"的语义

### 内置系统提示文案

```
You are yi-agent. You are a helpful general purpose agent designed by Gong Yichen (宫一尘). You have logical thinking, aim for the best, execute perfectly and always speak with evidence.

You work efficiently by minimizing round-trips. Tool use strategy:
- Independent operations (reading multiple files, parallel searches): issue
  MULTIPLE tool calls in a single response. They will be executed in parallel.
- Dependent operations that must run in sequence (create dir → write file →
  run tests): combine them into ONE bash call using && so the whole sequence
  completes in a single step.
- Only split work across turns when a later step genuinely depends on the
  RESULT of an earlier step.

Example: instead of 3 turns (mkdir, write, test), use one bash call:
  mkdir -p src/utils && echo '...' > src/utils/mod.rs && cargo test
```

文案结构:
1. 身份说明(由用户指定)
2. 工具使用策略,明确区分两类场景
3. 具体例子,给模型可参照的 pattern

### bash description 修改

现有:
```
Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls.
```

改为:
```
Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls. Prefer combining dependent steps with && into a single call (e.g. `mkdir -p foo && touch foo/bar.txt && ls foo`) rather than splitting across turns.
```

工具 description 给"怎么做",系统提示给"为什么和什么时候",两者呼应。

## 不做的事(YAGNI 边界)

- **不改 `tool_choice`**:不强制 `parallel_tool_calls`,让模型自己判断
- **不加状态检测**:不检测"连续单工具"模式做动态干预(方案 C 的思路)
- **不加新工具**:不引入 `batch` / `plan` 工具(方案 B 的思路)
- **不改 agent loop**:`join_all` 已经支持并行,无需动
- **不加测试验证"模型是否真的多返回了"**:这是模型行为,不是代码行为,
  无法用单元测试稳定覆盖。已有的 `agent_executes_parallel_tools_in_single_turn`
  测试(`agent.rs:817-877`)足以保证执行层的正确性

## 验证方式

- `cargo build` 确认编译通过
- `cargo test` 确认现有测试不受影响(特别是 `agent_executes_parallel_tools_in_single_turn`)
- 手动跑一次 TUI,给一个需要多步的任务(如"创建 src/utils/mod.rs 并写个空函数"),
  观察模型是否:
  - 独立操作时一次返回多个 tool call
  - 依赖操作时用 `&&` 合并
