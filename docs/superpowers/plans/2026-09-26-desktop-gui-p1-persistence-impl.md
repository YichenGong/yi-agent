# 桌面 GUI P1：会话持久化 + 历史侧栏 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让 app-server 的 thread 跨重启存活，并给 GUI 加一个历史侧栏，可列出、恢复（继续对话）、重命名、删除历史 thread。

**Architecture:** app-server 新增纯存储层 `thread_store.rs`（每 thread 一个只追加 `.jsonl` 日志 + 一个可变 `.meta.json`，落在 `<workdir>/.yi-agent/threads/`）；`thread/start` 改用 `thread-<uuid>` 并写 meta；driver 在每个 turn 结束时把本轮最终 Item + 核心 `Message` 快照追加落盘；新增 `thread/list` / `thread/resume` / `thread/rename` / `thread/delete` 四个方法。`thread/resume` 从磁盘载入 `messages` 经 `Agent::with_session` 恢复上下文，并把历史 Item 逐条以 `item/completed` 回放给前端。前端新增 `ThreadSidebar` 与 `Session.reset()`。

**Tech Stack:** Rust（tokio、serde、uuid、tempfile）；React 19 + TypeScript + Tailwind v4 + Vitest。

**设计文档:** `docs/superpowers/specs/2026-09-26-desktop-gui-p1-persistence-design.md`

---

## 实现前必读（零上下文工程师须知）

### 工作目录与命令

所有命令在 worktree 根目录 `/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/feat/desktop-gui-p1-persistence` 下执行。Rust 代码在 `yi-agent-rs/`，前端在 `desktop/`。

- Rust 测试：`cd yi-agent-rs && cargo test -p yi-agent-app-server`
- 前端测试：`cd desktop && npx vitest run`
- 前端构建：`cd desktop && npm run build`
- **提交前必须** `cd yi-agent-rs && cargo fmt --all`（Rust 改动）——否则 `just fmt-check` 会失败。
- 一次只在一个 worktree 跑 cargo；跑前 `ps aux | grep cargo` 确认无残留。
- Commit message 用 conventional commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。

### 关键既有事实（已核实，勿再假设）

1. **基线 server 从不 emit `userMessage` item。** `translate.rs` 只产生 `agentMessage` / `toolCall` 两种 Item（见 `src/translate.rs:174` 的 `on_event`）。前端 `App.tsx:65` 用 `session.addUserMessage(text)` **乐观插入**用户气泡（id 形如 `local-1`）。
   → 因此持久化时**必须**为每个 turn 合成一条 `UserMessage` item（id `user-<turn_id>`）放进 `items` 数组首部，否则 resume 回放会丢掉用户提问。回放时前端处于 reset 后的空状态，不会与乐观气泡重复。

2. **`Agent::run` 会把 user prompt / assistant / tool_result 都推进 `Session`**（`agent.rs:364/581/890`）。因此 turn 结束后 `agent.session().messages()` 就是完整核心历史，`TurnRecord.messages` 直接取它（全量快照，取最后一条记录即可）。

3. **`Item`（`protocol.rs:175`）与 `Message`（`message.rs:14`）都 `Serialize + Deserialize + Clone`**，可直接 JSON 往返。

4. **`Session` 从 `yi_agent_core` 根导出**（`yi_agent_core::Session`，`lib.rs:11`），有 `Session::new()` / `replace_messages(Vec<Message>)` / `messages()`。`Agent::with_session(Session)` 在 `agent.rs:296`。

5. **`ProviderRequest.messages: Vec<Message>`**（`provider.rs:18`）——测试可用它断言 resume 后上下文已恢复。

6. **`uuid = { version = "1", features = ["v4"] }`** 已在 workspace 依赖表（`yi-agent-rs/Cargo.toml:62`），但 app-server 未引用，需加到其 `Cargo.toml`。

7. **现有 server 主循环**：`run_with(reader, writer, cfg, permission_timeout, build_agent)`；`build_agent: Fn() -> anyhow::Result<BuiltAgent>`（`server.rs:70`）。`threads: HashMap<String, ThreadSession>`（`server.rs:119`），`next_thread` 计数器（`server.rs:120`）将被移除。错误码工厂在 `protocol.rs:69`（`not_initialized`/`unknown_thread`/`turn_in_progress`/`invalid_params`/`internal`）。

8. **driver 直接调用点**：测试 `driver_ignores_interrupt_tagged_with_other_turn`（`server.rs:1238`）、`driver_reports_finished_when_writer_fails`（`:1303`）、`driver_uses_unique_item_ids_across_turns`（`:1344`）、`stale_interrupt_does_not_cancel_turn_during_approval`（`:1689`）——新增 driver 参数后这 4 处都要改。

9. **前端 `Session`**（`desktop/src/lib/session.ts`）：`items` 是**同一个数组实例被就地修改**，`App` 用 `force(v=>v+1)` 触发重渲染。`reset()` 需要重建数组。

10. **前端 `Notification` 类型**（`desktop/src/lib/protocol.ts:41`）已有 `thread/started`，无需新增通知类型。

---

## Task 1: `thread_store.rs` — 类型 + create + append_turn + load

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`
- Test: 同文件内 `#[cfg(test)] mod tests`

**Step 1: 加 uuid 依赖**

在 `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` 的 `[dependencies]` 段（`serde` 行附近）加一行：

```toml
uuid.workspace = true
```

**Step 2: 写失败测试**

创建 `thread_store.rs`，先只写类型、空实现与测试：

