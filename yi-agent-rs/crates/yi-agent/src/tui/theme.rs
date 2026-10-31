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

    /// WCAG 2.1 contrast ratio. Local (12 lines) instead of pulling a palette
    /// crate: we only need relative luminance of two sRGB colors.
    fn contrast_ratio(a: Color, b: Color) -> f32 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }
}
