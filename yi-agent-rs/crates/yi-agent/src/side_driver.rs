//! TUI 的旁支调度任务。
//!
//! 旁支不能排在主 `DriverControl` 队列后面:那条队列只在主会话的**轮间边界**
//! 被消费,而旁支的意义恰恰是「主任务跑到一半也能问」。所以这里是一个独立的
//! 任务,与主 driver 并行:
//!
//! ```text
//!   TUI ──commands──▶ side_driver ──runner stream──▶ ConversationEvent ──▶ TUI
//!   TUI ──input─────▶ main driver  (两条通道互不相交)
//! ```
//!
//! 调度任务只拥有「至多一个旁支」的逻辑状态。真正的重活（fork 历史、跑一轮、
//! 停止并 join 真实 core 任务、清理资源目录）都在 `yi-agent-runtime` 的
//! `SideConversationRunner` / `SideHostResources` 里,这里**不复制**任何算法:
//! ledger 去重、TTL、槽位、转录预算都直接消费 runtime 的实现,于是 TUI 与
//! 桌面/手机在语义上不会漂移。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use yi_agent_runtime::side_conversation::{
    CloseReason, SideConversationRunner, SideError, SideErrorReason, SideHostResources,
    SideIdleDeadline, SideItem, SideRequestLedger, SideResourceLease, SideResult, SideRunnerEvent,
    SideSourcePublisher, SideStatus, SideToolPolicy, SideToolSetupFactory, SideTranscript,
};

use crate::tui::side::{ConversationEvent, ConversationPayload, ConversationTarget, SideOperation};

/// 旁支事件通道容量。一个满通道只会让调度任务等一下,不会丢正文。
const SIDE_EVENT_CHANNEL: usize = 256;

/// 命令通道容量。命令是低频、有界的人类操作。
const SIDE_COMMAND_CHANNEL: usize = 16;

/// 合并连续正文刷新的最短间隔。
///
/// 流式 delta 会在一轮里产生成百上千个事件,而 TUI 每帧只画一次。这层节流让
/// 「一帧内多个 delta」只产生一次 upsert,而且发送的永远是 item 的**当前完整
/// 版本**,所以消费者不会看到重复正文。
const SIDE_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// TUI 发往旁支调度的命令。
#[derive(Debug)]
pub enum SideDriverCommand {
    /// 打开旁支;`question` 非空时顺带作为第一轮问题。
    Open {
        request_id: String,
        question: Option<String>,
    },
    /// 在已打开的旁支里提一个新问题。
    Submit { request_id: String, text: String },
    /// 停止某个确定轮次。
    ///
    /// 除了 `turn_id`，还带上调用方**以为**在操作的目标：UI 手里的视图可能
    /// 已经过期（用户切到了另一个旁支，或旧旁支被关掉后新开了一个），只比
    /// `turn_id` 挡不住「旧视图的停止落到新旁支上」——两个代际的轮次 id 不
    /// 会相同，但**空 id** 会让比较退化。目标不符时产生该旧目标的
    /// `SideOperationError(Interrupt)`，不作用到当前 runner。
    Interrupt {
        expected_target: ConversationTarget,
        turn_id: String,
    },
    /// 关闭旁支。两个字段都是调用方确认过的条件，任一不符就拒绝。
    Close {
        expected_target: ConversationTarget,
        expected_active_turn_id: Option<String>,
    },
    /// 父会话即将 clear/delete:先建立逻辑屏障,主操作成功才提交作废。
    ParentBarrier {
        reason: CloseReason,
        reply: oneshot::Sender<ParentChangePermit>,
    },
    /// 退出:停止一切并结束任务。
    Exit,
}

