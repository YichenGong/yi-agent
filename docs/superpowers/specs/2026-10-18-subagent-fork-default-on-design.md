# Subagent 默认 fork 调用者上下文 设计

**目标：** 把 `spawn_agent` 的 `fork` 从「显式可选、默认关闭」翻转为
**「默认开启、可用 `fork: false` 关闭」**，并让模型通过工具 schema 与系统提示词
两处都明白这条规则。这样委派出去的子 agent 默认继承调用者的对话历史，不再从零
重新探索，直接改善「子 agent 慢」的体感。

**状态：** 设计已确认，待转实现计划。

**背景（本次实测）：** 子 agent 与 root 用的是同一份系统提示词、同一个模型
（`~/.yi-agent/models.json` 的 `subagent_model` = `default_model`），且 `fork` 默认
关闭，所以子 agent 每次都要重新读文件、重新发现父 agent 已经知道的事。实测编排层
本身很快（`created_at → task_started` 为 0–1s，见 `~/.yi-agent/runtime` 的
`tasks`/`events` 表），慢的是子 agent 自身要重建上下文。相关文档：
`docs/superpowers/specs/2026-10-02-subagent-context-fork-design.md`（原始 fork 设计，
决策 #2/#5 当时定为「显式可选」）。

---

## 1. 语义不变式

| 调用 | 行为 |
|---|---|
| 省略 `fork` | fork（新默认） |
| `fork: true` | fork（与今天一致） |
| `fork: false` | 不 fork；今天的老路径逐字节不变 |
| 默认路径且 fork 不可行 | **warn + 落回无 fork**，spawn 照常成功 |
| 显式 `fork: true` 且 fork 不可行 | 保持今天的硬失败（明确报错） |

**关键区别：** 硬失败语义只对**显式** `fork: true` 保留。「默认来的 fork」不可行时
必须降级——默认值应该让正常路径永远成功，否则父 agent 只是正常派活却可能整单失败，
比「派出去但没带上下文」更糟。这一点是本次改动的核心。

触发方式与原始设计一致：`spawn_agent` 的可选布尔参数 `fork`，`required` 仍只有
`task`，`additionalProperties: false` 不变。

---

## 2. 要改的位置

### 2.1 默认值决策点（唯一的控制流变化）

`yi-agent-subagent/src/lib.rs` 的 `spawn_fork()` 当前为：

```rust
fn spawn_fork(args: &Value) -> Result<bool, ToolResult> {
    match args.get("fork") {
        None => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(ToolResult::error("fork must be a boolean")),
    }
}
```

改为返回三态枚举，让调用方能区分「默认开启」与「显式要求」——降级只允许前者：

```rust
enum ForkChoice { Default, Yes, No }
```

- `None => Ok(ForkChoice::Default)`
- `Some(true) => Ok(ForkChoice::Yes)`
- `Some(false) => Ok(ForkChoice::No)`
- 其它 → 维持 `Err("fork must be a boolean")`（显式非布尔值仍报错，不忽略）

`spawn_fork_token()`（lib.rs:1803）据此决定：`No` 直接返回 `Ok(None)`；`Default`/`Yes`
都尝试 fork，但只有 `Yes` 在失败时把错误上抛，`Default` 改成 warn + `Ok(None)`。

### 2.2 降级点（两处硬失败）

**(a) caller 未绑定。** `upload_forked_context()`（lib.rs:1760）今天返回
`Err("fork requested but no caller context available")`。改后：`ForkChoice::Yes` 保留
该错误；`ForkChoice::Default` 返回 `Ok(None)` 并 `tracing::warn!`。

**(b) fork 载荷超上限。** 上限是 `DEFAULT_FORK_MAX_BYTES = 32 MiB`
（`yi-agent-store/src/runtime.rs:47`）。超限由 daemon 在 `begin_fork_upload`
（`runtime.rs:961` 的 `ForkTooLarge`）拒绝，它发生在**第一个 `BeginForkUpload`
请求**上——不是 spawn 请求。今天 `drive_fork_upload`（lib.rs:1713-1727）把该响应
转成 `Err`，于是 `spawn_fork_token` 报错、spawn 请求根本不会发出。

**默认路径的降级规则（统一一条）：** fork 尝试的任何失败，在 `ForkChoice::Default`
下都降级为「无 fork」，`ForkChoice::Yes` 下一律保持今天的报错。

这条规则刻意**不做错误分类**（不解析 `ForkTooLarge`、不区分拒绝与传输故障），
理由有二：

1. **不需要本地镜像 daemon 的上限。** 上限是 daemon 策略、可经
   `YI_AGENT_FORK_MAX_BYTES` 环境变量覆盖（`runtime.rs:49-50,536-539`），客户端
   硬编码一个 32 MiB 副本必然漂移。超限由 daemon 在第一个 `BeginForkUpload` 拒绝，
   且**拒绝发生在分配 token 之前**（`runtime.rs:955-965`），所以没有字节流动、没有
   token 泄漏、也无需 abort。
