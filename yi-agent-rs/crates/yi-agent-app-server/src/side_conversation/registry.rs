//! The process-wide side-conversation registry.
//!
//! One registry per host process, cloned (as an `Arc`) into every transport. That
//! is a correctness requirement rather than an optimisation: a per-connection
//! registry would give the desktop and the phone two different sides for the same
//! parent, each with its own runner and its own copy of the history.
//!
//! Three rules drive the design:
//!
//! 1. **One parent, one side.** `open` is create-or-get. Two concurrent opens
//!    collapse to one runner; the second waits for the first instead of starting a
//!    second provider stream.
//! 2. **A retry is not a new turn.** `turn_start` consults the request ledger
//!    *before* the busy check, so a client that retried after its first attempt was
//!    accepted gets the original turn id back rather than `turn_in_progress`.
//! 3. **A parent change is a barrier, not a notification.** Clearing or deleting a
//!    parent runs `begin_parent_change`, which refuses new opens for that parent
//!    before the destructive work starts and either commits (invalidating the
//!    generation) or rolls back if that work failed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use yi_agent_runtime::side_conversation::{
    CloseReason, SideConversationRunner, SideHostResources, SideIdleDeadline, SideRequestLedger,
    SideRunnerEvent, SideStatus, SideToolSetupFactory, SideTranscript,
};

use yi_agent_runtime::side_conversation::{SideError, SideErrorReason, SideUsage};

use super::lifecycle::{GenerationOwner, Phase};
use super::replay::SIDE_ITEM_FLUSH_INTERVAL;
use super::source::SideParentResolver;
use super::tickets::{EntryTarget, EntryTickets, ExitReceipt, ExitState};

/// How many closed sides a host remembers for idempotent retries.
pub const SIDE_TOMBSTONES: usize = 1024;

/// How long a tombstone lives.
///
/// Five minutes is enough for a reconnect to reconcile (spec §8.1) and short
/// enough that the table cannot become a second history.
pub const SIDE_TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// The wire-facing summary of one side (spec §8.1).
///
/// This is a *wire* type. It intentionally does **not** derive ts-rs: the TS
/// bindings are generated from `yi-agent-app-server`, which may depend on this
/// crate's runtime but not the other way around, so a ts-rs derive here could
/// never reach the exporter. `protocol::SideSummaryWire` mirrors it field for
/// field, and the conversion is an exhaustive `match`/destructure, so adding a
/// field or a status variant breaks compilation instead of silently drifting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SideSummary {
    pub side_id: String,
    pub parent_thread_id: String,
    pub status: SideStatus,
    pub snapshot_revision: u64,
    pub last_seq: u64,
    #[serde(default)]
    pub active_turn_id: Option<String>,
    /// Epoch milliseconds; `None` while `Creating` or `Running`.
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub generation: u64,
    /// `true` while an accepted turn has been asked to stop but has not yet proven
    /// its core task ended.
    pub stopping: bool,
}

/// What a tombstone keeps.
///
/// Deliberately only identifiers and request digests: no text, no items, no
/// provider output. A tombstone answers "did you already accept this?" and
/// nothing else, so it can never be used to recover content after a close.
#[derive(Debug, Clone)]
struct Tombstone {
    side_id: String,
    parent: String,
    generation: u64,
    accepted: Vec<(String, String)>,
    created_at: tokio::time::Instant,
}

/// One side's mutable state, held behind an async mutex because a turn touches it
/// across awaits.
pub(crate) struct SideEntry {
    side_id: String,
    parent: String,
    generation: u64,
    runner: SideConversationRunner,
    ledger: SideRequestLedger,
    deadline: SideIdleDeadline,
    transcript: SideTranscript,
    /// Monotonic state revision. Bumped once per published change.
    seq: AtomicU64,
    /// `true` between an accepted stop and the pump's terminal publication.
    stopping: std::sync::atomic::AtomicBool,
    /// `true` once `close` entered `Closing`.
    closing: bool,
    /// Captured once, when the side is created.
    config: SideEffectiveConfig,
}

impl SideEntry {
    fn summary(&self) -> SideSummary {
        let status = self.runner.status();
        let expires_at = match status {
            SideStatus::Creating | SideStatus::Running | SideStatus::Closing => None,
            _ => Some(epoch_millis(self.deadline.expires_at())),
        };
        SideSummary {
            side_id: self.side_id.clone(),
            parent_thread_id: self.parent.clone(),
            status,
            snapshot_revision: self.runner_revision(),
            last_seq: self.seq.load(Ordering::SeqCst),
            active_turn_id: self.runner.active_turn_id().map(str::to_string),
            expires_at,
            generation: self.generation,
            stopping: self.stopping.load(Ordering::SeqCst),
        }
    }

    fn runner_revision(&self) -> u64 {
        self.generation
    }
}

/// 投递一帧已定稿的修订。
///
/// 提成自由函数是为了让它能随任务搬走:spawn 出去的流任务只持有 hub 与 entry,
/// 没有 `&SideRegistry`。没有 hub(纯 runtime 单测)时是空操作,序号照样已经在
/// 锁内推进过了。
async fn deliver_with(
    hub: Option<Arc<crate::broadcast::Broadcaster>>,
    staged: StagedRevision,
) -> Option<u64> {
    let StagedRevision {
        parent,
        generation,
        seq,
        update,
    } = staged;
    let side_id = update.side_id().map(str::to_string)?;
    if let Some(hub) = hub {
        let notification = update.into_notification(seq, &parent, &side_id, generation);
        // 状态帧按父 thread 键投递,与普通 `turn/completed` 同一条路径。出站拥塞时
        // 等出口泵抽干:终态被丢掉,客户端会永久停在 running。
        let _ = crate::server::write_notification_await(&hub, &notification).await;
    }
    Some(seq)
}

/// 一轮旁支对话的发布任务。
///
/// 只持有 `hub` 与 `entry`:流事件在本任务里翻译并节流,结束时强制 flush 再发
/// 终态。之所以不留在 `SideRegistry::turn_start` 的调用栈里,是因为它要跨整个
/// 流生命周期存活,而请求必须立刻返回 turnId。
struct TurnPublisher {
    hub: Option<Arc<crate::broadcast::Broadcaster>>,
    entry: Arc<Mutex<SideEntry>>,
    turn_id: String,
    /// 已变更但还没发出的 item id。一个 id 在一个窗口内只发**当前完整版本**。
    dirty: std::collections::BTreeSet<String>,
}

