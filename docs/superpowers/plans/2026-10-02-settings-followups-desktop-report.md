# 桌面设置对话框两项遗留问题修复报告

日期：2026-10-02
分支：`fix/settings-followups`（基于 main）
工作树：`.worktrees/settings-followups`

工具链：`export PATH=/opt/homebrew/bin:$PATH`，命令在 `desktop/` 下执行
（`npx tsc --noEmit`、`npx vitest run`）。`node_modules` 已存在，未重装。

## 结论速览

- `npx tsc --noEmit`：退出码 0（改动前、改动后均为 0）。
- 全量 `npx vitest run`：改动前 **439 passed / 45 files**，改动后 **444 passed / 45 files**（+5 条新测试）。
- 提交：
  - `667d308` fix(desktop): make the composer's focus ring visible and theme-aware
  - `13ace30` fix(desktop): give the settings dialog focus management and ARIA tab wiring
- 未合并、未触碰 main。
- 全部改动仅落在允许的四个文件内：
  `desktop/src/components/MessageInput.tsx`、`MessageInput.test.tsx`、
  `SettingsDialog.tsx`、`SettingsDialog.test.tsx`。
  `SettingsGeneralTab.tsx` / `SettingsRemoteTab.tsx` 未改动。

---

## Finding D — 输入框聚焦反馈是空操作

### 代码改动

文件：`desktop/src/components/MessageInput.tsx`，第 159 行（`textarea` 的 `className`）。

```diff
-        className="flex-1 resize-none rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:border-line-strong focus:outline-none disabled:opacity-50"
+        className="flex-1 resize-none rounded-md border border-line-strong bg-surface px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none disabled:opacity-50"
```

只改了焦点色 token：`focus:border-line-strong` → `focus:border-fg-subtle`。
基类仍是 `border-line-strong`，其余类一个未动。`--fg-subtle` 是 `index.css`
里的语义 token（深色 `#737373`、浅色 `#737373`），基色 `--line-strong` 深色
`#404040`、浅色 `#d4d4d4`——两种主题下焦点色都与基色不同，且随 `data-theme`
换肤，不使用字面色阶。

### RED 证明

测试文件：`desktop/src/components/MessageInput.test.tsx`，第 130 行起
`it("gives the composer a visible, theme-aware focus affordance")`。

在 `MessageInput.tsx` 尚未修改时运行：

```
$ npx vitest run src/components/MessageInput.test.tsx src/components/SettingsDialog.test.tsx
 FAIL  src/components/MessageInput.test.tsx > MessageInput > gives the composer a visible, theme-aware focus affordance
AssertionError: expected 'focus:border-line-strong' not to be 'focus:border-line-strong' // Object.is equality
```

（同一断言即“焦点类与基类相同 ⇒ 没有焦点指示”）。

### GREEN 证明

实施改动后单独运行该测试文件：

```
$ npx vitest run src/components/MessageInput.test.tsx

 ✓ src/components/MessageInput.test.tsx (21 tests) 180ms

 Test Files  1 passed (1)
      Tests  21 passed (21)
```

### 断言口径说明（必须）

jsdom 不编译 Tailwind，运行时没有可断言的 CSS，因此该测试**只断言类名契约**，
不碰 CSS 编译：

- 元素上存在基色 token `border-line-strong`；
- 焦点 token `focus:border-*` 存在、且**不等于** `focus:border-line-strong`
  （即焦点态与基态可分辨）——这一条正是修复前跑红的原因；
- 焦点 token 取自语义 token 集合（`line|line-strong|fg|fg-muted|fg-subtle|fg-faint`）；
- `className` 不含 `focus:border-neutral-*` 字面色阶。

### 未做的事

未加 `focus:ring-*`（`border-*` 已足够给出可见反馈，且改动最小、风险最低）；
未加动画；未改动 `disabled:opacity-50` 等既有类。

---

## Finding E — 对话框缺少焦点管理与 tab/panel 接线

### 代码改动（`desktop/src/components/SettingsDialog.tsx`）

1. **焦点移入 / 还原** — 新增 `useEffect`，第 45–53 行：

```tsx
  useEffect(() => {
    if (!open) return;
    restoreRef.current =
      document.activeElement instanceof HTMLElement ? document.activeElement : null;
    // 落焦在选中的 Tab 上：键盘用户一进来就在 tablist 里，箭头键随即可用。
    tabRefs.current[activeAtOpen.current]?.focus();
    // 关闭（open 转 false）或卸载时把焦点还给来处。
    return () => restoreRef.current?.focus();
  }, [open]);
```

打开时把焦点移到选中的 Tab（`tabRefs` 见第 38 行；`activeAtOpen` 见第 42–43 行，
锁住“本次打开”的选区）；关闭或卸载时把焦点还给打开前的元素（`restoreRef`，第 40 行）。

2. **ARIA 接线** — id 约定在第 16–17 行；Tab 上 `aria-controls={panelId(t.id)}`
   （第 110 行）、`id={tabId(t.id)}`（第 104 行）、`tabIndex={t.id === active ? 0 : -1}`
   （第 111 行）；面板第 133–136 行：

```tsx
          <div
            role="tabpanel"
            id={panelId(active)}
            aria-labelledby={tabId(active)}
            className="min-h-0 flex-1 overflow-y-auto"
          >
```

