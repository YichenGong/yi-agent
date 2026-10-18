//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::lead_loop::{ThreadSnapshot, decide_wake, render_digest};
use crate::pairing::PairingState;
use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION,
    RequestEnvelope, RequestId, ResponseEnvelope, ReverseRequest, RpcError, Scope, ThreadStatus,
};
use crate::session::{
    CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt,
};
use crate::session_driver::{DriverAction, DriverActionKind, DriverState, SessionLauncher};
use crate::translate::Translator;
use crate::transport::{MessageReader, MessageWriter};
use crate::workspace_index::WorkspaceIndex;
use yi_agent_subagent::binding::RuntimeBinding;
use yi_agent_subagent::thread_root::ThreadRoot;

/// 权限审批等待客户端响应的默认超时;超时按 Deny 处理。
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// 会话驱动在主循环里跑一轮的间隔。
///
/// 采用「serve 主循环内周期性 tick」而非让驱动器并发访问 `threads`:整张
/// `threads` 表是主循环的局部量,重构成可共享结构会把改动面扩散到全部 RPC 分支。
/// 3s 足够跟上插件给出的动作,又远短于一个会话的生命周期。
const DRIVER_TICK: Duration = Duration::from_secs(3);

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

/// 首轮 LLM 标题生成的超时:超时即回退兜底标题，绝不卡住 turn 收尾。
const TITLE_TIMEOUT: Duration = Duration::from_secs(5);

/// 标签显示名的长度上限。必须与前端 `desktop/src/lib/threadTags.ts` 的
/// `TAG_NAME_MAX_LEN` 一致——超出会撑破菜单。
///
/// `pub(crate)`:校验 arm 在 `ui/settings/write` 搬进 `rpc/ui.rs` 后仍要读它
/// (测试模块经 `use super::*` 也取同一份)。
pub(crate) const TAG_NAME_MAX_LEN: usize = 24;

/// `image/read` 每次回传的原始字节上限。传输帧上限 1 MiB,base64 膨胀 4/3 后
/// 512 KiB 的原始块约 683 KiB,连同其它字段仍在单帧之内。
pub(crate) const IMAGE_READ_CHUNK_BYTES: u64 = 512 * 1024;

/// `image/read` 允许服务的最大文件字节数。分片只是传输形态,不代表可以无上限地
/// 服务任意大文件;与 `image_prep::MAX_IMAGE_BYTES` 同值(20 MiB)。
pub(crate) const IMAGE_READ_MAX_BYTES: u64 = 20 * 1024 * 1024;

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
}

/// 一个 cwd 的工具集与其权限检查器。
struct RuntimeTooling {
    registry: Arc<yi_agent_core::ToolRegistry>,
    permission: Arc<yi_agent_core::permission::PermissionChecker>,
    /// 这一份注册表对应的 manager(与 registry 同源)。
    process_manager: Arc<yi_agent_tools::ProcessManager>,
    /// The live caller transcript shared with this registry's delegation tools.
    /// `build_runtime_tooling` hands a clone to the tools and keeps this one, so
    /// binding it here (see [`wrap_for_delegation`]) is visible to those tools.
    caller: yi_agent_subagent::CallerContext,
}

/// 每 cwd 的已 attach runtime。app-server 是长驻多 cwd 进程,而 runtime 是按项目
/// 划分的,因此 attach 以 canonical cwd 为键、懒初始化。
///
/// 失败也缓存(存 `Err(原因)`):否则纯目录下每开一个 thread 都会重跑一遍注定失败的
/// bring-up(git 检查 + daemon 起停尝试)。
pub(crate) type ProjectRuntimes =
    Arc<StdMutex<HashMap<PathBuf, Result<Arc<RuntimeBinding>, String>>>>;

/// 每会话自己的 root,建在项目共享 binding 之上。
///
/// runtime(daemon)按项目共享,是 G2;root 按会话独立,是这次改动的核心——
/// 一个 root 才是一份 `MAX_DIRECT_CHILDREN` 预算,两个会话共用一个 root 就会共用
/// 一份预算,这正是桌面端「四个就满」的成因。
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
    /// Where the board registry lives. Injectable for the same reason
    /// `WorkspaceIndex` is: the `board/*` RPCs must be testable without
    /// touching the developer's real `~/.yi-agent`.
    pub(crate) board_dir: PathBuf,
    /// Where `models.json` lives. Injectable for the same reason `board_dir`
    /// is: `model/*` must be testable without writing the developer's real
    /// machine-level catalog (and its API keys).
    pub(crate) models_path: PathBuf,
    /// Where the global `.env` lives, for `onboarding/apply`. Injectable for the
    /// same reason `models_path` is: onboarding must never write the developer's
    /// real `~/.yi-agent/.env`.
    pub(crate) onboarding_env_path: PathBuf,
    /// Where `preferences.json` lives, for the `onboarding_dismissed` marker.
    /// Injectable for the same reason `onboarding_env_path` is: dismissing the
    /// first-run guide must never touch the developer's real preferences file.
    pub(crate) onboarding_preferences_path: PathBuf,
    /// Where the generic resident-daemon registry lives (`$HOME/.yi-agent`).
    /// Injectable for the same reason `board_dir` is: `board/create` must be
    /// testable without writing the developer's real resident registry.
    pub(crate) resident_dir: PathBuf,
    /// 拼 skill 根目录用的「家目录」。Injectable for the same reason `board_dir`
    /// is: `skill/list` / `skill/read` must be testable without reading the
    /// developer's real `~/.yi-agent/skills`.
    pub(crate) skills_home: PathBuf,
    /// The absolute path of the `superpowers-kanban` executable written into
    /// each project's supervisor manifest. Injectable for the same reason
    /// `board_dir` is: `board/create` must be testable without the plugin being
    /// installed on the machine.
    pub(crate) plugin_command: PathBuf,
    /// How `board/create` starts a project's daemon. Injectable for the same
    /// reason `board_dir` is: a test must not spawn a real detached process,
    /// and the app's production launcher is what an integration test would
    /// otherwise have to reproduce.
    pub(crate) launcher: BoardLauncher,
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
}

/// Starts a project's board daemon; see [`yi_agent_boards::lifecycle::create_with_project`].
pub(crate) type BoardLauncher = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;

/// Installs the resident watchman (macOS LaunchAgent) for `exe` under `home`;
/// see [`yi_agent_boards::watchman::install`].
pub(crate) type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;

/// Removes the resident watchman under `home`; see
/// [`yi_agent_boards::watchman::uninstall`].
pub(crate) type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// 生产环境的 `$HOME`。未设置时回退空路径——与 `board_dir` 同一策略：宁可把
/// 值守装进一个无人读取的目录，也不要因为环境缺失让整个服务起不来。
pub(crate) fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// 生产的安装闭包：设置 `YI_AGENT_DISABLE_WATCHMAN` 时退化成 no-op。
///
/// 这个开关是为那个**驱动真实二进制**的 e2e（`yi-agent/tests/board_e2e.rs`）而设：
/// 它以 `YI_AGENT_BOARD_E2E` 为闸、跑的是生产接线，于是 `board/create` 里的
/// 「首次创建时装值守」会走到这里。不拦的话它会在开发机真实的 launchd 域里执行
/// `launchctl bootstrap gui/<uid>`，并往真实的 `~/Library/LaunchAgents` 写 plist——
/// 测试绝不能碰宿主 launchd。为此 e2e 置该环境变量即可让安装/卸载变成空操作。
///
/// 只影响生产接线：测试用的 harness 各自注入记账闭包（见 `Harness`），不经过这里。
pub(crate) fn production_watchman_install() -> WatchmanInstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_exe: &Path, _home: &Path| Ok(()));
    }
    Arc::new(yi_agent_boards::watchman::install)
}

/// 生产的卸载闭包；`YI_AGENT_DISABLE_WATCHMAN` 开关同
/// [`production_watchman_install`]。
pub(crate) fn production_watchman_uninstall() -> WatchmanUninstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_home: &Path| Ok(()));
    }
    Arc::new(yi_agent_boards::watchman::uninstall)
}

/// 开关为开、且现有 plist 未指向当前可执行文件时安装值守。
///
/// best-effort：安装失败只记日志。调用方是 `board/create`，创建一个看板不该
/// 因为装不上开机自启而整体失败。`EXE` 换位置也要重装（升级后常见），故判据是
/// [`yi_agent_boards::watchman::is_installed_for`] 而非「plist 是否存在」。
pub(crate) fn ensure_watchman_installed(workdir: &Path, home: &Path, install: &WatchmanInstall) {
    if !crate::settings_store::load_watchman_enabled(workdir) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if yi_agent_boards::watchman::is_installed_for(home, &exe) {
        return;
    }
    if let Err(error) = install(&exe, home) {
        tracing::warn!(%error, "failed to install the board watchman");
    }
}

/// The key that identifies one project directory across this process.
///
/// Runtime attach and every later lookup must agree on this value, so it is
/// computed in exactly one place. A path that exists canonicalizes whole, which
/// resolves symlinks and, on macOS, `/tmp` -> `/private/tmp`. A path that does
/// not exist yet still has to key the *same* before and after it is created, so
/// the deepest existing ancestor is canonicalized and the remaining components
/// are appended unchanged. Without that second half an absent directory keys as
/// its literal spelling while the same directory keys as its canonical path once
/// it exists -- two keys for one project, which makes delegation silently
/// disappear (attach stores one key, the lookup misses it) whenever the two
/// spellings differ.
fn project_key(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    loop {
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        if let Some(name) = cursor.file_name() {
            suffix.push(name.to_os_string());
        }
        if let Ok(canonical) = std::fs::canonicalize(parent) {
            let mut key = canonical;
            for part in suffix.iter().rev() {
                key.push(part);
            }
            return key;
        }
        cursor = parent;
    }
}

/// 一次 attach 的结果:可能被换过工具集的 agent,以及该 thread 首个 turn 要激活的 runtime。
pub(crate) struct Activation {
    pub(crate) built: BuiltAgent,
    pub(crate) runtime: Option<Arc<ThreadRoot>>,
}

/// 为该 cwd 的 agent 接上委派能力。失败即降级:保留原 agent,只记 trace。
///
/// 调用方必须已拿到 `build_agent` 的产物,且**不得**在 `threads` 的可变借用内调用:
/// 本函数只经 `runtimes` 的 `Mutex` 访问,不触碰 `threads`,所以先调用、再
/// `threads.insert(..)` 是安全的。
#[allow(clippy::too_many_arguments)]
pub(crate) fn attach_delegation(
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    runtime_dir: &Path,
    models_path: &Path,
    cfg: &RuntimeConfig,
    cwd: &str,
    thread_id: &str,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    // 本次会话的关联根（主根之外），由调用方在 `with_session_extra_roots` 的同一
    // 作用域内捕获后传入——**不在此处再读槽位**，否则调用点一旦漏设就会静默拿到
    // 空集（或残留值）。
    extra_roots: Vec<PathBuf>,
    lead_inbox: &crate::lead::LeadInbox,
    // 阻塞式 `ask_lead` 的回信登记表。与 `lead_inbox` 必须成对传递:
    // 只传信箱而不传登记表,`await` 侧登记的名字将永远无人回填。
    lead_replies: &crate::lead::LeadReplies,
    // 本会话的起点 sha(默认 diff 基准)。**由调用方给出,不在本函数里读盘**:
    // `thread/start` 走到这里时 meta 尚未落盘(`start_thread_core` 先 attach 再
    // `create`),在这里读只会永远读到 `None`,工具就落不到会话起点上。调用方
    // 才知道自己在哪个分支——新建取 `session_anchor(cwd)`,resume 取已落盘 meta
    // 的 `anchor_sha`(旧会话没有该字段时传 `None`,表示「无起点」,不回填)。
    anchor_sha: Option<String>,
    built: BuiltAgent,
) -> Activation {
    let mut thread_cfg = cfg.clone();
    thread_cfg.workdir = PathBuf::from(cwd);
    let binding = match attach_cwd_runtime(runtimes, runtime_dir, &thread_cfg, models_path) {
        Ok(binding) => binding,
        Err(cause) => {
            tracing::warn!(
                stage = "attach",
                %cause,
                cwd,
                "subagent delegation unavailable for this thread"
            );
            return Activation {
                built,
                runtime: None,
            };
        }
    };
    // The daemon is shared per project, but the root is per conversation: the
    // root is what carries the child budget, so two conversations in one
    // directory must never share it.
    // The same captured set that goes into the thread meta: a conversation's
    // delegation root must carry the same linked roots the rest of the session
    // was built with, or the two disagree with no way to notice.
    let root = ThreadRoot::new(
        Arc::clone(&binding),
        thread_id,
        PathBuf::from(cwd),
        extra_roots.clone(),
    );
    if let Err(cause) = root.attach() {
        tracing::warn!(
            stage = "attach_root",
            %cause,
            cwd,
            thread_id,
            "subagent delegation unavailable for this thread"
        );
        return Activation {
            built,
            runtime: None,
        };
    }
    thread_roots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(thread_id.to_string(), Arc::clone(&root));
    match build_runtime_tooling(
        &thread_cfg,
        &root,
        thread_id,
        built.yolo.clone(),
        theme.clone(),
        git_diff.clone(),
        extra_roots,
        // 项目键从本次会话的 cwd 求得(与 `ask_lead` 的投递路由同一来源):
        // 会话给组长留言时,消息必须落到「本目录」的组长信箱。
        crate::thread_store::ThreadStore::new(Path::new(cwd)).project_key(),
        lead_inbox.clone(),
        lead_replies.clone(),
        anchor_sha,
    ) {
        Ok(tooling) => Activation {
            built: wrap_for_delegation(built, tooling),
            runtime: Some(root),
        },
        Err(cause) => {
            tracing::warn!(
                stage = "tooling",
                %cause,
                cwd,
                "subagent delegation unavailable for this thread"
            );
            Activation {
                built,
                runtime: None,
            }
        }
    }
}

/// Reject an `agent/*` request for a conversation the server does not host.
///
/// Distinct from an unattached conversation, which exists and answers with an
/// empty result: a thread id that was never started is a caller error.
pub(crate) async fn require_known_thread(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    id: RequestId,
) -> anyhow::Result<bool> {
    if threads.contains_key(thread_id) {
        return Ok(true);
    }
    write_response(
        hub,
        client,
        err_response(id, RpcError::unknown_thread(thread_id)),
    )
    .await?;
    Ok(false)
}

/// The runtime socket a conversation's children live on.
///
/// The socket belongs to the attached project root, which is keyed by the
/// thread's own directory. A thread that never attached has no socket, and every
/// `agent/*` answer for it is empty rather than an error.
pub(crate) fn socket_for_thread(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> Option<PathBuf> {
    let cwd = threads.get(thread_id)?.cwd.clone();
    let key = project_key(Path::new(&cwd));
    let entry = runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(&key)?
        .clone();
    entry
        .ok()
        .and_then(|binding| binding.current().ok())
        .map(|handle| handle.socket_path)
}

/// What a conversation-scoped cancellation did.
pub(crate) enum ThreadCancelOutcome {
    /// Nothing left to cancel, or the caller forced and everything was reaped.
    Cancelled(usize),
    /// Live children exist and the caller did not force: nothing was touched.
    NeedsConfirmation(usize),
}

/// Cancels every live child a conversation owns, before its files go away.
///
/// The children live in the shared project runtime, where the conversation's
/// files are not the authority, so deleting them alone would leave the agents
/// running where nobody can see them. The cancellation is scoped by the
/// conversation marker rather than the root: several conversations share one
/// attached root, and a root-scoped cancel would stop a sibling's work too.
///
/// Unless `force` is set, live children stop the deletion so the caller can ask
/// the user first: losing running work to one click on a sidebar × is not a
/// decision the backend should make silently.
pub(crate) fn cancel_thread_children(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    force: bool,
) -> Result<ThreadCancelOutcome, String> {
    let cwd = threads
        .get(thread_id)
        .map(|session| session.cwd.clone())
        .ok_or_else(|| "the thread is not held in memory".to_string())?;
    let key = project_key(Path::new(&cwd));
    let binding = {
        let guard = runtimes.lock().unwrap_or_else(|p| p.into_inner());
        guard.get(&key).cloned()
    }
    .ok_or_else(|| "no attached runtime for this project".to_string())??;
    let response = binding.send(
        |handle| yi_agent_store::ipc::IpcRequest::CancelThreadTasks {
            session_id: handle.session_id.clone(),
            thread_id: thread_id.to_owned(),
            force,
        },
    )?;
    match response {
        yi_agent_store::ipc::IpcResponse::ThreadTasksCancelled { task_ids } => {
            Ok(ThreadCancelOutcome::Cancelled(task_ids.len()))
        }
        yi_agent_store::ipc::IpcResponse::ThreadHasActiveChildren { count } => {
            Ok(ThreadCancelOutcome::NeedsConfirmation(count))
        }
        other => Err(format!(
            "daemon returned a non-cancellation response: {other:?}"
        )),
    }
}

/// The task summaries the daemon holds for a conversation's directory.
pub(crate) fn list_task_summaries(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
) -> Result<Vec<yi_agent_store::ipc::IpcTaskSummary>, String> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| "no attached runtime for this thread".to_string())?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .map_err(|error| error.to_string())?;
    match response {
        yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks } => Ok(tasks),
        other => Err(format!("daemon returned a non-summary response: {other:?}")),
    }
}

/// Read a task's persisted trace, plus the high-water mark to resume from.
pub(crate) fn read_task_trace(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    task_id: &str,
) -> Result<(Vec<crate::protocol::AgentTraceRow>, i64), RpcError> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| RpcError::unknown_thread(thread_id))?;
    let snapshot = yi_agent_store::ipc::read_task_trace(&socket, task_id)
        .map_err(|error| RpcError::internal(error.to_string()))?;
    Ok((trace_rows(snapshot.rows), snapshot.high_water_id))
}

/// The watch's opening frame carries the backlog the client would otherwise miss.
pub(crate) fn snapshot_for_watch(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    task_id: &str,
) -> Result<(Vec<crate::protocol::AgentTraceRow>, i64), RpcError> {
    read_task_trace(runtimes, threads, thread_id, task_id)
}

fn trace_rows(rows: Vec<yi_agent_store::ipc::IpcTraceRow>) -> Vec<crate::protocol::AgentTraceRow> {
    rows.into_iter().map(agent_trace_row).collect()
}

fn agent_trace_row(row: yi_agent_store::ipc::IpcTraceRow) -> crate::protocol::AgentTraceRow {
    crate::protocol::AgentTraceRow {
        event_id: row.event_id,
        task_id: row.task_id,
        kind: row.kind,
        payload_json: row.payload_json,
    }
}

/// Send one request to the daemon the conversation is attached to.
///
/// A request the daemon rejects is an error the caller must see, so the daemon's
/// own error frame is turned into the RPC error rather than swallowed. A
/// conversation with no attached runtime is an unknown-thread error, since there
/// is nothing to send to.
pub(crate) fn forward_to_daemon(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    thread_id: &str,
    request: yi_agent_store::ipc::IpcRequest,
) -> Result<yi_agent_store::ipc::IpcResponse, RpcError> {
    let socket = socket_for_thread(runtimes, threads, thread_id)
        .ok_or_else(|| RpcError::unknown_thread(thread_id))?;
    let response = yi_agent_store::ipc::send_request(&socket, request)
        .map_err(|error| RpcError::internal(format!("daemon is unavailable: {error}")))?;
    if let yi_agent_store::ipc::IpcResponse::Error { code, message } = &response {
        return Err(RpcError::internal(format!(
            "daemon rejected the request: {code:?} {}",
            message.clone().unwrap_or_default()
        )));
    }
    Ok(response)
}

/// Push the conversation's child list whenever it changes.
///
/// The first frame is skipped: the caller has just answered the same question
/// synchronously, so pushing it again would be a duplicate. Every later
/// difference is pushed whole, because the list is small and a client that
/// applies it wholesale cannot drift out of order.
pub(crate) async fn watch_children(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    socket: PathBuf,
    initial: Vec<crate::protocol::AgentChild>,
) {
    let mut last = initial;
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let tasks = match yi_agent_store::ipc::send_request(
            &socket,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        ) {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks }) => tasks,
            // A daemon that is briefly unavailable is not a change; the next
            // poll will pick the list back up.
            _ => continue,
        };
        let current = children_for_thread(&tasks, &thread_id);
        if current == last {
            continue;
        }
        last = current.clone();
        let notification = Notification::AgentChildrenUpdated {
            thread_id: thread_id.clone(),
            children: current,
        };
        if write_notification(&hub, &notification).await.is_err() {
            return;
        }
    }
}

