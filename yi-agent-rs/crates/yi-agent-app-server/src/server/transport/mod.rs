//! 传输层：stdio / relay / scoped 的 serve 入口与后台 pump。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 8）。`serve()` 仍在本文件。

pub(crate) mod notifications;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, mpsc, oneshot};

use crate::pairing::PairingState;
use crate::protocol::{
    ClientResponse, Notification, PROTOCOL_VERSION, RequestEnvelope, RequestId, RpcError, Scope,
};
use crate::session::ThreadSession;
use crate::session_driver::DriverState;
use serde_json::json;
// `power_clock.now()` 是 `Clock` trait 的方法，trait 必须在作用域里。
use crate::workspace_index::WorkspaceIndex;
use yi_agent_core::permission::Decision;
use yi_agent_power::clock::Clock as _;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::thread_root::ThreadRoot;

// `serve()` 的签名与函数体用到的其余名字仍由 `server.rs` 提供。
use crate::server::{
    BuiltAgent, ChildrenWatch, ClientInitialized, ClientScopes, ClientShellAuthorizations,
    DRIVER_TICK, EVICT_IDLE_AFTER, LEAD_LOOP_SENDER, LEAD_TICK, POWER_TICK, ProcessWatch,
    RuntimeAttachments, ServeLauncher, SyncAction, TraceWatch, TurnEvent, attached_runtimes,
    decision_label, driver_tick, err_response, evict_idle_runtimes, lead_tick_once, ok_response,
    parse_resume_hints, plan_sync, resolve_lead_target, route_client_response,
    settle_pending_lead_replies, store_lookup, take_changed_digests, write_notification,
    write_response,
};
pub(crate) mod scoped;
pub(crate) mod stdio;

