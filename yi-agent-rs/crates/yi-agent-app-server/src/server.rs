//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use std::sync::Mutex as StdMutex;
#[allow(unused_imports)]
use std::sync::atomic::AtomicU64;
#[allow(unused_imports)]
use tokio::sync::oneshot;
use tokio::sync::{Mutex, mpsc};

// `AttachFailure` 在 `ProjectRuntimes` 类型别名里出现；`json!` 只有
// `server/tests/*` 用（走 `use super::*`，只编进 test target）。`cargo fix` 只看
// lib target，两者都会被它当死代码删掉，故显式放行。
// 这些只在 `server/test_support.rs` / `server/tests/*` 里被点名（后者 `use
// super::*`，只编进 test target）。`cargo fix` 只看 lib target 会把它们当死代码
// 删掉，故统一放行。
#[allow(unused_imports)]
use crate::pairing::PairingState;
#[allow(unused_imports)]
use crate::workspace_index::WorkspaceIndex;
#[allow(unused_imports)]
use serde_json::json;
#[allow(unused_imports)]
use std::time::Duration;
#[allow(unused_imports)]
use yi_agent_core::permission::Decision;
#[allow(unused_imports)]
use yi_agent_runtime::config::RuntimeConfig;
#[allow(unused_imports)]
use yi_agent_subagent::attach::AttachFailure;

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
    instance_home_dir, production_factory, production_factory_with_binding, project_key,
    skills_home_dir, with_session_preserve_thinking,
};

pub(crate) mod rpc_io;
pub(crate) mod task_bridge;
pub(crate) mod watchers;

#[allow(unused_imports)]
pub(crate) use crate::protocol::ResponseEnvelope;
#[allow(unused_imports)]
pub(crate) use rpc_io::{
    DEVICE_REGISTRY, IMAGE_READ_CHUNK_BYTES, IMAGE_READ_MAX_BYTES, SideResponseSender,
    device_registry, err_response, forget_ws_device, install_device_registry, ok_response,
    write_notification, write_notification_await, write_response, ws_client_for_device,
};
#[allow(unused_imports)]
pub(crate) use task_bridge::{
    cancel_thread_children, forward_to_daemon, list_task_summaries, read_task_trace,
};
#[allow(unused_imports)]
pub(crate) use watchers::{
    children_for_thread, stop_thread_watches, stop_trace_watch, stream_trace, subagent_totals,
    trace_rows, watch_children, watch_processes,
};

pub(crate) mod thread_index;

#[allow(unused_imports)]
pub(crate) use thread_index::{
    Activation, SyncAction, SyncHint, TAG_NAME_MAX_LEN, attach_delegation, collect_pinned,
    decision_label, find_thread_dir, image_media_type, is_load_bearing, item_id,
    parse_client_decision, parse_resume_hints, plan_sync, record_last_item_id,
    require_known_thread, require_thread_id, resolve_thread_cwd, snapshot_for_watch,
    socket_for_thread, store_for, store_lookup, thread_cwd, thread_status, thread_summary_json,
    thread_summary_json_with_status, update_status, worktree_json,
};

// `saturate_u32` 在拆分前就是 `pub fn`：`yi-agent` crate 的 `turn_log.rs` 跨 crate
// 直接调 `yi_agent_app_server::server::saturate_u32`，故这里必须保持 `pub`，
// 不能并进上面的 `pub(crate) use`。
pub use thread_index::saturate_u32;

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
    PLUGIN_READY_POLL, agent_trace_row, ensure_watchman_installed, ensure_watchman_installed_in,
    production_watchman_install, production_watchman_uninstall, project_has_plugin_manifest,
    should_evict, skills_home_from, wait_for_plugin_query_socket_within, watchman_home_from,
};

pub(crate) mod assembly;

pub(crate) mod turn;

pub(crate) mod delegation;
pub(crate) mod driver;
pub(crate) mod foreground;

pub(crate) mod local_management;
pub(crate) mod management_client;
pub(crate) mod transport;

