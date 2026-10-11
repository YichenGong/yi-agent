# Single Attach Button Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面端把「附加文件」与「附加图片」两个按钮合为一个入口：点一次、开一次原生对话框，文档与图片在同一次选择里都能选，按扩展名自动分流。

**Architecture:** `MessageInput` 的 `onPickFiles`/`onPickImages` 两个 prop 收敛为一个 `onPickAttachments`，工具栏删掉第二个按钮。`App` 的 `pickIntoSession` 从「参数化的单一白名单」改为「并集白名单 + `pendingKindFromPath` 逐路径判类 + 按类选预检函数」，一次原生多选里文档与图片混选各自带正确 `kind` 入列。发送端的协议形态由 `kind` 决定，保持不变。远端（iOS）回落为 `pickImagesOnRemote()`。

**Tech Stack:** React + TypeScript + Vite + Vitest（jsdom），Tauri `@tauri-apps/plugin-dialog`（原生多选）。

## Global Constraints

- 按钮文案与 `aria-label` 恒为「附加文件」（不改名，保留既有测试锚点与工具栏第一个控件契约）。
- 仅桌面端改动；远端（`isRemoteClient()`）维持图片专用路径 `pickImagesOnRemote()`，不改。
- 服务端、协议（`turn/start` 的 input block 形态）、`attachmentLimits.ts` 的白名单与预检函数、`AttachmentChips`、`PendingAttachment` 结构，全部不改。
- 桌面端 `size` 传 0（dialog 只给路径，超限交服务端权威校验），沿用现状。
- 回归命令（在 `desktop/` 下）：`npx vitest run && npx tsc --noEmit && npm run build`。
- 提交信息风格沿用仓库既有：`feat(desktop): ...`。

---

## File Structure

- `desktop/src/components/MessageInput.tsx` — 工具栏按钮由两个合为一个；props 由 `onPickFiles`/`onPickImages` 收敛为 `onPickAttachments`。
- `desktop/src/components/MessageInput.test.tsx` — 默认 props 与按钮相关用例同步。
- `desktop/src/App.tsx` — `pickIntoSession` 泛化为并集 + 逐路径判类；删除 `pickFilesToAttach` / `pickImagesToAttach` 的桌面分支；接线处收敛为一个 `onPickAttachments`。保留 `pickImagesOnRemote`。
- `desktop/src/App.test.tsx` — 两个按钮锚点收敛为一个；两个 helper 合并；删除两个「跨入口被拒」用例；把「图片+文档」用例升级为「一次混选」用例。

> 说明：这四个文件必须一起改才能编译通过（`MessageInput` 的 prop 名被 `App` 使用），故合并为**一个实现任务**；随后一个独立的回归验证任务。

---

### Task 1: 合并为单一附件入口（MessageInput + App）

**Files:**
- Modify: `desktop/src/components/MessageInput.tsx`（props 声明 `:51-54`；按钮 JSX `:254-276`）
- Modify: `desktop/src/components/MessageInput.test.tsx`（默认 props `:26-27`；用例 `:318-355`）
- Modify: `desktop/src/App.tsx`（import `:33-41`；`pickIntoSession` `:1421-1467`；`pickFilesToAttach` `:1469-1481`；`pickImagesToAttach` `:1483-1499`；接线 `:1868-1869`）
- Modify: `desktop/src/App.test.tsx`（锚点 `:2655-2681`；用例 `:2697-2853`；远端锚点 `:3100`、`:3299`）

**Interfaces:**
- Consumes: `pendingKindFromPath(path): "document" | "image"`、`attachmentProblem(path, size)`、`imageAttachmentProblem(path, size)`、`ATTACHMENT_EXTENSIONS`、`IMAGE_EXTENSIONS`、`fileNameOf(path)`（均来自 `desktop/src/lib/attachmentLimits.ts`，不改）；`isRemoteClient()`、`pickImagesOnRemote()`（App 内部，保留）。
- Produces: `MessageInput` 的新 prop `onPickAttachments: () => void`（替代 `onPickFiles`/`onPickImages`）；`App` 内的 `pickAttachments(): Promise<void>`。

- [ ] **Step 1: 先改测试，让它红（MessageInput.test.tsx）**

把默认 props 里的两行（`:26-27`）：

```ts
    onPickFiles: vi.fn(),
    onPickImages: vi.fn(),
```

改为一行：

```ts
    onPickAttachments: vi.fn(),
```

把 `attachments` describe 内 `paperclip` 下方的两个用例（`:318-355`）整体替换为：

