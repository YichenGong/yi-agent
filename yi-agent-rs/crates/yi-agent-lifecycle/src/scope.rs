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

use sha2::{Digest, Sha256};

use crate::types::RuntimeScope;

/// The longest socket path `sockaddr_un.sun_path` accepts, excluding the
/// terminating NUL. Linux allows 108; macOS and the BSDs allow 104. Taking the
/// smaller bound keeps one behaviour across platforms.
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

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
///
/// This is the **only** place the path is computed. Every component — the guardian
/// that binds it, the frontends that lease from it, the desktop shell that reads
/// status through it — must resolve it here, because a disagreement means one side
/// listens where nobody connects.
///
/// The raw join is not always bindable: `sockaddr_un.sun_path` is capped (104 bytes
/// on macOS/BSD, 108 on Linux) while the default runtime directory is
/// `<workdir>/.yi-agent/runtime`, so a checkout deeper than about 85 bytes produces
/// a path `bind` rejects with `AF_UNIX path too long`. That failure used to be
/// invisible in the worst way: the guardian is spawned with its stderr discarded,
/// so it exited immediately and every caller saw only "the managed runtime did not
/// become ready" after waiting out the full 30-second startup budget — a stall on
/// session start followed by delegation being switched off for that session.
///
/// So an over-long path falls back to a short, stable name under the platform
/// temporary directory, derived from the runtime directory so guardian and clients
/// independently compute the same location. Distinct projects keep distinct
/// sockets; an explicitly shared runtime directory keeps being shared.
pub fn control_path(runtime_dir: &Path) -> PathBuf {
    let direct = runtime_dir.join(crate::GUARDIAN_SOCKET_FILE);
    if direct.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return direct;
    }
    // Hashed over the *canonical* directory, not the spelling this caller happens to
    // hold. The launcher and the guardian do not always name the same directory the
    // same way -- macOS's `/var` and `/private/var` are one directory, and the TUI's
    // runtime directory can reach the launcher before anyone canonicalises it. Two
    // spellings would otherwise produce two names, the guardian would bind one and
    // every client would poll the other, and the symptom would be the very timeout
    // this function exists to remove.
    let canonical =
        canonicalize_allowing_missing(runtime_dir).unwrap_or_else(|_| runtime_dir.into());
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    std::env::temp_dir().join(format!("guardian-{}.sock", &hex[..16]))
}

/// [`control_path`], but refusing when even the fallback cannot be bound.
///
/// A gateway this deep (`$TMPDIR` itself near the `sun_path` cap) is pathological,
/// yet silently returning a path `bind` will reject is exactly the failure mode this
/// module exists to end. Callers that bind — in practice the guardian — use this so
/// the reason is reported instead of inferred from a timeout.
pub fn control_socket(runtime_dir: &Path) -> Result<PathBuf, SocketPathTooLong> {
    let path = control_path(runtime_dir);
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: path.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(path)
}

/// A socket path that is still too long after falling back to the temp directory.
///
/// Reachable only when `TMPDIR` itself is absurdly deep. It is a distinct type
/// rather than a bare `io::Error` so a caller can report the two numbers that
/// explain it instead of a bind failure's unhelpful "path too long".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    pub path: String,
    pub limit: usize,
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the guardian control socket is {} bytes even after falling back to the temp \
             directory, over the {}-byte sun_path limit: {}",
            self.path.len(),
            self.limit,
            self.path
        )
    }
}

