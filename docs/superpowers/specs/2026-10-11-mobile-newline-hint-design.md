# 手机端输入框：换行能力与提示文案 — 设计

日期：2026-10-11
状态：设计待确认（方案 B：手机端补上换行能力）
分支：`fix/mobile-newline-hint`
范围：仅前端 `desktop/`（`MessageInput.tsx`、`App.tsx` 接线、两处测试、`docs/project-management/desktop.md`）

---

## 1. 问题与已核实事实

用户反馈：手机 APP 上输入框提示写着 `Shift+Enter for newline`，但手机软键盘没有 Shift
键，这条提示是错的。

核实后的事实：

- 提示文案只有一处：`desktop/src/components/MessageInput.tsx:247`
  的 `placeholder="Type a message… (Enter to send, Shift+Enter for newline)"`。

- `MessageInput` 桌面端与手机端**共用同一个组件**，目前不接收任何平台信息；`App` 已
  算出 `isMobile`（`App.tsx:188` 的 `useIsMobile()`，判据见
  `desktop/src/lib/useIsMobile.ts:23` 的 `detectMobile`：远端已配对 **或** iOS UA）。

- 手机软键盘的回车**恒为 `shiftKey === false`**。当前 `onKeyDown`（`MessageInput.tsx:225`
  的 `if (e.key === "Enter" && !e.shiftKey)`）在手机上必然命中，于是直接走
  `handleSend()` / `onInterrupt()` / `runSlash()`。结论：**手机上根本没有插入换行的
  路径**——不是提示措辞不对，而是它承诺了一个当前做不到的动作。

- 斜杠命令（`/help` `/cost` `/model` `/config` `/clear` `/compact`）的触发入口是
  **Enter**：`MessageInput.tsx:229`（无弹窗时 `parsed.kind === "command"` → `runSlash`）
  与 `:198`（弹窗开启时 Enter 接受高亮项）。`App.send`（`App.tsx:1333`）本身不解析
  斜杠——它只负责把文本 / 附件发出去。因此**手机上把「Enter 发送」改掉，会连带丢掉
  命令入口**，必须一并处理。

- `App` 里 `isMobile` 已经传给了 `ThreadSidebar`（`App.tsx:1744`）等处，同源复用即可。

## 2. 决策（方案 B）

手机端把回车让给「换行」，发送与命令执行都收敛到 **Send 按钮**：

1. **桌面端逐字节不变**：行为、文案、既有测试全部保持原样。
2. **手机端回车 = 插入换行**：`onKeyDown` 的两处 Enter 分支在 `isMobile` 时不再
   `preventDefault`，让 textarea 走默认行为（插入 `\n`）。
3. **手机端 Send 按钮补上命令入口**：点击 Send 时，若草稿解析为命令
   （`parseSlashInput` 得 `kind === "command"`）则执行该命令，否则照常发送。等价于把
   桌面端「回车那一步」挪到 Send 按钮上，从而不回归手机端的 `/cost` 等能力。
4. **提示文案分平台**：手机端 `Type a message… (Enter for newline, tap Send to send)`；
   桌面端维持 `Type a message… (Enter to send, Shift+Enter for newline)`。

被否的备选（仅记录）：A 只改文案、不补能力——手机上永远只能写单行，且提示里无论写什么
都掩盖不了「用户可能想要多行却做不到」的事实。B 让提示与实际能力对齐，代价是新增一个
`isMobile` prop 与一个 Send-命令分支。

## 3. 分层设计

### 3.1 `MessageInput`（`desktop/src/components/MessageInput.tsx`）

新增 prop：

```tsx
/**
 * 手机端（配对 iPhone / iOS webview）：软键盘没有 Shift 键，回车恒为
 * `shiftKey === false`。此时把回车让给「换行」，发送与命令执行收敛到 Send 按钮；
 * 桌面端为 `false`，行为与文案逐字节不变。
 */
isMobile?: boolean;   // 默认 false
```

三处改动：

1. **`placeholder` 分平台**（`:247`）：

   ```tsx
   placeholder={
     isMobile
       ? "Type a message… (Enter for newline, tap Send to send)"
       : "Type a message… (Enter to send, Shift+Enter for newline)"
   }
   ```

2. **两处 Enter 分支在手机上放行默认行为**：

   - 弹窗内（`:198`）：`if (e.key === "Enter" && !e.shiftKey && !isMobile)`
   - 弹窗外（`:225`）：`if (e.key === "Enter" && !e.shiftKey && !isMobile)`

   两处不命中时都不会 `preventDefault`（弹窗块末尾那句 `return` 只吞按键、不改默认），
   于是 textarea 插入换行。注意：`isImeEnter` 守卫（`:173`）保持不变——手机上中文
   输入法确认候选词的回车仍不该插入换行，仍由上方的 early-return 拦下。