```rust
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
}

impl ThreadStore {
    pub fn new(workdir: &Path) -> Self {
        Self {
            root: workdir.join(".yi-agent").join("threads"),
        }
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.meta.json"))
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.root.join(format!("{id}.jsonl"))
    }

    pub fn create(&self, _meta: &ThreadMeta) -> io::Result<()> {
        unimplemented!()
    }

    pub fn append_turn(&self, _id: &str, _turn: &TurnLine) -> io::Result<()> {
        unimplemented!()
    }

    pub fn load(&self, _id: &str) -> io::Result<Option<LoadedThread>> {
        unimplemented!()
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
}
```

**Step 3: 注册模块**

在 `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` 加 `pub mod thread_store;`（按字母序放在 `pub mod session;` 与 `pub mod translate;` 之间）。

**Step 4: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_store`
Expected: FAIL（`unimplemented!()` panic）。

**Step 5: 实现 create / append_turn / load**

在 `thread_store.rs` 的 `impl ThreadStore` 里替换三个 `unimplemented!()`，并加自由函数：

```rust
    pub fn create(&self, meta: &ThreadMeta) -> io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(meta).map_err(io_err)?;
        write_atomic(&self.meta_path(&meta.thread_id), &bytes)
    }

    pub fn append_turn(&self, id: &str, turn: &TurnLine) -> io::Result<()> {
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
        let log = self.log_path(id);
        let meta_path = self.meta_path(id);
        if !log.exists() && !meta_path.exists() {
            return Ok(None);
        }

        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        if let Ok(text) = std::fs::read_to_string(&log) {
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
```

文件末尾（`impl` 之后、`#[cfg(test)]` 之前）加自由函数：

```rust
fn io_err(e: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// epoch 毫秒。
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 用临时文件 + rename 原子替换,避免半写状态。
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
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
```

**Step 6: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_store`
Expected: PASS（4 个测试）。

**Step 7: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/Cargo.toml yi-agent-rs/crates/yi-agent-app-server/src/lib.rs yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): add thread store with create/append/load"
```

---

## Task 2: `thread_store.rs` — list + rename + touch + exists + delete

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`

**Step 1: 写失败测试**

在 `thread_store.rs` 的 `mod tests` 里追加：

```rust
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
        s.append_turn(
            "thread-a",
            &turn(vec![], vec![Message::user("hi")]),
        )
        .unwrap();
        let log_before = std::fs::read_to_string(s.log_path("thread-a")).unwrap();

        assert!(s.rename("thread-a", "new title").unwrap());
        assert_eq!(s.load("thread-a").unwrap().unwrap().meta.title.as_deref(), Some("new title"));
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
        // 第二次删除:文件已不在,返回 false(幂等,不报错)。
        assert!(!s.delete("thread-a").unwrap());
    }

    #[test]
    fn exists_true_if_either_file_present() {
        let (_d, s) = store();
        assert!(!s.exists("thread-a"));
        s.create(&meta("thread-a")).unwrap();
        assert!(s.exists("thread-a"));
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_store`
Expected: FAIL（方法不存在，编译错误）。

**Step 3: 实现**

在 `impl ThreadStore` 里追加：

```rust
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
            let Some(id) = name.strip_suffix(".meta.json") else {
                continue;
            };
            match std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
            {
                Some(m) => out.push(m),
                None => eprintln!("[app-server] skipping corrupt meta: {name}"),
            }
        }
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(out)
    }

    /// 只重写 meta 的 `title` + `updated_at`。返回 false 表示 thread 不存在。
    pub fn rename(&self, id: &str, title: &str) -> io::Result<bool> {
        let path = self.meta_path(id);
        let Some(mut meta) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
        else {
            return Ok(false);
        };
        meta.title = Some(title.to_string());
        meta.updated_at = now_millis();
        let bytes = serde_json::to_vec_pretty(&meta).map_err(io_err)?;
        write_atomic(&path, &bytes)?;
        Ok(true)
    }

    /// 每 turn 完成时调用:更新 `updated_at`,并在 `title` 仍为 `None` 时用
    /// `title_hint`(本轮 prompt)填充。thread 不存在时静默返回。
    pub fn touch(&self, id: &str, title_hint: Option<&str>) -> io::Result<()> {
        let path = self.meta_path(id);
        let Some(mut meta) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<ThreadMeta>(&t).ok())
        else {
            return Ok(());
        };
        meta.updated_at = now_millis();
        if meta.title.is_none() {
            if let Some(hint) = title_hint {
                let t = title_from(hint);
                if !t.is_empty() {
                    meta.title = Some(t);
                }
            }
        }
        let bytes = serde_json::to_vec_pretty(&meta).map_err(io_err)?;
        write_atomic(&path, &bytes)
    }

    /// thread 是否已知(meta 或 log 任一存在)。
    pub fn exists(&self, id: &str) -> bool {
        self.meta_path(id).exists() || self.log_path(id).exists()
    }

    /// 删除两个文件;文件缺失不算错误。返回删除前是否存在。
    pub fn delete(&self, id: &str) -> io::Result<bool> {
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
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_store`
Expected: PASS（11 个测试）。

**Step 5: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): add thread store list/rename/touch/delete"
```

---

## Task 3: `thread_store.rs` — 容错(损坏行、meta 缺失)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`

**Step 1: 写失败测试**

在 `mod tests` 追加：

```rust
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
        assert_eq!(loaded.items.len(), 2, "corrupt line must be skipped, not fatal");
        assert_eq!(loaded.messages, vec![Message::user("still here")]);
    }

    #[test]
    fn load_rebuilds_meta_when_missing() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.append_turn(
            "thread-a",
            &turn(vec![], vec![Message::user("recover me")]),
        )
        .unwrap();
        std::fs::remove_file(s.meta_path("thread-a")).unwrap();

        let loaded = s.load("thread-a").unwrap().expect("log still present");
        assert_eq!(loaded.meta.title.as_deref(), Some("recover me"));
        assert_eq!(loaded.meta.thread_id, "thread-a");
    }

    #[test]
    fn list_skips_orphan_and_corrupt_meta() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        // 孤立 .jsonl(无 meta)不应出现在 list 里。
        s.append_turn("thread-orphan", &turn(vec![], vec![])).unwrap();
        // 损坏 meta 也不应出现。
        std::fs::write(s.meta_path("thread-bad"), "{ not json").unwrap();

        let ids: Vec<String> = s.list().unwrap().into_iter().map(|m| m.thread_id).collect();
        assert_eq!(ids, vec!["thread-a"]);
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_store`
Expected: `load_skips_corrupt_log_lines` 与 `load_rebuilds_meta_when_missing` 应当已通过（Task 1 已实现容错）；`list_skips_orphan_and_corrupt_meta` 也通过。若全绿，说明 Task 1/2 的容错已到位——**保留这些测试**（它们是回归锁），继续 Step 3。

**Step 3: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "test(app-server): lock thread store fault tolerance"
```

---

## Task 4: server — agent 工厂接受 `Option<Session>`；`thread/start` 用 uuid + 写 meta

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 改工厂签名**

在 `server.rs`：

1. `run()`（`:46`）里的闭包改为接受 session：

```rust
    let cfg_for_factory = cfg.clone();
    run_with(reader, writer, cfg, PERMISSION_TIMEOUT, move |session| {
        let built = yi_agent_runtime::bootstrap::bootstrap_agent(
            &cfg_for_factory,
            yi_agent_runtime::bootstrap::PermissionMode::Interactive,
        )?;
        Ok(BuiltAgent {
            agent: apply_session(built.agent, session),
            decision_tx: built.decision_tx,
        })
    })
    .await
```

2. `run_with` 的泛型约束（`:80`）改为：

```rust
    F: Fn(Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> + Send + 'static,
```

3. 加自由函数（放在 `ok_response` 附近）：

```rust
/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
fn apply_session(
    agent: yi_agent_core::Agent,
    session: Option<yi_agent_core::Session>,
) -> yi_agent_core::Agent {
    match session {
        Some(s) => agent.with_session(s),
        None => agent,
    }
}
```

**Step 2: 改 `thread/start`**

在 `run_with` 顶部（`let writer = ...` 之后）加 store：

```rust
    let store = Arc::new(crate::thread_store::ThreadStore::new(&cfg.workdir));
```

删除 `let mut next_thread: u64 = 1;`（`:120`）。

把 `"thread/start"` 分支（`:216`）的开头与 agent 构造改为：

```rust
                    "thread/start" => {
                        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

                        let BuiltAgent { agent, decision_tx } = match build_agent(None) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?;
                                continue;
                            }
                        };

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);

                        let cwd = cfg.workdir.display().to_string();
                        let model = cfg.model.clone();

                        let now = crate::thread_store::now_millis();
                        let meta = crate::thread_store::ThreadMeta {
                            thread_id: thread_id.clone(),
                            cwd: cwd.clone(),
                            model: model.clone(),
                            created_at: now,
                            updated_at: now,
                            title: None,
                        };
                        if let Err(e) = store.create(&meta) {
                            // 持久化是尽力而为:写失败不阻断 thread 创建。
                            eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
                        }

                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                prompt_tx,
                                interrupt_tx,
                            },
                        );
