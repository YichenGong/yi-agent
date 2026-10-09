# 图片输入与显示设计（桌面 + iOS）

**目标：** 用户能把图片作为输入发给 agent，并且对话流里能**看到图片本身**——既包括用户自己发出去的图，也包括 agent 通过 `view_image` 看到的图。

**状态：** 设计已确认，待转实现计划。

**范围：** 两个平台（桌面 + iOS/remote）。图片进入模型上下文（路线 1：作为 user 消息的内容块直接进上下文，不是「落盘后由 agent 用工具按需读」）。

**不做：** 图片生成 / 编辑、拖拽入口、`detail` 的模型能力门控、图片 OCR、日志瘦身（见 §7 决策 A）。

---

## 1. 问题与已核实事实

当前端到端**不支持任何图片输入**，且对话流不显示图片。已核实的事实（含代码位置）：

**底座已存在**
- 类型层已支持图片：`ContentBlock::Image { source, detail }` / `ImageSource::{Base64, Url}`（`yi-agent-rs/crates/yi-agent-core/src/message.rs:36`）。
- 两个 provider 都已序列化图片：Anthropic（`yi-agent-llm/src/anthropic/types.rs:125`）、OpenAI（`yi-agent-llm/src/openai/types.rs:55` 的 `OpenaiContentPart::ImageUrl`）。
- `view_image` 工具已能读工作区图片并产出 `ContentBlock::Image`：PNG/JPEG/GIF/WebP，解码 + 缩放 + 预算降级（`yi-agent-tools/src/fs/view_image.rs`）。
- compaction 已把图片计入 token：`ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE`（`yi-agent-core/src/compact.rs:95`），`agent.rs:1645` 同口径。

**缺的是入口与显示**
- 前端文件选择器只认文档扩展名：`ATTACHMENT_EXTENSIONS = [pdf, docx, txt, md, csv, html, htm, rtf]`（`desktop/src/lib/attachmentLimits.ts:12`），图片被挡在选择器外；`attachmentProblem`（同文件 `:43`）因此拒收图片。
- 服务端附件**只传路径、不传内容**：`turn/start` 的 `input` block 只有 `{type:"text"}` / `{type:"attachment", path}`，`parse_input` 只识别这两种（`yi-agent-app-server/src/attachments.rs:139`）。附件由 agent 用 `read_document` 按需读，而 `read_document` **不支持图片**（`read_document.rs:425` 只分派 pdf/docx/html/txt/md/csv）。
- `agent.run()` 只接受纯文本：`pub async fn run(&mut self, user_prompt: String)`（`yi-agent-core/src/agent.rs:631`），内部 `session.push(Message::user(user_prompt))`。没有任何「接收内容块」的入口。
- 对话气泡只渲染文字 + 文件名 chip，不渲染图片：`desktop/src/components/ChatView.tsx:38`（`userMessage` 分支）。
- **工具结果的图片被丢弃**：`translate.rs:413` 的 `render_content` 把 `ContentBlock::Image` 翻成占位字符串 `[image]`（`translate.rs:424`），`Item::ToolCall.result` 是 `Option<String>`（`app-server/src/protocol.rs:487`）。agent 看到的图因此无法回显。
- 前端没有读文件字节的 RPC（无 `file/read` / `image/read`），也没有静态资源服务（`server.rs` 无 `ServeDir`）。

**协议与传输约束**
- **单帧上限 1 MiB**：`MAX_FRAME_BYTES = 1024 * 1024`（`app-server/src/protocol.rs:8`）。出站超限报错（`transport.rs:41`），**入站超限直接断连**（`ws.rs:462`）。已有的分片先例：`chunk_items_for_replay` 按条数与字节切分（`server.rs:4873`，测试断言每片 < 900 KB）。
- **iOS 是 remote ws 客户端**：没有 Tauri 原生选择器、拿不到本地绝对路径（`App.tsx:1353` 的 `pickFilesToAttach` 对 remote 直接 no-op）；未装 `tauri-plugin-fs`。ws 单帧上限同 1 MiB，relay 逐帧透传（`yi-agent-relay/src/lib.rs:152`）。故 iOS 的图片**只能作为字节分片上传**。
- **iOS 权限现状**：`desktop/src-tauri/Info.ios.plist` 只声明了 `NSCameraUsageDescription`（配对扫码用），无相册权限项。

