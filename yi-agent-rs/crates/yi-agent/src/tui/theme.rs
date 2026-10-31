//! TUI color themes: semantic slots + terminal background detection.

use ratatui::style::Color;

/// Which terminal background the theme was picked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiThemeKind {
    Dark,
    Light,
}

/// Semantic color slots. Renderers never name a raw ANSI color: they ask for a
/// role (`fg_muted`, `accent`, ...) so the same code reads correctly on both a
/// light and a dark terminal.
///
/// Values are `Rgb` on purpose. ANSI slots (`Color::DarkGray`) resolve to
/// whatever the terminal's palette says, which is exactly why the old TUI went
/// unreadable on light backgrounds: the same SGR 90 is near-invisible on one
/// theme and correct on another.
#[derive(Debug, Clone, Copy)]
pub struct TuiTheme {
    pub kind: TuiThemeKind,
    /// Body foreground. Used where a foreground must be explicit (e.g. text on
    /// the input stripe), not to paint over the terminal default.
    pub fg: Color,
    /// Secondary: tool-result summaries, trace steps, popup empty states.
    pub fg_muted: Color,
    /// Faintest: model name, fold hints, "已略去 N 条更早的历史".
    pub fg_faint: Color,
    /// Emphasis: inline code, math, slash menu, "发消息>".
    pub accent: Color,
    /// In progress / advisory.
    pub warn: Color,
    /// Failure / denial / cancellation.
    pub danger: Color,
    /// Success / completion.
    pub success: Color,
    /// Raised surface background (input stripe, popup cards).
    pub bg_raised: Color,
    /// Selected row foreground / background.
    pub sel_fg: Color,
    pub sel_bg: Color,
    /// Cursor. Rendering uses REVERSED, so these only matter where a solid
    /// block is needed.
    ///
    /// Nothing reads them yet: the cursor is drawn with REVERSED, which needs no
    /// colors. They exist so a future solid-block cursor has theme-controlled
    /// values instead of a fresh hardcoded white/black pair.
    #[allow(dead_code)]
    pub cursor_fg: Color,
    #[allow(dead_code)]
    pub cursor_bg: Color,
}

impl TuiTheme {
    /// Equivalent to the historical look, with the faint grays lightened: the
    /// old `DarkGray` (#585858) was only 2.34:1 on a dark background.
    pub const DARK: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Dark,
        fg: Color::Rgb(0xe4, 0xe4, 0xe4),       // 13.11:1 on #1e1e1e
        fg_muted: Color::Rgb(0xb2, 0xb2, 0xb2), //  7.86:1
        fg_faint: Color::Rgb(0x8c, 0x8c, 0x8c), //  4.96:1
        accent: Color::Rgb(0x5f, 0xd7, 0xd7),   //  9.68:1
        warn: Color::Rgb(0xe3, 0xc5, 0x6b),     //  9.90:1
        danger: Color::Rgb(0xff, 0x7b, 0x72),   //  6.61:1
        success: Color::Rgb(0x7e, 0xe7, 0x87),  // 10.85:1
        bg_raised: Color::Rgb(0x2a, 0x2a, 0x2a), // 1.16 vs #1e1e1e
        sel_fg: Color::Rgb(0xff, 0xff, 0xff),   //  5.06:1 on sel_bg
        sel_bg: Color::Rgb(0x2f, 0x6f, 0xbf),
        cursor_fg: Color::Rgb(0x1e, 0x1e, 0x1e),
        cursor_bg: Color::Rgb(0xe4, 0xe4, 0xe4),
    };

    /// Re-picked values, not an inversion: inverting the dark palette yields
    /// pure black on pure white and blows the contrast budget.
    pub const LIGHT: TuiTheme = TuiTheme {
        kind: TuiThemeKind::Light,
        fg: Color::Rgb(0x1f, 0x1f, 0x1f),       // 16.48:1 on #ffffff
        fg_muted: Color::Rgb(0x4a, 0x4a, 0x4a), //  8.86:1
        fg_faint: Color::Rgb(0x6b, 0x6b, 0x6b), //  5.33:1
        accent: Color::Rgb(0x0a, 0x6b, 0x6b),   //  6.31:1
        warn: Color::Rgb(0x7a, 0x5c, 0x00),     //  6.25:1
        danger: Color::Rgb(0xa0, 0x18, 0x18),   //  7.96:1
        success: Color::Rgb(0x14, 0x69, 0x1f),  //  6.84:1
        bg_raised: Color::Rgb(0xed, 0xed, 0xed), // 1.17 vs #ffffff
        sel_fg: Color::Rgb(0xff, 0xff, 0xff),   //  6.44:1 on sel_bg
        sel_bg: Color::Rgb(0x1f, 0x5f, 0xa8),
        cursor_fg: Color::Rgb(0xff, 0xff, 0xff),
        cursor_bg: Color::Rgb(0x1f, 0x1f, 0x1f),
    };

    pub fn of(kind: TuiThemeKind) -> TuiTheme {
        match kind {
            TuiThemeKind::Dark => TuiTheme::DARK,
            TuiThemeKind::Light => TuiTheme::LIGHT,
        }
    }
}

