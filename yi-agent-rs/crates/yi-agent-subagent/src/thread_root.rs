//! One conversation's own application root on a shared project daemon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use crate::AttachedRoot;
use crate::binding::{RuntimeBinding, RuntimeHandle, is_liveness_error, is_stale_root_response};

/// 身份层失败时对模型说的话。
///
/// 不可重放的请求（发消息、spawn）遇到陈旧 root 时，重放会产生真实的第二次副作用，
/// 所以只做「丢弃缓存 + 修好运行时」，然后把下一步交给调用方。这句话必须说清
/// 「已经修好了、再来一次就行」，否则模型会以为这个能力坏了。
pub(crate) fn stale_root_hint() -> &'static str {
    "the conversation root was replaced by a restarted runtime and has been \
     re-attached; the request was NOT re-sent because it changes daemon state. \
     Retry this call once."
}

/// 一条请求能否在身份层失败后被重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryPolicy {
    /// 请求本身幂等（只读查询，或在 daemon 侧按稳定键幂等）：可以安全重发。
    Idempotent,
    /// 请求会改变 daemon 状态（发消息、spawn、fork 上传）：绝不重发，
    /// 只修好运行时并把决定权交回调用方。
    SingleShot,
}

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
    /// This conversation's linked roots (the primary root excluded). Fixed when
    /// the root is created and re-sent verbatim on every re-attach.
    extra_roots: Vec<PathBuf>,
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
        extra_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            binding,
            thread_id: thread_id.into(),
            project_dir,
            extra_roots,
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
        // The TUI and the daemon's own workers are single-root by design: they
        // were handed one already-attached root, and there is no second root to
        // re-send.
        let thread_root = Self::new(binding, thread_id, project_dir, Vec::new());
        *thread_root.lock_cached() = Some(CachedRoot {
            generation: 0,
            root,
        });
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
                    // Re-sent on every attach, including a reconnect after a
                    // daemon replacement: the daemon records them on creation
                    // and restores them on adoption, so the session's root set
                    // cannot be lost by a retry.
                    extra_roots: self
                        .extra_roots
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                })?;
        let IpcResponse::ApplicationRootAttached {
            session_id,
            root_task_id,
            message_capability,
            workspace,
        } = response
        else {
            return Err(format!(
                "daemon rejected the conversation root: {response:?}"
            ));
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

    /// 这个会话根所属的项目目录。
    ///
    /// 回收空闲项目的 runtime 时要用它把挂在同一项目下的会话根按项目归类：
    /// 只摘 `runtimes` 表条目是不够的——`ThreadRoot` 自己也握着
    /// `Arc<RuntimeBinding>`，不摘掉这些引用，daemon 与监督线程根本不会停。
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// This conversation's root task id, once attached.
    pub fn root_task_id(&self) -> Option<String> {
        let generation = self.binding.generation();
        self.live_cached_root(generation).map(|root| root.task_id)
    }

    /// 以本会话 root 的身份发送一条请求；运行时失效时自愈一次并按 `policy` 决定
    /// 是否重发。
    ///
    /// 这是委派工具通往 wire 的唯一入口。`handle()` 只解析句柄、不处理失效，
    /// 七个工具过去各自 `handle()` + 裸 `send_request`，结果是**没人**处理失效。
    ///
    /// 三条分支：
    /// 1. 传输层失败（`is_liveness_error`）：与过去一致，重取句柄后重发一次。
    /// 2. 身份层失败（`is_stale_root_response`）：丢弃缓存 root、修好运行时；
    ///    `Idempotent` 重发一次，`SingleShot` 返回 [`stale_root_hint`]。
    /// 3. 其余：原样返回，绝不重试。
    pub(crate) fn send_as_root<F>(
        &self,
        policy: RetryPolicy,
        build: F,
    ) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        let handle = self.handle()?;
        match send_request(&handle.socket_path, build(&handle)) {
            Ok(response) if is_stale_root_response(&response) => {
                // 先丢弃归属旧 runtime 的 root：无论接下来能否修好运行时，
                // 这个 root 都已经不能再用。
                *self.lock_cached() = None;
                // 修不好（fixed binding 没有 repair plan，或 repair 本身失败）时
                // **不能**把内部错误抛给模型：关于这次调用的事实是原始业务应答。
                if let Err(error) = self.binding.repair() {
                    tracing::debug!(%error, "could not repair the runtime for a stale root");
                    return Ok(response);
                }
                if policy == RetryPolicy::SingleShot {
                    return Err(stale_root_hint().to_string());
                }
                // 缓存已被清空，`handle()` 会先 `attach()` 再取句柄：重挂到
                // 修好后的 runtime 上。这里只重发一次，没有递归。
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Ok(response) => Ok(response),
            Err(error) if is_liveness_error(&error) => {
                // 传输层失败说明请求根本没送达，重发一次是安全的。但**不能**直接
                // `handle()`：缓存 root 仍属于当前 generation，`attach()` 会短路
                // 返回 Ok，重发于是又打向同一个死 socket——这正是要修的那个 bug。
                // 必须先 `repair()`（它推进 generation，使缓存失效），再重挂。
                if let Err(repair_error) = self.binding.repair() {
                    // fixed binding 无从 repair（TUI / daemon worker 的情形）。
                    // 这里返回**原始传输错误**，不要把内部措辞泄漏给模型；
                    // 跳过重发也不可能成功，发了只是多一次失败。
                    tracing::debug!(%repair_error, "could not repair a dead runtime");
                    return Err(error.to_string());
                }
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Activates this conversation's root with its first objective. Idempotent
    /// at the daemon, so a re-run after a repair is safe.
    pub fn activate(&self, objective: &str) -> Result<(), String> {
        match self.send_as_root(RetryPolicy::Idempotent, |handle| {
            IpcRequest::ActivateApplicationRoot {
                session_id: handle.session_id.clone(),
                root_task_id: handle.task_id.clone(),
                capability: handle.capability.clone(),
                objective: objective.to_owned(),
            }
        }) {
            Ok(IpcResponse::ApplicationRootActivated) => Ok(()),
            Ok(other) => Err(format!(
                "daemon rejected the conversation activation: {other:?}"
            )),
            Err(error) => Err(error),
        }
    }

    /// Detaches this conversation's root. Best effort: a failure is a trace
    /// line, never an error that masks why the conversation is winding down.
    ///
    /// **不 attach、不 repair。** 过去的实现先 `handle()`（内部会 `attach()` →
    /// `binding.send`，失效即 repair），于是在 runtime 已退休时会冷启动一个
    /// daemon，并幂等地把本该删掉的 root 重建出来——「回收空闲项目」被这一步
    /// 直接抵消（见 `docs/bug-list.md` 的「监督线程只增不减」）。
    ///
    /// 缓存里没有属于当届 runtime 的 root 时，daemon 侧要么不认识它、要么整个
    /// runtime 都没了：这两种情况都没有东西可 detach。
    pub fn detach(&self) {
        let Some(root) = self.live_cached_root(self.binding.generation()) else {
            *self.lock_cached() = None;
            return;
        };
        let Ok(handle) = self.binding.current() else {
            return;
        };
        if let Err(error) = send_request(
            &handle.socket_path,
            IpcRequest::DetachApplicationRoot {
                session_id: root.session_id.clone(),
                root_task_id: root.task_id.clone(),
                capability: root.capability.clone(),
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
        let root_handle = ThreadRoot::new(
            fixed_binding(),
            "thread-a",
            PathBuf::from("/tmp/p"),
            Vec::new(),
        );
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
        let thread_root = ThreadRoot::new(
            fixed_binding(),
            "thread-a",
            PathBuf::from("/tmp/p"),
            Vec::new(),
        );
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
