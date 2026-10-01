//! One conversation's own application root on a shared project daemon.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use crate::AttachedRoot;
use crate::binding::{RuntimeBinding, RuntimeHandle, is_liveness_error};

/// The stable attach key for one conversation's root.
///
/// The project key cannot be used: two conversations in one directory must get
/// two roots, or they share one `MAX_DIRECT_CHILDREN` budget. The thread id is
/// stable across retries and across a daemon restart, so re-attaching the same
/// conversation adopts its own root instead of minting a new one.
pub fn application_root_key(thread_id: &str) -> String {
    format!("thread:{thread_id}")
}

/// One conversation's own root on a shared project runtime.
///
/// The `RuntimeBinding` is shared (one daemon per project, self-healing); this
/// type owns only the root triple, so two conversations in one directory have
/// two independent root tasks and two independent child budgets.
pub struct ThreadRoot {
    binding: Arc<RuntimeBinding>,
    thread_id: String,
    project_dir: PathBuf,
    /// The cached root triple together with the runtime generation it was minted
    /// against. The generation and the triple are stored as one value so the two
    /// can never be read apart: a root is only ever paired with the runtime that
    /// created it.
    cached: Mutex<Option<CachedRoot>>,
}

/// A conversation root plus the runtime generation that minted it.
struct CachedRoot {
    generation: u64,
    root: AttachedRoot,
}

/// Whether a cached root still belongs to the runtime the binding is currently
/// handing out.
///
/// A replacement daemon sweeps the previous daemon's roots away, so once the
/// binding installs a new runtime (`generation` moves on) any root minted
/// against the old one is dead and must be re-attached. Kept as a free function
/// so the rule is testable without a live daemon.
fn cached_root_is_live(cached_generation: u64, live_generation: u64) -> bool {
    cached_generation == live_generation
}

