//! `thread/resume`：从磁盘重建 agent 与 runtime（wave2 C3d）。
//!
//! 从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出，只做三处允许的改写。

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError, TurnStatus};
use crate::rpc::{AppState, Flow};
use crate::server::{
    BuiltAgent, ProcessWatch, attach_delegation, chunk_items_for_replay, err_response,
    interrupt_and_wait_for_persist, ok_response, require_thread_id, run_thread_driver,
    store_lookup, watch_processes, with_session_extra_roots, with_session_model_ref,
    with_session_preserve_thinking, with_session_reasoning_effort, write_notification,
    write_notification_await, write_response,
};
use crate::session::{InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/resume" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // Design §8:内存中的 thread 知道其权威 cwd,若目标目录已被删除,
            // 必须明确报错,而不是经 store_lookup 回退到 cfg.workdir(那会让
            // 后续 load 落空,退化成含混的 -32011,甚至悄悄换到别的目录)。
            // 冷 thread 的 meta 存在 workspace 目录内,删目录后本就不可读,
            // 仍走下方的 -32011,不在此覆盖。
            if let Some(session) = state.threads.get(&thread_id)
                && !Path::new(&session.cwd).is_dir()
            {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id.clone(),
                        RpcError::invalid_params(format!(
                            "working directory no longer exists: {}",
                            session.cwd
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
            // 冷 thread 才按全局索引 / cfg.workdir 定位,再从中载入历史。
            let thread_store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            // 若该 thread 仍在内存且有活跃 turn,先请求中断,再等待 driver
            // 落盘完成,否则紧随 turn/completed 的 resume 会读到尚未写入
            // 的历史。详见 `interrupt_and_wait_for_persist`。
            let paused_writer = match interrupt_and_wait_for_persist(
                state.threads,
                state.turn_rx,
                &thread_id,
                state.pending_lead_replies,
                state.lead_replies,
            )
            .await
            {
                Ok(paused) => paused,
                Err(error) => {
                    let mut rpc_error = RpcError::internal(error);
                    rpc_error.data = Some(json!({"retryable":true,"sessionRetained":true}));
                    write_response(state.hub, state.client, err_response(id, rpc_error)).await?;
                    return Ok(Flow::Continue);
                }
            };

            // Strict activation read: a meta that exists but cannot be parsed
            // must refuse to run tools, not silently rebuild into a conversation
            // whose sandbox record is gone. Listing still uses the tolerant
            // `load`; this path is the one that grants tool access.
            let mut loaded = match thread_store.load_for_activation(&thread_id) {
                Ok(Some(l)) => l,
                Ok(None) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };

            // 该 thread 持久化的自主权模式:决定重建 agent 时的 yolo 初值。
            let mode = loaded.meta.permission_mode;

            // meta 缺 cwd/model 时(损坏重建)用当前配置兜底。
            let cwd = if loaded.meta.cwd.is_empty() {
                state.cfg.workdir.display().to_string()
            } else {
                loaded.meta.cwd.clone()
            };

            // Captured before the messages move into the session: this decides whether a
            // missing baseline is "never ran" (safe to adopt today's default) or "ran under
            // unrecorded values" (must fall back to read-only).
            let has_history = !loaded.messages.is_empty();
            let mut session = yi_agent_core::Session::new();
            // 恢复上次用量,使 auto-compact 在 resume 后的首轮即生效。
            if let Some(u) = &loaded.usage {
                session.set_last_input_tokens(Some(u.input_tokens));
            }
            session.replace_messages(std::mem::take(&mut loaded.messages));

            // 按 cwd(`meta.cwd`,损坏时用 cfg.workdir 兜底)重建本 thread
            // 的 store,让 agent 与 driver 都跑在对话真实目录,而非全局
            // cfg.workdir——这正是此前 resume 的隐患所在。
            let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

            // 附属根从 meta 快照重建（不在 resume 时重读索引：关联是**会话属性**，
            // 建会话时定了就不再追改，否则同一会话的权限根集会随索引漂移）。
            // 已删的附属根静默丢弃——它是能力增量，不该让会话打不开；
            // cwd 已删仍走上方既有报错（那是权威基点）。
            let extra_roots: Vec<PathBuf> = loaded
                .meta
                .extra_roots
                .iter()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .collect();

            // 与 `thread/start` 同一道门禁:老会话的 meta 可能指向一个已被删除
            // 的条目,重建 agent 同样不该拿到空模型。
            if let Some(error) = super::missing_model_error(state, loaded.meta.model_ref.as_deref())
            {
                write_response(state.hub, state.client, err_response(id, error)).await?;
                return Ok(Flow::Continue);
            }

            // Early replay is a desktop-only optimization. The marker is a
            // directed, load-bearing frame: a remote WS client can ask for
            // `historyFirst` on a shared sidecar, and its disconnect must not be
            // able to tear the local session down. Remote clients also have their
            // own catch-up protocol (`thread/readItems` + sync plan), so they stay
            // on the legacy ordering.
            let history_token = req
                .params
                .get("historyToken")
                .and_then(serde_json::Value::as_str)
                .filter(|token| !token.is_empty());
            let history_first = *state.client == crate::broadcast::ClientId::local()
                && req
                    .params
                    .get("historyFirst")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                && history_token.is_some();
            if history_first {
                // Directed delivery, not a broadcast: `historyReady` promises the
                // caller that the transcript is already on its wire, so the
                // requestor must receive every frame regardless of its
                // `thread/subscribe` feed. Same for the partial/interrupted and
                // usage frames, which otherwise could be filtered out while the
                // marker still arrived.
                replay_history(state, &thread_id, &mut loaded, Some(state.client)).await?;
                // Reliable and targeted: another client or replay must not be unlocked.
                let ready = Notification::ThreadHistoryReady {
                    thread_id: thread_id.clone(),
                    request_token: history_token.unwrap_or_default().to_string(),
                };
                let frame =
                    serde_json::to_value(crate::protocol::NotificationEnvelope::new(&ready))?;
                state
                    .hub
                    .reply(state.client, frame)
                    .await
                    .map_err(|_| anyhow::anyhow!("history client disconnected"))?;
                // The next phase is synchronous. Let the independent output pump
                // run before it can park this worker during agent construction.
                tokio::task::yield_now().await;
            }

            let built = match with_session_model_ref(loaded.meta.model_ref.clone(), || {
                with_session_extra_roots(extra_roots.clone(), || {
                    with_session_reasoning_effort(loaded.meta.reasoning_effort.clone(), || {
                        with_session_preserve_thinking(loaded.meta.preserve_thinking, || {
                            (state.build_agent)(Some(session), Path::new(&cwd), mode)
                        })
                    })
                })
            }) {
                Ok(a) => a,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 与 `thread/start` 同款:上报/落盘的生效串取自**实际建出的 agent**
            // (`agent.config().model`),而非按清单再解析一次。工厂持有的是启动时
            // 快照,运行期新增的条目会让「另解析一次」与实际运行不一致
            // (任务 7 评审 Finding 1);不照抄 meta.model 也是为了让旧库/异常库里
            // 一个与当前清单不符的陈旧串被本次实际生效值取代。
            let model = built.agent.config().model.clone();
            // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。清除覆盖
            // 后的 meta 是 `None`,但工厂此时已回落到条目默认,故响应必须报
            // 该实际值,而非照抄 meta 的 null(否则客户端挂着的控件会骗人)。
            let reasoning_effort = built.agent.config().gen_params.reasoning_effort.clone();
            // 与 `reasoning_effort` 同源:上报**实际建出**的 agent 的开关
            // 值(取自 meta,经工厂落到 config),让重开的客户端立刻拿到
            // 初始状态,而不必等一次广播。
            let preserve_thinking = built.agent.config().preserve_thinking;
            let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
            // This conversation's sandbox comes from its own record, never from
            // today's defaults: `state.cfg.sandbox` is what *new* conversations
            // start with, and applying it here would silently widen a session
            // resumed after an unrelated settings change.
            let resolved_baseline = crate::session_sandbox::resolve_activation_baseline(
                loaded.meta.sandbox_baseline,
                None,
                None,
                crate::session_sandbox::SandboxBaseline::from_parts(
                    state.cfg.sandbox,
                    state.cfg.sandbox_promotable,
                ),
                has_history,
            );
            if let Some(warning) = resolved_baseline.warning {
                tracing::warn!(
                    thread_id,
                    ?warning,
                    "sandbox baseline resolved without full evidence"
                );
            }
            // Persist the decision so this conversation stops looking like "no
            // evidence". A failure here is only a warning: the in-memory
            // baseline below is what this run uses, and refusing to open the
            // conversation over a failed bookkeeping write would turn a
            // degraded record into an outage.
            if resolved_baseline.should_persist()
                && let Err(error) =
                    thread_store.set_sandbox_baseline(&thread_id, resolved_baseline.baseline)
            {
                tracing::warn!(thread_id, %error, "could not persist the resolved sandbox baseline");
            }
            let managed_authority = crate::server::managed_snapshot(state.managed);
            let require_managed = managed_authority.is_some();
            let activation = attach_delegation(
                state.runtimes,
                state.thread_roots,
                &runtime_dir,
                managed_authority,
                state.models_path,
                state.bringup,
                state.cfg,
                &cwd,
                &thread_id,
                state.theme_handle,
                state.git_diff_handle,
                // 与工厂槽位**同一个值**：resume 侧的委派路径必须与
                // 重建 agent 时下发到工厂的根集一致。
                extra_roots.clone(),
                state.lead_inbox,
                state.lead_replies,
                state.lead_control,
                state.lead_control_replies,
                // resume 路径：meta 已在盘上，起点取已落盘的 `anchor_sha`。
                // 旧会话没有该字段时是 `None`——那表示「无起点」，按方案 1 不回填。
                loaded.meta.anchor_sha.clone(),
                Some(state.lead_agent_enabled),
                // resume 不重注环境变量：`session_env` 在会话装配期定值，
                // 重开时沿用同一口径（空集），与 `thread/start` 一致。
                Vec::new(),
                state.live_settings,
                Some(resolved_baseline.baseline),
                built,
            );
            let activation = match activation.require_managed(require_managed) {
                Ok(activation) => activation,
                Err(error) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(error.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            let BuiltAgent {
                agent,
                provider,
                config,
                decision_tx,
                catalog,
                yolo,
                process_manager,
                mcp,
                sandbox,
                ..
            } = activation.built;
            state
                .pending_activation
                .insert(thread_id.clone(), activation.runtime);
            // MCP 配置固定在 thread cwd 下,与 `build_runtime_tooling` 同源。
            let mcp_workdir = std::path::PathBuf::from(cwd.as_str());

            let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
            let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
            let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
            let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
            // 同一 `Arc` 句柄:session 存一份供 `thread/list` 读,
            // driver 拿一份用于推送 `thread/status/updated`。
            let store_status = ThreadSession::new_status();
            // 与 driver 同处创建：resume 会重建 driver，跨 resume 复用 sender
            // 会让新 driver 永远收不到（旧 sender 的接收端已随旧 driver 退出）。
            let (subagent_tx, subagent_rx) =
                tokio::sync::watch::channel(crate::protocol::SubagentTotals::default());
            // 成为该 thread 的 writer 前必须拿到跨进程锁。拿不到就直接报错，
            // 不建半个 session——半建状态会让后续 resume 看到不一致的运行时。
            //
            // 取锁点放在这里（即将 insert 进 threads 表）而不是更早：在此之前的
            // 所有步骤（load、build_agent、attach_delegation）都只是**读**，
            // 不构成双写。放在这里也保证失败时没有留下任何注册项。
            // 只有 `Busy` 才拒绝：另一个 writer 真的在写它。
            // `Io`（锁文件建不出来）不阻断——锁是防双写的附加保护，不是会话
            // 能否打开的充要条件；此时退化为「无锁」并记一行，与加锁前行为一致。
            let writer_lock =
                match crate::thread_store::acquire_thread_lock(thread_store.root(), &thread_id) {
                    Ok(lock) => Some(lock),
                    Err(crate::thread_store::ThreadLockError::Busy(busy)) => {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(id, RpcError::internal(busy.to_string())),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                    Err(e @ crate::thread_store::ThreadLockError::Io(_)) => {
                        tracing::warn!(%e, thread_id = %thread_id, "会话写锁不可用，退化为无锁");
                        None
                    }
                };

            // 发布本会话作为父会话的旁问来源，理由与 `thread/start` 相同：插入
            // `threads` 之后响应立即返回，用户可能马上敲 `/side`。resume 的历史
            // 已在 agent 里，这条来源一开始就可用。
            let side_source = state.live_side_parents.and_then(|parents| {
                crate::side_conversation::LiveParentSource::install(
                    Arc::clone(parents),
                    thread_id.clone(),
                    &agent,
                    cwd.clone(),
                    crate::side_conversation::live_roots(&cwd, &extra_roots),
                    Default::default(),
                )
            });

            state.threads.insert(
                thread_id.clone(),
                ThreadSession {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
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
                    // 准入许可随 turn 生命周期管理；新建/恢复会话时尚未申请。
                    turn_permit: None,
                    side_source: side_source.clone(),
                },
            );

            if let Some(paused) = paused_writer {
                paused.stop();
            }

            // 与 `thread/start` 同款:先停旧守望者、再对本次生效的 manager 重订。
            // 这里正是缺陷所在——resume 一个活 thread 会新建 manager,若沿用
            // 「有守望者就不重建」的守卫,旧守望者会继续订阅被丢弃的 manager。
            if let Some(previous) = state.process_watches.remove(&thread_id) {
                previous.stop().await;
            }
            let rx = process_manager.subscribe();
            let handle = tokio::spawn(watch_processes(
                Arc::clone(state.hub),
                thread_id.clone(),
                Arc::clone(&process_manager),
                rx,
            ));
            state
                .process_watches
                .insert(thread_id.clone(), ProcessWatch { task: handle });

            let driver_hub = Arc::clone(state.hub);
            let driver_client = state.client.clone();
            let driver_turn_tx = state.turn_tx.clone();
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
                Arc::clone(state.pending),
                state.permission_timeout,
                Arc::clone(state.perm_seq),
                catalog,
                Arc::clone(&thread_store),
                Arc::clone(&store_status),
                provider,
                config,
                subagent_rx,
                mcp,
                mcp_workdir,
                // Resolved from the conversation's own root on the first turn.
                None,
                sandbox,
                Arc::clone(&process_manager),
                side_source,
            ));

            // 回放:thread/started → 若干批量 items/completed → 最近用量 → 响应。
            write_notification(
                state.hub,
                &Notification::ThreadStarted {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    model_ref: loaded.meta.model_ref.clone(),
                },
            )
            .await?;
            if !history_first {
                // Legacy broadcast path: unchanged wire ordering and filtering.
                replay_history(state, &thread_id, &mut loaded, None).await?;
            }
            // Promote only after acquiring the writer lock, as on the legacy path.
            // Early replay is read-only: a failed activation must leave the partial intact.
            if loaded.pending_turn {
                // 采纳的 partial 必须在这一刻**升格**进主 jsonl：它是这一轮
                // 唯一的持久副本，若不 append 就返回，用户下一条消息的 turn-start
                // checkpoint 会原子覆盖同一文件，该轮 items 就此永久丢失（而
                // session 上下文仍"记得"它们，item 与上下文就此不一致）。
                // 失败只记 stderr：恢复出不完整好过让整个 resume 报错；
                // 判据与 load 共用，重复调用不会重复 append。
                if let Err(e) = thread_store.promote_partial(&thread_id) {
                    eprintln!("[app-server] failed to promote recovered turn ({thread_id}): {e}");
                }
            }
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "thread_id": thread_id,
                        "cwd": cwd,
                        "model": model,
                        "model_ref": loaded.meta.model_ref,
                        "permission_mode": loaded.meta.permission_mode,
                        "reasoning_effort": reasoning_effort,
                        "preserve_thinking": preserve_thinking,
                    }),
                ),
            )
            .await?;
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// Replay durable transcript data without creating an agent or granting tool authority.
///
/// `targeted` is `Some(client)` only for the local early phase, where every frame
/// must reach the requestor directly (a subscription filter must not be able to
/// strand a `historyReady` without its history). `None` keeps the legacy
/// thread-keyed broadcast, frame-for-frame identical to the old implementation:
/// item chunks wait for backpressure, the usage and interrupted markers do not.
async fn replay_history(
    state: &AppState<'_>,
    thread_id: &str,
    loaded: &mut crate::thread_store::LoadedThread,
    targeted: Option<&crate::broadcast::ClientId>,
) -> anyhow::Result<()> {
    /// `blocking` selects the legacy delivery shim: chunks use the awaited
    /// backpressured send; usage/interrupted use the fire-and-forget broadcast
    /// (which deliberately ignores a closed client). A targeted frame always
    /// awaits enqueue so `historyReady` cannot overtake it.
    async fn send(
        state: &AppState<'_>,
        targeted: Option<&crate::broadcast::ClientId>,
        notification: &Notification,
        blocking: bool,
    ) -> anyhow::Result<()> {
        if let Some(client) = targeted {
            let frame =
                serde_json::to_value(crate::protocol::NotificationEnvelope::new(notification))?;
            return state
                .hub
                .reply(client, frame)
                .await
                .map_err(|_| anyhow::anyhow!("history client disconnected"));
        }
        if blocking {
            write_notification_await(state.hub, notification).await
        } else {
            write_notification(state.hub, notification).await
        }
    }

    // 回放：分块批量下发（每块一条 `items/completed`），经带背压的
    // 投递——本地 reliable 客户端队列满时等待而非丢帧。旧实现逐条
    // try_send,超过 256 格队列即静默丢帧,长会话尾部就此消失。
    for chunk in chunk_items_for_replay(std::mem::take(&mut loaded.items)) {
        send(
            state,
            targeted,
            &Notification::ItemsCompleted {
                thread_id: thread_id.to_string(),
                items: chunk,
            },
            true,
        )
        .await?;
    }
    // 回放的既要有「最后一轮快照」（上下文占用），也要有该 thread 的累计值
    // （`/usage` 与状态栏）——后者若不带，客户端 resume 后累计会从 0 重来。
    if let Some(u) = loaded.usage.take() {
        let totals = &loaded.totals;
        send(
            state,
            targeted,
            &Notification::TokenUsage {
                thread_id: thread_id.to_string(),
                model: u.model,
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                cache_creation_input_tokens: u.cache_creation_input_tokens,
                cache_read_input_tokens: u.cache_read_input_tokens,
                total_input_tokens: totals.input_tokens,
                total_output_tokens: totals.output_tokens,
                total_cache_creation_input_tokens: totals.cache_creation_input_tokens,
                total_cache_read_input_tokens: totals.cache_read_input_tokens,
                // 回放不带 subagent 汇总：它由 children watch 在会话恢复后
                // 推送（Task 5）。这里给 0 而不是省略字段，客户端因此不必
                // 处理「缺失」这一种额外状态。
                subagent_input_tokens: 0,
                subagent_output_tokens: 0,
                subagent_cache_read_tokens: 0,
            },
            false,
        )
        .await?;
    }
    // 末尾若是崩溃残留的未收尾 turn，补一条 interrupted 标记，
    // 让客户端把它显示为"这一轮被中断"，而不是当成正常轮次。
    if loaded.pending_turn {
        send(
            state,
            targeted,
            &Notification::TurnCompleted {
                thread_id: thread_id.to_string(),
                turn_id: "crashed".to_string(),
                status: TurnStatus::Interrupted,
                error: None,
                // 崩溃那轮从未走到收尾，meta 里没有它的完成时间；
                // 回放这一刻即客户端「得知它结束」的时刻。
                completed_at: Some(crate::thread_store::now_millis()),
            },
            false,
        )
        .await?;
    }
    Ok(())
}
