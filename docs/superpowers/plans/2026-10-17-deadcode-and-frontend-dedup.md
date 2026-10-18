# 死代码清理与前端重复消除 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删掉生产死代码 `SupervisorTools`、收敛 `spawn_agent` schema、修正命令目录漂移、消除前端四处重复。

**Architecture:** Rust 侧删死代码并把其测试迁到生产工具；`spawn_agent` 的描述/schema 提为 `yi-agent-subagent` 的单一函数；`/agents` usage 修正、`slash.rs` 死臂删除。前端侧：宽度模块工厂化、`KANBAN_PLUGIN` 去重、`RpcCall` 别名统一、新增 `lib/routes.ts`。

**Tech Stack:** Rust（`yi-agent-core` / `yi-agent-subagent` / `yi-agent`）；前端 TS（React + vitest）。

**Spec:** `docs/superpowers/specs/2026-10-17-deadcode-and-frontend-dedup-design.md`

## Global Constraints

- 行号以基线 `8e220ef2` 为准；改动前 `grep -n` 复核锚点。
- 环境（Rust）：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`，cargo 加 `--offline`。
- 环境（前端）：`export PATH="/opt/homebrew/bin:$PATH"`；用 `desktop/node_modules/.bin/tsc`、`desktop/node_modules/.bin/vitest`。
- **不改任何 RPC 方法名字符串**（`routes.ts` 只集中命名，值不变）。
- 只显式 `git add <path>`。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs` — 删 `SupervisorTools` 块。
- `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs` — 清理/迁移相关用例。
- `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` — 提 schema 为单一函数；两个 `Daemon*` 合并。
- `yi-agent-rs/crates/yi-agent/src/control_commands.rs` — 修 `/agents` usage。
- `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` — 删死臂。
- `desktop/src/lib/persistedWidth.ts` / `panelWidth.ts` / `sidebarWidth.ts` — 工厂化。
- `desktop/src/lib/rpcCall.ts` / `routes.ts` — 新增。
- `desktop/src/lib/pluginSettings.ts` / `superpowersKanbanSwitch.ts` — 常量去重。

---

### Task 1: 删除 `SupervisorTools` 死代码并处理其测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs:1975-2346`
- Modify: `yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs`

**Interfaces:**
- Produces: `core::subagent::supervisor` 不再导出 `SupervisorTools` / `SpawnAgentTool` / `WaitAgentTool` / `SendMessageTool`。

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-core --offline 2>&1 | grep -E "test result" | tail -5
grep -c "SupervisorTools" crates/yi-agent-core/tests/subagent_supervisor.rs
grep -c "fn " crates/yi-agent-core/tests/subagent_supervisor.rs
```

Expected: 测试全绿；文件内 `SupervisorTools` 出现 14 次（1 处 import + 13 处使用）、
共 57 个 `fn`。

- [ ] **Step 2: 删除该块**

`SupervisorTools` 文档注释起于 1975（`/// The per-task built-in tool set…`），块结束于
2346（`SendMessageTool` 的 `impl` 收尾 `}`）；其后 2348 起是 `#[cfg(test)] mod tests`。
删除 **1975-2346 全部内容**，使上一个 item 直接接到 `#[cfg(test)]`。

```bash
cd yi-agent-rs
python3 - <<'PY'
p='crates/yi-agent-core/src/subagent/supervisor.rs'
lines=open(p).read().splitlines(keepends=True)
assert 'per-task built-in tool set' in lines[1975-1], lines[1975-1]
del lines[1975-1:2346]      # 1-based 1975..=2346
open(p,'w').write(''.join(lines))
PY
sed -n '1970,1980p' crates/yi-agent-core/src/subagent/supervisor.rs
```

Expected: 打印出 `TaskState::…` 的 match 收尾后**紧跟** `#[cfg(test)]`，中间无空档。

> 已实测：`grep -rn "SupervisorTools" crates/yi-agent-core/src/` 在删除前只有
> `subagent/supervisor.rs` 命中，块外零引用，删除不会波及 core 其它源码。

- [ ] **Step 3: 处理受影响的 12 个测试**

实测删除后，`crates/yi-agent-core/tests/subagent_supervisor.rs` 里有 **12 个测试函数**
引用 `SupervisorTools`（编译在 3 个错误处早停，实际面更大——以本清单为准）：

