# 桌面端 /fork（会话级分支）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端能对一个已完结的会话做会话级复制——侧栏菜单「创建副本」与 `/fork` 斜杠命令两个入口，源会话不动，新副本可直接继续聊。

**Architecture:** 后端新增 `thread/fork` RPC，薄封装已有的 `ThreadStore::fork_thread`，并补齐两处它没有的能力（附件目录随迁、`(fork)` 后缀不被截断）。RPC 只落盘不起 session，桌面端随后走既有的 `thread/resume`。前端加侧栏菜单项与斜杠命令，两者共用同一个 RPC。

**Tech Stack:** Rust（app-server JSON-RPC，tokio，serde_json）、React + TypeScript（Vitest + Testing Library）、ts-rs 生成的协议类型。

## Global Constraints

- 分支与 worktree：本计划全程在 `.worktrees/feat-thread-fork`（分支 `feat/thread-fork`）内执行，**不得在 `main` 上直接改**。
- 提交前必须 `cd yi-agent-rs && cargo fmt --all`；合并前必须 `just ci` 全绿。
- 提交信息用 conventional commits，首行 ≤ 72 字符，**不得**写 `Co-Authored-By`。
- 新增/修改的行为必须有可验证判据（测试名或 `file:line`），并同步更新 `docs/project-management/` 对应模块文件。
- 错误文案对用户说中文；服务端 `ThreadBusy` 的 `Display` 已是中文，客户端原样展示，不再二次翻译。
- 状态只有 `[x]` / `[ ]` / `[-]` 三态，禁止 `[~]`。
- 单测不得调用真实 LLM，全部用现有 mock harness。

## 文件结构

| 文件 | 职责 | 本次动作 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | 会话落盘、fork 的物理复制 | 改 `fork_thread`：标题截断 + 附件随迁 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs` | thread 域 RPC 分支 | 新增 `thread/fork` 分支 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` | RPC 集成测试 | 加域映射用例 + fork 的四个 RPC 用例 |
| `desktop/src/lib/slash.ts` | 斜杠命令目录（唯一真相） | 加 `fork` 条目与分组 |
| `desktop/src/components/ThreadSidebar.tsx` | 侧栏会话菜单 | 加「创建副本」菜单项 + `onFork` prop |
| `desktop/src/App.tsx` | 动作编排 | 加 `forkThread` 与 `/fork` 分支 |
| `desktop/src/lib/slash.test.ts`、`ThreadSidebar.test.tsx`、`App.test.tsx` | 前端测试 | 各自加用例 |
| `docs/project-management/yi-agent-app-server.md`、`desktop.md` | 进度记录 | 各加条目 |

任务顺序是「后端存储 → 后端 RPC → 前端目录 → 前端侧栏 → 前端命令 → 文档」。前两个任务彼此独立于前端，可以并行开发；任务 3–5 有顺序依赖。

---

### Task 1: `fork_thread` 的标题截断与附件随迁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:315-410`（`fork_thread` 函数体）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` 的 `mod tests`（文件末尾，紧随现有 `fork_*` 用例）

**Interfaces:**
- Consumes: 同文件的 `title_from(&str) -> String`（`pub(crate)`，:1198）、`first_user_text(&[Message]) -> Option<String>`（私有，:1204）、`LoadedThread`（:255，字段 `meta/items/messages/usage/totals`）、`crate::attachments::stored_relative_path(thread_id, stored_name) -> String`（:58）。
- Produces: `pub fn fork_thread(&self, from: &str, new_id: &str) -> io::Result<String>` —— 签名不变，行为增强。Task 2 直接调用它，不感知内部改动。

- [ ] **Step 1: 写标题截断的失败测试**

在 `thread_store.rs` 的 `mod tests` 里，紧跟 `fork_copies_items_and_messages` 之后加：

```rust
    /// 源标题本身已是 30 字上限时，拼上 ` (fork)` 会被 `title_from` 再截一次，
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
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_keeps_the_marker_when_the_source_title_is_at_the_limit fork_falls_back_to_the_first_user_message_for_an_untitled_source`

Expected: 第一个用例 FAIL（`title` 不以 ` (fork)` 结尾）；第二个用例 FAIL（`title` 是 `None`，`.expect` panic）。

> 若 cargo 报 "unexpected argument"，说明一次只能过一个过滤器，改为连跑两次：
> `cargo test -p yi-agent-app-server --lib fork_keeps_the_marker` 然后
> `cargo test -p yi-agent-app-server --lib fork_falls_back`。

- [ ] **Step 3: 改 `fork_thread` 的标题逻辑**

把 `thread_store.rs:332-337` 这段：

```rust
        let now = now_millis();
        let title = loaded
            .meta
            .title
            .as_ref()
            .map(|t| title_from(&format!("{t} (fork)")));
```

换成：

```rust
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
```

- [ ] **Step 4: 运行测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_`

Expected: 全部 PASS（含既有的 `fork_copies_items_and_messages` 等；那条用例的源标题是短标题「原始标题」，断言 `"原始标题 (fork)"` 仍然成立）。

- [ ] **Step 5: 写附件随迁的失败测试**

在同一个 `mod tests` 里加（注意 `store.root()` 是 `<dir>/.yi-agent/threads`，工作区要上溯两级）：

```rust
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

        store.fork_thread("thread-no-files", "thread-no-files-forked").unwrap();

        assert!(
            !dir
                .path()
                .join(".yi-agent/attachments/thread-no-files-forked")
                .exists(),
            "没有附件就不该建空目录"
        );
    }