#[allow(unused_imports)]
pub(crate) use notifications::{pump_git_diff_notifications, pump_theme_notifications};
// `run` 与 `serve_stdio_with_relay` 由 `lib.rs` 以 `pub use` 再导出给 CLI，故这里
// 必须同样是 `pub`；其余仅在 crate 内使用。
pub(crate) use scoped::{parse_relay_url, serve_scoped, serve_scoped_core, serve_stdio};
pub use scoped::{run, serve_stdio_with_relay};
#[allow(unused_imports)]
pub(crate) use stdio::{pump_stdout, read_lines};

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
    // 每个客户端被告知的壳授权（见 [`ClientShellAuthorizations`]）。只有本地
    // stdio 能写入；远端设备的自报在 `initialize` 里被拒。
    client_shell_authorizations: ClientShellAuthorizations,
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
        // 旁支注册表:注入优先(测试),否则本函数自建(生产)。
        side_registry,
        // 结论由装配方算好：生产走 `from_process_environment`，测试直接给值。
        // `serve()` 不再解析——它看得到的 `build_kind()` 是进程级的，测试里恒为
        // `release`，在那儿解析会让注入永远失效。
        lead_agent_enabled,
    } = attachments;
    // 旁支注册表:一个进程一份。注入的那份(测试)已经带好替身 resolver;生产
    // 在这里自建,resolver 用「按 threadId 定位 store 后严格冷读」那条路径。
    let side_registry = match side_registry {
        Some(registry) => registry,
        None => {
            let pool = std::sync::Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
            let resources =
                // 实例状态根:与 `remote.json` 同一处,旁支目录落在它下面的
                // `side-conversations/`。
                yi_agent_runtime::side_conversation::SideHostResources::new(
                    remote_dir.clone(),
                    std::sync::Arc::clone(&pool),
                )
                .map_err(|error| anyhow::anyhow!("无法初始化旁支资源: {error}"))?;
            let resolver = std::sync::Arc::new(crate::side_conversation::WorkspaceResolver::new(
                std::sync::Arc::clone(&workspaces),
                cfg.clone(),
            ));
            Arc::new(crate::side_conversation::SideRegistry::new(
                resolver, resources, pool,
            ))
        }
    };
    // 旁支后台任务的统一持有者:宿主退出时 `abort_all()`,不让创建/关闭任务
    // 活过 server。
    let mut side_tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    // 旁支分页快照:一个宿主一份。连接断开时摘掉它的 token(见读循环的错误臂),
    // 关闭 side 时由注册表撤销(见 `close`)。两侧共用同一份,否则「关闭即失效」
    // 只会作用于其中一边。
    let side_replay = crate::side_conversation::shared_replay_store();
    side_registry.attach_replay(side_replay.clone()).await;
    side_registry.attach_hub(Arc::clone(&hub));

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
    let shell_authorizations = client_shell_authorizations;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();

    // 保活。**唯一持有断言的进程**就是这里（spec「为什么是单一持有者」）。
    //
    // 整条 tick 在独立线程里跑（见 `power_guard` 的模块注释）：一拍含
    // `getifaddrs`、电源来源与可能的 IOKit 调用，放主循环会和 RPC 抢时间片。
    // 只读一次盘：初值来自同一份 `Settings`，不会互相错位。
    let initial_settings = yi_agent_runtime::settings::load_from(&settings_path);
    let initial_enabled = yi_agent_runtime::settings::prevent_idle_sleep_enabled(&initial_settings);
    let (initial_grace_min, _initial_remote_min) =
        yi_agent_runtime::settings::power_windows_minutes(&initial_settings);
    let power_guard =
        crate::power_guard::PowerGuard::spawn(initial_enabled, initial_grace_min * 60);
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
                if client != crate::broadcast::ClientId::local()
                    && let Some(stamp) = power_clock.now() {
                    last_remote_touch = Some(stamp);
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
                        // 该连接的快照 token 随之作废:没人能再续页,继续钉住
                        // item 只会白占宿主 128 MiB 额度。
                        side_replay.lock().await.release_connection(client.as_str());
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
                // 本次请求的壳授权（只有本地 stdio 的 `initialize` 写得进）。
                // 每轮现取而不是缓存到循环外：同一连接上 `initialize` 与随后的
                // `runtime/status` 到达顺序由客户端决定，缓存的副本可能是握手前的
                // 空值，那样状态视图会无缘无故降级成 `Unknown`。
                let request_shell_authorization = shell_authorizations
                    .lock()
                    .await
                    .get(&client)
                    .cloned();
                macro_rules! app_state {
                    () => {
                        // 字段名逐项写出(而非位置参数):`models_path` /
                        // `onboarding_preferences_path` / `watchman_home` 三个相邻
                        // 字段同为 `&Path`,位置一旦写错照样编译通过、却会静默把
                        // 机器级路径接错。
                        crate::rpc::AppState {
                            hub: &hub,
                            client: &client,
                            is_remote: client != crate::broadcast::ClientId::local(),
                            shell_authorization: request_shell_authorization.as_ref(),
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
                            // 常驻登记目录就是实例根：迁移标记与它同处一个目录。
                            instance_home: &resident_dir,
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
                            side_registry: &side_registry,
                            side_tasks: &mut side_tasks,
                            side_replay: &side_replay,
                        }
                    };
                }

                // 16 个臂的差异只有域模块名，故用局部宏收敛成一行。
                // 不能抽成函数：`app_state!` 按名捕获 serve() 的局部量、
                // 且臂体内联 `continue`，两者都无法跨函数。
                macro_rules! dispatch_domain {
                    ($handler:path) => {{
                        let mut state = app_state!();
                        match $handler(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }};
                }

                match crate::rpc::domain_for(method.as_str()) {
                    crate::rpc::Domain::Ui => dispatch_domain!(crate::rpc::ui::handle),
                    crate::rpc::Domain::Model => dispatch_domain!(crate::rpc::model::handle),
                    crate::rpc::Domain::Onboarding => dispatch_domain!(crate::rpc::onboarding::handle),
                    crate::rpc::Domain::Resident => dispatch_domain!(crate::rpc::resident::handle),
                    crate::rpc::Domain::Runtime => dispatch_domain!(crate::rpc::runtime::handle),
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
                    crate::rpc::Domain::Image => dispatch_domain!(crate::rpc::image::handle),
                    crate::rpc::Domain::Workspace => dispatch_domain!(crate::rpc::workspace::handle),
                    crate::rpc::Domain::Process => dispatch_domain!(crate::rpc::process::handle),
                    crate::rpc::Domain::Pair => dispatch_domain!(crate::rpc::pair::handle),
                    crate::rpc::Domain::Turn => dispatch_domain!(crate::rpc::turn::handle),
                    crate::rpc::Domain::Agent => dispatch_domain!(crate::rpc::agent::handle),
                    crate::rpc::Domain::Skill => dispatch_domain!(crate::rpc::skill::handle),
                    crate::rpc::Domain::Flags => dispatch_domain!(crate::rpc::flags::handle),
                    crate::rpc::Domain::Remote => dispatch_domain!(crate::rpc::remote::handle),
                    crate::rpc::Domain::Mcp => dispatch_domain!(crate::rpc::mcp::handle),
                    crate::rpc::Domain::WebSearch => dispatch_domain!(crate::rpc::websearch::handle),
                    crate::rpc::Domain::Thread => dispatch_domain!(crate::rpc::thread::handle),
                    crate::rpc::Domain::Side => dispatch_domain!(crate::rpc::side::handle),
                    // 所有域都已有自己的模块:只有 `Domain::Unknown`(即 `initialize`
                    // 与未识别前缀)会落到这里,再走原始 dispatch。
                    // `Domain` 已是穷尽匹配,这里必须显式写出唯一剩下的变体 —— 用
                    // `_` 会让「新加一个域却忘了加 dispatch 臂」重新变得静默可行。
                    crate::rpc::Domain::Unknown => {}
                }
                match method.as_str() {
                    "initialize" => {
                        initialized.lock().await.insert(client.clone(), true);
                        // The desktop shell stamps the local stdio handshake with
                        // who this session speaks for. Only stdio may do so: over a
                        // websocket the same JSON is a claim by a remote device, and
                        // believing it would let a phone declare itself the App's
                        // owner. A malformed or absent field is simply "no shell",
                        // never a failed handshake.
                        let mut accepted_shell = None;
                        if let Some(authorization) =
                            crate::shell_authorization::from_initialize_params(&req.params)
                        {
                            let is_local = client == crate::broadcast::ClientId::local();
                            match authorization.accept(is_local) {
                                Ok(authorization) => {
                                    shell_authorizations
                                        .lock()
                                        .await
                                        .insert(client.clone(), authorization.clone());
                                    accepted_shell = Some(authorization.clone());
                                }
                                Err(reason) => {
                                    // Not fatal: the session just has no shell
                                    // behind it, which is the CLI's normal state.
                                    eprintln!(
                                        "[app-server] ignoring shell authorization from {}: {reason}",
                                        client.as_str()
                                    );
                                }
                            }
                        }
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
                        // 两边都改了这一帧，必须合并语义而不是二选一：
                        // - 本分支要通告旁支能力位与宿主 id（三端靠它决定是否
                        //   显示入口、靠 host id 判断「重连」还是「宿主换代」）；
                        // - main 要回答它刚认可的 shell 身份，让「shell 被识别」
                        //   成为可观测事实；被拒或没有该字段时就不出现，CLI 的
                        //   正常形状保持不变。
                        let mut response = json!({
                            "serverInfo": {
                                "name": "yi-agent-app-server",
                                "version": env!("CARGO_PKG_VERSION"),
                            },
                            "protocolVersion": PROTOCOL_VERSION,
                            // 旁支能力位与宿主 id 出自同一份宿主 resources:
                            // 同一个进程的所有连接(stdio、直连 ws、经 relay)
                            // 必须看到同一个 id,否则客户端会把「重连」当成
                            // 「宿主换代」而丢掉本地基线。
                            "capabilities": {
                                "side_conversation_v1": crate::protocol::SIDE_CONVERSATION_V1,
                            },
                            "sideHostInstanceId": side_registry.host_instance_id(),
                            "syncPlan": sync_plan,
                        });
                        if let Some(shell) = accepted_shell {
                            response["shell"] = json!({
                                "clientRunId": shell.client_run_id,
                                "scopeKey": shell.scope_key,
                            });
                        }
                        write_response(&hub, &client, ok_response(id, response)).await?;
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
            // 旁支后台任务必须在这里被**轮询**才真的跑:`JoinSet` 的 task 只在
            // 集合被 poll 时推进(它持有 spawn 出去的 future)。丢掉这一臂,所有
            // `side/open` 都会永远挂着不回。`if !is_empty()` 让集合空时该 future
            // 不参与竞争,省掉每轮一次的空转。
            Some(joined) = side_tasks.join_next(), if !side_tasks.is_empty() => {
                // 任务自己负责把结果/错误写回客户端(`SideResponseSender`),这里
                // 只剩「panic 也要留个痕」——静默吞掉 join 错误会让一个每次都
                // panic 的处理路径看起来只是「客户端没收到回复」。
                if let Err(error) = joined
                    && error.is_panic() {
                    eprintln!("[app-server] side task panicked: {error}");
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

                // 一次提交一致的三个业务值：探测与 IOKit 都在供电线程里，
                // 这里只持锁写字段、放锁后唤醒，绝不阻塞。值没变就不唤醒，
                // 避免每 10 秒一次无意义的 tick。
                power_guard.update(local || remote || daemon, enabled, grace_secs);
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
                        #[cfg(test)]
                        before_prompt_send: None,
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
                    #[cfg(test)]
                    before_prompt_send: None,
                };
                if let Err(error) = launcher.deliver_to_lead(&envelope).await {
                    eprintln!("[app-server] lead delivery failed: {error}");
                    // 回滚上面那条预付的记账:这条投递没有产生任何一轮,等待方
                    // 已从工具那里拿到错误,再留一条待回填只会在下一轮投递时
                    // 张冠李戴(把下一次的回复回填给这一次的 request_id)。
                    // 摘的是**这一条** id(而非队尾):与收口路径同一口径,不依赖
                    // 「刚 push 的一定在末尾」这条隐含前提。
                    if let Some(request_id) = &envelope.request_id
                        && let Some(ids) = pending_lead_replies.get_mut(&lead_thread_id) {
                        ids.retain(|id| id != request_id);
                        if ids.is_empty() {
                            pending_lead_replies.remove(&lead_thread_id);
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
                    #[cfg(test)]
                    before_prompt_send: None,
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

    // 旁支收口:先掐掉还在跑的创建/关闭任务,再让注册表把各 runner 停干净
    // (它要等核心任务真正 quiescent,所以必须 await)。放在 detached runtime
    // 之前:runner 里的工具可能还握着项目 root 的资源。
    side_tasks.abort_all();
    side_registry.shutdown().await;

    // 进程退出前断开所有项目 root。同步调用:已经不在热路径上,而进程即将结束。
    for binding in attached_runtimes(&runtimes) {
        binding.detach();
    }

    Ok(())
}
