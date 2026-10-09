# 桌面端输入区改造为「输入卡片」实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把桌面端输入区（composer）改造成一个整卡——textarea 无边框置于卡内上方，底部一行工具栏收进「附加文件 / Mode / 模型 / Send」，聚焦时整卡边框高亮。

**Architecture:** 只改 `MessageInput.tsx` 一个组件的 DOM 结构与 Tailwind 类名。新增一张「卡片」包裹层承载边框、圆角、底色与 `focus-within` 高亮；textarea 去掉自身边框/底色；附件 chip 行移入卡内顶部；底部工具栏由当前两行合并为一行（左「附加文件」，右 `Mode → 模型 → Send`，`justify-between`）。`App.tsx` 的接线、三个子组件的内部实现、发送/打断语义全部不动。

**Tech Stack:** React 19 + TypeScript + Tailwind CSS 4 + vitest（jsdom，不编译 Tailwind，故测试断**类名契约**）。

**设计文档:** `docs/superpowers/specs/2026-10-09-composer-input-card-design.md`

## Global Constraints

- 仓库工作目录：worktree `.worktrees/feat-desktop-composer-card`，分支 `feat/desktop-composer-card`。所有命令都在该 worktree 内执行。
- 前端命令：`cd desktop && npx tsc --noEmit`、`cd desktop && npx vitest run`。测试文件用 `/** @vitest-environment jsdom */` 头。
- **本机 PATH 里没有 node**：每条命令前先 `export PATH="/opt/homebrew/bin:$PATH"`（否则报 `npx: command not found`）。
- jsdom **不编译 Tailwind**：组件测试只能断言 `className` 字符串契约，不能断言计算样式。
- 颜色只用语义 token（`surface` / `panel` / `raised` / `line` / `line-strong` / `fg` / `fg-muted` / `fg-subtle` / `fg-faint`），**禁止**写字面色阶（如 `neutral-*`），否则两套主题必有一观感错。
- 视觉/无障碍文案（`aria-label`、按钮文字、placeholder）**一字不改**：它们是既有测试锚点。
- 不新增依赖；不改 `App.tsx`、`AttachmentChips.tsx`、`ModeChip.tsx`、`ModelPicker.tsx`、`SlashPopup.tsx`。
- commit message 用 conventional commits，**不写** `Co-Authored-By` 行。
- 改完同步更新 `docs/project-management/desktop.md`（项目规范要求，见 `CLAUDE.md`）。

## 目标 DOM 结构（两步走）

```
外层 .relative.border-t.border-line.bg-panel.p-3   ← 不动（钉底 + SlashPopup 定位上下文）
  ├─ SlashPopup（showPopup 时，绝对定位，不动）
  └─ 卡片 [data-testid=composer-card] .relative.rounded-lg.border.border-line-strong.bg-surface.focus-within:border-fg-subtle  ← Task 1 加
       ├─ [data-testid="attachment-chips"]（名字非空时，移入卡内）                                  ← Task 1 移
       └─ 行 .flex.items-end.gap-2.max-md:flex-col.max-md:items-stretch                             ← Task 1 加
            ├─ textarea（flex-1，无自身边框/底色）                                                  ← Task 1 改
            └─ 工具栏 [data-composer-toolbar] .flex.items-center.justify-between.gap-2.px-3.pb-2
                 │  .max-md:flex-wrap.max-md:justify-end                                            ← Task 2 加
                 ├─ 「附加文件」按钮（文档流中第一个控件）                                          ← Task 1 移入
                 └─ 右组 .flex.items-center.gap-2
                      ├─ ModeChip
                      ├─ modelPicker
                      └─ Send/Stop
```

两步走是为了让修改后的既有测试**每一步都跑得过**：Task 1 保留 textarea 的聚焦类（让 `MessageInput.test.tsx:152-169` 继续通过），Task 2 才把聚焦契约迁到卡片上。

---

