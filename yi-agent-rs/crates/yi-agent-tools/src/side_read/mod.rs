//! Revocable, handle-based read access for a side conversation.
//!
//! A side conversation inherits the parent's transcript and may need to read the
//! files it mentions. It must **not** inherit a general filesystem capability:
//! the parent's root set can be narrowed or revoked while the side is alive, and
//! a plain "check the path, then open the path" sequence loses that race (spec
//! §5.4, §7.2).
//!
//! Two rules make this safe:
//!
//! 1. Authorisation is re-derived on **every** open: the roots are read from
//!    [`SideRoots`] again, intersected with the roots captured when the side was
//!    created, and the target must be inside that intersection.
//! 2. The path is walked component by component with `openat(..., O_NOFOLLOW)`
//!    from a directory handle, so the decision is made about the very object
//!    that is opened. A directory swapped for a symlink after the check cannot
//!    redirect the open.
//!
//! Symlinks are refused outright in this first version rather than resolved.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

pub mod blocking;
#[cfg(test)]
mod blocking_tests;
pub mod file_tools;
#[cfg(test)]
mod file_tools_tests;

#[cfg(unix)]
mod unix;

/// One authorised root, in both forms.
///
/// `canonical` is what the handle walk is anchored to; `given` is how the parent
/// spelled it. macOS makes this unavoidable: `/var` is a symlink to
/// `/private/var`, so a path handed in as `/var/...` is lexically unrelated to
/// the canonical root even though it is the same directory. Matching on either
/// form and always opening beneath `canonical` keeps the decision about the
/// exact directory that is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AuthorizedRoot {
    given: PathBuf,
    canonical: PathBuf,
}

/// The live root set of the parent session.
///
/// Read on every open so a revoked or narrowed root takes effect immediately.
pub use blocking::SideBlockingPool;
pub use file_tools::{SideFileToolContext, register_side_file_tools};

pub trait SideRoots: Send + Sync {
    fn current_roots(&self) -> Vec<PathBuf>;
}

/// A pre-copied attachment: the authorised original and the side-private copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideFileAlias {
    pub original: PathBuf,
    pub copy: PathBuf,
}

/// Whether this build can enforce the side read guarantees.
///
/// When `false`, the side file capability must be reported as unsupported and
/// fail closed; there is no fallback to the ordinary (string-prefix) tools.
pub const SIDE_READ_SUPPORTED: bool = cfg!(unix);

/// Read access to the files a side conversation is allowed to see.
///
/// Cheap to clone: every clone shares the same authorisation source and the same
/// held directory handles.
#[derive(Clone)]
pub struct SideReadAccess {
    /// Fixed base for relative paths (the side's own working directory).
    primary: PathBuf,
    /// Roots the side was created with. The live set is intersected with these,
    /// so a root added to the parent later is never inherited.
    created_roots: Vec<AuthorizedRoot>,
    /// The parent's live roots.
    current: Arc<dyn SideRoots>,
    /// Registered attachment aliases.
    aliases: Arc<Mutex<Vec<ResolvedAlias>>>,
    /// Hook run after authorisation and before the open. Empty in production;
    /// tests install it to force a race at exactly that point.
    open_hook: OpenHook,
}

/// An optional callback invoked between authorisation and the open.
type OpenHook = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// An alias with its copy's directory held open.
#[derive(Clone)]
struct ResolvedAlias {
    original: PathBuf,
    copy: PathBuf,
    /// Handle to the directory that contains `copy`, so the copy can be opened
    /// without resolving a fresh absolute path.
    #[cfg(unix)]
    copy_dir: Arc<std::fs::File>,
    /// File name of `copy` inside `copy_dir`.
    copy_name: std::ffi::OsString,
}

