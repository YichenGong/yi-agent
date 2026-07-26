//! Cumulative per-model token cost tracking for `/cost`.

use std::collections::BTreeMap;
use yi_agent_core::TokenUsage;

use super::statusbar::format_thousands;

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
            return "Token 用量统计:\n\n(尚无数据)".to_string();
        }
        // markdown 表格:标题(普通段落)+ 表格。对齐交给 render_markdown。
        let mut out = String::from("Token 用量统计:\n\n");
        out.push_str("| 模型 | input | output | cache_create | cache_read | calls |\n");
        out.push_str("| --- | ---: | ---: | ---: | ---: | ---: |\n");
        for (model, cost) in &self.per_model {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                model,
                format_thousands(cost.input),
                format_thousands(cost.output),
                format_thousands(cost.cache_creation),
                format_thousands(cost.cache_read),
                format_thousands(cost.calls),
            ));
        }
        let mut total = ModelCost::default();
        for c in self.per_model.values() {
            total.input += c.input;
            total.output += c.output;
            total.cache_creation += c.cache_creation;
            total.cache_read += c.cache_read;
            total.calls += c.calls;
        }
        out.push_str(&format!(
            "| **总计** | **{}** | **{}** | **{}** | **{}** | **{}** |\n",
            format_thousands(total.input),
            format_thousands(total.output),
            format_thousands(total.cache_creation),
            format_thousands(total.cache_read),
            format_thousands(total.calls),
        ));
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
    fn render_two_models_markdown_structure() {
        let mut t = CostTracker::new();
        t.record("alpha", &usage(100, 10));
        t.record("beta", &usage(2000, 200));
        let s = t.render();
        // 标题段落
        assert!(
            s.starts_with("Token 用量统计:\n\n"),
            "should start with title: {s}"
        );
        // 表头行
        assert!(s.contains("| 模型 | input |"), "should have header row: {s}");
        // 对齐行
        assert!(
            s.contains("| --- | ---: |"),
            "should have alignment row: {s}"
        );
        // 数据行(按字典序)
        let ai = s.find("alpha").unwrap();
        let bi = s.find("beta").unwrap();
        assert!(ai < bi, "alpha should come before beta: {s}");
        // 总计行(加粗)
        assert!(s.contains("**总计**"), "should have bold total row: {s}");
        assert!(s.contains("**2,100**"), "should have bold total input: {s}");
        assert!(s.contains("**210**"), "should have bold total output: {s}");
    }
}
