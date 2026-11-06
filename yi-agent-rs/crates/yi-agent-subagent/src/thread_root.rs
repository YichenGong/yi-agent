//! One conversation's own application root on a shared project daemon.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use yi_agent_lifecycle::types::RootAuthorization;
use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use crate::AttachedRoot;
use crate::binding::{RuntimeBinding, RuntimeHandle, is_liveness_error, is_stale_root_response};

/// 身份层失败时对模型说的话。
///
/// 不可重放的请求（发消息、spawn）遇到陈旧 root 时，重放会产生真实的第二次副作用，
/// 所以只做「丢弃缓存 + 修好运行时」，然后把下一步交给调用方。这句话必须说清
/// 「已经修好了、再来一次就行」，否则模型会以为这个能力坏了。
/// 传输层含糊失败时对模型说的话。
///
/// 修好运行时之后才走到这里，所以"再来一次"是真的可用；重点是先让调用方**检查**
/// 上一次到底有没有发生，而不是闭着眼睛重放一个有副作用的请求。
pub(crate) fn ambiguous_delivery_hint() -> &'static str {
    "the runtime connection was replaced, but this request is not safe to replay: \
     it may already have been applied before the response was lost. The conversation \
     root has been re-attached; check whether the change happened before retrying."
}

pub(crate) fn stale_root_hint() -> &'static str {
    "the conversation root was replaced by a restarted runtime and has been \
     re-attached; the request was NOT re-sent because it changes daemon state. \
     Retry this call once."
}

/// 这是否一个"请求根本没写出去"的失败。
///
/// `connect` 在写任何字节之前就失败了，所以服务端不可能已经执行它。与之相对的
/// `TimedOut` / `UnexpectedEof` / `TruncatedFrame` 都发生在写入之后：应答丢了，
/// 副作用可能已经发生，那才是真正含糊的情形。
fn never_left_the_client(error: &yi_agent_store::ipc::IpcError) -> bool {
    use yi_agent_store::ipc::IpcError;
    match error {
        // Only the errors a `connect` on a unix socket can produce before the request
        // is written. `ConnectionReset` / `ConnectionAborted` look similar but may
        // happen *during* a write that already reached the peer, so they stay on the
        // ambiguous side. `TimedOut` and `UnexpectedEof` are read-side by construction.
        IpcError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

/// 一条请求能否在身份层失败后被重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryPolicy {
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
/// What this conversation's runtime will let it be.
///
/// The three answers are deliberately distinct. Collapsing them into `None` was the
/// bug that let a guardian's refusal look like embedded mode, so delegation fell
/// back to a resume secret the policy had just declined to authorize.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForegroundAuthority {
    /// No guardian serves this runtime: the host may use its own resume secret.
    Embedded,
    /// The guardian authorized this host; delegation proceeds as managed.
    Managed(yi_agent_lifecycle::types::RootAuthorization),
    /// A guardian exists and refused; embedded semantics may **not** be used.
    Refused(String),
}

impl ForegroundAuthority {
    /// The authorization, for callers that only care whether one exists.
    pub fn into_authorization(self) -> Option<yi_agent_lifecycle::types::RootAuthorization> {
        match self {
            Self::Managed(authorization) => Some(authorization),
            Self::Embedded | Self::Refused(_) => None,
        }
    }
}

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
    /// The container address minted for this conversation, per generation.
    ///
    /// Kept apart from `cached` because a container root is *not* an
    /// `AttachedRoot`: it carries no capability, and mixing the two would make it
    /// far too easy to hand a container's address to a legacy path that expects one.
    container: Mutex<Option<SessionContainerAttachment>>,
}

/// A conversation root plus the runtime generation that minted it.
struct CachedRoot {
    generation: u64,
    root: AttachedRoot,
}