/// 把 manager 的状态广播转成 `process/updated` 通知。
///
/// 只在状态变化时推送(`Output` 丢弃):高频输出走 `process/read` 按需拉取。
/// 关闭 thread 时由调用方 abort。
pub(crate) async fn watch_processes(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    manager: Arc<yi_agent_tools::ProcessManager>,
    mut rx: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
) {
    use yi_agent_tools::ProcessEvent;
    loop {
        match rx.recv().await {
            Ok(event) => {
                let (process_id, state) = match event {
                    ProcessEvent::Started { process_id } => (process_id, "starting"),
                    ProcessEvent::Ready { process_id } => (process_id, "ready"),
                    ProcessEvent::Exited { process_id, .. } => (process_id, "exited"),
                    ProcessEvent::Killed { process_id } => (process_id, "killed"),
                    // 输出是高频流:不转发,详情页自己按需增量拉。
                    ProcessEvent::Output { .. } => continue,
                };
                let _ = manager.list(); // 保证条目仍在(仅作存在性自检,结果丢弃)
                let _ = write_notification(
                    &hub,
                    &Notification::ProcessUpdated {
                        thread_id: thread_id.clone(),
                        process_id,
                        state: state.to_string(),
                    },
                )
                .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// Forward a watched task's rows until the stream ends or the task is aborted.
///
/// The subscription is non-blocking so the task yields between polls; it ends on
/// its own when the daemon closes the stream, and is aborted when the client
/// unwatches, watches another task, or closes the thread.
pub(crate) async fn stream_trace(
    hub: Arc<crate::broadcast::Broadcaster>,
    thread_id: String,
    task_id: String,
    socket: PathBuf,
    after_id: i64,
) {
    let subscription = match yi_agent_store::ipc::subscribe_trace(
        &socket,
        after_id,
        std::slice::from_ref(&task_id),
        &[],
    ) {
        Ok(subscription) => subscription,
        Err(error) => {
            tracing::warn!(%error, %thread_id, %task_id, "trace watch could not subscribe");
            return;
        }
    };
    if let Err(error) = subscription.set_nonblocking(true) {
        tracing::warn!(%error, %thread_id, %task_id, "trace watch could not poll");
        return;
    }
    let mut subscription = subscription;
    loop {
        match subscription.try_row() {
            Ok(Some(row)) => {
                let notification = Notification::AgentTraceEvent {
                    thread_id: thread_id.clone(),
                    task_id: task_id.clone(),
                    row: agent_trace_row(row),
                };
                if write_notification(&hub, &notification).await.is_err() {
                    return;
                }
            }
            // Nothing yet, or the stream ended: either way, yield and retry.
            // The stream ending is indistinguishable from a quiet moment by
            // design, so the loop is bounded by the abort that the caller does.
            Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
            Err(error) => {
                tracing::warn!(%error, %thread_id, %task_id, "trace watch stream failed");
                return;
            }
        }
    }
}

/// The children of one conversation, derived from a task-summary snapshot.
///
/// A conversation is scoped by the `thread_id` the spawner bound into its tools,
/// not by the directory: several conversations can share one directory and one
/// attached root, so the marker is the only thing that separates their children.
/// Root tasks are never children, and the `thread_id` filter is exact, so an
/// untagged task belongs to no conversation rather than to all of them.
pub(crate) fn children_for_thread(
    tasks: &[yi_agent_store::ipc::IpcTaskSummary],
    thread_id: &str,
) -> Vec<crate::protocol::AgentChild> {
    tasks
        .iter()
        .filter(|task| !task.is_root)
        .filter(|task| task.thread_id.as_deref() == Some(thread_id))
        .map(|task| crate::protocol::AgentChild {
            task_id: task.task_id.clone(),
            // `ListTaskSummaries` carries no objective; a later source can fill
            // it without changing this shape.
            objective: None,
            state: task.state.clone(),
            last_step: None,
            parent_task_id: task.parent_task_id.clone(),
        })
        .collect()
}

/// Whether a task state can still make progress.
#[cfg(test)]
fn is_terminal_state(state: &str) -> bool {
    matches!(
        state,
        "completed" | "completed_no_changes" | "failed" | "cancelled"
    )
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

/// 取出(或建立)该 cwd 的项目 runtime。
fn attach_cwd_runtime(
    runtimes: &ProjectRuntimes,
    runtime_dir: &Path,
    cfg: &RuntimeConfig,
    models_path: &Path,
) -> Result<Arc<RuntimeBinding>, String> {
    let key = project_key(&cfg.workdir);
    // A project directory that no longer exists has nothing to bring up, and the
    // bring-up path creates the runtime directory -- which would resurrect
    // `.yi-agent/runtime` inside a worktree that was already reclaimed. Refuse
    // before consulting the cache, and *drop* any cached entry: the cache stores
    // failures on purpose so a doomed bring-up is not re-run on every thread, but
    // a missing directory is recoverable -- the project can come back -- and a
    // cached `Err` would lock that recovery out forever.
    if !cfg.workdir.exists() {
        runtimes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        return Err(format!(
            "the project directory {} no longer exists",
            cfg.workdir.display()
        ));
    }
    if let Some(existing) = runtimes.lock().unwrap_or_else(|p| p.into_inner()).get(&key) {
        return existing.clone();
    }
    // Own a *usable* runtime: adopt a healthy daemon, replace a dead or wedged
    // one. The binding it returns heals itself on later calls, so a daemon that
    // dies after this point (e.g. its terminal closes) no longer strands the
    // thread's delegation tools.
    let outcome = yi_agent_subagent::attach::ensure_owned_runtime(
        cfg,
        runtime_dir.to_path_buf(),
        models_path,
    )
    .map(|attached| {
        RuntimeBinding::managed(
            cfg,
            runtime_dir.to_path_buf(),
            models_path.to_path_buf(),
            Arc::new(attached),
        )
    })
    .map_err(|failure| format!("{}: {}", failure.stage, failure.cause));
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key, outcome.clone());
    outcome
}

/// 断开已 attach、但已无 thread 使用的项目 root。
///
/// 判据是「该项目目录下还有没有活着的 thread」,而不是「被删的 thread 属于哪个项目」:
/// `thread/delete` 靠 `store_lookup` 按 thread_id 定位,分支里拿不到 cwd。这样删掉某个
/// 项目里的**一个** thread 不会连带废掉同项目其它 thread 的委派(daemon 对重复 detach
/// 幂等,但被断开后那个 driver 不会再激活第二次,故不能多断)。
pub(crate) fn detach_unused_runtimes(runtimes: &ProjectRuntimes, live_cwds: &[String]) {
    for binding in attached_runtimes(runtimes) {
        if live_cwds
            .iter()
            .any(|cwd| project_key(Path::new(cwd)) == project_key(&binding.project_root()))
        {
            continue;
        }
        binding.detach();
    }
}

/// 本进程已 attach 的 runtime(只看成功项)。
fn attached_runtimes(runtimes: &ProjectRuntimes) -> Vec<Arc<RuntimeBinding>> {
    runtimes
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .values()
        .filter_map(|entry| entry.clone().ok())
        .collect()
}

/// 委派就绪的工具集。
///
/// 只有内置工具与权限根搬到 checkout;skills 与 MCP 的项目根仍取 `cfg.workdir`
/// (用户选的项目)。`yolo` 必须是该 thread 自己的开关,否则运行期切 YOLO 时沙箱层
/// 与权限层会脱钩。
// 这些参数彼此独立(装配期各自注入),拆成结构体只会把同一批值再包一层。
#[allow(clippy::too_many_arguments)]
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
    // 本次会话的关联根（主根之外）。逐调用传入：它同时决定 FS 根集与 shell 沙箱的
    // 额外可写根，且不得跨会话共享。
    extra_roots: Vec<PathBuf>,
    // 本会话所属项目的键(见 `ThreadStore::project_key`)与该进程的组长信箱。
    // 两者绑在装配期,与下面的 thread marker 同一理由:模型看不到自己的
    // thread id,也不知道自己在哪个项目,只能由装配期注入。
    project_key: String,
    lead_inbox: crate::lead::LeadInbox,
    // 回信登记表与超时:两者一起决定 `blocking: true` 是否真的有界往返
    // (只有登记表而没有超时来源,或反之,都会让契约半途而废)。
    lead_replies: crate::lead::LeadReplies,
    // 本会话的起点 sha：`show_git_diff` 默认按它算 diff，并在回执里如实报出。
    // 与 project_key 同理——模型看不到会话元数据，装配期注入是唯一来源。
    anchor_sha: Option<String>,
) -> Result<RuntimeTooling, String> {
    // The tools get the root handle itself, not its snapshot: they re-resolve
    // the live socket and this conversation's own root on every call.
    let workspace_root = root.handle()?.workspace_root;
    // FS 根集 = 主根（工作区）+ 关联根。shell 沙箱的额外可写根由
    // `build_tool_setup_with_controller` 内部按同一口径推导（`roots[1..]` +
    // `cfg.sandbox_writable_roots`），故此处不再单独拼一遍。
    let mut roots = vec![workspace_root.clone()];
    roots.extend(extra_roots.iter().cloned());
    // 本会话的 thread store。根取 `cfg.workdir`,它在**此处就是 thread cwd**:
    // `attach_delegation` 在调用本函数之前先做了
    // `thread_cfg.workdir = PathBuf::from(cwd)`(冷启动、resume 两条生产路径同形)。
    //
    // 刻意**不**用上面的 `workspace_root`:那是 daemon 回传的 git **toplevel**
    // (`rev-parse --show-toplevel`),而组长的 meta 落在 `<cwd>/.yi-agent/threads`。
    // 会话 cwd 是仓库子目录时两者分叉,按 toplevel 去找子目录里的组长只会 `load` 回
    // `Ok(None)`,真组长静默丢掉两个工具。用 cwd 也让工具读的 store 与
    // `thread/lead`、冷启动写 meta 的那一份**同源**。装配期用它读落盘的 `role`,
    // 判定「本 thread 是不是组长」——这是唯一权威。
    let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
    // One controller, one truth: it backs the root's builtin tools AND the
    // subagent spawn tools, and it reads the thread's live YOLO switch.
    let controller =
        yi_agent_tools::SandboxController::new(yolo.clone(), cfg.sandbox, cfg.sandbox_promotable);
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(
        cfg,
        false,
        &roots,
        controller.clone(),
        vec![("YI_AGENT_THREAD_ID".to_string(), thread_id.to_string())],
    )
    .map_err(|error| error.to_string())?;
    let mut registry = (*setup.tools).clone();
    let process_manager = Arc::clone(&setup.process_manager);
    // One caller handle, shared with the delegation tools: the host binds it to
    // the thread's live session right after building the Agent, and the tools
    // re-read that session on every call.
    let caller = yi_agent_subagent::CallerContext::unbound();
    // The conversation marker is bound here, not inferred later: the model that
    // calls `spawn_agent` never sees a thread id, so each thread's tools carry
    // their own so every child they spawn is tagged with this conversation.
    yi_agent_subagent::register_attached_root_tools_in_thread(
        &mut registry,
        Arc::clone(root),
        controller,
        Some(thread_id.to_string()),
        caller.clone(),
    );
    let permission = yi_agent_runtime::bootstrap::load_permission_checker_with_roots(&roots, yolo)
        .map_err(|error| error.to_string())?;
    register_theme_tool(&mut registry, theme);
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
        // 仓库取 `cfg.workdir`,与上面的 `store` 同源:此处它就是 thread cwd,
        // 而不是 daemon 回传的 toplevel(见上文的注释)。
        cfg.workdir.clone(),
        anchor_sha,
    )));
    // 普通会话用它给组长留言。`from_thread_id` 绑在装配期,与
    // `register_attached_root_tools_in_thread` 的 thread marker 同理由:
    // 模型看不到自己的 thread id,只能由装配期注入。
    //
    // 两类会话**不注册**:
    // - 卡片会话(项目键落在 `.worktrees/kanban/` 下):它所在的那个 worktree 不是
    //   项目、没有组长,`ask_lead` 只会把消息投进一个空邮箱,并在主循环里经
    //   `resolve_lead_target` 给 worktree 造一个假组长(真机已有 9 例)。与
    //   `find_lead`/`is_lead_meta` 同一判定来源(`project_key` 就是
    //   `<cwd>/.yi-agent` 的 canonical 串,去掉末段即 cwd)。
    // - 组长自己:它就是那个收件人(`resolve_lead_target(project_key)` 反查回来的
    //   正是它),注册了等于自问自答——单向时每轮往自己信箱塞一条「来自会话<自己>」,
    //   阻塞时登记一个只有自己能回填的 request_id,无人应答、白等满 120s 超时。
    //   判定复用下面的 `is_lead_thread`(落盘 `ThreadMeta.role`,唯一权威),与
    //   lead_memory 的注册同源;组长要影响自己直接写即可,不需要这个工具。
    let is_lead = crate::lead_memory::is_lead_thread(&store, thread_id);
    let state_dir = Path::new(&project_key);
    let session_cwd = state_dir.parent().unwrap_or(state_dir);
    if !is_lead && !crate::thread_store::is_kanban_worktree(session_cwd) {
        registry.register(Arc::new(
            crate::lead::AskLeadTool::new(lead_inbox, project_key, thread_id.to_string())
                // 回信能力**必须**在这里接上:漏掉它没有编译错误,`blocking: true`
                // 只会静默退化成 P1 的显式拒绝。超时用共享常量,不写裸字面量——
                // 两个 2 分钟各自漂移时,「等待方超时」与「组长还有多久回来」会脱钩。
                .with_replies(lead_replies, crate::lead::ASK_LEAD_TIMEOUT),
        ));
    }
    // lead_memory 只给组长:普通会话与子 worker 一律看不到(spec §7.5)。判定用
    // 落盘的 `ThreadMeta.role`(唯一权威),不靠标题/名字/cwd 猜——判定函数见
    // `lead_memory::is_lead_thread`(结论已在上面的 `is_lead` 里取过一次,此处复用)。
    //
    // `project_root` 取 `cfg.workdir`(= thread cwd),理由与上面的 store 同根:
    // ① meta 落在 `<cwd>/.yi-agent/threads`,故账本也必须落在 `<cwd>/.yi-agent`
    //    下,两者才是同一个「项目」;
    // ② 本功能其余部分对「组长的项目」的定义就是 cwd —— Task 6 的后台 tick 用
    //    `session.store.project_key()`(= `<cwd>/.yi-agent`)枚举组长,
    //    `project_root_of(project_key)` 即其 parent = cwd;
    // ③ 若留成 git toplevel,`<repo>` 与 `<repo>/sub` 两个不同 project_key(两个
    //    不同组长)会共用同一份 `<repo>/.yi-agent/lead_memory/memory.md`,跨项目
    //    账本串味。
    if is_lead {
        let project_root = cfg.workdir.clone();
        registry.register(Arc::new(crate::lead_memory::LeadRememberTool::new(
            project_root.clone(),
        )));
        registry.register(Arc::new(crate::lead_memory::LeadRecallTool::new(
            project_root,
        )));
    }
    Ok(RuntimeTooling {
        registry: Arc::new(registry),
        permission,
        process_manager,
        caller,
    })
}

/// 把主题工具注册进一个 thread 的工具集。
///
/// 委派可用时走 [`build_runtime_tooling`] 的 registry,不可用时走工厂里用
/// bootstrap `tools` 克隆出来的 registry——两条路径都必须注册,否则非 git
/// 目录下自然语言切主题会静默失效。
fn register_theme_tool(
    registry: &mut yi_agent_core::ToolRegistry,
    theme: crate::theme_tool::ThemeHandle,
) {
    registry.register(Arc::new(crate::theme_tool::SetThemeTool::new(theme)));
}

/// 克隆一份 bootstrap 工具集并补上主题工具。
///
/// 非委派路径(`Agent::new` + bootstrap 克隆)与委派路径共用同一段注册逻辑,
/// 避免两条路径漂移出「一边有 tool、一边没有」的缺口。
fn registry_with_theme_tool(
    tools: &Arc<yi_agent_core::ToolRegistry>,
    theme: crate::theme_tool::ThemeHandle,
) -> yi_agent_core::ToolRegistry {
    let mut registry = (**tools).clone();
    register_theme_tool(&mut registry, theme);
    registry
}

/// 用带主题工具的工具集重建 thread agent,并保留原 agent 的审批路径。
///
/// `Agent::new` 从零开始、不含权限检查器与决定接收端,所以重建后必须用
/// [`Agent::permission_checker`] / [`Agent::decision_rx`] 取回并重新装上——
/// 否则非 git 目录下的 thread 会退化成不弹审批。两个取回器都为 `None`
/// (AutoAllow)时保持不装,与原 agent 一致。
///
/// provider 沿用 bootstrap 的同一个实例:再建一个会重读凭据、多一个 client。
fn rebuild_thread_agent_with_theme(
    built: yi_agent_runtime::bootstrap::AgentBootstrap,
    session: Option<yi_agent_core::Session>,
    theme: crate::theme_tool::ThemeHandle,
) -> BuiltAgent {
    let yi_agent_runtime::bootstrap::AgentBootstrap {
        agent,
        provider,
        tools,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
        ..
    } = built;
    let mut config = agent.config().clone();
    // 让重建后的 agent 沿用本次会话的 `preserve_thinking`：`build_agent_config`
    // 只认 `RuntimeConfig`（无该字段），故值经 thread-local 由本函数落到
    // `AgentConfig` 上（镜像 effort 的做法）。未设定时按默认 `true`。
    config.preserve_thinking = current_session_preserve_thinking();
    let permission = agent.permission_checker();
    let decision = agent.decision_rx();
    // Reuse the original session `Arc`, not a deep clone: a `CallerContext`
    // bound to this thread's live conversation must keep seeing the same
    // session across the rebuild.
    let session_handle = agent.session_handle();
    let registry = registry_with_theme_tool(&tools, theme);
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
            .with_session_arc(session_handle);
    if let (Some(checker), Some(rx)) = (permission, decision) {
        rebuilt = rebuilt.with_permission(checker, rx);
    }
    apply_session(&mut rebuilt, session);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        process_manager,
    }
}

/// 把 thread 的工具集与权限根换成 attached runtime 的,其余部件沿用。
fn wrap_for_delegation(built: BuiltAgent, tooling: RuntimeTooling) -> BuiltAgent {
    let BuiltAgent {
        agent,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        ..
    } = built;
    // Reuse the live session `Arc` so the delegation tools' `CallerContext`,
    // bound just below, survives the rebuild and keeps reading the thread's
    // current transcript rather than a snapshot taken here.
    let session_handle = agent.session_handle();
    let mut rebuilt =
        yi_agent_core::Agent::new(provider.clone(), tooling.registry.clone(), config.clone())
            .with_session_arc(session_handle.clone());
    if let Some(rx) = decision_rx.clone() {
        rebuilt = rebuilt.with_permission(tooling.permission.clone(), rx);
    }
    // Bind the caller to the rebuilt agent's *live* session handle. `tooling`'s
    // `caller` and the registry's tool instances share one inner slot, so this
    // bind is what makes `fork: true` read this conversation.
    tooling.caller.bind(session_handle);
    BuiltAgent {
        agent: rebuilt,
        provider,
        config,
        decision_tx,
        decision_rx,
        catalog,
        yolo,
        // 注册表换了,manager 必须跟着换:否则面板查的是被丢弃的那一份。
        process_manager: tooling.process_manager,
    }
}

/// Why a board query produced no answer.
///
/// `code` is the stable string the UI branches on: "there is no board here" is
/// a different remedy from "the daemon is down" and from "the plugin is not
/// installed", and a bare message collapsed all three into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardQueryError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl BoardQueryError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BoardQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

/// Ask a supervised plugin a question and return its answer verbatim.
///
/// The host attaches no meaning to `method` or `params`, and unwraps no board
/// shape here: this is a generic channel, so the UI (not the server) owns what a
/// card or a switch field means.
///
/// `project` is both where the plugin is installed and where its daemon runs:
/// the manifest lives at `<project>/.yi-agent/supervisors/`, the socket at
/// `<project>/.yi-agent/runtime/`, and the daemon is the project's own. Using the
/// app's cwd instead made every query for another project hit the wrong socket
/// (or none) — the "clicked and nothing happened" bug.
///
/// Deliberately **not** gated on any registry: whether an answer exists is the
/// socket's business, not the host's. Gating it on "does this project have a
/// board" made the generic channel refuse projects the plugin could have served,
/// and left the host knowing what a board is.
pub(crate) fn plugin_query(
    project: &Path,
    method: &str,
    plugin: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    if plugin.is_empty() {
        return Err(BoardQueryError::new(
            "invalid_params",
            "plugin/query needs a `plugin` name",
        ));
    }
    // The daemon that runs the plugin lives at the project root, which is also
    // what `board_daemon` dials.
    // 复用共享 helper 而不是手写 `.yi-agent/runtime`：它先认 `YI_AGENT_RUNTIME_DIR`，
    // 与工作区其它地方对「runtime socket 在哪」保持同一个定义。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(project);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        BoardQueryError::new(
            "daemon_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| {
        BoardQueryError::new(
            "daemon_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // A daemon that is up but has no answer for this plugin is the plugin's
        // absence, not the daemon's: the wording keeps the shape the desktop's
        // `pluginIsUnavailable` matches on.
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon returned an unexpected response: {other:?}"),
        )),
    }
}

