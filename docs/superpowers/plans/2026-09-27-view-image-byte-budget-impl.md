# view_image 编码后字节预算 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `view_image` 返回的图片 base64 不超过一个可配置预算，超限时按"源格式无损 → JPEG 质量阶梯 → 降分辨率"自动降级；仍不可达时返回**占位文字**而非报错。

**Architecture:** 全部改动落在 `crates/yi-agent-tools/src/fs/view_image.rs`。把现有"直通 or 一律转 PNG"的两分支换成一条 format-aware 阶梯函数 `encode_within_budget`。预算在构造期从 env 解析一次存进结构体，并通过参数注入纯函数以便测试。`LoadedImage` 由 struct 改为 enum（`Ready` / `Omitted`），`call()` 据此决定是否附带 `ContentBlock::Image`。

**Tech Stack:** Rust 2024 edition、`image` 0.25（features png/jpeg/gif/webp）、`base64` 0.22、`tracing` 0.1、tokio（测试）、tempfile。

**设计文档：** `docs/superpowers/specs/2026-09-27-view-image-byte-budget-design.md`

**工作目录：** 执行时按 CLAUDE.md 新建 worktree（例如
`.worktrees/feat/view-image-byte-budget`，分支 `feat/view-image-byte-budget`）。
以下所有相对路径以仓库根为准；Rust 命令在 `yi-agent-rs/` 下执行。

---

## Global Constraints

