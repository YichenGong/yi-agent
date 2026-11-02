//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures::StreamExt;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::lead_loop::{ThreadSnapshot, decide_wake, render_digest};
use crate::pairing::PairingState;
use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, PROTOCOL_VERSION, RequestEnvelope, RequestId,
    ReverseRequest, RpcError, Scope, SubagentTotals, ThreadStatus,
};
use crate::session::{
    CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt,
};
use crate::session_driver::{DriverAction, DriverActionKind, DriverState, SessionLauncher};
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

#[allow(unused_imports)]
pub(crate) use assembly::{
    RuntimeTooling, SESSION_EXTRA_ROOTS, SESSION_MODEL_REF, SESSION_REASONING_EFFORT,
    attach_cwd_runtime, attached_runtimes, build_runtime_tooling, current_session_extra_roots,
    current_session_model_ref, current_session_reasoning_effort, detach_unused_runtimes,
    rebuild_thread_agent_with_theme, register_theme_tool, registry_with_theme_tool,
    with_session_extra_roots, with_session_model_ref, with_session_reasoning_effort,
    wrap_for_delegation,
};

/// 权限审批等待客户端响应的默认超时;超时按 Deny 处理。
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// 会话驱动在主循环里跑一轮的间隔。
///
/// 采用「serve 主循环内周期性 tick」而非让驱动器并发访问 `threads`:整张
/// `threads` 表是主循环的局部量,重构成可共享结构会把改动面扩散到全部 RPC 分支。
/// 3s 足够跟上插件给出的动作,又远短于一个会话的生命周期。
const DRIVER_TICK: Duration = Duration::from_secs(3);

const EVICT_IDLE_AFTER: Duration = Duration::from_secs(600);

/// 保活策略的采样间隔。
///
/// 与 `DRIVER_TICK`（3s）刻意不同：电源/网络判定不需要 3s 的精度，而每拍都要
/// 走一次 `getifaddrs`（可能还有 `route` 子进程），3s 太密。10s 仍远小于 15 分钟
/// 的宽限窗口，迟到一拍不改变结论。
const POWER_TICK: Duration = Duration::from_secs(10);

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

/// 回收空闲项目的 runtime。
///
/// 每个「有活干」的事实都由调用方采好：`runtimes` 里的 binding 提供空闲时长，
/// `threads` 提供是否有回合在跑，磁盘与常驻登记提供两条豁免。
///
/// 移除条目即完成回收：`Arc<RuntimeBinding>` 的最后一个引用落在
/// `AttachedProjectRuntime` 上，它的 Drop 会停掉本进程起的 daemon 与监督线程，
/// 路由分区也随之清空。之后该项目再有委派时，`attach_cwd_runtime` 会懒重建。
///
/// 返回被回收的项目（供调用方记日志、也供测试断言）。
pub(crate) fn evict_idle_runtimes(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    resident_dir: &Path,
    now: Instant,
    ttl: Duration,
) -> Vec<PathBuf> {
    // 快照：只取「哪个项目」「空了多久」。持锁期间不做磁盘 IO，也不调用任何
    // 需要再取这把锁的方法（`project_key` 会 canonicalize，是 IO）。
    let snapshot: Vec<(PathBuf, Duration)> = runtimes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter_map(|(key, entry)| {
            entry
                .as_ref()
                .ok()
                .map(|binding| (key.clone(), binding.idle_for()))
        })
        .collect();
    if snapshot.is_empty() {
        return Vec::new();
    }

    // 哪些项目还有回合在跑。threads 的 `cwd` 先统一解析成项目键，避免对每个
    // (项目, 会话) 组合都 canonicalize 一次。
    let busy_projects: Vec<PathBuf> = threads
        .values()
        .filter(|session| {
            session.active_turn_id.is_some()
                || !matches!(
                    *session
                        .status
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                    ThreadStatus::Idle
                )
        })
        .map(|session| project_key(Path::new(&session.cwd)))
        .collect();

    let residents: Vec<PathBuf> = yi_agent_store::resident::list(resident_dir)
        .iter()
        .map(|resident| project_key(resident))
        .collect();

    let doomed: Vec<PathBuf> = snapshot
        .into_iter()
        .filter(|(key, idle_for)| {
            let facts = IdleFacts {
                last_active: now.checked_sub(*idle_for).unwrap_or(now),
                has_plugin_manifest: project_has_plugin_manifest(key),
                is_resident: residents.iter().any(|resident| resident == key),
                has_running_turn: busy_projects.iter().any(|busy| busy == key),
            };
            should_evict(&facts, now, ttl)
        })
        .map(|(key, _)| key)
        .collect();
    if doomed.is_empty() {
        return doomed;
    }

    // 回收分两步，顺序与「为什么这样才有效」见下。
    //
    // 1) 先让 binding **原地退休**：丢掉它自己的 daemon 与监督线程。
    //
    //    这一步不可省，也不能靠后续的 Drop 代替。会话的 driver 里，委派工具各持
    //    一份 `Arc<ThreadRoot>`，root 又持着 `Arc<RuntimeBinding>`——用户切走会话
    //    之后这些引用依然在（**会话本身必须留着**，否则对话就丢了）。只从
    //    `runtimes` 表里删条目只是少了一份引用，`Arc` 计数归不了零，`Drop` 永不
    //    触发，daemon 与每 500ms 一轮的监督线程继续跑到底——那正是「监督线程只增
    //    不减」本身。停进程必须显式做。
    //
    // 2) 再从表里摘掉条目。之后再有人按项目查 binding 会查不到，下次委派走
    //    `attach_cwd_runtime` 重建一个（冷启动）。
    //
    // 会话根**不动**：`retire` 推进了 generation，挂在旧 runtime 上的 root 会在
    // 下一次 `attach` 时被自己判定为失效并重挂，不需要宿主替它们清理。
    let retiring: Vec<Arc<RuntimeBinding>> = {
        let mut table = runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        doomed
            .iter()
            .filter_map(|key| table.remove(key))
            .filter_map(Result::ok)
            .collect()
    };
    for binding in retiring {
        binding.retire();
    }
    doomed
}

/// 组长后台循环的低频兜底 tick。
///
/// 与看板调度器刻意分开:**唤醒组长要真的花一次 token**。看板的 3s 是「状态机
/// 对齐」的节奏,而组长的后台循环主要是**兜底**——真正及时的唤醒来自事件路径
/// (前台 `ask_lead` 实时投递、`lead_rx` 分支),tick 只负责捞那些事件没覆盖到的
/// 变化(例如某个 thread 卡在审批上、没有任何新消息)。30s 既能让兜底在一个可
/// 接受的时间内生效,又不会让它变成「每 3s 烧一次钱」。
const LEAD_TICK: Duration = Duration::from_secs(30);

/// 后台循环投递时在 `LeadEnvelope.from_thread_id` 里用的标识。
///
/// `deliver_to_lead` 会把它渲染成注入文本的前缀 `[来自会话 <id>] ...`。用一
/// 个**显然不是 thread id** 的固定串(而不是某个真实 thread),是为了让组长一眼
/// 看出这轮是后台兜底喂进来的、而不是某个具体会话在找它;也避免下游把某个普通
/// 会话误认成「这条 digest 的来源会话」。
const LEAD_LOOP_SENDER: &str = "lead-loop";

/// 首轮 LLM 标题生成的后台任务超时:超时即回退兜底标题。
///
/// 这个上限保护的是**后台标题任务**本身（不让它无限期挂着一个 task），与 turn
/// 收尾无关——标题生成已不在收尾路径上，超时与否都不影响 turn 完成。
const TITLE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
///
/// 用 `set_session_messages` 就地替换内容而不是 `with_session` 换成新 `Arc`:会话
/// 句柄可能已被委派工具的 `CallerContext` 绑定(见 [`wrap_for_delegation`]),
/// 换 `Arc` 会让那枚句柄指向被丢弃的旧会话,`fork` 便再也看不到这位调用者的记录。
fn apply_session(agent: &mut yi_agent_core::Agent, session: Option<yi_agent_core::Session>) {
    if let Some(s) = session {
        agent.set_session_messages(s.messages().to_vec());
        // `set_session_messages` 只搬运消息,若不在此补写,resume 载入的
        // `last_input_tokens` 会在live 句柄上归零,使上面的注释承诺的
        // 「resume 后首轮 auto-compact 即生效」落空(`maybe_auto_compact`
        // 读到 `None` 直接短路)。句柄仍是同一个 Arc,只是补一个字段。
        agent
            .session_handle()
            .lock()
            .unwrap()
            .set_last_input_tokens(s.last_input_tokens());
    }
}

pub(crate) type WsDeviceRegistry = Arc<StdMutex<HashMap<String, crate::broadcast::ClientId>>>;

/// 回放分块的字节软预算（256 KiB）。单帧序列化后须**远小于**
/// `MAX_FRAME_BYTES = 1 MiB`,故留足余量。
const REPLAY_CHUNK_BYTES: usize = 256 * 1024;
/// 回放分块的条数上限，兜住「海量极小 item」把帧数压不下来的极端。
const REPLAY_CHUNK_MAX_ITEMS: usize = 200;

/// 把历史 items 切成回放帧的块：顺序保持；累计序列化字节超过
/// [`REPLAY_CHUNK_BYTES`] 或条数达到 [`REPLAY_CHUNK_MAX_ITEMS`] 即切块。
/// 单条自身超预算时它单独成块（容积为 1），不在此函数内再切分。
pub(crate) fn chunk_items_for_replay(
    items: Vec<crate::protocol::Item>,
) -> Vec<Vec<crate::protocol::Item>> {
    let mut chunks: Vec<Vec<crate::protocol::Item>> = Vec::new();
    let mut cur: Vec<crate::protocol::Item> = Vec::new();
    let mut cur_bytes = 0usize;
    for item in items {
        let approx = serde_json::to_vec(&item).map(|v| v.len()).unwrap_or(64);
        // 字节预算：当前块非空且再加一条会超预算 → 先结块。
        if !cur.is_empty() && cur_bytes + approx > REPLAY_CHUNK_BYTES {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur.push(item);
        cur_bytes += approx;
        // 条数上限：达到上限即结块（与字节预算互为兜底）。
        if cur.len() >= REPLAY_CHUNK_MAX_ITEMS {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// 逐字流合并器：按 thread 攒 `(item_id, text)`。跨 item_id 不合并（顺序优先）。
#[derive(Default)]
struct DeltaCoalescer {
    /// thread → 当前正在累加的 (item_id, text)。
    pending: std::collections::HashMap<String, (String, String)>,
}

impl DeltaCoalescer {
    const FLUSH_BYTES: usize = 4096;

    /// 追加一段 delta，返回**此刻应当立即发出去的**若干条（跨 item 或超限时）。
    fn push(&mut self, thread: &str, item_id: &str, delta: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        match self.pending.get_mut(thread) {
            Some((pid, text)) if pid == item_id => text.push_str(delta),
            _ => {
                if let Some(prev) = self.pending.remove(thread) {
                    out.push(prev);
                }
                self.pending
                    .insert(thread.to_string(), (item_id.to_string(), delta.to_string()));
            }
        }
        if self.pending.get(thread).map(|(_, t)| t.len()).unwrap_or(0) >= Self::FLUSH_BYTES {
            if let Some(p) = self.pending.remove(thread) {
                out.push(p);
            }
        }
        out
    }

    /// 取出并清空该 thread 的待发（屏障/tick 调用）。
    fn take(&mut self, thread: &str) -> Option<(String, String)> {
        self.pending.remove(thread)
    }

    /// 取出全部待发（tick 用）。
    fn take_all(&mut self) -> Vec<(String, (String, String))> {
        self.pending.drain().collect()
    }
}

/// 把客户端对反向请求的响应路由到等待中的 driver。
///
/// 返回 `Some(decision)` 表示本次响应**首次**命中;`None` 表示该审批已被别的
/// 客户端处理(或根本不存在)。后者不是错误:双端同时点"允许"时,后到者是
/// no-op 成功,而不是报错——报错会让另一端弹出一个无意义的失败提示。
async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) -> Option<(String, Decision)> {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return None;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        // 已被先到的应答取走:安静地当作 no-op,不 warn(双答是预期场景)。
        tracing::debug!("approval {key} was already resolved");
        return None;
    };
    let decision = parse_client_decision(resp.result.as_ref());
    let _ = tx.send(decision.clone());
    Some((key, decision))
}

/// 构造一个 `TurnEvent::Finished`,避免在 driver 里重复拼字段。
///
/// `reply` 是本轮的助手回复文本(见 [`lead_reply_text`])。收尾路径传实际取出的
/// 文本;**中途放弃**的路径(run 失败、写帧失败、客户端断连)传 `None`:那些轮
/// 没有「组长说了什么」可言,伪造一条空回复会让等待方以为得到了答复。
///
/// `lead_request_id` 是本轮替哪条阻塞式 `ask_lead` 跑的。**只有**由阻塞投递产生的
/// 那一轮带得出来(见 `deliver_to_lead`),其余构造点一律 `None`:它们产出的轮不能
/// 回填任何等待方。
fn finished_event(
    thread_id: &str,
    turn_id: &str,
    reply: Option<String>,
    lead_request_id: Option<String>,
) -> TurnEvent {
    TurnEvent::Finished {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
        reply,
        lead_request_id,
    }
}

/// 把某个 thread 上**这一轮**负责的那个等待方收口。
///
/// 只回填事件自己携带的那一个 `request_id`,并从待回填队列里**只**摘掉它(队列空
/// 了就把该 lead 的条目整个 drop)。
///
/// 为什么不能整条队列一次取走:`pending_lead_replies` 是一对多的(同一个组长可能
/// 同时被多个会话阻塞等待),而组长**一轮只回答一个提问**。整条取走就意味着把那
/// 一轮的文本同时回填给排队的每一个人——第二个提问人收到的是第一个的答案。用户
/// 直接往组长会话里打字也会产生一轮(`lead_request_id == None`),那种轮若参与
/// 回填,就是把用户的对话内容当成对某个等待方的答复。两种错配都在此排除:
/// **只**认本轮携带的身份,`None` 则谁都不回填。
///
/// 不变量:因本轮而产生的那个 id 已在投递**之前**入账(见主循环的 `lead_rx` 分支),
/// 故这里必然能摘到它;摘不到说明本轮没有任何等待方(或是用户轮),回填是 no-op。
///
/// `reply` 为 `None`(失败轮/纯工具轮/中途放弃)时只摘账、不回填:伪造一条空回复
/// 会让等待方以为组长答复了「空话」。
fn settle_pending_lead_replies(
    thread_id: &str,
    lead_request_id: Option<String>,
    reply: Option<String>,
    pending: &mut HashMap<String, Vec<String>>,
    replies: &crate::lead::LeadReplies,
) {
    // 本轮不替任何等待方跑(用户 turn / 看板卡 / 单向投递):队列**保持原样**,
    // 排队的人继续等它自己那一轮。
    let Some(request_id) = lead_request_id else {
        return;
    };
    if let Some(ids) = pending.get_mut(thread_id) {
        // 只摘这一个。该 id 一定在队列里(投递前入账),`retain` 比按位找更抗
        // 重复投递:即便同一 id 意外入账两次也一并摘干净。
        ids.retain(|id| id != &request_id);
        if ids.is_empty() {
            // 队列空了就把条目整个 drop,免得这张表随会话数无限长胖。
            pending.remove(thread_id);
        }
    }
    if let Some(reply) = reply {
        // `false` = 等待方已超时(drop 了接收端)或从未登记:不是错误,无需日志噪音。
        let _ = replies.resolve(&request_id, reply);
    }
}

/// 若 `thread_id` 有活跃 turn,中断它并等待 driver 落盘完成。
///
/// driver 是「先发 turn/completed,后 append/touch」,而 `TurnEvent::Finished`
/// 在落盘之后才发出;因此本函数返回后,调用方可以依赖「该 turn 已落盘」。
/// 有界等待:driver 异常卡死时不至于拖垮整个请求循环。
pub(crate) async fn interrupt_and_wait_for_persist(
    threads: &mut HashMap<String, ThreadSession>,
    turn_rx: &mut mpsc::Receiver<TurnEvent>,
    thread_id: &str,
    // 本函数会把 `turn_rx` 里的 `Finished` 取走(包括别的 thread 的),故也必须
    // 替主循环把这些事件的待回填队列收口,否则它们会悬到下一次投递。
    pending_lead_replies: &mut HashMap<String, Vec<String>>,
    lead_replies: &crate::lead::LeadReplies,
) {
    let Some(tid) = threads
        .get(thread_id)
        .and_then(|s| s.active_turn_id.clone())
    else {
        return;
    };
    if let Some(s) = threads.get(thread_id) {
        let _ = s.interrupt_tx.try_send(tid.clone());
    }
    let wait_for_persist = async {
        while let Some(TurnEvent::Finished {
            thread_id: done_id,
            turn_id: done_turn,
            reply,
            lead_request_id,
        }) = turn_rx.recv().await
        {
            if let Some(s) = threads.get_mut(&done_id) {
                if s.active_turn_id.as_deref() == Some(done_turn.as_str()) {
                    s.active_turn_id = None;
                }
            }
            // 这枚完成事件不会回到主循环了,它的回填机会就在此处(见函数说明)。
            // 同样只认事件自己携带的身份:别的 thread 的那一轮与这里的等待方无关。
            settle_pending_lead_replies(
                &done_id,
                lead_request_id,
                reply,
                pending_lead_replies,
                lead_replies,
            );
            if done_id == thread_id && done_turn == tid {
                break;
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_persist)
        .await
        .is_err()
    {
        eprintln!(
            "[app-server] timed out waiting for turn of {thread_id} to persist; \
             a late driver append may resurrect its files"
        );
    }
}

/// `build_partial` 的入参分组：提问本身、已完成的 item、用量。
///
/// 拆成结构体是为了让 `build_partial` 的参数个数落在 clippy 的 7 个门槛内，
/// 同时把「这三组各自是什么」写在类型上，比一串同类型的 `&[...]` 好读。
struct PartialTurnInput<'a> {
    turn_id: &'a str,
    user_prompt: &'a str,
    user_attachments: &'a [crate::protocol::Attachment],
    user_image_refs: &'a [crate::protocol::ImageRef],
}

/// 组装当前 turn 的 checkpoint：已 finalize 的 item（含开头的用户提问）+ 当前
/// 上下文快照。进行中的流式文本不在 `completed_items` 里，故天然不入内。
fn build_partial(
    input: PartialTurnInput<'_>,
    completed_items: &[crate::protocol::Item],
    messages: Vec<yi_agent_core::Message>,
    usage: Option<crate::thread_store::TurnUsage>,
    usage_total: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(
        input.turn_id,
        input.user_prompt,
        input.user_attachments.to_vec(),
        input.user_image_refs.to_vec(),
    ));
    items.extend(completed_items.iter().cloned());
    crate::thread_store::PartialTurn {
        turn_id: input.turn_id.to_string(),
        items,
        messages,
        usage,
        usage_total,
    }
}

/// 尽力写一次 checkpoint：失败只记 stderr，绝不打断 turn。
fn checkpoint(
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    partial: crate::thread_store::PartialTurn,
) {
    if let Err(e) = store.write_partial(thread_id, &partial) {
        eprintln!("[app-server] failed to write turn checkpoint ({thread_id}): {e}");
    }
}

/// 一个 turn 结束后（或被 clear / compact 改动后）的收尾：把当前 session 快照
/// 落盘、置 Idle，并在 `turn_id` 为 `Some` 时上报 Finished。
///
/// clear / compact 之后必须调用它：`/clear` 要落一条空快照（否则 resume 回放的是
/// 旧日志），`/compact` 要把压缩结果写回 `.jsonl`。此时**没有 turn 在收尾**，
/// 传 `turn_id = None`：不发 `TurnEvent::Finished`，也不 touch meta。
#[allow(clippy::too_many_arguments)]
async fn persist_and_finish_turn(
    thread_id: &str,
    turn_id: Option<&str>,
    // 随 `turn/completed` 广播出去的那个完成时刻。落盘必须复用它而不是重新取一次
    // 墙钟：广播发生在落盘之前，各取一次会让落盘值更晚，另一端的客户端收到广播
    // 立刻 markRead 时写下的已读水位就盖不住它，重连后蓝点复活。
    completed_at: i64,
    user_prompt: Option<&str>,
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    // 本轮若替某条阻塞式 `ask_lead` 跑,这里是它的 request_id;随 Finished 上报,
    // 主循环据此只回填那一个等待方(见 `settle_pending_lead_replies`)。
    lead_request_id: Option<String>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    // 本轮所有 provider 调用的累加值（`last_usage` 只是最后一次调用的快照）。
    usage_total: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
) {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    if let Some(prompt) = user_prompt {
        // 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{}", turn_id.unwrap_or("session-command")),
            text: prompt.to_string(),
            attachments: user_attachments.to_vec(),
            images: user_image_refs.to_vec(),
        });
    }
    items.extend(completed_items);

    // 先取助手侧材料：`items` 接下来会 move 进 `record`，此后无法再借用。
    let assistant_text = completed_items_agent_text(&items);
    // 落盘副本：关闭跨轮保留（`preserve_thinking == false`）时剥离思考块，使磁盘
    // 与「已放弃跨轮保留」的会话状态一致——否则每轮追加的日志行仍携带刚产出的
    // 凭证，重开会话即「复活」（spec §4.1/§7.4）。session 本体（展示与请求的权威）
    // 不受影响：这里只剥**落盘副本**，与请求路径的剥离互不干扰。
    // 默认 `true` 时逐字取会话内容 ⇒ 与今日落盘完全一致（零回归）。
    let persisted_messages = if agent.config().preserve_thinking {
        agent.session().messages().to_vec()
    } else {
        yi_agent_core::strip_thinking(agent.session().messages())
    };
    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        usage_total,
        messages: persisted_messages,
    };
    // append 失败则跳过 meta 维护：避免出现"幽灵" thread。
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else {
        // `last_item_id` 供 `thread/listAll` 报告"客户端是否落后"的**保守下界**；
        // 「锚点是否仍在」的权威判据是读真实 jsonl 的 `contains_item_id`（spec §3）。
        //
        // 只有本次落盘确实产出了 item 才更新游标。空 items 的落盘（`compact` /
        // `SetPreserveThinking`）不改变可见内容集合，写 `None` 会把「有 items」
        // 误报成「从未有过 item」，从而永久削弱 `listAll` 的「是否落后」信号。
        // 真正清空内容的 `clear` 路径在它的调用点显式清游标（见下）。
        if let Some(last) = record_last_item_id(&record) {
            if let Err(e) = store.set_last_item_id(thread_id, Some(&last)) {
                eprintln!("[app-server] failed to record last_item_id for {thread_id}: {e}");
            }
        }
        if let Some(prompt) = user_prompt {
            // 标题已在 turn 开始时（`run_thread_driver` 收到输入后）由
            // `title_on_first_input` 写入并尝试用 LLM 标题覆盖；这里只负责刷新
            // `updated_at`，返回值不再有意义。
            if let Err(e) = store.touch(thread_id, Some(prompt)) {
                eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
            }
        }
    }

    // 记下**回合结束**时刻：未读由 `last_completed_at > last_read_at` 派生，而
    // `updated_at` 在回合**开始**时也会被 `touch` 刷新，不能兼任这个职责（否则
    // 另一端刚发问、还在跑的会话会立刻亮蓝点）。
    //
    // 刻意放在 append 成功/失败**之外**：两者是独立失败。日志写不进去只说明内容
    // 没存下来，回合本身确确实实结束了、用户也在流里看到了输出。把这条记账绑在
    // append 成功上，这一轮就会在未读口径里凭空消失——另一端重连后毫无「有新内容」
    // 的提示，直到下一轮跑完。
    //
    // 只在 `turn_id.is_some()` 时写：`clear` / `compact` / `SetPreserveThinking`
    // 收尾传 `None`，它们不是回合结束，记下去会让刚清空上下文的会话凭空亮蓝点。
    if turn_id.is_some() {
        if let Err(e) = store.mark_turn_completed(thread_id, completed_at) {
            eprintln!("[app-server] failed to record turn completion for {thread_id}: {e}");
        }
    }

    // 广播「本轮已落盘」：delete/interrupt 路径靠这枚 Finished 判定落盘完成。
    // 标题生成已前移到 turn 开始处，收尾路径不再有任何可等待的模型调用。
    let _ = update_status(hub, status, thread_id, ThreadStatus::Idle).await;
    if let Some(turn_id) = turn_id {
        // 回填给等待方的回复:与标题材料同一份文本,故这里复用它而不是再抽一次
        // (extraction 只有一条口径,见 `lead_reply_text`)。身份与文本一起上报:
        // 收口方只认这一个 request_id(见 `settle_pending_lead_replies`)。
        let _ = turn_tx
            .send(finished_event(
                thread_id,
                turn_id,
                assistant_text.clone(),
                lead_request_id,
            ))
            .await;
    }
}

