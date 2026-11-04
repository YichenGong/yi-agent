//! 旁支冷读：严格、无副作用地读取一个未装载父会话的持久状态。
//!
//! 与 [`super::ThreadStore::load`] 的区别是这个路径**一处都不吞错**。宽松入口
//! 跳过坏行是给 UI 用的（宁可少显示一轮也别白屏）；但旁支要拿这份历史当唯一
//! 上下文继承给模型，静默跳过一行就等于让模型在失忆状态下回答——它不会知道自己
//! 少了什么，只会自信地编。
//!
//! 同样重要的一处**不做**的事：这里不 `promote_partial`、不修复写回、不取父的
//! writer lock。旁支是只读观察者；一旦它写回 partial 或抢锁，打开一个旁支就会
//! 改变父会话的磁盘状态，甚至在父正被别的进程写时阻塞它。

use std::io::{self, Read};
use std::time::SystemTime;

use yi_agent_core::{Message, forkable_messages};

use crate::thread_store::{
    PartialTurn, ThreadMeta, ThreadStore, TurnLine, io_err, read_optional_text, valid_id,
};

/// 单个文件允许读取的上限。
///
/// 上限存在的原因是**先拒绝再读**：一个被追加到 10 GiB 的日志如果先
/// `read_to_string` 再判断大小，进程已经在判定之前 OOM 了。`take(limit + 1)` 让
/// 我们最多读 `limit + 1` 字节，多出的那 1 字节正是「到底超没超」的证据。
pub const SIDE_META_MAX_BYTES: u64 = 1024 * 1024;
pub const SIDE_LOG_MAX_BYTES: u64 = 128 * 1024 * 1024;
pub const SIDE_PARTIAL_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// 三个文件的身份指纹：路径无关，只看内容是否被换过。
///
/// 记录 `len` 与 `mtime` 而不是只记 `len`：同长度覆盖写（例如 partial 在同一个
/// 回合里被重写而内容长度不变）只改 mtime，只看长度会把两份不同内容当成同一份。
/// 记 `ino` 是因为原子重命名（`write_atomic` 就是这么写的）会换 inode——同一路径
/// 换了 inode 就是换了文件，与内容是否相同无关。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ColdFileStamp {
    /// 文件不存在时为 `None`，否则为 `(len, ino, mtime_nanos)` 的摘要。
    pub meta: Option<FileIdentity>,
    pub log: Option<FileIdentity>,
    pub partial: Option<FileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub len: u64,
    pub inode: u64,
    /// mtime 的纳秒数。取 `None` 时退化为 0：某些文件系统不提供纳秒精度，
    /// 但 `len + inode` 仍然能捕获绝大多数替换。
    pub mtime_nanos: u128,
}

/// 一个一致、完整、严格解码的父会话快照。
///
/// `messages` 已经是**安全前缀**（尾部未配对的 tool_use 已移除）：调用方不必再
/// 处理悬空工具调用，也不必再克隆一次。
pub struct StrictThreadSnapshot {
    pub meta: ThreadMeta,
    pub messages: Vec<Message>,
    /// 持久化的用户附件，按出现顺序去重。
    pub attachments: Vec<crate::protocol::Attachment>,
    /// 读取这份快照时三个文件的身份。调用方在组装后重新取一次，相同才说明期间
    /// 父没有被改写。
    pub stamp: ColdFileStamp,
}

