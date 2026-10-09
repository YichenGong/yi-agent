# 图片输入与显示 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 用户能在桌面端与 iOS 端把图片发给 agent（图片作为内容块直接进模型上下文），并且对话流能显示用户发的图与 agent `view_image` 看到的图。

**Architecture:** 五层各改一点。core 开一个「接收内容块」的口子（`Agent::run_blocks`）并给图片块记录来源 `path`；tools 把 `view_image` 里私有的图片处理管线抽成公共 `image_prep` 模块供 app-server 复用；app-server 负责摄取（桌面路径 / iOS 分片上传）、把图片注入 user 消息、并在协议 `Item` 上暴露图片**引用**（绝不含 base64）；desktop 负责选择、上传与渲染。

**Tech Stack:** Rust（`image` / `base64` / `serde` / `axum` ws）、React 19 + TypeScript + Vite + Vitest（Tailwind 4）。

**设计文档：** `docs/superpowers/specs/2026-10-09-image-input-design.md`

## Global Constraints

- **单帧上限 1 MiB**：`MAX_FRAME_BYTES = 1024 * 1024`（`yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:8`）。出站超限报错，**入站超限直接断连**。任何 RPC 参数/响应都不得超出。
- **协议 `Item` 里绝不含 base64**：只放引用（`path` / `media_type` / `size`）。
- **路径安全**：所有服务端读文件都经 `resolve_and_check`（限 root，防穿越）；文件名经 `sanitize_filename`。
- **`image/read` 只服务可解码的图片**：不引入任意文件读取能力。
- **单图上限 20 MiB**：与 `view_image` 的 `MAX_IMAGE_BYTES` 一致（`yi-agent-tools/src/fs/view_image.rs:15`）。
- **分片原始字节 ≤ 512 KiB**（base64 后 ≈ 683 KiB < 1 MiB，留出 JSON 与帧封装余量）。
- **分支规范**：所有改动在 worktree 分支上进行，**严禁**直接在 `main` 上提交。用 `git worktree add .worktrees/feat-image-input -b feat/image-input`（本计划已在该分支）。合并走 `git merge --no-ff`。
- **提交前必须** `cd yi-agent-rs && cargo fmt --all`；commit message 用 conventional commits，**不写** `Co-Authored-By`。
- **测试纪律**：按 crate 跑，不要 `cargo test --workspace`（见 `CLAUDE.md`）。跑前 `ps aux | grep -c '[c]argo'` 确认无残留。
- **后端测试命令前缀**：`cd yi-agent-rs && cargo test -p <crate> ...`
- **前端测试命令前缀**：`cd desktop && npx vitest run <file>`
- **不做**：图片生成/编辑、拖拽、`detail` 能力门控、`read_document` 支持图片、日志瘦身。

---

## File Structure

**新增**
- `yi-agent-rs/crates/yi-agent-tools/src/image_prep.rs` — 公共图片处理管线（解码/缩放/编码/预算降级）。
- `yi-agent-rs/crates/yi-agent-app-server/src/image_upload.rs` — iOS 分片上传的 staging 注册表与收拢逻辑。
- `desktop/src/lib/useImageData.ts` — `image/read` 分片拉取 + 内存缓存。
- `desktop/src/components/AttachmentThumb.tsx` — 图片 chip 的缩略图。

**修改（后端）**
- `yi-agent-rs/crates/yi-agent-core/src/message.rs` — `ContentBlock::Image.path`。
- `yi-agent-rs/crates/yi-agent-core/src/agent.rs` — `run_blocks`。
- `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`、`fs/view_image.rs` — 改用 `image_prep`。
- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` — `ImageRef` + `Item.*.images`。
- `yi-agent-rs/crates/yi-agent-app-server/src/attachments.rs` — 图片输入解析。
- `yi-agent-rs/crates/yi-agent-app-server/src/session.rs` — `TurnPrompt.image_blocks`。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — 摄取/注入/`image/read`/上传 RPC。
- `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` — 工具结果图片 → 引用。

**修改（前端）**
- `desktop/src/lib/protocol.ts`、`lib/attachmentLimits.ts`、`lib/imageUpload.ts`、`lib/rpc.ts`
- `desktop/src/App.tsx`
- `desktop/src/components/{MessageInput,AttachmentChips,ChatView,ToolCallCard}.tsx`
- `desktop/src-tauri/Info.ios.plist`（仅当实测需要相册权限项）

**文档**
- `docs/project-management/{desktop,yi-agent-app-server,yi-agent-core,yi-agent-tools}.md`、`docs/bug-list.md`、`README.md`

---

## Task 1: core — 图片块记录来源 `path`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/message.rs`（`ContentBlock::Image`，约 `:36`；测试约 `:147`、`:194`、`:207`）

**Interfaces:**
- Consumes: 无。
- Produces: `ContentBlock::Image { source: ImageSource, detail: ImageDetail, path: Option<String> }` —— 供 Task 5/7/8 填值、Task 9 前端显示定位。

- [ ] **Step 1: 写失败测试**

在 `message.rs` 的 `mod tests` 末尾加：

```rust
#[test]
fn image_block_carries_an_optional_source_path() {
    let block = ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAA".into(),
        },
        detail: ImageDetail::High,
        path: Some(".yi-agent/attachments/t1/deadbeef-x.png".into()),
    };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(block, back);
}

#[test]
fn image_block_without_path_defaults_to_none() {
    // 旧会话里没有 `path` 字段，必须仍能反序列化。
    let json = r#"{"Image":{"source":{"Base64":{"media_type":"image/png","data":"AAA"}},"detail":"High"}}"#;
    let block: ContentBlock = serde_json::from_str(json).unwrap();
    match block {
        ContentBlock::Image { path, .. } => assert!(path.is_none()),
        _ => panic!("expected Image"),
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib message::tests::image_block`
Expected: 编译失败（`missing field path`）——这就是失败。

- [ ] **Step 3: 实现**

在 `message.rs` 把 `Image` 变体改为：

```rust
    Image {
        source: ImageSource,
        #[serde(default)]
        detail: ImageDetail,
        /// 图片的来源路径（相对 workspace 根），供前端显示与 `image/read` 定位。
        /// 旧序列化数据没有这个字段，缺省 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
```

同步给已有的三处构造/断言补 `path` 字段：`nested_tool_result_content`（`message.rs:148`）、`image_source_url_serde_roundtrip`（`:169`）、`image_detail_roundtrips_original`（`:193`）里的构造统一补 `path: None,`。（`image_detail_defaults_to_high_when_absent`（`:181`）是反序列化 JSON、无需改。）

