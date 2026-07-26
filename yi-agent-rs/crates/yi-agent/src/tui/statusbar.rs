//! Status bar: token counts (prefill/decode) + running task indicator + model.
//!
//! The status bar state ticks at 30hz (driven by the TUI poll loop) and
//! linearly interpolates the displayed token counts toward the latest
//! target reported by `ProviderEvent::Usage`. The running-task dot uses
//! a gray-to-white breathing pulse to make activity visible at a glance.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::tui::state::{RunningTaskRegistry, TaskStatus};

/// State for the status bar. Tick at ~30hz for smooth interpolation + spinner.
#[derive(Debug, Clone, Default)]
pub struct StatusBarState {
    target_input: u64,
    target_output: u64,
    display_input: u64,
    display_output: u64,
    spinner_phase: u32, // 0..360 (degrees)
    last_usage_time: Option<std::time::Instant>,
}

impl StatusBarState {
    /// Update the target token counts from a new Usage event.
    /// Targets are monotonic within a call (take the max), since providers
    /// may emit multiple Usage events as the stream progresses.
    /// Real usage overrides any prior estimate.
    pub fn set_token_target(&mut self, input: u64, output: u64) {
        self.target_input = self.target_input.max(input);
        self.target_output = self.target_output.max(output);
        self.last_usage_time = Some(std::time::Instant::now());
    }

    /// Set a heuristic prefill estimate at the start of a new LLM call.
    /// Resets per-call counters so each think turn shows its own values;
    /// real usage later overrides via `set_token_target` max.
    pub fn set_prefill_estimate(&mut self, input: u64) {
        self.target_input = input;
        self.target_output = 0;
        self.display_output = 0;
        self.last_usage_time = None;
    }

    /// Accumulate decode estimate from streamed text deltas.
    pub fn estimate_decode_tokens(&mut self, text: &str) {
        if self.last_usage_time.is_none() {
            self.target_output = self.target_output.saturating_add(estimate_tokens(text));
        }
    }

    /// Advance interpolation + spinner by one tick. Call at ~30hz.
    pub fn tick(&mut self) {
        // Linear interpolation: move ~1/10 of the remaining gap per tick,
        // with a minimum step of 1 so small targets still converge.
        let di = self.target_input.saturating_sub(self.display_input);
        let dd = self.target_output.saturating_sub(self.display_output);
        let step_i = (di / 10).max(1);
        let step_o = (dd / 10).max(1);
        self.display_input = self.display_input.saturating_add(step_i.min(di));
        self.display_output = self.display_output.saturating_add(step_o.min(dd));

        // Spinner phase: 8° per tick → ~1.5s per cycle at 30hz.
        self.spinner_phase = (self.spinner_phase + 8) % 360;

        // If no new usage for 1s, snap to target (call finished).
        if let Some(t) = self.last_usage_time {
            if t.elapsed() > std::time::Duration::from_secs(1) {
                self.display_input = self.target_input;
                self.display_output = self.target_output;
            }
        }
    }

    /// Reset per-call state for a new LLM call.
    pub fn reset_for_new_call(&mut self) {
        self.target_input = 0;
        self.target_output = 0;
        self.display_input = 0;
        self.display_output = 0;
        self.last_usage_time = None;
    }

    pub fn display_input_tokens(&self) -> u64 {
        self.display_input
    }
    pub fn display_output_tokens(&self) -> u64 {
        self.display_output
    }
    #[cfg(test)]
    pub fn target_output_tokens(&self) -> u64 {
        self.target_output
    }
    #[allow(dead_code)]
    pub fn spinner_hue(&self) -> u32 {
        self.spinner_phase
    }
    pub fn spinner_color(&self) -> Color {
        // Gray-to-white breathing: brightness oscillates in [100, 255].
        // spinner_phase is 0..360; map to a sine wave for a soft pulse.
        let phase = self.spinner_phase as f32 * std::f32::consts::PI / 180.0;
        let v = 100.0 + 77.5 * (phase.sin() + 1.0); // (sin+1)/2 * 155 + 100 → [100, 255]
        let v = v.round() as u8;
        Color::Rgb(v, v, v)
    }
}

/// Heuristic token estimate: ASCII ~4 chars/token, non-ASCII (CJK etc.) ~1.5 chars/token.
fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for c in text.chars() {
        if (c as u32) < 0x80 {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    (ascii as f32 / 4.0 + non_ascii as f32 / 1.5) as u64
}

/// Format an integer with thousands separators (e.g., 12345 → "12,345").
pub fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.insert(0, ',');
        }
        out.insert(0, c);
    }
    out
}