```

driver spawn 调用（`:250`）保持原样，但**Task 5 会加 store 参数**——本 task 先不动，只改工厂实参：`build_agent(None)`。

**Step 3: 修测试里的工厂**

把测试模块里所有工厂改为接受 session：

```rust
    fn build_test_agent(session: Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> {
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    Arc::new(MockProvider),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    yi_agent_core::AgentConfig::default(),
                ),
                session,
            ),
            decision_tx: None,
        })
    }
```

对 `build_slow_agent` / `build_delayed_agent` / `build_permission_agent` 同样加 `session: Option<yi_agent_core::Session>` 参数并包 `apply_session(agent, session)`。

`Harness::with_factory`（`:788`）的约束同步改为 `F: Fn(Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> + Send + 'static`，并保持 `Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)` 不变（函数名现在即符合签名）。

**新增 `Harness::with_config`**（持久化测试需要自定义 `cfg.workdir`）：把 `with_factory` 拆成两层——

```rust
        fn with_factory<F>(build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> + Send + 'static,
        {
            Self::with_config(test_config(), build, permission_timeout)
        }

        /// 用自定义 config + agent 工厂搭建 harness(持久化测试需要自定义 workdir)。
        fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> + Send + 'static,
        {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let handle = tokio::spawn(run_with(server_r, server_w, cfg, permission_timeout, build));
            Self {
                client_w,
                client_r: BufReader::new(client_r),
                handle,
            }
        }
```

`agent_factory_failure_returns_internal_error`（`:1007`）的闭包改为 `|_s: Option<yi_agent_core::Session>| Err::<BuiltAgent, _>(anyhow::anyhow!("boom"))`。

`eof_exits_gracefully`（`:954`）/`oversized_frame_returns_err`（`:1050`）里的 `build_test_agent` 直接传函数名即可（签名已匹配）。

**Step 4: 加新测试**

在 `mod tests` 追加：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_allocates_uuid_id_and_writes_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);

        let tid = start_thread(&mut h).await;
        assert!(tid.starts_with("thread-"), "expected thread-<uuid>, got {tid}");
        assert!(tid.len() > "thread-".len() + 8, "expected a uuid suffix: {tid}");

        let meta = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.meta.json"));
        assert!(meta.exists(), "thread/start must write meta at {}", meta.display());

        h.shutdown().await;
    }