- [ ] **Step 4: 跑测试确认通过 + 全 crate 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib message::`
Expected: PASS（全部 message 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/message.rs
git commit -m "feat(core): record an optional source path on image blocks"
```

---

## Task 2: core — `Agent::run_blocks` 接收内容块

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（`run` 约 `:631`，`start_run` 约 `:644`）

**Interfaces:**
- Consumes: Task 1 的 `ContentBlock`。
- Produces:
  - `pub async fn run_blocks(&mut self, blocks: Vec<ContentBlock>) -> Result<BoxStream<'static, AgentEvent>, AgentError>`
  - `pub async fn run(&mut self, user_prompt: String) -> ...`（行为不变，改为薄封装）
  - 内部 `start_run(&mut self, blocks: Option<Vec<ContentBlock>>)`。
  - 供 Task 5 调用。

- [ ] **Step 1: 写失败测试**

在 `agent.rs` 的 `mod tests` 里加（沿用该文件已有的 mock provider 构造方式，找到一个现成的 `Agent` 构造 helper）：

```rust
#[tokio::test]
async fn run_blocks_pushes_a_multimodal_user_message() {
    let mut agent = /* 复用本文件既有的 mock agent 构造 */;
    let blocks = vec![
        ContentBlock::Text("看看这张图".into()),
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
            detail: ImageDetail::High,
            path: Some(".yi-agent/attachments/t1/x.png".into()),
        },
    ];
    let mut stream = agent.run_blocks(blocks).await.unwrap();
    // drain 到结束
    while let Some(ev) = stream.next().await {
        if matches!(ev, AgentEvent::Done { .. }) {
            break;
        }
    }
    let msgs = agent.session().messages();
    let first = msgs.first().expect("the user message must be in the session");
    assert_eq!(first.role, Role::User);
    assert!(matches!(first.content[0], ContentBlock::Text(_)));
    assert!(matches!(first.content[1], ContentBlock::Image { .. }));
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib run_blocks_pushes`
Expected: 编译失败（`no method named run_blocks`）。

- [ ] **Step 3: 实现**

把 `run`（`agent.rs:631`）改为薄封装并新增 `run_blocks`：

```rust
    /// 用纯文本开一轮。
    pub async fn run(
        &mut self,
        user_prompt: String,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.run_blocks(vec![ContentBlock::Text(user_prompt)]).await
    }

    /// 用一组内容块开一轮（文本 + 图片等多模态输入）。
    pub async fn run_blocks(
        &mut self,
        blocks: Vec<ContentBlock>,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        self.start_run(Some(blocks)).await
    }
```

`start_run` 签名由 `Option<String>` 改为 `Option<Vec<ContentBlock>>`：

```rust
    async fn start_run(
        &mut self,
        blocks: Option<Vec<ContentBlock>>,
    ) -> Result<BoxStream<'static, AgentEvent>, AgentError> {
        // ...（原有 reset 逻辑不变）
        if let Some(blocks) = blocks {
            self.session
                .lock()
                .unwrap()
                .push(Message {
                    role: Role::User,
                    content: blocks,
                });
        }
        // ...（其余不变）
```

`retry_current_session` 里的 `start_run(None)` 保持 `None`。文件顶部确保 `ContentBlock` 与 `Role` 已在作用域（`use crate::message::{...}`）。

- [ ] **Step 4: 跑测试确认通过 + 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib agent`
Expected: PASS（`run_blocks` 新测 + 既有 agent 测试全绿）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "feat(core): add Agent::run_blocks for multimodal input"
```

---

## Task 3: tools — 抽出公共 `image_prep` 管线

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/image_prep.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`（模块声明）、`src/fs/view_image.rs`（改为调用）

**Interfaces:**
- Consumes: 无（用已有的 `image` / `base64` 依赖）。
- Produces（供 Task 5/8 调用）：
  ```rust
  pub const MAX_IMAGE_BYTES: u64;              // 20 * 1024 * 1024
  pub const DEFAULT_BASE64_BUDGET: usize;      // 4 * 1024 * 1024
  pub fn resolve_budget() -> usize;            // 读 YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES
  pub enum PreparedImage {
      Ready { media_type: String, data: String, width: u32, height: u32, note: Option<String> },
      Omitted { message: String },
  }
  pub fn prepare_image_file(path: &Path, detail: ImageDetail, budget: usize) -> Result<PreparedImage, ToolsError>;
  pub fn prepare_image_bytes(bytes: &[u8], detail: ImageDetail, budget: usize) -> Result<PreparedImage, ToolsError>;
  ```

- [ ] **Step 1: 写失败测试**

在新建的 `image_prep.rs` 里先写 `mod tests`：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_a_png_from_bytes() {
        let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png).unwrap();
        match prepare_image_bytes(&buf, ImageDetail::High, DEFAULT_BASE64_BUDGET).unwrap() {
            PreparedImage::Ready { media_type, data, width, height, .. } => {
                assert_eq!(media_type, "image/png");
                assert_eq!((width, height), (8, 8));
                assert!(!data.is_empty());
            }
            PreparedImage::Omitted { message } => panic!("unexpected omit: {message}"),
        }
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let tmp = tempfile::TempDir::new().unwrap();
        let missing = tmp.path().join("nope.png");
        assert!(matches!(
            prepare_image_file(&missing, ImageDetail::High, DEFAULT_BASE64_BUDGET),
            Err(ToolsError::NotFound(_))
        ));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib image_prep`
Expected: 编译失败（模块/函数不存在）。

- [ ] **Step 3: 实现**

在 `lib.rs` 加 `pub mod image_prep;`（与既有模块声明并列）。

`image_prep.rs`：把 `fs/view_image.rs` 里这些**逐字搬运**过来并改为 `pub`（它们本就不依赖工具上下文）：

- 常量 `MAX_IMAGE_BYTES`、`MAX_ENCODED_BASE64_BYTES`（改名导出为 `DEFAULT_BASE64_BUDGET` 并保留内部别名）、`MAX_ENCODED_ENV`、`HIGH_MAX_DIMENSION`、`ORIGINAL_MAX_DIMENSION`、`MIN_DIMENSION`、`JPEG_QUALITIES`
- `resolve_budget`、`parse_budget`、`b64_len`、`lossless_target`、`media_type_for`、`resize_to_cap`、`lossless_encode`、`jpeg_encode`、`encode_within_budget`
- `load_image` 的**解码部分**（去掉 `resolve_and_check`，那是工具侧的根校验）

