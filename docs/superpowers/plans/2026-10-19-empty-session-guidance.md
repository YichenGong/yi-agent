# 空会话引导实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面端没有选中会话时不再显示空白主区域，而是给出一块可操作的空状态：新建会话、从最近目录一键进入、模型未配置时直接去配置。

**Architecture:** 纯前端改动，不新增 RPC。新增一个纯展示组件 `EmptySessionState`，由 `App.tsx` 在主区域的「无会话且无看板」分支渲染；最近目录与 onboarding 状态都复用既有 hook（`useSessionState.workspaces`、`useSettingsState.onboarding`），不新增持久化。

**Tech Stack:** React 18 + TypeScript + Vitest + Testing Library（`desktop/`）。

## Global Constraints

- 不得在 `main` 分支上提交；所有改动在 `.worktrees/<branch>` 内完成。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。
- `cd desktop && npx tsc --noEmit` 必须无错；`npx vitest run` 必须全绿。
- 不新增 RPC，不改服务端：`workspace/list`、`workspace/remove`、`thread/start` 已具备全部能力。
- 不自动创建会话：用户没点之前不产生空会话（sepcc §目标 5）。
- 不改 `ChatView` 的空列表行为（spec §5）。
- 组件测试一律 `/** @vitest-environment jsdom */` 开头，`afterEach(cleanup)`。

---

### Task 1: 建出纯展示组件 `EmptySessionState`

**Files:**
- Create: `desktop/src/components/EmptySessionState.tsx`
- Create: `desktop/src/components/EmptySessionState.test.tsx`

**Interfaces:**
- Consumes: `Workspace`（`desktop/src/lib/protocol.ts:151-157`）。
- Produces:

```ts
export type EmptySessionStateProps = {
  /** 最近使用的目录，最近的在前；已不存在的目录也会传入，由组件置灰。 */
  workspaces: Workspace[];
  /** 是否需要先配置模型（由 onboarding 状态推导）。 */
  needsModel: boolean;
  /** 原生选择器是否可用（远程客户端没有 Tauri dialog）。 */
  canBrowse: boolean;
  /** 无 cwd = 打开原生选择器。 */
  onCreate: (cwd?: string) => void;
  onOpenSettings: () => void;
  /** 从最近目录里移除（只是视图操作，不删数据）。 */
  onForget: (path: string) => void;
};
export function EmptySessionState(props: EmptySessionStateProps): JSX.Element;
```

- [ ] **Step 1: 写会失败的测试**