```
:160   wait_agent_exposes_a_delivered_childs_commit
:377   spawn_tool_parses_optional_mode_and_rejects_invalid_values
:884   built_in_spawn_tool_returns_immediately_and_rejects_leaf_spawn
:909   send_message_delivers_to_a_direct_child_mailbox
:953   send_message_wakes_the_recipient_worker_inbox
:986   wait_all_returns_only_after_all_direct_children_are_terminal
:1018  wait_any_returns_after_the_first_terminal_child
:1054  wait_any_returns_only_terminal_child_reports
:1080  completed_worker_report_wakes_wait_agent
:1115  wait_agent_returns_completed_child_reports
:1139  wait_is_interrupted_by_a_permission_request
:1168  supervisor_toolset_registers_all_three_builtin_schemas
```

做法：

1. 删掉第 10 行 import 里的 `SupervisorTools`。
2. 删除上列 12 个用例中**纯测 `SupervisorTools` 包装层**的那些——它们验证的行为
   （wait_agent 交付 commit、send_message 唤醒 inbox、spawn 校验 mode 等）在
   `yi-agent-subagent` 已有等价覆盖。删前用
   `grep -n "send_message\|wait_agent\|spawn_agent" crates/yi-agent-subagent/src/lib.rs`
   核对无覆盖缺口；有缺口则把该用例改为驱动 `yi-agent-subagent` 的工具。
3. `:1168 supervisor_toolset_registers_all_three_builtin_schemas` 断言三件套 schema，
   **搬到 `yi-agent-subagent`** 改成断言 `Daemon*` 工具的 schema（Task 2 会建单一来源，
   正好复用）。

- [ ] **Step 4: 编译并跑测试**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-core --offline 2>&1 | grep -E "test result|^error" | tail -8
```

Expected: 全绿（用例数比基线少约 11-12 个）。

- [ ] **Step 5: 确认无残留 + commit**

```bash
cd yi-agent-rs && grep -rn "SupervisorTools" crates/ || echo "CLEAN"
cd .. && git add yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs yi-agent-rs/crates/yi-agent-core/tests/subagent_supervisor.rs
git commit -m "refactor(core): drop the production-dead SupervisorTools block"
```

---

### Task 2: 收敛 `spawn_agent` schema 到单一来源

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`DaemonSpawnAgentTool` 与 `DaemonApplicationSpawnAgentTool` 的 impl）

**Interfaces:**
- Produces: `pub(crate) fn spawn_agent_description() -> &'static str`、
  `pub(crate) fn spawn_agent_schema() -> serde_json::Value`。

- [ ] **Step 1: 确认两份完全同形**

```bash
cd yi-agent-rs
grep -n "impl Tool for DaemonSpawnAgentTool\|impl Tool for DaemonApplicationSpawnAgentTool" crates/yi-agent-subagent/src/lib.rs
```

Expected: 两处 impl。**已实测** `name()`、`description()`、`schema()` 三段逐字相同：

```rust
fn name(&self) -> &str { "spawn_agent" }
fn description(&self) -> &str { "Create an asynchronously scheduled direct child agent." }
```

`call()` 前 20 行也相同，**唯一差异**是：

```rust
// DaemonSpawnAgentTool：
let sandbox = match spawn_sandbox(&args, self.sandbox) { … };
// DaemonApplicationSpawnAgentTool：
let sandbox = match spawn_sandbox(&args, self.controller.effective()) { … };
```

- [ ] **Step 2: 写测试（钉住契约 + 断言两 impl 同源）**

在 `crates/yi-agent-subagent/src/lib.rs` 的测试模块加：

```rust
#[test]
fn spawn_agent_schema_pins_the_wire_contract() {
    let schema = spawn_agent_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"], serde_json::json!(["task"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["properties"]["mode"]["enum"],
        serde_json::json!(["coding", "read_only"])
    );
    assert_eq!(
        spawn_agent_description(),
        "Create an asynchronously scheduled direct child agent."
    );
}
```

- [ ] **Step 3: 运行确认失败**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-subagent --offline spawn_agent_schema_pins 2>&1 | tail -8`

Expected: 编译失败（`spawn_agent_schema` 未定义）。

- [ ] **Step 4: 提出单一函数并让两个 impl 共用**

在 `crates/yi-agent-subagent/src/lib.rs` 顶层（两个 impl 之前）加：

```rust
/// `spawn_agent` 面向模型的文案。
///
/// 普通 worker（`DaemonSpawnAgentTool`）与应用级子代理
/// （`DaemonApplicationSpawnAgentTool`）共用这一份，避免文案漂移。
pub(crate) fn spawn_agent_description() -> &'static str {
    "Create an asynchronously scheduled direct child agent."
}