```

**Step 5: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（原 69 + 新增 1）。

**Step 6: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): allocate uuid thread ids and write meta"
```

---

## Task 5: server — driver 每 turn 落盘

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 加 driver 参数**

`run_thread_driver`（`:480`）签名加一个参数（放在 `perm_seq` 之后）：

```rust
    perm_seq: Arc<AtomicU64>,
    store: Arc<crate::thread_store::ThreadStore>,
) where
```

在 `thread/start` 的 spawn 调用（`:250`）末尾加 `Arc::clone(&store)`。Task 7 的 resume spawn 也要加。

**Step 2: 在 driver 循环里收集 + 落盘**

在 `while let Some(TurnPrompt { turn_id, prompt }) = prompt_rx.recv().await {` 之后、`translator.set_turn(...)` 之前，加本轮累加器：

```rust
        // 本轮累加器:最终 item 与最近一次用量(用于落盘)。
        let mut completed_items: Vec<crate::protocol::Item> = Vec::new();
        let mut last_usage: Option<crate::thread_store::TurnUsage> = None;
        let prompt_for_title = prompt.clone();
```

把 `agent.run(prompt)` 改为 `agent.run(prompt)`（不变，因为已 clone 了标题用的副本）。

在 stream 循环的 `Some(e) => { ... }` 分支里，改写成先捕获 usage、再在翻译出的通知里收集 `ItemCompleted`：

```rust
                        Some(e) => {
                            if let yi_agent_core::AgentEvent::Usage { model, usage } = &e {
                                last_usage = Some(crate::thread_store::TurnUsage {
                                    model: model.clone(),
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                });
                            }
                            for n in translator.on_event(e) {
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
                                }
                                if write_notification(&writer, &n).await.is_err() {
                                    let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                    return;
                                }
                            }
                        }
```

在内层 `loop` 结束之后、`let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;`（`:597`）**之前**，加落盘：

```rust
        // 落盘(尽力而为):合成一条 userMessage item 放在首部,再拼本轮最终 item。
        // 基线 server 不 emit userMessage,必须在此补齐,否则 resume 会丢用户提问。
        let mut items =
            Vec::with_capacity(completed_items.len() + 1);
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{turn_id}"),
            text: prompt_for_title.clone(),
        });
        items.extend(completed_items.drain(..));

        let record = crate::thread_store::TurnLine::Turn {
            items,
            usage: last_usage.take(),
            messages: agent.session().messages().to_vec(),
        };
        if let Err(e) = store.append_turn(&thread_id, &record) {
            eprintln!("[app-server] failed to persist turn {turn_id} of {thread_id}: {e}");
        }
        if let Err(e) = store.touch(&thread_id, Some(prompt_for_title.as_str())) {
            eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
        }
```

> 注意：`agent.run()` 的 `Err(e)` 早退分支（`:501`）与 writer 失败 `return` 分支不落盘——设计明确持久化为尽力而为。

**Step 3: 修直接调用 driver 的 4 个测试**

给 `driver_ignores_interrupt_tagged_with_other_turn`（`:1238`）、`driver_reports_finished_when_writer_fails`（`:1303`）、`driver_uses_unique_item_ids_across_turns`（`:1344`）、`stale_interrupt_does_not_cancel_turn_during_approval`（`:1689`）的 `run_thread_driver(...)` 调用末尾各加一个 store 参数。用临时目录：

```rust
            Arc::new(crate::thread_store::ThreadStore::new(std::path::Path::new(
                "/tmp/yi-agent-app-server-test",
            ))),
```

（这些测试只关心通知序列，不检查落盘；写到 `/tmp` 下的固定路径无妨。更干净的做法是各测试内建 `TempDir` 并保活——若出现并发写冲突，改用它。）

