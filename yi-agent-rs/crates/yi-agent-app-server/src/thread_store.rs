//! thread 会话持久化:每 thread 一个只追加 `.jsonl` 日志 + 一个可变 `.meta.json`。
//!
//! 目录布局 `<workdir>/.yi-agent/threads/`:
//! - `<thread_id>.jsonl`     只追加,每 turn 一行 `TurnLine::Turn`
//! - `<thread_id>.meta.json` 可变,整体原子重写

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yi_agent_core::{ContentBlock, Message, Role};

use crate::protocol::Item;

/// 线程的自主权模式。持久化到 `.meta.json`,重开 app 后恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThreadMode {
    #[default]
    Normal,
    Yolo,
}

/// thread 元数据(`.meta.json` 的内容)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadMeta {
    pub thread_id: String,
    pub cwd: String,
    pub model: String,
    /// epoch 毫秒。
    pub created_at: i64,
    pub updated_at: i64,
    pub title: Option<String>,
    /// 该 thread 的自主权模式;旧 meta 缺失时默认 Normal。
    #[serde(default)]
    pub permission_mode: ThreadMode,
    /// 置顶顺序键：`Some` 表示已置顶（数值越大越靠前），`None` 表示未置顶。
    /// 旧 meta 缺字段时默认 `None`。
    #[serde(default)]
    pub pin_seq: Option<i64>,
    /// 该会话由看板创建时，它所属的项目根（绝对路径，canonical）。None = 普通会话。
    #[serde(default)]
    pub board_project: Option<String>,
    /// 该会话对应的看板卡 id。None = 普通会话。
    #[serde(default)]
    pub card_id: Option<String>,
    /// 会话模型覆盖：清单里的显示名。None = 跟随全局默认模型。
    /// 与 `model` 的区别：`model` 是**当前生效的 model 串**（显示用），
    /// `model_ref` 是用户在清单里选中的**显示名**（选择用）。
    #[serde(default)]
    pub model_ref: Option<String>,
}

/// 一次 turn 的 token 用量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 写入 prompt cache 的 token(Anthropic cache write);旧日志缺失按 0。
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// 命中 prompt cache 读取的 token;旧日志缺失按 0。
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}

/// `.jsonl` 里的一行。用带 tag 的枚举,便于日后扩展其它行类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnLine {
    Turn {
        items: Vec<Item>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        #[serde(default)]
        messages: Vec<Message>,
    },
}

/// 进行中 turn 的 checkpoint：崩溃后据此恢复"已完成的部分"。
///
/// 写时机见 `server.rs` 的 driver：turn 开始写一次，之后每次 item finalize
/// 重写，turn 收尾（append 主 jsonl）后删除。`items` 只含已 finalize 的 item
/// （含本轮的 `UserMessage` 提问），进行中的流式文本不入内。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PartialTurn {
    pub turn_id: String,
    pub items: Vec<Item>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TurnUsage>,
}

/// `load` 的结果:meta + 拼接后的 items + 最后一条记录的 messages/usage。
pub struct LoadedThread {
    pub meta: ThreadMeta,
    pub items: Vec<Item>,
    pub messages: Vec<Message>,
    pub usage: Option<TurnUsage>,
    /// 末尾是否是一段崩溃残留的、未收尾的 turn（来自 `.partial.json`）。
    /// `true` 时调用方（`thread/resume`）应在回放后补发 interrupted 标记。
    pub pending_turn: bool,
}

/// 纯存储层:不依赖协议/agent 之外的状态。
pub struct ThreadStore {
    root: PathBuf,
    /// 串行化 meta 的读-改-写:driver 的 `touch` 与主循环的 `rename` 会并发写同一文件。
    meta_lock: std::sync::Mutex<()>,
}

