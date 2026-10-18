//! RPC 方法到域 handler 的分派。每个域一个子模块,`server.rs` 只留 transport
//! 与主循环。
//!
//! 背景:`serve()` 里的 `match method.as_str()` 曾把 62 个域方法的内联臂全部
//! 堆在一个 3,000 余行的函数里,新 handler 无处可放。Task 3 把这些臂按域搬进
//! 本模块的子模块,**逐字搬移**(只改状态的取名路径),`serve()` 只保留
//! `initialize`、transport 与兜底 `_ =>`。
//!
//! 为什么 handler 不是纯 `-> Result<Value>` 函数:这些臂在应答时直接写传输层
//! (`write_response(&hub, &client, ...)` 在搬移前出现 189 次 —— 188 次在域臂内,
//! 第 189 次在兜底 `_ =>` 臂),并且靠 `continue` 结束本轮循环(125 次)。故每个
//! handler 收一份 [`AppState`],借到主循环的局部量与响应出口,并返回一个
//! [`Flow`] 告诉 `serve()` 该不该 `continue`。

// 域子模块:每个模块承载一个域,臂体逐字搬移自 `serve()` 的 dispatch。
pub(crate) mod agent;
pub(crate) mod board;
pub(crate) mod image;
pub(crate) mod mcp;
pub(crate) mod model;
pub(crate) mod onboarding;
pub(crate) mod pair;
pub(crate) mod plugin;
pub(crate) mod process;
pub(crate) mod resident;
pub(crate) mod skill;
pub(crate) mod thread;
pub(crate) mod turn;
pub(crate) mod ui;
pub(crate) mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::broadcast::{Broadcaster, ClientId};
use crate::pairing::PairingState;
use crate::protocol::Scope;
use crate::session::ThreadSession;
use crate::theme_tool::ThemeHandle;
use crate::workspace_index::WorkspaceIndex;
use yi_agent_subagent::binding::RuntimeBinding;
use yi_agent_subagent::thread_root::ThreadRoot;

/// 值守安装句柄。与 `server.rs` 里的 `pub(crate) type WatchmanInstall` 是**同一个**
/// 类型(同一个 `Arc<dyn Fn ..>`),在对齐处互换无碍。`ui/*` 与 `board/*` 都从那
/// 一份类型走(本模块不再重复声明 `BoardLauncher`,它只被 detach 后的
/// [`BoardPluginState`] 用到,直接引 `crate::server::BoardLauncher`)。
pub(crate) type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;
/// 值守卸载句柄,同 [`WatchmanInstall`]。
pub(crate) type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;

/// 项目的共享 runtime 登记表(按 canonical cwd 键)。与 `server.rs` 里的私有
/// `type ProjectRuntimes` 是**同一个**类型(同一个 `Arc<StdMutex<HashMap<..>>>`),
/// 在对齐处互换无碍——与 [`WatchmanInstall`] 同一手法。`agent/*` 与 `thread/*`
/// 的臂把它转发给 daemon 查询 helper(`socket_for_thread` 等)。
pub(crate) type ProjectRuntimes =
    Arc<StdMutex<HashMap<PathBuf, Result<Arc<RuntimeBinding>, String>>>>;

/// 每会话自身的 root 表(建在项目共享 binding 之上)。同 [`ProjectRuntimes`],
/// 与 `server.rs` 的同名私有别名是同一个类型。
pub(crate) type ThreadRoots = Arc<StdMutex<HashMap<String, Arc<ThreadRoot>>>>;

/// 会话 agent 工厂:`serve()` 的 `build_agent` 参数。`thread/start`(建新 agent)与
/// `thread/resume`(按历史重建)两个臂各调用一次。
///
/// 存的是**裸 trait 对象引用**而非 `&F`:AppState 刻意不带泛型参数(13 个域的
/// `handle` 签名必须一致),而 `&F` 恰好在 `app_state!()` 那一个调用点自动 unsize
/// 成 `&dyn Fn(..)` —— 同一个工厂、同一个调用,中间没有转接层。
pub(crate) type BuildAgent<'a> = &'a (
        dyn Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<crate::server::BuiltAgent>
            + Send
            + Sync
    );