```ts
    it("asks the parent to pick attachments from the single attach button", () => {
      const onPickAttachments = vi.fn();
      const { unmount } = renderInput({ onPickAttachments, value: "hi" });
      fireEvent.click(paperclip());
      expect(onPickAttachments).toHaveBeenCalledTimes(1);
      unmount();

      // A composer with no session is inert everywhere, the picker included.
      renderInput({ onPickAttachments, value: "hi", disabled: true });
      expect((paperclip() as HTMLButtonElement).disabled).toBe(true);
    });

    it("offers a single attach button (no separate image button)", () => {
      renderInput();
      // 文档与图片共用一个入口：图片按钮已并入回形针。
      expect(screen.queryByRole("button", { name: "附加图片" })).toBeNull();
    });
```

- [ ] **Step 2: 跑测试确认它红**

Run: `npx vitest run src/components/MessageInput.test.tsx`
Expected: FAIL — `附加图片` 按钮仍存在（`queryByRole` 非 null）；且 `onPickAttachments` 未接线（点击不触发）。

- [ ] **Step 3: 改 MessageInput.tsx 让测试转绿**

props 声明（`:51-54`）替换为：

```tsx
  /**
   * 用户点了回形针：打开文件选择器是父级的事（Tauri dialog 在 App 里）。
   *
   * 文档与图片共用一个入口：一次选择里可以混选，分类由父级按扩展名判定（见
   * `App` 的 `pickAttachments`）。白名单互斥，故路径的扩展名就足以分流。
   */
  onPickAttachments: () => void;
```

工具栏里删掉第二个按钮（`:265-276`），并把第一个按钮的 `onClick` 改掉，使该区域成为：

```tsx
            <button
              type="button"
              // 纯文本标签（仓库不用 emoji）；`aria-label` 与可见文字一致，是测试锚点。
              aria-label="附加文件"
              title="附加文件"
              className="rounded-md border border-line-strong px-2 py-0.5 text-xs text-fg-muted hover:bg-raised hover:text-fg disabled:opacity-50"
              onClick={onPickAttachments}
              disabled={disabled}
            >
              附加文件
            </button>
```

- [ ] **Step 4: 跑 MessageInput 测试确认全绿**

Run: `npx vitest run src/components/MessageInput.test.tsx`
Expected: PASS（含结构契约用例 `:217-234`：工具栏第一个控件 `aria-label === "附加文件"`）。

- [ ] **Step 5: 先改 App 测试，让它红（App.test.tsx）**

(a) 锚点与 helper（`:2655-2681`）替换为：

```ts
describe("App 附件接线", () => {
  // 文档与图片共用一个入口（合并后只有一个「附加文件」按钮）。
  const attachButton = () => screen.getByRole("button", { name: "附加文件" });
  const sendButton = () => screen.getByRole("button", { name: /^send$/i });

  /** 点附件入口触发原生多选，并等界面吸收结果（chips 或错误横幅）。 */
  async function pickWith(picked: string | string[] | null) {
    state.picks = [picked];
    await act(async () => {
      fireEvent.click(attachButton());
    });
  }

  /**
   * 点「附加文件」触发原生多选。一次选择里可以混着文档与图片：分类由扩展名决定，
   * 图片 chip 会经 `AttachmentThumb`/`useImageData` 真的发起 `image/read`，那是另一
   * 条链路的职责，这里只关心「有没有进待发列表、带的 kind 对不对」。
   */
  async function pickAttachments(picked: string | string[] | null) {
    await pickWith(picked);
  }
```

(b) 把所有 `pickFiles([...])` 调用改名为 `pickAttachments([...])`；把所有 `pickImages([...])` 调用改名为 `pickAttachments([...])`。具体位置：`:2703`、`:2739`、`:2771-2772`、`:2814`、`:2833`、`:2848`、`:2861`、`:2891`、`:2927`、`:2957`、`:2983`、`:3004`。

(c) 把「sends images before documents before the text block, and echoes only documents」用例（`:2763-2805`）的两次挑选：

```ts
    await pickFiles(["/tmp/报告.pdf"]);
    await pickImages(["/tmp/截图.png"]);
```

改为**一次混选**（本例即合并特性的旗舰用例）：

```ts
    // 一次选择里混选文档与图片，是合并入口的核心路径。
    await pickAttachments(["/tmp/报告.pdf", "/tmp/截图.png"]);
```

并把它上方的注释改为：

```ts
    // 同上一例：模拟 `image/read` 对待发绝对路径的拒绝（否则空桩会被伪造成图）。
```

