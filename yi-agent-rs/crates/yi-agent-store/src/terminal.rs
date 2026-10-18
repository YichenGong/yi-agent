//! 子任务终态 JSON 的公共解码。
//!
//! 终态形状由本 crate 的 `runtime` 模块写出（`text_completion_terminal_json` /
//! `budget_exhausted_terminal_json`），因此解码也归本模块所有，避免回收路径与
//! 读取路径各自再实现一遍而漂移。

/// 从终态 JSON 中取出 `text_completion` 形状的文本报告。
///
/// 返回 `None` 的四种情况：入参为 `None`、JSON 非法、`kind` 不是
/// `text_completion`、或 `report` 为空/纯空白。
///
/// 空白报告视为「无报告」：一份只有空白的报告对父级没有信息量，而 writer 侧
/// 用 `report.unwrap_or("")` 表示「没有内容」，两者语义一致。
pub fn text_completion_report(terminal_json: Option<&str>) -> Option<String> {
    let payload = serde_json::from_str::<serde_json::Value>(terminal_json?).ok()?;
    (payload.get("kind").and_then(serde_json::Value::as_str) == Some("text_completion"))
        .then(|| payload.get("report").and_then(serde_json::Value::as_str))
        .flatten()
        .filter(|report| !report.trim().is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_text_completion_report() {
        let json = r#"{"kind":"text_completion","report":"I got halfway through the refactor"}"#;
        assert_eq!(
            text_completion_report(Some(json)).as_deref(),
            Some("I got halfway through the refactor")
        );
    }

    #[test]
    fn extracts_a_report_from_a_budget_exhausted_payload() {
        let json =
            r#"{"reason":"turn_budget_exhausted","kind":"text_completion","report":"partial"}"#;
        assert_eq!(
            text_completion_report(Some(json)).as_deref(),
            Some("partial")
        );
    }

    #[test]
    fn returns_none_for_a_non_text_completion_kind() {
        let json = r#"{"kind":"delivery","report":"ignored"}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_for_an_empty_report() {
        let json = r#"{"kind":"text_completion","report":""}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_for_a_whitespace_only_report() {
        // 本设计刻意的行为：只有空白的报告视为「没有报告」。
        let json = r#"{"kind":"text_completion","report":"   "}"#;
        assert_eq!(text_completion_report(Some(json)), None);
    }

    #[test]
    fn returns_none_when_there_is_no_terminal_json() {
        assert_eq!(text_completion_report(None), None);
    }

    #[test]
    fn returns_none_for_invalid_json() {
        assert_eq!(text_completion_report(Some("not json")), None);
    }
}