2. **不需要重发 spawn，也就不依赖 spawn 幂等性。** fork 上传发生在 spawn 请求
   **之前**（lib.rs:1927 取 token，1932 才发 spawn），降级只是把 `fork_token` 置为
   `None` 再发 spawn——spawn 请求**总共只发一次**。

「默认路径掩盖真实故障」的担忧不成立：降级后紧接着就是同一个 socket 上的 spawn
请求，daemon 若真的坏了，那次调用会立刻报 `daemon is unavailable`。

### 2.3 文案（A + B）

**A —— 工具 schema（`spawn_agent_schema()`，lib.rs:1873-1877）：**

```json
"fork": {
  "type": "boolean",
  "default": true,
  "description": "The child inherits this conversation's history by default. Pass false for an isolated child that should start clean."
}
```

`default` 从 `false` 改为 `true`，描述从「劝退」改为「这是默认行为，`false` 是逃生口」。

**B —— 系统提示词（`AgentConfig::default_system_prompt()` 的
"Subagent integration" 段，`yi-agent-core/src/agent.rs:217-225）：**

新增一条规则，例如：

```
- A delegated child inherits your current conversation by default, so it already
  knows what you have read and decided. Pass `fork: false` only when the child
  must start from a clean slate.
```

理由：模型能否想到传 `fork` 目前完全依赖它读工具参数说明的自觉（`AGENTS.md` 与
系统提示词都没有提 fork，已核实无命中）。把规则放进 system prompt，它每轮可见，
不再埋在工具签名里。

**注意：** `default_system_prompt()` 同时是 root 与 subagent 的提示词（原始设计
§1.3 已证实父子提示词字面相同），改一处两边都生效；两个 spawn 工具也共用唯一的
`spawn_agent_schema()`（lib.rs:1895 与 2170 都调它），同样改一处处处生效。

### 2.4 返回可观测性

`spawn_agent` 成功返回的 payload（lib.rs:1948、2220 两处 `TaskSpawned` 分支）新增
`"forked": true|false`：

```json
{ "task_id": "...", "objective": "...", "status": "queued", "forked": true }
```

理由：降级是静默的，父 agent 会误以为子 agent 知道前面的事。该字段让父 agent 知情，
也让降级可被测试断言。

---

## 3. 测试

**改既有断言（3 处，语义反转）：**

- `lib.rs:2333` `both_spawn_schemas_offer_fork_and_default_to_false` —— 断言
  `default == true`，函数改名（如 `..._default_to_true`）。
- `subagent_fork_e2e.rs:308` `an_unforked_spawn_sends_only_the_objective_to_the_child`
  —— 省略 fork 的语义反转成「继承父历史 + objective 居后」。
- `subagent_fork_e2e.rs:15-16` 文件头 doc（现在写「省略 fork 时只发 objective，
  证明默认仍是旧的 unforked 行为」）随之更新。

**保留不动：** `fork_false_matches_the_unforked_path`（:338）、
`an_unforked_worker_sends_only_its_objective_first`（lib.rs:3948，
测的是 worker 层 `fork_messages: None`，不经过参数默认）。

**新增测试（4 项）：**

1. 省略 fork → 子 worker 首个 provider 请求含父历史，且返回 `forked: true`。
2. `fork: false` → 只含 objective，返回 `forked: false`（既有测试已覆盖行为，
   补 `forked` 字段断言）。
3. caller 未绑定 + 省略 fork → spawn **成功**，返回 `forked: false`（降级，不报错）。
4. 载荷超限 / 传输故障 + 省略 fork → spawn **成功**，返回 `forked: false`（降级）。
   实现上不专门构造「超 1 MiB 载荷」的 daemon（其上限只能经进程级环境变量
   `YI_AGENT_FORK_MAX_BYTES` 设置，在并行测试中不安全），而是用一个「已绑定 caller +
   不可达 socket」的单元测试覆盖**同一类**失败——daemon 在第一个 `BeginForkUpload`
   拒绝超限载荷时，走的正是这条 `Err` 通道。

**验证命令：**

- `cargo test -p yi-agent-subagent --lib`
- `cargo test -p yi-agent-core --lib`
- `cargo test -p yi-agent --test subagent_fork_e2e`
- `cargo fmt --all`

---

## 4. 文档同步

`docs/project-management/yi-agent-subagent.md:65-80` 那条现在写的是
「`default:false`…无绑定 caller 却要 fork 时显式报错，绝不静默回退」，与新语义
直接冲突，必须改写为：默认 fork、`fork:false` 关闭、默认路径不可行时降级为无 fork
并返回 `forked:false`、显式 `fork:true` 仍硬失败。同一条内的代码行号引用
（`src/lib.rs:1667` 等）也要同步。

---

## 5. 不做的事（YAGNI）

- 不改成「永远 fork、删掉参数」：保留 `fork: false` 逃生口，干净室任务与省 token
  场景仍需它，且既有 e2e 测试继续有效。
- 不做「超限时截断部分历史」：引入「部分上下文」新语义，实现复杂且难验证。
- 不动 fork 的传输协议（分块 base64 上传）与 32 MiB 上限本身。
- 不改 worker 的 provider/model 选择（那是「子 agent 慢」的另一个独立原因，
  不在本次范围）。