（该用例余下的 `expect(call?.params)` 断言逐字不变：发送端按 `kind` 分组，线上顺序仍是「图片块、文档块、文本块」。）

(d) 删除两个「跨入口被拒」用例（合并后不再成立）：
- 「rejects an image picked through the document picker」（`:2825-2839`）—— `.png` 现在被正常收为图片。
- 「rejects a document picked through the image picker」（`:2841-2853`）—— 图片按钮已不存在。

(e) 远端 describe 的锚点（`:3100`）：

```ts
  const imageButton = () => screen.getByRole("button", { name: "附加图片" });
```

改为：

```ts
  // 远端没有文档附件：合并后的单一入口在远端回落为图片选择器。
  const attachButton = () => screen.getByRole("button", { name: "附加文件" });
```

并把 `pickImagesRemote`（`:3108-3123`）里的 `fireEvent.click(imageButton());` 改为 `fireEvent.click(attachButton());`。

(f) 「still uses the plugin-dialog path on the desktop build」用例（`:3289-3304`）里的 `fireEvent.click(imageButton());`（`:3299`）改为 `fireEvent.click(attachButton());`。

- [ ] **Step 6: 跑 App 测试确认它红**

Run: `npx vitest run src/App.test.tsx -t "附件"`
Expected: FAIL — 桌面单一入口尚未接线：`.png` 仍会被 `attachmentProblem` 拒（报「不支持的文件类型」），混选用例与图片用例拿不到 chip。

- [ ] **Step 7: 改 App.tsx 让测试转绿**

(a) import（`:33-41`）里加入 `pendingKindFromPath`：

```ts
import {
  ATTACHMENT_EXTENSIONS,
  attachmentProblem,
  fileNameOf,
  imageAttachmentProblem,
  imageSizeProblem,
  IMAGE_EXTENSIONS,
  pendingKindFromPath,
  type PendingAttachment,
} from "./lib/attachmentLimits";
```

(b) 把 `pickIntoSession`（`:1421-1467`）、`pickFilesToAttach`（`:1469-1481`）、`pickImagesToAttach`（`:1483-1499`）三段整体替换为下面两段（`pickImagesOnRemote` 保持原样不动，紧跟其后）：

```tsx
  /**
   * 一次「原生多选 → 逐个预检 → 入待发列表」的入口（桌面端）。
   *
   * 文档与图片**共用一个选择器**：`filters` 给出并集（默认）与两个收窄项，用户不必
   * 先判断文件属于哪一类；分类由路径扩展名决定（两个白名单互斥，见
   * `pendingKindFromPath`），再按类选各自的预检函数（上限不同：文档 50 MB、图片
   * 20 MB）。发送时的协议形态仍由 `kind` 决定，本入口只负责带对 `kind` 入列。
   *
   * 逐个本地预检：不合格的就地报错（写进会话错误）并丢弃，合格的才进待发列表。
   * 报错与加入互不影响——一次多选里混着好坏文件是常态，不能因为一个坏文件把
   * 整次选择作废。
   */
  const pickIntoSession = async (): Promise<void> => {
    const id = store.currentId;
    if (!id) return;
    // iOS/远端没有原生选择器：桌面端能力，在别处不假装有。远端图片走自己的路
    // （见 `pickImagesOnRemote`），同样不碰这里。
    if (isRemoteClient()) return;
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      directory: false,
      multiple: true,
      filters: [
        // 默认项是并集：一次选择里文档与图片都能选。
        { name: "全部支持的格式", extensions: [...ATTACHMENT_EXTENSIONS, ...IMAGE_EXTENSIONS] },
        { name: "文档", extensions: [...ATTACHMENT_EXTENSIONS] },
        { name: "图片", extensions: [...IMAGE_EXTENSIONS] },
      ],
    });
    // 多选回数组；单选/取消回字符串或 null。统一成数组再处理。
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length === 0) return;

    const problems: string[] = [];
    const accepted: PendingAttachment[] = [];
    for (const p of paths) {
      const name = fileNameOf(p);
      // 桌面端拿不到 file size（dialog 只给路径）：大小交给服务端权威校验，
      // 这里只做扩展名预检——传 size=0 意为"未知"，不会误判为超限，避免把
      // 注定被拒的类型发出去。
      const kind = pendingKindFromPath(p);
      const problem =
        kind === "image" ? imageAttachmentProblem(p, 0) : attachmentProblem(p, 0);
      if (problem) {
        problems.push(`${name}：${problem}`);
        continue;
      }
      accepted.push({ path: p, name, size: 0, kind });
    }
    if (accepted.length > 0) {
      setPending((prev) => ({ ...prev, [id]: [...(prev[id] ?? []), ...accepted] }));
    }
    if (problems.length > 0) {
      store.view(id).session.lastError = problems.join("；");
      force((v) => v + 1);
    }
  };
```