**Step 4: 加落盘测试**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn turn_completion_persists_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = {
            let mut c = test_config();
            c.workdir = dir.path().to_path_buf();
            c
        };
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        let text = std::fs::read_to_string(&log).expect("turn must be persisted");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "one turn = one line");

        // 第一行必须含合成的 userMessage item 与 agent 回复。
        assert!(text.contains(r#""type":"userMessage""#), "missing user item: {text}");
        assert!(text.contains("hello"), "missing prompt text: {text}");
        assert!(text.contains(r#""type":"agentMessage""#), "missing agent item: {text}");
        assert!(text.contains(r#""role":"User""#), "missing core message: {text}");

        h.shutdown().await;
    }
```

> 上面 `let store = yi_agent_core::Session::new();` 两行是占位噪音，**删掉**（不要写）。直接读文件断言即可。

**Step 5: 跑测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS。

**Step 6: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): persist each completed turn to disk"
```

---

## Task 6: server — `thread/list`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 写失败测试**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_empty_returns_empty_array() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["threads"], serde_json::json!([]));
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_returns_created_threads_newest_first() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_a = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let tid_b = read_thread_start_response(&mut h, 3).await;

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/list must respond");
        let threads = v["result"]["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 2);
        // 两次 start 可能同毫秒;只断言集合与字段存在。
        let ids: std::collections::HashSet<&str> =
            threads.iter().map(|t| t["thread_id"].as_str().unwrap()).collect();
        assert!(ids.contains(tid_a.as_str()) && ids.contains(tid_b.as_str()));
        assert!(threads[0]["created_at"].is_number());
        assert!(threads[0]["updated_at"].is_number());
        h.shutdown().await;
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_list`
Expected: FAIL（`-32601 method not found`）。

**Step 3: 实现**

在 `match method.as_str()` 里、`"thread/start"` 之前插入：

```rust
                    "thread/list" => match store.list() {
                        Ok(metas) => {
                            let threads: Vec<serde_json::Value> = metas
                                .iter()
                                .map(|m| {
                                    json!({
                                        "thread_id": m.thread_id,
                                        "cwd": m.cwd,
                                        "model": m.model,
                                        "created_at": m.created_at,
                                        "updated_at": m.updated_at,
                                        "title": m.title,
                                    })
                                })
                                .collect();
                            write_response(&writer, ok_response(id, json!({ "threads": threads })))
                                .await?;
                        }
                        Err(e) => {
                            write_response(&writer, err_response(id, RpcError::internal(e.to_string())))
                                .await?;
                        }
                    },
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_list`
Expected: PASS。

**Step 5: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add thread/list"
```

---

## Task 7: server — `thread/resume`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 写失败测试**

需要能观察 provider 收到的历史。加一个 recording provider：

```rust
    /// 记录每次调用收到的 message 数,并回显 `n=<count>` 作为 agent 文本。
    struct RecordingProvider {
        seen: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl yi_agent_core::Provider for RecordingProvider {
        async fn call_stream(
            &self,
            req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let n = req.messages.len();
            self.seen.lock().unwrap().push(n);
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta(format!("n={n}")),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }
```

测试：

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_replays_history_and_restores_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_factory = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>| {
            Ok(BuiltAgent {
                agent: apply_session(
                    yi_agent_core::Agent::new(
                        Arc::new(RecordingProvider {
                            seen: Arc::clone(&seen_factory),
                        }),
                        Arc::new(yi_agent_core::ToolRegistry::new()),
                        yi_agent_core::AgentConfig::default(),
                    ),
                    session,
                ),
                decision_tx: None,
            })
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // turn 1
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        // resume:期望 thread/started → item/completed(含 user + agent) → 响应
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut replayed: Vec<String> = Vec::new();
        let mut resumed = false;
        for _ in 0..12 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("thread/started") => {}
                Some("item/completed") => {
                    replayed.push(v["params"]["item"]["type"].as_str().unwrap().to_string());
                }
                _ => {}
            }
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["result"]["thread_id"], tid);
                resumed = true;
                break;
            }
        }
        assert!(resumed, "thread/resume must respond");
        assert!(
            replayed.contains(&"userMessage".to_string()),
            "replay must include the user item: {replayed:?}"
        );
        assert!(
            replayed.contains(&"agentMessage".to_string()),
            "replay must include the agent item: {replayed:?}"
        );

        // turn 2:provider 应看到比 turn 1 更多的 message(上下文已恢复)
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let counts = seen.lock().unwrap().clone();
        assert_eq!(counts.len(), 2, "expected two provider calls: {counts:?}");
        assert!(
            counts[1] > counts[0],
            "resumed turn must carry prior context: {counts:?}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{"threadId":"nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_resume`
Expected: FAIL（`-32601`）。

**Step 3: 实现**

在 `"thread/start"` 之后插入：

```rust
                    "thread/resume" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let loaded = match store.load(&thread_id) {
                            Ok(Some(l)) => l,
                            Ok(None) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };

                        // 若该 thread 已在内存,先中断其活跃 turn,再以磁盘状态重建。
                        if let Some(s) = threads.get(&thread_id) {
                            if let Some(tid) = s.active_turn_id.clone() {
                                let _ = s.interrupt_tx.try_send(tid);
                            }
                        }

                        // meta 缺 cwd/model 时(损坏重建)用当前配置兜底。
                        let cwd = if loaded.meta.cwd.is_empty() {
                            cfg.workdir.display().to_string()
                        } else {
                            loaded.meta.cwd.clone()
                        };
                        let model = if loaded.meta.model.is_empty() {
                            cfg.model.clone()
                        } else {
                            loaded.meta.model.clone()
                        };

                        let mut session = yi_agent_core::Session::new();
                        session.replace_messages(loaded.messages.clone());

                        let BuiltAgent { agent, decision_tx } = match build_agent(Some(session)) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                prompt_tx,
                                interrupt_tx,
                            },
                        );

                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            driver_writer,
                            driver_turn_tx,
                            decision_tx,
                            Arc::clone(&pending),
                            permission_timeout,
                            Arc::clone(&perm_seq),
                            Arc::clone(&store),
                        ));

                        // 回放:thread/started → 每条历史 item/completed → 最近用量 → 响应。
                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        for item in loaded.items {
                            write_notification(
                                &writer,
                                &Notification::ItemCompleted {
                                    thread_id: thread_id.clone(),
                                    item,
                                },
                            )
                            .await?;
                        }
                        if let Some(u) = loaded.usage {
                            write_notification(
                                &writer,
                                &Notification::TokenUsage {
                                    thread_id: thread_id.clone(),
                                    model: u.model,
                                    input_tokens: u.input_tokens,
                                    output_tokens: u.output_tokens,
                                },
                            )
                            .await?;
                        }
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_resume`
Expected: PASS。

**Step 5: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add thread/resume with history replay"
```

---

