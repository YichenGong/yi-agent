# 单一附件入口设计（桌面端）

**目标：** 桌面端把「附加文件」与「附加图片」两个按钮合为一个入口：点一次、开一次原生对话框、文档与图片在同一次选择里都能选，按扩展名自动分流。

**状态：** 设计已确认（方案 A），待转实现计划。

**范围：** 仅桌面端（Tauri 原生对话框）。远端（iOS/remote）维持图片专用路径不变。

**不做：** 远端 iOS 的文档附件；不改服务端协议与运行时的附件摄取；不改发送时的协议形态（文档仍走清单、图片仍走内容块）；不动按钮文案外的工具栏布局。

---

## 1. 问题与已核实事实

工具栏里并列两个按钮（`desktop/src/components/MessageInput.tsx:254-276`）：「附加文件」调 `onPickFiles`，「附加图片」调 `onPickImages`。两者样式、禁用逻辑逐字相同，唯一区别是标签与 handler。

**已核实的事实（含代码位置）**

- 两个 handler 汇到**同一个骨架** `pickIntoSession()`（`desktop/src/App.tsx:1421`），只差四个参数：`filterName`、`extensions`、`problem`、`kind`。

  - `pickFilesToAttach`（`App.tsx:1475`）：`filterName="文档"`、`extensions=ATTACHMENT_EXTENSIONS`、`problem=attachmentProblem`、`kind="document"`。
  - `pickImagesToAttach`（`App.tsx:1491`）：非远端时同上，用 `filterName="图片"`、`IMAGE_EXTENSIONS`、`imageAttachmentProblem`、`kind="image"`；远端时转 `pickImagesOnRemote()`。

- 两个白名单**互斥**：`ATTACHMENT_EXTENSIONS = [pdf, docx, txt, md, csv, html, htm, rtf]`（上限 50 MB）与 `IMAGE_EXTENSIONS = [png, jpg, jpeg, gif, webp]`（上限 20 MB）（`desktop/src/lib/attachmentLimits.ts:12-32`）。

- **已有分类能力**：`pendingKindFromPath(path)`（`attachmentLimits.ts:109`）按扩展名把路径判成 `document` / `image`，正是合并后需要的分流函数；其注释明确「两个白名单不重叠，故扩展名足以判定类别」。

- 发送时的协议形态由 `kind` 决定（`PendingAttachment.kind`，`attachmentLimits.ts:35-53`）：文档走 `{type:"attachment", path}`（agent 用 `read_document` 按需读），图片走 `{type:"image", path}`（服务端摄取成内容块）。**合并只换入口，不碰协议**。

- **例外路径**：远端（iOS webview）`isRemoteClient()` 为真时没有 Tauri 原生选择器；`pickIntoSession` 对远端直接 no-op（`App.tsx:1435`），图片走 `pickImagesOnRemote()`（`App.tsx:1515`，`<input type="file" accept="image/*">` + 分片上传 + HEIC 转码）。远端本就不支持文档附件。

- 当前「分开入口」的理由（`App.tsx:1472`、`MessageInput.tsx:267`）是「白名单/上限不同，分开用户才不会挑错之后才被告知」。合并后该理由不成立：白名单互斥意味着单选择器里类型不匹配的文件会被就地预检拦下并报错（`pickIntoSession` 的 `problems` 呈现，`App.tsx:1463`），体感并不更差，反而少一次「猜哪个按钮」的判断。

## 2. 关键决策

1. **一个按钮 + 一次选择 + 按扩展名自动分流**（方案 A，用户已确认）。不做「一个按钮弹菜单再二选一」。
2. **按钮文案沿用「附加文件」**，`aria-label` 不变。少动既有测试锚点，也让工具栏第一个控件仍是左组（结构契约见 `MessageInput.test.tsx:217-234`）。
3. **对话框 `filters` 三项**：默认项「全部支持的格式」（两套扩展名的并集）、「文档」、「图片」。默认项让用户不必先想「这是文档还是图片」；后两项保留按类收窄的能力。
4. **远端不变**：合并后的单按钮在远端回落为 `pickImagesOnRemote()`（图片专用）。副作用是远端少了一个本来就 no-op 的「附加文件」按钮，属改善。

## 3. 分层设计

### 3.1 `MessageInput`（`desktop/src/components/MessageInput.tsx`）

- props 收敛：删 `onPickFiles` / `onPickImages`，改为单个 `onPickAttachments: () => void`。
- 工具栏里删掉「附加图片」按钮，只留一个「附加文件」按钮（`aria-label`/`title`/文案不变），`onClick` 指向 `onPickAttachments`。
- 其余（`sendDisabled`、chip 行、布局契约）不动。

### 3.2 `App`（`desktop/src/App.tsx`）