/// 向插件问一条通用问题（当前是 `plugin/settings/*`）。
///
/// 与 `plugin_query` 的区别：不受看板登记表门控——任何插件都能走；宿主只转发
/// `method`/`params`，不解释应答。
///
/// `scope` 是**要配置的那个项目**（`<项目>/.yi-agent/supervisors/` 放清单、
/// `<项目>/.yi-agent/runtime/` 放它自己的 daemon socket）。看板按项目安装、由项目
/// 自己的 daemon 监管，桌面侧车 app-server 的 workdir 却是用户 home：固定用 workdir
/// 会既找不到清单（读成 `plugin_not_installed`）、又拨不到 socket（读成
/// `plugin_unavailable`）——这正是「看板跑着、设置页却空着」的根因。
pub(crate) fn plugin_query_self(
    scope: &Path,
    plugin: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    if plugin.is_empty() {
        return Err(BoardQueryError::new(
            "invalid_params",
            "plugin/settings needs a `plugin` name",
        ));
    }
    let summary = list_installed_plugins(scope)
        .into_iter()
        .find(|summary| summary.name == plugin);
    let Some(summary) = summary else {
        return Err(BoardQueryError::new(
            "plugin_not_installed",
            format!("plugin {plugin} is not installed"),
        ));
    };
    if !summary.queryable {
        return Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("plugin {plugin} does not declare a query socket"),
        ));
    }
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(scope);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| {
        BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // The plugin received the request and answered with its own rejection
        // (e.g. an invalid settings payload). That is the plugin talking, not
        // the plugin being gone: `plugin_rejected` keeps the reason apart from
        // the two genuine unavailability arms above, so the desktop can show
        // the plugin's message instead of "插件未运行".
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(BoardQueryError::new(
            "plugin_rejected",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon returned an unexpected response: {other:?}"),
        )),
    }
}

/// 已装插件的一行。宿主不解释任何插件语义：`name`/`switch_key` 原样来自清单，
/// `queryable` 只是「该清单声明了 query_socket」。
pub(crate) struct PluginSummary {
    pub name: String,
    pub queryable: bool,
    pub switch_key: String,
}

/// 列出 `<scope>/.yi-agent/supervisors/` 下的插件清单。
///
/// 纯文件读取：daemon 没起也能列。目录缺失或清单损坏都只是「少一个插件」，
/// 不是错误——一个坏清单不该让整个设置页打不开。
///
/// `scope` 由调用方给出：看板插件是**按项目**安装的（清单落在
/// `<项目>/.yi-agent/supervisors/`），而桌面侧车 app-server 的 workdir 是用户
/// home。若固定读 workdir，桌面端在 home 下一个插件也看不到——这正是看板跑着、
/// 设置页却空着的根因。`plugins/list` 因此在调用方给了 `project` 时枚举该项目，
/// 否则回落到宿主自身 workdir（TUI 与未带 project 的调用保持原语义）。
pub(crate) fn list_installed_plugins(scope: &Path) -> Vec<PluginSummary> {
    let dir = scope.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| PluginSummary {
            name: manifest.name,
            queryable: manifest.query_socket.is_some(),
            switch_key: manifest.switch_key,
        })
        .collect()
}

/// The project a `board/*` request names, canonicalized when the directory
/// exists.
///
/// The registry compares paths literally, so one project spelled two ways
/// (a symlink, a trailing slash, a relative path) would otherwise register
/// twice and draw two sidebar entries for one project. Returns `None` for a
/// missing or empty `project`, which every caller answers with
/// `invalid_params`.
pub(crate) fn project_arg(params: &serde_json::Value) -> Option<PathBuf> {
    let raw = params.get("project").and_then(|value| value.as_str())?;
    if raw.is_empty() {
        return None;
    }
    Some(std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw)))
}

/// 通用插件通道（`plugins/list` / `plugin/settings/*`）的作用域。
///
/// 调用方给了非空 `project` 就用它，否则回落到宿主自身 workdir。插件按项目
/// 安装、由项目自己的 daemon 监管，而桌面侧车的 workdir 是 home，所以设置页
/// 必须能把作用域指到它要配置的那个项目（清单与 socket 都在它下面）；不传
/// `project` 的调用方（TUI、旧客户端）保持「宿主 workdir」的原语义。
pub(crate) fn plugin_scope(workdir: &Path, params: &serde_json::Value) -> PathBuf {
    params
        .get("project")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf())
}

/// Serialize a board value for the wire. A value that cannot serialize is the
/// null case rather than a hang: these shapes are plain data, so this never
/// fires in practice, but an RPC must always write *some* response.
pub(crate) fn to_json<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

