//! Trace facts produced by a running subagent, plus the write-side aggregator
//! that folds a streaming transcript into them.
//!
//! This module is pure logic: it owns no IO and no clock of its own. Every
//! entry point takes the current [`Instant`], which keeps it trivially testable
//! and free of a timer dependency.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Buffered assistant text is released once it reaches this many bytes.
pub const TRACE_TEXT_FLUSH_BYTES: usize = 512;

/// Buffered assistant text is released once it has been pending this long.
pub const TRACE_TEXT_FLUSH_INTERVAL: Duration = Duration::from_millis(200);

/// Tool call and tool result summaries are truncated to at most this many bytes.
pub const TRACE_TOOL_SUMMARY_LIMIT: usize = 1024;

/// One fact derived from a running subagent's transcript.
///
/// Serialized with an internal `type` tag in snake case, for example
/// `{"type": "tool_call", "name": "bash", "summary": "cargo test"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceFact {
    /// A chunk of the assistant's visible text.
    AssistantText { text: String },
    /// The assistant asked for a tool to run.
    ToolCall { name: String, summary: String },
    /// A tool finished running.
    ToolResult {
        name: String,
        is_error: bool,
        summary: String,
    },
    /// A state transition worth surfacing on its own.
    StateNote { note: String },
}

/// Folds a streaming transcript into [`TraceFact`]s.
///
/// Assistant text is buffered and only released when it reaches
/// [`TRACE_TEXT_FLUSH_BYTES`], when [`TraceAggregator::due`] observes
/// [`TRACE_TEXT_FLUSH_INTERVAL`] elapse, or when a structure boundary (a tool
/// call, tool result, or state note) arrives. A structure boundary always
/// releases the buffer first so that fact ordering follows the transcript.
#[derive(Debug, Default)]
pub struct TraceAggregator {
    pending: String,
    pending_since: Option<Instant>,
}

impl TraceAggregator {
    /// Creates an aggregator with an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffers an assistant text fragment, releasing it once the byte
    /// threshold is met.
    pub fn push_text(&mut self, fragment: &str, now: Instant) -> Vec<TraceFact> {
        self.pending.push_str(fragment);
        self.pending_since.get_or_insert(now);
        if self.pending.len() >= TRACE_TEXT_FLUSH_BYTES {
            self.take_pending()
        } else {
            Vec::new()
        }
    }

    /// Records a tool call, releasing buffered text first.
    pub fn push_tool_call(&mut self, name: &str, summary: &str, now: Instant) -> Vec<TraceFact> {
        let mut facts = self.flush(now);
        facts.push(TraceFact::ToolCall {
            name: name.to_owned(),
            summary: truncate_summary(summary).to_owned(),
        });
        facts
    }

    /// Records a tool result, releasing buffered text first.
    pub fn push_tool_result(
        &mut self,
        name: &str,
        is_error: bool,
        summary: &str,
        now: Instant,
    ) -> Vec<TraceFact> {
        let mut facts = self.flush(now);
        facts.push(TraceFact::ToolResult {
            name: name.to_owned(),
            is_error,
            summary: truncate_summary(summary).to_owned(),
        });
        facts
    }

    /// Records a state note, releasing buffered text first.
    pub fn push_state_note(&mut self, note: &str, now: Instant) -> Vec<TraceFact> {
        let mut facts = self.flush(now);
        facts.push(TraceFact::StateNote {
            note: note.to_owned(),
        });
        facts
    }

    /// Releases buffered text once it has been pending for
    /// [`TRACE_TEXT_FLUSH_INTERVAL`].
    pub fn due(&mut self, now: Instant) -> Vec<TraceFact> {
        match self.pending_since {
            Some(since) if now.duration_since(since) >= TRACE_TEXT_FLUSH_INTERVAL => {
                self.take_pending()
            }
            _ => Vec::new(),
        }
    }

    /// Releases buffered text, if there is any.
    pub fn flush(&mut self, _now: Instant) -> Vec<TraceFact> {
        self.take_pending()
    }

    /// Drains the buffer, never emitting an empty fact.
    fn take_pending(&mut self) -> Vec<TraceFact> {
        self.pending_since = None;
        if self.pending.is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.pending);
        vec![TraceFact::AssistantText { text }]
    }
}

