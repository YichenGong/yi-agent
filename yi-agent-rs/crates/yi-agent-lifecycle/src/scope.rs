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

/// The daemon's socket file name inside a socket directory.
pub const DAEMON_SOCKET_FILE: &str = "runtime.sock";

/// The directory holding both sockets for one runtime.
///
/// Always `<instance_home>/sockets/<16 hex>/`, so the path length depends only on
/// where the instance lives — never on how deep the project is. That is the whole
/// point: `sun_path` is 103 bytes here, and a deep checkout used to overflow it.
///
/// Both inputs are canonicalised because a hash over the raw spelling would make
/// one directory produce two sockets: macOS's `/var` and `/private/var` are the
/// same directory, and the launcher and the guardian do not always name it the
/// same way. Two spellings, two socket directories, guardian binds one and every
/// client polls the other — the exact timeout this module exists to remove.
pub fn socket_directory(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    canonical_or_self(instance_home)
        .join("sockets")
        .join(digest16(runtime_dir))
}

/// The guardian's control socket under the shared socket directory.
pub fn guardian_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    socket_directory(runtime_dir, instance_home).join(crate::GUARDIAN_SOCKET_FILE)
}

/// The business daemon's socket under the shared socket directory.
pub fn daemon_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    socket_directory(runtime_dir, instance_home).join(DAEMON_SOCKET_FILE)
}

