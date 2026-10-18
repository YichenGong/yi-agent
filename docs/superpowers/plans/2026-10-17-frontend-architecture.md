# 前端架构收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建 `src/ui/` 原语层并让组件迁入；按关注点拆分 `App.tsx` 的 41 个 `useState`；用 `lib/routes.ts` 替换 App.tsx 的路由字面量。

**Architecture:** 先立原语（`useEscapeKey`/`Modal`/`Popover`）并逐组件迁入（DOM 等价）；再把 App.tsx 的状态按关注点搬进 `src/hooks/`，JSX 不动、只改引用路径；最后替换路由字面量。

**Tech Stack:** React + TypeScript + vitest + `@testing-library/react`（jsdom）。

**Spec:** `docs/superpowers/specs/2026-10-17-frontend-architecture-design.md`

## Global Constraints

- 环境：`export PATH="/opt/homebrew/bin:$PATH"`；用 `node_modules/.bin/tsc`、`node_modules/.bin/vitest`（在 `desktop/` 下）。
- 组件测试文件首行需 `/** @vitest-environment jsdom */`（仓库既有约定）。
- **不改视觉**：迁入原语时把原 `className` 逐字作为参数传入，保证 DOM 不变。
- **不改 `lib/panelWidth.ts` / `sidebarWidth.ts` / `rpcCall.ts` / `routes.ts`**
  （`deadcode-and-frontend-dedup` 卡负责）。
- **本卡依赖 `lib/routes.ts` 已存在**（Task 5 用）。若尚不存在，先跳过 Task 5。
- 基线：`tsc --noEmit` 无输出；`vitest run` **89 文件 / 1113 测试**全绿。

---

## File Structure

- Create: `desktop/src/ui/useEscapeKey.ts`、`Modal.tsx`、`Popover.tsx`（+ 各自 `.test.tsx`）。
- Create: `desktop/src/hooks/{useBoardState,useProcessState,useDiffState,useSessionState,useSettingsState}.ts`。
- Modify: `desktop/src/App.tsx`、`src/components/{SettingsDialog,ModeChip,ModelPicker,ThreadSidebar,OnboardingWizard,PairingScreen}.tsx`。

---

### Task 1: `useEscapeKey` 原语

**Files:**
- Create: `desktop/src/ui/useEscapeKey.ts`, `desktop/src/ui/useEscapeKey.test.tsx`

**Interfaces:**
- Produces: `export function useEscapeKey(active: boolean, onEscape: () => void): void`

- [ ] **Step 1: 记录基线**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -4
```

Expected: `tsc` 无输出；89 文件 / 1113 测试全绿。

- [ ] **Step 2: 写测试**

`desktop/src/ui/useEscapeKey.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { renderHook } from "@testing-library/react";
import { fireEvent } from "@testing-library/react";
import { useEscapeKey } from "./useEscapeKey";

afterEach(() => vi.restoreAllMocks());

describe("useEscapeKey", () => {
  it("calls onEscape when active", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(true, onEscape));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onEscape).toHaveBeenCalledTimes(1);
  });

  it("ignores Escape when inactive", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(false, onEscape));
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onEscape).not.toHaveBeenCalled();
  });

  it("ignores other keys", () => {
    const onEscape = vi.fn();
    renderHook(() => useEscapeKey(true, onEscape));
    fireEvent.keyDown(document, { key: "Enter" });
    expect(onEscape).not.toHaveBeenCalled();
  });
});
```

- [ ] **Step 3: 运行确认失败**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/ui/useEscapeKey.test.tsx 2>&1 | tail -6`

Expected: 失败（模块不存在）。

- [ ] **Step 4: 实现**

`desktop/src/ui/useEscapeKey.ts`：

```ts
import { useEffect } from "react";

/**
 * 当 `active` 为真时，在 document 上监听 Escape 并调用 `onEscape`。
 *
 * 各调用点原先各写一份监听，但它们的**条件**与**回焦目标**都不同：条件由
 * `active` 表达；回焦由 `onEscape` 自己负责（它通常会把焦点交回触发按钮）。
 */
export function useEscapeKey(active: boolean, onEscape: () => void): void {
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onEscape();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [active, onEscape]);
}
```

