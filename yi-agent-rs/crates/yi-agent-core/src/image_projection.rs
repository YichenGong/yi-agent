//! 请求体图片投影：把超限的历史图片按无损失→有损失的顺序降级。
//!
//! 为什么需要它：网关按**字节**拒绝请求（413 `upstream_error`），而 auto-compact
//! 按 **token** 触发（`compact_threshold`）。长会话里内联图片的 base64 可达几十
//! MB，token 估算却按每张 1844 计（`compact::IMAGE_TOKEN_ESTIMATE`），两个口径
//! 相差约三个数量级，于是压缩永不触发、请求直接超限。
//!
//! 本模块只在**发送副本**上做投影：`Session` 持有的历史与落盘的 `.jsonl` 逐字
//! 不变。桌面端按 `path` 走 `image/read` 读回原图渲染，所以缩小或替换模型侧的
//! base64 对界面零影响。

use crate::message::{ContentBlock, ImageSource, Message};

/// 投影阈值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionLimits {
    /// 超过它就开始去重（无损）。
    pub soft_limit: usize,
    /// 超过它才允许有损降级（缩放、占位）。
    pub hard_limit: usize,
    /// 保留原始分辨率的最近图片张数（按不同 `path` 计）。
    pub keep_full_images: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            soft_limit: 2 * 1024 * 1024,
            hard_limit: 8 * 1024 * 1024,
            keep_full_images: 3,
        }
    }
}

/// 一次投影实际执行到哪一级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionStep {
    /// 未超软阈值，原样发送。
    AsIs,
    /// 只做了同 `path` 去重。
    Deduped,
    /// 去重后仍超硬阈值，把更旧的图缩小。
    Downscaled,
    /// 缩小后仍超硬阈值（或无法缩小），把更旧的图换成占位文字。
    Placeheld,
}

/// 把一张内联图片缩小到指定长边。
///
/// 实现放在能依赖图像编解码库的 crate（`yi-agent-tools`）里；`core` 只声明
/// 接口，未注入时第 2 级自动退化为第 3 级（占位）。
pub trait ImageDownscaler: Send + Sync {
    /// 返回 `(新 base64, 新 media_type)`；`None` 表示这张图无法缩放。
    fn downscale(&self, data: &str, media_type: &str, max_edge: u32) -> Option<(String, String)>;
}

impl std::fmt::Debug for dyn ImageDownscaler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 具体实现可能在 tools 侧、且持有缓存，打印类型名意义不大。
        f.write_str("<dyn ImageDownscaler>")
    }
}

/// 第 2 级缩放的目标长边。
pub const DOWNSCALE_MAX_EDGE: u32 = 1024;

/// 投影判定结果。`messages` 是可直接拿去序列化的副本。
#[derive(Debug, Clone)]
pub struct Projection {
    pub messages: Vec<Message>,
    pub step: ProjectionStep,
    /// 投影前的序列化字节数。
    pub bytes_before: usize,
    /// 投影后的序列化字节数。
    pub bytes_after: usize,
    /// 被替换为占位的图片数。
    pub placeheld: usize,
    /// 被要求缩小（但未在此处执行）的图片数。
    pub downscaled: usize,
}

/// 一处内联图片在消息树中的位置。
#[derive(Debug, Clone, Copy)]
struct ImageOccurrence {
    message: usize,
    /// 顶层 `content` 里的下标。
    block: usize,
    /// `ToolResult` 内层下标；顶层图片为 `None`。
    inner: Option<usize>,
    payload_len: usize,
}