/// 后台自愈循环:app 打开期间每 ~30s 一次,与常驻 watchman 重叠也无害(幂等)。
///
/// 单轮逻辑复用 `yi_agent_boards::watch::once`(读通用登记 → 交给 `ensure`),
/// **不再另写一份**「读登记 → 拉起」的胶水;`ensure_daemons` 同样是 CLI 与
/// watchman 共用的那一份。`resident_dir` 由调用方传入,循环内不重读 `HOME`。
fn spawn_board_watchman_loop(resident_dir: PathBuf) {
    tokio::spawn(async move {
        loop {
            yi_agent_boards::watch::once(&resident_dir, &mut |projects| {
                yi_agent_boards::watch::ensure_daemons(projects);
            });
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
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
    // make the whole server fail to start. Board RPCs called in that state
    // register into a directory nobody lists from, which is the same as having
    // no board — better than losing every unrelated RPC.
    let board_dir = yi_agent_boards::global_dir().unwrap_or_default();
    // Same fallback policy as `board_dir`: an unset HOME must not stop the
    // server. Resident RPCs then register where nobody lists from, which is the
    // same as having no resident need.
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 会话期自愈:app 打开期间后台每 ~30s 重新确保每个登记项目都有活着的 daemon,
    // 与 launchd watchman 互补。只在生产入口启动;`serve_stdio`/`serve_scoped` 不装,
    // 故单元测试永远不会拉起真实进程。传入已算好的 `resident_dir`,循环内不重读 HOME。
    spawn_board_watchman_loop(resident_dir.clone());
    // 主题句柄:`ui/settings/read|write` 与每个 thread 的 `set_theme` 工具共用。
    // 工厂闭包 `'static`,拿不到主循环里的 `theme`;先克隆一份专供工厂。
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    // 清单按指纹跟随 `models.json`:运行期改条目,新建 thread 立刻用上新值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
    // 启动期把存量 `.env` 模型键收边进清单（一次性、幂等）。
    migrate_legacy_model_keys_on_startup();
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
            board_dir,
            models_path: yi_agent_runtime::models::models_path(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `board_dir` 同一策略。
            onboarding_env_path: yi_agent_runtime::config::resolve_global_env_path()
                .unwrap_or_default(),
            onboarding_preferences_path: home_dir().join(".yi-agent").join("preferences.json"),
            resident_dir,
            skills_home: home_dir(),
            plugin_command: yi_agent_boards::scaffold::default_plugin_command().unwrap_or_default(),
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            // 进程一条组长信箱,由 `serve` 的主循环消费。
            lead_inbox: crate::lead::LeadInbox::new(),
            // 回信登记表与信箱同一实例范围:`blocking: true` 的 await 侧登记
            // 在这里,回填由同一个 `serve` 主循环做。
            lead_replies: crate::lead::LeadReplies::new(),
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
    let board_dir = yi_agent_boards::global_dir().unwrap_or_default();
    let resident_dir = yi_agent_store::resident::default_dir().unwrap_or_default();
    // 与 `run` 一致:本函数是合一模式的生产入口,故同样在会话期自愈地确保
    // 各登记项目有一枚活着的 daemon(桌面经 `--relay` 启动时不应丢掉该保障)。
    spawn_board_watchman_loop(resident_dir.clone());
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    let theme_for_factory = theme.clone();
    // 与 `run` 一致:清单按指纹跟随 `models.json`,工厂逐会话取当前值。
    let catalog =
        yi_agent_runtime::models::CatalogWatcher::new(yi_agent_runtime::models::models_path());
    // 与 `run` 一致:启动期做一次性 `.env` → 清单迁移。
    migrate_legacy_model_keys_on_startup();
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
            board_dir,
            models_path: yi_agent_runtime::models::models_path(),
            // 引导读写的全局文件。生产 = 真实的 `~/.yi-agent`;测试由 harness
            // 注入隔离 tempdir。HOME 缺失回退空路径,与 `board_dir` 同一策略。
            onboarding_env_path: yi_agent_runtime::config::resolve_global_env_path()
                .unwrap_or_default(),
            onboarding_preferences_path: home_dir().join(".yi-agent").join("preferences.json"),
            resident_dir,
            skills_home: home_dir(),
            plugin_command: yi_agent_boards::scaffold::default_plugin_command().unwrap_or_default(),
            launcher: Arc::new(yi_agent_boards::lifecycle::launch_if_absent),
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            // 进程一条组长信箱,由 `serve` 的主循环消费。
            lead_inbox: crate::lead::LeadInbox::new(),
            // 回信登记表与信箱同一实例范围:`blocking: true` 的 await 侧登记
            // 在这里,回填由同一个 `serve` 主循环做。
            lead_replies: crate::lead::LeadReplies::new(),
        },
        production_factory(cfg, theme_for_factory, catalog),
        // 桌面 = Admin(与 `run`/`serve_stdio` 的 stdio 注册完全一致)。
        Scope::Admin,
        Some(listener),
        Some(relay_url),
    )
    .await
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `model_ref`(清单显示名)。工厂闭包是
    /// `Fn` 且被 `'static` 持有,签名里带不进这个逐会话参数,故用 thread-local
    /// 传入。`build_agent` 同步执行、期间无 `await`,故槽位不会被跨任务污染。
    static SESSION_MODEL_REF: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的模型覆盖(清单显示名),供工厂解析。
///
/// 调用返回后恢复调用前的值,故嵌套调用(如 `thread/start` 内再建其它 agent)不会
/// 泄漏覆盖。`thread/start` 取请求参数、`thread/resume` 取 `loaded.meta.model_ref`。
pub(crate) fn with_session_model_ref<T>(model_ref: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_MODEL_REF.with(|slot| {
        let prev = slot.replace(model_ref);
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 `model_ref`;不在 [`with_session_model_ref`] 内时为空。
fn current_session_model_ref() -> Option<String> {
    SESSION_MODEL_REF.with(|slot| slot.borrow().clone())
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 effort 覆盖（七档之一）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进这个逐会话参数。
    /// 槽位为 `Some` 时**覆盖** `resolve_effective` 从条目带出的默认值；为 `None`
    /// 时跟随条目默认。解析链「会话覆盖 → 条目默认 → 未设置」由此收敛：工厂只认
    /// 「有没有覆盖」，**没有**「强制不发」这一态——清除覆盖等同于未设置(都跟随
    /// 条目默认),这样 resume 重建与活线程经 provider 自补的路径手感一致。
    static SESSION_REASONING_EFFORT: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的 effort 覆盖，供工厂解析。
///
/// `Some(tier)` 覆盖条目默认；`None` 表示本次会话没有覆盖，工厂沿用
/// `resolve_effective` 带出的条目默认。调用返回后恢复调用前的值，故嵌套调用不会
/// 泄漏覆盖。`thread/start` 传 `None`（新会话无覆盖），`thread/resume` 传
/// `meta.reasoning_effort`（清过的即为 `None` ⇒ 同样回落条目默认）。
pub(crate) fn with_session_reasoning_effort<T>(effort: Option<String>, f: impl FnOnce() -> T) -> T {
    SESSION_REASONING_EFFORT.with(|slot| {
        let prev = slot.replace(effort);
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 effort 覆盖；不在
/// [`with_session_reasoning_effort`] 内时为 `None`（无覆盖，跟随条目默认）。
fn current_session_reasoning_effort() -> Option<String> {
    SESSION_REASONING_EFFORT.with(|slot| slot.borrow().clone())
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的**关联根**（主根之外的附属根）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进这个逐会话参数。
    ///
    /// **必须逐调用传**：daemon 的 worker factory 按项目目录构建、被同目录多个会话
    /// 共享，把根集写进共享 `cfg` 会让 A 会话的关联根泄漏给同目录的其它会话（那些
    /// 会话会凭空拿到不该有的写权限）。故这里走 thread-local，工厂每次克隆 `cfg`
    /// 时再逐调用写入。
    static SESSION_EXTRA_ROOTS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 在调用工厂（`build_agent`）前设定本次会话的关联根，供工厂写入 `thread_cfg`。
///
/// 调用返回后恢复调用前的值，故嵌套调用（如 `thread/start` 内再建其它 agent）不会
/// 泄漏关联根。`thread/start` 取本会话的关联根、`thread/resume` 取
/// `meta.extra_roots`。
///
/// 生产调用点（`thread/start`、`thread/resume` 两个分支）各自捕获本会话的根集
/// 后传入槽位；`start_thread_core` 与 resume 侧的 `attach_delegation` 收到的都是
/// 同一个局部量，故槽位与委派路径不可能漂移。
pub(crate) fn with_session_extra_roots<T>(roots: Vec<PathBuf>, f: impl FnOnce() -> T) -> T {
    SESSION_EXTRA_ROOTS.with(|slot| {
        let prev = slot.replace(roots);
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的关联根；不在 [`with_session_extra_roots`] 内时为空
/// （空 = 单根会话，与改动前逐字节一致）。
///
/// 可见性与 [`current_session_model_ref`] 一致（同为私有 `fn`）：`mod tests` 是
/// `server` 的子模块，直接可达父模块私有项，故测试无需放宽可见性。
///
/// 读点有两个：生产代码 `production_factory`，以及测试里的记录型工厂
/// （断言槽位里真正可见的根集）。
fn current_session_extra_roots() -> Vec<PathBuf> {
    SESSION_EXTRA_ROOTS.with(|slot| slot.borrow().clone())
}

thread_local! {
    /// 本次 `build_agent` 调用所属会话的 `preserve_thinking`（跨轮 CoT 保留）。
    /// 与 [`SESSION_REASONING_EFFORT`] 同一理由：工厂闭包签名带不进这个逐会话
    /// 参数，而 `build_agent_config` 只认 `RuntimeConfig`（其中没有该字段）。
    ///
    /// `Some(v)` = 本次会话显式指定的值（`thread/resume` 传
    /// `meta.preserve_thinking`）；`None` = 未指定（直接构建与测试），此时按默认
    /// `true` 解释，与 `AgentConfig::default()` 一致 ⇒ 零回归。
    static SESSION_PRESERVE_THINKING: std::cell::RefCell<Option<bool>> =
        const { std::cell::RefCell::new(None) };
}

/// 在调用工厂(`build_agent`)前设定本次会话的 `preserve_thinking`，供工厂解析。
///
/// 调用返回后恢复调用前的值，故嵌套调用不会泄漏。`thread/start` 传 `true`
/// （新会话默认保留）；`thread/resume` 传 `meta.preserve_thinking`——让重开的
/// 会话沿用落盘的开关，否则被关闭保留的会话重开后会静默恢复为「保留」，与
/// 磁盘和已广播给各端的值相悖。
pub(crate) fn with_session_preserve_thinking<T>(preserve: bool, f: impl FnOnce() -> T) -> T {
    SESSION_PRESERVE_THINKING.with(|slot| {
        let prev = slot.replace(Some(preserve));
        let out = f();
        slot.replace(prev);
        out
    })
}

/// 工厂内部读取当前会话的 `preserve_thinking`；未设定时按默认 `true` 解释。
fn current_session_preserve_thinking() -> bool {
    SESSION_PRESERVE_THINKING.with(|slot| slot.borrow().unwrap_or(true))
}

/// 生产环境的 agent 工厂:按 thread 的 cwd 覆盖 workdir 与 yolo 后引导一个 agent。
///
/// `catalog` 在进程启动时经 `load_catalog()` 读入一次并共享;每次调用再按本次会话
/// 的 `model_ref`(由 [`with_session_model_ref`] 设定)解析 provider/api_url/
/// api_key/model 四个字段,使**整条会话**跑在清单条目的 url/key/model 上,而非
/// 全局 `cfg`(解析不中则原样回退 `cfg`)。
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
    catalog: Arc<yi_agent_runtime::models::CatalogWatcher>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        // 本会话的关联根（逐调用传入，不共享）。空 = 单根会话，与改动前一致。
        // **不得**把根集留在共享 `cfg` 里：本工厂按项目目录构建、被同目录多个会话
        // 共享，那样会让 A 会话的关联根泄漏给同目录的其它会话。
        thread_cfg.sandbox_writable_roots = current_session_extra_roots();
        let model_ref = current_session_model_ref();
        // 取「当前」清单:运行期新增/修改的条目立刻可见(原先持启动快照)。
        let catalog = catalog.current();
        // 按本次会话解析 provider/api_url/api_key/model;不中则原样回退 cfg。
        let mut thread_cfg = yi_agent_runtime::models::resolve_effective(
            &thread_cfg,
            &catalog,
            model_ref.as_deref(),
        );
        // 会话 effort 覆盖（若有）优先于条目默认：`resolve_effective` 已带出条目
        // 的 `reasoning_effort`，仅当槽位**确有覆盖**（`Some(tier)`）时才覆盖它。
        // 槽位为 `None` 时一律保留条目默认 —— 这同时覆盖「新会话无覆盖」与「显式
        // 清除覆盖」：清除 = 回落条目默认（解析链第 2 级），而非不发该字段。二者
        // 必须一致，否则同一个 meta 在「活线程清除」（provider 自补条目档）与
        // 「resume 重建」两条路径上会有两种线上行为。
        if let Some(effort) = current_session_reasoning_effort() {
            thread_cfg.reasoning_effort = Some(effort);
        }
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &thread_cfg,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        // 主题工具必须进每个 thread 的工具集;委派随后可能用
        // `wrap_for_delegation` 换掉 registry,那条路径([`build_runtime_tooling`])
        // 同样注册。`Agent::new` 会顺带清掉权限检查器,故重建后必须重新装上
        // (见 [`rebuild_thread_agent_with_theme`],否则非 git 目录会退化成不弹审批)。
        Ok(rebuild_thread_agent_with_theme(
            built,
            session,
            theme.clone(),
        ))
    }
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

/// 生产启动期的一次性 `.env` → 清单迁移，供两个生产入口（`run` 与
/// `serve_stdio_with_relay`）共用。
///
/// 只在生产入口调用：测试用 `Harness` 注入隔离路径，绝不能在这里产生副作用。
/// `HOME` 缺失时 `resolve_global_env_path` 为 `None`，此时不迁移（避免在一个
/// 凭空拼出的路径上写文件）。清单路径与 `RuntimeAttachments` 用的是同一个
/// `models_path()`，保证迁移落点与运行期读点是同一个文件。
fn migrate_legacy_model_keys_on_startup() {
    if let Some(env_path) = yi_agent_runtime::config::resolve_global_env_path() {
        let outcome = yi_agent_runtime::migrate::migrate_model_keys(
            &env_path,
            &yi_agent_runtime::models::models_path(),
        );
        yi_agent_runtime::migrate::log_outcome(&outcome, &env_path);
    }
}

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

/// [`serve_scoped`] 的合并模式入口:额外把一条环回 ws 前端(以及可选的中继
/// 客户端)接到**同一个** `serve()` 上。
///
/// `client_scope` 恒为 stdio 侧的 `Admin`;ws 侧每连接注册为 `Control`。单测直接
/// 传一个已绑定的 `listener` 并令 `relay_url = None`——不引入对真中继的依赖,手机
/// 用共享 `pairing` 铸出的 token 直接连环回监听。
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn serve_scoped_with_loopback<R, W, F>(
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
        listener,
        relay_url,
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
        board_dir,
        models_path,
        onboarding_env_path,
        onboarding_preferences_path,
        resident_dir,
        skills_home,
        plugin_command,
        launcher: board_launcher,
        theme,
        git_diff,
        view_frames,
        watchman_install,
        watchman_uninstall,
        watchman_home,
        lead_inbox,
        lead_replies,
    } = attachments;
    // driver task 会 clone 该 sender 上报 turn 完成事件;主循环持有它,
    // 保证 `turn_rx` 不会提前关闭。
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);
    // detach 出去的 board/plugin handler 唯一需要回到主循环的信号：stdio
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
                        // `onboarding_env_path` / `onboarding_preferences_path` /
                        // `watchman_home` 四个相邻字段同为 `&Path`,位置一旦
                        // 写错照样编译通过、却会静默把机器级路径接错。
                        crate::rpc::AppState {
                            hub: &hub,
                            client: &client,
                            client_scope,
                            cfg: &cfg,
                            theme_handle: &theme_handle,
                            models_path: &models_path,
                            onboarding_env_path: &onboarding_env_path,
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
                            pending_lead_replies: &mut pending_lead_replies,
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
                    crate::rpc::Domain::Board | crate::rpc::Domain::Plugin => {
                        // 门禁（initialize / scope）已在上面同步做完；这里只把
                        // **处理** detach 出去。主循环是单 task，内联 await 会串行
                        // 堵住所有 thread 的 RPC；主 runtime 是多线程的，故
                        // `tokio::spawn` 让它在别的 worker 上跑、主循环立刻回到
                        // `select!`。（`spawn_blocking(..).await` 做不到：主 task
                        // 仍要 await 它返回。）
                        let state = std::sync::Arc::new(crate::rpc::BoardPluginState {
                            hub: std::sync::Arc::clone(&hub),
                            client: client.clone(),
                            board_dir: board_dir.clone(),
                            resident_dir: resident_dir.clone(),
                            plugin_command: plugin_command.clone(),
                            theme_handle: theme_handle.clone(),
                            watchman_home: watchman_home.clone(),
                            watchman_install: std::sync::Arc::clone(&watchman_install),
                            watchman_uninstall: std::sync::Arc::clone(&watchman_uninstall),
                            board_launcher: std::sync::Arc::clone(&board_launcher),
                            fatal_tx: fatal_tx.clone(),
                        });
                        let domain = crate::rpc::domain_for(method.as_str());
                        let req = req.clone();
                        let id = id.clone();
                        tokio::spawn(async move {
                            let result = match domain {
                                crate::rpc::Domain::Board => {
                                    crate::rpc::board::handle_shared(&state, &req, id.clone()).await
                                }
                                _ => {
                                    crate::rpc::plugin::handle_shared(&state, &req, id.clone()).await
                                }
                            };
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
                )
                .await;
            }
            _ = lead_ticker.tick() => {
                // 组长后台循环的兜底轮,照看板 tick 内联跑:启动一轮要拿
                // `&mut threads`(投递走 `deliver_to_lead`),而 `threads` 是主循环
                // 独占的局部量,`tokio::spawn` 会拆掉这条前提(spec §6.1)。
                //
                // 「算」与「投」分两步:`lead_tick_once` 只读 `threads` 并返回 owned
                // 的投递清单,返回时借用已归还,下面才借 `&mut`。投递失败不中断本轮,
                // 更不能影响主循环处理其它请求。
                for delivery in lead_tick_once(&mut threads) {
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
        }
    }

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

/// 把一个设备 id 映射到它当前的 ws `ClientId`——设备被撤销时据此摘除连接。
///
/// 这张表在 **ws 传输内部**维护(`ws.rs`):每个已认证的连接把自己
/// `device_id → ClientId` 登记进去,断连即摘除。主循环(本文件)不能直接持有
/// 它——`ws.rs` 依赖 `server.rs`,反向依赖会成环——故 ws 层在启动时把
/// `Arc<Mutex<HashMap<String, ClientId>>>` 交给本函数要读的进程级句柄
/// ([`install_device_registry`])。表为空(纯 stdio、无 ws,或尚未起 ws)时
/// 安全返回 `None`。
pub(crate) fn ws_client_for_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.get(device_id).cloned()
}

/// 取出并移除某设备 id 的连接映射(撤销时用)。
///
/// 与 [`ws_client_for_device`] 同一张表,但顺带摘掉条目:设备已撤销,它的
/// `device_id → ClientId` 映射不该留在全局表里等下一个同名设备(或迟到的清理)
/// 撞上。真正的连接关闭由调用方 `hub.unregister` + ws 读循环的 disconnected
/// tick 完成;这里只负责让主循环侧不再记得它。
pub(crate) fn forget_ws_device(device_id: &str) -> Option<crate::broadcast::ClientId> {
    let registry = device_registry().get()?;
    let mut guard = registry.lock().unwrap_or_else(|p| p.into_inner());
    guard.remove(device_id)
}

/// 进程级「设备 id → ws `ClientId`」句柄。
///
/// 用 `OnceLock` 而非 `RuntimeAttachments` 字段:它天然是**每进程一张表**
/// (一个 app-server 进程只有一套 ws 连接),且只在 ws 传输启动时被设置一次。
/// ws 起不来时它保持未设置,`device/revoke` 于是退化为「只改设备表、不摘连接」,
/// 在纯 stdio 场景下本就没有 ws 连接可摘。
static DEVICE_REGISTRY: OnceLock<WsDeviceRegistry> = OnceLock::new();

pub(crate) type WsDeviceRegistry = Arc<StdMutex<HashMap<String, crate::broadcast::ClientId>>>;

fn device_registry() -> &'static OnceLock<WsDeviceRegistry> {
    &DEVICE_REGISTRY
}

/// 由 ws 传输在启动时登记它的「设备 id → `ClientId`」表(幂等;重复调用保留
/// 第一张)。返回该表,便于 ws 侧直接持有同一份 `Arc`。
pub(crate) fn install_device_registry(registry: WsDeviceRegistry) -> WsDeviceRegistry {
    device_registry().set(Arc::clone(&registry)).ok();
    device_registry().get().cloned().unwrap_or(registry)
}

pub(crate) fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}

pub(crate) fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}

pub(crate) async fn write_response(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(&resp)
        .map_err(|e| anyhow::anyhow!("failed to serialize response: {e}"))?;
    // 定向回复:`reply` 会 await 到入队成功,保留 stdio 改造前
    // 「写阻塞直到对端读」的语义。
    match hub.reply(client, frame).await {
        Ok(()) => Ok(()),
        // 写失败到**非本地**客户端(ws 手机)不得掀翻共享主循环:那是**多客户端
        // 共享**的一台服务器,一个被 `device/revoke` 踢掉、或被广播背压摘除
        // (慢消费者)的连接,不能让所有其它客户端(以及后续连接)一起陪葬。
        // 摘掉它、让本轮请求就此了结即可(它的读循环靠 `is_connected` 票 tick
        // 自行收尾)。
        Err(_) if *client != crate::broadcast::ClientId::local() => {
            tracing::warn!(
                client = client.as_str(),
                "app-server response write failed; dropping the ws client"
            );
            hub.unregister(client);
            Ok(())
        }
        // stdio 的 `local` 仍保留改造前的语义:出口泵已死,再写就是错,把错误
        // 抛出以结束会话(`driver_reports_finished_when_writer_fails` 钉死这条)。
        Err(_) => Err(anyhow::anyhow!("client {} disconnected", client.as_str())),
    }
}

pub(crate) async fn write_notification(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    // S2：列表层/全局帧恒推（无键）；内容层按 thread 过滤。无订阅者时等价于全广播。
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_for(key, frame);
    Ok(())
}

/// 与 [`write_notification`] 同款序列化与投递键，但走**带背压**的
/// [`crate::broadcast::Broadcaster::broadcast_await`]：本地 reliable 客户端队列
/// 满时等待而非丢帧。用于历史回放这类「必须完整送达」的批量下发。
pub(crate) async fn write_notification_await(
    hub: &crate::broadcast::Broadcaster,
    n: &Notification,
) -> anyhow::Result<()> {
    let frame = serde_json::to_value(NotificationEnvelope::new(n))
        .map_err(|e| anyhow::anyhow!("failed to serialize notification: {e}"))?;
    let key = match n.delivery() {
        crate::protocol::Delivery::Content => n.thread_key(),
        crate::protocol::Delivery::List | crate::protocol::Delivery::Global => None,
    };
    hub.broadcast_await(key, frame)
        .await
        .map_err(|_| anyhow::anyhow!("replay client closed"))?;
    Ok(())
}

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

/// 更新共享状态句柄并推送 `thread/status/updated`。
///
/// 加锁是同步的、不跨 `.await`；锁在写通知前即释放。锁中毒时沿用
/// `workspace_index.rs` 的恢复约定：取回内部值而非 panic(状态只是 UI 提示,
/// 不应因一次 panic 永久失效)。
async fn update_status(
    hub: &crate::broadcast::Broadcaster,
    handle: &std::sync::Mutex<ThreadStatus>,
    thread_id: &str,
    next: ThreadStatus,
) -> anyhow::Result<()> {
    *handle.lock().unwrap_or_else(|p| p.into_inner()) = next;
    write_notification(
        hub,
        &Notification::ThreadStatusUpdated {
            thread_id: thread_id.to_string(),
            status: next,
        },
    )
    .await
}

/// 读某 thread 的当前状态;不在内存(cold thread)一律 `Idle`。
fn thread_status(threads: &HashMap<String, ThreadSession>, thread_id: &str) -> ThreadStatus {
    threads
        .get(thread_id)
        .map(|s| *s.status.lock().unwrap_or_else(|p| p.into_inner()))
        .unwrap_or(ThreadStatus::Idle)
}

/// 跨工作目录收集已置顶的 thread meta，按置顶分区契约排序：
/// `pin_seq` 降序（越大越靠前），`updated_at` 降序，`thread_id` 升序。
pub(crate) fn collect_pinned(workspaces: &WorkspaceIndex) -> Vec<crate::thread_store::ThreadMeta> {
    let mut out = Vec::new();
    for dir in workspaces.list() {
        let path = Path::new(&dir);
        if !path.is_dir() {
            continue;
        }
        match crate::thread_store::ThreadStore::new(path).list() {
            Ok(metas) => out.extend(metas.into_iter().filter(|m| m.pin_seq.is_some())),
            Err(e) => eprintln!("[app-server] collect_pinned failed to list {dir}: {e}"),
        }
    }
    out.sort_by(|a, b| {
        b.pin_seq
            .cmp(&a.pin_seq)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
    out
}

/// 把 worktree 判定渲染成 `thread/delete` 的 `needs_confirmation.worktree` 字段。
///
/// `None` 表示"不适用/无需呈现"（普通会话、或不回收）——此时响应不带该字段，
/// 普通会话的既有响应因此零变化。
pub(crate) fn worktree_json(
    reclaim: &crate::worktree_reclaim::WorktreeReclaim,
) -> Option<serde_json::Value> {
    use crate::worktree_reclaim::WorktreeReclaim;
    match reclaim {
        WorktreeReclaim::None => None,
        WorktreeReclaim::Delete { path } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": "干净且源分支已并入，删除不会丢失工作",
        })),
        WorktreeReclaim::DestructiveDelete { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "remove",
            "reason": reason,
        })),
        WorktreeReclaim::Keep { path, reason } => Some(json!({
            "path": path.to_string_lossy(),
            "action": "keep",
            "reason": reason,
        })),
    }
}

/// 客户端在握手第一帧声明的、某会话的缓存状态。
///
/// 生产调用点：`initialize` 分支读 `params.resumeHints` → `parse_resume_hints`
/// → `plan_sync`（Task 6 落地）。
#[derive(Debug, Clone)]
pub(crate) struct SyncHint {
    pub(crate) thread_id: String,
    pub(crate) content_epoch: u64,
    pub(crate) last_item_id: Option<String>,
}

/// 服务端对该 hint 的裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncAction {
    /// 缓存内容必须整段作废，客户端须走全量 `thread/resume`。
    Full,
    /// 缓存内容**仍然有效**。注意这不是"客户端已是最新"——落后是正常的并存
    /// 形状，补齐由常规的 resume / readItems / 订阅流承担（spec §4.2）。
    None,
    /// 该会话已不存在，客户端须丢弃本地条目与视图。
    Drop,
}

/// 把 hints 与服务端现状裁成一份 `syncPlan`。纯函数：`server(id)` 由调用方
/// 提供该会话的 `(content_epoch, 锚点是否仍在日志中)`，`None` 表示不存在。
///
/// 判定按优先级短路（spec §4.2）：不存在 → drop；epoch 不符 → full；
/// 锚点缺失 → full；否则 none。返回的末位是服务端 epoch（`drop` 时 0），
/// 免得调用方为写 `syncPlan` 的 `epoch` 字段再查一次库。
pub(crate) fn plan_sync(
    hints: &[SyncHint],
    server: &dyn Fn(&str) -> Option<(u64, bool)>,
) -> Vec<(String, SyncAction, &'static str, u64)> {
    let mut out = Vec::with_capacity(hints.len());
    for h in hints {
        let (action, reason, epoch) = match server(&h.thread_id) {
            None => (SyncAction::Drop, "deleted", 0),
            Some((epoch, anchor_present)) => {
                if epoch != h.content_epoch {
                    (SyncAction::Full, "epoch_changed", epoch)
                } else if h.last_item_id.is_some() && !anchor_present {
                    (SyncAction::Full, "anchor_missing", epoch)
                } else {
                    (SyncAction::None, "match", epoch)
                }
            }
        };
        out.push((h.thread_id.clone(), action, reason, epoch));
    }
    out
}

/// 解析 `initialize` 的 `resumeHints`。上限 20 条（按 `cachedAt` 降序取，缺
/// `cachedAt` 视为 0），超出截断——声明必须是"非常小的数据量"（spec §4.1）。
/// 任何畸形条目静默跳过：声明是优化，不该让握手失败。
pub(crate) fn parse_resume_hints(raw: Option<&serde_json::Value>) -> Vec<SyncHint> {
    const MAX_HINTS: usize = 20;
    let Some(items) = raw.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut hints: Vec<(i64, SyncHint)> = items
        .iter()
        .filter_map(|it| {
            let thread_id = it.get("threadId")?.as_str()?.to_string();
            let content_epoch = it.get("contentEpoch").and_then(|v| v.as_u64()).unwrap_or(0);
            let last_item_id = it
                .get("lastItemId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let cached_at = it.get("cachedAt").and_then(|v| v.as_i64()).unwrap_or(0);
            Some((
                cached_at,
                SyncHint {
                    thread_id,
                    content_epoch,
                    last_item_id,
                },
            ))
        })
        .collect();
    // 降序：cachedAt 越大越新，优先保留。
    hints.sort_by_key(|a| std::cmp::Reverse(a.0));
    hints.truncate(MAX_HINTS);
    hints.into_iter().map(|(_, h)| h).collect()
}

/// 一条落盘记录里的最后一条 item id（`items` 的末项）。
fn record_last_item_id(record: &crate::thread_store::TurnLine) -> Option<String> {
    match record {
        crate::thread_store::TurnLine::Turn { items, .. } => {
            items.last().and_then(item_id).map(str::to_string)
        }
    }
}

/// 把 thread meta 渲染成 wire 上的 ThreadSummary（含 `pinned`）。
pub(crate) fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    let mut value = json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "model_ref": m.model_ref,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "title": m.title,
        "permission_mode": m.permission_mode,
        "pinned": m.pin_seq.is_some(),
        "driver_project": m.driver_project,
        "driver_ref": m.driver_ref,
        "tag": m.tag,
        "status": thread_status(threads, &m.thread_id),
        "content_epoch": m.content_epoch,
        "last_item_id": m.last_item_id,
    });
    // 只在**合法**组长时发送该 key:普通会话多一个 `"role": null` 会让旧客户端
    // 的字节比对与快照测试失效,而我们只需要一个新值。卡片 worktree 里的
    // `role:"lead"` 是误落盘的假组长(那个目录不是项目),绝不能经这条 wire 暴露,
    // 否则侧栏会为它渲染出一条「组长」行。
    if crate::thread_store::is_lead_meta(m) {
        value["role"] = json!("lead");
    }
    value
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

/// wire 上的决定标签,与 `parse_client_decision` 接受的取值互逆。
///
/// `AlwaysAllowPrefix` 丢掉前缀:本标签只回答"是谁、做了什么类型的决定",
/// 不含策略细节;需要前缀的客户端可保留自己的弹窗上下文。
fn decision_label(decision: &Decision) -> String {
    match decision {
        Decision::AllowOnce => "allow_once",
        Decision::AlwaysAllowTool => "always_allow_tool",
        Decision::AlwaysAllowPrefix(_) => "always_allow_prefix",
        Decision::Deny => "deny",
    }
    .to_string()
}

/// 解析客户端的权限决定;任何未知/畸形取值一律按 Deny 处理(fail-safe)。
fn parse_client_decision(result: Option<&serde_json::Value>) -> Decision {
    let Some(result) = result else {
        return Decision::Deny;
    };
    match result.get("decision").and_then(|d| d.as_str()) {
        Some("allow_once") => Decision::AllowOnce,
        Some("always_allow_tool") => Decision::AlwaysAllowTool,
        Some("always_allow_prefix") => match result.get("prefix").and_then(|p| p.as_str()) {
            Some(p) => Decision::AlwaysAllowPrefix(p.to_string()),
            None => Decision::Deny,
        },
        _ => Decision::Deny,
    }
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

/// 组装当前 turn 的 checkpoint：已 finalize 的 item（含开头的用户提问）+ 当前
/// 上下文快照。进行中的流式文本不在 `completed_items` 里，故天然不入内。
fn build_partial(
    turn_id: &str,
    user_prompt: &str,
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    completed_items: &[crate::protocol::Item],
    messages: Vec<yi_agent_core::Message>,
    usage: Option<crate::thread_store::TurnUsage>,
) -> crate::thread_store::PartialTurn {
    let mut items = Vec::with_capacity(completed_items.len() + 1);
    items.push(opening_user_item(
        turn_id,
        user_prompt,
        user_attachments.to_vec(),
        user_image_refs.to_vec(),
    ));
    items.extend(completed_items.iter().cloned());
    crate::thread_store::PartialTurn {
        turn_id: turn_id.to_string(),
        items,
        messages,
        usage,
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
    user_prompt: Option<&str>,
    user_attachments: &[crate::protocol::Attachment],
    user_image_refs: &[crate::protocol::ImageRef],
    // 本轮若替某条阻塞式 `ask_lead` 跑,这里是它的 request_id;随 Finished 上报,
    // 主循环据此只回填那一个等待方(见 `settle_pending_lead_replies`)。
    lead_request_id: Option<String>,
    agent: &yi_agent_core::Agent,
    completed_items: Vec<crate::protocol::Item>,
    last_usage: Option<crate::thread_store::TurnUsage>,
    store: &crate::thread_store::ThreadStore,
    hub: &crate::broadcast::Broadcaster,
    turn_tx: &mpsc::Sender<TurnEvent>,
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
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
        messages: persisted_messages,
    };
    // append 失败则跳过 meta 维护：避免出现"幽灵" thread。
    let mut first_turn = false;
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
            match store.touch(thread_id, Some(prompt)) {
                // 本次刚写入兜底截断标题 = 该 thread 的首轮。
                Ok(true) => first_turn = true,
                Ok(false) => {}
                Err(e) => eprintln!("[app-server] failed to update meta for {thread_id}: {e}"),
            }
        }
    }

    // 先广播「本轮已落盘」，再做标题生成：标题是锦上添花，绝不能拖住 turn 收尾。
    // delete/interrupt 路径正是靠这枚 Finished 判定落盘完成，而标题生成最长可等
    // TITLE_TIMEOUT——若排在它前面，会让收尾凭空多出 5 秒延迟。
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

    // 仅首轮生成标题：拿 touch 写入的兜底截断值作 CAS 凭据，尝试用 LLM 标题覆盖。
    // 任何失败/超时/空结果都静默保留兜底标题。clear / compact 传 `user_prompt = None`，
    // 天然不会走到这里。
    if first_turn {
        if let Some(prompt) = user_prompt {
            let fallback = crate::thread_store::title_from(prompt);
            match tokio::time::timeout(
                TITLE_TIMEOUT,
                yi_agent_core::title::generate_title(
                    provider,
                    config,
                    prompt,
                    assistant_text.as_deref(),
                ),
            )
            .await
            {
                Ok(Ok(Some(title))) => {
                    // 仅在 CAS **确实替换**（`Ok(true)`）后广播：`Ok(false)` 表示标题
                    // 已被手动改名抢占或 thread 不存在，磁盘上不是这个值，广播它会让
                    // 客户端显示一个不存在的标题。广播失败只记 stderr——标题是锦上
                    // 添花，绝不能拖住 turn 收尾。
                    match store.set_title_if_unchanged(thread_id, &fallback, &title) {
                        Ok(true) => {
                            if let Err(e) = write_notification(
                                hub,
                                &Notification::TitleChanged {
                                    thread_id: thread_id.to_string(),
                                    title,
                                },
                            )
                            .await
                            {
                                eprintln!(
                                    "[app-server] failed to broadcast title change {thread_id}: {e}"
                                );
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            eprintln!("[app-server] failed to set LLM title {thread_id}: {e}")
                        }
                    }
                }
                // 模型产出无法作为标题：保留兜底标题。
                Ok(Ok(None)) => {}
                Ok(Err(e)) => {
                    tracing::info!(%e, %thread_id, "title generation failed; keeping fallback");
                }
                Err(_) => {
                    tracing::info!(%thread_id, "title generation timed out; keeping fallback");
                }
            }
        }
    }
}

/// 从本轮 items 里取第一条助手文本：作为标题生成材料的"助手回复"侧。
///
/// 空文本的 `AgentMessage` 视为不存在：`build_title_material` 对 `Some("")` 会渲染
/// 一个悬空的"助手回复："标签，反而污染标题模型的输入。
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
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
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
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
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
                None,
                &[],
                &[],
                None,
                &agent,
                Vec::new(),
                None,
                store,
                hub,
                turn_tx,
                provider,
                config,
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
) {
    // 每个 thread 一个 translator:item id 带 turn_id 前缀(`item-<turn_id>-<n>`),
    // 故即便 resume 后计数器归 1,新 item 也不会与回放的历史 id 冲突。
    let mut translator = Translator::new(thread_id.clone());
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
                agent = apply_session_command(
                    agent, command, &provider, &config, &store, &thread_id, &hub, &turn_tx,
                    &status,
                )
                .await;
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
                &turn_id,
                &user_prompt,
                &user_attachments,
                &user_image_refs,
                &[],
                agent.session().messages().to_vec(),
                None,
            ),
        );

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
                            &turn_id,
                            &user_prompt,
                            &user_attachments,
                            &user_image_refs,
                            &completed_items,
                            agent.session().messages().to_vec(),
                            last_usage.clone(),
                        ),
                    );
                    checkpoint_dirty = false;
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
            agent = apply_session_command(
                agent, command, &provider, &config, &store, &thread_id, &hub, &turn_tx, &status,
            )
            .await;
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
        persist_and_finish_turn(
            &thread_id,
            Some(&turn_id),
            Some(&user_prompt),
            &user_attachments,
            &user_image_refs,
            lead_request_id,
            &agent,
            std::mem::take(&mut completed_items),
            last_usage.take(),
            &store,
            &hub,
            &turn_tx,
            &provider,
            &config,
            &status,
        )
        .await;

        // 收尾已把整轮 append 进主 jsonl；checkpoint 的使命结束。
        if let Err(e) = store.clear_partial(&thread_id) {
            eprintln!("[app-server] failed to clear turn checkpoint ({thread_id}): {e}");
        }
    }
}

/// 在全局索引的目录里定位 `thread_id` 所属目录。
fn find_thread_dir(workspaces: &WorkspaceIndex, thread_id: &str) -> Option<PathBuf> {
    workspaces
        .list()
        .into_iter()
        .map(PathBuf::from)
        .find(|d| crate::thread_store::ThreadStore::new(d).exists(thread_id))
}

