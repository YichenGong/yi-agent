# 手机端输入框换行与提示文案 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 手机端（配对 iPhone / iOS webview）输入框的回车改为插入换行、发送与斜杠命令执行收敛到 Send 按钮，并把提示文案从 `Shift+Enter for newline` 改成与能力一致的措辞；桌面端逐字节不变。

**Architecture:** `MessageInput` 新增 `isMobile?: boolean`（默认 `false`），手机端在两处 `onKeyDown` 的 Enter 分支加 `!isMobile` 守卫（放行 textarea 默认行为 = 插入换行），并把 Send 按钮的点击抽成 `handleSendClick`：手机端先判草稿是否为命令再决定执行或发送。`App` 把已有的 `isMobile` 透传进来。提示文案在同一个 JSX 里按 `isMobile` 二选一。

**Tech Stack:** React + TypeScript + Vite，测试用 Vitest + @testing-library/react（jsdom）。

## Global Constraints

- **严禁在 `main` 上直接提交**；本计划在 worktree `fix/mobile-newline-hint` 内执行。
- **桌面端行为与文案逐字节不变**：`isMobile` 默认 `false`，不传参的调用方（含既有全部测试）语义不变。
- **不用 emoji**：标签与文案一律纯文本（仓库约定）。
- **提交前跑测试**：`cd desktop && npx vitest run <改动文件>` 必须全绿。
- 文案字符串逐字符照抄本计划，不得改写：
  - 桌面：`Type a message… (Enter to send, Shift+Enter for newline)`
  - 手机：`Type a message… (Enter for newline, tap Send to send)`
- **jsdom 不实现 textarea 的默认编辑行为**：`fireEvent.keyDown(textarea, {key:"Enter"})` 不会真的往 value 里插 `\n`。因此手机端「回车换行」只能断言「未触发发送/打断/命令」，不能断言 value 里出现换行。

---

### Task 1: `MessageInput` 认出手机端并分流回车、Send 与文案

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx`（props 声明 `:56` 附近；`handleSend`/`runSlash` `:103-123`；Enter 分支 `:198`、`:225`；`placeholder` `:247`；Send `onClick` `:276`）
- Test: `desktop/src/components/MessageInput.test.tsx`（新增一个 `describe("mobile composer")`）

**Interfaces:**
- Consumes: 无（本任务不依赖其它任务）。
- Produces: `MessageInput` 的新 prop `isMobile?: boolean`（默认 `false`）；内部新函数 `handleSendClick(): void`。Task 2 依赖 `isMobile` 这个 prop 名。

- [ ] **Step 1: 先写手机端测试，让它红**

在 `desktop/src/components/MessageInput.test.tsx` 末尾（最后一个 `});` 之前，即顶层 `describe("MessageInput", ...)` 的收尾处）追加：

```tsx
  describe("mobile composer", () => {
    // 手机软键盘的回车恒为 shiftKey=false，且 jsdom 不实现 textarea 的默认
    // 编辑行为（不会真的插入 \n），所以「换行」只能断成「未触发任何动作」。
    it("does not send on Enter (Enter is for newline, tap Send to send)", () => {
      const onSend = vi.fn(async () => true);
      const onInterrupt = vi.fn();
      renderInput({ isMobile: true, onSend, onInterrupt });

      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "line one" } });

      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend).not.toHaveBeenCalled();
      expect(onInterrupt).not.toHaveBeenCalled();
    });

    it("still sends from the Send button", () => {
      const onSend = vi.fn(async () => true);
      renderInput({ isMobile: true, onSend });

      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "hello" } });
      fireEvent.click(screen.getByRole("button", { name: /send/i }));

      expect(onSend).toHaveBeenCalledWith("hello");
    });

    it("runs a slash command from the Send button (the Enter entry is gone)", () => {
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ isMobile: true, onSend, onSlashCommand });

      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/cost" } });
      fireEvent.click(screen.getByRole("button", { name: /send/i }));

      expect(onSlashCommand).toHaveBeenCalledWith("cost", null);
      expect(onSend).not.toHaveBeenCalled();
    });

    it("keeps a bare-slash Send inert, like the desktop Enter", () => {
      // 弹窗默认高亮是破坏性的 `/clear`；裸 `/` 尚未选定命令，Send 必须什么都不做。
      const onSend = vi.fn(async () => true);
      const onSlashCommand = vi.fn();
      renderInput({ isMobile: true, onSend, onSlashCommand });

      const textarea = screen.getByRole("textbox");
      fireEvent.change(textarea, { target: { value: "/" } });
      fireEvent.click(screen.getByRole("button", { name: /send/i }));

      expect(onSlashCommand).not.toHaveBeenCalled();
      expect(onSend).not.toHaveBeenCalled();
    });

    it("shows a hint without Shift+Enter", () => {
      renderInput({ isMobile: true });
      const textarea = screen.getByRole("textbox");
      expect(textarea.getAttribute("placeholder")).toContain("tap Send");
      expect(textarea.getAttribute("placeholder")).not.toContain("Shift");
    });

    it("keeps the desktop hint and Enter-to-send for the desktop build", () => {
      // 对照：默认（不传 isMobile）仍是桌面语义，逐字节不变。
      const onSend = vi.fn(async () => true);
      renderInput({ onSend });
      const textarea = screen.getByRole("textbox");
      expect(textarea.getAttribute("placeholder")).toBe(
        "Type a message… (Enter to send, Shift+Enter for newline)",
      );

      fireEvent.change(textarea, { target: { value: "hi" } });
      fireEvent.keyDown(textarea, { key: "Enter" });
      expect(onSend).toHaveBeenCalledWith("hi");
    });
  });