/// `spawn_agent` 的 JSON schema。
///
/// 两个 `Daemon*SpawnAgentTool` 共用；两工具的差异只在 `call()` 里沙箱来源
/// （`self.sandbox` vs `self.controller.effective()`），与 schema 无关。
pub(crate) fn spawn_agent_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "task": { "type": "string", "description": "Delegated objective." },
            "mode": {
                "type": "string",
                "enum": ["coding", "read_only"],
                "description": "Use 'coding' only when the child must change files. Defaults to 'read_only'."
            },
            "workdir": {
                "type": "string",
                "description": "Directory the child works in. Required for 'coding': create it yourself with `git worktree add <path> -b <branch>` first."
            },
            "fork": {
                "type": "boolean",
                "default": false,
                "description": "Fork the caller's current conversation into the child so it inherits context. Off by default: pass true only when the child genuinely needs the history."
            }
        },
        "required": ["task"],
        "additionalProperties": false
    })
}
```

两个 impl 改为：

```rust
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        spawn_agent_description()
    }

    fn schema(&self) -> Value {
        spawn_agent_schema()
    }
```

> 已实测两份 `schema()` 的 JSON 与 `description()` 文本逐字相同，直接搬用即可。
> 单源化由编译器保证（两处都调用同一个 `fn`），`call()` 的沙箱差异保持不动。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-subagent --offline 2>&1 | grep -E "test result|^error" | tail -5`

Expected: 全绿。