/// 用户首次输入落地时立刻命名的入口。
///
/// 旧的实现把标题生成放在 `persist_and_finish_turn`：用户要等整轮跑完（可能数十
/// 秒）才在侧栏看到有意义的标题。这里把它前移到 turn 开始处，材料也从「提问 +
/// 助手回复」收敛成「只取用户输入」——此刻助手还没有任何输出。
///
/// 触发条件是 `ThreadStore::touch` 返回 `true`（meta 的 title 由 `None` 变成兜底
/// 截断值）。组长会话（`is_lead_meta`）被 `touch` 直接跳过；插件驱动会话创建时
/// 标题已非空，因此天然不触发；手动改名之后 title 也非空，同样不会重新生成——
/// 一旦标题非空，`touch` 永不再返回 `true`，这条路径对同一会话只会有一次。
///
/// 材料只有用户输入（`prompt`，即 `display_text` 原话，不含附件清单）。因此
/// 纯图片 / 纯附件消息（`display_text` 为空）没有可用材料，不会生成标题，
/// 会话保留兜底或空标题。
///
/// **不得阻塞 driver**：调用方负责 `tokio::spawn`，本函数只做生成 → 清洗 → CAS
/// → 广播，任何失败都静默保留兜底标题（标题是锦上添花，绝不能影响 turn）。
async fn generate_title_for_first_input(
    thread_id: String,
    prompt: String,
    fallback: String,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    store: Arc<crate::thread_store::ThreadStore>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    // 材料只有用户输入：`None` 让 `build_title_material` 省略「助手回复：」段。
    let generated = tokio::time::timeout(
        TITLE_TIMEOUT,
        yi_agent_core::title::generate_title(&provider, &config, &prompt, None),
    )
    .await;
    match generated {
        Ok(Ok(Some(title))) => {
            // 仅在 CAS **确实替换**（`Ok(true)`）后广播：`Ok(false)` 表示标题已被
            // 手动改名抢占或 thread 不存在，磁盘上不是这个值，广播它会让客户端
            // 显示一个不存在的标题。
            match store.set_title_if_unchanged(&thread_id, &fallback, &title) {
                Ok(true) => {
                    if let Err(e) = write_notification(
                        &hub,
                        &Notification::TitleChanged {
                            thread_id: thread_id.clone(),
                            title,
                        },
                    )
                    .await
                    {
                        eprintln!("[app-server] failed to broadcast title change {thread_id}: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("[app-server] failed to set LLM title {thread_id}: {e}"),
            }
        }
        // 模型产出无法作为标题：保留兜底标题。
        //
        // 这条路径过去完全静默，导致「LLM 标题不常见」查了很久：推理模型把
        // `max_tokens` 烧在思考上、正文留空，就落在这里。留一条 info，
        // 下次同类症状能直接从日志看出来。
        Ok(Ok(None)) => {
            tracing::info!(
                %thread_id,
                "title generation produced no usable title; keeping fallback"
            );
        }
        Ok(Err(e)) => {
            tracing::info!(%e, %thread_id, "title generation failed; keeping fallback");
        }
        Err(_) => {
            tracing::info!(%thread_id, "title generation timed out; keeping fallback");
        }
    }
}

/// 从本轮 items 里取第一条助手文本：作为回填给组长等待方的回复。
///
/// 空文本的 `AgentMessage` 视为不存在，避免把一段空回复当成组长真的说了话。
///
/// 标题生成已不再使用它：标题在用户输入落地时就生成，此刻助手还没有输出
/// （见 [`generate_title_for_first_input`]）。
fn completed_items_agent_text(items: &[crate::protocol::Item]) -> Option<String> {
    items.iter().find_map(|item| match item {
        crate::protocol::Item::AgentMessage { text, .. } if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    })
}

/// 组长那轮要回填给等待方 thread 的文本。
///
/// 与落盘/标题生成**同一条口径**:直接复用 [`completed_items_agent_text`],不另立
/// 一套抽取逻辑。两处若各写一遍,「标题取的文本」与「回填给等待方的文本」会在
/// 今后任何一次口径变更(例如改成取最后一条)里悄悄分叉,而后者分叉的表现是
/// 等待方收到一段与组长实际回复无关的文字——比拿不到回复更难排查。
///
/// `None` = 本轮没有可回填的助手文本(失败轮、纯工具轮、写帧失败而中断的轮):
/// 调用方**不得**把它当成一条空回复回填,那会让等待方以为组长回了话。
fn lead_reply_text(items: &[crate::protocol::Item]) -> Option<String> {
    completed_items_agent_text(items)
}

/// 在**没有 turn 在跑**的时刻执行一条会话命令，返回（可能被重建过的）agent。
///
/// 清空与压缩都**就地**替换会话内容(`set_session_messages`),不换 `Arc`:同一
/// 会话句柄可能已被委派工具的 `CallerContext` 绑定,换句柄会让 `fork` 读到被丢弃
/// 的旧记录。按值传入/返回保留原样——将来若某个命令确实需要换掉 `Agent` 本体,
/// 这条签名就不必再改。
#[allow(clippy::too_many_arguments)]
async fn apply_session_command(
    mut agent: yi_agent_core::Agent,
    command: SessionCommand,
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
    store: &crate::thread_store::ThreadStore,
    thread_id: &str,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    status: &Arc<std::sync::Mutex<ThreadStatus>>,
    mcp: Option<&Arc<yi_agent_mcp::McpManager>>,
    mcp_workdir: &std::path::Path,
) -> yi_agent_core::Agent {
    match command {
        SessionCommand::Clear { reply } => {
            agent.set_session_messages(Vec::new());
            // `set_session_messages` 只换消息,不碰 `last_input_tokens`;若不在此清零,
            // 清空后仍留着一个陈旧的(可能很大的)计数,`maybe_auto_compact` 会在下一轮
            // 立刻误触发一次压缩。旧实现换的是全新 `Session`(计数为 `None`),这里补回
            // 那个语义——与 CLI 的 `/clear` 路径一致。
            agent
                .session_handle()
                .lock()
                .unwrap()
                .set_last_input_tokens(None);
            let truncate = store.truncate(thread_id);
            if let Err(e) = &truncate {
                eprintln!("[app-server] failed to truncate thread log {thread_id}: {e}");
            }
            // 世代递增：/clear 让可见内容集合从"有"变"无"，客户端手上的内容就此
            // 结构性不一致，必须靠 epoch 变化才能被判为过期（spec §3）。这是**唯一**
            // 的递增点——`compact` 不在此列，它不丢 item（spec §1.2）。
            // 失败只记 stderr：清空本身已成功，世代滞后最坏让客户端做一次多余的
            // 全量 resume（幂等），绝不因此让 clear 报错。
            if truncate.is_ok() {
                if let Err(e) = store.bump_content_epoch(thread_id) {
                    eprintln!("[app-server] failed to bump content epoch ({thread_id}): {e}");
                }
                // clear 是唯一真正清空可见内容的路径：世代已递增，游标也必须清掉，
                // 否则 listAll 会继续报一个已被删除的 item id。
                // 与上面的 bump 同款：失败只记 stderr，绝不让 clear 报错。
                if let Err(e) = store.set_last_item_id(thread_id, None) {
                    eprintln!("[app-server] failed to clear last_item_id for {thread_id}: {e}");
                }
            }
            // 不是 turn 收尾：turn_id 传 None（不发 Finished），也不 touch meta。
            persist_and_finish_turn(
                thread_id,
                None,
                // 不是回合结束（turn_id 为 None），完成时刻不写盘；传 0 明示。
                0,
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                // 会话命令不产生 provider 调用：本轮累加值为空。
                None,
                store,
                hub,
                turn_tx,
                status,
            )
            .await;
            // `truncate` 返回 io::Result<bool>，而 reply 通道是 Result<(), String>。
            let _ = reply.send(truncate.map(|_| ()).map_err(|e| e.to_string()));
        }
        SessionCommand::Compact { reply } => {
            let session = agent.session();
            let outcome = match yi_agent_core::compact_session(provider, config, &session).await {
                Ok(Some(compacted)) => {
                    // Replace the contents in place so the session `Arc` — and
                    // any `CallerContext` bound to it — stays valid.
                    agent.set_session_messages(compacted.messages().to_vec());
                    // The replaced session used to be brand new, so its token
                    // count meant "no measurement yet". Keep that: a stale
                    // pre-compaction count would re-trigger auto-compaction on
                    // the very next turn.
                    agent
                        .session_handle()
                        .lock()
                        .unwrap()
                        .set_last_input_tokens(None);
                    CompactOutcome::Compacted
                }
                Ok(None) => CompactOutcome::NotReduced,
                Err(e) => CompactOutcome::Failed(e.to_string()),
            };
            persist_and_finish_turn(
                thread_id,
                None,
                // 不是回合结束（turn_id 为 None），完成时刻不写盘；传 0 明示。
                0,
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                // 会话命令不产生 provider 调用：本轮累加值为空。
                None,
                store,
                hub,
                turn_tx,
                status,
            )
            .await;
            let _ = reply.send(outcome);
        }
        SessionCommand::SetModel {
            effective,
            model_ref,
            reply,
        } => {
            // provider 构造放在最前：失败就在这里短路，`agent` 尚未被消费，原样返回。
            // 这是「绝不半重建」的关键——绝不在换了 config/句柄之后才发现造不出 provider。
            let provider = match yi_agent_runtime::bootstrap::build_provider(&effective) {
                Ok(provider) => provider,
                Err(error) => {
                    tracing::warn!(%error, ?model_ref, "session model switch failed; keeping the old agent");
                    let _ = reply.send(Err(error.to_string()));
                    return agent;
                }
            };
            // 新 cfg 只覆盖 model（provider/api_url/api_key 已烤进新 provider；其余
            // budget/system_prompt 等沿用原 config，避免切模型顺带重置别的设置）。
            let mut next_config = agent.config().clone();
            next_config.model = effective.model.clone();
            // 三样必须原样保留：session `Arc`（否则委派 caller 读不到本会话）、
            // 工具注册表、审批路径（checker + decision_rx）。与
            // [`rebuild_thread_agent_with_theme`] 的语义一致。
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider, tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(model = %effective.model, ?model_ref, "session model switched");
            // Design §5.3 step 4:重建成功后广播本会话的生效模型,让所有订阅该
            // thread 的客户端(桌面 + 手机)立刻刷新,无需轮询。
            //
            // 在此直接广播、而非让 driver 经事件流走 Translator:`SetModel` 只在
            // **没有 turn 在跑**时经本函数执行,此刻不存在 `agent.run()` 的事件流
            // 来承载 `AgentEvent::ModelChanged`(translator 仅在本轮 stream 上被
            // 驱动)。直接广播与「重建完成」同一时刻发生;Translator 侧那条臂保留,
            // 供将来事件流真的承载该事件时复用。
            let _ = write_notification(
                hub,
                &Notification::ModelChanged {
                    thread_id: thread_id.to_string(),
                    model: effective.model.clone(),
                },
            )
            .await;
            let _ = reply.send(Ok(()));
            agent = rebuilt;
        }
        SessionCommand::SetReasoningEffort { effort, reply } => {
            // effort 不烤进 provider（URL/key 未变），故复用当前 `provider`，
            // 只换 `AgentConfig.gen_params.reasoning_effort` 后重建 agent。
            // 三样必须原样保留：session `Arc`、工具注册表、审批路径
            // （checker + decision_rx）。与 `SetModel` 的重建语义一致。
            let mut next_config = agent.config().clone();
            next_config.gen_params.reasoning_effort = effort.clone();
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider.clone(), tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(?effort, "session reasoning effort switched");
            let _ = reply.send(Ok(()));
            agent = rebuilt;
        }
        SessionCommand::ReloadMcp { reply } => match mcp {
            // 没有 manager 就没什么可重载的。这不该发生(mcp_reload 工具在
            // 无配置时也会带一个空 manager),但如实回错好过假装成功。
            None => {
                let _ = reply.send(Err("本会话没有可用的 MCP 管理器".into()));
            }
            Some(mcp) => {
                let report = mcp.reload(mcp_workdir, yi_agent_mcp::now_unix()).await;
                let failures: Vec<String> = report
                    .servers
                    .iter()
                    .filter_map(|s| s.error.as_ref().map(|e| format!("{}: {e}", s.name)))
                    .collect();

                // 用刷新后的工具集重建 agent:三样照旧保留(session `Arc`、
                // 审批路径),与 `SetModel` / `SetReasoningEffort` 的重建语义一致。
                let mut refreshed = (*agent.tools()).clone();
                mcp.refresh_registry(&mut refreshed);
                let next_config = agent.config().clone();
                let session_handle = agent.session_handle();
                let permission = agent.permission_checker();
                let decision = agent.decision_rx();
                let mut rebuilt = yi_agent_core::Agent::new(
                    Arc::clone(provider),
                    std::sync::Arc::new(refreshed),
                    next_config,
                )
                .with_session_arc(session_handle);
                if let (Some(checker), Some(rx)) = (permission, decision) {
                    rebuilt = rebuilt.with_permission(checker, rx);
                }
                tracing::info!(
                    servers = report.servers.len(),
                    failed = failures.len(),
                    "session MCP config reloaded"
                );
                agent = rebuilt;
                let _ = reply.send(if failures.is_empty() {
                    Ok(())
                } else {
                    Err(format!("部分 server 探测失败: {}", failures.join("；")))
                });
            }
        },
        SessionCommand::SetPreserveThinking { preserve, reply } => {
            // provider 不变（开关不烤进 URL/key），故复用当前 `provider`，只把
            // `AgentConfig.preserve_thinking` 换掉后重建 agent。
            //
            // **先**把 strip 落到共享 session 本体：`preserve_thinking` 只影响请求
            // 副本（见 core 的 `run_loop`），若不在此就地剥离，落盘/展示的权威
            // transcript 仍带着凭证，重开会话后它们会「复活」——与「关闭保留」的
            // 开关语义直接矛盾（spec §4.1）。`set_session_messages` 与
            // `session_handle()` 摸的是同一个 `Arc<Mutex<Session>>`，故先写后取。
            if !preserve {
                let stripped = yi_agent_core::strip_thinking(agent.session().messages());
                agent.set_session_messages(stripped);
            }
            let mut next_config = agent.config().clone();
            next_config.preserve_thinking = preserve;
            // 三样必须原样保留：session `Arc`（否则委派 caller 读不到本会话）、
            // 工具注册表、审批路径（checker + decision_rx）。与
            // `SetReasoningEffort` 的重建语义一致。
            let session_handle = agent.session_handle();
            let tools = agent.tools();
            let permission = agent.permission_checker();
            let decision = agent.decision_rx();
            let mut rebuilt = yi_agent_core::Agent::new(provider.clone(), tools, next_config)
                .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            tracing::info!(preserve, "session preserve-thinking switched");
            agent = rebuilt;
            // 立即把（可能已剥离的）会话状态落盘：否则「关闭后、下一轮开始前」
            // 重开会话，日志最后一行仍是切换前那条带凭证的记录，凭证就此「复活」
            // ——与「关闭保留」的语义矛盾（spec §7.4：关闭时落盘结果同样被剥离）。
            // 必须在 `agent = rebuilt` **之后**调用：`persist_and_finish_turn` 会读
            // `agent.config().preserve_thinking` 决定落盘副本是否剥离，读到旧值就
            // 会在关闭时仍把凭证写盘。与 `Clear` / `Compact` 同款：`turn_id` 传
            // `None`（不发 Finished），也不 touch meta（meta 已由 RPC 层落盘）。
            persist_and_finish_turn(
                thread_id,
                None,
                // 不是回合结束（turn_id 为 None），完成时刻不写盘；传 0 明示。
                0,
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                // 会话命令不产生 provider 调用：本轮累加值为空。
                None,
                store,
                hub,
                turn_tx,
                status,
            )
            .await;
            let _ = reply.send(Ok(()));
        }
    }
    agent
}

/// 单个 thread 的 driver task:串行消费 turn,驱动 `agent.run()` 的 stream,
/// 经 `Translator` 写成协议通知。
///
/// **取消安全**:`Agent::run()` 每次都会重置 cancel token,因此必须在
/// `run().await` 返回**之后**再取 `cancel_token()`,否则 `turn/interrupt` 无效。
///
/// **权限审批闭环**:遇到 `AgentEvent::PermissionRequest` 时,driver 发出反向
/// 请求 `item/toolCall/requestApproval`(id 取自进程级 `perm_seq`)并等待客户端
/// 经 `pending` 登记表回传的决定;超时或中断按 `Deny` 处理。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_thread_driver(
    thread_id: String,
    mut agent: yi_agent_core::Agent,
    mut prompt_rx: mpsc::Receiver<TurnPrompt>,
    mut interrupt_rx: mpsc::Receiver<String>,
    mut interject_rx: mpsc::Receiver<InterjectionRequest>,
    mut session_rx: mpsc::Receiver<SessionCommand>,
    hub: Arc<crate::broadcast::Broadcaster>,
    // 发起本 thread 的客户端(取自某次 thread/start)。反向审批请求**广播**给
    // 所有客户端(决策 5:谁先答谁生效),故这里不再需要它来寻址;保留它是为了
    // 复刻「发起方断开即收尾本轮」的原判据(见下方 `is_connected` 守卫)。
    client: crate::broadcast::ClientId,
    turn_tx: mpsc::Sender<TurnEvent>,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: Arc<AtomicU64>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    store: Arc<crate::thread_store::ThreadStore>,
    // 该 thread 的共享状态句柄;driver 在各转换点更新并推送
    // `thread/status/updated`(与写进 `ThreadSession.status` 的是同一 `Arc`)。
    status: Arc<std::sync::Mutex<ThreadStatus>>,
    // clear / compact 需要它们：compact 要调 provider 生成摘要，两者都要重建 agent。
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    // subagent 用量汇总。由 `watch_children` 写入，本 driver 读出来推给客户端。
    mut subagent_rx: tokio::sync::watch::Receiver<SubagentTotals>,
    // MCP 管理器。driver 每轮开跑前用它检查 `.yi-agent/mcp.json` 是否被改过，
    // `SessionCommand::ReloadMcp` 也靠它重新读盘。配置缺失时是 `None`。
    mcp: Option<Arc<yi_agent_mcp::McpManager>>,
    // 该 thread 的 cwd：`config.workdir` 在 driver 里被 clone 过多次，但
    // MCP 配置固定在 thread cwd 下的 `.yi-agent/mcp.json`，故显式传入更清楚。
    mcp_workdir: std::path::PathBuf,
) {
    // 每个 thread 一个 translator:item id 带 turn_id 前缀(`item-<turn_id>-<n>`),
    // 故即便 resume 后计数器归 1,新 item 也不会与回放的历史 id 冲突。
    // 累计基线：本 thread 已落盘轮次之和。读失败按 0 处理——最坏是首次打开时
    // 累计偏小，绝不能让 driver 起不来。
    let baseline = store
        .load(&thread_id)
        .ok()
        .flatten()
        .map(|loaded| loaded.totals)
        .unwrap_or_default();
    let mut translator = Translator::new(thread_id.clone(), baseline);
    // `/clear` 之前已经烧掉的 subagent 用量。daemon 的 task 记录不会随 clear 删除，
    // 故必须靠基线把旧量扣掉，否则清零后旧数字会被下一次推送带回来。
    let mut subagent_baseline = SubagentTotals::default();
    // 最近一次从 watch 收到的原始汇总（未减基线）。
    let mut subagent_raw = SubagentTotals::default();
    // 只在首个 turn 尝试激活:objective 会被写进 root 任务,不能用占位串,也不能
    // 每轮重写。即使 `activate` 为 None 也置位,避免每轮重试。
    let mut activation_attempted = false;
    // 内层 select 期间收到的会话命令。此刻 turn 正在跑，不能改 agent，
    // 暂存到这里，等本轮收尾时执行（见 persist_and_finish_turn 之后的处理）。
    let mut pending_session_command: Option<SessionCommand> = None;
    loop {
        // 空闲路径:没有 turn 在跑时收到的会话命令,立即执行——此刻改 agent 是安全的。
        // 没有这条路径,空闲 thread 永远读不到 clear / compact(命令会一直躺在 channel 里)。
        //
        // 只在 `select!` 里做 `recv`,把执行放到 `select!` 之外:`apply_session_command`
        // 按值消费并返回 agent,若写进分支体就会与另一分支的 `prompt_rx` 借用冲突。
        let turn_prompt = tokio::select! {
            Some(command) = session_rx.recv() => {
                let clearing = matches!(&command, SessionCommand::Clear { .. });
                agent = apply_session_command(
                    agent, command, &provider, &config, &store, &thread_id, &hub, &turn_tx,
                    &status, mcp.as_ref(), &mcp_workdir,
                )
                .await;
                if clearing {
                    // 顺序要紧：先把当前原始汇总钉成基线，再归零 translator。
                    // 不钉的话，下一次 children 推送仍带着 clear 前的量，数字会
                    // 立刻弹回旧值。
                    subagent_baseline = subagent_raw;
                    for n in translator.reset_totals() {
                        let _ = write_notification(&hub, &n).await;
                    }
                }
                continue;
            }
            // subagent 用量变化：`watch_children` 已把它算好送过来。空闲时也要
            // 处理，否则子任务在 turn 之外收尾时报的量要等到下一轮才显示。
            Ok(()) = subagent_rx.changed() => {
                subagent_raw = *subagent_rx.borrow_and_update();
                let totals = subagent_raw.saturating_sub(subagent_baseline);
                let notification = translator.set_subagent_totals(totals);
                let _ = write_notification(&hub, &notification).await;
                continue;
            }
            maybe_prompt = prompt_rx.recv() => maybe_prompt,
        };
        let Some(TurnPrompt {
            turn_id,
            prompt,
            activate,
            attachments: turn_attachments,
            image_blocks,
            image_refs,
            display_text,
            lead_request_id,
        }) = turn_prompt
        else {
            break; // prompt_rx 关闭:driver 收尾退出
        };
        if !activation_attempted {
            activation_attempted = true;
            if let Some(binding) = activate {
                let objective = prompt.clone();
                // 同步调用放到阻塞线程池:driver 是 async 任务,直接调用会占住 executor。
                // `activate` 先探活、必要时重建 runtime,所以即便 daemon 在这轮之前
                // 已经死掉(例如承载它的终端被关掉),这里也能自愈后再激活。
                let outcome =
                    tokio::task::spawn_blocking(move || binding.activate(&objective)).await;
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(cause)) => tracing::warn!(
                        stage = "activation",
                        %cause,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                    Err(error) => tracing::warn!(
                        stage = "activation",
                        %error,
                        %thread_id,
                        "subagent delegation unavailable for this thread"
                    ),
                }
            }
        }
        // 本轮累加器:最终 item 与最近一次用量(用于落盘)。
        // `last_usage` 记录的是本轮**最后一次** provider 调用的完整快照
        // (input 来自 `message_start`、output 来自 `message_delta`,由 Translator 合并)。
        let mut completed_items: Vec<crate::protocol::Item> = Vec::new();
        let mut last_usage: Option<crate::thread_store::TurnUsage> = None;
        // 本轮**所有** provider 调用的累加值，落盘供下次启动恢复累计基线。
        let mut turn_usage_total: Option<crate::thread_store::TurnUsage> = None;
        // 气泡正文=用户原话(不含注入的清单);`prompt`(带清单)只送模型。
        let user_prompt = display_text;
        // 本轮附件暂存:落盘时写进开启的 userMessage item。
        let user_attachments = turn_attachments;
        // 本轮图片暂存:落盘时写进开启的 userMessage item 的 `images`。
        // 直接取 `TurnPrompt` 上 `prepare_turn_core` 算好的那一份引用,不再从
        // 内容块里反推元数据:反推就得重新求 `size`,而实时开启项（`start_turn_core`
        // 里的 `opening_user_item`）用的是 prepare 阶段那一份——两处各算各的,
        // 一旦口径不同,同一个图在实时气泡与回放气泡上会显示成两个大小。
        let user_image_refs = image_refs;
        // 送给模型的内容块:文本(若有)在前,图片在后。
        let mut blocks: Vec<yi_agent_core::ContentBlock> = Vec::new();
        if !prompt.is_empty() {
            blocks.push(yi_agent_core::ContentBlock::Text(prompt.clone()));
        }
        blocks.extend(image_blocks.iter().cloned());
        // 防御:空 blocks 会给模型塞一条没有任何内容的消息。正常路径够不到,
        // 但有一条例外:一条**只有图片**的消息里那张图被丢弃时(上传句柄未知 /
        // 已过期——见 `prepare_turn_core` 的丢弃分支),文本与图片就都空了。
        // 此时这里必须自己兜住,而不是把边界甩给模型。
        if blocks.is_empty() {
            blocks.push(yi_agent_core::ContentBlock::Text(String::new()));
        }
        translator.set_turn(turn_id.clone());

        // turn 开始就把提问落进 checkpoint：即使这一轮随后立刻崩溃，
        // 至少提问不会丢。
        checkpoint(
            &store,
            &thread_id,
            build_partial(
                PartialTurnInput {
                    turn_id: &turn_id,
                    user_prompt: &user_prompt,
                    user_attachments: &user_attachments,
                    user_image_refs: &user_image_refs,
                },
                &[],
                agent.session().messages().to_vec(),
                None,
                None,
            ),
        );

        // 用户输入一到就命名会话:不再等本轮跑完。`touch` 仍兼任「是否首次」的
        // 判据——它返回 `true` 只可能是 meta 的 title 刚由 `None` 变成兜底截断值。
        // 生成放到独立 task:driver 是本 thread 唯一的串行执行者,若在这里 await,
        // 标题模型最慢会把用户首字延迟 TITLE_TIMEOUT（5 秒）。
        if !user_prompt.trim().is_empty() {
            match store.touch(&thread_id, Some(&user_prompt)) {
                Ok(true) => {
                    tokio::spawn(generate_title_for_first_input(
                        thread_id.clone(),
                        user_prompt.clone(),
                        crate::thread_store::title_from(&user_prompt),
                        Arc::clone(&provider),
                        config.clone(),
                        Arc::clone(&store),
                        Arc::clone(&hub),
                    ));
                }
                Ok(false) => {}
                Err(e) => eprintln!("[app-server] failed to update meta for {thread_id}: {e}"),
            }
        }

        // 每轮开跑前检查 `.yi-agent/mcp.json` 是否被改过(用户在别的窗口编辑、
        // 或 `mcp/set` 刚写过盘)。只探测变化的 server,缓存命中则秒过。
        if let Some(mcp) = &mcp {
            if mcp.needs_reload(&mcp_workdir, yi_agent_mcp::now_unix()) {
                let report = mcp.reload(&mcp_workdir, yi_agent_mcp::now_unix()).await;
                for s in &report.servers {
                    if let Some(err) = &s.error {
                        tracing::warn!(server = %s.name, error = %err, "MCP reload probe failed");
                    }
                }
                let mut refreshed = (*agent.tools()).clone();
                mcp.refresh_registry(&mut refreshed);
                let next_config = agent.config().clone();
                let session_handle = agent.session_handle();
                let permission = agent.permission_checker();
                let decision = agent.decision_rx();
                let mut rebuilt = yi_agent_core::Agent::new(
                    Arc::clone(&provider),
                    Arc::new(refreshed),
                    next_config,
                )
                .with_session_arc(session_handle);
                if let (Some(checker), Some(rx)) = (permission, decision) {
                    rebuilt = rebuilt.with_permission(checker, rx);
                }
                agent = rebuilt;
                tracing::info!("MCP config changed on disk; thread registry reloaded");
            }
        }

        // 每轮开跑前刷新 skills catalog:skills 热重载,让本轮看到最新的
        // system prompt(catalog 未变时输出逐字节相同,不破坏 prompt cache)。
        if let Some(handle) = &catalog {
            if let Some(new_prompt) = handle.current_system_prompt() {
                agent.set_system_prompt(Some(new_prompt));
            }
        }

        let mut stream = match agent.run_blocks(blocks).await {
            Ok(s) => s,
            Err(e) => {
                // run() 本身失败:翻译成 Error → turn/completed(failed)。
                for n in translator.on_event(yi_agent_core::AgentEvent::Error(e)) {
                    let _ = write_notification(&hub, &n).await;
                }
                // 这条路径不走 `persist_and_finish_turn`，但它**确实**是一次回合
                // 结束（`turn/completed(failed)` 已发出）。不记完成时间的话，
                // 「失败也是跑完了」这件事在另一端的未读判定里就查不到——重连后
                // 蓝点不亮，用户以为还在跑。
                // 复用刚广播出去的那个完成时刻，理由同常规收尾：落盘值若更晚，
                // 另一端的 markRead 就盖不住它，蓝点会复活。
                let failed_at = match translator.completed_at() {
                    0 => crate::thread_store::now_millis(),
                    at => at,
                };
                if let Err(e) = store.mark_turn_completed(&thread_id, failed_at) {
                    eprintln!("[app-server] failed to record turn completion for {thread_id}: {e}");
                }
                // 中途失败没有「组长说了什么」可言:回复传 `None`,绝不伪造一条
                // 空回复去糊弄等待方(那比拿不到回复更难排查)。
                let _ = turn_tx
                    .send(finished_event(&thread_id, &turn_id, None, None))
                    .await;
                // run() 失败即本轮结束:thread 立刻回 Idle(失败是事件不是状态)。
                let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Idle).await;
                // 本轮已结束（失败）：turn 开始写的 checkpoint 必须清掉，否则盘上会
                // 留一条已结束 turn 的残留，重启后被 resume 当成待恢复 turn。
                // 与其它 clear 站点同款：尽力删，失败只记 stderr，绝不影响 turn 收尾。
                if let Err(e) = store.clear_partial(&thread_id) {
                    eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                }
                continue;
            }
        };

        // 必须在 run() 之后捕获:run() 内部会重建 cancel token。
        let cancel_token = agent.cancel_token();
        // 同一次 run 的投递句柄;run() 结束即失效,下一轮重新取。
        let inbox = agent.inbox_handle();
        let mut cancel_sent = false;
        // 逐字流合并器（仅在本轮驱动内有效）。当存在订阅者时，`item/delta`
        // 先按 thread 攒批，再由 100ms tick 或非 delta 屏障刷出；无订阅者时
        // 走原来的直发路径，字节不变。
        let mut coalescer = DeltaCoalescer::default();
        let mut delta_tick = tokio::time::interval(Duration::from_millis(100));
        delta_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // checkpoint 去抖：item finalize 只标脏，由 500ms tick 合并成一次写盘，
        // 避免长 turn 里每个 item 都同步写一次。
        let mut checkpoint_tick = tokio::time::interval(Duration::from_millis(500));
        checkpoint_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut checkpoint_dirty = false;

        loop {
            tokio::select! {
                ev = stream.next() => {
                    match ev {
                        Some(yi_agent_core::AgentEvent::PermissionRequest {
                            request_id, tool_name, tool_input, prefix_suggestion, kind,
                        }) => {
                            let perm_id = format!("perm-{}", perm_seq.fetch_add(1, Ordering::SeqCst));
                            let (dtx, mut drx) = oneshot::channel::<Decision>();
                            pending.lock().await.insert(perm_id.clone(), dtx);

                            let reverse = ReverseRequest {
                                jsonrpc: JSONRPC_VERSION,
                                id: perm_id.clone(),
                                method: "item/toolCall/requestApproval",
                                params: json!({
                                    "thread_id": thread_id,
                                    "turn_id": turn_id,
                                    "request_id": request_id,
                                    "tool_name": tool_name,
                                    "tool_input": tool_input,
                                    "prefix_suggestion": prefix_suggestion,
                                    "kind": kind,
                                }),
                            };
                            let frame = match serde_json::to_value(&reverse) {
                                Ok(frame) => frame,
                                Err(e) => {
                                    tracing::error!("failed to serialize reverse request: {e}");
                                    pending.lock().await.remove(&perm_id);
                                    // 序列化失败即放弃本轮:无回复可回填。
                                    let _ = turn_tx.send(finished_event(&thread_id, &turn_id, None, None)).await;
                                    // 本轮就此结束：turn-start 写的 checkpoint 必须清掉，
                                    // 否则盘上留一条已结束 turn 的残留，重启后 resume 会
                                    // 把最后这一轮误标为 interrupted。与其它 clear 站点同款：
                                    // 尽力删，失败只记 stderr，绝不影响收尾。
                                    if let Err(e) = store.clear_partial(&thread_id) {
                                        eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                    }
                                    return;
                                }
                            };
                            // 反向审批请求**广播**给所有客户端(spec §4.4;Tier 1
                            // 决策 5:审批先到先得)。发起 turn 的是共享的
                            // app-server,等待决定的设备可能不止一台——每台都要
                            // 看到弹窗,谁先答谁生效,其余设备靠随后的
                            // `item/toolCall/approvalResolved` 广播关掉弹窗。
                            //
                            // 断连语义:`broadcast` 是 `try_send`,不会像旧的
                            // `reply` 那样在被寻址客户端已注销时返回 `Closed`。
                            // 若发起客户端已断开,本轮不该继续等一个永远不会到的
                            // 决定,故这里用 `is_connected` 复刻原判据——但仍要
                            // 先把帧广播出去,让其它在线客户端(如第二台手机)能
                            // 应答:发起方断线不等于该审批无人可答。
                            // 只推给"订阅了该 thread"的客户端 + 全收的客户端（桌面）：
                            // 没打开这个会话的手机不该被它的审批打断。
                            hub.broadcast_for(Some(&thread_id), frame);
                            if !hub.is_connected(&client) {
                                // 发起客户端已断开:与旧语义一致(写失败即收尾)。
                                pending.lock().await.remove(&perm_id);
                                // 客户端断连即放弃本轮:无回复可回填。
                                let _ = turn_tx.send(finished_event(&thread_id, &turn_id, None, None)).await;
                                // 本轮就此结束（客户端断连，不是崩溃）：清掉 checkpoint，
                                // 否则重启后会把这一轮误报为 interrupted。与其它站点同款。
                                if let Err(e) = store.clear_partial(&thread_id) {
                                    eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                }
                                return;
                            }
                            // 反向请求已写出:进入等待审批状态。
                            let _ = update_status(
                                &hub,
                                &status,
                                &thread_id,
                                ThreadStatus::AwaitingApproval,
                            )
                            .await;

                            // 等客户端决定;超时或中断按 Deny 处理。只接受针对当前 turn 的中断,
                            // 其它 turn 的残留信号忽略后继续等待(deadline 不重置)。
                            let timeout = tokio::time::sleep(permission_timeout);
                            tokio::pin!(timeout);
                            let decision = loop {
                                tokio::select! {
                                    d = &mut drx => break d.unwrap_or(Decision::Deny),
                                    _ = &mut timeout => {
                                        pending.lock().await.remove(&perm_id);
                                        break Decision::Deny;
                                    }
                                    Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                                        if target == turn_id {
                                            cancel_sent = true;
                                            cancel_token.cancel();
                                            pending.lock().await.remove(&perm_id);
                                            break Decision::Deny;
                                        }
                                        // 其它 turn 的残留信号:忽略,继续等待。
                                    }
                                }
                            };

                            // 决定已到(或超时/中断按 Deny):恢复为 Running,
                            // 继续消费本轮 stream。
                            let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Running)
                                .await;

                            if let Some(tx) = &decision_tx {
                                let _ = tx.send((request_id, decision)).await;
                            }
                        }
                        Some(e) => {
                            for n in translator.on_event(e) {
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
                                    checkpoint_dirty = true;
                                }
                                // 用量通知携带本轮累积快照(见 Translator::on_event),最后一条即落盘用的完整用量。
                                if let crate::protocol::Notification::TokenUsage {
                                    model,
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    ..
                                } = &n
                                {
                                    last_usage = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: *input_tokens,
                                        output_tokens: *output_tokens,
                                        cache_creation_input_tokens: *cache_creation_input_tokens,
                                        cache_read_input_tokens: *cache_read_input_tokens,
                                    });
                                    // 同一条通知里带着translator 算好的累计量：本轮值随时
                                    // 刷新，turn 收尾时取最后一次（= 全部调用之和）。
                                    let totals = translator.turn_totals();
                                    turn_usage_total = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: saturate_u32(totals.input_tokens),
                                        output_tokens: saturate_u32(totals.output_tokens),
                                        cache_creation_input_tokens: saturate_u32(
                                            totals.cache_creation_input_tokens,
                                        ),
                                        cache_read_input_tokens: saturate_u32(
                                            totals.cache_read_input_tokens,
                                        ),
                                    });
                                }
                                // 逐字流：仅当存在订阅者时合并；否则走原来的直发。
                                // **屏障**：任何**非 delta** 通知到达时，先刷它所属 thread
                                // 的待发 delta，保证「先吐字、再标记这项完成」的顺序不变。
                                // 缺这道屏障时，某 item 的 `item/completed` 会**越过**自己
                                // 那条尚未刷出的 delta 先发出：客户端先按 id 整条替换成
                                // 完整文本，随后迟到的 delta 又被就地追加一遍，尾部于是
                                // 重复。注释与实现必须同时成立，这里补上被漏掉的分支。
                                let mut to_send: Vec<Notification> = Vec::new();
                                match &n {
                                    Notification::ItemDelta { thread_id: t, item_id, delta }
                                        if hub.has_subscribed_clients() =>
                                    {
                                        for (pid, text) in coalescer.push(t, item_id, delta) {
                                            to_send.push(Notification::ItemDelta {
                                                thread_id: t.clone(),
                                                item_id: pid,
                                                delta: text,
                                            });
                                        }
                                    }
                                    _ => {
                                        // 非 delta 通知：先刷**它自己 thread** 的待发
                                        // delta（若有），再发这条通知。无订阅者时
                                        // 缓冲恒空，这里是 no-op、字节不变。
                                        if let Some(t) = n.thread_key() {
                                            if let Some((pid, text)) = coalescer.take(t) {
                                                to_send.push(Notification::ItemDelta {
                                                    thread_id: t.to_string(),
                                                    item_id: pid,
                                                    delta: text,
                                                });
                                            }
                                        }
                                        to_send.push(n.clone());
                                    }
                                }
                                for out in to_send {
                                    if write_notification(&hub, &out).await.is_err() {
                                        // 客户端可能已断开;先上报 Finished,
                                        // 避免 active_turn_id 永久卡住。
                                        // 写帧失败即放弃本轮:无回复可回填。
                                        let _ = turn_tx.send(finished_event(&thread_id, &turn_id, None, None)).await;
                                        // 本轮就此结束（写失败，不是崩溃）：清掉 checkpoint，
                                        // 否则重启后会把这一轮误报为 interrupted。与其它站点同款。
                                        if let Err(e) = store.clear_partial(&thread_id) {
                                            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
                                        }
                                        return;
                                    }
                                }
                            }
                        }
                        None => {
                            // 流结束：把该 thread 攒下的逐字流刷出，避免丢尾
                            // （最后一个 tick 之后、流结束之前可能仍有待发 delta）。
                            if let Some((pid, text)) = coalescer.take(&thread_id) {
                                let _ = write_notification(
                                    &hub,
                                    &Notification::ItemDelta {
                                        thread_id: thread_id.clone(),
                                        item_id: pid,
                                        delta: text,
                                    },
                                )
                                .await;
                            }
                            break;
                        }
                    }
                }
                _ = delta_tick.tick() => {
                    // 100ms 兜底刷出：即便没有后续非 delta 通知，攒下的逐字流
                    // 也必须按时送达。
                    for (t, (pid, text)) in coalescer.take_all() {
                        let _ = write_notification(
                            &hub,
                            &Notification::ItemDelta {
                                thread_id: t,
                                item_id: pid,
                                delta: text,
                            },
                        )
                        .await;
                    }
                }
                _ = checkpoint_tick.tick(), if checkpoint_dirty => {
                    checkpoint(
                        &store,
                        &thread_id,
                        build_partial(
                            PartialTurnInput {
                                turn_id: &turn_id,
                                user_prompt: &user_prompt,
                                user_attachments: &user_attachments,
                                user_image_refs: &user_image_refs,
                            },
                            &completed_items,
                            agent.session().messages().to_vec(),
                            last_usage.clone(),
                            turn_usage_total.clone(),
                        ),
                    );
                    checkpoint_dirty = false;
                }
                // subagent 用量在 turn 进行中变了：立刻推一条，让数字随子任务
                // 每次 provider 调用增长（这正是不落盘、走实时通道的理由）。
                Ok(()) = subagent_rx.changed() => {
                    subagent_raw = *subagent_rx.borrow_and_update();
                    let totals = subagent_raw.saturating_sub(subagent_baseline);
                    let notification = translator.set_subagent_totals(totals);
                    if write_notification(&hub, &notification).await.is_err() {
                        // 写帧失败即放弃本轮：与其它写失败站点同款。
                        let _ = turn_tx
                            .send(finished_event(&thread_id, &turn_id, None, None))
                            .await;
                        if let Err(e) = store.clear_partial(&thread_id) {
                            eprintln!(
                                "[app-server] failed to clear turn checkpoint ({thread_id}): {e}"
                            );
                        }
                        return;
                    }
                }
                Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                    // 只接受针对当前 turn 的中断;忽略上一轮残留的信号。
                    if target == turn_id {
                        cancel_sent = true;
                        cancel_token.cancel();
                    }
                    // 继续消费 stream,直到 run loop 发 Cancelled 并结束。
                }
                Some(request) = interject_rx.recv() => {
                    // 只并入这条请求所指的 turn;上一轮的残留请求直接还回客户端,
                    // 否则它会落进错误的上下文。
                    let matches_turn = request.turn_id == turn_id;
                    let accepted = matches_turn
                        && inbox
                            .as_ref()
                            .is_some_and(|handle| {
                                handle
                                    .interject(request.text.clone(), Some(request.interjection_id.clone()))
                                    .is_ok()
                            });
                    if !accepted {
                        // 未能并入(非当前 turn / 无句柄 / inbox 满):把文本还回去,
                        // 让客户端能恢复输入而不是静默丢弃。
                        for n in translator.on_event(
                            yi_agent_core::AgentEvent::InterjectionsReturned {
                                items: vec![yi_agent_core::Interjection {
                                    seq: 0,
                                    text: request.text,
                                    tag: Some(request.interjection_id),
                                }],
                            },
                        ) {
                            let _ = write_notification(&hub, &n).await;
                        }
                    }
                }
                Some(command) = session_rx.recv() => {
                    // 本轮已有一个待执行命令时保留先到的那个,后到的直接拒绝,
                    // 避免两端各自 await 一个永远不会有回应的 reply。
                    if pending_session_command.is_some() {
                        match command {
                            SessionCommand::Clear { reply } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::Compact { reply } => {
                                let _ = reply.send(CompactOutcome::Failed(
                                    "另有一个会话命令待执行".into(),
                                ));
                            }
                            SessionCommand::SetModel { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::SetReasoningEffort { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::SetPreserveThinking { reply, .. } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                            SessionCommand::ReloadMcp { reply } => {
                                let _ = reply.send(Err("另有一个会话命令待执行".into()));
                            }
                        }
                    } else {
                        pending_session_command = Some(command);
                    }
                }
            }
        }

        // 先执行本轮暂存的会话命令（此刻 turn 已结束，改 agent 是安全的），
        // 再为刚结束的 turn 收尾。
        if let Some(command) = pending_session_command.take() {
            let clearing = matches!(&command, SessionCommand::Clear { .. });
            agent = apply_session_command(
                agent,
                command,
                &provider,
                &config,
                &store,
                &thread_id,
                &hub,
                &turn_tx,
                &status,
                mcp.as_ref(),
                &mcp_workdir,
            )
            .await;
            if clearing {
                for n in translator.reset_totals() {
                    let _ = write_notification(&hub, &n).await;
                }
            }
            // 命令路径已 append 过最终状态;这里只需为这个 turn 发 Finished 让主循环清
            // active_turn_id,且**不要**再 append 一次(否则会用 turn 前的 session 覆盖)。
            let _ = update_status(&hub, &status, &thread_id, ThreadStatus::Idle).await;
            // 本轮的助手文本仍待回填给可能的等待方:命令路径结束的也是一个正常跑完的
            // turn,与下面的常规收尾同一口径(`completed_items` 此刻尚未被 take)。
            // 身份同样随事件上报:若本轮是替某条阻塞式 `ask_lead` 跑的,等待方收的
            // 仍是**这一轮**的答案(命令不改变谁在等哪一轮)。
            let _ = turn_tx
                .send(finished_event(
                    &thread_id,
                    &turn_id,
                    lead_reply_text(&completed_items),
                    lead_request_id,
                ))
                .await;
            // 这条路径不走常规收尾；本轮 checkpoint 同样必须清掉，否则会在盘上
            // 留一条已结束 turn 的残留，重启后被 resume 当成待恢复 turn。
            if let Err(e) = store.clear_partial(&thread_id) {
                eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
            }
            continue;
        }

        // 无暂存命令：常规收尾（把本轮结果落盘）。
        // 与已广播的 `turn/completed.completed_at` 同源；若本轮没有走终结事件
        // （流意外结束时 `translator` 没来得及定时刻），在这里补一个，以免这次
        // 回合结束在未读口径里凭空消失。
        let turn_completed_at = match translator.completed_at() {
            0 => crate::thread_store::now_millis(),
            at => at,
        };
        persist_and_finish_turn(
            &thread_id,
            Some(&turn_id),
            turn_completed_at,
            Some(&user_prompt),
            &user_attachments,
            &user_image_refs,
            lead_request_id,
            &agent,
            std::mem::take(&mut completed_items),
            last_usage.take(),
            turn_usage_total.take(),
            &store,
            &hub,
            &turn_tx,
            &status,
        )
        .await;

        // 收尾已把整轮 append 进主 jsonl；checkpoint 的使命结束。
        if let Err(e) = store.clear_partial(&thread_id) {
            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
        }
    }
}