新增两个公共入口：

```rust
/// 把已读入内存的图片字节处理成可上 wire 的形态。
pub fn prepare_image_bytes(
    bytes: &[u8],
    detail: ImageDetail,
    budget: usize,
) -> Result<PreparedImage, ToolsError> {
    // 逻辑 = 原 `load_image` 中 `std::fs::read` 之后的部分
}

/// 读文件并处理（路径应为已限根的绝对路径；不存在 → NotFound）。
pub fn prepare_image_file(
    path: &Path,
    detail: ImageDetail,
    budget: usize,
) -> Result<PreparedImage, ToolsError> {
    let metadata = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolsError::NotFound(path.to_path_buf())
        } else {
            ToolsError::Io(e)
        }
    })?;
    if metadata.is_dir() {
        return Err(ToolsError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "is a directory",
        )));
    }
    let bytes = std::fs::read(path).map_err(ToolsError::Io)?;
    prepare_image_bytes(&bytes, detail, budget)
}
```

把 `LoadedImage` 改名 `PreparedImage` 并加 `pub`。

- [ ] **Step 4: 让 `view_image` 改用新模块**

`fs/view_image.rs` 删除已搬走的私有函数/常量，`call` 改为：

```rust
        match yi_agent_tools::image_prep::prepare_image_file(&resolved, detail, self.budget) {
            Ok(yi_agent_tools::image_prep::PreparedImage::Ready {
                media_type, data, width, height, note,
            }) => {
                // label 拼装与原来逐字一致
                let label = match &note {
                    Some(note) => format!("viewed {} ({width}x{height}, {media_type}, {note})", args.path),
                    None => format!("viewed {} ({width}x{height}, {media_type})", args.path),
                };
                ToolResult::with_content(vec![
                    ContentBlock::Text(label),
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, data },
                        detail,
                        path: Some(args.path.clone()),
                    },
                ])
            }
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message }) => {
                ToolResult::text(format!("viewed {} ({message})", args.path))
            }
            Err(e) => e.into(),
        }
```

`ViewImageTool::new` 的 budget 取 `image_prep::resolve_budget()`。

- [ ] **Step 5: 跑测试（既有 view_image 测试即回归网）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib image_prep && cargo test -p yi-agent-tools --lib fs::view_image`
Expected: 全 PASS——**原有 `view_image` 断言一条不改仍全绿**，证明抽取无行为变化。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-tools/src/image_prep.rs yi-agent-rs/crates/yi-agent-tools/src/lib.rs yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs
git commit -m "refactor(tools): extract the reusable image preparation pipeline"
```

---

## Task 4: app-server — 协议 `ImageRef` 与 item 图片引用

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`Item` 约 `:470`；测试约 `:632`）

**Interfaces:**
- Consumes: 无。
- Produces:
  ```rust
  #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
  pub struct ImageRef {
      pub path: String,
      pub media_type: String,
      pub size: u64,
      #[serde(skip_serializing_if = "Option::is_none")]
      pub detail: Option<String>,
  }
  ```
  以及 `Item::UserMessage.images: Vec<ImageRef>`、`Item::UserInterjection.images: Vec<ImageRef>`、`Item::ToolCall.images: Vec<ImageRef>`（均 `#[serde(default, skip_serializing_if = "Vec::is_empty")]`）。**不含 base64。**

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 的测试模块加：

```rust
#[test]
fn an_item_carries_image_refs_but_never_base64() {
    let item = Item::UserMessage {
        id: "user-turn-1".into(),
        text: "看图".into(),
        attachments: Vec::new(),
        images: vec![ImageRef {
            path: ".yi-agent/attachments/t1/a1b2-x.png".into(),
            media_type: "image/png".into(),
            size: 1234,
            detail: Some("high".into()),
        }],
    };
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["images"][0]["path"], ".yi-agent/attachments/t1/a1b2-x.png");
    assert_eq!(v["images"][0]["media_type"], "image/png");
    let raw = v.to_string();
    assert!(!raw.contains("base64"), "an item must never inline image bytes: {raw}");
}

#[test]
fn an_item_without_images_omits_the_field() {
    let item = Item::UserMessage {
        id: "user-turn-1".into(),
        text: "hi".into(),
        attachments: Vec::new(),
        images: Vec::new(),
    };
    assert!(serde_json::to_value(&item).unwrap().get("images").is_none());
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib protocol::tests::an_item_carries`
Expected: 编译失败（`unknown field images`）。

- [ ] **Step 3: 实现**

加 `ImageRef`（位置：`Attachment` 定义附近）。三个变体加 `images` 字段（`UserMessage`、`UserInterjection`、`ToolCall`）：

```rust
    UserMessage {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
```

`ToolCall` 的 `images` 加在 `result` 之后。

**编译驱动修**：`Item::UserMessage { .. }` 的构造点需补 `images: Vec::new()`——`server.rs:5206`、`server.rs:6559`（`opening_user_item`）、`translate.rs:328`、`thread_store.rs` 测试等。`cargo check` 会逐个指出；本步骤只补字段使其编译。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib protocol::`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/translate.rs yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): expose image refs on protocol items"
```

---

## Task 5: app-server — 桌面路径摄取 + 注入模型

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/attachments.rs`（`parse_input` 约 `:139`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs`（`TurnPrompt` 约 `:14`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`prepare_turn_core` 约 `:6436`、`start_turn_core` 约 `:6640`、driver `agent.run` 约 `:5510`、`persist_and_finish_turn` 约 `:5194`）