/// 按 thread 定位其 store;索引找不到时回退 `cfg.workdir`。
fn store_for(
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    let dir = find_thread_dir(workspaces, thread_id).unwrap_or_else(|| cfg.workdir.clone());
    Arc::new(crate::thread_store::ThreadStore::new(&dir))
}

/// 取 thread 的 store:内存中的 thread 用其权威实例(与 driver 共享 lock);
/// 冷 thread 按全局索引定位,索引找不到再回退 `cfg.workdir`。
pub(crate) fn store_lookup(
    threads: &HashMap<String, ThreadSession>,
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    match threads.get(thread_id) {
        Some(s) => Arc::clone(&s.store),
        None => store_for(workspaces, cfg, thread_id),
    }
}

/// 解析某个 thread 的工作区 cwd(绝对路径)。
///
/// 内存中的会话最权威:直接取 `ThreadSession.cwd`(`thread/start` 已 canonicalize)。
/// 冷 thread(不在内存)回退 `store_lookup`——它按全局索引定位该 thread 的 store,
/// 或落回 `cfg.workdir`;store 的 root 恒为 `<cwd>/.yi-agent/threads`,上溯两级即得
/// 工作区(与 `thread/delete` 分支推导附件目录的写法同源)。
///
/// 找不到任何见证(索引里既没有该 thread 的目录,`cfg.workdir` 也不含它)时返回
/// `None`,由调用方按「未知 thread」拒绝。
pub(crate) fn thread_cwd(
    threads: &HashMap<String, ThreadSession>,
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Option<PathBuf> {
    if let Some(session) = threads.get(thread_id) {
        return Some(PathBuf::from(&session.cwd));
    }
    let store = store_lookup(threads, workspaces, cfg, thread_id);
    if store.exists(thread_id) {
        return store
            .root()
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf);
    }
    None
}

/// `image/read` 回传的 MIME:由 `guess_format` 猜出的格式决定。
///
/// 只认四种与 `image_prep`(`png/jpeg/gif/webp`)对齐的格式;`guess_format` 虽会
/// 认出 TIFF/BMP 等魔数,但那些解码器未启用,统一按 `application/octet-stream`
/// 报出(客户端据此可判「不是我能渲染的图」)。
pub(crate) fn image_media_type(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        _ => "application/octet-stream",
    }
}

/// 一条 `Item` 的稳定 id（用于 `thread/readItems` 的 `afterItemId` 切片与前端去重）。
pub(crate) fn item_id(item: &crate::protocol::Item) -> Option<&str> {
    match item {
        crate::protocol::Item::UserMessage { id, .. }
        | crate::protocol::Item::AgentMessage { id, .. }
        | crate::protocol::Item::ToolCall { id, .. }
        | crate::protocol::Item::UserInterjection { id, .. }
        | crate::protocol::Item::Thinking { id, .. } => Some(id),
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
            // 该 (项目, 插件) 的驱动记账跨 tick 存活:`results` 要在下一拍回报,
            // `threads` 要让插件看见自己起过的会话。
            let state = drivers
                .entry((project.clone(), plugin.clone()))
                .or_default();
            let mut launcher = ServeLauncher {
                threads,
                pending_activation,
                process_watches,
                uploads,
                runtimes,
                thread_roots,
                models_path,
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
            } => self
                // 两个归属值都给 `None`:`driver/next` 的 `launch` 应答按 spec §4.3
                // 只带 `workdir`/`title`/`objective`,不带任何归属信息。`driver_ref`
                // 因此恒为 `None`(缺口见 plan 归 G2 收口)。
                .launch_inner(workdir, title, objective, None, None)
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
    /// `driver_project` / `driver_ref` 是落进 meta 的两个通用归属值。通用驱动动作
    /// (`driver/next`)按 spec §4.3 只带 `workdir`/`title`/`objective`,不带任何归属
    /// 信息,故 `launch` 两个都传 `None`:宿主不认识卡片,`driver_ref` 便无从填起
    /// (缺口见 plan 归 G2 收口)。
    async fn launch_inner(
        &mut self,
        workdir: &str,
        title: &str,
        objective: &str,
        driver_project: Option<&str>,
        driver_ref: Option<&str>,
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
        // 新建路径：起点就是刚刚取的 HEAD。
        anchor_sha.clone(),
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
        ..
    } = activation.built;
    pending_activation.insert(thread_id.clone(), activation.runtime);

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

/// 解析 `thread/start` 的目标目录:显式 `params.cwd` 优先,缺省用 `cfg.workdir`。
///
/// 显式 `cwd` canonicalize + 校验是目录;失败写 `-32602` 并返回 `Ok(None)`
/// (调用方 continue)。缺省时沿用 `cfg.workdir` 原样,不 canonicalize:保持旧的
/// 单目录行为,避免 macOS `/var` → `/private/var` 之类改写破坏既有路径语义。
pub(crate) async fn resolve_thread_cwd(
    params: &serde_json::Value,
    cfg: &RuntimeConfig,
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => match std::fs::canonicalize(s) {
            Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
            _ => {
                write_response(
                    hub,
                    client,
                    err_response(id, RpcError::invalid_params("cwd is not a valid directory")),
                )
                .await?;
                Ok(None)
            }
        },
        _ => Ok(Some(cfg.workdir.display().to_string())),
    }
}

/// 从 params 提取 `threadId`;缺失时写 `-32602` 并返回 `Ok(None)`。
pub(crate) async fn require_thread_id(
    hub: &crate::broadcast::Broadcaster,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("threadId").and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(s.to_string())),
        None => {
            write_response(
                hub,
                client,
                err_response(id, RpcError::invalid_params("missing threadId")),
            )
            .await?;
            Ok(None)
        }
    }
}

/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
///
/// 非 text 的 block（含 attachment）由 `crate::attachments::parse_input` 另行处理；
/// 这里只看文本，空文本返回 `None`。**注意**：附件-only 的消息在这里是 `None`，
/// 是否放行由调用方结合附件列表决定（见 `prepare_turn_core`）。
pub(crate) fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let (text, _paths) = crate::attachments::parse_input(params);
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The `board/*` RPCs, driven through the real stdio loop.
///
/// They are side-effecting commands (a temp dir on disk) rather than pure
/// functions, so the seam under test is the dispatch itself: request in, frame
/// out. The harness points the registry at a temp dir, so these never touch the
/// developer's real `~/.yi-agent`.
#[cfg(test)]
mod board_rpc_tests {
    use super::*;
    // 请求/应答泵住在 `test_support`（与 `resident_rpc_tests` 共用同一份），
    // 别名保持下面各调用点原样。
    use crate::test_support::rpc_over_stdio as rpc;

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_then_remove_lists_the_project_between_the_two() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();
        let project_path = project
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project_path }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");
        assert_eq!(created["result"]["registered"], true, "{created}");

        let listed = rpc(&mut h, 3, "board/list", json!({})).await;
        let boards = listed["result"]["boards"].as_array().unwrap();
        assert_eq!(boards.len(), 1, "登记表必须恰好一条:{listed}");
        assert_eq!(boards[0]["project"], project_path);
        assert_eq!(boards[0]["status"]["registered"], true, "{listed}");

        let removed = rpc(
            &mut h,
            4,
            "board/remove",
            json!({ "project": project_path }),
        )
        .await;
        assert!(removed.get("error").is_none(), "{removed}");

        let listed = rpc(&mut h, 5, "board/list", json!({})).await;
        assert_eq!(
            listed["result"]["boards"].as_array().unwrap().len(),
            0,
            "移除后不得再出现在登记表:{listed}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_board_created_through_the_rpc_is_written_to_the_injected_registry() {
        // 侧栏按登记表决定给哪些项目画看板条目,所以「写进注入的那个目录」
        // 本身就是契约:写错地方等于点创建没反应。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();
        let project_path = project.path().canonicalize().unwrap();

        rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project_path.to_string_lossy() }),
        )
        .await;

        let boards = yi_agent_boards::registry::list(h.board_dir.path()).unwrap();
        assert_eq!(boards.len(), 1, "登记表必须落在注入的目录里");
        assert_eq!(boards[0].project, project_path);
        h.shutdown().await;
    }

    /// 开关为开（缺省）时，首次 `board/create` 确保值守已装：装的是当前 exe、
    /// 针对注入的 home。
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_board_ensures_the_watchman_is_installed() {
        // 自带 workdir，避免与其它测试共享的默认目录里的偏好互相干扰。
        let workdir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = workdir.path().to_path_buf();
        let mut h = tests::Harness::with_cfg(cfg).await;
        tests::initialize(&mut h).await;
        let calls = Arc::clone(&h.watchman_calls);
        let home = h._watchman_home.path().to_path_buf();
        let project = tempfile::TempDir::new().unwrap();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");

        let recorded = calls.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "开关为开时创建看板要装一次值守: {recorded:?}"
        );
        assert!(recorded[0].0, "是安装而不是卸载");
        assert_eq!(recorded[0].1, home, "必须针对注入的 home");
        h.shutdown().await;
    }

    /// 开关为关时 `board/create` 不得装值守：用户明确关掉了开机自启。
    #[tokio::test(flavor = "multi_thread")]
    async fn creating_a_board_leaves_the_watchman_alone_when_the_switch_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = tests::Harness::with_cfg(cfg).await;
        tests::initialize(&mut h).await;
        let calls = Arc::clone(&h.watchman_calls);

        let off = rpc(
            &mut h,
            2,
            "ui/settings/write",
            json!({ "board_watchman_enabled": false }),
        )
        .await;
        assert_eq!(off["result"]["ok"], true, "{off}");

        let project = tempfile::TempDir::new().unwrap();
        let created = rpc(
            &mut h,
            3,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(created.get("error").is_none(), "{created}");

        let recorded = calls.lock().unwrap().clone();
        assert!(
            recorded.iter().all(|(installed, _)| !installed),
            "关掉后只应有卸载，不应有安装: {recorded:?}"
        );
        h.shutdown().await;
    }

    /// 值守安装失败不得让 `board/create` 失败：看板本身已经建好。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_watchman_install_does_not_fail_board_creation() {
        let workdir = tempfile::TempDir::new().unwrap();
        let mut cfg = tests::default_config();
        cfg.workdir = workdir.path().to_path_buf();
        let mut h = tests::Harness::with_watchman(
            cfg,
            Arc::new(|_exe: &Path, _home: &Path| Err("launchctl is unavailable".to_string())),
            Arc::new(|_home: &Path| Ok(())),
        )
        .await;
        tests::initialize(&mut h).await;
        let project = tempfile::TempDir::new().unwrap();

        let created = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": project.path().canonicalize().unwrap().to_string_lossy() }),
        )
        .await;
        assert!(
            created.get("error").is_none(),
            "装值守失败不该拖垮创建:{created}"
        );
        assert_eq!(created["result"]["registered"], true, "{created}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_list_of_an_empty_registry_answers_with_no_boards() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let listed = rpc(&mut h, 2, "board/list", json!({})).await;
        assert_eq!(listed["result"]["boards"], json!([]));
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_without_a_project_is_invalid_params() {
        // `project` 是必填参数。空串会被当成当前目录，凭空给调用方开一块
        // 谁都认不出的看板；缺参数必须是 invalid_params，与其它 RPC 一致。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let response = rpc(&mut h, 2, "board/create", json!({})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        assert!(
            yi_agent_boards::registry::list(h.board_dir.path())
                .unwrap()
                .is_empty(),
            "缺参数的请求不得登记任何东西"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_remove_without_a_project_is_invalid_params() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let response = rpc(&mut h, 2, "board/remove", json!({})).await;
        assert_eq!(response["error"]["code"], -32602, "{response}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn board_create_for_a_missing_directory_reports_an_error_and_registers_nothing() {
        // 侧栏按登记表画条目，所以「拒绝」必须同时意味着「没登记」：否则
        // 点一次创建会多出一条点开什么都没有的看板。
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        let response = rpc(
            &mut h,
            2,
            "board/create",
            json!({ "project": missing.to_string_lossy() }),
        )
        .await;
        assert!(response.get("error").is_some(), "{response}");
        assert!(
            yi_agent_boards::registry::list(h.board_dir.path())
                .unwrap()
                .is_empty(),
            "被拒绝的创建不得登记"
        );
        h.shutdown().await;
    }
}

/// `resident/*`（通用常驻登记）与通用转发的 stdio 用例。
///
/// 与 `board/*` 无关：数据源是通用常驻登记（harness 已把它指向隔离 tempdir），
/// 故这些用例**不随看板域退役**——它们钉的正是「宿主只提供通用面」这件事。
#[cfg(test)]
mod resident_rpc_tests {
    use super::*;
    // 与 `board_rpc_tests` 共用同一份泵（原先两边各抄了一份）。
    use crate::test_support::rpc_over_stdio as rpc;

    /// `resident/list` 走完整 dispatch（域路由 + handler + 线格式），插进
    /// **已有**的登记目录，故 harness 的隔离 tempdir 就是数据源。
    #[tokio::test(flavor = "multi_thread")]
    async fn resident_list_answers_over_the_wire_from_the_injected_registry() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        yi_agent_store::resident::require(
            h.resident_dir.path(),
            Path::new("/registered/project"),
            "some-plugin",
        )
        .unwrap();

        let response = rpc(&mut h, 2, "resident/list", json!({})).await;

        assert_eq!(
            response["result"]["projects"][0]["project"], "/registered/project",
            "{response}"
        );
        assert_eq!(
            response["result"]["projects"][0]["required_by"][0], "some-plugin",
            "{response}"
        );
        h.shutdown().await;
    }

    /// 没有登记表时报空表（不是错误）：侧栏据此不画条目，与「登记表还没有」
    /// 是同一件事。
    #[tokio::test(flavor = "multi_thread")]
    async fn resident_list_is_empty_and_not_an_error_before_anything_registers() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;

        let response = rpc(&mut h, 2, "resident/list", json!({})).await;

        assert_eq!(response["result"], json!({ "projects": [] }), "{response}");
        assert!(response.get("error").is_none(), "{response}");
        h.shutdown().await;
    }

    /// 未登记的项目照样能被转发：通用通道不看登记表（门控已摘）。
    #[tokio::test(flavor = "multi_thread")]
    async fn plugin_query_answers_for_a_project_that_never_registered() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = h.board_dir.path().join("never-registered");
        std::fs::create_dir_all(&project).unwrap();

        let response = rpc(
            &mut h,
            2,
            "plugin/query",
            json!({
                "project": project.to_string_lossy(),
                "plugin": "superpowers-kanban",
                "method": "list",
                "params": {},
            }),
        )
        .await;

        // 没有 daemon 在答，于是失败——但失败的理由是「拨不到」，不是「没有看板」。
        assert_eq!(
            response["error"]["data"]["code"], "daemon_unavailable",
            "{response}"
        );
        h.shutdown().await;
    }
}

#[cfg(test)]
mod plugin_query_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// A fake daemon bound to `dir`'s runtime socket that records the request it
    /// received and replies with one frame carrying `result`, so the test can
    /// prove the host forwards the plugin name, method and params verbatim
    /// without interpreting any of them.
    pub(super) fn fake_daemon(
        dir: &Path,
        result: serde_json::Value,
    ) -> (
        PathBuf,
        Arc<StdMutex<serde_json::Value>>,
        std::thread::JoinHandle<()>,
    ) {
        // The exact path `plugin_query` dials, via the same helper the
        // implementation uses: a fake bound anywhere else would let a
        // wrong-path implementation pass.
        let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(dir);
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
        let seen = Arc::new(StdMutex::new(serde_json::Value::Null));
        let captured = Arc::clone(&seen);
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let request: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                *captured.lock().unwrap() = request.clone();
                // Echo the request id: the client rejects a response for another
                // request, so a fake daemon that hardcodes one would look broken.
                let reply = json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": result,
                });
                let mut stream = stream;
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        (socket, seen, handle)
    }

    /// 一个存在的项目目录。
    ///
    /// 曾经这里还会按需写一份 `boards.json`（`project_and_registry`），用来
    /// 表达「这个项目登记没登记」——门控在场时那决定了 `plugin_query` 答不答。
    /// 门控摘掉后登记表不再参与通用转发，故这个 helper 只剩「建一个目录」，
    /// 名字也随之改回它真正做的事。
    fn project_dir(dir: &Path) -> PathBuf {
        let project = dir.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        project
    }

    /// 通用转发不再按「这个项目有没有看板」拒答。
    ///
    /// 反向断言的是**具体错误码**，不是「有没有报错」：摘掉门控后，一个
    /// 未登记的项目会走到「拨 socket」，于是失败码是 `daemon_unavailable`
    /// （socket 不在），**不再**是 `board_not_created`。
    #[test]
    fn a_query_is_not_gated_on_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        // 不放 boards.json，也不要 nonce：这个项目从未登记过。
        let error = plugin_query(&project, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert_eq!(error.code, "daemon_unavailable", "{error}");
        assert_ne!(error.code, "board_not_created", "{error}");
    }

    /// 而且它**真的**拨到了那个项目的 socket：门控给的是「登记表说了算」，
    /// 摘掉之后「谁来答」只由 socket 是否应答决定。
    #[test]
    fn an_unregistered_project_still_reaches_its_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        // 没有 boards.json：登记表只会在**旧**实现里参与判断。
        let (_socket, seen, handle) = fake_daemon(
            &project,
            json!({ "type": "PluginResult", "value": { "cards": [] } }),
        );

        let value = plugin_query(&project, "list", "superpowers-kanban", json!({})).unwrap();

        assert_eq!(value["cards"], json!([]));
        let request = seen.lock().unwrap().clone();
        assert_eq!(request["command"]["method"], "list", "{request}");
        handle.join().unwrap();
    }

    #[test]
    fn a_query_reaches_the_daemon_with_the_plugin_and_method_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_dir(dir.path());
        let (_socket, seen, handle) = fake_daemon(
            &project,
            json!({ "type": "PluginResult", "value": { "cards": [] } }),
        );

        plugin_query(
            &project,
            "list",
            "superpowers-kanban",
            json!({ "verbose": true }),
        )
        .unwrap();
        handle.join().unwrap();

        let request = seen.lock().unwrap().clone();
        assert_eq!(request["command"]["type"], "PluginQuery");
        assert_eq!(request["command"]["plugin"], "superpowers-kanban");
        assert_eq!(request["command"]["method"], "list");
        assert_eq!(request["command"]["params"]["verbose"], true);
    }

    #[test]
    fn a_missing_plugin_name_is_refused_before_touching_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_dir(dir.path());
        let error = plugin_query(&project, "list", "", json!({})).unwrap_err();
        assert!(error.message.contains("plugin"), "{error}");
        assert_eq!(
            error.code, "invalid_params",
            "空 plugin 名与「没有看板」是两回事:{error}"
        );
    }

    #[test]
    fn an_unreachable_daemon_reports_that_the_plugin_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_dir(dir.path());
        let error = plugin_query(&project, "list", "superpowers-kanban", json!({})).unwrap_err();
        assert!(error.message.contains("daemon is unavailable"), "{error}");
        assert_eq!(error.code, "daemon_unavailable", "{error}");
    }

    #[test]
    fn a_daemon_that_does_not_supervise_the_plugin_reports_plugin_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_dir(dir.path());
        let (_socket, _seen, handle) = fake_daemon(
            &project,
            json!({
                "type": "Error",
                "code": "not_found",
                "message": "plugin superpowers-kanban is not available",
            }),
        );

        let error = plugin_query(&project, "list", "superpowers-kanban", json!({})).unwrap_err();
        handle.join().unwrap();

        assert_eq!(error.code, "plugin_unavailable", "{error}");
        assert!(error.message.contains("is not available"), "{error}");
        assert!(
            error.message.contains("plugin"),
            "既有措辞形状必须保留:{error}"
        );
    }

    /// A daemon that reached the plugin and relayed the plugin's OWN rejection
    /// (an invalid settings payload, say) is the plugin talking, not the plugin
    /// being gone. Reporting `plugin_unavailable` here makes the desktop say
    /// "插件未运行" and discards the plugin's reason; the code must be
    /// `plugin_rejected`. This is the app-server's own-workdir channel
    /// (`plugin_query_self`), which is what `plugin/settings/*` dispatch uses.
    #[test]
    fn a_plugin_side_rejection_is_not_reported_as_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("bad.json"),
            r#"{"name":"bad","command":"x","switch_key":"bad_on","query_socket":"{state_dir}/bad.sock"}"#,
        )
        .unwrap();
        let (_socket, _seen, handle) = fake_daemon(
            dir.path(),
            json!({
                "type": "Error",
                "code": "validation",
                "message": "interval_secs must be in [1, 3600], got 0",
            }),
        );

        let error = plugin_query_self(dir.path(), "bad", "settings.write", json!({})).unwrap_err();
        handle.join().unwrap();

        assert_eq!(error.code, "plugin_rejected", "{error}");
        assert_ne!(
            error.code, "plugin_unavailable",
            "别把拒绝说成没运行:{error}"
        );
        assert!(
            error.message.contains("interval_secs"),
            "插件的具体理由要带出来:{error}"
        );
    }

    /// The dispatch mapping the UI actually reads. The numeric code is the
    /// coarse fallback; `data.code` is the stable string, so both have to reach
    /// the wire together.
    ///
    /// 摘掉登记表门控后，未登记的项目会走到「拨 socket」而不是被拒，故这条
    /// 断言的是 `daemon_unavailable` + `-32021`：`board_not_created`（-32020）
    /// 已不存在——宿主不再有「这个项目没有看板」这个回答。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_rpc_reports_a_missing_daemon_in_data_for_an_unregistered_project() {
        let mut h = tests::Harness::new();
        tests::initialize(&mut h).await;
        let project = h.board_dir.path().join("unregistered");
        std::fs::create_dir_all(&project).unwrap();

        h.send(
            &json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "plugin/query",
                "params": {
                    "project": project.to_string_lossy(),
                    "plugin": "superpowers-kanban",
                    "method": "list",
                    "params": {},
                },
            })
            .to_string(),
        )
        .await;

        let value = loop {
            let value = h.read_value().await;
            if value.get("id") == Some(&json!(2)) {
                break value;
            }
        };
        assert_eq!(
            value["error"]["data"]["code"], "daemon_unavailable",
            "{value}"
        );
        assert_eq!(value["error"]["code"], -32021, "{value}");
        h.shutdown().await;
    }
}

