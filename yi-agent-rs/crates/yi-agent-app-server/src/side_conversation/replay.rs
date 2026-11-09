//! 修订序号、不可变分页与父订阅广播(spec §8.2)。
//!
//! 这一层解决三件彼此独立、却经常被混为一谈的事:
//!
//! 1. **一页必须来自同一时刻。** 客户端把一页装进本地基线时,`status`、`usage`、
//!    `accepted_requests`、`effective_config` 必须全部出自同一把锁内的一瞬。分两次
//!    读会拼出「第 3 页说还在跑、第 4 页的状态却是 idle」这种页间矛盾。
//! 2. **分页必须按字节,不能按条数。** 一条 128 KiB 的 item 与一百条 1 KiB 的
//!    item 条数差两个数量级,字节数却可能相同;按条数切会做出整帧被传输层拒掉的
//!    页。
//! 3. **每个已发布修订恰好一帧,`seq` 连续。** `seq` 是客户端判断「缺没缺帧」的
//!    唯一依据;一帧丢了就得全量对账,绝不允许拿 `max(seq)` 跳过缺口。
//!
//! 快照的**不可变性**靠 `Arc<SideItem>`:锁内只复制一次索引,锁外序列化。序列化
//! 一份 16 MiB 的转录不该占着 side 的锁——那会让同时到达的 `turnStart` 排队。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::protocol::SideSummaryWire;
use crate::protocol::{
    Item, SideEffectiveConfigWire, SideReadItemsResult, SideStatusWire, SideTokenTotalsWire,
    SideUsageWire,
};
use crate::side_conversation::{SideEffectiveConfig, SideSummary};
use yi_agent_runtime::side_conversation::{
    CloseReason, SIDE_TRANSCRIPT_MAX_ITEM_BYTES, SideError, SideErrorReason, SideItem,
    SideItemContent, SideResult, SideStatus, SideToolStatus, SideUsage,
};

/// 线上信封的硬上限:512 KiB。
///
/// 比 [`MAX_FRAME_BYTES`](1 MiB)低一半,给 relay 与 JSON 转义膨胀留余量(spec
/// §8.2)。分页按**实际序列化字节**试装,所以这是硬约束,不是估计值。
pub const SIDE_ENVELOPE_BUDGET_BYTES: usize = 512 * 1024;

/// 逐字 item 的节流窗口(spec §8.2:最多每 50ms 一次完整 upsert)。
pub const SIDE_ITEM_FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// 一页预留的 JSON-RPC 外壳与转义余量。
///
/// 条目字节之和不超过 `budget - reserve`;外壳本身(`jsonrpc`/`id`/`result` 与
/// 各元数据字段)远小于它。
pub const SIDE_PAGE_CONTROL_RESERVE: usize = 8 * 1024;

/// 一页可容纳的条目字节上限。
pub const SIDE_PAGE_BUDGET_BYTES: usize = SIDE_ENVELOPE_BUDGET_BYTES - SIDE_PAGE_CONTROL_RESERVE;

/// 快照 token 的惰性续期上限。
pub const SIDE_TOKEN_IDLE_TTL: Duration = Duration::from_secs(60);
/// 快照 token 的绝对上限:续期不能把一个 token 活过它。
pub const SIDE_TOKEN_ABSOLUTE_TTL: Duration = Duration::from_secs(5 * 60);
/// 每连接每个 side 最多一个快照 token。
pub const SIDE_TOKENS_PER_CONNECTION: usize = 1;
/// 每宿主最多八个快照 token。
pub const SIDE_TOKENS_PER_HOST: usize = 8;
/// 所有快照共同钉住的 item 字节上限。
///
/// 只限 token 个数不够:八个 token 各自钉住一份 16 MiB 转录就是 128 MiB,恰好
/// 是这里允许的上限,所以两个上限必须一起算。
pub const SIDE_SNAPSHOT_BYTES: usize = 128 * 1024 * 1024;

/// 一页内容的来源:注册表锁内捕获的一次一致读。
///
/// 所有字段在同一把锁内取值,之后不再变化;`items` 是不可变索引,`Arc` 指向当时
/// 的那一版,所以「快照之后 item 又被更新」不会改写已签发的快照。
#[derive(Debug, Clone)]
pub struct ReplayBaseline {
    pub side_id: String,
    pub parent_thread_id: String,
    pub generation: u64,
    pub snapshot_seq: u64,
    pub status: SideStatus,
    pub usage: SideUsage,
    pub active_turn_id: Option<String>,
    pub accepted_requests: BTreeMap<String, String>,
    pub effective_config: SideEffectiveConfig,
    pub items: Vec<Arc<SideItem>>,
}