/// 一次父变更的许可。
///
/// 拿到它之后执行真正的主操作:成功则 [`ParentChangePermit::commit`]（旁支作废）,
/// 失败则 `drop`（屏障回滚,旧旁支继续可用）。**不能**把「放行」做成无条件通知:
/// 那样主操作失败后旁支已经被作废,用户会莫名少一个能用的旁支。
#[derive(Debug)]
pub struct ParentChangePermit {
    /// `None` 表示已 commit 或已回滚,`Drop` 不再重复发送。
    outcome: Option<oneshot::Sender<ParentChangeOutcome>>,
}

/// 父变更许可的最终结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentChangeOutcome {
    Commit,
    Rollback,
}

impl ParentChangePermit {
    /// 主操作成功:旁支作废。
    ///
    /// 取走发送端,于是 `Drop` 看到 `None` 就不会再发一次 `Rollback`。
    pub fn commit(mut self) {
        if let Some(outcome) = self.outcome.take() {
            let _ = outcome.send(ParentChangeOutcome::Commit);
        }
    }
}

impl Drop for ParentChangePermit {
    fn drop(&mut self) {
        // 没 commit 就是失败路径:回滚屏障,旧旁支继续可用。
        if let Some(outcome) = self.outcome.take() {
            let _ = outcome.send(ParentChangeOutcome::Rollback);
        }
    }
}

struct ActiveTurn {
    turn_id: String,
    stream: futures::stream::BoxStream<'static, SideRunnerEvent>,
}

/// 旁支调度任务持有的全部状态。
pub struct SideDriver {
    commands: mpsc::Receiver<SideDriverCommand>,
    events: mpsc::Sender<ConversationEvent>,
    publisher: Arc<SideSourcePublisher>,
    resources: Arc<SideHostResources>,
    pool: Arc<yi_agent_tools::SideBlockingPool>,
    ledger: SideRequestLedger,
    slot: Option<Arc<SideResourceLease>>,
    runner: Option<SideConversationRunner>,
    transcript: Option<SideTranscript>,
    deadline: Option<SideIdleDeadline>,
    active: Option<ActiveTurn>,
    /// 已变更但还没发出的 item,按 id 去重后按 ordinal 顺序 flush。
    pending: Vec<Arc<SideItem>>,
    /// 下一次 flush 的最后期限。
    flush_at: Option<tokio::time::Instant>,
    /// 父变更屏障期间拒绝创建/提问。
    barrier: Option<CloseReason>,
    /// 屏障的最终结果回传端。
    barrier_outcome: Option<oneshot::Receiver<ParentChangeOutcome>>,
}

impl SideDriver {
    /// 创建调度任务并返回它的句柄。
    pub fn spawn(
        publisher: Arc<SideSourcePublisher>,
        resources: Arc<SideHostResources>,
        pool: Arc<yi_agent_tools::SideBlockingPool>,
        commands: mpsc::Receiver<SideDriverCommand>,
        events: mpsc::Sender<ConversationEvent>,
    ) -> tokio::task::JoinHandle<()> {
        let driver = Self {
            commands,
            events,
            publisher,
            resources,
            pool,
            ledger: SideRequestLedger::new(),
            slot: None,
            runner: None,
            transcript: None,
            deadline: None,
            active: None,
            pending: Vec::new(),
            flush_at: None,
            barrier: None,
            barrier_outcome: None,
        };
        tokio::spawn(driver.run())
    }

    async fn run(mut self) {
        loop {
            let idle_at = self.deadline.as_ref().map(|deadline| deadline.expires_at());
            let flush_at = self.flush_at;
            tokio::select! {
                biased;
                command = self.commands.recv() => {
                    match command {
                        Some(SideDriverCommand::Exit) | None => {
                            self.shutdown().await;
                            return;
                        }
                        Some(command) => self.handle_command(command).await,
                    }
                }
                outcome = barrier_outcome(&mut self.barrier_outcome), if self.barrier_outcome.is_some() => {
                    self.settle_parent_barrier(outcome).await;
                }
                frame = next_frame(&mut self.active), if self.active.is_some() => {
                    self.handle_frame(frame).await;
                }
                _ = sleep_until_opt(flush_at), if flush_at.is_some() => {
                    self.flush_items().await;
                }
                _ = sleep_until_opt(idle_at), if idle_at.is_some() => {
                    self.expire_idle().await;
                }
            }
        }
    }