```

- [ ] **Step 6: 运行新测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_copies_the_attachment_directory_and_rewrites_paths`

Expected: FAIL —— `dst_dir.join("deadbeef-a.png").is_file()` 为 false（目录压根没建）。

- [ ] **Step 7: 实现附件随迁**

在 `fork_thread` 里、`self.create(&meta)?;` **之后**、`let _new_lock = ...` **之前**插入一段，并在文件顶部（`use` 区）确认无需新增依赖（`std::fs`、`std::path` 已在用）。插入：

```rust
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
```

再在 `fork_thread` 下方（同模块、私有）加辅助函数：

```rust
/// 把 `src` 下的普通文件复制到 `dst`，跳过分片上传的 `.tmp` 暂存区。
///
/// 只遍历一层：附件目录的形状恒为 `<tid>/<hash>-<name>`（见
/// `attachments::stored_relative_path`），内嵌递归没有必要。非普通文件同样跳过
/// ——与 `image/read` 拒绝 FIFO 同一条理由：一个怪异条目不该把 serve 循环挂住。
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
```

然后改历史里的路径。在 `let mut loaded = ...`（**把现有的 `let loaded` 改成 `let mut loaded`**）之后、`self.create(&meta)?` 之前插入：

```rust
        // 路径重写与目录复制必须一起做：只复制不改引用，工具按旧路径读不到文件。
        rewrite_attachment_paths(&mut loaded, from, new_id);
```

并在 `thread_store.rs` 里（`copy_attachment_tree` 旁）加：

```rust
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
```

> 若 `Item::UserInterjection` / `Item::ToolCall` 的字段名与上面不一致，以
> `protocol.rs:823-852` 的实际定义为准（`attachments` / `images` 都是
> `Vec<...>`）。编译错误会直接指出漏了哪个变体，`_ => {}` 只是兜住
> `AgentMessage` / `Thinking` 等没有路径的变体。

- [ ] **Step 8: 运行全部 fork 测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_`

Expected: 全部 PASS（新增 5 条 + 既有 5 条）。

- [ ] **Step 9: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): fork 时随迁附件并保住标题的 (fork) 后缀"
```

---

### Task 2: `thread/fork` RPC 分支

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（在 `"thread/rename"` 分支附近新增）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（域映射表 + 文件末尾新增用例）

**Interfaces:**
- Consumes: Task 1 的 `store.fork_thread(&str, &str) -> io::Result<String>`；`server::store_lookup`、`server::thread_cwd`、`server::require_thread_id`、`server::ok_response`、`server::err_response`、`server::write_response`；`protocol::RpcError`；`rpc::Flow`、`rpc::Domain`。
- Produces: JSON-RPC 方法 `thread/fork`，params `{ threadId: string }`，result `{ thread_id: string, title: string | null }`。前端 Task 4/5 依赖这个形状。

- [ ] **Step 1: 先把新方法登记进域映射测试（失败）**

在 `rpc_tests.rs` 里 `("thread/diff/read", Domain::Thread),` 与
`("thread/lead", Domain::Thread),` 之间插入一行（这张表按方法名字母序排列，
`fork` 落在 `diff` 与 `lead` 之间）：

```rust
        ("thread/fork", Domain::Thread),
```

- [ ] **Step 2: 运行，确认这条断言本身先绿（它测的是 `domain_for` 前缀）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib rpc_methods_route_to_their_domains`

Expected: PASS —— `domain_for` 按 `thread` 前缀切分，加进表就能过。这条的价值是**防止有人把方法名写成 `fork/thread`**；真正的行为测试在 Step 5。

- [ ] **Step 3: 写 RPC 行为测试（失败）**

在 `rpc_tests.rs` 末尾加（用文件里已有的 `Harness`、`initialize`、`start_thread`、`read_response`、`write_meta` 助手）：

> **`Harness::new()` 不能用在这组测试里。** 它的 `cfg.workdir` 是写死的
> `/tmp/yi-agent-app-server-test`（见 `test_support::test_config`），而 `write_meta`
> 把 meta 写进你新建的 tempdir。`store_lookup` 先查 `WorkspaceIndex`（没注册 →
> 空），再回落到 `cfg.workdir` —— 两处都找不到那条会话，于是**必然**返回
> `-32011`，测试假失败。冷会话的用例一律用
> `Harness::with_cfg(cfg)` 并把 `cfg.workdir` 指到同一个 tempdir。

```rust
/// `thread/fork` 的最小闭环：返回新 id、落盘能看到、源会话字节不变。
#[tokio::test]
async fn thread_fork_copies_a_settled_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    write_meta(&cwd, "t-fork-src", "源会话", None, None);

    // workdir 必须与 `write_meta` 的目录一致，否则 `store_lookup` 找不到它。
    let mut cfg = test_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"thread/fork","params":{"threadId":"t-fork-src"}}"#)
        .await;
    let v = read_response(&mut h, 7).await;
    assert!(v.get("error").is_none(), "fork 应当成功: {v}");
    let new_id = v["result"]["thread_id"].as_str().unwrap().to_string();
    assert_ne!(new_id, "t-fork-src");
    assert_eq!(
        v["result"]["title"].as_str(),
        Some("源会话 (fork)"),
        "回包要带上落盘的标题，前端才能立刻显示"
    );

    let store = crate::thread_store::ThreadStore::new(&cwd);
    let loaded = store.load(&new_id).unwrap().expect("新会话必须落盘");
    assert_eq!(loaded.meta.forked_from.as_deref(), Some("t-fork-src"));
    assert!(
        store.load("t-fork-src").unwrap().is_some(),
        "源会话必须还在"
    );
    h.shutdown().await;
}