impl TurnPublisher {
    async fn run(mut self, stream: futures::stream::BoxStream<'static, SideRunnerEvent>) {
        let mut stream = stream;
        let mut ticker = tokio::time::interval(SIDE_ITEM_FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // 第一个 tick 立即就绪;先吃掉它,否则每轮开头都会白刷一次。
        ticker.tick().await;

        loop {
            tokio::select! {
                biased;
                next = stream.next() => {
                    let Some(event) = next else { break };
                    let mut guard = self.entry.lock().await;
                    if let Ok(items) = guard.transcript.apply(&self.turn_id, event.event.event()) {
                        for item in items {
                            // 只关心本轮的 item:同一条 side 的其它轮已经结束,
                            // 它们的 item 不会再变。
                            if item.turn_id == self.turn_id {
                                self.dirty.insert(item.id.clone());
                            }
                        }
                    }
                    drop(guard);
                }
                _ = ticker.tick() => self.flush_items().await,
            }
        }

        // 结束前强制 flush:客户端必须先拿全这一轮的 item,再看到终态。
        self.flush_items().await;
        self.finish().await;
    }

    /// 发布一个节流窗口内积累的 item 当前版本。
    async fn flush_items(&mut self) {
        if self.dirty.is_empty() {
            return;
        }
        let wanted = std::mem::take(&mut self.dirty);
        let pending: Vec<_> = {
            let guard = self.entry.lock().await;
            guard
                .transcript
                .items()
                .into_iter()
                .filter(|item| wanted.contains(&item.id))
                .collect()
        };
        for item in pending {
            let Ok(wire) = super::wire_item(&item) else {
                // 单条超限是转录层的缺陷;跳过它而不是拖垮整条流,客户端下一次
                // 全量对账会看到真实内容。
                continue;
            };
            let turn_id = item.turn_id.clone();
            let staged = {
                let mut guard = self.entry.lock().await;
                SideRegistry::stage(&mut guard, |entry| super::SidePublishedUpdate::Item {
                    side_id: entry.side_id.clone(),
                    turn_id,
                    item: wire,
                })
            };
            if let Some(staged) = staged {
                deliver_with(self.hub.clone(), staged).await;
            }
        }
    }

    /// 收尾:等泵证明核心任务真的结束,再发布含终态的 completed。
    async fn finish(self) {
        let staged = {
            let mut guard = self.entry.lock().await;
            // 流结束但 `active` 仍在:只有 `wait_quiescent` 能证明泵已经 join 了
            // 真实核心任务并发布了 Idle。不这样做,旁支会永远显示繁忙。
            let _ = guard.runner.wait_quiescent().await;
            guard.stopping.store(false, Ordering::SeqCst);
            guard.deadline.touch(tokio::time::Instant::now());
            let status = guard.summary().status;
            // `reason` 是稳定的机器可读串,`error` 是给人看的短说明:两者同源但
            // 不同用途,客户端按前者分支、按后者展示。
            let failure = guard.runner.last_error().cloned();
            let reason = failure.as_ref().map(|error| error.reason_str().to_string());
            let error = failure.as_ref().map(|error| error.message.clone());
            let usage = super::replay::wire_usage(guard.runner.usage());
            let turn_id = self.turn_id.clone();
            SideRegistry::stage(&mut guard, |entry| super::SidePublishedUpdate::Completed {
                side_id: entry.side_id.clone(),
                turn_id,
                status: crate::protocol::SideStatusWire::from_runtime(status),
                reason,
                error,
                usage,
            })
        };
        if let Some(staged) = staged {
            deliver_with(self.hub, staged).await;
        }
    }
}

/// 一次已在锁内定稿、等待投递的修订。
///
/// 存在这个中间类型的唯一理由是**锁边界**:字段在 entry 锁内读出,帧在锁外发出。
struct StagedRevision {
    parent: String,
    generation: u64,
    seq: u64,
    update: super::SidePublishedUpdate,
}

/// How a parent is currently occupied.
///
/// One parent has at most one generation in any non-terminal phase. A second
/// entry for the same parent binds to that generation instead of creating a
/// second environment, which is what keeps two transports from starting two
/// provider streams over one history.
enum ParentSlot {
    /// One generation exists (creating, open, closing, or failed-cleanup).
    Bound(Arc<GenerationOwner>),
    /// A destructive parent operation holds the parent: new entries are refused
    /// until it commits or rolls back.
    Changing,
}

/// Everything the registry mutates, under one lock.
///
/// Parents, sides and tickets share a lock on purpose. Splitting them is what
/// would allow a "bind this ticket" step to interleave with a "remove that
/// parent" step, so a revoked ticket could still create an environment, or a
/// closed generation could still be reached through a stale binding.
struct RegistryState {
    parents: HashMap<String, ParentSlot>,
    sides: HashMap<String, Arc<GenerationOwner>>,
    tickets: EntryTickets,
    tombstones: HashMap<String, Tombstone>,
}

/// The host's side registry.
pub struct SideRegistry {
    resolver: Arc<dyn SideParentResolver>,
    resources: Arc<SideHostResources>,
    state: Mutex<RegistryState>,
    /// Bumped on every successful create so a side id is never reused.
    next_generation: AtomicU64,
    /// 分页快照登记表。`None` 表示宿主没装(纯 registry 单测),此时关闭不必
    /// 撤销任何 token。
    replay: tokio::sync::Mutex<Option<super::SharedReplayStore>>,
    /// 状态修订的发布出口。`None` 表示没人订阅(纯 runtime 单测),此时
    /// `publish` 仍然推进序号,只是不发帧——序号本身是协议状态,不能因为
    /// 恰好没广播出口就停摆。
    hub: std::sync::Mutex<Option<Arc<crate::broadcast::Broadcaster>>>,
    closed: std::sync::atomic::AtomicBool,
    /// 清扫代次。每次 `close_all()` 自增。
    ///
    /// 构造在把一条 side 对外可见之前要复核它，不一致就放弃。没有它，
    /// 「关掉开关」与「正在创建」并发时会漏：构造要读完父会话、准备资源之后
    /// 才安装 entry，那时清扫可能已经跑完了。
    sweep_epoch: AtomicU64,
}

impl SideRegistry {
    /// The host is constructed without a blocking pool: a side copies no files,
    /// so nothing here needs one. Cleanup uses the pool owned by
    /// [`SideHostResources`], which already outlives every side.
    pub fn new(resolver: Arc<dyn SideParentResolver>, resources: Arc<SideHostResources>) -> Self {
        let host = resources.host_instance_id().to_string();
        Self {
            resolver,
            resources,
            state: Mutex::new(RegistryState {
                parents: HashMap::new(),
                sides: HashMap::new(),
                tickets: EntryTickets::new(host),
                tombstones: HashMap::new(),
            }),
            next_generation: AtomicU64::new(1),
            replay: tokio::sync::Mutex::new(None),
            hub: std::sync::Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
            sweep_epoch: AtomicU64::new(0),
        }
    }

    /// Install the broadcast outlet used for state revisions.
    ///
    /// `serve()` calls this once with its hub. Everything that publishes a revision
    /// (turn acceptance, transcript items, turn completion, close) goes through
    /// [`SideRegistry::publish`], which allocates the sequence number under the same
    /// lock that mutates the state, so a frame can never carry a stale `seq`.
    pub fn attach_hub(&self, hub: Arc<crate::broadcast::Broadcaster>) {
        *self.hub.lock().unwrap_or_else(|p| p.into_inner()) = Some(hub);
    }

