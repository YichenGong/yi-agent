# 长会话性能退化修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把"每个流式 delta 都做一份与整个会话成正比的工作"改成"只做与本次变更成正比的工作"，让流式开销不再随会话变长。

**Architecture:** 三处独立站点，同一原理。(1) `desktop` 前端把 `ChatView` 的转录拆成"冻结的历史前缀 + 活跃尾部"，并给 `Session` 加 `structuralVersion` 让结构性变更（就地替换/推理块接管/删除）能让前缀失效；(2) 流式中那条消息的 markdown 解析按时间节流；(3) TUI 的 `HistoryState` 用"脏 cell 集合"替代每帧全表指纹扫描，并加历史舍弃上限。服务端 checkpoint **不做任何行为变更**，只加回归保护（它已实测不是瓶颈，且是"崩溃不丢东西"的保证）。

**Tech Stack:** TypeScript + React 18 + vitest（`desktop/`）；Rust + ratatui + cargo test（`yi-agent-rs/crates/yi-agent/`）；`yi-agent-app-server`（回归测试用）。

## Global Constraints

- **不得在 `main` 分支上改代码或提交**：所有改动在 worktree `.worktrees/long-session-perf`（分支 `perf/long-session-scaling`）内进行。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）与 `cd desktop && npx tsc --noEmit`。
- **Commit message** 用 conventional commits（`perf:`、`fix:`、`test:` 等），首行 ≤72 字符，**不得写** `Co-Authored-By: Claude ...`。
- **按 crate 跑测试，不要 `--workspace`**；跑前 `ps aux | grep cargo` 确认为空（避免 OOM/exit 137 与锁竞争）。
- **协议不变、线路上字节不变、转录内容与顺序不变、持久化格式不变**。帧的**数量与时序也不变**。
- **崩溃恢复不丢东西（硬要求）**：任何改动都不得减少崩溃后可恢复的内容；checkpoint 行为**完全不动**。
- **真实 LLM 测试默认跳过**（`#[ignore]`）；本计划全部用 mock 测试与单测，不需要 API key。
- 前端性能断言一律断言**结构性不变量**（render 计数、比较次数），**不得断言毫秒**。
- 参考基线脚本：`cd desktop && node scripts/perf/run.mjs`（八项，全绿才算环境就绪）。

## File Structure

| 文件 | 职责 | 本计划触及 |
|---|---|---|
| `desktop/src/lib/session.ts` | 转录数据层：通知归并、item 定位、插入锚定 | 加 `structuralVersion`；加尾部定位辅助 |
| `desktop/src/lib/session.test.ts` | 数据层单测（64 例） | 加 `structuralVersion` 与定位的用例 |
| `desktop/src/components/ChatView.tsx` | 转录渲染：前缀/尾部拆分、memo 边界 | 核心改动（Task 1、2） |
| `desktop/src/components/ChatView.test.tsx` | 渲染单测（17 例，已有 render 计数 harness） | 加"前缀不重渲染""结构性变更后更新"用例 |
| `desktop/src/lib/useThrottledText.ts` | **新建**：流式文本的时间节流（尾沿补发） | Task 2 |
| `desktop/src/lib/useThrottledText.test.ts` | **新建**：节流策略单测（假时钟） | Task 2 |
| `desktop/src/App.tsx` | 宿主接线：把 `structuralVersion` 透传给 `ChatView` | Task 1 的一行接线 |
| `yi-agent-rs/crates/yi-agent/src/tui/history.rs` | TUI 历史：memo 缓存、脏集合、舍弃上限 | Task 3 |
| `yi-agent-rs/crates/yi-agent/src/tui/app.rs` | TUI 主循环 | Task 3（仅测试） |
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | checkpoint 存储 | Task 4（只加测试） |

**顺序**：Task 1 → Task 2（依赖 1）。Task 3、4 独立，可并行。

## Spec 覆盖对照

每个 spec 目标对应的任务与门禁（便于评审时逐条核对，也便于发现漏项）：

| spec 目标 | 由哪个任务实现 | 门禁 |
|---|---|---|
| G1 数据层定位 | Task 1 Step 3（`indexOfItem`，作为 G2 的附带项） | Task 1 Step 4（`session.test.ts` 全绿）+ 既有流式用例 |
| G2 冻结前缀 | Task 1 Step 7 | Task 1 Step 5/8（前缀不重渲染 + `structuralVersion` 变更后更新） |
| G3 markdown 节流 | Task 2 Step 3/5 | Task 2 Step 1/6（节流策略 + 终值不被吞） |
| G4 checkpoint 不退化 | Task 4（纯测试） | Task 4 Step 1/3（崩溃不丢已完成 items、不丢提问） |
| G5 TUI 每帧不随长度增长 + 舍弃上限 | Task 3 Step 3/8 | Task 3 Step 1/5b/6 |
| G6 零语义回归 | 所有任务（Global Constraints） | 每个任务的"运行全量测试"步骤 + 完成后的整合验证（协议/字节/格式均未改） |
| G7 崩溃不丢东西 | Task 4 + Global Constraints | Task 4 Step 1/3；实现上**不改** checkpoint 行为 |

**明确不在本计划内**：第五项（远程 delta 合并，spec §8.1）。它是遗留项，
已记录在 `docs/bug-list.md`，本计划不实现、不改动它相关的代码。

---

### Task 1: 冻结前缀 + 活跃尾部（G2，含 G1 定位）

**Files:**
- Modify: `desktop/src/lib/session.ts`（`structuralVersion` 字段 + 三类失效点 + `indexOfItem`）
- Modify: `desktop/src/lib/session.test.ts`
- Modify: `desktop/src/components/ChatView.tsx:116-177`（`ChatItem`）、`:190-241`（`ChatView` 渲染）
- Modify: `desktop/src/components/ChatView.test.tsx`
- Modify: `desktop/src/App.tsx:2132-2140`（透传 `structuralVersion`）

**Interfaces:**
- Consumes: 无（首个任务）。
- Produces:
  - `Session.structuralVersion: number` — 只读计数；结构性变更时自增，纯文本 append 时**不**变。
  - `Session.indexOfItem(id: string): number` — private；尾部偏置查找，未命中返回 `-1`。
  - `ChatView` 新增可选 prop `structuralVersion?: number`（默认 `0`）。
  - `ChatItem` props：`{ item, text: string, threadId, call }` — **`text` 是独立 primitive prop**（见下方"为什么"）。