/// 未知 id 要说是「不认识」，不能悄悄建一个空会话出来。
#[tokio::test]
async fn thread_fork_rejects_an_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":8,"method":"thread/fork","params":{"threadId":"t-nope"}}"#)
        .await;
    let v = read_response(&mut h, 8).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32011), "{v}");
    h.shutdown().await;
}

/// 运行中的会话一律拒绝：复制到半轮会产生一个「开头有提问、结尾是断掉的
/// tool_use」的坏副本。拒绝必须发生在**任何磁盘写入之前**。
#[tokio::test(flavor = "multi_thread")]
async fn thread_fork_refuses_a_running_thread_and_writes_nothing() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_checkpoint_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;

    // 起一轮不会立刻结束的 turn。`CheckpointProvider` 第一次调用给 tool_use、
    // 第二次起 `pending()` 永不收尾，故这一轮不会自己结束。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"do work"}}]}}}}"#
    ))
    .await;
    let _ = read_response(&mut h, 10).await;

    // 只数 `.meta.json`：运行中的 turn 会往同目录写 `<tid>.partial.json`
    // （turn 开始写一次、之后每 500ms 去抖刷新），数全部条目会与其竞态而偶发红。
    let count_metas = || {
        std::fs::read_dir(dir.path().join(".yi-agent/threads"))
            .map(|d| {
                d.flatten()
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
                    .count()
            })
            .unwrap_or(0)
    };
    let before = count_metas();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":11,"method":"thread/fork","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 11).await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(-32012),
        "运行中的会话必须被拒绝: {v}"
    );

    assert_eq!(count_metas(), before, "被拒绝的 fork 不该建出任何新会话");
    h.shutdown().await;
}

/// 源目录被删掉后不该 fork 出一个打不开的会话。
///
/// 会话必须先在内存里（`thread/start` 时给了 cwd），`thread_cwd` 才会返回那条
/// 路径并走到 `!cwd.is_dir()` 分支。若走冷会话路径，`store_lookup` 定位不到
/// 目录就回落到 `cfg.workdir`，最终报的是 `-32011`（未知会话）——那测的就不是
/// 这条分支了。
#[tokio::test]
async fn thread_fork_refuses_when_the_working_directory_is_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    let gone = dir.path().join("vanished");
    std::fs::create_dir_all(&gone).unwrap();
    let gone_str = gone.to_string_lossy().to_string();

    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 9, "method": "thread/start",
            "params": { "cwd": gone_str }
        })
        .to_string(),
    )
    .await;
    let started = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(9)))
        .await;
    let tid = started["result"]["thread_id"].as_str().unwrap().to_string();

    // 起会话之后目录才消失——这正是「项目目录被删/改名」的真机情形。
    std::fs::remove_dir_all(&gone).unwrap();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":12,"method":"thread/fork","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 12).await;
    assert_eq!(
        v["error"]["code"],
        serde_json::json!(-32602),
        "目录已消失必须明确报错，而不是落盘一个打不开的会话: {v}"
    );
    // 关键判据：**没有多出任何会话**。若 cwd 检查缺失，`fork_thread` 的
    // `create` 会 `create_dir_all(&self.root)` 把整棵 `<gone>/.yi-agent/threads`
    // 复活并写进一个新会话，这里就会数出第二条。
    //
    // 不断言 `!gone.exists()`：`thread/start` 的标题生成是异步的，可能在删除之后
    // 才回写 meta，把目录重新建出来——那是无关写入，不该让这条测试偶发变红。
    // 必须查**被删目录**上的那个 store：`ThreadStore::new(dir.path())` 指向的是
    // 另一条路径，恒为空，断言就成了假绿。目录不存在时 `list()` 返回空表，
    // 只有 fork 真的把目录复活并写进去，才会数出条目。
    // `ThreadStore::new` 接受的是**工作区根**（它自己拼 `.yi-agent/threads`）。
    let store = crate::thread_store::ThreadStore::new(&gone);
    let ids: Vec<String> = store
        .list()
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.thread_id)
        .collect();
    assert!(
        ids.is_empty(),
        "被拒绝的 fork 不该在已删除的目录里留下会话: {ids:?}"
    );
    h.shutdown().await;
}
```

- [ ] **Step 4: 运行，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_fork_`

Expected: 四条全部 FAIL，症状是 `-32601 method not found`（`{"error":{"code":-32601,...}}`）。

- [ ] **Step 5: 实现 `thread/fork` 分支**

先在 `rpc/thread.rs` 顶部的 `use crate::server::{...}` 里补一个名字——
现有的导入只有名字相近的 `resolve_thread_cwd`（那是解析 `params.cwd` 的异步
函数，不适用）。把 `thread_cwd,` 加进去（字母序在 `thread_summary_json` 前）：

```rust
    require_thread_id, resolve_thread_cwd, run_thread_driver, start_thread_core, store_lookup,
    thread_cwd, thread_summary_json, thread_summary_json_with_status, watch_processes,
```

然后在 `"thread/rename" => {` 之前插入：

