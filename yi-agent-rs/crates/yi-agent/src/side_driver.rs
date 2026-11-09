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
    CloseReason, SIDE_STOP_GRACE, SideConversationRunner, SideError, SideErrorReason,
    SideHostResources, SideIdleDeadline, SideItem, SideRequestLedger, SideResourceLease,
    SideResult, SideRunnerEvent, SideSourcePublisher, SideStatus, SideToolPolicy,
    SideToolSetupFactory, SideTranscript,
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
    /// 进入旁支;`question` 非空时顺带作为第一轮问题。
    ///
    /// `entry_id` 是**本次进入**的进程内标识。它与 `request_id` 分工不同：
    /// `request_id` 去重一次提问，`entry_id` 决定「这条命令属于哪一次进入」。
    /// 用户退出后迟到的创建完成、迟到的问题，都要靠它被识别成过期。
    Open {
        entry_id: String,
        request_id: String,
        question: Option<String>,
    },
    /// 在已打开的旁支里提一个新问题。
    Submit {
        entry_id: String,
        request_id: String,
        text: String,
    },
    /// 退出**一次进入**并销毁环境（`/back` 与 `/side-close` 同一条路）。
    ///
    /// 与 `Exit` 的区别是它只作用于这次进入：迟到/不匹配的 entry 不做事，
    /// 也绝不结束整个 actor——否则用户在主视图里按一次返回，旁支功能就没了。
    Leave { entry_id: String },
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
    entry_id: String,
    turn_id: String,
    stream: futures::stream::BoxStream<'static, SideRunnerEvent>,
}

/// 一次「进入」的进行中构造。
///
/// 创建必须**可取消**（spec §8）：用户在快照/资源准备期间按返回，命令循环
/// 仍要受理。所以构造跑在独立任务里，owner 只留缺席、取消 token 与进度。
struct CreatingEntry {
    entry_id: String,
    cancel: CancellationToken,
    /// 只被消费一次：等结果时 `take()` 出来，回包不可能被重复等待。
    rx: Option<oneshot::Receiver<SideResult<Constructed>>>,
    /// 构造期间到达的首问：只有这次进入仍然有效时才会被发出去。
    pending_question: Option<(String, String)>,
}

/// 一次正在排空的退出。
///
/// 保留它的 `entry_id`：排空失败时错误必须指名是**哪一次进入**没清理干净，
/// 而那时新的一次进入可能已经开始了。
struct DrainingEntry {
    entry_id: String,
    handle: tokio::task::JoinHandle<SideResult<()>>,
}

/// 构造完成后的产物。
struct Constructed {
    runner: SideConversationRunner,
    lease: Arc<SideResourceLease>,
}