**Interfaces:**
- Consumes: Task 2 `run_blocks`、Task 3 `prepare_image_file`/`PreparedImage`/`resolve_budget`、Task 4 `ImageRef`。
- Produces:
  - `attachments.rs`: `pub enum ImageInput { Path(String) }` + `pub fn parse_image_inputs(params: &Value) -> Vec<ImageInput>`（Task 8 会扩展为含 `Upload(String)`）。
  - `session.rs`: `TurnPrompt.image_blocks: Vec<ContentBlock>`、`PreparedTurn.image_refs: Vec<ImageRef>`。
  - driver 用 `run_blocks`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 测试模块加（沿用 `turn_start_copies_attachments_and_echoes_them_on_the_item` 的 harness 写法）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn turn_start_with_an_image_injects_an_image_block_and_an_item_ref() {
    let workdir = tempfile::TempDir::new().unwrap();
    let mut cfg = default_config();
    cfg.workdir = workdir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一张 8x8 PNG，放在工作区外（模拟从「图片」选择器选）。
    let outside = tempfile::TempDir::new().unwrap();
    let src = outside.path().join("shot.png");
    let img = image::RgbImage::from_fn(8, 8, |_, _| image::Rgb([1, 2, 3]));
    img.save_with_format(&src, image::ImageFormat::Png).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"image","path":"{}"}},{{"type":"text","text":"这是什么"}}]}}}}"#,
        src.display()
    ))
    .await;

    let mut opener: Option<serde_json::Value> = None;
    for _ in 0..14 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/started") {
            opener = Some(v["params"]["item"].clone());
            break;
        }
    }
    let item = opener.expect("the turn must open with an item");
    assert_eq!(item["type"], "userMessage", "{item}");
    assert_eq!(item["text"], "这是什么", "{item}");
    let rel = item["images"][0]["path"].as_str().expect("image path");
    assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");
    assert!(workdir.path().join(rel).is_file(), "image not copied: {rel}");
    // 气泡文本里不得注入清单。
    assert!(!item["text"].as_str().unwrap().contains("Attached files"), "{item}");
    h.shutdown().await;
}
```

再加一条**反向断言**（仿 `turn_start_prompt_reaching_the_model_contains_the_attachment_manifest` 的 mock provider 抓 prompt），断言送进模型的首条 user 消息含 `ContentBlock::Image`：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn an_image_reaches_the_model_as_a_content_block() {
    // 用 mock provider 记录 req.messages，断言首条 message 的 content 含 Image。
    // 结构照抄 turn_start_prompt_reaching_the_model_contains_the_attachment_manifest，
    // 把捕获的 prompts 换成捕获 messages，断言：
    //   messages[0].content 里存在 ContentBlock::Image { path: Some(..), .. }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turn_start_with_an_image`
Expected: FAIL（`item["images"]` 为 `null`；图片未被复制）。

- [ ] **Step 3: 实现 `parse_image_inputs`**

`attachments.rs` 增加：

```rust
/// `turn/start` 的图片输入来源。
#[derive(Debug, Clone, PartialEq)]
pub enum ImageInput {
    /// 桌面端：文件选择器给的绝对路径，服务端负责复制。
    Path(String),
}

/// 拆出图片输入块（`{type:"image", path}`）。非图片块一律忽略。
pub fn parse_image_inputs(params: &serde_json::Value) -> Vec<ImageInput> {
    let Some(input) = params.get("input").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in input {
        if block.get("type").and_then(|t| t.as_str()) == Some("image") {
            if let Some(p) = block.get("path").and_then(|p| p.as_str()) {
                out.push(ImageInput::Path(p.to_string()));
            }
        }
    }
    out
}
```

（`parse_input` 的 text/attachment 分支**不动**；图片块在 `parse_input` 里自然落入 `_ => {}`，不会污染文本。）

- [ ] **Step 4: 实现摄取与注入**

`session.rs` 的 `TurnPrompt` 加：

```rust
    /// 本轮的图片块（已编码，含 base64），随 user 消息进模型上下文。
    pub image_blocks: Vec<yi_agent_core::ContentBlock>,
```

`PreparedTurn`（`server.rs:6416`）加：

```rust
    /// 本轮图片元数据；回显在开启项上（`Item::UserMessage.images`）。
    pub(crate) image_refs: Vec<crate::protocol::ImageRef>,
```

`prepare_turn_core`（`server.rs:6436`）：沿用现有 `source_paths` 复制段落之后，加入图片处理。因为要复用 `cwd` 且需在占 `active_turn_id` 之前失败，插在「复制附件」同一段：

```rust
    let image_inputs = crate::attachments::parse_image_inputs(params);
    let (text_ok, _) = crate::attachments::parse_input(params);
    if text_ok.trim().is_empty() && source_paths.is_empty() && image_inputs.is_empty() {
        return Err(TurnPrepareError::EmptyInput);
    }
    // ...（复制文档附件，不变）
    // 复制 + 编码图片
    let mut image_refs = Vec::with_capacity(image_inputs.len());
    let mut image_blocks = Vec::with_capacity(image_inputs.len());
    for input in &image_inputs {
        let crate::attachments::ImageInput::Path(source) = input;
        let stored = match crate::attachments::store_attachment(
            Path::new(&cwd),
            &thread_id,
            Path::new(source),
            max_bytes,
        ) {
            Ok(a) => a,
            Err(e) => return Err(TurnPrepareError::InvalidAttachment(format!("image {source}: {e:?}"))),
        };
        // 编码（detail=High，沿用 view_image 的默认档）
        let prepared = match yi_agent_tools::image_prep::prepare_image_file(
            &Path::new(&cwd).join(&stored.path),
            yi_agent_core::ImageDetail::High,
            yi_agent_tools::image_prep::resolve_budget(),
        ) {
            Ok(yi_agent_tools::image_prep::PreparedImage::Ready { media_type, data, .. }) => (media_type, data),
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message }) => {
                return Err(TurnPrepareError::InvalidAttachment(format!("image {source}: {message}")));
            }
            Err(e) => return Err(TurnPrepareError::InvalidAttachment(format!("image {source}: {e:?}"))),
        };
        image_blocks.push(yi_agent_core::ContentBlock::Image {
            source: yi_agent_core::ImageSource::Base64 { media_type: prepared.0.clone(), data: prepared.1 },
            detail: yi_agent_core::ImageDetail::High,
            path: Some(stored.path.clone()),
        });
        image_refs.push(crate::protocol::ImageRef {
            path: stored.path,
            media_type: prepared.0,
            size: stored.size,
            detail: Some("high".into()),
        });
    }
```

把 `image_blocks` / `image_refs` 放进 `PreparedTurn`。

> 注意：`store_attachment` 会把文件复制进 `.yi-agent/attachments/<tid>/`；随后 `prepare_image_file` 读的必须是**已复制的那份**（root 内），所以用 `Path::new(&cwd).join(&stored.path)`。

driver（`server.rs:5510`）改为：

```rust
        let mut blocks: Vec<yi_agent_core::ContentBlock> = Vec::new();
        if !prompt.is_empty() {
            blocks.push(yi_agent_core::ContentBlock::Text(prompt.clone()));
        }
        blocks.extend(image_blocks);
        let mut stream = match agent.run_blocks(blocks).await {
```

（`TurnPrompt` 解构处 `server.rs:5440` 加 `image_blocks` 字段。）