/// 一个域 handler 的返回值:是否已经处理完本轮请求。
///
/// 搬移前,域臂用 `continue` 结束本轮主循环。搬出后 handler 不能再影响外层
/// 循环,于是把这个意图变成显式的返回值,由 `serve()` 翻译回同一个动作:
///
/// - [`Flow::Continue`] —— 本域已应答/已处理,**主循环回下一轮**(= 搬移前的
///   `continue`)。已搬移的域在 `serve()` 里被无条件翻成 `continue`。
/// - [`Flow::Unknown`] —— 方法不在本域内(域内 `_` 兜底)。`serve()` 据此把请求
///   交回**原始** `_ =>` dispatch,`method_not_found` 的响应逐字节不变。
///
/// 域臂里没有 `break`;主循环体内仅有的 3 处 `break` 都在臂**内层**的 `for`
/// 循环里(如 `thread/resume` 的重放块),搬移后仍是内层循环的 `break`,故本
/// 信号无须 `Break` 变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    /// 本域已处理,主循环回下一轮(等价于搬移前的 `continue`)。
    Continue,
    /// 方法不属于本域,交回 `serve()` 的原始 dispatch。
    Unknown,
}

/// [`Domain::Board`] 与 [`Domain::Plugin`] 的 handler 所需的全部状态。
///
/// 与 [`AppState`] 的**关键区别**：这里没有一个 `&mut`，全是 owned/`Arc`，故整份
/// 可 move 进 `tokio::spawn` 的任务。这两个域因此可以脱离主循环执行（主循环是
/// 单 task，内联 `await` 会串行堵住所有 thread 的 RPC）。
pub(crate) struct BoardPluginState {
    pub(crate) hub: std::sync::Arc<crate::broadcast::Broadcaster>,
    pub(crate) client: crate::broadcast::ClientId,
    pub(crate) board_dir: std::path::PathBuf,
    pub(crate) resident_dir: std::path::PathBuf,
    pub(crate) plugin_command: std::path::PathBuf,
    pub(crate) theme_handle: crate::theme_tool::ThemeHandle,
    pub(crate) watchman_home: std::path::PathBuf,
    pub(crate) watchman_install: crate::server::WatchmanInstall,
    /// 卸载句柄。本域无方法卸载值守(`board/remove` 只停守护进程),故当前无处
    /// 读它——但它是 brief 声明的接口字段,与安装句柄成对,保留以免日后加
    /// 「卸载值守」方法时又要动 `BoardPluginState` 的形状。与 `BoardCard` 的
    /// `kind` / `source` / `base` 同一手法。
    #[allow(dead_code)]
    pub(crate) watchman_uninstall: crate::server::WatchmanUninstall,
    pub(crate) board_launcher: crate::server::BoardLauncher,
    /// detach 之后唯一需要回到主循环的信号：stdio `local` 写失败是致命的
    /// （`driver_reports_finished_when_writer_fails` 钉死），而 detached 任务
    /// 无法用 `?` 把它抛回去。
    pub(crate) fatal_tx: tokio::sync::mpsc::Sender<anyhow::Error>,
}

/// 写一条响应；写失败（仅 stdio `local` 会失败）经 `fatal_tx` 上报主循环。
///
/// `_id` 保留在签名里（brief 的接口），但回包 `resp` 自身已携带 request id，
/// 故函数体无需读它。
pub(crate) async fn write_or_fatal(
    state: &BoardPluginState,
    _id: crate::protocol::RequestId,
    resp: crate::protocol::ResponseEnvelope,
) {
    if let Err(error) = crate::server::write_response(&state.hub, &state.client, resp).await {
        let _ = state.fatal_tx.send(error).await;
    }
}

/// 一个 RPC 方法所属的域。`serve()` 先按它选模块,再由模块内的
/// `match req.method.as_str()` 挑具体方法。
///
/// [`Domain::Unknown`] 只覆盖 `initialize` 与未识别前缀,让它们照旧走
/// `serve()` 的原始 `match` —— `initialize` 要动 `serve()` 私有的 `initialized`
/// 表,不适合外移;而兜底 `_ =>` 必须留在原处才能保住原样的
/// `method_not_found` 响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    /// `ui/*` 与 `config/*`(config/read)。
    Ui,
    /// `model/*`。
    Model,
    /// `onboarding/*`。
    Onboarding,
    /// `resident/*`(通用常驻登记:哪些项目要常驻 daemon)。
    Resident,
    /// `board/*`。
    Board,
    /// `plugin/*` 与 `plugins/*`。
    Plugin,
    /// `image/*`(含分片上传)。
    Image,
    /// `workspace/*`。
    Workspace,
    /// `process/*`。
    Process,
    /// `pair/*` 与 `device/*`(共享同一 `Arc<PairingState>`,故同域)。
    Pair,
    /// `turn/*`。
    Turn,
    /// `skill/*`(技能面板:三层技能根枚举 + 按需读正文)。
    Skill,
    /// `mcp/*`(设置页 MCP 面板:项目级 mcp.json + 工具 schema 缓存的只读视图)。
    Mcp,
    /// `thread/*`(会话生命周期:列表、建/恢复、改名、模型档、置顶、删除、订阅)。
    Thread,
    /// `agent/*`(子代理列表、轨迹读写、消息、两段式取消)。
    Agent,
    /// 未搬移者:只剩 `initialize` 与未识别前缀(全部域都已搬走)。
    Unknown,
}