**持久化约束**
- thread 日志按 `Vec<Message>` **原样落盘**：`TurnLine::Turn { items, usage, messages }`（`app-server/src/thread_store.rs:78`/`:92`），写入点 `persist_and_finish_turn`（`server.rs:5217`）。图片块的 base64 会一并写进 JSONL。
- `.yi-agent/` 是既有约定（`threads/`、`runtime/`、`preferences.json`、`attachments/`），根仓库 `.gitignore` 已含 `.yi-agent/`。

**已登记项**
- `docs/project-management/desktop.md:93`：「图片附件输入 — 判据：输入框可附加图片并随 prompt 发送」（P2 路线图，未实现）。
- `docs/bug-list.md:27`：「compaction 的 token 估算把 `ContentBlock::Image` 记为 0 token」——**过期条目**：代码已在 commit `b7a52a25` 修复（`compact.rs:95`），文档未同步。本次只做文档同步，不重复改代码。

**工具产出的图片当前已进日志**：`view_image` 返回的 base64 经 `persist_and_finish_turn` 一并落盘，故「显示 agent 看的图」不需要新增落盘机制，只需让前端能取到它。

## 2. 关键决策（逐条已确认）

1. **端到端、两个平台**：桌面与 iOS 都要能发图；对话流要能显示用户图与 agent 图。
2. **路线 1：图片直接进 user 消息上下文**（不做「落盘 + agent 按需 `view_image`」）。理由：图片的语义是「这一轮我看着这张图说话」，走工具会引入「模型可能不读图」的不确定性。代价是 core 要开一个接收内容块的口子。
3. **iOS 走服务端分片上传**（不做「客户端压成单帧内联」）。理由：服务端一套管线（解码/缩放/编码）两端共用，保留原图；客户端压缩会把特性劈成两套行为。
4. **显示走 `image/read` 拉字节**（不做逐 item 内联 base64 / 静态资源服务）。理由：1 MiB 帧限制下内联必然要分片，RPC + 前端缓存的读写方向更清晰、可缓存、可 LRU。
5. **图片落盘沿用 `attachments/<thread_id>/`**，图片块同时携带 `path`（相对 workspace 的路径）。理由：与既有文档附件同构，`thread/delete` 清理逻辑直接复用，且给「日志瘦身」留下升级点（§7-A）。
6. **日志里直接存 base64**（§7 决策 A）。
7. **HTTP 取图 + 分片上传**：见 §4。

## 3. 协议改动（向后兼容）

**(1) `turn/start` 的 `input` 新增图片 block：**

```json
{ "type": "image", "path": "<绝对路径>" }
```

`path` 是桌面文件选择器给的绝对路径；服务端负责复制 + 编码。`turn/interject` 保持只认文本（与附件现状一致）。

**(2) `Item` 新增图片引用：**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    /// 相对 workspace 的路径（`.yi-agent/attachments/<tid>/<hash>-name`）。
    pub path: String,
    pub media_type: String,
    pub size: u64,
    /// 编码时用的 detail 档位，供前端换算/展示。
    pub detail: ImageDetail,
}
```

`Item::UserMessage`、`Item::UserInterjection` 新增 `#[serde(default, skip_serializing_if = "Vec::is_empty")] images: Vec<ImageRef>`；`Item::ToolCall` 新增同名可选字段（与 `result` 并列，`skip_serializing_if` = 空）。

**关键约束：item 里只有引用（路径/类型/大小），绝不含 base64。** 帧上限 1 MiB 是硬约束，item 是每条通知都带的东西，塞图片必然炸帧。

**(3) 新增 RPC：**