每个 Tab 的 `aria-controls` 指向其面板 id，面板 `role="tabpanel"` 带匹配的
`id` 和 `aria-labelledby`。面板仍一次只渲染选中的那个，id/标签随之切换。

3. **方向键导航 + roving tabIndex** — `onTablistKeyDown`（第 67 行起），挂在
   `nav` 上（第 98 行 `onKeyDown={onTablistKeyDown}`）：

```tsx
  const onTablistKeyDown = (e: KeyboardEvent) => {
    const i = TABS.findIndex((t) => t.id === active);
    // aria-orientation="vertical"，但左右键同样惯例可用。
    let next = i;
    if (e.key === "ArrowDown" || e.key === "ArrowRight") next = (i + 1) % TABS.length;
    else if (e.key === "ArrowUp" || e.key === "ArrowLeft") next = (i - 1 + TABS.length) % TABS.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = TABS.length - 1;
    else return;
    e.preventDefault();
    const id = TABS[next].id;
    setActive(id);
    tabRefs.current[id]?.focus();
  };
```

支持 Left/Right（以及 Up/Down、Home/End），移动选区的同
时把焦点带过去；选中 Tab `tabIndex={0}`、其余 `tabIndex={-1}`，整个 tablist 只占一个 Tab 停靠点。

### RED 证明

在 `SettingsDialog.tsx` 尚未修改时运行同一命令，4 条新测试全部失败：

```
 FAIL  src/components/SettingsDialog.test.tsx > SettingsDialog focus management & tab wiring > moves focus into the dialog on open
AssertionError: expected false to be true // Object.is equality

 FAIL  src/components/SettingsDialog.test.tsx > SettingsDialog focus management & tab wiring > restores focus to the previously focused element on close
AssertionError: expected false to be true // Object.is equality

 FAIL  src/components/SettingsDialog.test.tsx > SettingsDialog focus management & tab wiring > wires each tab to its panel with aria-controls / role=tabpanel / aria-labelledby
TestingLibraryElementError: Unable to find an accessible element with the role "tabpanel"

 FAIL  src/components/SettingsDialog.test.tsx > SettingsDialog focus management & tab wiring > moves selection with ArrowRight and keeps roving tabIndex
AssertionError: expected null to be '0' // Object.is equality
```

（4 条 RED 加上 Finding D 的 1 条，合计 `Tests 5 failed | 27 passed (32)`。）

### GREEN 证明

实施改动后：

```
$ npx tsc --noEmit
TSC_EXIT=0

$ npx vitest run src/components/SettingsDialog.test.tsx

 ✓ src/components/SettingsDialog.test.tsx (11 tests) 268ms

 Test Files  1 passed (1)
      Tests  11 passed (11)
```

原有 7 条测试保持绿色，新增 4 条一并通过。新增测试（第 75 行起）覆盖：
焦点进入对话框、关闭还原到先前聚焦元素、`aria-controls`/`role="tabpanel"`/
`aria-labelledby` 一致接线、ArrowRight 移动选区且 roving tabIndex 正确。

### 未做的事（焦点陷阱）与原因

**没有实现完整的焦点陷阱。** 既未引入 focus-trap 库，也未手写 Tab 循环把焦点
锁在对话框内。理由：

- 本对话框没有任何需要附加聚焦管理的子控件，且用户要求“最小正确改动”，
  而一个正确的 Tab 循环需要枚举可聚焦元素、处理 `disabled`/`inert`、以及
  Shift+Tab 与动态内容（「远程访问」面板异步渲染）等边界，超出本次范围；
- 需求 1（焦点移入 + 还原）已实现，键盘用户打开即落在 tablist、可用箭头键操作；
  未陷阱的后果是：在对话框内继续按 Tab 可以把焦点移出到背景元素。

如果后续需要，最小补法是在对话框根节点（`role="dialog"` 那层）加
`tabIndex={-1}` 与一个 `onKeyDown` 处理 Tab 首尾回绕；本次不引入。

### 其他说明

- 面板保持互斥渲染（一次一个），因此任一时刻只有一个 `role="tabpanel"`，
  测试也对 `getAllByRole("tabpanel")` 断言长度为 1。
- `resize-none`、`disabled:opacity-50` 等既有类未回归。

---

## 全量测试计数

| 时点 | Test Files | Tests |
| --- | --- | --- |
| 改动前 | 45 passed | 439 passed |
| 改动后 | 45 passed | 444 passed |

命令：`npx vitest run`（`desktop/` 下）。改动后连续 10 次全量运行均为
`444 passed (444)`。

## 偏差与范围外发现

- **偶发失败（与本改动无关，未处理）**：有一次全量运行出现
  `Test Files 1 failed | 44 passed`、`Tests 1 failed | 443 passed`，报错位置在
  `desktop/src/components/ThreadSidebar.test.tsx` 第 405 行附近的
  `keeps the running badge mounted across a status flip`。该文件本次未触碰；
  单独运行 `ThreadSidebar.test.tsx` 3 次均 `46 passed`，随后全量运行 10 次均
  `444 passed`。判定为既有的偶发/顺序相关 flake，超出本次范围，未修复，仅记录。
- 除上述外无偏差；`SettingsDialog.tsx` 的面板不再有额外包裹层，面板 id 落在原
  `overflow-y-auto` 容器上（未新增 DOM 层级）。
- 未改动 `SettingsGeneralTab.tsx` / `SettingsRemoteTab.tsx`，也未从它们引入新的
  hook 或 prop。
