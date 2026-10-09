use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_and_check;
use crate::image_prep::{self, PreparedImage};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use yi_agent_core::{
    ContentBlock, ImageDetail, ImageSource, Tool, ToolMetadata, ToolResult, ToolSource,
};

pub struct ViewImageTool {
    ctx: Arc<ToolsContext>,
    budget: usize,
}

impl ViewImageTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self {
            ctx,
            budget: image_prep::resolve_budget(),
        }
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

        match image_prep::prepare_image_file(&resolved, detail, self.budget) {
            Ok(PreparedImage::Ready {
                media_type,
                data,
                width,
                height,
                note,
            }) => {
                let label = match &note {
                    Some(note) => {
                        format!(
                            "viewed {} ({width}x{height}, {media_type}, {note})",
                            args.path
                        )
                    }
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
            Ok(PreparedImage::Omitted { message }) => {
                ToolResult::text(format!("viewed {} ({message})", args.path))
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

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::path::Path;
    use tempfile::TempDir;

    // 管线已抽到 `crate::image_prep`（见该模块文档）。这里把这些等价名字带进
    // 作用域，使既有断言与测试体逐字不变；`PreparedImage` 即原 `LoadedImage`。
    use crate::image_prep::PreparedImage as LoadedImage;
    use crate::image_prep::{
        HIGH_MAX_DIMENSION, MAX_ENCODED_BASE64_BYTES, MAX_IMAGE_BYTES, b64_len,
        encode_within_budget, lossless_target, parse_budget,
    };

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
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .unwrap(),
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
        assert!(
            (683i64 - decoded.height() as i64).abs() <= 1,
            "unexpected height {}",
            decoded.height()
        );
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
        // Under the 6000 cap -> untouched at 3000 wide.
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
        let loaded = encode_within_budget(
            &decoded,
            ImageFormat::Png,
            &bytes,
            HIGH_MAX_DIMENSION,
            256 * 1024,
        )
        .unwrap();
        match loaded {
            LoadedImage::Ready {
                data,
                media_type,
                note,
                ..
            } => {
                assert!(data.len() <= 256 * 1024, "data {} > budget", data.len());
                assert_eq!(media_type, "image/jpeg");
                assert!(note.is_some(), "degradation must be noted");
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .unwrap();
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
            LoadedImage::Ready {
                data,
                media_type,
                note,
                ..
            } => {
                assert_eq!(media_type, "image/png");
                assert!(note.is_none(), "passthrough must not be noted");
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .unwrap();
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
        let loaded = encode_within_budget(
            &decoded,
            ImageFormat::Png,
            &bytes,
            HIGH_MAX_DIMENSION,
            64 * 1024,
        )
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
        let loaded = encode_within_budget(
            &decoded,
            ImageFormat::Jpeg,
            &bytes,
            HIGH_MAX_DIMENSION,
            256 * 1024,
        )
        .unwrap();
        match loaded {
            LoadedImage::Ready {
                media_type, note, ..
            } => {
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
            encode_within_budget(&decoded, ImageFormat::Png, &bytes, HIGH_MAX_DIMENSION, 1)
                .unwrap();
        match loaded {
            LoadedImage::Omitted { message } => {
                assert!(message.contains("image omitted"), "message: {message}");
            }
            LoadedImage::Ready { .. } => panic!("1-byte budget must be unreachable"),
        }
    }

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
}
