//! Status bar: token counts (prefill/decode) + running task indicator + model.
//!
//! The status bar state ticks at 30hz (driven by the TUI poll loop) and
//! linearly interpolates the displayed token counts toward the latest
//! target reported by `ProviderEvent::Usage`. The running-task dot breathes
//! between two theme colors so the pulse stays visible on light and dark
//! terminals alike.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use crate::tui::state::{RunningTaskRegistry, TaskStatus};
use crate::tui::theme::TuiTheme;

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
    ///
    /// `input` is always taken as a max (providers may emit multiple Usage
    /// events with growing input as cache stats arrive). For `output`, a
    /// positive value replaces any prior heuristic estimate — the API's real
    /// token count is authoritative. A zero `output` (Anthropic `message_start`
    /// emits Usage with only `input_tokens` known) is ignored on the output
    /// side so it does not clobber a running decode estimate.
    pub fn set_token_target(&mut self, input: u64, output: u64) {
        self.target_input = self.target_input.max(input);
        if output > 0 {
            self.target_output = output;
        }
        self.last_usage_time = Some(std::time::Instant::now());
    }

    /// Set a heuristic prefill estimate at the start of a new LLM call.
    /// Resets per-call counters so each think turn shows its own values;
    /// real usage later overrides via `set_token_target`.
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
        // with a minimum step of 1 so small targets still converge. The
        // display moves toward the target in *both* directions: auto-compaction
        // lowers the prefill estimate, and the display must follow it down.
        // It was previously up-only, so it froze at the stale pre-compaction
        // value until a usage-idle snap happened to fire.
        self.display_input = interpolate(self.display_input, self.target_input);
        self.display_output = interpolate(self.display_output, self.target_output);

        // Spinner phase: 4° per tick → ~3s per cycle at 30hz.
        self.spinner_phase = (self.spinner_phase + 4) % 360;

        // If no new usage for 1s, snap to target (call finished).
        if let Some(t) = self.last_usage_time
            && t.elapsed() > std::time::Duration::from_secs(1)
        {
            self.display_input = self.target_input;
            self.display_output = self.target_output;
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

    /// Called when tool execution begins (ToolCall event received).
    /// Resets the decode counter so the user sees a clear transition from
    /// "decoding" to "executing tools". Without this, the decode display
    /// would show the stale count from the previous LLM turn throughout
    /// the entire tool execution phase.
    pub fn on_tool_call_phase(&mut self) {
        self.target_output = 0;
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
    /// Breathing dot for the default (dark) theme.
    ///
    /// Kept for callers that predate theming; new call sites should pass the
    /// active theme to [`StatusBarState::spinner_color_for`]. `render_statusbar_with`
    /// uses the theme-aware form, so this only has test callers today.
    #[allow(dead_code)]
    pub fn spinner_color(&self) -> Color {
        self.spinner_color_for(TuiTheme::DARK)
    }

    /// Breathing dot between the theme's faint and body foreground.
    ///
    /// The old gray↔white ramp peaked at `Rgb(255,255,255)`, which is 1.00:1 on
    /// a white background — the dot vanished once per pulse on light terminals.
    pub fn spinner_color_for(&self, theme: TuiTheme) -> Color {
        let phase = self.spinner_phase as f32 * std::f32::consts::PI / 180.0;
        let t = (phase.sin() + 1.0) / 2.0; // 0.0 dim ..= 1.0 bright
        lerp_color(theme.fg_faint, theme.fg, t)
    }
}

/// Move `current` one interpolated step toward `target`: ~1/10 of the gap,
/// at least 1, never overshooting. Walks in both directions so the displayed
/// count can fall when the target shrinks (e.g. prefill after auto-compaction).
fn interpolate(current: u64, target: u64) -> u64 {
    let gap = current.abs_diff(target);
    let step = (gap / 10).max(1).min(gap);
    if current < target {
        current + step
    } else {
        current - step
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

/// Linear RGB interpolation. Used for the spinner ramp so both endpoints are
/// theme-controlled.
fn lerp_color(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return to;
    };
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color::Rgb(mix(fr, tr), mix(fg, tg), mix(fb, tb))
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

/// 状态栏里的一句话旁支标识。
///
/// 只占既有状态栏的宽度预算，**不**新开一行，也不画在转录首行（spec §7：
/// 介绍条会遮住旁支第一条正文）。读屏靠的就是这段文字：它必须能独立说明
/// 「你现在在旁支里、怎么回去」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideStatusLabel {
    /// 窄屏也必须留下的一段：身份 + 怎么回去。
    ///
    /// 单独存一份而不是从完整文本切出来：切字符串很容易把 `/back` 拦腰截断，
    /// 那正好是用户最需要的那几个字符。
    pub must: String,
    /// 预算够时显示的完整文本（身份 + 返回 + 父会话状态）。
    pub full: String,
}

impl SideStatusLabel {
    /// 旁支视图：身份 + 两条命令（窄屏必留），`extra` 是父会话状态或一条错误。
    ///
    /// `/side` 与 `/back` 同时出现是有意的：用户在旁支里要能一眼看到「怎么回去」，
    /// 而 `/side` 提示这个会话本来就是从 `/side` 来的、还能重入。
    pub fn side(extra: Option<&str>) -> Self {
        let must = "旁问 /side · /back".to_string();
        let full = match extra.map(str::trim).filter(|text| !text.is_empty()) {
            Some(extra) => format!("{must} 返回 · {extra}"),
            None => must.clone(),
        };
        Self { must, full }
    }

    /// 主视图：**只有**后台旁支操作失败时才出现，`extra` 是那条失败说明。
    ///
    /// 不写「旁问」身份——用户已经在主视图，写上会让人以为还在旁支里。
    pub fn main(extra: &str) -> Self {
        Self {
            must: "/side".to_string(),
            full: format!("/side {extra}"),
        }
    }
}

/// Render the status bar as a single `Line`.
///
/// Layout (left to right):
///   `● <tool> <elapsed>`  (only when foreground tasks are running)
///   `N proc running`  (only when managed background processes are active)
///   `已略去 N 条更早的历史`  (only when history cells were dropped at the cap)
///   `<旁问 · /back 父会话…>`  (only when the side view is visible)
///   `prefill <n>  decode <m>`
///   `<model>`
///
/// Dark-theme forwarder. The TUI draws with [`render_statusbar_with`] so it can
/// pass the detected theme; this keeps the pre-theming signature available
/// (tests call it) and only has test callers today.
#[allow(dead_code)]
pub fn render_statusbar<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    active_process_count: usize,
    trimmed_history: usize,
    model: &'a str,
    width: u16,
) -> Line<'a> {
    render_statusbar_with(
        state,
        tasks,
        active_process_count,
        trimmed_history,
        model,
        width,
        TuiTheme::DARK,
    )
}