| 方法 | 参数 | 结果 |
|---|---|---|
| `image/read` | `{ path, offset?, maxBytes? }` | `{ data: <base64 分片>, nextOffset: number \| null, mediaType, size }` |
| `image/upload/begin` | `{ threadId, name, mime, size }` | `{ uploadId, chunkSize }` |
| `image/upload/chunk` | `{ uploadId, index, data: <base64 分片> }` | `{}` |
| `image/upload/commit` | `{ uploadId }` | `{ uploadId }`（服务端把 staging 收拢为最终文件；`turn/start` 用 `{type:"uploaded_image", uploadId}` 引用它） |
| `image/upload/abort` | `{ uploadId }` | `{}` |

`image/read` 的响应分片大小由服务端定（≤ ~600 KB 原始字节 → base64 后 < 800 KB < 1 MiB），前端按 `nextOffset` 循环拉取直到 `null`。`path` 经 `resolve_and_check` 限根。

## 4. 分层设计

### 4.1 core：接收内容块的入口

新增

```rust
pub async fn run_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<BoxStream<'static, AgentEvent>, AgentError>;
```

`run(text)` 改为 `run_blocks(vec![ContentBlock::Text(text)])` 的薄封装。`start_run` 由 `Option<String>` 改为 `Option<Vec<ContentBlock>>`（`agent.rs:656` 处的 `Message::user` 换成 `Message { role: User, content: blocks }`）。

这是**通用改动**（任何多模态输入都走这条路），不是为图片特设。`retry_current_session`（`start_run(None)`）不受影响。

`ContentBlock::Image` 增加可选 `path`：

```rust
Image {
    source: ImageSource,
    #[serde(default)]
    detail: ImageDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
}
```

现有解构点（`compact.rs:95`/`:412`、`agent.rs:1645`、`translate.rs:424`、`anthropic/types.rs:125`）都用了 `{ .. }` 或单字段，加字段不破坏；`message.rs` 的两个测试需补字段。`path` 的用途：前端显示定位、`image/read` 取值、日志瘦身升级点。

### 4.2 tools：抽出可复用的图片处理管线

`view_image.rs` 里的 `load_image` / `encode_within_budget` / `resize_to_cap` / `lossless_encode` / `jpeg_encode` / `media_type_for` 抽到新模块 `yi-agent-tools/src/image_prep.rs`，公开：

```rust
pub struct PreparedImage { pub media_type: String, pub data: String, pub width: u32, pub height: u32, pub note: Option<String> }
pub fn prepare_image_bytes(bytes: &[u8], detail: ImageDetail, budget: usize) -> Result<PreparedImage, ToolsError>;
pub fn prepare_image_file(path: &Path, detail: ImageDetail, budget: usize) -> Result<PreparedImage, ToolsError>;
```

`ViewImageTool` 改为调用它（行为逐字不变，现有 `view_image` 测试即回归网）。app-server 依赖 `yi-agent-tools` 已有（`Cargo.toml` 核实），直接复用，**不新增图片依赖**（`image` / `base64` 已在 tools 的依赖里）。

### 4.3 app-server：摄取、注入、显示

**(a) 桌面路径摄取**：`prepare_turn`（`server.rs:6447`）里 `parse_input` 增加 `{type:"image", path}` 分支。图片与文档附件同等处理：`store_attachment` 复制进 `.yi-agent/attachments/<tid>/<hash>-name`（`attachments.rs:70`），然后调 `image_prep::prepare_image_file` 得到 `(media_type, base64)`。

- 图片**不**进「Attached files (read them with read_document)」清单（那份清单是给 `read_document` 用的）；图片直接作为内容块。
- 文档附件仍只进清单（现有行为不变）。