    async fn handle_command(&mut self, command: SideDriverCommand) {
        match command {
            SideDriverCommand::Open {
                request_id,
                question,
            } => {
                let outcome = self.open(request_id, question).await;
                self.report_operation_error(SideOperation::Open, outcome.err())
                    .await;
            }
            SideDriverCommand::Submit { request_id, text } => {
                let outcome = self.submit(request_id, text).await;
                self.report_operation_error(SideOperation::Submit, outcome.err())
                    .await;
            }
            SideDriverCommand::Interrupt {
                expected_target,
                turn_id,
            } => {
                // 先比目标:UI 手里的视图可能已经过期。目标不符时**必须报出来**,
                // 否则用户按了停止、界面毫无反应,而真正该停的东西还在跑。
                if !self.target_matches(&expected_target) {
                    self.report_stale_target(SideOperation::Interrupt, &expected_target)
                        .await;
                    return;
                }
                // 再比轮次:空闲或轮次已变时不算错误——那是「这一轮已经结束」,
                // 用户看到的状态本身是对的。
                let matches_active = self
                    .active
                    .as_ref()
                    .is_some_and(|active| active.turn_id == turn_id);
                if !matches_active {
                    return;
                }
                if let Some(runner) = self.runner.as_mut()
                    && let Err(error) = runner.interrupt(&turn_id).await
                {
                    self.report_operation_error(SideOperation::Interrupt, Some(error))
                        .await;
                }
                // 真正的结束由流自己上报:pump 在 join 完 core 任务后才发终态,
                // 所以这里不伪造 Idle。
            }
            SideDriverCommand::Close {
                expected_target,
                expected_active_turn_id,
            } => {
                if !self.target_matches(&expected_target) {
                    self.report_stale_target(SideOperation::Close, &expected_target)
                        .await;
                    return;
                }
                let outcome = self
                    .close(expected_active_turn_id, CloseReason::UserClosed)
                    .await;
                self.report_operation_error(SideOperation::Close, outcome.err())
                    .await;
            }
            SideDriverCommand::ParentBarrier { reason, reply } => {
                self.begin_parent_barrier(reason, reply);
            }
            SideDriverCommand::Exit => {} // 由 run 的分支处理
        }
    }