    /// Install the host's pagination table.
    ///
    /// The registry needs it for one thing: closing a side must revoke its snapshot
    /// tokens, otherwise a client could keep paging through a generation that no
    /// longer exists.
    pub async fn attach_replay(&self, replay: super::SharedReplayStore) {
        *self.replay.lock().await = Some(replay);
    }

    /// Publish one revision for `side_id`.
    ///
    /// Returns the `seq` it was assigned, or `None` when the side is already gone
    /// (a late completion after close must not resurrect it — the tombstone stays
    /// authoritative).
    ///
    /// The sequence is allocated **under the same lock that reads the state**, so
    /// two concurrent publishers can never emit the same number with different
    /// bodies. It is bumped before the frame leaves the lock: if the process dies in
    /// between, clients see a gap and re-read a snapshot — which is the contract.
    ///
    /// Only tests use this entry point: production paths already hold the entry
    /// lock for a state change and call [`Self::stage`] inside that same critical
    /// section, so they must not acquire it twice.
    #[cfg(test)]
    pub(crate) async fn publish(
        &self,
        side_id: &str,
        build: impl FnOnce(&SideEntry) -> super::SidePublishedUpdate,
    ) -> Option<u64> {
        let entry = self.entry(side_id).await.ok()?;
        let staged = {
            let mut guard = entry.lock().await;
            Self::stage(&mut guard, build)
        }?;
        self.deliver(staged).await
    }

    /// 锁内：分配序号、构造修订、推进 `seq`。
    ///
    /// 拆成独立一步是为了让**已经持有 entry 锁**的调用方(受理、收尾)在同一个
    /// 临界区里完成状态变更与序号推进,事后才投递。若把它们分成两次加锁,
    /// 「状态已变但 seq 没推进」的窗口里另一条路径会读到一个自相矛盾的摘要。
    fn stage(
        guard: &mut SideEntry,
        build: impl FnOnce(&SideEntry) -> super::SidePublishedUpdate,
    ) -> Option<StagedRevision> {
        let seq = super::next_seq(guard.seq.load(Ordering::SeqCst)).ok()?;
        // 先推进序号再构造修订:`Changed` 的摘要里带 `last_seq`,先构造就会把
        // 「上一版」的序号发出去,客户端据此判断水位时永远是旧值。
        guard.seq.store(seq, Ordering::SeqCst);
        let update = build(guard);
        Some(StagedRevision {
            parent: guard.parent.clone(),
            generation: guard.generation,
            seq,
            update,
        })
    }

    /// 锁外:投递一帧已定稿的修订。
    ///
    /// 没有 hub(纯 runtime 单测)时是空操作,序号照样推进。投递走带背压的路径:
    /// `side/closed` 与 `turnCompleted` 是终态,丢掉它们客户端会永久停在
    /// running,只能全量对账才恢复。
    async fn deliver(&self, staged: StagedRevision) -> Option<u64> {
        deliver_with(self.hub(), staged).await
    }

