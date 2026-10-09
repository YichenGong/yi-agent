//! 图片预处理管线：把磁盘/内存里的图片字节变成可上 wire 的形态。
//!
//! 这段逻辑原本私有于 `fs::view_image`。app-server 的附件上传（`image/read`）
//! 需要逐字一致的行为，所以抽成公共模块，避免两处实现漂移。
//!
//! 模块边界：只负责「字节 -> PreparedImage」，不负责根校验。
//! 路径限根（`path_util::resolve_and_check`）是工具侧的职责。

use crate::error::ToolsError;
use base64::Engine;
use std::path::Path;
use yi_agent_core::ImageDetail;

/// Largest image file we will read from disk (guards memory before decode).
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// base64 编码后的字节上限（真正上 wire 的体积；≈3 MiB 原始字节）。
pub const DEFAULT_BASE64_BUDGET: usize = 4 * 1024 * 1024;
/// 内部沿用名，等价于 `DEFAULT_BASE64_BUDGET`。
pub(crate) const MAX_ENCODED_BASE64_BYTES: usize = DEFAULT_BASE64_BUDGET;
/// 覆盖 `DEFAULT_BASE64_BUDGET` 的环境变量名。
const MAX_ENCODED_ENV: &str = "YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES";
/// Longest-edge cap for `detail: high`.
pub(crate) const HIGH_MAX_DIMENSION: u32 = 2048;
/// Longest-edge cap for `detail: original`.
const ORIGINAL_MAX_DIMENSION: u32 = 6000;
/// 分辨率下限：降采样到此为止。
const MIN_DIMENSION: u32 = 512;
/// JPEG 质量阶梯（从高到低）。
const JPEG_QUALITIES: [u8; 4] = [85, 70, 55, 40];

/// 解析预算。`None` / 非法 / 0 一律回退默认值。
pub(crate) fn parse_budget(raw: Option<&str>) -> usize {
    match raw.map(str::trim).and_then(|s| s.parse::<usize>().ok()) {
        Some(value) if value > 0 => value,
        _ => MAX_ENCODED_BASE64_BYTES,
    }
}

/// 从环境变量解析预算；非法值记一条 warn。
pub fn resolve_budget() -> usize {
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

/// base64 编码后的字节数（不分配即可算：每 3 字节 -> 4 字符）。
pub(crate) fn b64_len(raw_bytes: usize) -> usize {
    raw_bytes.div_ceil(3) * 4
}

/// 源格式对应的无损编码目标；`None` 表示该源格式没有可用的无损路径。
pub(crate) fn lossless_target(format: image::ImageFormat) -> Option<&'static str> {
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

pub(crate) fn encode_within_budget(
    decoded: &image::DynamicImage,
    source_format: image::ImageFormat,
    source_bytes: &[u8],
    dim_cap: u32,
    budget: usize,
) -> Result<PreparedImage, ToolsError> {
    let width = decoded.width();
    let height = decoded.height();
    let longest = width.max(height);

    // 步骤 0：原字节直通（无损、零重编码）。
    if longest <= dim_cap && b64_len(source_bytes.len()) <= budget {
        let data = base64::engine::general_purpose::STANDARD.encode(source_bytes);
        let media_type = media_type_for(source_format).to_string();
        return Ok(PreparedImage::Ready {
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
                return Ok(PreparedImage::Ready {
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
                return Ok(PreparedImage::Ready {
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
    Ok(PreparedImage::Omitted {
        message: format!(
            "image omitted: still {last_size} bytes at {MIN_DIMENSION}px downscaling, budget {budget}"
        ),
    })
}

/// 加载结果：要么是可用的图片，要么是超预算后的占位文字。
#[derive(Debug)]
pub enum PreparedImage {
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

/// 把已读入内存的图片字节处理成可上 wire 的形态。
pub fn prepare_image_bytes(
    bytes: &[u8],
    detail: ImageDetail,
    budget: usize,
) -> Result<PreparedImage, ToolsError> {
    let format = image::guess_format(bytes)
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

    let decoded = image::load_from_memory_with_format(bytes, format)
        .map_err(|e| ToolsError::ImageDecode(format!("failed to decode image: {e}")))?;

    let max_dimension = match detail {
        ImageDetail::High => HIGH_MAX_DIMENSION,
        ImageDetail::Original => ORIGINAL_MAX_DIMENSION,
    };

    encode_within_budget(&decoded, format, bytes, max_dimension, budget)
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

    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(ToolsError::ImageTooLarge {
            size: metadata.len(),
            max: MAX_IMAGE_BYTES,
        });
    }

    let bytes = std::fs::read(path).map_err(ToolsError::Io)?;

    prepare_image_bytes(&bytes, detail, budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepares_a_png_from_bytes() {
        let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8, y as u8, 0]));
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        match prepare_image_bytes(&buf, ImageDetail::High, DEFAULT_BASE64_BUDGET).unwrap() {
            PreparedImage::Ready {
                media_type,
                data,
                width,
                height,
                ..
            } => {
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