- [ ] **Step 5: 运行确认通过**

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/ui/useEscapeKey.test.tsx --reporter=dot 2>&1 | tail -5`

Expected: 3 项通过。

- [ ] **Step 6: commit**

```bash
git add desktop/src/ui/useEscapeKey.ts desktop/src/ui/useEscapeKey.test.tsx
git commit -m "feat(desktop): add the useEscapeKey primitive"
```

---

### Task 2: `Modal` 与 `Popover` 原语

**Files:**
- Create: `desktop/src/ui/Modal.tsx`, `desktop/src/ui/Modal.test.tsx`, `desktop/src/ui/Popover.tsx`

**Interfaces:**
- Consumes: `useEscapeKey`（Task 1）。
- Produces: `<Modal>`、`<Popover>`。

- [ ] **Step 1: 写 `Modal.tsx`（已实测）**

```tsx
import { useEffect, useRef, type ReactNode, type RefObject } from "react";
import { useEscapeKey } from "./useEscapeKey";

export interface ModalProps {
  open: boolean;
  onClose: () => void;
  /** 关闭后焦点交回的控件。 */
  restoreFocusTo?: RefObject<HTMLElement | null>;
  /** 遮罩类名（各弹层外观不同，故由调用方传入原类名，保证 DOM 不变）。 */
  overlayClassName?: string;
  /** 面板类名。 */
  panelClassName?: string;
  labelledBy?: string;
  children: ReactNode;
}

const DEFAULT_OVERLAY = "fixed inset-0 z-50 flex items-center justify-center";

/**
 * 弹层外壳：遮罩 + `role="dialog"` + Escape 关闭 + 焦点还原。
 *
 * 这几样原先在 `SettingsDialog`/`ModeChip`/`OnboardingWizard`/`PairingScreen`
 * 各写了一遍。
 */
export function Modal({
  open,
  onClose,
  restoreFocusTo,
  overlayClassName = DEFAULT_OVERLAY,
  panelClassName,
  labelledBy,
  children,
}: ModalProps) {
  useEscapeKey(open, onClose);

  // 只在「曾经打开过、现在关上了」时回焦：初次挂载（open=false）不得抢焦点。
  const wasOpen = useRef(false);
  useEffect(() => {
    if (open) {
      wasOpen.current = true;
      return;
    }
    if (wasOpen.current) {
      wasOpen.current = false;
      restoreFocusTo?.current?.focus();
    }
  }, [open, restoreFocusTo]);

  if (!open) return null;
  return (
    <div className={overlayClassName}>
      <div role="dialog" aria-modal="true" aria-labelledby={labelledBy} className={panelClassName}>
        {children}
      </div>
    </div>
  );
}
```

- [ ] **Step 2: 写 `Popover.tsx`（已实测编译）**

```tsx
import type { ReactNode } from "react";
import { useEscapeKey } from "./useEscapeKey";

export interface PopoverProps {
  open: boolean;
  onClose: () => void;
  /**
   * 是否渲染"点外关闭"的透明遮罩。
   *
   * 原实现里锚定下拉用 `fixed inset-0 z-10` + `onClick={close}`；有的下拉
   * （如 `MessageInput` 的 slash 弹窗）不用遮罩，故可关。
   */
  backdrop?: boolean;
  backdropClassName?: string;
  children: ReactNode;
}

/**
 * 锚定下拉/菜单外壳：可选透明遮罩 + Escape 关闭。
 *
 * 覆盖原先散在 `ModelPicker`/`ModeChip`/`ThreadSidebar` 的
 * `fixed inset-0 z-10` 下拉形态。
 */
export function Popover({
  open,
  onClose,
  backdrop = true,
  backdropClassName = "fixed inset-0 z-10",
  children,
}: PopoverProps) {
  useEscapeKey(open, onClose);
  if (!open) return null;
  return (
    <>
      {backdrop ? <div className={backdropClassName} onClick={onClose} /> : null}
      {children}
    </>
  );
}
```

- [ ] **Step 3: 写 `Modal.test.tsx`（已实测 7 项全绿）**

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { useRef } from "react";
import { Modal } from "./Modal";

afterEach(cleanup);

function Harness({ open, onClose }: { open: boolean; onClose: () => void }) {
  const trigger = useRef<HTMLButtonElement>(null);
  return (
    <>
      <button ref={trigger} type="button">
        open
      </button>
      <Modal open={open} onClose={onClose} restoreFocusTo={trigger} labelledBy="h">
        <h2 id="h">title</h2>
      </Modal>
    </>
  );
}

describe("Modal", () => {
  it("renders a dialog with the given label when open", () => {
    render(<Harness open onClose={() => {}} />);
    expect(screen.getByRole("dialog")).toBeDefined();
    expect(screen.getByRole("dialog").getAttribute("aria-modal")).toBe("true");
  });

  it("renders nothing when closed", () => {
    render(<Harness open={false} onClose={() => {}} />);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(<Harness open onClose={onClose} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it("restores focus to the trigger once closed", () => {
    const { rerender } = render(<Harness open onClose={() => {}} />);
    const trigger = screen.getByRole("button", { name: "open" });
    rerender(<Harness open={false} onClose={() => {}} />);
    expect(document.activeElement).toBe(trigger);
  });
});
```

