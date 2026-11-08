//! thread 会话持久化:每 thread 一个只追加 `.jsonl` 日志 + 一个可变 `.meta.json`。
//!
//! 目录布局 `<workdir>/.yi-agent/threads/`:
//! - `<thread_id>.jsonl`     只追加,每 turn 一行 `TurnLine::Turn`
//! - `<thread_id>.meta.json` 可变,整体原子重写

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use yi_agent_core::{ContentBlock, Message, Role};

use crate::protocol::Item;

/// 组长角色的 `ThreadMeta.role` 取值。一个项目目录至多一个。
pub const LEAD_ROLE: &str = "lead";

/// 线程的自主权模式。持久化到 `.meta.json`,重开 app 后恢复。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThreadMode {
    #[default]
    Normal,
    Yolo,
}

/// 合法的颜色标签 id。顺序即 UI 菜单顺序，**不含灰色**（深色主题下与「无标签」
/// 难以区分）。
pub const TAG_IDS: [&str; 6] = ["red", "orange", "yellow", "green", "blue", "purple"];

/// 该字符串是否是合法的颜色标签 id。
pub fn is_valid_tag(id: &str) -> bool {
    TAG_IDS.contains(&id)
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
    /// 被驱动的会话所属的项目根（绝对路径，canonical）。None = 普通会话。
    ///
    /// 字段名通用化自 `board_project`（被动方不再假定「谁」在驱动）。旧 meta 写的
    /// 是旧键，`alias` 让它们照样读得进来（零迁移：不写盘、不重命名文件）。
    #[serde(default, alias = "board_project")]
    pub driver_project: Option<String>,
    /// 驱动方交给本会话的**不透明**句柄（宿主不解释其含义，只在 worktree 判定时按
    /// 既有约定使用）。None = 普通会话。
    ///
    /// 字段名通用化自 `card_id`；旧键经 `alias` 兼容读取（零迁移，理由同上）。
    #[serde(default, alias = "card_id")]
    pub driver_ref: Option<String>,
    /// 会话模型覆盖：清单里的显示名。None = 跟随全局默认模型。
    /// 与 `model` 的区别：`model` 是**当前生效的 model 串**（显示用），
    /// `model_ref` 是用户在清单里选中的**显示名**（选择用）。
    #[serde(default)]
    pub model_ref: Option<String>,
    /// 会话级 reasoning effort 覆盖（七档之一）。None = 跟随条目默认（再回退
    /// 「未设置＝不发该字段」）。旧 meta 缺字段时默认 None，零迁移。
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 会话的颜色标签 id（`TAG_IDS` 之一）。`None` = 无标签。
    /// 旧 meta 缺字段时默认 `None`（零迁移）。
    #[serde(default)]
    pub tag: Option<String>,
    /// 是否跨轮保留思考块（Anthropic thinking / Responses encrypted_content 等）。
    ///
    /// `true`（默认）= 历史轮的推理凭证原样回传与落盘；`false` = 请求前剥离历史轮、
    /// 并把落盘 transcript 一并剥离（spec §4.1）。**旧 meta 缺字段必须解释为
    /// `true`**（零迁移：`bool` 的 `#[serde(default)]` 是 `false`，故必须显式给
    /// `default_true`，否则老会话会被静默降级为「丢弃凭证」）。
    #[serde(default = "default_true")]
    pub preserve_thinking: bool,
    /// 该会话的关联根（主根之外的附属根，canonical 绝对路径）。
    ///
    /// `thread/start` 时从本机索引快照，运行期不变——**不**随索引后续变化而追改，
    /// 故同一目录下先建的会话不会忽然多出一根。旧 meta 缺字段时为空 =
    /// 单根会话（`Vec` 的内建默认即空，无需 `default_true` 那样的显式默认）。
    #[serde(default)]
    pub extra_roots: Vec<String>,
    /// 会话角色标记。`Some("lead")` = 该会话是所属项目目录的组长;`None` = 普通会话。
    /// 旧 meta 缺字段一律读成 `None`(零迁移)。
    #[serde(default)]
    pub role: Option<String>,
    /// 会话起点：`thread/start` 时的 `HEAD` sha。默认 diff 基准用它。
    ///
    /// `None` = 建会话时不是 git 仓库，或该 meta 早于本字段（**不回填**：读操作
    /// 不产生写入，也不引入「首次打开时锚点 = 此刻」这种与「会话开始」不同的语义）。
    #[serde(default)]
    pub anchor_sha: Option<String>,
    /// 该会话**可见内容集合**的世代。只有会改变服务端 item 集合的操作才递增
    /// ——今天仅 `thread/clear`（`compact` 不递增：它落盘时 items 为空、历史
    /// item 分毫未动，见 spec §1.2）。旧 meta 缺字段必须解释为 0。
    ///
    /// 客户端握手时上报自己缓存的 epoch，服务端据此裁决是否需要全量重同步。
    #[serde(default)]
    pub content_epoch: u64,
    /// 该会话最后一条 item 的 id。仅用于 `thread/listAll` 携带「客户端是否落后」
    /// 的**保守下界**——真正的锚点有效性由 `contains_item_id` 读 jsonl 判定。
    /// 第二处写入点 `promote_partial` 不维护它（偏旧只导致多补一段，不漏内容）。
    #[serde(default)]
    pub last_item_id: Option<String>,
    /// 共享已读水位（epoch 毫秒）：**所有客户端共用的**「我读到哪了」。
    ///
    /// 任一端打开会话就推进它，服务端广播给其余端，于是「手机读过 ⇒ 桌面蓝点也
    /// 消失」。**不是**每设备独立位：那会让「一端读过、另一端仍亮」变成设计意图。
    /// 单调（只取 max），因此乱序、重试、时钟回拨都不会把已读打回未读。
    /// 旧 meta 缺字段必须读成 0（零迁移：那时它还没被读过，也不该凭空冒出蓝点）。
    #[serde(default)]
    pub last_read_at: i64,
    /// 最近一次**回合结束**的时间（epoch 毫秒）。
    ///
    /// 未读的判定式是 `last_completed_at > last_read_at`，故必须与 `updated_at`
    /// 分开记：`updated_at` 在**回合开始**时也会被 `touch` 刷新，拿它当完成时间
    /// 会让「另一端刚发问、还在跑」的会话立刻亮蓝点。`clear` / `compact` 不是
    /// 回合结束，不写本字段。
    #[serde(default)]
    pub last_completed_at: i64,
    /// 从哪个 thread fork 而来。**仅用于展示**，任何读取路径都不看它。
    ///
    /// 既有 meta 文件都没有这个字段。`Option<T>` 在 serde 里缺失即 `None`，
    /// 所以零迁移读得进来（`old_meta_without_forked_from_reads_as_none` 钉住
    /// 这一点）。`skip_serializing_if` 保证未 fork 的会话不写这个键，避免给
    /// 所有既有 meta 平白加一个 `null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// 该会话绑定的插件发布（零或多条）。
    ///
    /// 语义是「这个会话当初是为哪个**具体的**发布建的」，不是「这个插件现在
    /// 是什么版本」。两者必须分开：插件升级后，旧会话仍应带着它建时的版本，
    /// 于是用它发起的调用会被判为 `StaleBinding` 并被要求重建会话；如果这里
    /// 存的是「插件名」或「当前版本」，升级就会静默地把旧会话接到新版本上——
    /// 那等于让一个没验证过的新版本继承旧会话的上下文与权限。
    ///
    /// 旧 meta 缺该字段时为空（`Vec` 的内建默认），即「普通会话，不绑插件」，
    /// 这是**正确**的缺省而不是迁移遗留：绝大多数会话与插件无关。
    #[serde(default)]
    pub plugin_bindings: Vec<yi_agent_plugins::Binding>,
    /// The sandbox this conversation actually ran under when it started.
    ///
    /// Startup settings are the *default for new conversations*, so a saved
    /// change must never silently re-open an old conversation wider than it was
    /// when it began running the user's tools. This field is that record.
    ///
    /// Absent means a meta written before the field existed: the loader reports
    /// `None` and activation falls back to a conservative read-only baseline with
    /// a warning. Present-but-`null` or present-but-invalid is a corrupt claim,
    /// not history, and must refuse activation instead of guessing — hence the
    /// custom deserializer rather than plain `Option`.
    #[serde(
        default,
        deserialize_with = "deserialize_present_baseline",
        skip_serializing_if = "Option::is_none"
    )]
    pub sandbox_baseline: Option<crate::session_sandbox::SandboxBaseline>,
}

/// Distinguishes "field absent" from "field present but invalid".
///
/// `#[serde(default)]` alone maps both to `None`, which would let a hand-edited
/// or partially-written meta claim "no baseline" and be treated as legacy. The
/// caller needs the difference: absent is history and takes the conservative
/// fallback, while a present-but-wrong value is corruption and must be refused.
fn deserialize_present_baseline<'de, D>(
    deserializer: D,
) -> Result<Option<crate::session_sandbox::SandboxBaseline>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    crate::session_sandbox::SandboxBaseline::deserialize(deserializer).map(Some)
}

impl ThreadMeta {
    /// 该会话是否有未读的回合结束：完成时间晚于已读水位。
    ///
    /// 这是**全系统唯一**的未读判定，服务端与客户端同一条口径。`0/0`（旧 meta、
    /// 新建会话）恒为 `false`——不能因为缺字段就让人凭空看到蓝点。
    pub fn is_unread(&self) -> bool {
        self.last_completed_at > self.last_read_at
    }
}

/// `#[serde(default = "…")]` 的显式默认：`bool` 的内建默认是 `false`，而本开关
/// 的缺省语义必须为「保留」。
fn default_true() -> bool {
    true
}

/// 一条 meta 是否是**合法**组长:带组长角色,且不在插件 worktree 里。
///
/// 插件 worktree（[`crate::plugin_worktree`]）不是项目,那里的 `role:"lead"` 只
/// 可能是误落盘(真机已有 9 例:卡片会话 `ask_lead` → `resolve_lead_target` 懒创建)。
/// 把它当作非法,是为了让「谁是组长」在这一个函数里只有一种答案:`find_lead`、
/// wire 上的 `role`、`is_lead_thread`、`listAll` 的分组全部引用它。
pub fn is_lead_meta(meta: &ThreadMeta) -> bool {
    meta.role.as_deref() == Some(LEAD_ROLE)
        && !crate::plugin_worktree::is_plugin_worktree(Path::new(&meta.cwd))
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

/// 一个 thread 的累计用量。`u64`：长会话里 input 很容易超过 `u32` 上限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl UsageTotals {
    pub fn add_turn_usage(&mut self, usage: &TurnUsage) {
        self.input_tokens += usage.input_tokens as u64;
        self.output_tokens += usage.output_tokens as u64;
        self.cache_creation_input_tokens += usage.cache_creation_input_tokens as u64;
        self.cache_read_input_tokens += usage.cache_read_input_tokens as u64;
    }

    pub fn add(&mut self, other: &UsageTotals) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.cache_read_input_tokens == 0
    }
}

/// `.jsonl` 里的一行。用带 tag 的枚举,便于日后扩展其它行类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnLine {
    Turn {
        items: Vec<Item>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        /// 本轮**所有** provider 调用的累加值；老日志缺失时由 `usage` 近似。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_total: Option<TurnUsage>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_total: Option<TurnUsage>,
}

/// How [`ThreadStore::load_with_meta_policy`] treats an unreadable meta file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetaPolicy {
    /// Rebuild from the transcript and carry on (listing/UI reads).
    Tolerant,
    /// Fail (activation reads, where a guess could grant unrecorded access).
    Strict,
}

/// `load` 的结果:meta + 拼接后的 items + 最后一条记录的 messages/usage。
#[derive(Debug)]
pub struct LoadedThread {
    pub meta: ThreadMeta,
    pub items: Vec<Item>,
    pub messages: Vec<Message>,
    pub usage: Option<TurnUsage>,
    /// 该 thread 所有已落盘轮次的累计用量（老轮次按最后一次快照近似）。
    pub totals: UsageTotals,
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

/// 把 `src` 下的普通文件复制到 `dst`，跳过分片上传的 `.tmp` 暂存区。
///
/// 只遍历一层：附件目录的形状恒为 `<tid>/<hash>-<name>`（见
/// [`crate::attachments::stored_relative_path`]），内嵌递归没有必要。非普通文件
/// 同样跳过——与 `image/read` 拒绝 FIFO 同一条理由：一个怪异条目不该把主循环挂住。
fn copy_attachment_tree(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        if from.file_name().and_then(|n| n.to_str()) == Some(".tmp") {
            continue;
        }
        if !from.is_file() {
            continue;
        }
        if let Some(name) = from.file_name() {
            std::fs::copy(&from, dst.join(name))?;
        }
    }
    Ok(())
}

/// 把历史里指向源会话附件目录的路径改写成新会话的。
///
/// `Item` 与 `Message` 是两套不同的形状，都要走；`ToolResult` 里的内容块可能
/// 再嵌套 `Image`，故 `ContentBlock` 侧必须递归。
fn rewrite_attachment_paths(loaded: &mut LoadedThread, from: &str, to: &str) {
    let src_prefix = format!(".yi-agent/attachments/{from}/");
    let dst_prefix = format!(".yi-agent/attachments/{to}/");
    let remap = |p: &mut String| {
        // `strip_prefix` 而不是 `starts_with` + 拼接：`thread-a` 是 `thread-ab`
        // 的前缀，后者不该被改写。
        if let Some(rest) = p.strip_prefix(src_prefix.as_str()) {
            *p = format!("{dst_prefix}{rest}");
        }
    };

    for item in &mut loaded.items {
        match item {
            Item::UserMessage {
                attachments,
                images,
                ..
            }
            | Item::UserInterjection {
                attachments,
                images,
                ..
            } => {
                for a in attachments {
                    remap(&mut a.path);
                }
                for i in images {
                    remap(&mut i.path);
                }
            }
            Item::ToolCall { images, .. } => {
                for i in images {
                    remap(&mut i.path);
                }
            }
            _ => {}
        }
    }

    fn remap_blocks(blocks: &mut [ContentBlock], remap: &impl Fn(&mut String)) {
        for block in blocks {
            match block {
                ContentBlock::Image { path: Some(p), .. } => remap(p),
                ContentBlock::ToolResult { content, .. } => remap_blocks(content, remap),
                _ => {}
            }
        }
    }

    for message in &mut loaded.messages {
        remap_blocks(&mut message.content, &remap);
    }
}

impl ThreadStore {
    pub fn new(workdir: &Path) -> Self {
        Self {
            root: workdir.join(".yi-agent").join("threads"),
            meta_lock: std::sync::Mutex::new(()),
        }
    }

    /// 存储根(`<cwd>/.yi-agent/threads`)。
    ///
    /// 供调用方从 root 反推会话工作区——`thread/delete` 分支拿不到 cwd(`server.rs`
    /// 的 `detach_unused_runtimes` 注释明确警告不要为此改 `store_lookup` 签名),
    /// 附件清理需要它(见设计 §6(d))。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 该 store 所属的工作区（`root()` 是它下面的 `.yi-agent/threads`）。
    ///
    /// 测试与调用方要构造一份「cwd 与 store 对得上」的 meta：直接由 store 反推，
    /// 比再传一遍路径更不容易写错（写错的表现是冷读报「没有可继承的历史」）。
    /// Where this workspace keeps the per-conversation resume credentials.
    ///
    /// Kept beside `threads/` rather than inside a thread directory: a fork copies
    /// a thread wholesale, and a copied credential would let two conversations
    /// claim one daemon container. `fork_thread` therefore never touches it, and a
    /// forked conversation mints its own secret on first use.
    pub fn session_identity_directory(&self) -> PathBuf {
        self.workdir().join(".session-identities")
    }

    pub fn workdir(&self) -> &Path {
        self.root
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(&self.root)
    }

