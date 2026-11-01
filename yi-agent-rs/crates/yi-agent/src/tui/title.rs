//! 终端标签页标题（OSC 2）。
//!
//! 与 `notify/` 平级而非其子模块：通知是离散事件（攒候选、去抖、丢弃
//! 无所谓），标题是持续状态（有当前值、需 diff、退出必须还原）。两者没有
//! 共享状态，只复用 `sanitize` 与 `is_multiplexed` 两个纯工具。

// 本模块的 API 要到接线任务（`run_loop` 每帧 sync、退出 clear）才有生产
// 调用者；在那之前只有测试在用。分阶段提交时不该因此让 `clippy -D warnings`
// 变红，故按仓内既有做法（`tui/queued.rs:105`、`tui/slash.rs:153`）在
// 非测试构建下放开 `dead_code`。接线完成后删掉这行。
#![cfg_attr(not(test), allow(dead_code))]

use std::path::Path;

/// 标题的显示列宽上限。
///
/// 终端标签栏普遍 15-25 列，24 是「尽量多显示又不挤掉两侧标签」的折中；
/// 再长终端自己也会截断，而终端的截断不受我们控制（可能切在 CJK 中间）。
pub const MAX_COLUMNS: usize = 24;

/// 取不到项目名时的回退值。
pub const FALLBACK_PROJECT: &str = "yi-agent";

/// 运行态前缀。`●` 是 U+25CF，实测 `UnicodeWidthChar::width` 返回 `Some(1)`，
/// 加一个空格共 2 列。
const RUNNING_PREFIX: &str = "● ";

/// 从工作目录取项目名：最后一级目录名。
///
/// 判据与 `app-server/src/lead.rs` 的 `lead_title_for_cwd` 一致
/// （`file_name() → to_str() → 非空` 三步），但产物不同：那边拼
/// 「`{name} 组长`」、回退「组长」，这里不带后缀、回退 [`FALLBACK_PROJECT`]。
pub fn project_label(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(FALLBACK_PROJECT)
        .to_string()
}

/// 组装标题：运行态前缀 + 项目名，经消毒后按显示列宽截断到 [`MAX_COLUMNS`]。
///
/// 消毒在截断之前：项目目录名可以含 ESC，一个裸 ESC 就能截断 OSC 序列并
/// 注入终端指令。这是安全边界，不是格式美化。
pub fn compose(project: &str, running: bool) -> String {
    let raw = if running {
        format!("{RUNNING_PREFIX}{project}")
    } else {
        project.to_string()
    };
    let cleaned = crate::tui::notify::text::sanitize(&raw);
    crate::tui::wrap::truncate_to_width(&cleaned, MAX_COLUMNS)
}

#[cfg(test)]
mod tests {
    use super::*;

    use unicode_width::UnicodeWidthStr;

    #[test]
    fn compose_idle_is_just_the_project() {
        assert_eq!(compose("myproject", false), "myproject");
    }

    #[test]
    fn compose_running_adds_dot_prefix() {
        assert_eq!(compose("myproject", true), "● myproject");
    }

    #[test]
    fn compose_truncates_to_24_columns() {
        let long = "a".repeat(40);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
        assert!(out.ends_with('…'), "should end with an ellipsis: {out:?}");
    }

    #[test]
    fn compose_measures_cjk_by_display_width() {
        // 24 个汉字 = 48 列。按 `chars().count()` 截断会得出「24 个字符」
        // 但实际 48 列，标签栏错位。
        let long = "中".repeat(24);
        let out = compose(&long, false);
        let width = UnicodeWidthStr::width(out.as_str());
        assert!(width <= MAX_COLUMNS, "{out:?} is {width} cols");
    }

    #[test]
    fn compose_counts_the_dot_prefix_against_the_budget() {
        // 前缀占 2 列，所以运行态下留给项目名的预算少 2 列。用『留下的 a
        // 的个数』断言，而不是断言两边总宽度相等——后者恒真，测不出东西。
        let long = "a".repeat(40);
        let idle = compose(&long, false);
        let running = compose(&long, true);
        assert!(running.starts_with("● "), "{running:?}");
        let idle_a = idle.matches('a').count();
        let running_a = running.matches('a').count();
        assert_eq!(
            idle_a,
            MAX_COLUMNS - 1,
            "idle should keep {} a's plus the ellipsis: {idle:?}",
            MAX_COLUMNS - 1
        );
        assert_eq!(
            running_a,
            MAX_COLUMNS - 1 - 2,
            "the 2-column prefix must cost exactly 2 a's: {running:?}"
        );
    }

    #[test]
    fn compose_strips_control_characters_from_project() {
        // 项目目录名可以含控制字符。判定用断言而非肉眼：把一个真实会被
        // 终端解释的 OSC 2 序列塞进项目名。
        let out = compose("evil\u{1b}]2;pwned\u{7}", false);
        assert_eq!(out.matches('\u{1b}').count(), 0, "ESC: {out:?}");
        assert_eq!(out.matches('\u{7}').count(), 0, "BEL: {out:?}");
    }

    #[test]
    fn compose_never_exceeds_budget_for_any_shape() {
        let twenty_three = "a".repeat(23);
        let twenty_four = "a".repeat(24);
        let twenty_five = "a".repeat(25);
        let cjk_twelve = "中".repeat(12);
        let cjk_thirteen = "中".repeat(13);
        let mixed = "a中".repeat(20);
        let shapes = [
            "",
            "a",
            twenty_three.as_str(),
            twenty_four.as_str(),
            twenty_five.as_str(),
            cjk_twelve.as_str(),
            cjk_thirteen.as_str(),
            mixed.as_str(),
        ];
        for shape in shapes {
            for running in [false, true] {
                let out = compose(shape, running);
                let width = UnicodeWidthStr::width(out.as_str());
                assert!(
                    width <= MAX_COLUMNS,
                    "shape {shape:?} running={running} -> {out:?} at {width} cols"
                );
            }
        }
    }

    #[test]
    fn project_label_uses_last_path_segment() {
        assert_eq!(project_label(Path::new("/work/my-app")), "my-app");
    }

    #[test]
    fn project_label_falls_back_for_root() {
        assert_eq!(project_label(Path::new("/")), FALLBACK_PROJECT);
    }

    #[test]
    fn project_label_falls_back_for_empty_name() {
        assert_eq!(project_label(Path::new("")), FALLBACK_PROJECT);
    }
}
