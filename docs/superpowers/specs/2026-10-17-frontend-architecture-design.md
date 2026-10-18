# 设计：前端架构收敛（App.tsx 拆分、UI 原语、尺寸去重）

日期：2026-10-17
状态：待评审
关系：来自 2026-10-17 冗余审计的 P3（前端架构）与 P2-4 的延续。
前置：`deadcode-and-frontend-dedup` 卡会改 `lib/` 下的宽度/rpc 模块——**本卡不碰那
四个文件**（`panelWidth.ts` / `sidebarWidth.ts` / `rpcCall.ts` / `routes.ts`），
但会接手 `App.tsx`（那张卡刻意不改它）。

## 1. 背景与问题

### 1.1 `App.tsx` 是单一巨型组件

| 指标 | 实测值 |
|---|---|
| 行数 | **2,483** |
| `useState` | **41** |
| `useRef` | **29** |
| 顶层 `return (` JSX | 6 处（行 118…2058；主渲染从 **2058** 起） |
| 该文件内 RPC 路由字面量 | **52** 处 |

41 个 `useState` 覆盖了**看板、会话、进程、diff、设置、配对、onboarding** 七个
互不相干的关注点（`boardOn`/`boardCards`/`boardSource`/`boardError`/…、
`processes`/`openProcessId`/`processOutput`、`threadDiff`/`diffLoading`/`diffError`/
`activeCommit`/`diffBase`/`diffNote`、`settingsOpen`/`relayUrl`/`theme`…）。
任何一个关注点变化都会让 2,483 行的组件整体重渲。

### 1.2 没有 UI 原语层（`src/ui/` 不存在）

同类外观在各组件里手搓：

| 重复形态 | 实测处数 |
|---|---|
| `fixed inset-0` 遮罩（弹层/下拉/菜单） | **10** 处，分布在 6 个文件 |
| 各自实现的 Escape 关闭 | **7** 个源文件（`ApprovalDialog`/`MessageInput`/`ModeChip`/`ModelPicker`/`ProcessDetail`/`SettingsDialog`/`ThreadSidebar`） |
| 重复 ≥50 字符的 `className` 整串 | 最高一条重复 **10** 次：`"rounded border border-line bg-surface px-2 py-1 text-sm text-fg"` |

`Escape` 那 7 处**条件各不相同**（`if (!open) return` / `if (!newMenuOpen && menu === null) return` / 活跃元素守卫…），且**焦点归还目标不同**（`denyRef`/`triggerRef`/
"那个下拉的触发按钮"）——所以抽取必须保留每处的条件与回焦目标，不能一刀切。

### 1.3 `Modal` 语义已在多处重复实现

`SettingsDialog.tsx:115`、`ModeChip.tsx:157`、`OnboardingWizard.tsx:100`、
`PairingScreen.tsx:283` 各自写了 `fixed inset-0 z-50 flex items-center justify-center`
+ `role="dialog"` + `aria-modal="true"` + Escape 关闭 + 回焦。这是同一份语义的 4 份实现。

## 2. 目标与非目标

### 目标

1. 建 `src/ui/` 原语层：`Modal`（含遮罩/Escape/回焦/`role=dialog`）、
   `Popover`（锚定下拉）、`useEscapeKey` hook，并让既有组件迁入。
2. 按关注点把 `App.tsx` 的 41 个 `useState` 分到自定义 hook
   （`useBoardState`/`useProcessState`/`useDiffState`/`useSettingsState`…），
   App 只做组装；`App.tsx` 行数显著下降。
3. 用卡 1 产出的 `lib/routes.ts` 替换 App.tsx 里的 52 处路由字面量。

### 非目标

- **不改任何视觉呈现**：迁移必须是 DOM/类名等价的（仓库已有"桌面逐字节不变"
  的硬不变量，见 `SettingsDialog.tsx:161` 的注释）。
- 不引入 UI 框架/组件库依赖。
- 不改 `lib/panelWidth.ts` 等四个文件（属另一张卡）。
- 不做 App.test.tsx 的重写；迁移以既有测试全绿为验收。

## 3. 设计

### 3.1 `src/ui/` 原语

```tsx
// src/ui/useEscapeKey.ts
/**
 * 当 `active` 为真时，在 document 上监听 Escape 并调用 `onEscape`。
 *
 * 各调用点的条件与回焦行为不同，故条件由 `active` 表达、回焦由 `onEscape`
 * 自己负责（它通常调 `triggerRef.current?.focus()`）。
 */
export function useEscapeKey(active: boolean, onEscape: () => void): void;
```

