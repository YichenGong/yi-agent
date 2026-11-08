//! TUI 侧的会话存储封装。
//!
//! 只做三件事：持锁、把一轮的产物写进 `.jsonl`、把 `/clear` `/compact`
//! 翻译成存储层操作。所有真正的落盘语义都在 `ThreadStore`，这里不重复实现。

use std::path::Path;
use std::sync::Arc;

use yi_agent_app_server::protocol::Item;
use yi_agent_app_server::thread_store::{
    ThreadLockError, ThreadMeta, ThreadMode, ThreadStore, ThreadWriterLock, TurnLine, TurnUsage,
    UsageTotals, acquire_thread_lock, now_millis,
};
use yi_agent_core::Message;

/// 打开既有会话时的失败原因。
///
/// 三种必须分开：`Busy` 提示用户去 `fork`，`NotFound` 提示用 `threads` 看看
/// 有哪些会话，`Io` 是真正的磁盘问题。
#[derive(Debug)]
pub enum OpenError {
    /// 另一个 writer 持有该会话。
    Busy(String),
    /// 会话不存在。
    NotFound(String),
    Io(std::io::Error),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Busy(m) => write!(f, "{m}"),
            OpenError::NotFound(id) => write!(f, "找不到会话 {id}"),
            OpenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for OpenError {}

/// TUI 进程持有的会话句柄：store + 锁 + id。
pub struct TuiSessionStore {
    store: ThreadStore,
    thread_id: String,
    /// 只要本进程在写这个 thread 就持有。字段名带下划线是因为从不读取——
    /// 它的作用是活到 `TuiSessionStore` drop。
    _lock: Arc<ThreadWriterLock>,
    /// 已落盘轮次的累计用量，供 `Translator::new` 的 baseline 用。
    baseline: UsageTotals,
}

impl TuiSessionStore {
    /// 新建会话：立刻写 meta 并持锁。
    ///
    /// 不建 meta 的话 `--last` 永远看不到终端建的会话；不持锁的话等于给别的
    /// 进程留了后门。两件事必须一起做。
    /// Create a new conversation and record the sandbox it runs under.
    ///
    /// The baseline is written here, not derived later: this is the only moment
    /// the invoking configuration is known to be the one the conversation
    /// actually started with.
    pub fn create_new(
        workdir: &Path,
        model: &str,
        mode: ThreadMode,
        sandbox: yi_agent_tools::SandboxMode,
        sandbox_promotable: bool,
    ) -> std::io::Result<Self> {
        let store = ThreadStore::new(workdir);
        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());
        let now = now_millis();
        let meta = ThreadMeta {
            thread_id: thread_id.clone(),
            cwd: workdir.to_string_lossy().to_string(),
            model: model.to_string(),
            created_at: now,
            updated_at: now,
            title: None,
            permission_mode: mode,
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
            plugin_bindings: Vec::new(),
            sandbox_baseline: Some(
                yi_agent_app_server::session_sandbox::SandboxBaseline::from_parts(
                    sandbox,
                    sandbox_promotable,
                ),
            ),
        };
        store.create(&meta)?;
        let lock = acquire_thread_lock(store.root(), &thread_id)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(Self {
            store,
            thread_id,
            _lock: lock,
            baseline: UsageTotals::default(),
        })
    }

    /// 打开既有会话：载入并用其累计用量作为 baseline。
    ///
    /// 持锁失败与不存在是两回事，必须区分——前者提示 fork，后者提示 threads。
    pub fn open(workdir: &Path, thread_id: &str) -> Result<Self, OpenError> {
        let store = ThreadStore::new(workdir);
        let loaded = store
            .load(thread_id)
            .map_err(OpenError::Io)?
            .ok_or_else(|| OpenError::NotFound(thread_id.to_string()))?;

        let lock = acquire_thread_lock(store.root(), thread_id).map_err(|e| match e {
            ThreadLockError::Busy(busy) => OpenError::Busy(busy.to_string()),
            ThreadLockError::Io(io) => OpenError::Io(io),
        })?;

        Ok(Self {
            store,
            thread_id: thread_id.to_string(),
            _lock: lock,
            baseline: loaded.totals,
        })
    }