/// 把一个 RPC 方法名映射到它的域。
///
/// 取方法名 `/` 之前的那一段作为域前缀:`ui/*`、`thread/*`、`agent/*` 等的前缀
/// 都是其首段。`config/read` 例外地并入 [`Domain::Ui`](机器级宿主偏好,与
/// `ui/settings/*` 同处一个 preferences.json)。所有尚未搬移的前缀一律落到
/// [`Domain::Unknown`],`serve()` 照旧处理。
pub(crate) fn domain_for(method: &str) -> Domain {
    match method.split_once('/').map(|(head, _)| head) {
        Some("ui") | Some("config") => Domain::Ui,
        Some("model") => Domain::Model,
        Some("onboarding") => Domain::Onboarding,
        Some("resident") => Domain::Resident,
        Some("board") => Domain::Board,
        Some("plugin") | Some("plugins") => Domain::Plugin,
        Some("image") => Domain::Image,
        Some("workspace") => Domain::Workspace,
        Some("process") => Domain::Process,
        Some("pair") => Domain::Pair,
        Some("device") => Domain::Pair,
        Some("turn") => Domain::Turn,
        Some("skill") => Domain::Skill,
        Some("mcp") => Domain::Mcp,
        Some("thread") => Domain::Thread,
        Some("agent") => Domain::Agent,
        _ => Domain::Unknown,
    }
}

