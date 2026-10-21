//! Main-window sizing for the desktop app.
//!
//! `tauri.conf.json` carries the default size, but a default tuned for a laptop
//! is wrong on a 3440x1440 ultrawide (too small) and a default tuned for a
//! desktop is wrong on a 1280x720 panel (does not fit). This module clamps the
//! configured size to the monitor's work area once, at startup, so the window
//! always opens usable and centred.

/// Default opening size in logical pixels. Mirrors `tauri.conf.json`.
pub const DESIRED: (f64, f64) = (1280.0, 820.0);

/// Minimum size in logical pixels. Mirrors `tauri.conf.json`.
pub const MIN: (f64, f64) = (900.0, 600.0);

/// Largest fraction of the monitor work area the window may occupy.
const MAX_FRACTION: f64 = 0.9;

/// Logical-pixel geometry for the main window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
}

pub fn fit_to_work_area(
    desired: (f64, f64),
    min_size: (f64, f64),
    work_area: (f64, f64),
    scale: f64,
) -> Fit {
    // An unusable scale factor means we cannot compare physical pixels to the
    // logical ones the window API takes; keep the configured size rather than
    // guessing (a wrong guess would open the window off-screen).
    if !scale.is_finite() || scale <= 0.0 {
        return Fit {
            width: desired.0,
            height: desired.1,
            min_width: min_size.0,
            min_height: min_size.1,
        };
    }

    let work_w = work_area.0 / scale;
    let work_h = work_area.1 / scale;

    // A work area smaller than the minimum relaxes the minimum: opening a
    // window wider than the screen would be worse than a cramped one.
    let min_w = min_size.0.min(work_w);
    let min_h = min_size.1.min(work_h);

    Fit {
        // `clamp` order matters: cap by the work area first, then lift to the
        // minimum, so a tiny screen yields the minimum rather than a negative
        // or inverted range.
        width: desired.0.min(work_w * MAX_FRACTION).max(min_w),
        height: desired.1.min(work_h * MAX_FRACTION).max(min_h),
        min_width: min_w,
        min_height: min_h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare geometry with a tolerance: `work * 0.9` is exact for today's
    /// test inputs, but the tests should not depend on that.
    #[track_caller]
    fn assert_fit(actual: Fit, width: f64, height: f64, min_width: f64, min_height: f64) {
        const EPS: f64 = 1e-6;
        let expected = Fit {
            width,
            height,
            min_width,
            min_height,
        };
        let close = (actual.width - expected.width).abs() < EPS
            && (actual.height - expected.height).abs() < EPS
            && (actual.min_width - expected.min_width).abs() < EPS
            && (actual.min_height - expected.min_height).abs() < EPS;
        assert!(close, "expected {expected:?}, got {actual:?}");
    }

    #[test]
    fn keeps_the_default_size_on_a_large_display() {
        let fit = fit_to_work_area(DESIRED, MIN, (3440.0, 1410.0), 1.0);
        assert_fit(fit, 1280.0, 820.0, 900.0, 600.0);
    }

    #[test]
    fn converts_physical_work_area_with_the_scale_factor() {
        // 2880x1800 physical at scale 2 is 1440x900 logical: the height cap
        // binds (900 * 0.9 = 810) while the width fits (1280 <= 1296).
        let fit = fit_to_work_area(DESIRED, MIN, (2880.0, 1800.0), 2.0);
        assert_fit(fit, 1280.0, 810.0, 900.0, 600.0);
    }

    #[test]
    fn shrinks_to_ninety_percent_of_a_small_work_area() {
        let fit = fit_to_work_area(DESIRED, MIN, (1280.0, 720.0), 1.0);
        assert_fit(fit, 1152.0, 648.0, 900.0, 600.0);
    }

    #[test]
    fn never_opens_below_the_minimum_even_on_a_tiny_work_area() {
        // A work area smaller than the minimum relaxes the minimum to the work
        // area itself, so the window is never larger than the screen.
        let fit = fit_to_work_area(DESIRED, MIN, (800.0, 600.0), 1.0);
        assert_fit(fit, 800.0, 600.0, 800.0, 600.0);
    }

    #[test]
    fn falls_back_to_the_configured_size_without_a_usable_scale() {
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let fit = fit_to_work_area(DESIRED, MIN, (1280.0, 720.0), scale);
            assert_fit(fit, 1280.0, 820.0, 900.0, 600.0);
        }
    }
}
