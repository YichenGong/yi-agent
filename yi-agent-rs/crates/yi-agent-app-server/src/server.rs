//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::pairing::PairingState;
// 这几个 `use` 同时供 lib 与 test target 使用：`server/tests/*` 与
// `server/*_tests.rs` 走 `use super::*`，只编进 test target。`cargo fix` 只看
// lib target，会把"只在测试里被点名"的名字裁掉，导致 `cargo test` 编译失败。
// 故统一放行——可达性同样是契约。
#[allow(unused_imports)]
use crate::lead_loop::{ThreadSnapshot, decide_wake, render_digest};
#[allow(unused_imports)]
use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, PROTOCOL_VERSION, RequestEnvelope, RequestId,
    ReverseRequest, RpcError, Scope, SubagentTotals, ThreadStatus,
};
#[allow(unused_imports)]
use crate::session::{
    CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt,
};
#[allow(unused_imports)]
use crate::session_driver::{DriverAction, DriverActionKind, DriverState, SessionLauncher};
#[allow(unused_imports)]
use crate::translate::Translator;
use crate::transport::{MessageReader, MessageWriter};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_subagent::attach::AttachFailure;
use yi_agent_subagent::binding::RuntimeBinding;
use yi_agent_subagent::thread_root::ThreadRoot;

pub(crate) mod runtime_binding;

// 说明（适用于本节所有 re-export 块）：
// 搬出去之后，这些名字必须继续从 `crate::server::X` 可达——`rpc/*`、`ws.rs`、
// `mcp_rpc.rs`、`thread_store.rs`、`server/tests/*`（后者用 `use super::*`）都
// 依赖这条路径。`unused_imports` 只在「lib target 里恰好没有使用者」时才会报，
// 而 `cargo fix` 会据此把 re-export 删掉、连带打断 test target 与子模块。故本节
// 统一 `#[allow(unused_imports)]`：可达性是契约，不是死代码。
#[allow(unused_imports)]
pub(crate) use runtime_binding::{
    SESSION_PRESERVE_THINKING, current_session_preserve_thinking, extract_prompt, home_dir,
    instance_home_dir, production_factory, project_key, skills_home_dir,
    with_session_preserve_thinking,
};

pub(crate) mod rpc_io;
pub(crate) mod task_bridge;
pub(crate) mod watchers;

#[allow(unused_imports)]
pub(crate) use crate::protocol::ResponseEnvelope;
#[allow(unused_imports)]
pub(crate) use rpc_io::{
    DEVICE_REGISTRY, IMAGE_READ_CHUNK_BYTES, IMAGE_READ_MAX_BYTES, device_registry, err_response,
    forget_ws_device, install_device_registry, ok_response, write_notification,
    write_notification_await, write_response, ws_client_for_device,
};
#[allow(unused_imports)]
pub(crate) use task_bridge::{
    cancel_thread_children, forward_to_daemon, list_task_summaries, read_task_trace,
};
#[allow(unused_imports)]
pub(crate) use watchers::{
    children_for_thread, stream_trace, subagent_totals, trace_rows, watch_children, watch_processes,
};

pub(crate) mod thread_index;

#[allow(unused_imports)]
pub(crate) use thread_index::{
    Activation, SyncAction, SyncHint, TAG_NAME_MAX_LEN, attach_delegation, collect_pinned,
    decision_label, find_thread_dir, image_media_type, is_load_bearing, item_id,
    parse_client_decision, parse_resume_hints, plan_sync, record_last_item_id,
    require_known_thread, require_thread_id, resolve_thread_cwd, saturate_u32, snapshot_for_watch,
    socket_for_thread, store_for, store_lookup, thread_cwd, thread_status, thread_summary_json,
    thread_summary_json_with_status, update_status, worktree_json,
};

pub(crate) mod migration;
pub(crate) mod plugin_bridge;
pub(crate) mod watchman;

#[allow(unused_imports)]
pub(crate) use migration::{
    migrate_legacy_model_keys_on_startup, migrate_legacy_registry_file,
    migrate_legacy_resident_requests_on_startup,
};
#[allow(unused_imports)]
pub(crate) use plugin_bridge::{
    PLUGIN_READY_TIMEOUT, PluginQueryError, PluginSummary, list_installed_plugins, plugin_query,
    plugin_query_self, plugin_query_socket_ready, plugin_scope, project_arg,
    spawn_resident_watch_loop, wait_for_plugin_query_socket,
};
#[allow(unused_imports)]
pub(crate) use watchman::{
    PLUGIN_READY_POLL, agent_trace_row, ensure_watchman_installed, production_watchman_install,
    production_watchman_uninstall, project_has_plugin_manifest, should_evict, skills_home_from,
    wait_for_plugin_query_socket_within, watchman_home_from,
};

pub(crate) mod assembly;

pub(crate) mod turn;

pub(crate) mod driver;

#[allow(unused_imports)]
pub(crate) use driver::{
    DRIVER_TICK, DeltaCoalescer, EVICT_IDLE_AFTER, LEAD_LOOP_SENDER, LEAD_TICK, LeadGroup,
    LeadTarget, LeadTickDelivery, PERMISSION_TIMEOUT, POWER_TICK, PartialTurnInput, ServeLauncher,
    TITLE_TIMEOUT, driver_tick, evict_idle_runtimes, lead_tick_once, project_root_of,
    resolve_lead_target, run_thread_driver, snapshot_of, take_changed_digests,
};

#[allow(unused_imports)]
pub(crate) use turn::{
    PreparedTurn, REPLAY_CHUNK_BYTES, REPLAY_CHUNK_MAX_ITEMS, TurnPrepareError, apply_session,
    apply_session_command, build_partial, checkpoint, chunk_items_for_replay,
    completed_items_agent_text, emit_item, finished_event, generate_title_for_first_input,
    interrupt_and_wait_for_persist, lead_reply_text, linked_extra_roots, opening_user_item,
    persist_and_finish_turn, prepare_turn_core, route_client_response, settle_pending_lead_replies,
    start_thread_core, start_turn_core, turn_prepare_rpc_error,
};

#[allow(unused_imports)]
pub(crate) use assembly::{
    RuntimeTooling, SESSION_EXTRA_ROOTS, SESSION_MODEL_REF, SESSION_REASONING_EFFORT,
    attach_cwd_runtime, attached_runtimes, build_runtime_tooling, current_session_extra_roots,
    current_session_model_ref, current_session_reasoning_effort, detach_unused_runtimes,
    rebuild_thread_agent_with_theme, register_theme_tool, registry_with_theme_tool,
    with_session_extra_roots, with_session_model_ref, with_session_reasoning_effort,
    wrap_for_delegation,
};

/// `SystemClock::now` / `since` 来自 `Clock` trait，这里只借用方法，不带名字。
use yi_agent_power::clock::Clock as _;

/// 决定「这个项目的 runtime 现在能不能回收」所需的全部事实。
///
/// 采集与决策分离：调用方负责从 `threads` / 磁盘 / 常驻登记里取事实，本结构
/// 只承载它们，因此 [`should_evict`] 是纯函数，可以不起任何进程就测到。
pub(crate) struct IdleFacts {
    /// 该项目最后一次「有活干」的时刻（attach、起 turn 都算）。
    pub last_active: Instant,
    /// 项目里存在插件清单（`.yi-agent/supervisors/*.json`）。
    ///
    /// 有清单就意味着这个项目的 daemon 正在给插件提供 socket；回收会连带把
    /// 插件停掉，所以这类项目一律豁免。判据用「有没有清单」而不是写死某个
    /// 插件名：宿主不认识具体插件。
    pub has_plugin_manifest: bool,
    /// 该项目在 `resident-daemons.json` 里被要求常驻。
    pub is_resident: bool,
    /// 还有回合在跑（含子任务）。判据是「有没有活干」，不是「多久没人碰」。
    pub has_running_turn: bool,
}

/// driver task → 主循环的完成事件。
pub(crate) enum TurnEvent {
    Finished {
        thread_id: String,
        turn_id: String,
        /// 本轮的助手回复文本(取法见 [`lead_reply_text`])。
        ///
        /// 主循环必须在这一刻拿到它:阻塞式 `ask_lead` 的等待方按 `request_id`
        /// 收口,而收口用的正是这串文本。收尾之后 `items` 已 move 进落盘记录,
        /// 再想取就得回读磁盘(还会被 compact 丢锚点),故随事件带上来。
        ///
        /// `None` = 本轮没有可回填的助手文本(失败轮、纯工具轮、写帧失败而中断
        /// 的轮)。这类完成事件**不得**被当成一条空回复回填。
        reply: Option<String>,
        /// 本轮是替哪条阻塞式 `ask_lead` 跑的(见 [`TurnPrompt::lead_request_id`])。
        ///
        /// 收尾时**只**用它回填那一个等待方,故两个会话同时阻塞在同一个组长上时,
        /// 各自只收到**自己那一轮**的文本(见 [`settle_pending_lead_replies`])。
        /// `None` = 这一轮与任何等待方无关(普通用户 turn、看板卡、单向投递、失败
        /// 轮等),收尾时谁都不回填。
        lead_request_id: Option<String>,
    },
}

/// 工厂产出的 agent 及其重建所需的部件。
///
/// 项目 runtime attach 成功后,该 thread 的 agent 要用新的工具集与新的权限根**重建**
/// (见 `wrap_for_delegation`),所以这里只带出重建必须沿用的那些部件:重建 provider
/// 会重读凭据并多一个 client,重建决定通道会打断正在等待审批的 turn。被替换掉的
/// 工具集与权限检查器留在 `RuntimeTooling` 里,正是因为它俩不该出现在这张单子上。
pub(crate) struct BuiltAgent {
    pub(crate) agent: yi_agent_core::Agent,
    pub(crate) provider: Arc<dyn yi_agent_core::Provider>,
    pub(crate) config: yi_agent_core::AgentConfig,
    /// 交互模式下的权限决定回传端;None 表示该 agent 不需要审批。
    pub(crate) decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    pub(crate) decision_rx: Option<yi_agent_runtime::bootstrap::DecisionReceiver>,
    /// 刷新 skills catalog 的句柄;无 skills 服务时为 `None`。
    pub(crate) catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    /// 该 agent 的运行时 yolo 开关;`ThreadSession` 存它以便 RPC 即时切换。
    pub(crate) yolo: yi_agent_core::autonomy::YoloSwitch,
    /// 支撑该 agent 工具集里的进程工具的 manager。**与生效的工具集同行**：
    /// `build_runtime_tooling` 换掉工具集时必须一并替换它。
    pub(crate) process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// MCP 管理器;配置缺失时为 `None`。`mcp/set` 与每轮配置变更检查用它。
    pub(crate) mcp: Option<Arc<yi_agent_mcp::McpManager>>,
}