3. **Send 按钮：手机上先判命令再发送**（`:276` 的 `onClick`）。把点击处理抽成一个
   函数，桌面端直接 `handleSend()`，手机端走判命令的版本：

   ```tsx
   /**
    * 手机端 Send：草稿若是一条命令（`/cost` 等）就先执行它——这些命令在桌面端由
    * 回车触发，而手机端回车已让给换行，入口必须补到 Send 上。裸 `/`（尚未选定命令）
    * 与桌面端回车同义：inert，不发送也不执行（弹窗默认高亮是破坏性的 `/clear`）。
    */
   const handleSendClick = () => {
     if (isMobile && showPopup && options.length > 0 && parsed.kind !== "command") return;
     if (isMobile && parsed.kind === "command") {
       runSlash(parsed.name, parsed.args);
       return;
     }
     void handleSend();
   };
   ```

   `onClick={turnActive ? onInterrupt : handleSendClick}`。桌面端 `isMobile=false`，
   `handleSendClick` 退化为 `void handleSend()`，与现状等价。

   > 说明：`parsed.kind === "command"` 同时覆盖「已知命令」与「未知命令」——与桌面端
   > `:229` 的无弹窗分支一致（未知命令走 `runSlash` → `App.onSlashCommand` 的
   > `default` 报「未知命令」，不会发给 agent）。`parsed.kind === "path"`（如
   > `/Users/...` 两斜杠绝对路径）按现状落入正常发送。

### 3.2 `App`（`desktop/src/App.tsx`）

接线处（`App.tsx:1834` 的 `<MessageInput ... />`）加一行：

```tsx
isMobile={isMobile}
```

（`isMobile` 已在 `:188` 求得，无需新增状态。）

### 3.3 测试

`desktop/src/components/MessageInput.test.tsx`：

- 默认 props 的 `isMobile` 不传（保持 `undefined` = 桌面）→ 现有 35 例全部照旧通过。
- 新增 `describe("mobile composer", ...)`：
  - 手机上回车**不**发送：`renderInput({ isMobile: true, onSend })`，输入文本，`keyDown Enter`
    → `onSend` 未被调用。
  - 手机的 Send 按钮仍发送：同上但 `fireEvent.click(Send)` → `onSend` 被调用。
  - 手机 Send 执行命令：输入 `/cost`，点 Send → `onSlashCommand("cost", null)` 被调用、
    `onSend` 未被调用。
  - 手机裸 `/` 点 Send 为 inert：`onSlashCommand` 与 `onSend` 均未被调用。
  - 手机 placeholder 不含 `Shift+Enter`：`getByRole("textbox")` 的 `placeholder` 断言。
  - 对照：桌面（默认）placeholder 仍含 `Shift+Enter`，且回车仍发送（守「桌面不变」）。

`desktop/src/App.test.tsx`：

- 新增一条接线用例：远端（`localStorage["yi-agent.remote"]` 已设，同既有
  `describe("App 远端图片上传")` 的 `asRemote()` 手法）渲染后，输入框 `placeholder`
  为手机版；桌面（不设 remote）为桌面版。锚点 `screen.getByRole("textbox")` 的
  `placeholder` 属性。

### 3.4 文档

`docs/project-management/desktop.md`：

- `:45` 那条「输入框 Send/Stop + Enter/Shift+Enter」补一句手机端语义（回车换行、Send
  发送/执行命令）并更新 `MessageInput.tsx` 行号引用。
- 若新增独立条目更清晰，则加一条 `[x]` 条目，附可执行判据与代码位置。

## 4. 边界与不变量

- 桌面端：placeholder、回车语义、斜杠触发、IME 守卫 —— **逐字节不变**。
- IME：手机上中文输入法确认候选词的回车仍被 `isImeEnter` 拦下，不会插入换行
  （否则每个候选词确认都会多一个空行）。
- `turnActive` 时手机端回车不发送也不打断（textarea 此时 `disabled`，本就无法输入）；
  打断仍由 Stop 按钮负责——与桌面一致。
- 附件：「只发附件、不打字」的路径不受影响（Send 按钮逻辑不变）。

## 5. 判据

- `cd desktop && npx vitest run src/components/MessageInput.test.tsx src/App.test.tsx`
  （回退 `isMobile` 守卫或 Send 命令分支即失败）。
- `cd desktop && npx tsc --noEmit && npm run build`。
- `cd desktop && npx vitest run`（全量前端单测）。
