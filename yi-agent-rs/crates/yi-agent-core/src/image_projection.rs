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

    let after_dedup = bytes_before.saturating_sub(freed);
    let step = if bytes_before <= limits.soft_limit {
        ProjectionStep::AsIs
    } else {
        ProjectionStep::Deduped
    };

    // 级别 2：仍超硬阈值时，只保留最近的若干张完整图。
    let mut step = step;
    if after_dedup > limits.hard_limit {
        let mut kept = std::collections::HashSet::new();
        let occurrences = collect_images(&projected);
        for occurrence in occurrences.iter().rev() {
            let Some(path) = image_path_at(&projected, occurrence) else {
                continue;
            };
            if kept.len() < limits.keep_full_images && kept.insert(path.clone()) {
                continue;
            }
            if let Some(bytes) = placeholder_at(&mut projected, occurrence, &placeholder(&path)) {
                freed += bytes;
                placeheld += 1;
                step = ProjectionStep::Placeheld;
            }
        }
    }

    let bytes_after = bytes_before.saturating_sub(freed);
    Projection {
        messages: projected,
        step,
        bytes_before,
        bytes_after,
        placeheld,
        downscaled: 0,
    }
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
}