> `lastError` 即现状字段名（`App.tsx:1464`、`:1553`），照抄不动。

(c) 新增合并入口：

```tsx
  /**
   * 附件入口：桌面端打开一次原生多选（文档与图片混选，按扩展名分流）；远端
   * （iOS）没有共享文件系统，回落为图片专用选择器（那里本就不支持文档附件）。
   */
  const pickAttachments = (): Promise<void> =>
    isRemoteClient() ? pickImagesOnRemote() : pickIntoSession();
```

(d) 接线处（`:1868-1869`）两行：

```tsx
                onPickFiles={() => void pickFilesToAttach()}
                onPickImages={() => void pickImagesToAttach()}
```

改为一行：

```tsx
                onPickAttachments={() => void pickAttachments()}
```

- [ ] **Step 8: 跑测试确认全绿**

Run: `npx vitest run src/App.test.tsx`
Expected: PASS。

- [ ] **Step 9: 类型检查**

Run: `npx tsc --noEmit`
Expected: 无输出（0 error）。若报 `onPickFiles`/`onPickImages` 未知，说明漏改了 `MessageInput` 的 props 或 App 接线。

- [ ] **Step 10: 提交**

```bash
git add desktop/src/components/MessageInput.tsx desktop/src/components/MessageInput.test.tsx desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): merge the file and image pickers into one attach button"
```

---

### Task 2: 回归验证

**Files:** 无（只跑命令）。

**Interfaces:**
- Consumes: Task 1 的产出。
- Produces: 无（验证门）。

- [ ] **Step 1: 全量测试**

Run: `cd desktop && npx vitest run`
Expected: 全绿；重点确认 `MessageInput.test.tsx` 与 `App.test.tsx` 无失败、无 skip 进来的旧断言。

- [ ] **Step 2: 构建**

Run: `cd desktop && npx tsc --noEmit && npm run build`
Expected: `tsc` 无输出；`vite build` 成功产出 `dist/`。

- [ ] **Step 3: 全文核对残留锚点**

Run: `cd desktop && grep -rn "附加图片\|onPickFiles\|onPickImages\|pickFilesToAttach\|pickImagesToAttach" src`
Expected: 无匹配（`pickImagesOnRemote` 除外，那是保留的远端路径，不含上述任一子串）。若仍有命中，回到 Task 1 修净。

---

## Self-Review

**Spec coverage：**
- §3.1 MessageInput 单按钮 + prop 收敛 → Task 1 Step 3。
- §3.2 App 并集白名单 + `pendingKindFromPath` 判类 + 按类预检 → Task 1 Step 7(b)。
- §3.2 接线收敛 `onPickAttachments` → Task 1 Step 7(d)。
- §3.2 远端保持 `pickImagesOnRemote` → Task 1 Step 7(c)（`isRemoteClient()` 分支）。
- §3.3 不改服务端/协议/结构 → Global Constraints + 未列改动文件。
- §4 混选 → Task 1 Step 5(c) 的旗舰用例。
- §4 不支持类型仍被拒 → Task 1 Step 5(b) 保留的 `.mov` 用例。
- §4 远端仅图片 → 远端 describe 锚点改名（Step 5(e)）。
- §5 测试判据 → Task 1 Step 5 全部，Task 2 全量回归。
- §5 「distinct buttons」用例删除 → Step 5(d)（另一个在 Step 1）。
- §7 代价（默认并集、可就地报错）→ 已由 §7 记录，无需代码。

**Placeholder scan：** 无 TBD/TODO；每个代码步骤给了可直接粘贴的代码。Step 7(b) 对 `session.lastError` 字段名留了一句「以现状为准」的护栏——这是为了防止示意名与仓库现状不符，非待办项。

**Type consistency：** `onPickAttachments`（MessageInput prop）与 App 接线 `onPickAttachments={...}` 同名；`pickAttachments`（App 函数）、`pickIntoSession`（App 函数）、`pendingKindFromPath`（lib 函数）在测试与实现两侧一致；`PendingAttachment["kind"]` 取值 `"document" | "image"` 与 `pendingKindFromPath` 返回一致。
