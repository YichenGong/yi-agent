# 桌面端 `/usage` 命令实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端 slash 命令 `/usage`（取代 `/cost`）在会话有历史用量时显示真实的 token 用量、上下文占用与估算成本。

**Architecture:** 后端 `thread/readItems` 响应补一个 `usage` 字段（来自已读出的 `loaded.usage`，不新增 RPC）；前端在 `Session` 上增加「只在本地没有 live 用量时垫底」的入口，并把命令输出抽成纯函数 `formatUsage`。上下文上限复用 `config/read`（带 `threadId`）已有的 `compact_threshold`。

**Tech Stack:** Rust（yi-agent-app-server）、TypeScript + React 19 + Vitest（desktop）。

**Spec:** `docs/superpowers/specs/2026-10-21-desktop-usage-command-design.md`

## Global Constraints

- 工作目录是本 worktree：`.worktrees/desktop-usage`（分支 `feat/desktop-usage-command`）。**不要**在 `main` 上改任何文件。
- 前端命令一律用 `cd desktop && npx vitest run <file>` 验证；类型检查 `npx tsc --noEmit`。
- Rust 侧只跑 `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib <name>`；**不要**跑 `--workspace`。
- 提交前 `cd yi-agent-rs && cargo fmt --all`（如改了 Rust）。
- Commit 用 conventional commits，**不写** `Co-Authored-By`。
- 所有面向用户的文案用中文；代码注释沿用文件既有语言（Rust 中文注释、TS 部分英文注释）。

---

### Task 1: 后端 `thread/readItems` 回带持久化用量

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（`"thread/readItems"` 臂，写响应在 `:2031` 附近）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs:16139`（`read_items_returns_persisted_items_and_slices_after_id`）

**Interfaces:**
- Consumes: `crate::thread_store::ThreadStore::load` 返回的 `LoadedThread { items, usage: Option<TurnUsage>, .. }`；`TurnUsage` 已实现 `Serialize`（`thread_store.rs:196`）。
- Produces: `thread/readItems` 响应 `{ "items": Item[], "usage": TurnUsage | null }`。`usage` 字段的 JSON 形状：`{"model": string, "input_tokens": number, "output_tokens": number, "cache_creation_input_tokens": number, "cache_read_input_tokens": number}`。前端在 Task 2 消费它。

- [ ] **Step 1: 扩展既有测试，断言 `usage` 非空且与落盘一致**

在 `read_items_returns_persisted_items_and_slices_after_id` 里，`assert!(!items.is_empty(), ...)` 之后追加：

```rust
    // 同一份 load 已经读出了最后一轮用量：不回传等于把已付出的读盘丢掉，
    // 前端在「运行中会话只走只读补齐」的路径上就永远显示「暂无用量数据」。
    let usage = v["result"]["usage"]
        .as_object()
        .unwrap_or_else(|| panic!("readItems 必须回传持久化用量: {v}"));
    assert!(
        usage["input_tokens"].as_u64().unwrap_or(0) > 0,
        "回传的用量应来自真实跑过的一轮: {v}"
    );
    assert!(usage["model"].as_str().is_some_and(|m| !m.is_empty()), "{v}");
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib read_items_returns_persisted_items_and_slices_after_id`
Expected: FAIL，panic 信息 `readItems 必须回传持久化用量`。

- [ ] **Step 3: 实现——把 `loaded.usage` 放进响应**

该臂里 `items` 的构造用了 `loaded.items`（会 move）。改为先取 usage 再消费 items，最后写响应：

把

```rust
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "items": items })),
            )
            .await?;
```

改为

```rust
            // `usage` = 该 thread **最后一轮**的用量快照，与 `thread/resume` 回放
            // 的是同一份（`ThreadStore::read_log` 只保留最后一条非空 usage）。
            // 回传它的理由：运行中的会话按既有约束**绝不 resume**，只走这里；
            // 不带用量，客户端在那条路径上就永远拿不到这一份本来已经读出来的数据。
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({ "items": items, "usage": loaded.usage }),
                ),
            )
            .await?;
```

注意：`loaded.items` 在此之前已被 `into_iter()` 消费，`loaded.usage` 是另一个字段，Rust 允许部分移动后读取未移动字段——但 `loaded` 是被 `match after` 消费的。因此需要在该臂开头先把 usage 取出：

```rust
            let loaded_usage = loaded.usage.clone();