#[cfg(test)]
mod plugins_tests {
    use super::{list_installed_plugins, plugin_scope};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    #[test]
    fn an_absent_supervisors_directory_lists_no_plugins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_installed_plugins(dir.path()).is_empty());
    }

    /// 设置页「技能」面板的两个 RPC：三层分组、正文读取、越界拒绝，且都必须落在
    /// `Observe` 档（iOS 设备默认 Control，加闸即手机端不可用）。
    mod skill_rpc_tests {
        // 本模块嵌在 `plugins_tests` 里，故 harness 要用 crate 全路径（`super::tests`
        // 会解析到 `plugins_tests::tests`，那里不存在）。
        use crate::protocol::Scope;
        use crate::server::tests::{Harness, initialize};

        /// 在隔离 home 里造一个用户级 skill，返回它的 SKILL.md 路径。
        fn seed_user_skill(h: &Harness, name: &str) -> std::path::PathBuf {
            let dir = h.skills_home().join(".yi-agent/skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("SKILL.md");
            std::fs::write(
                &path,
                format!("---\nname: {name}\ndescription: 测试用。\n---\n# {name}\n\n正文"),
            )
            .unwrap();
            path
        }

        /// 握手后再发请求并取回该 id 的响应——未 initialize 的客户端除
        /// `initialize` 外一律被拒，少了这一步所有用例都会红在 `-32010`。
        async fn call(h: &mut Harness, id: i64, method: &str, params: &str) -> serde_json::Value {
            h.send(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
            ))
            .await;
            // 按 id 收敛:进程守望者可能把通知插进同一路流。
            for _ in 0..16 {
                let v = h.read_value().await;
                if v.get("id") == Some(&serde_json::json!(id)) {
                    return v;
                }
            }
            panic!("no response for id {id}");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn mcp_list_is_available_at_observe_scope() {
            let mut h = (Harness::with_scope(Scope::Observe)).await;
            initialize(&mut h).await;
            let resp = call(&mut h, 1, "mcp/list", "{}").await;
            assert!(
                resp.get("error").is_none(),
                "observe must list mcp servers: {resp}"
            );
            assert!(resp["result"]["servers"].is_array());
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_list_is_available_at_observe_scope() {
            let mut h = (Harness::with_scope(Scope::Observe)).await;
            initialize(&mut h).await;
            let resp = call(&mut h, 1, "skill/list", "{}").await;
            assert!(
                resp.get("error").is_none(),
                "observe must list skills: {resp}"
            );
            assert!(resp["result"]["groups"].is_array());
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_is_available_at_observe_scope() {
            let mut h = (Harness::with_scope(Scope::Observe)).await;
            let path = seed_user_skill(&h, "user-skill");
            initialize(&mut h).await;
            let resp = call(
                &mut h,
                2,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
            )
            .await;
            assert!(
                resp.get("error").is_none(),
                "observe must read skills: {resp}"
            );
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_list_groups_the_installed_skills() {
            let mut h = Harness::new();
            seed_user_skill(&h, "user-skill");
            initialize(&mut h).await;
            let resp = call(&mut h, 3, "skill/list", "{}").await;
            let groups = resp["result"]["groups"].as_array().unwrap();
            let user = groups
                .iter()
                .find(|g| g["scope"] == "user")
                .expect("user group present");
            assert!(
                user["skills"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["name"] == "user-skill")
            );
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_returns_the_body() {
            let mut h = Harness::new();
            let path = seed_user_skill(&h, "user-skill");
            initialize(&mut h).await;
            let resp = call(
                &mut h,
                4,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
            )
            .await;
            assert!(resp.get("error").is_none(), "read failed: {resp}");
            assert_eq!(resp["result"]["body"], "# user-skill\n\n正文");
            h.shutdown().await;
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn skill_read_refuses_a_path_outside_the_roots() {
            let mut h = Harness::new();
            let outside = h.skills_home().join("outside/SKILL.md");
            std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
            std::fs::write(&outside, "---\nname: x\ndescription: y\n---\nsecret").unwrap();
            initialize(&mut h).await;
            let resp = call(
                &mut h,
                5,
                "skill/read",
                &format!(r#"{{"path":"{}"}}"#, outside.to_string_lossy()),
            )
            .await;
            assert_eq!(resp["error"]["code"], -32602, "{resp}");
            h.shutdown().await;
        }
    }

    /// 通用插件通道的作用域：给了非空 `project` 就用它，否则回落宿主 workdir。
    /// 看板按项目安装，桌面侧车 workdir 是 home，这一条是「看板跑着、设置页却
    /// 空着」能修好的前提。
    #[test]
    fn the_plugin_scope_follows_an_explicit_project_and_defaults_to_the_workdir() {
        let workdir = Path::new("/tmp/yi-agent-host-workdir");
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({})),
            workdir.to_path_buf()
        );
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "/p/proj" })),
            PathBuf::from("/p/proj")
        );
        // 空/全空白都不算「指定了项目」，回落到 workdir。
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "" })),
            workdir.to_path_buf()
        );
        assert_eq!(
            plugin_scope(workdir, &serde_json::json!({ "project": "   " })),
            workdir.to_path_buf()
        );
    }

    /// 端到端复现并锁死根因：宿主 workdir（= 桌面侧车的 home）下没有清单，所以
    /// 不带 `project` 的 `plugins/list` 必须为空；带上项目后才能看到该项目里装的
    /// 看板。修复前第二段断言为空——正是用户看到的现象。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_project_scoped_plugins_list_finds_a_project_installed_plugin() {
        use super::tests::{Harness, initialize, test_config};

        let home = tempfile::tempdir().unwrap();
        let mut cfg = test_config();
        cfg.workdir = home.path().to_path_buf();
        let mut h = Harness::with_cfg(cfg).await;
        initialize(&mut h).await;

        let project = tempfile::tempdir().unwrap();
        let sup = project.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();

        // 不带 project：读的是宿主 workdir，这里没有清单。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"plugins/list","params":{}}"#)
            .await;
        let bare = read_response(&mut h, 2).await;
        assert_eq!(
            bare["result"]["plugins"].as_array().unwrap().len(),
            0,
            "宿主 workdir 下不该凭空冒出插件：{bare}"
        );

        // 带 project：枚举该项目自己的清单，看到看板。
        h.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "plugins/list",
                "params": { "project": project.path().to_string_lossy() },
            })
            .to_string(),
        )
        .await;
        let scoped = read_response(&mut h, 3).await;
        let plugins = scoped["result"]["plugins"].as_array().unwrap();
        assert_eq!(plugins.len(), 1, "按项目作用域应看到看板：{scoped}");
        assert_eq!(plugins[0]["name"], "superpowers-kanban");
        assert_eq!(plugins[0]["queryable"], true);
        h.shutdown().await;
    }

    /// 与 `plugins/list` 同源的根因回归：`plugin/settings/read` 也必须按项目
    /// 作用域。不带 `project` 时，宿主在 home 下找不到清单，读成
    /// `plugin_not_installed`（正是用户看到「设置页里没有看板」的那一步）；带上
    /// 项目后才会拨该项目 daemon 的 socket 并拿回设置。
    #[tokio::test(flavor = "multi_thread")]
    async fn project_scoped_settings_read_reaches_the_project_daemon() {
        use super::plugin_query_tests::fake_daemon;
        use super::tests::{Harness, initialize, test_config};

        let home = tempfile::tempdir().unwrap();
        let mut cfg = test_config();
        cfg.workdir = home.path().to_path_buf();
        let mut h = Harness::with_cfg(cfg).await;
        initialize(&mut h).await;

        let project = tempfile::tempdir().unwrap();
        let sup = project.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        let (_socket, _seen, handle) = fake_daemon(
            project.path(),
            json!({
                "type": "PluginResult",
                "value": { "settings": { "default_max_tasks": 3, "interval_secs": 10, "windows": [] } },
            }),
        );

        // 不带 project：宿主 workdir（= home）下没有清单 → 读成没装。
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"plugin/settings/read","params":{"plugin":"superpowers-kanban"}}"#,
        )
        .await;
        let bare = read_response(&mut h, 2).await;
        assert_eq!(
            bare["error"]["data"]["code"], "plugin_not_installed",
            "home 作用域下不该声称看板已装：{bare}"
        );

        // 带 project：拨该项目的 daemon，拿回它自己的设置。
        h.send(
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "plugin/settings/read",
                "params": {
                    "plugin": "superpowers-kanban",
                    "project": project.path().to_string_lossy(),
                },
            })
            .to_string(),
        )
        .await;
        let scoped = read_response(&mut h, 3).await;
        assert_eq!(
            scoped["result"]["settings"]["interval_secs"], 10,
            "按项目作用域应读到设置：{scoped}"
        );
        handle.join().unwrap();
        h.shutdown().await;
    }

    /// 读到 id 匹配的响应，跳过中间的通知帧。
    async fn read_response(h: &mut super::tests::Harness, id: i64) -> serde_json::Value {
        loop {
            let value = h.read_value().await;
            if value.get("id") == Some(&serde_json::json!(id)) {
                return value;
            }
        }
    }

    #[test]
    fn a_manifest_with_a_query_socket_is_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        std::fs::write(
            sup.join("plain.json"),
            r#"{"name":"plain","command":"y","switch_key":"plain_on"}"#,
        )
        .unwrap();
        let mut plugins = list_installed_plugins(dir.path());
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "plain");
        assert!(!plugins[0].queryable);
        assert_eq!(plugins[1].name, "superpowers-kanban");
        assert!(plugins[1].queryable);
        assert_eq!(plugins[1].switch_key, "superpowers_kanban");
    }

    #[test]
    fn a_plugin_not_in_the_manifests_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            super::plugin_query_self(dir.path(), "ghost", "settings.read", serde_json::json!({}))
                .unwrap_err();
        assert_eq!(error.code, "plugin_not_installed", "{error}");
    }

    #[test]
    fn an_installed_plugin_without_a_query_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("quiet.json"),
            r#"{"name":"quiet","command":"y","switch_key":"quiet_on"}"#,
        )
        .unwrap();
        let error =
            super::plugin_query_self(dir.path(), "quiet", "settings.read", serde_json::json!({}))
                .unwrap_err();
        assert_eq!(error.code, "plugin_unavailable", "{error}");
    }
}

/// 通用会话驱动接进 `serve` 之后的集成测试:真 `ServeLauncher` 起**可见**会话、
/// `driver_tick` 按 (项目, 插件) 拉拍、`append` 复用已有会话。
///
/// 夹具尽量走**真实路径**:真 socket(按请求转发到绑定的假 daemon)、真
/// `ThreadStore`、真 `driver_tick`、真 launcher 起的会话。假的只有模型与「插件怎么
/// 回答 `driver/next`」。
#[cfg(test)]
mod session_driver_tests {
    use super::*;
    use crate::server::tests::{build_slow_agent, build_test_agent, test_config};
    use crate::test_support::isolated_models_path;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::session_driver::DriverState;

    /// 一台听在项目 runtime socket 上的假 daemon。
    ///
    /// 它做生产 daemon 对 `PluginQuery` 做的那一件事——把查询转给绑定
    /// (`on_query`)的看板回答——并把每次插件调用记录在案,供断言。其余请求
    /// (`Status` 探活、daemon attach 等)统一回 `internal`:这既是诚实的「我不是
    /// 真 daemon」,也让 `attach_cwd_runtime` 按生产里的降级路径失败,而不必起一
    /// 个真 daemon。
    struct FakeDaemon {
        socket: PathBuf,
        calls: Arc<StdMutex<Vec<serde_json::Value>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        _dir: tempfile::TempDir,
        project: PathBuf,
        #[allow(dead_code)]
        runtime_dir: PathBuf,
    }

    impl FakeDaemon {
        fn bind(
            on_query: impl Fn(&Path, &str, &serde_json::Value) -> Result<serde_json::Value, String>
            + Send
            + 'static,
        ) -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let project = dir.path().join("project");
            std::fs::create_dir_all(&project).unwrap();
            let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&project);
            std::fs::create_dir_all(&runtime_dir).unwrap();
            let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let query_project = project.clone();