- [ ] **Step 6: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-subagent && cd ..
git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "refactor(subagent): single source for the spawn_agent description and schema"
```

---

### Task 3: 修正命令目录漂移并删死臂

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/control_commands.rs:72`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`

> **不含 `main.rs` / `tui/app.rs`**：`ControlCommand` 改名（`DriverControl`）连带
> `app.rs` 的 101 处引用，而归"后端结构收敛"卡负责（它要拆 `handle_key`）。
> 本卡与那张卡的**文件集零重叠**，故可并行、合并无冲突。

- [ ] **Step 1: 修 `/agents` usage**

在 `control_commands.rs` 的 `Self::Agents => spec(` 调用里，把第 3 个实参
（usage 字符串）由 `"[--project PATH]"` 改为 `"[--all|--active]"`——与
`app.rs:3303-3305` 的真实解析一致：

```rust
            Self::Agents => spec(
                self,
                "agents",
                "[--all|--active]",
                "显示 agent 任务树",
```

- [ ] **Step 2: 加一致性测试**

在 `crates/yi-agent/src/control_commands_tests.rs` 加（该文件的既有约定是
`use super::control_commands::ControlCommand;`，测试写在 crate 根模块里）：

```rust
#[test]
fn agents_usage_matches_the_tui_parser() {
    assert_eq!(ControlCommand::Agents.spec().usage, "[--all|--active]");
}
```

> `spec()` 返回的 `CommandSpec.usage` 就是上面那串（`spec()` 内部把 usage 原样搬入），
> 已实测该断言成立。

- [ ] **Step 3: 删 `slash.rs` 的三处死臂**

`control_spec()` 只对 **18** 个变体返回 `Some`（`Agents`…`Mcp`）；`name()` /
`description()` / `argument_usage()` 三个函数都在开头提前 `return`，其后 `match` 里
对应这 18 个变体的臂**永不可达**。改法（已实测编译 + 635 测试全绿）：

**`name()`** 只保留 10 个非控制变体 + `unreachable!` 兜底：

```rust
        match self {
            SlashCommand::Quit => "quit",
            SlashCommand::Clear => "clear",
            SlashCommand::Model => "model",
            SlashCommand::Effort => "effort",
            SlashCommand::Cost => "cost",
            SlashCommand::Compact => "compact",
            SlashCommand::Config => "config",
            SlashCommand::Runtime => "runtime",
            SlashCommand::Kanban => "superpowers-kanban",
            SlashCommand::Plugins => "plugins",
            // 其余变体都由上面的 `control_spec()` 提前返回。
            _ => unreachable!("control commands return their name from the catalog"),
        }
```

**`description()`** 同法（保留同样 10 个，`_ => unreachable!(...)`）。

**`argument_usage()`** 删掉 13 条控制命令臂（`Agents`/`Agent`/`Events`/`Diff`/
`Mailbox`/`Message`/`Cancel`/`Pause`/`Resume`/`Retry`/`Priority`/`Approve`/`Deny`/
`Review`/`Budget`/`Daemon`/`Mcp`），保留 `Model`/`Effort`/`Runtime`/`Kanban`/`Plugins`
与兜底 `_ => None`：

```rust
        match self {
            // `default` 是保留字（清空会话覆盖、回落清单 default_model），因此
            // 清单里同名的条目永远无法通过 `/model` 选中（见
            // `handle_model_command` 与未知名提示文案）。
            SlashCommand::Model => Some("[<name>|default]"),
            // `default` 同 `/model` 是保留字（清空本会话覆盖、回落条目默认）。
            SlashCommand::Effort => Some("[none|minimal|low|medium|high|xhigh|max|default]"),
            SlashCommand::Runtime => Some("[ask|always|never]"),
            SlashCommand::Kanban => Some("[on|off|create|remove|status|run|add <spec> <plan>]"),
            SlashCommand::Plugins => Some("[<name> [set <key> <value>]]"),
            // 其余变体的 usage 由 `control_spec()` 提供；没有 usage 的返回 `None`。
            _ => None,
        }
```

Run（确认删干净）: `cd yi-agent-rs && grep -c "SlashCommand::Agents" crates/yi-agent/src/tui/slash.rs`

Expected: `1`（只剩 `control_spec()` 里的映射）。

- [ ] **Step 4: 编译 + 测试**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent --offline 2>&1 | grep -E "test result|error" | tail -8
```

Expected: 全绿。

- [ ] **Step 5: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent && cd ..
git add yi-agent-rs/crates/yi-agent/src/control_commands.rs yi-agent-rs/crates/yi-agent/src/tui/slash.rs
git commit -m "fix(tui): correct the /agents usage and drop stray command arms"
```

---

### Task 4: 前端四处重复消除

**Files:**
- Create: `desktop/src/lib/persistedWidth.ts`, `desktop/src/lib/rpcCall.ts`, `desktop/src/lib/routes.ts`
- Modify: `desktop/src/lib/panelWidth.ts`, `desktop/src/lib/sidebarWidth.ts`, `desktop/src/lib/pluginSettings.ts`, `desktop/src/lib/superpowersKanbanSwitch.ts`

> **本卡不改 `App.tsx`**（它由前端架构卡处理），以避免两张卡并发修改同一文件。

**Interfaces:**
- Produces: `createPersistedWidth(spec)`, `type RpcCall`, `const RPC = {…}`。

- [ ] **Step 1: 记录基线**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -5
```

Expected: `tsc` 无输出；`vitest` 89 文件/1113 测试全绿。

- [ ] **Step 2: 写 `persistedWidth` 的失败测试**

新建 `desktop/src/lib/persistedWidth.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import { createPersistedWidth } from "./persistedWidth";

describe("createPersistedWidth", () => {
  it("clamps into the configured range", () => {
    const w = createPersistedWidth({ storageKey: "t", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.clamp(100)).toBe(200);
    expect(w.clamp(500)).toBe(400);
    expect(w.clamp(350)).toBe(350);
  });

  it("falls back to default on a non-finite value", () => {
    const w = createPersistedWidth({ storageKey: "t", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.clamp(Number.NaN)).toBe(300);
  });

  it("loads the default when storage is empty", () => {
    const w = createPersistedWidth({ storageKey: "missing-key", defaultPx: 300, minPx: 200, maxPx: 400 });
    expect(w.load()).toBe(300);
  });

  it("round-trips through save/load", () => {
    const w = createPersistedWidth({ storageKey: "rt", defaultPx: 300, minPx: 200, maxPx: 400 });
    w.save(333);
    expect(w.load()).toBe(333);
    localStorage.removeItem("rt");
  });
});
```

- [ ] **Step 3: 运行确认失败**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/lib/persistedWidth.test.ts 2>&1 | tail -8`

Expected: 失败（`persistedWidth` 模块不存在）。

- [ ] **Step 4: 实现 `persistedWidth.ts`**

```ts
/** 一条持久化的像素宽度（右栏 / 侧栏各一条），存 localStorage。 */
export interface PersistedWidthSpec {
  storageKey: string;
  defaultPx: number;
  minPx: number;
  maxPx: number;
}

export interface PersistedWidth {
  clamp(px: number): number;
  load(): number;
  save(px: number): void;
}

export function createPersistedWidth(spec: PersistedWidthSpec): PersistedWidth {
  const clamp = (px: number): number => {
    if (!Number.isFinite(px)) return spec.defaultPx;
    return Math.min(spec.maxPx, Math.max(spec.minPx, px));
  };
  return {
    clamp,
    load(): number {
      try {
        const raw = localStorage.getItem(spec.storageKey);
        if (raw === null) return spec.defaultPx;
        return clamp(parseFloat(raw));
      } catch {
        return spec.defaultPx;
      }
    },
    save(px: number): void {
      try {
        localStorage.setItem(spec.storageKey, String(clamp(px)));
      } catch {
        // 存储不可用（隐私模式等）时静默降级：宽度只是偏好。
      }
    },
  };
}
```

> `panelWidth.ts`/`sidebarWidth.ts` 原有的 try/catch 边界以两者现状为准（`load` 与
> `save` 各自是否有 try/catch），实现须与之等价，避免行为变化。

- [ ] **Step 5: 让两个宽度模块改用工厂（保留原导出名）**

`desktop/src/lib/panelWidth.ts` 改为：

```ts
import { createPersistedWidth } from "./persistedWidth";

/** Right rail (会话详情) width persisted to localStorage, in CSS pixels. */
export const DEFAULT_PANEL_WIDTH = 360;
export const MIN_PANEL_WIDTH = 260;
export const MAX_PANEL_WIDTH = 720;
export const PANEL_WIDTH_STORAGE_KEY = "yi-agent.panelWidth";

const width = createPersistedWidth({
  storageKey: PANEL_WIDTH_STORAGE_KEY,
  defaultPx: DEFAULT_PANEL_WIDTH,
  minPx: MIN_PANEL_WIDTH,
  maxPx: MAX_PANEL_WIDTH,
});

export const clampPanelWidth = width.clamp;
export const loadPanelWidth = width.load;
export const savePanelWidth = width.save;
```

`sidebarWidth.ts` 同法（`DEFAULT_SIDEBAR_WIDTH = 256`、`MIN = 200`、`MAX = 480`、
key `"yi-agent.sidebarWidth"`）。

- [ ] **Step 6: 跑测试与类型检查**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -4
node_modules/.bin/tsc --noEmit 2>&1 | tail -5
```

Expected: 全绿（**只有 `sidebarWidth.test.ts` 存在**——`panelWidth.test.ts` 基线就不存在，
宽度行为由新增的 `persistedWidth.test.ts` 覆盖）。已实测此改法：`tsc` 无输出、
`vitest` 89 文件 / 1113 测试全绿。

- [ ] **Step 7: 前端去重其余三项**

1. `KANBAN_PLUGIN`：删 `superpowersKanbanSwitch.ts:42` 的定义，改为
   `import { KANBAN_PLUGIN } from "./pluginSettings";`（若该文件曾导出它，补
   `export { KANBAN_PLUGIN };`）。
2. `lib/rpcCall.ts`：

   ```ts
   /** 插件/业务通道共用的 RPC 调用签名。 */
   export type RpcCall = <T = unknown>(method: string, params: unknown) => Promise<T>;
   ```

   把 `superpowersKanbanSwitch.ts` / `pluginSettings.ts` / `superpowersKanbanBoards.ts` /
   `onboarding.ts` / `models.ts` 里的别名改为 `export type BoardRpc = RpcCall;` 等。
3. `lib/routes.ts`：按
   `grep -rhoE '"[a-z]+/[A-Za-z]+"' desktop/src/lib | sort -u`
   结果建常量表，并把**`desktop/src/lib/` 下各 rpc 模块**里的路由字面量改为 `RPC.xxx`。
   **不要改 `App.tsx`**——它由前端架构卡处理，避免并发冲突。

- [ ] **Step 8: 全量验证 + commit**

```bash
cd desktop
export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -4
cd ..
git add desktop/src/lib/persistedWidth.ts desktop/src/lib/persistedWidth.test.ts desktop/src/lib/panelWidth.ts desktop/src/lib/sidebarWidth.ts desktop/src/lib/rpcCall.ts desktop/src/lib/routes.ts desktop/src/lib/pluginSettings.ts desktop/src/lib/superpowersKanbanSwitch.ts
git commit -m "refactor(desktop): dedupe width modules, KANBAN_PLUGIN, RPC aliases and add a routes table"
```

Expected: `tsc` 无输出；`vitest` 全绿。