impl SideReadAccess {
    /// Build access for one side.
    ///
    /// `primary` is fixed at construction and is the base for every relative
    /// path. Both it and `created_roots` are canonicalised once, here: that is
    /// the one trusted moment where resolving symlinks is legitimate. Later
    /// opens never touch a canonicalised-absolute path again.
    pub fn new(
        primary: PathBuf,
        created_roots: Vec<PathBuf>,
        current: Arc<dyn SideRoots>,
    ) -> io::Result<Self> {
        if !SIDE_READ_SUPPORTED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "handle-based side reads are unsupported on this platform",
            ));
        }
        let primary = canonical_dir(&primary)?;
        let mut roots: Vec<AuthorizedRoot> = Vec::new();
        for root in created_roots {
            match canonical_dir(&root) {
                Ok(canonical) => {
                    let given = abs_lexical(Path::new("/"), &root);
                    let authorized = AuthorizedRoot { given, canonical };
                    if !roots.iter().any(|r| r.canonical == authorized.canonical) {
                        roots.push(authorized);
                    }
                }
                // A root that does not resolve cannot be authorised.
                Err(_) => continue,
            }
        }
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "a side conversation needs at least one readable root",
            ));
        }
        Ok(Self {
            primary,
            created_roots: roots,
            current,
            aliases: Arc::new(Mutex::new(Vec::new())),
            open_hook: Arc::new(Mutex::new(None)),
        })
    }

    /// The base used for relative paths.
    pub fn primary(&self) -> &Path {
        &self.primary
    }

    /// Register pre-copied attachments.
    ///
    /// An alias only grants access to its exact `copy` path, and only while
    /// `original` is still authorised. The copy's directory is opened once and
    /// held, so serving the alias never resolves a new absolute path.
    pub fn set_attachment_aliases(&mut self, aliases: Vec<SideFileAlias>) -> io::Result<()> {
        if !SIDE_READ_SUPPORTED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "handle-based side reads are unsupported on this platform",
            ));
        }
        let mut resolved = Vec::with_capacity(aliases.len());
        for alias in aliases {
            let original = canonical_file(&alias.original)?;
            let copy = abs_lexical(&self.primary, &alias.copy);
            let parent = copy.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "alias copy has no parent")
            })?;
            let name = copy
                .file_name()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "alias copy has no file name")
                })?
                .to_owned();
            #[cfg(unix)]
            let copy_dir = {
                // `parent` comes from the side's own private copy root, so
                // resolving it once here is the same trusted moment as the
                // constructor's. The walk below never follows a symlink, and a
                // spelling like macOS's `/var` (a symlink to `/private/var`)
                // must be canonicalised first or the walk would refuse it.
                let canonical_parent = canonical_dir(parent)?;
                let dir = unix::open_directory_path(&canonical_parent)?;
                Arc::new(std::fs::File::from(dir))
            };
            #[cfg(not(unix))]
            let _ = parent;
            resolved.push(ResolvedAlias {
                original,
                copy,
                #[cfg(unix)]
                copy_dir,
                copy_name: name,
            });
        }
        *self.aliases.lock().unwrap() = resolved;
        Ok(())
    }

    /// Open a regular file for reading.
    ///
    /// Rejects directories, FIFOs, sockets, devices and symlinks.
    pub fn open_regular(&self, path: &Path) -> io::Result<std::fs::File> {
        self.open(path, false)
    }

    /// Open a directory handle, for a safe traversal.
    pub fn open_directory(&self, path: &Path) -> io::Result<std::fs::File> {
        self.open(path, true)
    }

    fn open(&self, path: &Path, want_dir: bool) -> io::Result<std::fs::File> {
        if !SIDE_READ_SUPPORTED {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "handle-based side reads are unsupported on this platform",
            ));
        }
        let target = abs_lexical(&self.primary, path);
        let effective = self.effective_roots();

        // An alias is served only from its held directory handle, and only
        // while its original is still authorised.
        if let Some(alias) = self.matching_alias(&target) {
            if !authorised(&alias.original, &effective) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "attachment alias is no longer authorised",
                ));
            }
            #[cfg(unix)]
            {
                self.run_open_hook();
                return unix::open_alias_copy(&alias.copy_dir, &alias.copy_name, want_dir);
            }
            #[cfg(not(unix))]
            unreachable!("SIDE_READ_SUPPORTED is false on non-unix targets");
        }

        let (root, relative) = effective
            .iter()
            .filter_map(|root| {
                if let Ok(rest) = target.strip_prefix(&root.given) {
                    return Some((root, rest));
                }
                target
                    .strip_prefix(&root.canonical)
                    .ok()
                    .map(|rest| (root, rest))
            })
            .max_by_key(|(root, _)| root.given.as_os_str().len())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("{} is outside every authorised side root", target.display()),
                )
            })?;
        // Re-checked after the strip: a lexical `..` can only survive inside the
        // relative part if it climbed out and back, which is not something to
        // reason about here.
        if relative
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "parent-directory components are not allowed",
            ));
        }

        self.run_open_hook();
        #[cfg(unix)]
        {
            // Anchor the walk on the *canonical* root: `given` is only a
            // spelling used for matching, never a path we open.
            unix::open_beneath(&root.canonical, relative, want_dir)
        }
        #[cfg(not(unix))]
        {
            let _ = (root, relative, want_dir);
            unreachable!("SIDE_READ_SUPPORTED is false on non-unix targets");
        }
    }

    /// Roots currently authorised: the intersection of what the side was created
    /// with and what the parent allows right now.
    fn effective_roots(&self) -> Vec<AuthorizedRoot> {
        let current = self
            .current
            .current_roots()
            .into_iter()
            .filter_map(|root| canonical_dir(&root).ok())
            .collect::<Vec<_>>();
        let mut effective: Vec<AuthorizedRoot> = Vec::new();
        for created in &self.created_roots {
            for now in &current {
                if let Some(canonical) = intersect_root(&created.canonical, now) {
                    // Carry the caller's spelling across: it is only used for
                    // prefix matching, never for opening.
                    let suffix = canonical
                        .strip_prefix(&created.canonical)
                        .unwrap_or(Path::new(""));
                    let given = abs_lexical(&created.given, suffix);
                    let authorized = AuthorizedRoot { given, canonical };
                    if !effective
                        .iter()
                        .any(|r| r.canonical == authorized.canonical)
                    {
                        effective.push(authorized);
                    }
                }
            }
        }
        effective
    }

    fn matching_alias(&self, target: &Path) -> Option<ResolvedAlias> {
        let aliases = self.aliases.lock().unwrap();
        // Exact match only: an alias must never become a directory capability.
        aliases.iter().find(|alias| alias.copy == target).cloned()
    }

    fn run_open_hook(&self) {
        let hook = self.open_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(all(test, unix))]
    fn set_open_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.open_hook.lock().unwrap() = Some(hook);
    }
}