/// Theme-aware [`render_statusbar`]. Every color comes from a semantic slot so
/// the row stays readable on a light terminal instead of relying on ANSI slots
/// (`Gray`/`DarkGray`) whose contrast depends on the user's palette.
pub fn render_statusbar_with<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    active_process_count: usize,
    trimmed_history: usize,
    model: &'a str,
    width: u16,
    theme: TuiTheme,
) -> Line<'a> {
    render_statusbar_with_side(
        state,
        tasks,
        active_process_count,
        trimmed_history,
        model,
        width,
        theme,
        None,
    )
}

/// 带旁支标识的状态栏。
///
/// 旁支段放在费用段**之前**：旁支身份与返回命令是「我在哪、怎么走」，
/// 比 token 计数更要紧；窄屏预算不够时先丢的是后者（费用/模型本来就会被
/// 逐段让位）。
#[allow(clippy::too_many_arguments)]
pub fn render_statusbar_with_side<'a>(
    state: &'a StatusBarState,
    tasks: &'a RunningTaskRegistry,
    active_process_count: usize,
    trimmed_history: usize,
    model: &'a str,
    width: u16,
    theme: TuiTheme,
    side: Option<&SideStatusLabel>,
) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    // The status row is exactly one terminal row tall, so the assembled line
    // must never exceed `width`; an over-wide line is right-clipped by ratatui
    // (the model name is the first casualty). Track the columns spent and stop
    // appending segments once the budget is gone.
    let budget = width as usize;
    let mut used = 0usize;
    let push = |spans: &mut Vec<Span<'a>>, span: Span<'a>, used: &mut usize| {
        let w = UnicodeWidthStr::width(span.content.as_ref());
        if *used + w <= budget {
            *used += w;
            spans.push(span);
        }
    };

    let running: Vec<&crate::tui::state::TaskState> = tasks
        .list()
        .into_iter()
        .filter(|t| t.status == TaskStatus::Running)
        .collect();
    if !running.is_empty() {
        let dot = Span::styled("●", Style::new().fg(state.spinner_color_for(theme)));
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
        push(&mut spans, dot, &mut used);
        push(&mut spans, Span::raw(label), &mut used);
        push(&mut spans, Span::raw("  "), &mut used);
    }

    if active_process_count > 0 {
        push(
            &mut spans,
            Span::raw(format!("{active_process_count} proc running  ")),
            &mut used,
        );
    }

    // Persistent notice that the scrollback was trimmed at `MAX_HISTORY_CELLS`.
    // It rides the same width budget as every other segment, so the one-line
    // invariant holds; it is omitted entirely while nothing has been dropped.
    if trimmed_history > 0 {
        push(
            &mut spans,
            Span::styled(
                format!("已略去 {trimmed_history} 条更早的历史  "),
                Style::new().fg(theme.fg_faint),
            ),
            &mut used,
        );
    }

    if let Some(side) = side {
        // 先算「至少要留出身份 + 返回命令」。预算只够这两项时也不能整段丢掉，
        // 否则窄屏用户看不出自己在旁支里、也看不到怎么回去。
        let room = budget.saturating_sub(used);
        let text = if room <= UnicodeWidthStr::width(side.must.as_str()) {
            crate::tui::wrap::truncate_to_width(side.must.trim_end(), room)
        } else {
            crate::tui::wrap::truncate_to_width(&side.full, room)
        };
        push(
            &mut spans,
            Span::styled(text, Style::new().fg(theme.fg_muted)),
            &mut used,
        );
        push(&mut spans, Span::raw("  "), &mut used);
    }

    push(&mut spans, Span::raw("prefill "), &mut used);
    push(
        &mut spans,
        Span::styled(
            format_thousands(state.display_input_tokens()),
            Style::new().fg(theme.fg_muted),
        ),
        &mut used,
    );
    push(&mut spans, Span::raw("  decode "), &mut used);
    push(
        &mut spans,
        Span::styled(
            format_thousands(state.display_output_tokens()),
            Style::new().fg(theme.fg_muted),
        ),
        &mut used,
    );
    push(&mut spans, Span::raw("  "), &mut used);
    // The model name is truncated (not dropped) so the closing segment still
    // shows which model is in use on a narrow terminal.
    push(
        &mut spans,
        Span::styled(
            crate::tui::wrap::truncate_to_width(model, budget.saturating_sub(used)),
            Style::new().fg(theme.fg_faint),
        ),
        &mut used,
    );

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn side_label_keeps_identity_and_back_command_on_a_narrow_bar() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let label = SideStatusLabel::side(Some("父会话运行中"));
        // 32 列：扣掉 prefill/decode 后留给旁支段的预算非常小。
        let line = render_statusbar_with_side(
            &state,
            &tasks,
            0,
            0,
            "test-model",
            32,
            TuiTheme::DARK,
            Some(&label),
        );
        let text = line_text(&line);
        assert!(text.contains("/back"), "窄屏必须留下返回命令，实际: {text}");
        assert!(text.contains("旁问"), "窄屏必须留下旁支身份，实际: {text}");
        assert!(
            UnicodeWidthStr::width(text.as_str()) <= 32,
            "状态栏必须保持一行不溢出，实际: {text:?}"
        );
    }

    #[test]
    fn side_error_text_is_not_dropped_by_the_cost_segments() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let label = SideStatusLabel::side(Some("关闭旁问失败：未送达"));
        let line = render_statusbar_with_side(
            &state,
            &tasks,
            0,
            0,
            "test-model",
            80,
            TuiTheme::DARK,
            Some(&label),
        );
        let text = line_text(&line);
        assert!(
            text.contains("未送达"),
            "错误尾部不能被费用/模型段挤掉，实际: {text}"
        );
    }

    #[test]
    fn main_label_does_not_claim_the_user_is_inside_a_side() {
        let label = SideStatusLabel::main("关闭失败：未送达");
        assert!(
            !label.full.contains("旁问"),
            "主视图的提示不能让人以为还在旁支里，实际: {}",
            label.full
        );
    }

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
        assert_eq!(h2, (h1 + 4) % 360);
    }

    #[test]
    fn statusbar_fits_the_terminal_width() {
        // The status row is one line tall: anything wider is clipped on the
        // right, and the model name is the first thing to disappear.
        let mut state = StatusBarState::default();
        state.set_token_target(123_456, 7_890);
        let tasks = RunningTaskRegistry::new();
        for width in [20u16, 40, 80] {
            let line = render_statusbar(
                &state,
                &tasks,
                0,
                0,
                "claude-opus-4-with-a-long-name",
                width,
            );
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= width as usize, "width {width}: status bar is {w} cols");
        }
    }

    /// The history cap trims the scrollback; the status bar must say so
    /// persistently instead of dropping cells silently. The notice rides the
    /// same width budget as every other segment, so a narrow terminal must
    /// never end up with a clipped (over-wide) status row.
    #[test]
    fn statusbar_reports_trimmed_history_within_the_width_budget() {
        let mut state = StatusBarState::default();
        state.set_token_target(123_456, 7_890);
        let tasks = RunningTaskRegistry::new();

        let line = render_statusbar(&state, &tasks, 0, 3, "claude-opus-4", 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("已略去"), "notice must be labelled: {text:?}");
        assert!(text.contains('3'), "dropped count must be shown: {text:?}");
        assert!(
            text.contains("历史"),
            "notice must mention history: {text:?}"
        );

        // Nothing dropped yet → the segment must be entirely absent.
        let clean = render_statusbar(&state, &tasks, 0, 0, "claude-opus-4", 80);
        let clean_text: String = clean.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !clean_text.contains("历史"),
            "zero dropped cells must stay silent: {clean_text:?}"
        );

        // The one-line width invariant still holds, including when the notice
        // is the only segment that fits.
        for width in [20u16, 24, 40, 80] {
            let line = render_statusbar(&state, &tasks, 0, 3, "claude-opus-4", width);
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(w <= width as usize, "width {width}: status bar is {w} cols");
        }
    }

    #[test]
    fn statusbar_keeps_the_model_name_when_it_fits() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let line = render_statusbar(&state, &tasks, 0, 0, "model", 80);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("model"), "model name was dropped: {text:?}");
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
        let line = render_statusbar(&state, &tasks, 0, 0, "claude-opus-4", 80);
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
        let line = render_statusbar(&state, &tasks, 0, 0, "model", 80);
        let text: String = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(text.contains("●"), "should show running dot: {text}");
        assert!(text.contains("bash"), "should show tool name: {text}");
    }

    #[test]
    fn test_render_statusbar_omits_inactive_managed_processes() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let line = render_statusbar(&state, &tasks, 0, 0, "model", 80);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            !text.contains("proc running"),
            "zero count must be omitted: {text}"
        );
    }

    #[test]
    fn test_render_statusbar_shows_managed_process_count_without_duration() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::new();
        let line = render_statusbar(&state, &tasks, 2, 0, "model", 80);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.contains("2 proc running"),
            "count should be rendered: {text}"
        );
        assert!(
            !text.contains("proc running 0."),
            "process field must not include time: {text}"
        );
    }

    #[test]
    fn test_render_statusbar_shows_bash_and_managed_processes_together() {
        let mut tasks = RunningTaskRegistry::new();
        tasks.on_tool_call("bash-1", "bash", "sleep 10", 120);
        let state = StatusBarState::default();
        let line = render_statusbar(&state, &tasks, 1, 0, "model", 80);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            text.contains("● bash"),
            "bash indicator should remain: {text}"
        );
        assert!(
            text.contains("1 proc running"),
            "process count should render: {text}"
        );
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

    /// After auto-compaction the retained context is much smaller, so the
    /// prefill estimate drops sharply. The display must follow it *down*;
    /// before the fix it was monotonic (up-only) and stayed at the stale
    /// pre-compaction value.
    #[test]
    fn test_prefill_follows_estimate_down_after_compaction() {
        let mut s = StatusBarState::default();
        s.set_prefill_estimate(500); // turn 1 estimate
        s.set_token_target(160_000, 200); // turn 1 real usage (pre-compaction)
        for _ in 0..120 {
            s.tick();
        }
        assert!(
            s.display_input_tokens() > 100_000,
            "display should have grown to the pre-compaction value, got {}",
            s.display_input_tokens()
        );

        // Tool execution runs (resets decode + clears last_usage_time, but
        // leaves the prefill display untouched), then the next turn compacts
        // and emits a much smaller estimate.
        s.on_tool_call_phase();
        s.set_prefill_estimate(30_000);
        for _ in 0..120 {
            s.tick();
        }

        assert!(
            s.display_input_tokens() < 40_000,
            "prefill display must follow the compacted estimate down, got {}",
            s.display_input_tokens()
        );
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
    fn test_real_usage_overrides_inflated_estimate() {
        // Reproduces the reported bug: heuristic estimate (e.g. 2370 from
        // long tool-call JSON + CJK text) far exceeds the real output_tokens
        // (e.g. 285). Real usage must replace the estimate, not be clamped
        // away by max().
        let mut s = StatusBarState::default();
        // Simulate ~2370 tokens of estimated decode from a long stream of
        // tool-call JSON deltas (ASCII, 4 chars/token).
        let big = "a".repeat(2370 * 4);
        s.estimate_decode_tokens(&big);
        assert!(
            s.target_output >= 2370,
            "estimate should reach ~2370, got {}",
            s.target_output
        );
        s.set_token_target(0, 285); // real usage from API
        assert_eq!(
            s.target_output, 285,
            "real usage (285) must override inflated estimate ({})",
            s.target_output
        );
    }

    #[test]
    fn test_input_only_usage_does_not_clobber_decode_estimate() {
        // Anthropic message_start emits Usage with output_tokens=0 (only
        // input_tokens are known at stream start). A zero output must NOT
        // replace a running heuristic estimate, otherwise the status bar
        // would flicker to 0 before the real output_tokens arrive in
        // message_delta.
        let mut s = StatusBarState::default();
        s.estimate_decode_tokens("hello world"); // ~2 tokens
        s.set_token_target(500, 0); // message_start: input only, output=0
        assert_eq!(
            s.target_output, 2,
            "output=0 (input-only event) must not clobber estimate"
        );
        assert_eq!(s.target_input, 500, "input should still update");
        // Subsequent estimate deltas must stop once any usage arrived.
        s.estimate_decode_tokens("more text here");
        assert_eq!(
            s.target_output, 2,
            "estimates must stop after any usage event"
        );
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

    /// When tool execution begins (ToolCall event), the decode display must
    /// reset so the user sees "decode ended, tools running" instead of the
    /// stale decode count from the previous LLM turn lingering during the
    /// entire tool execution phase.
    ///
    /// Without this, the decode counter stays frozen at the previous LLM
    /// response's value from when `Usage` arrived until the NEXT `Start`
    /// event (which could be seconds or minutes later if a bash command
    /// is long-running).
    #[test]
    fn test_tool_call_freezes_decode_display() {
        let mut s = StatusBarState::default();
        // Simulate LLM producing decode tokens
        s.set_prefill_estimate(5000);
        s.estimate_decode_tokens("hello world from the model");
        s.set_token_target(5000, 120);
        // Display is interpolating toward target
        s.tick();
        assert!(s.display_output_tokens() > 0, "decode should be showing");

        // Now tool execution begins — call the freeze/reset
        s.on_tool_call_phase();

        // The decode display should no longer show the stale count.
        // Either it's zeroed or frozen — the key requirement is that it
        // doesn't keep displaying the previous LLM turn's decode count.
        assert_eq!(
            s.display_output_tokens(),
            0,
            "decode display should reset when tool execution begins"
        );
        assert_eq!(
            s.target_output, 0,
            "decode target should reset when tool execution begins"
        );
    }

    /// The prefill/decode counters used `Color::Gray`, which is 1.90:1 on a
    /// white background — effectively invisible.
    #[test]
    fn token_counters_use_theme_muted_not_raw_gray() {
        let mut state = StatusBarState::default();
        state.set_token_target(1200, 340);
        // `Line` borrows the registry, so it needs a named local.
        let tasks = RunningTaskRegistry::default();
        let line = render_statusbar_with(&state, &tasks, 0, 0, "m", 80, TuiTheme::LIGHT);
        let flat: Vec<_> = line.spans.iter().collect();
        assert!(
            flat.iter()
                .any(|s| s.style.fg == Some(TuiTheme::LIGHT.fg_muted)),
            "expected a span painted with the light theme's muted foreground"
        );
        assert!(
            !flat.iter().any(|s| s.style.fg == Some(Color::Gray)),
            "raw Color::Gray must not reach the light theme"
        );
    }

    /// The breathing dot interpolates between two theme colors instead of
    /// gray↔white, where the brightest phase was 1.00:1 on white.
    #[test]
    fn spinner_pulses_between_theme_colors() {
        // Phase 0 and 180 are the two ends of the sine wave.
        let mut state = StatusBarState {
            spinner_phase: 270, // sin = -1 → dimmest
            ..Default::default()
        };
        let dim = state.spinner_color_for(TuiTheme::LIGHT);
        state.spinner_phase = 90; // sin = +1 → brightest
        let bright = state.spinner_color_for(TuiTheme::LIGHT);
        assert_eq!(dim, TuiTheme::LIGHT.fg_faint);
        assert_eq!(bright, TuiTheme::LIGHT.fg);
    }

    /// The trimmed-history notice and the model name were `Color::DarkGray`,
    /// which is near-invisible on a white background.
    #[test]
    fn faint_segments_use_theme_faint_not_raw_dark_gray() {
        let state = StatusBarState::default();
        let tasks = RunningTaskRegistry::default();
        let line = render_statusbar_with(&state, &tasks, 0, 3, "model", 80, TuiTheme::LIGHT);
        assert!(
            !line
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::DarkGray)),
            "raw Color::DarkGray must not reach the light theme"
        );
        assert_eq!(
            line.spans[0].style.fg,
            Some(TuiTheme::LIGHT.fg_faint),
            "the trimmed-history notice must be faint"
        );
        assert_eq!(
            line.spans.last().map(|s| s.style.fg),
            Some(Some(TuiTheme::LIGHT.fg_faint)),
            "the model name must be faint"
        );
    }
}