```

放在 `let store = ...` / `load` 之后、`let items: Vec<Item> = match after { ... }` 之前，然后把响应写 `json!({ "items": items, "usage": loaded_usage })`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib read_items`
Expected: PASS（含 `read_items_does_not_interrupt_a_running_turn`）。

- [ ] **Step 5: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): return the persisted usage from thread/readItems"
```

---

### Task 2: 前端把持久化用量折进会话

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（`Usage` 附近，`:142`）
- Modify: `desktop/src/lib/session.ts`（`usage` 字段旁，`:85`；`notice` 方法旁）
- Modify: `desktop/src/App.tsx`（两处 `thread/readItems` 调用：`:642` 与 `:684`）
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: Task 1 的 `thread/readItems` 响应 `{ items, usage }`（snake_case）。
- Produces:
  - `wireUsageToUsage(u: WireUsage | null | undefined): Usage | null`（导出，纯函数）
  - `Session.adoptPersistedUsage(u: Usage | null): void`

- [ ] **Step 1: 写失败的测试**

在 `desktop/src/lib/session.test.ts` 末尾（`describe("Session", ...)` 内）追加：

```ts
  it("adopts persisted usage only when no live snapshot exists", () => {
    const s = new Session();
    s.adoptPersistedUsage({ model: "m", input: 10, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usage).toEqual({ model: "m", input: 10, output: 1, cacheRead: 0, cacheWrite: 0 });

    // 持久化值只是一轮的落盘快照；live 通知代表「当前这一轮」，绝不能反过来
    // 被上一轮的旧数据覆盖。
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m2",
        input_tokens: 99,
        output_tokens: 3,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
      },
    });
    s.adoptPersistedUsage({ model: "old", input: 1, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usage).toEqual({ model: "m2", input: 99, output: 3, cacheRead: 0, cacheWrite: 0 });
  });

  it("ignores a null persisted usage", () => {
    const s = new Session();
    s.adoptPersistedUsage(null);
    expect(s.usage).toBeNull();
  });
```

在 `desktop/src/lib/protocol.test.ts` 末尾追加（该文件已存在，只加 `describe` 块并补 `wireUsageToUsage` 的导入）：

```ts
import { describe, it, expect } from "vitest";
import { wireUsageToUsage } from "./protocol";