- [ ] **Step 4: 验证**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run src/ui --reporter=dot 2>&1 | tail -5
```

Expected: `tsc` 无输出；`src/ui` 2 文件 / 7 测试全绿。**已实测此结果。**

- [ ] **Step 5: commit**

```bash
git add desktop/src/ui/Modal.tsx desktop/src/ui/Modal.test.tsx desktop/src/ui/Popover.tsx
git commit -m "feat(desktop): add Modal and Popover primitives"
```

---

### Task 3: 组件迁入原语（逐组件、DOM 等价）

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`、`ModeChip.tsx`、`ModelPicker.tsx`、`ThreadSidebar.tsx`

**Interfaces:**
- Consumes: `<Modal>`、`<Popover>`、`useEscapeKey`。

> **原则**：每次只迁一个组件，迁完立刻跑该组件的既有测试。类名逐字搬入参数，
> DOM 结构必须不变。

- [ ] **Step 1: `SettingsDialog` 迁入 `Modal`**

`SettingsDialog.tsx:87-94` 手写的 Escape effect 删掉，`:115` 的外壳改为：

```tsx
      <Modal
        open={open}
        onClose={onClose}
        overlayClassName="fixed inset-0 z-50 flex items-center justify-center"
        panelClassName="…"   {/* 逐字搬移原面板类名 */}
      >
        {/* 原有子内容不动 */}
      </Modal>
```

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/components/SettingsDialog.test.tsx --reporter=dot 2>&1 | tail -4`

Expected: 全绿（该测试已断言 Escape 与 `role="dialog"`）。

- [ ] **Step 2: `ModeChip` 的确认弹层迁入 `Modal`**

`ModeChip.tsx:157` 的 `fixed inset-0 z-50 … bg-black/60 backdrop-blur-sm` 外壳改为
`<Modal overlayClassName="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm" panelClassName="w-full max-w-md rounded-lg border border-line-strong bg-panel p-4 shadow-xl" onClose={close} labelledBy={headingId}>`。
`:42-48` 的手写 Escape effect 里若只服务该弹层则删除；**`close()` 里的
`triggerRef.current?.focus()` 保留**（那是该组件自己的回焦语义）。

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/components/ModeChip.test.tsx --reporter=dot 2>&1 | tail -4`

Expected: 全绿。

- [ ] **Step 3: `ModelPicker` 与 `ThreadSidebar` 的下拉迁入 `Popover`**

`ModelPicker.tsx:221`、`:268` 与 `ThreadSidebar.tsx:514`、`:688`、`:814` 的
`fixed inset-0 z-10` + `onClick` 改为 `<Popover open={…} onClose={…}>`。**注意**
`ModelPicker` 的 Escape 需要把焦点交回**那个**下拉的触发按钮（见其 `:137-138` 注释），
故 `onClose` 里保留原有的 `focus()` 调用；`ThreadSidebar` 的 `closeMenus` 同理。

Run: `cd desktop && export PATH="/opt/homebrew/bin:$PATH" && node_modules/.bin/vitest run src/components/ModelPicker.test.tsx src/components/ThreadSidebar.test.tsx --reporter=dot 2>&1 | tail -4`

Expected: 全绿。

- [ ] **Step 4: 确认遮罩处数下降**

```bash
cd desktop && grep -rn "fixed inset-0" src --include=*.tsx | grep -v "src/ui/" | wc -l
```

Expected: 从基线 10 降到 ≤ 3（其余走原语参数）。

