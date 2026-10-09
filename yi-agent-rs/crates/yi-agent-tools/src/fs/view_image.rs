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
/// base64 编码后的字节上限（真正上 wire 的体积；≈3 MiB 原始字节）。
const MAX_ENCODED_BASE64_BYTES: usize = 4 * 1024 * 1024;
/// 覆盖 `MAX_ENCODED_BASE64_BYTES` 的环境变量名。
const MAX_ENCODED_ENV: &str = "YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES";
/// Longest-edge cap for `detail: high`.
const HIGH_MAX_DIMENSION: u32 = 2048;
/// Longest-edge cap for `detail: original`.
const ORIGINAL_MAX_DIMENSION: u32 = 6000;

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
                        path: None,
                    },
                ])
            }
            Ok(LoadedImage::Omitted { message }) => {
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

fn load_image(path: &Path, detail: ImageDetail, budget: usize) -> Result<LoadedImage, ToolsError> {
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

    // 校验格式受支持；MIME 由 `encode_within_budget` 内的 `media_type_for` 决定。
    match format {
        image::ImageFormat::Png
        | image::ImageFormat::Jpeg
        | image::ImageFormat::Gif
        | image::ImageFormat::WebP => {}
        other => {
            return Err(ToolsError::ImageDecode(format!(
                "unsupported image format: {other:?}"
            )));
        }
    }

    let decoded = image::load_from_memory_with_format(&bytes, format)
        .map_err(|e| ToolsError::ImageDecode(format!("failed to decode image: {e}")))?;

    let max_dimension = match detail {
        ImageDetail::High => HIGH_MAX_DIMENSION,
        ImageDetail::Original => ORIGINAL_MAX_DIMENSION,
    };

    encode_within_budget(&decoded, format, &bytes, max_dimension, budget)
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