describe("wireUsageToUsage", () => {
  it("maps the snake_case wire snapshot to the client shape", () => {
    expect(
      wireUsageToUsage({
        model: "claude-sonnet-4",
        input_tokens: 10,
        output_tokens: 2,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 4,
      }),
    ).toEqual({ model: "claude-sonnet-4", input: 10, output: 2, cacheRead: 4, cacheWrite: 3 });
  });

  it("returns null for a missing snapshot", () => {
    expect(wireUsageToUsage(null)).toBeNull();
    expect(wireUsageToUsage(undefined)).toBeNull();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts src/lib/protocol.test.ts`
Expected: FAIL，`adoptPersistedUsage is not a function` / `wireUsageToUsage` 未导出。

- [ ] **Step 3: 实现协议映射**

`desktop/src/lib/protocol.ts` 在 `export interface Usage { ... }` 之后追加：

```ts
/**
 * `thread/readItems` / `thread/resume` 回包里持久化的上一轮用量（snake_case）。
 *
 * 与实时通知 `thread/tokenUsage/updated` 是同一份数据的两种来源：通知代表
 * 「当前这一轮」，这个字段代表「最后一轮落盘」。
 */
export interface WireUsage {
  model: string;
  input_tokens: number;
  output_tokens: number;
  cache_creation_input_tokens?: number;
  cache_read_input_tokens?: number;
}

/** 线形状 → 客户端形状。缺失/不全时按 0 处理（与通知路径同一口径）。 */
export function wireUsageToUsage(u: WireUsage | null | undefined): Usage | null {
  if (!u) return null;
  return {
    model: u.model,
    input: u.input_tokens,
    output: u.output_tokens,
    cacheRead: u.cache_read_input_tokens ?? 0,
    cacheWrite: u.cache_creation_input_tokens ?? 0,
  };
}
```

- [ ] **Step 4: 实现 `Session.adoptPersistedUsage`**

`desktop/src/lib/session.ts`，紧跟在 `note` 类的公开方法旁（例如 `notice(...)` 之后）：

```ts
  /**
   * 用服务端持久化的**上一轮**用量垫底。
   *
   * 只在本地还没有 live 快照时写入：`thread/tokenUsage/updated` 代表当前这一轮，
   * 比落盘的上一轮新，任何时候都不该被它覆盖。这条入口存在的唯一理由是
   * 「运行中的会话只走只读 `thread/readItems`、绝不 resume」——那条路径上不会
   * 有 live 通知，不垫底就永远显示「没有用量」。
   */
  adoptPersistedUsage(u: Usage | null): void {
    if (u === null || this.usage !== null) return;
    this.usage = u;
  }
```

- [ ] **Step 5: 在 `App` 的两处 readItems 调用点接入**

`desktop/src/App.tsx`：两处 `c0.request<{ items: Item[] }>("thread/readItems", ...)` 的类型改为
`c0.request<{ items: Item[]; usage?: WireUsage | null }>(...)`，并在 `v.session.upsertItems(r.items)` 之后加一行：

```ts
                v.session.adoptPersistedUsage(wireUsageToUsage(r.usage));
```

两处都要加（远端 silentTopUp 分支 `:642` 与桌面 running 分支 `:684`）。在文件顶部的 protocol 导入里追加 `wireUsageToUsage` 与 `type WireUsage`。

- [ ] **Step 6: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/session.test.ts src/lib/protocol.test.ts && npx tsc --noEmit`
Expected: PASS，tsc 无输出。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/protocol.test.ts desktop/src/lib/session.ts desktop/src/lib/session.test.ts desktop/src/App.tsx
git commit -m "feat(desktop): fold the persisted usage from readItems into the session"
```

---

### Task 3: 纯函数 `formatUsage`

**Files:**
- Create: `desktop/src/lib/usageCommand.ts`
- Test: `desktop/src/lib/usageCommand.test.ts`

**Interfaces:**
- Consumes: `Usage`（`lib/protocol`）、`estimateCost` / `formatCost`（`lib/pricing`）。
- Produces: `formatUsage(u: Usage, opts: { compactThreshold: number | null }): string`
  —— 多行文本，供 `App` 的 `/usage` 分支直接交给 `session.notice`。

- [ ] **Step 1: 写失败的测试**

```ts
import { describe, it, expect } from "vitest";
import { formatUsage } from "./usageCommand";

const base = {
  model: "claude-sonnet-4-20250514",
  input: 12345,
  output: 678,
  cacheRead: 10_000,
  cacheWrite: 0,
};

describe("formatUsage", () => {
  it("shows tokens, context occupancy and the estimated cost", () => {
    const out = formatUsage(base, { compactThreshold: 160_000 });
    expect(out).toContain("模型: claude-sonnet-4-20250514");
    expect(out).toContain("input: 12,345  output: 678");
    expect(out).toContain("cache read: 10,000  cache write: 0");
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
    expect(out).toMatch(/估算成本: \$0\.\d{4}|估算成本: < \$0\.01/);
  });

  it("omits the percentage when the threshold is unknown", () => {
    const out = formatUsage(base, { compactThreshold: null });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
    expect(out).not.toContain("/");
  });

  it("omits the percentage for a zero threshold instead of dividing by zero", () => {
    const out = formatUsage(base, { compactThreshold: 0 });
    expect(out).toContain("上下文: 12,345");
    expect(out).not.toContain("%");
  });

  it("prints an em dash for an unpriced model rather than a fake zero", () => {
    const out = formatUsage({ ...base, model: "some-local-llm" }, { compactThreshold: 1000 });
    expect(out).toContain("估算成本: —");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/usageCommand.test.ts`
Expected: FAIL，模块不存在。

- [ ] **Step 3: 实现**

```ts
/**
 * `/usage` 的正文。
 *
 * 纯函数：不碰 React、不碰 RPC，只把一份用量快照与上下文上限排成人读的几行。
 * 这样「有阈值/无阈值」「未知模型」这些分叉都能单测，不必拉起整个 App。
 */
import { estimateCost, formatCost } from "./pricing";
import type { Usage } from "./protocol";

/** 千分位。用量是给人看的数字，`12,345` 比 `12345` 容易读。 */
function thousands(n: number): string {
  return n.toLocaleString("en-US");
}

export function formatUsage(u: Usage, opts: { compactThreshold: number | null }): string {
  const limit = opts.compactThreshold;
  // 上限未知（老服务端 / 清单没声明 context_length）就不谈占用比例：分母是猜的，
  // 百分比只会误导。0 同理——那会让每一轮都触发压缩，不该被当成真实上限。
  const context =
    limit !== null && limit > 0
      ? `上下文: ${thousands(u.input)} / ${thousands(limit)} (${((u.input / limit) * 100).toFixed(1)}%)`
      : `上下文: ${thousands(u.input)}`;
  return [
    `模型: ${u.model}`,
    `input: ${thousands(u.input)}  output: ${thousands(u.output)}`,
    `cache read: ${thousands(u.cacheRead)}  cache write: ${thousands(u.cacheWrite)}`,
    context,
    `估算成本: ${formatCost(estimateCost(u))}`,
  ].join("\n");
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/usageCommand.test.ts`
Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/usageCommand.ts desktop/src/lib/usageCommand.test.ts
git commit -m "feat(desktop): add the /usage body as a pure formatter"
```

---

### Task 4: 命令目录 `cost` → `usage`

**Files:**
- Modify: `desktop/src/lib/slash.ts:22`
- Modify: `desktop/src/lib/slash.test.ts:11-33,44`
- Modify: `desktop/src/components/MessageInput.test.tsx:489-543,685-688`

**Interfaces:**
- Produces: 目录里的命令名 `usage`（弹窗、`/help`、App 分发共用同一份）。

- [ ] **Step 1: 改测试**

`slash.test.ts`：

```ts
      "usage",
```
替换列表里的 `"cost",`（保持字母序：clear, compact, config, help, model, usage）。

```ts
    expect(filterCommands("u").map((c) => c.name)).toEqual(["usage"]);
    expect(filterCommands("us").map((c) => c.name)).toEqual(["usage"]);
```
替换原来的两行 `filterCommands("co")` / `filterCommands("cost")`。

```ts
    expect(parseSlashInput("/usage")).toEqual({ kind: "command", name: "usage", args: null });
```
替换 `/cost` 那一行；`/help   cost` 的用例保持原样（`cost` 作为参数文本仍合法）。

`MessageInput.test.tsx`：四处
- 弹窗过滤用例：输入 `/co` 后只剩 `compact` / `config` 两条（`cost` 改名后不再匹配），
  删掉 `expect.stringContaining("/cost")` 那一行并把断言改成两条。
- "runs the selected command on Enter" 用例：`/cos` → `/us`，
  `toHaveBeenCalledWith("cost", null)` → `("usage", null)`。
- 手机端 Send 用例：`/cost` → `/usage`、`"cost"` → `"usage"`。
- `/help cost` 用例保持原样（`cost` 只是参数文本，不要求是命令名）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/slash.test.ts src/components/MessageInput.test.tsx`
Expected: FAIL（列表里没有 `usage`）。

- [ ] **Step 3: 改目录**

`desktop/src/lib/slash.ts`：

```ts
  { name: "usage", description: "查看 token 用量与上下文占用", usage: null, needsArg: false },
```

替换原 `cost` 行；同时把文件头注释里「6 个」的描述保持不变（数量没变）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/slash.test.ts src/components/MessageInput.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/slash.ts desktop/src/lib/slash.test.ts desktop/src/components/MessageInput.test.tsx
git commit -m "feat(desktop): rename the slash command /cost to /usage"
```

---

### Task 5: `/usage` 的执行分支

**Files:**
- Modify: `desktop/src/App.tsx:1623-1642`（`case "cost"` 整块）
- Test: `desktop/src/App.test.tsx`（`describe("App slash commands")` 内）

**Interfaces:**
- Consumes: `formatUsage`（Task 3）、`session.usage`（Task 2 会垫底）、`configReadParams`（既有）。
- Produces: 无（终端行为）。

- [ ] **Step 1: 写失败的测试**

在 `describe("App slash commands")` 内追加两个用例：

```tsx
  it("says the thread has no usage yet instead of a vague 'no data'", async () => {
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/usage" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/本会话还没有用量记录/);
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("renders the live usage with context occupancy from the session config", async () => {
    state.dataSources["config/read"] = () => ({
      provider: "anthropic",
      model: "claude-sonnet-4",
      compact_threshold: 160_000,
    });
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    act(() => {
      state.notifHandlers[0]({
        method: "thread/tokenUsage/updated",
        params: {
          thread_id: "t1",
          model: "claude-sonnet-4",
          input_tokens: 12_345,
          output_tokens: 678,
          cache_creation_input_tokens: 0,
          cache_read_input_tokens: 10_000,
        },
      });
    });

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/usage" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/上下文: 12,345 \/ 160,000 \(7\.7%\)/);
    expect(screen.getByText(/input: 12,345 {2}output: 678/)).toBeTruthy();
    // 读的是当前会话的阈值,不是进程级快照。
    expect(clients[0].requests.find((r) => r.method === "config/read")?.params).toEqual({
      threadId: "t1",
    });
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "usage"`
Expected: FAIL（`/usage` 落到 `default`，显示「未知命令」）。

- [ ] **Step 3: 实现**

把 `App.tsx` 的 `case "cost": { ... }` 整块替换为：

```ts
      case "usage": {
        const u = session.usage;
        if (!u) {
          session.notice("本会话还没有用量记录");
          break;
        }
        // 上下文上限按**当前会话**取（既有 `/config` 同一条路）：进程级快照的
        // compact_threshold 来自 sidecar 启动配置，与这个会话实际生效的模型无关。
        let threshold: number | null = null;
        try {
          const cfg = await c.request<Record<string, unknown>>(
            "config/read",
            configReadParams(id),
          );
          const v = cfg.compact_threshold;
          threshold = typeof v === "number" && v > 0 ? v : null;
        } catch {
          // 读配置失败不该吞掉用量：分母未知就只显示 token 数。
        }
        session.notice(formatUsage(u, { compactThreshold: threshold }));
        break;
      }
```

在文件顶部追加 `import { formatUsage } from "./lib/usageCommand";`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "usage"`
Expected: 2 passed。

- [ ] **Step 5: 全量前端验证**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全部通过（既有用例不得回归；若 `App.test.tsx` 里还有 `/cost` 字样，一并改掉）。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): run /usage with context occupancy and estimated cost"
```

---

### Task 6: 文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`（slash 命令那条，已存在 `[x] Slash 命令（/help /cost …）`）

- [ ] **Step 1: 更新那条 feature 记录**

把该条命令列表里的 `/cost` 改为 `/usage`，并在同一条内补一句判据，写清根因与修复：

```markdown
- [x] Slash 命令（`/help` `/usage` `/model` `/config` `/clear` `/compact`）— …（原有内容）…
  `/usage` 取代了旧的 `/cost`：后者只读前端 `Session.usage`，而该字段仅由实时通知
  `thread/tokenUsage/updated` 填充，运行中的会话按既有约束只走只读 `thread/readItems`、
  绝不 resume，于是常见情形下恒为 null（显示「暂无用量数据」）。修复两段：
  `thread/readItems` 的回包带上已读出的持久化用量（`yi-agent-app-server/src/rpc/thread.rs`），
  前端用 `Session.adoptPersistedUsage` 在无 live 快照时垫底
  （`desktop/src/lib/session.ts`）；输出抽成纯函数 `formatUsage`
  （`desktop/src/lib/usageCommand.ts`），包含模型 / input / output / cache /
  上下文占用（`input / compact_threshold`，取自 `config/read` 的会话视图）/ 估算成本。
  判据：`cd desktop && npx vitest run src/lib/usageCommand.test.ts src/lib/slash.test.ts src/App.test.tsx src/lib/session.test.ts`；
  `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib read_items` —
  [设计](../superpowers/specs/2026-10-21-desktop-usage-command-design.md)
```

- [ ] **Step 2: 检查 README 计数**

Run: `cd docs/project-management && grep -n "desktop" README.md`
若该模块的「完成 / 总计」计数因本改动变化（这里是替换而非新增，通常不变），同步更新。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: record the desktop /usage command in the module file"
```

---

### Task 7: 合并前验证

- [ ] **Step 1: 两边测试各跑一遍**

```bash
cd desktop && npx tsc --noEmit && npx vitest run
cd ../yi-agent-rs && cargo fmt --all -- --check && cargo test -p yi-agent-app-server --lib
```

Expected: 全绿。若 `cargo test` 卡住，先 `ps aux | grep -E "cargo|yi_agent"` 清残留进程。

- [ ] **Step 2: 检查是否还有 `/cost` 残留**

```bash
cd .. && grep -rn "\"cost\"\|/cost" desktop/src docs/project-management/desktop.md
```

Expected: 只剩 `MessageInput.test.tsx` 里作为**参数文本**的 `cost`（如 `/help cost` 用例）与 TUI 的 `tui/cost.rs` 引用（不属于桌面端）。桌面端命令表中不得再有 `cost` 条目。

- [ ] **Step 3: 交付**

按 `superpowers:finishing-a-development-branch` 合并回 `main`。