impl ReplayBaseline {
    /// 从注册表的一次快照构造基线。调用方必须在**同一把锁内**取得 `snapshot`。
    pub fn from_snapshot(snapshot: &crate::side_conversation::SideSnapshot) -> Self {
        Self {
            side_id: snapshot.summary.side_id.clone(),
            parent_thread_id: snapshot.summary.parent_thread_id.clone(),
            generation: snapshot.summary.generation,
            snapshot_seq: snapshot.summary.last_seq,
            status: snapshot.summary.status,
            usage: snapshot.usage,
            active_turn_id: snapshot.summary.active_turn_id.clone(),
            accepted_requests: snapshot.accepted.iter().cloned().collect(),
            effective_config: snapshot.config.clone(),
            items: snapshot.items.clone(),
        }
    }
}

/// 一个在途的、绑定到具体连接的快照。
#[derive(Debug)]
struct SnapshotToken {
    side_id: String,
    connection: String,
    baseline: ReplayBaseline,
    issued_at: Instant,
    touched_at: Instant,
    /// 已返回给客户端的 item 条数(下一页的起点)。
    emitted: usize,
    /// 这份快照钉住的字节数,计入宿主级总量。
    retained_bytes: usize,
}

/// 一页的内部产物。
#[derive(Debug)]
pub struct Page {
    pub result: SideReadItemsResult,
    /// 这是不是最后一页(末页已释放 token)。
    pub finished: bool,
    /// 本页装了几条。
    pub page_len: usize,
}

/// 分页登记表。
///
/// 按连接分账而不是全局一张表:token 的语义是「一条连接在按页读一个 side」,跨连接
/// 复用会让 A 的游标拼进 B 的帧。连接断开时 `release_connection` 一次撤掉它的全部
/// token——这正是按连接记账买到的能力。
#[derive(Default)]
pub struct ReplayStore {
    tokens: HashMap<String, SnapshotToken>,
    retained_bytes: usize,
}