/// WCAG 2.1 relative luminance of an sRGB color.
///
/// `Color::Reset` and the ANSI palette slots have no fixed sRGB value; they are
/// approximated as mid-gray so the math stays total instead of panicking.
pub(crate) fn relative_luminance(color: Color) -> f32 {
    fn channel(v: u8) -> f32 {
        let v = v as f32 / 255.0;
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    }
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        // Not used by any theme slot; a neutral value keeps callers total.
        _ => (0x80, 0x80, 0x80),
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// Resolve the theme from an explicit override and/or a raw OSC 11 reply.
///
/// Pure so the decision table is testable without a terminal. Precedence:
/// valid env override, then the reply's luminance, then `Dark`.
pub fn detect_theme_kind(env_override: Option<&str>, reply: Option<&str>) -> TuiThemeKind {
    match env_override
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("light") => return TuiThemeKind::Light,
        Some("dark") => return TuiThemeKind::Dark,
        Some(other) if !other.is_empty() => {
            tracing::warn!(
                value = %other,
                "ignoring unknown YI_AGENT_THEME; falling back to background detection"
            );
        }
        _ => {}
    }

    let Some(parsed) = reply.and_then(|r| parse_osc11_reply(r.as_bytes())) else {
        return TuiThemeKind::Dark;
    };
    if relative_luminance(Color::Rgb(parsed.0, parsed.1, parsed.2)) > 0.5 {
        TuiThemeKind::Light
    } else {
        TuiThemeKind::Dark
    }
}

/// Parse `ESC ] 11 ; rgb:RRRR/GGGG/BBBB <ST|BEL>` into 8-bit components.
///
/// Accepts 1- to 4-digit hex per channel (the xterm protocol allows any width
/// in that range; xterm itself writes 4) and both `ST` and `BEL` terminators.
///
/// The `ESC ] 11 ;` preamble is optional: callers that only keep the `rgb:`
/// tail can pass that too, so the decision is made on the `rgb:` marker
/// (`rgba:` also accepted) rather than on the wire preamble.
pub fn parse_osc11_reply(bytes: &[u8]) -> Option<(u8, u8, u8)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let body = text
        .split_once("rgb:")
        .or_else(|| text.split_once("rgba:"))
        .map(|(_, rest)| rest)?;
    // Strip the trailing ST (ESC \) / BEL / C1 ST, and any stray bytes that
    // follow the reply in a batched read.
    let body = body.split(['\x1b', '\x07', '\u{9c}']).next()?.trim();

    let mut parts = body.split('/');
    let mut channel = || -> Option<u8> {
        let raw = parts.next()?.trim();
        if raw.is_empty() || raw.len() > 4 || !raw.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let value = u32::from_str_radix(raw, 16).ok()?;
        // `ffff` → 255 and `ff` → 255: scale by the width actually written.
        let scale = 16u32.pow(raw.len() as u32) - 1;
        Some(((value * 255 + scale / 2) / scale) as u8)
    };
    Some((channel()?, channel()?, channel()?))
}

/// Detect the terminal background, falling back to `Dark` on any failure.
///
/// Must run in raw mode (see the OSC 11 note in
/// `docs/superpowers/specs/2026-10-31-tui-light-terminal-theme-design.md`
/// §3.3): without it the tty line-buffers and the reply never arrives.
///
/// Never writes to stdout/stderr on a failure path: a stray byte during TUI
/// startup corrupts the frame.
// Task 3 wires this into `run_tui` (after raw mode, before the alternate
// screen); until then nothing calls it and dead-code lint would fail `just lint`.
#[allow(dead_code)]
pub fn detect_theme_from_terminal() -> TuiThemeKind {
    let env_override = std::env::var("YI_AGENT_THEME").ok();
    if let Some(value) = env_override.as_deref() {
        let lowered = value.trim().to_ascii_lowercase();
        if lowered == "light" || lowered == "dark" {
            // A valid override needs no probe: skip the 200ms wait entirely.
            return detect_theme_kind(Some(value), None);
        }
    }
    let reply = query_osc11();
    detect_theme_kind(env_override.as_deref(), reply.as_deref())
}