impl ThreadStore {
    pub fn new(workdir: &Path) -> Self {
        Self {
            root: workdir.join(".yi-agent").join("threads"),
            meta_lock: std::sync::Mutex::new(()),
        }
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.meta.json"))
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    pub fn create(&self, meta: &ThreadMeta) -> io::Result<()> {
        if !valid_id(&meta.thread_id) {
            return Err(invalid_id(&meta.thread_id));
        }
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(meta).map_err(io_err)?;
        write_atomic(&self.meta_path(&meta.thread_id), &bytes)
    }

    pub fn append_turn(&self, id: &str, turn: &TurnLine) -> io::Result<()> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        std::fs::create_dir_all(&self.root)?;
        let mut line = serde_json::to_string(turn).map_err(io_err)?;
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(id))?;
        f.write_all(line.as_bytes())
    }

    /// 读并拼接主 jsonl：返回 `(items, 最近一条 messages, 最近一条 usage)`。
    /// 文件缺失视为空；损坏行跳过并记 stderr（绝不因此报错）。
    fn read_log(&self, id: &str) -> (Vec<Item>, Vec<Message>, Option<TurnUsage>) {
        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        match std::fs::read_to_string(self.log_path(id)) {
            Ok(text) => {
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<TurnLine>(line) {
                        Ok(TurnLine::Turn {
                            items: turn_items,
                            usage: turn_usage,
                            messages: turn_messages,
                        }) => {
                            items.extend(turn_items);
                            if !turn_messages.is_empty() {
                                messages = turn_messages;
                            }
                            if turn_usage.is_some() {
                                usage = turn_usage;
                            }
                        }
                        Err(e) => {
                            eprintln!("[app-server] skipping corrupt thread log line ({id}): {e}");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[app-server] failed to read thread log ({id}): {e}"),
        }
        (items, messages, usage)
    }

    /// 已落盘 jsonl 的 items 里是否已含某 item id。`None` 视为不匹配。
    fn log_contains_item_id(items: &[Item], first: Option<&str>) -> bool {
        match first {
            Some(first) => items
                .iter()
                .any(|it| crate::server::item_id(it) == Some(first)),
            None => false,
        }
    }

    /// 该 partial 是否"已被收尾"：取 partial 的**首个 item id** 与已落盘 items 比对。
    /// `true` = 该轮已成功 append 进主 jsonl（只是 delete partial 失败）。
    ///
    /// 这是 `load` 与 `promote_partial` **共用**的唯一判据：传给本函数的 `log_items`
    /// 必须是主 jsonl（不含 partial 合流）的 items，两个调用点才一致。
    fn partial_is_committed(partial: &PartialTurn, log_items: &[Item]) -> bool {
        let first = partial.items.first().and_then(crate::server::item_id);
        Self::log_contains_item_id(log_items, first)
    }

    pub fn load(&self, id: &str) -> io::Result<Option<LoadedThread>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let log = self.log_path(id);
        let meta_path = self.meta_path(id);
        if !log.exists() && !meta_path.exists() {
            return Ok(None);
        }

        let (mut items, mut messages, mut usage) = self.read_log(id);

        let meta = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
            .unwrap_or_else(|| rebuild_meta(id, &log, &messages));

        // 合流 checkpoint：仅当 partial 的首个 item id 尚未出现在主 jsonl
        // （= 该轮未成功收尾）时采纳。首个 item 是本轮提问 `user-<turn_id>`，
        // 全局唯一，足以判断"这轮是否已 append"。
        let mut pending_turn = false;
        if let Some(partial) = self.read_partial(id) {
            if !Self::partial_is_committed(&partial, &items) {
                for item in partial.items {
                    // 防御性去重：正常不与已落盘 items 重叠。
                    if !items
                        .iter()
                        .any(|it| crate::server::item_id(it) == crate::server::item_id(&item))
                    {
                        items.push(item);
                    }
                }
                if !partial.messages.is_empty() {
                    messages = partial.messages;
                }
                if partial.usage.is_some() {
                    usage = partial.usage;
                }
                pending_turn = true;
            }
        }

        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
            pending_turn,
        }))
    }

    /// 列出所有 thread 的 meta,按 `updated_at` 降序。
    ///
    /// 只读 `*.meta.json`;损坏的 meta 跳过并记 stderr。目录不存在视为空。
    pub fn list(&self) -> io::Result<Vec<ThreadMeta>> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e),
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".meta.json") {
                continue;
            }
            match std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
            {
                Some(m) => out.push(m),
                None => eprintln!("[app-server] skipping corrupt meta: {name}"),
            }
        }
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.thread_id.cmp(&b.thread_id))
        });
        Ok(out)
    }

    /// 在 store 锁内对 meta 做读-改-写。`Ok(None)` 表示 thread 不存在或 meta 不可读。
    ///
    /// 锁保证 `touch`(driver)与 `rename`(请求循环)不会互相覆盖对方的改动。
    fn update_meta<F: FnOnce(&mut ThreadMeta)>(
        &self,
        id: &str,
        f: F,
    ) -> io::Result<Option<ThreadMeta>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let _guard = self
            .meta_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let path = self.meta_path(id);
        let raw = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                eprintln!("[app-server] failed to read thread meta ({id}): {e}");
                return Ok(None);
            }
        };
        let mut meta = match serde_json::from_str::<ThreadMeta>(&raw) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[app-server] skipping unreadable thread meta ({id}): {e}");
                return Ok(None);
            }
        };
        f(&mut meta);
        let bytes = serde_json::to_vec_pretty(&meta).map_err(io_err)?;
        write_atomic(&path, &bytes)?;
        Ok(Some(meta))
    }

    /// 只重写 meta 的 `title` + `updated_at`。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn rename(&self, id: &str, title: &str) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| {
                meta.title = Some(title.to_string());
                meta.updated_at = now_millis();
            })?
            .is_some())
    }

    /// 只重写 meta 的 `permission_mode`。走同一把 `update_meta` 锁,
    /// 避免与并发 `rename` / `touch` 互相覆盖。
    ///
    /// 有意**不**刷新 `updated_at`:切换自主权模式属于设置变更而非 thread 活动,
    /// 不应影响按 `updated_at` 排序的列表顺序。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn set_permission_mode(&self, id: &str, mode: ThreadMode) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| meta.permission_mode = mode)?
            .is_some())
    }

    /// 写入置顶顺序键：`Some(seq)` = 置顶，`None` = 取消置顶。
    ///
    /// 有意**不**刷新 `updated_at`：置顶属于设置变更而非 thread 活动，不应影响
    /// 按 `updated_at` 排序的普通列表顺序（与 `set_permission_mode` 同理）。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn set_pin_seq(&self, id: &str, seq: Option<i64>) -> io::Result<bool> {
        Ok(self.update_meta(id, |meta| meta.pin_seq = seq)?.is_some())
    }

    /// 更新该 thread 的模型覆盖选择（清单里的显示名），保留其余 meta 字段。
    ///
    /// 走与 `rename` / `set_permission_mode` 相同的 `update_meta` 读-改-写锁与
    /// 原子写路径，避免与并发 `touch` / `rename` 互相覆盖。
    /// 返回 `Err(NotFound)` 表示 thread 不存在或 meta 不可读。
    pub fn set_model_ref(&self, id: &str, model_ref: Option<&str>) -> io::Result<()> {
        let updated = self.update_meta(id, |meta| {
            meta.model_ref = model_ref.map(str::to_string);
            meta.updated_at = now_millis();
        })?;
        match updated {
            Some(_) => Ok(()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "unknown thread")),
        }
    }

    /// 每 turn 完成时调用:更新 `updated_at`,并在 `title` 仍为 `None` 时用
    /// `title_hint`(本轮 prompt)填充。thread 不存在或 meta 不可读时静默返回。
    pub fn touch(&self, id: &str, title_hint: Option<&str>) -> io::Result<()> {
        self.update_meta(id, |meta| {
            meta.updated_at = now_millis();
            if meta.title.is_none() {
                if let Some(hint) = title_hint {
                    let t = title_from(hint);
                    if !t.is_empty() {
                        meta.title = Some(t);
                    }
                }
            }
        })
        .map(|_| ())
    }

    /// thread 是否已知(meta 或 log 任一存在)。
    pub fn exists(&self, id: &str) -> bool {
        if !valid_id(id) {
            return false;
        }
        self.meta_path(id).exists() || self.log_path(id).exists()
    }

    /// 删除两个文件;文件缺失不算错误。返回删除前是否存在。
    ///
    /// 持 `meta_lock`,与 `update_meta` 串行:否则并发的 `touch` 可能在删除后
    /// 把已读到的 meta 重新写回,留下一个无日志的幽灵 thread。
    pub fn delete(&self, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let _guard = self
            .meta_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let meta = self.meta_path(id);
        let log = self.log_path(id);
        let partial = self.partial_path(id);
        let existed = meta.exists() || log.exists() || partial.exists();
        for p in [meta, log, partial] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(existed)
    }

    fn partial_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.partial.json"))
    }

    /// 整体原子重写该 thread 的 checkpoint。`items` 已含本轮提问。
    pub fn write_partial(&self, id: &str, turn: &PartialTurn) -> io::Result<()> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec(turn).map_err(io_err)?;
        write_atomic(&self.partial_path(id), &bytes)
    }

    /// 删除 checkpoint；不存在则幂等成功。
    pub fn clear_partial(&self, id: &str) -> io::Result<()> {
        match std::fs::remove_file(self.partial_path(id)) {
            Ok(()) => Ok(()),
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 把"崩溃残留、尚未收尾"的 partial **升格**为权威日志的一条 turn，并清掉
    /// partial。`thread/resume` 采纳 partial 后必须调用它：否则该 partial 是这一轮
    /// 唯一的持久副本，用户下一条消息的 turn-start checkpoint 会**整体原子覆盖**
    /// 同一文件，该轮的 items 就此永久丢失（而 session 上下文仍"记得"它们，
    /// 造成 item 与上下文不一致）。
    ///
    /// 返回 `Ok(true)` = 确实 append 了一轮（调用方恢复了真实内容）；
    /// `Ok(false)` = 无需 append：partial 不存在，或该轮已收尾（首个 item id 已在
    /// 主 jsonl 里）、或 partial 为空/损坏无法采纳。
    ///
    /// 判据与 `load` 共用 [`Self::partial_is_committed`]，故 promote 与 load 对
    /// "这轮是否已收尾"永远一致：已收尾只清 partial，绝不重复 append。清 partial
    /// 失败只记 stderr——重复调用时判据会再次命中"已收尾"而幂等跳过 append。
    /// 写入格式仍是既有 `TurnLine::Turn`，不改 `.jsonl` 结构、不碰 `.meta.json`。
    pub fn promote_partial(&self, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let Some(partial) = self.read_partial(id) else {
            return Ok(false);
        };
        // 空 items（无 id）足以判定"未收尾"，但采纳它没有意义，只清残留。
        if partial.items.is_empty() {
            self.clear_partial(id)?;
            return Ok(false);
        }
        let (log_items, _, _) = self.read_log(id);
        if Self::partial_is_committed(&partial, &log_items) {
            self.clear_partial(id)?;
            return Ok(false);
        }
        self.append_turn(
            id,
            &TurnLine::Turn {
                items: partial.items,
                usage: partial.usage,
                messages: partial.messages,
            },
        )?;
        self.clear_partial(id)?;
        Ok(true)
    }

    /// 读 checkpoint；缺失或损坏返回 `None`（损坏记 stderr，绝不向上报错）。
    fn read_partial(&self, id: &str) -> Option<PartialTurn> {
        let text = std::fs::read_to_string(self.partial_path(id)).ok()?;
        match serde_json::from_str::<PartialTurn>(&text) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("[app-server] ignoring corrupt partial turn ({id}): {e}");
                None
            }
        }
    }

    /// 清空一个 thread 的**对话记录**，保留它的身份。
    ///
    /// 只删 `.jsonl`，保留 `.meta.json`：thread 仍在 `list()` 里、仍能 `resume`
    /// （回放为空），标题 / cwd / 权限模式不变。这是 `/clear` 的持久化语义——
    /// 若只清内存而不删日志，`resume` 会把旧消息回放回来，用户以为清空了实则没有。
    pub fn truncate(&self, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        // 与 `delete` 一样持锁：避免与并发 `update_meta` 交错。
        let _guard = self
            .meta_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let log = self.log_path(id);
        let existed = log.exists();
        match std::fs::remove_file(&log) {
            Ok(()) => {}
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // `/clear` 语义：连进行中的 checkpoint 一起丢弃，否则重启后它会
        // 把已清空的上下文又拼回来。
        if let Err(e) = self.clear_partial(id) {
            eprintln!("[app-server] failed to clear partial turn ({id}): {e}");
        }
        Ok(existed)
    }
}