impl ReplayStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 读一页。
    ///
    /// `token` 为空表示首读:用 `snapshot` 建立基线并签发 token。之后每页都必须带
    /// 同一个 token 与它上次返回的 cursor,否则 `snapshot_expired`——「拼两个时刻
    /// 的页」正是要拒绝的情形。
    pub fn read_page(
        &mut self,
        connection: &str,
        side_id: &str,
        token: Option<&str>,
        cursor: Option<&str>,
        snapshot: Option<ReplayBaseline>,
    ) -> SideResult<Page> {
        let now = Instant::now();
        self.prune(now);

        match token {
            None => {
                if cursor.is_some() {
                    return Err(SideError::new(
                        SideErrorReason::InvalidParams,
                        "cursor 必须与 snapshotToken 一起使用",
                    ));
                }
                let baseline = snapshot.ok_or_else(|| {
                    SideError::new(SideErrorReason::InvalidParams, "首读缺少一致性快照")
                })?;
                if baseline.side_id != side_id {
                    return Err(SideError::new(
                        SideErrorReason::InvalidParams,
                        "快照来源与 sideId 不一致",
                    ));
                }
                self.issue(connection, baseline, now)
            }
            Some(token) => {
                // 先粗判存在,再把「不属于这条连接/side」与「过期」分开报:前者是
                // 调用方的错,后者只需要重开快照。
                let entry = self.tokens.get(token).ok_or_else(expired)?;
                if entry.side_id != side_id || entry.connection != connection {
                    return Err(expired());
                }
                // cursor 的语义是「已经返回了多少条」,必须恰好等于当前已发条数:
                // 任何别的数字都可能让两页重叠或漏条。
                match (cursor, entry.emitted) {
                    (None, 0) => {}
                    (Some(given), emitted) if given == emitted.to_string() => {}
                    _ => return Err(expired()),
                }
                if let Some(entry) = self.tokens.get_mut(token) {
                    // 续期只在真正读到一页时推进;绝对上限看 `issued_at`,所以反复
                    // 续期也活不过五分钟。
                    entry.touched_at = now;
                }
                self.take_page(token, now)
            }
        }
    }

    /// 连接断开:撤掉它的全部 token。
    pub fn release_connection(&mut self, connection: &str) {
        for token in self.matching(|entry| entry.connection == connection) {
            self.drop_token(&token);
        }
    }

    /// side 关闭:它的快照立刻作废,不能从 tombstone 恢复正文。
    pub fn release_side(&mut self, side_id: &str) {
        for token in self.matching(|entry| entry.side_id == side_id) {
            self.drop_token(&token);
        }
    }

    /// Whether `token` exists, belongs to `connection`, and names `side_id`.
    ///
    /// Exists so a caller can ask "may I keep paging this side?" **without**
    /// consuming the token, and without reordering the errors a page can produce:
    /// a foreign token must still answer `snapshot_expired`, not "the side closed".
    pub fn owns_token(&self, connection: &str, side_id: &str, token: &str) -> bool {
        self.tokens
            .get(token)
            .map(|entry| entry.connection == connection && entry.side_id == side_id)
            .unwrap_or(false)
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    fn matching(&self, keep: impl Fn(&SnapshotToken) -> bool) -> Vec<String> {
        self.tokens
            .iter()
            .filter(|(_, entry)| keep(entry))
            .map(|(token, _)| token.clone())
            .collect()
    }

    fn issue(
        &mut self,
        connection: &str,
        baseline: ReplayBaseline,
        now: Instant,
    ) -> SideResult<Page> {
        // 每连接每 side 一个:同一连接重开快照必然是要换基线,替换旧的。
        for token in self
            .matching(|entry| entry.connection == connection && entry.side_id == baseline.side_id)
        {
            self.drop_token(&token);
        }
        if self.tokens.len() >= SIDE_TOKENS_PER_HOST {
            return Err(SideError::new(
                SideErrorReason::ResourceLimit,
                "宿主在途快照过多",
            ));
        }
        let retained_bytes = baseline
            .items
            .iter()
            .map(|item| item_bytes(item))
            .sum::<usize>();
        if self.retained_bytes + retained_bytes > SIDE_SNAPSHOT_BYTES {
            return Err(SideError::new(
                SideErrorReason::ResourceLimit,
                "快照保留字节超过宿主上限",
            ));
        }
        let token = uuid::Uuid::new_v4().to_string();
        self.retained_bytes += retained_bytes;
        self.tokens.insert(
            token.clone(),
            SnapshotToken {
                side_id: baseline.side_id.clone(),
                connection: connection.to_string(),
                baseline,
                issued_at: now,
                touched_at: now,
                emitted: 0,
                retained_bytes,
            },
        );
        self.take_page(&token, now)
    }

    fn take_page(&mut self, token: &str, now: Instant) -> SideResult<Page> {
        let Some(entry) = self.tokens.get(token) else {
            return Err(expired());
        };
        let start = entry.emitted;
        let mut items = Vec::new();
        let mut used = 0usize;
        for item in entry.baseline.items.iter().skip(start) {
            let wire = wire_item(item)?;
            let size = serde_json::to_vec(&wire)
                .map_err(|error| {
                    SideError::new(
                        SideErrorReason::SnapshotTooLarge,
                        format!("旁支条目无法序列化：{error}"),
                    )
                })?
                .len();
            // 第一条永远塞进去:单条已被转录层按 128 KiB 限住,这里若连第一条都
            // 放不下,说明页预算被配到比单条上限还小——那该报错,而不是静默回空页。
            if !items.is_empty() && used + size > SIDE_PAGE_BUDGET_BYTES {
                break;
            }
            used += size;
            items.push(wire);
        }
        let emitted = start + items.len();
        let finished = emitted >= entry.baseline.items.len();
        let result = SideReadItemsResult {
            items,
            snapshot_seq: entry.baseline.snapshot_seq,
            status: SideStatusWire::from_runtime(entry.baseline.status),
            usage: wire_usage(entry.baseline.usage),
            active_turn_id: entry.baseline.active_turn_id.clone(),
            accepted_requests: entry.baseline.accepted_requests.clone(),
            effective_config: wire_config(&entry.baseline.effective_config),
            snapshot_token: token.to_string(),
            next_cursor: (!finished).then(|| emitted.to_string()),
        };
        let page_len = result.items.len();
        if let Some(entry) = self.tokens.get_mut(token) {
            entry.emitted = emitted;
            if finished {
                // 末页即释放:客户端已拿全,再钉住 128 MiB 只为一个不会再用到的
                // token 没有意义。水位仍随本页返回。
                let retained = entry.retained_bytes;
                self.retained_bytes = self.retained_bytes.saturating_sub(retained);
                self.tokens.remove(token);
            }
        }
        let _ = now;
        Ok(Page {
            result,
            finished,
            page_len,
        })
    }

    fn prune(&mut self, now: Instant) {
        for token in self.matching(|entry| {
            now.duration_since(entry.touched_at) > SIDE_TOKEN_IDLE_TTL
                || now.duration_since(entry.issued_at) > SIDE_TOKEN_ABSOLUTE_TTL
        }) {
            self.drop_token(&token);
        }
    }

    fn drop_token(&mut self, token: &str) {
        if let Some(entry) = self.tokens.remove(token) {
            self.retained_bytes = self.retained_bytes.saturating_sub(entry.retained_bytes);
        }
    }
}

