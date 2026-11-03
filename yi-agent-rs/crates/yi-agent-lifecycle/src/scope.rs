//! Canonical runtime scope resolution.
//!
//! A runtime's identity is its **canonical** directory. Two spellings of one
//! directory (a symlinked project, `/var` vs `/private/var` on macOS) must resolve
//! to one scope, or they would each get their own lock and their own socket while
//! sharing one database — exactly the corruption the coordination lock exists to
//! prevent.
//!
//! Canonicalising the runtime directory itself is not always possible: on a first
//! start it does not exist yet. So resolution canonicalises the deepest existing
//! ancestor and appends the remainder, and callers re-check after creating it
//! (`ScopeExt::recheck`) because the remainder could itself be a symlink that only
//! appears later.
//!
//! This lives in `yi-agent-lifecycle` rather than in the supervisors because the
//! desktop shell must resolve the *same* path to find the guardian's control
//! socket, and it deliberately does not link the supervisors (they carry the
//! store, and therefore SQLite).

use std::io;
use std::path::{Component, Path, PathBuf};

use crate::types::RuntimeScope;

/// Resolve `runtime` and `project` into the scope they identify.
///
/// `instance_home` is recorded, not trusted: two configuration instances must
/// never silently share one runtime.
pub fn resolve(runtime: &Path, project: &Path, instance_home: &Path) -> io::Result<RuntimeScope> {
    let runtime_dir = canonicalize_allowing_missing(runtime)?;
    let project_key = canonicalize_allowing_missing(project)?;
    let instance_home = canonicalize_allowing_missing(instance_home)?;

    // The key is the canonical runtime directory. It is a string rather than a
    // hash so a diagnostic can show which directory two clients disagree about.
    let key = runtime_dir.to_string_lossy().into_owned();
    Ok(RuntimeScope {
        key,
        runtime_dir,
        project_key: project_key.to_string_lossy().into_owned(),
        instance_home,
    })
}

/// Canonicalise `path`, tolerating a missing tail.
///
/// `std::fs::canonicalize` fails on a non-existent path, which is the normal state
/// of a runtime directory before its first start. Walking up to the nearest
/// existing ancestor and re-appending the (lexically normalised) remainder gives a
/// stable answer without creating anything.
pub fn canonicalize_allowing_missing(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        normalise(path)
    } else {
        normalise(&std::env::current_dir()?.join(path))
    };

    // Fast path: the whole path exists.
    if let Ok(canonical) = std::fs::canonicalize(&absolute) {
        return Ok(canonical);
    }

    // Split into the deepest existing prefix and the missing remainder.
    let mut existing: &Path = &absolute;
    let mut remainder: Vec<&std::ffi::OsStr> = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            remainder.push(name);
        }
        // `parent()` of "/" is empty; stop there and let canonicalize("/") resolve.
        if parent.as_os_str().is_empty() {
            break;
        }
        if parent.exists() {
            let mut resolved = std::fs::canonicalize(parent)?;
            for component in remainder.iter().rev() {
                resolved.push(component);
            }
            return Ok(resolved);
        }
        existing = parent;
    }
    // Nothing along the path exists; fall back to the normalised absolute path.
    Ok(absolute)
}

/// Remove `.`/`..` lexically. Not a substitute for `canonicalize` (it cannot see
/// symlinks) but it keeps the missing-tail case from producing `a/../b`.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Scope operations that need filesystem access.
///
/// A trait rather than inherent methods keeps the wire type in [`crate::types`]
/// free of I/O.
pub trait ScopeExt {
    /// Re-resolve after the runtime directory has been created.
    fn recheck(&self) -> io::Result<RuntimeScope>;
    /// `Some(reason)` when `self` and `other` claim one runtime ambiguously.
    fn conflict_with(&self, other: &Self) -> Option<String>;
}

impl ScopeExt for RuntimeScope {
    fn recheck(&self) -> io::Result<Self> {
        let mut again = resolve(
            &self.runtime_dir,
            Path::new(&self.project_key),
            &self.instance_home,
        )?;
        // `project_key` and `instance_home` are already canonical strings, so
        // re-resolving them is a no-op unless the filesystem changed underneath.
        again.key = self.key.clone();
        Ok(again)
    }

    fn conflict_with(&self, other: &Self) -> Option<String> {
        // Only scopes that claim the same directory can conflict. Different
        // directories are different runtimes by definition, even in one project.
        if self.runtime_dir != other.runtime_dir {
            return None;
        }
        if self.project_key != other.project_key {
            return Some(format!(
                "runtime directory {} is claimed by two projects: {} and {}; \
                 point YI_AGENT_RUNTIME_DIR at a separate directory for one of them",
                self.runtime_dir.display(),
                self.project_key,
                other.project_key
            ));
        }
        if self.instance_home != other.instance_home {
            return Some(format!(
                "runtime directory {} is claimed by two instance homes: {} and {}; \
                 they would share one database under different configurations",
                self.runtime_dir.display(),
                self.instance_home.display(),
                other.instance_home.display()
            ));
        }
        None
    }
}

/// The runtime directory a project uses.
///
/// `YI_AGENT_RUNTIME_DIR` overrides it, which is what lets a development instance
/// run beside a stable one in the same checkout. Every component resolves it
/// through this one function so they cannot disagree.
pub fn runtime_directory(project: &Path) -> PathBuf {
    match std::env::var_os("YI_AGENT_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        Some(override_dir) => PathBuf::from(override_dir),
        None => project.join(".yi-agent").join("runtime"),
    }
}

/// The guardian's control socket inside a runtime directory.
pub fn control_path(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(crate::GUARDIAN_SOCKET_FILE)
}

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

    #[test]
    fn two_spellings_of_one_directory_share_one_key() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("project");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let a = resolve(&real.join(".yi-agent/runtime"), &real, dir.path()).unwrap();
        let b = resolve(&link.join(".yi-agent/runtime"), &link, dir.path()).unwrap();
        assert_eq!(a.key, b.key);
        assert_eq!(a.project_key, b.project_key);
        assert_eq!(a.conflict_with(&b), None);
    }

    #[test]
    fn one_directory_claimed_by_two_projects_is_a_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let one = resolve(&runtime, &dir.path().join("a"), dir.path()).unwrap();
        let two = resolve(&runtime, &dir.path().join("b"), dir.path()).unwrap();
        let reason = one.conflict_with(&two).expect("a conflict");
        assert!(reason.contains("two projects"), "{reason}");
    }

    #[test]
    fn different_runtime_directories_never_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let one = resolve(&dir.path().join("r1"), dir.path(), dir.path()).unwrap();
        let two = resolve(&dir.path().join("r2"), dir.path(), dir.path()).unwrap();
        assert_eq!(one.conflict_with(&two), None);
    }

    #[test]
    fn the_runtime_directory_defaults_under_the_project() {
        // Env-independent half: with no override the answer is project-local.
        let project = Path::new("/tmp/some-project");
        if std::env::var_os("YI_AGENT_RUNTIME_DIR").is_none() {
            assert_eq!(
                runtime_directory(project),
                project.join(".yi-agent").join("runtime")
            );
        }
        assert_eq!(
            control_path(Path::new("/r")),
            Path::new("/r").join(crate::GUARDIAN_SOCKET_FILE)
        );
    }
}