```rust
        "thread/fork" => {
            // 与 `thread/rename` / `thread/delete` 同档：Control 本就能
            // `thread/start` + `turn/start`，单开此门不扩大能力面。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 冷会话必须能 fork：复制一个旧会话去分支正是主要用法，所以不能
            // 只看 `state.threads`，要走 store_lookup。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let in_memory = state.threads.contains_key(&thread_id);
            if !in_memory && !store.exists(&thread_id) {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }

            // 运行中一律拒绝，且必须在任何磁盘动作之前：复制到半轮会产生一个
            // 结尾停在裸 tool_use 上的坏副本（详见 `fork_prefix_len` 的注释）。
            // 判据与 `power_busy::local_busy` 同一形状：有活跃 turn，或状态不是
            // Idle（含 awaiting_approval）。
            if let Some(session) = state.threads.get(&thread_id) {
                let status = *session
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if session.active_turn_id.is_some() || !matches!(status, ThreadStatus::Idle) {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::turn_in_progress(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }

            // 目录没了就没法写出一个能打开的会话：明确报错，而不是让它落盘之后
            // 才在 resume 阶段失败。口径对齐 `thread/resume` 的同款检查。
            let Some(cwd) = thread_cwd(state.threads, state.workspaces, state.cfg, &thread_id)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            if !cwd.is_dir() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(format!(
                            "working directory no longer exists: {}",
                            cwd.display()
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }

            let new_id = format!("thread-{}", uuid::Uuid::new_v4());
            match store.fork_thread(&thread_id, &new_id) {
                Ok(created) => {
                    // 标题从落盘的 meta 回读：`fork_thread` 已经算好（含无标题时的
                    // 兜底派生），这里再推导一遍只会两处规则漂移。
                    let title = store
                        .read_meta(&created)
                        .ok()
                        .flatten()
                        .and_then(|m| m.title);
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({ "thread_id": created, "title": title }),
                        ),
                    )
                    .await?;
                }
                // `WouldBlock` 时 message 已是 `ThreadBusy` 的中文（「会话 X 正被
                // 另一个 yi-agent 实例使用」），原样上抛，客户端不必再翻译。
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
```

**注意这里不发任何通知。** 侧栏行由客户端的 `thread/listAll` 拉取（前端任务里会先刷新再切）。

- [ ] **Step 6: 运行四条测试，确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_fork_`

Expected: 四条 PASS。

> 若 `thread_fork_refuses_when_the_working_directory_is_gone` 拿不到 `-32602`
> 而是 `-32011`：说明会话没进内存（`thread/start` 没成功），`thread_cwd` 走了
> 冷路径、`store_lookup` 又定位不到被删目录，于是回落到 `cfg.workdir`。
> **不要放宽断言**——那是测试构造问题：确认 `thread/start` 的响应里拿到了
> `thread_id`，且删除发生在拿到它之后。

- [ ] **Step 7: 跑整个 thread 域回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_`

Expected: 全绿。特别确认既有的 `thread/delete`、`thread/rename` 用例没受影响。

- [ ] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): 新增 thread/fork RPC"
```

---

### Task 3: 斜杠命令目录加 `fork`

**Files:**
- Modify: `desktop/src/lib/slash.ts:18-37`
- Test: `desktop/src/lib/slash.test.ts`

**Interfaces:**
- Produces: `SLASH_COMMANDS` 里新增 `{ name: "fork", description: string, usage: "[message]", needsArg: false }`；`HELP_GROUPS` 的「会话」组包含 `fork`。Task 5 的 `App.tsx` 分支按 `name === "fork"` 匹配。

- [ ] **Step 1: 改现有断言，让它先红**

`slash.test.ts` 第一条用例现在断言五条命令：

```ts
  it("lists exactly the five supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "config",
      "usage",
      "help",
    ]);
  });
```

改成：

```ts
  it("lists exactly the six supported commands, in popup order", () => {
    expect(SLASH_COMMANDS.map((c) => c.name)).toEqual([
      "clear",
      "compact",
      "fork",
      "config",
      "usage",
      "help",
    ]);
  });
```

再加一条针对新命令的：

```ts
  it("offers /fork with an optional message argument", () => {
    const fork = SLASH_COMMANDS.find((c) => c.name === "fork");
    expect(fork).toBeDefined();
    expect(fork!.usage).toBe("[message]");
    // 不强制带参数：不带就是「只建副本」。
    expect(fork!.needsArg).toBe(false);
    expect(fork!.description).toContain("会话空闲");
  });
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd desktop && npx vitest run src/lib/slash.test.ts`

Expected: FAIL —— 命令列表对不上，且 `fork` 为 `undefined`。

- [ ] **Step 3: 加命令与分组**

`slash.ts` 的 `SLASH_COMMANDS` 里，在 `compact` 之后插入（Popup 顺序即此顺序，`f` 开头排在 `c` 与 `u` 之间读起来顺）：

```ts
  { name: "fork", description: "从当前会话分叉一个新副本（需会话空闲）", usage: "[message]", needsArg: false },
```

`HELP_GROUPS` 的「会话」组改成：

```ts
  { title: "会话", commands: ["clear", "compact", "fork"] },