/// 进程级共享的分页登记表。
///
/// 为什么不是每条连接一份:token 的宿主级上限(8 份 / 128 MiB)是**宿主**资源,
/// 按连接各建一份就永远无法执行它。`release_connection` 负责在断连时清该连接的
/// 份额,所以共享也不会泄漏。
pub type SharedReplayStore = Arc<tokio::sync::Mutex<ReplayStore>>;

/// 建一份空的共享登记表。
pub fn shared_replay_store() -> SharedReplayStore {
    Arc::new(tokio::sync::Mutex::new(ReplayStore::new()))
}

/// 记一页的形状。**不记正文**:item 与旁支正文都是用户内容。
pub fn log_page(side_id: &str, page_len: usize, finished: bool) {
    tracing::debug!(side_id, page_len, finished, "side/readItems 返回一页");
}

fn expired() -> SideError {
    SideError::new(SideErrorReason::SnapshotExpired, "快照已过期，请重新读取")
}

fn item_bytes(item: &SideItem) -> usize {
    serde_json::to_vec(item).map(|raw| raw.len()).unwrap_or(0)
}

pub fn wire_usage(usage: SideUsage) -> SideUsageWire {
    SideUsageWire {
        total: SideTokenTotalsWire {
            input: usage.total.input,
            output: usage.total.output,
            cache_creation: usage.total.cache_creation,
            cache_read: usage.total.cache_read,
            reasoning: usage.total.reasoning,
        },
        complete: usage.complete,
    }
}

pub fn wire_config(config: &SideEffectiveConfig) -> SideEffectiveConfigWire {
    SideEffectiveConfigWire {
        model: config.model.clone(),
        reasoning_effort: config.reasoning_effort.clone(),
        preserve_thinking: config.preserve_thinking,
        // 旁支的工具集由白名单装配,且每个工具都必须自报只读才允许注册。
        read_only: true,
        tool_names: config.tool_names.clone(),
    }
}

/// 把一个转录条目适配成线上 `Item`。
///
/// 图片恒为空:旁支通知与回放不携带二进制(spec §8.2,也不加可读私有路径的
/// `image/read` 后门)。适配后重新核对单条上限——转录层已按 128 KiB 限住,这里再核
/// 一次是因为前缀(`提示：`)与 JSON 转义会改变字节数。超限就报错,不做第二次截断:
/// 两端对边界的理解必须一致。
pub fn wire_item(item: &SideItem) -> SideResult<Item> {
    let wire = match &item.content {
        SideItemContent::UserMessage { text } => Item::UserMessage {
            id: item.id.clone(),
            text: text.clone(),
            attachments: Vec::new(),
            images: Vec::new(),
        },
        SideItemContent::AgentMessage { text } => Item::AgentMessage {
            id: item.id.clone(),
            text: text.clone(),
        },
        SideItemContent::Thinking { text, redacted } => Item::Thinking {
            id: item.id.clone(),
            text: text.clone(),
            redacted: *redacted,
        },
        SideItemContent::ToolCall {
            call_id,
            name,
            input,
            result,
            status,
            ..
        } => Item::ToolCall {
            id: item.id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
            status: match status {
                SideToolStatus::Running => crate::protocol::ToolStatus::Running,
                SideToolStatus::Completed => crate::protocol::ToolStatus::Completed,
                SideToolStatus::Failed => crate::protocol::ToolStatus::Failed,
            },
            result: result.clone(),
            images: Vec::new(),
        },
        // 提示在切 item 时已计入预算,这里只加一次固定前缀。
        SideItemContent::Notice { text } => Item::AgentMessage {
            id: item.id.clone(),
            text: format!("提示：{text}"),
        },
    };
    let size = serde_json::to_vec(&wire)
        .map_err(|error| {
            SideError::new(
                SideErrorReason::SnapshotTooLarge,
                format!("旁支条目无法序列化：{error}"),
            )
        })?
        .len();
    if size > SIDE_TRANSCRIPT_MAX_ITEM_BYTES {
        return Err(SideError::new(
            SideErrorReason::SnapshotTooLarge,
            format!("旁支条目超过单条上限：{size} 字节"),
        ));
    }
    Ok(wire)
}