fn collect_images(messages: &[Message]) -> Vec<ImageOccurrence> {
    let mut found = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        for (block_index, block) in message.content.iter().enumerate() {
            match block {
                ContentBlock::Image {
                    source: ImageSource::Base64 { data, .. },
                    ..
                } => found.push(ImageOccurrence {
                    message: message_index,
                    block: block_index,
                    inner: None,
                    payload_len: data.len(),
                }),
                ContentBlock::ToolResult { content, .. } => {
                    for (inner_index, inner) in content.iter().enumerate() {
                        if let ContentBlock::Image {
                            source: ImageSource::Base64 { data, .. },
                            ..
                        } = inner
                        {
                            found.push(ImageOccurrence {
                                message: message_index,
                                block: block_index,
                                inner: Some(inner_index),
                                payload_len: data.len(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    }
    found
}

impl Message {
    /// 借用某一处图片块（按 `ImageOccurrence` 定位）。
    fn image_at(&self, occurrence: &ImageOccurrence) -> Option<&ContentBlock> {
        let outer = self.content.get(occurrence.block)?;
        match occurrence.inner {
            None => Some(outer),
            Some(inner) => match outer {
                ContentBlock::ToolResult { content, .. } => content.get(inner),
                _ => None,
            },
        }
    }
}

/// 把某一处图片替换成占位文字块，返回被释放的字节数。
fn placeholder_at(
    messages: &mut [Message],
    occurrence: &ImageOccurrence,
    label: &str,
) -> Option<usize> {
    let replacement_len = label.len() + 16; // JSON 引号与字段开销的保守估计
    let message = messages.get_mut(occurrence.message)?;
    let outer = message.content.get_mut(occurrence.block)?;
    let slot = match occurrence.inner {
        None => outer,
        Some(inner) => match outer {
            ContentBlock::ToolResult { content, .. } => content.get_mut(inner)?,
            _ => return None,
        },
    };
    *slot = ContentBlock::Text(label.to_string());
    Some(occurrence.payload_len.saturating_sub(replacement_len))
}

/// 把历史投影成一份更小的请求副本。
///
/// 顺序严格按「无损失 → 有损失」：
///
/// 1. 未超 `soft_limit`：一个字节都不动（`AsIs`）。
/// 2. 超软阈值：同 `path` 只保留**最新**一份完整图，更早的换成占位
///    （`Deduped`）。模型看过同一张图，重复副本没有任何信息增益。
/// 3. 仍超 `hard_limit`：最近 `keep_full_images` 张之外的图换成占位
///    （`Placeheld`）。没有 `path` 的图**永不替换**——模型无法按需重取，
///    换掉就是永久丢失，宁可让它超限交上层决定。
///
/// 传入的 `messages` 不会被修改。
pub fn project(messages: &[Message], limits: &ProjectionLimits) -> Projection {
    project_with(messages, limits, None)
}

/// 带缩放实现的投影。`downscaler` 为 `None` 时跳过第 2 级直接走占位。
pub fn project_with(
    messages: &[Message],
    limits: &ProjectionLimits,
    downscaler: Option<&dyn ImageDownscaler>,
) -> Projection {
    let mut projected = messages.to_vec();
    let bytes_before = serde_json::to_vec(&projected).map(|v| v.len()).unwrap_or(0);

    let mut placeheld = 0usize;
    let mut freed = 0usize;

    // 级别 1：同 path 去重，保留最新一份。
    if bytes_before > limits.soft_limit {
        let mut seen = std::collections::HashSet::new();
        let occurrences = collect_images(&projected);
        for occurrence in occurrences.iter().rev() {
            let Some(path) = image_path_at(&projected, occurrence) else {
                continue;
            };
            if seen.insert(path.clone()) {
                continue; // 最新一份，保留
            }
            if let Some(bytes) = placeholder_at(&mut projected, occurrence, &placeholder(&path)) {
                freed += bytes;
                placeheld += 1;
            }
        }
    }

    let step = if bytes_before <= limits.soft_limit {
        ProjectionStep::AsIs
    } else {
        ProjectionStep::Deduped
    };

    // 级别 2/3：仍超硬阈值时，从**最旧**的图开始逐张降级，每降一张就检查
    // 是否已回到硬阈值内，收敛即停 —— 降级是有损的，能少降一张就少降一张。
    //
    // 先缩小、缩不动才占位（占位等于让模型彻底看不见这张图）。`keep_full_images`
    // 只用于**第一轮**：最近 N 张不同 path 的图优先保住原分辨率，但若缩小其余
    // 图片仍不收敛，这些「受保护」的图也按同一顺序降级——否则在「每张图都
    // 3.9 MB、硬阈值 8 MB」的形状下，保住 3 张原图就已 11.7 MB，阶梯不收敛，
    // 413 照旧发生。
    let mut step = step;
    let mut downscaled = 0usize;
    let mut remaining = bytes_before.saturating_sub(freed);
    if remaining > limits.hard_limit {
        let mut protected: std::collections::HashSet<String> = std::collections::HashSet::new();
        // 从最新往旧数出受保护的 path（第一轮跳过它们）。
        for occurrence in collect_images(&projected).iter().rev() {
            if protected.len() >= limits.keep_full_images {
                break;
            }
            if let Some(path) = image_path_at(&projected, occurrence) {
                protected.insert(path);
            }
        }

        // 两轮：第一轮跳过受保护的原图，第二轮连它们一起降级。
        for allow_protected in [false, true] {
            if remaining <= limits.hard_limit {
                break;
            }
            for occurrence in collect_images(&projected) {
                if remaining <= limits.hard_limit {
                    break;
                }
                if !allow_protected
                    && let Some(path) = image_path_at(&projected, &occurrence)
                    && protected.contains(&path)
                {
                    continue;
                }
                if let Some(downscaler) = downscaler
                    && let Some(saved) = downscale_at(&mut projected, &occurrence, downscaler)
                {
                    remaining = remaining.saturating_sub(saved);
                    downscaled += 1;
                    step = ProjectionStep::Downscaled;
                    continue;
                }
                // 缩不了（无实现 / 解码失败 / 缩了不省字节）才占位。
                if let Some(path) = image_path_at(&projected, &occurrence)
                    && let Some(bytes) =
                        placeholder_at(&mut projected, &occurrence, &placeholder(&path))
                {
                    remaining = remaining.saturating_sub(bytes);
                    placeheld += 1;
                    step = ProjectionStep::Placeheld;
                }
            }
        }
    }

    let bytes_after = remaining;
    Projection {
        messages: projected,
        step,
        bytes_before,
        bytes_after,
        placeheld,
        downscaled,
    }
}
/// 就地缩小一处图片块，返回释放的字节数。
fn downscale_at(
    messages: &mut [Message],
    occurrence: &ImageOccurrence,
    downscaler: &dyn ImageDownscaler,
) -> Option<usize> {
    let message = messages.get_mut(occurrence.message)?;
    let outer = message.content.get_mut(occurrence.block)?;
    let slot = match occurrence.inner {
        None => outer,
        Some(inner) => match outer {
            ContentBlock::ToolResult { content, .. } => content.get_mut(inner)?,
            _ => return None,
        },
    };
    let ContentBlock::Image {
        source: ImageSource::Base64 { media_type, data },
        ..
    } = slot
    else {
        return None;
    };
    let before = data.len();
    let (new_data, new_media_type) = downscaler.downscale(data, media_type, DOWNSCALE_MAX_EDGE)?;
    if new_data.len() >= before {
        return None; // 缩小反而没省字节，不值得降质
    }
    let saved = before - new_data.len();
    *data = new_data;
    *media_type = new_media_type;
    Some(saved)
}

fn placeholder(path: &str) -> String {
    format!("[图片已省略: {path}]")
}

fn image_path_at(messages: &[Message], occurrence: &ImageOccurrence) -> Option<String> {
    match messages.get(occurrence.message)?.image_at(occurrence)? {
        ContentBlock::Image { path, .. } => path.clone(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(path: &str, payload_bytes: usize) -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/jpeg".into(),
                data: "A".repeat(payload_bytes),
            },
            detail: Default::default(),
            path: Some(path.to_string()),
        }
    }

    /// 一个 tool 往返：assistant 发 tool_use，紧随其后的是 tool_result。
    fn tool_round_trip(id: &str, path: &str, payload_bytes: usize) -> Vec<Message> {
        vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "view_image".into(),
                input: serde_json::json!({ "path": path }),
            }]),
            Message {
                role: crate::message::Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content: vec![
                        ContentBlock::Text(format!("viewed {path}")),
                        image(path, payload_bytes),
                    ],
                    is_error: false,
                }],
            },
        ]
    }

    #[test]
    fn under_soft_limit_returns_as_is() {
        let mut messages = vec![Message::user("hello")];
        messages.extend(tool_round_trip("t1", "a.png", 1_000));

        let projection = project(&messages, &ProjectionLimits::default());

        assert_eq!(projection.step, ProjectionStep::AsIs);
        assert_eq!(projection.placeheld, 0);
        assert_eq!(projection.downscaled, 0);
        assert_eq!(
            serde_json::to_vec(&projection.messages).unwrap(),
            serde_json::to_vec(&messages).unwrap(),
            "under the soft limit not one byte may change"
        );
    }

    #[test]
    fn dedup_keeps_newest_copy_per_path() {
        let limits = ProjectionLimits {
            soft_limit: 100_000,
            hard_limit: 100_000,
            keep_full_images: 10,
        };

        let mut messages = vec![Message::user("look")];
        for index in 0..5 {
            messages.extend(tool_round_trip(&format!("t{index}"), "same.png", 60_000));
        }

        let projection = project(&messages, &limits);

        assert_eq!(projection.step, ProjectionStep::Deduped);
        assert_eq!(projection.placeheld, 4, "four older copies replaced");
        assert_eq!(projection.downscaled, 0, "dedup alone sufficed");
        assert!(
            projection.bytes_after < projection.bytes_before,
            "dedup must shrink the body"
        );
    }

    #[test]
    fn dedup_is_lossless_when_soft_threshold_is_the_only_trigger() {
        // 软阈值只触发去重：只要去重后已低于硬阈值，就绝不缩图。
        let limits = ProjectionLimits {
            soft_limit: 50_000,
            hard_limit: 10 * 1024 * 1024,
            keep_full_images: 3,
        };
        let mut messages = vec![Message::user("look")];
        for index in 0..4 {
            messages.extend(tool_round_trip(&format!("t{index}"), "same.png", 60_000));
        }

        let projection = project(&messages, &limits);

        assert_eq!(projection.step, ProjectionStep::Deduped);
        assert_eq!(projection.downscaled, 0, "no lossy step without need");
        assert_eq!(projection.placeheld, 3);
    }

    #[test]
    fn keeps_newest_three_images_at_full_resolution() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 100_000,
            keep_full_images: 3,
        };
        let mut messages = vec![Message::user("look")];
        for index in 0..6 {
            messages.extend(tool_round_trip(
                &format!("t{index}"),
                &format!("img{index}.png"),
                30_000,
            ));
        }

        let projection = project(&messages, &limits);

        // 6 张不同图，保留最近 3 张，另外 3 张需要降级。
        assert_eq!(projection.downscaled + projection.placeheld, 3);
        assert!(
            projection.bytes_after <= limits.hard_limit,
            "body ({}) must fit the hard limit ({})",
            projection.bytes_after,
            limits.hard_limit
        );
    }

    #[test]
    fn images_without_path_are_counted_but_never_placeheld() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 1,
            keep_full_images: 0,
        };
        let mut image_block = image("ignored.png", 50_000);
        if let ContentBlock::Image { path, .. } = &mut image_block {
            *path = None;
        }
        let messages = vec![
            Message::user("see"),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: vec![image_block],
                is_error: false,
            }]),
        ];

        let projection = project(&messages, &limits);

        // 无 path 的图片无法让模型按需重取，替换成占位就是永久丢失；
        // 因此宁可超限也不 drop 它。
        assert_eq!(projection.placeheld, 0);
        assert!(
            matches!(
                projection.messages[1].content[0],
                ContentBlock::ToolResult { .. }
            ),
            "message shape preserved"
        );
    }

    #[test]
    fn plan_matches_measured_failure_shape() {
        // 复刻真实失败轮：6 个不同 path、25 个图片块、合计 69 MB。
        // 去重后约 14.5 MB，仍超 8 MB 硬阈值 → 必须继续降级。
        let limits = ProjectionLimits::default();
        let shapes = [
            ("furnished_plan.png", 3_910_000usize, 9usize),
            ("detail_baths.png", 1_960_000, 6),
            ("detail_entry.png", 1_920_000, 5),
            ("furnished_axon.png", 2_900_000, 2),
            ("detail_master.png", 2_130_000, 2),
            ("view_entry.png", 1_740_000, 1),
        ];
        let mut messages = vec![Message::user("复刻户型图")];
        let mut id = 0usize;
        for (path, payload, copies) in shapes {
            for _ in 0..copies {
                messages.extend(tool_round_trip(&format!("t{id}"), path, payload));
                id += 1;
            }
        }

        let projection = project(&messages, &limits);

        assert!(
            projection.bytes_before > 60_000_000,
            "fixture must reproduce the ~69 MB body, got {}",
            projection.bytes_before
        );
        assert!(
            projection.bytes_after <= limits.hard_limit,
            "projected body ({}) must fit the hard limit ({})",
            projection.bytes_after,
            limits.hard_limit
        );
        assert!(projection.placeheld > 0, "19 duplicate copies must go");
        assert_eq!(projection.step, ProjectionStep::Placeheld);
    }

    #[test]
    fn projection_never_mutates_the_source_messages() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 10,
            keep_full_images: 0,
        };
        let mut messages = vec![Message::user("look")];
        for index in 0..3 {
            messages.extend(tool_round_trip(&format!("t{index}"), "same.png", 40_000));
        }
        let before = serde_json::to_vec(&messages).unwrap();

        let _ = project(&messages, &limits);

        assert_eq!(
            before,
            serde_json::to_vec(&messages).unwrap(),
            "the on-disk authority must stay byte-identical"
        );
    }

    /// 假降级器：把 payload 截到 1/100，用来验证「缩得动就不占位」。
    struct HalvingDownscaler;

    impl ImageDownscaler for HalvingDownscaler {
        fn downscale(
            &self,
            data: &str,
            media_type: &str,
            max_edge: u32,
        ) -> Option<(String, String)> {
            assert_eq!(max_edge, DOWNSCALE_MAX_EDGE, "level 2 must use 1024");
            Some((data[..data.len() / 100].to_string(), media_type.to_string()))
        }
    }

    /// 缩不动的降级器：用来验证「缩不动就退到占位」。
    struct UnsupportedDownscaler;

    impl ImageDownscaler for UnsupportedDownscaler {
        fn downscale(&self, _: &str, _: &str, _: u32) -> Option<(String, String)> {
            None
        }
    }

    #[test]
    fn downscales_instead_of_placeholder_when_an_implementation_is_injected() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 100_000,
            keep_full_images: 0,
        };
        let mut messages = vec![Message::user("look")];
        messages.extend(tool_round_trip("t1", "a.png", 300_000));

        let projection = project_with(&messages, &limits, Some(&HalvingDownscaler));

        assert_eq!(projection.step, ProjectionStep::Downscaled);
        assert_eq!(
            projection.downscaled, 1,
            "the image must be shrunk, not dropped"
        );
        assert_eq!(projection.placeheld, 0);
        assert!(
            projection.bytes_after <= limits.hard_limit,
            "body {} must fit {}",
            projection.bytes_after,
            limits.hard_limit
        );
    }

    #[test]
    fn downscaler_absent_falls_back_to_placeholder() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 1000,
            keep_full_images: 0,
        };
        let mut messages = vec![Message::user("look")];
        messages.extend(tool_round_trip("t1", "a.png", 300_000));

        let projection = project_with(&messages, &limits, None);

        assert_eq!(projection.step, ProjectionStep::Placeheld);
        assert_eq!(projection.placeheld, 1);
        assert_eq!(projection.downscaled, 0);
    }

    #[test]
    fn unsupported_downscaler_falls_back_to_placeholder() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 1000,
            keep_full_images: 0,
        };
        let mut messages = vec![Message::user("look")];
        messages.extend(tool_round_trip("t1", "a.png", 300_000));

        let projection = project_with(&messages, &limits, Some(&UnsupportedDownscaler));

        assert_eq!(projection.step, ProjectionStep::Placeheld);
        assert_eq!(projection.placeheld, 1, "an unscalable image is placeheld");
    }

    /// 缩后每张图都变成 1/100。
    fn payload_sizes(projection: &Projection) -> Vec<(Option<String>, usize)> {
        projection
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .flat_map(|c| c.iter())
            .filter_map(|b| match b {
                ContentBlock::Image {
                    source: ImageSource::Base64 { data, .. },
                    path,
                    ..
                } => Some((path.clone(), data.len())),
                _ => None,
            })
            .collect()
    }

    /// 第一轮只降旧图；若这样就能收敛，最新 N 张必须逐字保留原分辨率。
    #[test]
    fn newest_images_keep_full_resolution_while_it_suffices() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 320_000, // 缩掉一张（省 297k）即可收敛
            keep_full_images: 1,
        };
        let mut messages = vec![Message::user("look")];
        messages.extend(tool_round_trip("t1", "old.png", 300_000));
        messages.extend(tool_round_trip("t2", "new.png", 300_000));

        let projection = project_with(&messages, &limits, Some(&HalvingDownscaler));

        let sizes = payload_sizes(&projection);
        let newest = sizes
            .iter()
            .find(|(path, _)| path.as_deref() == Some("new.png"))
            .expect("newest image present");
        assert_eq!(newest.1, 300_000, "newest image stays full size");
        assert!(
            projection.bytes_after <= limits.hard_limit,
            "body {} must converge to {}",
            projection.bytes_after,
            limits.hard_limit
        );
    }

    /// 保护最近 N 张是**优先**而非**绝对**：只降旧图不收敛时，受保护的图也要降，
    /// 否则阶梯停在一个仍超硬阈值的请求体上，413 照旧发生。
    ///
    /// 这正对应实测故障形状：每张 3.9 MB、硬阈值 8 MB，保住 3 张原图就是
    /// 11.7 MB —— 不放弃保护就永远收敛不了。
    #[test]
    fn protection_yields_when_downscaling_the_rest_does_not_converge() {
        let limits = ProjectionLimits {
            soft_limit: 10,
            hard_limit: 100_000,
            keep_full_images: 1,
        };
        let mut messages = vec![Message::user("look")];
        messages.extend(tool_round_trip("t1", "old.png", 300_000));
        messages.extend(tool_round_trip("t2", "new.png", 300_000));

        let projection = project_with(&messages, &limits, Some(&HalvingDownscaler));

        let sizes = payload_sizes(&projection);
        let newest = sizes
            .iter()
            .find(|(path, _)| path.as_deref() == Some("new.png"))
            .expect("newest image present");
        assert_eq!(
            newest.1, 3_000,
            "the newest image must also be downscaled to converge"
        );
        assert!(
            projection.bytes_after <= limits.hard_limit,
            "body {} must converge to {}",
            projection.bytes_after,
            limits.hard_limit
        );
    }
}