/// Truncates a summary to [`TRACE_TOOL_SUMMARY_LIMIT`] bytes without splitting
/// a multi-byte character.
fn truncate_summary(summary: &str) -> &str {
    if summary.len() <= TRACE_TOOL_SUMMARY_LIMIT {
        return summary;
    }
    let end = summary
        .char_indices()
        .take_while(|(index, _)| *index <= TRACE_TOOL_SUMMARY_LIMIT)
        .last()
        .map_or(0, |(index, _)| index);
    &summary[..end]
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn text_fragments_are_merged_until_the_byte_threshold() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        assert!(aggregator.push_text("hello ", now).is_empty());
        assert!(aggregator.push_text("world", now).is_empty());
        let facts = aggregator.push_text(&"x".repeat(TRACE_TEXT_FLUSH_BYTES), now);
        assert_eq!(
            facts,
            vec![TraceFact::AssistantText {
                text: format!("hello world{}", "x".repeat(TRACE_TEXT_FLUSH_BYTES))
            }]
        );
    }

    #[test]
    fn a_tool_call_flushes_buffered_text_first() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        aggregator.push_text("thinking out loud", now);
        let facts = aggregator.push_tool_call("bash", "cargo test", now);
        assert_eq!(facts.len(), 2);
        assert_eq!(
            facts[0],
            TraceFact::AssistantText {
                text: "thinking out loud".into()
            }
        );
        assert_eq!(
            facts[1],
            TraceFact::ToolCall {
                name: "bash".into(),
                summary: "cargo test".into()
            }
        );
    }

    #[test]
    fn buffered_text_flushes_on_the_time_window() {
        let mut aggregator = TraceAggregator::new();
        let start = Instant::now();
        aggregator.push_text("slow stream", start);
        assert!(
            aggregator
                .due(start + TRACE_TEXT_FLUSH_INTERVAL / 2)
                .is_empty()
        );
        let facts = aggregator.due(start + TRACE_TEXT_FLUSH_INTERVAL);
        assert_eq!(
            facts,
            vec![TraceFact::AssistantText {
                text: "slow stream".into()
            }]
        );
    }

    #[test]
    fn tool_summaries_are_truncated_to_the_limit() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        let facts = aggregator.push_tool_result(
            "bash",
            false,
            &"y".repeat(TRACE_TOOL_SUMMARY_LIMIT * 2),
            now,
        );
        let TraceFact::ToolResult { summary, .. } = &facts[0] else {
            panic!("expected tool result")
        };
        assert!(summary.len() <= TRACE_TOOL_SUMMARY_LIMIT);
    }

    #[test]
    fn an_empty_buffer_never_emits_an_empty_text_fact() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        assert!(aggregator.flush(now).is_empty());
        assert!(
            aggregator
                .push_state_note("running", now)
                .iter()
                .all(|fact| !matches!(fact, TraceFact::AssistantText { .. }))
        );
    }

    #[test]
    fn a_state_note_flushes_buffered_text_first() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        aggregator.push_text("wrapping up", now);
        let facts = aggregator.push_state_note("completed", now);
        assert_eq!(
            facts,
            vec![
                TraceFact::AssistantText {
                    text: "wrapping up".into()
                },
                TraceFact::StateNote {
                    note: "completed".into()
                },
            ]
        );
    }

    #[test]
    fn text_flushes_exactly_at_the_byte_threshold() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        let almost = "x".repeat(TRACE_TEXT_FLUSH_BYTES - 1);
        assert!(aggregator.push_text(&almost, now).is_empty());
        let facts = aggregator.push_text("y", now);
        assert_eq!(
            facts,
            vec![TraceFact::AssistantText {
                text: format!("{almost}y")
            }]
        );
    }

    #[test]
    fn tool_summaries_are_truncated_on_a_utf8_boundary() {
        let mut aggregator = TraceAggregator::new();
        let now = Instant::now();
        // "€" is three bytes, so the limit lands in the middle of a character.
        let facts = aggregator.push_tool_call("bash", &"€".repeat(TRACE_TOOL_SUMMARY_LIMIT), now);
        let TraceFact::ToolCall { summary, .. } = &facts[0] else {
            panic!("expected tool call")
        };
        assert!(summary.len() <= TRACE_TOOL_SUMMARY_LIMIT);
        assert!(summary.chars().all(|character| character == '€'));
        assert_eq!(summary.chars().count(), (TRACE_TOOL_SUMMARY_LIMIT - 1) / 3);
    }

    #[test]
    fn facts_round_trip_through_their_snake_case_type_tag() {
        let fact = TraceFact::ToolResult {
            name: "bash".into(),
            is_error: true,
            summary: "boom".into(),
        };
        let json = serde_json::to_value(&fact).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "type": "tool_result",
                "name": "bash",
                "is_error": true,
                "summary": "boom",
            })
        );
        assert_eq!(serde_json::from_value::<TraceFact>(json).unwrap(), fact);
    }
}
