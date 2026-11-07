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
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use yi_agent_runtime::side_conversation::{
    CloseReason, SideConversationRunner, SideHostResources, SideIdleDeadline, SideRequestLedger,
    SideRunnerEvent, SideStatus, SideToolSetupFactory, SideTranscript,
};

use yi_agent_runtime::side_conversation::{SideError, SideErrorReason, SideUsage};
use yi_agent_tools::SideBlockingPool;

use super::replay::SIDE_ITEM_FLUSH_INTERVAL;
use super::source::SideParentResolver;

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
enum ParentSlot {
    /// A creation is in flight; everyone else waits on the notify.
    Creating(Arc<Notify>),
    Ready(String),
}

/// The host's side registry.
pub struct SideRegistry {
    resolver: Arc<dyn SideParentResolver>,
    resources: Arc<SideHostResources>,
    pool: Arc<SideBlockingPool>,
    parents: Mutex<HashMap<String, ParentSlot>>,
    sides: Mutex<HashMap<String, Arc<Mutex<SideEntry>>>>,
    tombstones: Mutex<HashMap<String, Tombstone>>,
    /// The notify of each in-flight create, kept until the creator wakes waiters.
    ///
    /// Separate from `parents` because the placeholder is *replaced* by `Ready` on
    /// success while the notify still has waiters; mixing the two would lose them.
    creating_notify: Mutex<HashMap<String, Arc<Notify>>>,
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
}