- **不在 `main` 上改代码**：所有变更先建 worktree + 分支（CLAUDE.md）。
- **Commit message 不写 `Co-Authored-By`**；用 conventional commits；首行 ≤72 字符。
- **提交前必须跑 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- **不要并发跑 `cargo test`**；被中断会留僵尸测试进程导致后续卡死/exit 137。
- **不跑 `cargo test --workspace`**；按 crate 跑。
- 预算默认值：`MAX_ENCODED_BASE64_BYTES = 4 * 1024 * 1024`（≈3 MiB 原始字节）。
- env 覆盖名：`YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES`；解析失败回退默认值 + `tracing::warn`。
- 分辨率下限 `MIN_DIMENSION = 512`；降采样步长 `×0.75`；JPEG 质量阶梯 `[85, 70, 55, 40]`。
- 现有 `MAX_IMAGE_BYTES = 20 MiB`（读盘闸门）与 `ToolsError::ImageTooLarge` **保留不动**。
- 编码后预算耗尽**不走** `ToolsError`，返回占位文字 `ToolResult`。
- 缩放滤镜沿用 `FilterType::Triangle`（`view_image.rs:199`）。
- **实现约束（已通过编译验证）**：编码一律走 `DynamicImage::write_with_encoder`
  （inherent 方法，**无需 import `ImageEncoder` trait**；它会经 `make_compatible_img`
  自动处理 RGBA→JPEG 的 alpha 丢弃）。测试里 `base64 ... .decode(...)` 需要
  `use base64::Engine;`（该 import 已在 `view_image.rs:5`）。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs` — 全部实现 + 测试（本 crate 既有约定：工具与其单测同文件）。
- `docs/project-management/yi-agent-tools.md` — feature 行补预算行为。
- `docs/bug-list.md` — 413 那条改为已修。

不改：`yi-agent-llm`、`yi-agent-core`、provider 层（compaction 估算见另一计划）。

**任务粒度说明：** `LoadedImage` 由 struct 改 enum 后，`load_image` 的构造点与
`call()` 的解构点会同时编译失败，三者必须原子地一起改。因此"阶梯+枚举+接线"
合并为 Task 2，不拆。

---

## Task 1: 预算解析 + 构造期注入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:14-29`（常量 + 结构体）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:105`（`load_image` 调用点传预算）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:136`（`load_image` 签名加 `budget`）
- Test: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs`（`mod tests`）

**Interfaces:**
- Produces:
  - `const MAX_ENCODED_BASE64_BYTES: usize = 4 * 1024 * 1024;`
  - `const MAX_ENCODED_ENV: &str = "YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES";`
  - `fn parse_budget(raw: Option<&str>) -> usize` — 纯函数，可测
  - `fn resolve_budget() -> usize` — 读 env，走 `parse_budget`
  - `struct ViewImageTool { ctx: Arc<ToolsContext>, budget: usize }`
  - `fn load_image(path: &Path, detail: ImageDetail, budget: usize) -> Result<LoadedImage, ToolsError>`（本任务只加参数，阶梯在 Task 2）

- [ ] **Step 1: 写失败测试（`parse_budget`）**

加到 `mod tests` 里：

```rust
    #[test]
    fn parse_budget_uses_value_when_present() {
        assert_eq!(parse_budget(Some("1048576")), 1_048_576);
    }

    #[test]
    fn parse_budget_falls_back_on_missing_or_invalid() {
        assert_eq!(parse_budget(None), MAX_ENCODED_BASE64_BYTES);
        assert_eq!(parse_budget(Some("not-a-number")), MAX_ENCODED_BASE64_BYTES);
        assert_eq!(parse_budget(Some("")), MAX_ENCODED_BASE64_BYTES);
        assert_eq!(parse_budget(Some("0")), MAX_ENCODED_BASE64_BYTES);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-tools --lib fs::view_image::tests::parse_budget`
Expected: FAIL（`cannot find function parse_budget`）

- [ ] **Step 3: 实现常量、解析函数与构造期注入**

在 `view_image.rs` 常量区（`MAX_IMAGE_BYTES` 之后）加：

```rust
/// base64 编码后的字节上限（真正上 wire 的体积；≈3 MiB 原始字节）。
const MAX_ENCODED_BASE64_BYTES: usize = 4 * 1024 * 1024;
/// 覆盖 `MAX_ENCODED_BASE64_BYTES` 的环境变量名。
const MAX_ENCODED_ENV: &str = "YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES";

/// 解析预算。`None` / 非法 / 0 一律回退默认值。
fn parse_budget(raw: Option<&str>) -> usize {
    match raw.map(str::trim).and_then(|s| s.parse::<usize>().ok()) {
        Some(value) if value > 0 => value,
        _ => MAX_ENCODED_BASE64_BYTES,
    }
}

/// 从环境变量解析预算；非法值记一条 warn。
fn resolve_budget() -> usize {
    match std::env::var(MAX_ENCODED_ENV) {
        Ok(raw) => {
            let budget = parse_budget(Some(&raw));
            if budget == MAX_ENCODED_BASE64_BYTES && raw.trim().parse::<usize>().is_err() {
                tracing::warn!(
                    env = MAX_ENCODED_ENV,
                    value = %raw,
                    "invalid view_image byte budget; using default"
                );
            }
            budget
        }
        Err(_) => MAX_ENCODED_BASE64_BYTES,
    }
}
```

结构体与构造器改为：

```rust
pub struct ViewImageTool {
    ctx: Arc<ToolsContext>,
    budget: usize,
}

impl ViewImageTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self {
            ctx,
            budget: resolve_budget(),
        }
    }
}
```

`call()` 里把预算传下去：

```rust
        match load_image(&resolved, detail, self.budget) {
```

`load_image` 签名加参数（本任务先忽略它，Task 2 使用）：

```rust
fn load_image(path: &Path, detail: ImageDetail, budget: usize) -> Result<LoadedImage, ToolsError> {
    let _ = budget; // 由 Task 2 的阶梯消费
```

- [ ] **Step 4: 跑测试与编译**

Run: `cargo test -p yi-agent-tools --lib fs::view_image 2>&1 | tail -20`
Expected: PASS（新增 2 个 + 既有 10 个 = 12 passed）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/fs/view_image.rs
git commit -m "feat(tools): inject view_image encoded-byte budget"
```

---

## Task 2: format-aware 阶梯 + 枚举 + 接线（原子任务）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:56-61`（`LoadedImage` struct → enum）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:105-123`（`call()`）
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/view_image.rs:136-212`（`load_image` 末尾）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `budget` / `load_image(.., budget)`（Task 1）
- Produces:
  - `const MIN_DIMENSION: u32 = 512;`
  - `const JPEG_QUALITIES: [u8; 4] = [85, 70, 55, 40];`
  - `enum LoadedImage { Ready { media_type: String, data: String, width: u32, height: u32, note: Option<String> }, Omitted { message: String } }`
  - `fn b64_len(raw_bytes: usize) -> usize`
  - `fn lossless_target(image::ImageFormat) -> Option<&'static str>`
  - `fn media_type_for(image::ImageFormat) -> &'static str`
  - `fn resize_to_cap<'a>(&'a DynamicImage, u32) -> Cow<'a, DynamicImage>`
  - `fn lossless_encode(&DynamicImage, ImageFormat) -> Result<Option<(Vec<u8>, &'static str)>, ToolsError>`
  - `fn jpeg_encode(&DynamicImage, u8) -> Result<Vec<u8>, ToolsError>`
  - `fn encode_within_budget(&DynamicImage, ImageFormat, &[u8], u32, usize) -> Result<LoadedImage, ToolsError>`
  - 测试辅助 `fn write_noisy_png(path: &Path, w: u32, h: u32)`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 顶部加噪声图辅助（噪声不可压缩，否则 PNG 会压到几 KB、永远触不到预算）：

```rust
    /// 确定性伪随机噪声 PNG：不可压缩，确保能触发预算。
    fn write_noisy_png(path: &Path, w: u32, h: u32) {
        let mut state: u32 = 0x1234_5678;
        let img = RgbImage::from_fn(w, h, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            Rgb([
                (state & 0xff) as u8,
                (state >> 8 & 0xff) as u8,
                (state >> 16 & 0xff) as u8,
            ])
        });
        img.save_with_format(path, ImageFormat::Png).unwrap();
    }
```

测试：

```rust
    #[test]
    fn b64_len_matches_real_encoding() {
        for n in [0usize, 1, 2, 3, 4, 100, 1000] {
            let raw = vec![7u8; n];
            let real = base64::engine::general_purpose::STANDARD.encode(&raw).len();
            assert_eq!(b64_len(n), real, "n={n}");
        }
    }

    #[test]
    fn lossless_target_excludes_jpeg_and_gif() {
        assert!(lossless_target(ImageFormat::Png).is_some());
        assert!(lossless_target(ImageFormat::WebP).is_some());
        assert!(lossless_target(ImageFormat::Jpeg).is_none());
        assert!(lossless_target(ImageFormat::Gif).is_none());
    }

    #[test]
    fn encode_within_budget_shrinks_oversize_photo() {
        let bytes = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("photo.png");
            write_noisy_png(&path, 3000, 2000);
            std::fs::read(&path).unwrap()
        };
        let decoded = image::load_from_memory(&bytes).unwrap();
        let loaded =
            encode_within_budget(&decoded, ImageFormat::Png, &bytes, HIGH_MAX_DIMENSION, 256 * 1024)
                .unwrap();
        match loaded {
            LoadedImage::Ready { data, media_type, note, .. } => {
                assert!(data.len() <= 256 * 1024, "data {} > budget", data.len());
                assert_eq!(media_type, "image/jpeg");
                assert!(note.is_some(), "degradation must be noted");
                let raw = base64::engine::general_purpose::STANDARD.decode(&data).unwrap();
                image::load_from_memory(&raw).unwrap();
            }
            LoadedImage::Omitted { message } => panic!("unexpected omitted: {message}"),
        }
    }

    #[test]
    fn encode_within_budget_passes_through_when_it_fits() {
        let bytes = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("small.png");
            write_png(&path, 8, 8);
            std::fs::read(&path).unwrap()
        };
        let decoded = image::load_from_memory(&bytes).unwrap();
        let loaded = encode_within_budget(
            &decoded,
            ImageFormat::Png,
            &bytes,
            HIGH_MAX_DIMENSION,
            MAX_ENCODED_BASE64_BYTES,
        )
        .unwrap();
        match loaded {
            LoadedImage::Ready { data, media_type, note, .. } => {
                assert_eq!(media_type, "image/png");
                assert!(note.is_none(), "passthrough must not be noted");
                let raw = base64::engine::general_purpose::STANDARD.decode(&data).unwrap();
                assert_eq!(raw, bytes, "step 0 must keep source bytes verbatim");
            }
            LoadedImage::Omitted { .. } => panic!("should fit"),
        }
    }

    #[test]
    fn encode_within_budget_keeps_png_for_screenshots() {
        // 大片纯色 + 少量线条：PNG 极小，转 JPEG 反而更大。
        let mut img = RgbImage::from_pixel(4000, 3000, Rgb([250, 250, 250]));
        for x in 0..4000 {
            img.put_pixel(x, 10, Rgb([0, 0, 0]));
        }
        let bytes = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("shot.png");
            img.save_with_format(&path, ImageFormat::Png).unwrap();
            std::fs::read(&path).unwrap()
        };
        let decoded = image::load_from_memory(&bytes).unwrap();
        let loaded =
            encode_within_budget(&decoded, ImageFormat::Png, &bytes, HIGH_MAX_DIMENSION, 64 * 1024)
                .unwrap();
        match loaded {
            LoadedImage::Ready { media_type, .. } => assert_eq!(media_type, "image/png"),
            LoadedImage::Omitted { .. } => panic!("screenshot PNG should fit"),
        }
    }

    #[test]
    fn encode_within_budget_jpeg_source_never_becomes_png() {
        let mut img = RgbImage::new(1200, 900);
        let mut state: u32 = 99;
        for (_, _, p) in img.enumerate_pixels_mut() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *p = Rgb([
                (state & 0xff) as u8,
                (state >> 8 & 0xff) as u8,
                (state >> 16 & 0xff) as u8,
            ]);
        }
        let bytes = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("pic.jpg");
            img.save_with_format(&path, ImageFormat::Jpeg).unwrap();
            std::fs::read(&path).unwrap()
        };
        let decoded = image::load_from_memory(&bytes).unwrap();
        let loaded =
            encode_within_budget(&decoded, ImageFormat::Jpeg, &bytes, HIGH_MAX_DIMENSION, 256 * 1024)
                .unwrap();
        match loaded {
            LoadedImage::Ready { media_type, note, .. } => {
                assert_eq!(media_type, "image/jpeg", "jpeg source must not become png");
                assert!(note.is_some());
            }
            LoadedImage::Omitted { message } => panic!("jpeg source should fit: {message}"),
        }
    }

    #[test]
    fn encode_within_budget_omits_when_unreachable() {
        let bytes = {
            let tmp = TempDir::new().unwrap();
            let path = tmp.path().join("photo.png");
            write_noisy_png(&path, 2000, 1500);
            std::fs::read(&path).unwrap()
        };
        let decoded = image::load_from_memory(&bytes).unwrap();
        let loaded =
            encode_within_budget(&decoded, ImageFormat::Png, &bytes, HIGH_MAX_DIMENSION, 1).unwrap();
        match loaded {
            LoadedImage::Omitted { message } => {
                assert!(message.contains("image omitted"), "message: {message}");
            }
            LoadedImage::Ready { .. } => panic!("1-byte budget must be unreachable"),
        }
    }
```

再加 `call()` 级别的接线测试：

```rust
    #[tokio::test]
    async fn view_image_label_notes_degradation() {
        let tmp = TempDir::new().unwrap();
        write_noisy_png(&tmp.path().join("photo.png"), 3000, 2000);
        let tool = ViewImageTool {
            ctx: Arc::new(ToolsContext::new(tmp.path().to_path_buf())),
            budget: 256 * 1024,
        };
        let result = tool.call(serde_json::json!({"path": "photo.png"})).await;
        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text(t) => {
                assert!(t.contains("viewed"));
                assert!(t.contains("jpeg") || t.contains("downscaled"), "label: {t}");
            }
            _ => panic!("expected leading text label"),
        }
    }

    #[tokio::test]
    async fn view_image_omits_image_with_note_when_budget_unreachable() {
        let tmp = TempDir::new().unwrap();
        write_noisy_png(&tmp.path().join("photo.png"), 3000, 2000);
        let tool = ViewImageTool {
            ctx: Arc::new(ToolsContext::new(tmp.path().to_path_buf())),
            budget: 1,
        };
        let result = tool.call(serde_json::json!({"path": "photo.png"})).await;
        assert!(!result.is_error, "omission is not an error");
        assert!(
            !result
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "omitted result must carry no image block"
        );
        let text = result
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text(t) => Some(t.clone()),
                _ => None,
            })
            .expect("expected text");
        assert!(text.contains("image omitted"), "text: {text}");
        assert!(!text.contains("data:image"), "must not leak base64");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-tools --lib fs::view_image 2>&1 | tail -30`
Expected: FAIL（`cannot find function encode_within_budget` / `b64_len` / `lossless_target`）

- [ ] **Step 3: 实现（枚举 + 辅助 + 阶梯 + 接线）**

先把 `LoadedImage` 由 struct 改为 enum（替换 `:56-61`）：

```rust
/// 加载结果：要么是可用的图片，要么是超预算后的占位文字。
enum LoadedImage {
    Ready {
        media_type: String,
        data: String,
        width: u32,
        height: u32,
        /// 降级信息，如 `"jpeg q70"` / `"jpeg q55, downscaled"`；未降级为 `None`。
        note: Option<String>,
    },
    Omitted {
        message: String,
    },
}
```

常量（`MIN_DIMENSION` / `JPEG_QUALITIES`）与辅助函数：

```rust
/// 分辨率下限：降采样到此为止。
const MIN_DIMENSION: u32 = 512;
/// JPEG 质量阶梯（从高到低）。
const JPEG_QUALITIES: [u8; 4] = [85, 70, 55, 40];

/// base64 编码后的字节数（不分配即可算：每 3 字节 -> 4 字符）。
fn b64_len(raw_bytes: usize) -> usize {
    raw_bytes.div_ceil(3) * 4
}

/// 源格式对应的无损编码目标；`None` 表示该源格式没有可用的无损路径。
fn lossless_target(format: image::ImageFormat) -> Option<&'static str> {
    match format {
        image::ImageFormat::Png => Some("image/png"),
        image::ImageFormat::WebP => Some("image/webp"),
        _ => None,
    }
}

/// 源格式 -> MIME（与既有 `source_media_type` 判定一致）。
fn media_type_for(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => "application/octet-stream",
    }
}

/// 在 `cap` 内等比缩放；不放大。返回 Cow 以便未缩放时零拷贝。
fn resize_to_cap<'a>(
    decoded: &'a image::DynamicImage,
    cap: u32,
) -> std::borrow::Cow<'a, image::DynamicImage> {
    let longest = decoded.width().max(decoded.height());
    if longest <= cap {
        std::borrow::Cow::Borrowed(decoded)
    } else {
        std::borrow::Cow::Owned(decoded.resize(cap, cap, image::imageops::FilterType::Triangle))
    }
}

/// 无损重编码（PNG / WebP-lossless）。返回 `(raw_bytes, media_type)`。
///
/// 用 `DynamicImage::write_with_encoder`：它是 inherent 方法（无需 import
/// `ImageEncoder`），并经 `make_compatible_img` 自动做色彩类型转换。
fn lossless_encode(
    image: &image::DynamicImage,
    format: image::ImageFormat,
) -> Result<Option<(Vec<u8>, &'static str)>, ToolsError> {
    let mut buffer = Vec::new();
    match format {
        image::ImageFormat::Png => {
            image
                .write_with_encoder(image::codecs::png::PngEncoder::new(&mut buffer))
                .map_err(|e| ToolsError::ImageDecode(format!("png encode failed: {e}")))?;
        }
        image::ImageFormat::WebP => {
            image
                .write_with_encoder(image::codecs::webp::WebPEncoder::new_lossless(&mut buffer))
                .map_err(|e| ToolsError::ImageDecode(format!("webp encode failed: {e}")))?;
        }
        // JPEG/GIF 没有可用的无损路径（GIF 会被解成首帧）。
        _ => return Ok(None),
    }
    let media_type = lossless_target(format).expect("checked by match above");
    Ok(Some((buffer, media_type)))
}

/// 有损 JPEG 编码；alpha 由 `write_with_encoder` 自动丢弃。
fn jpeg_encode(image: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, ToolsError> {
    let mut buffer = Vec::new();
    image
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut buffer,
            quality,
        ))
        .map_err(|e| ToolsError::ImageDecode(format!("jpeg encode failed: {e}")))?;
    Ok(buffer)
}

fn encode_within_budget(
    decoded: &image::DynamicImage,
    source_format: image::ImageFormat,
    source_bytes: &[u8],
    dim_cap: u32,
    budget: usize,
) -> Result<LoadedImage, ToolsError> {
    let width = decoded.width();
    let height = decoded.height();
    let longest = width.max(height);

    // 步骤 0：原字节直通（无损、零重编码）。
    if longest <= dim_cap && b64_len(source_bytes.len()) <= budget {
        let data = base64::engine::general_purpose::STANDARD.encode(source_bytes);
        let media_type = media_type_for(source_format).to_string();
        return Ok(LoadedImage::Ready {
            media_type,
            data,
            width,
            height,
            note: None,
        });
    }

    let mut cap = dim_cap;
    let mut last_size = b64_len(source_bytes.len());
    loop {
        let scaled = resize_to_cap(decoded, cap);
        let downscaled = scaled.width() < width || scaled.height() < height;
        let down_note = if downscaled { Some("downscaled") } else { None };

        // 步骤 1：按源格式无损重编码。
        if let Some((bytes, media_type)) = lossless_encode(scaled.as_ref(), source_format)? {
            last_size = b64_len(bytes.len());
            if last_size <= budget {
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                return Ok(LoadedImage::Ready {
                    media_type: media_type.to_string(),
                    data,
                    width: scaled.width(),
                    height: scaled.height(),
                    note: down_note.map(str::to_string),
                });
            }
        }

        // 步骤 2：JPEG 质量阶梯。
        for quality in JPEG_QUALITIES {
            let bytes = jpeg_encode(scaled.as_ref(), quality)?;
            last_size = b64_len(bytes.len());
            if last_size <= budget {
                let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                let note = match down_note {
                    Some(d) => format!("jpeg q{quality}, {d}"),
                    None => format!("jpeg q{quality}"),
                };
                return Ok(LoadedImage::Ready {
                    media_type: "image/jpeg".to_string(),
                    data,
                    width: scaled.width(),
                    height: scaled.height(),
                    note: Some(note),
                });
            }
        }

        // 步骤 3：降分辨率重试（512 这一档也要完整试一次）。
        if cap <= MIN_DIMENSION {
            break;
        }
        cap = (cap * 3 / 4).max(MIN_DIMENSION);
    }

    // 步骤 4：占位文字（非报错）。
    Ok(LoadedImage::Omitted {
        message: format!(
            "image omitted: still {last_size} bytes at {MIN_DIMENSION}px downscaling, budget {budget}"
        ),
    })
}
```

`load_image` 末尾（保留 `:137-177` 的 metadata / 目录 / 大小 / `guess_format` / decode）
把那两处"直通 or PNG"分支**整体替换**为：

```rust
    let max_dimension = match detail {
        ImageDetail::High => HIGH_MAX_DIMENSION,
        ImageDetail::Original => ORIGINAL_MAX_DIMENSION,
    };

    encode_within_budget(&decoded, format, &bytes, max_dimension, budget)
```

并移除 Task 1 里那行占位 `let _ = budget;`。

`call()` 的组装改为（替换 `:105-123`）：

```rust
        match load_image(&resolved, detail, self.budget) {
            Ok(LoadedImage::Ready {
                media_type,
                data,
                width,
                height,
                note,
            }) => {
                let label = match &note {
                    Some(note) => {
                        format!("viewed {} ({width}x{height}, {media_type}, {note})", args.path)
                    }
                    None => format!("viewed {} ({width}x{height}, {media_type})", args.path),
                };
                ToolResult::with_content(vec![
                    ContentBlock::Text(label),
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, data },
                        detail,
                    },
                ])
            }
            Ok(LoadedImage::Omitted { message }) => {
                ToolResult::text(format!("viewed {} ({message})", args.path))
            }
            Err(e) => e.into(),
        }
```

- [ ] **Step 4: 跑全部 view_image 测试（回归 + 新增）**

Run: `cargo test -p yi-agent-tools --lib fs::view_image 2>&1 | tail -40`
Expected: PASS。既有 10 个不回归（尤其 `view_image_original_detail_keeps_more_resolution`、
`view_image_resizes_large_image`、`view_image_label_mentions_path_and_dimensions`），新增全通过。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/fs/view_image.rs
git commit -m "feat(tools): add format-aware byte ladder and placeholder for view_image"
```

---

## Task 3: 文档与总验证

**Files:**
- Modify: `docs/project-management/yi-agent-tools.md:27`
- Modify: `docs/bug-list.md:44`

- [ ] **Step 1: 更新 tools 文档**

把 `docs/project-management/yi-agent-tools.md:27` 的 view_image 行保留原句并追加：
"base64 编码后受 `MAX_ENCODED_BASE64_BYTES`（默认 4 MiB，
`YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES` 覆盖）约束；超限按
`源格式无损 → JPEG q85/70/55/40 → ×0.75 降采样(下限512)` 降级，降级注记在 label；
仍不可达则返回 `image omitted` 占位文字（非报错）。"

- [ ] **Step 2: 更新 bug-list**

把 `docs/bug-list.md:44` 的 413 待办改为已修：`- [x] ...`，追加
"修复：view_image 编码后字节预算 + 阶梯降级 + 占位兜底
（`crates/yi-agent-tools/src/fs/view_image.rs`）；验证：
`cargo test -p yi-agent-tools --lib fs::view_image`"。保留原文以便追溯。

- [ ] **Step 3: 全 crate 回归**

Run: `cargo test -p yi-agent-tools --lib 2>&1 | tail -15`
Expected: 全绿（无 view_image 之外的回归）。

- [ ] **Step 4: fmt 检查**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Expected: 无输出。

- [ ] **Step 5: 提交**

```bash
git add docs/project-management/yi-agent-tools.md docs/bug-list.md
git commit -m "docs: record view_image byte budget behavior and close 413 todo"
```

---

## 验收（对应 spec §7）

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::view_image
```

必须证明：(a) 超限照片被降进预算；(b) 能无损时不做有损；(c) 截图不被无谓转 JPEG；
(d) 降级在 label 里可见；(e) 预算不可达时返回占位文字且不回传字节；
(f) JPEG 源跳过无损步 / WebP 源走 WebP 无损。
