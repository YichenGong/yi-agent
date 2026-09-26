# 图片读取（view_image 工具）实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 新增 `view_image` 工具，让模型能读取工作区图片并作为多模态内容块送入 Anthropic 与 OpenAI 两个 provider。

**Architecture:** 工具在 `yi-agent-tools` 内部完成「读文件 → 识别格式 → 解码 → 超限缩放 → base64」全流程，返回 `ToolResult`（含 `ContentBlock::Text` 标签 + `ContentBlock::Image`）。`yi-agent-core` 的 `ContentBlock::Image` 增加 `detail` 字段。Anthropic provider 已能序列化图片，只需补字段解构；OpenAI provider 新增 `image_url` part，并把「tool 结果里的图片」拆成 tool 消息 + 紧随其后的 user 消息（Chat Completions 不允许 tool 消息带图）。

**Tech Stack:** Rust 2024 edition、`image` 0.25、`base64` 0.22、serde、tokio（测试）、tempfile。

**设计文档：** `docs/superpowers/specs/2026-09-26-view-image-tool-design.md`

**工作目录：** `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/view-image-tool`。以下所有路径都相对该 worktree。Rust 命令在 `yi-agent-rs/` 下执行。

**通用约定：**
- 每个任务先写失败测试、再实现、再跑通、再 commit。
- commit 用 conventional commits，**不要**写 `Co-Authored-By` 行。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认没有残留 cargo/测试进程（残留进程会持锁导致卡死）。

---

## Task 1: `ContentBlock::Image` 增加 `detail` 字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/message.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs:20`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs:125`
- Test: `yi-agent-rs/crates/yi-agent-core/src/message.rs`（`#[cfg(test)] mod tests`）

**Step 1: 写失败测试**

在 `message.rs` 的 `mod tests` 里追加：

```rust
#[test]
fn image_detail_defaults_to_high_when_absent() {
    // A block deserialized without `detail` must default to High so old
    // serialized sessions keep loading.
    let json = r#"{"Image":{"source":{"Base64":{"media_type":"image/png","data":"AAA"}}}}"#;
    let block: ContentBlock = serde_json::from_str(json).unwrap();
    match block {
        ContentBlock::Image { detail, .. } => assert_eq!(detail, ImageDetail::High),
        _ => panic!("expected Image"),
    }
}

#[test]
fn image_detail_roundtrips_original() {
    let block = ContentBlock::Image {
        source: ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAA".into(),
        },
        detail: ImageDetail::Original,
    };
    let json = serde_json::to_string(&block).unwrap();
    let back: ContentBlock = serde_json::from_str(&json).unwrap();
    assert_eq!(block, back);
}

#[test]
fn image_detail_wire_str() {
    assert_eq!(ImageDetail::High.as_wire_str(), "high");
    assert_eq!(ImageDetail::Original.as_wire_str(), "original");
}
```

同时把已有两个测试 `nested_tool_result_content`（`:131`）和
`image_source_url_serde_roundtrip`（`:148`）里的 `ContentBlock::Image { source }`
补成 `ContentBlock::Image { source, detail: ImageDetail::High }`。

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib message::`
Expected: 编译失败，`cannot find type ImageDetail` / `missing field detail`。

**Step 3: 实现**

`message.rs`：在 `ImageSource` 定义之后加枚举，并给 `Image` 变体加字段：

```rust
/// Requested fidelity for an image block. Only affects how the image is
/// resized before being sent; OpenAI maps it to `image_url.detail`,
/// Anthropic has no equivalent field and ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageDetail {
    #[default]
    High,
    Original,
}

impl ImageDetail {
    /// Lowercase name used on the wire (OpenAI `image_url.detail`).
    pub fn as_wire_str(&self) -> &'static str {
        match self {
            ImageDetail::High => "high",
            ImageDetail::Original => "original",
        }
    }
}
```

```rust
    Image {
        source: ImageSource,
        #[serde(default)]
        detail: ImageDetail,
    },
```

`lib.rs:20`：

```rust
pub use message::{ContentBlock, ImageDetail, ImageSource, Message, Role};
```

`anthropic/types.rs:125`：

```rust
            ContentBlock::Image { source, .. } => AnthropicContentBlock::Image {
                source: source.into(),
            },
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib message::`
Expected: PASS。

再确认整个 core + llm 能编译：

Run: `cd yi-agent-rs && cargo test -p yi-agent-core -p yi-agent-llm --no-run`
Expected: 编译通过（`yi-agent-llm` 的 anthropic 测试随之更新）。

**Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-core/src/message.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs
git commit -m "feat(core): add ImageDetail to ContentBlock::Image"
```