```

同时把文件顶部注释里「Mirrors the "local" subset」这段不用改——`fork` 也是本地命令（走 RPC，不经 daemon 的 TUI 目录）。

- [ ] **Step 4: 运行，确认通过**

Run: `cd desktop && npx vitest run src/lib/slash.test.ts`

Expected: 全绿。特别确认 `help groups` 那条「拍平后恰好等于目录」通过——它正是漏改分组时的看门人。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/slash.ts desktop/src/lib/slash.test.ts
git commit -m "feat(desktop): 斜杠命令目录新增 /fork"
```

---

### Task 4: 侧栏「创建副本」

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`（props 声明 ~:150-215，会话菜单 ~:1230-1300）
- Modify: `desktop/src/App.tsx`（`renameThread` 旁新增 `forkThread`，props 传递 ~:2501）
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 RPC `thread/fork`，params `{ threadId }`。
- Produces: `ThreadSidebar` 新 prop `onFork: (id: string) => void`；`App` 内部 `forkThread(id: string): Promise<void>`。Task 5 会复用 `forkThread` 的请求部分。

- [ ] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 的 `renderSidebar` 之后给 props 加默认值——`sidebarProps()` 里已有的那一串加上：

```ts
    onFork: vi.fn(),
```

（放在 `onDelete: vi.fn(),` 之后。）

然后在 `describe("ThreadSidebar")` 里、`"clears the tag from the menu"` 之后加：

```tsx
  it("offers 创建副本 in the thread menu and reports the id", () => {
    const onFork = vi.fn();
    const { container } = renderSidebar({ onFork });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);

    fireEvent.click(screen.getByRole("menuitem", { name: "创建副本" }));
    expect(onFork).toHaveBeenCalledWith("1");
  });

  it("closes the thread menu after 创建副本", () => {
    const { container } = renderSidebar({ onFork: vi.fn() });
    fireEvent.contextMenu(container.querySelector<HTMLElement>('[data-thread-row="1"]')!);
    fireEvent.click(screen.getByRole("menuitem", { name: "创建副本" }));
    expect(container.querySelector('[data-thread-menu=""]')).toBeNull();
  });
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: FAIL —— 找不到 `menuitem` 名为「创建副本」。

- [ ] **Step 3: 加 prop 声明**

`ThreadSidebar.tsx` 的 props 类型里，紧跟 `onDelete: (id: string) => void;` 之后加：

```ts
  onFork: (id: string) => void;
```

并在解构参数列表里、`onDelete,` 之后加 `onFork,`。

- [ ] **Step 4: 加菜单项**

在会话菜单（`data-thread-menu` 那个 `role="menu"` 的 `div`）里，「重命名」那个
`<button>` **之前**插入。放在这里是因为它与「重命名」是同一类操作（都是对这份
会话本身动手），且不与标签/分组那两块的 `border-b` 分隔线打架：

```tsx
            <button
              type="button"
              role="menuitem"
              data-fork-thread=""
              onClick={() => {
                closeMenus();
                onFork(t.thread_id);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              创建副本
            </button>
```

> `closeMenus()` 必须在 `onFork` 之前调用：与同一菜单里其它项一致，且第二条测试
> 就钉这一点。

- [ ] **Step 5: 运行 ThreadSidebar 测试，确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`

Expected: 全绿。

- [ ] **Step 6: 写 App 层失败测试**

`App.test.tsx` 里，在 `/compact` 那组用例之后加一个新的 `it`。它断言两件事：发出了 `thread/fork`，且**没有**发 `thread/resume`（侧栏入口不切会话）。

```tsx
  it("creates a copy from the sidebar without switching threads", async () => {
    // fork 之后 App 会 `refreshThreads()`：桩的 `thread/listAll` 只回 `state.threads`，
    // 新会话不在里面就会被当成「已不存在」。故 fork 时要把它推进 seed。
    state.dataSources["thread/fork"] = () => {
      state.threads = [
        { thread_id: "thread-copy", title: "源 (fork)", permission_mode: state.mode },
        ...(state.threads ?? []),
      ];
      return { thread_id: "thread-copy", title: "源 (fork)" };
    };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const resumeCountBefore = clients[0].requests.filter((r) => r.method === "thread/resume").length;

    // 右键当前会话行，点「创建副本」。
    const row = document.querySelector<HTMLElement>("[data-thread-row]");
    expect(row).not.toBeNull();
    fireEvent.contextMenu(row!);
    fireEvent.click(screen.getByRole("menuitem", { name: "创建副本" }));

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/fork")).toBe(true),
    );
    // 侧栏入口刻意不切会话：不该多出一次 resume。
    expect(
      clients[0].requests.filter((r) => r.method === "thread/resume").length,
    ).toBe(resumeCountBefore);
  });
```

- [ ] **Step 7: 运行，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "creates a copy from the sidebar"`

Expected: FAIL —— `thread/fork` 请求没发出去（App 还没实现 `forkThread`）。

- [ ] **Step 8: 实现 `forkThread` 并接线**

在 `App.tsx` 的 `renameThread`（约 :1243）之后加：

```ts
  /**
   * 复制一份会话，源会话不动。
   *
   * **刻意不切会话**：从侧栏复制一个会话，用户的注意力不该被抢走。新条目靠
   * `(fork)` 标题 + 更新时间出现在列表顶部，这本身就是反馈（桌面端没有 toast）。
   */
  const forkThread = async (id: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/fork", { threadId: id });
    } catch (e) {
      setCurrentError(formatError(e));
      return;
    }
    await sess.refreshThreads();
  };
```

在 `<ThreadSidebar ...>` 的 `onDelete={deleteThread}` 之后加：