```tsx
/** @vitest-environment jsdom */
import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import { EmptySessionState } from "./EmptySessionState";

const ws = (path: string, extra: Partial<{ exists: boolean; linked_to: string | null }> = {}) => ({
  path,
  exists: extra.exists ?? true,
  linked_to: extra.linked_to ?? null,
});

function renderState(overrides: Partial<Parameters<typeof EmptySessionState>[0]> = {}) {
  const props = {
    workspaces: [],
    needsModel: false,
    canBrowse: true,
    onCreate: vi.fn(),
    onOpenSettings: vi.fn(),
    onForget: vi.fn(),
    ...overrides,
  };
  render(<EmptySessionState {...props} />);
  return props;
}

describe("EmptySessionState", () => {
  it("tells the user how to start and offers the native picker", () => {
    const props = renderState();
    expect(screen.getByRole("heading", { name: "开始一个新会话" })).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /新建会话/ }));
    expect(props.onCreate).toHaveBeenCalledWith(undefined);
  });

  it("lists recent directories in order and creates in the clicked one", () => {
    const props = renderState({
      workspaces: [ws("/a/one"), ws("/b/two"), ws("/c/three")],
    });
    const items = screen.getAllByRole("button", { name: /在 .* 中新建会话/ });
    // 只列最近 6 个；顺序按传入顺序。
    expect(items.map((el) => el.getAttribute("data-path"))).toEqual([
      "/a/one",
      "/b/two",
      "/c/three",
    ]);
    fireEvent.click(items[1]);
    expect(props.onCreate).toHaveBeenCalledWith("/b/two");
  });

  it("caps the recent list at six entries", () => {
    renderState({
      workspaces: Array.from({ length: 8 }, (_, i) => ws(`/p/${i}`)),
    });
    expect(screen.getAllByRole("button", { name: /中新建会话/ })).toHaveLength(6);
  });

  it("disables a directory that no longer exists and labels it", () => {
    renderState({ workspaces: [ws("/gone", { exists: false })] });
    const button = screen.getByRole("button", { name: /在 gone 中新建会话/ }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(screen.getByText("目录不存在")).toBeTruthy();
  });

  it("hides linked roots so the same project is not listed twice", () => {
    renderState({
      workspaces: [ws("/main"), ws("/sub", { linked_to: "/main" })],
    });
    expect(screen.queryByText("sub")).toBeNull();
    expect(screen.getByText("main")).toBeTruthy();
  });

  it("forgets a recent directory without creating anything", () => {
    const props = renderState({ workspaces: [ws("/a/one")] });
    fireEvent.click(screen.getByRole("button", { name: "从最近项目移除 one" }));
    expect(props.onForget).toHaveBeenCalledWith("/a/one");
    expect(props.onCreate).not.toHaveBeenCalled();
  });

  it("points at settings when no model is configured", () => {
    const props = renderState({ needsModel: true });
    fireEvent.click(screen.getByRole("button", { name: "去配置" }));
    expect(props.onOpenSettings).toHaveBeenCalled();
  });

  it("omits the settings nudge when a model is configured", () => {
    renderState({ needsModel: false });
    expect(screen.queryByRole("button", { name: "去配置" })).toBeNull();
  });

  it("hides the native picker when browse is unavailable", () => {
    renderState({ canBrowse: false, workspaces: [ws("/a/one")] });
    expect(screen.queryByRole("button", { name: /新建会话…/ })).toBeNull();
    // 最近目录入口仍在。
    expect(screen.getByRole("button", { name: /在 one 中新建会话/ })).toBeTruthy();
  });

  it("renders no recent block at all when the list is empty", () => {
    renderState({ workspaces: [] });
    expect(screen.queryByRole("heading", { name: "最近项目" })).toBeNull();
  });
});
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/components/EmptySessionState.test.tsx`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 实现组件**