            let calls: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let stop = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let request: serde_json::Value =
                        serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                    let command = &request["command"];
                    let method = command["method"].as_str().unwrap_or_default().to_string();
                    let value = if command["type"] == "PluginQuery" {
                        let plugin = command["plugin"].as_str().unwrap_or_default().to_string();
                        let params = command["params"].clone();
                        recorded.lock().unwrap().push(json!({
                            "plugin": plugin,
                            "method": method,
                            "params": params.clone(),
                        }));
                        // 宿主现在是通用的:任何声明了 `session_driver` 的插件都会被
                        // 问,故假 daemon 不再按插件名路由,只把请求转给绑定的回答。
                        match on_query(&query_project, &method, &params) {
                            Ok(result) => json!({ "type": "PluginResult", "value": result }),
                            // 被拒的查询在宿主侧呈现为传输错误(与「daemon 没有该插件的
                            // 路由」同型)——宿主据此放弃这一拍,不影响其它项目/插件。
                            Err(_) => {
                                json!({ "type": "Error", "code": "internal", "message": "refused" })
                            }
                        }
                    } else if command["type"] == "Status" {
                        // `is_running` 靠这个探活。诚实回答它,否则恢复路径会把
                        // 「我没有真 daemon」误读成「项目没有看板」。
                        json!({ "type": "Status", "high_water_event_id": 0 })
                    } else {
                        json!({ "type": "Error", "code": "internal", "message": "not a real daemon" })
                    };
                    let reply = json!({
                        "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                        "request_id": request["request_id"],
                        "result": value,
                    });
                    let mut stream = stream;
                    let _ = stream.write_all(reply.to_string().as_bytes());
                    let _ = stream.write_all(b"\n");
                }
            });
            Self {
                socket,
                calls,
                stop,
                handle: Some(handle),
                _dir: dir,
                project,
                runtime_dir,
            }
        }

        fn calls(&self) -> Vec<serde_json::Value> {
            self.calls.lock().unwrap().clone()
        }

        fn calls_to(&self, method: &str) -> Vec<serde_json::Value> {
            self.calls()
                .into_iter()
                .filter(|call| call["method"] == method)
                .collect()
        }
    }

    impl Drop for FakeDaemon {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::os::unix::net::UnixStream::connect(&self.socket);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// `ServeLauncher` 需要的全部主循环局部量,一次性备齐。
    struct Host {
        threads: HashMap<String, ThreadSession>,
        pending_activation: HashMap<String, Option<Arc<ThreadRoot>>>,
        process_watches: HashMap<String, ProcessWatch>,
        uploads: crate::image_upload::UploadRegistry,
        runtimes: ProjectRuntimes,
        thread_roots: ThreadRoots,
        cfg: RuntimeConfig,
        hub: Arc<crate::broadcast::Broadcaster>,
        turn_tx: mpsc::Sender<TurnEvent>,
        turn_rx: mpsc::Receiver<TurnEvent>,
        pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
        perm_seq: Arc<AtomicU64>,
        theme: crate::theme_tool::ThemeHandle,
        git_diff: crate::git_diff_tool::GitDiffHandle,
        workspaces: WorkspaceIndex,
        workdir: PathBuf,
        /// 委派链路要读的机器清单路径。指向本测试的隔离目录,避免读到开发者
        /// 真实的 `~/.yi-agent/models.json` 而改变解析出的 provider。
        models_path: PathBuf,
        /// 该 launcher 的组长信箱。测试不投递,故用一份空信箱即可。
        lead_inbox: crate::lead::LeadInbox,
        /// 该 launcher 的回信登记表,与信箱同一理由:测试不投递,空表即可。
        lead_replies: crate::lead::LeadReplies,
        _workdir_dir: tempfile::TempDir,
        _theme_dir: tempfile::TempDir,
        _workspace_dir: tempfile::TempDir,
    }

    impl Host {
        fn new() -> Self {
            let workdir_dir = tempfile::TempDir::new().unwrap();
            let workdir = workdir_dir.path().to_path_buf();
            let theme_dir = tempfile::TempDir::new().unwrap();
            let workspace_dir = tempfile::TempDir::new().unwrap();
            let (turn_tx, turn_rx) = mpsc::channel::<TurnEvent>(8);
            Self {
                threads: HashMap::new(),
                pending_activation: HashMap::new(),
                process_watches: HashMap::new(),
                uploads: crate::image_upload::UploadRegistry::new(),
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                cfg: test_config(),
                hub: Arc::new(crate::broadcast::Broadcaster::new()),
                turn_tx,
                turn_rx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                perm_seq: Arc::new(AtomicU64::new(1)),
                theme: crate::theme_tool::ThemeHandle::new(theme_dir.path().to_path_buf()),
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                workspaces: WorkspaceIndex::new(workspace_dir.path().join("workspaces.json")),
                workdir,
                models_path: isolated_models_path(theme_dir.path()),
                lead_inbox: crate::lead::LeadInbox::new(),
                lead_replies: crate::lead::LeadReplies::new(),
                _workdir_dir: workdir_dir,
                _theme_dir: theme_dir,
                _workspace_dir: workspace_dir,
            }
        }

        fn launcher<'a, F>(&'a mut self, build_agent: &'a F) -> ServeLauncher<'a, F>
        where
            F: Fn(
                Option<yi_agent_core::Session>,
                &Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>,
        {
            ServeLauncher {
                lead_inbox: &self.lead_inbox,
                lead_replies: &self.lead_replies,
                lead_target: None,
                threads: &mut self.threads,
                pending_activation: &mut self.pending_activation,
                process_watches: &mut self.process_watches,
                uploads: &mut self.uploads,
                runtimes: &self.runtimes,
                thread_roots: &self.thread_roots,
                models_path: &self.models_path,
                cfg: &self.cfg,
                hub: &self.hub,
                turn_tx: &self.turn_tx,
                pending: &self.pending,
                permission_timeout: PERMISSION_TIMEOUT,
                perm_seq: &self.perm_seq,
                theme: &self.theme,
                git_diff: &self.git_diff,
                workspaces: &self.workspaces,
                build_agent,
            }
        }

        /// 像真主循环那样收 `TurnEvent::Finished`:清除该 thread 的
        /// `active_turn_id`,直到看到目标 thread 的完成事件。
        ///
        /// 不这样做,`active_turn_id` 永远不会被清——在 `serve` 里那是主循环的
        /// 活儿,这条测试没有主循环,必须自己复刻。
        async fn settle_turn(&mut self, thread_id: &str, timeout: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + timeout;
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return self.is_idle(thread_id);
                }
                match tokio::time::timeout(remaining, self.turn_rx.recv()).await {
                    Ok(Some(TurnEvent::Finished {
                        thread_id: id,
                        turn_id,
                        reply: _,
                        lead_request_id: _,
                    })) => {
                        if let Some(session) = self.threads.get_mut(&id) {
                            if session.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                                session.active_turn_id = None;
                            }
                        }
                        if id == thread_id {
                            return true;
                        }
                    }
                    Ok(None) => return false,
                    Err(_) => return self.is_idle(thread_id),
                }
            }
        }

        /// 该 thread 此刻是否**空闲**(`Idle` 且无活跃 turn)——与 `driver_tick`
        /// 采集的快照用的是同一条判据。
        fn is_idle(&self, thread_id: &str) -> bool {
            self.threads.get(thread_id).is_some_and(|session| {
                matches!(
                    *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                    ThreadStatus::Idle
                ) && session.active_turn_id.is_none()
            })
        }

        /// 跑一轮真 `driver_tick`，用本 fixture 的全部局部量。
        ///
        /// 与主循环的 tick 分支同一形态：驱动记账由调用方持有(跨 tick 存活),
        /// 其余局部量按 `&mut` 顺序借给 launcher。
        async fn tick<F>(
            &mut self,
            resident_dir: &Path,
            drivers: &mut HashMap<(PathBuf, String), DriverState>,
            build_agent: &F,
        ) where
            F: Fn(
                Option<yi_agent_core::Session>,
                &Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>,
        {
            super::driver_tick(
                resident_dir,
                drivers,
                &mut self.threads,
                &mut self.pending_activation,
                &mut self.process_watches,
                &mut self.uploads,
                &self.runtimes,
                &self.thread_roots,
                &self.models_path,
                &self.cfg,
                &self.hub,
                &self.turn_tx,
                &self.pending,
                PERMISSION_TIMEOUT,
                &self.perm_seq,
                &self.theme,
                &self.git_diff,
                &self.workspaces,
                build_agent,
                &self.lead_inbox,
                &self.lead_replies,
            )
            .await;
        }
    }

    /// 一个声明了 `session_driver` 的驱动插件名。
    ///
    /// 只是**名字**：宿主对插件的了解止于「清单里 `session_driver: true`」，
    /// 没有任何插件专有词汇。
    const DRIVER_PLUGIN: &str = "driver";

    /// 把一个声明了 `session_driver` 的插件装进项目，并让发现环看得见这个项目。
    ///
    /// 两处都只写**通用**事实：清单里的通用字段 `session_driver` 与通用常驻登记表
    /// （`resident::require`）。宿主不该认识 `name` 之外的任何插件专有内容。
    fn install_driver_plugin(project: &Path, resident_dir: &Path, name: &str) {
        let supervisors = project.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&supervisors).unwrap();
        std::fs::write(
            supervisors.join(format!("{name}.json")),
            json!({
                "name": name,
                "command": format!("/bin/{name}"),
                "args": [],
                "switch_key": format!("{name}_on"),
                "restart_backoff_ms": 1000,
                "restart_backoff_max_ms": 30000,
                "stop_when_disabled": false,
                "query_socket": format!("{{state_dir}}/{name}.sock"),
                "session_driver": true,
            })
            .to_string(),
        )
        .unwrap();
        yi_agent_store::resident::require(resident_dir, project, name).unwrap();
    }

    /// 子进程里跑一个 `#[ignore]` 探针,取回它的 stdout+stderr 合并文本。
    ///
    /// **为什么非得开子进程**:libtest 默认把 `eprintln!` 收进线程私有的捕获缓冲
    /// (`std::io::set_output_capture`),根本不写 fd 2——父进程怎么重定向 fd 2 都
    /// 看不到(实测两种模式见 `task-6-report.md` 的 FIX ROUND 1)。子进程加
    /// `--nocapture` 后 `eprintln!` 才落回真 fd 2,这时才可观察。
    ///
    /// 探针用 `#[ignore]` 标记:常规 `cargo test` 不会直接跑它(它只负责「在子进程
    /// 里制造一次 tick」),由本函数的调用方显式带 `--ignored --exact` 拉起。
    fn logs_of_ignored_probe(probe: &str) -> String {
        let exe = std::env::current_exe().expect("the test binary must be locatable");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                probe,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .expect("the test binary must be runnable");
        let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "the probe {probe} must pass; its output was:\n{text}"
        );
        text
    }

    /// 探针(默认跳过,由 `logs_of_ignored_probe` 在子进程里拉起):
    /// 项目在常驻登记表里、但没有任何声明 `session_driver` 的插件 → 一拍 tick。
    #[test]
    #[ignore]
    fn probe_a_registered_project_without_a_driver_plugin() {
        let resident_dir = tempfile::TempDir::new().unwrap();
        let project_dir = tempfile::TempDir::new().unwrap();
        // 登记为常驻,却不写任何清单目录:这正是「注册过了却没人可问」。
        yi_agent_store::resident::require(resident_dir.path(), project_dir.path(), "some-plugin")
            .unwrap();
        let mut host = Host::new();
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
    }

    /// 探针(默认跳过):插件清单在、可查询,但 daemon/套接字拨不通 → 一拍 tick。
    #[test]
    #[ignore]
    fn probe_a_driver_plugin_that_cannot_be_reached() {
        let resident_dir = tempfile::TempDir::new().unwrap();
        let project_dir = tempfile::TempDir::new().unwrap();
        // 目录就叫 `project`,好让日志断言能核对「这一行点了名」而不是碰巧匹配。
        let project = project_dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        // 清单写了 `query_socket`,却没有 daemon 听在那个 socket 上。
        install_driver_plugin(&project, resident_dir.path(), DRIVER_PLUGIN);
        let mut host = Host::new();
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
    }

    /// F1:登记为常驻却没有驱动插件的项目,必须在日志里说明「这一拍没问任何人」。
    ///
    /// 这是「静默停摆」与「问了、失败了」在运维眼里唯一的分界:若这一行不存在,
    /// 一个健康的 daemon 永远不被问起也不留痕。
    #[test]
    fn a_registered_project_without_a_driver_plugin_says_nothing_was_asked() {
        let logs = logs_of_ignored_probe(
            "server::session_driver_tests::probe_a_registered_project_without_a_driver_plugin",
        );
        assert!(
            logs.contains("has no session_driver plugin, so nothing was asked"),
            "the never-asked case must leave a log line: {logs}"
        );
    }

    /// F1:问不通的 (项目, 插件) 必须在日志里各留一行,而不是无声跳过。
    ///
    /// `driver_step` 把查询失败整体吞成「不推进」,故这一行只能由 tick 侧留下。
    #[test]
    fn a_driver_plugin_that_cannot_be_reached_leaves_a_log_line() {
        let logs = logs_of_ignored_probe(
            "server::session_driver_tests::probe_a_driver_plugin_that_cannot_be_reached",
        );
        assert!(
            logs.contains("driver tick: could not reach")
                && logs.contains(DRIVER_PLUGIN)
                && logs.contains("/project:"),
            "a failed query must name the plugin and the project: {logs}"
        );
    }

    /// 探针(默认跳过):查询**成功**、字面也读得出来,但对不上契约——键名漂移
    /// (`type` 而非契约里的 `kind`)。一拍 tick。
    #[test]
    #[ignore]
    fn probe_a_driver_answer_the_host_cannot_parse() {
        let daemon = FakeDaemon::bind(|_project, _method, _params| {
            Ok(json!({ "action_id": "a1", "type": "launch", "workdir": "/w" }))
        });
        let resident_dir = tempfile::TempDir::new().unwrap();
        install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
        let mut host = Host::new();
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
    }

    /// 探针(默认跳过):插件答契约里的 `{"action": null}`——正常空闲。
    #[test]
    #[ignore]
    fn probe_a_driver_that_answers_the_contract_null() {
        let daemon = FakeDaemon::bind(|_project, _method, _params| Ok(json!({ "action": null })));
        let resident_dir = tempfile::TempDir::new().unwrap();
        install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
        let mut host = Host::new();
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
    }

    /// 第三种无声失败:查询**成功**、字面可读,却是宿主解析不出的形状。它与
    /// 「插件说无事可做」在 `driver_step` 里同型(都回 `None`),不在这里分辨就每
    /// 3s 无声重复一次。日志须点名插件、项目,并带上原答的顶层键供人看漂移。
    #[test]
    fn an_unparseable_driver_answer_leaves_a_log_line() {
        let logs = logs_of_ignored_probe(
            "server::session_driver_tests::probe_a_driver_answer_the_host_cannot_parse",
        );
        assert!(
            logs.contains("cannot parse")
                && logs.contains(DRIVER_PLUGIN)
                && logs.contains("/project answered driver/next")
                && logs.contains("action_id")
                && logs.contains("type"),
            "an unparseable answer must name the plugin, the project and the keys: {logs}"
        );
        assert!(
            !logs.contains("could not reach"),
            "答案读得出来,这不是「问不通」那一类: {logs}"
        );
    }

    /// 反向对照:契约的 `{"action": null}` 是**正常空闲**,不得被当成断链。
    ///
    /// 这条与上一条互为闸门:若把「说不清」与「无事可做」混为一谈(例如去掉
    /// `is_explicit_no_action` 判定),上一条仍会通过而这一条会因每拍一行噪音而红。
    #[test]
    fn a_contract_null_answer_is_not_reported_as_drift() {
        let logs = logs_of_ignored_probe(
            "server::session_driver_tests::probe_a_driver_that_answers_the_contract_null",
        );
        assert!(
            !logs.contains("cannot parse"),
            "契约的空闲形状是正常态,不该被打成断链: {logs}"
        );
    }

    /// 一轮 tick：插件回 launch/append → 宿主真的按它拉会话，且**不调用**任何
    /// `board.*` 方法名。
    ///
    /// 这是「宿主不再说看板话」这条约束的**载荷断言**：记录到的每一次插件调用都
    /// 必须是通用的 `driver/next`。第一拍插件给一个 `launch`（宿主起可见会话），
    /// 首轮跑完后第二拍它给一个 `append`（宿主复用同一个会话再投一轮）——两个动作
    /// 都经 `driver_step` + 真 `ServeLauncher` 落地，插件同时从 `threads[].idle`
    /// 拿到了宿主唯一能给的通用事实。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_drives_the_plugin_through_driver_next_only() {
        let mut host = Host::new();
        let workdir = host.workdir.to_string_lossy().to_string();
        let work_path = workdir.clone();
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = Arc::clone(&rounds);
        // 每拍请求里 `threads[0].idle` 的原样记录(缺省记 `null`)。
        let idle_seen: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorded_idle = Arc::clone(&idle_seen);
        let daemon = FakeDaemon::bind(move |_project, method, params| {
            if method != "driver/next" {
                return Ok(json!({ "ok": true }));
            }
            // 原样记下本拍请求里的 `threads[0].idle`(本拍尚无会话时是 `null`),
            // 供 F4 断言「宿主经 tick 送出的是真 idle,不是常量」。
            recorded_idle
                .lock()
                .unwrap()
                .push(params["threads"][0]["idle"].clone());
            match seen.fetch_add(1, Ordering::SeqCst) {
                // 第一拍：起一个会话。
                0 => Ok(json!({
                    "action_id": "a1",
                    "kind": "launch",
                    "workdir": work_path,
                    "title": "driven · card-1",
                    "objective": "do the first thing",
                })),
                // 第二拍：往它已经起了的那个会话里追加一轮。thread_id 只可能来自
                // 宿主上一拍回报的 `results`（`threads[]` 也带了它）。
                1 => {
                    let thread_id = params["threads"][0]["thread_id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    Ok(json!({
                        "action_id": "a2",
                        "kind": "append",
                        "thread_id": thread_id,
                        "objective": "round two",
                    }))
                }
                _ => Ok(serde_json::Value::Null),
            }
        });
        let resident_dir = tempfile::TempDir::new().unwrap();
        install_driver_plugin(&daemon.project, resident_dir.path(), "driver");
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();

        // 第一拍：launch。
        host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
            .await;
        assert_eq!(host.threads.len(), 1, "the launch registers one session");
        let thread_id = host.threads.keys().next().cloned().unwrap();
        assert!(
            host.threads[&thread_id].active_turn_id.is_some(),
            "the first turn must be占位, otherwise the plugin sees it idle at once"
        );
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the driver must consume the plugin's objective and finish"
        );

        // 第二拍：append 到同一个会话。
        host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
            .await;

        // 第三拍：只问不做（插件回 `null`）。它让第二拍动手后的空闲事实有一次
        // 上送机会——`append` 在那拍里刚投递完，此刻该会话正忙（见下面的 F4 断言）。
        host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
            .await;

        let calls = daemon.calls();
        assert_eq!(
            calls.len(),
            3,
            "one plugin call per tick, three ticks: {calls:?}"
        );
        let methods: Vec<&str> = calls
            .iter()
            .map(|call| call["method"].as_str().unwrap_or_default())
            .collect();
        assert!(
            methods.iter().all(|method| *method == "driver/next"),
            "the host drives the generic protocol and nothing else: {methods:?}"
        );
        assert!(
            methods.iter().all(|method| !method.starts_with("board.")),
            "no board vocabulary may leave the host: {methods:?}"
        );
        // 上面 `all(== "driver/next")` 已经蕴含「至少问过一次」,故不再单列
        // `calls_to("driver/next").is_empty()` 断言——它对本用例是冗余的。

        assert_eq!(host.threads.len(), 1, "append must reuse the session");
        assert!(
            host.threads[&thread_id].active_turn_id.is_some(),
            "the appended round must be delivered to the same session"
        );
        // F4:`threads[].idle` 是**经 tick 上送的真事实**,不是常量。三拍快照依次是
        // 「还没有会话(无 thread → 数组下标缺省 `null`)」「首轮已收尾(空闲 `true`)」
        // 「刚追加的一轮仍在飞(忙 `false`)」——同一字段三种取值,常量填不出来。
        let idle_series: Vec<serde_json::Value> = idle_seen.lock().unwrap().clone();
        assert_eq!(
            idle_series,
            vec![serde_json::Value::Null, json!(true), json!(false)],
            "the tick must wire real idle, not a constant: {idle_series:?}"
        );
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the appended round must run and finish"
        );
    }

    /// 核心行为:launcher 的一个 `launch` 动作落成一个**登记在册、driver 活着、
    /// 首轮已投递、标题已落盘**的可见会话。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_launcher_starts_a_registered_session_with_a_title_and_a_first_turn() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let thread_id = {
            let mut launcher = host.launcher(&build_test_agent);
            launch_via_driver(
                &mut launcher,
                &workdir.to_string_lossy(),
                "driven · card-1",
                "Implement the plan for card-1",
            )
            .await
            .expect("the launch must succeed for a healthy factory")
        };

        assert_eq!(host.threads.len(), 1, "the driven thread is registered");
        let session = host
            .threads
            .get(&thread_id)
            .expect("the session is visible");
        assert_eq!(session.cwd, workdir.to_string_lossy());
        assert!(
            session.active_turn_id.is_some(),
            "the first turn must be占位, otherwise the session looks idle instantly"
        );
        assert_eq!(
            session
                .store
                .load(&thread_id)
                .unwrap()
                .and_then(|loaded| loaded.meta.title),
            Some("driven · card-1".to_string()),
            "the sidebar needs a name, not a raw thread id"
        );

        // driver 真的收了首轮并收尾:主循环会收到 `Finished`,清除占位。
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the driver must consume the prompt and finish"
        );
        assert!(host.pending_activation.contains_key(&thread_id));
    }

    /// build 失败时绝不谎报:没有 thread 被登记。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failing_build_leaves_no_session_behind() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let build_failure = |_s: Option<yi_agent_core::Session>,
                             _cwd: &Path,
                             _m: crate::thread_store::ThreadMode| {
            Err::<BuiltAgent, _>(anyhow::anyhow!("no model for you"))
        };
        {
            let mut launcher = host.launcher(&build_failure);
            let error = launch_via_driver(
                &mut launcher,
                &workdir.to_string_lossy(),
                "driven · card-1",
                "Implement the plan for card-1",
            )
            .await
            .expect_err("a factory failure must surface, not be swallowed");
            assert!(error.to_string().contains("no model"), "{error}");
        }
        assert!(host.threads.is_empty(), "a failed launch registers nothing");
    }

    /// 驱动器端到端:插件经 `driver/next` 交出一个 `launch`,宿主把它起成
    /// **可见**会话——客户端的 hub 里出现 thread-keyed 的开启项,且首个内容项
    /// 就是本轮 objective(用户提问),不是 agent 的 item。
    ///
    /// 走**真 `driver_tick` + 真 `ServeLauncher` + 真 socket 转发**,是接线的
    /// 最小完整闭环。插件同时从 `threads[].idle` 得知这一轮是否跑完。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_driven_session_is_visible_and_its_first_content_item_is_the_objective() {
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_rounds = Arc::clone(&rounds);
        let work_path = Arc::new(StdMutex::new(String::new()));
        let handed_work = Arc::clone(&work_path);
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "driver/next" => {
                if seen_rounds.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Ok(serde_json::Value::Null);
                }
                Ok(json!({
                    "action_id": "a1",
                    "kind": "launch",
                    "workdir": handed_work.lock().unwrap().clone(),
                    "title": "driven · card-1",
                    "objective": "Implement the plan for card-1",
                }))
            }
            _ => Ok(json!({ "ok": true })),
        });

        let mut host = Host::new();
        // 监听宿主 hub:被驱动会话是 thread-keyed 的 content 通知,注册一个客户端
        // 就能看到驱动器起会话时到底发了什么、按什么顺序。
        let mut frames = host
            .hub
            .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));
        let workdir = host.workdir.clone();
        *work_path.lock().unwrap() = workdir.to_string_lossy().to_string();
        let resident_dir = tempfile::TempDir::new().unwrap();
        install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
        let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();

        host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
            .await;

        assert_eq!(host.threads.len(), 1, "exactly one visible session");
        let thread_id = host.threads.keys().next().cloned().unwrap();

        // 被驱动会话对客户端「可见」的前提是 hub 里有它的 item;其中最关键的
        // 是**本轮开启项**(用户提问=插件给的 objective)必须排在 agent 任何 item
        // 之前——修复前它只在落盘时补发,点开会话会在最底部才看到开场白。
        let mut seen: Vec<(String, Option<String>, Option<String>)> = Vec::new();
        while let Ok(v) = frames.try_recv() {
            let method = v["method"].as_str().unwrap_or_default().to_string();
            let item = &v["params"]["item"];
            let itype = item["type"].as_str().map(str::to_string);
            let itext = item["text"].as_str().map(str::to_string);
            seen.push((method, itype, itext));
        }
        let first_content = seen
            .iter()
            .find(|(_, t, _)| t.is_some())
            .expect("the launch must emit at least one item frame");
        assert_eq!(
            first_content.1.as_deref(),
            Some("userMessage"),
            "the driven session's first content item must be the opening objective, \
             not an agent item: {seen:?}"
        );
        assert_eq!(
            first_content.0, "item/started",
            "the opening item starts the transcript: {seen:?}"
        );

        // 插件在下一拍必须看到这个会话,且能读出「闲不闲」这个通用事实。
        assert_eq!(
            drivers[&(daemon.project.clone(), DRIVER_PLUGIN.to_string())]
                .threads
                .len(),
            1,
            "the host must remember the thread it launched for the plugin"
        );
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the driven thread must finish its first turn"
        );
    }

    /// 真主循环接线:启动 `serve` 后,3s tick 把插件经 `driver/next` 交出的动作
    /// 落成会话——证明驱动器确实挂在主循环上,而不是只在单测里可调。
    ///
    /// 首个 tick 被刻意推迟到 3s(见 `DRIVER_TICK` 的说明),故这里等到 15s。
    #[tokio::test(flavor = "multi_thread")]
    async fn the_serve_loop_tick_launches_a_driven_session() {
        let workdir_dir = tempfile::TempDir::new().unwrap();
        let workdir = workdir_dir.path().canonicalize().unwrap();
        let work_path = workdir.to_string_lossy().to_string();
        let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen_rounds = Arc::clone(&rounds);
        let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
            "driver/next" => {
                if seen_rounds.fetch_add(1, Ordering::SeqCst) > 0 {
                    return Ok(serde_json::Value::Null);
                }
                Ok(json!({
                    "action_id": "a1",
                    "kind": "launch",
                    "workdir": work_path,
                    "title": "driven · card-1",
                    "objective": "do the first thing",
                }))
            }
            _ => Ok(json!({ "ok": true })),
        });

        let mut h = crate::server::tests::Harness::new();
        // 发现环的两个通用来源:常驻登记表(harness 的隔离目录)与插件清单
        // (项目自己的 `.yi-agent/supervisors/`)。
        install_driver_plugin(&daemon.project, h.resident_dir.path(), DRIVER_PLUGIN);
        crate::server::tests::initialize(&mut h).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut launched = false;
        while tokio::time::Instant::now() < deadline {
            let asked = !daemon.calls_to("driver/next").is_empty();
            let sessions = crate::thread_store::ThreadStore::new(&workdir)
                .list()
                .unwrap_or_default();
            if asked && !sessions.is_empty() {
                launched = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            launched,
            "the serve tick must drive the plugin and start the session it asks for; \
             plugin calls so far: {:?}",
            daemon.calls()
        );

        h.shutdown().await;
    }

    /// 构造一个**新建会话**动作,并走真正的 `SessionLauncher` 入口。
    async fn launch_via_driver<F>(
        launcher: &mut ServeLauncher<'_, F>,
        workdir: &str,
        title: &str,
        objective: &str,
    ) -> anyhow::Result<String>
    where
        F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>,
    {
        SessionLauncher::launch(
            launcher,
            &DriverAction {
                action_id: "act-launch".into(),
                kind: DriverActionKind::Launch {
                    workdir: workdir.to_string(),
                    title: title.to_string(),
                    objective: objective.to_string(),
                },
            },
        )
        .await
    }

    /// **本任务的核心断言**:一次 `append` 动作必须**复用**已有会话——
    /// 不新建 thread,原 thread 上多出一轮;且因为走的是「无响应帧」路径,
    /// 只发 thread-keyed 通知。
    #[tokio::test(flavor = "multi_thread")]
    async fn an_append_action_reuses_the_existing_thread() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let workdir = workdir.to_string_lossy().to_string();
        let mut frames = host
            .hub
            .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));
        // launcher 的客户端身份就是 `ClientId::local()`(`launch_inner` 里构造),而
        // **响应帧**经 `hub.reply(client, …)` 定向投递——**未登记的 client 会被直接
        // 丢弃**(`Broadcaster::reply` 对未知 id 返回 `Closed`)。所以只注册一个 ws
        // client 的旧版断言,只能证明「广播里没有响应帧」,证明不了「宿主没发响应帧」。
        // 把 local 也登记进来,响应帧才会落进一个真正被观察的队列。
        let mut local_frames = host
            .hub
            .register_reliable(crate::broadcast::ClientId::local());

        // ① 先起一个会话(launch 动作)。
        let thread_id = {
            let mut launcher = host.launcher(&build_test_agent);
            launch_via_driver(
                &mut launcher,
                &workdir,
                "看板 · card-1",
                "do the first thing",
            )
            .await
            .expect("a healthy factory must start the session")
        };
        assert_eq!(
            host.threads.len(),
            1,
            "the launch registers exactly one thread"
        );
        assert!(
            host.threads
                .get(&thread_id)
                .is_some_and(|s| s.active_turn_id.is_some()),
            "the first turn must be占位 before the append"
        );
        // 收尾首轮:主循环语义是「收 Finished → 清占位」。不这样做,`append`
        // 会撞上 `TurnInProgress`,测的就不是 append 了。
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the first turn must finish before the append"
        );

        // ② 再追一轮(append 动作):**同一个** thread_id,不新铸 uuid。
        let appended = {
            let mut launcher = host.launcher(&build_test_agent);
            SessionLauncher::launch(
                &mut launcher,
                &DriverAction {
                    action_id: "act-append".into(),
                    kind: DriverActionKind::Append {
                        thread_id: thread_id.clone(),
                        objective: "now merge it".to_string(),
                    },
                },
            )
            .await
            .expect("appending to a live thread must succeed")
        };

        assert_eq!(appended, thread_id, "append must return the same thread");
        assert_eq!(host.threads.len(), 1, "no second thread may appear");
        let session = host.threads.get(&thread_id).expect("the session survives");
        assert!(
            session.active_turn_id.is_some(),
            "the existing thread must have received a new (occupied) turn"
        );

        // 盘上也只有这一个会话:append 没有多写一份 meta。
        let metas = crate::thread_store::ThreadStore::new(Path::new(&workdir))
            .list()
            .unwrap();
        assert_eq!(
            metas.len(),
            1,
            "append must not create a second session on disk"
        );

        // ③ 可见性走 thread-keyed 通知(spec §4.4):append 轮必须与首轮一样,发出
        //    `turn/started`(桌面用它断开 reasoning run 并置 `turnActive`)以及开启项
        //    (用户提问=插件给的 objective)。`turn/started` 是**内容通知**,不是 RPC
        //    响应帧——它经 `broadcast` 扇出,而非 `reply` 定向应答。
        let mut seen = Vec::new();
        while let Ok(v) = frames.try_recv() {
            assert!(
                v.get("method").is_some(),
                "the launcher must emit notifications only, never a response frame: {v}"
            );
            let item = &v["params"]["item"];
            seen.push((
                v["method"].as_str().unwrap_or_default().to_string(),
                item["text"].as_str().map(str::to_string),
            ));
        }
        assert!(
            seen.iter().any(|(method, text)| method == "item/started"
                && text.as_deref() == Some("now merge it")),
            "the appended objective must open the new turn: {seen:?}"
        );
        assert!(
            seen.iter().any(|(method, _)| method == "turn/started"),
            "the append must announce the turn it starts (spec §4.4 visibility): {seen:?}"
        );

        // ④ 现在这个断言才真的能证明「没有响应帧」:launcher 的客户端身份是
        //    `ClientId::local()`,上面已把它登记进 hub。**响应帧**经 `hub.reply`
        //    定向投递,未登记的 client 会被静默丢弃——旧版只注册 ws client,响应帧
        //    根本不会出现在任何被观察的队列里,那句断言其实只覆盖了广播。
        //
        //    这里对 local 队列用「有没有 `id`」来判:响应信封(`ResponseEnvelope`)
        //    必带 `id` 且无 `method`,通知信封反之。local 同时也会收到广播,所以不能
        //    断言它「空」——但可以断言它里面**没有一帧是响应**。
        let mut local_seen = Vec::new();
        while let Ok(v) = local_frames.try_recv() {
            local_seen.push(v);
        }
        assert!(
            local_seen.iter().all(|v| v.get("id").is_none()),
            "the launcher must not answer any RPC client: a response frame reached local: {local_seen:?}"
        );

        // 正向对照(判别力证明):这条观察通道**确实**看得见响应帧。手工写一帧响应
        // 给 local,它必须落到同一个队列里——否则上面的断言就是在断言「没观察」。
        //
        // 先把队列排空:被追加的那一轮还在跑,它的 thread-keyed 广播帧会随时插进
        // local 的队列;不清空的话,下面取到的「第一帧」可能是一条通知,这条对照
        // 就会因时序而偶发失败。
        while local_frames.try_recv().is_ok() {}
        write_response(
            &host.hub,
            &crate::broadcast::ClientId::local(),
            ok_response(RequestId::Num(1), json!({})),
        )
        .await
        .expect("the observed local client is registered, so the reply must land");
        // 排空只清掉「此刻已在队列里」的帧,写入之后仍可能有广播抢在响应前面落地,
        // 故不取「下一帧」,而是有界地找那帧带 `id` 的响应。
        let mut probe = None;
        for _ in 0..64 {
            match local_frames.try_recv() {
                Ok(frame) if frame.get("id").is_some() => {
                    probe = Some(frame);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        let probe = probe.expect("a written response must land in the observed local queue");
        assert!(
            probe.get("method").is_none(),
            "the control frame must look like a response, not a notification: {probe}"
        );

        // 收尾这一轮,确认它真的被 driver 消费(而不只是占了个位)。
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the appended turn must run and finish"
        );
        assert!(
            host.is_idle(&thread_id),
            "after the appended turn settles the thread is idle again"
        );
    }

    /// **追问轮撞上忙会话**(spec §10 的 append TOCTOU 行):宿主必须干净地拒绝,
    /// 且**不得碰**那个活 turn 的任何记账。
    ///
    /// 这是 Finding 1 的回归测试。旧实现在 `prepare_and_deliver` 的失败分支里清
    /// `active_turn_id`,而 `prepare_turn_core` 只在**最后一步**才写占位——失败返回
    /// 全都发生在那之前,故那次清零只能清掉**别人的**占位:这里的活 turn 会因此
    /// 变成无主 turn(`turn/cancel` 静默失效、`is_idle` 谎报空闲),与 spec §10
    /// 「`ok:false`、零副作用」相悖。
    ///
    /// 用 `build_slow_agent`:`SlowProvider` 的 delta 流不收敛,首轮**永不结束**,
    /// 于是「有活 turn」是这一条测试真实造出来的状态(而非靠手写占位)。
    #[tokio::test(flavor = "multi_thread")]
    async fn an_append_into_a_busy_thread_fails_without_touching_the_live_turn() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let workdir = workdir.to_string_lossy().to_string();

        // ① 起一个会话,它的首轮**一直在跑**(slow provider)。
        let thread_id = {
            let mut launcher = host.launcher(&build_slow_agent);
            launch_via_driver(&mut launcher, &workdir, "看板 · card-1", "run forever")
                .await
                .expect("a healthy factory must start the session")
        };
        let live_turn = host
            .threads
            .get(&thread_id)
            .and_then(|s| s.active_turn_id.clone())
            .expect("the slow first turn must be占位");
        assert!(
            !host.is_idle(&thread_id),
            "a thread with a live turn must not be reported idle"
        );

        // ② 现在对它做一次 `append`:必须失败,而且是「turn 撞车」这个具体原因。
        let error = {
            let mut launcher = host.launcher(&build_slow_agent);
            SessionLauncher::launch(
                &mut launcher,
                &DriverAction {
                    action_id: "act-append".into(),
                    kind: DriverActionKind::Append {
                        thread_id: thread_id.clone(),
                        objective: "a second round".to_string(),
                    },
                },
            )
            .await
            .expect_err("an append into a busy thread must be refused, not queued")
        };
        assert!(
            error.to_string().contains("turn already in progress"),
            "the plugin must learn the real reason (busy, not unknown thread): {error}"
        );

        // ③ **本测试的核心**:那个活 turn 的记账原封不动。
        let session = host.threads.get(&thread_id).expect("the session survives");
        assert_eq!(
            session.active_turn_id.as_deref(),
            Some(live_turn.as_str()),
            "a refused prepare must not untrack the live turn it never owned"
        );
        assert!(
            !host.is_idle(&thread_id),
            "the busy session must still be reported busy afterwards"
        );
        assert_eq!(host.threads.len(), 1, "no session may be created or lost");

        // ④ 被拒的那次调用**没有**为该 thread 开新的一轮(没有第二个占位)。
        //    只此一处活跃 turn id,且仍是原来那个。
        assert_eq!(
            host.threads
                .get(&thread_id)
                .map(|s| s.active_turn_id.clone()),
            Some(Some(live_turn)),
            "exactly the original live turn remains"
        );
    }

    /// 未知 thread 的 `append` 必须**明确失败**、且不留下任何副作用:插件据此
    /// 决定重试或放弃(`{ok:false, error}`),宿主不得替它改写状态。
    #[tokio::test(flavor = "multi_thread")]
    async fn an_append_to_an_unknown_thread_fails_without_side_effects() {
        let mut host = Host::new();
        let mut frames = host
            .hub
            .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));

        let mut launcher = host.launcher(&build_test_agent);
        let error = SessionLauncher::launch(
            &mut launcher,
            &DriverAction {
                action_id: "act-append".into(),
                kind: DriverActionKind::Append {
                    thread_id: "thread-does-not-exist".into(),
                    objective: "merge it".to_string(),
                },
            },
        )
        .await
        .expect_err("an unknown thread must not be silently accepted");

        assert_eq!(
            error.to_string(),
            "unknown thread: thread-does-not-exist",
            "the error must name the thread the plugin asked about"
        );
        assert!(host.threads.is_empty(), "no session may be created");
        assert!(
            frames.try_recv().is_err(),
            "a refused append must not emit any frame"
        );
    }

    /// `driver/next` 的 `launch` 应答只带 `workdir`/`title`/`objective`(spec §4.3),
    /// 宿主**不认识 card id**——所以走通用动作起的会话,`driver_ref` 必须是 `None`
    /// (计划已把该填充缺口记给 G2),而 `driver_project` 同样为 `None`(发现环按
    /// 项目遍历,却仍不填这两个归属值——同一缺口,见 plan 归 G2)。这里同时钉住
    /// 「会话仍然可见、首轮仍然投递」,即缺这两个归属值不影响起会话本身。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_launch_action_records_no_driver_origin_and_still_starts_a_turn() {
        let mut host = Host::new();
        let workdir = host.workdir.clone();
        let workdir = workdir.to_string_lossy().to_string();
        let thread_id = {
            let mut launcher = host.launcher(&build_test_agent);
            launch_via_driver(
                &mut launcher,
                &workdir,
                "看板 · card-1",
                "do the first thing",
            )
            .await
            .expect("a healthy factory must start the session")
        };

        let metas = crate::thread_store::ThreadStore::new(Path::new(&workdir))
            .list()
            .unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(
            metas[0].driver_ref, None,
            "the host does not know a card id any more (spec §2 goal 1 / §4.3)"
        );
        assert_eq!(metas[0].driver_project, None);
        assert_eq!(
            metas[0].title.as_deref(),
            Some("看板 · card-1"),
            "the plugin's title still lands in the meta"
        );
        assert!(
            host.threads
                .get(&thread_id)
                .is_some_and(|s| s.active_turn_id.is_some()),
            "the first turn must still be occupied"
        );
        assert!(
            host.settle_turn(&thread_id, Duration::from_secs(10)).await,
            "the driver must consume the delivered objective"
        );
    }
}