```tsx
// src/ui/Modal.tsx
export interface ModalProps {
  open: boolean;
  onClose: () => void;
  /** 关闭后焦点交回的控件。 */
  restoreFocusTo?: React.RefObject<HTMLElement | null>;
  /** 遮罩类名。默认 `"fixed inset-0 z-50 flex items-center justify-center"`。 */
  overlayClassName?: string;
  /** 面板类名（各弹层外观不同，故由调用方给）。 */
  panelClassName?: string;
  labelledBy?: string;
  children: React.ReactNode;
}
```

`Modal` 内部：遮罩 + `role="dialog"` + `aria-modal="true"` + `useEscapeKey(open, onClose)`
+ 打开时回焦、关闭时还原。**迁入时 `overlayClassName`/`panelClassName` 逐字传入原
类名**，保证 DOM 不变。

```tsx
// src/ui/Popover.tsx
export interface PopoverProps {
  open: boolean;
  onClose: () => void;
  /** 用于"点外关闭"的透明遮罩（原实现的 `fixed inset-0 z-10`）。 */
  backdrop?: boolean;
  children: React.ReactNode;
}
```

`Popover` 覆盖 `ModelPicker`/`ModeChip`/`ThreadSidebar` 里那 5 处
`fixed inset-0 z-10` + `onClick={close}` 的下拉形态。

### 3.2 `App.tsx` 按关注点拆分

新建 `src/hooks/`，每个 hook 收一个关注点（**只搬 state 与 effect，不搬 JSX**，
以把回归面压到最小）：

```
src/hooks/useBoardState.ts      boardOn/boardSource/boardCards/boardPluginMissing/
                                boardError/selectedBoard/boards/boardSummaries/
                                doneExpanded/watchmanEnabled/watchmanWarning
src/hooks/useProcessState.ts    processes/openProcessId/processOutput
src/hooks/useDiffState.ts       threadDiff/diffLoading/diffError/activeCommit/
                                diffBase/diffNote
src/hooks/useSessionState.ts    currentId/groups/pinned/workspaces/traceRows/
                                detailStack/panelOpen/detailTab/sidebarOpen
src/hooks/useSettingsState.ts   settingsOpen/relayUrl/theme/onboarding/
                                onboardingNotice/pairingVersion/repairing/repairNotice
```

每个 hook 返回 `{ …state, …actions }`，`App` 里改为
`const board = useBoardState(rpc);` 等。**JSX 保持不动**，只把引用从
`boardOn` 改成 `board.boardOn`——这一步是纯机械替换，可用
`perl -pi -e 's/\bboardOn\b/board.boardOn/g'` 之类逐组推进，每组一次提交、跑一次测试。

> 判据：拆完后 `App.tsx` 的 `useState` 计数应降到个位数；每个 hook 的
> 依赖（`rpc`、`transport` 等）显式作为参数传入。

### 3.3 路由字面量替换

用卡 1 的 `lib/routes.ts`：把 App.tsx 的 52 处 `rpc("thread/list", …)` 改为
`rpc(RPC.threadList, …)`。**只改 App.tsx**（其余文件的字面量留作后续顺手替换）。

## 4. 测试策略

- **DOM 等价**：`src/ui/` 原语迁移后，相关组件的既有测试（`SettingsDialog.test.tsx`、
  `ModeChip.test.tsx`、`ModelPicker.test.tsx`、`ThreadSidebar.test.tsx`…）必须
  逐条通过——它们已断言 `role="dialog"`/Escape 行为/aria 属性。
- **原语单测**：新增 `useEscapeKey.test.ts`、`Modal.test.tsx`、`Popover.test.tsx`，
  覆盖"active=false 时不响应 Escape""关闭后焦点回到 `restoreFocusTo`"。
- **App 拆分**：`App.test.tsx`（含 1,226 行附近的看板断言）必须全绿且用例数不减。
- 基线：`tsc --noEmit` 无输出；`vitest run` **89 文件 / 1113 测试**全绿。

## 5. 验收标准

- `ls desktop/src/ui/` 存在且含 `Modal.tsx`/`Popover.tsx`/`useEscapeKey.ts`（+测试）。
- `grep -rc "fixed inset-0" desktop/src/**/*.tsx` 的**总处数**从 10 降至原语的 2-3 处
  （其余改为 `<Modal>`/`<Popover>`）。
- `grep -rc '"Escape"' desktop/src/components/*.tsx` 只剩 `useEscapeKey` 一处实现
  （`MessageInput` 的键盘处理是另一回事，可保留）。
- `grep -c "useState" desktop/src/App.tsx` 从 41 降到 < 12。
- `grep -c "" desktop/src/App.tsx` 显著下降（目标 < 1,500 行）。
- `tsc --noEmit` 无输出；`vitest run` 89 文件 / 1113 测试全绿（用例数不减）。