---

## Task 2: `view_image` 工具（读取 + 缩放 + base64）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/error.rs`
- Create: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:24,66`

**Step 1: 加依赖**

`yi-agent-tools/Cargo.toml` 的 `[dependencies]` 段加：

```toml
# 图片读取
image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }
base64 = "0.22"
```

`[dev-dependencies]` 已有 `tempfile`，无需再加。

**Step 2: 加错误变体**

`error.rs` 在 `ToolsError` 里加：

```rust
    #[error("image too large: {size} bytes (max {max})")]
    ImageTooLarge { size: u64, max: u64 },

    #[error("image decode failed: {0}")]
    ImageDecode(String),
```

**Step 3: 写失败测试**

创建 `fs/view_image.rs`，先只放测试需要的骨架 + 测试：

```rust
use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_and_check;
use async_trait::async_trait;
use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use yi_agent_core::{
    ContentBlock, ImageDetail, ImageSource, Tool, ToolMetadata, ToolResult, ToolSource,
};

/// Largest image file we will read from disk (guards memory before decode).
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Longest-edge cap for `detail: high`.
const HIGH_MAX_DIMENSION: u32 = 2048;
/// Longest-edge cap for `detail: original`.
const ORIGINAL_MAX_DIMENSION: u32 = 6000;

pub struct ViewImageTool {
    ctx: Arc<ToolsContext>,
}

impl ViewImageTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

/// `detail` arg, parsed by serde so an invalid value becomes `ArgsParse`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum DetailArg {
    #[default]
    High,
    Original,
}

impl From<DetailArg> for ImageDetail {
    fn from(d: DetailArg) -> Self {
        match d {
            DetailArg::High => ImageDetail::High,
            DetailArg::Original => ImageDetail::Original,
        }
    }
}

#[derive(Deserialize)]
struct ViewImageArgs {
    path: String,
    #[serde(default)]
    detail: DetailArg,
}

struct LoadedImage {
    media_type: String,
    data: String,
    width: u32,
    height: u32,
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &str {
        "view_image"
    }

    fn description(&self) -> &str {
        "View a local image file (png/jpeg/gif/webp) when visual inspection is \
         needed. Returns the image so you can see it."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Relative or absolute path to an image file within root"
                },
                "detail": {
                    "type": "string",
                    "enum": ["high", "original"],
                    "description": "Image detail level. 'high' (default) caps the longest edge at 2048px; 'original' preserves up to 6000px."
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: ViewImageArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        let resolved = match resolve_and_check(self.ctx.root(), &args.path) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };

        let detail: ImageDetail = args.detail.into();

        match load_image(&resolved, detail) {
            Ok(img) => {
                let label = format!(
                    "viewed {} ({}x{}, {})",
                    args.path, img.width, img.height, img.media_type
                );
                ToolResult::with_content(vec![
                    ContentBlock::Text(label),
                    ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: img.media_type,
                            data: img.data,
                        },
                        detail,
                    },
                ])
            }
            Err(e) => e.into(),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

