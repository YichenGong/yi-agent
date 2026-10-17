# 设计：死代码清理与前端重复消除

日期：2026-10-17
状态：待评审
关系：本文来自 2026-10-17 的仓库冗余审计（P1-2 与 P2-2/P2-3/P2-4 合并）。
审计结论：`SupervisorTools` 是生产死代码、`spawn_agent` schema 重复三份、命令目录
已漂移、前端有五处可去重的重复。

## 1. 背景与问题

### 1.1 `SupervisorTools` 是生产死代码（Rust）

`yi-agent-rs/crates/yi-agent-core/src/subagent/supervisor.rs:1978-2346`（约 370 行）
定义了 `SupervisorTools` / `SpawnAgentTool` / `WaitAgentTool` / `SendMessageTool`。
grep 全仓引用**只出现在 core 自己的测试**：

```
crates/yi-agent-core/tests/subagent_supervisor.rs  (import + 16 处使用)
crates/yi-agent-core/src/subagent/supervisor.rs    (定义本身)
```

生产路径走的是 `yi-agent-subagent` 的 `Daemon*` 工具。即这 370 行是**只为测试存在
的公开 API**，且它正是 §1.2 重复的一份。

### 1.2 `spawn_agent` schema 三重复（Rust）

同一份 `name`/`description`/JSON schema 出现三处：

| 位置 | 角色 |
|---|---|
| `core/src/subagent/supervisor.rs:2029` | `SpawnAgentTool`（生产死代码，见 §1.1） |
| `subagent/src/lib.rs:1855` | `DaemonSpawnAgentTool`（生产） |
| `subagent/src/lib.rs:2151` | `DaemonApplicationSpawnAgentTool`（生产） |

后两者的实现体约 100 行、仅 4 行不同（沙箱来源、`thread_id`/`capability` 的有无）。
删掉 §1.1 后仍剩两份，需合并。

### 1.3 命令目录已漂移（Rust）

`yi-agent/src/control_commands.rs` 是命令元数据的单一真相源，`tui/slash.rs` 的
`name()`/`description()`/`argument_usage()` 都通过 `control_spec()` 提前返回目录值——
因此这三个函数里对应 18 个控制命令变体（`Agents`…`Mcp`）的 match 臂**不可达**，
而它们已经和目录**不一致**：

- 目录 `control_commands.rs:72`：`/agents` 的 usage 是 `(agents) [--project PATH]`
- 死分支 `slash.rs:168`：`[--all|--active]`
- 真实 handler（`app.rs:3305`）接受 `--all|--active`，且 `app.rs:3292` 告诉用户
  `/agents --all`

即**线上目录写错了**，而旁边还躺着两个已经漂移的影子副本。

另外 `yi-agent/src/main.rs:2509` 与 `control_commands.rs:7` 各有一个
`enum ControlCommand`——**同 crate 同名不同义**（前者是 TUI→driver 的重建指令，后者
是 daemon 控制目录）。

### 1.4 前端五处重复

| 项 | 证据 |
|---|---|
| `lib/panelWidth.ts` ≡ `lib/sidebarWidth.ts` | 各 32 行，规范化后仅 6 行不同（全是常量值）；`clamp/load/save` 结构逐字相同 |
| `KANBAN_PLUGIN` 定义两次 | `lib/pluginSettings.ts:5` 与 `lib/superpowersKanbanSwitch.ts:42` 都是 `export const KANBAN_PLUGIN = "superpowers-kanban"` |
| RPC 调用别名重复 5 份 | `BoardRpc`(×2)、`PluginRpc`、`OnboardingRpc`、`ModelRpc`，全是 `<T = unknown>(method, params) => Promise<T>` |
| 无路由常量表 | 135 个路由字符串字面量散落（52 个在 `App.tsx`），打错编译期无感 |
| 前端"无报告"语义 | （属 P3，不在本卡） |

## 2. 目标与非目标

### 目标

1. 删除 `SupervisorTools` 死代码块，并把其测试迁到生产工具上（保留覆盖）。
2. 合并 `subagent/src/lib.rs` 里两个近乎相同的 `Daemon*SpawnAgentTool`。
3. 修正 `/agents` 目录 usage、删 `slash.rs` 三个函数里的死 `match` 臂、重命名重复的
   `ControlCommand`。
4. 前端：`panelWidth`/`sidebarWidth` 合一、`KANBAN_PLUGIN` 去重、RPC 别名统一、
   新增 `lib/routes.ts` 路由常量表。

### 非目标

- 不动 `ye-agent` 的 provider/agent 逻辑。
- 不为前端补设计系统原语（属 P3）。
- 不改任何 RPC 方法名字符串（`routes.ts` 只做"集中命名"，值不变）。

## 3. 设计

### 3.1 Rust：删死代码 + 合并 schema