`start_turn_core`（`server.rs:6653`）发 `TurnPrompt` 时带上 `image_blocks: prepared.image_blocks`。

开启项：`opening_user_item`（`server.rs:6554`）加 `images` 参数，`persist_and_finish_turn`（`server.rs:5206`）与 `opening_user_item` 都填 `images: image_refs.to_vec()`；`persist_and_finish_turn` 签名加 `user_image_refs: &[crate::protocol::ImageRef]`（其两个调用点同步传参）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turn_start_with_an_image && cargo test -p yi-agent-app-server --lib an_image_reaches_the_model`
Expected: PASS。

- [ ] **Step 6: 回归 + 提交**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: PASS（既有 attachments/turn 测试全绿）。

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/{attachments.rs,session.rs,server.rs}
git commit -m "feat(app-server): accept image inputs and inject them as content blocks"
```

---

## Task 6: app-server — `image/read` RPC（分片回传）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（RPC dispatch：在 `"workspace/list"`（`:2682`）附近加分支）

**Interfaces:**
- Consumes: `resolve_and_check` 的等价限根逻辑（app-server 侧已有 worktree/root 解析：参考 `git_diff.rs` 的 root 处理或 `store_lookup` 的 cwd 解析）。
- Produces: RPC `image/read` → `{ data: String, nextOffset: Option<u64>, mediaType: String, size: u64 }`，分片原始字节 `IMAGE_READ_CHUNK_BYTES = 512 * 1024`。

- [ ] **Step 1: 写失败测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn image_read_round_trips_a_file_in_chunks() {
    // 起一个 thread（拿到 cwd），把一张 > 512KiB 的 PNG 放进 .yi-agent/attachments/<tid>/，
    // 调 image/read，循环按 nextOffset 拉取，把 base64 解码后拼起来，断言 == 源字节。
}

#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_path_outside_the_workspace() {
    // path 用 "../secret.png"，断言返回 invalid_params 错误。
}

#[tokio::test(flavor = "multi_thread")]
async fn image_read_refuses_a_non_image_file() {
    // 写一个 .txt，断言被拒（不是图片）。
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_read`
Expected: FAIL（方法不存在 → `method not found`）。

- [ ] **Step 3: 实现**

在 dispatch 加分支 `"image/read" => { ... }`：

```rust
                    "image/read" => {
                        let Some(thread_id) = req.params.get("threadId").and_then(|v| v.as_str())
                        else {
                            write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("threadId required"))).await?;
                            continue;
                        };
                        let Some(rel) = req.params.get("path").and_then(|v| v.as_str()) else {
                            write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("path required"))).await?;
                            continue;
                        };
                        // 解析该 thread 的 cwd（内存优先，其次 store_lookup）——与既有
                        // 路径限额同源。
                        let Some(root) = thread_cwd(&threads, &workspaces, &cfg, thread_id) else {
                            write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("unknown thread"))).await?;
                            continue;
                        };
                        // 限根：拒绝 `..` 与绝对路径逃逸。
                        let joined = Path::new(&root).join(rel);
                        let canonical = match joined.canonicalize() {
                            Ok(p) => p,
                            Err(e) => { write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params(format!("cannot read image: {e}")))).await?; continue; }
                        };
                        let root_canonical = Path::new(&root).canonicalize().unwrap_or_else(|_| PathBuf::from(&root));
                        if !canonical.starts_with(&root_canonical) {
                            write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("path escapes the workspace"))).await?;
                            continue;
                        }
                        let bytes = match std::fs::read(&canonical) {
                            Ok(b) => b,
                            Err(e) => { write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params(format!("cannot read image: {e}")))).await?; continue; }
                        };
                        // 必须是可解码的图片：不引入任意文件读取。
                        let fmt = match image::guess_format(&bytes) {
                            Ok(f) => f,
                            Err(_) => { write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("not a supported image"))).await?; continue; }
                        };
                        if bytes.len() as u64 > IMAGE_READ_MAX_BYTES {
                            write_response(&hub, &client, err_response(id.clone(), RpcError::invalid_params("image too large"))).await?;
                            continue;
                        }
                        let offset = req.params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
                        let max_bytes = req.params.get("maxBytes").and_then(|v| v.as_u64())
                            .unwrap_or(IMAGE_READ_CHUNK_BYTES).min(IMAGE_READ_CHUNK_BYTES);
                        let start = (offset as usize).min(bytes.len());
                        let end = (start + max_bytes as usize).min(bytes.len());
                        let next = if end < bytes.len() { Some(end as u64) } else { None };
                        let data = base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]);
                        write_response(&hub, &client, ok_response(id.clone(), json!({
                            "data": data,
                            "nextOffset": next,
                            "mediaType": media_type_for_format(fmt),
                            "size": bytes.len(),
                        }))).await?;
                        continue;
                    }
```

常量（`server.rs` 顶部）：`IMAGE_READ_CHUNK_BYTES: u64 = 512 * 1024`、`IMAGE_READ_MAX_BYTES: u64 = 20 * 1024 * 1024`（与 `image_prep::MAX_IMAGE_BYTES` 同值）。

`image` / `base64` 若未在 `yi-agent-app-server/Cargo.toml`，加 `image = { workspace = true }`（或按 tools 的写法 `default-features = false, features = ["png","jpeg","gif","webp"]`）与 `base64 = "0.22"`。加一个 `thread_cwd` 小 helper（内存 `threads.get(id).cwd`，否则 `store_lookup` → 该 store root 的父目录；若已有等价 helper 直接复用）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_read`
Expected: PASS（3 条）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/{Cargo.toml,src/server.rs}
git commit -m "feat(app-server): add a chunked image/read RPC"
```

---

## Task 7: app-server — 工具结果里的图片进入 item 引用

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（`AgentEvent::ToolResult` 约 `:270`；`render_content` 约 `:413`）

**Interfaces:**
- Consumes: Task 4 `ImageRef`、Task 1 `ContentBlock::Image.path`。
- Produces: `Item::ToolCall.images` 填 `view_image` 产出的图片引用；`render_content` 的文本行为不变（仍写 `[image]` 占位进 `result`）。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn a_tool_result_image_becomes_an_item_ref_without_base64() {
    let mut t = translator();
    let out = t.on_event(AgentEvent::ToolCall {
        id: "call-1".into(),
        name: "view_image".into(),
        input: serde_json::json!({"path": "logo.png"}),
    });
    assert!(!out.is_empty());
    let out = t.on_event(AgentEvent::ToolResult {
        id: "call-1".into(),
        result: ToolResult::with_content(vec![
            ContentBlock::Text("viewed logo.png (8x8, image/png)".into()),
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type: "image/png".into(), data: "AAAA".into() },
                detail: ImageDetail::High,
                path: Some(".yi-agent/attachments/t1/a1-logo.png".into()),
            },
        ]),
    });
    let item = /* 取 ItemCompleted/ToolCall item */;
    let v = serde_json::to_value(&item).unwrap();
    assert_eq!(v["images"][0]["path"], ".yi-agent/attachments/t1/a1-logo.png");
    assert!(!v.to_string().contains("AAAA"), "no base64 in the item");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_tool_result_image_becomes`