/// Intersection of two roots by component containment.
///
/// `a` inside `b` yields `a` (the narrower root wins); `b` inside `a` yields
/// `b`; unrelated roots yield `None`. This is deliberately *not* set equality:
/// a parent narrowing its root must narrow the side, not silently match nothing.
fn intersect_root(a: &Path, b: &Path) -> Option<PathBuf> {
    if a.starts_with(b) {
        Some(a.to_path_buf())
    } else if b.starts_with(a) {
        Some(b.to_path_buf())
    } else {
        None
    }
}

/// Whether `target` lies inside one of `roots`.
fn authorised(target: &Path, roots: &[AuthorizedRoot]) -> bool {
    roots
        .iter()
        .any(|root| target.starts_with(&root.given) || target.starts_with(&root.canonical))
}

/// Resolve `path` against `base` and strip `.`/`..` lexically.
///
/// `..` is resolved without touching the filesystem, so a path that climbs out
/// of a root simply stops matching it. Nothing here consults a symlink.
fn abs_lexical(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
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

/// Resolve a trusted file once, at construction.
fn canonical_file(path: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    Ok(canonical)
}

/// Resolve a trusted directory root once, at construction.
fn canonical_dir(path: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", path.display()),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    /// A `SideRoots` whose set a test can mutate between calls.
    struct TestRoots(RwLock<Vec<PathBuf>>);

    impl SideRoots for TestRoots {
        fn current_roots(&self) -> Vec<PathBuf> {
            self.0.read().unwrap().clone()
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        roots: Arc<TestRoots>,
        access: SideReadAccess,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let roots = Arc::new(TestRoots(RwLock::new(vec![path.clone()])));
        let access = SideReadAccess::new(path.clone(), vec![path.clone()], roots.clone()).unwrap();
        Fixture {
            _dir: dir,
            path,
            roots,
            access,
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn side_read_access_rechecks_roots_and_blocks_path_races() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"ok").unwrap();
        let roots = Arc::new(TestRoots(RwLock::new(vec![dir.path().to_owned()])));
        let access =
            SideReadAccess::new(dir.path().into(), vec![dir.path().into()], roots.clone()).unwrap();
        assert!(access.open_regular(Path::new("a.txt")).is_ok());
        roots.0.write().unwrap().clear();
        assert!(access.open_regular(Path::new("a.txt")).is_err());
    }

    #[test]
    fn side_read_roots_accepts_relative_and_absolute_paths() {
        let f = fixture();
        write(&f.path.join("sub/b.txt"), b"data");
        let mut relative = f.access.open_regular(Path::new("sub/b.txt")).unwrap();
        let mut absolute = f.access.open_regular(&f.path.join("sub/b.txt")).unwrap();
        let mut a = String::new();
        let mut b = String::new();
        std::io::Read::read_to_string(&mut relative, &mut a).unwrap();
        std::io::Read::read_to_string(&mut absolute, &mut b).unwrap();
        assert_eq!(a, "data");
        assert_eq!(b, "data");
    }

    #[test]
    fn side_read_parent_adding_a_root_is_not_inherited() {
        let f = fixture();
        let other = tempfile::tempdir().unwrap();
        write(&other.path().join("secret.txt"), b"nope");
        // The parent grants itself a new root after the side exists.
        f.roots.0.write().unwrap().push(other.path().to_path_buf());
        let error = f
            .access
            .open_regular(&other.path().join("secret.txt"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn side_read_parent_narrowing_the_root_narrows_the_side() {
        let f = fixture();
        write(&f.path.join("keep/yes.txt"), b"ok");
        write(&f.path.join("drop/no.txt"), b"no");
        // The parent narrows its root to a subdirectory.
        *f.roots.0.write().unwrap() = vec![f.path.join("keep")];
        assert!(f.access.open_regular(Path::new("keep/yes.txt")).is_ok());
        assert!(
            f.access.open_regular(Path::new("drop/no.txt")).is_err(),
            "a narrowed root must not keep the wider one reachable"
        );
    }

    #[test]
    fn side_read_parent_widening_does_not_widen_the_side() {
        let f = fixture();
        let sub = f.path.join("sub");
        write(&sub.join("inside.txt"), b"ok");
        // Side was created with the narrow root only.
        let access = SideReadAccess::new(sub.clone(), vec![sub.clone()], f.roots.clone()).unwrap();
        write(&f.path.join("outside.txt"), b"no");
        // The parent now has the wider root, but the side must stay narrow.
        assert!(access.open_regular(&f.path.join("outside.txt")).is_err());
        assert!(access.open_regular(&sub.join("inside.txt")).is_ok());
    }

    #[test]
    fn side_read_rejects_parent_directory_escapes() {
        let f = fixture();
        write(&f.path.join("a.txt"), b"ok");
        // Climbing out of the root then back in must not be accepted.
        assert!(
            f.access.open_regular(Path::new("../a.txt")).is_err(),
            "lexical .. must not escape the root"
        );
        assert!(f.access.open_regular(Path::new("sub/../../a.txt")).is_err());
    }

    #[test]
    fn side_read_prefix_lookalike_root_is_rejected() {
        let f = fixture();
        // A sibling directory whose name *starts with* the root's name.
        let sibling = f.path.with_extension("evil");
        std::fs::create_dir_all(&sibling).unwrap();
        write(&sibling.join("x.txt"), b"no");
        let result = f.access.open_regular(&sibling.join("x.txt"));
        let _ = std::fs::remove_dir_all(&sibling);
        assert!(
            result.is_err(),
            "a string-prefix match must not authorise a sibling directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn side_read_rejects_a_directory_swapped_for_a_symlink() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let f = fixture();
        write(&f.path.join("real/inner.txt"), b"ok");
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("inner.txt"), b"leaked");

        // Swap `real` for a symlink pointing outside the root *after* the
        // authorisation check but *before* the open. A "check the path, then
        // open the path" implementation follows the new symlink and reads the
        // outside file; the handle-based walk refuses it.
        let dir = f.path.join("real");
        let moved = f.path.join("real-moved");
        let outside_path = outside.path().to_path_buf();
        let swapped = std::sync::Arc::new(AtomicBool::new(false));
        let swapped_clone = swapped.clone();
        let access = f.access.clone();
        access.set_open_hook(std::sync::Arc::new(move || {
            std::fs::rename(&dir, &moved).unwrap();
            std::os::unix::fs::symlink(&outside_path, &dir).unwrap();
            swapped_clone.store(true, Ordering::SeqCst);
        }));
        let result = access.open_regular(Path::new("real/inner.txt"));
        assert!(
            swapped.load(Ordering::SeqCst),
            "the swap must have happened"
        );
        assert!(
            result.is_err(),
            "a symlinked directory must never be followed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn side_read_rejects_symlinked_files_and_special_files() {
        use std::os::unix::fs::symlink;
        let f = fixture();
        write(&f.path.join("real.txt"), b"ok");
        symlink(f.path.join("real.txt"), f.path.join("link.txt")).unwrap();
        assert!(
            f.access.open_regular(Path::new("link.txt")).is_err(),
            "a symlinked file must be refused"
        );

        let fifo = f.path.join("pipe");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed");
        let error = f.access.open_regular(Path::new("pipe")).unwrap_err();
        assert!(
            matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::PermissionDenied
            ),
            "a FIFO must be refused, got {error:?}"
        );

        // A directory is not a regular file.
        std::fs::create_dir_all(f.path.join("dir")).unwrap();
        assert!(f.access.open_regular(Path::new("dir")).is_err());
        // ...but it opens as a directory handle.
        assert!(f.access.open_directory(Path::new("dir")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn side_read_alias_is_revoked_with_its_original() {
        let f = fixture();
        write(&f.path.join("attachment.png"), b"original");
        let copies = tempfile::tempdir().unwrap();
        let copy = copies.path().join("copy.png");
        write(&copy, b"copy-bytes");

        let mut access = f.access.clone();
        access
            .set_attachment_aliases(vec![SideFileAlias {
                original: f.path.join("attachment.png"),
                copy: copy.clone(),
            }])
            .unwrap();
        // The exact copy path is served while the original is authorised.
        let mut file = access.open_regular(&copy).unwrap();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut file, &mut text).unwrap();
        assert_eq!(text, "copy-bytes");

        // Revoking the original's root revokes the alias too.
        f.roots.0.write().unwrap().clear();
        let error = access.open_regular(&copy).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        // And the alias does not leak the directory: a sibling is not served.
        *f.roots.0.write().unwrap() = vec![f.path.clone()];
        let sibling = copies.path().join("sibling.png");
        write(&sibling, b"sibling");
        assert!(access.open_regular(&sibling).is_err());
    }
}