fn load_image(path: &Path, detail: ImageDetail) -> Result<LoadedImage, ToolsError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> ViewImageTool {
        ViewImageTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
        img.save_with_format(path, ImageFormat::Png).unwrap();
    }

    fn write_jpeg(path: &Path, w: u32, h: u32) {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 64]));
        img.save_with_format(path, ImageFormat::Jpeg).unwrap();
    }

    fn image_block(result: &ToolResult) -> &ContentBlock {
        result
            .content
            .iter()
            .find(|b| matches!(b, ContentBlock::Image { .. }))
            .expect("expected an image block")
    }

    fn base64_bytes(block: &ContentBlock) -> (String, Vec<u8>) {
        match block {
            ContentBlock::Image {
                source: ImageSource::Base64 { media_type, data },
                ..
            } => (
                media_type.clone(),
                base64::engine::general_purpose::STANDARD.decode(data).unwrap(),
            ),
            _ => panic!("expected base64 image"),
        }
    }

    #[tokio::test]
    async fn view_image_returns_image_block_for_png() {
        let tmp = TempDir::new().unwrap();
        write_png(&tmp.path().join("pic.png"), 8, 8);
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "pic.png"})).await;
        assert!(!result.is_error);
        let (media_type, bytes) = base64_bytes(image_block(&result));
        assert_eq!(media_type, "image/png");
        // Bytes must decode back to a valid image.
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[tokio::test]
    async fn view_image_accepts_jpeg() {
        let tmp = TempDir::new().unwrap();
        write_jpeg(&tmp.path().join("pic.jpg"), 8, 8);
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "pic.jpg"})).await;
        assert!(!result.is_error);
        let (media_type, _) = base64_bytes(image_block(&result));
        assert_eq!(media_type, "image/jpeg");
    }

    #[tokio::test]
    async fn view_image_resizes_large_image() {
        let tmp = TempDir::new().unwrap();
        write_png(&tmp.path().join("big.png"), 3000, 1000);
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "big.png"})).await;
        assert!(!result.is_error);
        let (_, bytes) = base64_bytes(image_block(&result));
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), 2048);
        // Aspect ratio preserved (3000:1000 == 3:1).
        assert_eq!(decoded.width(), 2048);
        assert_eq!(decoded.height(), 683);
    }

    #[tokio::test]
    async fn view_image_original_detail_keeps_more_resolution() {
        let tmp = TempDir::new().unwrap();
        write_png(&tmp.path().join("big.png"), 3000, 1000);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "big.png", "detail": "original"}))
            .await;
        assert!(!result.is_error);
        let (_, bytes) = base64_bytes(image_block(&result));
        let decoded = image::load_from_memory(&bytes).unwrap();
        // Under 6000 cap -> untouched at 3000 wide.
        assert_eq!(decoded.width(), 3000);
        assert_eq!(decoded.height(), 1000);
    }

    #[tokio::test]
    async fn view_image_rejects_unsupported_format() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("notes.txt"), b"just some text").unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "notes.txt"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn view_image_rejects_oversize_file() {
        let tmp = TempDir::new().unwrap();
        let big = vec![0u8; (MAX_IMAGE_BYTES + 1) as usize];
        std::fs::write(tmp.path().join("huge.png"), &big).unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "huge.png"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn view_image_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "nope.png"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn view_image_rejects_invalid_detail() {
        let tmp = TempDir::new().unwrap();
        write_png(&tmp.path().join("pic.png"), 8, 8);
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "pic.png", "detail": "low"}))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn view_image_escapes_root() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn view_image_label_mentions_path_and_dimensions() {
        let tmp = TempDir::new().unwrap();
        write_png(&tmp.path().join("pic.png"), 12, 7);
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"path": "pic.png"})).await;
        match &result.content[0] {
            ContentBlock::Text(t) => {
                assert!(t.contains("pic.png"));
                assert!(t.contains("12x7"));
            }
            _ => panic!("expected leading text label"),
        }
    }
}
```

**Step 4: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::view_image`
Expected: 编译失败或 `todo!()` panic（`not yet implemented`）。同时 `mod.rs`/`lib.rs` 尚未导出，先临时用 `cargo build` 验证模块能被编译——见 Step 5 一起做。

**Step 5: 注册模块与工具**

`fs/mod.rs` 加：

```rust
pub mod view_image;
```
以及 `pub use view_image::ViewImageTool;`。

`lib.rs:24` 的 `pub use fs::{...}` 加 `ViewImageTool`：

```rust
pub use fs::{EditTool, GlobTool, GrepTool, ReadTool, ViewImageTool, WriteTool};
```

`lib.rs:66` 附近，在 `ReadTool` 之后注册（读图是只读操作，只读会话也要有）：

```rust
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    registry.register(Arc::new(ViewImageTool::new(ctx.clone())));
```

**Step 6: 实现 `load_image`**

替换 `todo!()`：