/// 每 cwd 的已 attach runtime。app-server 是长驻多 cwd 进程,而 runtime 是按项目
/// 划分的,因此 attach 以 canonical cwd 为键、懒初始化。
///
/// 失败也缓存(存 `Err`):否则纯目录下每开一个 thread 都会重跑一遍注定失败的
/// bring-up(git 检查 + daemon 起停尝试)。
///
/// 缓存项存的是结构化的 [`yi_agent_subagent::attach::AttachFailure`] 而不是字符串,
/// 因为「能不能重试」必须和失败原因一起记住: **瞬时**失败(对端 daemon 还在启动
/// 窗口、socket 尚未 bind)一旦被当成终态, 整个进程的后续会话都会永久失去
/// `spawn_agent`。2026-11-01 的 desktop 冷启动就是这样丢的委派能力。
pub(crate) type ProjectRuntimes =
    Arc<StdMutex<HashMap<PathBuf, Result<Arc<RuntimeBinding>, AttachFailure>>>>;

/// 每会话自己的 root,建在项目共享 binding 之上。
///
/// runtime(daemon)按项目共享,是 G2;root 按会话独立,是这次改动的核心——
/// 一个 root 才是一份 `MAX_DIRECT_CHILDREN` 预算,两个会话共用一个 root 就会共用
/// 一份预算,这正是桌面端「一个会话满了、全目录都满」的成因。
pub(crate) type ThreadRoots = Arc<StdMutex<HashMap<String, Arc<ThreadRoot>>>>;

/// 模型驱动的视图帧表：每会话一帧，**内存态**（不落盘，重启即空）。
///
/// 与 `git_diff` 句柄并列为 [`RuntimeAttachments`] 字段，理由同 `theme` /
/// `lead_inbox`：`thread/view/read` 在主循环里读它，而写帧的 watcher 是另一个
/// task，自造一份就会读写两张不相干的表。
pub(crate) type ViewFrames = Arc<tokio::sync::Mutex<HashMap<String, crate::git_diff::ViewFrame>>>;

/// 本进程的 runtime 接线:daemon 按项目共享,root 按会话持有。
///
/// 两者成对传递,调用点无法只更新一半——只换了 daemon 映射、忘了 root 映射,
/// 正是「每个会话应当有自己的 root」这条不变量最危险的破坏方式。
pub(crate) struct RuntimeAttachments {
    pub(crate) runtimes: ProjectRuntimes,
    pub(crate) thread_roots: ThreadRoots,
    /// Where `models.json` lives. Injectable for the same reason `resident_dir`
    /// is: `model/*` must be testable without writing the developer's real
    /// machine-level catalog (and its API keys).
    pub(crate) models_path: PathBuf,
    /// Where `websearch.json` lives. Injectable for the same reason
    /// `models_path` is: the 网页搜索 panel must be testable without writing
    /// the developer's real machine-level config.
    pub(crate) websearch_path: PathBuf,
    /// Where `settings.json` lives. Injectable for the same reason
    /// `models_path` is: the runtime settings panel must be testable without
    /// writing the developer's real machine-level config.
    pub(crate) settings_path: PathBuf,
    /// 组长开关的**生效值**，装配时算一次。
    ///
    /// 只放结论、不放路径：若把 `flags.json` 路径传下来再让 `serve()` 解析，
    /// 就会用到进程级 `build_kind()`（测试里恒为 `release`），注入永远失效。
    /// 生产的构造点统一调 [`crate::lead_gate::from_process_environment`]，
    /// 测试直接给 `true` / `false`。
    pub(crate) lead_agent_enabled: bool,
    /// Where `preferences.json` lives, for the `onboarding_dismissed` marker.
    /// Injectable for the same reason `models_path` is: dismissing the first-run
    /// guide must never touch the developer's real preferences file.
    pub(crate) onboarding_preferences_path: PathBuf,
    /// Where the generic resident-daemon registry lives (`$HOME/.yi-agent`).
    /// Injectable: `resident/*` must be testable without writing the
    /// developer's real resident registry.
    pub(crate) resident_dir: PathBuf,
    /// 拼 skill 根目录用的「家目录」。Injectable for the same reason
    /// `resident_dir` is: `skill/list` / `skill/read` must be testable without
    /// reading the developer's real `~/.yi-agent/skills`.
    pub(crate) skills_home: PathBuf,
    /// 实例状态根（`remote.json` / `remote-status.json` 所在目录）。可注入的
    /// 理由同 `resident_dir`：手机端项目清单必须能在测试里读隔离 tempdir。
    pub(crate) remote_dir: PathBuf,
    /// 桌面主题句柄：`ui/settings/read|write` 维护它，每个 thread 的
    /// `set_theme` 工具也持同一实例，故任一路径改主题都会落盘 + 广播。
    pub(crate) theme: crate::theme_tool::ThemeHandle,
    /// Git Diff 焦点句柄：`show_git_diff` 工具持它广播，watcher 翻译成
    /// `ui/gitDiff/focus` 通知。每个 thread 的工具与 watcher 共用同一实例。
    pub(crate) git_diff: crate::git_diff_tool::GitDiffHandle,
    /// 模型驱动的视图帧表（每会话一帧，内存态）。与 `git_diff` 句柄并列。可
    /// 注入的理由同 `lead_inbox`：`thread/view/read` 读它与写它的 watcher 必须
    /// 是**同一个** `Arc`，测试也要能在主循环之外往同一张表里塞帧。
    pub(crate) view_frames: ViewFrames,
    /// 安装常驻值守（macOS LaunchAgent）。可注入的理由同 `launcher`：测试里
    /// 绝不能真的调 `launchctl`，也绝不能碰用户真实的 `~/Library/LaunchAgents`。
    pub(crate) watchman_install: WatchmanInstall,
    /// 卸载常驻值守。可注入的理由同 `watchman_install`。
    pub(crate) watchman_uninstall: WatchmanUninstall,
    /// 值守安装/卸载所针对的 home（生产：真实 `$HOME`；测试：tempdir）。
    /// 它是「往哪个 `Library/LaunchAgents` 写」与「读哪个 plist 判已装」的
    /// 唯一来源，抽出来测试才不会误判真实 home 里的安装状态。
    pub(crate) watchman_home: PathBuf,
    /// 按名字解析插件可执行文件（`plugin/install` 的第二步）。可注入的理由
    /// 见 [`ResolvePlugin`]：靠改 `PATH` 来测会在并行用例间互相污染。
    pub(crate) resolve_plugin: ResolvePlugin,
    /// 拉起项目 daemon（`plugin/install` 的最后一步）。可注入的理由同
    /// `watchman_install`：生产实现会真的 `spawn` 一个 detached 进程。
    pub(crate) ensure_daemon: EnsureDaemon,
    /// 组长信箱:普通会话经 `ask_lead` 投递的消息进这里,主循环的
    /// `select!` 分支消费它。与 `theme`/`git_diff` 同样的注入理由——测试要在
    /// 主循环之外拿到**同一个**实例投递,自造一份会投进没人听的通道。
    pub(crate) lead_inbox: crate::lead::LeadInbox,
    /// 阻塞式 `ask_lead` 的回信登记表:普通会话登记 `request_id` 后 await,
    /// 主循环在组长那轮收尾时按 id 回填。
    ///
    /// 必须与 [`Self::lead_inbox`] 同一注入理由,且**必须真的传进装配**
    /// (`attach_delegation` → `build_runtime_tooling` → `AskLeadTool::with_replies`):
    /// 漏传一个字段没有任何编译错误,`blocking: true` 只是静默退化成 P1 的显式
    /// 拒绝——这正是本任务最危险的一处,故测试用 `Harness::lead_repliers` 驱动
    /// 真实装配(见 `a_blocking_ask_lead_receives_the_lead_reply`)。
    pub(crate) lead_replies: crate::lead::LeadReplies,
    /// 组长写通道:组长工具投递「取消 / 打回」,主循环消费。与 `lead_inbox` 相对、
    /// 同一注入理由——测试要在主循环之外拿同一个实例投递。
    pub(crate) lead_control: crate::lead_control::LeadControlInbox,
    /// 组长写结果登记表:工具侧登记 `request_id`,主循环执行完回填。
    pub(crate) lead_control_replies: crate::lead_control::LeadControlReplies,
}

/// Installs the resident watchman (macOS LaunchAgent) for `exe` under `home`;
/// see [`yi_agent_supervisors::watchman::install`].
pub(crate) type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;

/// Removes the resident watchman under `home`; see
/// [`yi_agent_supervisors::watchman::uninstall`].
pub(crate) type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// 按插件名解析可执行文件；生产即
/// [`crate::plugin_worktree::resolve_plugin_command`]。
///
/// 之所以是做成一等可注入句柄、而不是让 handler 直接调那个函数：它的实现读
/// `PATH`，而测「按请求里的名字解析」就必须让某个名字可解析。若靠改进程级
/// `PATH`，同进程里并行跑的其它用例会在一小段窗口里看到假的 `PATH`（它们
/// 可能正在 `spawn` 子进程）——那是**偶发**失败，不是确定性的证据。注入之后
/// 测试完全不需要碰进程环境。
pub(crate) type ResolvePlugin = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// 保证某个项目有活着的 daemon；见
/// [`yi_agent_supervisors::daemon::ensure_running`]。
///
/// 可注入的理由同 `watchman_install`：生产实现会 `spawn` 一个 detached 的
/// `daemon serve` 并等待它就绪，测试里绝不能真的拉起进程。
pub(crate) type EnsureDaemon = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;

#[cfg(test)]
mod path_semantics_tests;

#[cfg(test)]
mod test_support;

/// What a conversation-scoped cancellation did.
pub(crate) enum ThreadCancelOutcome {
    /// Nothing left to cancel, or the caller forced and everything was reaped.
    Cancelled(usize),
    /// Live children exist and the caller did not force: nothing was touched.
    NeedsConfirmation(usize),
}