### Task 1: 卡片外壳（边框、无边框 textarea、chip 入卡）

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx:116-250`
- Test: `desktop/src/components/MessageInput.test.tsx`（改 `renderInput` 辅助函数；新增用例；改 `:171-188`）

**Interfaces:**
- Consumes: 无（纯组件内部结构改动）。
- Produces: 卡片元素（textarea 的祖父）类名含 `border-line-strong` + `bg-surface` + `focus-within:border-fg-subtle`；textarea 类名含 `max-md:w-full` + `max-md:flex-none`；工具栏类名含 `max-md:flex-wrap` + `max-md:justify-end`。Task 2 依赖这些类名。

- [ ] **Step 0: 让测试辅助函数能传 `modelPicker`**

`renderInput` 的入参类型是 `Partial<ComponentProps<typeof MessageInput>>`，而 `modelPicker` **不在** `MessageInput` 的 props 里（它是 App 接线时现拼的 JSX），所以直接传会因为类型不符而 `tsc` 报错；硬塞进去还会被展开成未知 DOM 属性、触发 React 告警。把 `MessageInput.test.tsx` 顶部的 `renderInput`（当前 `:10-42`）整体替换为：

```tsx
function renderInput(
  overrides: Partial<ComponentProps<typeof MessageInput>> & { modelPicker?: ReactNode } = {},
) {
  // 单独摘出 modelPicker：它不属于 MessageInput 的 props 类型，若跟着 `{...props}`
  // 展开会被当成未知 DOM 属性跑到 <div> 上。
  const { modelPicker, ...rest } = overrides;
  const props: ComponentProps<typeof MessageInput> = {
    turnActive: false,
    onSend: vi.fn(async () => true),
    onInterrupt: vi.fn(),
    mode: "normal",
    onModeChange: vi.fn(),
    onSlashCommand: vi.fn(),
    value: "",
    onDraftChange: vi.fn(),
    attachments: [],
    onPickFiles: vi.fn(),
    onRemoveAttachment: vi.fn(),
    ...rest,
  };
  // The composer is controlled: mirror `onDraftChange` back into `value` exactly
  // as App does (against the current session's draft), so typing behaves like the
  // real app instead of a frozen value.
  function Harness() {
    const [value, setValue] = useState(props.value);
    return (
      <MessageInput
        {...props}
        modelPicker={modelPicker}
        value={value}
        onDraftChange={(next) => {
          props.onDraftChange(next);
          setValue(next);
        }}
      />
    );
  }
  return { ...render(<Harness />), props };
}
```

并在文件顶部把 `import type { ComponentProps } from "react";` 改为 `import type { ComponentProps, ReactNode } from "react";`。

- [ ] **Step 1: 写失败的测试（新增结构用例 + 更新移动端用例）**

在 `desktop/src/components/MessageInput.test.tsx` 的 `describe("MessageInput", ...)` 内、`it("stacks the composer at the phone breakpoint ...")` 之前，插入：

```tsx
  it("draws one card around the composer, with a borderless textarea", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    const card = screen.getByTestId("composer-card");
    // 边框搬到卡片上：整卡就是「输入框」的视觉边界，聚焦时整卡高亮。
    expect(card.className).toContain("rounded-lg");
    expect(card.className).toContain("border-line-strong");
    expect(card.className).toContain("bg-surface");
    expect(card.className).toContain("focus-within:border-fg-subtle");
    // textarea 自身不再是那个「带边框的盒子」。
    expect(textarea.className).not.toContain("border-line-strong");
  });

  it("puts the attachment chips inside the card", () => {
    renderInput({
      attachments: [{ path: "/tmp/report.pdf", name: "report.pdf", size: 10 }],
    });
    const card = screen.getByTestId("composer-card");
    const chips = screen.getByTestId("attachment-chips");
    // 卡内：chip 行的祖先链里有卡片元素。
    expect(card.contains(chips)).toBe(true);
  });

  it("moves the model picker into the toolbar row inside the card", () => {
    renderInput({ modelPicker: <span data-testid="picker">picker</span> });
    const row = screen.getByRole("textbox").nextElementSibling as HTMLElement;
    const toolbar = row.querySelector('[data-composer-toolbar]') as HTMLElement;
    expect(toolbar.contains(screen.getByTestId("picker"))).toBe(true);
    // 主操作（Send/Stop）和模型选择器同处一行。
    expect(toolbar.contains(screen.getByRole("button", { name: /send/i }))).toBe(true);
  });