    pub(crate) fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.meta.json"))
    }

    pub(crate) fn log_path(&self, id: &str) -> PathBuf {
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

    /// 从 `from` 物理复制出一个新会话。
    ///
    /// **必须实现在本模块内**：标题截断要用 `title_from`（`pub(crate)`），
    /// 写在 CLI 侧只能要么扩大 API、要么复制一份截断规则。
    ///
    /// 只读源会话——`load` 已在内存里合并了崩溃残留的 partial，不需要也不应该
    /// 对源调 `promote_partial`。
    pub fn fork_thread(&self, from: &str, new_id: &str) -> io::Result<String> {
        if !valid_id(new_id) {
            return Err(invalid_id(new_id));
        }

        // 先取源锁：防止复制到「半轮」内容。拿不到直接失败，不建半个新会话。
        let _src_lock = acquire_thread_lock(&self.root, from).map_err(|e| match e {
            ThreadLockError::Busy(busy) => {
                io::Error::new(io::ErrorKind::WouldBlock, busy.to_string())
            }
            ThreadLockError::Io(err) => err,
        })?;

        // Strict on the source: a fork copies this conversation's messages
        // *and* its sandbox baseline. If the meta cannot be read, the baseline
        // is unknown, and a tolerant read would silently rebuild the meta and
        // hand the copy "no record" — which then activates at the read-only
        // floor at best, or adopts a default the user never chose for this
        // history at worst. Refusing here keeps a corrupt source from being
        // laundered into a fresh, apparently-legitimate conversation.
        let mut loaded = self
            .load_for_activation(from)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("thread {from}")))?;

        let now = now_millis();
        // `title_from` 会把整串再截到 30 字。源标题通常已经是 30 字，直接拼
        // ` (fork)` 会被截掉后缀，副本与原件在侧栏里一模一样。故先给后缀留位置。
        const FORK_SUFFIX: &str = " (fork)";
        const TITLE_LIMIT: usize = 30;
        let title = loaded
            .meta
            .title
            .clone()
            // 老会话可能没有标题：从首条用户消息派生，否则副本会和原件一起显示
            // `(untitled)`，同样分不出谁是谁。
            .or_else(|| first_user_text(&loaded.messages))
            .map(|t| {
                let normalized = t.split_whitespace().collect::<Vec<_>>().join(" ");
                let head: String = normalized
                    .chars()
                    .take(TITLE_LIMIT - FORK_SUFFIX.chars().count())
                    .collect();
                format!("{head}{FORK_SUFFIX}")
            });

        let meta = ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: new_id.to_string(),
            cwd: loaded.meta.cwd.clone(),
            model: loaded.meta.model.clone(),
            created_at: now,
            updated_at: now,
            title,
            permission_mode: loaded.meta.permission_mode,
            // 新会话不继承用户的整理意图
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: loaded.meta.model_ref.clone(),
            reasoning_effort: loaded.meta.reasoning_effort.clone(),
            tag: None,
            preserve_thinking: loaded.meta.preserve_thinking,
            // 历史提到的文件可能在这些根里，不继承则 fork 后工具访问不到。
            // 已消失的目录要滤掉，否则 fork 出来的会话一启动就报错。
            extra_roots: loaded
                .meta
                .extra_roots
                .iter()
                .filter(|p| Path::new(p).is_dir())
                .cloned()
                .collect(),
            // 唯一组长不变量：一个项目只能有一个 lead
            role: None,
            // A fork inherits the source conversation's sandbox, not today's
            // default. The messages being copied were produced under the
            // source's baseline, and widening the sandbox on copy would grant
            // tools access the user never authorized for that history.
            sandbox_baseline: loaded.meta.sandbox_baseline,
            forked_from: Some(from.to_string()),
            content_epoch: 0,
            // 锚点/已读/完成时刻都不能继承：继承 last_item_id 会让桌面端的
            // 锚点校验拒绝 fork 会话的首轮；新会话必须未读、从未完成。
            last_item_id: None,
            anchor_sha: loaded.meta.anchor_sha.clone(),
            last_read_at: 0,
            last_completed_at: 0,
        };
        // 路径重写与目录复制必须一起做：只复制不改引用，工具按旧路径读不到文件。
        rewrite_attachment_paths(&mut loaded, from, new_id);

        self.create(&meta)?;

        // 附件跟会话走同一条生命周期：`thread/delete` 会删掉
        // `.yi-agent/attachments/<tid>/`。不搬过来的话，用户「试完分支、删掉
        // 旧那条」之后，副本里的图片会静默变成空白。
        //
        // cwd 从 store 的 root（`<cwd>/.yi-agent/threads`）上溯两级推出，与
        // `thread/delete` 同一写法：`meta.cwd` 在 meta 缺失/损坏时会被
        // `rebuild_meta` 填成空串，不能用来拼路径。
        if let Some(cwd) = self.root().parent().and_then(|p| p.parent()) {
            let attachments_root = cwd.join(".yi-agent").join("attachments");
            let src_dir = attachments_root.join(from);
            if src_dir.is_dir() {
                let dst_dir = attachments_root.join(new_id);
                copy_attachment_tree(&src_dir, &dst_dir)?;
            }
        }

        // 新会话从诞生起就归本进程
        let _new_lock = acquire_thread_lock(&self.root, new_id).map_err(|e| match e {
            ThreadLockError::Busy(busy) => {
                io::Error::new(io::ErrorKind::WouldBlock, busy.to_string())
            }
            ThreadLockError::Io(err) => err,
        })?;

        // `usage` 与 `usage_total` 都必须给：`rpc/thread.rs:662` 的
        // `if let Some(u) = loaded.usage` 会挡掉 None 时的整个用量通知，
        // 只给 usage_total 的话客户端仍显示 0。
        let totals = loaded.totals;
        let usage_total = TurnUsage {
            model: loaded
                .usage
                .as_ref()
                .map(|u| u.model.clone())
                .unwrap_or_else(|| loaded.meta.model.clone()),
            input_tokens: crate::server::saturate_u32(totals.input_tokens),
            output_tokens: crate::server::saturate_u32(totals.output_tokens),
            cache_creation_input_tokens: crate::server::saturate_u32(
                totals.cache_creation_input_tokens,
            ),
            cache_read_input_tokens: crate::server::saturate_u32(totals.cache_read_input_tokens),
        };
        self.append_turn(
            new_id,
            &TurnLine::Turn {
                items: loaded.items,
                usage: loaded.usage,
                usage_total: Some(usage_total),
                messages: loaded.messages,
            },
        )?;

        Ok(new_id.to_string())
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

    /// 读并拼接主 jsonl：返回 `(items, 最近一条 messages, 最近一条 usage, 累计用量)`。
    /// 文件缺失视为空；损坏行跳过并记 stderr（绝不因此报错）。
    fn read_log(&self, id: &str) -> (Vec<Item>, Vec<Message>, Option<TurnUsage>, UsageTotals) {
        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        let mut totals = UsageTotals::default();
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
                            usage_total: turn_total,
                            messages: turn_messages,
                        }) => {
                            items.extend(turn_items);
                            if !turn_messages.is_empty() {
                                messages = turn_messages;
                            }
                            // 累计优先取本轮累加值；老日志没有就退回该轮最后快照。
                            if let Some(total) = &turn_total {
                                totals.add_turn_usage(total);
                            } else if let Some(last) = &turn_usage {
                                totals.add_turn_usage(last);
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
        (items, messages, usage, totals)
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

    /// 严格读一次，供驱动侧「恢复原会话」使用（spec §7）。
    ///
    /// 与 [`Self::load`] 的区别是**一处都不吞错**：meta 缺失/损坏、ID 或 cwd 与
    /// 期望不符、日志坏行、partial 损坏，全部返回错误。为什么必须这么严：调用方
    /// 要用它恢复一个**真实发生过**的会话并继续合并，一份被静默拼错的历史会让
    /// 模型在失忆状态下操作仓库；宁可阻塞让人来处理。
    ///
    /// 调用方**必须**先持有 `acquire_thread_lock(store.root(), id)` 返回的锁——本方法
    /// 不重复抢锁（同进程会拿到同一把 `Arc`，但跨进程的意义在调用方那一侧）。
    ///
    /// 语义边界（与 spec §7 逐条对齐）：
    /// - 三个文件都不存在 ⇒ `Ok(None)`（这个会话根本没落过盘）。
    /// - meta 存在且合法、log 合法为空 ⇒ 空历史可恢复（合法的空会话）。
    /// - 只有 meta、没有 log 也没有可采纳的 partial ⇒ 拒绝：这包括用户 `/clear`
    ///   后的 meta-only 冷会话，不能拿失忆上下文去合并。
    /// - 有非空 items 却没有可恢复的 messages ⇒ 拒绝：那不是「合法空历史」，
    ///   而是旧记录丢了上下文，续跑会比阻塞更糟。
    /// - 坏 partial 即使主日志完好也拒绝：partial 可能是那一轮唯一的副本，
    ///   静默丢掉等于丢数据。
    ///
    /// 失败时不改写任何文件——测试用 `std::fs::read` 逐字节验证过。
    pub fn restore_for_driver(
        &self,
        id: &str,
        expected_cwd: &Path,
    ) -> io::Result<Option<LoadedThread>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let meta_path = self.meta_path(id);
        let log_path = self.log_path(id);
        let partial_path = self.partial_path(id);

        let meta_text = read_optional_text(&meta_path)?;
        let log_text = read_optional_text(&log_path)?;
        let partial_text = read_optional_text(&partial_path)?;

        // 三个文件都没有：这个会话从未落过盘。
        if meta_text.is_none() && log_text.is_none() && partial_text.is_none() {
            return Ok(None);
        }

        // meta 必须存在且可解析：没有它就没有模型、cwd、权限、关联根等恢复材料，
        // 用 `rebuild_meta` 猜一份等于换了个会话。
        let meta_text = meta_text.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("thread {id} has history but no meta file"),
            )
        })?;
        let meta: ThreadMeta = serde_json::from_str(&meta_text).map_err(io_err)?;
        if meta.thread_id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "thread {id} meta names a different thread: {:?}",
                    meta.thread_id
                ),
            ));
        }

        // cwd 必须精确对上（canonical 比较：同一个目录可以有尾斜杠等不同写法）。
        // 对不上的话恢复出来的会话会在错误的仓库里干活——比不恢复危险得多。
        let want = canonical_or_self(expected_cwd);
        let got = canonical_or_self(Path::new(&meta.cwd));
        if want != got {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "thread {id} lives in {} but was requested for {}",
                    meta.cwd,
                    expected_cwd.display()
                ),
            ));
        }

        // 日志：坏行不跳过（宽松入口才跳），逐行严格解析。
        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        let mut totals = UsageTotals::default();
        if let Some(text) = &log_text {
            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let turn: TurnLine = serde_json::from_str(line).map_err(io_err)?;
                match turn {
                    TurnLine::Turn {
                        items: turn_items,
                        usage: turn_usage,
                        usage_total: turn_total,
                        messages: turn_messages,
                    } => {
                        items.extend(turn_items);
                        if !turn_messages.is_empty() {
                            messages = turn_messages;
                        }
                        if let Some(total) = &turn_total {
                            totals.add_turn_usage(total);
                        } else if let Some(last) = &turn_usage {
                            totals.add_turn_usage(last);
                        }
                        if turn_usage.is_some() {
                            usage = turn_usage;
                        }
                    }
                }
            }
        }

        // partial：存在就必须是合法的，而且要先可靠升格，再交出新 turn。
        let mut pending_turn = false;
        if let Some(text) = &partial_text {
            let partial: PartialTurn = serde_json::from_str(text).map_err(io_err)?;
            if !partial.items.is_empty() {
                // 升格（append 主日志 + 清 partial）。先算是否已收尾：
                // 已收尾只清 partial，绝不重复 append。
                let committed = Self::partial_is_committed(&partial, &items);
                if !committed {
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
                    if let Some(total) = &partial.usage_total {
                        totals.add_turn_usage(total);
                    } else if let Some(last) = &partial.usage {
                        totals.add_turn_usage(last);
                    }
                    if partial.usage.is_some() {
                        usage = partial.usage;
                    }
                    pending_turn = true;
                }
                self.promote_partial_strict(id, committed)?;
            }
        }

        // meta-only（没有 log、没有可采纳的 partial）拒绝：`/clear` 后的冷会话
        // 正属此列，失忆续跑比让用户人工处理更危险。
        if log_text.is_none() && items.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("thread {id} has only meta and no restorable history"),
            ));
        }
        // 有内容却没有任何可恢复的 messages：旧记录丢了上下文。空 log（合法空会话）
        // 不受影响，因为那时 items 本来就是空的。
        if !items.is_empty() && messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("thread {id} has items but no restorable messages"),
            ));
        }

        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
            totals,
            pending_turn,
        }))
    }

    /// partial 的严格升格：append 成功才清；append 失败保留 partial 并报错。
    ///
    /// `committed` 由调用方用**严格读出的**主日志 items 经
    /// [`Self::partial_is_committed`] 算好传入。刻意不在本函数里重读：宽松的
    /// [`Self::read_log`] 会吞掉坏行，用它判断「这轮是否已 append」正是严格入口
    /// 不该做的推断（spec §7「严格读取必须返回真实错误，不 validate 后再调宽松
    /// load」）。判据本身仍与宽松入口共用，故两边语义不会漂移。
    ///
    /// 幂等：`committed == true`（该轮已进主日志，只是上次清 partial 失败）时只
    /// 再清一次，绝不重复 append。
    fn promote_partial_strict(&self, id: &str, committed: bool) -> io::Result<()> {
        let Some(partial) = self.read_partial_strict(id)? else {
            return Ok(());
        };
        if partial.items.is_empty() || committed {
            return self.clear_partial(id);
        }
        self.append_turn(
            id,
            &TurnLine::Turn {
                items: partial.items,
                usage: partial.usage,
                usage_total: partial.usage_total,
                messages: partial.messages,
            },
        )?;
        // append 成功才清；clear 失败时返回错误，调用方不得投递新轮。
        self.clear_partial(id)
    }

    /// Read the meta file, treating only "no such file" as absent.
    ///
    /// Used by the activation paths (`load_for_activation`,
    /// `read_meta_for_activation`) so a hand-edited or partially-written meta
    /// cannot masquerade as a conversation that never had a recorded baseline.
    pub fn read_meta_strict(&self, id: &str) -> io::Result<Option<ThreadMeta>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        match std::fs::read_to_string(self.meta_path(id)) {
            Ok(text) => serde_json::from_str::<ThreadMeta>(&text)
                .map(Some)
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("thread {id} has an unreadable meta: {error}"),
                    )
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Just the meta, for callers that need the baseline before deciding whether
    /// to load a transcript at all. Same strictness as
    /// [`ThreadStore::load_for_activation`].
    pub fn read_meta_for_activation(&self, id: &str) -> io::Result<Option<ThreadMeta>> {
        self.read_meta_strict(id)
    }

    /// 严格读 partial：只有「文件不存在」才返回 `None`，损坏/不可读都是错误。
    fn read_partial_strict(&self, id: &str) -> io::Result<Option<PartialTurn>> {
        match read_optional_text(&self.partial_path(id))? {
            Some(text) => serde_json::from_str(&text).map(Some).map_err(io_err),
            None => Ok(None),
        }
    }

    pub fn load(&self, id: &str) -> io::Result<Option<LoadedThread>> {
        self.load_with_meta_policy(id, MetaPolicy::Tolerant)
    }

    /// Activation read: the same transcript loading as [`ThreadStore::load`], but
    /// a meta file that exists yet cannot be read or parsed is an error instead
    /// of being silently rebuilt from the log.
    ///
    /// The difference matters only for decisions that must not run on a guess.
    /// `load`'s tolerance exists so a corrupt meta never hides a conversation
    /// from the UI; re-opening it to run tools is a different question, and there
    /// "we could not read the record of what this conversation was allowed to
    /// do" must refuse rather than default to *something*.
    ///
    /// A missing meta file is still compatible history (`None` from the meta
    /// reader), not corruption: that is the pre-baseline case the conservative
    /// fallback exists for.
    pub fn load_for_activation(&self, id: &str) -> io::Result<Option<LoadedThread>> {
        self.load_with_meta_policy(id, MetaPolicy::Strict)
    }

    fn load_with_meta_policy(
        &self,
        id: &str,
        policy: MetaPolicy,
    ) -> io::Result<Option<LoadedThread>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let log = self.log_path(id);
        let meta_path = self.meta_path(id);
        if !log.exists() && !meta_path.exists() {
            return Ok(None);
        }

        let (mut items, mut messages, mut usage, mut totals) = self.read_log(id);

        // One read, one decision: the policy below applies to this very meta
        // value, not to a pre-flight probe that a later read could contradict.
        let meta = match policy {
            MetaPolicy::Tolerant => std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
                .unwrap_or_else(|| rebuild_meta(id, &log, &messages)),
            MetaPolicy::Strict => match self.read_meta_strict(id)? {
                Some(meta) => meta,
                // No meta file at all: legacy history, still loadable.
                None => rebuild_meta(id, &log, &messages),
            },
        };

        // 合流 checkpoint：仅当 partial 的首个 item id 尚未出现在主 jsonl
        // （= 该轮未成功收尾）时采纳。首个 item 是本轮提问 `user-<turn_id>`，
        // 全局唯一，足以判断"这轮是否已 append"。
        let mut pending_turn = false;
        if let Some(partial) = self.read_partial(id)
            && !Self::partial_is_committed(&partial, &items)
        {
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
            // 未收尾的轮次同样计入累计（它已经真实发生过）。
            if let Some(total) = &partial.usage_total {
                totals.add_turn_usage(total);
            } else if let Some(last) = &partial.usage {
                totals.add_turn_usage(last);
            }
            if partial.usage.is_some() {
                usage = partial.usage;
            }
            pending_turn = true;
        }

        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
            totals,
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

    /// 组长归属键:项目状态目录 `<cwd>/.yi-agent` 的 canonical 字符串。
    ///
    /// 用 canonical 路径而非 cwd 原串,是因为同一个目录可以有多种写法(尾斜杠、
    /// 符号链接、相对路径),原串比较会让同一项目被判成两个项目、造出两个组长。
    /// canonicalize 失败(目录尚未创建)时退回原串:此时也还没有任何会话,
    /// 键取什么都只会被用一次。
    pub fn project_key(&self) -> String {
        let state_dir = self
            .root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone());
        state_dir
            .canonicalize()
            .unwrap_or(state_dir)
            .display()
            .to_string()
    }

    /// 本目录的组长,按 `created_at` 升序取第一个。
    ///
    /// 取「最早创建」而非「最新」,是为了让懒创建幂等:并发/重复调用无论如何
    /// 都收敛到同一个组长,而不是每次挑一个不同的。
    ///
    /// 只认**合法**组长(见 [`is_lead_meta`]):卡片 worktree 里那份 `role:"lead"`
    /// 是假组长,必须忽略,否则该 worktree 会在侧栏里变成带组长的顶层项目组。
    pub fn find_lead(&self) -> io::Result<Option<ThreadMeta>> {
        let mut leads: Vec<ThreadMeta> = self.list()?.into_iter().filter(is_lead_meta).collect();
        leads.sort_by_key(|a| a.created_at);
        Ok(leads.into_iter().next())
    }

    /// 只读一份 meta(不解析 jsonl 历史)。
    ///
    /// 与 [`load`](Self::load) 的区别:`load` 会把整份对话日志读进来,而只需要
    /// meta 字段(如 `config/read` 冷会话的 `model_ref` / `permission_mode`)的
    /// 调用方不该为此付 O(历史长度) 的代价。
    pub fn read_meta(&self, id: &str) -> io::Result<Option<ThreadMeta>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let _guard = self
            .meta_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let raw = match std::fs::read_to_string(self.meta_path(id)) {
            Ok(text) => text,
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                eprintln!("[app-server] failed to read thread meta ({id}): {e}");
                return Ok(None);
            }
        };
        match serde_json::from_str::<ThreadMeta>(&raw) {
            Ok(meta) => Ok(Some(meta)),
            Err(e) => {
                eprintln!("[app-server] skipping unreadable thread meta ({id}): {e}");
                Ok(None)
            }
        }
    }

    /// 在 store 锁内对 meta 做读-改-写。`Ok(None)` 表示 thread 不存在或 meta 不可读。
    ///
    /// 锁保证 `touch`(driver)与 `rename`(请求循环)不会互相覆盖对方的改动。
    fn update_meta<F: FnOnce(&mut ThreadMeta)>(
        &self,
        id: &str,
        f: F,
    ) -> io::Result<Option<ThreadMeta>> {
        self.update_meta_with(id, |meta| {
            f(meta);
            true
        })
    }

    /// 与 [`ThreadStore::update_meta`] 相同，但由闭包决定**是否落盘**。
    ///
    /// 返回 `false` 表示「本次无事可做」：此时完全不写文件（`rename` 会推进
    /// mtime，让盯着 meta 的 watcher 白重读一次），但仍返回读到的 meta，调用方
    /// 据此拿到当前值。用于幂等操作——重复标记已读、重复记同一个完成时间。
    fn update_meta_with<F: FnOnce(&mut ThreadMeta) -> bool>(
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
        if !f(&mut meta) {
            return Ok(Some(meta));
        }
        let bytes = serde_json::to_vec_pretty(&meta).map_err(io_err)?;
        write_atomic(&path, &bytes)?;
        Ok(Some(meta))
    }

    /// 仅更新合法组长标题。复用 `update_meta` 的锁与原子写路径，拒绝误把普通 thread 改成组长标题。
    pub(crate) fn set_lead_title(&self, id: &str, title: &str) -> io::Result<bool> {
        let mut updated = false;
        self.update_meta(id, |meta| {
            if is_lead_meta(meta) && meta.title.as_deref() != Some(title) {
                meta.title = Some(title.to_string());
                updated = true;
            }
        })?;
        Ok(updated)
    }

    /// 只重写 meta 的 `title` + `updated_at`。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn rename(&self, id: &str, title: &str) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| {
                if !is_lead_meta(meta) {
                    meta.title = Some(title.to_string());
                    meta.updated_at = now_millis();
                }
            })?
            .is_some())
    }

    /// 条件写入标题：仅当当前 `title` 恰好等于 `expected` 时替换为 `new`。
    ///
    /// 用于"LLM 生成标题"覆盖"兜底截断标题"：以兜底值为 CAS 凭据，用户若在两次
    /// 写入之间手动 `rename`，此处便会放弃覆盖——手动改名优先。返回 `Ok(false)`
    /// 表示未替换（标题已变、或 thread 不存在）。
    pub fn set_title_if_unchanged(&self, id: &str, expected: &str, new: &str) -> io::Result<bool> {
        // `update_meta` 的闭包是 `FnOnce`，用 `&mut` 捕获即可在调用后读取是否实际
        // 替换；`Ok(None)`（未知 id / meta 不可读）时闭包未执行，`replaced` 保持 false。
        let mut replaced = false;
        self.update_meta(id, |meta| {
            if !is_lead_meta(meta) && meta.title.as_deref() == Some(expected) {
                meta.title = Some(new.to_string());
                meta.updated_at = now_millis();
                replaced = true;
            }
        })?;
        Ok(replaced)
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

    /// Record the sandbox a conversation runs under.
    ///
    /// Written when activation had to *decide* a baseline rather than read one:
    /// a pre-baseline meta with history gets the conservative read-only floor,
    /// and a meta created outside `thread/start` (the lazily seeded lead) adopts
    /// today's default. Persisting both means the decision is made once and the
    /// conversation never re-enters the "no evidence" path.
    ///
    /// Deliberately does not bump `updated_at`: like `set_permission_mode`, this
    /// is bookkeeping, not conversation activity, and bumping it would reorder
    /// the sidebar on every resume of an old thread.
    pub fn set_sandbox_baseline(
        &self,
        id: &str,
        baseline: crate::session_sandbox::SandboxBaseline,
    ) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| meta.sandbox_baseline = Some(baseline))?
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

    /// 递增该 thread 的**内容世代**。调用方必须是「会改变服务端 item 集合」的
    /// 操作——今天只有 `thread/clear`（成功 `truncate` 之后）。加新调用点前请回到
    /// spec §3 的判据确认，`compact` **不是**这样的操作。
    ///
    /// 有意**不**刷新 `updated_at`：世代变更属内容维护，不应影响按活动时间的
    /// 列表排序（与 `set_permission_mode` 同款）。返回 false 表示 thread 不存在
    /// 或 meta 不可读。
    pub fn bump_content_epoch(&self, id: &str) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| {
                meta.content_epoch = meta.content_epoch.saturating_add(1)
            })?
            .is_some())
    }

    /// 记录该 thread 最后一条 item 的 id（`None` = 该次落盘没有 item）。
    /// 与 `persist_and_finish_turn` 的 `touch` 相邻调用，共用 `meta_lock`。
    pub fn set_last_item_id(&self, id: &str, last: Option<&str>) -> io::Result<()> {
        self.update_meta(id, |meta| meta.last_item_id = last.map(str::to_string))?;
        Ok(())
    }

    /// 该 thread 的日志里是否含某 item id（用于校验客户端上报的上下文锚点）。
    ///
    /// 读真实 `.jsonl` 而非信任 meta 的 `last_item_id` 冗余：冗余由
    /// `persist_and_finish_turn` 维护，`promote_partial` 不维护它，故它可能偏旧；
    /// 而「锚点是否仍在」是正确性判据，必须读权威来源。
    pub fn contains_item_id(&self, id: &str, item_id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let (items, _, _, _) = self.read_log(id);
        Ok(items
            .iter()
            .any(|it| crate::server::item_id(it) == Some(item_id)))
    }

    /// 写入会话的颜色标签（`None` = 清除）。
    ///
    /// 有意**不**刷新 `updated_at`：打标签是用户视图标记而非 thread 活动，不应把
    /// 会话顶到「最近更新」最前面（与 `set_pin_seq` / `set_permission_mode` 同理）。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn set_tag(&self, id: &str, tag: Option<&str>) -> io::Result<bool> {
        Ok(self
            .update_meta(id, |meta| meta.tag = tag.map(str::to_string))?
            .is_some())
    }

    /// 推进共享已读水位到「此刻」，返回 `(last_read_at, last_completed_at)`。
    ///
    /// 单调：取 `max(旧值, now)`，故重复调用、乱序调用、系统时钟回拨都不会让
    /// 已读倒退（倒退会让已经消掉的蓝点重新亮起）。
    ///
    /// 两个值在**同一次**读-改-写里返回：调用方要用它们算
    /// `completed > read`，分两次读会让它在写入与读取之间看到一个自相矛盾的
    /// 组合（例如刚好在两步之间完成了一轮）。
    ///
    /// 有意**不**刷新 `updated_at`：读一个会话不是会话活动，不该把它顶到列表
    /// 最前面（否则「点开看一眼」就会重排侧栏）。
    ///
    /// 返回 `None` 表示 thread 不存在或 meta 不可读。
    pub fn mark_read(&self, id: &str) -> io::Result<Option<(i64, i64)>> {
        let mut watermarks = (0, 0);
        let updated = self.update_meta_with(id, |meta| {
            let next = meta.last_read_at.max(now_millis());
            // 水位没推进（重复点开同一个会话、或时钟没动）⇒ 不落盘。写一次只会
            // 推进 mtime，让盯着 meta 的地方白重读；原子文件本身也没变化。
            let advanced = next != meta.last_read_at;
            meta.last_read_at = next;
            watermarks = (meta.last_read_at, meta.last_completed_at);
            advanced
        })?;
        Ok(updated.map(|_| watermarks))
    }

    /// 记录一次**回合结束**，返回新的完成时间。
    ///
    /// 与 [`mark_read`] 一样不刷新 `updated_at`（`touch` 已经在回合结束时做过
    /// 这件事，这里再写一次只会是无谓的第二次落盘）。`clear` / `compact` 不是
    /// 回合结束，调用方不得走到这里。返回 `None` 表示 thread 不存在。
    ///
    /// `at` 由调用方给，且必须是**随 `turn/completed` 广播出去的那个值**：广播
    /// 发生在落盘之前，若这里另取一次 `now_millis()`，落盘值必然更晚。另一端的
    /// 客户端收到广播就发 `thread/markRead`，写下的已读水位于是低于完成水位，
    /// 服务端判为「读完又跑完一轮」——重连后蓝点复活。两处同源才能闭合这条缝。
    pub fn mark_turn_completed(&self, id: &str, at: i64) -> io::Result<Option<i64>> {
        let mut new_value = 0;
        let updated = self.update_meta_with(id, |meta| {
            // 取 max：`at` 由**广播那一刻**决定，可能比先前记录的值小（时钟回拨，
            // 或同一回合的两条收尾路径都走了一遍）。完成是单调事实，只前进。
            let next = meta.last_completed_at.max(at);
            let advanced = next != meta.last_completed_at;
            meta.last_completed_at = next;
            new_value = meta.last_completed_at;
            advanced
        })?;
        Ok(updated.map(|_| new_value))
    }

    /// 直接把已读水位写成给定值。**仅供测试**模拟「时钟回拨后再标记」。
    #[cfg(test)]
    pub fn set_last_read_at_for_test(&self, id: &str, at: i64) -> io::Result<()> {
        self.update_meta(id, |meta| meta.last_read_at = at)?;
        Ok(())
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

    /// 一次原子写同时落盘会话覆盖(`model_ref`)与**生效串**(`model`)。
    ///
    /// 切模型必须让这两个字段同进同出:分两次 `update_meta` 会给并发 `touch` /
    /// `rename` 留出「ref 已换、生效串还是旧的」的窗口,`thread/list` 便会读到
    /// 自相矛盾的组合。与 [`set_model_ref`] 共用同一把读-改-写锁与原子写路径;
    /// 有意保留两份方法(而非让 `set_model_ref` 委托),因为只改 ref 的旧调用
    /// 语义是「不动生效串」,委托会多读一次 meta 并隐式改写它。
    /// 返回 `Err(NotFound)` 表示 thread 不存在或 meta 不可读。
    pub fn set_model(&self, id: &str, model_ref: Option<&str>, model: &str) -> io::Result<()> {
        let updated = self.update_meta(id, |meta| {
            meta.model_ref = model_ref.map(str::to_string);
            meta.model = model.to_string();
            meta.updated_at = now_millis();
        })?;
        match updated {
            Some(_) => Ok(()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "unknown thread")),
        }
    }

    /// 设置/清除该 thread 的 reasoning effort 覆盖（七档之一）。
    ///
    /// 与 [`set_model_ref`] 同款：走 `update_meta` 的读-改-写锁与原子写路径，避免
    /// 与并发 `touch` / `rename` 互相覆盖。`None` = 清覆盖（回落到条目默认）。
    /// 返回 `Err(NotFound)` 表示 thread 不存在或 meta 不可读。
    pub fn set_reasoning_effort(&self, id: &str, effort: Option<&str>) -> io::Result<()> {
        let updated = self.update_meta(id, |meta| {
            meta.reasoning_effort = effort.map(str::to_string);
            meta.updated_at = now_millis();
        })?;
        match updated {
            Some(_) => Ok(()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "unknown thread")),
        }
    }

    /// 设置该 thread 的 `preserve_thinking`（跨轮 CoT 保留）覆盖。
    ///
    /// 与 [`set_reasoning_effort`] 同款：走 `update_meta` 的读-改-写锁与原子写路径。
    /// 返回 `Err(NotFound)` 表示 thread 不存在或 meta 不可读。
    pub fn set_preserve_thinking(&self, id: &str, preserve: bool) -> io::Result<()> {
        let updated = self.update_meta(id, |meta| {
            meta.preserve_thinking = preserve;
            meta.updated_at = now_millis();
        })?;
        match updated {
            Some(_) => Ok(()),
            None => Err(io::Error::new(io::ErrorKind::NotFound, "unknown thread")),
        }
    }

    /// 每 turn 完成时调用:更新 `updated_at`,并在 `title` 仍为 `None` 时用
    /// `title_hint`(本轮 prompt)填充。
    ///
    /// 返回 `true` = 本次确实写入了标题(即该 thread 的首轮);调用方据此决定是否
    /// 再发起一次 LLM 标题生成。未知 id 或 meta 不可读时静默返回 `Ok(false)`。
    pub fn touch(&self, id: &str, title_hint: Option<&str>) -> io::Result<bool> {
        // `update_meta` 的闭包是 `FnOnce`,因此用 `&mut` 捕获即可在调用后读取是否
        // 实际写入;`Ok(None)`(未知 id / meta 不可读)时闭包未执行,`wrote` 保持 false。
        let mut wrote = false;
        self.update_meta(id, |meta| {
            meta.updated_at = now_millis();
            if !is_lead_meta(meta)
                && meta.title.is_none()
                && let Some(hint) = title_hint
            {
                let t = title_from(hint);
                if !t.is_empty() {
                    meta.title = Some(t);
                    wrote = true;
                }
            }
        })?;
        Ok(wrote)
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

    pub(crate) fn partial_path(&self, id: &str) -> PathBuf {
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
        let (log_items, _, _, _) = self.read_log(id);
        if Self::partial_is_committed(&partial, &log_items) {
            self.clear_partial(id)?;
            return Ok(false);
        }
        self.append_turn(
            id,
            &TurnLine::Turn {
                items: partial.items,
                usage: partial.usage,
                usage_total: partial.usage_total,
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

/// 读一个可选文件：只有 `NotFound` 是「没有」，其余 I/O 错误一律上抛。
///
/// 宽松入口（`read_partial` / `read_log`）用 `.ok()` 把读错压成「没有」；恢复
/// 原会话不能这样——「读不了」和「不存在」在恢复语义下是相反的结论。
pub(crate) fn read_optional_text(path: &Path) -> io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// canonical 比较用：路径不存在或不可 canonical 时退回原样，好让比较仍然确定。
pub(crate) fn canonical_or_self(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// 该串是否可能是一个 thread id。
///
/// 驱动侧在校验 `resume_append` 的绑定时要用它:一个明显不是 id 的串该报
/// `ThreadNotFound`(插件的绑定坏了),而不是等到严格读才以 I/O 错误出现,
/// 那样插件只能按「不确定」处理。
pub(crate) fn is_valid_thread_id(id: &str) -> bool {
    valid_id(id)
}

/// Generated ids look like `thread-<uuid>`; allow only a conservative charset so
/// an id can never name a path outside the store root or a nested file.
pub(crate) fn valid_id(id: &str) -> bool {
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
///
/// `pub(crate)`:LLM 标题生效后,上层需要用它复算兜底标题,以便与 `set_title_if_unchanged`
/// 的 CAS 凭据保持完全一致。
pub(crate) fn title_from(hint: &str) -> String {
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
        plugin_bindings: Vec::new(),
        thread_id: id.to_string(),
        cwd: String::new(),
        model: String::new(),
        created_at: created,
        updated_at: created,
        title: first_user_text(messages),
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        // 缺 meta 时从日志重建:无从得知曾经的开关,按默认「保留」解释。
        preserve_thinking: true,
        extra_roots: Vec::new(),
        role: None,
        content_epoch: 0,
        last_item_id: None,
        anchor_sha: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
        sandbox_baseline: None,
    }
}

// ---------------------------------------------------------------------------
// 跨进程会话写锁
// ---------------------------------------------------------------------------

/// 某个 thread 的写者锁路径：`<root>/<thread-id>.lock`。
///
/// 与 `.jsonl` / `.meta.json` 平级。`list()` 只认 `.meta.json` 后缀，天然跳过它。
pub fn thread_lock_path(root: &Path, thread_id: &str) -> std::path::PathBuf {
    root.join(format!("{thread_id}.lock"))
}

/// 会话已被另一个 writer 持有。
#[derive(Debug)]
pub struct ThreadBusy {
    pub thread_id: String,
}

impl std::fmt::Display for ThreadBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "会话 {} 正被另一个 yi-agent 实例使用", self.thread_id)
    }
}

impl std::error::Error for ThreadBusy {}

/// 取锁失败的两种原因。**必须分开**：磁盘不可写报成「会话被占用」会让用户
/// 永远查不出原因，而「被占用」正是要提示用户去 `fork` 的那种。
#[derive(Debug)]
pub enum ThreadLockError {
    /// 另一个 writer 正持有该会话。
    Busy(ThreadBusy),
    /// 建目录 / 打开锁文件失败。这不是「被占用」，调用方应照常继续。
    Io(std::io::Error),
}

impl std::fmt::Display for ThreadLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(e) => e.fmt(f),
            Self::Io(e) => write!(f, "无法创建会话锁文件: {e}"),
        }
    }
}

