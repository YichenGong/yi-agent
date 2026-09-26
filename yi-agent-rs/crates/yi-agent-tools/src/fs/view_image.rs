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
    let resized = decoded.resize(
        max_dimension,
        max_dimension,
        image::imageops::FilterType::Triangle,
    );
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
}