Expected: FAIL（`images` 为空）。

- [ ] **Step 3: 实现**

`translate.rs` 增加：

```rust
/// 从工具结果里抽出图片的**引用**（不含 base64）。
fn image_refs_from(blocks: &[ContentBlock]) -> Vec<crate::protocol::ImageRef> {
    let mut out = Vec::new();
    for block in blocks {
        if let ContentBlock::Image { source, path, .. } = block {
            if let Some(rel) = path {
                let media_type = match source {
                    ContentBlock::ImageSource::Base64 { media_type, .. } => media_type.clone(),
                    _ => "image/*".to_string(),
                };
                out.push(crate::protocol::ImageRef {
                    path: rel.clone(),
                    media_type,
                    size: 0, // 工具侧不知道原文件大小；UI 用不到，保守填 0
                    detail: None,
                });
            }
        }
    }
    out
}
```

> 修正：`source` 是 `yi_agent_core::ImageSource`，匹配 `ImageSource::Base64 { media_type, .. }`（不要写成 `ContentBlock::ImageSource`）。

在 `AgentEvent::ToolResult` 分支里，取 `rendered` 的同时算出 refs，并把它传进 `complete_tool(...)`（给 `complete_tool` 加一个 `images: Vec<ImageRef>` 参数，构造 `Item::ToolCall { ..., images }`）。

- [ ] **Step 4: 跑测试确认通过 + 回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib translate`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/translate.rs
git commit -m "feat(app-server): surface tool-result images as item refs"
```

---

## Task 8: app-server — iOS 分片上传 RPC

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/image_upload.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{lib.rs,attachments.rs,server.rs,session.rs}`

**Interfaces:**
- Consumes: Task 3 `prepare_image_file`、Task 5 的 `ImageInput` 与摄取路径。
- Produces:
  ```rust
  pub struct UploadRegistry { /* uploads: HashMap<String, UploadSession> */ }
  impl UploadRegistry {
      pub fn new() -> Self;
      pub fn begin(&mut self, cwd: &Path, thread_id: &str, name: &str, size: u64) -> Result<String, String>; // -> uploadId
      pub fn append(&mut self, upload_id: &str, index: u64, bytes: &[u8]) -> Result<(), String>;
      pub fn commit(&mut self, cwd: &Path, thread_id: &str, upload_id: &str) -> Result<String, String>; // -> 落盘后的相对 path
      pub fn abort(&mut self, upload_id: &str);
      pub fn sweep_expired(&mut self); // TTL 清理
  }
  ```
  `attachments.rs` 的 `ImageInput` 扩展为 `Path(String) | Upload(String)`。

- [ ] **Step 1: 写失败测试**

`image_upload.rs` 的 `mod tests`：

```rust
#[test]
fn begin_chunk_commit_produces_a_stored_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut reg = UploadRegistry::new();
    let id = reg.begin(tmp.path(), "t1", "shot.jpg", 6).unwrap();
    reg.append(&id, 0, b"abc").unwrap();
    reg.append(&id, 1, b"def").unwrap();
    let rel = reg.commit(tmp.path(), "t1", &id).unwrap();
    assert!(rel.starts_with(".yi-agent/attachments/t1/"));
    assert_eq!(std::fs::read(tmp.path().join(&rel)).unwrap(), b"abcdef");
}

#[test]
fn a_chunk_out_of_order_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut reg = UploadRegistry::new();
    let id = reg.begin(tmp.path(), "t1", "x.jpg", 6).unwrap();
    reg.append(&id, 1, b"def").unwrap_err(); // index 必须从 0 起
}