## Task 8: server — `thread/rename` + `thread/delete`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 写失败测试**

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_updates_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"my chat"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert!(v.get("error").is_none(), "rename must succeed: {v}");

        let meta = std::fs::read_to_string(
            dir.path().join(".yi-agent/threads").join(format!("{tid}.meta.json")),
        )
        .unwrap();
        assert!(meta.contains("my chat"), "meta must carry the title: {meta}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_rejects_empty_title() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"   "}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602, "blank title must be rejected: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/rename","params":{"threadId":"nope","title":"x"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_removes_files_and_unknown_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(
            !dir.path().join(".yi-agent/threads").join(format!("{tid}.meta.json")).exists(),
            "meta must be gone"
        );

        // 再次删除:thread 已完全未知 → -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_active_thread_removes_it_from_memory() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等 turn 活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut deleted = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert!(v.get("error").is_none(), "delete must succeed: {v}");
                deleted = true;
                break;
            }
        }
        assert!(deleted, "thread/delete must respond");

        // 删除后该 thread 已不在内存:再发 turn 应得 -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"x"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                assert_eq!(v["error"]["code"], -32011, "deleted thread must be unknown: {v}");
                break;
            }
        }
        h.shutdown().await;
    }
```

**Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_rename thread_delete`
Expected: FAIL（`-32601`）。

**Step 3: 实现**

在 `"thread/resume"` 之后插入：

```rust
                    "thread/rename" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let title = req
                            .params
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        if title.is_empty() {
                            write_response(
                                &writer,
                                err_response(id, RpcError::invalid_params("title must not be empty")),
                            )
                            .await?;
                            continue;
                        }
                        match store.rename(&thread_id, &title) {
                            Ok(true) => {
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/delete" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let in_memory = threads.contains_key(&thread_id);
                        let on_disk = store.exists(&thread_id);
                        if !in_memory && !on_disk {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        // 活跃 thread:先中断,再从内存移除(drop prompt_tx 让 driver 收尾)。
                        if let Some(s) = threads.get(&thread_id) {
                            if let Some(tid) = s.active_turn_id.clone() {
                                let _ = s.interrupt_tx.try_send(tid);
                            }
                        }
                        threads.remove(&thread_id);
                        if let Err(e) = store.delete(&thread_id) {
                            eprintln!("[app-server] failed to delete thread files for {thread_id}: {e}");
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
```

**Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（全绿）。

**Step 5: fmt + commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add thread/rename and thread/delete"
```

---

## Task 9: 前端 — `Session.reset()` + `ThreadSummary` 类型

**Files:**
- Modify: `desktop/src/lib/session.ts`
- Modify: `desktop/src/lib/session.test.ts`
- Modify: `desktop/src/lib/protocol.ts`

**Step 1: 写失败测试**

在 `session.test.ts` 追加：

```ts
  it("reset clears all state and starts a fresh items array", () => {
    const s = new Session();
    const before = s.items;
    s.addUserMessage("hi");
    s.apply({ method: "turn/started", params: { thread_id: "t", turn_id: "u1" } });
    s.apply({
      method: "thread/tokenUsage/updated",
      params: { thread_id: "t", model: "m", input_tokens: 1, output_tokens: 2 },
    });
    s.apply({ method: "error", params: { message: "boom" } });

    s.reset();

    expect(s.items).not.toBe(before);
    expect(s.items).toHaveLength(0);
    expect(s.turnActive).toBe(false);
    expect(s.lastStatus).toBeNull();
    expect(s.lastError).toBeNull();
    expect(s.usage).toBeNull();
  });
```

**Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run session`
Expected: FAIL（`s.reset is not a function`）。

**Step 3: 实现 reset**

在 `desktop/src/lib/session.ts` 的 `class Session` 里、`addUserMessage` 之前加：

```ts
  /**
   * Drop all accumulated state. Must be called *before* issuing
   * `thread/resume`, because replay notifications can arrive before the resume
   * response — resetting after would wipe the freshly replayed history.
   *
   * Replaces `items` with a new array (rather than truncating in place) so
   * React's identity-based memoization notices the change.
   */
  reset(): void {
    this.items = [];
    this.turnActive = false;
    this.lastStatus = null;
    this.lastError = null;
    this.usage = null;
  }
```

**Step 4: 加 `ThreadSummary` 类型**

在 `desktop/src/lib/protocol.ts` 末尾加：

```ts
/** A persisted thread as returned by `thread/list`. */
export interface ThreadSummary {
  thread_id: string;
  cwd: string;
  model: string;
  created_at: number;
  updated_at: number;
  title: string | null;
}
```

**Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run session`
Expected: PASS（11 个）。

**Step 6: commit**

```bash
git add desktop/src/lib/session.ts desktop/src/lib/session.test.ts desktop/src/lib/protocol.ts
git commit -m "feat(desktop): add Session.reset and ThreadSummary type"
```

---

## Task 10: 前端 — `ThreadSidebar` 组件

**Files:**
- Create: `desktop/src/components/ThreadSidebar.tsx`

**Step 1: 创建组件**

（组件渲染由 `tsc` + `vite build` 验证；无 jsdom，不写单测，沿用基线策略。）

```tsx
import { useState } from "react";
import type { ThreadSummary } from "../lib/protocol";

/** Compact relative time, e.g. "3m", "2h", "5d". */
function relativeTime(ms: number): string {
  const diff = Date.now() - ms;
  const sec = Math.floor(diff / 1000);
  if (sec < 60) return "now";
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min}m`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour}h`;
  const day = Math.floor(hour / 24);
  return `${day}d`;
}

