//! thread driver 主循环：消费 driver 事件、把 delta 合并后投递给会话。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 7）。

use crate::protocol::{
    JSONRPC_VERSION, Notification, ReverseRequest, SubagentTotals, ThreadStatus,
};
use crate::session::{CompactOutcome, InterjectionRequest, SessionCommand, TurnPrompt};
use crate::translate::Translator;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use yi_agent_core::permission::Decision;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use futures::StreamExt;

use crate::server::{
    DeltaCoalescer, PartialTurnInput, TurnEvent, apply_session_command, build_partial, checkpoint,
    finished_event, generate_title_for_first_input, lead_reply_text, persist_and_finish_turn,
    saturate_u32, update_status, write_notification,
};

/// Whether the process groups this turn started are all gone.
///
/// Only a real observation counts: a group this turn left behind is `false`, and
/// the daemon then keeps the execution unsettled instead of letting a later
/// driver take over while something may still be writing. Processes that were
/// already alive when the turn began are not this turn's, so they are excluded --
/// a long-lived server from an earlier turn must not make every later stop
/// unprovable.
fn tools_stopped_since(manager: &yi_agent_tools::ProcessManager, before: &[String]) -> bool {
    manager.list().iter().all(|process| {
        if before.iter().any(|id| id == &process.process_id) {
            return true;
        }
        matches!(
            process.status,
            yi_agent_tools::ProcessStatus::Exited { .. }
                | yi_agent_tools::ProcessStatus::Killed
                | yi_agent_tools::ProcessStatus::FailedToStart { .. }
        )
    })
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
    // This process's authority to drive foreground roots, when it has one.
    //
    // `None` is the honest answer for an embedded runtime, a build without the
    // contract and a runtime whose guardian refused; in every one of those the turn
    // runs with legacy semantics, which is exactly the promise the contract makes.
    // The host is shared by every thread because the control channel accounts for
    // *clients*: one lease per thread would look like several frontends.
    foreground_host: Option<Arc<crate::server::foreground::ForegroundDispatch>>,
    // The sandbox policy this thread's tools were built with. The per-turn delegation
    // tool set is built from it, so the spawn tools read the same live YOLO switch as
    // the rest of the registry instead of a second, drifting copy.
    sandbox: yi_agent_tools::SandboxController,
    // The process manager that builds this thread's shell/process tools. A turn's
    // stop proof has to name whether the groups it started are actually gone, and
    // this is the only place that knows. It is observed, never trusted blindly:
    // see `tools_stopped_since`.
    process_manager: Arc<yi_agent_tools::ProcessManager>,
) {
    // Foreground authority is per *process*, so it is resolved once -- lazily, from
    // the first root this thread is handed -- and kept for the life of the driver. A
    // lease registered per turn would look like a new client every time and keep the
    // runtime alive after the window closed.
    let mut foreground_host: Option<Arc<crate::server::foreground::ForegroundDispatch>> =
        foreground_host;
    let mut foreground_host_resolved = foreground_host.is_some();
    // The root this thread drives its activity through, captured on the turn that
    // activated it. Later turns carry `activate: None` (activation is once per thread),
    // so the driver has to remember instead of asking the prompt again.
    let mut foreground_root: Option<Arc<yi_agent_subagent::thread_root::ThreadRoot>> = None;
    // Per-turn delegation. Built from the same root the foreground contract uses, so
    // there is exactly one place that decides "can this conversation delegate".
    //
    // Deliberately *not* the same thing as activation: activation is a once-per-thread
    // fact about the runtime, while delegation is re-decided every turn. A turn whose
    // preparation fails runs the conversation without delegation and says so; it does
    // not leave a hook for the next turn to inherit.
    let mut delegation: Option<crate::server::delegation::ThreadDelegation> = None;

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
                // Kept so every turn after this one can still report activity: the
                // activation happens once per thread, so later prompts carry `None`.
                foreground_root = Some(Arc::clone(&binding));
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
        // Foreground activity for this turn. Resolved on the first root the driver sees
        // and reused afterwards; a failure here leaves the turn running with legacy
        // semantics rather than aborting a working conversation.
        let mut foreground_turn: Option<yi_agent_subagent::foreground::ForegroundTurn> = None;
        if !foreground_host_resolved {
            foreground_host_resolved = true;
            if let Some(root) = foreground_root.as_ref() {
                match crate::server::foreground::ForegroundDispatch::connect_root(root) {
                    Ok(Some(host)) => foreground_host = Some(Arc::new(host)),
                    Ok(None) => {}
                    Err(cause) => {
                        tracing::warn!(%cause, "foreground authority unavailable for this thread")
                    }
                }
            }
        }
        if let (Some(host), Some(root)) = (foreground_host.as_ref(), foreground_root.as_ref()) {
            foreground_turn = host.begin(root, &turn_id);
        }

        // Delegation for this turn. Order is deliberate: prepare/begin first, then
        // install the tools, then announce. A client that sees `ready: true` can be
        // sure `spawn_agent` will be accepted by the daemon, and a client that sees a
        // failure gets a stable code instead of a tool that fails only when called.
        let mut delegation_turn: Option<(
            Arc<yi_agent_subagent::session_delegation::SessionTurnClient>,
            yi_agent_lifecycle::foreground::ExecutionIdentity,
        )> = None;
        //
        // This block runs for *every* turn, root or no root. A thread without a
        // delegation root must end up with no delegation tools at all: the
        // assembly installs legacy instances, and leaving them in place would hand
        // the model a `spawn_agent` that still answers -- the very "a settled
        // history stopped delegation, yet the tools kept working" hole this change
        // closes. The status is announced either way, so "I have no delegation" is
        // a visible fact rather than a silent gap.
        if delegation.is_none()
            && let Some(root) = foreground_root.as_ref()
        {
            delegation = Some(crate::server::delegation::ThreadDelegation::for_thread(
                Arc::clone(root),
                &store.session_identity_directory(),
            ));
        }
        // Resolve managed vs embedded *before* preparing: a guardian's refusal must
        // be reported as such, not silently replaced by the host's own secret.
        let authority_failure = match (delegation.as_mut(), foreground_host.as_ref()) {
            (Some(driver), Some(manager)) => driver.resolve_authority(manager).err(),
            // No guardian manager: an embedded deployment, which is a normal answer.
            _ => None,
        };
        let prepared = match authority_failure {
            Some(failure) => crate::server::delegation::PreparedTurn::NotReady(failure),
            None => delegation
                .as_mut()
                .map(|driver| driver.prepare_turn_with(&turn_id, foreground_host.as_deref()))
                .unwrap_or_else(|| {
                    crate::server::delegation::PreparedTurn::NotReady(
                        crate::server::delegation::DelegationFailure::Unavailable(
                            "this conversation has no delegation root".to_string(),
                        ),
                    )
                }),
        };
        // A fresh registry per turn, never a mutation of the one a previous
        // turn's tools still hold: that is what stops a slow call from borrowing
        // the authority of the turn that replaced it.
        let mut registry = (*agent.tools()).clone();
        // Strip whatever delegation tools are there *before* deciding, so a turn
        // that could not be prepared ends up with none at all.
        for name in [
            "spawn_agent",
            "send_message",
            "wait_agent",
            "inspect_agent",
            "cancel_agent",
        ] {
            registry.remove(name);
        }
        let failure = match prepared {
            crate::server::delegation::PreparedTurn::Ready { turn, execution } => {
                let caller = yi_agent_subagent::CallerContext::unbound();
                caller.bind(agent.session_handle());
                yi_agent_subagent::register_session_turn_tools(
                    &mut registry,
                    Arc::clone(&turn),
                    sandbox.clone(),
                    Some(thread_id.clone()),
                    caller,
                );
                delegation_turn = Some((turn, execution));
                None
            }
            crate::server::delegation::PreparedTurn::NotReady(failure) => Some(failure),
        };
        agent.set_tools_for_next_run(Arc::new(registry));
        let notification = crate::protocol::Notification::DelegationStatusUpdated {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
            ready: failure.is_none(),
            failure: failure.map(|failure| failure.payload()),
        };
        let _ = write_notification(&hub, &notification).await;
        // Every exit path below runs this. The turn's tools must stop writing the
        // moment the turn ends, whatever ended it.
        //
        // Two facts are reported, in this order, and the order is the point:
        //
        // 1. the turn closes **locally first**, so a tool that outlived the run
        //    cannot write even if the daemon never answers;
        // 2. the execution is retired with a stop proof built from what this driver
        //    actually observed -- the run was joined, and the process groups the run
        //    started are gone.
        //
        // A `false` here is not a bug to paper over: the daemon then keeps the
        // execution unsettled and refuses a later driver, which is the correct
        // answer for "a process may still be writing".
        macro_rules! finish_delegation {
            ($run:expr, $before:expr) => {
                if let (Some((turn, execution)), Some(driver)) =
                    (delegation_turn.as_ref(), delegation.as_mut())
                {
                    driver.finish_turn(turn);
                    // `joined` is an observation, so it may only be claimed when the
                    // control we hold really joined. Two cases are honestly *not*
                    // joined:
                    //
                    // * `run_tracked_blocks` failed, so no control exists -- the
                    //   execution never started, and "joined" would describe a task
                    //   that was never created;
                    // * `stop_and_join` returned an error (the root panicked), so the
                    //   join has no result to point at.
                    //
                    // Reporting `joined: false` leaves the execution unsettled, which
                    // is the correct answer: a later driver is refused instead of
                    // being told a process that may still be writing is gone.
                    let joined = match $run.as_mut() {
                        Some(run) => run.stop_and_join(Duration::from_secs(2)).await.is_ok(),
                        None => false,
                    };
                    let tools_stopped = tools_stopped_since(&process_manager, $before);
                    if let Err(error) = driver.record_stopped(execution, joined, tools_stopped) {
                        // Never silent: a stop that could not be reported is the
                        // difference between "the next turn works" and "the next
                        // turn is refused as StopUnproven".
                        tracing::warn!(
                            thread_id = %thread_id,
                            turn_id = %turn.context().turn_id,
                            %error,
                            "the turn closed but its execution stop was not recorded"
                        );
                        let notice = crate::protocol::Notification::DelegationStatusUpdated {
                            thread_id: thread_id.clone(),
                            turn_id: turn.context().turn_id.clone(),
                            ready: false,
                            failure: Some(crate::protocol::DelegationFailurePayload {
                                code: "stop_unrecorded".to_string(),
                                reason: format!(
                                    "this turn's stop could not be reported to the daemon: {error}"
                                ),
                            }),
                        };
                        let _ = write_notification(&hub, &notice).await;
                    }
                }
            };
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
        if let Some(mcp) = &mcp
            && mcp.needs_reload(&mcp_workdir, yi_agent_mcp::now_unix())
        {
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
            let mut rebuilt =
                yi_agent_core::Agent::new(Arc::clone(&provider), Arc::new(refreshed), next_config)
                    .with_session_arc(session_handle);
            if let (Some(checker), Some(rx)) = (permission, decision) {
                rebuilt = rebuilt.with_permission(checker, rx);
            }
            agent = rebuilt;
            tracing::info!("MCP config changed on disk; thread registry reloaded");
        }

        // 每轮开跑前刷新 skills catalog:skills 热重载,让本轮看到最新的
        // system prompt(catalog 未变时输出逐字节相同,不破坏 prompt cache)。
        if let Some(handle) = &catalog
            && let Some(new_prompt) = handle.current_system_prompt()
        {
            agent.set_system_prompt(Some(new_prompt));
        }

        // The process groups already alive when this turn starts. Anything this
        // turn adds and still leaves running at exit is *not* proven stopped, and
        // the daemon must be told so.
        let processes_at_start = process_manager.list();
        let processes_at_start: Vec<String> = processes_at_start
            .into_iter()
            .map(|process| process.process_id)
            .collect();

        // Tracked, not plain: the plain API detaches the run's real task, so a turn
        // that ended would leave the loop unjoined and "the execution stopped" would
        // be a claim rather than an observation.
        let tracked = agent.run_tracked_blocks(blocks).await;
        let mut run_control = None;
        let mut stream = match tracked {
            Ok(run) => {
                run_control = Some(run.control);
                run.events
            }
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
                // A failed run is still a finished turn: closing it here keeps the
                // daemon's view from holding a turn that can never end.
                if let Some(mut turn) = foreground_turn.take() {
                    turn.finish();
                }
                // Same for delegation. `run_tracked_blocks` failed, so there is no
                // control to join -- and that is exactly the honest evidence: the
                // execution never started, so its tools cannot be running. Reporting
                // `tools_stopped` from a real observation (the manager's list) still
                // applies, and an exception is never invented.
                finish_delegation!(run_control, processes_at_start.as_slice());
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
                                    finish_delegation!(run_control, processes_at_start.as_slice());
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
                                finish_delegation!(run_control, processes_at_start.as_slice());
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
                                        if let Some(t) = n.thread_key()
                                        && let Some((pid, text)) = coalescer.take(t) {
                                            to_send.push(Notification::ItemDelta {
                                                thread_id: t.to_string(),
                                                item_id: pid,
                                                delta: text,
                                            });
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
                                        finish_delegation!(run_control, processes_at_start.as_slice());
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
                        finish_delegation!(run_control, processes_at_start.as_slice());
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
            if let Some(mut turn) = foreground_turn.take() {
                turn.finish();
            }
            finish_delegation!(run_control, processes_at_start.as_slice());
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
        // Closed on the normal path too, and after the turn is persisted: a close that
        // ran first would tell the daemon the turn ended while it was still being
        // written, which is the one ordering a reader can observe.
        if let Some(mut turn) = foreground_turn.take() {
            turn.finish();
        }
        finish_delegation!(run_control, processes_at_start.as_slice());
    }
}