impl std::error::Error for ThreadLockError {}

impl From<ThreadBusy> for ThreadLockError {
    fn from(e: ThreadBusy) -> Self {
        Self::Busy(e)
    }
}

/// 持有的会话写锁。`Drop` 时关闭 `File`，内核随之释放 `flock`。
///
/// **锁文件永不删除。** 删掉会让「已打开同一 inode 的竞争者」锁在 unlinked 文件
/// 上，第三个进程新建同名文件后两者都以为持锁（见 `device_store.rs:78` 的同类说明）。
pub struct ThreadWriterLock {
    /// 只为持有 `flock` 而存在。字段名带下划线是因为从不读取。
    _file: std::fs::File,
    path: std::path::PathBuf,
}

impl Drop for ThreadWriterLock {
    fn drop(&mut self) {
        // 从进程级去重表移除自己，下一次 acquire 才会重新走内核 flock。
        if let Some(table) = PROCESS_LOCKS.get()
            && let Ok(mut table) = table.lock()
        {
            // 只有当表里那份**已经失效**时才删：若此时表项还升级得出来，
            // 说明是「表里那份 `Weak` 刚被我们 drop 触发」，删掉它是安全的；
            // 反过来若同路径已被别的 Arc 重新插入，则不能误删。
            let stale = table
                .get(&self.path)
                .map(|w| w.strong_count() == 0)
                .unwrap_or(true);
            if stale {
                table.remove(&self.path);
            }
        }
        // `_file` 在此之后 drop，内核锁随之释放。
    }
}