/// 检查一个待发送的 JSON-RPC 信封是否超预算。
///
/// **看的是完整信封**,不是其中的 `items`:一个 400 KiB 的正文加上转义、外壳与
/// 元数据就可能越过 512 KiB。超限的错误里**不带原文**,否则「拒绝一个超大帧」本身
/// 又变成了一个超大帧。
pub fn encode_side_envelope(value: &serde_json::Value) -> SideResult<Vec<u8>> {
    let encoded = serde_json::to_vec(value).map_err(|error| {
        SideError::new(
            SideErrorReason::SnapshotTooLarge,
            format!("旁支帧无法序列化：{error}"),
        )
    })?;
    if encoded.len() > SIDE_ENVELOPE_BUDGET_BYTES {
        return Err(SideError::new(
            SideErrorReason::SnapshotTooLarge,
            format!(
                "旁支帧超过 {} 字节上限（实际 {} 字节）",
                SIDE_ENVELOPE_BUDGET_BYTES,
                encoded.len()
            ),
        ));
    }
    Ok(encoded)
}

/// 一个已发布修订的序号。
///
/// `checked_add` 而不是 `+`:耗尽时回 `resource_limit`,而不是在 debug 下 panic、
/// 在 release 下回绕成 0——后者会让客户端的「缺号检测」永久混乱。
pub fn next_seq(seq: u64) -> SideResult<u64> {
    seq.checked_add(1)
        .ok_or_else(|| SideError::new(SideErrorReason::ResourceLimit, "旁支修订序号已耗尽"))
}

/// 一次待发布的修订。
///
/// 每个变体恰好对应一帧通知;`into_notification` 把它的 `seq` 钉住。
#[derive(Debug, Clone)]
pub enum SidePublishedUpdate {
    Changed {
        side_id: String,
        summary: SideSummary,
    },
    Item {
        side_id: String,
        turn_id: String,
        item: Item,
    },
    Completed {
        side_id: String,
        turn_id: String,
        status: SideStatusWire,
        reason: Option<String>,
        error: Option<String>,
        usage: SideUsageWire,
    },
    Closed {
        side_id: String,
        reason: CloseReason,
    },
}

impl SidePublishedUpdate {
    /// 这条修订属于哪个 side。投递需要它,而 `into_notification` 的其余参数由
    /// 注册表提供(父 id 与 generation 不属于单帧内容)。
    pub fn side_id(&self) -> Option<&str> {
        match self {
            Self::Changed { side_id, .. }
            | Self::Item { side_id, .. }
            | Self::Completed { side_id, .. }
            | Self::Closed { side_id, .. } => Some(side_id),
        }
    }

    /// 把这次更新变成一帧通知。
    ///
    /// 订阅键恒为**父** threadId(见 `Notification::thread_key`):桌面 Feed::All
    /// 直接收到,手机把当前父 pin 在既有订阅窗口,不额外占名额。
    pub fn into_notification(
        self,
        seq: u64,
        parent_thread_id: &str,
        side_id: &str,
        generation: u64,
    ) -> crate::protocol::Notification {
        use crate::protocol::Notification;
        let parent_thread_id = parent_thread_id.to_string();
        let side_id = side_id.to_string();
        match self {
            Self::Changed { summary, .. } => Notification::SideChanged {
                parent_thread_id,
                side_id,
                generation,
                seq,
                summary: SideSummaryWire::from_registry(&summary),
            },
            Self::Item { turn_id, item, .. } => Notification::SideItemUpdated {
                parent_thread_id,
                side_id,
                generation,
                seq,
                turn_id,
                item,
            },
            Self::Completed {
                turn_id,
                status,
                reason,
                error,
                usage,
                ..
            } => Notification::SideTurnCompleted {
                parent_thread_id,
                side_id,
                generation,
                seq,
                turn_id,
                status,
                reason,
                error,
                usage,
            },
            Self::Closed { reason, .. } => Notification::SideClosed {
                parent_thread_id,
                side_id,
                generation,
                seq,
                reason: reason.as_str().to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests;
