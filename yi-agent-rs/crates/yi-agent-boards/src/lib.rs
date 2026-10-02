//! Project-level Superpowers kanban boards: the global registry of which
//! projects own a board, plus the per-project scaffolding that makes a board
//! discoverable and switchable.
//!
//! A leaf crate: it depends on nothing else in the workspace, so the registry
//! can be read by the desktop app, the app-server, and the TUI alike.

use std::ffi::OsStr;
use std::path::PathBuf;

pub mod board_daemon;
pub mod lifecycle;
pub mod registry;
pub mod scaffold;

/// Directory holding the global board registry and the shared lease slots:
/// `$HOME/.yi-agent/superpowers-kanban`.
///
/// Errors when `HOME` is unset rather than guessing a fallback: writing a
/// registry into an unrelated directory would be worse than failing loudly.
pub fn global_dir() -> std::io::Result<PathBuf> {
    global_dir_from(std::env::var_os("HOME").as_deref())
}

fn global_dir_from(home: Option<&OsStr>) -> std::io::Result<PathBuf> {
    let home = home.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot locate the board registry: HOME is not set",
        )
    })?;
    Ok(PathBuf::from(home)
        .join(".yi-agent")
        .join("superpowers-kanban"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn the_global_dir_is_under_home() {
        let dir = global_dir_from(Some(OsStr::new("/home/tester"))).unwrap();
        assert_eq!(dir, PathBuf::from("/home/tester/.yi-agent/superpowers-kanban"));
    }

    #[test]
    fn a_missing_home_is_an_error_not_a_fallback() {
        let error = global_dir_from(None).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(error.to_string().contains("HOME"), "{error}");
    }

    #[test]
    fn the_default_global_dir_lands_under_the_real_home() {
        // HOME is set in every environment that runs these tests; if it is not,
        // the function must fail rather than invent a directory.
        let dir = global_dir().unwrap();
        assert!(dir.is_absolute(), "{dir:?}");
        assert!(dir.ends_with(".yi-agent/superpowers-kanban"), "{dir:?}");
    }
}