fn io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// Generated ids look like `thread-<uuid>`; allow only a conservative charset so
/// an id can never name a path outside the store root or a nested file.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn invalid_id(id: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("invalid thread id: {id:?}"),
    )
}

/// epoch 毫秒。
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 计算重排后的 `pin_seq` 赋值。`order` 为**从顶到底**的完整有序 id 列表，
/// `current` 为这些 id 当前的 `pin_seq`。
///
/// 前提：`current` 需覆盖 `order` 中的 id（缺失按未置顶补号处理）。
///
/// 算法：取 `order` 中 id 在 `current` 里现有 `pin_seq` 的**互异**值升序得
/// `seqs`；把 `order` 从顶到底依次赋值为 `seqs` 的从大到小（`order[0]` 拿最大）。
/// 复用既有互异数值做双射，永不产生新的重复值，且与「数值越大越靠前」的排序契约一致。
/// 若互异值不够（有 `None` 或历史重复值），从 `max(seqs)+1` 起补足。
pub(crate) fn assign_pin_seqs(
    order: &[String],
    current: &HashMap<String, Option<i64>>,
) -> Vec<(String, i64)> {
    let mut seqs: Vec<i64> = order
        .iter()
        .filter_map(|id| current.get(id).copied().flatten())
        .collect();
    seqs.sort_unstable();
    seqs.dedup();
    if seqs.len() < order.len() {
        let mut next = seqs
            .last()
            .map(|m| m.saturating_add(1))
            .unwrap_or_else(now_millis);
        while seqs.len() < order.len() {
            seqs.push(next);
            next = next.saturating_add(1);
        }
    }
    order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), seqs[seqs.len() - 1 - i]))
        .collect()
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 用临时文件 + rename 原子替换,避免半写状态。
///
/// 临时名带 pid + 进程内递增序号:同一文件可能有多个写者(driver 的 touch 与
/// 主循环的 rename),固定名会互相截断。
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(format!(".tmp.{}.{}", std::process::id(), seq));
    let tmp = PathBuf::from(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 把一段文本规整为标题:压缩空白 + 截断到 30 个字符。
fn title_from(hint: &str) -> String {
    let normalized = hint.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(30).collect()
}

/// 从 messages 里取第一条 user 文本作为标题。
fn first_user_text(messages: &[Message]) -> Option<String> {
    for m in messages {
        if m.role != Role::User {
            continue;
        }
        for block in &m.content {
            if let ContentBlock::Text(t) = block {
                let title = title_from(t);
                if !title.is_empty() {
                    return Some(title);
                }
            }
        }
    }
    None
}

/// `.meta.json` 缺失/损坏时,从日志重建最小 meta(cwd/model 留空,由上层兜底)。
fn rebuild_meta(id: &str, log: &Path, messages: &[Message]) -> ThreadMeta {
    let created = std::fs::metadata(log)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or_else(now_millis);
    ThreadMeta {
        thread_id: id.to_string(),
        cwd: String::new(),
        model: String::new(),
        created_at: created,
        updated_at: created,
        title: first_user_text(messages),
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        board_project: None,
        card_id: None,
        model_ref: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store() -> (TempDir, ThreadStore) {
        let dir = TempDir::new().unwrap();
        let s = ThreadStore::new(dir.path());
        (dir, s)
    }

    fn meta(id: &str) -> ThreadMeta {
        ThreadMeta {
            thread_id: id.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: None,
            card_id: None,
            model_ref: None,
        }
    }

    fn turn(items: Vec<Item>, messages: Vec<Message>) -> TurnLine {
        TurnLine::Turn {
            items,
            usage: None,
            messages,
        }
    }

    #[test]
    fn create_then_load_round_trips_meta() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let loaded = s.load("thread-a").unwrap().expect("thread must exist");
        assert_eq!(loaded.meta.thread_id, "thread-a");
        assert_eq!(loaded.meta.cwd, "/tmp");
        assert!(loaded.items.is_empty());
        assert!(loaded.messages.is_empty());
    }

    #[test]
    fn append_turn_then_load_returns_items_and_messages() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let items = vec![Item::UserMessage {
            id: "user-turn-1".into(),
            text: "hi".into(),
        }];
        let messages = vec![Message::user("hi")];
        s.append_turn("thread-a", &turn(items, messages.clone()))
            .unwrap();

        let loaded = s.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.messages, messages);
    }

    #[test]
    fn truncate_drops_the_log_but_keeps_the_thread_identity() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "user-turn-1".into(),
                    text: "hi".into(),
                }],
                vec![Message::user("hi")],
            ),
        )
        .unwrap();

        let existed = s.truncate("thread-a").unwrap();
        assert!(existed, "truncate must report the thread existed");
        // 身份仍在（meta 保留 → list 仍能列出该 thread）。
        assert!(s.exists("thread-a"), "meta must survive truncate");
        assert_eq!(s.list().unwrap().len(), 1, "thread must stay listed");
        // 但对话内容已清空。
        let loaded = s.load("thread-a").unwrap().expect("thread must load");
        assert!(loaded.messages.is_empty(), "messages must be dropped");
        assert!(loaded.items.is_empty(), "items must be dropped");
    }

    #[test]
    fn truncate_rejects_an_invalid_id() {
        let (_d, s) = store();
        let err = s.truncate("../escape").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn load_missing_thread_returns_none() {
        let (_d, s) = store();
        assert!(s.load("nope").unwrap().is_none());
    }

    #[test]
    fn load_concatenates_items_across_turns_and_keeps_last_messages() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "user-turn-1".into(),
                    text: "one".into(),
                }],
                vec![Message::user("one")],
            ),
        )
        .unwrap();
        let last_messages = vec![
            Message::user("one"),
            Message::assistant(vec![ContentBlock::Text("two".into())]),
        ];
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "user-turn-2".into(),
                    text: "two".into(),
                }],
                last_messages.clone(),
            ),
        )
        .unwrap();

        let loaded = s.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 2, "items must be concatenated");
        assert_eq!(loaded.messages, last_messages, "last record's messages win");
    }

    #[test]
    fn rejects_ids_that_escape_the_store_root() {
        let (_d, s) = store();
        let long = "a".repeat(129);
        for bad in ["../evil", "a/b", "..", "", "a\\b", long.as_str()] {
            assert!(s.load(bad).is_err(), "load must reject escaping id {bad:?}");
            assert!(
                s.append_turn(bad, &turn(vec![], vec![])).is_err(),
                "append_turn must reject escaping id {bad:?}"
            );
            let bad_meta = ThreadMeta {
                thread_id: bad.into(),
                ..meta("placeholder")
            };
            assert!(
                s.create(&bad_meta).is_err(),
                "create must reject escaping id {bad:?}"
            );
        }
        // A valid id still works end to end.
        s.create(&meta("thread-a")).unwrap();
        assert!(s.load("thread-a").unwrap().is_some());
    }

    #[test]
    fn load_log_read_error_is_not_fatal() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        // Put a directory where the log file is expected so read_to_string fails
        // with a non-NotFound error (IsADirectory).
        std::fs::create_dir(s.log_path("thread-a")).unwrap();

        let loaded = s
            .load("thread-a")
            .expect("read error must be logged, not propagated")
            .expect("meta exists, so thread must load");
        assert!(loaded.items.is_empty(), "unreadable log yields no items");
        assert_eq!(loaded.meta.thread_id, "thread-a");
    }

    #[test]
    fn list_returns_meta_sorted_by_updated_at_desc() {
        let (_d, s) = store();
        for (id, updated) in [("thread-old", 10), ("thread-new", 30), ("thread-mid", 20)] {
            let mut m = meta(id);
            m.updated_at = updated;
            s.create(&m).unwrap();
        }
        let ids: Vec<String> = s.list().unwrap().into_iter().map(|m| m.thread_id).collect();
        assert_eq!(ids, vec!["thread-new", "thread-mid", "thread-old"]);
    }

    #[test]
    fn list_on_missing_root_is_empty() {
        let dir = TempDir::new().unwrap();
        let s = ThreadStore::new(&dir.path().join("does-not-exist"));
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn rename_updates_only_meta() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn("thread-a", &turn(vec![], vec![Message::user("hi")]))
            .unwrap();
        let log_before = std::fs::read_to_string(s.log_path("thread-a")).unwrap();

        assert!(s.rename("thread-a", "new title").unwrap());
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("new title")
        );
        assert_eq!(
            std::fs::read_to_string(s.log_path("thread-a")).unwrap(),
            log_before,
            "rename must not touch the log"
        );
    }

    #[test]
    fn rename_unknown_returns_false() {
        let (_d, s) = store();
        assert!(!s.rename("nope", "x").unwrap());
    }

    #[test]
    fn touch_sets_title_only_when_absent_and_bumps_updated_at() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.touch("thread-a", Some("  first   message  ")).unwrap();
        let m = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(m.title.as_deref(), Some("first message"));
        assert!(m.updated_at > 1, "updated_at must be bumped");

        s.touch("thread-a", Some("ignored")).unwrap();
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("first message"),
            "an existing title must not be overwritten"
        );
    }

    #[test]
    fn delete_removes_both_files_and_is_repeatable() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn("thread-a", &turn(vec![], vec![])).unwrap();

        assert!(s.delete("thread-a").unwrap());
        assert!(!s.meta_path("thread-a").exists());
        assert!(!s.log_path("thread-a").exists());
        assert!(!s.delete("thread-a").unwrap());
    }

    #[test]
    fn exists_true_if_either_file_present() {
        let (_d, s) = store();
        assert!(!s.exists("thread-a"), "neither file: must not exist");

        // log-only (append_turn creates the log but no meta)
        s.append_turn("thread-a", &turn(vec![], vec![])).unwrap();
        assert!(s.exists("thread-a"), "log-only must count as existing");

        // meta present too
        s.create(&meta("thread-a")).unwrap();
        assert!(s.exists("thread-a"), "meta+log must exist");
    }

    #[test]
    fn rename_touch_delete_reject_invalid_ids() {
        let (_d, s) = store();
        for bad in ["../evil", "a/b", "", "a\\b"] {
            assert!(s.rename(bad, "x").is_err(), "rename must reject {bad:?}");
            assert!(s.touch(bad, None).is_err(), "touch must reject {bad:?}");
            assert!(s.delete(bad).is_err(), "delete must reject {bad:?}");
            assert!(!s.exists(bad), "exists must be false for {bad:?}");
        }
    }

    #[test]
    fn touch_unknown_id_is_silent_noop() {
        let (_d, s) = store();
        s.touch("nope", Some("x"))
            .expect("unknown id must not error");
    }

    #[test]
    fn rename_corrupt_meta_returns_false() {
        let (_d, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(s.meta_path("thread-a"), b"{ not json").unwrap();
        assert!(!s.rename("thread-a", "x").unwrap());
    }

    #[test]
    fn list_tie_break_is_deterministic_by_id() {
        let (_d, s) = store();
        for id in ["thread-c", "thread-a", "thread-b"] {
            let mut m = meta(id);
            m.updated_at = 5;
            s.create(&m).unwrap();
        }
        let ids: Vec<String> = s.list().unwrap().into_iter().map(|m| m.thread_id).collect();
        assert_eq!(ids, vec!["thread-a", "thread-b", "thread-c"]);
    }

    #[test]
    fn touch_after_delete_does_not_resurrect_meta() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        assert!(s.delete("thread-a").unwrap());

        s.touch("thread-a", Some("ghost"))
            .expect("touch on a deleted thread must not error");
        assert!(
            !s.meta_path("thread-a").exists(),
            "touch must not recreate a deleted thread"
        );
        assert!(!s.exists("thread-a"));
    }

    #[test]
    fn load_skips_corrupt_log_lines() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "u1".into(),
                    text: "ok".into(),
                }],
                vec![Message::user("ok")],
            ),
        )
        .unwrap();
        // 模拟崩溃截断:追加一行非法 JSON。
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(s.log_path("thread-a"))
                .unwrap();
            writeln!(f, "{{ not json").unwrap();
        }
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "u2".into(),
                    text: "still here".into(),
                }],
                vec![Message::user("still here")],
            ),
        )
        .unwrap();

        let loaded = s.load("thread-a").unwrap().unwrap();
        assert_eq!(
            loaded.items.len(),
            2,
            "corrupt line must be skipped, not fatal"
        );
        assert_eq!(loaded.messages, vec![Message::user("still here")]);
    }

    #[test]
    fn load_rebuilds_meta_when_missing() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn("thread-a", &turn(vec![], vec![Message::user("recover me")]))
            .unwrap();
        std::fs::remove_file(s.meta_path("thread-a")).unwrap();

        let loaded = s.load("thread-a").unwrap().expect("log still present");
        assert_eq!(loaded.meta.title.as_deref(), Some("recover me"));
        assert_eq!(loaded.meta.thread_id, "thread-a");
        // rebuild_meta 留空 cwd/model,由上层兜底——锁住该契约。
        assert_eq!(loaded.meta.cwd, "");
        assert_eq!(loaded.meta.model, "");
    }

    #[test]
    fn load_rebuilds_meta_when_corrupt() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn("thread-a", &turn(vec![], vec![Message::user("recover me")]))
            .unwrap();
        std::fs::write(s.meta_path("thread-a"), "{ not json").unwrap();

        let loaded = s.load("thread-a").unwrap().expect("log still present");
        assert_eq!(loaded.meta.title.as_deref(), Some("recover me"));
        assert_eq!(loaded.meta.thread_id, "thread-a");
    }

    #[test]
    fn load_tolerates_truncated_final_line() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::UserMessage {
                    id: "u1".into(),
                    text: "ok".into(),
                }],
                vec![Message::user("ok")],
            ),
        )
        .unwrap();
        // 模拟崩溃:最后一行只写了一半,没有换行。
        {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(s.log_path("thread-a"))
                .unwrap();
            f.write_all(b"{\"type\":\"turn\",\"items\":[").unwrap();
        }

        let loaded = s.load("thread-a").unwrap().unwrap();
        assert_eq!(
            loaded.items.len(),
            1,
            "truncated final line must be skipped, not fatal"
        );
        assert_eq!(loaded.messages, vec![Message::user("ok")]);
    }

    #[test]
    fn list_skips_orphan_and_corrupt_meta() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        // 孤立 .jsonl(无 meta)不应出现在 list 里。
        s.append_turn("thread-orphan", &turn(vec![], vec![]))
            .unwrap();
        // 损坏 meta 也不应出现。
        std::fs::write(s.meta_path("thread-bad"), "{ not json").unwrap();

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|m| m.thread_id).collect();
        assert_eq!(ids, vec!["thread-a"]);
    }

    #[test]
    fn turn_usage_loads_without_cache_fields() {
        // 旧格式:没有 cache 字段的 usage 行仍须能加载,缺失字段按 0。
        let line = r#"{"type":"turn","items":[],"usage":{"model":"m","input_tokens":7,"output_tokens":2}}"#;
        let parsed: TurnLine = serde_json::from_str(line).unwrap();
        let TurnLine::Turn { usage, .. } = parsed;
        let u = usage.unwrap();
        assert_eq!(u.input_tokens, 7);
        assert_eq!(u.output_tokens, 2);
        assert_eq!(u.cache_creation_input_tokens, 0);
        assert_eq!(u.cache_read_input_tokens, 0);
    }

    #[test]
    fn turn_usage_round_trips_cache_fields() {
        let u = TurnUsage {
            model: "m".into(),
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 30,
            cache_read_input_tokens: 40,
        };
        let s = serde_json::to_string(&u).unwrap();
        let back: TurnUsage = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cache_creation_input_tokens, 30);
        assert_eq!(back.cache_read_input_tokens, 40);
    }

    #[test]
    fn permission_mode_defaults_to_normal_and_roundtrips() {
        let v: ThreadMeta = serde_json::from_str(
            r#"{"thread_id":"t","cwd":"/x","model":"m","created_at":0,"updated_at":0,"title":null}"#,
        )
        .unwrap();
        assert_eq!(v.permission_mode, ThreadMode::Normal); // 旧 meta 兼容
        let mut m = v;
        m.permission_mode = ThreadMode::Yolo;
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"permission_mode\":\"yolo\""));
        let back: ThreadMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(back.permission_mode, ThreadMode::Yolo);
    }

    #[test]
    fn set_permission_mode_persists() {
        let (_d, s) = store();
        let m = meta("thread-1");
        s.create(&m).unwrap();
        assert!(s.set_permission_mode("thread-1", ThreadMode::Yolo).unwrap());
        let loaded = s.load("thread-1").unwrap().unwrap();
        assert_eq!(loaded.meta.permission_mode, ThreadMode::Yolo);
    }

    #[test]
    fn set_permission_mode_unknown_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_permission_mode("nope", ThreadMode::Yolo).unwrap());
    }

    #[test]
    fn set_permission_mode_rejects_invalid_ids() {
        let (_d, s) = store();
        for bad in ["../evil", "a/b", "", "a\\b"] {
            assert!(
                s.set_permission_mode(bad, ThreadMode::Yolo).is_err(),
                "set_permission_mode must reject {bad:?}"
            );
        }
    }

    #[test]
    fn new_meta_is_not_pinned() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
    }

    #[test]
    fn set_pin_seq_sets_value_without_bumping_updated_at() {
        let (_d, s) = store();
        let mut m = meta("thread-a");
        m.updated_at = 7;
        s.create(&m).unwrap();
        assert!(s.set_pin_seq("thread-a", Some(42)).unwrap());
        let got = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(got.pin_seq, Some(42));
        assert_eq!(got.updated_at, 7, "置顶不得刷新 updated_at");
    }

    #[test]
    fn set_pin_seq_none_clears_it() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_pin_seq("thread-a", Some(1)).unwrap();
        assert!(s.set_pin_seq("thread-a", None).unwrap());
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
    }

    #[test]
    fn set_pin_seq_unknown_id_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_pin_seq("nope", Some(1)).unwrap());
    }

    #[test]
    fn legacy_meta_without_pin_seq_deserializes_unpinned() {
        let (_d, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        // 旧格式：没有 pin_seq 字段。
        let legacy = r#"{"thread_id":"thread-a","cwd":"/tmp","model":"m",
            "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal"}"#;
        std::fs::write(s.meta_path("thread-a"), legacy).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
    }

    #[test]
    fn assign_pin_seqs_reproduces_requested_order_top_to_bottom() {
        let mut cur = HashMap::new();
        cur.insert("a".to_string(), Some(10));
        cur.insert("b".to_string(), Some(20));
        cur.insert("c".to_string(), Some(30));
        // 请求顺序：c, a, b（从顶到底）。
        let order = vec!["c".to_string(), "a".to_string(), "b".to_string()];
        let out = assign_pin_seqs(&order, &cur);
        // 还原顺序：按 seq 降序读出 id，应等于 order。
        let mut pairs: Vec<(&String, i64)> = out.iter().map(|(i, s)| (i, *s)).collect();
        pairs.sort_by(|x, y| y.1.cmp(&x.1));
        let got: Vec<&String> = pairs.iter().map(|(i, _)| *i).collect();
        assert_eq!(
            got,
            vec![&"c".to_string(), &"a".to_string(), &"b".to_string()]
        );
        // 互异。
        let mut seqs: Vec<i64> = out.iter().map(|(_, s)| *s).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 3, "不得产生重复 seq");
    }

    #[test]
    fn assign_pin_seqs_pads_none_values_and_stays_unique() {
        let order = vec!["a".to_string(), "b".to_string()];
        let mut cur = HashMap::new();
        cur.insert("a".to_string(), None); // 异常态：声称置顶但无 seq
        cur.insert("b".to_string(), Some(5));
        let out = assign_pin_seqs(&order, &cur);
        let mut seqs: Vec<i64> = out.iter().map(|(_, s)| *s).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 2, "补号后仍须互异");
        // a 在最顶 → 其 seq 更大。
        let a = out.iter().find(|(i, _)| i == "a").unwrap().1;
        let b = out.iter().find(|(i, _)| i == "b").unwrap().1;
        assert!(a > b);
    }

    #[test]
    fn assign_pin_seqs_is_idempotent() {
        let order = vec!["x".to_string(), "y".to_string()];
        let mut cur = HashMap::new();
        cur.insert("x".to_string(), Some(100));
        cur.insert("y".to_string(), Some(50));
        let first = assign_pin_seqs(&order, &cur);
        let cur2: HashMap<String, Option<i64>> =
            first.iter().map(|(i, s)| (i.clone(), Some(*s))).collect();
        let second = assign_pin_seqs(&order, &cur2);
        let mut a: Vec<i64> = first.iter().map(|(_, s)| *s).collect();
        let mut b: Vec<i64> = second.iter().map(|(_, s)| *s).collect();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "以当前顺序再算一次应稳定");
    }

    fn sample_meta(id: &str) -> ThreadMeta {
        ThreadMeta {
            thread_id: id.to_string(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: None,
            card_id: None,
            model_ref: None,
        }
    }

    fn user_item(id: &str, text: &str) -> crate::protocol::Item {
        crate::protocol::Item::UserMessage {
            id: id.into(),
            text: text.into(),
        }
    }

    #[test]
    fn load_without_partial_is_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "hi")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert!(!loaded.pending_turn, "no partial file ⇒ nothing pending");
    }

    #[test]
    fn load_merges_an_uncommitted_partial_turn() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "first")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        // 崩溃残留：下一轮只写了 partial，主 jsonl 里没有它的首个 item。
        store
            .write_partial(
                "thread-a",
                &PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![
                        user_item("user-turn-t2", "second"),
                        crate::protocol::Item::AgentMessage {
                            id: "item-turn-t2-1".into(),
                            text: "partial answer".into(),
                        },
                    ],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        let ids: Vec<String> = loaded
            .items
            .iter()
            .map(|i| crate::server::item_id(i).unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["user-t1", "user-turn-t2", "item-turn-t2-1"]);
        assert!(loaded.pending_turn, "uncommitted partial ⇒ pending_turn");
    }

    #[test]
    fn load_ignores_a_partial_whose_turn_is_already_committed() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        // 收尾成功（提问已进 jsonl），但 delete partial 失败。
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-turn-t2", "second")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial(
                "thread-a",
                &PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![user_item("user-turn-t2", "second")],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1, "must not duplicate the committed turn");
        assert!(!loaded.pending_turn);
    }

    #[test]
    fn promote_partial_appends_the_recovered_turn_and_clears_it() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "first")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial(
                "thread-a",
                &PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![
                        user_item("user-turn-t2", "second"),
                        crate::protocol::Item::AgentMessage {
                            id: "item-turn-t2-1".into(),
                            text: "half".into(),
                        },
                    ],
                    messages: vec![yi_agent_core::Message::user("carried")],
                    usage: None,
                },
            )
            .unwrap();

        // 采纳：追加成主 jsonl 的一轮，并清掉 partial。
        assert!(store.promote_partial("thread-a").unwrap());
        assert!(!dir
            .path()
            .join(".yi-agent/threads/thread-a.partial.json")
            .exists());

        // 冷 load：崩溃轮的内容在、只一份、上下文仍是崩溃轮的。
        let loaded = store.load("thread-a").unwrap().unwrap();
        let ids: Vec<String> = loaded
            .items
            .iter()
            .map(|i| crate::server::item_id(i).unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["user-t1", "user-turn-t2", "item-turn-t2-1"]);
        assert!(!loaded.pending_turn);
        assert_eq!(loaded.messages, vec![yi_agent_core::Message::user("carried")]);

        // 幂等：partial 已清，再 promote 是 no-op，绝不重复 append。
        assert!(!store.promote_partial("thread-a").unwrap());
        assert_eq!(
            store.load("thread-a").unwrap().unwrap().items.len(),
            3,
            "must not double-append"
        );
    }

    #[test]
    fn promote_partial_clears_without_appending_when_already_committed() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        // 收尾成功（提问已进 jsonl），但 delete partial 失败。
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-turn-t2", "second")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial(
                "thread-a",
                &PartialTurn {
                    turn_id: "turn-t2".into(),
                    items: vec![user_item("user-turn-t2", "second")],
                    messages: vec![],
                    usage: None,
                },
            )
            .unwrap();

        // 判据与 load 一致：首 item id 已在 jsonl ⇒ 不重复 append，只清残留。
        assert!(!store.promote_partial("thread-a").unwrap());
        assert!(!dir
            .path()
            .join(".yi-agent/threads/thread-a.partial.json")
            .exists());
        assert_eq!(
            store.load("thread-a").unwrap().unwrap().items.len(),
            1,
            "must not duplicate the committed turn"
        );
    }

    #[test]
    fn promote_partial_without_a_partial_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        assert!(!store.promote_partial("thread-a").unwrap());
    }

    #[test]
    fn load_ignores_a_corrupt_partial() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "hi")],
                    usage: None,
                    messages: vec![],
                },
            )
            .unwrap();
        let threads = dir.path().join(".yi-agent/threads");
        std::fs::write(threads.join("thread-a.partial.json"), b"{ not json").unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1, "corrupt partial must be ignored");
        assert!(!loaded.pending_turn);
    }

    #[test]
    fn clear_partial_removes_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .write_partial("thread-a", &PartialTurn::default())
            .unwrap();
        store.clear_partial("thread-a").unwrap();
        assert!(!dir
            .path()
            .join(".yi-agent/threads/thread-a.partial.json")
            .exists());
        // 幂等：再删一次仍成功。
        store.clear_partial("thread-a").unwrap();
    }

    #[test]
    fn delete_removes_the_partial_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store.write_partial("thread-a", &PartialTurn::default()).unwrap();
        assert!(store.delete("thread-a").unwrap());
        assert!(!dir.path().join(".yi-agent/threads/thread-a.partial.json").exists());
    }

    #[test]
    fn truncate_removes_the_partial_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn { items: vec![user_item("user-t1", "hi")], usage: None, messages: vec![] },
            )
            .unwrap();
        store.write_partial("thread-a", &PartialTurn::default()).unwrap();
        store.truncate("thread-a").unwrap();
        assert!(!dir.path().join(".yi-agent/threads/thread-a.partial.json").exists());
    }
    #[test]
    fn a_meta_without_board_fields_defaults_to_none() {
        let json = r#"{"thread_id":"t1","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let meta: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.board_project, None, "旧 meta 缺字段必须回 None");
        assert_eq!(meta.card_id, None);
    }

    #[test]
    fn board_fields_round_trip() {
        let meta = ThreadMeta {
            thread_id: "t1".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 2,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: Some("/proj".into()),
            card_id: Some("c1".into()),
            model_ref: None,
        };
        let text = serde_json::to_string(&meta).unwrap();
        let back: ThreadMeta = serde_json::from_str(&text).unwrap();
        assert_eq!(back.board_project.as_deref(), Some("/proj"));
        assert_eq!(back.card_id.as_deref(), Some("c1"));
    }

    #[test]
    fn a_legacy_meta_without_model_ref_loads_as_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        std::fs::create_dir_all(dir.path().join(".yi-agent").join("threads")).unwrap();
        // 沿用既有测试里写 meta 的方式；这里断言缺字段反序列化为 None。
        let json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let meta: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.model_ref, None);
        let _ = store;
    }

    #[test]
    fn set_model_ref_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let now = now_millis();
        let meta = ThreadMeta {
            thread_id: "t".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: now,
            updated_at: now,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            board_project: None,
            card_id: None,
            model_ref: None,
        };
        store.create(&meta).unwrap();
        store.set_model_ref("t", Some("B")).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.model_ref.as_deref(), Some("B"));
        store.set_model_ref("t", None).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.model_ref, None);
    }
}