- **`SupervisorTools`**：整块删除（`supervisor.rs:1978-2346` 之间的
  `SupervisorTools`、`SpawnAgentTool`、`WaitAgentTool`、`SendMessageTool`）。其
  `#[cfg(test)] mod tests` 里针对这三件套的用例，改为驱动 `yi-agent-subagent` 的
  `Daemon*` 工具（该 crate 已有等价测试，如 `daemon_worker_reports_text_completion_*`，
  故多数用例可直接删；仅"工具 schema 文案正确"一类值得保留，迁到 `subagent` crate）。
- **schema 单一来源**：在 `yi-agent-subagent` 里提出

  ```rust
  pub(crate) fn spawn_agent_description() -> &'static str { "…" }
  pub(crate) fn spawn_agent_schema() -> serde_json::Value { json!({ … }) }
  ```

  两个 `Daemon*SpawnAgentTool` 共用；两者差异（沙箱来源、`thread_id`/`capability`）
  留在各自结构体字段上，实现体收敛为一个带参数的辅助函数。

### 3.2 Rust：命令目录收敛

- 修 `control_commands.rs:72` 的 `/agents` usage 为 `(agents) [--all|--active]`
  （与真实 handler 一致）。
- 删 `slash.rs` 里 `name()`/`description()` 中**不可达**的 18×2 个 match 臂
  （每个 `control_spec()` 返回 `Some` 的变体各一条）；保留 `Quit/Clear/Model/Effort/
  Cost/Compact/Config/Runtime/Kanban/Plugins` 等**非控制命令**的臂。
- `main.rs:2509` 的 `enum ControlCommand` 改名为 `DriverControl`（并同步其引用点），
  消除同 crate 同名。

### 3.3 前端：四处收敛

1. **宽度模块合一**：新建 `lib/persistedWidth.ts`：

   ```ts
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
   export function createPersistedWidth(spec: PersistedWidthSpec): PersistedWidth;
   ```

   `panelWidth.ts` / `sidebarWidth.ts` 改为用各自的 spec 调它并**保留原有导出名**
   （`DEFAULT_PANEL_WIDTH` 等），使调用点零改动。

2. **`KANBAN_PLUGIN` 单一来源**：保留 `lib/pluginSettings.ts:5` 的定义，
   `superpowersKanbanSwitch.ts` 改为 `import { KANBAN_PLUGIN } from "./pluginSettings"`
   并 `export { KANBAN_PLUGIN }`（若原导出被外部引用）。

3. **RPC 别名统一**：新建 `lib/rpcCall.ts`：

   ```ts
   /** 所有插件/业务通道共用的 RPC 调用签名。 */
   export type RpcCall = <T = unknown>(method: string, params: unknown) => Promise<T>;
   ```

   五个模块改为 `export type BoardRpc = RpcCall;` 之类（保留别名以免改调用点），
   或直接替换为 `RpcCall`。

4. **路由常量表**：新建 `lib/routes.ts`，把出现过的 RPC 方法名集中为常量：

   ```ts
   /** app-server JSON-RPC 方法名。值即线上字符串，勿改。 */
   export const RPC = {
     threadStart: "thread/start",
     pluginQuery: "plugin/query",
     // …按 grep 结果补齐
   } as const;
   ```

   调用点逐步改用 `RPC.threadStart` —— **本卡只建立表并替换五个 rpc 模块内的字面量**。
   `App.tsx` 的 52 处字面量替换**留到前端架构卡**（那里本就要大改 `App.tsx`），
   避免两张卡并发修改同一文件。

## 4. 测试策略

- **Rust**：删代码后 `cargo test --workspace` 必须全绿；为 schema 单一来源加一条
  断言（两个 `Daemon*` spawn 工具的 `name()`/schema 相等）。
- **目录一致性**：加一条测试，断言 `control_commands` 里每个 `CommandSpec.usage` 与
  `/agents` 的实际参数集不矛盾（至少对 `/agents` 钉死 `--all|--active`）。
- **前端**：`panelWidth`/`sidebarWidth` 各自既有测试保留并通过；新增
  `persistedWidth.test.ts` 覆盖 clamp/load/save 边界；`tsc --noEmit` 与 `vitest run`
  全绿（基线 89 文件/1113 测试）。
- **路由表**：断言 `RPC` 的每个值与原字面量一致（防止改名）。

## 5. 验收标准

- `grep -rn "SupervisorTools" crates/` 无命中。
- `grep -rn "fn text_completion_report"`（与 P0-B 无关）无关；此处验收：
  `grep -c "\"spawn_agent\"" crates/yi-agent-subagent/src/lib.rs` 的 schema 定义收敛为 1 处。
- `grep -rn "enum ControlCommand" crates/yi-agent/src/` 只剩 `control_commands.rs` 一处。
- 前端 `panelWidth.ts` + `sidebarWidth.ts` 的总行数显著下降且测试全绿。
- `cargo clippy --workspace --all-targets -- -D warnings` 与前端 `tsc --noEmit` 均无警告。
