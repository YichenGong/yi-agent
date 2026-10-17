//! RPC 方法到域 handler 的分派。每个域一个子模块,`server.rs` 只留 transport
//! 与主循环。
//!
//! 背景:`serve()` 里的 `match method.as_str()` 曾把 58 个域方法的内联臂全部
//! 堆在一个 3,000 余行的函数里,新 handler 无处可放。Task 3 把这些臂按域搬进
//! 本模块的子模块,**逐字搬移**(只改状态的取名路径),`serve()` 只保留
//! `initialize`、transport 与兜底 `_ =>`。
//!
//! 为什么 handler 不是纯 `-> Result<Value>` 函数:这些臂在应答时直接写传输层
//! (`write_response(&hub, &client, ...)` 在搬移前出现 189 次),并且靠
//! `continue` 结束本轮循环(128 次)。故每个 handler 收一份 [`AppState`],借到
//! 主循环的局部量与响应出口,并返回一个 [`Flow`] 告诉 `serve()` 该不该
//! `continue`。

// 域子模块:每个模块承载一个域,臂体逐字搬移自 `serve()` 的 dispatch。
pub(crate) mod board;
pub(crate) mod onboarding;
pub(crate) mod model;
pub(crate) mod ui;

use std::path::Path;
use std::sync::Arc;

use yi_agent_runtime::config::RuntimeConfig;

use crate::broadcast::{Broadcaster, ClientId};
use crate::protocol::Scope;
use crate::theme_tool::ThemeHandle;

/// 值守安装句柄。与 `server.rs` 里的私有 `type WatchmanInstall` 是**同一个**
/// 类型(同一个 `Arc<dyn Fn ..>`),在对齐处互换无碍——故这里重声明一份,省下
/// 「为搬移改 `server.rs` 的可见性」这一步。
pub(crate) type WatchmanInstall = Arc<dyn Fn(&Path, &Path) -> Result<(), String> + Send + Sync>;
/// 值守卸载句柄,同 [`WatchmanInstall`]。
pub(crate) type WatchmanUninstall = Arc<dyn Fn(&Path) -> Result<(), String> + Send + Sync>;
/// 看板守护进程启动器句柄,同 [`WatchmanInstall`]。
pub(crate) type BoardLauncher = Arc<dyn Fn(&Path) -> Result<bool, String> + Send + Sync>;

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

/// 一个 RPC 方法所属的域。`serve()` 先按它选模块,再由模块内的
/// `match req.method.as_str()` 挑具体方法。
///
/// [`Domain::Unknown`] 覆盖 `initialize` 与所有尚未搬移的域,让它们照旧走
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
    /// `board/*`。
    Board,
    /// `plugin/*` 与 `plugins/*`。
    Plugin,
    /// 未搬移(含 `initialize`)。
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
        Some("board") => Domain::Board,
        Some("plugin") | Some("plugins") => Domain::Plugin,
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
/// 字段分三层:响应出口与调用方、路径与宿主偏好(共享只读)、注入句柄。
/// batch 2/3 会继续往这里加(`threads`、`tracked`、`runtimes`、`uploads`、
/// `process_watches`、`pairing`、`theme`、`build_agent` 等);[`AppState::new`] 是
/// 唯一的构造点,加一个字段只需改它一处和 `serve()` 的 `app_state!()`。
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
    /// 主题句柄:`ui/settings/*` 读写它,`board/create` 用它取 workdir。
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
    /// 看板注册表目录。搬移前臂里的 `board_dir`。
    pub(crate) board_dir: &'a Path,
    /// 常驻守护注册表目录。搬移前臂里的 `resident_dir`。
    pub(crate) resident_dir: &'a Path,
    /// 写进各项目 supervisor 清单的 `superpowers-kanban` 可执行文件路径。
    /// 搬移前臂里的 `plugin_command`。
    pub(crate) plugin_command: &'a Path,
    /// 值守安装/卸载所针对的 home。搬移前臂里的 `watchman_home`。
    pub(crate) watchman_home: &'a Path,

    // ---- 注入句柄(测试可替身) ----
    /// 安装常驻值守。搬移前臂里的 `watchman_install`。
    pub(crate) watchman_install: &'a WatchmanInstall,
    /// 卸载常驻值守。搬移前臂里的 `watchman_uninstall`。
    pub(crate) watchman_uninstall: &'a WatchmanUninstall,
    /// 如何为项目拉起看板守护进程。搬移前臂里的 `board_launcher`。
    pub(crate) board_launcher: &'a BoardLauncher,
}

impl<'a> AppState<'a> {
    /// 在 `serve()` 的 `app_state!()` 宏里构造:逐项借用主循环的局部量。
    ///
    /// 参数多(15 个)是「就地借用」的直接代价 —— 这里刻意不引入一层聚合结构,
    /// 免得为批量搬移再造一套与主循环局部量平行的所有权。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        hub: &'a Arc<Broadcaster>,
        client: &'a ClientId,
        client_scope: Scope,
        cfg: &'a RuntimeConfig,
        theme_handle: &'a ThemeHandle,
        models_path: &'a Path,
        onboarding_env_path: &'a Path,
        onboarding_preferences_path: &'a Path,
        board_dir: &'a Path,
        resident_dir: &'a Path,
        plugin_command: &'a Path,
        watchman_home: &'a Path,
        watchman_install: &'a WatchmanInstall,
        watchman_uninstall: &'a WatchmanUninstall,
        board_launcher: &'a BoardLauncher,
    ) -> Self {
        Self {
            hub,
            client,
            client_scope,
            cfg,
            theme_handle,
            models_path,
            onboarding_env_path,
            onboarding_preferences_path,
            board_dir,
            resident_dir,
            plugin_command,
            watchman_home,
            watchman_install,
            watchman_uninstall,
            board_launcher,
        }
    }
}