/// One conversation's child-list watcher.
///
/// The plan called for a kind-filtered `SubscribeTrace`; polling the summary
/// list is used instead because it is strictly cheaper here (summaries are tiny
/// and carry the objective/state the rail needs, which trace rows do not) and it
/// reuses the same read the initial `agent/children/list` does, so the pushed
/// list can never disagree with a fresh request. The observable contract is
/// unchanged: whole-list `agent/children/updated` notifications on change.
pub(crate) struct ChildrenWatch {
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl ChildrenWatch {
    pub(crate) async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// 每个 thread 一个进程状态守望者:订阅该 thread 生效 manager 的广播,把
/// 低频状态事件转成 `process/updated` 通知。`Output` 事件在此丢弃。
pub(crate) struct ProcessWatch {
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl ProcessWatch {
    pub(crate) async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// One conversation's live trace watch.
///
/// At most one task per conversation is watched, because the client shows one
/// detail at a time; watching another replaces the watch rather than adding a
/// second stream, so rows never arrive for a task the user has left.
pub(crate) struct TraceWatch {
    pub(crate) task: tokio::task::JoinHandle<()>,
}

impl TraceWatch {
    /// Stop forwarding and wait for the task to observe the cancellation.
    pub(crate) async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

/// app-server 入口:在 stdio(或任意读写流)上跑 JSON-RPC 主循环。
///
/// `cfg` 同时用于 `config/read` 响应与(每个 thread 的)`bootstrap_agent`。
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    // 配对状态(配对码 + 设备表)与 workspace 索引同位构造:主循环持有同一个
    // `Arc`,故 `pair/create` 铸出的码与 `device/list` / `device/revoke` 读写的
    // 是**同一张**设备表。`run` 的公开签名不因此改变。
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        crate::device_store::default_path(),
    )));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    // A fallback, not a policy: resolution failing (unset HOME) would otherwise
    // make the whole server fail to start. Resident RPCs then register into a
    // directory nobody lists from, which is the same as having no resident need
    // — better than losing every unrelated RPC.
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 会话期自愈:app 打开期间后台每 ~30s 重新确保每个登记项目都有活着的 daemon,
    // 与 launchd watchman 互补。只在生产入口启动;`serve_stdio`/`serve_scoped` 不装,
    // 故单元测试永远不会拉起真实进程。传入已算好的 `resident_dir`,循环内不重读 HOME。
    spawn_resident_watch_loop(resident_dir.clone());
    // 主题句柄:`ui/settings/read|write` 与每个 thread 的 `set_theme` 工具共用。
    // 工厂闭包 `'static`,拿不到主循环里的 `theme`;先克隆一份专供工厂。
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    // 清单按指纹跟随 `models.json`:运行期改条目,新建 thread 立刻用上新值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
    // 启动期把存量 `.env` 模型键收边进清单（一次性、幂等）。
    migrate_legacy_model_keys_on_startup();
    // 启动期把旧登记表导入通用常驻登记（一次性、幂等；失败只 warn）。
    migrate_legacy_resident_requests_on_startup(&resident_dir);
    serve_stdio(
        reader,
        writer,
        cfg.clone(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes,
            thread_roots,
            models_path: yi_agent_runtime::models::models_path(),
            websearch_path: yi_agent_tools::websearch_path(),
            settings_path: yi_agent_runtime::settings::settings_path(),
            // 开关的结论。生产构造点共用同一个解析函数，避免各写一份「怎么算」。
            lead_agent_enabled: crate::lead_gate::from_process_environment(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `resident_dir` 同一策略。
            onboarding_preferences_path: instance_home_dir().join("preferences.json"),
            resident_dir,
            skills_home: skills_home_dir(),
            remote_dir: instance_home_dir(),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            resolve_plugin: Arc::new(crate::plugin_worktree::resolve_plugin_command),
            ensure_daemon: Arc::new(yi_agent_supervisors::daemon::ensure_running),
            // 进程一条组长信箱,由 `serve` 的主循环消费。
            lead_inbox: crate::lead::LeadInbox::new(),
            // 回信登记表与信箱同一实例范围:`blocking: true` 的 await 侧登记
            // 在这里,回填由同一个 `serve` 主循环做。
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
        },
        production_factory(cfg, theme_for_factory, catalog),
    )
    .await
}

/// 合并入口:`app-server --listen stdio:// --relay <url>` 用。
///
/// 与 [`run`] 同构(自建 workspaces/pairing/attachments/factory —— `RuntimeAttachments`
/// 与 `production_factory` 是 crate 私有的,故本函数**自包含**,供二进制 crate
/// 直接调用),差别只在多接一条环回 ws 前端 + 中继客户端:
///
/// - stdio 客户端仍是 `ClientId = local`、`Scope::Admin`(桌面 GUI,全权);
/// - 经环回 ws 接入的手机是 `ws-<uuid>`、`Control`(与直连 ws 时行为一致);
/// - 二者**共用同一个** `serve()` 与同一套 hub / `threads` / `client_scopes`,故
///   任一侧的 turn 事件、审批请求、状态通知都扇出到双方(spec §2.1)。
///
/// 环回 listener 绑 `127.0.0.1:0`,中继客户端是**出站**连接,故网络路径上没有
/// 新增暴露面;环回 ws 仍需 token(本函数用共享 `pairing` 取一枚
/// `seed_local_device("relay-bridge")` 凭据交中继客户端;该取法按名字**幂等**,
/// 重启不会重复落表)。stdio EOF(桌面退出)⇒
/// `serve()` 返回 ⇒ 整个合并会话优雅退出(spec §3.2 不变量 4)。
pub async fn serve_stdio_with_relay<R, W>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    relay_url: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
        crate::device_store::default_path(),
    )));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 与 `run` 一致:本函数是合一模式的生产入口,故同样在会话期自愈地确保
    // 各登记项目有一枚活着的 daemon(桌面经 `--relay` 启动时不应丢掉该保障)。
    spawn_resident_watch_loop(resident_dir.clone());
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    // 与 `run` 一致:清单按指纹跟随 `models.json`,工厂逐会话取当前值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
    // 与 `run` 一致:启动期做一次性 `.env` → 清单迁移。
    migrate_legacy_model_keys_on_startup();
    // 与 `run` 一致:启动期把旧登记表导入通用常驻登记(一次性、幂等)。
    migrate_legacy_resident_requests_on_startup(&resident_dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    serve_scoped_core(
        reader,
        writer,
        cfg.clone(),
        PERMISSION_TIMEOUT,
        workspaces,
        pairing,
        Arc::new(crate::broadcast::Broadcaster::new()),
        RuntimeAttachments {
            runtimes,
            thread_roots,
            models_path: yi_agent_runtime::models::models_path(),
            websearch_path: yi_agent_tools::websearch_path(),
            settings_path: yi_agent_runtime::settings::settings_path(),
            // 开关的结论。生产构造点共用同一个解析函数，避免各写一份「怎么算」。
            lead_agent_enabled: crate::lead_gate::from_process_environment(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `resident_dir` 同一策略。
            onboarding_preferences_path: instance_home_dir().join("preferences.json"),
            resident_dir,
            skills_home: skills_home_dir(),
            remote_dir: instance_home_dir(),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            resolve_plugin: Arc::new(crate::plugin_worktree::resolve_plugin_command),
            ensure_daemon: Arc::new(yi_agent_supervisors::daemon::ensure_running),
            // 进程一条组长信箱,由 `serve` 的主循环消费。
            lead_inbox: crate::lead::LeadInbox::new(),
            // 回信登记表与信箱同一实例范围:`blocking: true` 的 await 侧登记
            // 在这里,回填由同一个 `serve` 主循环做。
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
        },
        production_factory(cfg, theme_for_factory, catalog),
        // 桌面 = Admin(与 `run`/`serve_stdio` 的 stdio 注册完全一致)。
        Scope::Admin,
        Some(listener),
        Some(relay_url),
    )
    .await
}

/// stdio 传输的接线:注册唯一 `local` 客户端,起读写泵,再进传输无关的 `serve`。
///
/// 主循环不再持有 reader/writer;`read_lines` 与 `pump_stdout` 各自独占一条流,
/// 只与主循环交换 channel 消息(均 cancel-safe)。退出前摘除 `local` 客户端并
/// 等待出口泵排空,保证最后一个响应总能写出。
///
/// stdio 就是桌面:注册为 `local` + `Scope::Admin`,保留全部能力。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_stdio<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    serve_scoped(
        reader,
        writer,
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        hub,
        attachments,
        build_agent,
        // 桌面 stdio 是 Admin:所有既有 RPC 行为不变。
        Scope::Admin,
    )
    .await
}

fn parse_relay_url(relay_url: &str) -> anyhow::Result<(String, String)> {
    yi_agent_relay::parse_relay_parts(relay_url)
}

#[cfg(test)]
mod legacy_migration_tests;

/// 与 [`serve_stdio`] 逐字节相同,只多一个 `client_scope`。
///
/// 存在的唯一理由是让"低权客户端的门禁"可测:测试经 `Harness::with_scope`
/// 以 `Control` 注册那个 `local` 客户端。生产路径上 `serve_stdio` 传
/// `Scope::Admin`,`run` 与 `serve_stdio` 的公开签名都不因此改变。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_scoped<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scope: Scope,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    // `None` = 纯 stdio:不挂 ws 前端、不起中继,行为与改造前**逐字节一致**。
    serve_scoped_core(
        reader,
        writer,
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        hub,
        attachments,
        build_agent,
        client_scope,
        None,
        None,
    )
    .await
}

