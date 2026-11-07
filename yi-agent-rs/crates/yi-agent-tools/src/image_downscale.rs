//! 请求体降级用的图片缩放器。
//!
//! `yi-agent-core` 不依赖 `image`（见 `yi-agent-core/Cargo.toml`），因此缩放
//! 实现放在这里，经 `AgentConfig::image_downscaler` 注入。职责只有一个：把一张
//! 内联 base64 图片缩到指定长边，供请求体降级的第 2 级使用。
//!
//! 归档不受影响：缩放在**请求副本**上执行，落盘的 `.jsonl` 与 `Session` 逐字
//! 保留原图（`image_projection` 只改副本）。

use base64::Engine;
use std::collections::HashMap;
use std::sync::Mutex;
use yi_agent_core::image_projection::ImageDownscaler;

/// 缩放结果的媒体类型：统一用 JPEG（无损格式在大尺寸下省不了字节）。
const OUTPUT_MEDIA_TYPE: &str = "image/jpeg";
/// JPEG 质量：1024 长边下 q80 肉眼几乎无差，实测户型图标注仍清晰。
const OUTPUT_QUALITY: u8 = 80;

/// 缓存键：`(data 长度, 前 64 字符哈希, 目标长边)`；值是 `(新 base64, 媒体类型)`，
/// `None` 表示这张图缩不了（结果同样缓存，避免每轮重复解码失败）。
type CacheKey = (usize, u64, u32);
type CacheValue = Option<(String, String)>;

/// 带记忆化的缩放器。
///
/// 3 MB PNG 解码+缩放约 100~300 ms，历史里几十张图会让每轮请求都慢几秒。
/// 以 `(data 长度, 前 64 字节, 长边)` 为键缓存：`data` 长度与头部已足以区分
/// 不同的图，且不必对整段 base64 做哈希。
#[derive(Default)]
pub struct ImagePrepDownscaler {
    cache: Mutex<HashMap<CacheKey, CacheValue>>,
}

impl ImagePrepDownscaler {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ImageDownscaler for ImagePrepDownscaler {
    fn downscale(&self, data: &str, _media_type: &str, max_edge: u32) -> Option<(String, String)> {
        let key = (data.len(), head_hash(data), max_edge);
        if let Ok(cache) = self.cache.lock()
            && let Some(hit) = cache.get(&key)
        {
            return hit.clone();
        }

        let result = downscale_uncached(data, max_edge);

        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, result.clone());
        }
        result
    }
}

/// 取 base64 前 64 个字符的哈希：不同的图几乎必然不同，同图必然相同。
fn head_hash(data: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let head = &data[..data.len().min(64)];
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    head.hash(&mut hasher);
    hasher.finish()
}

/// 真正的解码 / 缩放 / 编码。返回 `None` 表示「这张图缩不了或缩了不省字节」，
/// 由调用方退到占位（第 3 级）。
fn downscale_uncached(data: &str, max_edge: u32) -> Option<(String, String)> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let decoded = image::load_from_memory(&raw).ok()?;
    let scaled = crate::image_prep::resize_to_cap(&decoded, max_edge);
    let bytes = crate::image_prep::jpeg_encode(scaled.as_ref(), OUTPUT_QUALITY).ok()?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    // 只有真的更小才算成功：重编码小图往往变大，那种情况不算「降级成功」。
    if encoded.len() >= data.len() {
        return None;
    }
    Some((encoded, OUTPUT_MEDIA_TYPE.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};

    fn png_base64(width: u32, height: u32) -> String {
        let img = RgbImage::from_fn(width, height, |x, y| {
            Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        });
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    #[test]
    fn downscales_a_large_image_within_the_edge_cap() {
        let source = png_base64(3000, 3000);
        let downscaler = ImagePrepDownscaler::new();

        let (data, media_type) = downscaler
            .downscale(&source, "image/png", 1024)
            .expect("must shrink");

        assert_eq!(media_type, OUTPUT_MEDIA_TYPE);
        assert!(
            data.len() < source.len(),
            "downscaled {} must be smaller than {}",
            data.len(),
            source.len()
        );

        // 解码回来确认长边没超过上限。
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .unwrap();
        let decoded = image::load_from_memory(&raw).unwrap();
        assert!(decoded.width().max(decoded.height()) <= 1024);
    }

    #[test]
    fn repeated_calls_hit_the_cache() {
        let source = png_base64(2000, 2000);
        let downscaler = ImagePrepDownscaler::new();

        let first = downscaler.downscale(&source, "image/png", 1024).unwrap();
        let second = downscaler.downscale(&source, "image/png", 1024).unwrap();

        assert_eq!(first, second, "same input must yield the same output");
        assert_eq!(downscaler.cache.lock().unwrap().len(), 1, "one entry");
    }

    #[test]
    fn a_small_image_is_not_worth_downscaling() {
        // 8x8 PNG 已远小于 1024 长边，重编码只会更大 → 返回 None。
        let source = png_base64(8, 8);
        let downscaler = ImagePrepDownscaler::new();
        assert!(downscaler.downscale(&source, "image/png", 1024).is_none());
    }

    #[test]
    fn invalid_base64_is_reported_as_unsupported() {
        let downscaler = ImagePrepDownscaler::new();
        assert!(
            downscaler
                .downscale("not base64!!", "image/png", 1024)
                .is_none()
        );
    }
}