// `lib.rs` 以 `pub use server::{run, serve_stdio_with_relay}` 把它们交给 CLI，
// 这条链路必须是 `pub`。
#[allow(unused_imports)]
pub(crate) use transport::{
    parse_relay_url, pump_git_diff_notifications, pump_settings_notifications, pump_stdout,
    pump_theme_notifications, read_lines, serve, serve_scoped, serve_scoped_core, serve_stdio,
};
// `lib.rs` 以 `pub use server::{run, serve_stdio_with_relay}` 把它们交给 CLI，
// 这条链路必须一路 `pub`。`cargo fix` 会把它当"未用"删掉（lib target 里确实没
// 有用到它的地方，用的是 lib.rs 的再导出），故放行。
#[allow(unused_imports)]
pub use transport::{
    run, run_with_binding, serve_stdio_with_relay, serve_stdio_with_relay_and_binding,
};

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
    /// The sandbox policy the current registry's tools were built with.
    ///
    /// Carried alongside the agent so a turn that installs its own delegation
    /// tools can build them from the same policy as the rest of the set. A
    /// controller created ad hoc per turn would silently diverge from the yolo
    /// switch the session's other tools read.
    pub(crate) sandbox: yi_agent_tools::SandboxController,
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

/// What the shell has told this sidecar it may attach.
///
/// Per session, never process-global. The key is the five-part
/// [`yi_agent_lifecycle::production::RootAuthorityKey`]: scope, runtime generation,
/// client, conversation, root. A path map cannot express that -- two conversations in
/// one directory have different roots -- so there is deliberately no way to look an
/// entry up by directory.
pub(crate) type RootAuthorities =
    Arc<StdMutex<yi_agent_subagent::managed_binding::RootAuthorityStore>>;

/// The managed path's two halves: where authority is installed, and who to ask.
///
/// One value so the pair cannot be half-wired. Built by the bootstrap handler, because
/// until the credential is fixed there is no epoch to name in a request and no
/// authority a snapshot could legitimately carry.
pub(crate) struct ManagedAuthority {
    pub(crate) store: RootAuthorities,
    pub(crate) broker: Arc<dyn yi_agent_subagent::managed_binding::ManagedRootAuthority>,
    /// The App run this sidecar speaks for, fixed by the authenticated handshake.
    ///
    /// Not read from the intent: a conversation that could name its own client could
    /// reach another App run's authority.
    pub(crate) client_run_id: String,
}

impl std::fmt::Debug for ManagedAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedAuthority")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

/// Where the managed authority lands once the bootstrap authenticates.
///
/// A shared slot rather than a plain field on [`RuntimeAttachments`] because the
/// credential it depends on arrives *after* the server is constructed: `serve` builds
/// the management handle, then the first frame bootstraps it. A plain field would have
/// to be `None` forever, and the attach path would silently keep taking the embedded
/// branch on a managed deployment -- the exact failure this task removes.
pub(crate) type ManagedSlot = Arc<StdMutex<Option<Arc<ManagedAuthority>>>>;