/// 通用会话驱动的一轮:对每个「需要常驻 daemon」的项目、每个声明了
/// `session_driver` 的插件,拉一拍(`driver_step`)。
///
/// **内联在主循环里**(3s interval tick),不与任何 RPC 分支并发——`threads`
/// 等仍是主循环的局部量,`&mut` 借用按顺序发生(见设计约束:不让驱动另起任务
/// 并发访问 `threads`)。
///
/// 发现来源全是**通用**事实:项目来自 `yi_agent_store::resident::list`(哪些项目
/// 需要常驻 daemon),插件来自 `driver_plugins`(清单里 `session_driver: true`)。
/// 宿主不认识看板、卡片或合并——它只转发 `driver/next` 并执行插件给出的通用动作。
///
/// 会话空闲快照(`idle`)在**循环内**按 tick 采集:一轮 tick 里若刚起了一个会话,
/// 下一拍插件才看得到它的状态;且闭包持有的是 owned `HashMap`,不与 launcher
/// 需要的 `&mut threads` 冲突。
///
/// 两处失败**只记日志**,绝不中断本拍、也不影响 RPC:项目在常驻登记表里却没有任何
/// 驱动插件(宿主这一拍**根本没问**),以及某个插件这一拍问不通(问了但没打通)。
/// 二者必须分得开,否则「我没人可问」在日志里与「我问了、它失败了」一模一样。
/// 与主循环其它失败点同款:一个插件不可用不得影响其它插件。
#[allow(clippy::too_many_arguments)]
async fn driver_tick<F>(
    resident_dir: &Path,
    drivers: &mut HashMap<(PathBuf, String), DriverState>,
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    uploads: &mut crate::image_upload::UploadRegistry,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    models_path: &Path,
    cfg: &RuntimeConfig,
    hub: &Arc<crate::broadcast::Broadcaster>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    workspaces: &WorkspaceIndex,
    build_agent: &F,
    lead_inbox: &crate::lead::LeadInbox,
    lead_replies: &crate::lead::LeadReplies,
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
    // 组长开关：被驱动的会话与手工会话必须是同一个判定，否则「关掉开关」
    // 只对用户手工开的会话生效，而看板/驱动会话仍会注册组长工具。
    lead_agent_enabled: bool,
) where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    for project in yi_agent_store::resident::list(resident_dir) {
        // 「没人可问」必须与「问了、失败了」在日志里分得开:项目已在常驻登记表里,
        // 却拿不出任何声明 `session_driver` 的插件(清单目录缺失,或里面每份清单都
        // 损坏被 `load_manifests` 丢掉)。若只在这里静默 `continue`,一个健康的 daemon
        // 就会永远不被问起而无一行日志——那正是「注册过了却什么都没发生」的现场。
        let plugins = crate::session_driver::driver_plugins(&project);
        if plugins.is_empty() {
            eprintln!(
                "[app-server] driver tick: {} is registered as resident but has no \
                 session_driver plugin, so nothing was asked",
                project.display()
            );
        }
        for plugin in plugins {
            // 先快照 idle(owned `HashMap`):闭包因此不再借用 `threads`,与下面
            // launcher 的 `&mut threads` 不冲突。
            let snapshot: HashMap<String, bool> = threads
                .iter()
                .map(|(thread_id, session)| {
                    let idle = matches!(
                        *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                        ThreadStatus::Idle
                    ) && session.active_turn_id.is_none();
                    (thread_id.clone(), idle)
                })
                .collect();
            let idle = move |thread_id: &str| snapshot.get(thread_id).copied().unwrap_or(false);
            // 全机运行会话数:必须在构造 launcher **之前**算——launcher 持有
            // `&mut threads`,之后再借 `threads` 就是借用冲突。口径与上面的 idle
            // 快照同源(同一份 `ThreadSession`),所有归属的会话都在内:手动开的、
            // 别的插件起的、看板起的。插件靠它算「全机余量」,限的是总流量。
            let sessions_running = crate::session_driver::running_sessions(threads);
            // 该 (项目, 插件) 的驱动记账跨 tick 存活:`results` 要在下一拍回报,
            // `threads` 要让插件看见自己起过的会话。
            let state = drivers
                .entry((project.clone(), plugin.clone()))
                .or_default();
            // 活着与否只能由宿主回答:`threads` 表的成员即活会话(会话结束就从表里
            // 摘掉)。每拍刷新一次,插件据此持有/释放并发名额、判 stale——它**不能**
            // 从 idle 推断(空闲的会话还活着),更不能从卡片状态推断。
            for driven in &mut state.threads {
                driven.alive = threads.contains_key(&driven.thread_id);
            }
            let mut launcher = ServeLauncher {
                threads,
                pending_activation,
                process_watches,
                uploads,
                runtimes,
                thread_roots,
                models_path,
                lead_agent_enabled,
                cfg,
                hub,
                turn_tx,
                pending,
                permission_timeout,
                perm_seq,
                theme,
                git_diff,
                workspaces,
                build_agent,
                lead_inbox,
                // 驱动器从不对组长投递,故回信登记表原样借入即可(签名要求)。
                lead_replies,
                // 写通道与结果登记表同理:驱动器不执行写指令,只为满足 launcher 签名。
                lead_control,
                lead_control_replies,
                // 驱动器从不对组长投递:`LeadTarget` 的解析会**落盘**一个组长
                // meta(见 `resolve_lead_target`),而驱动器每 3s 就要为每个
                // (项目, 插件) 构造一次 launcher——在这里解析会让每次 tick 都
                // 替所有项目写一遍 meta,并把「谁拉起组长」变成驱动器。`None`
                // 是诚实取值:该 launcher 只负责起被驱动的会话。
                lead_target: None,
            };
            // 查询闭包走**通用**插件通道(`plugin_query_self`):宿主只转发
            // `method`/`params`,不解释应答,也不受任何看板登记表门控。
            let scope = project.clone();
            let log_project = project.clone();
            let log_plugin = plugin.clone();
            // 本拍插件的**原样应答**。`driver_step` 把「解析不出动作」与「插件说
            // 无事可做」都压成 `None`,故要分辨第三种无声失败(问通了、字面能读、
            // 但对不上契约)只能由调用方自己留一份原答——见下面 `None` 分支的日志。
            // 闭包按引用捕获它(`driver_step` 只借 `&dyn Fn`,无 `'static` 要求),
            // 故本拍结束即可读回,不必用线程局部量或全局状态。
            let last_answer: StdMutex<Option<serde_json::Value>> = StdMutex::new(None);
            let query = |params: &serde_json::Value| {
                match plugin_query_self(&scope, &plugin, "driver/next", params.clone()) {
                    Ok(answer) => {
                        *last_answer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(answer.clone());
                        Ok(answer)
                    }
                    Err(error) => {
                        // `driver_step` 把查询失败整体吞成「这一拍不推进」(`Err(_) =>
                        // return None`),它的签名与语义是插件契约的一部分,这里不改。
                        // 但失败必须留痕:否则「问了、没打通」与「问了、插件说无事可做」
                        // 在日志里完全同型,坏清单/daemon 掉线就成了无声停摆。
                        eprintln!(
                            "[app-server] driver tick: could not reach {} for {}: {}",
                            log_plugin,
                            log_project.display(),
                            error.message
                        );
                        Err(error.message)
                    }
                }
            };
            let outcome = crate::session_driver::driver_step(
                &query,
                &mut state.threads,
                &mut state.results,
                &mut launcher,
                &idle,
                sessions_running,
            )
            .await;
            // 本拍的结果**不在此处收割**:插件靠 `results` 才知道动作成没成、
            // 起了哪个 thread(spec §4.3),而 `driver_step` 只把它交回调用方。
            // 存进跨 tick 的记账里,下一拍才回报——早一拍回报等于插件还没被告知
            // 结果就先被问下一步。
            if let Some(result) = outcome {
                state.results.push(result);
            } else if state.results.is_empty() {
                // 第三种无声失败:查询**成功了**,但宿主解析不出动作(字段漂移、
                // 插件比宿主新、应答根本不是约定形状)。它在 `driver_step` 里与
                // 「插件说无事可做」同型(都回 `None`),不在这里分辨的话,断链就
                // 每 3s 无声重复一次。前两种失败(没人可问 / 问不通)已各有日志,
                // 这里补第三处。
                //
                // 两个前提使它不误报:本拍 `results` 为空——非空就说明至少交出了
                // 一个动作,或上一拍的动作还在等回报;且只认契约形状
                // `{"action": null}` 为「正常空闲」(见 driver-protocol.json 的
                // `answers.nothing`),那是唯一能合法回 `None` 的稀有情形。
                // 剩下仍回 `None` 的,只能是对不上契约的应答。
                let answer = last_answer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if let Some(answer) = answer {
                    if !crate::session_driver::is_explicit_no_action(&answer) {
                        let keys = match answer.as_object() {
                            Some(map) => map.keys().cloned().collect::<Vec<_>>().join(", "),
                            None => "<not a JSON object>".to_string(),
                        };
                        eprintln!(
                            "[app-server] driver tick: {log_plugin} for {} answered driver/next \
                             with a shape the host cannot parse (top-level keys: {keys}), so this \
                             tick did nothing",
                            log_project.display()
                        );
                    }
                }
            }
        }
    }
}