/// 域 handler 需要的主循环局部量的**按请求借用**。
///
/// 在 `serve()` 里用 `app_state!()` 宏就地构造,只借用本轮 dispatch 需要的
/// 局部量,借到臂末(通常是 `continue`)为止 —— 故主循环各局部量的所有权与
/// 借用生命周期与搬移前完全一致,本结构**不**改变任何状态的所有权,也不引入
/// 新的共享状态。
///
/// 字段分三层:响应出口与调用方、路径与宿主偏好(共享只读)、注入句柄,外加
/// 主循环的可变局部量(按域分批搬入,见 `batch 3` 一节)与 `batch 4` 新增的
/// 宿主路径 / 组长信箱一组。三层里已全部有主:域 handler 需要的量都已在结构里
/// 就位。唯一的构造点是 `serve()` 里的 `app_state!()` 宏,它按**字段名**逐项写出
/// 32 个字段(见 [`AppState`] 各字段的注释),故加一个字段只需改该宏一处;同类型
/// 的相邻字段因逐名写出而不会错位 —— 四个相邻的 `&Path`(`models_path` 到
/// `onboarding_preferences_path`)尤其需要这条保证。
///
/// Task 4 把 `board/*` 与 `plugin/*` detach 出主循环后,这两域不再经本结构借
/// 主循环局部量:原来只被它们读的 `board_dir` / `resident_dir` / `plugin_command` /
/// `board_launcher` 已随处理搬进 [`BoardPluginState`],故从本结构移除(留着就是
/// 死字段)。
pub(crate) struct AppState<'a> {
    // ---- 响应出口与调用方 ----
    /// 广播出口:handler 用它定向回包(`write_response`)与做订阅
    /// (`hub.subscribe`)。搬移前臂里的 `hub`。
    pub(crate) hub: &'a Arc<Broadcaster>,
    /// 当前请求来自哪个客户端:所有定向应答的寻址目标。搬移前臂里的 `client`。
    pub(crate) client: &'a ClientId,
    /// 调用方已登记的权限级(`Observe < Control < Admin`)。`serve()` 在
    /// dispatch 前查好,handler 用它做臂内的门禁(如 `model/*` 的写操作)。
    /// 搬移前臂里的 `client_scope`。
    pub(crate) client_scope: Scope,

    // ---- 路径与宿主偏好(共享只读) ----
    /// 生效的运行时配置。搬移前臂里的 `cfg`。
    pub(crate) cfg: &'a RuntimeConfig,
    /// 主题句柄:`ui/settings/*` 读写它,`plugin/*` 用它取 workdir。
    /// 搬移前臂里的 `theme_handle`。
    pub(crate) theme_handle: &'a ThemeHandle,
    /// 机器级模型清单路径(`~/.yi-agent/models.json`)。搬移前臂里的
    /// `models_path`。
    pub(crate) models_path: &'a Path,
    /// 全局 `.env` 路径,供 `onboarding/apply` 落盘。搬移前臂里的
    /// `onboarding_env_path`。
    pub(crate) onboarding_env_path: &'a Path,
    /// 引导是否已关闭的标记文件路径。搬移前臂里的
    /// `onboarding_preferences_path`。
    pub(crate) onboarding_preferences_path: &'a Path,
    /// 值守安装/卸载所针对的 home。搬移前臂里的 `watchman_home`。
    pub(crate) watchman_home: &'a Path,
    /// 通用常驻登记目录（`$HOME/.yi-agent`）。`resident/list` 的唯一输入，
    /// 与值守读登记表用的是同一份，故同属「路径与宿主偏好」这一层，按 `&` 借用。
    pub(crate) resident_dir: &'a Path,

    // ---- 注入句柄(测试可替身) ----
    /// 安装常驻值守。搬移前臂里的 `watchman_install`。
    pub(crate) watchman_install: &'a WatchmanInstall,
    /// 卸载常驻值守。`ui/*` 的卸载臂用它。搬移前臂里的 `watchman_uninstall`。
    pub(crate) watchman_uninstall: &'a WatchmanUninstall,

    // ---- 主循环的可变局部量(按域逐批搬入) ----
    /// 分片上传登记表(`image/upload/*`)。搬移前臂里的 `uploads`,按 `&mut` 借用
    /// —— 臂内 `begin` / `append` / `commit` / `abort` 都要改它。
    pub(crate) uploads: &'a mut crate::image_upload::UploadRegistry,
    /// 内存中的会话表(`threads`)。`image/*` 经 `thread_cwd` 查 cwd,`turn/*` 读
    /// 会话并投递中断/追加,`process/*` 取该 thread 的进程管理器。搬移前臂里的
    /// `threads`,按 `&mut` 借用(`turn/start` 经 `start_turn_core` 要 `&mut`)。
    pub(crate) threads: &'a mut HashMap<String, ThreadSession>,
    /// 附加到 thread 的运行时登记(设备/工作区开关)。`image/*` 经 `thread_cwd`
    /// 冷回退时要用,`workspace/*` 直接增删。搬移前臂里的 `workspaces`。
    pub(crate) workspaces: &'a Arc<WorkspaceIndex>,
    /// 配对码与设备注册表:`pair/create` 铸码,`pair/redeem` 兑成设备 token,
    /// `device/list`/`device/revoke` 读写同一张表(与 ws 升级时的 `?pair=` 等价)。
    /// 搬移前臂里的 `pairing`,由调用方注入。
    pub(crate) pairing: &'a Arc<PairingState>,
    /// attach 产出、待该 thread 的首个 turn 激活的 runtime。`turn/start` 经
    /// `start_turn_core` 读取它。搬移前臂里的 `pending_activation`;batch 3 改为按
    /// `&mut` 借用——`thread/start` 经 `start_thread_core` 写入、`thread/resume`
    /// 直接 `insert`,两处都要独占(`turn/start` 那条只读路径由 `&mut` 自动重借)。
    pub(crate) pending_activation: &'a mut HashMap<String, Option<Arc<ThreadRoot>>>,

    // ---- batch 3:`thread/*` 与 `agent/*` 依赖的主循环局部量 ----
    /// 项目共享 runtime 登记表。`thread/*` 与 `agent/*` 都经 daemon IPC helper
    /// (`socket_for_thread` / `forward_to_daemon` / `detach_unused_runtimes`)查它;
    /// `thread/start`、`thread/resume` 还把它交给 `start_thread_core` /
    /// `attach_delegation`。搬移前臂里的 `runtimes`;只读借用(`Arc` 内部自锁)。
    pub(crate) runtimes: &'a ProjectRuntimes,
    /// 每会话自己的 root 表。`thread/resume` 经 `attach_delegation` 挂 root,
    /// `thread/delete` 摘掉该会话的 root 并 `detach`。搬移前臂里的 `thread_roots`。
    pub(crate) thread_roots: &'a ThreadRoots,
    /// 每 thread 一个进程状态守望者。`thread/start`/`thread/resume`/`thread/delete`
    /// 增删它,`agent/trace/unwatch` 停它。搬移前臂里的 `process_watches`,按 `&mut`
    /// 借用。
    pub(crate) process_watches: &'a mut HashMap<String, crate::server::ProcessWatch>,
    /// 每 thread 至多一条被关注的轨迹流。`agent/trace/watch` 换它,
    /// `agent/trace/unwatch`/`thread/delete` 停它。搬移前臂里的 `trace_watches`。
    pub(crate) trace_watches: &'a mut HashMap<String, crate::server::TraceWatch>,
    /// 每 thread 至多一个子任务列表守望者。`agent/children/list` 建立它,
    /// `agent/trace/unwatch`/`thread/delete` 停它。搬移前臂里的 `children_watches`。
    pub(crate) children_watches: &'a mut HashMap<String, crate::server::ChildrenWatch>,
    /// 反向权限请求的等待登记表。`thread/start`/`thread/resume` 交给
    /// `start_thread_core`/`run_thread_driver`。搬移前臂里的 `pending`。
    pub(crate) pending: &'a Arc<tokio::sync::Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    /// 审批 id 的全局序号(跨 thread 唯一)。与 [`AppState::pending`] 同路交出去。
    /// 搬移前臂里的 `perm_seq`。
    pub(crate) perm_seq: &'a Arc<std::sync::atomic::AtomicU64>,
    /// 权限审批默认超时。与 [`AppState::pending`] 同路交出去。搬移前臂里的
    /// `permission_timeout`;按值拷贝(`Duration` 是 `Copy`)。
    pub(crate) permission_timeout: Duration,
    /// 主循环的 turn 完成事件接收端。`thread/resume` 与 `thread/delete` 经
    /// `interrupt_and_wait_for_persist` 用它等待 driver 落盘。搬移前臂里的
    /// `turn_rx`,按 `&mut` 借用。
    pub(crate) turn_rx: &'a mut mpsc::Receiver<crate::server::TurnEvent>,
    /// 主循环的 turn 完成事件发送端(driver task 用它上报)。`thread/start` /
    /// `thread/resume` 各 clone 一份给 driver。搬移前臂里的 `turn_tx`。
    pub(crate) turn_tx: &'a mpsc::Sender<crate::server::TurnEvent>,
    /// 会话 agent 工厂。`thread/start`(建新 agent)与 `thread/resume`(按历史
    /// 重建)各调用一次。搬移前臂里的 `build_agent`。
    pub(crate) build_agent: BuildAgent<'a>,
    /// Git Diff 焦点句柄。与 [`AppState::theme_handle`] 同路,交给会话的工具集。
    /// 搬移前臂里的 `git_diff_handle`。
    pub(crate) git_diff_handle: &'a crate::git_diff_tool::GitDiffHandle,
    /// 模型驱动的视图帧表(每会话一帧,内存态)。`thread/view/read` 读它、按需
    /// 重算后写回。与 [`AppState::git_diff_handle`] 同一注入理由:测试要在主循环
    /// 之外往**同一张**表里塞帧。
    pub(crate) view_frames: &'a crate::server::ViewFrames,

    // ---- batch 4:`skill/*` 与 `thread/*`(组长)新增的宿主路径与信箱 ----
    /// 技能根解析所针对的 home(生产为 `home_dir()`,`skills_home()` 可替身)。
    /// `skill/*` 经它枚举三层技能根,故与 [`AppState::cfg`] 同属宿主级只读输入;
    /// AppState 刻意不带泛型参数(13 个域的 `handle` 签名必须一致),故自带一份
    /// 引用而非依赖注入的 trait。搬移前臂里的 `skills_home`。
    pub(crate) skills_home: &'a Path,
    /// 组长信箱:会话给组长留言经 [`crate::lead::LeadInbox`] 投递。`thread/start`
    /// 与 `thread/resume` 各把它交给 `start_thread_core` / `attach_delegation`
    /// (后者转交给组长工具集),故与 [`AppState::runtimes`] 同路按 `&` 借用。
    /// 搬移前臂里的 `lead_inbox`。
    pub(crate) lead_inbox: &'a crate::lead::LeadInbox,
    /// 阻塞式 `ask_lead` 的回信登记表。`thread/start` 与 `thread/resume` 各把它
    /// 交给 `start_thread_core` / `attach_delegation`(转交给 `AskLeadTool`),
    /// 与 [`AppState::lead_inbox`] 同一实例、同一理由,故同样按 `&` 借用。
    /// 搬移前臂里的 `lead_replies`。
    pub(crate) lead_replies: &'a crate::lead::LeadReplies,
    /// 待回填的阻塞式 `ask_lead` 登记表(按组长 thread_id 归组 request_id)。
    /// `thread/resume` 与 `thread/delete` 经 `interrupt_and_wait_for_persist`
    /// 消费 `turn_rx` 时替主循环收口,故与 [`AppState::turn_rx`] 同路按 `&mut`
    /// 借用。搬移前臂里的 `pending_lead_replies`。
    pub(crate) pending_lead_replies: &'a mut HashMap<String, Vec<String>>,
}