- [ ] **Step 5: 全量验证 + commit**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -4
cd .. && git add desktop/src/components desktop/src/App.tsx
git commit -m "refactor(desktop): move dialogs and popovers onto the ui primitives"
```

Expected: `tsc` 无输出；测试全绿、用例数不减（1113）。

---

### Task 4: `App.tsx` 按关注点拆 hook

**Files:**
- Create: `desktop/src/hooks/useBoardState.ts`、`useProcessState.ts`、`useDiffState.ts`、`useSessionState.ts`、`useSettingsState.ts`
- Modify: `desktop/src/App.tsx`

**Interfaces:**
- Produces: 每个 hook 返回 `{ …state, …actions }`。

- [ ] **Step 1: 记录基线**

```bash
cd desktop
grep -c "useState" src/App.tsx
grep -c "useRef" src/App.tsx
grep -c "" src/App.tsx
```

Expected: 41 / 29 / 2483。

- [ ] **Step 2: 逐关注点搬移（每个 hook 一次提交）**

顺序按"最内聚、依赖最少"：`useDiffState` → `useProcessState` → `useBoardState` →
`useSessionState` → `useSettingsState`。

每个 hook 的做法：

1. 把该关注点的 `useState`/`useRef`/`useEffect` **整块剪切**进 `src/hooks/<name>.ts`。
2. 依赖（`rpc`、`transport`、`currentId` 等）作为 hook 参数显式传入。
3. hook 返回 `{ …state, …actions }`。
4. `App.tsx` 里 `const diff = useDiffState(rpc, currentId);`，并把 JSX 里的引用由
   `threadDiff` 改为 `diff.threadDiff`（纯机械替换）：

   ```bash
   perl -pi -e 's/\bthreadDiff\b/diff.threadDiff/g; s/\bdiffLoading\b/diff.diffLoading/g; s/\bdiffError\b/diff.diffError/g' src/App.tsx
   ```

   > 逐个 state 名替换前先用 `grep -c "\b<name>\b" src/App.tsx` 量清出现次数，
   > 避免误伤同名字符串或局部变量。

5. `node_modules/.bin/vitest run src/App.test.tsx --reporter=dot` 必须全绿。
6. commit：`refactor(desktop): extract use<X>State from App`。

- [ ] **Step 3: 收尾验证**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
grep -c "useState" src/App.tsx; grep -c "" src/App.tsx
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run --reporter=dot 2>&1 | tail -4
```

Expected: `useState` < 12；行数显著下降；`tsc` 无输出；测试全绿、用例数不减。

- [ ] **Step 4: commit**

```bash
cd .. && git add desktop/src/hooks desktop/src/App.tsx
git commit -m "refactor(desktop): extract per-concern state hooks from App"
```

---

### Task 5: 路由字面量替换（依赖 `lib/routes.ts`）

**Files:**
- Modify: `desktop/src/App.tsx`

**Interfaces:**
- Consumes: `desktop/src/lib/routes.ts`（由 `deadcode-and-frontend-dedup` 卡创建）。

- [ ] **Step 1: 前置检查**

```bash
cd desktop && ls src/lib/routes.ts 2>/dev/null || echo "SKIP: routes.ts not yet present"
```

Expected: 若不存在则**跳过本 Task**（它由另一张卡产出，看板合并后再做）。

- [ ] **Step 2: 量清字面量**

```bash
cd desktop && grep -oE '"[a-z]+/[A-Za-z]+"' src/App.tsx | sort | uniq -c | sort -rn
```

Expected: 约 52 处（基线）。

- [ ] **Step 3: 逐名替换**

对每个落在 `RPC` 表里的方法名，用 `perl` 替换（**值不变，只换写法**）：

```bash
perl -pi -e 's/"thread\/list"/RPC.threadList/g' src/App.tsx
```

替换后确认导入：`import { RPC } from "./lib/routes";`

- [ ] **Step 4: 验证 + commit**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH"
node_modules/.bin/tsc --noEmit 2>&1 | tail -3
node_modules/.bin/vitest run src/App.test.tsx --reporter=dot 2>&1 | tail -4
grep -cE '"[a-z]+/[A-Za-z]+"' src/App.tsx
cd .. && git add desktop/src/App.tsx
git commit -m "refactor(desktop): use the RPC route table in App"
```

Expected: `tsc` 无输出；测试全绿；字面量计数降到 0（或仅剩不在表内的）。