/// 表里存 `Weak`：存 `Arc` 会让表与锁互相持有，`ThreadWriterLock::drop`
/// 永远不触发——于是 `flock` 永不释放，同一进程内「关掉会话再打开」会
/// 一直被自己挡住（表现为 `WouldBlock`），且每条会话都泄漏一个描述符。
type LockTable = std::sync::Mutex<HashMap<PathBuf, std::sync::Weak<ThreadWriterLock>>>;

/// 进程级去重表：`flock` 按**打开文件描述**加锁，不按路径。
///
/// 同一进程内两次 `open` 同一路径会得到两个描述符，各拿一次锁互不阻塞——于是
/// 「本进程已持有」会被自己判成空闲。表挂在进程级静态量上，与
/// `group_store.rs:120` 的 `static LOCK` 同思路。
static PROCESS_LOCKS: std::sync::OnceLock<LockTable> = std::sync::OnceLock::new();

fn process_locks() -> &'static LockTable {
    PROCESS_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 把 `flock` 失败分类成「被占用」或「真故障」。
///
/// 单独成函数是为了能被**直接测试**：真实路径上的非 Busy 错误（EBADF 之类）
/// 在进程里很难自然制造，而"打印后放行"正是要根除的行为——没有这条可直接驱动
/// 的分类函数，就只能靠测文件 open 失败来假装覆盖，那条路根本走不到 flock。
fn classify_flock_error(error: std::io::Error, thread_id: &str, path: &Path) -> ThreadLockError {
    if matches!(
        error.raw_os_error(),
        Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN
    ) {
        return ThreadLockError::Busy(ThreadBusy {
            thread_id: thread_id.to_string(),
        });
    }
    // 真故障（坏描述符 / 不支持的 fs）不能被误报成「会话被占用」，也**不能
    // 当作拿到锁放行**：放行等于两个进程同时写同一份历史。上抛 `Io`，由调用方
    // 决定降级（原 RPC 调用者的选择不变，driver 恢复路径则必须失败）。
    ThreadLockError::Io(std::io::Error::new(
        error.kind(),
        format!("flock failed on {}: {error}", path.display()),
    ))
}