```tsx
import type { Workspace } from "../lib/protocol";

export type EmptySessionStateProps = {
  workspaces: Workspace[];
  needsModel: boolean;
  canBrowse: boolean;
  onCreate: (cwd?: string) => void;
  onOpenSettings: () => void;
  onForget: (path: string) => void;
};

/** 与侧栏分组同一口径：`linked_to` 非空的是附属根，不在这里重复出现。 */
function topLevel(workspaces: Workspace[]): Workspace[] {
  return workspaces.filter((w) => !w.linked_to);
}

function basename(path: string): string {
  const parts = path.split("/").filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

/**
 * 首屏空状态：没有选中会话时替代空白主区域。
 *
 * 纯展示：所有动作都由 `App` 注入。这里不读 RPC、不读 localStorage，
 * 因此可以在测试里逐条断言按钮的可点性。
 */
export function EmptySessionState({
  workspaces,
  needsModel,
  canBrowse,
  onCreate,
  onOpenSettings,
  onForget,
}: EmptySessionStateProps) {
  const recent = topLevel(workspaces).slice(0, 6);

  return (
    <div className="flex min-h-0 flex-1 items-center justify-center overflow-y-auto px-6 py-10">
      <div className="w-full max-w-xl">
        <h1 className="text-lg font-medium text-fg">开始一个新会话</h1>
        <p className="mt-1 text-sm text-fg-muted">
          选择一个项目目录，或从最近使用里挑一个。
        </p>

        {needsModel && (
          <div
            role="status"
            className="mt-4 flex items-center justify-between gap-3 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-xs text-amber-400"
          >
            <span>还没有配置模型，新建会话无法开始对话。</span>
            <button
              type="button"
              onClick={onOpenSettings}
              className="shrink-0 rounded border border-amber-500/50 px-2 py-0.5 hover:bg-amber-500/20"
            >
              去配置
            </button>
          </div>
        )}

        {canBrowse ? (
          <button
            type="button"
            onClick={() => onCreate(undefined)}
            className="mt-4 rounded-md border border-line-strong px-3 py-1.5 text-sm text-fg hover:bg-surface"
          >
            新建会话…
          </button>
        ) : (
          <p className="mt-4 text-xs text-fg-subtle">
            当前客户端不能打开本机目录选择器，请从下面的最近项目进入。
          </p>
        )}

        {recent.length > 0 && (
          <section className="mt-6">
            <h2 className="text-xs font-medium uppercase tracking-wide text-fg-subtle">
              最近项目
            </h2>
            <ul className="mt-2 divide-y divide-line rounded-md border border-line">
              {recent.map((workspace) => (
                <li key={workspace.path} className="flex items-center gap-2 px-3 py-2">
                  <button
                    type="button"
                    data-path={workspace.path}
                    aria-label={`在 ${basename(workspace.path)} 中新建会话`}
                    title={workspace.path}
                    disabled={!workspace.exists}
                    onClick={() => onCreate(workspace.path)}
                    className="min-w-0 flex-1 text-left text-sm text-fg disabled:cursor-not-allowed disabled:opacity-40"
                  >
                    <span className="block truncate">{basename(workspace.path)}</span>
                    <span className="block truncate text-xs text-fg-subtle">
                      {workspace.path}
                    </span>
                  </button>
                  {!workspace.exists && (
                    <span className="shrink-0 text-xs text-amber-400">目录不存在</span>
                  )}
                  <button
                    type="button"
                    aria-label={`从最近项目移除 ${basename(workspace.path)}`}
                    onClick={() => onForget(workspace.path)}
                    className="shrink-0 rounded px-1.5 py-0.5 text-xs text-fg-subtle hover:text-fg"
                  >
                    移除
                  </button>
                </li>
              ))}
            </ul>
          </section>
        )}
      </div>
    </div>
  );
}
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/components/EmptySessionState.test.tsx`
Expected: PASS（10 个用例）。

- [ ] **Step 5: 提交**

```bash
cd desktop && npx tsc --noEmit
git add src/components/EmptySessionState.tsx src/components/EmptySessionState.test.tsx
git commit -m "feat(desktop): 空会话状态组件"
```

---

### Task 2: 把空状态接进 `App.tsx` 主区域

**Files:**
- Modify: `desktop/src/App.tsx:2228-2240`（主区域渲染）
- Modify: `desktop/src/App.tsx`（新增 `onOpenModelSettings` 与 `canBrowse`）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 1 的 `EmptySessionState`；`sess.workspaces`、`prefs.onboarding.needed`、`onNew`、`onBrowse`、`removeWorkspace`。
- Produces: 无（UI 行为）。

**判定条件（全部满足才渲染空状态）:**

```ts
const showEmptyState =
  sess.currentId === null && board.selectedBoard === null;
```

`board.selectedBoard !== null` 时走现有的看板臂，不渲染空状态（spec §1）。恢复/加载中的保护由「`currentId` 有值时才进入会话臂」天然满足：恢复中 `currentId` 已置位，不会闪空状态。

- [ ] **Step 1: 写会失败的测试**

在 `App.test.tsx` 里加（沿用该文件既有的 harness 构造方式；若无现成 harness，则用 `render(<App .../>)` 与既有的 mock RPC 工厂）：

