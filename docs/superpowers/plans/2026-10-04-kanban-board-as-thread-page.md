# 看板作为会话的平级页面 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面端看板从「盖在主区域、要点收起才离开」改成与会话平级的主区域页面：点任意会话即跳转回会话页，同时删除已冗余的「收起/展开」全套。

**Architecture:** 纯前端、纯本地 state 改动。主区域仍由 `App.tsx` 的 `selectedBoard` 分支决定画会话页还是看板页，只是把「离开看板」绑定到切换会话这一动作上（`selectThread` / `newThread` 里 `setSelectedBoard(null)`），进入看板仍走侧栏的 `onOpenBoard`。删除 `boardCollapsed` 状态、`SuperpowersKanbanCollapsedBar` 组件及其测试、`SuperpowersKanbanSettings` 的 `onCollapse` prop 与「收起」按钮。

**Tech Stack:** 桌面 Tauri + React 19、TypeScript、Tailwind；测试 vitest 3 + jsdom + @testing-library/react。

**Spec:** `docs/superpowers/specs/2026-10-04-kanban-board-as-thread-page-design.md`

## Global Constraints

- 行号以基线 `cccb884` 为准；改前先按文件内 anchor 文本 grep 复核（行号可能前移）。
- 只改桌面端（`desktop/src/`）；不动插件、不动宿主 RPC、不动看板四列视图与 2 秒轮询。
- `ThreadSidebar.tsx` 里另有一个同名 `boardCollapsed`（`useState<Set<string>>`，侧栏「看板会话」小节的折叠），**与本改动无关，严禁误删**——只删 `App.tsx` 里那个 `useState(false)`。
- 删除文件用 `git rm`；只显式 `git add <path>`，绝不 `git add -A`。
- 运行环境：`export PATH="/opt/homebrew/bin:$PATH"`；命令在 `desktop/` 下执行。
- 每个 Task 结束时测试全绿；Task 3 结束再跑 `tsc --noEmit` 与 `npm run build`。
- 中文文案与既有注释风格保持一致；不引入新依赖。

---

## File Structure

- `desktop/src/App.tsx` — 会话切换离开看板；`onOpenBoard` 去掉收起；主区域去掉收起分支与状态。
- `desktop/src/components/SuperpowersKanbanSettings.tsx` — 删 `onCollapse` prop 与「收起」按钮。
- `desktop/src/components/SuperpowersKanbanSettings.test.tsx` — 删 `onCollapse` 相关用例。
- `desktop/src/components/SuperpowersKanbanCollapsedBar.tsx` — **删除**。
- `desktop/src/components/SuperpowersKanbanCollapsedBar.test.tsx` — **删除**。
- `desktop/src/App.test.tsx` — 新增 3 条跳转用例；删/改依赖收起的用例。

---

### Task 1: 点会话离开看板（`selectThread` / `newThread`）

**Files:**
- Modify: `desktop/src/App.tsx`（`selectThread` 约 412-455；`newThread` 约 482-515）
- Test: `desktop/src/App.test.tsx`（新增到 `describe("App 主区域看板")`）

**Interfaces:**
- Consumes: 现有 `setSelectedBoard: (v: string | null) => void`（`App.tsx:231`）。
- Produces: 无新导出；行为契约——任何把会话切到前台的路径都会清空 `selectedBoard`。

- [ ] **Step 1: 写失败测试**

在 `desktop/src/App.test.tsx` 的 `describe("App 主区域看板")` 内（例如 `it("切换项目后看板问的是新项目", …)` 之后）追加两条用例：

```tsx
  it("看板打开时点侧栏另一条会话 → 跳到该会话页，看板消失（无需点收起）", async () => {
    state.threads = [
      { thread_id: "t1", title: "one", permission_mode: "normal" },
      { thread_id: "t2", title: "two", permission_mode: "normal" },
    ];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    render(<App />);
    // 启动自动选第一条会话（t1），再看板页。
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    // 点侧栏另一条会话：应当直接跳过去，看板退场。
    fireEvent.click(screen.getByText("two"));

    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/resume",
        params: { threadId: "t2" },
      }),
    );
  });

  it("看板打开时新建会话 → 落到会话页，看板消失", async () => {
    state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
    state.boards = [{ project: "/proj" }];
    state.groupWorkspaces = ["/proj"];
    // 「+ New thread」在有最近目录时弹菜单，点其中一项即 onNew(path)。
    state.dataSources["workspace/list"] = () => ({
      workspaces: [{ path: "/proj", exists: true }],
    });
    render(<App />);
    await waitFor(() => expect(screen.getByLabelText("看板")).toBeTruthy());
    await openBoardFor("/proj");
    await screen.findByText("Superpowers 看板");

    fireEvent.click(screen.getByText("+ New thread"));
    fireEvent.click(await screen.findByRole("menuitem", { name: "proj" }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/start")).toBe(true),
    );
    await waitFor(() => expect(screen.queryByLabelText("Superpowers 看板")).toBeNull());
  });
```

