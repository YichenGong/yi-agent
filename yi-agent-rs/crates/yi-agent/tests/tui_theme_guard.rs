//! Static guard: only `tui/theme.rs` may name raw values that do not adapt to
//! the terminal background.
//!
//! This is the TUI-side counterpart of `desktop/src/lib/themeTokens.test.ts`,
//! which caught the same class of bug (literal color scales pinned to one
//! theme) on the desktop side.
//!
//! ## Why the cut is not "the first `#[cfg(test)]`"
//!
//! An earlier draft cut the source at the first line containing `#[cfg(test)]`.
//! That is silently broken in this tree, and it failed *open* — the guard would
//! have passed forever:
//!
//! - `app.rs:88` is `#[cfg(test)] fn from_senders(..)`, a **production helper**
//!   inside `impl TuiChannels`, not a test module. Cutting there truncates
//!   `app.rs` at line 88, so `run_tui`, every renderer and every call site after
//!   it stops being scanned. A bait color inserted into `run_loop` would be
//!   invisible to the guard.
//! - `history.rs:1450` is `#[cfg(test)] mod perf_counters`, a helper module
//!   whose counters are read from production code (under `#[cfg(test)]`) inside
//!   `ensure_cache`. Only `mod tests` at `:1515` starts the test corpus.
//!
//! So the cut looks for the first `#[cfg(test)]` whose following non-blank,
//! non-attribute, non-comment line opens a `mod <name containing "test">`.
//! [`the_cut_only_skips_test_modules`] pins that in: it asserts nothing but test
//! modules follows a cut, so a future edit that puts production code after a
//! test module fails here instead of quietly escaping the color scan.

use std::path::{Path, PathBuf};

const BANNED: &[&str] = &["Color::Gray", "Color::DarkGray", "Color::Indexed("];

fn tui_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui")
}

/// Byte offset of the first `#[cfg(test)]` that guards a **test module**, or
/// `None` when the file has no test module (it is production throughout).
fn test_module_cut(text: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.contains("#[cfg(test)]") && opens_test_module(&text[offset + line.len()..]) {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// Whether the text after a `#[cfg(test)]` attribute starts a test module.
///
/// Attribute lines are skipped so `#[cfg(test)]` + `#[allow(dead_code)]` +
/// `mod tests` still counts as a test module.
fn opens_test_module(rest: &str) -> bool {
    let declaration = rest
        .lines()
        .map(str::trim)
        .find(|line| !(line.is_empty() || line.starts_with("#[") || line.starts_with("//")));
    declaration
        .and_then(|line| line.strip_prefix("mod "))
        .map(|name| name.contains("test"))
        .unwrap_or(false)
}

/// The source with the test modules removed.
///
/// Test assertions may legitimately name raw colors (they assert the *old*
/// value when locking in a behavior), so only production code is scanned.
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).expect("source readable");
    match test_module_cut(&text) {
        Some(cut) => text[..cut].to_string(),
        None => text,
    }
}

/// Every `.rs` file in `src/tui`, paired with its path.
fn tui_sources() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(tui_dir())
        .expect("tui dir")
        .map(|entry| entry.expect("dir entry").path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("rs"))
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "no sources found under {}",
        tui_dir().display()
    );
    paths
}

#[test]
fn no_raw_colors_outside_theme_module() {
    let mut violations: Vec<String> = Vec::new();
    for path in tui_sources() {
        if path.file_name().and_then(|n| n.to_str()) == Some("theme.rs") {
            continue; // the one place raw values are allowed
        }
        let source = production_source(&path);
        for (number, line) in source.lines().enumerate() {
            // An explicit opt-out, so a legitimate exception is greppable.
            if line.contains("theme-guard: allow") {
                continue;
            }
            for banned in BANNED {
                if line.contains(banned) {
                    violations.push(format!("{}:{}: {banned}", path.display(), number + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "raw terminal-dependent colors must move into tui/theme.rs:\n{}",
        violations.join("\n")
    );
}

/// The guard only holds if the cut discards tests and nothing else.
///
/// A cut placed at a `#[cfg(test)]` *production helper* would both truncate the
/// scanned region and pass vacuously, which is exactly the bug the previous
/// draft of this file shipped with. Top-level declarations after a cut must all
/// be test modules.
#[test]
fn the_cut_only_skips_test_modules() {
    for path in tui_sources() {
        let text = std::fs::read_to_string(&path).expect("source readable");
        let Some(cut) = test_module_cut(&text) else {
            continue;
        };
        let mut offenders: Vec<String> = Vec::new();
        for (number, line) in text[cut..].lines().enumerate() {
            // Rustfmt indents everything inside a module, so a column-0 keyword
            // is a top-level item of the file.
            let is_declaration = [
                "fn ",
                "pub fn ",
                "pub(crate) fn ",
                "struct ",
                "pub struct ",
                "enum ",
                "pub enum ",
                "impl ",
                "impl<",
                "trait ",
                "pub trait ",
                "const ",
                "pub const ",
                "static ",
                "pub static ",
                "macro_rules! ",
                "use ",
                "pub use ",
                "mod ",
                "pub mod ",
            ]
            .iter()
            .any(|keyword| line.starts_with(keyword));
            if !is_declaration {
                continue;
            }
            let is_test_module = line
                .strip_prefix("mod ")
                .or_else(|| line.strip_prefix("pub mod "))
                .map(|name| name.contains("test"))
                .unwrap_or(false);
            if !is_test_module {
                offenders.push(format!(
                    "{}:{}: {}",
                    path.display(),
                    cut + number + 1,
                    line.trim_end()
                ));
            }
        }
        assert!(
            offenders.is_empty(),
            "production code follows the `#[cfg(test)]` cut, so it escapes the \
             color scan; move the cut later:\n{}",
            offenders.join("\n")
        );
    }
}