impl std::error::Error for SocketPathTooLong {}

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

    /// The real checkout that produced the silent failure in practice. Its direct
    /// control socket is 121 bytes, so `bind` refuses it on macOS (104-byte cap).
    const DEEP_RUNTIME_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/runtime";

    #[test]
    fn a_short_runtime_directory_keeps_the_control_socket_inside_it() {
        let runtime_dir = Path::new("/tmp/project/.yi-agent/runtime");

        let socket = control_path(runtime_dir);

        assert_eq!(
            socket,
            runtime_dir.join(crate::GUARDIAN_SOCKET_FILE),
            "a runtime directory that fits must keep its existing layout"
        );
    }

    #[test]
    fn a_deep_runtime_directory_yields_a_control_socket_that_fits_the_limit() {
        let runtime_dir = Path::new(DEEP_RUNTIME_DIR);
        assert!(
            runtime_dir
                .join(crate::GUARDIAN_SOCKET_FILE)
                .as_os_str()
                .len()
                > MAX_SOCKET_PATH_BYTES,
            "precondition: the direct control socket must overflow the limit"
        );

        let socket = control_path(runtime_dir);

        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "socket must fit sockaddr_un.sun_path, got {} bytes: {}",
            socket.as_os_str().len(),
            socket.display()
        );
    }

    /// The regression itself, stated as a fact rather than as a comment: the direct
    /// path really is unbindable, so the fallback is what stands between a user and
    /// a 30-second stall followed by delegation being switched off.
    #[test]
    fn the_direct_control_path_for_a_deep_checkout_cannot_be_bound() {
        let direct = Path::new(DEEP_RUNTIME_DIR).join(crate::GUARDIAN_SOCKET_FILE);

        let bound = std::os::unix::net::UnixListener::bind(&direct);

        // Either the OS refuses the length outright, or (on a platform with a larger
        // cap) the directory does not exist. Both mean "not usable as-is"; the length
        // check is what makes the first case impossible to reach in production.
        assert!(
            bound.is_err(),
            "precondition: binding {direct:?} must fail so the fallback is meaningful"
        );
    }

    /// The resolved path must really be bindable, not merely short: this is the whole
    /// point of the fallback, and a test that only measured bytes could pass while
    /// `bind` still failed.
    #[test]
    fn the_resolved_control_socket_for_a_deep_checkout_can_be_bound() {
        let socket = control_socket(Path::new(DEEP_RUNTIME_DIR)).expect("resolves");

        let listener = std::os::unix::net::UnixListener::bind(&socket);
        assert!(
            listener.is_ok(),
            "the resolved socket must bind for real, got {:?} on {}",
            listener.err(),
            socket.display()
        );
        let _ = std::fs::remove_file(&socket);
    }

    #[test]
    fn the_control_socket_fallback_is_deterministic_and_per_runtime_directory() {
        let deep_one = Path::new(DEEP_RUNTIME_DIR);
        let deep_two = Path::new(
            "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/other-repo/.yi-agent/runtime",
        );

        let first = control_path(deep_one);
        let second = control_path(deep_one);
        let other = control_path(deep_two);

        assert_eq!(first, second, "one runtime directory must mean one socket");
        assert_ne!(
            first, other,
            "distinct projects must not share a guardian socket"
        );
        assert!(
            !first.starts_with(deep_one),
            "the fallback must leave the overflowing directory"
        );
    }

    /// The launcher and the guardian must agree *even when they spell the directory
    /// differently*, and they really do: macOS's `/var` and `/private/var` are one
    /// directory, and a runtime directory can reach the launcher before anything has
    /// canonicalised it while the guardian resolves its scope from a canonical path.
    ///
    /// Hashing the spelling rather than the directory would give two names, the
    /// guardian would bind one and every client would poll the other — which is
    /// indistinguishable from the timeout this module exists to remove.
    #[test]
    fn two_spellings_of_one_deep_runtime_directory_share_one_control_socket() {
        let directory = tempfile::tempdir().unwrap();
        // Deep enough that the fallback is taken, so the name is actually hashed.
        let deep = directory.path().join("d".repeat(90)).join("runtime");
        std::fs::create_dir_all(&deep).unwrap();
        let canonical = std::fs::canonicalize(&deep).unwrap();
        assert!(
            deep.as_os_str().len() > MAX_SOCKET_PATH_BYTES,
            "precondition: the fallback must be in play, got {} bytes: {}",
            deep.as_os_str().len(),
            deep.display()
        );

        // The two spellings differ only if the platform has a symlinked root, which is
        // exactly the macOS case; skip the assertion where they are identical.
        if deep == canonical {
            return;
        }

        assert_eq!(
            control_path(&deep),
            control_path(&canonical),
            "one directory must mean one socket no matter how it is spelled"
        );
    }
}