```tsx
          onFork={(id) => void forkThread(id)}
```

并给 `ThreadSidebar` 的 props 类型补 `onFork`（Task 4 Step 3 已加）。

- [ ] **Step 9: 运行，确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "creates a copy from the sidebar"`

Expected: PASS。

- [ ] **Step 10: 跑整套前端测试**

Run: `cd desktop && npx vitest run`

Expected: 全绿。若 `App.test.tsx` 里别的用例因为 `ThreadSidebar` 多了一个必填 prop 而报类型错，说明它们的 props 构造没走 `sidebarProps()`——按 `ThreadSidebar.test.tsx` 的做法补 `onFork: vi.fn()`。

- [ ] **Step 11: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): 侧栏会话菜单新增「创建副本」"
```

---

### Task 5: `/fork` 斜杠命令

**Files:**
- Modify: `desktop/src/App.tsx`（`onSlashCommand` 的 `switch`，约 :1953-1995）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: Task 2 的 `thread/fork`；Task 3 的目录条目；Task 4 的 `forkThread` 请求形状；`store.setDraft(id, text)`（`lib/threadStore.ts:306`）；既有的 `selectThread`、`sess.refreshThreads`、`deleteCachedContent`。
- Produces: 无对外接口，纯行为。

- [ ] **Step 1: 写三条失败测试**

在 `App.test.tsx` 的 `/compact` 用例之后加：

```tsx
  it("forks on /fork and carries the message into the new thread's draft", async () => {
    // 与侧栏用例同理：`refreshThreads` 之后新会话必须能从 listAll 里读到，
    // 否则 `selectThread` 的对账会把它判成「服务端已删除」。
    state.dataSources["thread/fork"] = () => {
      state.threads = [
        { thread_id: "thread-forked", title: "源 (fork)", permission_mode: state.mode },
        ...(state.threads ?? []),
      ];
      return { thread_id: "thread-forked", title: "源 (fork)" };
    };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/fork 换个思路试试" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/fork")).toBe(true),
    );
    // 切到新会话：之后必须为它发一次 resume。
    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) => r.method === "thread/resume" && (r.params as any)?.threadId === "thread-forked",
        ),
      ).toBe(true),
    );
    // /fork 后面的文字成为新会话的草稿，不自动发送。
    await waitFor(() => expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("换个思路试试"));
    expect(clients[0].requests.some((r) => r.method === "turn/start")).toBe(false);
  });

  it("leaves the new thread's draft empty when /fork has no message", async () => {
    state.dataSources["thread/fork"] = () => {
      state.threads = [
        { thread_id: "thread-forked", title: "源 (fork)", permission_mode: state.mode },
        ...(state.threads ?? []),
      ];
      return { thread_id: "thread-forked", title: "源 (fork)" };
    };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/fork" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await waitFor(() =>
      expect(
        clients[0].requests.some(
          (r) => r.method === "thread/resume" && (r.params as any)?.threadId === "thread-forked",
        ),
      ).toBe(true),
    );
    // 命令文本不能被当成草稿搬到新会话。
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("");
  });

  it("explains a refused /fork instead of switching threads", async () => {
    state.rejectCode = { "thread/fork": -32012 };
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.some((r) => r.method === "thread/resume")).toBe(true),
    );
    const resumeCountBefore = clients[0].requests.filter((r) => r.method === "thread/resume").length;

    const textarea = screen.getByRole("textbox");
    fireEvent.change(textarea, { target: { value: "/fork" } });
    fireEvent.keyDown(textarea, { key: "Enter" });

    await screen.findByText(/正在运行，跑完再 fork/);
    expect(
      clients[0].requests.filter((r) => r.method === "thread/resume").length,
    ).toBe(resumeCountBefore);
  });
```

- [ ] **Step 2: 运行，确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "/fork"`

Expected: 三条 FAIL —— `/fork` 落进 `default` 分支，显示「未知命令: /fork」。

- [ ] **Step 3: 实现 `case "fork"`**

在 `onSlashCommand` 的 `switch` 里，`case "clear"` **之前**插入。注意参数必须在第一个 `await` 之前取到——`runSlash` 在 `onSlashCommand` 返回后会同步执行 `onDraftChange("")`。

```ts
      case "fork": {
        // `runSlash` 调本函数后**不 await**，紧接着同步执行 `onDraftChange("")`
        // 清空当前会话的草稿。故 seed 必须在第一次 await 之前取到。
        const seed = args;
        let newId: string;
        try {
          const r = await c.request<{ thread_id: string; title?: string | null }>(
            "thread/fork",
            { threadId: id },
          );
          newId = r.thread_id;
        } catch (e) {
          session.notice(
            (e as { code?: number })?.code === -32012
              ? "这个会话正在运行，跑完再 fork"
              : `分叉失败: ${formatError(e)}`,
          );
          break;
        }
        // `thread/fork` 不发通知，侧栏行只能来自 listAll。先刷新再切，否则
        // 用户会短暂看到一个「列表里不存在」的会话。
        await sess.refreshThreads();
        // 预先写进**新会话**的草稿：输入框 value 直接读 view(id).draft，先写好
        // 才能在新会话首帧就显示。此刻 currentId 仍指向源会话，不能用 changeDraft。
        if (seed) store.setDraft(newId, seed);
        await selectThread(newId);
        break;
      }
```

- [ ] **Step 4: 运行三条测试，确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "/fork"`

