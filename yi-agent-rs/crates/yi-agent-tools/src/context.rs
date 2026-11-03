use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::managed_process::{ManagedProcessPolicy, ManagedSpawner};

/// Either an unmanaged context, or a managed one with its spawner.
struct ManagedSpawn {
    policy: ManagedProcessPolicy,
    spawner: Option<ManagedSpawner>,
}

impl ManagedSpawn {
    fn unmanaged() -> Self {
        Self {
            policy: ManagedProcessPolicy::Unmanaged,
            spawner: None,
        }
    }

    fn managed(policy: ManagedProcessPolicy, spawner: ManagedSpawner) -> Self {
        Self {
            policy,
            spawner: Some(spawner),
        }
    }
}

/// Shared context for all builtin tools.
///
/// `roots` is the session's root set: `roots[0]` is the primary root and is the
/// base for every relative path; later entries are linked roots the session may
/// also read and write. `cwd` persists across BashTool calls and is always
/// relative to the primary root.
pub struct ToolsContext {
    roots: Vec<PathBuf>,
    cwd: Mutex<PathBuf>,
    /// 注入给 shell 子进程的环境变量（如当前会话的 thread id）。默认空集，
    /// 此时子进程行为与今天完全一致。
    session_env: Vec<(String, String)>,
    /// How spawned processes are owned.
    ///
    /// `Unmanaged` keeps the original direct-spawn path: a manual daemon or a test
    /// harness has no guardian and makes no promise that quitting stops anything.
    /// In a managed runtime this is a frontend policy **and** a spawner, so every
    /// child is registered under one owner before it runs.
    managed: ManagedSpawn,
}

impl ToolsContext {
    /// Single-root constructor. Equivalent to [`Self::with_roots`] with one root.
    pub fn new(root: PathBuf) -> Self {
        Self::with_roots(vec![root])
    }

    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        Self::with_roots_and_env(roots, Vec::new())
    }

    /// 同 [`Self::with_roots`]，但额外带上注入 shell 子进程的环境变量。
    pub fn with_roots_and_env(roots: Vec<PathBuf>, session_env: Vec<(String, String)>) -> Self {
        let cwd = roots.first().cloned().unwrap_or_default();
        Self {
            roots,
            cwd: Mutex::new(cwd),
            session_env,
            managed: ManagedSpawn::unmanaged(),
        }
    }

    /// The same context, with every spawned process owned by one frontend.
    ///
    /// Consuming `self` rather than mutating in place on purpose: a context that
    /// switched owners mid-session would leave earlier children under the old
    /// owner and later ones under the new, and "which client owns this?" would stop
    /// being answerable.
    pub fn into_managed(mut self, policy: ManagedProcessPolicy, spawner: ManagedSpawner) -> Self {
        self.managed = ManagedSpawn::managed(policy, spawner);
        self
    }

    /// The ownership policy for spawned processes.
    pub fn managed_policy(&self) -> &ManagedProcessPolicy {
        &self.managed.policy
    }

    /// The spawner to use, when this runtime is managed.
    pub fn managed_spawner(&self) -> Option<&ManagedSpawner> {
        self.managed.spawner.as_ref()
    }

    /// 注入给 shell 子进程的环境变量（默认空）。
    pub fn session_env(&self) -> &[(String, String)] {
        &self.session_env
    }

    /// The primary root: the base for relative paths and for `cwd`.
    pub fn root(&self) -> &Path {
        self.roots
            .first()
            .map(PathBuf::as_path)
            .unwrap_or(Path::new(""))
    }

    /// The whole root set, primary first.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// `path` expressed relative to whichever root contains it; `None` when it
    /// is outside every root. Used by glob/grep so reported matches stay relative
    /// to the root they actually came from.
    pub fn relative_to_roots(&self, path: &Path) -> Option<String> {
        relative_to_roots(&self.roots, path)
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    pub fn set_cwd(&self, path: PathBuf) {
        *self.cwd.lock().unwrap() = path;
    }
}

/// 自由函数版 [`ToolsContext::relative_to_roots`]：只收根集切片，供拿不到
/// `ToolsContext` 的调用方（如 `grep_search`）复用同一套"命中哪个根就相对哪个根"
/// 的规则，避免规则在两处各写一遍后走样。
pub(crate) fn relative_to_roots(roots: &[PathBuf], path: &Path) -> Option<String> {
    roots
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map(|rel| rel.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_cwd_to_root() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        assert_eq!(ctx.root(), Path::new("/tmp/foo"));
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn set_cwd_updates_cwd() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        ctx.set_cwd(PathBuf::from("/tmp/bar"));
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/bar"));
        // root unchanged
        assert_eq!(ctx.root(), Path::new("/tmp/foo"));
    }

    #[test]
    fn with_roots_keeps_primary_first_and_shares_paths() {
        let ctx = ToolsContext::with_roots(vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
        assert_eq!(ctx.root(), Path::new("/tmp/a"));
        assert_eq!(
            ctx.roots(),
            &[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]
        );
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/a"));
    }

    #[test]
    fn relative_to_roots_reports_the_matching_root() {
        let ctx = ToolsContext::with_roots(vec![PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
        assert_eq!(
            ctx.relative_to_roots(Path::new("/tmp/b/src/main.rs")),
            Some("src/main.rs".to_string())
        );
        assert_eq!(ctx.relative_to_roots(Path::new("/tmp/elsewhere/x")), None);
    }

    #[test]
    fn a_context_without_env_exposes_an_empty_session_env() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        assert!(ctx.session_env().is_empty());
    }

    #[test]
    fn a_context_carries_its_session_env() {
        let ctx = ToolsContext::with_roots_and_env(
            vec![PathBuf::from("/tmp/foo")],
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        );
        assert_eq!(
            ctx.session_env(),
            &[("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())]
        );
    }
}