- [ ] **Step 1: 写失败测试（数据层：structuralVersion 只对结构性变更自增）**

追加到 `desktop/src/lib/session.test.ts` 末尾（`describe("Session")` 之外，新建一个 describe）：

```ts
describe("Session structuralVersion", () => {
  it("does not change when streamed text is appended in place", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "" } },
    });
    const before = s.structuralVersion;
    s.apply({ method: "item/delta", params: { thread_id: "t", item_id: "a1", delta: "x" } });
    // A pure append mutates `item.text` in place: the history prefix above it
    // has not moved, so the frozen subtree must stay frozen.
    expect(s.structuralVersion).toBe(before);
  });

  it("changes when an item is replaced in place on item/completed", () => {
    const s = new Session();
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "hi" } },
    });
    const before = s.structuralVersion;
    s.apply({
      method: "item/completed",
      params: { thread_id: "t", item: { type: "agentMessage", id: "a1", text: "hi there" } },
    });
    expect(s.structuralVersion).toBeGreaterThan(before);
  });

  it("changes when a reasoning block is taken over by a server item", () => {
    const s = new Session();
    s.apply({ method: "thread/reasoningDelta", params: { thread_id: "t", text: "think", kind: "Content" } });
    const before = s.structuralVersion;
    s.apply({
      method: "item/started",
      params: { thread_id: "t", item: { type: "thinking", id: "th1", text: "think" } },
    });
    expect(s.structuralVersion).toBeGreaterThan(before);
  });

  it("changes when a locally minted bubble is dropped", () => {
    const s = new Session();
    s.addUserMessage("oops");
    const before = s.structuralVersion;
    s.dropLocalUserMessage("oops");
    expect(s.structuralVersion).toBeGreaterThan(before);
  });
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts -t structuralVersion`
Expected: FAIL — `structuralVersion` 为 `undefined`，`expect(undefined).toBe(...)` 不通过。

- [ ] **Step 3: 实现 `structuralVersion` 与 `indexOfItem`**

在 `desktop/src/lib/session.ts` 的 `Session` 类里，紧跟 `lastServerItemId` 声明之后加入：

```ts
  /**
   * Bumped only when the **structure** of `items` changes: a slot is replaced,
   * an item is inserted or removed, a reasoning block is taken over. A pure
   * text append (`item.text += delta`) does NOT bump it.
   *
   * `ChatView` uses this as the reset key for its frozen history prefix. It is
   * deliberately a counter and not a snapshot: the prefix only needs to know
   * "did anything above me move?", which is exactly what this answers.
   */
  structuralVersion = 0;
```

在同一类里加辅助方法（放在 `insertAfterLastServerItem` 之前）：

```ts
  /**
   * Resolve the index of `id`, biased to the tail.
   *
   * During a turn the mutating target is always near the end — the trailing
   * agent message, or that message displaced by one when a reasoning block
   * resumes after it (`this.items.push(block)` in the reasoning arm). Scanning
   * **backwards** therefore hits in a bounded number of steps.
   *
   * Do NOT "optimise" this to a tail check plus `Array.prototype.findIndex`:
   * the fallback scans forward from the head, so whenever the target sits at
   * `length - 2` it walks the entire history on every delta. Measured (see
   * `desktop/scripts/perf/index-lookup.mjs`): 400420 comparisons vs 420.
   */
  private indexOfItem(id: string): number {
    for (let i = this.items.length - 1; i >= 0; i -= 1) {
      if (this.items[i].id === id) return i;
    }
    return -1;
  }
```

然后把 `item/delta` 分支里的定位改为：

```ts
        const index = this.indexOfItem(item_id);
```

并在**结构性变更**处自增。逐处如下：

在 `insertAfterLastServerItem` 的插入之后（`this.items.push(item)` 与 `this.items.splice(...)` 两个分支都要）加 `this.structuralVersion += 1;`：

```ts
    if (last < 0) {
      this.items.push(item);
      this.structuralVersion += 1;
    } else {
      this.items.splice(last + 1, 0, item);
      this.structuralVersion += 1;
    }
```

在 `item/completed` 的就地替换处（`this.items[index] = item;`）与推理块接管处（`this.items[held] = item;`）各加一行 `this.structuralVersion += 1;`。

在 `dropLocalUserMessage` 的 `this.items.splice(index, 1)` 之后加 `this.structuralVersion += 1;`。

在 `upsertItems` 的 `this.items[index] = normalized;` 与 `this.items[local] = normalized;` 之后各加 `this.structuralVersion += 1;`（它走 `insertAfterLastServerItem` 时已计一次，不要重复计：只在**替换**分支加，插入分支由上面覆盖）。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: PASS（含既有 64 例，全绿）。

- [ ] **Step 5: 写失败测试（渲染层：流式期间前缀整棵不重渲染）**

追加到 `desktop/src/components/ChatView.test.tsx`：先扩展已有的计数 harness，把 agent 消息的渲染也算进去。

```ts
// Appended to the existing `vi.mock` block at the top of the file: count how
// often items render, so a streamed delta on the tail cannot hide a re-render
// of the settled history above it.
const calls = vi.hoisted(() => ({ tool: 0, text: 0 }));
```

把已有的 `vi.mock("./MarkdownText", ...)` 改为：

```ts
vi.mock("./MarkdownText", () => ({
  AgentMessage: ({ text }: { text: string }) => {
    // Counted so a streamed delta on the tail cannot hide a re-render of the
    // settled messages above it.
    calls.text += 1;
    return <div>{text}</div>;
  },
}));
```

并在 `afterEach` 里一并重置：`calls.text = 0;`

新增用例：

