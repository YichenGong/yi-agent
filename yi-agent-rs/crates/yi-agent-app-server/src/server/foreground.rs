//! Foreground activity for the app-server's threads.
//!
//! The app-server is a frontend of the project runtime exactly like the TUI and a
//! headless run: it holds one lease per process and asks the guardian to let it drive
//! each conversation's foreground root. This module is the thin adapter that keeps that
//! fact out of the turn driver, where it would otherwise tangle with prompt handling.
//!
//! Two rules are inherited from the contract and restated here because they are the
//! ones a wiring mistake breaks:
//!
//! * **A refusal is not an error.** `None` means "this runtime does not offer the
//!   contract to this conversation" and the turn runs with legacy semantics. Surfacing
//!   it would turn an unsupported deployment into a broken one.
//! * **Activity never controls the turn.** Every method swallows a failed write into a
//!   diagnostic; nothing here cancels work, renews a lease or retries a tool.

use std::sync::Arc;

use yi_agent_subagent::foreground::{ForegroundHost, ForegroundTurn};
use yi_agent_subagent::thread_root::ThreadRoot;

/// The process's foreground authority plus the per-conversation driver it hands out.
///
/// One per app-server, shared by every thread driver: the lease belongs to the client,
/// not to a conversation.
pub(crate) struct ForegroundDispatch {
    host: ForegroundHost,
}

impl ForegroundDispatch {
    /// Opens a driver for a conversation whose authority the shell already brokered.
    ///
    /// Deliberately bypasses [`ForegroundHost`], and with it `FrontendLease::register`:
    /// on a managed deployment the App's lease belongs to the desktop shell, and a
    /// sidecar that registered its own `app-server:{pid}` client would become a second
    /// App frontend -- exactly the shape that lets an orphaned sidecar keep renewing
    /// after the window is gone. The capability it binds with comes from the root's own
    /// session store, which only accepts what the shell brokered.
    pub(crate) fn begin_authorized(
        root: &Arc<ThreadRoot>,
        turn_id: &str,
    ) -> Option<yi_agent_subagent::foreground::ForegroundTurn> {
        let authorization = root.managed_root_authorization()?;
        // One bind operation per conversation, stable across turns: the daemon refuses a
        // second driver for the same root until the first is proven stopped, so a fresh
        // id per turn would make the second turn unpublishable.
        let operation_id =
            yi_agent_subagent::thread_root::managed_bind_operation_id(root.thread_id());
        match root.bind_foreground_driver(authorization, &operation_id) {
            Ok(Some(mut turn)) => {
                // Open the turn on the same driver, exactly as `ForegroundHost::begin`
                // does: a driver that never hears about the turn reports activity
                // against nothing, and the daemon would then see a sequence it cannot
                // attribute to any turn.
                turn.begin(turn_id);
                Some(turn)
            }
            Ok(None) => None,
            Err(cause) => {
                // Losing activity reporting must not lose the turn: the conversation
                // still works, it just stops being observable.
                tracing::warn!(%cause, "foreground driver unavailable for a managed root");
                None
            }
        }
    }

    /// Registers this app-server as a frontend of `runtime_dir`'s runtime.
    ///
    /// `Ok(None)` for an embedded runtime and for a runtime with no guardian. Both are
    /// normal: the app-server is useful either way, and a launch must not fail because
    /// a display feature is unavailable.
    pub(crate) fn connect(runtime_dir: &std::path::Path) -> std::io::Result<Option<Self>> {
        let client_run_id = format!("app-server:{}", std::process::id());
        let Some(host) = ForegroundHost::connect(
            runtime_dir,
            yi_agent_lifecycle::types::ClientKind::App,
            client_run_id,
        )?
        else {
            return Ok(None);
        };
        Ok(Some(Self { host }))
    }