impl ThreadStore {
    /// 严格、有界、无副作用地读取一个父会话。
    ///
    /// - 三个文件都不存在 ⇒ `Ok(None)`：这个会话没有可继承的东西。
    /// - 任一文件损坏 / 截断 / 超限 ⇒ `Err`：宁可让用户看到一条明确的创建失败，
    ///   也不要把一份不完整的历史当成完整快照交给模型。
    /// - 成功时**不改写任何文件**，也不取 writer lock。
    pub fn read_side_snapshot_strict(&self, id: &str) -> io::Result<Option<StrictThreadSnapshot>> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a valid thread id: {id:?}"),
            ));
        }
        let meta_path = self.meta_path(id);
        let log_path = self.log_path(id);
        let partial_path = self.partial_path(id);

        // 先取指纹再读内容：读完再取一次（由调用方做）才能证明「读到的就是
        // 判定时的那个文件」。顺序反过来会在读与取指纹之间留下盲区。
        let stamp = self.side_cold_stamp(id)?;

        let meta_text = read_bounded(&meta_path, SIDE_META_MAX_BYTES)?;
        let log_text = read_bounded(&log_path, SIDE_LOG_MAX_BYTES)?;
        let partial_text = read_bounded(&partial_path, SIDE_PARTIAL_MAX_BYTES)?;

        if meta_text.is_none() && log_text.is_none() && partial_text.is_none() {
            return Ok(None);
        }

        // 有历史却没有 meta：无法知道 cwd / 模型 / 权限，重建一份等于凭空造一个
        // 与父无关的会话，故拒绝。
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

        // 严格逐行：坏行是错误，不是「跳过这一行」。
        let mut messages: Vec<Message> = Vec::new();
        let mut attachments: Vec<crate::protocol::Attachment> = Vec::new();
        if let Some(text) = &log_text {
            for line in text.lines() {
                let turn = decode_side_log_line_strict(line.as_bytes())?;
                let TurnLine::Turn {
                    items: turn_items,
                    messages: turn_messages,
                    ..
                } = turn;
                collect_attachments(&turn_items, &mut attachments);
                if !turn_messages.is_empty() {
                    messages = turn_messages;
                }
            }
        }

        // partial：尚未收尾的那一轮是真实的、已发生过的内容，必须纳入；但**不**
        // 升格、不清空。它的 messages 更新（它比主日志更晚），规则与宽松入口一致。
        if let Some(text) = &partial_text {
            let partial: PartialTurn = serde_json::from_str(text).map_err(io_err)?;
            collect_attachments(&partial.items, &mut attachments);
            if !partial.messages.is_empty() {
                messages = partial.messages;
            }
        }

        if messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("thread {id} has no restorable messages"),
            ));
        }

        // 尾部未配对的 tool_use 必须移除：旁支不会执行那个工具，留着它会让模型
        // 看到一次「永远等不到结果」的调用，或者更糟——让 provider 拒绝整个请求。
        let safe = forkable_messages(&yi_agent_core::Session::from_messages(messages));

        Ok(Some(StrictThreadSnapshot {
            meta,
            messages: safe,
            attachments,
            stamp,
        }))
    }

    /// 三个文件的当前身份。路径不存在记为 `None`，其它错误原样返回。
    pub fn side_cold_stamp(&self, id: &str) -> io::Result<ColdFileStamp> {
        if !valid_id(id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("not a valid thread id: {id:?}"),
            ));
        }
        Ok(ColdFileStamp {
            meta: file_identity(&self.meta_path(id))?,
            log: file_identity(&self.log_path(id))?,
            partial: file_identity(&self.partial_path(id))?,
        })
    }
}

/// 严格解码一行日志。
///
/// 与 `read_log` 的关键差别：**空行也是错误**。写入端 `append_turn` 只会写完整
/// 的一行（序列化 + `'\n'`），所以文件中间出现空行，说明有别的写者用别的方式
/// 夹了内容进来，或者文件被截断后又被拼接——两种情况下「跳过空行继续读」都会
/// 把两份不同来源的历史混成一份。
pub fn decode_side_log_line_strict(bytes: &[u8]) -> io::Result<TurnLine> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a blank line cannot appear inside a thread log",
        ));
    }
    serde_json::from_slice::<TurnLine>(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("corrupt thread log line: {error}"),
        )
    })
}

/// 有界读取：不存在返回 `None`，超过 `limit` 返回 `InvalidData`。
fn read_bounded(path: &std::path::Path, limit: u64) -> io::Result<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // 先看 metadata 里的长度：多数超限文件不必真的读 128 MiB 才发现。
    if let Ok(meta) = file.metadata()
        && meta.len() > limit
    {
        return Err(too_large(path, meta.len(), limit));
    }
    let mut buf = Vec::new();
    // `take(limit + 1)` 让「刚好等于上限」通过、「多一字节」被拒，且绝不读更多。
    file.take(limit.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    if buf.len() as u64 > limit {
        return Err(too_large(path, buf.len() as u64, limit));
    }
    String::from_utf8(buf).map(Some).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not UTF-8", path.display()),
        )
    })
}

fn too_large(path: &std::path::Path, len: u64, limit: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} is {len} bytes, over the {limit}-byte side read limit",
            path.display()
        ),
    )
}

fn file_identity(path: &std::path::Path) -> io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(meta) => Ok(Some(FileIdentity {
            len: meta.len(),
            inode: meta.ino(),
            mtime_nanos: meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|delta| delta.as_nanos())
                .unwrap_or(0),
        })),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// 收集一轮里的用户附件。
///
/// 只取 `UserMessage`/`UserInterjection` 的 `attachments` 字段——那是宿主在受理
/// 请求时写下的**结构化**记录。这里刻意不扫描文本里的路径：从自然语言里猜一个
/// 路径，等于把「模型提到过的任意文件」变成旁支可读的资源。
fn collect_attachments(
    items: &[crate::protocol::Item],
    out: &mut Vec<crate::protocol::Attachment>,
) {
    use crate::protocol::Item;
    for item in items {
        let list = match item {
            Item::UserMessage { attachments, .. } | Item::UserInterjection { attachments, .. } => {
                attachments
            }
            _ => continue,
        };
        for attachment in list {
            if !out.iter().any(|seen| seen.path == attachment.path) {
                out.push(attachment.clone());
            }
        }
    }
}

/// 供 `read_optional_text` 的既有调用方复用：本模块改了它的可见性语义，故在
/// 这里显式引用一次，避免 `unused import` 在不同 feature 组合下抖动。
#[allow(dead_code)]
fn _keep_read_optional_text(path: &std::path::Path) -> io::Result<Option<String>> {
    read_optional_text(path)
}

#[cfg(test)]
mod tests;