```ts
describe("ChatView frozen prefix", () => {
  it("does not re-render history above the streaming tail", () => {
    const history: Item[] = [];
    for (let i = 0; i < 200; i += 1) {
      history.push({ type: "agentMessage", id: `h${i}`, text: `history ${i}` });
    }
    const tail = { type: "agentMessage", id: "tail", text: "" } as Item;
    const list = [...history, tail];

    const { rerender } = render(<ChatView items={list} />);
    const settled = calls.text;
    expect(settled).toBe(201); // every item rendered once on mount

    // Stream 5 deltas *in place*, exactly like `Session.apply` does.
    for (let i = 1; i <= 5; i += 1) {
      (tail as { text: string }).text = "chunk ".repeat(i);
      rerender(<ChatView items={list} />);
    }

    // Only the tail may have re-rendered. The 200 history items must not have.
    expect(calls.text - settled).toBeLessThanOrEqual(5);
  });

  it("re-renders history when structuralVersion changes", () => {
    const history: Item[] = [
      { type: "agentMessage", id: "h1", text: "old" },
      { type: "agentMessage", id: "tail", text: "streaming" },
    ];
    const { rerender } = render(<ChatView items={history} structuralVersion={1} />);
    const before = calls.text;

    // An `item/completed` replace: same index, new object, new text.
    history[0] = { type: "agentMessage", id: "h1", text: "settled" };
    rerender(<ChatView items={history} structuralVersion={2} />);

    expect(calls.text).toBeGreaterThan(before);
    expect(screen.getByText("settled")).toBeTruthy();
  });
});
```

- [ ] **Step 6: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx -t "frozen prefix"`
Expected: FAIL — 第一个用例 `calls.text - settled` 会等于 200（全部重建）或 `structuralVersion` prop 不被接受（TS 报错）。

- [ ] **Step 7: 实现 `ChatItem` memo 边界 + 前缀冻结**

在 `desktop/src/components/ChatView.tsx`，把 `function ChatItem({...})`（`:116`）整体替换为下面两个定义。

**关键：`text` 必须是独立 prop。** 比较器**不能**读 `item.text`——`Session.apply` 是先就地改 `item.text` 再渲染，`prev.item` 与 `next.item` 是同一个对象，读 `item.text` 等于拿当前值和自己比，永远相等，尾部会**冻住**（实测见 `desktop/scripts/perf/mutation-trap.mjs`）。

```tsx
/**
 * One transcript item.
 *
 * `text` is passed as its own primitive prop on purpose. `item/delta` mutates
 * the trailing agent message **in place**, so `prev.item === next.item` always
 * holds; a comparator that read `item.text` would be comparing the current
 * value against itself and the streamed reply would freeze. Comparing the
 * `text` prop instead lets React's default shallow comparison notice the change
 * while leaving every other message untouched.
 *
 * `threadId` / `call` are the image-read seam (see `App`'s `imageCall`); both are
 * stable, so they do not affect that memoization.
 */
const ChatItem = memo(
  function ChatItem({
    item,
    text,
    threadId,
    call,
  }: {
    item: Item | NoticeItem | ReasoningItem;
    text: string;
    threadId: string | null;
    call: ImageReadCall;
  }) {
    // The original switch, with `item.text` reads replaced by the `text` prop.
    switch (item.type) {
      case "notice":
        return item.config ? (
          <ConfigCard config={item.config.data} error={item.config.error} />
        ) : (
          <div
            data-testid="notice"
            className={
              item.level === "Warn"
                ? "my-1 self-center rounded-md bg-amber-500/10 px-3 py-1 text-xs text-amber-300"
                : "my-1 self-center rounded-md bg-raised/60 px-3 py-1 text-xs text-fg-muted"
            }
          >
            {item.text}
          </div>
        );
      case "reasoning":
        return <ReasoningBlock text={item.text} />;
      case "thinking":
        return <ReasoningBlock text={item.text} redacted={item.redacted} />;
      case "userMessage":
        return (
          <div className="my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white">
            {item.attachments && item.attachments.length > 0 && (
              <div className="mb-1 flex flex-wrap gap-1">
                {item.attachments.map((a) => (
                  <span
                    key={a.path}
                    className="rounded bg-blue-700/60 px-1.5 py-0.5 text-xs text-blue-50"
                    title={a.path}
                    data-testid="bubble-attachment"
                  >
                    {a.name || fileNameOf(a.path)}
                  </span>
                ))}
              </div>
            )}
            {item.images && item.images.length > 0 && (
              <div className="mb-1 flex flex-col gap-1">
                {item.images.map((img) => (
                  <BubbleImage key={img.path} path={img.path} threadId={threadId} call={call} />
                ))}
              </div>
            )}
            {item.text}
          </div>
        );
      case "agentMessage":
        return <AgentMessage text={text} />;
      case "toolCall":
        return <ToolCallRow item={item} threadId={threadId} call={call} />;
      default: {
        const unknown = item as { type?: string };
        return (
          <div className="my-1 font-mono text-xs text-fg-subtle">
            [unsupported item type: {unknown.type ?? "unknown"}]
          </div>
        );
      }
    }
  },
  // Compare the mutable primitive, never `item.text`.
  (prev, next) => prev.item === next.item && prev.text === next.text,
);
```

> **注意**：上面复刻了原 `ChatItem` 的全部分支。实现时请**逐字对照原函数**（`ChatView.tsx:116-177`）确保没有漏掉分支；本计划为可读性折叠了部分 JSX，但**结构必须与原实现一致**——尤其是 `data-testid`、`className`、`redacted` 与 `images` 分支。

在 `ChatView` 之前加前缀组件：

```tsx
/**
 * The settled history above the streaming tail.
 *
 * Its whole purpose is to *not* re-render while only the tail grows. It bails
 * out whenever `items`, `count` and `structuralVersion` are all unchanged; a
 * pure text append touches none of them, so a long session performs no work
 * proportional to its length per delta.
 *
 * `count` is explicit because `items` is mutated in place: its length changes
 * without the array identity changing, so the memo cannot rely on the array
 * reference alone to notice that a new item appeared.
 */