/// 驱动器起会话的真实实现:复用无帧内核(`start_thread_core` +
/// `prepare_turn_core`),**不**调用会向客户端发帧的 `start_turn_core`。
///
/// 它借用主循环的局部量(`threads` / `pending_activation` / `process_watches`)
/// 直接登记会话,故驱动器无需另起一个并发访问它们的任务。
///
/// `build_agent` 是 `serve` 的工厂参数(按引用借入,不要求 `'static`,也不要求
/// launcher 自己拥有它)——与 `thread/start` 分支用同一个工厂,因此被驱动的会话与
/// 手工会话的 agent 构造完全同源。
struct ServeLauncher<'a, F> {
    threads: &'a mut HashMap<String, ThreadSession>,
    pending_activation: &'a mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &'a mut HashMap<String, ProcessWatch>,
    /// 上传登记表：被驱动的会话不起于 RPC，但 `prepare_turn_core` 的签名要求它
    /// （`{type:"uploaded_image"}` 只可能来自 RPC；`driver/next` 的动作不带上传句柄）。
    uploads: &'a mut crate::image_upload::UploadRegistry,
    runtimes: &'a ProjectRuntimes,
    thread_roots: &'a ThreadRoots,
    /// 机器级模型清单路径:被驱动的会话的委派链路与手工会话读同一份,而非各自
    /// 重读 `$HOME`。
    models_path: &'a Path,
    cfg: &'a RuntimeConfig,
    hub: &'a Arc<crate::broadcast::Broadcaster>,
    turn_tx: &'a mpsc::Sender<TurnEvent>,
    pending: &'a Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &'a Arc<AtomicU64>,
    theme: &'a crate::theme_tool::ThemeHandle,
    git_diff: &'a crate::git_diff_tool::GitDiffHandle,
    workspaces: &'a WorkspaceIndex,
    build_agent: &'a F,
    /// 组长信箱:普通会话经 `ask_lead` 投递的消息从这里被消费。与主循环共享
    /// 同一个实例(见 `serve` 的注入),自造一份会投进没人听的通道。
    lead_inbox: &'a crate::lead::LeadInbox,
    /// 阻塞式 `ask_lead` 的回信登记表:与 `lead_inbox` 同一实例、同一理由。
    /// 它经 `start_thread_core` 一路进 `AskLeadTool::with_replies`。
    lead_replies: &'a crate::lead::LeadReplies,
    /// 该 launcher 对应项目的组长 thread id 与 cwd,由消费方在调用前解析。
    /// 用 owned 值而非从 `threads` 反查:组长可能尚未被打开,不在 `threads` 里。
    /// `None` = 该 launcher 不承载组长投递(目前只有 `driver_tick` 构造的
    /// launcher,见其构造点)。
    lead_target: Option<LeadTarget>,
    /// 组长写通道：工具侧把「取消 / 打回」投进这里，主循环消费它去动 `threads`。
    /// 与 `lead_inbox` 相对（那个是 thread→lead，这个是 lead→thread），同一实例、
    /// 同一注入理由——自造一份就投进没人听的通道。
    ///
    /// 它经 `start_thread_core` / `attach_delegation` 一路进 `build_runtime_tooling`，
    /// 在装配期注册两个写工具（与上面 `lead_replies` 同一条通路）。
    lead_control: &'a crate::lead_control::LeadControlInbox,
    /// 写结果登记表：与 `lead_control` 成对转交（工具侧登记 `request_id`，主循环
    /// 执行完回填）。只传通道而不传登记表，组长那侧将永远等不到结果。
    lead_control_replies: &'a crate::lead_control::LeadControlReplies,
    /// 组长开关的生效值。交给 `start_thread_core`，使被驱动的会话与手工会话
    /// 用同一个判定。
    lead_agent_enabled: bool,
}