```rust
fn load_image(path: &Path, detail: ImageDetail) -> Result<LoadedImage, ToolsError> {
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

    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ToolsError::ImageTooLarge {
            size: metadata.len(),
            max: MAX_IMAGE_BYTES,
        });
    }

    let bytes = std::fs::read(path)?;

    let format = image::guess_format(&bytes)
        .map_err(|e| ToolsError::ImageDecode(format!("unrecognized image format: {e}")))?;

    let source_media_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        other => {
            return Err(ToolsError::ImageDecode(format!(
                "unsupported image format: {other:?}"
            )));
        }
    };

    let decoded = image::load_from_memory_with_format(&bytes, format)
        .map_err(|e| ToolsError::ImageDecode(format!("failed to decode image: {e}")))?;

    let max_dimension = match detail {
        ImageDetail::High => HIGH_MAX_DIMENSION,
        ImageDetail::Original => ORIGINAL_MAX_DIMENSION,
    };

    let longest = decoded.width().max(decoded.height());
    if longest <= max_dimension {
        // No resize: keep the original bytes verbatim (lossless, cheap).
        return Ok(LoadedImage {
            media_type: source_media_type.to_string(),
            data: base64::engine::general_purpose::STANDARD.encode(&bytes),
            width: decoded.width(),
            height: decoded.height(),
        });
    }

    // Resize preserving aspect ratio, then re-encode as PNG.
    let resized = decoded.resize(max_dimension, max_dimension, image::imageops::FilterType::Triangle);
    let mut cursor = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut cursor, image::ImageFormat::Png)
        .map_err(|e| ToolsError::ImageDecode(format!("failed to encode image: {e}")))?;
    let data = base64::engine::general_purpose::STANDARD.encode(cursor.into_inner());
    Ok(LoadedImage {
        media_type: "image/png".to_string(),
        data,
        width: resized.width(),
        height: resized.height(),
    })
}
```

**Step 7: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::view_image`
Expected: 10 个测试全 PASS。

再确认注册没破坏其它测试：

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib`
Expected: 全 PASS（若工具数量被断言的测试失败，更新对应断言）。

**Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-tools
git commit -m "feat(tools): add view_image tool for reading images"
```

---

## Task 3: OpenAI —— user 消息支持图片 part

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`
- Test: 同文件 `mod tests`

**Step 1: 写失败测试**

在 `openai/types.rs` 的 `mod tests` 追加：

```rust
#[test]
fn user_message_with_image_serializes_content_parts() {
    let req = ProviderRequest {
        model: "gpt-4o".into(),
        system: None,
        messages: vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text("what is this?".into()),
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAA".into(),
                    },
                    detail: ImageDetail::High,
                },
            ],
        }],
        tools: vec![],
        params: GenParams::default(),
    };
    let o: OpenaiRequest = req.into();
    let json = serde_json::to_value(&o).unwrap();
    let content = &json["messages"][0]["content"];
    assert!(content.is_array(), "content must be an array when it has an image");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "what is this?");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,AAA");
    assert_eq!(content[1]["image_url"]["detail"], "high");
}

#[test]
fn user_message_without_image_stays_string() {
    // Regression: plain text user content must still serialize as a bare string.
    let req = ProviderRequest {
        model: "gpt-4o".into(),
        system: None,
        messages: vec![Message::user("hi")],
        tools: vec![],
        params: GenParams::default(),
    };
    let o: OpenaiRequest = req.into();
    let json = serde_json::to_value(&o).unwrap();
    assert_eq!(json["messages"][0]["content"], "hi");
}
```

顶部 `use` 补 `ImageDetail, ImageSource`（`Role` 已在 core 导出，测试里需要
`use yi_agent_core::{...}`；当前测试模块只 `use super::*` + `use yi_agent_core::{GenParams, Message}`，
把 `Message` 那行改成 `use yi_agent_core::{GenParams, ImageDetail, ImageSource, Message, Role};`）。

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types::user_message_with_image`
Expected: FAIL —— 图片被丢弃，`content` 是字符串 `"what is this?"`。

**Step 3: 实现**

在 `openai/types.rs` 顶部 `use` 补 `ImageSource`：

```rust
use yi_agent_core::{ContentBlock, ImageSource, ProviderRequest, Role, ToolSchema};
```

新增 part 类型（放在 `OpenaiContent` 之后）：

```rust
#[derive(Serialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenaiContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenaiImageUrl },
}