    /// 打开旁支。
    ///
    /// 已有旁支时重复 open 是幂等的:把现有 id 再报一次让 TUI 切过去,而不是
    /// 造第二个 runner（单父单旁支）。
    async fn open(&mut self, request_id: String, question: Option<String>) -> SideResult<()> {
        if let Some(reason) = self.barrier {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                format!("父会话正在变更（{}），请稍后再开旁支", reason.as_str()),
            ));
        }
        if self.runner.is_some() {
            self.announce_existing().await?;
            if let Some(question) = question {
                self.submit(request_id, question).await?;
            }
            return Ok(());
        }

        // 快照是唯一的读父入口:它给出独立 Session、固定 config 与同一 provider。
        let snapshot = self.publisher.snapshot().await?;
        let side_id = uuid::Uuid::new_v4().to_string();
        let lease = self.resources.reserve_slot(&side_id)?;
        let cancel = CancellationToken::new();
        let prepared = self
            .resources
            .prepare(Arc::clone(&lease), &snapshot, cancel)
            .await?;
        let tools = SideToolSetupFactory::new(
            snapshot.clone(),
            Arc::clone(&prepared.access),
            Arc::clone(&self.pool),
            side_id.clone(),
            Arc::clone(&prepared.lease) as Arc<dyn Send + Sync>,
        );
        let runner = SideConversationRunner::new(side_id.clone(), snapshot, prepared, tools)?;

        self.transcript = Some(SideTranscript::side_defaults());
        self.deadline = Some(SideIdleDeadline::new(tokio::time::Instant::now()));
        self.slot = Some(lease);
        self.runner = Some(runner);
        self.announce_existing().await?;

        if let Some(question) = question {
            self.submit(request_id, question).await?;
        }
        Ok(())
    }

    async fn announce_existing(&mut self) -> SideResult<()> {
        let Some(runner) = self.runner.as_ref() else {
            return Ok(());
        };
        let (side_id, generation, status) = (
            runner.side_id().to_string(),
            runner.generation(),
            runner.status(),
        );
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id: side_id.to_string(),
                generation,
            },
            None,
            ConversationPayload::SideStatus(status),
        )
        .await
    }

    /// 接受一轮并开始消费它的事件流。
    async fn submit(&mut self, request_id: String, text: String) -> SideResult<()> {
        let Some(runner) = self.runner.as_mut() else {
            return Err(SideError::new(
                SideErrorReason::SideNotFound,
                "还没有打开的旁支，先用 /side",
            ));
        };
        // 重试对账必须发生在 busy 检查之前:第一次尝试已经开始后重发同一请求,
        // 期望拿回原轮次而不是 turn_in_progress。
        if let Some(existing) = self.ledger.lookup(&request_id, &text)? {
            self.report_turn_id(&existing).await?;
            return Ok(());
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.ledger.accept(request_id, &text, turn_id.clone())?;
        let stream = runner.start_turn(turn_id.clone(), text.clone()).await?;
        if let Some(transcript) = self.transcript.as_mut() {
            let items = transcript.begin_turn(&turn_id, &text)?;
            self.queue_items(items);
        }
        if let Some(deadline) = self.deadline.as_mut() {
            deadline.touch(tokio::time::Instant::now());
        }
        self.active = Some(ActiveTurn {
            turn_id: turn_id.clone(),
            stream,
        });
        self.report_turn_id(&turn_id).await
    }

    /// 处理一帧 runner 事件。
    async fn handle_frame(&mut self, frame: Option<SideRunnerEvent>) {
        let Some(frame) = frame else {
            // 流结束但没经过终态帧（例如通道被关）:仍要收尾,否则状态会停在
            // Running。
            self.finish_turn().await;
            return;
        };
        let terminal = matches!(
            frame.event.event(),
            yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Error(_)
                | yi_agent_core::AgentEvent::Cancelled
        );
        let turn_id = frame.turn_id.clone();
        if let Some(transcript) = self.transcript.as_mut()
            && let Ok(items) = transcript.apply(&turn_id, frame.event.event())
        {
            let owned: Vec<_> = items
                .into_iter()
                .filter(|item| item.turn_id == turn_id)
                .collect();
            self.queue_items(owned);
        }
        if terminal {
            // 终态之前必须把这一轮的正文发全,否则客户端会永久缺最后一小段。
            self.flush_items().await;
            self.finish_turn().await;
        }
    }

    /// 一轮结束:更新 usage、清空 active、把状态放回 Idle/Failed。
    async fn finish_turn(&mut self) {
        self.active = None;
        let Some(runner) = self.runner.as_mut() else {
            return;
        };
        // 真实终止（含 core 任务 join）由 runner 在流结束后完成;等它发布终态
        // 再读,避免把「刚 cancel」误报成「已 idle」。
        let _ = runner.wait_quiescent().await;
        let usage = runner.usage();
        let status = if runner.last_error().is_some() {
            SideStatus::Failed
        } else {
            SideStatus::Idle
        };
        if let Some(deadline) = self.deadline.as_mut() {
            deadline.touch(tokio::time::Instant::now());
        }
        let (side_id, generation) = (runner.side_id().to_string(), runner.generation());
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id: side_id.to_string(),
                generation,
            },
            None,
            ConversationPayload::SideUsage(usage),
        )
        .await
        .ok();
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id: side_id.to_string(),
                generation,
            },
            None,
            ConversationPayload::SideStatus(status),
        )
        .await
        .ok();
    }

    /// 关闭旁支并等待真实任务结束。
    async fn close(
        &mut self,
        expected_active_turn_id: Option<String>,
        reason: CloseReason,
    ) -> SideResult<()> {
        let Some(mut runner) = self.runner.take() else {
            return Ok(());
        };
        let actual = runner.active_turn_id().map(str::to_string);
        if expected_active_turn_id.is_some() && expected_active_turn_id != actual {
            // 条件冲突:把 runner 放回去,什么都不改。误停一个更新的轮次比拒绝
            // 这次关闭糟得多。
            self.runner = Some(runner);
            return Err(SideError::new(
                SideErrorReason::CloseConflict,
                "旁支已经开始了新的一轮，请确认后再关闭",
            ));
        }
        let side_id = runner.side_id().to_string();
        let generation = runner.generation();
        let outcome = runner.close().await;
        self.active = None;
        self.pending.clear();
        self.flush_at = None;
        self.transcript = None;
        self.deadline = None;
        // 槽位随 lease 的最后一个 owner 释放;目录的延迟清理也由它管。
        self.slot = None;
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id: side_id.to_string(),
                generation,
            },
            None,
            ConversationPayload::SideClosed(reason),
        )
        .await
        .ok();
        outcome
    }

    /// 建立父变更屏障,并把许可交给调用方。
    fn begin_parent_barrier(
        &mut self,
        reason: CloseReason,
        reply: oneshot::Sender<ParentChangePermit>,
    ) {
        let (outcome_tx, outcome_rx) = oneshot::channel();
        if reply
            .send(ParentChangePermit {
                outcome: Some(outcome_tx),
            })
            .is_err()
        {
            // 调用方已经走了:不建立屏障,避免永久挂住。
            return;
        }
        self.barrier = Some(reason);
        self.barrier_outcome = Some(outcome_rx);
    }

    /// 父变更结果落定。
    ///
    /// `Commit` 时旁支作废并推进代际;`Rollback`（含调用方丢弃许可）时解除
    /// 屏障,旧旁支原样可用。
    async fn settle_parent_barrier(&mut self, outcome: Option<ParentChangeOutcome>) {
        self.barrier_outcome = None;
        let Some(reason) = self.barrier.take() else {
            return;
        };
        // 发送端被丢弃（调用方 panic）等价于回滚:宁可保留旧旁支,也不能在主
        // 操作没成功时把它作废。
        if outcome != Some(ParentChangeOutcome::Commit) {
            return;
        }
        let _ = self.close(None, reason).await;
    }

    /// TTL 到期:与显式关闭同一条路径,只是原因不同。
    async fn expire_idle(&mut self) {
        let Some(deadline) = self.deadline.as_ref() else {
            return;
        };
        let now = tokio::time::Instant::now();
        if !deadline.is_expired(now) {
            return;
        }
        let running = self
            .runner
            .as_ref()
            .map(|runner| runner.status() == SideStatus::Running)
            .unwrap_or(false);
        if running {
            // 正在跑的一轮不算空闲:10 分钟上限由 runner 自己管。
            if let Some(deadline) = self.deadline.as_mut() {
                deadline.touch(now);
            }
            return;
        }
        let _ = self.close(None, CloseReason::Expired).await;
    }

    async fn shutdown(&mut self) {
        // 退出必须真正停掉这一路:close 会 join 真实 core 任务。
        let _ = self.close(None, CloseReason::HostShutdown).await;
    }

    fn queue_items(&mut self, items: Vec<Arc<SideItem>>) {
        if items.is_empty() {
            return;
        }
        for item in items {
            match self
                .pending
                .iter()
                .position(|existing| existing.id == item.id)
            {
                Some(index) => {
                    if self.pending[index].ordinal <= item.ordinal {
                        self.pending[index] = item;
                    }
                }
                None => {
                    let position = self
                        .pending
                        .iter()
                        .position(|existing| existing.ordinal > item.ordinal)
                        .unwrap_or(self.pending.len());
                    self.pending.insert(position, item);
                }
            }
        }
        if self.flush_at.is_none() {
            self.flush_at = Some(tokio::time::Instant::now() + SIDE_FLUSH_INTERVAL);
        }
    }

    async fn flush_items(&mut self) {
        if self.pending.is_empty() {
            self.flush_at = None;
            return;
        }
        let items = std::mem::take(&mut self.pending);
        self.flush_at = None;
        let Some(runner) = self.runner.as_ref() else {
            return;
        };
        let (side_id, generation) = (runner.side_id().to_string(), runner.generation());
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id: side_id.to_string(),
                generation,
            },
            None,
            ConversationPayload::SideItems(items),
        )
        .await
        .ok();
    }

    async fn report_turn_id(&mut self, turn_id: &str) -> SideResult<()> {
        let Some(runner) = self.runner.as_ref() else {
            return Ok(());
        };
        let (side_id, generation, status) = (
            runner.side_id().to_string(),
            runner.generation(),
            runner.status(),
        );
        dispatch(
            &self.events,
            ConversationTarget::Side {
                side_id,
                generation,
            },
            Some(turn_id.to_string()),
            ConversationPayload::SideStatus(status),
        )
        .await
    }

    /// 当前 runner 是否就是调用方以为的那个旁支。
    fn target_matches(&self, expected: &ConversationTarget) -> bool {
        match (expected, self.runner.as_ref()) {
            (
                ConversationTarget::Side {
                    side_id,
                    generation,
                },
                Some(runner),
            ) => runner.side_id() == side_id && runner.generation() == *generation,
            // 主目标不是旁支调度的动作对象。
            (ConversationTarget::Main, _) => false,
            (_, None) => false,
        }
    }

    /// 报告一个针对**已不匹配目标**的操作失败。
    ///
    /// 目标已经不在了，所以没有 runner、也没有转录可写：错误必须靠独立帧
    /// 回给发起它的那个视图，否则就是「按了关闭，界面什么也没发生」。
    async fn report_stale_target(
        &mut self,
        operation: SideOperation,
        expected: &ConversationTarget,
    ) {
        let message = match operation {
            SideOperation::Close => "旁问已经变化（可能已关闭或被替换），请刷新后再试",
            SideOperation::Interrupt => "旁问已经变化（可能已关闭或被替换），停止未生效",
            _ => "旁问已经变化（可能已关闭或被替换），请重试",
        };
        let _ = self
            .dispatch_operation_error(expected.clone(), operation, message.to_string())
            .await;
    }

    /// 把一次操作失败同时写进转录与独立错误帧。
    ///
    /// 两条路径都不是冗余的：转录让**正在看**这个旁支的人看到前后文，独立帧
    /// 让**已经回到主视图**的人也能看到后台关闭失败——后者恰恰是最容易静默
    /// 的一种（主视图不渲染旁支转录）。
    async fn report_operation_error(&mut self, operation: SideOperation, error: Option<SideError>) {
        let Some(error) = error else {
            return;
        };
        tracing::warn!(
            reason = error.reason.as_str(),
            message = %error.message,
            operation = ?operation,
            "side driver error"
        );
        let Some(runner) = self.runner.as_ref() else {
            // `Open` 失败时还没有 runner：这是旧实现静默丢掉错误的那个分支。
            // 错误必须回到发起方，目标用 `Main`——它是「用户在主视图里敲了
            // /side」的那次请求，不属于任何旁支身份。
            let _ = self
                .dispatch_operation_error(
                    ConversationTarget::Main,
                    operation,
                    error.message.clone(),
                )
                .await;
            return;
        };
        let target = ConversationTarget::Side {
            side_id: runner.side_id().to_string(),
            generation: runner.generation(),
        };
        let turn_id = runner.active_turn_id().unwrap_or_default().to_string();
        if let Some(transcript) = self.transcript.as_mut()
            && let Ok(items) = transcript.push_notice(&turn_id, &error.message)
        {
            self.queue_items(items);
            self.flush_items().await;
        }
        let _ = self
            .dispatch_operation_error(target, operation, error.message)
            .await;
    }

    async fn dispatch_operation_error(
        &mut self,
        target: ConversationTarget,
        operation: SideOperation,
        message: String,
    ) -> SideResult<()> {
        dispatch(
            &self.events,
            target,
            None,
            ConversationPayload::SideOperationError { operation, message },
        )
        .await
    }

    fn ui_gone() -> SideError {
        SideError::new(SideErrorReason::Cancelled, "TUI 已退出，旁支事件无人接收")
    }
}