/// Reads the managed authority out of its slot.
///
/// The lock is held only to clone the `Arc`, never across an attach: the slot is written
/// once, by the bootstrap, and an attach that held the lock while talking to a guardian
/// would block the handshake that could replace it.
pub(crate) fn managed_snapshot(slot: &ManagedSlot) -> Option<Arc<ManagedAuthority>> {
    slot.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

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
    /// The shell-brokered root authority, when this sidecar serves a desktop shell.
    ///
    /// `None` is the embedded/CLI shape: there is no shell and no broker, so a
    /// conversation attaches with its own stable key. `Some` means the managed path is
    /// in force, and then a conversation that has no entry must be refused rather than
    /// falling back -- the fallback is exactly the unattributed attach this design
    /// exists to remove.
    ///
    /// The store and the broker travel together because a conversation needs both: the
    /// store answers "may I attach?", the broker answers "may I have one?". Splitting
    /// them into two optional fields would let a caller install one and forget the
    /// other, which reads as an authorized session that cannot ever ask for a root.
    pub(crate) managed: ManagedSlot,
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
    /// The process-wide live-settings binding, if this host has one.
    ///
    /// It is handed to every project daemon this process starts *itself* (the
    /// embedded policy), so the subagent workers that daemon runs follow a save
    /// at their next safe point. A managed daemon is a separate process and
    /// resolves the same file on its own; the binding is simply unused there.
    ///
    /// `None` is the historical static behaviour, which is what tests and the
    /// `serve_scoped`/`serve_stdio` shims keep using.
    pub(crate) live_settings:
        Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
    /// 组长开关的**生效值**，装配时算一次。
    ///
    /// 只放结论、不放路径：若把 `flags.json` 路径传下来再让 `serve()` 解析，
    /// 就会用到进程级 `build_kind()`（测试里恒为 `release`），注入永远失效。
    /// 生产的构造点统一调 [`crate::lead_gate::from_process_environment`]，
    /// 测试直接给 `true` / `false`。
    pub(crate) lead_agent_enabled: bool,
    /// 旁问开关。**判定函数**而不是 `bool`：用户会在运行中拨开关，装配时算
    /// 一次的快照会让「关掉」在已运行的宿主上无效（spec §3.2）。
    ///
    /// 生产构造点统一调 [`crate::side_gate::from_process_environment`]；测试直接
    /// 给闭包，避免依赖进程级 `YI_AGENT_HOME` 与恒为 `release` 的 build kind。
    pub(crate) side_gate: crate::side_gate::SideGate,
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
    /// 实例状态根（插件生命周期状态、授权表、安装登记所在处）。
    ///
    /// 可注入的理由同 `resident_dir`：驱动闸门要读它下面的授权表，测试必须能
    /// 指到隔离目录，否则会读到开发者真实的 `~/.yi-agent`，而"读到别人机器上
    /// 的授权"这种测试既不稳定也不安全。
    pub(crate) instance_home: PathBuf,
    /// 实例状态根（`remote.json` / `remote-status.json` 所在目录）。可注入的
    /// 理由同 `resident_dir`：手机端项目清单必须能在测试里读隔离 tempdir。
    pub(crate) remote_dir: PathBuf,
    /// 旁支注册表。`None` = 生产路径,由 `serve()` 按本进程的实例家目录自建;
    /// 测试注入一份带替身 resolver 的实例,于是旁支 RPC 不必依赖真实项目目录。
    ///
    /// 为什么是 Option 而不是必填:`SideRegistry::new` 要 `SideHostResources`
    /// (会真的在 `side-conversations/` 下建目录并取 flock),让每个装配点自己
    /// 构造就等于每个测试都在真实实例根里留垃圾。生产只有一个构造点。
    pub(crate) side_registry: Option<Arc<crate::side_conversation::SideRegistry>>,
    /// 本进程正在驱动的父会话表（旁问来源）。`None` = 生产路径，由 `serve()`
    /// 自建一份；测试注入时能直接观察「发布 / 刷新 / 退出摘除」的可见效果，
    /// 而不是靠间接推断。
    ///
    /// 与 `side_registry` 同为 `Option` 的理由相同：它是宿主级共享状态，不该
    /// 由每个装配点自己 new 一份（那样 resolver 看到的与 driver 写进去的会是
    /// 两张不同的表，旁问永远读不到活跃父）。
    pub(crate) live_side_parents: Option<Arc<crate::side_conversation::LiveSideParents>>,
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
    /// 官方插件目录文件。**按连接注入**：同一进程里的两个 app-server（本地与
    /// 远端，或多账号）必须能各自指向不同的官方源，而进程级
    /// `YI_AGENT_PLUGIN_CATALOG` 只能有一个值。并行测试也靠它隔离。
    pub(crate) plugin_catalog: PathBuf,
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

#[cfg(test)]
mod legacy_migration_tests;

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

/// 每个客户端被告知的「它在替谁干活」。
///
/// 只由本地 stdio 的 `initialize` 写入（见 `shell_authorization`）：远端设备
/// 自报同一个字段一律忽略，否则一台手机可以把身份栽赃给别人。
pub(crate) type ClientShellAuthorizations =
    Arc<Mutex<HashMap<crate::broadcast::ClientId, crate::shell_authorization::ShellAuthorization>>>;

pub(crate) type WsDeviceRegistry = Arc<StdMutex<HashMap<String, crate::broadcast::ClientId>>>;

#[cfg(test)]
mod resident_rpc_tests;

#[cfg(test)]
mod plugin_query_tests;

#[cfg(test)]
mod plugins_tests;

mod driver_resume;

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