const HistoryPrefix = memo(
  function HistoryPrefix({
    items,
    count,
    threadId,
    call,
  }: {
    items: (Item | NoticeItem | ReasoningItem)[];
    count: number;
    threadId: string | null;
    call: ImageReadCall;
  }) {
    const out = [];
    for (let i = 0; i < count; i += 1) {
      const it = items[i];
      out.push(
        <ChatItem
          key={it.id ?? i}
          item={it}
          text={it.type === "agentMessage" ? it.text : ""}
          threadId={threadId}
          call={call}
        />,
      );
    }
    return <>{out}</>;
  },
  // `items`/`count` unchanged + no structural change ⇒ nothing above moved.
  (prev, next) =>
    prev.items === next.items &&
    prev.count === next.count &&
    prev.threadId === next.threadId &&
    prev.call === next.call,
);
```

在 `ChatView` 的 props 里加 `structuralVersion`，并把渲染改为前缀 + 尾部：

```tsx
export function ChatView({
  items,
  error,
  retrying,
  threadId = null,
  call,
  structuralVersion = 0,
}: {
  items: (Item | NoticeItem | ReasoningItem)[];
  error?: string | null;
  retrying?: { attempt: number; max: number; cause: RetryCause } | null;
  threadId?: string | null;
  call?: ImageReadCall;
  /**
   * Bumped by `Session` when the transcript's *structure* changes (a slot is
   * replaced, an item inserted or removed). Passed into the frozen prefix so a
   * structural change rebuilds it while a pure text append skips it entirely.
   */
  structuralVersion?: number;
}) {
```

把 `ChatView` 的返回里 `{items.map(...)}` 那段（`:241`）替换为：

```tsx
      {/* Re-keying on `structuralVersion` is what makes the structure change
          reach the frozen prefix: `items` is the same array instance forever,
          so without a changing prop the prefix would never rebuild. */}
      <HistoryPrefix
        key={structuralVersion}
        items={items}
        count={items.length - 1}
        threadId={threadId}
        call={effectiveCall}
      />
      {tail && (
        <ChatItem
          key={tail.id ?? "tail"}
          item={tail}
          text={tail.type === "agentMessage" ? tail.text : ""}
          threadId={threadId}
          call={effectiveCall}
        />
      )}
```

并在已有的 `lastItem` / `lastText` 计算处补：

```tsx
  // The live tail is rendered outside the frozen prefix. `tail` is `undefined`
  // for an empty transcript, which is how the prefix's `count` of -1 is guarded.
  const tail = items.length > 0 ? items[items.length - 1] : undefined;
```

- [ ] **Step 8: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx && npx vitest run src/lib/session.test.ts && npx tsc --noEmit`
Expected: PASS。TS 必须干净（`ChatView` 的 `structuralVersion` 是可选 prop，既有调用点不受影响）。

- [ ] **Step 9: 接线 `App.tsx`**

在 `desktop/src/App.tsx` 的 `<ChatView ... />`（`:2132`）加一行：

```tsx
                structuralVersion={current?.session.structuralVersion ?? 0}
```

- [ ] **Step 10: 运行全量前端测试与类型检查**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: PASS（既有 1231 例全绿）。

- [ ] **Step 11: 复跑性能脚本，确认"随会话增长"已消失**

Run: `cd desktop && node scripts/perf/turn.mjs`
Expected: `current` 行消失（G2 已是默认形态）或 depth 扫描里不再随历史爬升；`G2` 行持平。这是**参考基线**，不作门禁。

- [ ] **Step 12: Commit**

```bash
cd desktop && npx tsc --noEmit
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts \
        desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx \
        desktop/src/App.tsx
git commit -m "perf(desktop): freeze the settled history above the streaming tail

Every notification re-rendered the whole transcript, so a turn cost grew
with the session length. Split ChatView into a frozen prefix plus the live
tail, and give Session a structuralVersion so in-place replacements still
rebuild the prefix.

The mutable text travels as its own prop: comparing item.text in the memo
would compare the current value against itself (Session.apply mutates it in
place before React renders) and the reply would freeze."
```

---

### Task 2: 流式 markdown 节流（G3）

**Files:**
- Create: `desktop/src/lib/useThrottledText.ts`
- Create: `desktop/src/lib/useThrottledText.test.ts`
- Modify: `desktop/src/components/ChatView.tsx`（活跃尾部使用节流后的文本）

**Interfaces:**
- Consumes: Task 1 的 `ChatItem`（`text` prop）。
- Produces:
  - `useThrottledText(text: string, ms: number): string` — 返回节流后的文本；窗口内的多次变更合并，**尾沿补发**（窗口结束时渲染最后一次）；`text` 不再变化时立即返回终值。
  - `THROTTLE_MS: number` — 默认窗口，导出以便测试与调用方共用。

- [ ] **Step 1: 写失败测试（节流策略，假时钟）**

创建 `desktop/src/lib/useThrottledText.test.ts`：

```ts
/** @vitest-environment jsdom */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useThrottledText, THROTTLE_MS } from "./useThrottledText";

beforeEach(() => {
  vi.useFakeTimers();
});
afterEach(() => {
  vi.useRealTimers();
});

describe("useThrottledText", () => {
  it("returns the first value immediately", () => {
    const { result } = renderHook(({ text }) => useThrottledText(text, THROTTLE_MS), {
      initialProps: { text: "a" },
    });
    expect(result.current).toBe("a");
  });

  it("collapses rapid updates into one, then flushes the last one (trailing edge)", () => {
    const { result, rerender } = renderHook(
      ({ text }) => useThrottledText(text, THROTTLE_MS),
      { initialProps: { text: "" } },
    );
    // Ten deltas inside one window.
    for (let i = 1; i <= 10; i += 1) {
      rerender({ text: "x".repeat(i) });
    }
    // Still inside the window: the intermediate values must not each render.
    expect(result.current).toBe("");
    act(() => {
      vi.advanceTimersByTime(THROTTLE_MS);
    });
    // Trailing edge: the LAST value is shown, not the first.
    expect(result.current).toBe("x".repeat(10));
  });

  it("does not swallow the final value when the stream ends mid-window", () => {
    const { result, rerender } = renderHook(
      ({ text }) => useThrottledText(text, THROTTLE_MS),
      { initialProps: { text: "" } },
    );
    rerender({ text: "final answer" });
    // The authoritative `item/completed` text must render even though the
    // window has not elapsed — a swallowed final value is a correctness bug.
    expect(result.current).toBe("final answer");
  });
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/lib/useThrottledText.test.ts`
Expected: FAIL — 模块不存在。

- [ ] **Step 3: 实现节流 hook**

创建 `desktop/src/lib/useThrottledText.ts`：

```ts
import { useEffect, useRef, useState } from "react";

/**
 * The throttle window for streamed text, in ms.
 *
 * ~4 frames at 60Hz: long enough that a token-per-delta firehose collapses into
 * a few re-parses per second, short enough that typing-speed text still looks
 * live. Only the *streaming* message is throttled; settled messages are memoized
 * on their text and cost nothing either way.
 */
export const THROTTLE_MS = 60;

/**
 * Throttle `text` to at most one update per `ms`, **trailing-edge**.
 *
 * Why trailing: the value that must never be lost is the *last* one. A leading
 * edge would show the first token of a window and then hold it, so the visible
 * text would lag behind the stream by a full window for the rest of the turn.
 *
 * The final value is returned eagerly when `text` stops changing between
 * renders (the caller has just seen the settled text — e.g. `item/completed`
 * replaced the whole item), so a window can never swallow the authoritative
 * text.
 */
export function useThrottledText(text: string, ms: number = THROTTLE_MS): string {
  const [shown, setShown] = useState(text);
  const lastFlush = useRef(0);
  const pending = useRef(text);

  useEffect(() => {
    pending.current = text;
    if (text === shown) return;

    const now = Date.now();
    const elapsed = now - lastFlush.current;
    if (elapsed >= ms) {
      lastFlush.current = now;
      setShown(text);
      return;
    }

    const timer = window.setTimeout(() => {
      lastFlush.current = Date.now();
      // Trailing edge: flush whatever the latest value is, not the one that
      // arrived when the timer was scheduled.
      setShown(pending.current);
    }, ms - elapsed);
    return () => window.clearTimeout(timer);
  }, [text, ms, shown]);

  return shown;
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/lib/useThrottledText.test.ts`
Expected: PASS（三个用例）。

- [ ] **Step 5: 在活跃尾部接上节流**

在 `desktop/src/components/ChatView.tsx` 里 import 并在尾部使用：

```tsx
import { useThrottledText } from "../lib/useThrottledText";
```

在 `ChatView` 内、`tail` 计算之后加：

```tsx
  // Only the streaming tail is throttled. Keep the raw text for every other
  // consumer (the stick-to-bottom effect already reads `lastText`).
  const rawTailText = tail && tail.type === "agentMessage" ? tail.text : "";
  const throttledTailText = useThrottledText(rawTailText);
```

并在尾部 `<ChatItem>` 上传：

```tsx
          text={tail.type === "agentMessage" ? throttledTailText : ""}
```

> **注意**：`HistoryPrefix` 内部**不要**用节流值——历史文本不再变化，节流只会引入无谓的定时器。

- [ ] **Step 6: 写失败测试（ChatView 不再每 delta 重解析）**

追加到 `desktop/src/components/ChatView.test.tsx`：

```ts
describe("ChatView streaming markdown throttle", () => {
  it("re-parses far fewer times than there are deltas, and still shows the final text", () => {
    const list: Item[] = [{ type: "agentMessage", id: "a1", text: "" } as Item];
    const { container, rerender } = render(<ChatView items={list} />);
    const settled = calls.text;

    for (let i = 1; i <= 100; i += 1) {
      (list[0] as { text: string }).text = "tok ".repeat(i);
      rerender(<ChatView items={list} />);
    }

    // Far fewer than 100: the window collapses them.
    expect(calls.text - settled).toBeLessThan(50);

    // And the terminal value is visible — a throttle must not swallow it.
    (list[0] as { text: string }).text = "FINAL";
    rerender(<ChatView items={list} />);
    expect(container.textContent).toContain("FINAL");
  });
});
```

- [ ] **Step 7: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx && npx vitest run`
Expected: PASS（全量前端测试）。

- [ ] **Step 8: 类型检查并提交**

```bash
cd desktop && npx tsc --noEmit
git add desktop/src/lib/useThrottledText.ts desktop/src/lib/useThrottledText.test.ts \
        desktop/src/components/ChatView.tsx desktop/src/components/ChatView.test.tsx
git commit -m "perf(desktop): throttle the streaming message's markdown

The growing message re-parsed its whole accumulated text on every delta,
so one turn cost scaled quadratically. Throttle it to one update per
window, trailing-edge so the last value is never lost, and return the
settled value eagerly so item/completed is never swallowed."
```

---

### Task 3: TUI 脏 cell 集合 + 历史舍弃上限（G5）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/history.rs`（`HistoryCache` 脏集合、`ensure_cache`、`push`、`clear`、`MAX_HISTORY_CELLS`）
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（仅新增测试）

**Interfaces:**
- Consumes: 无（独立于 Task 1/2）。
- Produces:
  - `MAX_HISTORY_CELLS: usize` — 历史 cell 上限（导出，供测试与调用方引用）。
  - `HistoryState::dropped_cells(&self) -> usize` — 已被舍弃的 cell 计数（测试与状态栏可用）。
  - `HistoryCache.dirty: std::collections::BTreeSet<usize>` — 待重渲染的 cell 下标。

- [ ] **Step 1: 写失败测试（流式期间不随历史长度整表扫描）**

追加到 `yi-agent-rs/crates/yi-agent/src/tui/history.rs` 的 `mod tests`（与既有 `cell_renders_for_one_frame` 同风格）：

```rust
    /// Streaming must not touch the settled scrollback: the number of cell
    /// re-renders for one streamed delta must not grow with `cells.len()`.
    ///
    /// This is the TUI counterpart of the desktop delivery: a long session must
    /// not pay for its length on every frame.
    #[test]
    fn a_streamed_delta_does_not_rescan_the_whole_history() {
        fn renders_for_one_delta(cells: usize) -> usize {
            let mut state = HistoryState::new();
            for index in 0..cells {
                state.push(
                    HistoryCell::AssistantMessage {
                        markdown: format!("answer {index} with some words"),
                    },
                    80,
                );
            }
            // Warm: everything rendered at this width.
            let width = state.text_width(80, 24);
            let _ = state.flattened_line_count(width);

            crate::tui::cell::reset_lines_call_count();
            state.push_event(AgentEvent::AssistantText(" more".into()), 80);
            crate::tui::cell::lines_call_count()
        }

        let small = renders_for_one_delta(10);
        let large = renders_for_one_delta(200);
        assert_eq!(
            small, large,
            "a streamed delta re-rendered {small} vs {large} cells; it must not \
             depend on the history length"
        );
        assert!(large <= 2, "only the mutated tail may re-render, got {large}");
    }
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent a_streamed_delta_does_not_rescan_the_whole_history`
Expected: FAIL —— 现在 `ensure_cache` 全表比对指纹，`large` 会大于 `small`（或 `large > 2`）。

- [ ] **Step 3: 让失效精确化（脏集合）**

在 `history.rs` 的 `HistoryCache` 结构体（`:171`）里加字段：

```rust
    /// Cell indexes known to need a re-render.
    ///
    /// `invalidate_cell` already knows exactly which cell an event mutated; this
    /// set carries that knowledge to `ensure_cache` so a frame no longer has to
    /// re-discover it by fingerprinting every cell. A `BTreeSet` (not a `HashSet`)
    /// so a batch of mutations is processed in ascending index order, which keeps
    /// `recompute_cache_accelerators` linear.
    dirty: std::collections::BTreeSet<usize>,
```

在 `HistoryCache::new` 里初始化 `dirty: std::collections::BTreeSet::new(),`。

在 `invalidate_cell`（`:644`）里登记：

```rust
    fn invalidate_cell(&self, index: usize) {
        let mut cache = self.cache.borrow_mut();
        if let Some(entry) = cache.entries.get_mut(index) {
            entry.lines = None;
        }
        cache.dirty.insert(index);
    }
```

> 注意 `invalidate_cell` **不再**调用 `note_content_change`——它现在只登记 dirty，
> 由调用方（`invalidate_mutated_cells` / `push_event`）在末尾统一 `note_content_change`。
> 检查所有调用点，确保各自末尾仍有一次 `note_content_change()`（`push_event` 在
> `:1059` 已有；`toggle_fold_selected`、`toggle_permission_expanded` 需要补）。

**⚠️ 必须一并修的"沉默失效"点。** 全表扫描有一个副作用：它会在每帧**兜住**那些
"改了 cell 却没登记"的路径。改成脏集合后，这类路径会变成**陈旧渲染**。
已 grep 出当前**只调 `note_content_change()`、不调 `invalidate_cell`** 的就地修改点，
它们都必须补登记：

- `extend_last_assistant_text`（`history.rs:281`）：`markdown.push_str(more)` 之后，
  改为登记尾部——`self.invalidate_cell(self.cells.len() - 1);`（替换原来的
  `self.note_content_change();`）。**这是最重要的一处**：它由 `trace.rs:200` 的子
  agent 轨迹流每段文本调用一次。
- `push`（`:270`）与 `push_event`（`:821`）：它们**新增** cell，由 `ensure_cache`
  的"新尾部"循环覆盖，无需登记；但要在末尾保留各自现有的 `note_content_change()`。

实现后**必须**用 Step 5 的既有测试与新增的"扩展尾部的文本必须可见"测试把关：
后者断言 `extend_last_assistant_text` 之后渲染出的行确实变长（见 Step 5b）。

把 `ensure_cache`（`:313`）的全表循环改为只处理脏集合与新尾部：

```rust
            // Only the cells that were explicitly invalidated, plus any newly
            // appended ones, can differ. Fingerprinting the whole scrollback here
            // is what made a frame cost grow with the session length.
            let mut candidates: std::collections::BTreeSet<usize> = cache.dirty.clone();
            for index in cache.entries.len()..self.cells.len() {
                candidates.insert(index);
            }

            let mut changed = Vec::new();
            for index in candidates {
                if index >= self.cells.len() {
                    continue;
                }
                let fingerprint = cell_fingerprint(&self.cells[index]);
                let unchanged = cache
                    .entries
                    .get(index)
                    .map(|entry| entry.fingerprint == fingerprint && entry.lines.is_some())
                    .unwrap_or(false);
                if !unchanged {
                    changed.push((index, fingerprint));
                }
            }
            cache.dirty.clear();
```

> **宽高变化**仍必须整表重建（wrap 是宽度相关的）：保留 `cache.width != width` 分支里
> 的 `cache.entries.clear()`，并同时 `cache.dirty.clear()`（清空后 `entries.len()==0`，
> 上面的新尾部循环会把所有下标重新加入，行为等价于整表重建）。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent a_streamed_delta_does_not_rescan_the_whole_history`
Expected: PASS。

- [ ] **Step 5: 运行 TUI 既有测试，确认无回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: PASS（含 `app.rs:7854` 那条"流式 4 token 只重渲染 ≤8 cell"与 `history.rs` 的既有缓存测试）。
若 `history.rs` 的部分测试因 `note_content_change` 位置调整而失败，**优先修实现而不是改断言**。

- [ ] **Step 5b: 写测试，钉住"就地扩展尾部必须重新渲染"**

脏集合引入后，这类**沉默失效**是最容易漏的回归：改了 cell 却没登记，于是渲染陈旧。
追加到 `history.rs` 的 `mod tests`：

```rust
    /// Extending the trailing assistant message mutates it in place. With the
    /// per-frame full scan gone, this path relies on its invalidation being
    /// registered — otherwise the appended text renders stale.
    ///
    /// Covers both entry points: `extend_last_assistant_text` (the subagent
    /// trace stream, `trace.rs`) and the `AssistantText` event arm.
    #[test]
    fn appended_assistant_text_is_visible_after_the_full_scan_is_gone() {
        fn rendered_text(state: &HistoryState, width: u16) -> String {
            state
                .flattened_lines(width)
                .into_iter()
                .flat_map(|(_, line)| line.spans.into_iter().map(|s| s.content.into_owned()))
                .collect()
        }

        let mut state = HistoryState::new();
        state.push(HistoryCell::from_assistant_text("first"), 80);
        let width = state.text_width(80, 24);

        // 1) the trace-stream entry point
        assert!(state.extend_last_assistant_text(" second", 80));
        assert!(
            rendered_text(&state, width).contains("first second"),
            "extend_last_assistant_text must invalidate the cell it mutated"
        );

        // 2) the agent-event entry point
        state.push_event(AgentEvent::AssistantText(" third".into()), 80);
        assert!(
            rendered_text(&state, width).contains("first second third"),
            "the AssistantText arm must invalidate the cell it appended to"
        );
    }
```

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent appended_assistant_text_is_visible_after_the_full_scan_is_gone`
Expected: PASS。**若失败**，正是上面警告的沉默失效——补 `invalidate_cell` 登记，别改断言。

- [ ] **Step 6: 写失败测试（历史舍弃上限）**

```rust
    /// A very long session must not grow without bound; the oldest cells are
    /// dropped, and the transcript says so instead of silently losing them.
    #[test]
    fn history_is_capped_and_reports_what_it_dropped() {
        let mut state = HistoryState::new();
        for index in 0..(MAX_HISTORY_CELLS + 50) {
            state.push(
                HistoryCell::AssistantMessage {
                    markdown: format!("answer {index}"),
                },
                80,
            );
        }
        assert!(state.cells.len() <= MAX_HISTORY_CELLS);
        assert!(state.dropped_cells() >= 50, "dropped count must be reported");
        // The most recent cell must survive: dropping is oldest-first.
        let last = state.cells.last().expect("history must not be empty");
        match last {
            HistoryCell::AssistantMessage { markdown } => {
                assert!(markdown.contains(&format!("answer {}", MAX_HISTORY_CELLS + 49)));
            }
            other => panic!("expected the newest assistant message, got {other:?}"),
        }
        // And the visible window must still be coherent after the drop.
        let width = state.text_width(80, 24);
        state.reconcile_scroll_offset(width, 24);
        assert!(state.scroll_offset <= state.max_scroll_offset(width, 24));
    }
```

- [ ] **Step 7: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent history_is_capped_and_reports_what_it_dropped`
Expected: FAIL — `MAX_HISTORY_CELLS` / `dropped_cells` 不存在。

- [ ] **Step 8: 实现舍弃上限**

在 `history.rs` 顶部（`HistoryCache` 之前）加常量：

```rust
/// Upper bound on retained history cells.
///
/// A TUI session can run for days; without a cap the render cache and the cells
/// themselves grow without bound. Dropping is display-only: the authoritative
/// transcript lives in the server's `.jsonl` and is re-fetched by
/// `thread/resume`, so nothing is lost — see [`HistoryState::push`].
pub const MAX_HISTORY_CELLS: usize = 2000;
```

在 `HistoryState` 加字段与访问器：

```rust
    /// How many cells have been dropped from the head to respect
    /// [`MAX_HISTORY_CELLS`]. Surfaced so the UI can say the history was trimmed
    /// rather than silently losing it.
    dropped_cells: usize,
```

```rust
    /// Number of cells dropped from the head of the history so far.
    pub fn dropped_cells(&self) -> usize {
        self.dropped_cells
    }
```

在 `HistoryState::new` 与 `from_cells` 里初始化 `dropped_cells: 0,`。

> **加字段不影响既有测试**（已核对）：`history.rs` 里三处 `HistoryState { .. }`
> 构造（`:1311`、`:2054`、`:3164`）都以 `..HistoryState::new()` 收尾，
> 不会因为多一个私有字段而编译失败。`Debug` 实现用的是
> `finish_non_exhaustive()`（`:212`），也无需改动。

在 `push`（`:270`）与 `push_event`（`:821`）的**末尾**（`apply_scroll_delta` 之后）调用一个新方法：

```rust
    /// Enforce [`MAX_HISTORY_CELLS`], dropping oldest-first.
    ///
    /// Dropping the head shifts every index, so the cached entries must shift
    /// with them. Rather than reshuffling the cache under the borrow, the cache
    /// is rebuilt lazily: `entries` is cleared and the surviving cells are
    /// treated as new by `ensure_cache`'s tail scan.
    fn enforce_history_cap(&mut self) {
        if self.cells.len() <= MAX_HISTORY_CELLS {
            return;
        }
        let excess = self.cells.len() - MAX_HISTORY_CELLS;
        self.cells.drain(0..excess);
        self.dropped_cells += excess;
        self.selected = None;
        let cache = self.cache.get_mut();
        cache.entries.clear();
        cache.dirty.clear();
        cache.cumulative.truncate(1);
        cache.part_offsets.clear();
        cache.decision = None;
        // The viewport is re-derived from scratch after a head drop.
        self.scroll_offset = 0;
    }
```

在 `clear`（`:294`）里也重置 `self.dropped_cells = 0;`。

- [ ] **Step 9: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent`
Expected: PASS（新用例 + 既有全部）。

- [ ] **Step 10: 格式化、clippy 与提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo clippy -p yi-agent --all-targets -- -D warnings
git add yi-agent-rs/crates/yi-agent/src/tui/history.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "perf(tui): stop rescanning the whole history every frame

HistoryCache knew which cell an event mutated but threw that away, so
ensure_cache rediscovered it by fingerprinting every cell each frame — a
per-frame cost proportional to the session length. Carry the invalidated
indexes in a dirty set instead.

Also cap retained cells so a days-long session cannot grow without bound,
reporting the drop count rather than silently losing history (the
authoritative transcript lives in the server log)."
```

---

### Task 4: checkpoint 回归保护（G4，无行为变更）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（**只加测试**）

**Interfaces:**
- Consumes: 既有 `ThreadStore::write_partial` / `load` / `promote_partial`（签名不变）。
- Produces: 无新 API——本任务**不改变任何生产行为**，只把"不丢东西"钉成测试。

- [ ] **Step 1: 写测试（崩溃前已完成的 items 必须全部可恢复）**

追加到 `thread_store.rs` 的 `mod tests`：

```rust
    /// The user's requirement: after a crash, everything produced must still be
    /// visible. A checkpoint written mid-turn must survive `load` intact and be
    /// promotable into the authoritative log.
    ///
    /// This pins existing behaviour; it does not ask for any change. It exists so
    /// a future "optimisation" cannot silently thin the checkpoint.
    #[test]
    fn a_crash_mid_turn_loses_none_of_the_completed_items() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-crash")).unwrap();

        // A turn in progress: the opening prompt plus N completed items.
        // Built inline like the existing partial tests do — there is no
        // `assistant_item` helper in this module.
        let mut items = vec![user_item("user-turn-x", "do the thing")];
        for i in 0..50 {
            items.push(crate::protocol::Item::AgentMessage {
                id: format!("item-turn-x-{i}"),
                text: format!("step {i}"),
            });
        }
        store
            .write_partial(
                "thread-crash",
                &PartialTurn {
                    turn_id: "turn-x".into(),
                    items: items.clone(),
                    messages: Vec::new(),
                    usage: None,
                },
            )
            .unwrap();

        // Crash here: nothing else is written. Resume must see everything.
        let loaded = store.load("thread-crash").unwrap().unwrap();
        assert!(loaded.pending_turn, "an uncommitted turn must be reported");
        assert_eq!(
            loaded.items.len(),
            items.len(),
            "a crash must not lose completed items"
        );

        // And promoting it must make the content authoritative, not drop it.
        assert!(store.promote_partial("thread-crash").unwrap());
        let after = store.load("thread-crash").unwrap().unwrap();
        assert!(!after.pending_turn);
        assert_eq!(after.items.len(), items.len());
    }