/// 一次组长投递的目标:该项目当前应承载消息的组长 thread。
///
/// `Clone`(而非借用 `self.lead_target`)是刻意的:`deliver_to_lead` 随后要把
/// `&mut self.threads` 交给内核,任何未结束的 `&self.lead_target` 借用都会与它
/// 冲突。三个 `String` 的克隆相对一次 agent 装配可以忽略。
#[derive(Clone)]
struct LeadTarget {
    cwd: String,
    thread_id: String,
}

/// 通用动作的执行者:插件说「起一个会话」或「往已有会话追加一轮」,这里照做。
///
/// 这就是宿主与插件的分界:**动作由插件给出**(`DriverAction`),宿主不认识卡片、
/// 不认识队列;`Launch` 建会话、`Append` 复用会话,两者共用同一段「准备 + 通知 +
/// 投递」(`prepare_and_deliver`),差别只在 thread 是否已存在。
///
/// **一个 RPC 响应帧都不发**:动作不是客户端请求,没有可应答的 id。可发的只有
/// thread-keyed 通知(`turn/started`、开启项、`thread/status/updated` 的 Running)。
impl<F> SessionLauncher for ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String> {
        match &action.kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
                driver_project,
                driver_ref,
                env,
            } => self
                // 归属由插件在动作里给出（`driver_project` 项目根 / `driver_ref` 驱动
                // 方句柄），宿主原样落进 meta。缺字段（旧插件）时为 `None`，退化成
                // 「未分组」，与改动前一致——绝不让缺字段把整条动作判废。
                .launch_inner(
                    workdir,
                    title,
                    objective,
                    driver_project.as_deref(),
                    driver_ref.as_deref(),
                    env.clone(),
                )
                .await
                .inspect_err(|error| {
                    // 插件只把失败记成 `{ok:false, error}`,自己不留日志;不在这里说清
                    // 原因,「这个会话为什么没起来」就没有任何线索。
                    eprintln!("[app-server] could not launch a session in {workdir}: {error}");
                }),
            DriverActionKind::Append {
                thread_id,
                objective,
            } => {
                // 会话必须已经在 `threads` 里:append 的语义就是「续跑一个**本进程
                // 看得见**的会话」,冷会话无从续起(唤醒它要读盘恢复历史,那是另一
                // 个动作)。故先探一次,把「未知 thread」在**投递之前**判掉——否则
                // `prepare_turn_core` 会回一个「turn 校验失败」,掩盖真正的病因。
                if !self.threads.contains_key(thread_id) {
                    return Err(anyhow::anyhow!("unknown thread: {thread_id}"));
                }
                let thread_id = thread_id.clone();
                self.prepare_and_deliver(&thread_id, objective)
                    .await
                    .inspect_err(|error| {
                        eprintln!("[app-server] could not append a turn to {thread_id}: {error}");
                    })
            }
        }
    }
}