Expected: 三条 PASS。

> 若第一条在 `resume(thread-forked)` 处超时：`selectThread` 对新 id 会走
> `thread/resume`（`syncPlan` 判 `full`）。测试桩需要为任意 threadId 回一个成功
> 结果——确认 `state.dataSources` 的默认 `thread/resume` 不按 id 匹配。若它按 id
> 白名单，给用例补一条
> `state.dataSources["thread/resume"] = () => ({ permission_mode: "normal" })`。

- [ ] **Step 5: 跑整套前端测试**

Run: `cd desktop && npx vitest run`

Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): /fork 斜杠命令分叉当前会话"
```

---

### Task 6: 类型生成与全量校验

**Files:**
- 可能变更: `desktop/src/lib/generated/`（若 ts-rs 导出受影响）

**Interfaces:**
- Consumes: Task 1–5 的全部改动。
- Produces: 无。

- [ ] **Step 1: 确认没有 wire 类型改动**

本次没有新增/修改 `protocol.rs` 里的 `#[derive(TS)]` 结构（`thread/fork` 的响应是即席 `json!`）。运行绑定检查以证实这一点：

Run: `cd yi-agent-rs && just bindings-check`

Expected: PASS 且 `desktop/src/lib/generated/` 无 diff。若报出 diff，说明有人在实现时把响应改成了具名结构——要么接受并提交生成物，要么改回 `json!`。

- [ ] **Step 2: 跑完整 CI**

Run: `cd yi-agent-rs && just ci`

Expected: 12 道门禁全绿（`fmt-check`、`lint`、`test`、`build`、`bindings-check`、`secret-check` 等）。

- [ ] **Step 3: 跑前端测试**

Run: `cd desktop && npx vitest run`

Expected: 全绿。

- [ ] **Step 4: 修复任何遗留问题并提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A && git commit -m "chore: /fork 的绑定与 CI 校验" || echo "nothing to commit"
```

（若无改动，跳过提交。）

---

### Task 7: 项目进度文档

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（若模块计数变化）

**Interfaces:**
- Consumes: Task 1–6 的测试名。
- Produces: 无。

- [ ] **Step 1: 在 app-server 模块文件里加条目**

在 `docs/project-management/yi-agent-app-server.md` 对应小节（thread 类 RPC）加：

```markdown
- [x] `thread/fork`：会话级复制，源会话不动。运行中拒绝（`-32012`），未知 id
      `-32011`，cwd 已删 `-32602`，跨进程写锁冲突时把 `ThreadBusy` 的中文原样
      放进 `-32603`。附件目录随迁并重写历史路径。
      判据：`cargo test -p yi-agent-app-server --lib thread_fork_`（4 条）与
      `cargo test -p yi-agent-app-server --lib fork_`（10 条）。
      代码：`rpc/thread.rs` 的 `"thread/fork"` 分支、`thread_store.rs` 的
      `fork_thread` / `copy_attachment_tree` / `rewrite_attachment_paths`。
```

- [ ] **Step 2: 在 desktop 模块文件里加条目**

在 `docs/project-management/desktop.md` 加：

```markdown
- [x] 会话级分支：侧栏会话菜单「创建副本」（不切会话）与 `/fork` 斜杠命令
      （切到新会话，`/fork <文字>` 的文字成为新会话草稿，不自动发送）。
      判据：`npx vitest run src/components/ThreadSidebar.test.tsx -t 创建副本`、
      `npx vitest run src/App.test.tsx -t "/fork"`。
      代码：`components/ThreadSidebar.tsx` 的 `data-fork-thread` 菜单项、
      `App.tsx` 的 `forkThread` 与 `onSlashCommand` 的 `case "fork"`。
```

- [ ] **Step 3: 更新模块索引计数**

在 `docs/project-management/README.md` 的模块索引表里，把 `yi-agent-app-server.md`
与 `desktop.md` 两行的「完成 / 总计」按实际增量各加对应条数。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/
git commit -m "docs(project-management): 记录会话级 fork"
```

---

## 自检结果

**1. 规格覆盖**

| spec 章节 | 覆盖任务 |
|---|---|
| 1. `thread/fork` RPC（存在性、运行中、cwd、锁语义） | Task 2 |
| 2. 三种失败的错误码 | Task 2 Step 5 + Step 3 的四条测试 |
| 3. 附件随迁 + 路径重写全集 | Task 1 Step 5–8 |
| 4. `(fork)` 后缀不被截断 + 无标题兜底 | Task 1 Step 1–4 |
| 5. 侧栏「创建副本」不切会话 | Task 4 |
| 6. `/fork`、草稿时序、`refreshThreads` 先于 `selectThread` | Task 5 |
| 7. 数据流 | Task 2（后端半段）+ Task 5（前端半段） |
| 8. 已知限制（无通知、worktree、组长） | 无任务：三条都是**不实现**的决定，由 Task 2 注释与 Task 7 文档承载 |

**2. 占位符扫描**

无 TBD/TODO；每个代码步骤都给了可粘贴的完整代码；每条测试都写了断言与失败症状。

**2.1 API 实拍核对（逐条对照真实代码，非凭记忆）**

