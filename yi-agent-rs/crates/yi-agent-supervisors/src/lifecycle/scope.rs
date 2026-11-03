//! Canonical runtime scope.
//!
//! A runtime's identity is its **canonical** directory. Two spellings of one
//! directory (a symlinked project, `/var` vs `/private/var` on macOS) must resolve
//! to one scope, or they would each get their own lock and their own socket while
//! sharing one database — exactly the corruption the coordination lock exists to
//! prevent.
//!
//! Canonicalising the runtime directory itself is not always possible: on a first
//! start it does not exist yet. So the resolution canonicalises the deepest
//! existing ancestor and appends the remainder, and callers re-check after creating
//! it (`ScopeExt::recheck`) because the remainder could itself be a symlink that
//! only appears later.
//!
//! The implementation is shared with the desktop shell
//! (`yi_agent_lifecycle::scope`): the shell must find the same control socket and
//! cannot link this crate (it carries the store, and therefore SQLite).

use std::io;
use std::path::{Path, PathBuf};

use yi_agent_lifecycle::types::RuntimeScope;

/// Resolve `runtime` and `project` into the scope they identify.
///
/// `instance_home` is recorded, not trusted: it is part of the scope's identity
/// because two configuration instances must never silently share one runtime.
pub fn resolve(runtime: &Path, project: &Path, instance_home: &Path) -> io::Result<RuntimeScope> {
    yi_agent_lifecycle::scope::resolve(runtime, project, instance_home)
}

/// Canonicalise `path`, tolerating a missing tail.
///
/// Thin re-export of the shared implementation: the desktop shell resolves scopes
/// too, and two spellings of this rule would give one directory two sockets.
pub fn canonicalize_allowing_missing(path: &Path) -> io::Result<PathBuf> {
    yi_agent_lifecycle::scope::canonicalize_allowing_missing(path)
}

/// Scope operations that need filesystem access. Kept as a trait so the wire type
/// in `yi-agent-lifecycle` stays dependency-free.
///
/// This is the shared trait, re-exported so existing importers keep working; the
/// single implementation lives in `yi-agent-lifecycle`.
pub use yi_agent_lifecycle::scope::ScopeExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_made_absolute_before_canonicalising() {
        let cwd = std::env::current_dir().unwrap();
        let resolved = canonicalize_allowing_missing(Path::new("some/missing/dir")).unwrap();
        assert!(resolved.is_absolute(), "{resolved:?}");
        assert!(
            resolved.starts_with(&cwd)
                || resolved.starts_with(std::fs::canonicalize(&cwd).unwrap())
        );
        assert!(resolved.ends_with("some/missing/dir"), "{resolved:?}");
    }

    #[test]
    fn dot_components_are_removed_from_a_missing_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/./b/../c");
        let resolved = canonicalize_allowing_missing(&path).unwrap();
        assert!(resolved.ends_with("a/c"), "{resolved:?}");
    }

    #[test]
    fn an_existing_path_canonicalises_through_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(
            canonicalize_allowing_missing(&link).unwrap(),
            std::fs::canonicalize(&real).unwrap()
        );
    }
}