/// Render the status bar as a single `Line`.
///
/// Layout (left to right):
///   `● <tool> <elapsed>`  (only when tasks running)
///   `prefill <n>  decode <m>`
///   `<model>`
pub fn render_statusbar<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    model: &'a str,
) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();

    let running: Vec<&crate::tui::state::TaskState> = tasks
        .list()
        .into_iter()
        .filter(|t| t.status == TaskStatus::Running)
        .collect();
    if !running.is_empty() {
        let dot = Span::styled("●", Style::new().fg(state.spinner_color()));
        let count = running.len();
        let oldest = running
            .iter()
            .map(|t| t.elapsed())
            .max()
            .unwrap_or_default();
        let secs = oldest.as_secs_f32();
        let label = if count == 1 {
            format!(" {} {:.1}s", running[0].tool_name, secs)
        } else {
            format!(" {}({}) {:.1}s", running[0].tool_name, count, secs)
        };
        spans.push(dot);
        spans.push(Span::raw(label));
        spans.push(Span::raw("  "));
    }

    spans.push(Span::raw("prefill "));
    spans.push(Span::styled(
        format_thousands(state.display_input_tokens()),
        Style::new().fg(Color::Gray),
    ));
    spans.push(Span::raw("  decode "));
    spans.push(Span::styled(
        format_thousands(state.display_output_tokens()),
        Style::new().fg(Color::Gray),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(model, Style::new().fg(Color::DarkGray)));

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_interpolation_approaches_target() {
        let mut s = StatusBarState::default();
        s.set_token_target(1000, 500);
        // Within ~120 ticks (4s at 30hz) we should be within 10 of target.
        for _ in 0..120 {
            s.tick();
        }
        let di = s.display_input_tokens();
        assert!(
            (di as i64 - 1000).abs() < 10,
            "display_input {di} should approach 1000"
        );
        let dd = s.display_output_tokens();
        assert!(
            (dd as i64 - 500).abs() < 10,
            "display_output {dd} should approach 500"
        );
    }

    #[test]
    fn test_token_interpolation_snaps_after_idle() {
        let mut s = StatusBarState::default();
        s.set_token_target(10_000, 5_000);
        s.tick();
        assert!(s.display_input_tokens() < 10_000); // not yet snapped
        // Simulate >1s idle since last usage.
        s.last_usage_time = Some(std::time::Instant::now() - std::time::Duration::from_secs(2));
        s.tick();
        assert_eq!(s.display_input_tokens(), 10_000);
        assert_eq!(s.display_output_tokens(), 5_000);
    }

    #[test]
    fn test_reset_for_new_call() {
        let mut s = StatusBarState::default();
        s.set_token_target(123, 456);
        s.tick();
        s.reset_for_new_call();
        assert_eq!(s.display_input_tokens(), 0);
        assert_eq!(s.display_output_tokens(), 0);
        assert_eq!(s.target_input, 0);
    }

    #[test]
    fn test_spinner_hue_advances() {
        let mut s = StatusBarState::default();
        let h1 = s.spinner_hue();
        s.tick();
        let h2 = s.spinner_hue();
        assert_ne!(h1, h2);
        assert_eq!(h2, (h1 + 8) % 360);
    }

    #[test]
    fn test_format_thousands() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1234), "1,234");
        assert_eq!(format_thousands(10_000), "10,000");
        assert_eq!(format_thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn test_render_statusbar_empty_state() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let line = render_statusbar(&state, &tasks, "claude-opus-4");
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            text.contains("prefill 0"),
            "empty state should still show prefill: {text}"
        );
        assert!(text.contains("claude-opus-4"));
    }

    #[test]
    fn test_render_statusbar_with_running_task() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("t1", "bash", "ls", 120);
        let state = StatusBarState::default();
        let line = render_statusbar(&state, &tasks, "model");
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("●"), "should show running dot: {text}");
        assert!(text.contains("bash"), "should show tool name: {text}");
    }

    #[test]
    fn test_set_prefill_estimate_sets_target() {
        let mut s = StatusBarState::default();
        s.set_prefill_estimate(500);
        assert_eq!(s.target_input, 500);
    }

    #[test]
    fn test_prefill_estimate_resets_per_call() {
        let mut s = StatusBarState::default();
        s.set_token_target(1000, 50);
        // New think turn: estimate resets everything
        s.set_prefill_estimate(500);
        assert_eq!(s.target_input, 500);
        assert_eq!(s.target_output, 0, "decode should reset for new call");
    }

    #[test]
    fn test_estimate_decode_accumulates() {
        let mut s = StatusBarState::default();
        s.estimate_decode_tokens("hello"); // 5 ascii → 1
        s.estimate_decode_tokens("world"); // 5 ascii → 1
        assert_eq!(s.target_output, 2);
    }

    #[test]
    fn test_real_usage_overrides_decode_estimate() {
        let mut s = StatusBarState::default();
        s.estimate_decode_tokens("hello world"); // ~2 tokens estimated
        s.set_token_target(0, 100); // real usage: 100
        assert_eq!(s.target_output, 100, "real usage should override estimate");
    }

    #[test]
    fn test_reset_clears_estimates() {
        let mut s = StatusBarState::default();
        s.set_prefill_estimate(500);
        s.estimate_decode_tokens("hi");
        s.reset_for_new_call();
        assert_eq!(s.target_input, 0);
        assert_eq!(s.target_output, 0);
    }
}