**(b) iOS 分片上传**：`image/upload/begin` 按 `threadId` 解析 cwd，分配 `uploadId`，在 `.yi-agent/attachments/<tid>/.tmp/<uploadId>` 落一个 staging 文件；`chunk` 按 `index` 顺序追加写入；`commit` 把 staging 收拢成与 (a) 相同的 `store_attachment` 结果 + 同一 `prepare_image_file` 编码，此后清理 staging。客户端拿到 `uploadId` 后，在本轮 `turn/start` 的 `input` 里用 `{type:"uploaded_image", uploadId}` 引用它，`prepare_turn` 在服务端解析为已落盘的图像块。

> 收敛点：无论桌面路径还是 iOS 上传，最终都落到**同一个** `store_attachment` + `prepare_image_file`，后续完全一致。

**(c) 注入模型**：driver（`server.rs:5510` 的 `agent.run(prompt)`）改为：

- 文本 = 用户原话（不带图片清单）；
- `blocks = [Text(文本), Image{source: Base64{...}, detail, path}, ...]`；
- 调 `agent.run_blocks(blocks)`。

文档附件的清单注入保持在文本里（现有 `prompt_with_attachments` 不变，只在有文档附件时生效）；图片走内容块。两者可共存。

**(d) 显示**：`translate.rs` 的 `render_content` 仍产出文本（`result` 是给模型/日志看的字符串），但在 `AgentEvent::ToolResult` 分支里**额外**扫描 `result.content` 里的 `ContentBlock::Image { path, .. }`，收进 `Item::ToolCall.images`。用户消息同理：`prepare_turn` 把本轮的 `ImageRef` 放进 `Item::UserMessage.images`（现已在 `server.rs:5206` 附近构造该 item）。

**(e) `image/read`**：`resolve_and_check` 限根 → 确认文件存在、能 `image::guess_format` 识别为图片 → 按 offset/maxBytes 读分片 → base64 回传。**只服务可解码的图片**，不引入任意文件读取。

### 4.4 desktop：选择、上传、显示

- **图片 chip**：`PendingAttachment` 增加 `kind: "document" | "image"`（`attachmentLimits.ts`）；`pickFilesToAttach`（`App.tsx:1353`）扩成可选图片（桌面加一个独立按钮或扩 `filters`，`ATTACHMENT_EXTENSIONS` 旁新增 `IMAGE_EXTENSIONS = [png, jpg, jpeg, gif, webp]`）；`AttachmentChips` 对图片显示缩略图。
- **发送**：桌面把图片路径放进 `input`（`{type:"image", path}`）；iOS 先上传、commit 后用返回句柄放进 `input`。
- **iOS 选择器**：`<input type="file" accept="image/*">`（webview 原生，系统相册交付）。取字节 → 分片 `image/upload/*`。若交付格式是 HEIC（`image` crate 不认），客户端先用 canvas 转 JPEG；转不了则明确报错。
- **显示**：`ChatView` 的 `userMessage` 分支与 `ToolCallCard` 渲染 `<img>`；抽一个 `useImageData(path)` hook：`image/read` 分片循环 → Blob URL 或 data URL；模块级 LRU 缓存（键 = `path + 编码 size + detail`），避免滚动重取。渲染上沿用 `MarkdownText` 已有的 `prose-img:max-w-full` 风格约束（`MarkdownText.tsx:46`）。
- **iOS/remote 与桌面共用**：两者都调 `image/read`（桌面走 Tauri IPC，iOS 走 ws），无平台分支。

### 4.5 安全与上限

- 上传：单图上限 **20 MiB**（同 `view_image` 的 `MAX_IMAGE_BYTES`，`view_image.rs:15`）；分片 ≤ ~700 KB（base64 后 < 1 MiB）；上传会话有 **TTL**（如 10 分钟）与**并发条数上限**；`abort`/超时清理 `.tmp/<uploadId>`。
- `image/read` 限根 + 图片识别 + 分片上限。
- 文件名经既有 `sanitize_filename`（`attachments.rs:35`）防目录穿越。
- iOS 相册权限：`<input type="file" accept="image/*">` 走系统选择器，由系统弹权限，应用本身不直接读取相册——**实测确认**是否需要额外 `NSPhotoLibraryUsageDescription`（若系统选择器已覆盖则不加）。

