//! Cumulative per-model token cost tracking for `/cost`.

use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;
use yi_agent_core::TokenUsage;

use super::statusbar::format_thousands;

/// Pad `s` with leading spaces so its display width equals `width` (right-align).
fn pad_left(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{}", " ".repeat(width - w), s)
    }
}

/// Pad `s` with trailing spaces so its display width equals `width` (left-align).
fn pad_right(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - w))
    }
}

/// Per-model accumulated token counters.
#[derive(Debug, Clone, Default)]
pub struct ModelCost {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub calls: u64,
}

/// Cumulative token usage tracker, keyed by model name.
#[derive(Debug, Clone, Default)]
pub struct CostTracker {
    per_model: BTreeMap<String, ModelCost>,
}

impl CostTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, model: &str, usage: &TokenUsage) {
        let m = self.per_model.entry(model.to_string()).or_default();
        m.input += usage.input_tokens as u64;
        m.output += usage.output_tokens as u64;
        m.cache_creation += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
        m.cache_read += usage.cache_read_input_tokens.unwrap_or(0) as u64;
        m.calls += 1;
    }

    pub fn render(&self) -> String {
        if self.per_model.is_empty() {
            return "Token 用量统计:\n(尚无数据)".to_string();
        }

        let mut rows: Vec<[String; 6]> = Vec::new();
        for (model, cost) in &self.per_model {
            rows.push([
                model.clone(),
                format_thousands(cost.input),
                format_thousands(cost.output),
                format_thousands(cost.cache_creation),
                format_thousands(cost.cache_read),
                format_thousands(cost.calls),
            ]);
        }

        let mut total = ModelCost::default();
        for c in self.per_model.values() {
            total.input += c.input;
            total.output += c.output;
            total.cache_creation += c.cache_creation;
            total.cache_read += c.cache_read;
            total.calls += c.calls;
        }

        let header = [
            "模型".to_string(),
            "input".to_string(),
            "output".to_string(),
            "cache_create".to_string(),
            "cache_read".to_string(),
            "calls".to_string(),
        ];
        let total_row = [
            "总计".to_string(),
            format_thousands(total.input),
            format_thousands(total.output),
            format_thousands(total.cache_creation),
            format_thousands(total.cache_read),
            format_thousands(total.calls),
        ];

        // Compute column widths from header + all rows + total row.
        let mut widths = [0usize; 6];
        let all_rows: Vec<[String; 6]> = std::iter::once(header.clone())
            .chain(rows.iter().cloned())
            .chain(std::iter::once(total_row.clone()))
            .collect();
        for r in &all_rows {
            for (i, cell) in r.iter().enumerate() {
                widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }

        let mut out = String::from("Token 用量统计:\n");
        let mut first = true;
        for r in &all_rows {
            for (i, cell) in r.iter().enumerate() {
                if i == 0 {
                    out.push_str(&pad_right(cell, widths[i]));
                } else {
                    out.push_str(&format!("  {}", pad_left(cell, widths[i])));
                }
            }
            out.push('\n');
            if first {
                let sep: String =
                    std::iter::repeat_n('─', widths.iter().sum::<usize>() + 2 * 5).collect();
                out.push_str(&sep);
                out.push('\n');
            }
            first = false;
        }
        // Trailing newline already added by last row; trim if desired.
        if out.ends_with('\n') {
            out.pop();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u32, output: u32) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
        }
    }

    #[test]
    fn record_single_model_accumulates() {
        let mut t = CostTracker::new();
        t.record("claude", &usage(100, 50));
        t.record("claude", &usage(200, 30));
        let m = t.per_model.get("claude").unwrap();
        assert_eq!(m.input, 300);
        assert_eq!(m.output, 80);
    }

    #[test]
    fn record_multiple_models_separate() {
        let mut t = CostTracker::new();
        t.record("a", &usage(10, 1));
        t.record("b", &usage(20, 2));
        assert_eq!(t.per_model.len(), 2);
        assert_eq!(t.per_model.get("a").unwrap().input, 10);
        assert_eq!(t.per_model.get("b").unwrap().input, 20);
    }

    #[test]
    fn record_increments_calls() {
        let mut t = CostTracker::new();
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        assert_eq!(t.per_model.get("m").unwrap().calls, 3);
    }

    #[test]
    fn record_accumulates_cache_fields() {
        let mut t = CostTracker::new();
        let u1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_input_tokens: Some(20),
            cache_read_input_tokens: Some(10),
        };
        let u2 = TokenUsage {
            input_tokens: 200,
            output_tokens: 30,
            cache_creation_input_tokens: Some(5),
            cache_read_input_tokens: Some(40),
        };
        t.record("m", &u1);
        t.record("m", &u2);
        let m = t.per_model.get("m").unwrap();
        assert_eq!(m.cache_creation, 25);
        assert_eq!(m.cache_read, 50);
    }

    #[test]
    fn render_empty_shows_no_data() {
        let t = CostTracker::new();
        let s = t.render();
        assert!(s.contains("Token 用量统计"), "should have title: {s}");
        assert!(s.contains("尚无数据"), "empty should show no-data: {s}");
    }

    #[test]
    fn render_single_model_has_header_data_total() {
        let mut t = CostTracker::new();
        t.record("claude-sonnet-4-5", &usage(12345, 6789));
        let s = t.render();
        assert!(s.contains("input"), "should have header: {s}");
        assert!(s.contains("output"), "should have header: {s}");
        assert!(
            s.contains("claude-sonnet-4-5"),
            "should have model row: {s}"
        );
        assert!(
            s.contains("12,345"),
            "should format input with thousands: {s}"
        );
        assert!(
            s.contains("6,789"),
            "should format output with thousands: {s}"
        );
        assert!(s.contains("总计"), "should have total row: {s}");
    }

    #[test]
    fn render_multiple_models_sorted() {
        let mut t = CostTracker::new();
        t.record("zeta", &usage(1, 1));
        t.record("alpha", &usage(2, 2));
        t.record("mid", &usage(3, 3));
        let s = t.render();
        let ai = s.find("alpha").unwrap();
        let mi = s.find("mid").unwrap();
        let zi = s.find("zeta").unwrap();
        assert!(ai < mi && mi < zi, "models should be sorted alphabetically");
    }

    #[test]
    fn render_total_row_sums_all_models() {
        let mut t = CostTracker::new();
        t.record("a", &usage(100, 10));
        t.record("b", &usage(200, 20));
        let s = t.render();
        assert!(s.contains("300"), "total input should be 300: {s}");
        assert!(s.contains("30"), "total output should be 30: {s}");
    }

    #[test]
    fn render_shows_calls_column() {
        let mut t = CostTracker::new();
        t.record("m", &usage(1, 1));
        t.record("m", &usage(1, 1));
        let s = t.render();
        assert!(s.contains("calls"), "should have calls header: {s}");
        assert!(s.contains("2"), "should show call count 2: {s}");
    }

    #[test]
    fn render_two_models_exact_snapshot() {
        let mut t = CostTracker::new();
        t.record("alpha", &usage(100, 10));
        t.record("beta", &usage(2000, 200));
        let s = t.render();
        // Verify exact structure: title, header, separator, two model rows, total row = 6 lines.
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(
            lines.len(),
            6,
            "should have 6 lines (title, header, separator, 2 models, total): {s}"
        );
        assert_eq!(lines[0], "Token 用量统计:");
        let header_line = lines[1];
        assert!(
            header_line.contains("模型")
                && header_line.contains("input")
                && header_line.contains("calls"),
            "header line: {header_line}"
        );
        let sep_line = lines[2];
        assert!(
            sep_line.chars().all(|c| c == '─'),
            "separator line should be all box-drawing chars: {sep_line}"
        );
        let alpha_line = lines[3];
        assert!(
            alpha_line.contains("alpha") && alpha_line.contains("100"),
            "alpha row: {alpha_line}"
        );
        let beta_line = lines[4];
        assert!(
            beta_line.contains("beta") && beta_line.contains("2,000"),
            "beta row: {beta_line}"
        );
        // Total row: 2100 input, 210 output.
        let total_line = lines[5];
        assert!(
            total_line.contains("总计"),
            "total row should contain 总计: {total_line}"
        );
        assert!(
            total_line.contains("2,100"),
            "total input 2,100: {total_line}"
        );
        assert!(total_line.contains("210"), "total output 210: {total_line}");
    }
}