- [ ] **Step 2: 跑测试，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "点侧栏另一条会话"`

Expected: FAIL —— 看板仍在（`Superpowers 看板` 未变 null），因为 `selectThread` 不清 `selectedBoard`。
（第二条同理 fail。）

- [ ] **Step 3: 改 `selectThread`**

`App.tsx` 内 `selectThread` 开头（`store.select(id); setCurrentId(id);` 之后、`force((v) => v + 1);` 之前）插入：

```tsx
    // 会话页成为当前页：离开看板（若正在看）。这样点侧栏任意会话即「跳转」
    // 过去，不必再点一次「收起看板」。setSelectedBoard 幂等，不在看板时无害。
    setSelectedBoard(null);
```

改后开头应形如：

```tsx
  const selectThread = async (id: string) => {
    store.select(id);
    setCurrentId(id);
    // 会话页成为当前页：离开看板（若正在看）。……
    setSelectedBoard(null);
    // 手机端选中会话即收起抽屉，把宽度让回聊天区。
    if (isMobile) setSidebarOpen(false);
    force((v) => v + 1);
```

- [ ] **Step 4: 改 `newThread`**

`App.tsx` 内 `newThread` 的 `store.select(t.thread_id); setCurrentId(t.thread_id);` 之后加同一句：

```tsx
      // 新建会话同样把焦点带回会话页：与点会话一致。
      setSelectedBoard(null);
```

- [ ] **Step 5: 跑测试，确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx`

Expected: PASS，且无既有用例转红。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): selecting a thread leaves the board (board ≡ thread page)"
```

---

### Task 2: 删除看板「收起」状态与折叠条

**Files:**
- Modify: `desktop/src/App.tsx`（import 约 33；状态约 231-236；渲染约 1257-1320；`onOpenBoard` 约 553-566）
- Delete: `desktop/src/components/SuperpowersKanbanCollapsedBar.tsx`
- Delete: `desktop/src/components/SuperpowersKanbanCollapsedBar.test.tsx`
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 1 的「切会话离开看板」行为。
- Produces: 无 `boardCollapsed` 状态；`SuperpowersKanbanCollapsedBar` 不再存在；`onOpenBoard(path)` 语义不变（打开/切换/同项目 no-op）。

- [ ] **Step 1: 删组件文件**

```bash
git rm desktop/src/components/SuperpowersKanbanCollapsedBar.tsx \
       desktop/src/components/SuperpowersKanbanCollapsedBar.test.tsx
```

- [ ] **Step 2: 改 `App.tsx`**

(a) 删 import（约 33 行）：

```tsx
import { SuperpowersKanbanCollapsedBar } from "./components/SuperpowersKanbanCollapsedBar";
```

(b) 删状态与其注释（约 232-234 行）：

```tsx
  // 看板可以「收起」而不「关闭」：收起后轮询与状态照旧，只是不画面板，
  // 并在原处留一条可展开的横条。收起是纯 UI 选择，不进 store。
  const [boardCollapsed, setBoardCollapsed] = useState(false);
```

(c) 简化 `onOpenBoard`（约 552-566），删除两处 `setBoardCollapsed`：

```tsx
  /**
   * 打开看板只改 selectedBoard，不动 currentId。
   *
   * 主区域只有「会话页 / 看板页」两态（见 spec 2026-10-04）：「收起」已被删除，
   * 离开看板统一走「切到某条会话」（selectThread/newThread 清 selectedBoard）。
   */
  const onOpenBoard = (path: string) => {
    // 已经在该看板页：纯 no-op，不重读、不清屏。
    if (path === selectedBoard) return;
    // 换项目等于换问题：上一个项目的失败说法和卡片留在屏幕上只会误导。
    setBoardError(null);
    setBoardCards([]);
    setDoneExpanded(false); // 换项目 → 新看板从折叠开始
    setSelectedBoard(path);
  };