#[derive(Serialize, Debug)]
pub struct OpenaiImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}
```

`OpenaiContent` 增加 `Parts` 变体（`ToolCalls` 保留不动）：

```rust
#[derive(Serialize, Debug)]
#[serde(untagged)]
pub enum OpenaiContent {
    Text(String),
    Parts(Vec<OpenaiContentPart>),
    ToolCalls(Vec<OpenaiToolCall>),
}
```

在 `extract_text` 附近加辅助函数：

```rust
/// Build an `image_url` part from an image content block.
fn image_part(block: &ContentBlock) -> Option<OpenaiContentPart> {
    match block {
        ContentBlock::Image { source, detail } => {
            let url = match source {
                ImageSource::Base64 { media_type, data } => {
                    format!("data:{media_type};base64,{data}")
                }
                ImageSource::Url(url) => url.clone(),
            };
            Some(OpenaiContentPart::ImageUrl {
                image_url: OpenaiImageUrl {
                    url,
                    detail: Some(detail.as_wire_str().to_string()),
                },
            })
        }
        _ => None,
    }
}

/// User-message content: a bare string when there are no images, otherwise a
/// content-part array (OpenAI only accepts image parts in user messages).
fn user_content(blocks: &[ContentBlock]) -> OpenaiContent {
    let has_image = blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if !has_image {
        return OpenaiContent::Text(extract_text(blocks));
    }
    let mut parts: Vec<OpenaiContentPart> = Vec::new();
    let text = extract_text(blocks);
    if !text.is_empty() {
        parts.push(OpenaiContentPart::Text { text });
    }
    parts.extend(blocks.iter().filter_map(image_part));
    OpenaiContent::Parts(parts)
}
```

把 `Role::User` 分支（`:139-148`）改为：

```rust
                Role::User => {
                    messages.push(OpenaiMessage {
                        role: "user".to_string(),
                        name: None,
                        content: Some(user_content(&m.content)),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types`
Expected: 全 PASS（含既有回归测试）。

**Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs
git commit -m "feat(llm): serialize image parts in OpenAI user messages"
```

---

## Task 4: OpenAI —— tool 结果里的图片拆成 tool + user 两条消息

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs`（`Role::Tool` 分支 `:192-215`）
- Test: 同文件 `mod tests`

**背景（必读）：** Chat Completions 的 `role:"tool"` 消息 content 只能是字符串或
文本 part 数组，**不能带图片**。而且一条 assistant 消息里的 N 个 tool_call
必须紧跟 N 条 tool 消息（配对），中间不能插 user 消息。因此：先输出**所有**
tool 消息，再把**所有**图片合并进**一条**紧随其后的 user 消息。

**Step 1: 写失败测试**

```rust
#[test]
fn tool_result_with_image_splits_into_tool_then_user() {
    let result = ContentBlock::ToolResult {
        tool_use_id: "call_01".into(),
        content: vec![
            ContentBlock::Text("viewed logo.png (8x8, image/png)".into()),
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAA".into(),
                },
                detail: ImageDetail::High,
            },
        ],
        is_error: false,
    };
    let req = ProviderRequest {
        model: "gpt-4o".into(),
        system: None,
        messages: vec![Message::tool_results(vec![result])],
        tools: vec![],
        params: GenParams::default(),
    };
    let o: OpenaiRequest = req.into();
    let json = serde_json::to_value(&o).unwrap();
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "expected tool message + follow-up user message");

    // 1) tool message: string content, image replaced by placeholder
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "call_01");
    let body = msgs[0]["content"].as_str().expect("tool content must be a string");
    assert!(body.contains("viewed logo.png"));
    assert!(body.contains("image attached"));

    // 2) user message: image_url part
    assert_eq!(msgs[1]["role"], "user");
    let content = &msgs[1]["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "image_url");
    assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,AAA");
}

#[test]
fn tool_results_with_images_emit_all_tools_before_user() {
    // Two tool results, both with images: both tool messages must precede
    // the single user message, or OpenAI rejects the pairing.
    let mk = |id: &str, data: &str| ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: vec![
            ContentBlock::Text("viewed".into()),
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: data.into(),
                },
                detail: ImageDetail::High,
            },
        ],
        is_error: false,
    };
    let req = ProviderRequest {
        model: "gpt-4o".into(),
        system: None,
        messages: vec![Message::tool_results(vec![mk("c1", "AAA"), mk("c2", "BBB")])],
        tools: vec![],
        params: GenParams::default(),
    };
    let o: OpenaiRequest = req.into();
    let json = serde_json::to_value(&o).unwrap();
    let msgs = json["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "c1");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "c2");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"].as_array().unwrap().len(), 2);
}

