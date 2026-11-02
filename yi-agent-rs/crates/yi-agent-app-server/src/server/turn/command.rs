//! 会话命令：把客户端命令套用到当前会话配置上。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。

use std::sync::Arc;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::{Notification, ThreadStatus};
use crate::server::{TurnEvent, persist_and_finish_turn, write_notification};
use crate::session::{CompactOutcome, SessionCommand};
use tokio::sync::mpsc;

/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
///
/// 用 `set_session_messages` 就地替换内容而不是 `with_session` 换成新 `Arc`:会话
/// 句柄可能已被委派工具的 `CallerContext` 绑定(见 [`wrap_for_delegation`]),
/// 换 `Arc` 会让那枚句柄指向被丢弃的旧会话,`fork` 便再也看不到这位调用者的记录。
pub(crate) fn apply_session(
    agent: &mut yi_agent_core::Agent,
    session: Option<yi_agent_core::Session>,
) {
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
/// 在**没有 turn 在跑**的时刻执行一条会话命令，返回（可能被重建过的）agent。
///
/// 清空与压缩都**就地**替换会话内容(`set_session_messages`),不换 `Arc`:同一
/// 会话句柄可能已被委派工具的 `CallerContext` 绑定,换句柄会让 `fork` 读到被丢弃
/// 的旧记录。按值传入/返回保留原样——将来若某个命令确实需要换掉 `Agent` 本体,
/// 这条签名就不必再改。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_session_command(
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