```

> 注意：本文件的 `renderInput` 用受控的 `Harness` 把 `onDraftChange` 回灌进 `value`，所以 `fireEvent.change` 之后 `textarea.value` 与 `parsed` 都会同步更新，`/cost` 的解析能正常工作。

- [ ] **Step 2: 跑测试确认它红**

Run: `cd desktop && npx vitest run src/components/MessageInput.test.tsx -t "mobile composer"`
Expected: FAIL —— `placeholder` 仍含 `Shift`；手机端 Enter 仍会调用 `onSend`；Send 点击 `/cost` 时调的是 `onSend` 而非 `onSlashCommand`。

- [ ] **Step 3: 加 `isMobile` prop 声明**

在 `desktop/src/components/MessageInput.tsx` 的函数参数解构里（`onPickAttachments,` 一行之后）加：

```tsx
  isMobile = false,
```

并在 `onPickAttachments: () => void;` 之后（props 类型里）加：

```tsx
  /**
   * 手机端（配对 iPhone / iOS webview）：软键盘没有 Shift 键，回车恒为
   * `shiftKey === false`。此时把回车让给「换行」，发送与命令执行收敛到 Send 按钮；
   * 桌面端为 `false`，行为与文案逐字节不变。
   */
  isMobile?: boolean;
```

- [ ] **Step 4: 把 Send 点击抽成 `handleSendClick`**

在 `runSlash`（`:119`）定义之后加：

```tsx
  /**
   * Send 按钮的点击处理。
   *
   * 桌面端就是 `handleSend`。手机端回车已让给换行，命令（`/cost` 等）的入口必须补
   * 到 Send 上：草稿解析为命令就先执行它。裸 `/`（`parsed.kind === "none"` 但弹窗
   * 开着）按桌面端回车的既有语义处理为 inert——弹窗默认高亮是破坏性的 `/clear`，
   * 未经用户选择不得执行。
   */
  const handleSendClick = () => {
    if (isMobile && parsed.kind === "command") {
      runSlash(parsed.name, parsed.args);
      return;
    }
    if (isMobile && showPopup && options.length > 0) return;
    void handleSend();
  };
```

把 Send 按钮的 `onClick`（`:276`）改为：

```tsx
                onClick={turnActive ? onInterrupt : handleSendClick}