export function ThreadSidebar({
  threads,
  currentId,
  busy,
  onSelect,
  onRename,
  onDelete,
  onNew,
}: {
  threads: ThreadSummary[];
  currentId: string | null;
  busy: boolean;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
  onNew: () => void;
}) {
  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState("");

  const commit = (id: string) => {
    const title = draft.trim();
    if (title) onRename(id, title);
    setEditingId(null);
  };

  return (
    <aside className="flex w-64 shrink-0 flex-col border-r border-neutral-800 bg-neutral-900">
      <button
        type="button"
        onClick={onNew}
        disabled={busy}
        className="m-2 rounded-md bg-neutral-800 px-3 py-2 text-left text-sm text-neutral-100 hover:bg-neutral-700 disabled:opacity-50"
      >
        + New thread
      </button>
      <div className="flex-1 overflow-y-auto">
        {threads.map((t) => {
          const active = t.thread_id === currentId;
          return (
            <div
              key={t.thread_id}
              onClick={() => !busy && !editingId && onSelect(t.thread_id)}
              className={`group flex items-center justify-between gap-1 px-3 py-2 text-sm ${
                active ? "bg-neutral-800 text-neutral-100" : "text-neutral-400 hover:bg-neutral-800/50"
              } ${busy ? "cursor-not-allowed opacity-60" : "cursor-pointer"}`}
            >
              {editingId === t.thread_id ? (
                <input
                  autoFocus
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onBlur={() => commit(t.thread_id)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") commit(t.thread_id);
                    if (e.key === "Escape") setEditingId(null);
                  }}
                  className="w-full rounded bg-neutral-950 px-1 py-0.5 text-sm text-neutral-100 outline-none"
                />
              ) : (
                <>
                  <div className="min-w-0 flex-1">
                    <div
                      className="truncate"
                      title={t.title ?? t.thread_id}
                      onDoubleClick={() => {
                        if (busy) return;
                        setEditingId(t.thread_id);
                        setDraft(t.title ?? "");
                      }}
                    >
                      {t.title ?? "(untitled)"}
                    </div>
                    <div className="text-xs text-neutral-600">{relativeTime(t.updated_at)}</div>
                  </div>
                  <button
                    type="button"
                    disabled={busy}
                    onClick={(e) => {
                      e.stopPropagation();
                      onDelete(t.thread_id);
                    }}
                    title="Delete"
                    className="hidden shrink-0 rounded px-1 text-neutral-500 hover:text-red-400 group-hover:block disabled:opacity-50"
                  >
                    ×
                  </button>
                </>
              )}
            </div>
          );
        })}
        {threads.length === 0 && (
          <p className="px-3 py-2 text-xs text-neutral-600">No history yet.</p>
        )}
      </div>
    </aside>
  );
}
```

**Step 2: 构建验证**

Run: `cd desktop && npm run build`
Expected: 成功（`tsc` 无错 + vite 产出）。

**Step 3: commit**

```bash
git add desktop/src/components/ThreadSidebar.tsx
git commit -m "feat(desktop): add ThreadSidebar component"
```

---

## Task 11: 前端 — `App.tsx` 接线 + 布局

**Files:**
- Modify: `desktop/src/App.tsx`

**Step 1: 改 App**

要点（完整替换 `App` 组件体与 `useEffect`）：

1. 新增状态与刷新函数：

```tsx
  const [threads, setThreads] = useState<ThreadSummary[]>([]);
  const busy = session.turnActive;

  const refreshThreads = async () => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<{ threads: ThreadSummary[] }>("thread/list", {});
      setThreads(r.threads);
    } catch {
      // 列表刷新失败不打断对话;下一次事件会再试。
    }
  };
```

2. `onNotification` 里在 `turn/completed` 后刷新列表（标题/时间随之更新）：

```tsx
    client.onNotification((n) => {
      session.apply(n);
      force((v) => v + 1);
      if (n.method === "turn/completed") void refreshThreads();
    });
```

3. 挂载流程：`initialize` → `thread/list` → 有历史则 resume 最近一条，否则 `thread/start`：

```tsx
    (async () => {
      await client.request("initialize", {});
      const list = await client.request<{ threads: ThreadSummary[] }>("thread/list", {});
      setThreads(list.threads);
      if (list.threads.length > 0) {
        await resumeThread(list.threads[0].thread_id);
      } else {
        await newThread();
      }
      setStatus("connected");
    })().catch((e) => {
      const msg = formatError(e);
      session.lastError = msg;
      setStatus(`error: ${msg}`);
    });