    /// Where this conversation's private resume credential lives.
    ///
    /// Delegated to `ThreadStore` rather than rebuilt here: `fork_thread` copies
    /// the transcripts root and must never copy a credential, so the location has
    /// to be the one store-level place that knows that.
    pub fn session_identity_directory(&self) -> std::path::PathBuf {
        self.store.session_identity_directory()
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn baseline_totals(&self) -> UsageTotals {
        self.baseline
    }

    /// 一轮结束后的落盘。失败由调用方记日志，不阻断对话。
    pub fn append_turn(
        &self,
        items: &[Item],
        messages: &[Message],
        usage: Option<TurnUsage>,
        usage_total: Option<TurnUsage>,
    ) -> std::io::Result<()> {
        self.store.append_turn(
            &self.thread_id,
            &TurnLine::Turn {
                items: items.to_vec(),
                usage,
                usage_total,
                messages: messages.to_vec(),
            },
        )
    }

    /// 更新 `updated_at` 并在标题为空时用首轮 prompt 兜底。
    ///
    /// 用既有的 `touch` 而不是自己拼标题：`title_from` 与 `first_user_text`
    /// 都是 crate 私有，CLI 调不到；而且 `touch` 还处理了 `is_lead_meta` 不写
    /// 标题这条规则。
    pub fn touch(&self, first_prompt: &str) -> std::io::Result<()> {
        let _ = self.store.touch(&self.thread_id, Some(first_prompt))?;
        Ok(())
    }

    /// `/clear`：删对话记录 + 清 partial + 递增 `content_epoch`。
    ///
    /// 复用既有 `truncate`——它同时做前两件事，注释里写明了这就是 `/clear`
    /// 的持久化语义。`content_epoch` 另外递增：客户端靠它判断要不要全量重同步。
    pub fn clear(&self) -> std::io::Result<()> {
        self.store.truncate(&self.thread_id)?;
        self.store.bump_content_epoch(&self.thread_id)?;
        Ok(())
    }

    /// `/compact`：只换上下文，不写 items。
    ///
    /// items 为空是**故意的**：`/compact` 不改变用户可见的内容集合，回放时
    /// 不该多出一段。这条与 `thread/compact` 的落盘口径一致。
    pub fn compact(&self, new_messages: &[Message]) -> std::io::Result<()> {
        self.store.append_turn(
            &self.thread_id,
            &TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: new_messages.to_vec(),
            },
        )
    }

    /// 载入可种进新 `Session` 的转录。
    ///
    /// 用 `forkable_messages` 截断，而不是整份 messages：会话末尾若停在
    /// 「assistant 发了 tool_use 但 tool_result 还没回来」的半轮，整份种子
    /// 会让下一轮请求带上一个没有配对结果的 tool_use——API 直接 400。
    /// 同一函数 fork 也在用，两处共享一条截断规则。
    ///
    /// 返回的 `last_input_tokens` 让 auto-compact 在 resume 后的首轮即生效，
    /// 不必等到第一次真实测量。
    pub fn load_seed(&self) -> std::io::Result<(Vec<Message>, Option<u32>)> {
        let loaded = self
            .store
            .load(&self.thread_id)?
            .ok_or_else(|| std::io::Error::other("thread disappeared"))?;
        let session = yi_agent_core::Session::from_messages(loaded.messages);
        let seed = yi_agent_core::forkable_messages(&session);
        let last_input = loaded.usage.as_ref().map(|u| u.input_tokens);
        Ok((seed, last_input))
    }

    /// 回放用：返回落盘的 items 与是否有崩溃残留的轮次。
    ///
    /// 与 `load_seed` 分开：种子要的是可发给 provider 的转录，回放要的是
    /// 用户可见的条目，两者不是同一份数据。
    pub fn load_for_replay(
        &self,
    ) -> std::io::Result<yi_agent_app_server::thread_store::LoadedThread> {
        self.store
            .load(&self.thread_id)?
            .ok_or_else(|| std::io::Error::other("thread disappeared"))
    }