/// stdio(+ 可选环回 ws / 中继)的共享实现。
///
/// `listener` 为 `None` 时就是纯 stdio([`serve_scoped`] / [`serve_stdio`]),此时
/// 不建任何 ws/中继接线,行为与改造前逐字节一致;为 `Some` 时,stdio 的 `local`
/// 与每条环回 ws 连接**共用同一个** `serve()`、同一套 hub/scopes/threads。
///
/// 本节点的唯一不变量:**恰好一个 `serve(...)` 调用**。两条前端都只是往同一个
/// `inbound_tx` 灌帧、把出站登记进同一个 hub。
#[allow(clippy::too_many_arguments)]
async fn serve_scoped_core<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    hub: Arc<crate::broadcast::Broadcaster>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scope: Scope,
    listener: Option<tokio::net::TcpListener>,
    relay_url: Option<String>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    let local = crate::broadcast::ClientId::local();
    // 可靠登记:stdio 只有这一条出站流,广播的背压**不得**摘除它。桌面 host
    // 一旦来不及读 stdout,旧的 lossy `broadcast` 会把它当慢消费者摘掉,主循环
    // 随即写响应失败、sidecar 以错误退出——用户看到的就是「打开很多页面后
    // broken pipe (os error 32)」。真正断连由 `pump_stdout` 写失败时显式摘除。
    let outbound = hub.register_reliable(local.clone());
    // channel 里携带 `Result`,区分「读到一行」「EOF(channel 关闭)」与
    // 「读/传输错误」。若不区分,超大帧或 broken pipe 会被误当成干净 EOF。
    let (inbound_tx, inbound_rx) =
        mpsc::channel::<(crate::broadcast::ClientId, anyhow::Result<String>)>(64);
    let pump = tokio::spawn(pump_stdout(
        outbound,
        writer,
        Arc::clone(&hub),
        local.clone(),
    ));
    // stdio EOF 的观察者(仅合并模式需要,理由见下方 oneshot 的注释)。
    let merged = listener.is_some();
    let (stdio_eof_tx, stdio_eof_rx) = tokio::sync::oneshot::channel::<()>();
    // `read_lines` 独占 reader,并把 stdio 读端「结束」经 oneshot 转成主循环可
    // `select!` 的事件(纯 stdio 下传 `None`,行为与改造前一致)。
    tokio::spawn(read_lines(
        reader,
        local.clone(),
        inbound_tx.clone(),
        if merged { Some(stdio_eof_tx) } else { None },
    ));
    // scope 随连接登记一次;主循环按 `client` id 查它。stdio 会话只此一条,故
    // 传入的表就是一张只装 `local` 的注册表(与 ws 的动态注册同型,便于共享)。
    let client_scopes: ClientScopes =
        Arc::new(Mutex::new(HashMap::from([(local.clone(), client_scope)])));
    let client_initialized: ClientInitialized = Arc::new(Mutex::new(HashMap::new()));

    // ---------- 合并模式:把一条环回 ws 前端(与可选的中继客户端)接到同一个
    // `serve()` 上。`listener` 为 `None` 时以下整块都跳过,纯 stdio 因此与改造前
    // 一致。 ----------
    //
    // stdio EOF 的观察者:合并后入站 channel 至少还有 ws 前端一只 sender,单靠
    // `recv() == None` 已无法感知 stdio 读端关闭。桌面关闭(EOF)必须整进程退出
    // ——spec §3.2 不变量 4「桌面关了,手机没东西可控」。故 `read_lines` 结束时
    // 经 oneshot 通知主循环,后者据此 `break`;`serve` 返回后 `inbound_rx` 被丢弃,
    // 仍在灌帧的前端(ws / 中继)随之收尾。纯 stdio 下 `stdio_eof_rx` 为 `None`,
    // `select!` 该分支 `pending`,EOF 仍由 channel 关闭感知(与改造前一致)。
    let mut stdio_eof_rx = if merged {
        Some(Box::pin(stdio_eof_rx))
    } else {
        None
    };
    let mut frontend: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    let mut relay_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
    if let Some(listener) = listener {
        // 环回监听地址:中继客户端要用它拼本地 ws URL。listener 随后被 move 进
        // 前端,故先取地址。
        let addr = listener.local_addr()?;
        // 每进程一张「设备 id → ws ClientId」表。本进程只会有一条 ws 前端(stdio
        // 或纯 ws 二选一),故与 `serve_ws_inner` 一样经进程级句柄安装即可。
        let device_clients = install_device_registry(Arc::new(StdMutex::new(HashMap::new())));
        // axum 前端:握手鉴权、升级后把每条连接登记为 `ws-<uuid>`/Control,并把
        // 帧灌进**同一个** `inbound_tx`、把出站登记进**同一个** hub。
        frontend = Some(crate::ws::attach_ws_frontend(
            listener,
            Arc::clone(&hub),
            inbound_tx.clone(),
            Arc::clone(&client_scopes),
            Arc::clone(&client_initialized),
            device_clients,
            Arc::clone(&pairing),
        ));
        // 中继客户端是 **outbound** 连接,不新增入站端口。仅在给了 `relay_url` 时
        // 才起(生产路径);单测传 `None`,自己用共享 `pairing` 铸出的 token 连前端。
        if let Some(relay_url) = relay_url.as_deref() {
            let (relay_base, session) = parse_relay_url(relay_url)?;
            let relay = url::Url::parse(&relay_base)
                .map_err(|e| anyhow::anyhow!("invalid relay url `{relay_base}`: {e}"))?;
            let app_server_ws =
                url::Url::parse(&format!("ws://{addr}/ws")).expect("loopback ws url");
            // 本机中继桥的凭据:与桌面共用同一个 `pairing`,取一枚 Control 设备
            // (等价于本机走一次正常配对),交中继客户端带进 `?token=`。**不**把环回
            // ws 改成免认证——那会削弱「准入即认证」。该取法按名字幂等:同名已铸过
            // 即复用同一台设备,故每次 `--relay` 启动不会新增永久凭据。
            let local_token = pairing.seed_local_device("relay-bridge");
            relay_task = Some(tokio::spawn(async move {
                yi_agent_relay::run_client(relay, app_server_ws, session, local_token).await
            }));
        }
    }

    // 原 `inbound_tx` 在这里就必须丢弃:此后入站 channel 的 sender 只剩
    // `read_lines`(及合并模式下的 ws 前端)。否则它一直被本函数持有,stdio EOF
    // 后 channel 永不关闭,主循环的 `recv()` 拿不到 `None`,优雅退出被拖住。
    drop(inbound_tx);

    // `remote/relayTargets` 需要**原始** relay 串（含本机的 `?session=`）：桌面侧
    // `derive_relay` 的种子就是「原有 session」，两边输入不同形就派生出不同的
    // session，手机便会连到一个没人监听的 channel。此处仍持有 `relay_url` 的所有权
    // （上面只借用了 `as_deref`），故可直接搬过来。
    let relay_url_for_targets = relay_url;

    let result = serve(
        inbound_rx,
        Arc::clone(&hub),
        cfg,
        permission_timeout,
        workspaces,
        pairing,
        attachments,
        build_agent,
        client_scopes,
        client_initialized,
        stdio_eof_rx.take(),
        relay_url_for_targets,
    )
    .await;
    // `serve` 返回(EOF 或传输错误)后主循环不再产出帧。摘除 `local` 客户端会
    // 关闭它的出站 channel;出口泵先把队列里已入队的帧全部写出、再因 channel
    // 关闭退出,故必须 `await` 它——否则 EOF 前刚入队的最后一个响应会随进程
    // 退出而丢失(与改造前「写完才结束」的 stdio 语义不符)。
    hub.unregister(&local);
    let _ = pump.await;
    // 合并模式收尾:stdio EOF(桌面退出)或前端出错后,前端监听任务与中继客户端
    // 都必须停——二者都是 spawn 出的**独立**任务(tokio 在 JoinHandle drop 时不
    // 取消),不显式 abort 就会继续 accept/重连。
    if let Some(frontend) = frontend {
        frontend.abort();
    }
    if let Some(relay_task) = relay_task {
        relay_task.abort();
    }
    result
}

/// 从一条 `AsyncRead` 逐行读取并送入主循环。
///
/// `MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,因此由本任务
/// 独占 reader,主循环只 select cancel-safe 的 channel `recv`。
async fn read_lines<R>(
    reader: R,
    client: crate::broadcast::ClientId,
    tx: mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    // 合并模式下,stdio 读端「结束」要在主循环里被 `select!` 看到。纯 stdio 传
    // `None`:此时 EOF 直接体现为入站 channel 关闭(`tx` 随本任务结束而 drop),
    // 与改造前一致。
    eof_tx: Option<tokio::sync::oneshot::Sender<()>>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = MessageReader::new(reader);
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                if tx.send((client.clone(), Ok(line))).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // EOF → 丢弃 tx → 主循环优雅退出
            Err(e) => {
                tracing::error!("app-server read error: {e}");
                let _ = tx.send((client.clone(), Err(e))).await;
                break;
            }
        }
    }
    // 通知主循环「stdio 读端结束了」。纯 stdio 下 `eof_tx` 为 `None`,no-op。
    if let Some(eof_tx) = eof_tx {
        let _ = eof_tx.send(());
    }
}

/// 把该客户端的出站帧写到一条 `AsyncWrite`(stdio 场景即 stdout)。
///
/// 写失败(对端已消失)时把该客户端从 hub 摘除:主循环之后经 `hub.reply` 定向写
/// 响应会拿到 `Closed`,按**改造前**的语义以错误退出——`write_response(..)?` 曾把
/// 写失败直接变成会话错误,这条路径必须保留。
///
/// **不持有入站 channel 的 sender**:否则读端 EOF 后 channel 仍有一只存活 sender,
/// 主循环的 `recv` 永不返回 `None`,优雅退出被拖住。入站的存活只由 `read_lines`
/// 决定(它 EOF 即丢弃自己的 sender)。
async fn pump_stdout<W>(
    mut outbound: mpsc::Receiver<serde_json::Value>,
    writer: W,
    hub: Arc<crate::broadcast::Broadcaster>,
    client: crate::broadcast::ClientId,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let writer = MessageWriter::new(writer);
    while let Some(frame) = outbound.recv().await {
        if let Err(e) = writer.write_value(&frame).await {
            tracing::error!("app-server stdout write failed: {e}");
            // 出口泵写失败 = 对端真的没了(stdout 管道断裂)。这是 stdio 会话的
            // **唯一**断连判据:可靠登记不会被背压摘除,故此处必须显式摘除,
            // 让主循环随后的 `reply`/`write_response` 拿到 `Closed` 并按既有
            // 致命语义结束会话(`driver_reports_finished_when_writer_fails`)。
            hub.unregister(&client);
            break;
        }
    }
}

/// 每连接的 scope 注册表(共享、可增长)。
///
/// ws 在连接建立时登记 `ClientId → device.scope`、断连即摘除;主循环按 id 查。
/// stdio 路径只装一个 `local` 条目。用 `Arc<Mutex<..>>` 而非启动快照,是因为 ws
/// 的 `ClientId` 在 upgrade 闭包里才铸造。
pub(crate) type ClientScopes = Arc<Mutex<HashMap<crate::broadcast::ClientId, Scope>>>;

/// 每客户端的 `initialize` 状态(共享、可增长)。
///
/// 与 [`ClientScopes`] 同型、同生命周期:传输层在连接建立时**无须**登记——缺省
/// 即「未握手」(fail-closed),主循环收到 `initialize` 时置位,连接断连时由传输
/// 层摘除。用共享表而非主循环里的局部 `HashMap`,是为了能在断连时**回收**条目:
/// 否则每个 ws 连接都会永久留下一个 `ClientId` 键(连接可来去,表只增不减)。
pub(crate) type ClientInitialized = Arc<Mutex<HashMap<crate::broadcast::ClientId, bool>>>;