impl SideRegistry {
    pub fn new(
        resolver: Arc<dyn SideParentResolver>,
        resources: Arc<SideHostResources>,
        pool: Arc<SideBlockingPool>,
    ) -> Self {
        Self {
            resolver,
            resources,
            pool,
            parents: Mutex::new(HashMap::new()),
            sides: Mutex::new(HashMap::new()),
            tombstones: Mutex::new(HashMap::new()),
            creating_notify: Mutex::new(HashMap::new()),
            next_generation: AtomicU64::new(1),
            replay: tokio::sync::Mutex::new(None),
            hub: std::sync::Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
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

    /// Create or return the parent's side.
    ///
    /// Concurrent calls for one parent collapse: the first becomes the creator and
    /// the rest wait on its notify, then re-read the table. That is what keeps two
    /// transports from starting two provider streams on one history.
    pub async fn open(&self, parent: &str) -> Result<SideSummary, SideError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SideError::new(SideErrorReason::SideClosing, "宿主正在关闭"));
        }
        loop {
            // The wait future is created and *enabled* while the map lock is held.
            // Creating it after dropping the lock leaves a window in which the
            // creator publishes `Ready` and notifies nobody; the waiter then sleeps
            // on a notification that already happened. `enable` registers now, so
            // that race cannot exist.
            let wait = {
                let mut parents = self.parents.lock().await;
                match parents.get(parent) {
                    Some(ParentSlot::Ready(side_id)) => Some(Err(side_id.clone())),
                    Some(ParentSlot::Creating(notify)) => {
                        let mut future = Box::pin(Arc::clone(notify).notified_owned());
                        future.as_mut().enable();
                        Some(Ok(future))
                    }
                    None => {
                        let notify = Arc::new(Notify::new());
                        self.creating_notify
                            .lock()
                            .await
                            .insert(parent.to_string(), Arc::clone(&notify));
                        parents.insert(
                            parent.to_string(),
                            ParentSlot::Creating(Arc::clone(&notify)),
                        );
                        None
                    }
                }
            };
            match wait {
                Some(Err(side_id)) => return self.summary_of(&side_id).await,
                // Someone else is creating: wait for them rather than racing.
                Some(Ok(future)) => future.await,
                None => return self.create(parent).await,
            }
        }
    }

    /// The body of `open` for the caller that won the creation race.
    async fn create(&self, parent: &str) -> Result<SideSummary, SideError> {
        let outcome = self.create_inner(parent).await;
        {
            let mut parents = self.parents.lock().await;
            match &outcome {
                Ok(summary) => {
                    parents.insert(
                        parent.to_string(),
                        ParentSlot::Ready(summary.side_id.clone()),
                    );
                }
                // A failed create must not leave a placeholder behind: that would
                // make the parent permanently un-openable.
                Err(_) => {
                    parents.remove(parent);
                }
            }
        }
        // Always wake the waiters, success or failure. On success they re-read the
        // map and find `Ready`; on failure they find no slot and become the next
        // creator.
        self.wake_creating(parent).await;
        outcome
    }

    /// Wake waiters registered on a parent's placeholder.
    ///
    /// The placeholder is replaced by `Ready` on success, so the only handle to its
    /// notify is the clone every waiter took. Keeping the notify in the entry would
    /// mean holding it for the side's whole life for one wake-up; instead the
    /// creator notifies through the same `Arc` it created. That is why the
    /// placeholder stores the `Arc` and the creator keeps its own clone.
    async fn wake_creating(&self, parent: &str) {
        let notify = self.creating_notify.lock().await.remove(parent);
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
    }

    async fn create_inner(&self, parent: &str) -> Result<SideSummary, SideError> {
        let source = self.resolver.resolve(parent).await?;
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst);
        let side_id = uuid::Uuid::new_v4().to_string();

        // The slot is reserved before any file work so a burst of opens cannot
        // exceed the host's allowance while the copies are still in flight.
        let lease = self.resources.reserve_slot(&side_id)?;
        let cancel = CancellationToken::new();
        let prepared = self
            .resources
            .prepare(Arc::clone(&lease), &source, cancel.clone())
            .await?;

        let tools = SideToolSetupFactory::new(
            source.clone(),
            Arc::clone(&prepared.access),
            Arc::clone(&self.pool),
            side_id.clone(),
            prepared.lease.clone() as Arc<dyn Send + Sync>,
        );
        // Captured before the source is moved into the runner: afterwards the fixed
        // values are only reachable through the runner, and the point of the snapshot
        // is that it never changes again.
        let config = SideEffectiveConfig {
            model: source.config.model.clone(),
            reasoning_effort: source.config.gen_params.reasoning_effort.clone(),
            preserve_thinking: source.config.preserve_thinking,
            tool_names: yi_agent_runtime::side_conversation::SideToolPolicy::default()
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
            let mut sides = self.sides.lock().await;
            sides.insert(side_id.clone(), Arc::clone(&entry));
        }
        let guard = entry.lock().await;
        Ok(guard.summary())
    }

    /// Every live side for one parent (zero or one, but the wire shape is a list).
    pub async fn list(&self, parent: &str) -> Vec<SideSummary> {
        let side_ids: Vec<String> = {
            let parents = self.parents.lock().await;
            match parents.get(parent) {
                Some(ParentSlot::Ready(side_id)) => vec![side_id.clone()],
                _ => Vec::new(),
            }
        };
        let mut out = Vec::new();
        for side_id in side_ids {
            if let Ok(summary) = self.summary_of(&side_id).await {
                out.push(summary);
            }
        }
        out
    }

    async fn summary_of(&self, side_id: &str) -> Result<SideSummary, SideError> {
        let entry = self.entry(side_id).await?;
        let guard = entry.lock().await;
        Ok(guard.summary())
    }

    async fn entry(&self, side_id: &str) -> Result<Arc<Mutex<SideEntry>>, SideError> {
        let sides = self.sides.lock().await;
        sides.get(side_id).cloned().ok_or_else(|| {
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
        let entry = self.entry(side_id).await?;
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
        tokio::spawn(task.run(stream));
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
    pub async fn close(&self, side_id: &str, expected: Option<&str>) -> Result<(), SideError> {
        let entry = match self.entry(side_id).await {
            Ok(entry) => entry,
            Err(_) => {
                // A repeated close against a tombstone is idempotent (spec §8.1).
                let tombstones = self.tombstones.lock().await;
                return if tombstones.contains_key(side_id) {
                    Ok(())
                } else {
                    Err(SideError::new(
                        SideErrorReason::SideNotFound,
                        format!("找不到旁支 {side_id}"),
                    ))
                };
            }
        };
        let (parent, generation, accepted) = {
            let mut guard = entry.lock().await;
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
            guard.closing = true;
            let accepted: Vec<(String, String)> =
                guard.ledger.accepted_requests().into_iter().collect();
            (guard.parent.clone(), guard.generation, accepted)
        };

        {
            let mut guard = entry.lock().await;
            guard.runner.close().await?;
        }
        // `side/closed` 是**终态**,必须在本条 side 从表里消失之前发布:客户端收到
        // 它就丢弃该 generation 的本地内容与在途分页,不等一个已经不存在的快照。
        // 它走带背压的投递,拥塞时宁可阻塞也不丢——丢了另一端会停在旧视图。
        let closed = {
            let mut guard = entry.lock().await;
            Self::stage(&mut guard, |entry| super::SidePublishedUpdate::Closed {
                side_id: entry.side_id.clone(),
                reason: CloseReason::UserClosed,
            })
        };
        if let Some(staged) = closed {
            self.deliver(staged).await;
        }
        self.release_side_snapshots(side_id).await;
        self.sides.lock().await.remove(side_id);
        {
            let mut parents = self.parents.lock().await;
            if let Some(ParentSlot::Ready(current)) = parents.get(&parent)
                && current == side_id
            {
                parents.remove(&parent);
            }
        }
        let mut tombstones = self.tombstones.lock().await;
        tombstones.insert(
            side_id.to_string(),
            Tombstone {
                side_id: side_id.to_string(),
                parent,
                generation,
                accepted,
                created_at: tokio::time::Instant::now(),
            },
        );
        Self::prune_tombstones(&mut tombstones, tokio::time::Instant::now());
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
        let tombstones = self.tombstones.lock().await;
        tombstones.get(side_id).map(|stone| ClosedSide {
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
        let tombstones = self.tombstones.lock().await;
        tombstones.get(side_id).map(|stone| stone.accepted.clone())
    }

    /// A consistent snapshot of one side: items plus the metadata a client needs to
    /// reconcile.
    pub async fn snapshot(&self, side_id: &str) -> Result<SideSnapshot, SideError> {
        let entry = self.entry(side_id).await?;
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
    pub async fn begin_parent_change(&self, parent: &str) -> ParentChangeGuard<'_> {
        let removed = {
            let mut parents = self.parents.lock().await;
            parents.remove(parent)
        };
        let side_id = match &removed {
            Some(ParentSlot::Ready(side_id)) => Some(side_id.clone()),
            _ => None,
        };
        ParentChangeGuard {
            registry: self,
            parent: parent.to_string(),
            slot: removed,
            side_id,
            committed: false,
        }
    }

    async fn apply_parent_change(&self, parent: &str, side_id: Option<&str>, reason: CloseReason) {
        let Some(side_id) = side_id else {
            return;
        };
        let Ok(entry) = self.entry(side_id).await else {
            return;
        };
        let (generation, accepted, side_parent) = {
            let mut guard = entry.lock().await;
            guard.closing = true;
            let accepted = guard.ledger.accepted_requests().into_iter().collect();
            (guard.generation, accepted, guard.parent.clone())
        };
        // The close is best effort here: the parent is already going away, and a
        // side that fails to converge must not keep the destructive operation
        // waiting. It still leaves a tombstone so a retry is answered honestly.
        {
            let mut guard = entry.lock().await;
            let _ = guard.runner.close().await;
        }
        let _ = reason;
        self.sides.lock().await.remove(side_id);
        let mut tombstones = self.tombstones.lock().await;
        tombstones.insert(
            side_id.to_string(),
            Tombstone {
                side_id: side_id.to_string(),
                parent: side_parent,
                generation,
                accepted,
                created_at: tokio::time::Instant::now(),
            },
        );
        Self::prune_tombstones(&mut tombstones, tokio::time::Instant::now());
        let _ = parent;
    }

    /// Stop every side and wait (bounded) for their resources.
    ///
    /// Never called from the request loop: the plan is explicit that shutdown must
    /// not block RPC handling on runner teardown.
    pub async fn shutdown(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let entries: Vec<(String, Arc<Mutex<SideEntry>>)> = {
            let sides = self.sides.lock().await;
            sides
                .iter()
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect()
        };
        for (side_id, entry) in &entries {
            let mut guard = entry.lock().await;
            guard.closing = true;
            let _ = guard.runner.close().await;
            let _ = side_id;
        }
        self.sides.lock().await.clear();
        self.parents.lock().await.clear();
    }

    pub async fn side_count(&self) -> usize {
        self.sides.lock().await.len()
    }

    pub async fn tombstone_count(&self) -> usize {
        self.tombstones.lock().await.len()
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
    registry: &'a SideRegistry,
    parent: String,
    slot: Option<ParentSlot>,
    side_id: Option<String>,
    committed: bool,
}

impl ParentChangeGuard<'_> {
    /// Finish the change: the old generation is invalidated and its side is gone.
    pub async fn commit(mut self, reason: CloseReason) {
        self.committed = true;
        let side_id = self.side_id.take();
        self.registry
            .apply_parent_change(&self.parent, side_id.as_deref(), reason)
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
            && let Ok(mut parents) = self.registry.parents.try_lock()
        {
            parents.insert(self.parent.clone(), slot);
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