/// 把一条事件投给 TUI。
///
/// 自由函数而不是 `&self` 方法:`run` 的 future 里持有 runner 的事件流,它只
/// 保证 `Send` 而不保证 `Sync`,任何借 `&SideDriver` 的 await 都会让整个任务
/// 不再是 `Send`。这里只借通道,而 `mpsc::Sender` 是 `Sync` 的。
async fn dispatch(
    events: &mpsc::Sender<ConversationEvent>,
    target: ConversationTarget,
    turn_id: Option<String>,
    event: ConversationPayload,
) -> SideResult<()> {
    events
        .send(ConversationEvent {
            target,
            turn_id,
            event,
        })
        .await
        .map_err(|_| SideDriver::ui_gone())
}

/// 取下一帧;流结束时返回 `None`,没有活跃轮时返回一个永不就绪的 future
/// （该分支同时被 `if` 守卫关掉,这里只是让类型成立）。
async fn next_frame(active: &mut Option<ActiveTurn>) -> Option<SideRunnerEvent> {
    match active.as_mut() {
        Some(turn) => turn.stream.next().await,
        None => std::future::pending().await,
    }
}

async fn barrier_outcome(
    outcome: &mut Option<oneshot::Receiver<ParentChangeOutcome>>,
) -> Option<ParentChangeOutcome> {
    match outcome.as_mut() {
        Some(receiver) => receiver.await.ok(),
        None => std::future::pending().await,
    }
}