/// One attached container's address, and nothing else.
///
/// Deliberately not an [`AttachedRoot`]: a container attach never returns a
/// capability, so there is no field here a caller could mistake for one. The
/// binding that authorises writes is issued separately by `prepare`, and only to
/// a host that proved an identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionContainerAttachment {
    pub session_id: String,
    pub root_id: String,
    /// Where the container's root runs. Cached so a re-attach can be skipped
    /// without re-deriving it from the request.
    pub workspace_root: PathBuf,
    /// The daemon generation this address was resolved against.
    pub generation: u64,
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
            container: Mutex::new(None),
        })
    }

    /// Wraps a root that already exists on a fixed runtime, without attaching.
    ///
    /// The TUI and the daemon's own workers own their daemon for the life of the
    /// process and were handed an already-attached root; they must not re-attach
    /// through the wire. The fixed binding reports generation 0 forever, so the
    /// root stays live and no repair is ever attempted.
    pub fn from_handle(binding: Arc<RuntimeBinding>, root: AttachedRoot) -> Arc<Self> {
        // The root's own session id is the only key available here, and for a
        // single-root host it is also the right one: nothing will attach a second
        // container for this process. A host that *does* know its conversation key
        // must use [`Self::adopt`] instead.
        let thread_id = root.session_id.clone();
        Self::adopt(binding, root, thread_id)
    }

    /// Wraps an already-attached root as *this conversation's* root.
    ///
    /// Separate from [`Self::from_handle`] because the two ids are not the same
    /// thing: `AttachedRoot::session_id` is the daemon's root session, which is
    /// minted fresh on every attach, while a conversation's container key is
    /// `thread:<thread_id>` -- stable across restarts and retries. Adopting with
    /// the session id would make this conversation's container a *second* task:
    /// the root the host attached (keyed by the thread) plus one derived from the
    /// session, and a turn's fence would name whichever the write happened to use.
    ///
    /// A host that keys its application root by the conversation passes that key
    /// here, which is what makes the attached root and the container one task.
    pub fn adopt(
        binding: Arc<RuntimeBinding>,
        root: AttachedRoot,
        thread_id: impl Into<String>,
    ) -> Arc<Self> {
        let thread_id = thread_id.into();
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

    /// Attaches (or adopts) this conversation's *container* on the shared daemon.
    ///
    /// Separate from [`Self::attach`] on purpose. The legacy attach hands back a
    /// capability, which is exactly what a restarted host must not be able to lift
    /// by naming a thread key; this one answers with an address only, and the
    /// authority comes later from [`Self::prepare_session`] against an identity.
    ///
    /// `creation_resume_digest` is sent only for the call that *creates* the
    /// container. Sending it on an adopt would propose a new digest to an existing
    /// container, which is how one host would take over another's session.
    pub fn attach_session_container(
        &self,
        identity: &yi_agent_lifecycle::session_delegation::SessionIdentity,
        operation_id: &str,
        creation_resume_digest: Option<String>,
    ) -> Result<SessionContainerAttachment, crate::session_delegation::SessionClientError> {
        use crate::session_delegation::SessionClientError;

        let generation = self.binding.generation();
        if let Some(cached) = self.live_session_container(generation) {
            return Ok(cached);
        }
        // A revive is tried here rather than inside the binding so the failure stays
        // classifiable: only a request that provably never left this process may be
        // sent again, and a repair+retry has to know that. Repair first when the
        // answered cleanup left no live runtime behind.
        let (shared, generation) = self.binding.current_with_generation();
        let response = match crate::attach::probe_runtime(&shared.socket_path) {
            crate::attach::RuntimeProbe::Healthy => send_request(
                &shared.socket_path,
                IpcRequest::AttachSessionContainer {
                    idempotency_key: application_root_key(&self.thread_id),
                    workspace: self.project_dir.clone(),
                    extra_roots: self
                        .extra_roots
                        .iter()
                        .map(|path| path.to_string_lossy().to_string())
                        .collect(),
                    operation_id: operation_id.to_string(),
                    creation_resume_digest: creation_resume_digest.clone(),
                    identity: Some(identity.clone()),
                },
            )
            .map_err(|error| match error {
                yi_agent_store::ipc::IpcError::Io(io)
                    if matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::NotConnected
                    ) =>
                {
                    SessionClientError::Connect(io.to_string())
                }
                other => SessionClientError::Transport(other.to_string()),
            }),
            probe => Err(SessionClientError::Connect(format!(
                "the runtime at {} is {probe:?}",
                shared.socket_path.display()
            ))),
        }?;
        let generation = self.binding.generation().max(generation);
        let IpcResponse::SessionContainerAttached { root, workspace } = response else {
            return Err(match response {
                IpcResponse::SessionDelegationRejected { failure } => {
                    SessionClientError::Rejected(failure)
                }
                other => SessionClientError::Protocol(format!(
                    "the daemon did not attach a session container: {other:?}"
                )),
            });
        };
        let attachment = SessionContainerAttachment {
            session_id: root.session_id,
            root_id: root.root_id,
            workspace_root: workspace.path,
            generation,
        };
        *self
            .container
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(attachment.clone());
        Ok(attachment)
    }

    /// Drops the cached container address.
    ///
    /// A container attach is cached per generation so a turn does not re-attach
    /// every time. When the daemon this address belongs to turns out to be gone,
    /// the cache is exactly what would keep the host from ever recovering: the
    /// next attach would find a "live" generation and short-circuit onto a socket
    /// nobody serves. Dropping it is what lets the repair+mint happen once.
    pub fn forget_session_container(&self) {
        *self
            .container
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// The runtime binding this conversation resolves through.
    ///
    /// Exposed so a host that must recover from a dead daemon can repair the one
    /// binding the cache, the attach and the tools all read -- repairing a copy
    /// would advance a generation nobody else observes.
    pub fn binding(&self) -> &Arc<RuntimeBinding> {
        &self.binding
    }

    /// The cached container address for the live generation, if any.
    fn live_session_container(&self, generation: u64) -> Option<SessionContainerAttachment> {
        let mut cached = self
            .container
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match cached.as_ref() {
            Some(entry) if entry.generation == generation => Some(entry.clone()),
            Some(_) => {
                *cached = None;
                None
            }
            None => None,
        }
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
    /// Where this conversation's runtime lives, when the binding knows.
    ///
    /// A host resolves the guardian through this. `None` is the honest answer for a
    /// binding that never learned a directory -- the TUI used to be that, which is why
    /// `RuntimeBinding::fixed_in` exists.
    pub fn runtime_directory(&self) -> Option<PathBuf> {
        self.binding.runtime_dir()
    }

    /// The conversation this root belongs to.
    ///
    /// Public because a host needs it to key per-conversation bookkeeping (the
    /// foreground driver's authorization cache, for one) without reaching into the
    /// struct's fields.
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

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
    /// 1. 传输层失败（`is_liveness_error`）：`Idempotent` 修好运行时后重发一次；
    ///    `SingleShot` **只修连接，不重发**。过去这里不看策略、无条件重发，等于把
    ///    "请求根本没送达"当成已知事实——而 `is_liveness_error` 也覆盖
    ///    `TimedOut` 和 `UnexpectedEof`：服务端可能已经执行完，只是应答没回来。
    ///    对 spawn/发消息这类请求，重发的代价是真实的第二次副作用。
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
                // **不能**直接 `handle()`：缓存 root 仍属于当前 generation，
                // `attach()` 会短路返回 Ok，重发于是又打向同一个死 socket——这正是
                // 要修的那个 bug。必须先 `repair()`（它推进 generation，使缓存失效），
                // 再重挂。
                if let Err(repair_error) = self.binding.repair() {
                    // fixed binding 无从 repair（TUI / daemon worker 的情形）。
                    // 这里返回**原始传输错误**，不要把内部措辞泄漏给模型；
                    // 跳过重发也不可能成功，发了只是多一次失败。
                    tracing::debug!(%repair_error, "could not repair a dead runtime");
                    return Err(error.to_string());
                }
                if policy == RetryPolicy::SingleShot && !never_left_the_client(&error) {
                    // 连接已修好，但这次请求的**结果未知**：服务端可能已执行而应答
                    // 丢失。自动重发会让 spawn/发消息发生第二次。
                    //
                    // 例外是"连都没连上"：`connect` 失败意味着一个字节都没写出去，
                    // 结果不可能是"未知"，重发是安全的。把它一并当成含糊失败会让
                    // "终端被关掉"这种最常见的失效形态变成永远无法自愈。
                    return Err(ambiguous_delivery_hint().to_string());
                }
                let handle = self.handle()?;
                send_request(&handle.socket_path, build(&handle)).map_err(|e| e.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// [`Self::send_as_root`] for integration tests.
    ///
    /// The production callers are the seven tools in `lib.rs`, which are the wrong
    /// layer for a test that only needs "what does this transport failure turn into
    /// under this policy". Exposing the seam rather than re-implementing the branch
    /// keeps the test honest: if the policy stops being consulted, this fails.
    #[doc(hidden)]
    pub fn send_as_root_for_test<F>(
        &self,
        policy: RetryPolicy,
        build: F,
    ) -> Result<IpcResponse, String>
    where
        F: Fn(&RuntimeHandle) -> IpcRequest,
    {
        self.send_as_root(policy, build)
    }

    /// Asks the guardian for authority over this conversation's root.
    ///
    /// A separate step from [`Self::bind_foreground_driver`] on purpose: the
    /// authorization is the narrow secret the daemon will accept a bind with, and a
    /// caller that fails to bind must be able to drop it rather than hold it.
    ///
    /// Every `None` here is a *normal* answer, not an error to paper over:
    ///
    /// * no runtime directory (a fixed binding: the TUI and daemon workers) -- this
    ///   runtime has no guardian of ours to ask;
    /// * no guardian serving that directory -- embedded mode;
    /// * the guardian refused -- a lease-only observer, or a build without the
    ///   capability.
    ///
    /// In all three the host should keep running the conversation with legacy
    /// semantics. Treating them as failures would make a supported deployment look
    /// broken, and the plan's promise is limited to managed runtimes.
    pub fn authorize_foreground_root(
        &self,
        lease: &yi_agent_lifecycle::client::FrontendLease,
        operation_id: &str,
    ) -> Result<Option<RootAuthorization>, String> {
        Ok(self
            .authority_decision(lease, operation_id)?
            .into_authorization())
    }

    /// The same question, answered with the *reason*, not flattened to `None`.
    ///
    /// Delegation needs the difference: "no guardian exists" is embedded mode and
    /// may use the host's own resume secret, while "a guardian exists and said no"
    /// must never fall back to it -- that would turn a policy refusal into
    /// permission. Both are normal outcomes and neither is an error.
    pub fn foreground_authority(
        &self,
        lease: &yi_agent_lifecycle::client::FrontendLease,
        operation_id: &str,
    ) -> Result<crate::thread_root::ForegroundAuthority, String> {
        self.authority_decision(lease, operation_id)
    }

    fn authority_decision(
        &self,
        lease: &yi_agent_lifecycle::client::FrontendLease,
        operation_id: &str,
    ) -> Result<crate::thread_root::ForegroundAuthority, String> {
        let Some(runtime_dir) = self.binding.runtime_dir() else {
            return Ok(crate::thread_root::ForegroundAuthority::Embedded);
        };
        let control_path = runtime_dir.join(yi_agent_lifecycle::GUARDIAN_SOCKET_FILE);
        if !control_path.exists() {
            return Ok(crate::thread_root::ForegroundAuthority::Embedded);
        }
        let intent = yi_agent_lifecycle::foreground::RootBindIntent {
            operation_id: operation_id.to_string(),
            thread_key: application_root_key(&self.thread_id),
            workspace: self.project_dir.clone(),
            // Linked roots are already part of the attach; re-sending them here would
            // describe a second set and the daemon would compare it against the one it
            // recorded at attach time.
            extra_roots: Vec::new(),
            previous_root_capability: None,
        };
        let deadline =
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(5));
        match lease.authorize_root(&intent, &deadline) {
            Ok(authorization) => Ok(crate::thread_root::ForegroundAuthority::Managed(
                authorization,
            )),
            // A refusal is the guardian's policy answer: this host may run the turn
            // without activity reporting, but it must not present itself as managed.
            // Only a transport failure is worth surfacing, and even then the caller
            // decides -- the conversation is not lost.
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::debug!(%error, thread_id = %self.thread_id, "foreground authority refused");
                Ok(crate::thread_root::ForegroundAuthority::Refused(
                    error.to_string(),
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
                tracing::debug!(%error, thread_id = %self.thread_id, "foreground lifecycle unsupported");
                Ok(crate::thread_root::ForegroundAuthority::Embedded)
            }
            Err(error) => Err(error.to_string()),
        }
    }

    /// Binds this host as the root's foreground driver, or reports why it cannot.
    ///
    /// `Ok(None)` means this runtime does not serve the contract; the caller keeps
    /// legacy semantics. The driver itself is returned so the host can open and close
    /// turns with it.
    pub fn bind_foreground_driver(
        self: &Arc<Self>,
        authorization: RootAuthorization,
        operation_id: &str,
    ) -> Result<Option<crate::foreground::ForegroundTurn>, String> {
        match crate::foreground::ForegroundTurn::bind(
            // The driver holds the root for the life of the turn, so it takes the
            // caller's handle rather than minting a second one: two `ThreadRoot`s for
            // one conversation would each cache their own attach result.
            Arc::clone(self),
            authorization,
            operation_id.to_string(),
        ) {
            Ok(turn) => Ok(Some(turn)),
            Err(error) => {
                tracing::debug!(%error, thread_id = %self.thread_id, "foreground driver unavailable");
                Ok(None)
            }
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
    // ---------------------------------------------------------------------
    // The guardian entry point, against a real guardian over a real socket.
    //
    // Everything below `authorize_foreground_root` is covered where it lives: the
    // lease gate in `yi-agent-supervisors`, the capability minting in the guardian's
    // root authority table, the bridge in the daemon. What is only here is the host's
    // view: which key it asks for, what a refusal looks like, and what an absent
    // guardian looks like. Those three answers are what a host branches on, so they
    // are pinned against the production guardian rather than a stub.
    // ---------------------------------------------------------------------

    use std::path::PathBuf as TestPathBuf;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
    use std::time::{Duration as TestDuration, Instant as TestInstant};

    use yi_agent_lifecycle::client::FrontendLease;
    use yi_agent_lifecycle::deadline::Deadline;
    use yi_agent_lifecycle::types::{ArtifactIdentity, ClientKind, LeaseMode, RuntimeScope};
    use yi_agent_supervisors::lifecycle::guardian::{
        GUARDIAN_SOCKET_FILE, serve_with_bridge_for_test,
    };

    /// A guardian served over a real socket, plus a daemon-side peer for the
    /// authority bridge.
    ///
    /// The peer is not the real daemon on purpose: the daemon's own exchange is tested
    /// against a real `Daemon` in `yi-agent-store`. Here it only has to answer the two
    /// frames the guardian sends, so that a request that *should* succeed does.
    struct GuardianFixture {
        directory: tempfile::TempDir,
        runtime_dir: TestPathBuf,
        stop: Arc<AtomicBool>,
        server: Option<std::thread::JoinHandle<std::io::Result<()>>>,
        peer: Option<std::thread::JoinHandle<()>>,
    }

    impl GuardianFixture {
        fn start() -> Self {
            // `/tmp` keeps the socket path inside the platform's `sun_path` limit; the
            // randomness lives in the directory name rather than the root.
            let directory = tempfile::Builder::new()
                .prefix("fgroot")
                .tempdir_in("/tmp")
                .expect("a temp dir");
            let runtime_dir = directory.path().join("rt");
            std::fs::create_dir_all(&runtime_dir).expect("the runtime dir");
            let scope = RuntimeScope {
                key: runtime_dir.to_string_lossy().into_owned(),
                runtime_dir: runtime_dir.clone(),
                project_key: directory.path().to_string_lossy().into_owned(),
                instance_home: directory.path().join("instance"),
            };

            let (guardian_end, peer_end) =
                std::os::unix::net::UnixStream::pair().expect("a socket pair");
            // Production hands the guardian a non-blocking descriptor; a blocking one
            // here could not catch a timeout that is silently ignored.
            guardian_end
                .set_nonblocking(true)
                .expect("the guardian end is non-blocking");

            let peer = std::thread::spawn(move || answer_bridge(peer_end));
            let stop = Arc::new(AtomicBool::new(false));
            let server_stop = Arc::clone(&stop);
            let server = std::thread::spawn(move || {
                serve_with_bridge_for_test(
                    scope,
                    ArtifactIdentity {
                        build_id: "test-build".into(),
                        display_version: "0.0.0".into(),
                        executable: "/usr/bin/true".into(),
                        sha256: "00".into(),
                        schema_min: 1,
                        schema_max: 1,
                        handover_version: 1,
                        business_protocol_min: 1,
                        business_protocol_max: 1,
                        capabilities: Vec::new(),
                    },
                    move || server_stop.load(AtomicOrdering::SeqCst),
                    guardian_end,
                )
            });

            let socket = runtime_dir.join(GUARDIAN_SOCKET_FILE);
            let readiness = TestInstant::now() + TestDuration::from_secs(10);
            while !socket.exists() {
                assert!(
                    TestInstant::now() < readiness,
                    "the guardian never bound its control socket"
                );
                std::thread::sleep(TestDuration::from_millis(5));
            }

            Self {
                directory,
                runtime_dir,
                stop,
                server: Some(server),
                peer: Some(peer),
            }
        }

        fn control_socket(&self) -> TestPathBuf {
            self.runtime_dir.join(GUARDIAN_SOCKET_FILE)
        }

        /// A `ThreadRoot` whose binding points at this fixture's runtime directory.
        fn root(&self, thread_id: &str) -> Arc<ThreadRoot> {
            ThreadRoot::new(
                RuntimeBinding::for_runtime_dir(
                    test_config(self.directory.path()),
                    self.runtime_dir.clone(),
                    RuntimeHandle {
                        socket_path: self.control_socket(),
                        workspace_root: self.directory.path().to_path_buf(),
                        session_id: thread_id.to_string(),
                        task_id: "root".into(),
                        capability: "cap".into(),
                    },
                ),
                thread_id,
                self.directory.path().to_path_buf(),
                Vec::new(),
            )
        }

        fn lease(&self, id: &str, mode: LeaseMode) -> FrontendLease {
            FrontendLease::register(
                &self.control_socket(),
                id.to_string(),
                ClientKind::App,
                mode,
                &Deadline::after(TestDuration::from_secs(5)),
            )
            .expect("the guardian admits the client")
        }
    }

    impl Drop for GuardianFixture {
        fn drop(&mut self) {
            self.stop.store(true, AtomicOrdering::SeqCst);
            // Wake the accept loop so its stop check runs now, not on the next tick.
            let _ = std::os::unix::net::UnixStream::connect(self.control_socket());
            if let Some(server) = self.server.take() {
                let _ = server.join();
            }
            if let Some(peer) = self.peer.take() {
                let _ = peer.join();
            }
        }
    }

    /// `RuntimeConfig` has no `Default`, so the test writes the fields out. Same shape
    /// as `attach.rs`'s `base_config`, trimmed to what a path lookup could ever read.
    fn test_config(workdir: &std::path::Path) -> yi_agent_runtime::config::RuntimeConfig {
        yi_agent_runtime::config::RuntimeConfig {
            provider: "openai".into(),
            api_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "base-model".into(),
            api_style: yi_agent_core::ApiStyle::Auto,
            reasoning_effort: None,
            max_turns: 1,
            max_resident_subagents: 0,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: workdir.to_path_buf(),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_ratio: 80,
            model_context_length: None,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: true,
            model_injected: false,
            ..yi_agent_runtime::config::RuntimeConfig::load_lenient(
                &yi_agent_runtime::config::ConfigOverrides::default(),
            )
            .expect("a lenient config")
        }
    }

    /// Answers the authority bridge the way the daemon does.
    fn answer_bridge(mut stream: std::os::unix::net::UnixStream) {
        use std::io::Write;
        use yi_agent_lifecycle::foreground::BridgeFrame;

        while let Ok(raw) = yi_agent_lifecycle::codec::read_frame(&mut stream) {
            let Ok(frame) = yi_agent_lifecycle::codec::decode::<BridgeFrame>(&raw) else {
                return;
            };
            let answer = match frame {
                BridgeFrame::Prepare {
                    nonce,
                    root_id,
                    requested_seq,
                    ..
                } => BridgeFrame::Challenge {
                    nonce,
                    root_id,
                    authority_seq: requested_seq,
                },
                BridgeFrame::Authority(message) => {
                    let stamp = match &message {
                        yi_agent_lifecycle::foreground::AuthorityMessage::Install {
                            stamp, ..
                        }
                        | yi_agent_lifecycle::foreground::AuthorityMessage::Renew { stamp } => {
                            stamp
                        }
                        _ => return,
                    };
                    BridgeFrame::Committed {
                        nonce: stamp.challenge_nonce.clone(),
                        root_id: stamp.root_id.clone(),
                        binding_epoch: 1,
                    }
                }
                _ => return,
            };
            if yi_agent_lifecycle::codec::write_frame(&mut stream, &answer).is_err() {
                return;
            }
            let _ = stream.flush();
        }
    }

    #[test]
    fn foreground_root_entry_a_business_lease_obtains_authority_for_its_own_thread() {
        let fixture = GuardianFixture::start();
        let lease = fixture.lease("host-run", LeaseMode::Business);
        let root = fixture.root("session-abc");

        let authorization = root
            .authorize_foreground_root(&lease, "op-1")
            .expect("the request reaches the guardian")
            .expect("a live business lease is authorized");

        assert!(
            !authorization.root_id.is_empty(),
            "the guardian allocates a root id"
        );
        // A capability derived from the thread key would be guessable by anyone who
        // knows the conversation; the whole point is that it is generated.
        let thread_key = application_root_key("session-abc");
        assert_ne!(authorization.capability, thread_key);
        assert!(!authorization.capability.contains(&thread_key));
    }

    #[test]
    fn foreground_root_entry_a_lease_only_observer_is_told_no_without_failing() {
        let fixture = GuardianFixture::start();
        let lease = fixture.lease("host-observer", LeaseMode::LeaseOnly);
        let root = fixture.root("session-observer");

        // `None`, not an error: the host can still run the conversation, it just cannot
        // report activity for it. Turning this into `Err` would make a supported
        // deployment look broken.
        assert!(
            root.authorize_foreground_root(&lease, "op-1")
                .expect("a policy refusal is not a transport failure")
                .is_none(),
            "a lease-only observer must not obtain root authority"
        );
    }

    #[test]
    fn foreground_root_entry_a_runtime_without_a_guardian_answers_none() {
        // The embedded deployment: a binding for a directory with no guardian. `None`
        // means the contract is not offered here, which is where a host reads it.
        let directory = tempfile::Builder::new()
            .prefix("fgabs")
            .tempdir_in("/tmp")
            .expect("a temp dir");
        let runtime_dir = directory.path().join("rt");
        std::fs::create_dir_all(&runtime_dir).expect("the runtime dir");
        let root = ThreadRoot::new(
            RuntimeBinding::for_runtime_dir(
                test_config(directory.path()),
                runtime_dir.clone(),
                RuntimeHandle {
                    socket_path: runtime_dir.join(GUARDIAN_SOCKET_FILE),
                    workspace_root: directory.path().to_path_buf(),
                    session_id: "s".into(),
                    task_id: "root".into(),
                    capability: "cap".into(),
                },
            ),
            "session-embedded",
            directory.path().to_path_buf(),
            Vec::new(),
        );
        // No runtime directory at all is the *other* way to have no guardian, and it
        // must answer the same. A fixed binding is what the TUI holds.
        let fixed = ThreadRoot::new(
            fixed_binding(),
            "session-tui",
            TestPathBuf::from("/tmp/repo"),
            Vec::new(),
        );
        // A lease cannot even be registered without a guardian, so this test uses a
        // fixture's lease against a *different* runtime: the shape under test is the
        // path lookup, not the gate.
        let other = GuardianFixture::start();
        let lease = other.lease("host-elsewhere", LeaseMode::Business);
        assert!(
            root.authorize_foreground_root(&lease, "op-1")
                .expect("an absent guardian is not an error")
                .is_none(),
            "no guardian serving the directory means the contract is not offered"
        );
        assert!(
            fixed
                .authorize_foreground_root(&lease, "op-1")
                .expect("a fixed binding is not an error")
                .is_none(),
            "a fixed binding never reaches a guardian at all"
        );
    }

    #[test]
    fn foreground_host_reuses_one_authorization_per_conversation() {
        use crate::foreground::ForegroundHost;

        let fixture = GuardianFixture::start();
        let host = ForegroundHost::connect(
            &fixture.runtime_dir,
            yi_agent_lifecycle::types::ClientKind::App,
            "host-app".to_string(),
        )
        .expect("connecting to a served runtime is not a failure")
        .expect("the fixture serves a guardian");

        let root = fixture.root("session-host");
        let first = host
            .authorization(&root, "bind-1")
            .expect("the request reaches the guardian")
            .expect("a live business lease is authorized");
        // A second ask must not mint a second capability: the cached one is reused, so
        // a transient refusal cannot disable a conversation that is already reporting.
        let second = host
            .authorization(&root, "bind-1")
            .expect("the cached authorization is returned without asking again")
            .expect("the cache is populated");
        assert_eq!(
            first.capability, second.capability,
            "re-authorizing every turn would churn the guardian's authority table"
        );

        // A different conversation gets its own root: two conversations in one
        // directory must never share one authorization.
        let other = root_of(&fixture, "session-host-2");
        let third = host
            .authorization(&other, "bind-2")
            .expect("a second conversation is served too")
            .expect("a live business lease is authorized");
        assert_ne!(
            third.root_id, first.root_id,
            "each conversation must key its own root"
        );
    }

    #[test]
    fn foreground_host_without_a_lease_offers_nothing() {
        use crate::foreground::ForegroundHost;

        // The embedded deployment: a runtime directory no guardian serves.
        let directory = tempfile::Builder::new()
            .prefix("fgnolease")
            .tempdir_in("/tmp")
            .expect("a temp dir");
        let runtime_dir = directory.path().join("rt");
        std::fs::create_dir_all(&runtime_dir).expect("the runtime dir");
        assert!(
            ForegroundHost::connect(
                &runtime_dir,
                yi_agent_lifecycle::types::ClientKind::App,
                "host-embedded".to_string(),
            )
            .expect("an absent guardian is not an error")
            .is_none(),
            "no guardian means no host authority"
        );
    }

    /// A second conversation on the same fixture runtime.
    fn root_of(fixture: &GuardianFixture, thread_id: &str) -> Arc<ThreadRoot> {
        fixture.root(thread_id)
    }
}