```

把既有 `it("stacks the composer at the phone breakpoint ...")`（`:171-188`）的正文替换为：

```tsx
  it("stacks the composer at the phone breakpoint so the input keeps its width", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    // 桌面那一行是"输入框 + 右侧控件"；手机上 `max-md` 变体让它纵向堆叠，输入框独占整行。
    // jsdom 不编译 Tailwind，故只断类名契约。
    const row = textarea.parentElement!;
    expect(row.className).toContain("max-md:flex-col");
    expect(row.className).toContain("max-md:items-stretch");
    // 输入框在手机上必须"占满整行"而不是"可伸缩的 flex 项"。
    expect(textarea.className).toContain("max-md:w-full");
    expect(textarea.className).toContain("max-md:flex-none");
    // 工具栏在手机上撑满整行并横向换行（模型选择器可能很长），换行后仍贴右。
    const toolbar = row.querySelector('[data-composer-toolbar]') as HTMLElement;
    expect(toolbar.className).toContain("max-md:flex-wrap");
    expect(toolbar.className).toContain("max-md:justify-end");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/MessageInput.test.tsx`
Expected: 新增的 3 个用例 FAIL（`card.className` 取到的是当前外层 `border-t border-line bg-panel p-3`，不含 `rounded-lg` / `focus-within:border-fg-subtle`；`toolbar.contains(picker)` 为 false）。

- [ ] **Step 3: 改实现（引入卡片，textarea 去边框，工具栏收纳，保留 textarea 聚焦类）**

用下面整块替换 `MessageInput.tsx` 的 `return (...)`（当前 `:116-251`）：

```tsx
  return (
    <div className="relative border-t border-line bg-panel p-3">
      {showPopup && <SlashPopup commands={options} selected={selected} />}
      {/*
       * 输入卡片：整卡承载边框、圆角与底色，聚焦（textarea 或卡内任意控件）时整卡高亮。
       * 边框从 textarea 搬到卡片上——这样"输入的地方"是一个整体，而不是一块输入框加
       * 一列散在框外的控件。
       */}
      <div
        className="rounded-lg border border-line-strong bg-surface transition-colors focus-within:border-fg-subtle"
        data-testid="composer-card"
      >
        <AttachmentChips attachments={attachments} onRemove={onRemoveAttachment} />
        {/*
         * Phone breakpoint: the desktop row is 输入框 + 底部工具栏。`max-md` 让手机改成
         * 纵向：输入框独占整行，工具栏落到它下面。桌面（>768px，含可缩到 720px 的窗口）
         * 不匹配该变体。
         */}
        <div className="flex items-end gap-2 max-md:flex-col max-md:items-stretch">
          <textarea
            ref={inputRef}
            value={text}
            onChange={(e) => {
              const next = e.target.value;
              onDraftChange(next);
              // Re-arm the popup whenever the text still looks like a command name.
              setPopupOpen(/^\/[^\s/]*$/.test(next.trim()));
            }}
            onCompositionStart={ime.onCompositionStart}
            onCompositionEnd={ime.onCompositionEnd}
            onBlur={ime.resetComposition}
            onKeyDown={(e) => {
              if (isImeEnter(e, ime.composing.current)) return;
              if (showPopup && options.length > 0) {
                if (e.key === "ArrowDown") {
                  e.preventDefault();
                  setSelected((i) => (i + 1) % options.length);
                  return;
                }
                if (e.key === "ArrowUp") {
                  e.preventDefault();
                  setSelected((i) => (i - 1 + options.length) % options.length);
                  return;
                }
                if (e.key === "Tab") {
                  e.preventDefault();
                  const picked = options[Math.min(selected, options.length - 1)];
                  onDraftChange(`/${picked.name} `);
                  setPopupOpen(false);
                  inputRef.current?.focus();
                  return;
                }
                if (e.key === "Escape") {
                  e.preventDefault();
                  setPopupOpen(false);
                  return;
                }
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  // A bare "/" names no command (`parseSlashInput("/")` is
                  // `{ kind: "none" }`), yet the popup is offered for it and its
                  // DEFAULT highlight is index 0 — the destructive `/clear`. A stray
                  // Enter on that untouched default would erase the transcript, so it
                  // is inert until the user either types a name character (kind turns
                  // "command", as for "/cos") or moves the highlight with ↑/↓ (an
                  // explicit choice, which the line below honours). Tab still
                  // completes.
                  if (parsed.kind !== "command" && selected === 0) return;
                  // A space closes the popup (name-mode only), so no arguments can
                  // be pending here: accepting the highlighted row is exactly what
                  // "complete and run" means (`/cos` -> `/cost`). Fully typed
                  // commands — arguments and unknowns included — reach the
                  // no-popup branch below with `parsed` intact.
                  const picked = options[Math.min(selected, options.length - 1)];
                  runSlash(picked.name, null);
                  return;
                }
                return; // 弹窗开启时吞掉其余按键,不作文本处理
              }
              if (e.key === "Escape" && popupOpen) {
                e.preventDefault();
                setPopupOpen(false);
                return;
              }
              if (e.key === "Enter" && !e.shiftKey) {
                // Confirming a candidate with Enter is the IME's key, not the user's:
                // let the composition land in the box and wait for the next Enter.
                e.preventDefault();
                if (parsed.kind === "command") {
                  // This branch is reached when the popup matched nothing: unknown
                  // commands and matched commands alike belong to the command
                  // layer, never to the agent (`/nope` reports, it does not send).
                  runSlash(parsed.name, parsed.args);
                  return;
                }
                if (parsed.kind === "path") {
                  // Two-slash first token is a path (TUI parity) — falls through.
                } else if (turnActive) {
                  onInterrupt();
                  return;
                }
                void handleSend();
              }
            }}
            disabled={disabled || sending || turnActive}
            rows={3}
            placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
            /*
             * 排序敏感：Task 2 的聚焦契约依赖这里仍带 `focus:border-fg-subtle`（本步骤
             * 保留它，好让既有的聚焦用例继续通过）。`border`/`bg-transparent` 让 textarea
             * 退化成卡内无边框文本区。
             */
            className="min-w-0 flex-1 resize-none border border-transparent bg-transparent px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none disabled:opacity-50 max-md:w-full max-md:flex-none"
          />
          <div
            data-composer-toolbar
            className="flex items-center justify-between gap-2 px-3 pb-2 max-md:flex-wrap max-md:justify-end"
          >
            <button
              type="button"
              // 纯文本标签（仓库不用 emoji）；`aria-label` 与可见文字一致，是测试锚点。
              aria-label="附加文件"
              title="附加文件"
              className="rounded-md border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:bg-raised hover:text-fg disabled:opacity-50"
              onClick={onPickFiles}
              disabled={disabled}
            >
              附加文件
            </button>
            <div className="flex items-center gap-2">
              <ModeChip mode={mode} onChange={onModeChange} disabled={mode === null} />
              {modelPicker}
              <button
                type="button"
                onClick={turnActive ? onInterrupt : () => void handleSend()}
                disabled={sendDisabled}
                className={
                  turnActive
                    ? "rounded-md bg-red-600 px-4 py-2 text-sm font-medium text-white hover:bg-red-500 disabled:opacity-50"
                    : "rounded-md bg-blue-600 px-4 py-2 text-sm font-medium text-white hover:bg-blue-500 disabled:opacity-50"
                }
              >
                {turnActive ? "Stop" : "Send"}
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/components/MessageInput.test.tsx`
Expected: 全部 PASS（含既有的聚焦用例 `:152-169`——它读的仍是 textarea 的类名，本步骤保留了 `focus:border-fg-subtle`）。

- [ ] **Step 5: 跑整个前端测试套**

Run: `cd desktop && npx vitest run`
Expected: 全部 PASS。特别确认 `src/App.test.tsx`（「附加文件」按钮锚点、附件接线）仍通过。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx
git commit -m "feat(desktop): wrap the composer in an input card"
```

---

### Task 2: 聚焦契约迁到卡片上

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx`（textarea 的 `className` 一行）
- Test: `desktop/src/components/MessageInput.test.tsx:152-169`（重写聚焦用例）

**Interfaces:**
- Consumes: Task 1 建的卡片元素（textarea 的祖父）与它的 `focus-within:border-fg-subtle`。
- Produces: textarea 类名**不再**含 `focus:border-*`；聚焦契约由卡片承担。用户的验收判据「点卡内任意控件整卡高亮」由此达成。

- [ ] **Step 1: 重写聚焦用例（先跑红）**

把 `MessageInput.test.tsx` 的 `it("gives the composer a visible, theme-aware focus affordance", ...)`（当前 `:152-169`）整段替换为：

```tsx
  it("gives the composer a visible, theme-aware focus affordance", () => {
    renderInput();
    const textarea = screen.getByRole("textbox");
    const card = screen.getByTestId("composer-card");
    // jsdom 不编译 Tailwind，没有可断言的 CSS，因此这里只断类名契约：
    // 焦点指示落在**卡片**上（`focus-within` 让卡内任意控件获得焦点时整卡高亮，
    // 包括底部工具栏的按钮），基色 token 与焦点色 token 必须不同。
    const classes = card.className.split(/\s+/);
    const baseBorder = classes.find((c) =>
      /^border-(line|line-strong|fg|fg-muted|fg-subtle|fg-faint|surface|panel|raised)$/.test(c),
    );
    const focusBorder = classes.find((c) => c === "focus-within:border-fg-subtle");
    expect(baseBorder).toBe("border-line-strong");
    expect(focusBorder).toBe("focus-within:border-fg-subtle");
    // 焦点态等于基色就等于没有焦点指示（本断言先跑红）。
    expect(focusBorder).not.toBe(`focus-within:${baseBorder}`);
    // 不写字面色阶，否则两套主题里必有一有一观感错。
    expect(card.className).not.toMatch(/focus-within:border-neutral-/);
    // 边框只有一处：textarea 不该再自画一个边框，否则卡内会出现"框里套框"。
    expect(textarea.className).not.toMatch(/(^|\s)focus:border-/);
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/MessageInput.test.tsx -t "focus affordance"`
Expected: FAIL —— textarea 的类名仍含 `focus:border-fg-subtle`（Task 1 为了通过旧用例保留了它），最后一条断言先跑红。

- [ ] **Step 3: 改实现（去掉 textarea 的自身边框与聚焦类）**

把 textarea 的 `className` 从：

```
"min-w-0 flex-1 resize-none border border-transparent bg-transparent px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:border-fg-subtle focus:outline-none disabled:opacity-50 max-md:w-full max-md:flex-none"
```

改为：

```
"min-w-0 flex-1 resize-none bg-transparent px-3 py-2 text-sm text-fg placeholder:text-fg-faint focus:outline-none disabled:opacity-50 max-md:w-full max-md:flex-none"
```

同时删掉 Task 1 在 `className` 上方留下的那段"排序敏感"注释（它已经过期，聚焦契约现在归卡片）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/components/MessageInput.test.tsx`
Expected: 全部 PASS。

- [ ] **Step 5: 跑整个前端测试套**

Run: `cd desktop && npx vitest run`
Expected: 全部 PASS。

- [ ] **Step 6: 手动视觉验收（开发服务器）**

Run: `cd desktop && npm run dev`
逐条对照设计文档 §6：整卡边框、底部一行（左「附加文件」/ 右 `Mode`、模型、`Send`）、点 textarea 或卡内按钮整卡边框变亮、chip 在卡内顶部、模型下拉向上弹出可用、深/浅主题各看一遍。
Expected: 全部符合。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx
git commit -m "refactor(desktop): let the input card own the focus ring"
```

---

### Task 3: 更新项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`

**Interfaces:**
- Consumes: Task 1/2 的最终代码与测试结果。
- Produces: 无（文档）。

- [ ] **Step 1: 更新「聊天 UI」那条判据的行号**

`docs/project-management/desktop.md:45` 里引用了 `desktop/src/components/MessageInput.tsx:40` / `:58` / `:50`。用实际新行号订正（跑 `grep -n "Stop\" : \"Send\|const handleSend\|onClick={turnActive" desktop/src/components/MessageInput.tsx` 取行号），并保留原有说明文字。

- [ ] **Step 2: 在 `desktop.md` 的输入区条目后追加一条**

在 `:94`（文档附件输入）之后新增一行，形如：

```markdown
- [x] 输入区改造为「输入卡片」（模型下拉 / 附加文件 / Mode 全部收进卡内，聚焦整卡高亮）— 判据：`cd desktop && npx tsc --noEmit && npx vitest run src/components/MessageInput.test.tsx`（先回退 textarea 的 `focus:border-*` 或把卡片类名改回 `border-t border-line bg-panel` 即失败）；代码：`desktop/src/components/MessageInput.tsx`（卡片包裹层含 `rounded-lg border border-line-strong bg-surface focus-within:border-fg-subtle`；无边框 textarea；底部一行工具栏 `justify-between`，左「附加文件」右 `Mode`/模型/`Send`；附件 chips 移入卡内顶部），`desktop/src/App.tsx` 接线不变 — [设计](../superpowers/specs/2026-10-09-composer-input-card-design.md)
```

- [ ] **Step 3: 同步 README 模块索引计数**

`README.md:248` 的 desktop 行是 `| desktop | 37 / 50 | [详情](docs/project-management/desktop.md) |`。本次新增 1 条 feature（输入卡改造），改为 `| desktop | 38 / 50 | ... |`。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/desktop.md README.md
git commit -m "docs: record the composer input-card rework"
```

---

### Task 4: 合并回 main

**Files:** 无（git 操作）。

- [ ] **Step 1: 最终验证**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全部 PASS。

- [ ] **Step 2: 合并（在 main 上执行）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git merge --no-ff feat/desktop-composer-card
```

- [ ] **Step 3: 清理 worktree 与分支（合并完成后、且没有会话仍把该目录当工作目录时才做）**

```bash
git worktree remove .worktrees/feat-desktop-composer-card
git branch -d feat/desktop-composer-card
```