```tsx
it("shows the empty state instead of a blank main area on a fresh start", async () => {
  const { container } = await renderApp(); // 既有 harness：初始化完成、无会话
  expect(await screen.findByRole("heading", { name: "开始一个新会话" })).toBeTruthy();
  // 禁用输入框会让人以为输入坏了：无会话时整块不渲染。
  expect(container.querySelector("textarea")).toBeNull();
});

it("does not show the empty state while a board is open", async () => {
  await renderApp({ openBoard: "/p/board" });
  expect(screen.queryByRole("heading", { name: "开始一个新会话" })).toBeNull();
});
```

（若 `renderApp` 不支持 `openBoard` 选项，改为在既有看板测试的 harness 上追加这两条断言，不要为此新建一套 mock。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "empty state"`
Expected: FAIL（找不到标题）。

- [ ] **Step 3: 加渲染分支**

把主区域那段 `<> <ChatView/> <MessageInput/> </>` 外面包一层三臂条件：

```tsx
) : sess.currentId === null && board.selectedBoard === null ? (
  <EmptySessionState
    workspaces={sess.workspaces}
    needsModel={prefs.onboarding.needed}
    canBrowse={!isRemoteClient()}
    onCreate={(cwd) => void onNew(cwd)}
    onOpenSettings={onOpenModelSettings}
    onForget={(path) => void removeWorkspace(path)}
  />
) : (
  <>
    {/* 现有 ChatView + MessageInput，逐字不动 */}
  </>
)}
```

`onOpenModelSettings` 的实现：`() => prefs.setSettingsOpen(true)`。

注意：`SettingsDialog` 的 Tab 选择是**组件内部状态**（`SettingsDialog.tsx:108` 的 `useState<TabId>("general")`），props 里没有 `initialTab`，因此"直接跳到模型页"目前无法从外部指定。本计划**不新增**这个接缝（不在本设计的范围内，且会给面板加一个只为一个按钮服务的 API）：点「去配置」打开设置面板，用户自己点「模型」Tab。

如果实施时认为必须直达模型页，最小改法是给 `SettingsDialog` 加一个可选 `initialTab?: TabId`，在 `open` 从 `false → true` 时把 `active` 设为它（一个 `useEffect` 即可），并在 `App.test.tsx` 里断言 `settings-tab-models` 被选中。这属于可选增强，不是本设计的验收项。

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "empty state"`
Expected: PASS

- [ ] **Step 5: 跑既有 App 全量测试确认无回归**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS。**注意**：既有的「无会话」断言若依赖 `textarea` 存在，会开始失败——这正是 spec 要求的行为变更（不渲染 disabled 输入框），按新语义更新那些断言，不要为了保住旧断言而回退。

- [ ] **Step 6: 提交**

```bash
cd desktop && npx tsc --noEmit
git add src/App.tsx src/App.test.tsx
git commit -m "feat(desktop): 无会话时渲染空状态引导"
```

---

### Task 3: 刷新时序与边界收口

**Files:**
- Modify: `desktop/src/App.tsx`（回到无会话状态时刷新最近目录）
- Test: `desktop/src/components/EmptySessionState.test.tsx`（已覆盖） / `desktop/src/App.test.tsx`

**为什么需要:** `sess.refreshWorkspaces()` 目前只在初始化、`onBrowse`、`onNew`、`removeWorkspace` 之后调用（`App.tsx:883-917`、`1420` 附近）。用户从会话回到"无会话"时（例如新建失败、或关闭最后一个会话），列表可能停留在旧顺序。

- [ ] **Step 1: 写会失败的测试**

```tsx
it("refreshes recent directories when returning to the empty state", async () => {
  const { call } = await renderApp();
  // 先进入一个会话，再回到无会话状态。
  await openSomeThread();
  await returnToEmptyState(); // 用 App 现有的「回到首页/新建」入口
  expect(callsTo(call, "workspace/list").length).toBeGreaterThan(1);
});
```

若当前没有"回到无会话"的用户入口，本任务**只做** Step 2 的防御性刷新（在 `currentId` 变为 `null` 的 effect 里），并把该测试标记为 `it.todo` 并在提交信息里说明——不要为了造入口而新增 UI（spec 非目标）。