/// 旁支调度任务持有的全部状态。
pub struct SideDriver {
    commands: mpsc::Receiver<SideDriverCommand>,
    events: mpsc::Sender<ConversationEvent>,
    publisher: Arc<SideSourcePublisher>,
    resources: Arc<SideHostResources>,
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
    /// 当前**唯一**一次进入的标识（spec §8「UI 以本次入场 token 授权帧」）。
    ///
    /// 帧、提交、退出都要核对它：一个已经离开的进入所产生的迟到结果不能写进
    /// 新视图，也不能被当成「用户对现在的旁支做了什么」。
    entry_id: Option<String>,
    /// 正在构造中的那次进入。
    creating: Option<CreatingEntry>,
    /// 正在排空的上一代环境（spec §6.1：清理未排空时拒绝新环境，不并行建）。
    draining: Option<DrainingEntry>,
    /// 仅测试用：构造前必须取得的一个许可。
    ///
    /// 生产里创建慢的原因是快照与资源准备（真 IO），测试不能靠「等一秒看看有
    /// 没有输出」来判断命令循环是否被阻塞——那种断言既慢又假。这里用一个
    /// 显式的门：测试拿走唯一许可，创建就停在**已知位置**，直到测试放行。
    #[cfg(test)]
    creation_gate: Option<Arc<tokio::sync::Semaphore>>,
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
        commands: mpsc::Receiver<SideDriverCommand>,
        events: mpsc::Sender<ConversationEvent>,
    ) -> tokio::task::JoinHandle<()> {
        Self::spawn_inner(publisher, resources, commands, events, None)
    }

    /// 仅测试：带「构造门」启动，用来断言命令循环不被创建阻塞。
    #[cfg(test)]
    pub fn spawn_with_creation_gate(
        publisher: Arc<SideSourcePublisher>,
        resources: Arc<SideHostResources>,
        commands: mpsc::Receiver<SideDriverCommand>,
        events: mpsc::Sender<ConversationEvent>,
        gate: Arc<tokio::sync::Semaphore>,
    ) -> tokio::task::JoinHandle<()> {
        Self::spawn_inner(publisher, resources, commands, events, Some(gate))
    }

    fn spawn_inner(
        publisher: Arc<SideSourcePublisher>,
        resources: Arc<SideHostResources>,
        commands: mpsc::Receiver<SideDriverCommand>,
        events: mpsc::Sender<ConversationEvent>,
        // 生产构建里没有测试门，参数用不到；显式忽略而不是条件编译签名，
        // 那样 `spawn` 与 `spawn_with_creation_gate` 才能共用一个实现。
        #[cfg_attr(not(test), allow(unused_variables))] gate: Option<Arc<tokio::sync::Semaphore>>,
    ) -> tokio::task::JoinHandle<()> {
        let driver = Self {
            commands,
            events,
            publisher,
            resources,
            ledger: SideRequestLedger::new(),
            slot: None,
            runner: None,
            transcript: None,
            deadline: None,
            active: None,
            pending: Vec::new(),
            flush_at: None,
            entry_id: None,
            creating: None,
            draining: None,
            #[cfg(test)]
            creation_gate: gate,
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
                // 构造完成与命令**同等优先**：创建慢时命令循环仍要能受理退出
                // （spec §8「side_driver 的命令循环不能 await 完整个 open 后才
                // 看到退出」）。
                done = creation_outcome(&mut self.creating), if self.creating.is_some() => {
                    self.settle_construction(done).await;
                }
                drained = drain_outcome(&mut self.draining), if self.draining.is_some() => {
                    self.settle_draining(drained).await;
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
                entry_id,
                request_id,
                question,
            } => {
                let outcome = self.open(entry_id, request_id, question).await;
                self.report_operation_error(SideOperation::Open, outcome.err())
                    .await;
            }
            SideDriverCommand::Submit {
                entry_id,
                request_id,
                text,
            } => {
                if !self.entry_matches(&entry_id) {
                    // 迟到的问题属于一次已经结束的进入：**不提交**，也不报错
                    // （用户已经离开，报错只会在他现在看的东西上打字）。
                    return;
                }
                let outcome = self.submit(request_id, text).await;
                self.report_operation_error(SideOperation::Submit, outcome.err())
                    .await;
            }
            SideDriverCommand::Leave { entry_id } => {
                self.leave(&entry_id).await;
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
            SideDriverCommand::ParentBarrier { reason, reply } => {
                self.begin_parent_barrier(reason, reply);
            }
            SideDriverCommand::Exit => {} // 由 run 的分支处理
        }
    }

    /// 进入旁支。
    ///
    /// 三种情形：
    /// - 同一次进入再来一次（重复命令）：幂等，把现有 id 再报一遍让 TUI 切过去；
    /// - 正在为**同一个** entry 构造：不重复 spawn，等它完成；
    /// - 否则开始一次新进入，把构造放到独立任务里（命令循环立刻可继续）。
    async fn open(
        &mut self,
        entry_id: String,
        request_id: String,
        question: Option<String>,
    ) -> SideResult<()> {
        if let Some(reason) = self.barrier {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                format!("父会话正在变更（{}），请稍后再开旁支", reason.as_str()),
            ));
        }
        // 同一个 entry 的重复 open：这一次进入已经在进行/已打开，不造第二套。
        if self.entry_id.as_deref() == Some(entry_id.as_str()) {
            if self.runner.is_some() {
                self.announce_existing().await?;
                if let Some(question) = question {
                    self.submit(request_id, question).await?;
                }
                return Ok(());
            }
            // 构造还在路上：先记下首问，等构造完成再发。
            if let Some(creating) = self.creating.as_mut() {
                creating.pending_question = question.map(|q| (request_id, q));
                return Ok(());
            }
        }
        // 已有环境（另一次进入留在后台的）：复用同一代，不重建快照。
        // `entry_id` 换成这次进入的，于是后续提交/退出都定位到本次停留。
        if self.runner.is_some() {
            self.entry_id = Some(entry_id);
            self.announce_existing().await?;
            if let Some(question) = question {
                self.submit(request_id, question).await?;
            }
            return Ok(());
        }
        // 上一代还在排空：拒绝新建。先放行会让同一父上出现第二套环境，
        // 而旧那套的资源槽还没让出来（spec §6.1 唯一性）。
        if let Some(draining) = self.draining.as_ref() {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                format!("上一条旁问正在退出（{}），请稍后重试", draining.entry_id),
            ));
        }
        // 已有一次构造在跑（不同 entry）：拒绝第二个，绝不并行建两套。
        if self.creating.is_some() {
            return Err(SideError::new(
                SideErrorReason::SideLimit,
                "旁问正在创建，请稍候",
            ));
        }
        self.entry_id = Some(entry_id.clone());
        let cancel = CancellationToken::new();
        let (tx, rx) = oneshot::channel();
        let publisher = Arc::clone(&self.publisher);
        let resources = Arc::clone(&self.resources);
        let constructing_entry = entry_id.clone();
        #[cfg(test)]
        let gate = self.creation_gate.clone();
        #[cfg(not(test))]
        let gate = None;
        // 快照与资源准备都可能慢；它们在独立任务里跑，命令循环继续受理退出。
        let task_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = tx.send(
                construct_side(publisher, resources, constructing_entry, task_cancel, gate).await,
            );
        });
        self.creating = Some(CreatingEntry {
            entry_id,
            cancel,
            rx: Some(rx),
            pending_question: question.map(|q| (request_id, q)),
        });
        Ok(())
    }

    /// 构造任务的回包落定。
    async fn settle_construction(&mut self, outcome: Option<SideResult<Constructed>>) {
        let Some(creating) = self.creating.take() else {
            return;
        };
        // 要发送的首问只在这里取；后面的分支都不会把它漏给别人。
        let pending = creating.pending_question;
        let Some(result) = outcome else {
            // 构造任务消失（panic 或被丢弃）：当作失败，但只在这次进入仍然
            // 有效时才报——用户已经离开的话，他不该收到一条「打开失败」。
            if self.entry_matches(&creating.entry_id) {
                self.entry_id = None;
                let _ = self
                    .dispatch_operation_error(
                        ConversationTarget::Main,
                        SideOperation::Open,
                        "旁问创建被中断".to_string(),
                    )
                    .await;
            }
            return;
        };
        match result {
            Ok(constructed) => {
                // 用户已经离开：构造出来的东西**立刻销毁**，不 announce、不发首问。
                if !self.entry_matches(&creating.entry_id) {
                    let mut runner = constructed.runner;
                    let _ = runner.close().await;
                    drop(constructed.lease);
                    return;
                }
                self.transcript = Some(SideTranscript::side_defaults());
                self.deadline = Some(SideIdleDeadline::new(tokio::time::Instant::now()));
                self.slot = Some(constructed.lease);
                self.runner = Some(constructed.runner);
                if let Err(error) = self.announce_existing().await {
                    self.report_operation_error(SideOperation::Open, Some(error))
                        .await;
                    return;
                }
                if let Some((request_id, question)) = pending
                    && let Err(error) = self.submit(request_id, question).await
                {
                    self.report_operation_error(SideOperation::Submit, Some(error))
                        .await;
                }
            }
            Err(error) => {
                if !self.entry_matches(&creating.entry_id) {
                    return;
                }
                self.entry_id = None;
                self.report_operation_error(SideOperation::Open, Some(error))
                    .await;
            }
        }
    }

    /// 退出一次进入（`/back`、`/side-close` 都走这里）。
    ///
    /// 与 `Close` 的差别：`Close` 是带轮次条件的老入口（保留兼容），`Leave`
    /// 只认 entry，于是「用户确认的是哪一次进入」与「要销毁的是哪一套」是同
    /// 一个判断。销毁本身放在独立任务里：runner close 要等 core 任务 join，
    /// 最长 5 秒（`SIDE_STOP_GRACE`），命令循环不能在这段里失去对新的 `/side`
    /// 与退出的受理能力。
    async fn leave(&mut self, entry_id: &str) {
        if !self.entry_matches(entry_id) {
            // 过期 entry：这次进入已经结束。什么都不做——尤其绝不关闭现在这一
            // 套（那是用户之后重新进入建出来的）。
            return;
        }
        // 正在构造：取消它。构造完成分支会看到 entry 已失效，自己销毁产物。
        if let Some(creating) = self.creating.as_mut() {
            creating.cancel.cancel();
            creating.pending_question = None;
        }
        self.entry_id = None;
        // 立刻撤掉 UI 侧的授权与本地正文：界面不等资源排空，等待只在这里发生。
        let Some(mut runner) = self.runner.take() else {
            self.active = None;
            self.pending.clear();
            self.flush_at = None;
            self.transcript = None;
            self.deadline = None;
            return;
        };
        let side_id = runner.side_id().to_string();
        let generation = runner.generation();
        self.active = None;
        self.pending.clear();
        self.flush_at = None;
        self.transcript = None;
        self.deadline = None;
        // 槽位随 lease 的最后一个 owner 释放，不在取消那一刻先释放：先放占位
        // 会允许同一父出现第二套环境（spec §6.1 唯一性）。
        let slot = self.slot.take();
        let events = self.events.clone();
        // 终态帧仍标上**被退出那次进入**的 entry：它描述的是那一代的结局。
        // TUI 本地此时已经清掉 entry，会按授权规则丢弃它——这正是对的，
        // 界面不该为一次已经离开的进入再建视图；但事件本身仍需可追溯。
        let closing_entry = Some(entry_id.to_string());
        let started = tokio::time::Instant::now();
        // 排空任务：runner close 自己完成 stop/join 与资源确认，之后才丢 lease。
        let handle = tokio::spawn(async move {
            let outcome = runner.close().await;
            drop(runner);
            drop(slot);
            // 只有**真的**清理干净才发终态。失败不发 `SideClosed`：那会让
            // 界面显示「已关闭」而资源还占着，重试也就没人会做了。失败由
            // `settle_draining` 报一条主视图错误。
            if outcome.is_ok() {
                dispatch(
                    &events,
                    closing_entry,
                    ConversationTarget::Side {
                        side_id: side_id.to_string(),
                        generation,
                    },
                    None,
                    ConversationPayload::SideClosed(CloseReason::UserClosed),
                )
                .await
                .ok();
            } else {
                tracing::warn!(
                    side_id = %side_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "side leave drain failed"
                );
            }
            outcome
        });
        self.draining = Some(DrainingEntry {
            entry_id: entry_id.to_string(),
            handle,
        });
    }

    /// 排空完成后的收尾。
    ///
    /// 成功才清掉「上一代尚未销毁」这个事实。失败保留提示：资源可能还占着，
    /// 用户需要知道重试而不是以为已经干净了。
    async fn settle_draining(&mut self, outcome: Option<SideResult<()>>) {
        let Some(draining) = self.draining.take() else {
            return;
        };
        let joined = outcome.unwrap_or_else(|| {
            Err(SideError::new(
                SideErrorReason::Cancelled,
                "旁问退出任务被中断",
            ))
        });
        if let Err(error) = joined {
            tracing::warn!(
                entry_id = %draining.entry_id,
                reason = error.reason.as_str(),
                message = %error.message,
                "side leave drain failed"
            );
            let _ = self
                .dispatch_operation_error(
                    ConversationTarget::Main,
                    SideOperation::Close,
                    format!("旁问退出尚未确认（{}）", error.message),
                )
                .await;
        }
    }

    /// 这条命令/帧是否属于当前这次进入。
    fn entry_matches(&self, entry_id: &str) -> bool {
        self.entry_id.as_deref() == Some(entry_id)
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
        let entry = self.entry_id.clone();
        dispatch(
            &self.events,
            entry,
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
            // 轮次挂到**当前这次进入**上：进入被结束之后，迟到的事件流不能
            // 再往新视图里写上一轮的内容。
            entry_id: self.entry_id.clone().unwrap_or_default(),
            turn_id: turn_id.clone(),
            stream,
        });
        self.report_turn_id(&turn_id).await
    }

    /// 处理一帧 runner 事件。
    async fn handle_frame(&mut self, frame: Option<SideRunnerEvent>) {
        // 上一轮属于一次已经结束的进入：丢弃它的收尾副作用（不推 items、不改
        // 状态），否则用户在新视图里会看到上一段旁支的尾巴。
        if let Some(active) = self.active.as_ref()
            && !self.entry_matches(&active.entry_id)
        {
            self.active = None;
            return;
        }
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
        let entry = self.entry_id.clone();
        dispatch(
            &self.events,
            entry.clone(),
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
            entry,
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
    ///
    /// 无条件的：`/side-close`、`/back`、父变更、空闲回收、宿主退出都表达
    /// 「这一代到此为止」，spec §4.3 明确不再二次询问「是否停止当前回答」。
    /// 旧实现按调用方的 `active_turn_id` 拒绝一次「正在回答时的关闭」，那让
    /// 用户按了返回却退不出去——正是这次要修的症状。
    async fn close(&mut self, reason: CloseReason) -> SideResult<()> {
        let Some(mut runner) = self.runner.take() else {
            return Ok(());
        };
        let side_id = runner.side_id().to_string();
        let generation = runner.generation();
        let entry = self.entry_id.clone();
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
            entry,
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
        let _ = self.close(reason).await;
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
        let _ = self.close(CloseReason::Expired).await;
    }

    async fn shutdown(&mut self) {
        // 应用退出时，构造中的那一次也必须被取消：它的产物会在
        // `settle_construction` 看到 entry 已失效后自行销毁。不取消就会留一个
        // 没人要的环境在后台继续准备资源。
        if let Some(creating) = self.creating.as_mut() {
            creating.cancel.cancel();
            creating.pending_question = None;
        }
        self.entry_id = None;
        // 退出必须真正停掉这一路:close 会 join 真实 core 任务。
        let _ = self.close(CloseReason::HostShutdown).await;
        // 上一代若还在排空，等它结束：进程退出前不该留下自认为「会自己收尾」
        // 的任务。`close` 已经把本代停掉，这里等的只是另一次更早的退出。
        if let Some(draining) = self.draining.take() {
            let _ = tokio::time::timeout(SIDE_STOP_GRACE, draining.handle).await;
        }
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
        let entry = self.entry_id.clone();
        dispatch(
            &self.events,
            entry,
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
            self.entry_id.clone(),
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
        // 错误帧**不**带 entry：它必须永远能被看见，哪怕它讲的是一次已经
        // 结束的进入（那正是「你按的退出为什么没生效」）。
        dispatch(
            &self.events,
            None,
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

/// 在独立任务里建立一次旁支的运行时对象。
///
/// 单独成函数有两个原因：它要跑在 `tokio::spawn` 里（不能借 `&mut SideDriver`），
/// 而取消必须在这里被**再次**检查——命令循环收到退出到取消生效之间还有几个
/// await，只在上游判一次会让一个已经没人要的环境建出来。
async fn construct_side(
    publisher: Arc<SideSourcePublisher>,
    resources: Arc<SideHostResources>,
    entry_id: String,
    cancel: CancellationToken,
    gate: Option<Arc<tokio::sync::Semaphore>>,
) -> SideResult<Constructed> {
    // 测试门：拿不到许可就在这里等，取消则立刻退出。生产 `gate` 恒为 None。
    //
    // 必须与 `cancel` 同时等待：一个「卡在门上」的构造如果对取消失聪，测试就
    // 只能靠超时判断，而那正好复现不了「退出能否取消创建」这条断言。
    if let Some(gate) = gate {
        let acquired = tokio::select! {
            permit = gate.acquire_owned() => permit.is_ok(),
            () = cancel.cancelled() => false,
        };
        if !acquired {
            return Err(SideError::new(
                SideErrorReason::Cancelled,
                "旁支在创建期间被取消",
            ));
        }
    }
    // 快照是唯一的读父入口:它给出独立 Session、固定 config 与同一 provider。
    let snapshot = publisher.snapshot().await?;
    if cancel.is_cancelled() {
        return Err(SideError::new(
            SideErrorReason::Cancelled,
            "旁支在创建期间被取消",
        ));
    }
    let side_id = uuid::Uuid::new_v4().to_string();
    let lease = resources.reserve_slot(&side_id)?;
    let prepared = resources
        .prepare(Arc::clone(&lease), &snapshot, cancel.clone())
        .await?;
    if cancel.is_cancelled() {
        // 资源已经预留：丢掉 lease 让清理线程回收，绝不把半成品交给 owner。
        return Err(SideError::new(
            SideErrorReason::Cancelled,
            "旁支在资源准备期间被取消",
        ));
    }
    // 旁支没有工具；factory 只携带它的身份。
    let tools = SideToolSetupFactory::new(side_id.clone());
    let runner = SideConversationRunner::new(side_id, snapshot, prepared, tools)?;
    let _ = entry_id;
    Ok(Constructed { runner, lease })
}

/// 取一次构造结果；没在构造时返回一个永不就绪的 future（该分支同样被守卫关掉）。
async fn creation_outcome(creating: &mut Option<CreatingEntry>) -> Option<SideResult<Constructed>> {
    match creating.as_mut().and_then(|creating| creating.rx.take()) {
        Some(rx) => rx.await.ok(),
        // 守卫保证不会走到这里；返回 None 会被当作「构造任务消失」处理。
        None => None,
    }
}

/// 取一次排空结果；没有排空任务时永不就绪（该分支同时被守卫关掉）。
async fn drain_outcome(draining: &mut Option<DrainingEntry>) -> Option<SideResult<()>> {
    match draining.as_mut() {
        Some(draining) => Some((&mut draining.handle).await.unwrap_or_else(|_| {
            Err(SideError::new(
                SideErrorReason::Cancelled,
                "旁问退出任务被中断",
            ))
        })),
        None => std::future::pending().await,
    }
}

/// 把一条事件投给 TUI。
///
/// 自由函数而不是 `&self` 方法:`run` 的 future 里持有 runner 的事件流,它只
/// 保证 `Send` 而不保证 `Sync`,任何借 `&SideDriver` 的 await 都会让整个任务
/// 不再是 `Send`。这里只借通道,而 `mpsc::Sender` 是 `Sync` 的。
async fn dispatch(
    events: &mpsc::Sender<ConversationEvent>,
    entry_id: Option<String>,
    target: ConversationTarget,
    turn_id: Option<String>,
    event: ConversationPayload,
) -> SideResult<()> {
    events
        .send(ConversationEvent {
            entry_id,
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
) -> SideDriverHandles {
    let (commands_tx, commands_rx) = mpsc::channel(SIDE_COMMAND_CHANNEL);
    let (events_tx, events_rx) = mpsc::channel(SIDE_EVENT_CHANNEL);
    let join = SideDriver::spawn(publisher, resources, commands_rx, events_tx);
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

/// 旁支可用的工具集合,供 `/help` 展示。
///
/// 恒为空:旁支只回答进入时的对话快照,不读文件、不联网、不调 MCP。保留这个
/// 函数是让界面文案从策略取值,而不是在 UI 里另写一份"能干什么"。
pub fn allowed_side_tools() -> Vec<&'static str> {
    SideToolPolicy.allowed_names().to_vec()
}

#[cfg(test)]
mod tests;
