//! thread 会话持久化:每 thread 一个只追加 `.jsonl` 日志 + 一个可变 `.meta.json`。
//!
//! 目录布局 `<workdir>/.yi-agent/threads/`:
//! - `<thread_id>.jsonl`     只追加,每 turn 一行 `TurnLine::Turn`
//! - `<thread_id>.meta.json` 可变,整体原子重写

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yi_agent_core::{ContentBlock, Message, Role};

use crate::protocol::Item;

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
}

/// 一次 turn 的 token 用量。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
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

/// `load` 的结果:meta + 拼接后的 items + 最后一条记录的 messages/usage。
pub struct LoadedThread {
    pub meta: ThreadMeta,
    pub items: Vec<Item>,
    pub messages: Vec<Message>,
    pub usage: Option<TurnUsage>,
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

    pub fn load(&self, id: &str) -> io::Result<Option<LoadedThread>> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let log = self.log_path(id);
        let meta_path = self.meta_path(id);
        if !log.exists() && !meta_path.exists() {
            return Ok(None);
        }

        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        match std::fs::read_to_string(&log) {
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

        let meta = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
            .unwrap_or_else(|| rebuild_meta(id, &log, &messages));

        Ok(Some(LoadedThread {
            meta,
            items,
            messages,
            usage,
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
    pub fn delete(&self, id: &str) -> io::Result<bool> {
        if !valid_id(id) {
            return Err(invalid_id(id));
        }
        let meta = self.meta_path(id);
        let log = self.log_path(id);
        let existed = meta.exists() || log.exists();
        for p in [meta, log] {
            match std::fs::remove_file(&p) {
                Ok(()) => {}
                Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
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

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 用临时文件 + rename 原子替换,避免半写状态。
///
/// 临时名带 pid + 进程内递增序号:同一文件可能有多个写者(driver 的 touch 与
/// 主循环的 rename),固定名会互相截断。
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
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
}