## 5. 测试判据

**core**
- `run_blocks` 把图片块推进 session：`cargo test -p yi-agent-core --lib`（新增用例断言 `run_blocks([Text, Image])` 后 session 首条消息含 `ContentBlock::Image`）。
- `Image` 的 `path` 字段 serde 往返 + 缺省 `None`（旧数据可反序列化）。
- `run(text)` 行为不回归（现有 `agent` 测试即回归网）。

**tools**
- `image_prep::prepare_image_file` 复现 `view_image` 的既有断言（解码/缩放/预算降级），`view_image` 测试全绿即证明抽提无行为变化。

**app-server**
- `turn/start` 带 `{type:"image", path}` → 文件被复制进 `attachments/<tid>/`、user 消息含 `ContentBlock::Image`、`Item::UserMessage.images` 非空、model prompt 无图片清单。
- `image/read` 分片往返：多次请求按 `nextOffset` 拼接后与源字节一致；越界路径被拒；非图片文件被拒。
- `image/upload/begin|chunk|commit`：上传后落盘内容正确、`commit` 返回的句柄能用于 `turn/start`；`abort`/超时清理 staging；超限/条数超限被拒。
- `translate`：`ToolResult` 含 `ContentBlock::Image` → `Item::ToolCall.images` 含该引用，且 item 序列化后**不含 base64 子串**。
- `thread/delete` → 图片随附件一并清理（复用现有覆盖）。

**desktop**
- chips 增删、图片 chip 渲染缩略图、`image/read` 分片客户端拼接、LRU 缓存命中不重复请求。
- 气泡与工具卡渲染 `<img>`（`data-testid` 锚点）。
- 回归：`cd desktop && npx vitest run && npx tsc --noEmit && npm run build`；`cargo test -p yi-agent-core -p yi-agent-tools -p yi-agent-app-server`。

## 6. 范围

**改动文件**
- `yi-agent-core/src/{agent.rs,message.rs}`（`run_blocks` + `path` 字段）
- `yi-agent-tools/src/{image_prep.rs（新增）,fs/view_image.rs,lib.rs}`
- `yi-agent-app-server/src/{protocol.rs,server.rs,translate.rs,attachments.rs（图片分支）}`
- `desktop/src/{App.tsx,lib/attachmentLimits.ts,lib/protocol.ts,lib/rpc.ts,components/{MessageInput,AttachmentChips,ChatView,ToolCallCard}.tsx,lib/useImageData.ts（新增）}`
- `desktop/src-tauri/Info.ios.plist`（若实测需要相册权限项）
- `docs/project-management/{desktop.md, yi-agent-app-server.md, yi-agent-core.md, yi-agent-tools.md}`、`docs/bug-list.md`、`README.md`（计数同步）

**不做**
- 图片生成 / 编辑；拖拽；`detail` 能力门控（项目无 modality 元数据）；`read_document` 支持图片（图片不走文本抽取）；日志瘦身（§7）。

## 7. 已知代价与后续升级点

**A. 日志直接内联 base64（v1 决策）**：模型看的图按最长边 2048、预算处理过（典型手机照 ≈ 0.3–0.6 MB → base64 ≈ 0.5–0.8 MB），**每张图在日志里约占 0.5 MB**；30 张图 ≈ 15 MB，replay 需整体解析。选择 v1 直接存（无 rehydration 失败面、无新增钩子），`path` 字段已留好升级点：将来可改为「只存 `path`、加载时按需重编码」，届时需处理「文件被改/删」的降级分支。

**B. 帧上限决定了所有大数据都要分片**：`image/read` 与 `image/upload/*` 都是分片协议；前端要正确处理 `nextOffset` 循环与失败重试。

**C. bug-list 过期条目**：`docs/bug-list.md:27` 标 `[ ]` 但代码已修（`compact.rs:95`，commit `b7a52a25`）。本次同步改为 `[x]` 并注明修复位置，不重复改代码。