impl ThreadRoot {
    pub fn new(
        binding: Arc<RuntimeBinding>,
        thread_id: impl Into<String>,
        project_dir: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            thread_id: thread_id.into(),
            project_dir,
            cached: Mutex::new(None),
        })
    }

    /// Wraps a root that already exists on a fixed runtime, without attaching.
    ///
    /// The TUI and the daemon's own workers own their daemon for the life of the
    /// process and were handed an already-attached root; they must not re-attach
    /// through the wire. The fixed binding reports generation 0 forever, so the
    /// root stays live and no repair is ever attempted.
    pub fn from_handle(binding: Arc<RuntimeBinding>, root: AttachedRoot) -> Arc<Self> {
        let thread_id = root.session_id.clone();
        let project_dir = root.workspace.path.clone();
        let thread_root = Self::new(binding, thread_id, project_dir);
        *thread_root.lock_cached() = Some(CachedRoot { generation: 0, root });
        thread_root
    }

    fn lock_cached(&self) -> std::sync::MutexGuard<'_, Option<CachedRoot>> {
        self.cached
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cached root if it still belongs to the live runtime, else `None`.
    ///
    /// Dropping a stale root here -- and never returning it -- is what keeps the
    /// `handle()` invariant: a root is only ever paired with the daemon that
    /// minted it.
    fn live_cached_root(&self, live_generation: u64) -> Option<AttachedRoot> {
        let mut cached = self.lock_cached();
        match cached.as_ref() {
            Some(entry) if cached_root_is_live(entry.generation, live_generation) => {
                Some(entry.root.clone())
            }
            Some(_) => {
                *cached = None;
                None
            }
            None => None,
        }
    }

    /// Attaches (or adopts) this conversation's root on the shared daemon.
    ///
    /// Idempotent: the stable key means a second call adopts the root the first
    /// one created, including after a daemon restart.
    pub fn attach(&self) -> Result<(), String> {
        // Resolve the runtime first, then check the cache against exactly that
        // runtime: the check and the mint must agree on which daemon mints the
        // root, or a repair between the two would leave a root pointed at a
        // daemon that does not know it.
        let generation = self.binding.generation();
        if self.live_cached_root(generation).is_some() {
            return Ok(());
        }
        // The binding repairs and retries once on a liveness failure, so a daemon
        // that died while a root was cached is recovered instead of surfacing a
        // raw transport error. The generation comes back together with the answer,
        // naming the runtime that actually served this mint -- never a later one a
        // concurrent repair may have installed in the meantime.
        let (response, generation) =
            self.binding
                .send_with_generation(|_handle| IpcRequest::AttachApplicationRoot {
                    idempotency_key: application_root_key(&self.thread_id),
                    workspace: self.project_dir.clone(),
                })?;
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            workspace,
        } = response
        else {
            return Err(format!("daemon rejected the conversation root: {response:?}"));
        };
        *self.lock_cached() = Some(CachedRoot {
            generation,
            root: AttachedRoot {
                session_id,
                task_id: root_task_id,
                capability: message_capability,
                workspace,
            },
        });
        Ok(())
    }

    /// The handle a delegation call should act as: the shared socket plus this
    /// conversation's own root triple.
    pub fn handle(&self) -> Result<RuntimeHandle, String> {
        // attach() mints or repairs the root against the live runtime, so the
        // root it leaves cached belongs to the live generation.
        self.attach()?;
        // Read the socket and the generation from one snapshot: if a repair lands
        // after this, the generation moves and the cache is invalidated, so the
        // next call re-attaches rather than serving a root this socket never knew.
        let (shared, generation) = self.binding.current_with_generation();
        let root = self
            .live_cached_root(generation)
            .ok_or_else(|| "the conversation root is not attached".to_string())?;
        Ok(RuntimeHandle {
            socket_path: shared.socket_path,
            workspace_root: shared.workspace_root,
            session_id: root.session_id,
            task_id: root.task_id,
            capability: root.capability,
        })
    }

    /// This conversation's root task id, once attached.
    pub fn root_task_id(&self) -> Option<String> {
        let generation = self.binding.generation();
        self.live_cached_root(generation)
            .map(|root| root.task_id)
    }

    /// Sends one request as this conversation's root, re-attaching and retrying
    /// once if the runtime turns out to be unreachable.
    ///
    /// The retry runs the whole `handle()` path again, so a replacement daemon
    /// (whose sweep removed the old root) is re-attached rather than receiving a
    /// request naming a task it never heard of.
    fn send_as_root<F>(&self, build: F) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        let handle = self.handle()?;
        match send_request(&handle.socket_path, build(&handle)) {
            Ok(response) => Ok(response),
            Err(error) if is_liveness_error(&error) => {
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Activates this conversation's root with its first objective. Idempotent
    /// at the daemon, so a re-run after a repair is safe.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        match self.send_as_root(|handle| IpcRequest::ActivateApplicationRoot {
            session_id: handle.session_id.clone(),
            root_task_id: handle.task_id.clone(),
            capability: handle.capability.clone(),
            objective: objective.to_owned(),
        }) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!("daemon rejected the conversation activation: {other:?}")),
            Err(error) => Err(error),
        }
    }

    /// Detaches this conversation's root. Best effort: a failure is a trace
    /// line, never an error that masks why the conversation is winding down.
    pub fn detach(&self) {
        // No repair here: a detach against a dead daemon has nothing to undo.
        let Ok(handle) = self.handle() else {
            return;
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
            },
        ) {
            tracing::warn!(%error, "could not detach the conversation root");
        }
        *self.lock_cached() = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::RuntimeBinding;
    use yi_agent_core::subagent::task::WorkspaceLeaseId;
    use yi_agent_core::subagent::worker::WorkerWorkspace;

    fn fixed_binding() -> Arc<RuntimeBinding> {
        RuntimeBinding::fixed(RuntimeHandle {
            socket_path: PathBuf::from("/nonexistent/never-listening.sock"),
            workspace_root: PathBuf::from("/tmp/workspace"),
            session_id: "shared-session".into(),
            task_id: "shared-root".into(),
            capability: "shared-cap".into(),
        })
    }

    fn root(session: &str, task: &str) -> AttachedRoot {
        AttachedRoot {
            session_id: session.into(),
            task_id: task.into(),
            capability: "cap".into(),
            workspace: WorkerWorkspace {
                lease_id: WorkspaceLeaseId::new(),
                repository_root: PathBuf::from("/tmp/repo"),
                path: PathBuf::from("/tmp/repo"),
                branch: String::new(),
                parent_branch: String::new(),
                base_commit: String::new(),
            },
        }
    }

    #[test]
    fn a_cached_root_is_live_only_within_its_own_generation() {
        assert!(cached_root_is_live(0, 0));
        assert!(cached_root_is_live(3, 3));
        assert!(
            !cached_root_is_live(0, 1),
            "a repair that installed a new runtime must invalidate the old root"
        );
        assert!(!cached_root_is_live(2, 1));
    }

    #[test]
    fn a_repaired_runtime_drops_the_root_minted_before_it() {
        let root_handle = ThreadRoot::new(fixed_binding(), "thread-a", PathBuf::from("/tmp/p"));
        *root_handle.lock_cached() = Some(CachedRoot {
            generation: 0,
            root: root("session-before", "root-before"),
        });

        // Still the generation that minted it: the cached root survives.
        let live = root_handle
            .live_cached_root(0)
            .expect("the root belongs to generation 0");
        assert_eq!(live.task_id, "root-before");

        // A repair moved the binding to generation 1: the old root is gone and
        // must not be handed out again.
        assert!(
            root_handle.live_cached_root(1).is_none(),
            "a root minted before the repair must not be served"
        );
        assert!(
            root_handle.lock_cached().is_none(),
            "the stale root must be dropped, not merely skipped"
        );
    }

    #[test]
    fn attach_is_a_no_op_while_a_live_root_is_cached() {
        // The binding points at a socket nothing listens on, so any real IPC
        // would fail. A live cached root must short-circuit before touching it.
        let thread_root = ThreadRoot::new(fixed_binding(), "thread-a", PathBuf::from("/tmp/p"));
        *thread_root.lock_cached() = Some(CachedRoot {
            generation: 0,
            root: root("s", "adopted-root"),
        });
        assert!(
            thread_root.attach().is_ok(),
            "attach must adopt the live cached root without a round trip"
        );
        assert_eq!(thread_root.root_task_id().as_deref(), Some("adopted-root"));
    }

    #[test]
    fn a_conversation_key_is_stable_and_distinct_per_thread() {
        assert_eq!(application_root_key("thread-a"), "thread:thread-a");
        assert_eq!(application_root_key("thread-b"), "thread:thread-b");
        assert_eq!(
            application_root_key("thread-a"),
            application_root_key("thread-a"),
            "the same conversation must key the same root on every attempt"
        );
        assert_ne!(
            application_root_key("thread-a"),
            application_root_key("thread-b"),
            "two conversations must not share one root"
        );
    }
}