```

(d) 改主区域渲染（约 1257-1266）：删掉收起横条分支，条件去掉 `&& !boardCollapsed`：

```tsx
        <div className="relative flex min-w-0 flex-1 flex-col">
          {/* 看板是主区域的一个平级页面：选中才出现，问的是被选中那个项目。
              离开看板走「点某条会话」（selectThread/newThread 清 selectedBoard）；
              没有选中时主区域就是原来的对话。 */}
          {selectedBoard !== null ? (
```

(e) 删除传给 `SuperpowersKanbanSettings` 的 `onCollapse`（约 1281 行）：

```tsx
                onCollapse={() => setBoardCollapsed(true)}
```

- [ ] **Step 3: 改既有测试**

(a) 删除整条用例 `it("看板可收起、并可从收起横条再展开", …)`（`App.test.tsx:840-859`）——收起已不存在，「离开看板」由 Task 1 的用例覆盖。

(b) 改 `it("选中看板时主区域显示该项目看板，且不再有全局左列", …)`（约 827-838）：把

```tsx
    // 看板展开时提供收起按钮（收起 = 不画面板但保留看板本身）。
    expect(screen.getByLabelText("收起看板")).toBeTruthy();
```

改为固化「不再有收起」的新语义：

```tsx
    // 收起已删除：离开看板走「点会话」，不再有「收起看板」按钮。
    expect(screen.queryByLabelText("收起看板")).toBeNull();
```

(c) 其余对 `收起看板` 的 `queryByLabelText(...) === null` 断言（`:811`、`:1006` 一带）**保留**，仍成立。

- [ ] **Step 4: 跑测试，确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx`

Expected: PASS；`SuperpowersKanbanCollapsedBar.test.tsx` 已不在文件列表。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "refactor(desktop): drop the board collapse state and collapsed bar"
```

（`git rm` 的两个删除已在暂存区，一并提交。）

---

### Task 3: 删 `SuperpowersKanbanSettings` 的 `onCollapse`

**Files:**
- Modify: `desktop/src/components/SuperpowersKanbanSettings.tsx`
- Test: `desktop/src/components/SuperpowersKanbanSettings.test.tsx`

**Interfaces:**
- Consumes: 无（Task 2 已不再传 `onCollapse`）。
- Produces: `SuperpowersKanbanSettings` props = `{ switchOn, source, onToggle, watchmanEnabled, onToggleWatchman, watchmanWarning? }`。

- [ ] **Step 1: 删测试里的两条 `onCollapse` 用例**

删除 `SuperpowersKanbanSettings.test.tsx` 中的：

- `it("renders the collapse affordance only when a handler is supplied", …)`（约 83-109）
- `it("reports the collapse request once per click", …)`（约 111-125）

其余（开关状态/来源、toggle 回调、值守开关、warning）保留。

- [ ] **Step 2: 改组件**

`SuperpowersKanbanSettings.tsx`：删 props 里的 `onCollapse` 与「收起」按钮。

删解构项：

```tsx
  onCollapse,
```

删类型项：

```tsx
  /** Supplied by a host that can collapse the panel; omitted, no button shows. */
  onCollapse?: () => void;
```

删标题行右侧按钮（约 39-48）：

```tsx
        {onCollapse && (
          <button
            type="button"
            aria-label="收起看板"
            className="text-xs text-fg-subtle hover:text-fg-muted"
            onClick={onCollapse}
          >
            收起
          </button>
        )}
```

标题行的 `justify-between` 可保留（只有 label 时无副作用），不引入无关改动。

- [ ] **Step 3: 跑测试，确认通过**

Run: `cd desktop && npx vitest run src/components/SuperpowersKanbanSettings.test.tsx`

Expected: PASS。

- [ ] **Step 4: 全量回归 + 类型 + 构建**

```bash
cd desktop && npx vitest run
cd desktop && npx tsc --noEmit
cd desktop && npm run build
```

Expected: 全部通过。`tsc` 必须无「`boardCollapsed` / `SuperpowersKanbanCollapsedBar` / `onCollapse` 未使用或找不到」类报错——那是残留引用的信号。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/SuperpowersKanbanSettings.tsx \
        desktop/src/components/SuperpowersKanbanSettings.test.tsx
git commit -m "refactor(desktop): remove the now-unused onCollapse from board settings"
```

---

## 覆盖自检（Spec ↔ Task）

| Spec 要求 | 落点 |
| --- | --- |
| §2.1 选中会话 → 离开看板 | Task 1（`selectThread`） |
| §2.2 新建会话 → 离开看板 | Task 1（`newThread`） |
| §2.3 进入看板 = 侧栏条目（保留） | 无改动（`onOpenBoard` 入口已在，Task 2 仅简化） |
| §4.1 `onOpenBoard` 简化 + 同项目 no-op | Task 2 Step 2(c) |
| §4.1 主区域条件去 `boardCollapsed` | Task 2 Step 2(d) |
| §4.1 删 `boardCollapsed` 状态/import/`onCollapse` 传参 | Task 2 Step 2(a)(b)(e) |
| §4.2 `SuperpowersKanbanSettings` 删 `onCollapse` | Task 3 |
| §4.3 删 CollapsedBar 组件与测试 | Task 2 Step 1 |
| §6 新增 3 条用例 | Task 1 Step 1 两条 + Task 1 用例「tz 后回看板」见下注 |
| §6 删/改收起用例 | Task 2 Step 3、Task 3 Step 1 |
| §6 回归 vitest/tsc/build | Task 3 Step 4 |

> 注：spec §6 第 3 条「从会话回看板」其实由既有用例 `it("切回同一个看板不会重读", …)` 与 `it("切换项目后看板问的是新项目", …)` 覆盖（侧栏条目仍是唯一入口，Task 2 后重跑即回归验证），故不新增重复用例；如执行时认为需要，可在 Task 2 Step 3 追加一条「切到会话后点侧栏看板条目 → 看板重新出现」。

## 类型/命名一致性自检

- Task 2 删除的 `setBoardCollapsed` 与 `boardCollapsed` 在 Task 1 中未被引用；Task 1 只新增 `setSelectedBoard(null)`。
- `SuperpowersKanbanSettings` 的 `onCollapse` 在 Task 2 Step 2(e) 之后无任何传参点，Task 3 才删 prop——两步各自可编译（optional prop 缺失合法）。
- `ThreadSidebar` 的 `boardCollapsed`（`Set<string>`）三处贯穿不改。