```

- [ ] **Step 5: 两处 Enter 分支加 `!isMobile` 守卫**

`:198`（弹窗内）与 `:225`（弹窗外）两处：

```tsx
                if (e.key === "Enter" && !e.shiftKey) {
```

改为：

```tsx
                if (e.key === "Enter" && !e.shiftKey && !isMobile) {
```

> 两处不命中时都不会 `preventDefault`（`:218` 的 `return` 只吞按键、不改默认行为），于是 textarea 在手机上插入换行。`isImeEnter` 守卫（`:173`）保持不变：中文输入法确认候选词的回车仍被上方 early-return 拦下，不会插入换行。

- [ ] **Step 6: 提示文案分平台**

`:247`：

```tsx
            placeholder="Type a message… (Enter to send, Shift+Enter for newline)"
```

改为：

```tsx
            placeholder={
              isMobile
                ? "Type a message… (Enter for newline, tap Send to send)"
                : "Type a message… (Enter to send, Shift+Enter for newline)"
            }
```

- [ ] **Step 7: 跑测试确认全绿**

Run: `cd desktop && npx vitest run src/components/MessageInput.test.tsx`
Expected: PASS（原有的 35 例 + 新增 6 例）。

- [ ] **Step 8: 类型检查**

Run: `cd desktop && npx tsc --noEmit`
Expected: 无输出（0 error）。

- [ ] **Step 9: 提交**

```bash
git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx
git commit -m "feat(desktop): let the mobile composer use Enter for newline"
```

---

### Task 2: `App` 把 `isMobile` 接线到 `MessageInput`

**Files:**
- Modify: `desktop/src/App.tsx`（`<MessageInput ... />` 接线块 `:1834-1860`）
- Test: `desktop/src/App.test.tsx`（新增一条接线用例）

**Interfaces:**
- Consumes: Task 1 的 `MessageInput` prop `isMobile?: boolean`。
- Produces: 无（末端接线）。

- [ ] **Step 1: 先写接线测试，让它红**

在 `desktop/src/App.test.tsx` 末尾追加：

```tsx
describe("App 输入框提示按平台分流", () => {
  const asRemote = () =>
    localStorage.setItem(
      "yi-agent.remote",
      JSON.stringify({ url: "wss://relay.test/ws", token: "yia_tok" }),
    );

  it("shows the newline hint (no Shift+Enter) on the phone client", async () => {
    asRemote();
    render(<App />);
    const textarea = await screen.findByRole("textbox");
    await waitFor(() => {
      expect(textarea.getAttribute("placeholder")).toContain("tap Send");
    });
    expect(textarea.getAttribute("placeholder")).not.toContain("Shift");
  });

  it("keeps the Shift+Enter hint on the desktop build", async () => {
    render(<App />);
    const textarea = await screen.findByRole("textbox");
    await waitFor(() => {
      expect(textarea.getAttribute("placeholder")).toContain("Shift+Enter");
    });
  });
});
```

> 若文件顶部的公共 setup（client 桩、`thread/resume` 等）不满足，参照既有 `describe("App 远端图片上传")` 的 `asRemote()` 手法与相邻用例的等待方式补齐——本文件已有大量同类 `render(<App />)` + `waitFor` 用例可直接对照。

- [ ] **Step 2: 跑测试确认它红**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "输入框提示按平台分流"`
Expected: FAIL —— 手机端用例拿到的 `placeholder` 仍是桌面版（含 `Shift+Enter`）。

- [ ] **Step 3: 接线**

在 `desktop/src/App.tsx` 的 `<MessageInput ... />` 块里（`disabled={current === null}` 一行附近）加：

```tsx
                isMobile={isMobile}
```

（`isMobile` 已在 `App.tsx:188` 由 `useIsMobile()` 求得，无需新增状态。）

- [ ] **Step 4: 跑测试确认全绿**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): wire isMobile into the composer hint"
```

---

### Task 3: 同步模块文档并全量回归

**Files:**
- Modify: `docs/project-management/desktop.md`（`:45` 那条「输入框 Send/Stop + Enter/Shift+Enter」）
- 无代码改动。

**Interfaces:**
- Consumes: Task 1、Task 2 的产出。
- Produces: 无（收尾）。

- [ ] **Step 1: 更新 `docs/project-management/desktop.md`**

在 `:45` 那条「聊天 UI…输入框 Send/Stop + Enter/Shift+Enter…」的描述里，把 `MessageInput.tsx` 的行号引用更新为实际行号，并补一句手机端语义。定位方式：

Run: `grep -n "Enter 发送 / Shift+Enter 换行" docs/project-management/desktop.md`

在该句后追加（保持既有「代码位置 + 判据」的写法）：

```
；手机端（`desktop/src/lib/useIsMobile.ts:23` 的 `detectMobile`）回车改为插入换行、
发送与斜杠命令统一走 Send 按钮（`desktop/src/components/MessageInput.tsx` 的
`isMobile` / `handleSendClick`），提示文案相应改成不含 `Shift+Enter` 的措辞
```

并把该条里 `MessageInput.tsx` 的行号引用改成 `git grep -n` 实测的最新行号。

- [ ] **Step 2: 全量前端测试**

Run: `cd desktop && npx vitest run`
Expected: 全绿（较改动前多 6 条 `MessageInput` 用例与 2 条 `App` 用例）。

- [ ] **Step 3: 类型检查 + 构建**

Run: `cd desktop && npx tsc --noEmit && npm run build`
Expected: `tsc` 无输出；`vite build` 成功产出 `dist/`。

- [ ] **Step 4: 残留核对**

Run: `cd desktop && grep -rn "Shift+Enter" src`
Expected: 只命中桌面分支的那一处字符串（`MessageInput.tsx`）与 `MessageInput.test.tsx` 里断言桌面不变的用例；不得出现在手机端分支。

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/desktop.md
git commit -m "docs(desktop): note the mobile composer's Enter/Send semantics"
```

---

## Self-Review

**Spec coverage：**
- §2.2 手机端回车 = 换行 → Task 1 Step 5。
- §2.3 手机端 Send 补命令入口 → Task 1 Step 4。
- §2.4 提示文案分平台 → Task 1 Step 6。
- §3.1 `isMobile` prop + 三处改动 → Task 1 Step 3-6。
- §3.2 `App` 接线 → Task 2 Step 3。
- §3.3 测试（`MessageInput.test.tsx` 6 例 + `App.test.tsx` 2 例）→ Task 1 Step 1、Task 2 Step 1。
- §3.4 文档 → Task 3 Step 1。
- §4 边界（IME、turnActive、附件）→ Task 1 Step 5 的守卫不影响 `isImeEnter`；Send 逻辑未动附件分支。
- §5 判据 → Task 3 Step 2-4。

**Placeholder scan：** 无 TBD/TODO；所有代码步骤给了可直接粘贴的代码。Task 3 Step 1 用「grep 定位 + 追加」而非写死行号，避免与实测行号漂移（该文件条目本身引用的行号本就需要随改动更新）。

**Type consistency：** prop 名 `isMobile`（Task 1 声明、Task 2 接线、两处测试传参）一致；`handleSendClick`（Task 1 定义、`onClick` 使用）一致；`parsed` / `showPopup` / `options` / `runSlash` 均为文件中已有标识符（`:89`、`:96`、`:97`、`:119`）。