/// Sixteen hex characters of the canonical path's SHA-256.
fn digest16(path: &Path) -> String {
    let digest = Sha256::digest(canonical_or_self(path).as_os_str().as_encoded_bytes());
    digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Canonical when possible; the original path otherwise (the directory may not
/// exist yet on a first start, which is not an error here).
fn canonical_or_self(path: &Path) -> PathBuf {
    canonicalize_allowing_missing(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The instance home a socket path should be derived from.
///
/// Priority: `YI_AGENT_INSTANCE_HOME` (a process that was told which instance it
/// serves), then `YI_AGENT_HOME`, then `$HOME/.yi-agent`.
///
/// Why not just `YI_AGENT_HOME`: a guardian spawns its daemon with
/// `YI_AGENT_INSTANCE_HOME` set explicitly and `YI_AGENT_HOME` only inherited. If
/// the two ever disagreed, reading only `YI_AGENT_HOME` would put the daemon's
/// socket somewhere the frontend never looks.
pub fn instance_home_for_sockets() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YI_AGENT_INSTANCE_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty());
    instance_home_from_env(override_value.as_deref(), home.as_deref())
}

/// `YI_AGENT_HOME` / `$HOME` -> instance home, including the `.yi-agent` suffix
/// normalisation.
///
/// This must stay semantically identical to `store::paths::instance_home()`
/// (`paths.rs:20`). It is duplicated here because `lifecycle` cannot depend on
/// `store` (the dependency runs the other way). A test in `store` pins the two
/// together, which is why this is `#[doc(hidden)] pub` rather than private.
#[doc(hidden)]
pub fn instance_home_from_env(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_value {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return Some(
                if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
                    dir
                } else {
                    dir.join(".yi-agent")
                },
            );
        }
    }
    home.map(|home| home.join(".yi-agent"))
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

/// The guardian's control socket for a runtime directory.
///
/// This is the **only** place the path is computed. Every component — the guardian
/// that binds it, the frontends that lease from it, the desktop shell that reads
/// status through it — must resolve it here, because a disagreement means one side
/// listens where nobody connects.
///
/// The path is `<instance_home>/sockets/<hash>/guardian-control.sock`, so its length
/// depends on where the instance lives and never on how deep the project is. The
/// previous layout put it beside the runtime directory, which overflowed
/// `sun_path` (103 bytes here) on a deep checkout; the guardian's stderr is
/// discarded, so that failure surfaced only as a 30-second stall followed by
/// delegation being switched off.
///
/// Callers that do not know the instance home (most of them) get it from
/// [`instance_home_for_sockets`]. Callers that *do* know it — the guardian, which
/// carries a `RuntimeScope` — should call [`guardian_socket`] directly instead.
pub fn control_path(runtime_dir: &Path) -> PathBuf {
    let home = instance_home_for_sockets().unwrap_or_else(|| {
        // Diagnostic placeholder only. All three sources missing is unreachable on a
        // normal host (launchd, containers and SSH all set HOME). A path that cannot
        // exist fails `bind` loudly instead of quietly landing somewhere unexpected.
        //
        // `eprintln!` rather than `tracing`: `lifecycle` deliberately carries no
        // logging stack (the desktop shell links it and it must stay light).
        eprintln!(
            "yi-agent: instance home is unknown (no YI_AGENT_INSTANCE_HOME, \
             YI_AGENT_HOME or HOME); the socket path is a placeholder"
        );
        PathBuf::from("<unknown-instance-home>")
    });
    guardian_socket(runtime_dir, &home)
}

/// [`control_path`], but refusing when the derived path cannot be bound.
///
/// Callers that bind — in practice the guardian — use this so an over-long instance
/// home is reported instead of inferred from a timeout.
pub fn control_socket(runtime_dir: &Path) -> Result<PathBuf, SocketPathTooLong> {
    reject_if_too_long(control_path(runtime_dir))
}

fn reject_if_too_long(path: PathBuf) -> Result<PathBuf, SocketPathTooLong> {
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: path.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(path)
}

/// A socket path longer than `sockaddr_un.sun_path` accepts.
///
/// Reachable only when the instance home itself is very deep — a project's depth
/// no longer contributes, so this reports the one input a user can actually change.
/// It is a distinct type rather than a bare `io::Error` so a caller can report the
/// two numbers that explain it instead of a bind failure's unhelpful "path too
/// long".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    pub path: String,
    pub limit: usize,
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the guardian control socket would be {} bytes, over the {}-byte sun_path \
             limit: {}",
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

    fn control_path_with_home(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
        guardian_socket(runtime_dir, instance_home)
    }

    #[test]
    fn socket_directory_is_independent_of_runtime_dir_depth() {
        let home = Path::new("/home/alice/.yi-agent");
        let short = Path::new("/tmp/p/.yi-agent/runtime");
        let deep = PathBuf::from(format!("/tmp/{}", "x".repeat(600)));

        let one = socket_directory(short, home);
        let two = socket_directory(&deep, home);

        assert_eq!(
            one.parent(),
            two.parent(),
            "both hashes live under the same sockets/ directory"
        );
        assert_ne!(one, two, "different runtimes must not collide");
    }

    #[test]
    fn guardian_and_daemon_sockets_share_one_directory() {
        let runtime = Path::new("/tmp/p/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        let dir = socket_directory(runtime, home);

        assert_eq!(
            guardian_socket(runtime, home),
            dir.join(crate::GUARDIAN_SOCKET_FILE)
        );
        assert_eq!(daemon_socket(runtime, home), dir.join(DAEMON_SOCKET_FILE));
    }

    #[test]
    fn the_hash_is_over_the_canonical_runtime_directory() {
        // Two spellings of one directory must hash the same, or the guardian binds
        // one socket and every client polls another. A `TempDir` lands under
        // `/var/folders/...`, whose canonical form is `/private/var/folders/...`.
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let runtime = real.join("project/.yi-agent/runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let alias = match real.to_str().and_then(|s| s.strip_prefix("/private")) {
            Some(rest) => PathBuf::from(rest),
            None => real.clone(),
        };
        let alias_runtime = alias.join("project/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        assert_eq!(
            socket_directory(&runtime, home),
            socket_directory(&alias_runtime, home),
            "both spellings of one directory must hash to one socket directory"
        );
    }

    #[test]
    fn a_bare_yi_agent_home_gains_the_dot_yi_agent_suffix() {
        // 必须与 `store::paths::instance_home()` 同义：`/foo` -> `/foo/.yi-agent`。
        assert_eq!(
            instance_home_from_env(
                Some(std::ffi::OsStr::new("/tmp/dev-a")),
                Some(Path::new("/home/me"))
            ),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_instance_home_already_ending_in_dot_yi_agent_is_kept_verbatim() {
        assert_eq!(
            instance_home_from_env(Some(std::ffi::OsStr::new("/tmp/dev-a/.yi-agent")), None),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn no_override_and_no_home_yields_none() {
        assert_eq!(instance_home_from_env(None, None), None);
    }

    #[test]
    fn an_empty_override_falls_through_to_home() {
        assert_eq!(
            instance_home_from_env(Some(std::ffi::OsStr::new("")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.yi-agent"))
        );
    }

    #[test]
    fn a_deep_instance_home_produces_a_path_that_still_fits() {
        let home = Path::new("/home/alice/.yi-agent");

        for len in [200usize, 500, 900] {
            let deep = PathBuf::from(format!("/work/{}", "d".repeat(len)));
            let socket = guardian_socket(&deep, home);
            assert!(
                socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
                "guardian socket must fit at workdir len {len}: {} bytes",
                socket.as_os_str().len()
            );
        }
    }

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
        // The control socket no longer follows the runtime directory: it lives under
        // the instance home, so its shape is asserted in
        // `the_control_socket_lives_in_the_shared_socket_directory` instead.
    }

    /// The real checkout that produced the silent failure in practice. Its direct
    /// control socket is 121 bytes, so `bind` refuses it on macOS (104-byte cap).
    const DEEP_RUNTIME_DIR: &str = "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.yi-agent/runtime";

    #[test]
    fn the_control_socket_is_actually_bindable() {
        // A `tempfile::TempDir` is unusable here: on macOS it resolves to
        // `/private/var/folders/<...>/T/.tmpXXXXXX` (68 bytes), and the 47-byte
        // socket tail takes that past the 103-byte cap. `/tmp` (68 bytes after
        // canonicalisation) leaves room -- which is itself a useful reminder that
        // the socket's length budget is real, not theoretical.
        let home = std::path::PathBuf::from(format!("/tmp/ya-bind-probe-{}", std::process::id()));
        let runtime = home.join("project/.yi-agent/runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let socket = control_path_with_home(&runtime, &home);
        assert!(
            socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
            "precondition: probe path must fit, got {} bytes: {}",
            socket.as_os_str().len(),
            socket.display()
        );
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .unwrap_or_else(|error| panic!("binding {}: {error}", socket.display()));
        drop(listener);
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn an_over_long_instance_home_is_refused_not_redirected() {
        let runtime = Path::new("/tmp/project/.yi-agent/runtime");
        let home = PathBuf::from(format!("/{}", "h".repeat(80)));

        let refused = reject_if_too_long(guardian_socket(runtime, &home));

        assert!(
            refused.is_err(),
            "a path that cannot fit must be an error, not a silent move to $TMPDIR"
        );
        let message = refused.unwrap_err().to_string();
        assert!(
            !message.contains("falling back"),
            "stale wording: {message}"
        );
        assert!(!message.contains("TMPDIR"), "stale wording: {message}");
        assert!(message.contains("103"), "must state the limit: {message}");
    }

    #[test]
    fn the_control_socket_lives_in_the_shared_socket_directory() {
        let runtime = Path::new("/tmp/project/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        let socket = control_path_with_home(runtime, home);

        assert_eq!(socket, guardian_socket(runtime, home));
        assert_eq!(
            socket.file_name().unwrap(),
            crate::GUARDIAN_SOCKET_FILE,
            "the file name is unchanged"
        );
        assert_eq!(
            socket
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            "sockets",
            "must sit under a sockets/ directory: {}",
            socket.display()
        );
        assert!(
            !socket.starts_with(runtime),
            "must not follow the project directory any more: {}",
            socket.display()
        );
    }

    #[test]
    fn a_deep_runtime_directory_does_not_lengthen_the_control_socket() {
        let home = Path::new("/home/alice/.yi-agent");
        let shallow = control_path_with_home(Path::new("/tmp/p"), home);
        let deep_path = PathBuf::from(format!("/tmp/{}", "x".repeat(600)));
        let deep = control_path_with_home(&deep_path, home);

        assert_eq!(
            shallow.as_os_str().len(),
            deep.as_os_str().len(),
            "length must depend only on instance_home, never on project depth"
        );
        assert!(deep.as_os_str().len() <= MAX_SOCKET_PATH_BYTES);
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
        // Uses an explicit instance home under /tmp: a test must never bind inside
        // the developer's real `~/.yi-agent`, and `tempfile::TempDir` resolves to a
        // path too long for the socket tail (see `the_control_socket_is_actually_bindable`).
        let home = std::path::PathBuf::from(format!("/tmp/ya-deep-probe-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let runtime = Path::new(DEEP_RUNTIME_DIR);

        let socket = guardian_socket(runtime, &home);
        let socket_dir = socket.parent().unwrap();
        std::fs::create_dir_all(socket_dir).unwrap();

        let listener = std::os::unix::net::UnixListener::bind(&socket);
        assert!(
            listener.is_ok(),
            "the resolved socket must bind for real, got {:?} on {}",
            listener.err(),
            socket.display()
        );
        let _ = std::fs::remove_file(&socket);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_control_socket_is_deterministic_and_per_runtime_directory() {
        let home = Path::new("/home/alice/.yi-agent");
        let one = Path::new(DEEP_RUNTIME_DIR);
        let two = Path::new(
            "/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/other-repo/.yi-agent/runtime",
        );

        let first = guardian_socket(one, home);
        let second = guardian_socket(one, home);
        let other = guardian_socket(two, home);

        assert_eq!(first, second, "one runtime directory must mean one socket");
        assert_ne!(
            first, other,
            "distinct projects must not share a guardian socket"
        );
        assert!(
            !first.starts_with(one),
            "the socket must leave the project directory entirely"
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
    fn two_spellings_of_one_runtime_directory_share_one_control_socket() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let runtime = real.join("d".repeat(90)).join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let alias = match real.to_str().and_then(|s| s.strip_prefix("/private")) {
            Some(rest) => PathBuf::from(rest),
            None => real.clone(),
        };
        let alias_runtime = alias.join("d".repeat(90)).join("runtime");

        // The two spellings differ only where the platform has a symlinked root,
        // which is exactly the macOS case; skip where they are identical.
        if runtime == alias_runtime {
            return;
        }

        let home = Path::new("/home/alice/.yi-agent");
        assert_eq!(
            guardian_socket(&runtime, home),
            guardian_socket(&alias_runtime, home),
            "one directory must mean one socket no matter how it is spelled"
        );
    }
}