```

4. 三个操作：

```tsx
  const resumeThread = async (threadId: string) => {
    const c = clientRef.current;
    if (!c || session.turnActive) return;
    // 必须同步 reset:回放通知可能先于 resume 响应到达。
    session.reset();
    force((v) => v + 1);
    try {
      const t = await c.request<ThreadInfo & { thread_id: string }>("thread/resume", {
        threadId,
      });
      setThreadId(t.thread_id);
      setThreadInfo({ cwd: t.cwd, model: t.model });
    } catch (e) {
      session.lastError = formatError(e);
      setThreadId(null);
      setThreadInfo(null);
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const newThread = async () => {
    const c = clientRef.current;
    if (!c || session.turnActive) return;
    session.reset();
    force((v) => v + 1);
    try {
      const t = await c.request<ThreadInfo & { thread_id: string }>("thread/start", {});
      setThreadId(t.thread_id);
      setThreadInfo({ cwd: t.cwd, model: t.model });
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const renameThread = async (id: string, title: string) => {
    const c = clientRef.current;
    if (!c) return;
    const prev = threads;
    setThreads((ts) => ts.map((t) => (t.thread_id === id ? { ...t, title } : t)));
    try {
      await c.request("thread/rename", { threadId: id, title });
    } catch (e) {
      setThreads(prev);
      session.lastError = formatError(e);
      force((v) => v + 1);
    }
    await refreshThreads();
  };

  const deleteThread = async (id: string) => {
    const c = clientRef.current;
    if (!c || session.turnActive) return;
    try {
      await c.request("thread/delete", { threadId: id });
    } catch (e) {
      session.lastError = formatError(e);
      force((v) => v + 1);
      return;
    }
    if (id === threadId) {
      session.reset();
      setThreadId(null);
      setThreadInfo(null);
      force((v) => v + 1);
    }
    await refreshThreads();
  };
```

> `resumeThread` / `newThread` 在 `useEffect` 里被调用，但定义在其后——JS 函数声明提升对 `const` 箭头函数**不适用**。把 `refreshThreads` / `resumeThread` / `newThread` 定义**放在 `useEffect` 之前**（`clientRef` 已声明处之后），或在 effect 内用 `void (async () => { ... })()` 内联等价逻辑。推荐前者：把三个函数定义移到 `useEffect` 上方。`renameThread` / `deleteThread` 放哪都行。

5. 布局改为 `flex-row` + 侧栏：

```tsx
  return (
    <>
      <div
        className="flex h-screen flex-row bg-neutral-950 text-neutral-100"
        inert={approval !== null}
      >
        <ThreadSidebar
          threads={threads}
          currentId={threadId}
          busy={busy}
          onSelect={resumeThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onNew={newThread}
        />
        <div className="flex min-w-0 flex-1 flex-col">
          <StatusBar
            cwd={threadInfo?.cwd ?? null}
            model={threadInfo?.model ?? null}
            status={status}
            usage={session.usage}
          />
          <ChatView items={session.items} error={session.lastError} />
          <MessageInput turnActive={session.turnActive} onSend={send} onInterrupt={interrupt} />
        </div>
      </div>
      {approval && (
        <ApprovalDialog
          key={approval.id}
          request={approval}
          onDecide={async (decision) => {
            try {
              await clientRef.current?.respond(approval.id, decision);
            } catch (e) {
              session.lastError = formatError(e);
              force((v) => v + 1);
            } finally {
              setApproval(null);
            }
          }}
        />
      )}
    </>
  );
```

6. 导入：`import { ThreadSidebar } from "./components/ThreadSidebar";` 与 `import type { ApprovalRequest, ThreadSummary } from "./lib/protocol";`。

> `useEffect` 的依赖数组保持 `[session]`；`refreshThreads` 等只依赖 `clientRef`（稳定）与 `setState`（稳定），首渲染捕获的闭包可安全复用。

**Step 2: 构建验证**

Run: `cd desktop && npm run build && npx vitest run`
Expected: 构建成功；vitest 17 个全绿。

**Step 3: commit**

```bash
git add desktop/src/App.tsx
git commit -m "feat(desktop): wire history sidebar into App"
```

---

## Task 12: 文档同步

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`
- Modify: `desktop/README.md`（若有测试计数/能力清单）

**Step 1: 更新 app-server 模块文件**

在 `## Features` 追加一条（保持"可验证判据 = 代码位置"风格）：

```markdown
- [x] 会话持久化 + 历史方法（`thread_store.rs`：每 thread 一个只追加 `.jsonl` + 一个可变 `.meta.json`，落 `<workdir>/.yi-agent/threads/`；`thread/start` 分配 `thread-<uuid>` 并写 meta；driver 每 turn 落盘最终 Item + 核心 `Message` 快照；新增 `thread/list` / `thread/resume`（回放 + `Agent::with_session` 恢复上下文）/ `thread/rename` / `thread/delete`）— `src/thread_store.rs:1` / `src/server.rs:216`（thread/start）/ `src/server.rs:480`（driver 落盘）
```

更新 `**验证命令：**` 里的测试数（`69` → 实际值）。

**Step 2: 更新 desktop 模块文件**

- 把 `不做什么（延后）` 里的 `- 不做会话持久化 / 历史侧栏` **删掉**（已做）。
- `## Features` 追加：

```markdown
- [x] 会话持久化 + 历史侧栏（列表 / 恢复并继续对话 / 双击内联重命名 / 删除 / New thread / 当前高亮）— `desktop/src/lib/session.ts:25`（`reset`）/ `desktop/src/components/ThreadSidebar.tsx:1` / `desktop/src/App.tsx`（`thread/list` 填充 + `resumeThread` 前置同步 `reset` 防回放竞态）
```

- 更新 `**验证命令：**` 里的前端测试数（17 → 实际值）。

**Step 3: 更新 README 索引**

`docs/project-management/README.md`：若 `yi-agent-app-server` 与 `desktop` 的"完成 / 总计"计数变化，同步更新（desktop 从 `10 / 11` 起，本阶段可能变 `11 / 11` 或新增一行 feature）。

**Step 4: 跑全量验证**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
cd ../desktop && npx vitest run && npm run build
```
Expected: 全绿。

**Step 5: commit**

```bash
git add docs/project-management/ desktop/README.md
git commit -m "docs: record P1 thread persistence and history sidebar"
```

---

## 最终验证（全部任务完成后）

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server && cargo test -p yi-agent-core
cd ../desktop && npx vitest run && npm run build
```

人工冒烟（可选，需 GUI 环境）：

```bash
cd desktop && npm run sidecar && npm run tauri dev
# 1. 发一条消息 → 侧栏出现条目（标题=消息前 30 字）
# 2. 重启 app → 侧栏仍在；点击条目 → 历史完整回放
# 3. 恢复后继续发消息 → agent 记得前文
# 4. 双击重命名 → 重启后仍是新标题
# 5. 删除 → 条目与磁盘文件消失
```
