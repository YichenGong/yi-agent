//! Cumulative per-model token cost tracking for `/cost`.

use std::collections::BTreeMap;
use yi_agent_core::TokenUsage;

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

    pub fn record(&mut self, _model: &str, _usage: &TokenUsage) {
        todo!()
    }

    pub fn render(&self) -> String {
        todo!()
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
}