- [ ] **Step 2: 实现**

在 `App.tsx` 已有的 effect 区加：

```tsx
// 回到无会话状态时重读最近目录：索引的排序由服务端维护，客户端只镜像。
useEffect(() => {
  if (sess.currentId === null) void sess.refreshWorkspaces();
}, [sess.currentId]);
```

注意依赖只放 `sess.currentId`：`refreshWorkspaces` 每次都新建函数，放进来会每次渲染都触发。

- [ ] **Step 3: 运行测试**

Run: `cd desktop && npx vitest run src/App.test.tsx src/components/EmptySessionState.test.tsx`
Expected: PASS

- [ ] **Step 4: 提交**

```bash
cd desktop && npx tsc --noEmit
git add src/App.tsx src/App.test.tsx
git commit -m "feat(desktop): 回到空会话状态时刷新最近目录"
```

---

### Task 4: 全量验证与文档登记

**Files:**
- Modify: `docs/project-management/desktop.md`

- [ ] **Step 1: 跑前端全量**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

Expected: 全绿、无类型错误。

- [ ] **Step 2: 登记进度**

在 `docs/project-management/desktop.md` 里追加：

```markdown
- [x] 空会话引导（首屏不空白 + 最近项目快速入口）— 无选中会话且未打开看板时渲染 `EmptySessionState`（`desktop/src/components/EmptySessionState.tsx`），提供「新建会话…」、最近 6 个目录一键新建、失效目录置灰、模型未配置时「去配置」；`MessageInput` 在无会话时整块不渲染。判据：`cd desktop && npx vitest run src/components/EmptySessionState.test.tsx src/App.test.tsx` — [设计](../superpowers/specs/2026-10-19-empty-session-guidance-design.md)
```

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md
git commit -m "docs(project-management): 登记空会话引导完成"
```

---

## 自检记录

**Spec 覆盖检查：**
- 设计 §1（判定条件 + 三臂）→ Task 2。
- 设计 §2（组件 props 与内容结构）→ Task 1。设计写的是 `onOpenSettings`（打开设置里的「模型」Tab）；`SettingsDialog` 目前没有外部指定 Tab 的 props，Task 2 降级为"打开设置面板"，并把直达 Tab 列为可选增强（不新增 RPC，也不阻塞验收）。
- 设计 §3（数据复用与刷新点）→ Task 3。
- 设计 §4（`needsModel` 用 `onboarding.needed`，不叠加 dismissed）→ Task 2；`App.tsx:1406` 已经在写入前算入 `!status.dismissed`，所以这里直接用 `needed` 是正确的。
- 设计「错误处理与边界」6 行 → Task 1 的 10 个用例逐条覆盖（远程无 picker、全失效、linked_to、空列表）。
- 设计「测试与验收」7 条 → Task 1–4。

**占位符扫描：** 无 TBD/TODO。Task 2 的 `onOpenModelSettings` 与 Task 3 的「回到无会话」入口依赖既有代码；前者给出实现约束（不新增 RPC），后者允许在入口不存在时退化为 `it.todo` 并说明，这不影响主目标。

**类型一致性：** `EmptySessionStateProps`（Task 1）在 Task 2 按同名字段传参；`Workspace` 取自 `protocol.ts:151`，与 `workspace/list` 的返回一致。

**已知风险：**
- `App.test.tsx` 体量大，`-t` 过滤时注意它的 harness 可能共享状态；优先单跑整个文件确认基线。
- `canBrowse` 用 `isRemoteClient()` 推导，与 `pickDirectory` 内部返回 `null` 的判断同源（`App.tsx:857`），不会出现"按钮可用但点了没反应"。
- 空状态不渲染 `MessageInput` 会改变既有布局假设（例如某些测试靠 `textarea` 定位）；这是 spec 的显式要求，按新语义改断言。