/// 取某个 thread 的写锁。进程内幂等（同一路径返回同一个 `Arc`）。
///
/// 非阻塞：冲突立即返回 [`ThreadBusy`]，调用方决定是报错还是提示 `fork`。
pub fn acquire_thread_lock(
    root: &Path,
    thread_id: &str,
) -> Result<Arc<ThreadWriterLock>, ThreadLockError> {
    let path = thread_lock_path(root, thread_id);

    // 先查表：本进程已持有则直接返回同一个 Arc。
    // 用 `Weak::upgrade` 而非直接 clone：表项可能已随上一次 drop 失效。
    {
        let mut table = process_locks().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = table.get(&path).and_then(std::sync::Weak::upgrade) {
            return Ok(existing);
        }
        // 失效项顺手清掉，避免表无限增长。
        table.remove(&path);
    }

    std::fs::create_dir_all(root).map_err(ThreadLockError::Io)?;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(ThreadLockError::Io)?;

    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome != 0 {
        return Err(classify_flock_error(
            std::io::Error::last_os_error(),
            thread_id,
            &path,
        ));
    }

    let lock = Arc::new(ThreadWriterLock {
        _file: file,
        path: path.clone(),
    });
    process_locks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path, Arc::downgrade(&lock));
    Ok(lock)
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
            plugin_bindings: Vec::new(),
            thread_id: id.into(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        }
    }

    fn turn(items: Vec<Item>, messages: Vec<Message>) -> TurnLine {
        TurnLine::Turn {
            items,
            usage: None,
            usage_total: None,
            messages,
        }
    }

    // -----------------------------------------------------------------
    // 严格恢复（`restore_for_driver`）：驱动侧恢复原会话的唯一入口。
    // 每一条都断言「失败时不改写文件」，因为静默损坏比报错更难查。
    // -----------------------------------------------------------------

    fn restore_user_item(id: &str, text: &str) -> Item {
        Item::UserMessage {
            id: id.into(),
            text: text.into(),
            attachments: Vec::new(),
            images: Vec::new(),
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    fn dir_meta(id: &str, cwd: &Path) -> ThreadMeta {
        let mut meta = meta(id);
        meta.cwd = cwd.to_string_lossy().into_owned();
        meta
    }

    fn write_meta(s: &ThreadStore, m: &ThreadMeta) {
        write_file(
            &s.meta_path(&m.thread_id),
            serde_json::to_vec(m).unwrap().as_slice(),
        );
    }

    #[test]
    fn driver_restore_rejects_a_corrupt_log_without_rewriting_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        std::fs::create_dir_all(store.root()).unwrap();
        let bad = b"not-json\n";
        std::fs::write(store.log_path("thread-1"), bad).unwrap();
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
        assert_eq!(std::fs::read(store.log_path("thread-1")).unwrap(), bad);
    }

    #[test]
    fn driver_restore_rejects_a_corrupt_log_even_when_meta_is_good() {
        // 关键：meta 合法，坏的是日志。若没有这条，上面那条可能只因「缺 meta」
        // 就通过，等于没测到坏日志。
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        let bad = b"{\"type\":\"turn\",\"items\":[]}\nnot-json\n";
        write_file(&store.log_path("thread-1"), bad);
        let error = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(store.log_path("thread-1")).unwrap(), bad);
    }

    #[test]
    fn driver_restore_returns_none_when_nothing_was_ever_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        assert!(
            store
                .restore_for_driver("thread-1", dir.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn driver_restore_accepts_a_legitimately_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        write_file(&store.log_path("thread-1"), b"");
        let loaded = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap()
            .expect("empty log with meta is a legitimate fresh session");
        assert!(loaded.items.is_empty());
        assert!(!loaded.pending_turn);
    }

    #[test]
    fn driver_restore_round_trips_items_messages_and_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        store
            .append_turn(
                "thread-1",
                &turn(
                    vec![restore_user_item("user-turn-1", "hi")],
                    vec![Message::user("hi")],
                ),
            )
            .unwrap();
        let loaded = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.meta.thread_id, "thread-1");
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.messages, vec![Message::user("hi")]);
    }

    #[test]
    fn driver_restore_rejects_a_missing_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_file(
            &store.log_path("thread-1"),
            br#"{"type":"turn","items":[],"messages":[]}
"#,
        );
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_rejects_a_corrupt_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_file(&store.meta_path("thread-1"), b"{");
        write_file(&store.log_path("thread-1"), b"");
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_rejects_a_meta_for_a_different_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-2", dir.path()));
        write_file(&store.log_path("thread-1"), b"");
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_rejects_a_cwd_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        let other = dir.path().join("other");
        std::fs::create_dir_all(&other).unwrap();
        write_meta(&store, &dir_meta("thread-1", &other));
        write_file(&store.log_path("thread-1"), b"");
        let error = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn driver_restore_rejects_meta_only_sessions() {
        // 用户 `/clear` 之后的冷会话正是这个形状：身份还在、历史没了。
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_rejects_items_without_restorable_messages() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        // 有 item、没有 messages：旧记录丢了上下文。
        store
            .append_turn(
                "thread-1",
                &turn(vec![restore_user_item("user-1", "hi")], vec![]),
            )
            .unwrap();
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_accepts_a_valid_partial_only_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        write_file(&store.log_path("thread-1"), b"");
        store
            .write_partial(
                "thread-1",
                &PartialTurn {
                    turn_id: "t1".into(),
                    items: vec![restore_user_item("user-t1", "hi")],
                    messages: vec![Message::user("hi")],
                    usage: None,
                    usage_total: None,
                },
            )
            .unwrap();

        let loaded = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap()
            .unwrap();
        assert!(loaded.pending_turn, "recovered turn must be flagged");
        assert_eq!(loaded.messages, vec![Message::user("hi")]);
        // 升格后主日志里有了这一轮，partial 已清。
        assert!(!store.partial_path("thread-1").exists());
        let reloaded = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap()
            .unwrap();
        assert!(!reloaded.pending_turn, "second read must not re-append");
        assert_eq!(
            std::fs::read_to_string(store.log_path("thread-1"))
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            1,
            "promotion must be idempotent"
        );
    }

    #[test]
    fn driver_restore_rejects_a_corrupt_partial_even_with_a_good_log() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        store
            .append_turn(
                "thread-1",
                &turn(
                    vec![restore_user_item("user-1", "hi")],
                    vec![Message::user("hi")],
                ),
            )
            .unwrap();
        let bad = b"{ not json";
        write_file(&store.partial_path("thread-1"), bad);
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
        assert_eq!(std::fs::read(store.partial_path("thread-1")).unwrap(), bad);
    }

    #[test]
    fn driver_restore_does_not_deliver_when_promotion_cannot_clear_the_partial() {
        // 让 clear 失败：把 partial 路径换成目录。此时 append 已成功但清不掉
        // checkpoint，必须报错（调用方不得投递新轮）。
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        write_file(&store.log_path("thread-1"), b"");
        let partial = store.partial_path("thread-1");
        std::fs::create_dir_all(&partial).unwrap();
        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
    }

    #[test]
    fn driver_restore_does_not_duplicate_the_append_when_only_the_clear_failed() {
        // 计划要求的那条精确路径：**append 成功、clear 失败**。用 immutable 位让
        // 删除失败（与「partial 是目录」不同，那种在 append 之前就失败了）。
        // 关键断言是重试幂等：第二次读必须认出「这轮已进主日志」，不再 append。
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        write_file(&store.log_path("thread-1"), b"");
        store
            .write_partial(
                "thread-1",
                &PartialTurn {
                    turn_id: "t1".into(),
                    items: vec![restore_user_item("user-t1", "hi")],
                    messages: vec![Message::user("hi")],
                    usage: None,
                    usage_total: None,
                },
            )
            .unwrap();

        let partial = store.partial_path("thread-1");
        let c = std::ffi::CString::new(partial.to_str().unwrap()).unwrap();
        assert_eq!(
            unsafe { libc::chflags(c.as_ptr(), libc::UF_IMMUTABLE) },
            0,
            "the test needs the partial to resist deletion"
        );

        let error = store
            .restore_for_driver("thread-1", dir.path())
            .expect_err("a clear that cannot happen must be an error");
        assert!(
            partial.exists(),
            "the checkpoint must survive a failed clear: {error}"
        );
        // append 确实发生了：主日志现在有整整一轮。
        assert_eq!(
            std::fs::read_to_string(store.log_path("thread-1"))
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            1,
            "the turn must have been promoted before the clear failed"
        );

        // 放开不可变位后重试：判据命中「已收尾」⇒ 只清 partial，绝不重复 append。
        assert_eq!(
            unsafe { libc::chflags(c.as_ptr(), 0) },
            0,
            "the test must restore the partial"
        );
        let reloaded = store
            .restore_for_driver("thread-1", dir.path())
            .unwrap()
            .unwrap();
        assert!(
            !partial.exists(),
            "the retry must finally clear the checkpoint"
        );
        assert_eq!(
            std::fs::read_to_string(store.log_path("thread-1"))
                .unwrap()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            1,
            "a retry must never append the same turn twice"
        );
        // 第一次读采纳了真实内容（pending_turn 标记它曾是未收尾轮）。
        assert!(reloaded.messages.iter().any(|m| m == &Message::user("hi")));
    }

    #[test]
    fn driver_restore_leaves_the_partial_when_the_append_fails() {
        // append 失败（主日志路径是个目录）时，partial 是这一轮唯一的副本，
        // 必须原样留着：既不清、也不报成功。
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        write_meta(&store, &dir_meta("thread-1", dir.path()));
        std::fs::create_dir_all(store.log_path("thread-1")).unwrap();
        store
            .write_partial(
                "thread-1",
                &PartialTurn {
                    turn_id: "t1".into(),
                    items: vec![restore_user_item("user-t1", "hi")],
                    messages: vec![Message::user("hi")],
                    usage: None,
                    usage_total: None,
                },
            )
            .unwrap();
        let before = std::fs::read(store.partial_path("thread-1")).unwrap();

        assert!(store.restore_for_driver("thread-1", dir.path()).is_err());
        assert_eq!(
            std::fs::read(store.partial_path("thread-1")).unwrap(),
            before,
            "a failed append must not consume the only copy of the turn"
        );
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
            attachments: Vec::new(),
            images: Vec::new(),
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
                plugin_bindings: Vec::new(),
                thread_id: bad.into(),
                role: None,
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
    fn read_meta_returns_the_meta_without_parsing_the_log() {
        let (_dir, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_model("thread-a", Some("Session Only"), "deepseek-x")
            .unwrap();
        s.set_permission_mode("thread-a", ThreadMode::Yolo).unwrap();

        let read = s.read_meta("thread-a").unwrap().expect("meta must exist");
        assert_eq!(read.model_ref.as_deref(), Some("Session Only"));
        assert_eq!(read.model, "deepseek-x");
        assert_eq!(read.permission_mode, ThreadMode::Yolo);

        assert!(s.read_meta("thread-missing").unwrap().is_none());
        assert!(
            s.read_meta("../escape").is_err(),
            "invalid ids must not touch the fs"
        );
    }

    #[test]
    fn set_lead_title_does_not_change_a_regular_thread() {
        let (_dir, s) = store();
        let mut regular = meta("thread-regular-title");
        regular.title = Some("regular title".into());
        s.create(&regular).unwrap();

        assert!(
            !s.set_lead_title("thread-regular-title", "forced lead title")
                .unwrap()
        );
        assert_eq!(
            s.load("thread-regular-title")
                .unwrap()
                .unwrap()
                .meta
                .title
                .as_deref(),
            Some("regular title")
        );
    }

    #[test]
    fn lead_title_write_entry_points_preserve_the_project_title() {
        let (dir, s) = store();
        let basename = dir.path().file_name().unwrap().to_str().unwrap();
        let title = format!("{basename} 组长");
        let mut lead = meta("thread-lead-title");
        lead.cwd = dir.path().display().to_string();
        lead.title = Some(title.clone());
        lead.role = Some(LEAD_ROLE.to_string());
        s.create(&lead).unwrap();

        assert!(!s.touch("thread-lead-title", Some("prompt")).unwrap());
        // The expected value deliberately matches the stored lead title: the role guard,
        // not just the ordinary CAS mismatch, must prevent the LLM title write.
        assert!(
            !s.set_title_if_unchanged("thread-lead-title", &title, "LLM title")
                .unwrap()
        );
        assert!(s.rename("thread-lead-title", "manual title").unwrap());
        assert_eq!(
            s.load("thread-lead-title")
                .unwrap()
                .unwrap()
                .meta
                .title
                .as_deref(),
            Some(title.as_str())
        );
    }

    #[test]
    fn lead_touch_does_not_fill_an_empty_title_from_the_prompt() {
        let (dir, s) = store();
        let mut lead = meta("thread-lead-empty-title");
        lead.cwd = dir.path().display().to_string();
        lead.role = Some(LEAD_ROLE.to_string());
        s.create(&lead).unwrap();

        assert!(!s.touch("thread-lead-empty-title", Some("prompt")).unwrap());
        assert_eq!(
            s.load("thread-lead-empty-title")
                .unwrap()
                .unwrap()
                .meta
                .title,
            None
        );
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
    fn set_title_if_unchanged_replaces_on_match() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.touch("thread-a", Some("placeholder")).unwrap();
        let replaced = s
            .set_title_if_unchanged("thread-a", "placeholder", "智能标题")
            .unwrap();
        assert!(replaced);
        let m = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(m.title.as_deref(), Some("智能标题"));
        // 覆盖也是一次标题变更：必须刷新 updated_at，与相邻的 touch 测试同标准。
        assert!(m.updated_at > 1, "updated_at must be bumped on replace");
    }

    #[test]
    fn set_title_if_unchanged_yields_to_concurrent_rename() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.touch("thread-a", Some("placeholder")).unwrap();
        // 用户抢先改名：当前标题不再是 expected，CAS 必须放弃覆盖。
        s.rename("thread-a", "user title").unwrap();
        let replaced = s
            .set_title_if_unchanged("thread-a", "placeholder", "智能标题")
            .unwrap();
        assert!(!replaced);
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("user title")
        );
    }

    #[test]
    fn set_title_if_unchanged_unknown_id_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_title_if_unchanged("nope", "x", "y").unwrap());
    }

    #[test]
    fn touch_sets_title_only_when_absent_and_bumps_updated_at() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let wrote = s.touch("thread-a", Some("  first   message  ")).unwrap();
        assert!(wrote, "first touch must report that it wrote a title");
        let m = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(m.title.as_deref(), Some("first message"));
        assert!(m.updated_at > 1, "updated_at must be bumped");

        let wrote_again = s.touch("thread-a", Some("ignored")).unwrap();
        assert!(!wrote_again, "an existing title must not be overwritten");
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
        assert!(
            !s.touch("nope", Some("x"))
                .expect("unknown id must not error")
        );
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
                    attachments: Vec::new(),
                    images: Vec::new(),
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
    fn totals_sum_usage_total_across_turns() {
        let (_dir, s) = store();
        s.create(&meta("t1")).unwrap();
        let with_totals = |totals: u32, last: u32| TurnLine::Turn {
            items: Vec::new(),
            usage: Some(TurnUsage {
                model: "m".into(),
                input_tokens: last,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            usage_total: Some(TurnUsage {
                model: "m".into(),
                input_tokens: totals,
                output_tokens: 2,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 4,
            }),
            messages: Vec::new(),
        };
        s.append_turn("t1", &with_totals(100, 50)).unwrap();
        s.append_turn("t1", &with_totals(200, 60)).unwrap();
        let loaded = s.load("t1").unwrap().unwrap();
        assert_eq!(loaded.totals.input_tokens, 300);
        assert_eq!(loaded.totals.output_tokens, 4);
        assert_eq!(loaded.totals.cache_creation_input_tokens, 6);
        assert_eq!(loaded.totals.cache_read_input_tokens, 8);
        // `usage` 语义不变：仍是最后一轮的最后一次快照。
        assert_eq!(loaded.usage.unwrap().input_tokens, 60);
    }

    #[test]
    fn totals_fall_back_to_last_snapshot_when_usage_total_missing() {
        // 老日志没有 `usage_total`：只能拿该行的 `usage` 近似。
        let (dir, s) = store();
        s.create(&meta("t2")).unwrap();
        let line = r#"{"type":"turn","items":[],"usage":{"model":"m","input_tokens":7,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"messages":[]}"#;
        std::fs::write(
            dir.path().join(".yi-agent/threads/t2.jsonl"),
            format!("{line}\n"),
        )
        .unwrap();
        let loaded = s.load("t2").unwrap().unwrap();
        assert_eq!(loaded.totals.input_tokens, 7);
        assert_eq!(loaded.totals.output_tokens, 2);
    }

    #[test]
    fn totals_ignore_rows_without_usage() {
        let (_dir, s) = store();
        s.create(&meta("t3")).unwrap();
        s.append_turn(
            "t3",
            &TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: Vec::new(),
            },
        )
        .unwrap();
        let loaded = s.load("t3").unwrap().unwrap();
        assert!(loaded.totals.is_empty());
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
    fn set_tag_sets_value_without_bumping_updated_at() {
        let (_d, s) = store();
        let mut m = meta("thread-a");
        m.updated_at = 7;
        s.create(&m).unwrap();
        assert!(s.set_tag("thread-a", Some("red")).unwrap());
        let got = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(got.tag.as_deref(), Some("red"));
        assert_eq!(got.updated_at, 7, "打标签不得刷新 updated_at");
    }

    #[test]
    fn set_tag_none_clears_it() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_tag("thread-a", Some("blue")).unwrap();
        assert!(s.set_tag("thread-a", None).unwrap());
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.tag, None);
    }

    #[test]
    fn set_tag_unknown_id_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_tag("nope", Some("red")).unwrap());
    }

    #[test]
    fn legacy_meta_without_tag_deserializes_to_none() {
        // 严格解析：直接对 **类型** `ThreadMeta` 断言，**绕过** `load()`。
        //
        // 为什么不能只靠下方的盘上往返断言：`load()` 反序列化失败时会回落
        // `rebuild_meta`，而后者硬编码 `tag: None`。任何使 `from_str::<ThreadMeta>`
        // 失败的变化都会让 `load()` 悄悄改走兜底路径，「读回 tag == None」照旧成立
        // —— 该断言对反序列化回归完全失明。已用探针证实：把必填的 `model` 挖掉后，
        // 严格解析报错，而 `load(...).meta.tag` 仍为 `None`。
        //
        // 这份 JSON 省略**全部** `#[serde(default)]` 字段，即最严苛的「旧 meta」形状。
        let legacy_json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let legacy: ThreadMeta = serde_json::from_str(legacy_json)
            .expect("旧 meta（缺所有后加字段）必须能被 ThreadMeta 直接反序列化");
        assert_eq!(legacy.tag, None, "旧 meta 缺 tag 字段必须回 None（零迁移）");

        // 对照断言：下面两个字段**没有** serde 对 `Option` 的隐式兜底，其
        // `#[serde(default)]` 是真能失败的 ——
        //   * `permission_mode: ThreadMode`（非 `Option`）：摘掉属性后上面这句
        //     `from_str` 直接报 `missing field permission_mode`；
        //   * `preserve_thinking: bool` 用了 `default = "default_true"`：删掉属性会
        //     静默变 `false`，本断言随即可失败。
        // 已实证：摘掉 `permission_mode` 的 `#[serde(default)]` 后本测试失败；而 `tag`
        // 那条（`Option`）断言不会失败 —— serde derive 的 `missing_field` 对缺字段的
        // `Option<T>` 直接给 `None`（serde-1.0.229 `src/private/de.rs`）。
        assert_eq!(
            legacy.permission_mode,
            ThreadMode::Normal,
            "旧 meta 缺 permission_mode 必须回 Normal（零迁移）"
        );
        assert!(
            legacy.preserve_thinking,
            "旧 meta 缺 preserve_thinking 必须回 true（零迁移）"
        );

        // 互补属性（保留）：同一份旧 JSON 落盘后经 `load()` 读回仍为 None。
        let (_d, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        // 旧格式：没有 tag 字段。
        let legacy = r#"{"thread_id":"thread-a","cwd":"/tmp","model":"m",
            "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal"}"#;
        std::fs::write(s.meta_path("thread-a"), legacy).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.tag, None);
    }

    #[test]
    fn is_valid_tag_accepts_the_six_ids_only() {
        for id in TAG_IDS {
            assert!(is_valid_tag(id), "{id} 必须是合法标签");
        }
        assert!(!is_valid_tag("gray"), "不含灰色");
        assert!(!is_valid_tag("RED"), "大小写敏感");
        assert!(!is_valid_tag(""));
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
        pairs.sort_by_key(|x| std::cmp::Reverse(x.1));
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

    #[test]
    fn fork_copies_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-src");
        meta.title = Some("原始标题".into());
        store.create(&meta).unwrap();

        let messages = vec![yi_agent_core::Message {
            role: yi_agent_core::Role::User,
            content: vec![yi_agent_core::ContentBlock::Text("hi".into())],
        }];
        store
            .append_turn(
                "thread-src",
                &TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "u1".into(),
                        text: "hi".into(),
                        attachments: Vec::new(),
                        images: Vec::new(),
                    }],
                    usage: None,
                    usage_total: None,
                    messages: messages.clone(),
                },
            )
            .unwrap();

        let new_id = store.fork_thread("thread-src", "thread-forked").unwrap();
        assert_eq!(new_id, "thread-forked");

        let loaded = store.load("thread-forked").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1, "items 必须复制过去");
        assert_eq!(loaded.messages.len(), 1, "messages 必须复制过去");
        assert_eq!(loaded.meta.forked_from.as_deref(), Some("thread-src"));
        assert_eq!(
            loaded.meta.title.as_deref(),
            Some("原始标题 (fork)"),
            "标题要标出 fork 来源，否则列表里两条一模一样"
        );
    }

    /// 源标题本身已是 30 字上限时，直接拼 ` (fork)` 会被 `title_from` 再截一次，
    /// 后缀整个丢掉——副本和原会话在侧栏里一模一样。
    #[test]
    fn fork_keeps_the_marker_when_the_source_title_is_at_the_limit() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-long-title");
        meta.title = Some("一二三四五六七八九十一二三四五六七八九十一二三四五六七八九十".into());
        store.create(&meta).unwrap();
        store
            .append_turn(
                "thread-long-title",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store
            .fork_thread("thread-long-title", "thread-long-forked")
            .unwrap();
        let title = store
            .load("thread-long-forked")
            .unwrap()
            .unwrap()
            .meta
            .title
            .unwrap();

        assert!(
            title.ends_with(" (fork)"),
            "fork 标记必须保留，否则侧栏里两条一模一样: {title}"
        );
        assert!(
            title.chars().count() <= 30,
            "标题仍要遵守 30 字上限: {title}"
        );
    }

    /// 老会话可能根本没有标题（meta 里 `title` 是 None）。此时不能直接报 None，
    /// 否则副本和原件都显示 `(untitled)`，同样分不出谁是谁。
    #[test]
    fn fork_falls_back_to_the_first_user_message_for_an_untitled_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let store_meta = sample_meta("thread-untitled");
        store.create(&store_meta).unwrap();
        store
            .append_turn(
                "thread-untitled",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: vec![yi_agent_core::Message {
                        role: yi_agent_core::Role::User,
                        content: vec![yi_agent_core::ContentBlock::Text("帮我改仓库".into())],
                    }],
                },
            )
            .unwrap();

        store
            .fork_thread("thread-untitled", "thread-untitled-forked")
            .unwrap();
        let title = store
            .load("thread-untitled-forked")
            .unwrap()
            .unwrap()
            .meta
            .title
            .expect("无标题时也要从首条用户消息派生一个");

        assert_eq!(title, "帮我改仓库 (fork)");
    }

    #[test]
    fn fork_clears_fields_that_would_break_the_desktop() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-src2");
        meta.last_item_id = Some("stale-item".into());
        meta.last_read_at = 1_700_000_000_000;
        meta.last_completed_at = 1_700_000_000_000;
        meta.role = Some(LEAD_ROLE.to_string());
        meta.content_epoch = 7;
        store.create(&meta).unwrap();
        store
            .append_turn(
                "thread-src2",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store.fork_thread("thread-src2", "thread-forked2").unwrap();
        let loaded = store.load("thread-forked2").unwrap().unwrap();

        assert_eq!(
            loaded.meta.last_item_id, None,
            "继承 last_item_id 会让桌面端锚点校验直接拒绝 fork 会话的首轮"
        );
        assert_eq!(loaded.meta.last_read_at, 0, "新会话必须是未读");
        assert_eq!(loaded.meta.last_completed_at, 0, "新会话从未完成过");
        assert_eq!(
            loaded.meta.role, None,
            "不能继承 lead，否则一个项目两个组长"
        );
        assert_eq!(loaded.meta.content_epoch, 0, "新会话的内容世代从头算");
    }

    #[test]
    fn fork_does_not_touch_the_source_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-src3")).unwrap();
        store
            .append_turn(
                "thread-src3",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        let log = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join("thread-src3.jsonl");
        let before = std::fs::metadata(&log).unwrap().modified().unwrap();
        let before_len = std::fs::metadata(&log).unwrap().len();

        store.fork_thread("thread-src3", "thread-forked3").unwrap();

        let after = std::fs::metadata(&log).unwrap();
        assert_eq!(after.len(), before_len, "fork 是只读操作，不能改源日志");
        assert_eq!(after.modified().unwrap(), before, "源日志 mtime 不能变");
    }

    #[test]
    fn fork_carries_usage_totals() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-src4")).unwrap();
        store
            .append_turn(
                "thread-src4",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: Some(TurnUsage {
                        model: "m".into(),
                        input_tokens: 100,
                        output_tokens: 50,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    usage_total: Some(TurnUsage {
                        model: "m".into(),
                        input_tokens: 100,
                        output_tokens: 50,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }),
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store.fork_thread("thread-src4", "thread-forked4").unwrap();
        let loaded = store.load("thread-forked4").unwrap().unwrap();
        assert_eq!(
            loaded.totals.input_tokens, 100,
            "累计用量必须跟着过去，否则面板归零"
        );
        assert!(
            loaded.usage.is_some(),
            "usage 必须是 Some——rpc/thread.rs:662 的 if let Some(u) 会挡掉 None 的整个通知"
        );
    }

    /// `thread/delete` 会把 `.yi-agent/attachments/<tid>/` 整个删掉。fork 若不
    /// 把附件搬过去，用户「试完分支、删掉旧那条」之后副本里的图片全变空白。
    #[test]
    fn fork_copies_the_attachment_directory_and_rewrites_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-with-files");
        meta.cwd = dir.path().display().to_string();
        store.create(&meta).unwrap();

        let src_dir = dir.path().join(".yi-agent/attachments/thread-with-files");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("deadbeef-a.png"), b"png-bytes").unwrap();
        // 分片上传的暂存区：未 commit 的半成品，不该跟着副本走。
        std::fs::create_dir_all(src_dir.join(".tmp")).unwrap();
        std::fs::write(src_dir.join(".tmp/upload-1"), b"partial").unwrap();

        let doc = crate::protocol::Attachment {
            name: "a.png".into(),
            path: ".yi-agent/attachments/thread-with-files/deadbeef-a.png".into(),
            mime: Some("image/png".into()),
            size: 9,
        };
        let image = crate::protocol::ImageRef {
            path: ".yi-agent/attachments/thread-with-files/deadbeef-a.png".into(),
            media_type: "image/png".into(),
            size: 9,
            detail: None,
        };
        store
            .append_turn(
                "thread-with-files",
                &TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "u1".into(),
                        text: "看这张图".into(),
                        attachments: vec![doc],
                        images: vec![image],
                    }],
                    usage: None,
                    usage_total: None,
                    messages: vec![yi_agent_core::Message {
                        role: yi_agent_core::Role::User,
                        content: vec![yi_agent_core::ContentBlock::Image {
                            source: yi_agent_core::ImageSource::Url("x".into()),
                            detail: Default::default(),
                            path: Some(
                                ".yi-agent/attachments/thread-with-files/deadbeef-a.png".into(),
                            ),
                        }],
                    }],
                },
            )
            .unwrap();

        store
            .fork_thread("thread-with-files", "thread-files-forked")
            .unwrap();

        let dst_dir = dir.path().join(".yi-agent/attachments/thread-files-forked");
        assert!(
            dst_dir.join("deadbeef-a.png").is_file(),
            "附件本体必须复制过去"
        );
        assert!(
            !dst_dir.join(".tmp").exists(),
            "分片上传的暂存区不该跟着副本走"
        );
        assert!(
            src_dir.join("deadbeef-a.png").is_file(),
            "源附件必须原样保留"
        );

        let loaded = store.load("thread-files-forked").unwrap().unwrap();
        let want = ".yi-agent/attachments/thread-files-forked/deadbeef-a.png";
        match &loaded.items[0] {
            crate::protocol::Item::UserMessage {
                attachments,
                images,
                ..
            } => {
                assert_eq!(attachments[0].path, want, "文档路径要指到新目录");
                assert_eq!(images[0].path, want, "图片引用同样要改");
            }
            other => panic!("unexpected item: {other:?}"),
        }
        match &loaded.messages[0].content[0] {
            yi_agent_core::ContentBlock::Image { path, .. } => {
                assert_eq!(path.as_deref(), Some(want), "转录里的图片路径也要改");
            }
            other => panic!("unexpected block: {other:?}"),
        }
    }

    /// 前缀比较必须按**整段目录名**，不能用 starts_with 直接拼：`thread-a` 是
    /// `thread-ab` 的前缀，误伤会把另一个会话的引用改到自己头上。
    #[test]
    fn fork_does_not_rewrite_paths_that_merely_share_the_id_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-a");
        meta.cwd = dir.path().display().to_string();
        store.create(&meta).unwrap();

        let foreign = ".yi-agent/attachments/thread-ab/beef-b.png".to_string();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![crate::protocol::Item::UserMessage {
                        id: "u1".into(),
                        text: "x".into(),
                        attachments: vec![crate::protocol::Attachment {
                            name: "b.png".into(),
                            path: foreign.clone(),
                            mime: None,
                            size: 1,
                        }],
                        images: Vec::new(),
                    }],
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store.fork_thread("thread-a", "thread-a-forked").unwrap();
        let loaded = store.load("thread-a-forked").unwrap().unwrap();
        match &loaded.items[0] {
            crate::protocol::Item::UserMessage { attachments, .. } => {
                assert_eq!(
                    attachments[0].path, foreign,
                    "只共享前缀的别的会话路径不能被改写"
                );
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    /// 源没有附件目录时不该凭空建一个空目录。
    /// `ToolCall.images` 与 `UserInterjection` 也要走重写：漏掉任一条，
    /// 副本里那些图片就会指回源会话的目录，源会话被删后变成空白。
    #[test]
    fn fork_rewrites_tool_call_and_interjection_image_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-mixed");
        meta.cwd = dir.path().display().to_string();
        store.create(&meta).unwrap();

        let old = ".yi-agent/attachments/thread-mixed/x.png".to_string();
        let want = ".yi-agent/attachments/thread-mixed-forked/x.png".to_string();
        let img = || crate::protocol::ImageRef {
            path: old.clone(),
            media_type: "image/png".into(),
            size: 1,
            detail: None,
        };
        store
            .append_turn(
                "thread-mixed",
                &TurnLine::Turn {
                    items: vec![
                        crate::protocol::Item::ToolCall {
                            id: "c1".into(),
                            call_id: "c1".into(),
                            name: "read_image".into(),
                            input: serde_json::json!({}),
                            status: crate::protocol::ToolStatus::Completed,
                            result: None,
                            images: vec![img()],
                        },
                        crate::protocol::Item::UserInterjection {
                            id: "u2".into(),
                            text: "再看这张".into(),
                            attachments: vec![crate::protocol::Attachment {
                                name: "x.png".into(),
                                path: old.clone(),
                                mime: None,
                                size: 1,
                            }],
                            images: vec![img()],
                        },
                    ],
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store
            .fork_thread("thread-mixed", "thread-mixed-forked")
            .unwrap();
        let loaded = store.load("thread-mixed-forked").unwrap().unwrap();

        match &loaded.items[0] {
            crate::protocol::Item::ToolCall { images, .. } => {
                assert_eq!(images[0].path, want, "ToolCall.images 必须重写");
            }
            other => panic!("unexpected item: {other:?}"),
        }
        match &loaded.items[1] {
            crate::protocol::Item::UserInterjection {
                attachments,
                images,
                ..
            } => {
                assert_eq!(attachments[0].path, want, "插话的附件也要重写");
                assert_eq!(images[0].path, want, "插话的图片也要重写");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn fork_creates_no_attachment_dir_when_the_source_has_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let mut meta = sample_meta("thread-no-files");
        meta.cwd = dir.path().display().to_string();
        store.create(&meta).unwrap();
        store
            .append_turn(
                "thread-no-files",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        store
            .fork_thread("thread-no-files", "thread-no-files-forked")
            .unwrap();

        assert!(
            !dir.path()
                .join(".yi-agent/attachments/thread-no-files-forked")
                .exists(),
            "没有附件就不该建空目录"
        );
    }

    #[test]
    fn fork_rejects_an_unknown_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let err = store.fork_thread("thread-nope", "thread-new").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// A corrupt source must not be laundered into a clean-looking copy. With a
    /// tolerant read the meta would be rebuilt and the copy would claim "no
    /// baseline", which activation would then have to treat as unknown history.
    #[test]
    fn fork_refuses_a_corrupt_source_meta() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-corrupt-src"),
            r#"{"thread_id":"thread-corrupt-src","cwd":"/tmp","model":"m",
                "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal",
                "sandbox_baseline":{"mode":"not-a-mode","promotable":false}}"#,
        )
        .unwrap();
        let error = s
            .fork_thread("thread-corrupt-src", "thread-corrupt-copy")
            .expect_err("a corrupt source must not be forked");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            !s.meta_path("thread-corrupt-copy").exists(),
            "a refused fork must not leave a half-created conversation behind"
        );
    }

    fn sample_meta(id: &str) -> ThreadMeta {
        ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: id.to_string(),
            cwd: "/tmp".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        }
    }

    fn user_item(id: &str, text: &str) -> crate::protocol::Item {
        crate::protocol::Item::UserMessage {
            id: id.into(),
            text: text.into(),
            attachments: Vec::new(),
            images: Vec::new(),
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
                    usage_total: None,
                    messages: vec![],
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert!(!loaded.pending_turn, "no partial file ⇒ nothing pending");
    }

    /// `Item::Thinking` 走既有 item 机制落盘/回放：append → load 后逐字等价，
    /// 且磁盘上的 JSONL **不含**任何凭证（signature / encrypted_content）。
    #[test]
    fn thinking_item_round_trips_and_stays_credential_free() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        let thinking = crate::protocol::Item::Thinking {
            id: "item-turn1-2".into(),
            text: "cot summary".into(),
            redacted: false,
        };
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "hi"), thinking.clone()],
                    usage: None,
                    usage_total: None,
                    messages: vec![],
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(
            loaded.items.last(),
            Some(&thinking),
            "thinking item must replay verbatim after append + load"
        );

        // 磁盘上不得出现凭证字段：展示条目只带明文与 redacted 位。
        let log =
            std::fs::read_to_string(dir.path().join(".yi-agent/threads/thread-a.jsonl")).unwrap();
        assert!(log.contains("\"type\":\"thinking\""), "got: {log}");
        assert!(
            !log.contains("signature") && !log.contains("encrypted_content"),
            "persisted thinking item must never carry a credential: {log}"
        );
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
                    usage_total: None,
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
                    usage_total: None,
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
                    usage_total: None,
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
                    usage_total: None,
                },
            )
            .unwrap();

        let loaded = store.load("thread-a").unwrap().unwrap();
        assert_eq!(
            loaded.items.len(),
            1,
            "must not duplicate the committed turn"
        );
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
                    usage_total: None,
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
                    usage_total: None,
                },
            )
            .unwrap();

        // 采纳：追加成主 jsonl 的一轮，并清掉 partial。
        assert!(store.promote_partial("thread-a").unwrap());
        assert!(
            !dir.path()
                .join(".yi-agent/threads/thread-a.partial.json")
                .exists()
        );

        // 冷 load：崩溃轮的内容在、只一份、上下文仍是崩溃轮的。
        let loaded = store.load("thread-a").unwrap().unwrap();
        let ids: Vec<String> = loaded
            .items
            .iter()
            .map(|i| crate::server::item_id(i).unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["user-t1", "user-turn-t2", "item-turn-t2-1"]);
        assert!(!loaded.pending_turn);
        assert_eq!(
            loaded.messages,
            vec![yi_agent_core::Message::user("carried")]
        );

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
                    usage_total: None,
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
                    usage_total: None,
                },
            )
            .unwrap();

        // 判据与 load 一致：首 item id 已在 jsonl ⇒ 不重复 append，只清残留。
        assert!(!store.promote_partial("thread-a").unwrap());
        assert!(
            !dir.path()
                .join(".yi-agent/threads/thread-a.partial.json")
                .exists()
        );
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
                    usage_total: None,
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
        assert!(
            !dir.path()
                .join(".yi-agent/threads/thread-a.partial.json")
                .exists()
        );
        // 幂等：再删一次仍成功。
        store.clear_partial("thread-a").unwrap();
    }

    #[test]
    fn delete_removes_the_partial_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .write_partial("thread-a", &PartialTurn::default())
            .unwrap();
        assert!(store.delete("thread-a").unwrap());
        assert!(
            !dir.path()
                .join(".yi-agent/threads/thread-a.partial.json")
                .exists()
        );
    }

    #[test]
    fn truncate_removes_the_partial_checkpoint() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-a")).unwrap();
        store
            .append_turn(
                "thread-a",
                &TurnLine::Turn {
                    items: vec![user_item("user-t1", "hi")],
                    usage: None,
                    usage_total: None,
                    messages: vec![],
                },
            )
            .unwrap();
        store
            .write_partial("thread-a", &PartialTurn::default())
            .unwrap();
        store.truncate("thread-a").unwrap();
        assert!(
            !dir.path()
                .join(".yi-agent/threads/thread-a.partial.json")
                .exists()
        );
    }
    #[test]
    fn a_meta_without_driver_fields_defaults_to_none() {
        let json = r#"{"thread_id":"t1","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let meta: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(meta.driver_project, None, "旧 meta 缺字段必须回 None");
        assert_eq!(meta.driver_ref, None);
    }

    #[test]
    fn driver_fields_round_trip() {
        let meta = ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: "t1".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 2,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: Some("/proj".into()),
            driver_ref: Some("c1".into()),
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        };
        let text = serde_json::to_string(&meta).unwrap();
        let back: ThreadMeta = serde_json::from_str(&text).unwrap();
        assert_eq!(back.driver_project.as_deref(), Some("/proj"));
        assert_eq!(back.driver_ref.as_deref(), Some("c1"));
    }

    /// 字段名通用化是**零迁移**的:磁盘上按旧键(`board_project`/`card_id`)写的 meta
    /// 仍必须读进新字段——否则升级后所有既有驱动会话都会失去归属,侧栏归组与
    /// worktree 判定一并退化。`#[serde(alias = …)]` 是这条保证的唯一实现。
    #[test]
    fn legacy_meta_written_with_the_old_keys_loads_into_the_new_fields() {
        let json = r#"{"thread_id":"t1","cwd":"/w","model":"m","created_at":1,
            "updated_at":2,"title":null,"board_project":"/proj","card_id":"c1"}"#;
        let meta: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(
            meta.driver_project.as_deref(),
            Some("/proj"),
            "旧键 board_project 必须读进 driver_project"
        );
        assert_eq!(
            meta.driver_ref.as_deref(),
            Some("c1"),
            "旧键 card_id 必须读进 driver_ref"
        );
    }

    /// 新键是**唯一**的写出形态:不再双写旧键(单一名字,不留两种盘上表示)。
    #[test]
    fn the_new_keys_are_the_only_ones_written() {
        let meta = ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: "t1".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: 1,
            updated_at: 2,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: Some("/proj".into()),
            driver_ref: Some("c1".into()),
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&meta).unwrap()).unwrap();
        assert_eq!(value["driver_project"], "/proj");
        assert_eq!(value["driver_ref"], "c1");
        assert!(
            value.get("board_project").is_none() && value.get("card_id").is_none(),
            "旧键不得再被写出(alias 只用于读): {value}"
        );
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
        // 同一行的旧 meta 也没有 `extra_roots`：缺字段必须读成空集（= 单根会话，
        // 与关联能力上线前逐字节等价的运行语义）。
        assert!(meta.extra_roots.is_empty());
        let _ = store;
    }

    /// `extra_roots` 落盘后必须原样读回，且**不**写进旧 meta 时也当空集——
    /// 后者即「旧库零迁移」。
    #[test]
    fn extra_roots_round_trip_and_legacy_meta_defaults_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let meta = ThreadMeta {
            plugin_bindings: Vec::new(),
            extra_roots: vec!["/w/linked-b".into(), "/w/linked-c".into()],
            ..sample_meta("t-roots")
        };
        store.create(&meta).unwrap();
        let loaded = store.load("t-roots").unwrap().unwrap();
        assert_eq!(
            loaded.meta.extra_roots,
            vec!["/w/linked-b".to_string(), "/w/linked-c".to_string()]
        );

        // 旧形状的 meta（无该字段）读成空集。
        let mut value = serde_json::to_value(sample_meta("t-legacy")).unwrap();
        value.as_object_mut().unwrap().remove("extra_roots");
        let back: ThreadMeta = serde_json::from_value(value).unwrap();
        assert!(back.extra_roots.is_empty());
    }

    #[test]
    fn set_model_ref_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        let now = now_millis();
        let meta = ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: "t".into(),
            cwd: "/w".into(),
            model: "m".into(),
            created_at: now,
            updated_at: now,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        };
        store.create(&meta).unwrap();
        store.set_model_ref("t", Some("B")).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.model_ref.as_deref(), Some("B"));
        store.set_model_ref("t", None).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.model_ref, None);
    }

    /// 旧 meta 缺 `reasoning_effort` 字段 → `None`(零迁移);设值往返;
    /// 清覆盖回 `None`。与 `set_model_ref_round_trips` 同款:走真实 `create`
    /// + `set_reasoning_effort` + `load` 的读-改-写路径。
    #[test]
    fn reasoning_effort_round_trips_and_legacy_defaults_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        // 旧 meta 无该字段(逐字模拟今天的盘上 JSON):反序列化必须给 `None`。
        let json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let legacy: ThreadMeta = serde_json::from_str(json).unwrap();
        assert_eq!(legacy.reasoning_effort, None, "旧 meta 缺字段必须回 None");

        store.create(&meta("t")).unwrap();
        assert_eq!(
            store.load("t").unwrap().unwrap().meta.reasoning_effort,
            None
        );
        store.set_reasoning_effort("t", Some("high")).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.reasoning_effort.as_deref(), Some("high"));
        store.set_reasoning_effort("t", None).unwrap();
        let loaded = store.load("t").unwrap().unwrap();
        assert_eq!(loaded.meta.reasoning_effort, None);
    }

    /// 未知 id 必须回 `Err(NotFound)`,与 `set_model` 同一错误语义(调用方据此
    /// 映射 `unknown_thread`),而不是静默成功。
    #[test]
    fn set_reasoning_effort_on_an_unknown_thread_is_not_found() {
        let (_d, s) = store();
        let err = s
            .set_reasoning_effort("ghost", Some("high"))
            .expect_err("an unknown thread must not be silently accepted");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// `preserve_thinking` 的零迁移契约(spec §4.1):旧 meta 缺该字段时必须回
    /// `true`(保留),而不是 `bool` 的 `serde(default)` = `false`。
    #[test]
    fn preserve_thinking_defaults_true_and_legacy_meta_migrates() {
        // 逐字模拟今天的盘上 JSON:没有 preserve_thinking 字段 ⇒ 必须回 true。
        // 必填字段(thread_id/cwd/model/created_at/updated_at/title)照实给全,
        // 只**故意**省掉 preserve_thinking。
        let legacy: ThreadMeta = serde_json::from_str(
            r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#,
        )
        .unwrap();
        assert!(
            legacy.preserve_thinking,
            "旧 meta 缺字段必须回 true(零迁移:老会话按「保留」解释)"
        );
    }

    /// `set_preserve_thinking` + `load` 的真实往返:true/false 都必须落盘并可读回。
    #[test]
    fn preserve_thinking_round_trips_through_set_and_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&meta("t")).unwrap();
        // 新建的 meta 默认保留。
        assert!(
            store.load("t").unwrap().unwrap().meta.preserve_thinking,
            "新建 meta 必须默认保留思考"
        );

        store.set_preserve_thinking("t", false).unwrap();
        assert!(
            !store.load("t").unwrap().unwrap().meta.preserve_thinking,
            "关闭开关必须落盘为 false"
        );

        store.set_preserve_thinking("t", true).unwrap();
        assert!(
            store.load("t").unwrap().unwrap().meta.preserve_thinking,
            "重新开启必须落盘为 true"
        );
    }

    /// 未知 id 与 `set_reasoning_effort` 同一错误语义:回 `Err(NotFound)`,调用方
    /// 据此映射 `unknown_thread`,而不是静默成功。
    #[test]
    fn set_preserve_thinking_on_an_unknown_thread_is_not_found() {
        let (_d, s) = store();
        let err = s
            .set_preserve_thinking("ghost", false)
            .expect_err("an unknown thread must not be silently accepted");
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    /// 组长标记必须能在落盘后读回,且旧 meta(无 `role`)读取为 `None`。
    #[test]
    fn role_round_trips_and_legacy_meta_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());

        let lead = ThreadMeta {
            plugin_bindings: Vec::new(),
            thread_id: "thread-lead-1".into(),
            cwd: dir.path().display().to_string(),
            model: "m".into(),
            created_at: 1,
            updated_at: 1,
            title: None,
            permission_mode: ThreadMode::Normal,
            pin_seq: None,
            driver_project: None,
            driver_ref: None,
            model_ref: None,
            reasoning_effort: None,
            tag: None,
            preserve_thinking: true,
            extra_roots: Vec::new(),
            role: Some(LEAD_ROLE.to_string()),
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
            forked_from: None,
            sandbox_baseline: None,
        };
        store.create(&lead).unwrap();
        store
            .append_turn(
                "thread-lead-1",
                &TurnLine::Turn {
                    items: Vec::new(),
                    usage: None,
                    usage_total: None,
                    messages: Vec::new(),
                },
            )
            .unwrap();

        let found = store.find_lead().unwrap().expect("lead must be found");
        assert_eq!(found.thread_id, "thread-lead-1");
        assert_eq!(found.role.as_deref(), Some(LEAD_ROLE));

        // 旧 meta:手工写一份不含 `role` 的 JSON,必须读成 None 且不被 find_lead 选中。
        let raw = serde_json::json!({
            "thread_id": "thread-old-1",
            "cwd": dir.path().display().to_string(),
            "model": "m",
            "created_at": 2,
            "updated_at": 2,
            "title": null,
            "permission_mode": "normal",
            "pin_seq": null,
            "driver_project": null,
            "driver_ref": null,
            "model_ref": null,
            "reasoning_effort": null,
            "preserve_thinking": true
        });
        std::fs::write(
            dir.path().join(".yi-agent/threads/thread-old-1.meta.json"),
            serde_json::to_vec_pretty(&raw).unwrap(),
        )
        .unwrap();
        let loaded = store.load("thread-old-1").unwrap().unwrap();
        assert_eq!(
            loaded.meta.role, None,
            "legacy meta must read as a plain thread"
        );
        // 组长仍只有那一个。
        assert_eq!(
            store.find_lead().unwrap().unwrap().thread_id,
            "thread-lead-1"
        );
    }

    /// 多个组长并存(理论上的异常态)时 `find_lead` 必须稳定返回最早创建的那个,
    /// 使懒创建幂等:并发两次调用不会在第二次又造一个新组长。
    #[test]
    fn find_lead_is_stable_when_more_than_one_exists() {
        let dir = tempfile::tempdir().unwrap();
        let store = ThreadStore::new(dir.path());
        for (id, created) in [("thread-lead-b", 20i64), ("thread-lead-a", 10i64)] {
            store
                .create(&ThreadMeta {
                    plugin_bindings: Vec::new(),
                    thread_id: id.into(),
                    cwd: dir.path().display().to_string(),
                    model: "m".into(),
                    created_at: created,
                    updated_at: created,
                    title: None,
                    permission_mode: ThreadMode::Normal,
                    pin_seq: None,
                    driver_project: None,
                    driver_ref: None,
                    model_ref: None,
                    reasoning_effort: None,
                    tag: None,
                    preserve_thinking: true,
                    extra_roots: Vec::new(),
                    role: Some(LEAD_ROLE.to_string()),
                    content_epoch: 0,
                    last_item_id: None,
                    anchor_sha: None,
                    last_read_at: 0,
                    last_completed_at: 0,
                    forked_from: None,
                    sandbox_baseline: None,
                })
                .unwrap();
            store
                .append_turn(
                    id,
                    &TurnLine::Turn {
                        items: Vec::new(),
                        usage: None,
                        usage_total: None,
                        messages: Vec::new(),
                    },
                )
                .unwrap();
        }
        assert_eq!(
            store.find_lead().unwrap().unwrap().thread_id,
            "thread-lead-a"
        );
        assert_eq!(
            store.find_lead().unwrap().unwrap().thread_id,
            "thread-lead-a"
        );
    }

    /// 项目键必须是 `<cwd>/.yi-agent` 的 canonical 路径:同一 cwd 的两种写法
    /// (含符号链接)必须得到同一个键,否则会造出两个组长。
    #[test]
    fn project_key_is_canonical_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let store = ThreadStore::new(&real);
        assert_eq!(
            store.project_key(),
            real.join(".yi-agent").display().to_string()
        );
        assert_eq!(store.project_key(), store.project_key());
    }

    /// 插件 worktree 不是「项目」:它里面**没有**组长。
    ///
    /// 一条落在那里的 `role:"lead"` meta 是假组长(真机数据里已有 9 例:
    /// 卡片会话 `ask_lead` → `resolve_lead_target` 在 worktree 里懒创建)。
    /// `find_lead` 必须无视它,否则该 worktree 会在侧栏里变成一个带组长的
    /// 顶层项目组。
    ///
    /// 路径判据本身住在 [`crate::plugin_worktree`](它自己的单测覆盖各条路径),
    /// 这里只钉「`is_lead_meta` 确实用了那个判据」。
    #[test]
    fn a_kanban_worktree_never_hosts_a_lead() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join(".worktrees/kanban/card-1");
        std::fs::create_dir_all(&worktree).unwrap();
        let store = ThreadStore::new(&worktree);
        let mut phantom = meta("thread-lead-phantom");
        phantom.cwd = worktree.display().to_string();
        phantom.role = Some(LEAD_ROLE.to_string());
        store.create(&phantom).unwrap();

        assert!(
            store.find_lead().unwrap().is_none(),
            "a card worktree has no lead, even when a lead meta sits there"
        );

        // 同一个 role 落在普通项目目录里仍被认成组长:排除的是**路径**,不是 role。
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let project_store = ThreadStore::new(&project);
        let mut real = meta("thread-lead-real");
        real.cwd = project.display().to_string();
        real.role = Some(LEAD_ROLE.to_string());
        project_store.create(&real).unwrap();
        assert_eq!(
            project_store.find_lead().unwrap().unwrap().thread_id,
            "thread-lead-real"
        );
    }

    /// 旧 meta（本字段出现前落盘的文件）必须读成 epoch 0 / last_item_id None，
    /// 而不是解析失败——这是零迁移的硬要求（与 `pin_seq` / `reasoning_effort` 同款）。
    #[test]
    fn a_legacy_meta_without_the_new_fields_reads_as_epoch_zero() {
        // 直接反序列化最老的 meta 形状：这条路径**不会**回退到 `rebuild_meta`，
        // 因此只有 `#[serde(default)]` 在位时才能通过。
        let legacy_json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let legacy: ThreadMeta = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(
            legacy.content_epoch, 0,
            "missing content_epoch must read as 0"
        );
        assert_eq!(
            legacy.last_item_id, None,
            "missing last_item_id must read as None"
        );
        assert_eq!(legacy.thread_id, "t", "other fields must survive");
    }

    /// 未读由两个时间戳派生：`last_completed_at > last_read_at`。
    ///
    /// 这条测试钉住判定式本身：新建的会话两个水位都是 0 ⇒ 不未读（旧数据零迁移
    /// 的结果）；只写完成时间 ⇒ 未读；已读推进到完成之后 ⇒ 不再未读。
    #[test]
    fn unread_is_derived_from_the_two_watermarks() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let fresh = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(fresh.last_read_at, 0, "a new thread has never been read");
        assert_eq!(fresh.last_completed_at, 0);
        assert!(
            !fresh.is_unread(),
            "0/0 must read as read: old metas predate the field and must not sprout dots"
        );

        let completed = s
            .mark_turn_completed("thread-a", now_millis())
            .unwrap()
            .expect("thread exists");
        assert!(completed > 0);
        let done = s.load("thread-a").unwrap().unwrap().meta;
        assert!(done.last_completed_at > 0);
        assert!(
            done.is_unread(),
            "a completion after the read mark is unread"
        );

        let (read_at, completed_at) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert_eq!(
            completed_at, done.last_completed_at,
            "both watermarks come back together"
        );
        let after = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(after.last_read_at, read_at);
        assert!(!after.is_unread(), "reading clears the dot");
    }

    /// 已读水位单调：重复标记不倒退，也不刷新 `updated_at`。
    #[test]
    fn mark_read_is_monotonic_and_does_not_touch_updated_at() {
        let (_d, s) = store();
        let mut m = meta("thread-a");
        m.updated_at = 7;
        s.create(&m).unwrap();

        let (first, _) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert!(first > 0);
        // 打已读是用户视图标记而非会话活动：不得把该行顶到列表最上方。
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.updated_at,
            7,
            "marking read must not refresh updated_at"
        );

        // 第二次标记：`now` 可能相等或更大，但绝不能更小。
        let (second, _) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert!(
            second >= first,
            "the read watermark must never go backwards"
        );

        // 时钟被拨回时也取 max：直接写一个更小的值验证同一个不变量。
        s.set_last_read_at_for_test("thread-a", 1).unwrap();
        let (back, _) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert!(
            back >= second,
            "a clock rollback must not rewind the watermark"
        );
    }

    /// 水位没推进时**不写盘**：重复点开同一个会话不该白改一次 meta 的 mtime。
    ///
    /// 判据用 mtime 而不是内容：内容在两种写法下都一样，区别只在于有没有真的
    /// 走一次「序列化 + 原子 rename」。盯着 meta 的 watcher 正是靠 mtime 判断
    /// 「要不要重读」，所以无谓的写会变成无谓的重读。
    #[test]
    fn marking_read_without_advancing_writes_nothing() {
        let (d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let path = d.path().join(".yi-agent/threads/thread-a.meta.json");

        let (first, _) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert!(first > 0);
        let after_first = std::fs::metadata(&path).unwrap().modified().unwrap();

        // 把水位钉到「很远的将来」，再标记一次：`max(旧值, now)` 仍是旧值。
        s.set_last_read_at_for_test("thread-a", first + 10_000_000)
            .unwrap();
        let pinned = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(pinned >= after_first);

        let (again, _) = s.mark_read("thread-a").unwrap().expect("thread exists");
        assert_eq!(again, first + 10_000_000, "the watermark must not move");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            pinned,
            "a no-op read mark must not rewrite the meta file"
        );
    }

    /// 同一个完成时刻记两次只写一次盘（失败路径与常规收尾可能都走到这里）。
    #[test]
    fn recording_the_same_completion_twice_writes_once() {
        let (d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let path = d.path().join(".yi-agent/threads/thread-a.meta.json");

        let at = now_millis();
        let first = s
            .mark_turn_completed("thread-a", at)
            .unwrap()
            .expect("thread exists");
        assert_eq!(first, at);
        let written = std::fs::metadata(&path).unwrap().modified().unwrap();

        let second = s
            .mark_turn_completed("thread-a", at)
            .unwrap()
            .expect("thread exists");
        assert_eq!(second, at, "the completion time must not move");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            written,
            "recording the same completion twice must not rewrite the meta file"
        );

        // 更早的时间同样不推进（时钟回拨 / 乱序收尾）。
        let third = s
            .mark_turn_completed("thread-a", at - 1000)
            .unwrap()
            .expect("thread exists");
        assert_eq!(third, at, "the completion watermark only moves forward");
    }

    #[test]
    fn mark_read_unknown_id_is_none() {
        let (_d, s) = store();
        assert!(s.mark_read("nope").unwrap().is_none());
    }

    /// 旧 meta 缺两个新字段：读成 0，且不得凭空造出未读。
    #[test]
    fn legacy_meta_without_read_watermarks_reads_as_zero() {
        // 严格解析（绕过 `load()` 的 `rebuild_meta` 兜底），理由同
        // `legacy_meta_without_tag_deserializes_to_none`。
        let legacy_json = r#"{"thread_id":"t","cwd":"/w","model":"m","created_at":1,"updated_at":2,"title":null}"#;
        let legacy: ThreadMeta = serde_json::from_str(legacy_json)
            .expect("old metas must deserialize into the current ThreadMeta");
        assert_eq!(
            legacy.last_read_at, 0,
            "missing last_read_at must read as 0"
        );
        assert_eq!(
            legacy.last_completed_at, 0,
            "missing last_completed_at must read as 0"
        );
        assert!(
            !legacy.is_unread(),
            "a legacy thread must not sprout an unread dot"
        );
    }

    #[test]
    fn bump_content_epoch_increments_without_touching_updated_at() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let before = s.load("thread-a").unwrap().unwrap().meta;

        assert!(
            s.bump_content_epoch("thread-a").unwrap(),
            "an existing thread must bump"
        );
        let after = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(after.content_epoch, before.content_epoch + 1);
        // 世代变更属内容维护，不是会话活动：不得改变按 updated_at 的列表顺序。
        assert_eq!(
            after.updated_at, before.updated_at,
            "bump must not touch updated_at"
        );

        assert!(s.bump_content_epoch("thread-a").unwrap());
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.content_epoch,
            2,
            "each bump advances by exactly one"
        );
    }

    #[test]
    fn bump_content_epoch_on_an_unknown_thread_is_false_not_an_error() {
        let (_d, s) = store();
        assert!(!s.bump_content_epoch("nope").unwrap());
    }

    #[test]
    fn contains_item_id_matches_only_ids_present_in_the_log() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(
                vec![Item::AgentMessage {
                    id: "a-1".into(),
                    text: "hi".into(),
                }],
                vec![],
            ),
        )
        .unwrap();

        assert!(s.contains_item_id("thread-a", "a-1").unwrap());
        assert!(!s.contains_item_id("thread-a", "a-2").unwrap());
        // 清空日志后锚点即失效——这正是 `thread/clear` 后的形状。
        s.truncate("thread-a").unwrap();
        assert!(!s.contains_item_id("thread-a", "a-1").unwrap());
    }

    #[test]
    fn contains_item_id_on_an_unknown_thread_is_false_not_an_error() {
        // 未知会话（没有日志文件）⇒ 不匹配，而不是把整个握手带崩。这条是 C 判据
        // （`turn/start` 的锚点校验）与握手裁决的**唯一权威入口**，必须钉住：
        // `read_log` 在文件不存在时返回空 items ⇒ `false`（fail-closed，宁可让
        // 客户端多补一次，也不同意为锚点仍在）。
        let (_d, s) = store();
        assert!(!s.contains_item_id("never-created", "a-1").unwrap());
    }

    #[test]
    fn contains_item_id_rejects_a_malformed_id() {
        // 非法 id（含路径分隔符）是**调用方 bug**，不是"不匹配"：必须 Err，
        // 否则会静默把一次畸形请求当成"锚点不存在"而误判失效。
        let (_d, s) = store();
        assert!(s.contains_item_id("bad/id", "a-1").is_err());
    }

    #[test]
    fn set_last_item_id_records_and_clears_the_cursor() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.set_last_item_id("thread-a", Some("i-9")).unwrap();
        assert_eq!(
            s.load("thread-a")
                .unwrap()
                .unwrap()
                .meta
                .last_item_id
                .as_deref(),
            Some("i-9")
        );
        s.set_last_item_id("thread-a", None).unwrap();
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.last_item_id, None);
    }

    /// The user's requirement: after a crash, everything produced must still be
    /// visible. A checkpoint written mid-turn must survive `load` intact and be
    /// promotable into the authoritative log.
    ///
    /// This pins existing behaviour; it does not ask for any change. It exists so
    /// a future "optimisation" cannot silently thin the checkpoint.
    #[test]
    fn a_crash_mid_turn_loses_none_of_the_completed_items() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-crash")).unwrap();

        // A turn in progress: the opening prompt plus N completed items.
        // Built inline like the existing partial tests do — there is no
        // `assistant_item` helper in this module.
        let mut items = vec![user_item("user-turn-x", "do the thing")];
        for i in 0..50 {
            items.push(crate::protocol::Item::AgentMessage {
                id: format!("item-turn-x-{i}"),
                text: format!("step {i}"),
            });
        }
        store
            .write_partial(
                "thread-crash",
                &PartialTurn {
                    turn_id: "turn-x".into(),
                    items: items.clone(),
                    messages: Vec::new(),
                    usage: None,
                    usage_total: None,
                },
            )
            .unwrap();

        // Crash here: nothing else is written. Resume must see everything.
        let loaded = store.load("thread-crash").unwrap().unwrap();
        assert!(loaded.pending_turn, "an uncommitted turn must be reported");
        assert_eq!(
            loaded.items.len(),
            items.len(),
            "a crash must not lose completed items"
        );

        // And promoting it must make the content authoritative, not drop it.
        assert!(store.promote_partial("thread-crash").unwrap());
        let after = store.load("thread-crash").unwrap().unwrap();
        assert!(!after.pending_turn);
        assert_eq!(after.items.len(), items.len());
    }

    /// The driver checkpoints the opening prompt before the model is even called
    /// (`server.rs` "turn 开始就把提问落进 checkpoint"). A crash immediately
    /// after that must still show the question the user typed.
    #[test]
    fn the_opening_prompt_survives_an_immediate_crash() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        store.create(&sample_meta("thread-early")).unwrap();

        store
            .write_partial(
                "thread-early",
                &PartialTurn {
                    turn_id: "turn-e".into(),
                    items: vec![user_item("user-turn-e", "did you save me?")],
                    messages: Vec::new(),
                    usage: None,
                    usage_total: None,
                },
            )
            .unwrap();

        let loaded = store.load("thread-early").unwrap().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(
            crate::server::item_id(&loaded.items[0]),
            Some("user-turn-e"),
            "the opening prompt must be recoverable"
        );
    }

    #[test]
    fn thread_lock_is_reacquirable_after_drop() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        {
            let _g = acquire_thread_lock(root, "thread-reacq").unwrap();
        }
        // drop 之后必须能重新拿到：进程内表要清干净，否则同一进程内
        // 「关掉再打开同一会话」会永远返回同一个旧 Arc（且内核锁状态不明）。
        let again = acquire_thread_lock(root, "thread-reacq").unwrap();
        assert!(Arc::strong_count(&again) >= 1);
    }

    /// 非 Busy 的 flock 失败（这里用真实 EBADF）必须是 `Io`，**不能**打印后
    /// 放行——放行意味着两个进程同时写同一份历史。走到这条路径需要真正的 flock
    /// 失败，而不是文件 open 失败。
    #[test]
    fn a_real_flock_failure_classifies_as_io_not_busy() {
        // 从无效 fd 上真跑一次 flock，拿到内核给的 EBADF。
        let bad_fd: std::os::unix::io::RawFd = -1;
        assert_ne!(
            unsafe { libc::flock(bad_fd, libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let error = std::io::Error::last_os_error();
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));

        match classify_flock_error(error, "thread-ebadf", Path::new("/tmp/x.lock")) {
            ThreadLockError::Io(io) => {
                assert!(
                    io.to_string().contains("flock failed"),
                    "故障必须带上说明: {io}"
                );
            }
            other => panic!("真故障必须报 Io，实际: {other:?}"),
        }
    }

    /// EWOULDBLOCK 走同一分类函数时必须变成 Busy（而不是 Io）。
    #[test]
    fn a_would_block_flock_failure_classifies_as_busy() {
        let error = std::io::Error::from_raw_os_error(libc::EWOULDBLOCK);
        match classify_flock_error(error, "thread-busy", Path::new("/tmp/x.lock")) {
            ThreadLockError::Busy(busy) => assert_eq!(busy.thread_id, "thread-busy"),
            other => panic!("被占用必须报 Busy，实际: {other:?}"),
        }
    }

    #[test]
    fn thread_lock_reports_io_failure_not_busy() {
        // `.yi-agent` 被一个普通文件占住时，锁文件建不出来。这**不是**
        // 「会话被占用」——若报成 Busy，用户会去找一个并不存在的另一个实例。
        let dir = TempDir::new().unwrap();
        let workdir = dir.path();
        std::fs::write(workdir.join(".yi-agent"), b"not a directory").unwrap();
        let root = workdir.join(".yi-agent").join("threads");

        match acquire_thread_lock(&root, "thread-io-fail") {
            Err(ThreadLockError::Io(_)) => {}
            Err(other) => panic!("IO 故障必须报 Io，实际: {other:?}"),
            Ok(_) => panic!("建不出锁文件时必须失败，而不是静默放行"),
        }
    }

    #[test]
    fn old_meta_without_forked_from_reads_as_none() {
        let (_dir, s) = store();
        // 既有 meta 文件全都没有 forked_from。缺 #[serde(default)] 时反序列化
        // 会失败，load 走 rebuild_meta 兜底并把 cwd 冲成空串——这里同时断言 cwd。
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-nofork"),
            r#"{"thread_id":"thread-nofork","cwd":"/tmp/keep-me","model":"m","created_at":1,
                "updated_at":1,"title":null,"permission_mode":"normal"}"#,
        )
        .unwrap();
        let loaded = s.load("thread-nofork").unwrap().expect("meta must parse");
        assert_eq!(loaded.meta.forked_from, None);
        assert_eq!(
            loaded.meta.cwd, "/tmp/keep-me",
            "cwd 被冲成空串说明反序列化失败了，走了 rebuild_meta 兜底"
        );
    }

    /// 旧 meta 没有 `plugin_bindings`：必须零迁移读成空表（普通会话）。
    ///
    /// 若缺 `#[serde(default)]`，反序列化会整份失败，`load` 走 `rebuild_meta`
    /// 兜底——那会把 cwd 等字段一起冲掉。所以这里同时断言 cwd 还在。
    #[test]
    fn old_meta_without_plugin_bindings_reads_as_an_empty_list() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-plain"),
            r#"{"thread_id":"thread-plain","cwd":"/tmp/keep-me","model":"m","created_at":1,
                "updated_at":1,"title":null,"permission_mode":"normal"}"#,
        )
        .unwrap();
        let loaded = s.load("thread-plain").unwrap().expect("meta must parse");
        assert!(
            loaded.meta.plugin_bindings.is_empty(),
            "a session that never bound a plugin has no bindings"
        );
        assert_eq!(
            loaded.meta.cwd, "/tmp/keep-me",
            "cwd 被冲成空串说明反序列化失败了，走了 rebuild_meta 兜底"
        );
    }

    /// resume 一个插件会话：绑定的**具体版本与代次**必须原样回来。
    ///
    /// 这是"插件升级后旧会话被判陈旧"唯一的信息来源。若 resume 时把绑定丢了
    /// （或改写成"当前版本"），旧会话就会被静默接到新版本上——正是设计里
    /// 明确要避免的那条路。
    #[test]
    fn resuming_a_plugin_session_keeps_the_exact_plugin_binding() {
        let (_dir, s) = store();
        let binding = yi_agent_plugins::Binding {
            host_id: "host-abc".into(),
            release: yi_agent_plugins::ReleaseId {
                plugin: yi_agent_plugins::PluginId::parse("example-plugin").unwrap(),
                version: "1.4.0".into(),
                target: "darwin-aarch64".into(),
                sha256: "ab".repeat(32),
            },
            activation: 3,
            project: yi_agent_plugins::ProjectId::parse("/tmp/project").unwrap(),
            grant: 2,
        };
        let mut meta = meta("thread-plugin");
        meta.cwd = "/tmp/project".into();
        meta.plugin_bindings = vec![binding.clone()];
        std::fs::create_dir_all(&s.root).unwrap();
        s.create(&meta).unwrap();

        let loaded = s.load("thread-plugin").unwrap().expect("meta must parse");
        assert_eq!(
            loaded.meta.plugin_bindings,
            vec![binding],
            "resume must not drop or rewrite the binding"
        );
    }

    /// A meta written before baselines existed must read as "no record", not as
    /// a parse failure: those conversations are still openable, and activation
    /// decides what floor to give them.
    #[test]
    fn a_meta_without_a_baseline_reads_as_none() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-legacy-baseline"),
            r#"{"thread_id":"thread-legacy-baseline","cwd":"/tmp/keep-me","model":"m",
                "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal"}"#,
        )
        .unwrap();
        let active = s
            .load_for_activation("thread-legacy-baseline")
            .unwrap()
            .expect("legacy meta must still be readable");
        assert_eq!(active.meta.sandbox_baseline, None);
        assert_eq!(
            active.meta.cwd, "/tmp/keep-me",
            "cwd reset to empty means the meta failed to parse and was rebuilt"
        );
    }

    /// Absent (compatible history) and present-but-null (a claim that cannot be
    /// honored) must not collapse into the same answer.
    #[test]
    fn a_present_but_null_baseline_is_refused_at_activation() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-null-baseline"),
            r#"{"thread_id":"thread-null-baseline","cwd":"/tmp","model":"m",
                "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal",
                "sandbox_baseline":null}"#,
        )
        .unwrap();
        let error = s
            .load_for_activation("thread-null-baseline")
            .expect_err("an explicit null baseline must not activate");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        // The tolerant read still works: listing must never hide a thread.
        assert!(s.load("thread-null-baseline").unwrap().is_some());
    }

    #[test]
    fn an_unknown_baseline_mode_is_refused_at_activation() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-bad-mode"),
            r#"{"thread_id":"thread-bad-mode","cwd":"/tmp","model":"m",
                "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal",
                "sandbox_baseline":{"mode":"god-mode","promotable":true}}"#,
        )
        .unwrap();
        let error = s
            .load_for_activation("thread-bad-mode")
            .expect_err("an unknown sandbox mode must not activate");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_wrongly_shaped_baseline_is_refused_at_activation() {
        let (_dir, s) = store();
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-bad-shape"),
            r#"{"thread_id":"thread-bad-shape","cwd":"/tmp","model":"m",
                "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal",
                "sandbox_baseline":{"mode":"workspace-write","promotable":"yes"}}"#,
        )
        .unwrap();
        assert!(
            s.load_for_activation("thread-bad-shape").is_err(),
            "a string where a bool belongs is corruption, not history"
        );
    }

    #[test]
    fn a_read_only_baseline_is_persisted_with_promotion_off() {
        let (_dir, s) = store();
        let mut meta = sample_meta("thread-ro");
        meta.sandbox_baseline = Some(
            crate::session_sandbox::SandboxBaseline::new("read-only", true)
                .expect("read-only is a valid mode"),
        );
        s.create(&meta).unwrap();
        let loaded = s.load_for_activation("thread-ro").unwrap().unwrap();
        let baseline = loaded
            .meta
            .sandbox_baseline
            .expect("baseline must round-trip");
        assert!(!baseline.promotable, "read-only can never be promotable");
    }

    #[test]
    fn a_baseline_round_trips_through_disk() {
        let (_dir, s) = store();
        let mut meta = sample_meta("thread-baseline-roundtrip");
        meta.sandbox_baseline =
            Some(crate::session_sandbox::SandboxBaseline::new("workspace-write", true).unwrap());
        s.create(&meta).unwrap();
        let raw = std::fs::read_to_string(s.meta_path("thread-baseline-roundtrip")).unwrap();
        assert!(
            raw.contains("\"sandbox_baseline\""),
            "the baseline must actually be written, not dropped by serde"
        );
        let loaded = s
            .load_for_activation("thread-baseline-roundtrip")
            .unwrap()
            .unwrap();
        assert_eq!(
            loaded.meta.sandbox_baseline, meta.sandbox_baseline,
            "the record read back must be the record written"
        );
    }

    /// An unrecorded baseline carries forward on fork: the copied history ran
    /// under the source's sandbox, so the copy must not be widened.
    #[test]
    fn a_fork_inherits_the_source_baseline() {
        let (_dir, s) = store();
        let mut meta = sample_meta("thread-src");
        meta.sandbox_baseline =
            Some(crate::session_sandbox::SandboxBaseline::new("read-only", false).unwrap());
        s.create(&meta).unwrap();
        s.fork_thread("thread-src", "thread-copy").unwrap();
        let forked = s.load("thread-copy").unwrap().expect("forked thread");
        assert_eq!(
            forked.meta.sandbox_baseline, meta.sandbox_baseline,
            "a fork must not adopt today's default instead of the source's sandbox"
        );
    }

    #[test]
    fn old_meta_without_anchor_reads_as_none() {
        let (_dir, s) = store();
        // 旧 meta 没有 anchor_sha 字段：必须零迁移地读成 None。
        // 路径用 store 自己的布局（`<root>/<id>.meta.json`），与既有的
        // `legacy_meta_without_pin_seq_deserializes_unpinned` 同形。
        std::fs::create_dir_all(&s.root).unwrap();
        std::fs::write(
            s.meta_path("thread-old"),
            r#"{"thread_id":"thread-old","cwd":"/tmp","model":"m","created_at":1,"updated_at":1,
                "title":null,"permission_mode":"normal","pin_seq":null,"board_project":null,
                "card_id":null,"model_ref":null,"reasoning_effort":null,"tag":null,
                "preserve_thinking":true,"extra_roots":[],"role":null}"#,
        )
        .unwrap();
        let loaded = s.load("thread-old").unwrap().expect("meta must parse");
        assert_eq!(loaded.meta.anchor_sha, None);
    }

    // --- 跨进程会话写锁（Task 1）---

    #[test]
    fn thread_lock_is_idempotent_within_a_process() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let a = acquire_thread_lock(root, "thread-lock-idem").unwrap();
        let b = acquire_thread_lock(root, "thread-lock-idem").unwrap();
        // 同一进程内必须拿到同一个 Arc，否则 flock 按描述符加锁会自欺欺人
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn thread_lock_file_survives_release() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        {
            let _g = acquire_thread_lock(root, "thread-lock-keep").unwrap();
        }
        assert!(
            thread_lock_path(root, "thread-lock-keep").exists(),
            "锁文件不能被删除：删了会让已持有旧 inode 的进程与新建同名文件的进程都以为持锁"
        );
    }

    #[test]
    fn thread_lock_rejects_a_second_process() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let _held = acquire_thread_lock(root, "thread-lock-proc").unwrap();

        // 子进程尝试拿同一把锁：必须失败
        let exe = std::env::current_exe().unwrap();
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "thread_store::tests::child_tries_thread_lock",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_LOCK_ROOT", root)
            .env("YI_LOCK_THREAD", "thread-lock-proc")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("LOCK_BUSY"),
            "子进程应报告 LOCK_BUSY，实际: {stderr}"
        );
    }

    /// 由 `thread_lock_rejects_a_second_process` 以子进程方式调用。
    #[test]
    #[ignore]
    fn child_tries_thread_lock() {
        let root = std::path::PathBuf::from(std::env::var("YI_LOCK_ROOT").unwrap());
        let id = std::env::var("YI_LOCK_THREAD").unwrap();
        match acquire_thread_lock(&root, &id) {
            Ok(_) => eprintln!("LOCK_ACQUIRED_UNEXPECTEDLY"),
            Err(e) => eprintln!("LOCK_BUSY {e}"),
        }
    }
}
