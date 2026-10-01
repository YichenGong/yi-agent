//! 由一对 spec/plan 路径派生卡片 id。
//!
//! 规则必须与宿主侧（TUI 与 app-server）**逐字一致**：同一对文件在两端必须
//! 派生出同一个 id，否则「桌面上看到的卡」与「CLI 入队的卡」会是两张。
//! 因此这里刻意抄写同一套 slug 规则，并由一致性测试锁住。

/// 由一对路径派生卡片 id：两个文件 stem 各自 slug 化后拼接。
pub fn card_id_for(spec: &str, plan: &str) -> String {
    let stem = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let slug = |text: &str| {
        let mut out = String::new();
        let mut last_dash = false;
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    };
    let id = format!("{}-{}", slug(&stem(spec)), slug(&stem(plan)));
    if id == "-" || id.is_empty() {
        "card".to_string()
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::card_id_for;

    #[test]
    fn derives_the_id_from_both_file_stems() {
        assert_eq!(
            card_id_for("docs/a.spec.md", "docs/a.plan.md"),
            "a-spec-a-plan"
        );
    }

    #[test]
    fn slugs_punctuation_and_case() {
        assert_eq!(card_id_for("My Spec.md", "My Plan.md"), "my-spec-my-plan");
    }

    #[test]
    fn stripes_directories_and_extension() {
        assert_eq!(
            card_id_for("/tmp/x/2026-10-01-foo.md", "/tmp/y/2026-10-01-foo-plan.md"),
            "2026-10-01-foo-2026-10-01-foo-plan"
        );
    }

    /// 与宿主共享的样例。`yi-agent-app-server` 的
    /// `derive_card_id_matches_the_tui_rule` 断言的是同一对输入与同一结果；
    /// 若哪天宿主规则变了而这里没跟上，这条会先炸。
    #[test]
    fn matches_the_host_rule_for_the_shared_sample() {
        assert_eq!(
            card_id_for("docs/a-feature.spec.md", "docs/a-feature.plan.md"),
            "a-feature-spec-a-feature-plan"
        );
    }

    #[test]
    fn falls_back_to_card_when_nothing_usable_remains() {
        assert_eq!(card_id_for("", ""), "card");
    }
}