/// Ask the terminal for its background color, bounded to 200ms.
///
/// Returns the raw reply text for `detect_theme_kind`. `None` on timeout, on a
/// non-tty, or when the terminal does not implement OSC 11.
///
/// Uses `libc::poll` on fd 0 rather than `crossterm::event::poll`/`read`: the
/// latter *consumes* the reply byte, so a following `Read` would block forever
/// and blow the 200ms budget.
#[allow(dead_code)] // Reached only through `detect_theme_from_terminal`.
fn query_osc11() -> Option<String> {
    use std::io::{IsTerminal, Read, Write};

    const BUDGET: std::time::Duration = std::time::Duration::from_millis(200);
    const POLL_SLICE_MS: u128 = 10;
    const MAX_REPLY_BYTES: usize = 256;

    if !std::io::stdin().is_terminal() {
        return None;
    }

    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        if out.write_all(b"\x1b]11;?\x1b\\").is_err() || out.flush().is_err() {
            tracing::warn!("OSC 11 probe could not write the query");
            return None;
        }
    }

    let deadline = std::time::Instant::now() + BUDGET;
    let mut stdin = std::io::stdin();
    let mut collected: Vec<u8> = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            tracing::warn!("OSC 11 probe timed out; assuming a dark background");
            return None;
        }
        // Poll in short slices so a terminal that ignores OSC 11 cannot hang
        // startup, and so the deadline is honoured even if poll overshoots.
        let remaining = deadline.saturating_duration_since(now);
        let slice_ms = remaining.as_millis().clamp(1, POLL_SLICE_MS) as libc::c_int;

        let mut pollfd = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is one initialized entry and nfds is 1, so poll
        // reads and writes exactly that element.
        let ready = unsafe { libc::poll(&mut pollfd, 1, slice_ms) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            tracing::warn!(%error, "OSC 11 probe poll failed");
            return None;
        }

        match stdin.read(&mut byte) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                tracing::warn!(%error, "OSC 11 probe read failed");
                return None;
            }
        }

        collected.push(byte[0]);
        // BEL terminates; ST is ESC \ (the ESC itself is just accumulated).
        if byte[0] == 0x07 || (byte[0] == b'\\' && collected.len() >= 2) {
            break;
        }
        if collected.len() > MAX_REPLY_BYTES {
            tracing::warn!("OSC 11 probe overshot {MAX_REPLY_BYTES} bytes; giving up");
            return None;
        }
    }

    String::from_utf8(collected)
        .inspect_err(|error| tracing::warn!(%error, "OSC 11 probe reply was not UTF-8"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_themes_meet_contrast_floor() {
        // Every foreground slot must clear WCAG AA body text (4.5:1) against the
        // background it is assumed to sit on.
        for (theme, assumed_bg) in [
            (TuiTheme::DARK, Color::Rgb(0x1e, 0x1e, 0x1e)),
            (TuiTheme::LIGHT, Color::Rgb(0xff, 0xff, 0xff)),
        ] {
            for (name, fg) in [
                ("fg", theme.fg),
                ("fg_muted", theme.fg_muted),
                ("fg_faint", theme.fg_faint),
                ("accent", theme.accent),
                ("warn", theme.warn),
                ("danger", theme.danger),
                ("success", theme.success),
            ] {
                let ratio = contrast_ratio(fg, assumed_bg);
                assert!(
                    ratio >= 4.5,
                    "{:?} {name} is only {ratio:.2}:1 on the assumed background",
                    theme.kind
                );
            }
        }
    }

    #[test]
    fn raised_surface_is_distinguishable() {
        // The input stripe must read as a distinct band, not invisible.
        assert!(contrast_ratio(TuiTheme::DARK.bg_raised, Color::Rgb(0x1e, 0x1e, 0x1e)) >= 1.15);
        assert!(contrast_ratio(TuiTheme::LIGHT.bg_raised, Color::Rgb(0xff, 0xff, 0xff)) >= 1.15);
    }

    #[test]
    fn selection_text_is_readable() {
        for theme in [TuiTheme::DARK, TuiTheme::LIGHT] {
            let ratio = contrast_ratio(theme.sel_fg, theme.sel_bg);
            assert!(ratio >= 4.5, "{:?} selection is {ratio:.2}:1", theme.kind);
        }
    }

    #[test]
    fn body_text_on_raised_surface_is_readable() {
        for theme in [TuiTheme::DARK, TuiTheme::LIGHT] {
            let ratio = contrast_ratio(theme.fg, theme.bg_raised);
            assert!(
                ratio >= 4.5,
                "{:?} body-on-raised is {ratio:.2}:1",
                theme.kind
            );
        }
    }

    #[test]
    fn of_maps_kind_to_the_matching_constant() {
        assert_eq!(TuiTheme::of(TuiThemeKind::Dark).kind, TuiThemeKind::Dark);
        assert_eq!(TuiTheme::of(TuiThemeKind::Light).kind, TuiThemeKind::Light);
        assert_eq!(TuiTheme::of(TuiThemeKind::Light).fg, TuiTheme::LIGHT.fg);
    }

    #[test]
    fn env_var_overrides_probe() {
        // Even if the terminal says dark, an explicit override wins.
        assert_eq!(
            detect_theme_kind(Some("light"), Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Light
        );
        assert_eq!(
            detect_theme_kind(Some("dark"), Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn env_var_is_trimmed_and_case_insensitive() {
        assert_eq!(
            detect_theme_kind(Some("  LIGHT  "), None),
            TuiThemeKind::Light
        );
        assert_eq!(detect_theme_kind(Some("Dark"), None), TuiThemeKind::Dark);
    }

    #[test]
    fn invalid_env_var_falls_through_to_probe() {
        // A typo must not lock the user into the wrong theme silently; fall
        // through to the terminal's own answer.
        assert_eq!(
            detect_theme_kind(Some("purple"), Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Light
        );
        assert_eq!(
            detect_theme_kind(Some("purple"), Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn osc11_reply_parsed_as_light() {
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\"),
            Some((0xff, 0xff, 0xff))
        );
        assert_eq!(
            detect_theme_kind(None, Some("rgb:ffff/ffff/ffff")),
            TuiThemeKind::Light
        );
    }

    #[test]
    fn osc11_reply_parsed_as_dark() {
        assert_eq!(
            detect_theme_kind(None, Some("rgb:1e1e/1e1e/1e1e")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn osc11_bel_terminator_is_accepted() {
        // xterm answers with BEL instead of ST depending on the version.
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:0000/0000/0000\x07"),
            Some((0x00, 0x00, 0x00))
        );
    }

    #[test]
    fn four_digit_components_are_scaled() {
        // `rgb:ffff/ffff/ffff` is 4 hex digits per channel, not 2.
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:8080/8080/8080\x1b\\"),
            Some((0x80, 0x80, 0x80))
        );
        assert_eq!(
            parse_osc11_reply(b"\x1b]11;rgb:ff/ff/ff\x1b\\"),
            Some((0xff, 0xff, 0xff))
        );
    }

    #[test]
    fn probe_timeout_falls_back_to_dark() {
        // No reply at all: startup must not hang or panic.
        assert_eq!(detect_theme_kind(None, None), TuiThemeKind::Dark);
    }

    #[test]
    fn unparseable_reply_falls_back_to_dark() {
        assert_eq!(detect_theme_kind(None, Some("garbage")), TuiThemeKind::Dark);
        assert_eq!(
            detect_theme_kind(None, Some("rgb:zz/zz/zz")),
            TuiThemeKind::Dark
        );
    }

    #[test]
    fn mid_gray_lands_on_the_dark_side() {
        // Threshold is luminance > 0.5. #808080 is ~0.216, so it must be Dark:
        // a grayish terminal should keep the historical look rather than flip
        // to a light theme with weak contrast.
        assert_eq!(
            detect_theme_kind(None, Some("rgb:8080/8080/8080")),
            TuiThemeKind::Dark
        );
    }

    /// WCAG 2.1 contrast ratio. Local (12 lines) instead of pulling a palette
    /// crate: we only need relative luminance of two sRGB colors.
    fn contrast_ratio(a: Color, b: Color) -> f32 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }
}