#[cfg(test)]
// 这里的模块名必须与文件名不同:`#[path]` 只换文件来源,模块路径由 `mod tests`
// 决定,故仍是 `crate::server::tests`,该路径下 227 个测试名一字未变(若命名成
// `mod rpc_tests;` 则路径会变成 `server::rpc_tests`,测试名全变)。
#[path = "rpc_tests.rs"]
pub(crate) mod tests;
/// 主题守望者的回归测试。
///
/// 这些测试直接驱动生产里的 [`pump_theme_notifications`],不再照抄一份循环体:
/// 「Lagged 后仍存活」只有在真正跑被 spawn 的那段代码时才算数。
#[cfg(test)]
mod theme_watcher_tests {
    use super::*;

    /// 快进:让订阅者跑赢容量 16 的广播、把订阅者落在后面,`recv()` 就会返回
    /// `Lagged`(`ThemeHandle::new` 用的正是 `broadcast::channel(16)`)。
    ///
    /// 先 `yield_now` 保证 spawn 的守望者已挂上订阅:否则 17 次 `send` 一场空,
    /// 之后再订阅只会收到第 201 次的值,测不出 Lagged。
    async fn lag_the_subscriber(theme: &crate::theme_tool::ThemeHandle) {
        tokio::task::yield_now().await;
        for i in 0..(16 + 200) {
            theme.set(if i % 2 == 0 {
                crate::settings_store::Theme::Light
            } else {
                crate::settings_store::Theme::Dark
            });
        }
    }

    /// 回归:`Lagged` 不得终结守望者。
    ///
    /// 旧写法 `while let Ok(theme) = rx.recv().await` 把 `Lagged` 与 `Closed` 一并
    /// 当作退出条件;一次背压(客户端一时读得慢)就把守望者杀掉,此后本进程再也
    /// 推不出 `ui/settings/updated`——对话框或 `set_theme` 工具改的主题静默丢失。
    #[tokio::test]
    async fn theme_watcher_survives_a_lagged_broadcast() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let local = crate::broadcast::ClientId::local();
        let mut out = hub.register_reliable(local);
        let theme = crate::theme_tool::ThemeHandle::new(std::env::temp_dir());
        let pump = tokio::spawn(pump_theme_notifications(
            theme.subscribe(),
            Arc::clone(&hub),
        ));

        lag_the_subscriber(&theme).await;

        // 让守望者重新比发送者快:它必须先处理 `Lagged`(不退出),再收到这次值。
        let mut saw_light_after_the_lag = false;
        for _ in 0..200 {
            theme.set(crate::settings_store::Theme::Light);
            match tokio::time::timeout(crate::test_support::EVENT_WAIT, out.recv()).await {
                Ok(Some(frame)) => {
                    if frame["method"] == "ui/settings/updated"
                        && frame["params"]["theme"] == "light"
                    {
                        saw_light_after_the_lag = true;
                        break;
                    }
                }
                Ok(None) => panic!("the local client's outbound queue closed unexpectedly"),
                Err(_) => break,
            }
        }
        pump.abort();
        assert!(
            saw_light_after_the_lag,
            "the theme watcher must stay alive across a Lagged broadcast and still push \
             ui/settings/updated; `while let Ok` returns instead"
        );
    }

    /// 守住的另一半语义:`Closed`(所有发送端都没了)才终止。
    ///
    /// 这条防止把守望者改成「永不退出」的忙循环。
    #[tokio::test]
    async fn theme_watcher_ends_when_the_broadcast_closes() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let (tx, rx) = tokio::sync::broadcast::channel::<crate::settings_store::Theme>(16);
        let pump = tokio::spawn(pump_theme_notifications(rx, Arc::clone(&hub)));
        drop(tx);
        let finished = tokio::time::timeout(crate::test_support::EVENT_WAIT, pump)
            .await
            .expect("the watcher must exit once the broadcast is closed");
        assert!(finished.is_ok());
    }

    /// 投递分层：列表层/全局帧无 thread 键（恒推），内容层按 thread 键过滤。
    #[test]
    fn notification_delivery_classifies_list_and_content() {
        use crate::protocol::{Delivery, Notification};
        let status = Notification::ThreadStatusUpdated {
            thread_id: "t2".into(),
            status: crate::protocol::ThreadStatus::Running,
        };
        assert_eq!(status.delivery(), Delivery::List);
        assert_eq!(
            Notification::ThreadStarted {
                thread_id: "t2".into(),
                cwd: "/w".into(),
                model: "m".into(),
                model_ref: None
            }
            .delivery(),
            Delivery::List
        );
        assert_eq!(
            Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "d".into()
            }
            .delivery(),
            Delivery::Content
        );
        // 回合结束是一桩**列表层/注意力**事实，不是会话正文：未读蓝点唯一的
        // 驱动。远程客户端只订阅寥寥几条会话，若把它按内容层过滤，窗口外跑完的
        // 会话就永远不亮蓝点——用户以为「没跑完」。
        assert_eq!(
            Notification::TurnCompleted {
                thread_id: "t2".into(),
                turn_id: "u".into(),
                status: crate::protocol::TurnStatus::Completed,
                error: None
            }
            .delivery(),
            Delivery::List
        );
        assert_eq!(
            Notification::UiSettingsUpdated {
                theme: "dark".into()
            }
            .delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::Error {
                message: "x".into()
            }
            .delivery(),
            Delivery::Global
        );
        assert_eq!(
            Notification::ToolCallApprovalResolved {
                perm_id: "p".into(),
                by: "c".into(),
                decision: "allow_once".into()
            }
            .delivery(),
            Delivery::Global
        );
    }

    /// 订阅 t1 的客户端：仍收到 t2 的 `thread/status/updated`（列表层恒推），
    /// 但收不到 t2 的 `item/delta`（内容层过滤）。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_layer_is_always_delivered_while_content_is_filtered() {
        let hub = crate::broadcast::Broadcaster::new();
        let sub = crate::broadcast::ClientId::ws(uuid::Uuid::from_u128(1));
        let mut rx = hub.register(sub.clone());
        hub.subscribe(&sub, vec!["t1".to_string()]);

        // 列表层：未订阅的 t2 的状态帧也必须到达。
        write_notification(
            &hub,
            &Notification::ThreadStatusUpdated {
                thread_id: "t2".into(),
                status: crate::protocol::ThreadStatus::Running,
            },
        )
        .await
        .unwrap();
        assert_eq!(rx.recv().await.unwrap()["params"]["thread_id"], "t2");

        // 列表层：未订阅的 t2 的**回合结束**同样必须到达——它是未读蓝点唯一的
        // 驱动，被过滤掉就再没有第二次机会（内容层被挡下，客户端无从得知跑完了）。
        // 用 `try_recv` 而非 `recv().await`：投递是同步 `try_send`，帧已在该分支
        // 入队或丢弃；`await` 在 RED 下会挂死而不是快速失败。
        write_notification(
            &hub,
            &Notification::TurnCompleted {
                thread_id: "t2".into(),
                turn_id: "u1".into(),
                status: crate::protocol::TurnStatus::Completed,
                error: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            rx.try_recv().expect("未订阅的 t2 的回合结束帧也必须到达")["method"],
            "turn/completed"
        );

        // 内容层：未订阅的 t2 的 delta 必须被挡下。
        write_notification(
            &hub,
            &Notification::ItemDelta {
                thread_id: "t2".into(),
                item_id: "i".into(),
                delta: "leak".into(),
            },
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err(), "订阅 t1 不该收到 t2 的内容帧");
    }
}

/// Git Diff 守望者的回归测试。
///
/// 与上面的主题守望者同理:直接驱动生产里的 [`pump_git_diff_notifications`]。
/// 新增的写帧表尤其必须这样测——工具在模型侧算好的帧能不能到 `thread/view/read`
/// 手里,全取决于这段循环里那次 `insert`。
#[cfg(test)]
mod git_diff_watcher_tests {
    use super::*;

    fn frame(command: &str) -> crate::git_diff::ViewFrame {
        crate::git_diff::ViewFrame {
            command: Some(command.to_string()),
            as_kind: "log".to_string(),
            exit_code: Some(0),
            stdout: "cafe123 subject\n".to_string(),
            stderr: String::new(),
            truncated: false,
            timed_out: false,
            note: None,
            base: None,
            base_kind: String::new(),
        }
    }

    /// D3 的另一半:工具报的「我给你看了什么」必须与面板真读到的帧一致。
    /// 工具算帧、watcher 落表,谁漏了这一步,面板都会显示旧帧(或空帧),
    /// 而回执照样声称展示了命令输出——正是要杜绝的误导。
    #[tokio::test]
    async fn the_watcher_files_the_frame_under_its_thread() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let frames: ViewFrames = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let git_diff = crate::git_diff_tool::GitDiffHandle::new();
        let pump = tokio::spawn(pump_git_diff_notifications(
            git_diff.subscribe(),
            Arc::clone(&hub),
            Arc::clone(&frames),
        ));

        git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
            thread_id: Some("thread-1".into()),
            command: Some("git log --oneline -1".into()),
            note: Some("here".into()),
            frame: Some(Box::new(frame("git log --oneline -1"))),
        });

        let mut filed = None;
        for _ in 0..200 {
            if let Some(f) = frames.lock().await.get("thread-1").cloned() {
                filed = Some(f);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        pump.abort();
        let filed = filed.expect("the watcher must file the frame the tool computed");
        assert_eq!(filed.command.as_deref(), Some("git log --oneline -1"));
        assert_eq!(filed.as_kind, "log");
    }

    /// 无 thread 的焦点(前端按当前会话处理)不进表:键都没有,写进去只会有个
    /// 取不出来的孤儿条目。
    #[tokio::test]
    async fn a_frameless_or_threadless_focus_files_nothing() {
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let frames: ViewFrames = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let git_diff = crate::git_diff_tool::GitDiffHandle::new();
        let pump = tokio::spawn(pump_git_diff_notifications(
            git_diff.subscribe(),
            Arc::clone(&hub),
            Arc::clone(&frames),
        ));

        git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
            thread_id: None,
            command: None,
            note: None,
            frame: Some(Box::new(frame("git log"))),
        });
        git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
            thread_id: Some("thread-2".into()),
            command: None,
            note: None,
            frame: None,
        });
        // 让守望者把两条都处理完,再断言表是空的。
        let mut drained = false;
        for _ in 0..200 {
            if git_diff.subscribe().try_recv().is_err() && frames.lock().await.is_empty() {
                drained = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        pump.abort();
        assert!(drained, "the frame table must stay empty");
        assert!(frames.lock().await.is_empty());
    }
}