    /// 这份会话是否已有值得 resume 的内容。
    ///
    /// 刚启动就退出的空会话不该提示「你可以 resume」——那条命令恢复出来仍是
    /// 空的，提示它只会让用户白跑一趟。`items` 与 `messages` 都要看：`/compact`
    /// 写的是 `items: []` + 压缩后的 `messages`，此时屏幕上没有新条目但上下文
    /// 确实存在。
    pub fn has_resumable_content(&self) -> std::io::Result<bool> {
        let loaded = self.load_for_replay()?;
        Ok(!loaded.items.is_empty() || !loaded.messages.is_empty())
    }

    /// 把崩溃残留的 checkpoint 升格进主日志。
    ///
    /// resume 必须做：采纳的 partial 是那一轮唯一的持久副本，不 append 就返回，
    /// 用户下一条消息的 turn-start checkpoint 会原子覆盖同一文件，该轮 items
    /// 永久丢失，而 session 上下文仍「记得」它们，造成 item 与上下文不一致。
    ///
    /// 返回 `true` 表示确实升格了。
    pub fn promote_pending(&self) -> std::io::Result<bool> {
        self.store.promote_partial(&self.thread_id)
    }

    #[cfg(test)]
    fn load_for_test(&self) -> std::io::Result<yi_agent_app_server::thread_store::LoadedThread> {
        Ok(self
            .store
            .load(&self.thread_id)?
            .expect("thread should exist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_core::Role;

    /// 有界轮询重开一个刚刚 `drop` 过的会话。
    ///
    /// `drop` 只关掉本进程那个描述符，内核释放 `flock` 与 `close()` 之间没有
    /// 同步语义。本职位的偶发来源更具体：**测试并行时的 fork**。同模块的
    /// `second_open_of_same_thread_is_busy` 会 `spawn` 子进程，fork 会复制
    /// **所有线程**持有的描述符——包括别的测试正持有的那把锁。子进程 `exec`
    /// 之前内核认为锁仍被持有，于是紧随 `drop` 的非阻塞 `open` 偶发拿到
    /// `Busy`。实测：单线程跑 26 次零失败；并行满载 15 次偶发 1~3 次失败；
    /// 只跑 `session_store::tests` 20 次零失败，跑整个测试二进制才出现，
    /// 据此定位为释放时序而非逻辑错误（同 `docs/bug-list.md` 里
    /// `superpowers-kanban-runner` `lease.rs` 的已知同类问题）。
    ///
    /// 只改测试：产品语义（冲突即 `Busy`，不阻塞）不动。
    fn reopen_within_a_bounded_wait(dir: &std::path::Path, id: &str) -> TuiSessionStore {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match TuiSessionStore::open(dir, id) {
                Ok(store) => return store,
                Err(OpenError::Busy(_)) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(other) => panic!("重开会话失败: {other}"),
            }
        }
    }

    #[test]
    fn create_new_writes_meta_and_holds_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "claude-test",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        assert!(store.thread_id().starts_with("thread-"));

        // meta 必须已落盘，否则 `--last` 看不到终端建的会话
        let meta_path = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{}.meta.json", store.thread_id()));
        assert!(meta_path.exists(), "create_new 必须立刻写 meta");

        // 锁文件必须存在
        let lock_path = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{}.lock", store.thread_id()));
        assert!(lock_path.exists());
    }

    #[test]
    fn append_then_reopen_restores_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        let id = store.thread_id().to_string();

        let items = vec![Item::UserMessage {
            id: "u1".into(),
            text: "hello".into(),
            attachments: Vec::new(),
            images: Vec::new(),
        }];
        let messages = vec![Message {
            role: Role::User,
            content: vec![yi_agent_core::ContentBlock::Text("hello".into())],
        }];
        store.append_turn(&items, &messages, None, None).unwrap();
        store.touch("hello").unwrap();
        drop(store);

        let reopened = reopen_within_a_bounded_wait(dir.path(), &id);
        let loaded = reopened.load_for_test().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.meta.title.as_deref(), Some("hello"));
    }

    #[test]
    fn clear_empties_the_log_but_keeps_the_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        let id = store.thread_id().to_string();
        store
            .append_turn(
                &[Item::UserMessage {
                    id: "u1".into(),
                    text: "x".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                &[],
                None,
                None,
            )
            .unwrap();

        store.clear().unwrap();

        let loaded = store.load_for_test().unwrap();
        assert!(loaded.items.is_empty(), "truncate 后 items 必须为空");
        let meta_path = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{id}.meta.json"));
        assert!(meta_path.exists(), "meta 必须保留");
        assert_eq!(loaded.meta.content_epoch, 1, "content_epoch 必须递增");
    }

    #[test]
    fn second_open_of_same_thread_is_busy() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        let id = first.thread_id().to_string();
        let lock_path = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{id}.lock"));

        // **必须用真子进程**：`flock` 按打开文件描述加锁，同进程内再 open 一次
        // 会先命中进程级去重表，根本走不到内核。而且本进程自己还持有 `first`，
        // 不先释放的话子进程也抢不到——所以先 drop 掉。
        drop(first);

        let exe = std::env::current_exe().unwrap();
        let mut holder = std::process::Command::new(exe)
            .args([
                "--exact",
                "session_store::tests::child_holds_lock",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_LOCK_PATH", &lock_path)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        {
            use std::io::{BufRead, BufReader};
            let stdout = holder.stdout.take().unwrap();
            let mut reader = BufReader::new(stdout);
            let mut seen = String::new();
            let mut got = false;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                seen.push_str(&line);
                if line.contains("LOCK_HELD") {
                    got = true;
                    break;
                }
            }
            assert!(got, "子进程必须先成功持锁，实际: {seen}");
        }

        match TuiSessionStore::open(dir.path(), &id) {
            Err(OpenError::Busy(_)) => {}
            other => panic!("期望 Busy，实际 {:?}", other.err().map(|e| e.to_string())),
        }

        holder.kill().ok();
        holder.wait().ok();
    }

    /// 由 `second_open_of_same_thread_is_busy` 以子进程方式调用。
    #[test]
    #[ignore]
    fn child_holds_lock() {
        use std::io::Write;
        let path = std::path::PathBuf::from(std::env::var("YI_LOCK_PATH").unwrap());
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .unwrap();
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        assert_eq!(rc, 0, "子进程必须能拿到锁");
        println!("LOCK_HELD");
        std::io::stdout().flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(30));
    }

    #[test]
    fn compact_replaces_messages_without_adding_items() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();

        store
            .append_turn(
                &[Item::UserMessage {
                    id: "u1".into(),
                    text: "first".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                &[Message {
                    role: Role::User,
                    content: vec![yi_agent_core::ContentBlock::Text("first".into())],
                }],
                None,
                None,
            )
            .unwrap();

        let compacted = vec![Message {
            role: Role::User,
            content: vec![yi_agent_core::ContentBlock::Text("摘要".into())],
        }];
        store.compact(&compacted).unwrap();

        let loaded = store.load_for_test().unwrap();
        // items 不变：/compact 不改变用户可见的内容集合。
        assert_eq!(loaded.items.len(), 1, "compact 不该增删 items");
        // messages 换成了压缩后的那份。
        assert_eq!(loaded.messages.len(), 1);
        assert!(matches!(
            &loaded.messages[0].content[0],
            yi_agent_core::ContentBlock::Text(t) if t == "摘要"
        ));
    }

    #[test]
    fn load_seed_drops_a_trailing_unpaired_tool_use() {
        use yi_agent_core::ContentBlock;

        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();

        // 一轮正常的问答 + 一条「assistant 发了 tool_use 但没结果」的半轮。
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text("hi".into())],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text("hello".into())],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text("run it".into())],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tool-1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"cmd": "ls"}),
                }],
            },
        ];
        store.append_turn(&[], &messages, None, None).unwrap();

        let (seed, _) = store.load_seed().unwrap();
        // 末尾那条孤立 tool_use 必须被丢掉：带着它发下一轮请求，API 会 400
        // （tool_use 没有配对的 tool_result）。
        assert_eq!(
            seed.len(),
            3,
            "末尾未配对的 tool_use 必须被裁掉，实际 {} 条",
            seed.len()
        );
        assert_eq!(seed[2].role, Role::User);
    }

    #[test]
    fn load_for_replay_returns_items_and_pending_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        store
            .append_turn(
                &[Item::UserMessage {
                    id: "u1".into(),
                    text: "回放我".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                &[],
                None,
                None,
            )
            .unwrap();

        let loaded = store.load_for_replay().unwrap();
        assert_eq!(loaded.items.len(), 1, "回放要的是 items，不是 messages");
        assert!(!loaded.pending_turn);
        // 与 load_seed 互不干扰：种子侧照样能读到同一份 messages。
        assert!(store.load_seed().is_ok());
    }

    #[test]
    fn a_fresh_session_has_nothing_to_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        assert!(
            !store.has_resumable_content().unwrap(),
            "刚建的空会话不该提示 resume"
        );
    }

    #[test]
    fn a_session_with_a_turn_is_resumable() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        store
            .append_turn(
                &[Item::UserMessage {
                    id: "u1".into(),
                    text: "问".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                &[Message {
                    role: Role::User,
                    content: vec![yi_agent_core::ContentBlock::Text("问".into())],
                }],
                None,
                None,
            )
            .unwrap();
        assert!(store.has_resumable_content().unwrap());
    }

    #[test]
    fn a_compact_only_session_is_still_resumable() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        // /compact 的落盘形态：items 为空，但 messages 有压缩后的上下文。
        store
            .compact(&[Message {
                role: Role::User,
                content: vec![yi_agent_core::ContentBlock::Text("摘要".into())],
            }])
            .unwrap();
        assert!(
            store.has_resumable_content().unwrap(),
            "屏上没有条目但上下文在，仍值得 resume"
        );
    }

    #[test]
    fn a_cleared_session_has_nothing_to_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        store
            .append_turn(
                &[Item::UserMessage {
                    id: "u1".into(),
                    text: "问".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                &[Message {
                    role: Role::User,
                    content: vec![yi_agent_core::ContentBlock::Text("问".into())],
                }],
                None,
                None,
            )
            .unwrap();
        store.clear().unwrap();
        assert!(
            !store.has_resumable_content().unwrap(),
            "/clear 之后恢复出来是空的，不该提示 resume"
        );
    }

    /// CLI 侧 resume：插件绑定的**版本与代次**必须原样回来。
    ///
    /// 与 `app-server` 的 `resuming_a_plugin_session_keeps_the_exact_binding`
    /// 是同一行为的另一条建模路径（CLI 用 `TuiSessionStore::open`，桌面用
    /// `ThreadStore::load`）。两条都要钉：只测一边的话，另一条路径哪天改成
    /// "顺手刷新成当前版本"，插件升级就会静默把旧会话接到新版本上。
    #[test]
    fn resuming_a_plugin_session_keeps_the_plugin_binding_in_the_cli_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(
            dir.path(),
            "m",
            ThreadMode::Normal,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        )
        .unwrap();
        let thread_id = store.thread_id().to_string();
        let binding = yi_agent_plugins::Binding {
            host_id: "host-cli".into(),
            release: yi_agent_plugins::ReleaseId {
                plugin: yi_agent_plugins::PluginId::parse("example-plugin").unwrap(),
                version: "1.4.0".into(),
                target: "darwin-aarch64".into(),
                sha256: "cd".repeat(32),
            },
            activation: 5,
            project: yi_agent_plugins::ProjectId::parse("/tmp/project").unwrap(),
            grant: 1,
        };
        let raw = ThreadStore::new(dir.path());
        let mut meta = raw.load(&thread_id).unwrap().expect("just created").meta;
        meta.plugin_bindings = vec![binding.clone()];
        // `create` 是覆盖写；用同一份 meta 落回去模拟"建会话时已带绑定"。
        raw.create(&meta).unwrap();
        drop(store); // 释放写锁，否则 open 拿到 Busy。

        let reopened = TuiSessionStore::open(dir.path(), &thread_id).expect("reopen");
        let loaded = ThreadStore::new(dir.path())
            .load(&thread_id)
            .unwrap()
            .expect("still there");
        assert_eq!(
            loaded.meta.plugin_bindings,
            vec![binding],
            "CLI resume must not drop or rewrite the binding"
        );
        // 句柄本身也拿到了同一个会话（不是新建了一个）。
        assert_eq!(reopened.thread_id(), thread_id);
    }

    #[test]
    fn open_of_unknown_thread_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        match TuiSessionStore::open(dir.path(), "thread-nope") {
            Err(OpenError::NotFound(_)) => {}
            other => panic!("期望 NotFound，实际 {:?}", other.err()),
        }
    }
}
