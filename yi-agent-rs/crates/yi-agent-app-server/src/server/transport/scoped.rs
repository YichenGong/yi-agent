//! scoped / relay / stdio 三种 serve 入口。

use crate::pairing::PairingState;
use crate::protocol::Scope;
use crate::workspace_index::WorkspaceIndex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
use yi_agent_runtime::config::RuntimeConfig;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    BuiltAgent, ClientInitialized, ClientScopes, ClientShellAuthorizations, PERMISSION_TIMEOUT,
    ProjectRuntimes, RuntimeAttachments, ThreadRoots, home_dir, install_device_registry,
    instance_home_dir, migrate_legacy_model_keys_on_startup,
    migrate_legacy_resident_requests_on_startup, production_factory, production_watchman_install,
    production_watchman_uninstall, pump_stdout, read_lines, serve, skills_home_dir,
    spawn_resident_watch_loop,
};

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
    // Resolved once at this entry point and handed to every consumer that has to
    // agree with it (watch loop, launcher, driver tick).
    let bringup = std::sync::Arc::new(yi_agent_subagent::attach::process_bringup_choice());
    spawn_resident_watch_loop(resident_dir.clone(), std::sync::Arc::clone(&bringup));
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
            managed: Arc::new(StdMutex::new(None)),
            // The same snapshot the watch loop above was started with: one
            // resolution per entry point, shared with every attach/install.
            bringup: std::sync::Arc::clone(&bringup),
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
            // 实例状态根：插件生命周期/授权表/安装登记都在它下面。
            instance_home: instance_home_dir(),
            remote_dir: instance_home_dir(),
            side_registry: None,
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            // 官方目录文件：按连接注入，不读进程环境变量（见字段文档）。
            plugin_catalog: crate::rpc::plugin_management::default_catalog_path(),
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
    // Resolved once at this entry point and handed to every consumer that has to
    // agree with it (watch loop, launcher, driver tick).
    let bringup = std::sync::Arc::new(yi_agent_subagent::attach::process_bringup_choice());
    spawn_resident_watch_loop(resident_dir.clone(), std::sync::Arc::clone(&bringup));
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
            managed: Arc::new(StdMutex::new(None)),
            // The same snapshot the watch loop above was started with: one
            // resolution per entry point, shared with every attach/install.
            bringup: std::sync::Arc::clone(&bringup),
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
            // 实例状态根：插件生命周期/授权表/安装登记都在它下面。
            instance_home: instance_home_dir(),
            remote_dir: instance_home_dir(),
            side_registry: None,
            theme,
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            watchman_install: production_watchman_install(),
            watchman_uninstall: production_watchman_uninstall(),
            watchman_home: home_dir(),
            // 官方目录文件：按连接注入，不读进程环境变量（见字段文档）。
            plugin_catalog: crate::rpc::plugin_management::default_catalog_path(),
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
        managed_prepare_handle(),
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
        // 手工 `app-server` 与测试都没有壳标记,于是拿到 `None`,语义不变。
        managed_prepare_handle(),
    )
    .await
}

/// 本地 stdio 的管理面:只有壳启动的 sidecar 才有。
///
/// 判断依据是壳在 spawn 前写进环境的标记。手工 `yi-agent app-server`、
/// `--listen`、relay 桥与全部测试都拿不到它,因此**不会**凭空多出一个
/// 「永远等在未认证状态」的会话——那种会话会把所有业务帧静默挡掉。
fn managed_prepare_handle() -> Option<Arc<crate::server::local_management::ManagementHandle>> {
    management_surface(crate::server::local_management::requested_by_launcher())
}

/// `requested` 为真时才给管理面,且**测试构建恒不给**。
///
/// 上一版直接信任 `requested_by_launcher()`,而它读的是**进程环境**:在
/// yi-agent 自己的会话里跑 `cargo test`,测试进程会继承壳写进去的
/// `YI_AGENT_LOCAL_MANAGEMENT=1`,于是 `serve_stdio` 凭空多出一个未认证的
/// 管理面,所有业务帧被 bootstrap 闸挡成
/// `the local session has not completed its bootstrap`
/// (`agent_factory_failure_returns_internal_error` 实测因此恒失败)。这条
/// 路径的承诺本就是「全部测试都拿不到它」,所以测试构建里短路掉,而不是让
/// 每个用例去清环境。管理面自己的用例一律经 `Harness::with_management`
/// 显式注入,不经过这里。
fn management_surface(
    requested: bool,
) -> Option<Arc<crate::server::local_management::ManagementHandle>> {
    if cfg!(test) || !requested {
        return None;
    }
    Some(Arc::new(
        crate::server::local_management::ManagementHandle::new(Arc::new(
            crate::server::local_management::ManagedPrepareBackend,
        )),
    ))
}
pub(crate) fn parse_relay_url(relay_url: &str) -> anyhow::Result<(String, String)> {
    yi_agent_relay::parse_relay_parts(relay_url)
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
    management: Option<Arc<crate::server::local_management::ManagementHandle>>,
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
        management,
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
pub(crate) async fn serve_scoped_core<R, W, F>(
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
    management: Option<std::sync::Arc<crate::server::local_management::ManagementHandle>>,
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
        management.clone(),
    ));
    // scope 随连接登记一次;主循环按 `client` id 查它。stdio 会话只此一条,故
    // 传入的表就是一张只装 `local` 的注册表(与 ws 的动态注册同型,便于共享)。
    let client_scopes: ClientScopes =
        Arc::new(Mutex::new(HashMap::from([(local.clone(), client_scope)])));
    let client_initialized: ClientInitialized = Arc::new(Mutex::new(HashMap::new()));
    let client_shell_authorizations: ClientShellAuthorizations =
        Arc::new(Mutex::new(HashMap::new()));

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
        client_shell_authorizations,
        stdio_eof_rx.take(),
        relay_url_for_targets,
        management,
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

#[cfg(test)]
mod managed_surface_tests {
    use super::*;

    /// 测试构建**恒不**从环境派生管理面。
    ///
    /// 回归：一次真实的 `just ci` 失败——在 yi-agent 自己的会话里跑测试,进程
    /// 继承了壳的 `YI_AGENT_LOCAL_MANAGEMENT=1`,`serve_stdio` 于是多出一个
    /// 未认证的管理面,`thread/start` 被 bootstrap 闸挡掉,
    /// `agent_factory_failure_returns_internal_error` 拿到的拒绝体只有字符串
    /// `error`、没有数字 `code`(实测 `left: Null, right: -32603`)。
    #[test]
    fn a_test_build_never_invents_a_management_surface() {
        assert!(
            management_surface(true).is_none(),
            "a test build must not derive a management surface from the \
             inherited environment; management tests inject one explicitly"
        );
    }

    /// 没有壳标记时不给管理面——这条在两种构建里都成立。
    ///
    /// 它校验的是入参这一侧；测试构建里 `cfg!(test)` 恒真，release 侧
    /// 「标记为真就给」的分支只能靠上面的用例间接保证，故不在此重复断言。
    #[test]
    fn no_marker_means_no_management_surface() {
        assert!(management_surface(false).is_none());
    }
}