    /// Registers this app-server against the runtime that serves `root`.
    ///
    /// Split from [`Self::connect`] so a caller holding only the conversation's root
    /// (the turn driver does) can reach the guardian without being handed the runtime
    /// directory separately -- one source of truth for "where is this runtime".
    pub(crate) fn connect_root(root: &Arc<ThreadRoot>) -> std::io::Result<Option<Self>> {
        match root.runtime_directory() {
            Some(dir) => Self::connect(&dir),
            // No directory: an embedded runtime, which never has a guardian.
            None => Ok(None),
        }
    }

    /// Whether this conversation's runtime authorizes *delegation* for this host.
    ///
    /// Separate from [`Self::begin`], which only opens activity reporting: the
    /// answers are deliberately not flattened, because a guardian's refusal must not
    /// be read as "no guardian" and silently replaced by a resume secret.
    pub(crate) fn authority(
        &self,
        root: &Arc<ThreadRoot>,
        operation_id: &str,
    ) -> Result<yi_agent_subagent::thread_root::ForegroundAuthority, String> {
        self.host.authority(root, operation_id)
    }

    /// Opens the turn, or reports that this conversation cannot be driven.
    ///
    /// `operation_id` identifies the *bind*, not the turn: it is stable for the life of
    /// the process because a second bind without a proven stop is refused by the daemon
    /// by design.
    pub(crate) fn begin(&self, root: &Arc<ThreadRoot>, turn_id: &str) -> Option<ForegroundTurn> {
        match self.host.begin(root, &self.operation_id(), turn_id) {
            Ok(turn) => turn,
            Err(cause) => {
                // A transport failure leaves the conversation's own state untouched,
                // so the honest answer is "no activity this turn", not a failed turn.
                tracing::warn!(%cause, "foreground activity unavailable for this turn");
                None
            }
        }
    }

    fn operation_id(&self) -> String {
        // Stable across turns *and* across a turn retry: the id names the bind attempt,
        // and a new one would be a second driver, which the daemon refuses until the
        // first is proven stopped.
        format!("app-server-bind:{}", std::process::id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A root with no runtime directory must not reach a guardian.
    ///
    /// This is the embedded deployment and the shape the TUI had before
    /// `RuntimeBinding::fixed_in`: the binding was handed a root and never learned
    /// where its runtime lives. Answering `None` is the honest reading, and it is what
    /// keeps an embedded runtime from looking like a broken managed one.
    #[test]
    fn foreground_dispatch_refuses_a_root_without_a_runtime_directory() {
        let root = yi_agent_subagent::thread_root::ThreadRoot::from_handle(
            yi_agent_subagent::binding::RuntimeBinding::fixed(
                yi_agent_subagent::binding::RuntimeHandle {
                    socket_path: std::path::PathBuf::from("/nonexistent/runtime.sock"),
                    workspace_root: std::path::PathBuf::from("/tmp/foreground-project"),
                    session_id: "session-1".into(),
                    task_id: "root-1".into(),
                    capability: "cap-1".into(),
                },
            ),
            yi_agent_subagent::AttachedRoot {
                session_id: "session-1".into(),
                task_id: "root-1".into(),
                capability: "cap-1".into(),
                workspace: yi_agent_core::subagent::worker::WorkerWorkspace {
                    lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
                    repository_root: std::path::PathBuf::from("/tmp/foreground-project"),
                    path: std::path::PathBuf::from("/tmp/foreground-project"),
                    branch: String::new(),
                    parent_branch: String::new(),
                    base_commit: String::new(),
                },
            },
        );

        assert!(
            ForegroundDispatch::connect_root(&root)
                .expect("an absent runtime directory is not an error")
                .is_none(),
            "a binding that never learned a runtime directory cannot reach a guardian"
        );
    }

    /// A runtime directory with no guardian behind it is the embedded case.
    #[test]
    fn foreground_dispatch_refuses_a_runtime_without_a_guardian() {
        let directory = tempfile::TempDir::new().expect("a temp dir");
        let runtime_dir = directory.path().join("runtime");
        std::fs::create_dir_all(&runtime_dir).expect("the runtime dir");
        assert!(
            ForegroundDispatch::connect(&runtime_dir)
                .expect("no guardian is not an error")
                .is_none(),
            "an empty runtime directory offers no foreground contract"
        );
    }
}