| 计划里用到 | 真实位置 | 结论 |
|---|---|---|
| `ThreadStore::fork_thread(&str,&str)->io::Result<String>` | `thread_store.rs:315` | ✅ 签名一致 |
| `ThreadStore::load / exists / read_meta / root` | `:495 / :964 / :621 / :287` | ✅ |
| `store.root()` = `<cwd>/.yi-agent/threads` | `:275-280` | ✅ 上溯两级即工作区根 |
| `server::store_lookup / thread_cwd / require_thread_id` | `server.rs:6122 / 6146 / 7895` | ✅ 名字与签名一致 |
| `RpcError::insufficient_scope / unknown_thread / turn_in_progress / invalid_params / internal` | `protocol.rs:149 / 132 / 135 / 118 / 121` | ✅ 错误码 -32014/-32011/-32012/-32602/-32603 |
| `ThreadSession.status: Arc<Mutex<ThreadStatus>>`、`active_turn_id` | `session.rs:156 / 137`；`ThreadStatus` 是 `Copy`（`protocol.rs:778`） | ✅ 判据与 `power_busy::local_busy` 同形 |
| `Item::{UserMessage,UserInterjection}.{attachments,images}`、`Item::ToolCall.images` | `protocol.rs:824-870` | ✅ |
| `ContentBlock::Image { path, .. }` / `ToolResult { content, .. }` | `message.rs:30-44` | ✅ 需递归 `ToolResult` |
| `Harness::{new, with_cfg, with_config}`、`initialize/start_thread/read_response/read_matching/write_meta` | `rpc_tests.rs:2032 / 2037 / 2055 / 2523 / 2537 / 2665 / 9320` | ✅ |
| `test_config()` 的 workdir 固定为 `/tmp/yi-agent-app-server-test` | `test_support.rs:96` | ⚠️ 见下方修正 1 |
| `SLASH_COMMANDS` / `HELP_GROUPS` / 五条命令断言 | `slash.ts:18 / 35`、`slash.test.ts:9` | ✅ |
| `onSlashCommand` 的 `switch`、`default` 报「未知命令」 | `App.tsx:1901 / 1985` | ✅ |
| `runSlash` 不 await 且随即同步 `onDraftChange("")` | `MessageInput.tsx:163-168` | ✅ 草稿时序陷阱属实 |
| `store.setDraft` 内部走 `view(id)`（不存在则创建） | `threadStore.ts:306 / 86-93` | ✅ 新 id 可直接写 |
| `selectThread(id, opts?)` 对未知 id 判 `full` 而非 `drop` | `syncPlan.ts:57-77` | ✅ 不会误删 |
| `ThreadSidebar` 的 `onDelete` 位置、`data-thread-menu` 菜单结构 | `ThreadSidebar.tsx:191 / 1230` | ✅ |
| `sidebarProps()` 统一构造 props | `ThreadSidebar.test.tsx:45` | ✅ 加必填 prop 不会连带破测试 |
| `just bindings-check` / `just ci` | `justfile:47 / 36` | ✅ recipe 存在 |

**2.2 本轮修正的实质问题**

1. **RPC 测试的 workdir 与 `Harness::new()` 不匹配**（会必然假失败）：`write_meta`
   写进 tempdir，`Harness::new()` 的 `cfg.workdir` 却是写死的 `/tmp/...`，
   `store_lookup` 两处都找不到 ⇒ 恒报 `-32011`。已改为 `Harness::with_cfg(cfg)`
   且 `cfg.workdir` 指向同一 tempdir。
2. **「目录已删」用例测错了分支**：原写法用冷会话 + `add_workspace`，`thread_cwd`
   找不到目录就回落 `cfg.workdir`，报的是 `-32011` 而不是 `-32602`。已改为
   先 `thread/start`（带 cwd）让会话进内存，再删目录。
3. **`thread_cwd` 不在 `rpc/thread.rs` 的 import 里**：现有导入只有名字相近的
   `resolve_thread_cwd`（那是解析 `params.cwd` 的异步函数）。已补 import 步骤。
4. **域映射表插入位置写错**：表按方法名字母序，`thread/fork` 应在
   `thread/diff/read` 与 `thread/lead` 之间，不是 `thread/delete` 之前。
5. **运行中用例的「文件数不变」会与 checkpoint 竞态**：运行中的 turn 每 500ms
   往同目录刷 `<tid>.partial.json`，数全部条目会偶发变红。已改为只数
   `.meta.json`。
6. **「目录已删」用例的残留断言是假绿**：`ThreadStore::new(dir.path())` 指向另一条
   路径，恒为空。已改为在**被删目录**上建 store 再断言为空。
7. **前端测试桩缺 `listAll` 种子**：`thread/fork` 之后 App 会 `refreshThreads`，
   而桩的 `thread/listAll` 只回 `state.threads`，新会话不在里面就被判「已删除」。
   三处测试已在 fork 的 `dataSources` 里把新会话推进种子。
8. **侧栏菜单项插入位置**：改到「重命名」之前（同属「对这份会话动手」），
   并去掉 `border-b`，避免与标签/分组两块的分隔线打架。

**3. 类型一致性**

- `fork_thread(&self, from: &str, new_id: &str) -> io::Result<String>`：Task 1 保持签名，Task 2 按此调用。✅
- `thread/fork` 的 result 形状 `{ thread_id, title }`：Task 2 产出，Task 4（只读不解析）与 Task 5（`r.thread_id`）一致。✅
- `onFork: (id: string) => void`：Task 4 三处（props 类型、解构、App 传参）同名。✅
- `store.setDraft(id, text)`：Task 5 与 `lib/threadStore.ts:306` 的实际签名一致。✅
