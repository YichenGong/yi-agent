//! 插件查询通道：socket 就绪探测、查询本身、以及常驻 watch 循环。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。`plugin/*` 的 `handle` 在
//! `rpc/plugin.rs`，本文件只放**宿主侧**的查询原语。

use std::path::{Path, PathBuf};
use std::time::Duration;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::wait_for_plugin_query_socket_within;

/// `plugin/install` 等插件查询通道就绪的上限。
///
/// 装完清单、开了开关、拉起 daemon 之后，插件进程还要过两关才可查：daemon 的
/// 监督循环（500ms 一轮）把它登记进转发表，插件自己要 bind 查询 socket。只等
/// daemon 的 `Status` 就回包，调用方紧接着发 `plugin/query` 会撞上
/// `plugin_unavailable`——桌面「建看板」正是这个顺序。
///
/// 5s 覆盖「冷启动 + 一轮监督 + bind」，又短到一次失败的安装不会看起来像挂死。
pub(crate) const PLUGIN_READY_TIMEOUT: Duration = Duration::from_secs(5);
/// 插件清单声明的查询 socket 现在是否有人监听。
///
/// 判据只有一条：能不能 `connect` 上。Unix socket 的 `connect` 成功当且仅当有
/// 进程在 `listen`；陈旧 socket 文件会 `ECONNREFUSED`，故不会被误认。
///
/// 纯路径/清单输入，不含任何插件名：宿主只说「这份清单声明的通道通不通」。
pub(crate) fn plugin_query_socket_ready(
    project: &Path,
    manifest: &yi_agent_supervisors::manifest::SupervisorManifest,
    connect: &dyn Fn(&Path) -> bool,
) -> bool {
    let layout = yi_agent_supervisors::supervisor::Layout::for_workdir(project);
    match manifest.query_socket_path(
        &layout.workdir,
        &layout.state_dir(&manifest.name),
        &layout.runtime_dir,
    ) {
        Some(Ok(socket)) => connect(&socket),
        // 没声明查询通道：对调用方来说就是「立刻可查」——没有东西要等。
        None => true,
        // 路径深到回退后仍放不下：插件不会在那个位置 bind，等下去只会白等。
        Some(Err(_)) => true,
    }
}
/// 等插件查询通道就绪，最多 [`PLUGIN_READY_TIMEOUT`]。
///
/// 阻塞调用（含 sleep），调用方放进 blocking 池。返回是否在期限内就绪；
/// 超时**不是错误**——清单与开关已落盘，常驻监督会继续重试。
pub(crate) fn wait_for_plugin_query_socket(
    project: &Path,
    manifest: &yi_agent_supervisors::manifest::SupervisorManifest,
) -> bool {
    wait_for_plugin_query_socket_within(project, manifest, PLUGIN_READY_TIMEOUT)
}
/// Why a plugin-channel query produced no answer.
///
/// `code` is the stable string the UI branches on: "the daemon is down" is a
/// different remedy from "the plugin is not installed", and a bare message
/// collapsed the reasons into one. The host attaches no board meaning here —
/// whether any given answer exists is the daemon's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginQueryError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}
impl PluginQueryError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for PluginQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}
/// Ask a supervised plugin a question and return its answer verbatim.
///
/// The host attaches no meaning to `method` or `params`, and unwraps no shape
/// here: this is a generic channel, so the UI (not the server) owns what any
/// particular field means.
///
/// `project` is both where the plugin is installed and where its daemon runs:
/// the manifest lives at `<project>/.yi-agent/supervisors/`, the socket at
/// `<project>/.yi-agent/runtime/`, and the daemon is the project's own. Using the
/// app's cwd instead made every query for another project hit the wrong socket
/// (or none) — the "clicked and nothing happened" bug.
///
/// Deliberately **not** gated on any registry: whether an answer exists is the
/// socket's business, not the host's. Gating it on a project-level registry made
/// the generic channel refuse projects the plugin could have served, and left the
/// host owning plugin-specific vocabulary.
pub(crate) fn plugin_query(
    project: &Path,
    method: &str,
    plugin: &str,
    params: serde_json::Value,
    gate: Option<&crate::plugin_gate::Gate>,
) -> Result<serde_json::Value, PluginQueryError> {
    if plugin.is_empty() {
        return Err(PluginQueryError::new(
            "invalid_params",
            "plugin/query needs a `plugin` name",
        ));
    }
    // 先看这个插件是不是被自己的 flag 关掉了。daemon 停着时下面必然拿到
    // "连接失败"，那句话会把用户引向"插件坏了"；这里给的是"去设置页打开"。
    //
    // 通用：只认清单的 `switch_key`，不认插件名。没有清单（未安装）时不判定，
    // 交给后面的 `plugin_unavailable`——"没装"和"关着"是两件事。
    //
    // `plugin_query` 是自由函数、拿不到 `AppState`，故构建类型由调用方传：
    // 测试传 `None`（不判定），生产传进程级真实值。
    if let Some(switch_key) = gate.and_then(|gate| gate.disabled(project, plugin)) {
        return Err(PluginQueryError::new(
            "plugin_disabled",
            format!("this feature is disabled; turn on `{switch_key}` in Settings > Experimental"),
        ));
    }
    // The daemon that runs the plugin lives at the project root, which is also
    // what the supervised daemon dials.
    // 复用共享 helper 而不是手写 `.yi-agent/runtime`：它先认 `YI_AGENT_RUNTIME_DIR`，
    // 与工作区其它地方对「runtime socket 在哪」保持同一个定义。
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(project);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        PluginQueryError::new(
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
        PluginQueryError::new(
            "daemon_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        // A daemon that is up but has no answer for this plugin is the plugin's
        // absence, not the daemon's: the wording keeps the shape the desktop's
        // `pluginIsUnavailable` matches on.
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(PluginQueryError::new(
            "plugin_unavailable",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(PluginQueryError::new(
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
) -> Result<serde_json::Value, PluginQueryError> {
    if plugin.is_empty() {
        return Err(PluginQueryError::new(
            "invalid_params",
            "plugin/settings needs a `plugin` name",
        ));
    }
    let summary = list_installed_plugins(scope, None)
        .into_iter()
        .find(|summary| summary.name == plugin);
    let Some(summary) = summary else {
        return Err(PluginQueryError::new(
            "plugin_not_installed",
            format!("plugin {plugin} is not installed"),
        ));
    };
    if !summary.queryable {
        return Err(PluginQueryError::new(
            "plugin_unavailable",
            format!("plugin {plugin} does not declare a query socket"),
        ));
    }
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(scope);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        PluginQueryError::new(
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
        PluginQueryError::new(
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
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(PluginQueryError::new(
            "plugin_rejected",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(PluginQueryError::new(
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
    /// 这个插件此刻是否被自己的 flag 关着（`false` = 关着）。
    ///
    /// 有 `Gate` 时按真实开关算；没有时（拿不到构建类型的调用方）恒为
    /// `true`——"不知道"绝不能显示成"关着"，那会让设置页凭空报一片停用。
    pub enabled: bool,
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
pub(crate) fn list_installed_plugins(
    scope: &Path,
    gate: Option<&crate::plugin_gate::Gate>,
) -> Vec<PluginSummary> {
    let dir = scope.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| {
            let switch_key = manifest.switch_key;
            // 判定用的是**清单自己**的 switch_key，与 `plugin/query` 同一口径：
            // 宿主不认识插件名，只认识它声明的开关。
            let disabled = gate.is_some_and(|gate| {
                !crate::plugin_gate::flag_is_on(&gate.flags_path, &switch_key, gate.build_kind)
            });
            PluginSummary {
                name: manifest.name,
                queryable: manifest.query_socket.is_some(),
                switch_key,
                enabled: !disabled,
            }
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
/// 后台自愈循环:app 打开期间每 ~30s 一次,与常驻 watchman 重叠也无害(幂等)。
///
/// 单轮逻辑复用 `yi_agent_supervisors::watch::once`(读通用登记 → 交给 `ensure`),
/// **不再另写一份**「读登记 → 拉起」的胶水;`ensure_daemons` 同样是 CLI 与
/// watchman 共用的那一份。`resident_dir` 由调用方传入,循环内不重读 `HOME`。
///
/// 它是通用的:登记表里**任何**要求常驻的项目都会被确保有一个活着的进程,
/// 与是哪个插件无关。
pub(crate) fn spawn_resident_watch_loop(
    resident_dir: PathBuf,
    bringup: std::sync::Arc<yi_agent_subagent::binding::BringUpChoice>,
) {
    // The resident directory *is* the instance home (`paths::instance_home`), so the
    // retirement marker sits beside the registration it governs. Deriving it instead
    // of re-reading `YI_AGENT_HOME` also keeps the injected dir used in tests.
    let instance_home = resident_dir.clone();
    // Whether this process owns an embedded daemon, decided once from the entry
    // point's own declaration. A managed frontend already has the guardian's
    // daemon; starting an embedded `daemon serve` here would make the two bind the
    // same socket, and the guardian's daemon is the one that loses -- every later
    // conversation then runs without delegation because its project can never
    // reach `managedReady`. Reading it outside the loop is deliberate: the entry
    // point cannot change while the process lives, and a per-tick read would make
    // the two branches look interchangeable.
    // Read from the same snapshot the attach path uses. Asking the entry point's
    // declaration a second time is how watch and attach came to disagree: an env
    // override would flip one and not the other.
    let embedded = bringup.policy == yi_agent_subagent::attach::BringUpPolicy::Embedded;
    tokio::spawn(async move {
        loop {
            // 两道闸门都在 `once_unattended_as` 里：进程级（本前端是不是 managed）
            // 与实例级（迁移标记）。前者是本轮补的——一个**全新**实例没有迁移标记，
            // 却已经是 managed，只看标记就会让它在这里起一个 embedded daemon，与
            // guardian 的 daemon 抢同一个 socket。登记仍然读，但「无人时应运行」的旧
            // 语义已被 managed 生命周期取代。循环**不退出**，因为迁移标记可以在同一
            // 进程存活期间写入（用户在 App 里确认迁移），退出会让状态永远停在旧判断上。
            yi_agent_supervisors::watch::once_unattended_as(
                &instance_home,
                &resident_dir,
                embedded,
                &mut |projects| {
                    yi_agent_supervisors::watch::ensure_daemons(projects);
                },
            );
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}