    /// 当前广播出口(可能没有:hub 由 `serve()` 装上,纯 runtime 单测没有它)。
    fn hub(&self) -> Option<Arc<crate::broadcast::Broadcaster>> {
        self.hub.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// This host process's identity.
    ///
    /// Every transport in one process must answer with the same id, so clients can
    /// tell "same host, reconnected" from "host restarted" without guessing. The id
    /// lives on the resource layer because that is what a restart actually replaces.
    pub fn host_instance_id(&self) -> &str {
        self.resources.host_instance_id()
    }

    /// Issue one entry ticket for `parent`.
    ///
    /// Touches no parent state: no resolver call, no directory, no provider. The
    /// whole point of splitting "may I open" from "open" is that the first question
    /// is cheap, bounded and revocable, and the second is only asked with a
    /// capability in hand.
    pub async fn prepare_entry(&self, parent: &str) -> Result<EntryTarget, SideError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SideError::new(SideErrorReason::SideClosing, "宿主正在关闭"));
        }
        let mut state = self.state.lock().await;
        state.tickets.prepare(parent, tokio::time::Instant::now())
    }

    /// Create or return the side a ticket authorises.
    ///
    /// The ticket is the only way in: a caller without one cannot create an
    /// environment, so "who may open" and "which parent" are one decision rather
    /// than two that can disagree.
    pub async fn open_entry(
        self: &Arc<Self>,
        target: &EntryTarget,
    ) -> Result<SideSummary, SideError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SideError::new(SideErrorReason::SideClosing, "宿主正在关闭"));
        }
        let epoch = self.sweep_epoch.load(Ordering::SeqCst);
        let parent = target.parent().to_string();
        let bound = {
            let mut state = self.state.lock().await;
            // A ticket that is prepared right now may still be bound; one that is
            // closed (or unknown/expired) is refused here, before any work.
            let existing = state.tickets.resolve(target, tokio::time::Instant::now())?;
            match existing {
                Some(side) => state.sides.get(&side.side_id).cloned().ok_or_else(|| {
                    SideError::new(SideErrorReason::SideClosing, "该票据对应的旁支已经退出")
                })?,
                None => match state.parents.get(&parent).map(|slot| match slot {
                    ParentSlot::Bound(owner) => Some(Arc::clone(owner)),
                    ParentSlot::Changing => None,
                }) {
                    Some(Some(owner)) => {
                        // Bind to the generation that already exists. This is the
                        // "one parent, one environment" rule stated as an
                        // operation: a second caller joins, never creates.
                        state.tickets.bind(
                            target,
                            owner.side().clone(),
                            tokio::time::Instant::now(),
                        )?;
                        owner
                    }
                    Some(None) => {
                        return Err(SideError::new(
                            SideErrorReason::SideClosing,
                            "父会话正在变更，暂不能进入旁问",
                        ));
                    }
                    None => {
                        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
                        let side_id = uuid::Uuid::new_v4().to_string();
                        let owner =
                            GenerationOwner::new(side_id.clone(), generation, parent.clone());
                        // Bind **before** spawning any construction: a ticket that
                        // was revoked after this point must never create.
                        state.tickets.bind(
                            target,
                            owner.side().clone(),
                            tokio::time::Instant::now(),
                        )?;
                        state
                            .parents
                            .insert(parent.clone(), ParentSlot::Bound(Arc::clone(&owner)));
                        state.sides.insert(side_id, Arc::clone(&owner));
                        Arc::clone(&owner)
                    }
                },
            }
        };

        self.ensure_constructing(Arc::clone(&bound), parent, epoch)
            .await;
        self.wait_open(&bound).await
    }

    /// Spawn the construction task once per generation.
    async fn ensure_constructing(
        self: &Arc<Self>,
        owner: Arc<GenerationOwner>,
        parent: String,
        epoch: u64,
    ) {
        if owner.phase() != Phase::Creating {
            return;
        }
        if !owner.claim_construction() {
            return;
        }
        let registry = Arc::clone(self);
        let task_owner = Arc::clone(&owner);
        // The construction handle is registered *inside* the task start, so two
        // concurrent callers cannot both spawn: `construction_pending` flips
        // before the first await.
        let handle = tokio::spawn(async move {
            registry.construct(task_owner, parent, epoch).await;
        });
        owner.set_construction(handle).await;
    }

    /// The body of one generation's creation.
    async fn construct(self: Arc<Self>, owner: Arc<GenerationOwner>, parent: String, epoch: u64) {
        let cancel = owner.cancel();
        let outcome = self.build_entry(&owner, &parent, epoch, cancel).await;
        match outcome {
            Ok(summary) => {
                // The generation is usable only if nobody asked to close while it
                // was being built. `try_open` is the compare-and-set for that.
                if !owner.try_open() {
                    // Exit arrived during construction: drop what was built and let
                    // the drain that is already running reclaim it.
                    let _ = summary;
                    self.finish_failed_construction(&owner, None).await;
                }
            }
            Err(error) => {
                tracing::debug!(reason = error.reason_str(), "side construction failed");
                owner.set_error(error);
                let error = owner.error();
                self.finish_failed_construction(&owner, error).await;
            }
        }
    }

    /// Build the environment of one generation.
    async fn build_entry(
        self: &Arc<Self>,
        owner: &Arc<GenerationOwner>,
        parent: &str,
        epoch: u64,
        cancel: CancellationToken,
    ) -> Result<SideSummary, SideError> {
        let source = {
            // Cancellation must be observable while the parent is being read: a
            // user who leaves during `Creating` should not wait for a cold read of
            // a large history.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    return Err(SideError::new(SideErrorReason::Cancelled, "旁支在准备期间被取消"));
                }
                resolved = self.resolver.resolve(parent) => resolved?,
            }
        };
        if cancel.is_cancelled() {
            return Err(SideError::new(
                SideErrorReason::Cancelled,
                "旁支在准备期间被取消",
            ));
        }
        let side_id = owner.side().side_id.clone();
        let generation = owner.side().generation;

        // The slot is reserved before any file work so a burst of opens cannot
        // exceed the host's allowance while the copies are still in flight.
        let lease = self.resources.reserve_slot(&side_id)?;
        let prepared = self
            .resources
            .prepare(Arc::clone(&lease), &source, cancel.clone())
            .await?;

        // A side has no tools; the factory only carries its identity.
        let tools = SideToolSetupFactory::new(side_id.clone());
        // Captured before the source is moved into the runner: afterwards the fixed
        // values are only reachable through the runner, and the point of the snapshot
        // is that it never changes again.
        let config = SideEffectiveConfig {
            model: source.config.model.clone(),
            reasoning_effort: source.config.gen_params.reasoning_effort.clone(),
            preserve_thinking: source.config.preserve_thinking,
            tool_names: yi_agent_runtime::side_conversation::SideToolPolicy
                .allowed_names()
                .iter()
                .map(|name| (*name).to_string())
                .collect(),
        };
        let runner = SideConversationRunner::new(side_id.clone(), source, prepared, tools)?;
        let now = tokio::time::Instant::now();
        let entry = Arc::new(Mutex::new(SideEntry {
            side_id: side_id.clone(),
            parent: parent.to_string(),
            generation,
            runner,
            ledger: SideRequestLedger::new(),
            deadline: SideIdleDeadline::new(now),
            transcript: SideTranscript::side_defaults(),
            seq: AtomicU64::new(0),
            stopping: std::sync::atomic::AtomicBool::new(false),
            closing: false,
            config,
        }));

        {
            // Install only while the generation is still `Creating`: an exit that
            // arrived during construction must not leave a built environment behind
            // (its lease would never be released).
            if self.sweep_epoch.load(Ordering::SeqCst) != epoch {
                return Err(SideError::new(
                    SideErrorReason::SideClosing,
                    "清扫期间创建的旁支已丢弃",
                ));
            }
            if !owner.install_entry_if_creating(Arc::clone(&entry)).await {
                return Err(SideError::new(
                    SideErrorReason::SideClosing,
                    "退出期间创建的旁支已丢弃",
                ));
            }
        }
        let guard = entry.lock().await;
        Ok(guard.summary())
    }

    /// Remove a generation whose creation failed, and prove its slot is released.
    ///
    /// A failed creation still holds a resource slot: the directory may exist and
    /// its receipt must be confirmed before the slot is considered free. The
    /// generation is dropped from the maps so the parent is openable again, and the
    /// ticket is finished so the caller gets an honest "that entry is gone".
    async fn finish_failed_construction(
        self: &Arc<Self>,
        owner: &Arc<GenerationOwner>,
        error: Option<SideError>,
    ) {
        let side = owner.side().clone();
        let parent = owner.parent().to_string();
        let side_id = side.side_id.clone();
        let entry = owner.take_entry().await;
        if let Some(entry) = entry {
            let mut guard = entry.lock().await;
            guard.closing = true;
            let _ = guard.runner.close().await;
        }
        // Dropping the entry drops the last lease; confirm the receipt before the
        // slot is treated as free.
        if let Ok(mut receipt) = self.resources.cleanup_receipt(&side_id) {
            let _ = receipt.wait(super::lifecycle::SIDE_DRAIN_GRACE).await;
        }
        self.release_side_snapshots(&side_id).await;
        {
            let mut state = self.state.lock().await;
            state.sides.remove(&side_id);
            if matches!(
                state.parents.get(&parent),
                Some(ParentSlot::Bound(owner)) if owner.side() == &side
            ) {
                state.parents.remove(&parent);
            }
            state.tickets.finish(&side, tokio::time::Instant::now());
        }
        if let Some(error) = error {
            owner.set_error(error);
        }
        owner.set_phase(Phase::Closed);
    }

    /// Wait until `owner` is usable, or explain why it is not.
    async fn wait_open(&self, owner: &Arc<GenerationOwner>) -> Result<SideSummary, SideError> {
        let mut rx = owner.watch();
        loop {
            // Read the phase into a value before awaiting: `watch::Ref` holds a
            // `std` RwLock guard, which is not `Send` and would make the whole
            // `open` future non-`Send` (so it could not be spawned off the loop).
            let phase = owner.phase();
            match phase {
                Phase::Open => {
                    let Some(entry) = owner.entry().await else {
                        return Err(SideError::new(
                            SideErrorReason::SideClosing,
                            "旁支环境尚未安装",
                        ));
                    };
                    let guard = entry.lock().await;
                    return Ok(guard.summary());
                }
                Phase::Creating => {}
                Phase::Closing | Phase::Closed => {
                    // A generation that never became usable reports *why*: an
                    // unknown parent is a different answer from "somebody else is
                    // exiting", and collapsing them would tell the user to retry
                    // something that can never work.
                    if let Some(error) = owner.error() {
                        return Err(error);
                    }
                    return Err(SideError::new(
                        SideErrorReason::SideClosing,
                        "上一条旁问正在退出，请稍后重试",
                    ));
                }
                Phase::Failed => {
                    return Err(owner.error().unwrap_or_else(|| {
                        SideError::new(SideErrorReason::SideClosing, "旁支上次退出未完成")
                    }));
                }
            }
            if rx.changed().await.is_err() {
                return Err(SideError::new(
                    SideErrorReason::SideClosing,
                    "旁支在等待期间消失",
                ));
            }
        }
    }

    /// Exit the generation `target` names.
    ///
    /// The first call marks the exact generation `Closing` and starts one drain
    /// task; it returns before the drain finishes, because the caller must not wait
    /// on a provider stop. A later call polls the *same* generation: `Closing` while
    /// the drain runs, `Closed` once its resources are confirmed released, `Failed`
    /// when cleanup could not prove it (a retry resumes the same generation).
    pub async fn exit_entry(
        self: &Arc<Self>,
        target: &EntryTarget,
    ) -> Result<ExitReceipt, SideError> {
        let (receipt, owner) = {
            let mut state = self.state.lock().await;
            let receipt = state.tickets.revoke(target, tokio::time::Instant::now())?;
            let owner = receipt
                .generation
                .as_ref()
                .and_then(|side| state.sides.get(&side.side_id).cloned());
            (receipt, owner)
        };
        let Some(owner) = owner else {
            // Unused ticket, or a generation that is already gone: both are a
            // no-op, and neither may create or kill anything else.
            return Ok(ExitReceipt {
                state: ExitState::Closed,
                generation: receipt.generation,
            });
        };
        owner.begin_closing();
        match owner.phase() {
            Phase::Closed => Ok(ExitReceipt {
                state: ExitState::Closed,
                generation: receipt.generation,
            }),
            Phase::Failed => Ok(ExitReceipt {
                state: ExitState::Closing,
                generation: receipt.generation,
            }),
            _ => {
                if owner.begin_drain() {
                    let registry = Arc::clone(self);
                    let draining = Arc::clone(&owner);
                    tokio::spawn(async move {
                        let _ = registry.drain(draining, CloseReason::UserClosed).await;
                    });
                }
                Ok(ExitReceipt {
                    state: ExitState::Closing,
                    generation: receipt.generation,
                })
            }
        }
    }

    /// The one cleanup path for a generation.
    ///
    /// Everything expensive happens here, outside the state lock and outside the
    /// request path: cancel construction, close the runner (which joins the real
    /// core task), join forwarders, revoke snapshots, confirm the resource receipt,
    /// and only then remove the generation and answer `Closed`.
    async fn drain(
        self: &Arc<Self>,
        owner: Arc<GenerationOwner>,
        reason: CloseReason,
    ) -> Result<(), SideError> {
        let side = owner.side().clone();
        let side_id = side.side_id.clone();
        let parent = owner.parent().to_string();
        owner.set_phase(Phase::Closing);
        // Stop new work first: any turn accepted after this point must see Closing.
        owner.cancel().cancel();

        let grace = super::lifecycle::SIDE_DRAIN_GRACE;
        let failure = async {
            // 1. A construction still running must observe the cancellation rather
            //    than be aborted: aborting drops the read mid-flight and races the
            //    resource cleanup it already queued.
            if let Some(handle) = owner.take_construction().await {
                match tokio::time::timeout(super::lifecycle::SIDE_CONSTRUCTION_GRACE, handle).await
                {
                    Ok(Ok(())) | Ok(Err(_)) => {}
                    Err(_) => {
                        return Err(SideError::new(
                            SideErrorReason::SideClosing,
                            "creation_did_not_observe_cancellation",
                        ));
                    }
                }
            }
            // 2. Close the runner. `close` waits for the pump's join evidence, so a
            //    `Closed` here really means the core task ended.
            if let Some(entry) = owner.entry().await {
                let mut guard = entry.lock().await;
                guard.closing = true;
                match tokio::time::timeout(grace, guard.runner.close()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(SideError::new(
                            SideErrorReason::SideClosing,
                            "runner_close_timeout",
                        ));
                    }
                }
            }
            Ok::<(), SideError>(())
        }
        .await;

        if let Err(error) = failure {
            // Keep the generation: its owner is the only thing that can retry the
            // *same* drain, and dropping it would abandon a live core task.
            tracing::warn!(reason = error.reason_str(), "side drain failed");
            owner.set_error(error.clone());
            owner.set_phase(Phase::Failed);
            owner.end_drain();
            return Err(error);
        }

        // 3. Forwarders hold the entry (and the transcript) alive; join them before
        //    dropping it so no late item can be published for a closed generation.
        for handle in owner.take_publishers().await {
            let _ = tokio::time::timeout(grace, handle).await;
        }

        // 4. Terminal frame while the entry still exists. The budget is deliberately
        //    short: resources are already being reclaimed, and a client that misses
        //    this reconciles through `side/exit` or `side/list`.
        if let Some(entry) = owner.entry().await {
            let closed = {
                let mut guard = entry.lock().await;
                Self::stage(&mut guard, |entry| super::SidePublishedUpdate::Closed {
                    side_id: entry.side_id.clone(),
                    reason,
                })
            };
            if let Some(staged) = closed {
                let _ = tokio::time::timeout(
                    super::lifecycle::SIDE_CLOSED_NOTIFY_BUDGET,
                    self.deliver(staged),
                )
                .await;
            }
        }

        // 5. Capture the dedup table (request id -> turn id, no text) while the
        //    entry still exists: a retried request must be answered against the
        //    generation that accepted it, not against a fresh one.
        let accepted: Vec<(String, String)> = match owner.entry().await {
            Some(entry) => {
                let guard = entry.lock().await;
                guard.ledger.accepted_requests().into_iter().collect()
            }
            None => Vec::new(),
        };

        // Revoke paging snapshots: they hold the body, and the tombstone must
        // never become a way back into deleted content.
        self.release_side_snapshots(&side_id).await;

        // 6. Take the entry out (and drop it) so the last lease is released, then
        //    confirm the delete before claiming the slot back.
        let entry = owner.take_entry().await;
        drop(entry);
        if let Ok(mut receipt) = self.resources.cleanup_receipt(&side_id) {
            match receipt.wait(grace).await {
                Ok(()) => {}
                Err(error) => {
                    tracing::warn!(reason = error.reason_str(), "side cleanup unconfirmed");
                    owner.set_error(error.clone());
                    owner.set_phase(Phase::Failed);
                    owner.end_drain();
                    return Err(error);
                }
            }
        }

        // 7. Only now: forget the generation, release the parent, finish its exact
        //    tickets, and remember a tombstone that answers retries honestly.
        {
            let mut state = self.state.lock().await;
            state.sides.remove(&side_id);
            if matches!(
                state.parents.get(&parent),
                Some(ParentSlot::Bound(owner)) if owner.side() == &side
            ) {
                state.parents.remove(&parent);
            }
            state.tickets.finish(&side, tokio::time::Instant::now());
            state.tombstones.insert(
                side_id.clone(),
                Tombstone {
                    side_id: side_id.clone(),
                    parent: parent.clone(),
                    generation: side.generation,
                    accepted,
                    created_at: tokio::time::Instant::now(),
                },
            );
            Self::prune_tombstones(&mut state.tombstones, tokio::time::Instant::now());
        }
        owner.set_phase(Phase::Closed);
        owner.end_drain();
        Ok(())
    }

    /// Every live side for one parent (zero or one, but the wire shape is a list).
    pub async fn list(&self, parent: &str) -> Vec<SideSummary> {
        let owner = {
            let state = self.state.lock().await;
            match state.parents.get(parent) {
                Some(ParentSlot::Bound(owner)) => Some(Arc::clone(owner)),
                _ => None,
            }
        };
        let Some(owner) = owner else {
            return Vec::new();
        };
        match owner.entry().await {
            Some(entry) => {
                let guard = entry.lock().await;
                vec![guard.summary()]
            }
            // A generation that is still being built is reported as `Creating`:
            // hiding it would make a second client create a second environment.
            None => vec![SideRegistry::creating_summary(&owner)],
        }
    }

    fn creating_summary(owner: &GenerationOwner) -> SideSummary {
        SideSummary {
            side_id: owner.side().side_id.clone(),
            parent_thread_id: owner.parent().to_string(),
            status: SideStatus::Creating,
            snapshot_revision: owner.side().generation,
            last_seq: 0,
            active_turn_id: None,
            expires_at: None,
            generation: owner.side().generation,
            stopping: false,
        }
    }

    async fn owner(&self, side_id: &str) -> Option<Arc<GenerationOwner>> {
        let state = self.state.lock().await;
        state.sides.get(side_id).cloned()
    }

    async fn entry(&self, side_id: &str) -> Result<Arc<Mutex<SideEntry>>, SideError> {
        let Some(owner) = self.owner(side_id).await else {
            return Err(SideError::new(
                SideErrorReason::SideNotFound,
                format!("找不到旁支 {side_id}"),
            ));
        };
        owner.entry().await.ok_or_else(|| {
            SideError::new(
                SideErrorReason::SideNotFound,
                format!("找不到旁支 {side_id}"),
            )
        })
    }

    /// Refuse a generation that is on its way out.
    ///
    /// Reads and submits share this check: after `Closing` there is no body to read
    /// and no turn to accept, and an accepted one would publish into a transcript
    /// that is about to be dropped.
    async fn live_entry(&self, side_id: &str) -> Result<Arc<Mutex<SideEntry>>, SideError> {
        let Some(owner) = self.owner(side_id).await else {
            return Err(SideError::new(
                SideErrorReason::SideNotFound,
                format!("找不到旁支 {side_id}"),
            ));
        };
        if owner.is_closing() {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                format!("旁支 {side_id} 已关闭"),
            ));
        }
        owner.entry().await.ok_or_else(|| {
            SideError::new(
                SideErrorReason::SideNotFound,
                format!("找不到旁支 {side_id}"),
            )
        })
    }

    /// Accept one turn, or return the turn a retried request already became.
    pub async fn turn_start(
        &self,
        side_id: &str,
        text: String,
        request_id: String,
    ) -> Result<String, SideError> {
        let entry = self.live_entry(side_id).await?;
        let mut guard = entry.lock().await;
        if guard.closing || guard.runner.status() == SideStatus::Closing {
            return Err(SideError::new(
                SideErrorReason::SideNotFound,
                format!("旁支 {side_id} 已关闭"),
            ));
        }

        // Dedup first: a retry of an accepted request must return the original turn
        // even though that turn made the side busy.
        if let Some(turn_id) = guard.ledger.lookup(&request_id, &text)? {
            return Ok(turn_id);
        }
        if guard.runner.active_turn_id().is_some() {
            return Err(SideError::new(
                SideErrorReason::TurnInProgress,
                "旁支已有一轮在跑",
            ));
        }

        let turn_id = uuid::Uuid::new_v4().to_string();
        // Prepare acceptance and the desktop's shared transcript before starting
        // the runner. A budget/wire failure must neither spend a provider turn
        // nor leave an accepted request or a partially recorded question behind.
        // Transcript bodies are Arc-backed: cloning only copies the index/handles.
        let mut ledger = guard.ledger.clone();
        ledger.accept(request_id, &text, turn_id.clone())?;
        let mut transcript = guard.transcript.clone();
        let question_items = transcript.begin_turn(&turn_id, &text)?;
        let question_items = question_items
            .iter()
            .filter(|item| item.turn_id == turn_id)
            .map(|item| super::wire_item(item))
            .collect::<Result<Vec<_>, _>>()?;

        let stream = guard.runner.start_turn(turn_id.clone(), text).await?;
        guard.ledger = ledger;
        guard.transcript = transcript;
        guard.deadline.touch(tokio::time::Instant::now());
        // 受理与「Running 已发布」在同一临界区内完成:客户端只有在收到这条
        // changed 之后才敢认为自己发的这条消息被受理了。推迟到模型启动之后发,
        // 一个启动很慢的 provider 就会让客户端长时间看不到受理结果。
        let accepted = Self::stage(&mut guard, |entry| super::SidePublishedUpdate::Changed {
            side_id: entry.side_id.clone(),
            summary: entry.summary(),
        });
        // Publish the accepted question before draining any provider output.
        let mut question_updates = Vec::new();
        for item in question_items {
            if let Some(update) =
                Self::stage(&mut guard, |entry| super::SidePublishedUpdate::Item {
                    side_id: entry.side_id.clone(),
                    turn_id: turn_id.clone(),
                    item,
                })
            {
                question_updates.push(update);
            }
        }
        drop(guard);
        if let Some(staged) = accepted {
            self.deliver(staged).await;
        }
        for update in question_updates {
            self.deliver(update).await;
        }

        // Drain the stream off the request path. The pump already joined the core
        // task before it publishes a terminal event, so this task only translates.
        let task = TurnPublisher {
            // 把 hub 搬进任务:spawn 之后 `&self` 已经不在作用域里,而投递只需要
            // 出站句柄与 entry。
            hub: self.hub(),
            entry,
            turn_id: turn_id.clone(),
            dirty: std::collections::BTreeSet::new(),
        };
        let handle = tokio::spawn(task.run(stream));
        // The forwarder holds the entry (and its transcript) alive. A drain must be
        // able to join it, otherwise a closed generation could still publish an item
        // after its terminal frame.
        if let Some(owner) = self.owner(side_id).await {
            owner.push_publisher(handle).await;
        }
        Ok(turn_id)
    }

    /// Ask the named turn to stop. A stale turn id is a no-op, never a way to kill
    /// a newer turn.
    pub async fn interrupt(&self, side_id: &str, turn_id: &str) -> Result<(), SideError> {
        let entry = self.entry(side_id).await?;
        let mut guard = entry.lock().await;
        // The runner itself matches the turn id, but setting `stopping` here means a
        // caller sees the accepted stop immediately, before the join completes.
        if guard.runner.active_turn_id() == Some(turn_id) {
            guard.stopping.store(true, Ordering::SeqCst);
        }
        guard.runner.interrupt(turn_id).await?;
        // 停止请求被受理也是一次**状态发布**:`stopping` 变了,另一端要立刻看到,
        // 否则它仍显示可发送、直到终态才更新。
        let staged = Self::stage(&mut guard, |entry| super::SidePublishedUpdate::Changed {
            side_id: entry.side_id.clone(),
            summary: entry.summary(),
        });
        drop(guard);
        if let Some(staged) = staged {
            self.deliver(staged).await;
        }
        Ok(())
    }

    /// Close the side, conditionally on the turn the caller believed was active.
    ///
    /// Retained for the legacy `side/close` method, which a pre-lifecycle client
    /// still calls. It runs the **same** drain as `exit_entry`: keeping a second,
    /// simpler teardown path is how one of them quietly stops releasing resources.
    pub async fn close(
        self: &Arc<Self>,
        side_id: &str,
        expected: Option<&str>,
    ) -> Result<(), SideError> {
        let Some(owner) = self.owner(side_id).await else {
            // A repeated close against a tombstone is idempotent (spec §8.1).
            let state = self.state.lock().await;
            return if state.tombstones.contains_key(side_id) {
                Ok(())
            } else {
                Err(SideError::new(
                    SideErrorReason::SideNotFound,
                    format!("找不到旁支 {side_id}"),
                ))
            };
        };
        let Some(entry) = owner.entry().await else {
            // A generation still being built has no runner to confirm a turn
            // against; a close here is an exit, and the drain cancels construction.
            owner.begin_closing();
            if owner.begin_drain() {
                let registry = Arc::clone(self);
                let draining = Arc::clone(&owner);
                let _ = registry.drain(draining, CloseReason::UserClosed).await;
            }
            return Ok(());
        };
        {
            let guard = entry.lock().await;
            let actual = guard.runner.active_turn_id().map(str::to_string);
            // CAS and the transition into Closing happen in one critical section, so
            // a turn accepted concurrently cannot slip in between them.
            if actual.as_deref() != expected {
                let summary = guard.summary();
                return Err(SideError::new(
                    SideErrorReason::CloseConflict,
                    format!(
                        "旁支状态已变化：当前活跃轮为 {:?}，状态 {:?}",
                        summary.active_turn_id, summary.status
                    ),
                ));
            }
        }
        // The local handle must not outlive this check: drain drops the *last*
        // reference to release the resource lease, and holding one here would keep
        // the slot alive forever (cleanup would time out).
        drop(entry);
        owner.begin_closing();
        if owner.begin_drain() {
            self.drain(Arc::clone(&owner), CloseReason::UserClosed)
                .await?;
        } else {
            // Another drain owns this generation: wait for its verdict instead of
            // starting a second close over the same runner.
            let mut rx = owner.watch();
            loop {
                let phase = *rx.borrow_and_update();
                if !matches!(phase, Phase::Closing) {
                    break;
                }
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// 撤销某个 side 的全部分页快照。
    ///
    /// 关闭之后不得再从 tombstone 恢复正文:快照里只有标识与 item,没有「已关闭」
    /// 这个事实,留着它等于给客户端一条通往已删内容的旁路。
    pub async fn release_side_snapshots(&self, side_id: &str) {
        let Some(replay) = self.replay.lock().await.clone() else {
            return;
        };
        replay.lock().await.release_side(side_id);
    }

    fn prune_tombstones(tombstones: &mut HashMap<String, Tombstone>, now: tokio::time::Instant) {
        tombstones.retain(|_, stone| now.duration_since(stone.created_at) < SIDE_TOMBSTONE_TTL);
        while tombstones.len() > SIDE_TOMBSTONES {
            let oldest = tombstones
                .iter()
                .min_by_key(|(_, stone)| stone.created_at)
                .map(|(id, _)| id.clone());
            match oldest {
                Some(id) => {
                    tombstones.remove(&id);
                }
                None => break,
            }
        }
    }

    /// What a tombstone remembers about a closed side.
    ///
    /// Used to answer "which parent/generation was this?" after the side itself is
    /// gone, which is what a reconnect needs to decide between "closed" and
    /// "never existed".
    pub async fn tombstone(&self, side_id: &str) -> Option<ClosedSide> {
        let state = self.state.lock().await;
        state.tombstones.get(side_id).map(|stone| ClosedSide {
            side_id: stone.side_id.clone(),
            parent_thread_id: stone.parent.clone(),
            generation: stone.generation,
            accepted: stone.accepted.clone(),
        })
    }

    /// The dedup table of a live or remembered side, for reconnect reconciliation.
    pub async fn accepted_requests(&self, side_id: &str) -> Option<Vec<(String, String)>> {
        if let Ok(entry) = self.entry(side_id).await {
            let guard = entry.lock().await;
            return Some(guard.ledger.accepted_requests().into_iter().collect());
        }
        let state = self.state.lock().await;
        state
            .tombstones
            .get(side_id)
            .map(|stone| stone.accepted.clone())
    }

    /// A consistent snapshot of one side: items plus the metadata a client needs to
    /// reconcile.
    pub async fn snapshot(&self, side_id: &str) -> Result<SideSnapshot, SideError> {
        // Reads go through the same barrier as submits: after `Closing` there is
        // nothing to page through, and serving the body would outlive the decision
        // to destroy it.
        let entry = self.live_entry(side_id).await?;
        let guard = entry.lock().await;
        Ok(SideSnapshot {
            summary: guard.summary(),
            items: guard.transcript.items(),
            usage: guard.runner.usage(),
            accepted: guard.ledger.accepted_requests().into_iter().collect(),
            config: guard.config.clone(),
        })
    }

    /// Start a barrier around a destructive parent operation.
    pub async fn begin_parent_change(self: &Arc<Self>, parent: &str) -> ParentChangeGuard<'_> {
        let removed = {
            let mut state = self.state.lock().await;
            state
                .parents
                .insert(parent.to_string(), ParentSlot::Changing)
        };
        let owner = match removed {
            Some(ParentSlot::Bound(owner)) => Some(owner),
            _ => None,
        };
        ParentChangeGuard {
            registry: self,
            parent: parent.to_string(),
            slot: replaced_slot(&owner),
            owner,
            committed: false,
        }
    }

    async fn apply_parent_change(
        self: &Arc<Self>,
        parent: &str,
        owner: Option<Arc<GenerationOwner>>,
        reason: CloseReason,
    ) {
        let Some(owner) = owner else {
            return;
        };
        // The parent is already going away, so a failure to converge must not keep
        // the destructive operation waiting. The generation stays tracked on a
        // failure, so a retry drains the same one instead of abandoning it.
        owner.begin_closing();
        if owner.begin_drain() {
            let _ = self.drain(Arc::clone(&owner), reason).await;
        }
        // Release the barrier this change installed: the old generation is gone, and
        // the parent must be openable again. `Changing` is only ever ours here.
        let mut state = self.state.lock().await;
        if matches!(state.parents.get(parent), Some(ParentSlot::Changing)) {
            state.parents.remove(parent);
        }
    }

    /// Stop every side and wait (bounded) for their resources.
    ///
    /// Never called from the request loop: the plan is explicit that shutdown must
    /// not block RPC handling on runner teardown.
    /// 关闭所有旁支，但不把 registry 置为终态。
    ///
    /// 与 [`Self::shutdown`] 的语义差：
    ///
    /// - **不碰 `closed`**：开关是可逆的，关掉之后用户还能再打开。
    /// - **写 tombstone**：与 `close()` 一致，让「刚关掉的那条」还能回答
    ///   `side_not_found` 而不是「从未存在」（重连时需要这个区别）。
    /// - **发布 `side/closed`**：客户端靠这个终态帧丢弃本地内容与在途分页。
    ///
    /// 并发的两个要点：
    ///
    /// - 自增 `sweep_epoch`：构造在安装 entry 之前复核，不一致就放弃。
    /// - 有界多轮：第一轮期间可能有构造落定成 `Open`。
    pub async fn close_all(self: &Arc<Self>) {
        self.sweep_epoch.fetch_add(1, Ordering::SeqCst);
        // 三轮足够覆盖「第一轮期间创建落定」这一种形状。超过上限记警告而不是
        // 静默返回——留下一条活旁支是本功能最不该有的失败模式。
        const MAX_ROUNDS: usize = 3;
        for round in 0..MAX_ROUNDS {
            let owners: Vec<Arc<GenerationOwner>> = {
                let state = self.state.lock().await;
                state.sides.values().map(Arc::clone).collect()
            };
            if owners.is_empty() {
                self.drop_ready_parents().await;
                return;
            }
            for owner in owners {
                owner.begin_closing();
                if owner.begin_drain() {
                    let registry = Arc::clone(self);
                    let draining = Arc::clone(&owner);
                    let _ = tokio::spawn(async move {
                        let _ = registry.drain(draining, CloseReason::UserClosed).await;
                    })
                    .await;
                }
            }
            self.drop_ready_parents().await;
            if self.side_count().await == 0 {
                return;
            }
            if round + 1 == MAX_ROUNDS {
                // `await` 不能写在 `tracing` 的字段表达式里：`format_args!` 生成的
                // 临时值会跨 await 存活，把整个 `close_all` future 变成非 `Send`，
                // 于是它无法从 `tokio::spawn` 里调用（`flags/set` 就是这样用的）。
                let remaining = self.side_count().await;
                tracing::warn!(
                    remaining,
                    "close_all hit its round limit; some sides are still live"
                );
            }
        }
    }

    /// 移除所有空的 / 已完成的父占位，保留仍在排空的代次。
    async fn drop_ready_parents(&self) {
        let mut state = self.state.lock().await;
        let live: Vec<String> = state.sides.keys().cloned().collect();
        state.parents.retain(|_, slot| match slot {
            ParentSlot::Changing => true,
            ParentSlot::Bound(owner) => live.contains(&owner.side().side_id),
        });
    }

    pub async fn shutdown(self: &Arc<Self>) {
        self.closed.store(true, Ordering::SeqCst);
        let owners: Vec<Arc<GenerationOwner>> = {
            let state = self.state.lock().await;
            state.sides.values().map(Arc::clone).collect()
        };
        for owner in owners {
            owner.begin_closing();
            if owner.begin_drain() {
                let _ = self
                    .drain(Arc::clone(&owner), CloseReason::HostShutdown)
                    .await;
            }
        }
        let mut state = self.state.lock().await;
        state.sides.clear();
        state.parents.clear();
    }

    pub async fn side_count(&self) -> usize {
        self.state.lock().await.sides.len()
    }

    pub async fn tombstone_count(&self) -> usize {
        self.state.lock().await.tombstones.len()
    }
}

/// A closed side, as remembered in the tombstone table.
#[derive(Debug, Clone)]
pub struct ClosedSide {
    pub side_id: String,
    pub parent_thread_id: String,
    pub generation: u64,
    pub accepted: Vec<(String, String)>,
}

/// A consistent read of one side.
pub struct SideSnapshot {
    pub summary: SideSummary,
    pub items: Vec<Arc<yi_agent_runtime::side_conversation::SideItem>>,
    pub usage: SideUsage,
    pub accepted: Vec<(String, String)>,
    pub config: SideEffectiveConfig,
}

/// The side's configuration as fixed **at creation**, in redacted form.
///
/// Deliberately not read from the parent when a client asks: a side inherits its
/// parent once, at open, and does not follow later edits. Re-reading `config/read`
/// would answer a different question than "what is this side running with".
///
/// Holds no secret: no API key, no headers, no system prompt.
/// The slot a parent held before a change barrier replaced it.
fn replaced_slot(owner: &Option<Arc<GenerationOwner>>) -> Option<ParentSlot> {
    owner
        .as_ref()
        .map(|owner| ParentSlot::Bound(Arc::clone(owner)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideEffectiveConfig {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub preserve_thinking: bool,
    pub tool_names: Vec<String>,
}

/// A barrier around a destructive parent operation.
///
/// Created before the destructive work, committed after it succeeded. Dropping it
/// uncommitted restores the parent's slot, so a failed clear does not leave the
/// parent permanently unable to open a side.
pub struct ParentChangeGuard<'a> {
    registry: &'a Arc<SideRegistry>,
    parent: String,
    slot: Option<ParentSlot>,
    owner: Option<Arc<GenerationOwner>>,
    committed: bool,
}

impl ParentChangeGuard<'_> {
    /// Finish the change: the old generation is invalidated and its side is gone.
    pub async fn commit(mut self, reason: CloseReason) {
        self.committed = true;
        let owner = self.owner.take();
        self.registry
            .apply_parent_change(&self.parent, owner, reason)
            .await;
    }
}

impl Drop for ParentChangeGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Roll back synchronously: `Drop` cannot await, and the slot map is small
        // enough for `try_lock` to be the pragmatic choice. If it is contended the
        // eager path already restored nothing, so simply not restoring is safe —
        // the next open would create a fresh side over the untouched parent files.
        if let Some(slot) = self.slot.take()
            && let Ok(mut state) = self.registry.state.try_lock()
        {
            state.parents.insert(self.parent.clone(), slot);
        }
    }
}

fn epoch_millis(instant: tokio::time::Instant) -> i64 {
    // The deadline lives on the tokio clock so tests can advance it; converting to
    // wall-clock milliseconds for the wire is done relative to now, which keeps the
    // two clocks consistent under `start_paused`.
    let now = tokio::time::Instant::now();
    let remaining = instant.saturating_duration_since(now);
    let wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (wall + remaining).as_millis() as i64
}

#[cfg(test)]
mod tests;