#[test]
fn abort_removes_the_staging_file() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut reg = UploadRegistry::new();
    let id = reg.begin(tmp.path(), "t1", "x.jpg", 6).unwrap();
    reg.append(&id, 0, b"abc").unwrap();
    reg.abort(&id);
    assert!(!tmp.path().join(".yi-agent/attachments/t1/.tmp").join(&id).exists());
}
```

server 级：`image/upload/*` 三段式后，用返回的 `uploadId` 走 `turn/start` 的 `{type:"uploaded_image", uploadId}`，断言图片被复制 + item.images 非空。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_upload`
Expected: 编译失败（模块不存在）。

- [ ] **Step 3: 实现 registry**

`image_upload.rs`：staging 文件路径 = `<cwd>/.yi-agent/attachments/<tid>/.tmp/<uploadId>`。`begin` 记录 name/size/next_index/received/created_at 并创建空 staging 文件（父目录 `create_dir_all`）。`append` 校验 `index == next_index`、`received + bytes.len() <= size`，追加写。`commit` 校验 `received == size`，读回 staging 字节，调 `crate::attachments::store_attachment`（源为 staging 文件）得到 `Attachment`，返回其 `path`；随后清理 staging 与记录。`abort` 删 staging 文件与记录。`sweep_expired` 删除 `created_at` 超过 `UPLOAD_TTL`（10 分钟）的会话。

`lib.rs` 加 `pub mod image_upload;`。

- [ ] **Step 4: 实现 RPC 与输入解析**

`attachments.rs` 的 `ImageInput` 加 `Upload(String)`；`parse_image_inputs` 识别 `{type:"uploaded_image", uploadId}`。

`server.rs`：
- serve 循环定义 `let mut uploads = crate::image_upload::UploadRegistry::new();`（与 `workspaces` 并列），每轮循环开头 `uploads.sweep_expired();`。
- dispatch 加 `image/upload/begin|chunk|commit|abort` 四个分支（`begin` 需 `threadId` 解析 cwd；`chunk` 的 `data` 是 base64，**在服务端解码**，解码后累计不得超过 `size` 与 20 MiB）。
- `prepare_turn_core` 需能解析 `ImageInput::Upload(id)` → 用 registry 取已落盘 path。因为 `prepare_turn_core` 当前签名不含 registry，给它加参数 `uploads: &mut UploadRegistry`（调用点同步）。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_upload && cargo test -p yi-agent-app-server --lib uploaded_image`
Expected: PASS。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/{image_upload.rs,lib.rs,attachments.rs,server.rs,session.rs}
git commit -m "feat(app-server): add chunked image upload RPCs for the remote client"
```

---

## Task 9: desktop — 协议类型、图片附件预检与发送

**Files:**
- Modify: `desktop/src/lib/protocol.ts`、`desktop/src/lib/attachmentLimits.ts`、`desktop/src/App.tsx`

**Interfaces:**
- Consumes: Task 4 的 `ImageRef`（wire 同形）、Task 5 的 `{type:"image", path}` 与 `{type:"uploaded_image", uploadId}`。
- Produces: TS `ImageRef`；`PendingAttachment.kind: "document" | "image"`；`IMAGE_EXTENSIONS`；`imageAttachmentProblem`。

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/attachmentLimits.test.ts` 加：

```ts
it("accepts images but rejects a document-only extension for the image picker", () => {
  expect(imageAttachmentProblem("/tmp/a.png", 100)).toBeNull();
  expect(imageAttachmentProblem("/tmp/a.jpg", 100)).toBeNull();
  expect(imageAttachmentProblem("/tmp/a.pdf", 100)).toMatch(/不支持/);
  expect(imageAttachmentProblem("/tmp/a.png", 21 * 1024 * 1024)).toMatch(/20 MB/);
});
```

`desktop/src/App.test.tsx` 加：发送一条带图片 pending 的消息时，`input` 数组按 `{type:"image", path}` 排在文本之前。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/attachmentLimits.test.ts`
Expected: FAIL（`imageAttachmentProblem` 未定义）。

- [ ] **Step 3: 实现协议与预检**

`protocol.ts`：

```ts
export interface ImageRef {
  path: string;
  media_type: string;
  size: number;
  detail?: string;
}
```

`Item` 的 `userMessage` / `user_interjection` 加 `images?: ImageRef[]`，`toolCall` 加 `images?: ImageRef[]`。

`attachmentLimits.ts` 加：

```ts
export const IMAGE_EXTENSIONS = ["png", "jpg", "jpeg", "gif", "webp"] as const;
export const MAX_IMAGE_BYTES = 20 * 1024 * 1024;

export function imageAttachmentProblem(path: string, size: number): string | null {
  const ext = extensionOf(path);
  if (!(IMAGE_EXTENSIONS as readonly string[]).includes(ext)) {
    return `不支持的图片类型：${ext || "（无扩展名）"}`;
  }
  if (size > MAX_IMAGE_BYTES) return "图片超过 20 MB 上限";
  return null;
}
```

`PendingAttachment` 加 `kind: "document" | "image"`；`pickFilesToAttach` 给既有文档 chip 填 `kind: "document"`。

- [ ] **Step 4: 发送组装**

`App.tsx` 的 `send`：把 `files` 按 `kind` 分组，`input` 组装顺序为 `[...images, ...attachments, {type:"text"}]`（图片块与文档附件块都在文本前，与现有「附件先于文本」的注释一致）；`session.addUserMessage` 的 attachments 参数只带文档（图片走 `images`，由服务端回显）。

- [ ] **Step 5: 跑测试 + 提交**

Run: `cd desktop && npx vitest run src/lib/attachmentLimits.test.ts src/App.test.tsx && npx tsc --noEmit`
Expected: PASS。

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/attachmentLimits.ts desktop/src/App.tsx
git commit -m "feat(desktop): accept image attachments in the composer"
```

---

## Task 10: desktop — 图片 chip 与缩略图

**Files:**
- Create: `desktop/src/components/AttachmentThumb.tsx`
- Modify: `desktop/src/components/AttachmentChips.tsx`、`MessageInput.tsx`（图片选择按钮）

**Interfaces:**
- Consumes: Task 9 `PendingAttachment.kind`。
- Produces: `<AttachmentThumb path={string} />`（Task 11 的 `useImageData` 消费者）。

- [ ] **Step 1: 写失败测试**

`AttachmentChips.test.tsx` 加：`kind: "image"` 的 chip 渲染缩略图节点（`data-testid="attachment-thumb"`）且仍可移除；`kind: "document"` 的不渲染缩略图。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/AttachmentChips.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

`AttachmentThumb.tsx`：接 `path`，调 `useImageData(path)`（Task 11 产出；本步骤可先渲染占位并以 `data-testid` 锚定，Task 11 接上真实字节）。`AttachmentChips` 对 `kind === "image"` 渲染缩略图 + 文件名。

`MessageInput` 在「附加文件」旁加「附加图片」按钮（`aria-label="附加图片"`），回调 `onPickImages` 由 App 提供（桌面扩 `filters` 到 `IMAGE_EXTENSIONS`；remote 走 Task 12）。

- [ ] **Step 4: 跑测试 + 提交**

Run: `cd desktop && npx vitest run src/components/AttachmentChips.test.tsx src/components/MessageInput.test.tsx`
Expected: PASS。

```bash
git add desktop/src/components/AttachmentThumb.tsx desktop/src/components/AttachmentChips.tsx desktop/src/components/MessageInput.tsx
git commit -m "feat(desktop): render image attachment chips with thumbnails"
```

---

## Task 11: desktop — `image/read` 客户端、缓存与气泡/工具卡渲染

**Files:**
- Create: `desktop/src/lib/useImageData.ts`
- Modify: `desktop/src/components/{ChatView,ToolCallCard}.tsx`

**Interfaces:**
- Consumes: Task 6 RPC `image/read`、Task 4 `ImageRef`。
- Produces: `useImageData(path: string): { url: string | null; error: string | null }`（分片拉取 → Blob URL；模块级 LRU 缓存）。

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/useImageData.test.ts`（用 mock transport 断言：多次 `image/read` 按 `nextOffset` 循环、拼接后的字节正确、第二个相同 path 的消费者命中缓存不再发请求）。

`ChatView.test.tsx` 加：`userMessage` 带 `images` 时渲染 `<img data-testid="bubble-image">`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/useImageData.test.ts`
Expected: FAIL。

- [ ] **Step 3: 实现**

`useImageData.ts`：`request("image/read", { threadId, path, offset })` 循环到 `nextOffset === null`；base64 → `Uint8Array` → `Blob` → `URL.createObjectURL`；缓存键 `path`，LRU 上限（如 60 条），淘汰时 `revokeObjectURL`。需要 `threadId` + `client`：通过参数或既有 context 注入（沿用项目现有取 client 的方式）。

`ChatView`：`userMessage` 分支在附件 chips 下渲染 `item.images?.map(i => <img ... />)`。`ToolCallCard`：`item.images` 存在时在 result 前渲染图片。

- [ ] **Step 4: 跑测试 + 回归 + 提交**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: PASS。

```bash
git add desktop/src/lib/useImageData.ts desktop/src/components/ChatView.tsx desktop/src/components/ToolCallCard.tsx
git commit -m "feat(desktop): render user and tool images in the transcript"
```

---

## Task 12: desktop — iOS 图片选择器与分片上传客户端

**Files:**
- Create: `desktop/src/lib/imageUpload.ts`
- Modify: `desktop/src/App.tsx`、`desktop/src-tauri/Info.ios.plist`（仅当实测需要）

**Interfaces:**
- Consumes: Task 8 RPC `image/upload/*`。
- Produces: `uploadImage(client, threadId, file: File): Promise<string>`（返回 `uploadId`）。

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/imageUpload.test.ts`：给定一个 mock client 与 `File`，断言按 `UPLOAD_CHUNK_BYTES` 切块、`begin` 一次、`chunk` 按 index 递增、`commit` 一次并返回 `uploadId`；HEIC 输入时走 canvas 转 JPEG（用 mock canvas 或断言走转换分支）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/imageUpload.test.ts`
Expected: FAIL。

- [ ] **Step 3: 实现**

`imageUpload.ts`：`UPLOAD_CHUNK_BYTES = 512 * 1024`。若 `file.type` 为 `image/heic`/`image/heif`（`image` crate 不认），先用 `createImageBitmap` + `canvas.toBlob("image/jpeg", 0.9)` 转码；转不了则抛明确错误。分片 base64（`FileReader.readAsDataURL` 或 `btoa` on `Uint8Array`）→ `image/upload/chunk`。

`App.tsx`：remote 分支加 `onPickImages`，用动态创建的 `<input type="file" accept="image/*" multiple>`；选中后逐个 `uploadImage`，把返回的 `uploadId` 作为 pending（`kind:"image"`，标记 `uploadId`）；`send` 时对这类 pending 组装 `{type:"uploaded_image", uploadId}`。

- [ ] **Step 4: 实测确认 iOS 相册权限**

在 iOS 真机（见 `docs/relay-deploy.md` §4 的构建步骤）打开选择器，确认系统弹权限且能把照片发出去。**若**系统选择器未覆盖而需显式权限，往 `Info.ios.plist` 加 `NSPhotoLibraryUsageDescription`（给出用途文案）；否则不加。

- [ ] **Step 5: 跑测试 + 提交**

Run: `cd desktop && npx vitest run src/lib/imageUpload.test.ts src/App.test.tsx && npx tsc --noEmit`
Expected: PASS。

```bash
git add desktop/src/lib/imageUpload.ts desktop/src/App.tsx desktop/src-tauri/Info.ios.plist
git commit -m "feat(desktop): pick and upload images on the remote client"
```

---

## Task 13: 文档同步

**Files:**
- Modify: `docs/project-management/desktop.md`（`:93`）、`docs/project-management/yi-agent-app-server.md`、`docs/project-management/yi-agent-core.md`、`docs/project-management/yi-agent-tools.md`、`docs/bug-list.md`（`:27`）、`README.md`

**Interfaces:** 无（纯文档）。

- [ ] **Step 1: 改 `desktop.md`**

把 `:93` 的 `- [ ] 图片附件输入 — 判据：输入框可附加图片并随 prompt 发送` 改为 `[x]`，判据写全：代码位置（`desktop/src/App.tsx` 的 `pickFilesToAttach`/`onPickImages`、`desktop/src/lib/useImageData.ts`、`desktop/src/components/ChatView.tsx`）+ 验证命令（`cd desktop && npx vitest run src/lib/useImageData.test.ts src/lib/imageUpload.test.ts src/components/ChatView.test.tsx`）。

- [ ] **Step 2: 改 `bug-list.md:27`**

把 compaction 图片 token 那条由 `[ ]` 改为 `[x]`，注明「已在 commit `b7a52a25` 修复：`compact.rs:95` 记 `IMAGE_TOKEN_ESTIMATE`；本条为文档过期，代码无需改动」。

- [ ] **Step 3: 登记模块条目**

- `yi-agent-core.md`：`Image.path` 字段 + `Agent::run_blocks`（判据：`cargo test -p yi-agent-core --lib message:: agent::`）。
- `yi-agent-tools.md`：`image_prep` 公共模块（判据：`cargo test -p yi-agent-tools --lib image_prep fs::view_image`）。
- `yi-agent-app-server.md`：图片输入、`image/read`、`image/upload/*`、item.images（判据：`cargo test -p yi-agent-app-server --lib image_read image_upload turn_start_with_an_image`）。

- [ ] **Step 4: 同步 `README.md` 计数**

按各模块新增的 feature 条数更新索引表的「完成 / 总计」（desktop 尤其：`37 / 50` → 按实际增量）。

- [ ] **Step 5: 提交**

```bash
git add docs/ README.md
git commit -m "docs: register image input and display in project management"
```

---

## Self-Review

**Spec coverage**
- §3 协议（image block / ImageRef / image read+upload）→ Task 4、5、6、8。
- §4.1 core `run_blocks` + `Image.path` → Task 1、2。
- §4.2 tools `image_prep` → Task 3。
- §4.3 app-server 摄取/注入/显示 → Task 5、6、7、8。
- §4.4 desktop 选择/上传/显示 → Task 9、10、11、12。
- §7-C bug-list 过期条目 → Task 13。
- 安全与上限 → Task 6（限根/图片校验/分片）、Task 8（TTL/条数/上限）。
- 测试判据 §5 → 每任务的 Step 1/4。

**风险点（实现时留意）**
- Task 8 给 `prepare_turn_core` 加 `uploads` 参数会牵动多个调用点（含看板 launcher）；用编译器逐个收敛。
- Task 9 的发送组装要与既有「附件 block 排在文本前」的注释保持一致；`turn/interject` 不带图片（与附件现状一致）。
- Task 11 的 `threadId`/`client` 注入：沿用项目既有的取 client 方式，不要新建全局单例。
- Task 12 的 HEIC 转码是**实测依赖**项（Step 4），不能在未实测时假设系统交付 JPEG。
