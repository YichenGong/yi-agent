# 桌面端 `/usage` 命令（替换 `/cost`）

**日期：** 2026-10-21
**状态：** 设计已确认，待实现

## 1. 问题

桌面端 `/cost` 在多数情况下显示「暂无用量数据」，即使该会话确实跑过带用量的轮次。

根因（两条，都在取数路径上，不是数据缺失）：

1. 前端 `Session.usage` **只**由实时通知 `thread/tokenUsage/updated` 填充
   （`desktop/src/lib/session.ts:631`）。该通知只在 driver 消费本轮 stream 时发出
   （`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5154`）。
2. 冷会话的补数据走两条路，都读不到用量：
   - **运行中的会话**只走只读 `thread/readItems`（`desktop/src/App.tsx:684`），
     它的响应只有 `items`，把 `ThreadStore::load` 已经读出的 `loaded.usage` 丢掉了
     （`yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:2038`）。
   - `thread/resume` 会回放 `loaded.usage`（`rpc/thread.rs:645`），但「看一眼运行中
     的会话绝不 resume」这条既有约束让这条路径在常见场景下走不到。

数据本身是落盘的：`.yi-agent/threads/*.jsonl` 每轮带 `usage`
（实测 `input_tokens: 142330 / output_tokens: 115 / cache_read_input_tokens: 142208`）。

## 2. 目标

- 用户输入 `/usage`（**取代** `/cost`）时，只要该会话有历史用量，就必须显示真实数据；
  没有则明确说「本会话还没有用量」，而不是笼统的「暂无用量数据」。
- 内容：模型 + input / output / cache read / cache write + 估算成本 +
  **上下文占用**（`input / compact_threshold`，百分比）。

`/cost` 不是别名：目录里直接改名为 `usage`，帮助与弹窗只出现一条。

## 3. 后端改动（`yi-agent-app-server`）

`thread/readItems` 的响应加一个 `usage` 字段（`TurnUsage | null`，缺省即 null，
老客户端忽略）：

```json
{ "items": [...], "usage": { "model": "…", "input_tokens": 142330,
  "output_tokens": 115, "cache_creation_input_tokens": 0,
  "cache_read_input_tokens": 142208 } }
```

- 来源就是 `load(&thread_id)` 已经返回的 `loaded.usage`，不额外读盘。
- `usage` 是**该 thread 最后一轮**的用量快照（与 `thread/resume` 回放的是同一份），
  不是累计值——与既有语义一致（`ThreadStore::read_log` 只保留最后一条非空 usage）。
- 无用量时字段取 `null`。前端对「字段缺省」与「字段为 `null`」按同一判定处理
  （都视为无数据），不引入第三种状态——老服务端不带该字段时行为不变。

`compact_threshold` **不用新接口**：`config/read` 带 `threadId` 已经返回它
（`yi-agent-runtime/src/config.rs:466`），桌面端已有该调用路径（`/config`）。

## 4. 前端改动

### 4.1 把持久化用量折进会话

`Session` 增加一个只在「本地没有 live usage」时写入的入口：

```ts
/** 用服务端持久化的上一轮用量垫底；已有 live 值时不覆盖。 */
adoptPersistedUsage(u: Usage | null): void
```

规则：`usage !== null && this.usage === null` 才写。live 通知永远优先——它代表
「当前这一轮」，比上一轮新。

`App` 的两处 `thread/readItems` 调用（`App.tsx:642` 与 `App.tsx:684`）在
`upsertItems` 之后调用 `adoptPersistedUsage(toUsage(r.usage))`。`toUsage` 是
`desktop/src/lib/protocol.ts` 里的纯映射（snake_case → camelCase），放纯函数模块
以便单元测试。

`thread/resume` 路径不用改：服务端回放的 `thread/tokenUsage/updated` 已经走
`Session.apply`。

### 4.2 命令改名与执行

`desktop/src/lib/slash.ts`：`cost` → `usage`，描述改为「查看 token 用量与上下文占用」。
`App.tsx` 的分支 `case "cost"` 改为 `case "usage"`。

执行流程：

1. `const u = session.usage`。为空 → `notice("本会话还没有用量记录")`（比原来的
   「暂无用量数据」更明确：说明是「本会话没有」，不是「功能坏了」）。
2. 非空 → 并行取 `config/read`（带 `threadId`）拿 `compact_threshold`；
   失败**不阻断**输出，只是那一行显示「上下文上限：未知」。
3. 输出（notice，多行）：

```
模型: claude-sonnet-4
input: 12,345  output: 678
cache read: 10,000  cache write: 0
上下文: 12,345 / 160,000 (7.7%)
估算成本: $0.0421
```

- 千分位用前端本地格式化（`Intl.NumberFormat`），与状态栏的滚动数字不共用组件。
- 百分比 = `input / compact_threshold`，`compact_threshold` 缺失或为 0 时不显示该行
  的分母与百分比，只显示 `上下文: 12,345`。
- 成本用既有 `estimateCost` / `formatCost`（未知模型显示 `—`，不假装是 $0）。

## 5. 测试

- `desktop/src/lib/slash.test.ts`：命令表断言改为 `usage`；`filterCommands("c")`
  等前缀用例同步。
- `desktop/src/components/MessageInput.test.tsx`：把以 `/cost` 为样例的补全/执行用例
  改成 `/usage`（`/cos` 补全 → `compact`/`config` 两条，断言相应调整）。
- 新增 `desktop/src/lib/usageCommand.test.ts`（纯函数 `formatUsage`）：
  - 有 `compact_threshold` → 含百分比行；
  - 无阈值 → 只显示 token 数，不出现 `%`；
  - 未知模型 → 成本为 `—`；
  - cache 为 0 时照常显示 `0`。
- 新增 `desktop/src/lib/session.test.ts` 用例：`adoptPersistedUsage` 只在
  `usage === null` 时写入；已有 live 值时不覆盖。
- `desktop/src/App.test.tsx`：`/usage` 在无数据时输出「本会话还没有用量记录」。
- 后端 `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`：
  `read_items_returns_persisted_items_and_slices_after_id` 扩展断言——跑完一轮后
  `result.usage` 非 null 且 `input_tokens` 与落盘一致；未知 thread 的报错分支不变。

## 6. 不做

- 不做**跨轮累计**（TUI 的 `/cost` 是累计值，`tui/cost.rs`）。桌面端的用量来源是
  「本轮快照」，跨轮累计需要服务端新增按 thread 聚合的存储与接口，超出本次修复范围。
- 不新增 RPC。
- 不改状态栏的 `in / out` 显示。