impl<F> ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    /// 起一个会话并投递它的首轮。
    ///
    /// 会话与 RPC 起的 thread 完全同型:同一内核、同一 driver、同一侧栏视角。唯一的
    /// 不同是 mode(驱动会话跑 Yolo)与「不发任何 RPC 帧」。
    ///
    /// `driver_project` / `driver_ref` 是落进 meta 的两个通用归属值,由插件在
    /// `driver/next` 的 `launch` 动作里给出(spec §5.5:唯一同时知道项目根与驱动
    /// 方句柄的是插件)。宿主只做透传:`driver_project` 让 `thread/listAll` 把会话折
    /// 进项目组,`driver_ref` 让桌面侧栏把它归入「驱动会话」小节。旧插件不发时为
    /// `None`,退化成未分组(与改动前逐字一致)。
    async fn launch_inner(
        &mut self,
        workdir: &str,
        title: &str,
        objective: &str,
        driver_project: Option<&str>,
        driver_ref: Option<&str>,
        // 插件要注入本会话 shell 的环境变量。宿主不解释任何一个键：它们的含义
        // 属于插件（「这张卡是哪张」不是宿主的通用事实）。
        env: Vec<(String, String)>,
    ) -> anyhow::Result<String> {
        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());
        let built = (self.build_agent)(
            None,
            Path::new(workdir),
            crate::thread_store::ThreadMode::Yolo,
        )?;
        // 生效串取自实际建出的 agent,与 `thread/start`/`thread/resume` 同口径,避免
        // 工厂快照与另读磁盘不一致而上报一个未真正采用的模型。
        let model = built.agent.config().model.clone();

        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.models_path,
            self.cfg,
            workdir,
            crate::thread_store::ThreadMode::Yolo,
            thread_id.clone(),
            // 看板会话起在卡片 worktree 里，那不是用户登记的「主根」，没有关联根
            // 可言；空集 = 单根会话（与改动前逐字一致，spec §7「看板项目列表行为
            // 不变」）。
            Vec::new(),
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.git_diff,
            self.workspaces,
            // 驱动会话不带 model_ref;生效串取自上面实际建出的 agent,与
            // `thread/start`/`thread/resume` 同口径。
            model,
            None,
            // 通用化的两个 meta 字段:项目根,以及「驱动方给的不透明句柄」。
            driver_project,
            driver_ref,
            self.lead_inbox,
            self.lead_replies,
            self.lead_control,
            self.lead_control_replies,
            Some(self.lead_agent_enabled),
            env,
        )
        .await?;

        // 侧栏显示的名片:插件给的 `看板 · <spec stem>`。不设的话,这个会话在
        // 首轮落盘前标题为空、用户只看到一串 thread id——「可见会话」就不成立了。
        // 失败只记日志:标题是展示层信息,不该让一个会话起不来。
        if !title.is_empty() {
            if let Some(session) = self.threads.get(&thread_id) {
                if let Err(error) = session.store.rename(&thread_id, title) {
                    eprintln!("[app-server] could not title driven thread {thread_id}: {error}");
                }
            }
        }

        self.prepare_and_deliver(&thread_id, objective).await
    }

    /// 执行一条组长写指令。
    ///
    /// 只有主循环能调：它持有 `&mut self.threads`。工具侧永远够不到这条路径
    /// （它们只往 `lead_control` 投递），这就是「守住 §6.1 独占」的落点。
    pub(crate) async fn apply_lead_control(
        &mut self,
        cmd: crate::lead_control::LeadControlCmd,
    ) -> Result<String, String> {
        use crate::lead_control::{ControlTarget, LeadControlKind, classify_target};

        // 目标 thread 所属的项目键：从它的 store 现取（每个 thread 有自己的
        // `cwd` → store，`threads` 里可能有多个项目的 thread）。取不到 = 目标不在场。
        let target_project_key = self
            .threads
            .get(&cmd.thread_id)
            .map(|s| s.store.project_key());
        let target = classify_target(
            &cmd.project_key,
            target_project_key.as_deref().unwrap_or(""),
            target_project_key.is_some(),
        );
        if target == ControlTarget::NotOpen {
            return Err(format!(
                "目标会话 {} 当前未打开（或不属于本项目），无法下达指令",
                cmd.thread_id
            ));
        }

        match cmd.kind {
            LeadControlKind::Interrupt => {
                let Some(session) = self.threads.get(&cmd.thread_id) else {
                    return Err("目标会话不在场".to_string());
                };
                let Some(turn_id) = session.active_turn_id.clone() else {
                    // 无活跃轮：报错，不静默 no-op（spec §3.2）。
                    return Err("目标当前没有活跃轮，无法打断".to_string());
                };
                // 与 `turn/interrupt` 逐字同源：幂等 level 信号 + try_send 不阻塞。
                let _ = session.interrupt_tx.try_send(turn_id);
                Ok(format!("已打断 {} 的当前轮", cmd.thread_id))
            }
            LeadControlKind::Reject => {
                let text = format!("[组长打回] {}", cmd.reason);
                // 有活跃轮 → 走 interjection（与用户中途打字同一路径）；
                // 无活跃轮 → 起一轮（spec §3.1）。
                if let Some(session) = self.threads.get(&cmd.thread_id) {
                    if let Some(turn_id) = session.active_turn_id.clone() {
                        let interjection_id =
                            format!("interject-{}-{}", turn_id, uuid::Uuid::new_v4());
                        if session
                            .interject_tx
                            .send(crate::session::InterjectionRequest {
                                turn_id,
                                interjection_id,
                                text,
                            })
                            .await
                            .is_err()
                        {
                            return Err("目标 driver 已退出".to_string());
                        }
                        return Ok(format!("已把打回指令并入 {} 的当前轮", cmd.thread_id));
                    }
                }
                // 无活跃轮（或目标刚转为 idle）：起一轮，走 prepare_and_deliver。
                self.prepare_and_deliver(&cmd.thread_id, &text)
                    .await
                    .map(|tid| format!("已向 {tid} 投递打回指令（起了新的一轮）"))
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// 准备并投递一轮,**不发任何 RPC 响应帧**。
    ///
    /// 首轮(新建会话)与追问轮(已有会话)共用:两者只差 thread 是否已存在——
    /// 调用方负责保证它存在(首轮刚由 `start_thread_core` 登记,追问轮由调用方先探)。
    async fn prepare_and_deliver(
        &mut self,
        thread_id: &str,
        objective: &str,
    ) -> anyhow::Result<String> {
        let hub = Arc::clone(self.hub);
        let params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": objective }],
        });
        let prepared =
            match prepare_turn_core(self.threads, self.pending_activation, self.uploads, &params)
                .await
            {
                Ok(prepared) => prepared,
                Err(error) => {
                    // **绝不在这里碰 `active_turn_id`**(曾有一版在此清占位,是错的)。
                    //
                    // `prepare_turn_core` 只在**最后一步**(复制附件成功之后)才写占位
                    // (`server.rs` 的「占位(复制成功之后)」),而它所有失败返回
                    // (`UnknownThread` / `TurnInProgress` / `InvalidAttachment` /
                    // `InvalidImage` …)都在那之前,之后没有任何可能失败的步骤。所以
                    // 走到这里意味着**本次调用从未设定过占位**:该 thread 上的
                    // `active_turn_id` 只可能属于**另一个正在跑的 turn**——追问轮撞上
                    // 忙会话的 TOCTOU(spec §10)。
                    //
                    // 清零它就是把那个活 turn 变成无主:`turn/cancel` 静默失效、
                    // `interrupt_and_wait_for_persist` 立即返回(`thread/delete` 于是可能
                    // 拆掉一个还在跑的会话)、`is_idle` 谎报空闲(下一拍驱动会收到假的
                    // 「这轮已结束」)。spec §10 对本行的处置只有一条:干净地回报
                    // `ok:false`,对既有会话零副作用。
                    //
                    // 把具体原因带上:调用方只把它写进 `{ok:false, error}`,
                    // 「准备失败」四个字不足以定位(是 thread 不在?还是 turn 撞车?)。
                    return Err(anyhow::anyhow!(
                        "could not prepare a turn for {thread_id}: {}",
                        turn_prepare_rpc_error(error).message
                    ));
                }
            };
        // 照 spec §4.4 的可见性要求:两个动作(首轮与追问轮)都发 `turn/started`,
        // 且与 `turn/start` 的 RPC 路径同序——它是该轮的第一帧。这是 thread-keyed
        // 的**内容通知**,不是 RPC 响应帧(不引入任何 `write_response`):桌面
        // `desktop/src/lib/session.ts` 用它断开 reasoning run 并置 `turnActive`,
        // 少了它,被驱动会话在侧栏里「运行中」的标志就只剩余额更弱的
        // `thread/status/updated`。
        write_notification(
            &hub,
            &Notification::TurnStarted {
                thread_id: prepared.thread_id.clone(),
                turn_id: prepared.turn_id.clone(),
            },
        )
        .await?;
        // 状态走 `thread/status/updated`(通知),不是向某个 client 的响应,故可发:
        // 侧栏据此看到该会话正在跑。用 prepared 的句柄,保证与 `active_turn_id`
        // 的占位来自同一个 session。
        let _ = update_status(
            &hub,
            &prepared.status_handle,
            &prepared.thread_id,
            ThreadStatus::Running,
        )
        .await;
        // 照 $18 发本轮的开启项(用户提问=插件给的 objective),再投递 prompt。
        // 这不是「向客户端发 RPC 帧」——它是 thread-keyed 的 content 通知,与
        // RPC 客户端收到的同一条流;少了它,会话点开后 objective 会像修复前
        // 那样被迟到追到最底部(见 `opening_user_item`)。
        emit_item(
            &hub,
            &prepared.thread_id,
            opening_user_item(
                &prepared.turn_id,
                &prepared.display_text,
                prepared.attachments.clone(),
                prepared.image_refs.clone(),
            ),
        )
        .await?;
        if prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id,
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
                lead_request_id: None,
            })
            .await
            .is_err()
        {
            return Err(anyhow::anyhow!(
                "thread driver for {} is gone before its prompt",
                prepared.thread_id
            ));
        }
        Ok(prepared.thread_id)
    }

    /// 把一条投递给组长的消息转成组长 thread 的一次 turn。
    ///
    /// 与 `launch_inner` 的三点差异,每一点都是刻意的:
    /// - 复用**已存在的**组长 thread id(不新铸 uuid),因此重复投递落在同一个会话;
    /// - `mode` 用 Normal,而非被驱动会话所用的 Yolo 强制值(组长会话不是被驱动的会话);
    /// - 不设 `driver_project` / `driver_ref`,组长不是被驱动的会话。
    ///
    /// **不发任何 RPC 帧**:投递不是客户端请求,没有可应答的 id。唯一可发的是
    /// `thread/status/updated` 这类不带 id 的通知,这里也不发——投递方(某个普通
    /// 会话的 `ask_lead`)不需要它,而多发一帧会让「谁触发这轮」变得含糊。
    async fn deliver_to_lead(
        &mut self,
        envelope: &crate::lead::LeadEnvelope,
    ) -> anyhow::Result<String> {
        // 先取出目标(克隆),此后整段函数都不再借用 `self.lead_target`:
        // `start_thread_core` 要 `&mut self.threads`,未结束的 `&self.…` 借用
        // 会让它借不到。
        let target = self
            .lead_target
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this launcher has no lead target"))?;
        // `mut` 只为下方冷启动的历史恢复:「先建空会话、再就地填入历史」是
        // `apply_session` 的形式,它保住的正是 session 的 `Arc`(见那里的说明)。
        let mut built = (self.build_agent)(
            None,
            Path::new(&target.cwd),
            crate::thread_store::ThreadMode::Normal,
        )?;
        let model = built.agent.config().model.clone();

        // 组长已在本进程内打开时,直接往它已有的 driver 投递,不必重建
        // (重建会丢掉该会话已累积的上下文,并让正在跑的那轮被替换)。
        if let Some(session) = self.threads.get(&target.thread_id) {
            let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
            let text = format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text);
            // 与 `thread/resume` 同款:激活只在本 thread 首次起 driver 时做。
            let activate = self
                .pending_activation
                .get(&target.thread_id)
                .cloned()
                .flatten();
            let send = session
                .prompt_tx
                .send(TurnPrompt {
                    turn_id,
                    prompt: text.clone(),
                    activate,
                    attachments: Vec::new(),
                    image_blocks: Vec::new(),
                    image_refs: Vec::new(),
                    display_text: text,
                    // 唯一会带身份的构造点:本轮就是替这条阻塞投递跑的,收尾时
                    // 只回填这一个 request_id(见 `settle_pending_lead_replies`)。
                    lead_request_id: envelope.request_id.clone(),
                })
                .await;
            return match send {
                Ok(()) => Ok(target.thread_id),
                Err(_) => Err(anyhow::anyhow!("lead driver is gone")),
            };
        }

        // 冷启动:该组长的 driver 不在本进程(进程重启,或它的 driver 因
        // `prompt_rx` 关闭而收尾)。`build_agent` 此刻建出的是**空会话**,而磁盘上的
        // `.jsonl` 记着此前投递过的每一轮。不填回去,同一条 thread 就会有两种语义:
        // warm 路径有记忆、cold 路径失忆。故这里镜像 `thread/resume` 的恢复三件事:
        // 载入 → `replace_messages` → 恢复上次用量(使 auto-compact 在恢复后的首轮
        // 即生效)。
        //
        // 组长的 meta 刚由 `ensure_lead_meta` 建、尚无 `.jsonl` 时 `load` 回 `None`:
        // 维持现状(空会话),与一次全新 `thread/start` 完全一致。
        // 关联根是**会话属性**（建会话时快照，之后不追改），所以这里取该会话自己
        // meta 里的那份，而**不**现读索引——与 `thread/resume` 同源。组长由
        // `thread/lead` 懒创建（未经 `thread/start` 的快照路径），故其 meta 里通常
        // 是空集：这是如实反映"这个会话创建时没有关联根"，不是降级。
        let lead_extra_roots: Vec<PathBuf> =
            match crate::thread_store::ThreadStore::new(Path::new(&target.cwd))
                .load(&target.thread_id)
            {
                Ok(Some(loaded)) => {
                    let roots: Vec<PathBuf> = loaded
                        .meta
                        .extra_roots
                        .iter()
                        .map(PathBuf::from)
                        .filter(|p| p.is_dir())
                        .collect();
                    let mut restored = yi_agent_core::Session::new();
                    if let Some(usage) = &loaded.usage {
                        restored.set_last_input_tokens(Some(usage.input_tokens));
                    }
                    restored.replace_messages(loaded.messages);
                    // `apply_session` 而非 `with_session`:前者就地替换内容、保住 session
                    // 的 `Arc`,与 `thread/resume` 的重建路径走同一个助手,因此委派工具
                    // 持有的 `CallerContext` 不会在恢复后指向一个被丢弃的旧会话。
                    apply_session(&mut built.agent, Some(restored));
                    roots
                }
                Ok(None) => Vec::new(),
                // 按 `thread/resume` 的口径:读不出来就明确失败,绝不静默地起一个
                // 失忆的组长。(id 由 `ensure_lead_meta` 铸造,恒合法;`load` 的读错误
                // 内部已吞成空历史,故此处实际不可达。)
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "could not load the lead's history for {}: {e}",
                        target.thread_id
                    ));
                }
            };

        // 冷启动:走与 `launch_inner` 相同的无帧内核(登记会话 + spawn driver)。
        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.models_path,
            self.cfg,
            &target.cwd,
            crate::thread_store::ThreadMode::Normal,
            target.thread_id.clone(),
            lead_extra_roots,
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.git_diff,
            self.workspaces,
            model,
            None,
            None,
            None,
            self.lead_inbox,
            self.lead_replies,
            self.lead_control,
            self.lead_control_replies,
            Some(self.lead_agent_enabled),
            // 组长会话不是驱动会话，没有插件句柄可注入。
            Vec::new(),
        )
        .await?;

        // 无帧地占好首个 turn,再自行投递 prompt——与 `launch_inner` 同一顺序。
        let params = json!({
            "threadId": target.thread_id,
            "input": [{ "type": "text",
                        "text": format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text) }],
        });
        let prepared =
            prepare_turn_core(self.threads, self.pending_activation, self.uploads, &params)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "could not prepare the lead's first turn: {}",
                        turn_prepare_rpc_error(error).message
                    )
                })?;
        if prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id,
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
                lead_request_id: envelope.request_id.clone(),
            })
            .await
            .is_err()
        {
            return Err(anyhow::anyhow!(
                "lead driver is gone before its first prompt"
            ));
        }
        Ok(target.thread_id)
    }
}