/// 主题变化守望者:把 [`crate::theme_tool::ThemeHandle`] 的广播翻译成
/// `ui/settings/updated`,经 hub 扇出——stdio 的 `local` 与每个 ws 客户端都要收到。
///
/// 单独成形是为了可测:测试直接驱动这段生产循环,而不是照抄一份。
///
/// `Lagged` 必须当作**继续**:它只是订阅者一时落后(客户端读得慢),`recv()` 仍可
/// 继续调用;若像旧写法那样把它与 `Closed` 一并 `return`,一次背压就会杀掉守望者,
/// 此后本进程再也推不出主题通知。只有 `Closed`(所有发送端都没了)才结束。
async fn pump_theme_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::settings_store::Theme>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    loop {
        let theme = match rx.recv().await {
            Ok(theme) => theme,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "theme watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let n = Notification::UiSettingsUpdated {
            theme: theme.as_str().to_string(),
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}

/// Git Diff 焦点守望者：把 [`crate::git_diff_tool::GitDiffHandle`] 的广播翻译成
/// `ui/gitDiff/focus` 通知扇出。
///
/// 与 [`pump_theme_notifications`] 同构，`Lagged` 同样当作**继续**：它只是订阅者
/// 一时落后，`recv()` 仍可继续调用；只有 `Closed`（所有发送端都没了）才结束。
async fn pump_git_diff_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::git_diff_tool::GitDiffFocus>,
    hub: Arc<crate::broadcast::Broadcaster>,
    // 会话帧表。watcher 与 `thread/view/read` 必须写读**同一张**表,否则面板
    // 读到的永远是空的;帧由工具算好随广播带来,这里只负责落表。
    view_frames: ViewFrames,
) {
    loop {
        let focus = match rx.recv().await {
            Ok(focus) => focus,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "git diff watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        // 先落帧再发通知:通知到达前端时,前端随后的 `thread/view/read` 必须已经
        // 能读到这一帧,否则面板会先画一次旧帧(甚至空帧)。
        if let (Some(thread_id), Some(frame)) = (focus.thread_id.clone(), focus.frame) {
            view_frames.lock().await.insert(thread_id, *frame);
        }
        let n = Notification::UiGitDiffFocus {
            thread_id: focus.thread_id,
            note: focus.note,
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}

/// 主循环的传输无关核心:agent 工厂由调用方注入(测试用 mock provider)。
///
/// **取消安全**:入站读取由传输层任务负责(`read_lines` / WS 读循环),主循环只在
/// 一个 channel 上 `recv`。
///
/// `client_scopes` 是**每连接一次**的 scope 登记(传输层注册客户端时确定):
/// stdio 的 `local` 是 `Admin`,WS 连接是它 token 的 scope。scope 不进 channel
/// 载荷——它不随单条消息变化,而客户端 id 已经是载荷的一部分,用一张表按 id
/// 查即可。
///
/// 这张表是**共享可增长**的注册表(`Arc<Mutex<..>>`)而非启动快照:ws 的
/// `ClientId` 在 upgrade 闭包里才铸造、且随连接动态来去,启动时的一份 `HashMap`
/// 根本无法按 id 命中它们。stdio 路径只装一个 `local` 条目,与快照等价。
///
/// `pairing` 与 `workspaces` 一样由调用方注入,因为 `pair/create` 的兑现必须
/// 落在**同一个**设备表上:注入使 `run`(生产)与测试 `Harness` 各自决定表在
/// 哪里,同时 `pair/create`、`device/list`、`device/revoke` 共享同一个 `Arc`。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve<F>(
    mut inbound: mpsc::Receiver<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    hub: Arc<crate::broadcast::Broadcaster>,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
    attachments: RuntimeAttachments,
    build_agent: F,
    client_scopes: ClientScopes,
    // 每客户端 `initialize` 状态的共享表(见 [`ClientInitialized`]):放在主循环
    // 之外,传输层断连时才摘得掉键。主循环在此处插入/读取,不自己拥有它。
    client_initialized: ClientInitialized,
    // 合并模式下 stdio 读端关闭的信号(见 `serve_scoped_core` 里 `read_lines` 的
    // oneshot);纯 stdio / 纯 ws 传 `None`,该 `select!` 分支 `pending`。
    mut stdio_eof: Option<std::pin::Pin<Box<tokio::sync::oneshot::Receiver<()>>>>,
    // 本进程自己的中继地址（含 `?session=`），原样传自 `--relay`。只有
    // `remote/relayTargets` 用它为每个项目派生 session；`None` = 本机没配中继。
    // 传**原始串**而不是剥掉 session 的 base：桌面侧 `derive_relay` 的种子是
    // 「原有 session」，两边输入必须同形才能派生出同一个 session。
    relay_url: Option<String>,
) -> anyhow::Result<()>
where
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + Sync
        + 'static,
{
    let RuntimeAttachments {
        runtimes,
        thread_roots,
        models_path,
        websearch_path,
        settings_path,
        onboarding_preferences_path,
        resident_dir,
        skills_home,
        remote_dir,
        theme,
        git_diff,
        view_frames,
        watchman_install,
        watchman_uninstall,
        watchman_home,
        resolve_plugin,
        ensure_daemon,
        lead_inbox,
        lead_replies,
        lead_control,
        lead_control_replies,
        // 结论由装配方算好：生产走 `from_process_environment`，测试直接给值。
        // `serve()` 不再解析——它看得到的 `build_kind()` 是进程级的，测试里恒为
        // `release`，在那儿解析会让注入永远失效。
        lead_agent_enabled,
    } = attachments;
    // 进程级探测去重表：整个主循环共用一份，`model/probe` 与 `model/upsert`
    // 的后台探测都从这里 `try_begin`。只活在内存里，重启即清空。
    let probes = crate::model_rpc::ProbeRegistry::default();
    // driver task 会 clone 该 sender 上报 turn 完成事件;主循环持有它,
    // 保证 `turn_rx` 不会提前关闭。
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);
    // detach 出去的 plugin handler 唯一需要回到主循环的信号：stdio
    // `local` 写失败是致命的（`?` 语义从前由内联 await 直接传播）。
    let (fatal_tx, mut fatal_rx) = mpsc::channel::<anyhow::Error>(1);

    // 反向权限请求的等待登记表:key 为 `perm-<seq>`,value 为向 driver
    // 回传决定的 oneshot 端。主循环在读到客户端响应时据此路由。
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // 审批 id 必须跨 thread 全局唯一:每个 thread 的 PermissionChecker 都从
    // request_id=1 开始,若用 request_id 直接做 key 会跨 thread 碰撞。
    let perm_seq = Arc::new(AtomicU64::new(1));

    // 每客户端 `initialized` 与 scope 登记。Tier 0 只有一个客户端时
    // `initialized` 等价于一个 bool;Tier 1 起 stdio 与多台 ws 设备共享本主
    // 循环,「谁 initialize 过了」「谁是什么 scope」都必须按 ClientId 记账,
    // 否则一台设备握手会替另一台解锁、或低权设备的请求被当成高权。
    // scope 由传输层在注册时给定(`client_scopes` 参数);这里补一条兜底,
    // 未登记的客户端按最低权 `Observe` 处理(fail-closed)。
    // `initialized` 由调用方传入(共享表),理由同 `client_scopes`:断连时传输层
    // 要能摘键,主循环持有私有 `HashMap` 就够不着它。
    let initialized = client_initialized;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();

    // 保活。**唯一持有断言的进程**就是这里（spec「为什么是单一持有者」）。
    //
    // 整条 tick 在独立线程里跑（见 `power_guard` 的模块注释）：它含子进程与
    // 2 秒级 TCP 超时，放主循环会把 RPC 卡住。
    // 只读一次盘：三个初值来自同一份 `Settings`，不会互相错位。
    let initial_settings = yi_agent_runtime::settings::load_from(&settings_path);
    let initial_enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(&initial_settings);
    let (initial_grace_min, initial_remote_min) =
        yi_agent_runtime::settings::power_windows_minutes(&initial_settings);
    let power_guard = crate::power_guard::PowerGuard::spawn(
        initial_enabled,
        initial_grace_min * 60,
        initial_remote_min * 60,
    );
    // daemon 侧的 busy 另由 `BusySampler` 采（同样是阻塞 IPC，同样不能进主循环）。
    let busy_sampler = crate::power_busy::BusySampler::spawn(Arc::clone(&runtimes));
    // 远端最后一次发来请求的时刻。**只**由非 local 客户端刷新：桌面壳用 stdio
    // 连的，若不排除，「本地桌面每发一条消息」都会把它刷成真，于是 busy 恒真、
    // 电池下永不释放。
    //
    // 时钟与 `power_guard` 线程里那条**是同一种读数**（睡眠感知单调钟）。它是
    // 系统级读数，不随实例变化，所以两边各建一个 `SystemClock` 得到的是同一口径。
    // 反之若这里改用 `Instant::now()`，睡 8 小时后它会说「刚交互过、还在 5 分钟内」，
    // 而 policy 说「超过 15 分钟」——两个判据打架，`ok` 恒为真。
    let power_clock = yi_agent_power::clock::SystemClock::new();
    let mut last_remote_touch: Option<yi_agent_power::clock::Stamp> = None;

    // 该 thread 首个 turn 要激活的 runtime。驱动里做激活(不在请求循环里)以免一个
    // thread 的 socket 调用卡住所有 thread;这里只暂存 attach 的产物。
    let mut pending_activation: HashMap<String, Option<Arc<ThreadRoot>>> = HashMap::new();
    // 分片上传的会话登记表（iOS 客户端）。与 `threads` 同属主循环局部量，串行
    // 访问故无需加锁；每轮循环开头 `sweep_expired()` 收走超时会话。
    let mut uploads = crate::image_upload::UploadRegistry::new();
    // 每个 thread 至多一条被关注的轨迹流。换任务即替换(不并存),thread 删除即收尾。
    let mut trace_watches: HashMap<String, TraceWatch> = HashMap::new();
    // 每个 thread 至多一个子任务列表守望者,首次 agent/children/list 时建立。
    let mut children_watches: HashMap<String, ChildrenWatch> = HashMap::new();
    // 每个 thread 一个进程状态守望者,建立 thread 时拉起。
    let mut process_watches: HashMap<String, ProcessWatch> = HashMap::new();

    // 会话驱动状态:每个 (项目, 插件) 一份驱动记账(该插件起过的会话 + 待回报的
    // 动作结果)。只被主循环的 tick 分支读写,与 `threads` 同属主循环局部量。
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    // 驱动 tick:3s 一次。`interval` 的第一个 tick **立即就绪**——若就这么进
    // 循环,任何在 3s 门限前到达的请求都会先被一整轮阻塞式插件 I/O 排队。这里把
    // 首个 tick 推到 3s 之后:启动瞬间「驱动立刻动起来」的收益,不值得让第一个
    // 请求等一轮 tick。
    let mut driver_ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + DRIVER_TICK, DRIVER_TICK);
    // 组长后台循环的兜底 tick:30s 一次,与看板同款「首个 tick 推到门限之后」
    // ——否则启动瞬间就会先跑一轮组长扫描,把第一个请求排在一整轮(可能落盘的)
    // 组长 I/O 之后。低频是刻意的:唤醒组长要真花钱(见 `LEAD_TICK`)。
    let mut lead_ticker =
        tokio::time::interval_at(tokio::time::Instant::now() + LEAD_TICK, LEAD_TICK);
    // 主循环侧的保活拍：只做「读设置 + 算 activity + 写两个原子」。
    // 真正的探测与断言在线程里（`power_guard`），这里不碰任何 I/O，
    // 所以用 `interval` 而不是 `interval_at`——立刻跑一次也无害。
    let mut power_ticker = tokio::time::interval(POWER_TICK);
    // 上次**已投递**的 digest 文本(键 = project_key)。审批期的重复 tick 会给出
    // 一模一样的文本,靠它压掉(见 `take_changed_digests`)。纯内存、不落盘:
    // 重启后重投一次是可接受的代价,比多一个索引文件简单得多。
    let mut lead_digest_latch: HashMap<String, String> = HashMap::new();

    // 主题变化 → ui/settings/updated(全局一条流,与 thread 无关)。经 hub 广播:
    // stdio 的 `local` 与任意 ws 设备都能收到并切 `data-theme`。
    tokio::spawn(pump_theme_notifications(
        theme.subscribe(),
        Arc::clone(&hub),
    ));
    // 写路径仍需 theme 句柄;`set` 会落盘 + 广播,由上面的 watcher 推通知。
    let theme_handle = theme.clone();
    // Git Diff 焦点 → ui/gitDiff/focus。与主题守望者同构：Lagged 继续，
    // 只有 Closed 才结束。
    tokio::spawn(pump_git_diff_notifications(
        git_diff.subscribe(),
        Arc::clone(&hub),
        Arc::clone(&view_frames),
    ));
    // 建 thread 的工具仍需该句柄(与 watcher 同一实例,故通知不落空)。
    let git_diff_handle = git_diff.clone();
    // 组长信箱的消费端。订阅**在这里**、而不是在 RPC 分支里懒订阅:订阅者一旦
    // 晚于投递建立,那条消息就永远收不到(见 `LeadInbox::deliver` 的「无人订阅」
    // 退化分支)。主循环是唯一消费者,故一条 `broadcast` 通道即可。
    //
    // 消费必须留在本循环的 `select!` 里:`threads` 是本函数独占的 `&mut`,投放
    // 到 `tokio::spawn` 需要 `'static` 或共享可变状态,两者都会拆掉「一个主循环
    // 串行拥有 threads」这条前提(与通用会话驱动器同一约束)。
    let mut lead_rx = lead_inbox.subscribe();
    // 组长写通道的消费端。与 `lead_rx` 同一理由:订阅要在投递之前建立,
    // 且消费必须留在本循环的 `select!`(`threads` 是本函数独占的 `&mut`)。
    let mut lead_control_rx = lead_control.subscribe();
    // 「哪条阻塞请求正等着哪个组长的本轮结束」。key 是组长 thread id、value 是
    // 待回填的 `request_id` 队列:同一个组长可能同时被多个会话阻塞等待(它自己
    // 串行跑 turn,但投递可以叠队),所以是一对多而非一对一。
    //
    // 在主循环本地而非 `LeadReplies` 内部记账:登记表只认 `request_id`,不知道
    // 「谁在等哪一轮」;把「等哪一轮」也塞进它,就会让一个纯登记表长出路由知识。
    // 与 `threads` 同属主循环局部量,同样串行访问,故无需加锁。
    let mut pending_lead_replies: HashMap<String, Vec<String>> = HashMap::new();

    loop {
        // 上传登记表的 TTL 清理：每轮循环开头收走超时的 staging 会话。放在这里
        // 而不是另起定时器，理由同通用会话驱动器——登记表是主循环局部量，`&mut`
        // 借用必须串行。
        uploads.sweep_expired();
        tokio::select! {
            // 合并模式:stdio 读端 EOF → 结束整个循环(并丢弃 `inbound`),故
            // 桌面关闭即整进程优雅退出(spec §3.2 不变量 4)。纯 stdio / 纯 ws
            // 下该 future 为 `pending`(或 `None` 分支永不就绪)。
            _ = async {
                match stdio_eof.as_mut() {
                    Some(rx) => { let _ = rx.await; }
                    None => std::future::pending::<()>().await,
                }
            } => break,
            line = inbound.recv() => {
                let Some((client, item)) = line else { break }; // EOF → graceful exit
                // 远端交互时间戳：**只**记非 local 客户端。
                // `ClientId::ws(..)` 是唯一一种「远端」，而 relay 不是独立入口
                // （`--relay` 的环回桥走同一个 `serve_ws_body`），所以这一个判据
                // 同时覆盖手机直连与经 relay 连接，不需要按传输方式分支。
                // 注意：畸形帧也刷新——它同样证明「有人正在指挥我」。
                if client != crate::broadcast::ClientId::local() {
                    if let Some(stamp) = power_clock.now() {
                        last_remote_touch = Some(stamp);
                    }
                }
                // 早退守卫:某客户端(如被 `device/revoke` 踢掉、或被广播背压
                // 摘除)在断连后仍可能有一帧已在 channel 里排队。它已经不在 hub
                // 上,任何按它寻址的响应都写不出去;在主循环里处理它只会走到
                // `write_response` 的 `Closed` 分支。这里直接丢弃,别再费事。
                //
                // 只管**非本地**客户端:stdio 的 `local` 若已注销(出口泵写失败),
                // 仍要走 `write_response` 的致命分支退出会话——那是改造前的语义
                // (`oversized_frame_returns_err` / `driver_reports_finished_when_writer_fails`
                // 钉死的),不能因为这个守卫被悄悄改成优雅退出。
                if client != crate::broadcast::ClientId::local() && !hub.is_connected(&client) {
                    continue;
                }
                let line = match item {
                    Ok(l) => l,
                    Err(e) => {
                        // 尽力告知客户端我们为何退出。
                        let _ = write_response(
                            &hub, &client,
                            err_response(
                                RequestId::Num(0),
                                RpcError::invalid_request(format!("transport error: {e}")),
                            ),
                        )
                        .await;
                        // stdio 只有 `local` 一个客户端,其读端出错即整个会话结束:
                        // 把失败向上抛出,避免被伪装成干净退出(`oversized_frame_returns_err`)。
                        // WS 传输是**多客户端共享**一条主循环,单个连接的传输错误只
                        // 能摘除该 ClientId(spec §6),连接与服务器都必须存活。
                        if client == crate::broadcast::ClientId::local() {
                            return Err(e);
                        }
                        hub.unregister(&client);
                        continue;
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("app-server parse error: {e}");
                        // 畸形帧里没有可用的 id。JSON-RPC 2.0 要求此时响应
                        // 的 id 为 `null`,但 `RequestId` 目前没有 Null 变体,
                        // 故暂以 id 0 代替(见 docs/bug-list.md)。
                        write_response(
                            &hub, &client,
                            err_response(RequestId::Num(0), RpcError::parse_error(e.to_string())),
                        )
                        .await?;
                        continue;
                    }
                };
                let req: RequestEnvelope = match serde_json::from_value(value.clone()) {
                    Ok(r) => r,
                    Err(_) => {
                        // 没有 `method`:可能是客户端对反向请求的响应。必须带
                        // `result` 或 `error` 才算响应,否则视为畸形帧报错。
                        match serde_json::from_value::<ClientResponse>(value) {
                            Ok(resp) if resp.result.is_some() || resp.error.is_some() => {
                                // 首次命中才广播:第二次应答(另一台设备也点了
                                // "允许")是 no-op 成功,不再产生广播,也就不会
                                // 来回弹提示。
                                if let Some((perm_id, decision)) =
                                    route_client_response(resp, &pending).await
                                {
                                    write_notification(
                                        &hub,
                                        &Notification::ToolCallApprovalResolved {
                                            perm_id,
                                            by: client.as_str().to_string(),
                                            decision: decision_label(&decision),
                                        },
                                    )
                                    .await?;
                                }
                            }
                            _ => {
                                write_response(
                                    &hub, &client,
                                    err_response(
                                        RequestId::Num(0),
                                        RpcError::parse_error("malformed frame"),
                                    ),
                                )
                                .await?;
                            }
                        }
                        continue;
                    }
                };
                let id = req.id.clone();
                let method = req.method.clone();

                // 未 initialize 前,除 `initialize` 外的请求一律拒绝。按客户端
                // 判断,而非全局一个 bool。
                let client_initialized = initialized
                    .lock()
                    .await
                    .get(&client)
                    .copied()
                    .unwrap_or(false);
                if !client_initialized && method != "initialize" {
                    write_response(&hub, &client, err_response(id, RpcError::not_initialized())).await?;
                    continue;
                }

                // admin 类方法:control/observe 客户端一律拒绝。放在
                // `!initialized` 检查之后,故未握手的客户端仍先得到
                // `not_initialized`;scope 缺失按 fail-closed 的 `Observe` 处理。
                //
                // `pair/create`(铸出可换设备 token 的配对码)与 `device/revoke`
                // (踢设备、废 token)是特权桌面操作,一并入闸:低权客户端若能铸
                // 凭据或踢设备,scope 体系形同虚设。`device/list` 只暴露设备名/
                // scope/时间戳(无秘密),任何已握手客户端可读,故**不**入闸。
                //
                // 注:`thread/setPermissionMode` 原在此列,现按 `Control` 放行
                // （见下方方法分支内的门禁）。设计
                // （2026-10-02-mobile-remote-access-design.md §5.4）承诺的「引导去
                // 桌面端授权」入口在代码里并不存在,admin-only 使手机端的 YOLO 切换
                // 结构性不可用。它与 `thread/setModel` 同档:Control 客户端本就能
                // `turn/start` 并在 YOLO 会话上批准任意工具,单开此门不扩大既有能力面。
                //
                // 注:`thread/delete` 原在此列,现同样按 `Control` 放行（见下方分支
                // 内的门禁,2026-10-11 修订）。手机端「长按 = 右键」菜单的「删除」
                // 对齐桌面右键/悬停删除按钮,admin-only 使它必然 `-32014`、与 YOLO
                // 同一死路。它属与 `thread/rename` 同级的会话管理,Control 本就能
                // `thread/start`/`turn/start`,单开此门不扩大既有能力面。`process/kill`
                // (杀进程)仍严格 admin。
                const ADMIN_METHODS: [&str; 3] = ["process/kill", "pair/create", "device/revoke"];
                let client_scope = client_scopes
                    .lock()
                    .await
                    .get(&client)
                    .copied()
                    .unwrap_or(Scope::Observe);
                if ADMIN_METHODS.contains(&method.as_str()) && client_scope < Scope::Admin {
                    write_response(
                        &hub,
                        &client,
                        err_response(id, RpcError::insufficient_scope(Scope::Admin)),
                    )
                    .await?;
                    continue;
                }

                // 域 handler 需要主循环的局部量;每个域臂按需构造一次
                // [`crate::rpc::AppState`],借用到臂末(通常 `continue`)为止,故主循环
                // 各局部量的所有权与借用关系与搬移前完全一致。
                macro_rules! app_state {
                    () => {
                        // 字段名逐项写出(而非位置参数):`models_path` /
                        // `onboarding_preferences_path` / `watchman_home` 三个相邻
                        // 字段同为 `&Path`,位置一旦写错照样编译通过、却会静默把
                        // 机器级路径接错。
                        crate::rpc::AppState {
                            hub: &hub,
                            client: &client,
                            client_scope,
                            cfg: &cfg,
                            theme_handle: &theme_handle,
                            models_path: &models_path,
                            websearch_path: &websearch_path,
                            settings_path: &settings_path,
                            probes: &probes,
                            onboarding_preferences_path: &onboarding_preferences_path,
                            watchman_home: &watchman_home,
                            resident_dir: &resident_dir,
                            watchman_install: &watchman_install,
                            watchman_uninstall: &watchman_uninstall,
                            uploads: &mut uploads,
                            threads: &mut threads,
                            workspaces: &workspaces,
                            pairing: &pairing,
                            pending_activation: &mut pending_activation,
                            runtimes: &runtimes,
                            thread_roots: &thread_roots,
                            process_watches: &mut process_watches,
                            trace_watches: &mut trace_watches,
                            children_watches: &mut children_watches,
                            pending: &pending,
                            perm_seq: &perm_seq,
                            permission_timeout,
                            turn_rx: &mut turn_rx,
                            turn_tx: &turn_tx,
                            build_agent: &build_agent,
                            git_diff_handle: &git_diff_handle,
                            view_frames: &view_frames,
                            skills_home: &skills_home,
                            lead_inbox: &lead_inbox,
                            lead_replies: &lead_replies,
                            lead_control: &lead_control,
                            lead_control_replies: &lead_control_replies,
                            lead_agent_enabled,
                            pending_lead_replies: &mut pending_lead_replies,
                            remote_dir: &remote_dir,
                            relay_url: relay_url.as_deref(),
                        }
                    };
                }
                match crate::rpc::domain_for(method.as_str()) {
                    crate::rpc::Domain::Ui => {
                        let mut state = app_state!();
                        match crate::rpc::ui::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Model => {
                        let mut state = app_state!();
                        match crate::rpc::model::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Onboarding => {
                        let mut state = app_state!();
                        match crate::rpc::onboarding::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Resident => {
                        let mut state = app_state!();
                        match crate::rpc::resident::handle(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Plugin => {
                        // 门禁（initialize / scope）已在上面同步做完；这里只把
                        // **处理** detach 出去。主循环是单 task，内联 await 会串行
                        // 堵住所有 thread 的 RPC；主 runtime 是多线程的，故
                        // `tokio::spawn` 让它在别的 worker 上跑、主循环立刻回到
                        // `select!`。（`spawn_blocking(..).await` 做不到：主 task
                        // 仍要 await 它返回。）
                        let state = std::sync::Arc::new(crate::rpc::PluginChannelState {
                            hub: std::sync::Arc::clone(&hub),
                            client: client.clone(),
                            resident_dir: resident_dir.clone(),
                            theme_handle: theme_handle.clone(),
                            watchman_home: watchman_home.clone(),
                            watchman_install: std::sync::Arc::clone(&watchman_install),
                            watchman_uninstall: std::sync::Arc::clone(&watchman_uninstall),
                            resolve_plugin: std::sync::Arc::clone(&resolve_plugin),
                            ensure_daemon: std::sync::Arc::clone(&ensure_daemon),
                            plugin_gate: crate::plugin_gate::Gate::from_process_environment(),
                            fatal_tx: fatal_tx.clone(),
                        });
                        let req = req.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let result =
                                crate::rpc::plugin::handle_shared(&state, &req, id.clone()).await;
                            match result {
                                Ok(crate::rpc::Flow::Continue) => {}
                                // 域内兜底：与搬移前的原始 dispatch 逐字一致。
                                Ok(crate::rpc::Flow::Unknown) => {
                                    crate::rpc::write_or_fatal(
                                        &state,
                                        id.clone(),
                                        err_response(id, RpcError::method_not_found(&req.method)),
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    let _ = state.fatal_tx.send(error).await;
                                }
                            }
                        });
                        continue;
                    }
                    crate::rpc::Domain::Image => {
                        let mut state = app_state!();
                        match crate::rpc::image::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Workspace => {
                        let mut state = app_state!();
                        match crate::rpc::workspace::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Process => {
                        let mut state = app_state!();
                        match crate::rpc::process::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Pair => {
                        let mut state = app_state!();
                        match crate::rpc::pair::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Turn => {
                        let mut state = app_state!();
                        match crate::rpc::turn::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Agent => {
                        let mut state = app_state!();
                        match crate::rpc::agent::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Skill => {
                        let mut state = app_state!();
                        match crate::rpc::skill::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Flags => {
                        let mut state = app_state!();
                        match crate::rpc::flags::handle(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Remote => {
                        let mut state = app_state!();
                        match crate::rpc::remote::handle(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Mcp => {
                        let mut state = app_state!();
                        match crate::rpc::mcp::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::WebSearch => {
                        let mut state = app_state!();
                        match crate::rpc::websearch::handle(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    crate::rpc::Domain::Thread => {
                        let mut state = app_state!();
                        match crate::rpc::thread::handle(&mut state, &req, id.clone()).await? {
                            // 已搬移的域在应答后一律回下一轮(等价于搬移前的
                            // `continue`)。
                            crate::rpc::Flow::Continue => continue,
                            // 域内 `_` 才可能到这里:交回下面的原始 dispatch。
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
                    // 所有域都已有自己的模块:只有 `Domain::Unknown`(即 `initialize`
                    // 与未识别前缀)会落到这里,再走原始 dispatch。
                    // `Domain` 已是穷尽匹配,这里必须显式写出唯一剩下的变体 —— 用
                    // `_` 会让「新加一个域却忘了加 dispatch 臂」重新变得静默可行。
                    crate::rpc::Domain::Unknown => {}
                }
                match method.as_str() {
                    "initialize" => {
                        initialized.lock().await.insert(client.clone(), true);
                        // 客户端在**第一帧**声明它手上有什么（spec §4.1）：这不需要
                        // 新增往返——initialize 是连上那一刻必发的帧。仅手机端会带
                        // `resumeHints`；桌面/老客户端不带 ⇒ 空 plan，零回归。
                        let hints = parse_resume_hints(req.params.get("resumeHints"));
                        let by_id = |id: &str| {
                            hints
                                .iter()
                                .find(|h| h.thread_id == id)
                                .and_then(|h| h.last_item_id.as_deref())
                        };
                        let plan = plan_sync(&hints, &|id| {
                            // 定位该 thread 的 store：内存里的 thread 用它自己的
                            // （与 driver 共享锁），冷 thread 走全局索引。
                            let store = store_lookup(&threads, &workspaces, &cfg, id);
                            let loaded = store.load(id).ok().flatten()?;
                            // 锚点校验读真实 jsonl（`contains_item_id`），不信任 meta
                            // 的 `last_item_id` 冗余。仅在 hint 真的带了 `lastItemId`
                            // 时才查盘；hint 的锚点为 null（该会话从未有过 item）时
                            // 视为"仍在"——否则会把"还没有内容"误判成"锚点缺失"而
                            // 平白触发一次全量。读失败同样按"仍在"处理：宁可让客户端
                            // 少做一次全量，也不要因一次 IO 抖动误判为失效。
                            let anchor_present = match by_id(id) {
                                Some(anchor) => store.contains_item_id(id, anchor).unwrap_or(true),
                                None => true,
                            };
                            Some((loaded.meta.content_epoch, anchor_present))
                        });
                        let sync_plan: Vec<serde_json::Value> = plan
                            .into_iter()
                            .map(|(thread_id, action, reason, epoch)| {
                                let action = match action {
                                    SyncAction::Full => "full",
                                    SyncAction::None => "none",
                                    SyncAction::Drop => "drop",
                                };
                                json!({
                                    "threadId": thread_id,
                                    "action": action,
                                    "epoch": epoch,
                                    "reason": reason,
                                })
                            })
                            .collect();
                        write_response(
                            &hub, &client,
                            ok_response(
                                id,
                                json!({
                                    "serverInfo": {
                                        "name": "yi-agent-app-server",
                                        "version": env!("CARGO_PKG_VERSION"),
                                    },
                                    "protocolVersion": PROTOCOL_VERSION,
                                    "capabilities": {},
                                    "syncPlan": sync_plan,
                                }),
                            ),
                        )
                        .await?;
                    }
                    _ => {
                        write_response(&hub, &client, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                }
            }
            ev = turn_rx.recv() => {
                if let Some(TurnEvent::Finished { thread_id, turn_id, reply, lead_request_id }) = ev {
                    if let Some(s) = threads.get_mut(&thread_id) {
                        // 仅当完成的正是当前活跃 turn 时才清除,避免迟到的
                        // 完成事件误清掉已开始的下一个 turn。
                        if s.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                            s.active_turn_id = None;
                        }
                    }
                    // 组长那轮跑完 = 阻塞等待方的收口时刻。只回填**这一轮**替其跑
                    // 的那个 request_id(身份随事件带上来);这一轮若与任何等待方
                    // 无关(用户直接对组长说话),则谁都不回填。
                    settle_pending_lead_replies(
                        &thread_id,
                        lead_request_id,
                        reply,
                        &mut pending_lead_replies,
                        &lead_replies,
                    );
                }
            }
            Some(error) = fatal_rx.recv() => return Err(error),
            _ = power_ticker.tick() => {
                // 开关与两个窗口每拍现读磁盘，所以设置页保存后下一拍就生效，
                // 不必重启进程。`load_from` 是「读一次文件 + 逐字段校验」，
                // 10s 一次开销可忽略。
                //
                // **同一拍内只读一次盘**：`settings` 这一个变量同时喂给开关、
                // 两个窗口与远端判定，四处不会读到不同版本。
                let settings = yi_agent_runtime::settings::load_from(&settings_path);
                let enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(&settings);
                let (grace_min, remote_min) =
                    yi_agent_runtime::settings::power_windows_minutes(&settings);
                let (grace_secs, remote_secs) = (grace_min * 60, remote_min * 60);

                // 本进程侧：本地会话 ∪ 远端近期交互。两项都是纯内存判断，
                // 放主循环里是安全的（不碰 I/O，也不起子进程）。
                let local = crate::power_busy::local_busy(&threads);
                let remote = crate::power_busy::remote_touch_busy(
                    last_remote_touch,
                    &power_clock,
                    Duration::from_secs(remote_secs),
                );
                // `latest()` 永远立刻返回（上一拍的结果），绝不等待。
                let daemon = busy_sampler.latest();

                // 只写原子。探测与 IOKit 都在供电线程里，这里绝不阻塞。
                power_guard.set_activity(local || remote || daemon);
                power_guard.set_enabled(enabled);
                power_guard.set_windows(grace_secs, remote_secs);
            }
            _ = driver_ticker.tick() => {
                // 通用会话驱动器内联跑一轮:问每个驱动插件「下一步起/续哪个
                // 会话」并按它的回答执行。所有局部量按顺序借用,不与 RPC 分支
                // 并发,故 `threads` 无需改成共享状态——也**不得**另起任务去碰它。
                driver_tick(
                    &resident_dir,
                    &mut drivers,
                    &mut threads,
                    &mut pending_activation,
                    &mut process_watches,
                    &mut uploads,
                    &runtimes,
                    &thread_roots,
                    &models_path,
                    &cfg,
                    &hub,
                    &turn_tx,
                    &pending,
                    permission_timeout,
                    &perm_seq,
                    &theme,
                    &git_diff_handle,
                    &workspaces,
                    &build_agent,
                    &lead_inbox,
                    &lead_replies,
                    &lead_control,
                    &lead_control_replies,
                    lead_agent_enabled,
                )
                .await;

                // 回收空闲项目的 runtime。放在 tick 里跑而不是定时任务里：
                // `threads` 是主循环独占的局部量，判「有没有回合在跑」必须读它，
                // 另起任务就得把它改成共享结构，改动面会扩散到全部 RPC 分支。
                // 回收只是从表里摘条目，真正的停机由 Drop 链完成，不阻塞主循环。
                let evicted = evict_idle_runtimes(
                    &runtimes,
                    &threads,
                    &resident_dir,
                    Instant::now(),
                    EVICT_IDLE_AFTER,
                );
                for project in evicted {
                    // 记一行就够：这是正常的生命周期事件，不是错误。
                    eprintln!(
                        "[app-server] reclaimed idle runtime for {}",
                        project.display()
                    );
                }
            }
            _ = lead_ticker.tick() => {
                // 关掉组长时这一臂整体空转:底下要算 digest、要投递、要起 turn,
                // 全是为组长存在的。**continue 而不是 return**——主循环还得
                // 继续服务其它 RPC。
                if !lead_agent_enabled {
                    continue;
                }
                // 组长后台循环的兜底轮,照看板 tick 内联跑:启动一轮要拿
                // `&mut threads`(投递走 `deliver_to_lead`),而 `threads` 是主循环
                // 独占的局部量,`tokio::spawn` 会拆掉这条前提(spec §6.1)。
                //
                // 「算」与「投」分两步:`lead_tick_once` 只读 `threads` 并返回 owned
                // 的投递清单,返回时借用已归还,下面才借 `&mut`。投递失败不中断本轮,
                // 更不能影响主循环处理其它请求。
                //
                // 再经 `take_changed_digests` 压掉与上次已投递文本相同的项:
                // 审批期里状态没变的每个 30s tick 都会算出**一模一样**的 digest,
                // 不过这一层就会把一次审批变成最多 10 个完整 LLM turn。
                for delivery in
                    take_changed_digests(lead_tick_once(&mut threads), &mut lead_digest_latch)
                {
                    let envelope = crate::lead::LeadEnvelope {
                        project_key: delivery.project_key,
                        from_thread_id: LEAD_LOOP_SENDER.to_string(),
                        text: delivery.text,
                        // 后台 digest 是**单向**的:没有请求方在等它的回复。
                        // `None` 也让 `deliver_to_lead` 构造的 `TurnPrompt` 天然
                        // 带上 `lead_request_id: None`,不会误挂进阻塞回填表。
                        request_id: None,
                    };
                    let mut launcher = ServeLauncher {
                        threads: &mut threads,
                        pending_activation: &mut pending_activation,
                        process_watches: &mut process_watches,
                        uploads: &mut uploads,
                        runtimes: &runtimes,
                        thread_roots: &thread_roots,
                        models_path: &models_path,
                        lead_agent_enabled,
                        cfg: &cfg,
                        hub: &hub,
                        turn_tx: &turn_tx,
                        pending: &pending,
                        permission_timeout,
                        perm_seq: &perm_seq,
                        theme: &theme,
                        git_diff: &git_diff_handle,
                        workspaces: &workspaces,
                        build_agent: &build_agent,
                        lead_inbox: &lead_inbox,
                        lead_replies: &lead_replies,
                        lead_control: &lead_control,
                        lead_control_replies: &lead_control_replies,
                        lead_target: Some(delivery.lead),
                    };
                    if let Err(error) = launcher.deliver_to_lead(&envelope).await {
                        eprintln!("[app-server] lead background delivery failed: {error}");
                    }
                }
            }
            envelope = lead_rx.recv() => {
                let Ok(envelope) = envelope else { continue };
                // 按 project_key 找到该项目的组长。找不到就跳过并记日志:
                // 一条投递失效不该影响主循环处理其它请求。
                let target = match resolve_lead_target(&envelope.project_key) {
                    Ok(target) => target,
                    Err(error) => {
                        eprintln!(
                            "[app-server] could not resolve a lead for {}: {error}",
                            envelope.project_key
                        );
                        continue;
                    }
                };
                // 记账必须在投递**之前**:组长那轮可能在 `deliver_to_lead` 返回前
                // 就跑完并发出 Finished(空会话 + 快 provider 是常态),那时若还没
                // 记下 request_id,这枚完成事件就会被当成「无人等待」丢掉,等待方
                // 只能干等到超时。投递失败时再回滚(见下方 `Err` 分支)。
                let lead_thread_id = target.thread_id.clone();
                if let Some(request_id) = &envelope.request_id {
                    pending_lead_replies
                        .entry(lead_thread_id.clone())
                        .or_default()
                        .push(request_id.clone());
                }
                let mut launcher = ServeLauncher {
                    threads: &mut threads,
                    pending_activation: &mut pending_activation,
                    process_watches: &mut process_watches,
                    uploads: &mut uploads,
                    runtimes: &runtimes,
                    thread_roots: &thread_roots,
                    models_path: &models_path,
                    lead_agent_enabled,
                    cfg: &cfg,
                    hub: &hub,
                    turn_tx: &turn_tx,
                    pending: &pending,
                    permission_timeout,
                    perm_seq: &perm_seq,
                    theme: &theme,
                    git_diff: &git_diff_handle,
                    workspaces: &workspaces,
                    build_agent: &build_agent,
                    lead_inbox: &lead_inbox,
                    lead_replies: &lead_replies,
                    lead_control: &lead_control,
                    lead_control_replies: &lead_control_replies,
                    lead_target: Some(target),
                };
                if let Err(error) = launcher.deliver_to_lead(&envelope).await {
                    eprintln!("[app-server] lead delivery failed: {error}");
                    // 回滚上面那条预付的记账:这条投递没有产生任何一轮,等待方
                    // 已从工具那里拿到错误,再留一条待回填只会在下一轮投递时
                    // 张冠李戴(把下一次的回复回填给这一次的 request_id)。
                    // 摘的是**这一条** id(而非队尾):与收口路径同一口径,不依赖
                    // 「刚 push 的一定在末尾」这条隐含前提。
                    if let Some(request_id) = &envelope.request_id {
                        if let Some(ids) = pending_lead_replies.get_mut(&lead_thread_id) {
                            ids.retain(|id| id != request_id);
                            if ids.is_empty() {
                                pending_lead_replies.remove(&lead_thread_id);
                            }
                        }
                    }
                }
            }
            cmd = lead_control_rx.recv() => {
                let Ok(cmd) = cmd else { continue };
                // 照着 `lead_rx` 分支的形状构造 launcher（每个分支都得自带一份，
                // 因为 `threads` 是 `&mut`，不能跨分支共享）。
                let mut launcher = ServeLauncher {
                    threads: &mut threads,
                    pending_activation: &mut pending_activation,
                    process_watches: &mut process_watches,
                    uploads: &mut uploads,
                    runtimes: &runtimes,
                    thread_roots: &thread_roots,
                    models_path: &models_path,
                    lead_agent_enabled,
                    cfg: &cfg,
                    hub: &hub,
                    turn_tx: &turn_tx,
                    pending: &pending,
                    permission_timeout,
                    perm_seq: &perm_seq,
                    theme: &theme,
                    git_diff: &git_diff_handle,
                    workspaces: &workspaces,
                    build_agent: &build_agent,
                    lead_inbox: &lead_inbox,
                    lead_replies: &lead_replies,
                    lead_control: &lead_control,
                    lead_control_replies: &lead_control_replies,
                    // 写指令不投递给组长，故没有 lead target。
                    lead_target: None,
                };
                let request_id = cmd.request_id.clone();
                let outcome = launcher.apply_lead_control(cmd).await;
                // 回填结果：工具侧按 request_id 收口。无人等待（已超时）是安全的。
                lead_control_replies.resolve(&request_id, outcome);
            }
        }
    }

    // 显式释放断言，再拆项目 root。
    //
    // 顺序有讲究：`detach()` 会停掉本进程起的 daemon，那是「本进程还在干活」的
    // 最后证据。若断言在 detach 之后才由 `Drop` 释放，中间这段时间系统可能已经
    // 开始休眠流程。显式先 drop 让断言立刻消失。
    //
    // `PowerGuard::drop` 会置 `stop` 并 `join`；`join` 最长等 50ms（分段轮询，
    // 见 `power_guard::sleep_until_stop`），不会拖慢退出。
    drop(power_guard);
    drop(busy_sampler);

    // 进程退出前断开所有项目 root。同步调用:已经不在热路径上,而进程即将结束。
    for binding in attached_runtimes(&runtimes) {
        binding.detach();
    }

    Ok(())
}

pub(crate) type WsDeviceRegistry = Arc<StdMutex<HashMap<String, crate::broadcast::ClientId>>>;

#[cfg(test)]
mod resident_rpc_tests;

#[cfg(test)]
mod plugin_query_tests;

#[cfg(test)]
mod plugins_tests;

#[cfg(test)]
mod session_driver_tests;

#[cfg(test)]
// 目录模块：`src/server/tests/mod.rs`。模块路径仍是 `crate::server::tests`，
// 故被 `--exact` 硬编码的 `server::tests::child_holds_thread_lock` 不受影响。
pub(crate) mod tests;
#[cfg(test)]
mod theme_watcher_tests;

#[cfg(test)]
mod git_diff_watcher_tests;

#[cfg(test)]
mod watchman_install_tests;

#[cfg(test)]
mod plugin_ready_tests;

#[cfg(test)]
mod evict_idle_tests;