#[test]
fn tool_result_without_image_stays_string() {
    let result = ContentBlock::ToolResult {
        tool_use_id: "call_01".into(),
        content: vec![ContentBlock::Text("ok".into())],
        is_error: false,
    };
    let req = ProviderRequest {
        model: "gpt-4o".into(),
        system: None,
        messages: vec![Message::tool_results(vec![result])],
        tools: vec![],
        params: GenParams::default(),
    };
    let o: OpenaiRequest = req.into();
    let json = serde_json::to_value(&o).unwrap();
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["content"], "ok");
}
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types::tool_result_with_image`
Expected: FAIL —— 目前只有 1 条 tool 消息、图片被丢。

**Step 3: 实现**

把 `Role::Tool` 分支整体替换为：

```rust
                Role::Tool => {
                    // OpenAI tool messages may only carry text, and all tool
                    // messages for one assistant turn must be contiguous.
                    // Collect images and emit them afterwards in a single
                    // user message (the only role that accepts image parts).
                    let mut image_parts: Vec<OpenaiContentPart> = Vec::new();

                    for block in m.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            let text = extract_text(&content);
                            let has_image = content
                                .iter()
                                .any(|b| matches!(b, ContentBlock::Image { .. }));
                            if has_image {
                                image_parts.extend(content.iter().filter_map(image_part));
                            }

                            let mut body = if is_error {
                                format!("error: {}", text)
                            } else {
                                text
                            };
                            if has_image {
                                body.push_str("\n[image attached in the following message]");
                            }

                            messages.push(OpenaiMessage {
                                role: "tool".to_string(),
                                name: None,
                                content: Some(OpenaiContent::Text(body)),
                                tool_calls: None,
                                tool_call_id: Some(tool_use_id),
                            });
                        }
                    }

                    if !image_parts.is_empty() {
                        messages.push(OpenaiMessage {
                            role: "user".to_string(),
                            name: None,
                            content: Some(OpenaiContent::Parts(image_parts)),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                }
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib openai::types`
Expected: 全 PASS。

**Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/src/openai/types.rs
git commit -m "feat(llm): split images out of OpenAI tool results into a user message"
```

---

## Task 5: Anthropic —— tool_result 图片保留回归测试

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs`（`mod tests`）

**说明：** Anthropic 的转换在 Task 1 已改好（补 `..` 解构），本任务只加一个
锁定行为的测试，防止以后有人把 tool_result 里的图片当噪音过滤掉。

**Step 1: 写测试**

在 `anthropic/types.rs` 的 `mod tests` 追加：

```rust
#[test]
fn tool_result_image_is_preserved() {
    let result = ContentBlock::ToolResult {
        tool_use_id: "t1".into(),
        content: vec![
            ContentBlock::Text("viewed logo.png (8x8, image/png)".into()),
            ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAA".into(),
                },
                detail: ImageDetail::High,
            },
        ],
        is_error: false,
    };
    let req = ProviderRequest {
        model: "claude-sonnet-4-5".into(),
        system: None,
        messages: vec![Message::tool_results(vec![result])],
        tools: vec![],
        params: GenParams::default(),
    };
    let a: AnthropicRequest = req.into();
    let json = serde_json::to_value(&a).unwrap();
    // Tool role -> "user" message whose content[0] is the tool_result.
    let content = json["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "tool_result");
    let inner = content[0]["content"].as_array().unwrap();
    assert_eq!(inner[0]["type"], "text");
    assert_eq!(inner[1]["type"], "image");
    assert_eq!(inner[1]["source"]["type"], "base64");
    assert_eq!(inner[1]["source"]["media_type"], "image/png");
    assert_eq!(inner[1]["source"]["data"], "AAA");
}
```

测试模块顶部 `use` 补 `ImageDetail, ImageSource`：
`use yi_agent_core::{GenParams, ImageDetail, ImageSource, Message};`

**Step 2: 跑测试确认通过（应当直接通过）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --lib anthropic::types::tool_result_image_is_preserved`
Expected: PASS。若 FAIL，检查 Task 1 的解构改动。

**Step 3: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs
git commit -m "test(llm): lock Anthropic tool_result image serialization"
```

---

## Task 6: 端到端 mock 验证（可选但推荐）

**Files:**
- Test: 现有 `yi-agent-rs/crates/yi-agent-llm/tests/` 下的 wiremock 测试（用 Glob 找 `tests/*.rs`），或新增一个测试文件

**目标：** 断言真实请求体里出现 `image_url`，证明从工具产物到 HTTP body 的链路
是通的（而不是只测了类型转换）。

**Step 1: 找到现有 wiremock 测试的写法**

Run: `cd yi-agent-rs && ls crates/yi-agent-llm/tests/`
然后读其中一个文件，照抄其 mock server 与 provider 构造方式。

**Step 2: 写测试**

构造一个含 `ContentBlock::Image` 的 user 消息（或 tool result），发请求，
在 wiremock 的 `received_requests()` 里断言 body 含 `"image_url"` 与
`"data:image/png;base64,"`。

**Step 3: 跑通并提交**

Run: `cd yi-agent-rs && cargo test -p yi-agent-llm --test <file>`
Expected: PASS。

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-llm/tests
git commit -m "test(llm): assert image_url reaches the OpenAI request body"
```

---

## Task 7: 文档同步

**Files:**
- Modify: `docs/bug-list.md:10`
- Modify: `docs/project-management/yi-agent-core.md:35`
- Modify: `docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/yi-agent-llm.md`
- Modify: `docs/project-management/README.md`（模块索引计数）
- Modify: `README.md`（若含计数表）

**Step 1: bug-list**

- `:10`「确认是否支持图片读取。」改为 `[x]`，并在括号里写可验证判据，
  例如：`（修复：新增 view_image 工具，见 yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs；验证：cargo test -p yi-agent-tools --lib fs::view_image）`
- 新增一条未修复项（本次不修）：

```
- [ ] compaction 的 token 估算把 `ContentBlock::Image` 记为 0 token（`yi-agent-rs/crates/yi-agent-core/src/compact.rs`、`agent.rs`），图片进入上下文后可能低估、延迟 auto-compact 触发
```

**Step 2: 模块文件**

- `yi-agent-core.md:35`「图片工具」改为 `[x]`，指向 `message.rs` 的
  `ImageDetail` 与设计文档链接。
- `yi-agent-tools.md`：登记 `view_image` 工具（名称、参数、只读、测试命令）。
- `yi-agent-llm.md`：登记「OpenAI 图片序列化（user 消息 image_url；tool 结果
  拆分为 tool + user 两条）」与「Anthropic tool_result 图片」。
- `README.md` / `docs/project-management/README.md`：同步"完成 / 总计"计数。

**Step 3: 提交**

```bash
git add docs README.md
git commit -m "docs: record image reading (view_image) support"
```

---

## Task 8: 收尾验证

**Step 1: 确认没有残留 cargo 进程**

Run: `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo clean`

**Step 2: 格式化**

Run: `cd yi-agent-rs && cargo fmt --all && git status --short`
Expected: 无未格式化改动。

**Step 3: 按 crate 跑测试**

Run（逐个跑，不要 `--workspace`）：
```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib
cd yi-agent-rs && cargo test -p yi-agent-tools --lib
cd yi-agent-rs && cargo test -p yi-agent-llm --lib
```
Expected: 全 PASS。

**Step 4: 跑仓库 CI 入口**

Run: `cd yi-agent-rs && just ci`（若 `just ci` 会跑 workspace 全量而触发 OOM，
改用 `just fmt-check` + 上述三个 crate 的测试）
Expected: PASS。

**Step 5: 提交遗留改动（若有）**

```bash
git status --short
git add -A && git commit -m "chore: fmt"
```

**Step 6: 合并回 main**

按 `superpowers:finishing-a-development-branch`：

```bash
# 回到主仓库
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/view-image-tool
git worktree remove .worktrees/feat/view-image-tool
git branch -d feat/view-image-tool
```

注意：主仓库工作区有用户未提交的 `docs/bug-list.md` 改动，合并时若冲突，
**不要**丢弃用户那两行笔记。

---

## 备注：与设计文档的偏差

- 设计文档第 4.4 节把「`detail` 取值非法」列为 `ToolsError::ArgsParse`。
  本计划用 `DetailArg` 枚举 + serde 反序列化实现，非法值自然产生
  `serde_json::Error` → `ArgsParse`，与设计一致，无需额外错误变体。
- 设计文档提到的 `UnsupportedImageFormat` 错误，本计划统一用
  `ToolsError::ImageDecode(String)` 承载（格式不识别与解码失败合并），
  减少错误变体数量。