/// 一轮后台 tick 对**某个项目**要做的一次投递:投给谁、投什么文本。
///
/// 用 owned 值而不是借用 `threads`:投递要拿 `&mut threads`(走
/// `ServeLauncher::deliver_to_lead`),任何未结束的借用都会与之冲突。这也是
/// 把「算」与「投」分成两步的全部理由。
struct LeadTickDelivery {
    project_key: String,
    lead: LeadTarget,
    text: String,
}

/// 算一轮时按项目聚起来的一组中间量。
struct LeadGroup {
    project_key: String,
    /// 该项目的组长;`None` = 还没有组长 → 该组整体跳过(**不创建**)。
    lead: Option<LeadTarget>,
    snapshots: Vec<ThreadSnapshot>,
}

/// 一个会话的机械快照。
///
/// 只取**进程内事实**:状态只活在 `ThreadSession.status` 里(磁盘 `ThreadMeta`
/// 没有 status 字段)。因此「本进程没打开过的 thread」天然不在 `threads` 里,也就
/// 不会被算成「卡住」——这正是想要的:后台循环不为看不见的会话花钱。
///
/// `title` / `last_user_text` / `updated_at` 刻意留空:本轮唯一的判据是 status
/// (`pending_asks` 恒 0,见 [`lead_tick_once`]),读盘只会把每 30s 一次兜底变成
/// N 次 IO,而 `render_digest` 也不用这三个字段。
fn snapshot_of(session: &ThreadSession) -> ThreadSnapshot {
    ThreadSnapshot {
        thread_id: session.thread_id.clone(),
        title: None,
        status: *session.status.lock().unwrap_or_else(|p| p.into_inner()),
        last_user_text: None,
        updated_at: 0,
    }
}

/// 由项目键(状态目录 `<项目根>/.yi-agent`)反推项目根,与 `resolve_lead_target`
/// 同一口径。
fn project_root_of(project_key: &str) -> Option<String> {
    Path::new(project_key)
        .parent()
        .map(|root| root.display().to_string())
}

/// 组长后台循环一轮的**纯计算**部分:算出哪些项目的组长该被唤醒、该注入什么文本。
///
/// 抽成独立函数是为了**可测**:测试只断言这个返回值,不必观测一次真实的 LLM 轮
/// (spec §6.1 要的是「后台循环住主循环」,不是「测一次唤醒就得烧 token」)。它
/// 不落盘、不发帧、不 spawn,主循环的 `select!` 分支只负责把结果逐条投递。
///
/// 三条刻意约束:
/// - **不懒创建组长**:判定「有没有组长」只用 `ThreadStore::find_lead()`(纯读)。
///   绝不能用 `resolve_lead_target` —— 它内部会 `ensure_lead_meta`,于是每 30s
///   的兜底都会替每个项目落一个组长 meta,把「谁拉起组长」变成后台循环。没有
///   组长就是没有:跳过。
/// - **`pending_asks` 传 0**:阻塞式 `ask_lead` 的投递已由主循环的 `lead_rx`
///   分支实时处理(那条路径本来就最及时)。这里再报一次「有 ask」只会把同一次
///   求助变成两轮,既贵又吵。
/// - **只认本进程已打开的会话**:分组键取自会话自己的 store,快照只含
///   `status`;磁盘上未被打开的 thread 看不到实时状态,按 Idle 处理(不唤醒)。
fn lead_tick_once(threads: &mut HashMap<String, ThreadSession>) -> Vec<LeadTickDelivery> {
    // 阶段一(只读):按项目分组,给每组定出组长与一组 owned 快照。
    // 先读完再算,`threads` 的借用在本阶段结束时就归还,不会与调用方的 `&mut`
    // 冲突(与看板 tick 的「先快照成 owned,再借 &mut」是同一手法)。
    let mut groups: Vec<LeadGroup> = Vec::new();
    for session in threads.values() {
        let project_key = session.store.project_key();
        let Some(root) = project_root_of(&project_key) else {
            // 项目键没有 parent(理论上不可达):无处找组长,跳过。
            continue;
        };
        let idx = match groups.iter().position(|g| g.project_key == project_key) {
            Some(idx) => idx,
            None => {
                let lead = match session.store.find_lead() {
                    Ok(Some(meta)) => Some(LeadTarget {
                        cwd: root,
                        thread_id: meta.thread_id,
                    }),
                    _ => None,
                };
                groups.push(LeadGroup {
                    project_key,
                    lead,
                    snapshots: Vec::new(),
                });
                groups.len() - 1
            }
        };
        // 组长不是「它自己的 thread」:把组长列进去会让每一轮都自我唤醒,
        // 那等于把低频兜底变成永久烧钱的自激循环。
        if groups[idx]
            .lead
            .as_ref()
            .is_some_and(|lead| lead.thread_id == session.thread_id)
        {
            continue;
        }
        groups[idx].snapshots.push(snapshot_of(session));
    }

    // 阶段二(纯算):每组判一次「要不要起一轮」,要就把注入文本渲出来。
    let mut deliveries = Vec::new();
    for group in groups {
        // 没有组长 → 整组跳过。这里**不创建**组长。
        let Some(lead) = group.lead else {
            continue;
        };
        let wakes = decide_wake(&group.snapshots, 0);
        if wakes.is_empty() {
            continue;
        }
        deliveries.push(LeadTickDelivery {
            project_key: group.project_key,
            lead,
            text: render_digest(&group.snapshots, &wakes),
        });
    }
    deliveries
}

/// 过滤掉与上次**已投递**文本相同的投递项,并更新闩锁。
///
/// 背景:`decide_wake` 对每个 `AwaitingApproval` 无条件产出 `ThreadStuck`,而
/// `LEAD_TICK = 30s`、`PERMISSION_TIMEOUT = 300s`——一次审批窗会让组长被同一条
/// 消息唤醒最多 10 次,每次都是带全部上下文的完整 LLM turn(纯噪声 + 真金白银)。
///
/// 去重键取**文本本身**(而非 thread_id/原因):不用理解状态机,天然覆盖所有
/// wake reason;态势仍每轮现算(`render_digest` 照常跑),只是不重复**投递**
/// 一模一样的文本。内容一变(状态变了 / 新 thread 卡住)即恢复投递。
///
/// 这里刻意**先写闩锁再返回**:投递失败的下一轮不该重投同一文本——失败也已经
/// 花了那一轮的成本,无限重试只会放大(与 `lead_rx` 分支那套 request_id 回滚
/// 不同,那条路径有请求方在等回信,这条是单向 digest)。
fn take_changed_digests(
    deliveries: Vec<LeadTickDelivery>,
    latch: &mut HashMap<String, String>,
) -> Vec<LeadTickDelivery> {
    let mut out = Vec::new();
    for delivery in deliveries {
        if latch.get(&delivery.project_key) == Some(&delivery.text) {
            continue;
        }
        latch.insert(delivery.project_key.clone(), delivery.text.clone());
        out.push(delivery);
    }
    out
}

/// 由项目键反推该项目当前应承载消息的组长。
///
/// 项目键本身就是 `<cwd>/.yi-agent` 的 canonical 路径(见 `ThreadStore::project_key`),
/// 故 cwd 可由它去掉末段得到。组长 meta 不存在时当场补建——用户可能直接让某个
/// 会话给组长留言,而从未打开过组长对话。
///
/// 只经 `store` 落盘 meta,不触碰 agent / runtime / 任何 `cfg` 字段,故**不需要**
/// `RuntimeConfig`:把它列在签名里只会是一个没人读的参数(brief 的签名多带了
/// `cfg`,经核实无消费方,已去掉)。
fn resolve_lead_target(project_key: &str) -> anyhow::Result<LeadTarget> {
    let state_dir = Path::new(project_key);
    let cwd = state_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project key has no parent: {project_key}"))?;
    let store = crate::thread_store::ThreadStore::new(cwd);
    let (meta, _created) = crate::lead::ensure_lead_meta(&store, cwd)?;
    Ok(LeadTarget {
        cwd: cwd.display().to_string(),
        thread_id: meta.thread_id,
    })
}

/// 起一个 thread 的内核:attach_delegation → 建 driver 通道 → 登记进 `threads`
/// → spawn 进程守望者与 driver task。
///
/// `built` 由调用方先经 `build_agent` 造好(工厂在 `serve`/驱动器里同步调用,不在
/// 本内核内),这样内核不必把 `&F` 跨 await 持有——否则会强加 `F: Sync` 并波及
/// `serve` 的 Send 边界。只负责「得到一个已登记、driver 已 spawn 的 thread_id」
/// 这件事;RPC 层的事(`resolve_thread_cwd` 的错误响应、`thread/started` 通知与
/// 最终响应)留在调用方,以便驱动器复用同一内核而不发 RPC 帧。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_thread_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    models_path: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    mode: crate::thread_store::ThreadMode,
    thread_id: String,
    // 本会话的关联根（主根之外的附属根），由调用方在 `with_session_extra_roots`
    // 的同一作用域内捕获后传入：既写进 meta（快照），也交给 `attach_delegation`
    // 供委派路径构造多根工具集。**不在此处读槽位**，否则调用点一旦漏设就会静默
    // 拿到空集（或残留值）。
    extra_roots: Vec<PathBuf>,
    built: BuiltAgent,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    workspaces: &WorkspaceIndex,
    // 该会话的生效模型串与覆盖名(由调用方按清单解析后传入),落进 ThreadMeta
    // 并写进 ThreadSession,与 `thread/start`/`thread/resume` 同一来源。
    model: String,
    model_ref: Option<String>,
    // 被驱动会话的所属项目根与驱动方给的不透明句柄(通用名;旧名见
    // `ThreadMeta` 的 `serde(alias)`)。两者都只为 worktree 判定与侧栏归组落盘。
    driver_project: Option<&str>,
    driver_ref: Option<&str>,
    // 进程的组长信箱:转交 `attach_delegation`,使本会话的 `ask_lead` 能投递。
    lead_inbox: &crate::lead::LeadInbox,
    // 进程的回信登记表:与信箱同路转交,使本会话的 `ask_lead blocking: true`
    // 真的能等到回信(`attach_delegation` 把它接进 `AskLeadTool::with_replies`)。
    lead_replies: &crate::lead::LeadReplies,
    // 组长写通道与结果登记表:与 `lead_inbox`/`lead_replies` 同路转交(经
    // `attach_delegation` 进 `build_runtime_tooling`),使本会话若判定为组长便能
    // 注册那两个写工具。必须是主循环共享的同一实例——自造一份只会投进没人听的
    // 通道。
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
    // 组长开关。由调用方从实例状态取好后传入（见 `lead_gate` 的模块文档：
    // 不在这里读全局量，那在并行测试里会互相污染）。
    lead_agent_enabled: Option<bool>,
    // 调用方额外注入给本会话 shell 的环境变量。被驱动的会话用它带插件给的卡片
    // 句柄；RPC 起的会话传空集（与改动前逐字一致）。
    extra_session_env: Vec<(String, String)>,
) -> anyhow::Result<()> {
    let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(cwd)));

    // 会话起点（默认 diff 基准）。**必须在 `attach_delegation` 之前算好并传进去**：
    // 该函数要把起点交给 `show_git_diff`，而 meta 要到本函数后半段才 `create`；
    // 若让 `attach_delegation` 自己读盘，新建路径永远读到 `None`，工具的默认基准
    // 就落不到「本次会话起点」，只剩未提交改动——与它自己的描述不符。
    // 取值口径不变：仍是「建 meta 时取 HEAD」（此后 HEAD 随会话推进）。
    let anchor_sha = crate::git_diff::session_anchor(std::path::Path::new(cwd));

    // 委派是可选能力:项目 runtime 起不来就保留原 agent,只记 trace。
    // 接线刻意放在这里(而非 `threads` 守卫之内),避免与其可变借用冲突。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(cwd));
    let activation = attach_delegation(
        runtimes,
        thread_roots,
        &runtime_dir,
        models_path,
        cfg,
        cwd,
        &thread_id,
        theme,
        git_diff,
        // 调用方捕获的同一根集：落进 meta 的与交给委派路径的必须是同一个值，
        // 否则「meta 说有一根、实际工具集里没有」这类不一致无从察觉。
        extra_roots.clone(),
        lead_inbox,
        lead_replies,
        lead_control,
        lead_control_replies,
        // 新建路径：起点就是刚刚取的 HEAD。
        anchor_sha.clone(),
        lead_agent_enabled,
        extra_session_env,
        built,
    );
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        catalog,
        yolo,
        process_manager,
        mcp,
        ..
    } = activation.built;
    pending_activation.insert(thread_id.clone(), activation.runtime);
    // MCP 配置固定在 thread cwd 下(不是 daemon 回传的 toplevel):与
    // `build_runtime_tooling` 里 `ThreadStore::new(&cfg.workdir)` 同一口径。
    let mcp_workdir = std::path::PathBuf::from(cwd);

    let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
    let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
    let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
    let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);

    let now = crate::thread_store::now_millis();
    // 会话起点已在上面算好并交给 `attach_delegation`（同一个值也落进 meta）：
    // 两处必须是同一个 sha，否则「meta 里的起点」与「工具用的起点」会分叉。
    let meta = crate::thread_store::ThreadMeta {
        thread_id: thread_id.clone(),
        cwd: cwd.to_string(),
        model: model.clone(),
        created_at: now,
        updated_at: now,
        title: None,
        permission_mode: mode,
        pin_seq: None,
        driver_project: driver_project.map(str::to_string),
        driver_ref: driver_ref.map(str::to_string),
        model_ref,
        reasoning_effort: None,
        tag: None,
        // 新会话默认开启跨轮保留(与 `ThreadMeta` 的缺省语义一致)。
        preserve_thinking: true,
        // 从索引快照下来的关联根：运行期不变（后续改索引不追改本会话）。
        extra_roots: extra_roots
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect(),
        role: None,
        content_epoch: 0,
        last_item_id: None,
        anchor_sha,
        // 新会话：从未被读过、也从未完成过回合 ⇒ 0/0，不亮蓝点。
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
    };
    // 组长 meta 可能已由 `ensure_lead_meta` 落盘:此时**不得覆盖**,否则会把它
    // 的 `role: "lead"` 抹掉(本函数的 meta 不带 role),懒创建出来的组长就
    // 成了一个普通会话。先探测再建,代价是一次 `meta.json` 读;`thread/start`
    // 每次都用新铸的 uuid,故该路径行为不变(仍然是建)。
    let already_persisted = thread_store.load(&thread_id).ok().flatten().is_some();
    if !already_persisted {
        if let Err(e) = thread_store.create(&meta) {
            // 持久化是尽力而为:写失败不阻断 thread 创建。
            eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
        }
    }
    // 记录到全局「最近目录」索引,供 thread/listAll 与侧栏复用。
    // 索引键统一为 canonical,与 workspace/remove 的规范化对齐;
    // thread 的 cwd / meta 仍保持 `resolve_thread_cwd` 的原样。
    let index_path = std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    if let Err(e) = workspaces.add(&index_path) {
        eprintln!(
            "[app-server] failed to record workspace {}: {e}",
            index_path.display()
        );
    }

    // 先建句柄:同一 `Arc` 既存进 session 供 `thread/list` 读,
    // 也交给 driver 供其推送 `thread/status/updated`。
    let store_status = ThreadSession::new_status();
    // driver 与 children watch 之间唯一的通道：watch 侧只送最新汇总，
    // driver 侧据此生成完整的 TokenUsage（它才拿得到 total_*）。
    let (subagent_tx, subagent_rx) = tokio::sync::watch::channel(SubagentTotals::default());
    // 新会话同样要成为 writer，同样要取锁。`thread/start` 刚生成的 id 理论上
    // 不会与别人撞，但锁的意义是「同一路径只有一个 writer」，不是「防撞 id」：
    // 落盘后用同一 id 另开一个进程 resume 时，这把锁就是唯一屏障。
    // 只有 `Busy` 才拒绝；`Io`（锁文件建不出来）退化为无锁，与加锁前行为一致。
    let writer_lock =
        match crate::thread_store::acquire_thread_lock(thread_store.root(), &thread_id) {
            Ok(lock) => Some(lock),
            Err(crate::thread_store::ThreadLockError::Busy(busy)) => {
                return Err(anyhow::anyhow!(busy.to_string()));
            }
            Err(e @ crate::thread_store::ThreadLockError::Io(_)) => {
                tracing::warn!(%e, thread_id = %thread_id, "会话写锁不可用，退化为无锁");
                None
            }
        };
    threads.insert(
        thread_id.clone(),
        ThreadSession {
            thread_id: thread_id.clone(),
            cwd: cwd.to_string(),
            model: model.clone(),
            active_turn_id: None,
            yolo,
            process_manager: Arc::clone(&process_manager),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::clone(&thread_store),
            status: Arc::clone(&store_status),
            subagent_tx,
            writer_lock,
        },
    );

    // 先停旧守望者、再订阅本次生效的 manager。`thread/start` 通常没有
    // 旧守望者(remove 得到 None,no-op),但 `thread/resume` 恢复一个
    // 仍在内存的活 thread 时会换掉生效的 manager:旧守望者若被
    // `contains_key` 守卫留下,就仍订阅那份已被丢弃的 manager,与
    // `process/list` 用的新 manager 脱节,`process/updated` 静默失联。
    if let Some(previous) = process_watches.remove(&thread_id) {
        previous.stop().await;
    }
    let rx = process_manager.subscribe();
    let handle = tokio::spawn(watch_processes(
        Arc::clone(hub),
        thread_id.clone(),
        Arc::clone(&process_manager),
        rx,
    ));
    process_watches.insert(thread_id.clone(), ProcessWatch { task: handle });

    // 每个 thread 一个 driver task:独占 agent 与两个 receiver,
    // 串行驱动 turn。
    let driver_hub = Arc::clone(hub);
    let driver_client = client.clone();
    let driver_turn_tx = turn_tx.clone();
    let driver_thread_id = thread_id.clone();
    tokio::spawn(run_thread_driver(
        driver_thread_id,
        agent,
        prompt_rx,
        interrupt_rx,
        interject_rx,
        session_rx,
        driver_hub,
        driver_client,
        driver_turn_tx,
        decision_tx,
        Arc::clone(pending),
        permission_timeout,
        Arc::clone(perm_seq),
        catalog,
        Arc::clone(&thread_store),
        Arc::clone(&store_status),
        provider,
        config,
        subagent_rx,
        mcp,
        mcp_workdir,
    ));

    Ok(())
}