```

- [ ] **Step 2: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_crash_mid_turn_loses_none_of_the_completed_items`
Expected: PASS（这是既有行为的锚点。**若它失败**，说明现状已经会丢东西——那是必须先报告的真实缺陷，不要改断言迁就。）

- [ ] **Step 3: 写测试（turn 刚开始就崩溃也不丢提问）**

```rust
    /// The driver checkpoints the opening prompt before the model is even called
    /// (`server.rs` "turn 开始就把提问落进 checkpoint"). A crash immediately
    /// after that must still show the question the user typed.
    #[test]
    fn the_opening_prompt_survives_an_immediate_crash() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-early")).unwrap();

        store
            .write_partial(
                "thread-early",
                &PartialTurn {
                    turn_id: "turn-e".into(),
                    items: vec![user_item("user-turn-e", "did you save me?")],
                    messages: Vec::new(),
                    usage: None,
                },
            )
            .unwrap();

        let loaded = store.load("thread-early").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(
            crate::server::item_id(&loaded.items[0]),
            Some("user-turn-e"),
            "the opening prompt must be recoverable"
        );
    }
```

> **注意**：`crate::server::item_id` 的签名是
> `pub(crate) fn item_id(item: &Item) -> Option<&str>`（`server.rs:4774`），
> 所以比较要用 `Some("user-turn-e")` 而不是字符串；测试模块已 `use` 了
> `crate::protocol::Item`，直接构造 `Item::AgentMessage` 即可。