async fn sleep_until_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// 旁支通道对。
pub struct SideDriverHandles {
    pub commands: mpsc::Sender<SideDriverCommand>,
    pub events: mpsc::Receiver<ConversationEvent>,
    pub join: tokio::task::JoinHandle<()>,
}

/// 建立命令/事件通道并启动调度任务。
pub fn spawn_side_driver(
    publisher: Arc<SideSourcePublisher>,
    resources: Arc<SideHostResources>,
    pool: Arc<yi_agent_tools::SideBlockingPool>,
) -> SideDriverHandles {
    let (commands_tx, commands_rx) = mpsc::channel(SIDE_COMMAND_CHANNEL);
    let (events_tx, events_rx) = mpsc::channel(SIDE_EVENT_CHANNEL);
    let join = SideDriver::spawn(publisher, resources, pool, commands_rx, events_tx);
    SideDriverHandles {
        commands: commands_tx,
        events: events_rx,
        join,
    }
}

/// 旁支实例根目录（`side-conversations/` 的父目录）。
///
/// 与 app-server 用同一处实例状态根,所以两个宿主的旁支目录并列而不冲突:
/// 每个宿主进程有自己的 `host_instance_id` 子目录与排他锁。
pub fn side_host_root(workdir: &std::path::Path) -> PathBuf {
    yi_agent_store::paths::instance_home().unwrap_or_else(|| workdir.join(".yi-agent"))
}

/// 旁支只读工具白名单,供 `/help` 与顶部提示展示同一份来源。
pub fn allowed_side_tools() -> Vec<&'static str> {
    SideToolPolicy::default().allowed_names().to_vec()
}

#[cfg(test)]
mod tests;