/// `prepare_turn_core` 的失败。每一种都对应 `turn/start` 的一条固定错误响应,
/// 由 `start_turn_core` 负责把它写出去(这样「校验」与「写响应」解耦,驱动器可以
/// 只复用前者而不向任何客户端广播)。
enum TurnPrepareError {
    MissingThreadId,
    EmptyInput,
    UnknownThread(String),
    TurnInProgress(String),
    /// 附件不可读 / 超出上限 / 不是文件。
    InvalidAttachment(String),
    /// 显式路径的图片输入（`{type:"image", path}`）不可用。带 `data.code =
    /// "invalid_image"`，客户端据此指认是哪张图被拒（见 `invalid_image`）。
    InvalidImage(String),
}

/// 一个已经「占位」好的 turn,但**尚未向任何客户端发帧**。
///
/// 校验通过后 `active_turn_id` 已被占成 `turn_id`;`prompt_tx` 是投递通道,
/// `status_handle` 供调用方推 Running 状态。字段全部 `pub(crate)`,以便
/// `ServeLauncher` 复用这条无帧路径。
pub(crate) struct PreparedTurn {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) prompt_tx: mpsc::Sender<TurnPrompt>,
    /// 送给模型的 prompt（含附件清单注入）。
    pub(crate) prompt: String,
    /// 气泡正文:用户原话，**不含**注入的清单。
    pub(crate) display_text: String,
    /// 本轮附件元数据；回显在开启项上，并随 `TurnPrompt` 交给 driver 落盘。
    pub(crate) attachments: Vec<crate::protocol::Attachment>,
    /// 本轮图片内容块（含 base64），随 `TurnPrompt` 进模型上下文。
    pub(crate) image_blocks: Vec<yi_agent_core::ContentBlock>,
    /// 本轮图片元数据；回显在开启项上（`Item::UserMessage.images`）。
    pub(crate) image_refs: Vec<crate::protocol::ImageRef>,
    pub(crate) activate: Option<Arc<ThreadRoot>>,
    pub(crate) status_handle: Arc<std::sync::Mutex<ThreadStatus>>,
}

/// 起一个 turn 的**无帧准备段**:校验 thread 存在、`active_turn_id` 空闲、
/// 提取 prompt、占 `active_turn_id = Some(turn_id)`、构造 `TurnPrompt`。
///
/// **不写任何通知 / 响应**——正因如此驱动器才能「起会话而不向客户端发帧」
/// (驱动器不得复用带帧的 `start_turn_core`)。失败返回
/// `Err(TurnPrepareError)`,由调用方决定怎么写错误响应(或对驱动器而言记日志)。
async fn prepare_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    params: &serde_json::Value,
) -> Result<PreparedTurn, TurnPrepareError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or(TurnPrepareError::MissingThreadId)?
        .to_string();
    // 一次性拆出「文本 + 附件源路径 + 图片输入」:文本为空但有附件或图片时仍然
    // 放行(附件-only / 图片-only)。
    let (text, source_paths) = crate::attachments::parse_input(params);
    let image_inputs = crate::attachments::parse_image_inputs(params);
    let text_opt = if text.trim().is_empty() {
        None
    } else {
        Some(text)
    };
    if text_opt.is_none() && source_paths.is_empty() && image_inputs.is_empty() {
        return Err(TurnPrepareError::EmptyInput);
    }

    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    // 内层作用域:让 `&mut threads` 的借用先结束(下面复制附件还要再借一次)。
    let (prompt_tx, status_handle, cwd) = {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        (
            session.prompt_tx.clone(),
            Arc::clone(&session.status),
            session.cwd.clone(),
        )
    };

    // 复制附件进工作区。放在占 `active_turn_id` **之前**:失败要能干净地拒绝,
    // 不留一个「已占用但起不来」的 turn。
    let max_bytes = crate::attachments::resolve_max_bytes();
    let mut attachments = Vec::with_capacity(source_paths.len());
    for source in &source_paths {
        match crate::attachments::store_attachment(
            Path::new(&cwd),
            &thread_id,
            Path::new(source),
            max_bytes,
        ) {
            Ok(attachment) => attachments.push(attachment),
            Err(crate::attachments::AttachmentError::TooLarge { size, max }) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {source} is {size} bytes, over the {max} byte limit"
                )));
            }
            Err(crate::attachments::AttachmentError::NotAFile(path)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {} is not a regular file",
                    path.display()
                )));
            }
            Err(crate::attachments::AttachmentError::Io(e)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "could not read attachment {source}: {e}"
                )));
            }
        }
    }

    // 复制 + 编码图片。同样放在占 `active_turn_id` **之前**。
    //
    // 两种来源在这里**收敛**到同一条管线（裁定 R3）：
    // - `Path`（桌面）：文件选择器给的根外绝对路径，先 `store_attachment` 复制进
    //   工作区；
    // - `Upload`（iOS 分片上传）：`image/upload/*` 已经把 staging 收拢成同一份
    //   `store_attachment` 结果（含 `size` = 存盘文件字节长度），这里只反查登记表。
    //
    // 之后一律读**根内副本**再编码：`prepare_image_file` 在根外读原文件也可以，
    // 但那样回显的 `path` 与真正读到的字节可能来自两个位置；统一读根内副本，
    // `path` 与内容才必然同源，`ImageRef.size` 也才必然等于该文件的字节长度。
    //
    // 坏图的两条处置**刻意不同**，判据是「失败能不能靠重试自愈」：
    // - `Upload`：`uploadId` 未知 / 未 commit / 已过期（10 分钟 TTL）都是**不可
    //   自愈**的——服务端手里已没有重试所需的任何东西，客户端每重发一次都只会拿
    //   到一模一样的失败，而它还会把 chip 留着，于是整轮对话被这一张图钉死。故
    //   **丢弃这张图**（warn 一条）并继续起 turn：用户照样能把消息发出去。
    //   注：上传路径的图已由 `begin` 的 `MAX_IMAGE_BYTES` 设闸，故「超大」不会
    //   走到这里；但「上传的字节其实不是图」会——`commit` 只收拢字节，判图是这
    //   一步的事。
    // - `Path`：路径是这次请求里的显式参数，指不到/不是图是**调用方请求本身**的
    //   问题（桌面端的图在本地盘上，重发同一路径多半就是同一个结果），仍按
    //   `InvalidImage` 硬拒，与文档附件的 `InvalidAttachment` 同口径。
    let mut image_blocks = Vec::with_capacity(image_inputs.len());
    let mut image_refs = Vec::with_capacity(image_inputs.len());
    for input in &image_inputs {
        let (label, stored) = match input {
            crate::attachments::ImageInput::Path(source) => {
                let stored = match crate::attachments::store_attachment(
                    Path::new(&cwd),
                    &thread_id,
                    Path::new(source),
                    max_bytes,
                ) {
                    Ok(stored) => stored,
                    Err(e) => {
                        return Err(TurnPrepareError::InvalidImage(format!(
                            "image {source}: {e:?}"
                        )));
                    }
                };
                (source.clone(), stored)
            }
            crate::attachments::ImageInput::Upload(upload_id) => {
                // 未 commit / 未知 / 已过期都落到这里。
                //
                // 反查必须带 `thread_id`：`uploadId` 是客户端手里的不透明句柄，
                // 只看 id 不看归属，就能把别条 thread 的图摄进本 turn（跨 thread
                // 取数）。归属不符与「未知」一样丢弃，不向调用方区分两者。
                match uploads.resolve_for(upload_id, &thread_id) {
                    Some(attachment) => (upload_id.clone(), attachment),
                    None => {
                        tracing::warn!(
                            %thread_id,
                            upload_id = %upload_id,
                            "dropping unresolvable uploaded image; continuing the turn",
                        );
                        continue;
                    }
                }
            }
        };
        let stored_abs = Path::new(&cwd).join(&stored.path);
        match yi_agent_tools::image_prep::prepare_image_file(
            &stored_abs,
            yi_agent_core::ImageDetail::High,
            yi_agent_tools::image_prep::resolve_budget(),
        ) {
            Ok(yi_agent_tools::image_prep::PreparedImage::Ready {
                media_type, data, ..
            }) => {
                image_blocks.push(yi_agent_core::ContentBlock::Image {
                    source: yi_agent_core::ImageSource::Base64 {
                        media_type: media_type.clone(),
                        data,
                    },
                    detail: yi_agent_core::ImageDetail::High,
                    path: Some(stored.path.clone()),
                });
                image_refs.push(crate::protocol::ImageRef {
                    path: stored.path,
                    media_type,
                    size: stored.size,
                    detail: Some(yi_agent_core::ImageDetail::High.as_wire_str().to_string()),
                });
            }
            // 已落盘、但读不出/超预算的上传图同样丢：它和「未知 uploadId」一样属于
            // 重发必然复现的失败，不该毒死整轮。
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message })
                if matches!(input, crate::attachments::ImageInput::Upload(_)) =>
            {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    %message,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message }) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {message}"
                )));
            }
            Err(e) if matches!(input, crate::attachments::ImageInput::Upload(_)) => {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    error = ?e,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Err(e) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {e:?}"
                )));
            }
        }
    }

    // 占位(复制成功之后)。
    {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        session.active_turn_id = Some(turn_id.clone());
    }
    let activate = pending_activation.get(&thread_id).cloned().flatten();
    // prompt 送进模型时带上附件清单;气泡文本仍是原话(见 `display_text`)。
    let prompt = crate::attachments::prompt_with_attachments(
        text_opt.as_deref().unwrap_or(""),
        &attachments,
    );
    Ok(PreparedTurn {
        thread_id,
        turn_id,
        prompt_tx,
        prompt,
        display_text: text_opt.unwrap_or_default(),
        attachments,
        image_blocks,
        image_refs,
        activate,
        status_handle,
    })
}

/// 把 `prepare_turn_core` 的失败翻成 6a 建立的错误响应(逐字不变)。
fn turn_prepare_rpc_error(error: TurnPrepareError) -> RpcError {
    match error {
        TurnPrepareError::MissingThreadId => RpcError::invalid_params("missing threadId"),
        TurnPrepareError::EmptyInput => RpcError::invalid_params("missing or empty input text"),
        TurnPrepareError::UnknownThread(thread_id) => RpcError::unknown_thread(&thread_id),
        TurnPrepareError::TurnInProgress(thread_id) => RpcError::turn_in_progress(&thread_id),
        TurnPrepareError::InvalidAttachment(msg) => RpcError::invalid_params(msg),
        TurnPrepareError::InvalidImage(msg) => RpcError::invalid_image(msg),
    }
}

/// The item that opens a turn: the user's own message.
///
/// The baseline server emitted this **only at persist time**
/// (`persist_and_finish_turn`), so a client watching a turn live saw the agent's
/// items stream in first and the prompt appended last — a viewer opening the
/// thread later found the opening prompt at the very bottom of the transcript.
/// Emitting it as the turn's *first* item, from the one helper both the RPC path
/// (`start_turn_core`) and the board path (`ServeLauncher`) call, fixes the order
/// at the source and keeps the id identical to the persisted one (`user-<turn_id>`),
/// so the desktop merges the live bubble with the replayed one by id.
fn opening_user_item(
    turn_id: &str,
    text: &str,
    attachments: Vec<crate::protocol::Attachment>,
    images: Vec<crate::protocol::ImageRef>,
) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage {
        id: format!("user-{turn_id}"),
        text: text.to_string(),
        attachments,
        images,
    }
}

/// Emit one item as both `item/started` and `item/completed`.
///
/// Content-layer notifications carry no response frame, so both the RPC path and
/// the board launcher may (and must, to stay identical) call this. Failures are
/// propagated: a broken hub is a real fault, not something to swallow.
async fn emit_item(
    hub: &Arc<crate::broadcast::Broadcaster>,
    thread_id: &str,
    item: crate::protocol::Item,
) -> anyhow::Result<()> {
    write_notification(
        hub,
        &Notification::ItemStarted {
            thread_id: thread_id.to_string(),
            item: item.clone(),
        },
    )
    .await?;
    write_notification(
        hub,
        &Notification::ItemCompleted {
            thread_id: thread_id.to_string(),
            item,
        },
    )
    .await
}

/// 起一个 turn 的 RPC 包装:调 `prepare_turn_core` → 发 `turn/started` →
/// 推 Running 状态 → 写响应 → 投递 prompt。
///
/// 校验失败时在此就地写错误响应并返回 `Ok(None)`,调用方据此 continue——帧的
/// 顺序与错误响应语义与 6a 抽取前逐字一致(响应急在 prompt 投递之前)。
/// 成功返回 `Ok(Some(thread_id))`。
pub(crate) async fn start_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    let prepared = match prepare_turn_core(threads, pending_activation, uploads, params).await {
        Ok(prepared) => prepared,
        Err(error) => {
            write_response(hub, client, err_response(id, turn_prepare_rpc_error(error))).await?;
            return Ok(None);
        }
    };

    // 顺序确定:先 turn/started 通知,再发本轮开启项(用户提问),再推 Running
    // 状态,再响应,最后投递 prompt。开启项排在 agent 任何 item 之前,实时观看与
    // 事后回放的顺序才一致(见 `opening_user_item`)。
    write_notification(
        hub,
        &Notification::TurnStarted {
            thread_id: prepared.thread_id.clone(),
            turn_id: prepared.turn_id.clone(),
        },
    )
    .await?;
    emit_item(
        hub,
        &prepared.thread_id,
        opening_user_item(
            &prepared.turn_id,
            &prepared.display_text,
            prepared.attachments.clone(),
            prepared.image_refs.clone(),
        ),
    )
    .await?;
    update_status(
        hub,
        &prepared.status_handle,
        &prepared.thread_id,
        ThreadStatus::Running,
    )
    .await?;
    write_response(
        hub,
        client,
        ok_response(id, json!({ "turn_id": prepared.turn_id.clone() })),
    )
    .await?;

    if prepared
        .prompt_tx
        .send(TurnPrompt {
            turn_id: prepared.turn_id,
            prompt: prepared.prompt,
            activate: prepared.activate,
            attachments: prepared.attachments,
            image_blocks: prepared.image_blocks,
            image_refs: prepared.image_refs,
            display_text: prepared.display_text,
            lead_request_id: None,
        })
        .await
        .is_err()
    {
        // driver 已退出(理论上不会):清掉活跃标记,
        // 避免后续 turn 永远报 turn_in_progress。
        if let Some(s) = threads.get_mut(&prepared.thread_id) {
            s.active_turn_id = None;
        }
    }
    Ok(Some(prepared.thread_id))
}

/// 解析某主根当前关联的附属根：取索引里的记录，丢掉已不存在的目录。
///
/// 静默丢弃（而非报错）是有意的：附属根是能力增量，它消失不该让会话打不开；
/// `cwd` 本身已删仍照既有行为报错（`resolve_thread_cwd` / resume 的守卫），
/// 因为那是权威基点。
///
/// `WorkspaceIndex::children` 返回的已是 canonical 绝对路径（`link` 写入前
/// canonicalize 过），故此处无需再规范化。
///
/// 可见性:`thread/start` 的臂已搬进 [`crate::rpc::thread`],故与
/// [`resolve_thread_cwd`] 同样放宽到 `pub(crate)`(只放宽可见性,值/行为不变)。
pub(crate) fn linked_extra_roots(workspaces: &WorkspaceIndex, cwd: &str) -> Vec<PathBuf> {
    workspaces
        .children(Path::new(cwd))
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}

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