- `pickIntoSession` 泛化：新增一条入口 `pickAttachments`，改为**单一并集白名单 + 逐路径判类**：

  - `filters` = `[{name:"全部支持的格式", extensions:[...ATTACHMENT_EXTENSIONS, ...IMAGE_EXTENSIONS]}, {name:"文档", extensions:ATTACHMENT_EXTENSIONS}, {name:"图片", extensions:IMAGE_EXTENSIONS}]`；
  - 对每个选中路径：`kind = pendingKindFromPath(p)`，再按 `kind` 选预检函数（`imageAttachmentProblem` / `attachmentProblem`），`size=0`（桌面端 dialog 只给路径，超限交服务端权威校验，沿用现状），通过后带该 `kind` 入列。

  > 实现取「泛化 `pickIntoSession`」而非「在 `pickFilesToAttach` 里硬编码并集」：让 `pickIntoSession` 接受「白名单 → 判类」的策略（或直接在函数内用 `pendingKindFromPath` 判类），保持「一次原生多选 → 逐个预检 → 入待发列表」的骨架只有一份。

- 删除 `pickFilesToAttach` 与 `pickImagesToAttach` 的**桌面分支**；保留 `pickImagesOnRemote` 不动。合并后的入口 `pickAttachments`：`isRemoteClient() ? pickImagesOnRemote() : pickIntoSession({...并集...})`。
- 接线处（`App.tsx:1868-1869`）把 `onPickFiles` + `onPickImages` 换成 `onPickAttachments={() => void pickAttachments()}`。

### 3.3 不改的部分

服务端 `parse_input` / `store_attachment` / `image_prep`、`PendingAttachment` 结构、`AttachmentChips`、发送逻辑（`kind` 决定 block 形态）全部不动。

## 4. 边界情况

- **一次选择里混着文档 + 图片**：逐路径判类，各自带正确 `kind` 入列，`problems` 里只列被拒的。这是合并后新增、也是最需要测的路径。
- **不支持的类型**：默认「全部支持的格式」只含并集扩展名；但用户仍可切到「全部文件」式收窄/放宽（Tauri 允许用户选任意，视平台而定），非白名单类型由 `attachmentProblem` 就地报「不支持的文件类型」。行为与现状一致。
- **远端**：单按钮 → `pickImagesOnRemote()`，仅图片；文档附件在远端仍不可用（既有约束，不扩大范围）。
- **同名/重复**：与现状一致，不新增去重（`pickIntoSession` 现状即不去重）。

## 5. 测试判据

**`desktop/src/components/MessageInput.test.tsx`**
- 删「keeps documents and images on distinct buttons」与「disables the image picker...」，替换为：单按钮点击调用 `onPickAttachments` 恰好一次；`disabled` 时该按钮置灰。
- `renderInput` 默认 props：`onPickFiles`/`onPickImages` → `onPickAttachments`。
- 结构契约用例（`:231-233`）断言工具栏第一个控件 `aria-label === "附加文件"`：**保留**（合并后仍成立）。

**`desktop/src/App.test.tsx`**
- `describe("App 附件接线")` 的 `paperclip`/`imageButton` 两个锚点收敛为一个 `attachButton`；`pickFiles`/`pickImages` 两个 helper 并为一个。
- 新增用例：**一次选择里同时给文档与图片路径** → `turn/start` 的 `input` 里既有 `{type:"attachment", path}` 又有 `{type:"image", path}`，顺序为「所有附件 block 在文本 block 之前」（沿用既有顺序契约，`:2716`）。
- 保留既有用例：不支持类型报错、chips 增删、只带附件可发送。
- 远端用例（`App.test.tsx:3100` 附近）：合并后单按钮在远端仍触发 `<input type="file" accept="image/*">` 链路，断言不变（锚点改名）。

**回归命令**
- `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`。

## 6. 范围

**改动文件**
- `desktop/src/components/MessageInput.tsx`
- `desktop/src/App.tsx`
- `desktop/src/components/MessageInput.test.tsx`
- `desktop/src/App.test.tsx`

**不做**
- 服务端任何改动；`attachmentLimits.ts` 的白名单/预检函数（被直接复用，不改）；远端文档附件；按钮文案改为「附件」；工具栏布局调整。

## 7. 已知代价

- 合并后用户在一个选择器里可能挑到「本来该走另一个入口」的类型——由就地预检报错兜住，代价是从「按钮层面防错」降级为「选择器层面报错」，换取少一个按钮与更少的心智负担。
- 默认过滤器是并集，OS 对话框的默认视图会同时显示文档与图片；这是本设计想要的（一次选完），但对「只想挑文档」的用户少了一层收窄，需要点一下过滤器——「文档」项即为它保留。