- [ ] **Step 4: 运行全部相关测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（含既有 checkpoint/恢复/崩溃用例）。

- [ ] **Step 5: 格式化与提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "test(app-server): pin that a crash loses no completed items

The checkpoint rewrites a whole snapshot every 500ms, which looks wasteful
but measures at ~9ms per write inside the thread's own driver task, so it
is not a bottleneck — and it is what makes crash recovery show everything
produced. The user requires that: nothing may be dropped.

No behaviour change. These tests exist so a later optimisation cannot thin
the checkpoint without failing."
```

---

### Task 5: 文档同步（项目规范要求同一 PR 内完成）

**Files:**
- Modify: `docs/bug-list.md`（第 57 条 TUI 卡顿改为已修，附判据）
- Modify: `docs/project-management/desktop.md`（登记前端两项）
- Modify: `docs/project-management/yi-agent-tui.md`（登记 TUI 项）

**Interfaces:**
- Consumes: Task 1-4 的落地事实与验证命令。
- Produces: 文档条目；无代码接口。

- [ ] **Step 1: 更新 `docs/bug-list.md` 第 57 条**

把该条的 `- [ ]` 改为 `- [x]`，并补上根因与判据（照该文件既有风格：根因 + 代码位置 + 验证命令 + "还原即红"的证明）。判据用：

```
cargo test -p yi-agent --bin yi-agent a_streamed_delta_does_not_rescan_the_whole_history history_is_capped_and_reports_what_it_dropped
```

- [ ] **Step 2: 在 `desktop.md` 登记前端两项**

在 `desktop.md` 合适位置（与其它 `[x]` feature 同级）各加一条，带 **file:line** 判据：

- G2：`desktop/src/components/ChatView.tsx` 的 `HistoryPrefix` + `desktop/src/lib/session.ts` 的 `structuralVersion`；判据 `cd desktop && npx vitest run src/components/ChatView.test.tsx src/lib/session.test.ts`。
- G3：`desktop/src/lib/useThrottledText.ts`；判据 `cd desktop && npx vitest run src/lib/useThrottledText.test.ts src/components/ChatView.test.tsx`。

- [ ] **Step 3: 在 `docs/project-management/yi-agent-tui.md` 登记 TUI 项**

写明 `MAX_HISTORY_CELLS`、`dropped_cells()`、脏集合，以及判据命令。

- [ ] **Step 4: 同步 `docs/project-management/README.md` 的计数**

按模块索引表把对应模块的"完成/总计"计数改对（只改实际变化的行）。

- [ ] **Step 5: 确认无遗留占位与本次 spec 的"未做项"仍在**

Run: `rg -n "第五项|remote delta|delta 合并" docs/bug-list.md docs/superpowers/specs/2026-10-18-long-session-perf-design.md`
Expected: 第五项（远程 delta 合并）条目**依然存在且未勾选**——它是本 spec 明确未做的遗留项，不得被误删。

- [ ] **Step 6: Commit**

```bash
git add docs/bug-list.md docs/project-management/
git commit -m "docs: record the long-session performance fixes

Sync bug-list (the TUI stall is fixed) and the module files for the
desktop and TUI work, with the same acceptance commands the code ships
with. The deferred funnel item (remote delta coalescing) stays open."
```

---

## 完成后的整合

全部任务完成后，按项目规范走 `superpowers:finishing-a-development-branch`：
在 worktree 内跑完整验证 → `git merge --no-ff` 回 `main` → 删分支 + 移除 worktree。

合并前必须全绿：

```bash
cd desktop && npx vitest run && npx tsc --noEmit && npm run build
cd ../yi-agent-rs && cargo fmt --all -- --check && cargo test -p yi-agent --bin yi-agent && cargo test -p yi-agent-app-server --lib
```

**提醒**：本 spec 的**第五项（远程 delta 合并）仍未做**，已在 `docs/bug-list.md` 与 spec §8.1 记录。本计划**不**包含它。
