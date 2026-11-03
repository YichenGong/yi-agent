# CLI 级会话 resume 与 fork 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `yi-agent` 终端的会话能落盘、能 `resume` 回来、能 `fork` 出新会话，与桌面端共用同一份 `<cwd>/.yi-agent/threads/` 存储。

**Architecture:** 不新建存储层——`ThreadStore` 已实现落盘/载入/截断/清空。本计划做两件事：(1) 给 TUI 接上 `ThreadStore`（目前 `crates/yi-agent/src` 里零命中）；(2) 在 CLI 表面加 `resume` / `fork` / `threads` 三个子命令。加一把 `flock` 会话写锁防止 TUI 与 app-server 交错写同一 `.jsonl`。

**Tech Stack:** Rust 2024 edition, MSRV 1.85, clap 4（derive），`libc::flock`（不能用 `File::try_lock`，1.89 才有），ratatui（既有 TUI），serde_json（既有）。

**Spec:** `docs/superpowers/specs/2026-11-01-cli-session-resume-design.md`

## Global Constraints

- **MSRV 1.85**：禁用 `std::fs::File::try_lock` / `try_lock_shared`（1.89 稳定）。跨进程锁一律用 `libc::flock`。
- **禁止在 `main` 分支改代码**。开工前按 `superpowers:using-git-worktrees` 建 worktree。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- Commit message 用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- **不新增 `TurnLine` 变体**。`/clear` 走既有 `ThreadStore::truncate`，`/compact` 走空 items 的 `Turn` 行。
- **不新增第二套 AgentEvent→Item 映射**。复用 `yi_agent_app_server::translate::Translator::on_event`。
- **不新增第二套截断规则**。复用 `yi_agent_core::forkable_messages`。
- 测试一律 mock provider（wiremock），不调真实 LLM。按 crate 跑：`cargo test -p yi-agent-app-server`、`cargo test -p yi-agent`。不要 `--workspace`（会 OOM）。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"` 确认无残留进程。

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `crates/yi-agent-app-server/src/thread_store.rs` | 锁、`fork_thread`、`WriterLock` 类型 | 修改（+约 250 行） |
| `crates/yi-agent/src/session_store.rs` | TUI 侧的 store 持有与落盘封装 | **新建** |
| `crates/yi-agent/src/tui/replay.rs` | `Item → HistoryCell` 纯函数 | **新建** |
| `crates/yi-agent/src/resume.rs` | resume/fork/threads 子命令实现 | **新建** |
| `crates/yi-agent/src/config.rs` | clap 子命令定义 | 修改 |
| `crates/yi-agent/src/main.rs` | dispatch 接入 | 修改 |
| `crates/yi-agent/src/tui/mod.rs` | 挂 `pub mod replay;` | 修改 |

分文件理由：`thread_store.rs` 已经 2000+ 行，`fork_thread` 有编译约束必须住那里（见 Task 4 说明），但 TUI 侧逻辑不放进去——`yi-agent` 不该反过来污染存储层。`replay.rs` 是纯函数，独立可测。

---

## Task 1: 跨进程会话写锁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（在 `impl ThreadStore` 之后追加，约行 1200）
- Test: 同一个文件的 `#[cfg(test)] mod tests`（既有）

**Interfaces:**
- Consumes: 无（本任务独立）
- Produces:
  - `pub struct ThreadWriterLock`（不透明，`Drop` 时释放内核锁）
  - `pub struct ThreadBusy { pub thread_id: String }`，实现 `std::error::Error` + `Display`
  - `pub fn acquire_thread_lock(root: &Path, thread_id: &str) -> Result<Arc<ThreadWriterLock>, ThreadBusy>`
  - `pub fn thread_lock_path(root: &Path, thread_id: &str) -> PathBuf`

- [ ] **Step 1: 写失败的测试**

在 `thread_store.rs` 的 `#[cfg(test)] mod tests` 里追加：

```rust
#[test]
fn thread_lock_is_idempotent_within_a_process() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let a = acquire_thread_lock(root, "thread-lock-idem").unwrap();
    let b = acquire_thread_lock(root, "thread-lock-idem").unwrap();
    // 同一进程内必须拿到同一个 Arc，否则 flock 按描述符加锁会自欺欺人
    assert!(Arc::ptr_eq(&a, &b));
}

#[test]
fn thread_lock_file_survives_release() {
    let dir = tempfile::TempDir::new().unwrap();
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
    let dir = tempfile::TempDir::new().unwrap();
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
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_lock
```

Expected: 编译失败，`cannot find function acquire_thread_lock`。

- [ ] **Step 3: 实现**

在 `thread_store.rs` 顶部 `use` 区确认有 `std::os::unix::io::AsRawFd`；追加：

```rust
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
        write!(
            f,
            "会话 {} 正被另一个 yi-agent 实例使用",
            self.thread_id
        )
    }
}

impl std::error::Error for ThreadBusy {}

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
        let mut table = PROCESS_LOCKS
            .get()
            .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(table) = table.as_mut() {
            table.remove(&self.path);
        }
        // `_file` 在此之后 drop，内核锁随之释放。
    }
}

/// 进程级去重表：`flock` 按**打开文件描述**加锁，不按路径。
///
/// 同一进程内两次 `open` 同一路径会得到两个描述符，各拿一次锁互不阻塞——于是
/// 「本进程已持有」会被自己判成空闲。表挂在进程级静态量上，与
/// `group_store.rs:120` 的 `static LOCK` 同思路。
static PROCESS_LOCKS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<ThreadWriterLock>>>,
> = std::sync::OnceLock::new();

fn process_locks(
) -> &'static std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<ThreadWriterLock>>>
{
    PROCESS_LOCKS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 取某个 thread 的写锁。进程内幂等（同一路径返回同一个 `Arc`）。
///
/// 非阻塞：冲突立即返回 [`ThreadBusy`]，调用方决定是报错还是提示 `fork`。
pub fn acquire_thread_lock(
    root: &Path,
    thread_id: &str,
) -> Result<Arc<ThreadWriterLock>, ThreadBusy> {
    let path = thread_lock_path(root, thread_id);

    // 先查表：本进程已持有则直接返回同一个 Arc。
    {
        let table = process_locks().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = table.get(&path) {
            return Ok(Arc::clone(existing));
        }
    }

    std::fs::create_dir_all(root).map_err(|_| ThreadBusy {
        thread_id: thread_id.to_string(),
    })?;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|_| ThreadBusy {
            thread_id: thread_id.to_string(),
        })?;

    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome != 0 {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(c) if c == libc::EWOULDBLOCK || c == libc::EAGAIN
        ) {
            return Err(ThreadBusy {
                thread_id: thread_id.to_string(),
            });
        }
        // 真故障（坏描述符 / 不支持的 fs）不能被误报成「会话被占用」，
        // 否则用户永远查不出原因。这里退化为放行并记账，由调用方的 IO 错误暴露。
        eprintln!("[thread_store] flock failed on {}: {error}", path.display());
    }

    let lock = Arc::new(ThreadWriterLock {
        _file: file,
        path: path.clone(),
    });
    process_locks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path, Arc::clone(&lock));
    Ok(lock)
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_lock
```

Expected: 3 passed（`child_tries_thread_lock` 是 `#[ignore]`，不参与计数）。

- [ ] **Step 5: 确认既有测试没被破坏**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store
```

Expected: 全 green。特别注意 `list()` 相关测试仍通过——锁文件不该出现在列表里。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs && \
git commit -m "feat(thread_store): 跨进程会话写锁

flock + 进程级去重表。flock 按打开文件描述加锁，同一进程两次 open
同一路径各拿一次锁互不阻塞，所以必须在静态表里记住「本进程已持有」。

锁文件永不删除：删掉会让已持有旧 inode 的进程与新建同名文件的进程
都以为持锁（同 device_store.rs:78 的说明）。"
```

---

## Task 1b: 给 `ThreadMeta` 加 `forked_from`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`

**Interfaces:**
- Produces: `ThreadMeta.forked_from: Option<String>`

**为什么单独一步、且必须在 Task 3 之前：** Task 3 的 `TuiSessionStore::create_new` 会构造 `ThreadMeta` 完整字面量，Task 7 的 `fork_thread` 也要读它。若等到 Task 7 才加字段，Task 3 编译不过（`missing field forked_from`）。加字段本身是纯机械改动，单独成步便于评审。

- [ ] **Step 1: 加字段**

```rust
    /// 从哪个 thread fork 而来。**仅用于展示**，任何读取路径都不看它。
    ///
    /// `#[serde(default)]` 是必须的：既有 meta 文件都没有这个字段，缺默认值会让
    /// 反序列化失败，`load` 走 `rebuild_meta` 兜底并把 `cwd` 冲成空串。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
```

- [ ] **Step 2: 让编译器找出所有待补的字面量**

```bash
cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -c "missing field"
```

Expected: 一个大于 0 的数字。逐个补 `forked_from: None`。

- [ ] **Step 3: 确认全 crate 编译通过**

```bash
cd yi-agent-rs && cargo build -p yi-agent-app-server
```

Expected: 编译成功。

- [ ] **Step 4: 有一段既有 meta 必须仍能反序列化**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store
```

Expected: 全 green。若 `load_*` 测试变红，说明 `#[serde(default)]` 漏了。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs && \
git commit -m "feat(thread_store): ThreadMeta 增加 forked_from 展示字段

必须 serde(default)：既有 meta 文件没有这个字段，缺默认值会让反序列化
失败并走 rebuild_meta 兜底，把 cwd 冲成空串。"
```

---

## Task 2: app-server 在启动 driver 时持锁

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/resume` 的 activation 路径，约行 6100-6200）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `acquire_thread_lock`、`ThreadWriterLock`（Task 1）
- Produces: `ThreadSession` 增加字段 `writer_lock: Option<Arc<ThreadWriterLock>>`；`thread/activate` 内部的 `start_driver_for` 返回 `Result<_, ThreadBusy>`

**背景（违反它就是做出一个把桌面堵死的 bug）：** app-server 的 thread 常驻内存，`state.threads` 只在会话结束才 `remove`。若在这里无条件长期持锁，桌面开着会话就等于 sidecar 持锁数小时，终端永远 `resume` 不了。所以**只在它真的要成为该 thread 的 writer 时取锁**，即启动 driver 那一刻，且 thread 从表里移除时随 `Arc` drop 释放。

- [ ] **Step 1: 写失败的测试**

`rpc_tests.rs` 的既有设施是 `Harness` + `initialize` + `read_response`（见 `rpc_tests.rs:1962`、`:2521`、`:2663`），**不要**自己编 `TestDaemon`。照下面写：

```rust
/// 桌面（app-server）与终端（另一个 writer）不能同时成为同一 thread 的 writer。
///
/// 关键反例：若把锁实现成「app-server 一旦打开 thread 就长期持锁」，
/// 桌面挂着某会话时终端永远 resume 不了。锁必须只在启动 driver 时取。
#[tokio::test(flavor = "multi_thread")]
async fn writer_lock_refuses_a_second_writer_on_the_same_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    // 建一个 thread
    h.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "thread/start",
        "params": { "cwd": cwd.to_string_lossy() }
    }).to_string()).await;
    let v = read_response(&mut h, 2).await;
    let tid = v["result"]["threadId"].as_str().unwrap().to_string();

    // 模拟另一个 writer（终端）已持有该 thread 的锁：
    // 用独立描述符自己 flock，绕开进程内去重表。
    let store = crate::thread_store::ThreadStore::new(&cwd);
    let lock_path = crate::thread_store::thread_lock_path(store.root_for_test(), &tid);
    let other = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    let rc = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&other),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    assert_eq!(rc, 0, "占位 flock 必须成功");

    // 桌面尝试 resume（= 启动该 thread 的 driver）：必须被拒
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    )).await;
    let v = read_response(&mut h, 3).await;
    assert!(
        v.get("error").is_some(),
        "第二个 writer 必须被拒绝，实际: {v}"
    );
    let message = v["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("正被另一个") || message.contains("busy"),
        "错误信息要能指导用户，实际: {message}"
    );

    // 释放占位锁后必须能正常 resume（否则就是锁没释放的死锁 bug）
    drop(other);
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
    )).await;
    let mut ok = None;
    for _ in 0..32 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(4)) {
            ok = Some(v);
            break;
        }
    }
    let ok = ok.expect("第二次 resume 必须有响应");
    assert!(ok.get("result").is_some(), "锁释放后必须能 resume，实际: {ok}");
    h.shutdown().await;
}

/// 同一 app-server 内的多客户端订阅不受 writer lock 影响（桌面 + 手机）。
#[tokio::test(flavor = "multi_thread")]
async fn writer_lock_does_not_block_subscriptions() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    h.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "thread/start",
        "params": { "cwd": cwd.to_string_lossy() }
    }).to_string()).await;
    let v = read_response(&mut h, 2).await;
    let tid = v["result"]["threadId"].as_str().unwrap().to_string();

    // 订阅不该被 writer lock 挡住
    h.send(&serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "thread/subscribe",
        "params": { "threadIds": [tid] }
    }).to_string()).await;
    let v = read_response(&mut h, 3).await;
    assert!(v.get("error").is_none(), "订阅不该受 writer lock 影响，实际: {v}");
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib writer_lock_
```

Expected: FAIL——第二个 writer 目前不会被拒（还没接锁）。

- [ ] **Step 3: 给 `ThreadSession` 加字段并在 activation 取锁**

`server.rs` 里 `ThreadSession` 结构体追加：

```rust
    /// 本进程作为该 thread writer 的凭据。`Some` 即表示「我在写它」。
    /// thread 从 `threads` 表移除时随 `Arc` drop 自然释放。
    pub(crate) writer_lock: Option<std::sync::Arc<crate::thread_store::ThreadWriterLock>>,
```

找到 `thread/activate` 里构造 `ThreadSession` 的地方（搜索 `ThreadSession {`），在拿到 `store` 之后、构造 `ThreadSession` 之前插入：

```rust
    // 成为该 thread 的 writer 前必须拿到跨进程锁。拿不到就直接报错，
    // 不建半个 session——半建状态会让后续 resume 看到不一致的运行时。
    let writer_lock = crate::thread_store::acquire_thread_lock(store.root_for_test(), &thread_id)
        .map_err(|busy| RpcError::internal(busy.to_string()))?;
```

并把 `writer_lock: Some(writer_lock),` 加进结构体字面量。

同时给 `ThreadStore` 加一个测试可见的根路径访问器（`thread_store.rs`）：

```rust
    /// 存储根的只读句柄。测试与调用方需要拼锁文件路径时用。
    pub fn root_for_test(&self) -> &Path {
        &self.root
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib writer_lock_
```

Expected: 2 passed。

- [ ] **Step 5: 跑该 crate 全部测试，确认没有回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 全 green。若 `thread/resume` 相关测试变红，多半是测试里重复 activate 同一 thread 撞了自己的锁——把测试改为先 `thread/close` 或用不同 thread id，**不要**因此把锁去掉。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs && \
git commit -m "feat(app-server): 启动 driver 时持会话写锁

只在真正成为 writer（启动 driver）时取锁，不覆盖多客户端订阅——
桌面与手机共享同一 driver，本来就不存在双写。

桌面开着的会话会让终端 resume 被拒，这是设计意图：提示用户 fork。"
```

---

## Task 3: TUI 会话存储封装

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/session_store.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（加 `mod session_store;`）
- Test: `yi-agent-rs/crates/yi-agent/src/session_store.rs` 内的 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `ThreadStore`、`ThreadWriterLock`、`acquire_thread_lock`、`now_millis`、`Translator`
- Produces:

```rust
pub struct TuiSessionStore { /* ... */ }

impl TuiSessionStore {
    /// 新建会话：写初始 meta 并持锁。
    pub fn create_new(workdir: &Path, model: &str, mode: ThreadMode) -> io::Result<Self>;
    /// 打开既有会话：载入历史并持锁。锁被占时返回 Err。
    pub fn open(workdir: &Path, thread_id: &str) -> Result<Self, OpenError>;
    pub fn thread_id(&self) -> &str;
    pub fn baseline_totals(&self) -> UsageTotals;
    pub fn append_turn(&self, items: &[Item], messages: &[Message], usage: Option<TurnUsage>, usage_total: Option<TurnUsage>) -> io::Result<()>;
    pub fn touch(&self, first_prompt: &str) -> io::Result<()>;
    pub fn clear(&self) -> io::Result<()>;
    pub fn compact(&self, new_messages: &[Message]) -> io::Result<()>;
}

pub enum OpenError { Busy(String), NotFound(String), Io(io::Error) }
```

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_new_writes_meta_and_holds_lock() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(dir.path(), "claude-test", ThreadMode::Normal)
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
    fn second_open_of_same_thread_is_busy() {
        let dir = tempfile::TempDir::new().unwrap();
        let first = TuiSessionStore::create_new(dir.path(), "m", ThreadMode::Normal).unwrap();
        let id = first.thread_id().to_string();

        // 同进程内 acquire 是幂等的，所以这里要用一个假的「别的进程已持有」
        // 场景：直接对锁文件加 flock，绕过进程内表。
        let lock_path = dir
            .path()
            .join(".yi-agent")
            .join("threads")
            .join(format!("{id}.lock"));
        let other = std::fs::OpenOptions::new()
            .write(true)
            .open(&lock_path)
            .unwrap();
        // 先释放本进程的（模拟终端退出），再用独立描述符占住
        drop(first);
        let rc = unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&other), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "占位 flock 应成功");

        match TuiSessionStore::open(dir.path(), &id) {
            Err(OpenError::Busy(_)) => {}
            other => panic!("期望 Busy，实际 {other:?}"),
        }
    }

    #[test]
    fn append_then_reopen_restores_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(dir.path(), "m", ThreadMode::Normal).unwrap();
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

        let reopened = TuiSessionStore::open(dir.path(), &id).unwrap();
        let loaded = reopened.load_for_test().unwrap();
        assert_eq!(loaded.items.len(), 1);
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.meta.title.as_deref(), Some("hello"));
    }

    #[test]
    fn clear_empties_the_log_but_keeps_the_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = TuiSessionStore::create_new(dir.path(), "m", ThreadMode::Normal).unwrap();
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
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib session_store
```

Expected: 编译失败，`session_store` 模块不存在。

- [ ] **Step 3: 实现 `session_store.rs`**

```rust
//! TUI 侧的会话存储封装。
//!
//! 只做三件事：持锁、把一轮的产物写进 `.jsonl`、把 `/clear` `/compact`
//! 翻译成存储层操作。所有真正的落盘语义都在 `ThreadStore`，这里不重复实现。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use yi_agent_app_server::thread_store::{
    acquire_thread_lock, now_millis, ThreadBusy, ThreadMeta, ThreadMode, ThreadStore, ThreadWriterLock,
    UsageTotals,
};
use yi_agent_app_server::protocol::Item;
use yi_agent_core::Message;

/// 打开既有会话时的失败原因。
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
    pub fn create_new(workdir: &Path, model: &str, mode: ThreadMode) -> std::io::Result<Self> {
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
            forked_from: None,
            content_epoch: 0,
            last_item_id: None,
            anchor_sha: None,
            last_read_at: 0,
            last_completed_at: 0,
        };
        store.create(&meta)?;
        let lock = acquire_thread_lock(store.root_for_test(), &thread_id).map_err(|busy| {
            std::io::Error::new(std::io::ErrorKind::WouldBlock, busy.to_string())
        })?;
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

        let lock = acquire_thread_lock(store.root_for_test(), thread_id)
            .map_err(|busy: ThreadBusy| OpenError::Busy(busy.to_string()))?;

        Ok(Self {
            store,
            thread_id: thread_id.to_string(),
            _lock: lock,
            baseline: loaded.totals,
        })
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
        usage: Option<yi_agent_app_server::thread_store::TurnUsage>,
        usage_total: Option<yi_agent_app_server::thread_store::TurnUsage>,
    ) -> std::io::Result<()> {
        self.store.append_turn(
            &self.thread_id,
            &yi_agent_app_server::thread_store::TurnLine::Turn {
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
    /// 复用既有 `truncate`——它同时做这三件事，且注释里写明了这就是 `/clear`
    /// 的持久化语义。
    pub fn clear(&self) -> std::io::Result<()> {
        self.store.truncate(&self.thread_id)?;
        self.store.bump_content_epoch(&self.thread_id)?;
        Ok(())
    }

    /// `/compact`：只换上下文，不写 items。
    pub fn compact(&self, new_messages: &[Message]) -> std::io::Result<()> {
        self.store.append_turn(
            &self.thread_id,
            &yi_agent_app_server::thread_store::TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: new_messages.to_vec(),
            },
        )
    }

    #[cfg(test)]
    fn load_for_test(&self) -> std::io::Result<yi_agent_app_server::thread_store::LoadedThread> {
        Ok(self.store.load(&self.thread_id)?.expect("thread should exist"))
    }
}
```

同时 `main.rs` 加 `mod session_store;`（按字母序插在 `mod schedule_intent;` 之后）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib session_store
```

Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/session_store.rs yi-agent-rs/crates/yi-agent/src/main.rs && \
git commit -m "feat(cli): TUI 会话存储封装

create_new 同时写 meta 与持锁：不写 meta 则 --last 看不到终端会话，
不持锁则给别的进程留后门。

/clear 复用 ThreadStore::truncate（它已同时做删日志、清 partial、
持 meta_lock 三件事），/compact 写一条空 items 的 Turn 行。"
```

---

## Task 4: `Item → HistoryCell` 回放映射

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/replay.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`（加 `pub mod replay;`）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/replay.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `yi_agent_app_server::protocol::{Item, ToolStatus}`、`crate::tui::cell::{HistoryCell, CallState}`
- Produces: `pub fn items_to_cells(items: &[Item]) -> Vec<HistoryCell>`

- [ ] **Step 1: 写失败的测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::{CallState, HistoryCell};

    fn user(text: &str) -> Item {
        Item::UserMessage {
            id: "u".into(),
            text: text.into(),
            attachments: Vec::new(),
            images: Vec::new(),
        }
    }

    fn tool(status: ToolStatus, result: Option<&str>) -> Item {
        Item::ToolCall {
            id: "t".into(),
            call_id: "c".into(),
            name: "bash".into(),
            input: serde_json::json!({ "cmd": "ls" }),
            status,
            result: result.map(str::to_string),
            images: Vec::new(),
        }
    }

    #[test]
    fn user_message_maps_to_user_cell() {
        let cells = items_to_cells(&[user("hi")]);
        assert_eq!(cells.len(), 1);
        assert!(matches!(&cells[0], HistoryCell::UserMessage { text } if text == "hi"));
    }

    #[test]
    fn completed_tool_call_maps_to_success_not_failed() {
        let cells = items_to_cells(&[tool(ToolStatus::Completed, Some("ok"))]);
        // ToolCall + ToolResult
        assert_eq!(cells.len(), 2);
        assert!(
            matches!(&cells[0], HistoryCell::ToolCall { state: CallState::Success, .. }),
            "Completed 必须映射成 Success；写成 Failed 会让所有历史工具调用变红"
        );
        assert!(matches!(&cells[1], HistoryCell::ToolResult { is_error: false, .. }));
    }

    #[test]
    fn running_tool_call_reads_as_interrupted() {
        let cells = items_to_cells(&[tool(ToolStatus::Running, None)]);
        assert!(matches!(&cells[0], HistoryCell::ToolCall { state: CallState::Running, .. }));
        match &cells[1] {
            HistoryCell::ToolResult { result_text, is_error } => {
                assert_eq!(result_text, "中断");
                assert!(!is_error, "中断不是错误");
            }
            other => panic!("期望 ToolResult，实际 {other:?}"),
        }
    }

    #[test]
    fn failed_tool_call_without_output_is_marked() {
        let cells = items_to_cells(&[tool(ToolStatus::Failed, None)]);
        assert!(matches!(&cells[0], HistoryCell::ToolCall { state: CallState::Failed, .. }));
        match &cells[1] {
            HistoryCell::ToolResult { result_text, is_error } => {
                assert_eq!(result_text, "(无输出)");
                assert!(is_error);
            }
            other => panic!("期望 ToolResult，实际 {other:?}"),
        }
    }

    #[test]
    fn redacted_thinking_produces_no_cell() {
        let item = Item::Thinking {
            id: "th".into(),
            text: String::new(),
            redacted: true,
        };
        assert!(
            items_to_cells(&[item]).is_empty(),
            "redacted 的 Thinking text 为空，硬映射会渲染出一条空白折叠块"
        );
    }

    #[test]
    fn thinking_is_folded_by_default() {
        let item = Item::Thinking {
            id: "th".into(),
            text: "reasoning".into(),
            redacted: false,
        };
        let cells = items_to_cells(&[item]);
        assert!(matches!(&cells[0], HistoryCell::Reasoning { expanded: false, .. }));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib replay
```

Expected: 编译失败，`replay` 模块不存在。

- [ ] **Step 3: 实现**

```rust
//! 把落盘的 `Item` 还原成 TUI 的 `HistoryCell`。
//!
//! 没有这一层，`resume` 之后上下文是对的、屏幕却是空的——用户会以为历史丢了。

use yi_agent_app_server::protocol::{Item, ToolStatus};

use crate::tui::cell::{CallState, HistoryCell};

/// 按顺序把 items 摊平成可渲染的 cells。
///
/// 一个 `ToolCall` 会产出两个 cell（调用 + 结果），与实时渲染的形态一致。
/// 不插 `Separator`：分隔线只在 turn 边界由实时路径插入，这里再插会让重开
/// 一次会话就多出一堆横线。
pub fn items_to_cells(items: &[Item]) -> Vec<HistoryCell> {
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::UserMessage { text, .. } | Item::UserInterjection { text, .. } => {
                out.push(HistoryCell::UserMessage { text: text.clone() });
            }
            Item::AgentMessage { text, .. } => {
                out.push(HistoryCell::AssistantMessage {
                    markdown: text.clone(),
                });
            }
            Item::ToolCall {
                id,
                name,
                input,
                status,
                result,
                ..
            } => {
                // `ToolStatus` 与 `CallState` 不同名：Completed -> Success。
                let state = match status {
                    ToolStatus::Running => CallState::Running,
                    ToolStatus::Completed => CallState::Success,
                    ToolStatus::Failed => CallState::Failed,
                };
                out.push(HistoryCell::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                    state,
                    expanded: false,
                });
                let (result_text, is_error) = match status {
                    ToolStatus::Completed => (result.clone().unwrap_or_default(), false),
                    ToolStatus::Failed => (
                        result.clone().unwrap_or_else(|| "(无输出)".to_string()),
                        true,
                    ),
                    // 崩溃残留：永远停在 Running，不给说明会像卡死。
                    ToolStatus::Running => ("中断".to_string(), false),
                };
                out.push(HistoryCell::ToolResult {
                    id: id.clone(),
                    result_text,
                    is_error,
                    expanded: false,
                });
            }
            Item::Thinking { text, redacted, .. } => {
                // redacted 时 text 是空串；硬映射会渲染出一条空白折叠块。
                if *redacted || text.is_empty() {
                    continue;
                }
                out.push(HistoryCell::Reasoning {
                    text: text.clone(),
                    expanded: false,
                });
            }
        }
    }
    out
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib replay
```

Expected: 6 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/tui/replay.rs yi-agent-rs/crates/yi-agent/src/tui/mod.rs && \
git commit -m "feat(tui): Item 到 HistoryCell 的回放映射

ToolStatus 是 Running/Completed/Failed，CallState 是 Running/Success/Failed，
不同名，必须显式转换。写成 Completed->Failed 会让所有历史工具调用变红。

Running 状态的 tool call 是崩溃残留，结果文本写「中断」而不是留空。
redacted 的 Thinking text 为空，跳过不生成 cell。"
```

---

## Task 5: TUI driver 落盘

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`run_tui_agent` 的 driver 闭包，约行 2100-2500）
- Test: 无单测（TUI driver 不可单测），由 Task 8 的端到端覆盖

**Interfaces:**
- Consumes: `TuiSessionStore`、`Translator`、`items_to_cells`
- Produces: driver 内 `Option<TuiSessionStore>` 与 `Option<Translator>`；`/clear` `/compact` 分支接入

**说明：** 这一步改动面最大、最贴现有代码。`run_tui_agent` 的 driver 闭包目前用 `Agent::new(...).with_permission(...)` 构造，没有 session。要改成启动即持 `Arc<Mutex<Session>>`。

- [ ] **Step 1: 在闭包外建 store，闭包内接 session**

在 `run_tui_agent` 里、`tokio::spawn` 之前插入（`cli` 与 `workdir` 此时仍可用）：

```rust
    // 会话存储：resume 时由调用方传入已打开的 store，新会话则现建。
    // 建失败不阻断 TUI——只是这次会话不落盘，用户仍能对话。
    let session_store: Option<Arc<crate::session_store::TuiSessionStore>> =
        match crate::session_store::TuiSessionStore::create_new(
            &workdir,
            &config.model,
            if config.yolo {
                yi_agent_app_server::thread_store::ThreadMode::Yolo
            } else {
                yi_agent_app_server::thread_store::ThreadMode::Normal
            },
        ) {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                eprintln!("[yi-agent] 本次会话不落盘: {e}");
                None
            }
        };
```

环节说明：Task 6 会把这里改成「有 `--resume` 就用 `open`」。本任务先只做新会话。

- [ ] **Step 2: 闭包启动时持 session Arc**

把 driver 闭包内：

```rust
            let mut agent = yi_agent_core::Agent::new(
                Arc::clone(&provider_clone),
                Arc::clone(&current_tools),
                config_clone,
            )
            .with_permission(Arc::clone(&current_checker), Arc::clone(&decision_rx));
```

改成：

```rust
            // session Arc 必须在首次构造时就存在，且要跨 rebuild 存活——
            // 子 agent 的 CallerContext 绑在它上面。
            let session_handle = Arc::new(std::sync::Mutex::new(yi_agent_core::Session::new()));
            let mut agent = yi_agent_core::Agent::new(
                Arc::clone(&provider_clone),
                Arc::clone(&current_tools),
                config_clone,
            )
            .with_permission(Arc::clone(&current_checker), Arc::clone(&decision_rx))
            .with_session_arc(Arc::clone(&session_handle));
```

并把 `session_handle` 传给 `rebuild_driver_agent`（Task 6 处理 resume 时也复用它）。

- [ ] **Step 3: 每轮累积 items，在终止事件后落盘**

在闭包顶部（`let mut live = LiveRebuild::new(...)` 旁边）建**跨轮存活**的 `translator`，在 loop 内维护每轮清空的累积器：

```rust
            // Translator 负责把 AgentEvent 翻译成 Item，与 app-server 落盘走完全
            // 相同的实现，不存在第二套口径。跨轮存活：committed 累计要连续。
            let mut translator = session_store.as_ref().map(|s| {
                yi_agent_app_server::translate::Translator::new(
                    s.thread_id().to_string(),
                    s.baseline_totals(),
                )
            });
            let mut turn_items: Vec<yi_agent_app_server::protocol::Item> = Vec::new();
            let mut last_usage: Option<yi_agent_app_server::thread_store::TurnUsage> = None;
            let mut usage_total: Option<yi_agent_app_server::thread_store::TurnUsage> = None;
            // 每轮的 session 基线长度，落盘时用来判断有没有被取消回滚。
            let mut base_len = 0usize;
```

在事件转发处（`agent_tx.send(ev).await` 之前）追加。**两个用量字段都从 `Notification::TokenUsage` 里取**，与 app-server 的做法逐字一致（`server.rs:5715-5735`）——那条通知携带的就是本轮完整快照，不需要给 `Translator` 加任何新 API：

```rust
                                        if let Some(t) = translator.as_mut() {
                                            for n in t.on_event(ev.clone()) {
                                                match n {
                                                    yi_agent_app_server::protocol::Notification::ItemCompleted { item, .. } => {
                                                        turn_items.push(item);
                                                    }
                                                    yi_agent_app_server::protocol::Notification::TokenUsage {
                                                        model,
                                                        input_tokens,
                                                        output_tokens,
                                                        cache_creation_input_tokens,
                                                        cache_read_input_tokens,
                                                        ..
                                                    } => {
                                                        last_usage = Some(yi_agent_app_server::thread_store::TurnUsage {
                                                            model: model.clone(),
                                                            input_tokens,
                                                            output_tokens,
                                                            cache_creation_input_tokens,
                                                            cache_read_input_tokens,
                                                        });
                                                        // 同一条通知里带着 translator 算好的累计量
                                                        let totals = t.turn_totals();
                                                        usage_total = Some(yi_agent_app_server::thread_store::TurnUsage {
                                                            model,
                                                            input_tokens: yi_agent_app_server::server::saturate_u32(totals.input_tokens),
                                                            output_tokens: yi_agent_app_server::server::saturate_u32(totals.output_tokens),
                                                            cache_creation_input_tokens: yi_agent_app_server::server::saturate_u32(totals.cache_creation_input_tokens),
                                                            cache_read_input_tokens: yi_agent_app_server::server::saturate_u32(totals.cache_read_input_tokens),
                                                        });
                                                    }
                                                    _ => {}
                                                }
                                            }
                                        }
```

**但 `turn_items` 里没有用户提问——必须自己补。** 这是最容易漏、且漏了后果最重的一步：`Translator::on_event` 只翻译 agent 侧事件（`AssistantText` / `ToolCall` / `Thinking` / `UserInterjection`），**从不产出 `Item::UserMessage`**。`server.rs:4794` 的注释把原因写死了：

> 基线 server 不 emit userMessage，必须在落盘时补齐，否则 resume 会丢用户提问。

不补的话，resume 出来的会话只有回答、没有问题——用户会以为自己的话没被记住。所以落盘前先把本轮的 `Item::UserMessage` 插到最前面：

```rust
                            if let Some(store) = session_store.as_ref() {
                                // 与 server.rs:4794 同款补齐：Translator 不产出
                                // UserMessage，缺了它 resume 会丢提问。
                                let mut items = Vec::with_capacity(turn_items.len() + 1);
                                items.push(yi_agent_app_server::protocol::Item::UserMessage {
                                    id: format!("user-{}", uuid::Uuid::new_v4()),
                                    text: text.clone(),
                                    attachments: Vec::new(),
                                    images: Vec::new(),
                                });
                                items.extend(turn_items.iter().cloned());

                                if let Err(e) = store.append_turn(&items, &messages, last_usage.clone(), usage_total.clone()) {
                                    eprintln!("[yi-agent] 落盘失败（对话继续）: {e}");
                                }
                                if let Err(e) = store.touch(&prompt_for_log) {
                                    eprintln!("[yi-agent] 更新 meta 失败: {e}");
                                }
                            }
```

**要点：`translator` 跨轮存活，每轮只 `set_turn`，不要重建。** 重建会把 `committed` 累计清零，用量就断在每轮边界上。每轮开始时：

```rust
                // set_turn 必须在 on_event 之前调：alloc_item_id 依赖 turn_id。
                // 不要在这里重建 Translator：那会清掉 committed 累计。
                if let Some(t) = translator.as_mut() {
                    t.set_turn(format!("turn-{}", uuid::Uuid::new_v4()));
                }
                turn_items.clear();
                last_usage = None;
                usage_total = None;
                // 必须在 `agent.run(text)` 之前取：run 内部会 push 本轮 user
                // message（agent.rs:739），取晚了基线就含它，net 恒少 1。
                base_len = session_handle
                    .lock()
                    .map(|s| s.messages().len())
                    .unwrap_or(0);
```

**item id 必须全局唯一。** `Translator` 的 `alloc_item_id` 生成的是 `item-{turn_id}-{n}`，而 `next_item` 只在 `Translator::new` 时置 1、`set_turn` **不重置**它。所以只要 `turn_id` 每轮唯一，id 就不会撞。**坏掉的情形**是不调 `set_turn` 直接 `on_event`：`turn_id` 为空串，所有行生成 `item--1`、`item--2`，跨轮重复；而 `partial_is_committed`（`thread_store.rs`）正是靠**首个 item id** 判断「这轮是否已落盘」，id 重复会让崩溃恢复误判，把已落盘的轮次当成未收尾再追加一遍。

`AgentEvent` 已派生 `Clone`（`agent.rs:265` 的 `#[derive(Debug, Clone, Serialize)]`），`ev.clone()` 可直接用。

**落盘不能只挂在 `Done` 上——`Cancelled` 同样是终止事件。** `is_terminal`（`main.rs:1626`）把 `Done`、`Cancelled`、`Error` 三者都当终止。而 agent 的取消路径（`agent.rs:1134`、`1163`、`1190`、`1248`）都是 `truncate(keep)` 之后发 `Cancelled` 再 `return`，**压根不发 `Done`**。只挂 `Done` 的话，用户每按一次 Ctrl+C 那一轮就静默丢失，resume 回来完全没有痕迹。

所以落盘点是「见到终止事件就收尾」，`match` 形状照 `main.rs:1626` 的 `is_terminal` 写：

```rust
                            // `Done` / `Cancelled` / `Error` 都是终止事件。
                            // 取消路径不发 Done（agent.rs:1134 等），漏掉
                            // Cancelled 会让每次 Ctrl+C 的那一轮永久丢失。
                            let terminal = matches!(
                                ev,
                                yi_agent_core::AgentEvent::Done { .. }
                                    | yi_agent_core::AgentEvent::Cancelled
                                    | yi_agent_core::AgentEvent::Error(_)
                            );
```

**并且必须按 session 的实际长度裁剪 `turn_items`。** 取消路径会 `truncate(keep)` 回滚 session（`safe_cancel_truncate_len`），但 `turn_items` 是纯累积的，里面的 item 对应的是**回滚前**的事件流。不裁剪就会把被撤销的回答写进磁盘，而 `messages` 快照里没有它——item 与上下文对不上，resume 后 UI 显示一段 model 并没有说过的话。

裁剪规则：记下每轮开始时的 baseline 长度，落盘前按 `messages.len() - baseline` 折算保留多少 item。

**先留一份提问副本。** `agent.run(text)` 的签名是 `run(&mut self, user_prompt: String)`（`agent.rs:702`），会消费掉 `text`；而落盘要用它补 `Item::UserMessage`、`touch` 要用它做标题兜底。所以在 `match agent.run(text).await` **之前**加一行：

```rust
                // `agent.run(text)` 会 move 掉 text；落盘与 touch 都需要它。
                let prompt_for_log = text.clone();
```

**完整落盘块**（终止事件到达后、`is_running` 置 false 之前）：

```rust
                            if terminal {
                                if let Some(store) = session_store.as_ref() {
                                    let messages = session_handle
                                        .lock()
                                        .map(|s| s.messages().to_vec())
                                        .unwrap_or_default();

                                    // 本轮 net 增长的 message 数。取消时 session
                                    // 被 truncate 回滚，net 会变小甚至为 0。
                                    let net = messages.len().saturating_sub(base_len);

                                    // 必须自己补 UserMessage：Translator 只翻译
                                    // agent 侧事件，从不产出它（server.rs:4794
                                    // 同款注释）。漏了这一步，resume 出来的会话
                                    // 只有回答没有问题。
                                    let mut items = Vec::with_capacity(turn_items.len() + 1);
                                    items.push(yi_agent_app_server::protocol::Item::UserMessage {
                                        id: format!("user-{}", uuid::Uuid::new_v4()),
                                        text: prompt_for_log.clone(),
                                        attachments: Vec::new(),
                                        images: Vec::new(),
                                    });

                                    // 按 session 实际长度裁剪：宁可少写不可多写。
                                    // 多写的内容在 session 里不存在，resume 后会
                                    // 显示模型没说过的话。`net == 0` 表示连 user
                                    // prompt 都被回滚了，此时只留那 1 条 user item
                                    // ——它确实是用户问过的，不算伪造。
                                    let keep = net.max(1).min(turn_items.len() + 1);
                                    items.extend(turn_items.iter().take(keep - 1).cloned());

                                    if let Err(e) = store.append_turn(&items, &messages, last_usage.clone(), usage_total.clone()) {
                                        eprintln!("[yi-agent] 落盘失败（对话继续）: {e}");
                                    }
                                    if let Err(e) = store.touch(&prompt_for_log) {
                                        eprintln!("[yi-agent] 更新 meta 失败: {e}");
                                    }
                                }
                            }
```

**`saturate_u32` 需要提升可见性**：它在 `server.rs:4711` 是私有 `fn`。改为 `pub(crate) fn saturate_u32`——本计划里 `thread_store::fork_thread` 也要用它，两处都不能复制一份（复制就等于两套溢出语义）。

- [ ] **Step 4: `/clear` 与 `/compact` 接存储**

`DriverControl::Clear` 分支（`main.rs:2133`）追加：

```rust
                            if let Some(store) = session_store.as_ref() {
                                if let Err(e) = store.clear() {
                                    eprintln!("[yi-agent] /clear 落盘失败: {e}");
                                }
                            }
                            // 用量累计也要归零：否则清空对话后状态栏仍显示旧总数，
                            // 用户会以为清空没生效。app-server 在同一位置调
                            // `translator.reset_totals()`（server.rs:5368）。
                            if let Some(t) = translator.as_mut() {
                                let _ = t.reset_totals();
                            }
```

`DriverControl::Compact` 分支（`main.rs:2149`）追加：

```rust
                            if let Some(store) = session_store.as_ref() {
                                let messages = session_handle
                                    .lock()
                                    .map(|s| s.messages().to_vec())
                                    .unwrap_or_default();
                                if let Err(e) = store.compact(&messages) {
                                    eprintln!("[yi-agent] /compact 落盘失败: {e}");
                                }
                            }
```

- [ ] **Step 5: 编译并跑既有测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib
```

Expected: 全 green。落盘失败不该让任何既有测试变红。

- [ ] **Step 6: 手动冒烟**

```bash
cd /tmp && rm -rf resume-smoke && mkdir resume-smoke && cd resume-smoke && \
echo "检查 .yi-agent/threads 下是否生成了 meta 与 jsonl" && \
ls ~/.yi-agent 2>/dev/null
```

启动 `cargo run -p yi-agent` 聊一句后退出，然后：

```bash
find /tmp/resume-smoke/.yi-agent/threads -type f | sort
```

Expected: 看到 `<id>.meta.json`、`<id>.jsonl`、`<id>.lock`。`.jsonl` 里至少一行 `{"type":"turn",...}`。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/tui/ yi-agent-rs/crates/yi-agent-app-server/src/translate.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs && \
git commit -m "feat(cli): TUI 每轮落盘

driver 启动即持 session Arc，每轮用 translate::Translator 把 AgentEvent
翻成 Item（与 app-server 完全同一实现），Done 后 append_turn + touch。

/clear 接 ThreadStore::truncate，/compact 写空 items 的 Turn 行。
落盘失败只记日志，不阻断对话。"
```

---

## Task 6: CLI 表面与 resume

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`（`Command` 枚举）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（dispatch + `run_tui_agent` 接受 store）
- Create: `yi-agent-rs/crates/yi-agent/src/resume.rs`
- Test: `yi-agent-rs/crates/yi-agent/src/config.rs` 内 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `TuiSessionStore::open`、`forkable_messages`、`items_to_cells`
- Produces:
  - `Command::Resume { thread_id: Option<String>, last: bool, prompt: Option<String> }`
  - `Command::Fork { thread_id: String, prompt: Option<String> }`
  - `Command::Threads`
  - `pub fn resolve_thread_id(store: &ThreadStore, spec: &str) -> Result<String, ResolveError>`
  - `pub fn pick_last(store: &ThreadStore, workdir: &Path) -> Option<ThreadMeta>`

- [ ] **Step 1: 写 CLI 消歧的失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_last_with_positional_treats_it_as_prompt() {
        let cli = Cli::try_parse_from(["yi-agent", "resume", "--last", "修复这个 bug"]).unwrap();
        let Some(Command::Resume { thread_id, last, prompt }) = cli.command else {
            panic!("应解析为 Resume");
        };
        assert!(last);
        assert_eq!(thread_id, None, "positional 必须被当成 prompt 而不是 thread id");
        assert_eq!(prompt.as_deref(), Some("修复这个 bug"));
    }

    #[test]
    fn resume_without_last_treats_positional_as_thread_id() {
        let cli = Cli::try_parse_from(["yi-agent", "resume", "thread-abc"]).unwrap();
        let Some(Command::Resume { thread_id, last, prompt }) = cli.command else {
            panic!("应解析为 Resume");
        };
        assert!(!last);
        assert_eq!(thread_id.as_deref(), Some("thread-abc"));
        assert_eq!(prompt, None);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib config::tests::resume
```

Expected: 编译失败，`Command::Resume` 不存在。

- [ ] **Step 3: 加子命令定义**

`config.rs` 的 `Command` 枚举里追加：

```rust
    /// 恢复一个既有会话并继续对话。
    Resume {
        /// 会话 id（或唯一前缀）。省略时配合 `--last` 使用。
        thread_id: Option<String>,
        /// 恢复同目录最近一条会话。
        #[arg(long)]
        last: bool,
        /// 恢复后立即发送的提示词。
        prompt: Option<String>,
    },
    /// 复制一份会话历史，开新会话继续。
    Fork {
        /// 源会话 id（或唯一前缀）。
        thread_id: String,
        /// fork 后立即发送的提示词。
        prompt: Option<String>,
    },
    /// 列出当前目录的会话。
    Threads,
```

`thread_id` 是普通 positional、`prompt` 也是 positional 会冲突。用 `#[arg()]` 保持两个 positional 会有歧义，所以 `prompt` 声明为第二个 positional：

```rust
    Resume {
        /// 会话 id（或唯一前缀）。省略时配合 `--last` 使用。
        thread_id: Option<String>,
        #[arg(long)]
        last: bool,
        /// 恢复后立即发送的提示词。
        prompt: Option<String>,
    },
```

clap 允许两个 `Option<String>` positional，按顺序绑定。`resume --last "x"` 会把 `"x"` 绑到 `thread_id`（第一个 positional）——这正是要消歧的地方，在 dispatch 里处理（Step 5）。

- [ ] **Step 4: 写 `resume.rs` 的解析逻辑**

```rust
//! `resume` / `fork` / `threads` 三个子命令的实现。
//!
//! 落盘语义一律在 `TuiSessionStore`；这里只做「解析 id → 打开 store → 交给
//! TUI」和「fork 的编排」。

use std::path::{Path, PathBuf};

use yi_agent_app_server::thread_store::{ThreadMeta, ThreadStore};

/// id 解析失败。
#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound(spec) => {
                write!(f, "找不到会话 {spec}（用 `yi-agent threads` 查看）")
            }
            ResolveError::Ambiguous(candidates) => {
                writeln!(f, "前缀有歧义，候选：")?;
                for c in candidates {
                    writeln!(f, "  {c}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// 把「完整 id 或唯一前缀」解析成完整 id。
pub fn resolve_thread_id(store: &ThreadStore, spec: &str) -> Result<String, ResolveError> {
    let all = store.list().unwrap_or_default();
    if all.iter().any(|m| m.thread_id == spec) {
        return Ok(spec.to_string());
    }
    let matches: Vec<String> = all
        .iter()
        .filter(|m| m.thread_id.starts_with(spec))
        .map(|m| m.thread_id.clone())
        .collect();
    match matches.len() {
        0 => Err(ResolveError::NotFound(spec.to_string())),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => Err(ResolveError::Ambiguous(matches)),
    }
}

/// `--last`：同目录最近一条。
///
/// 比较前先 canonical，否则同一目录的符号链接与相对路径写法匹配不上。
pub fn pick_last(store: &ThreadStore, workdir: &Path) -> Option<ThreadMeta> {
    let want = workdir.canonicalize().unwrap_or_else(|_| workdir.to_path_buf());
    let mut all = store.list().ok()?;
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    all.into_iter().find(|m| {
        // `rebuild_meta` 重建出的老会话 cwd 是空串，按「与当前目录相同」处理。
        if m.cwd.is_empty() {
            return true;
        }
        let got = PathBuf::from(&m.cwd);
        got.canonicalize().map(|p| p == want).unwrap_or(false)
    })
}
```

- [ ] **Step 5: dispatch 接入**

`main.rs` 的 `match cli.command` 里追加：

```rust
        Some(Command::Resume {
            thread_id,
            last,
            prompt,
        }) => {
            // `resume --last "x"` 的 positional 落在 thread_id 上，但它是提示词。
            let (spec, prompt) = if last {
                (None, thread_id.or(prompt))
            } else {
                (thread_id, prompt)
            };
            run_resume(cli, spec, last, prompt)
        }
        Some(Command::Threads) => run_threads(cli),
        Some(Command::Fork { thread_id, prompt }) => run_fork(cli, thread_id, prompt),
```

- [ ] **Step 6: 给 `run_agent` 加可选 seed，不要复制它的装配逻辑**

`run_agent`（`main.rs:1298`）里有 skills 注册、MCP 注册、builtin tools、process manager、权限 checker 等 100+ 行装配。**绝对不能为 resume 复制一份**——那会立刻产生两个漂移的实现。

改成接受一个可选 `ResumeSeed`：

```rust
/// resume 时带进 TUI 的既有会话。
pub(crate) struct ResumeSeed {
    pub store: crate::session_store::TuiSessionStore,
    /// 已按 `forkable_messages` 截断、可直接种进 Session 的转录。
    pub messages: Vec<yi_agent_core::Message>,
    /// 上次的 input token 数，让 auto-compact 在首轮即生效。
    pub last_input_tokens: Option<u32>,
    /// 启动后立即发送的提示词。
    pub prompt: Option<String>,
}

fn run_agent(cli: Cli) -> Result<()> {
    run_agent_with_seed(cli, None)
}

fn run_agent_with_seed(cli: Cli, seed: Option<ResumeSeed>) -> Result<()> {
    // 这里就是现有 `run_agent`（main.rs:1298）的完整函数体，逐字搬过来。
    // 唯一的改动是末尾 `run_tui_agent(...)` 多传一个 `seed`。
    run_tui_agent(
        provider,
        tools,
        agent_config,
        agent_workdir,
        checker,
        decision_tx,
        decision_rx,
        cli,
        config,
        base_registry,
        process_manager,
        prompt.catalog,
        mcp,
        seed,          // 新增参数
    )
}
```

即：把现有 `run_agent` 的函数体整体改名为 `run_agent_with_seed`，补一个多出来的 `seed` 形参并透传给 `run_tui_agent`；再留一个 5 行的 `run_agent(cli)` 薄包装调用 `run_agent_with_seed(cli, None)`。这样 `None` 分支是逐字零回归。

- [ ] **Step 7: 实现 `run_resume` / `run_threads` / `run_fork`**

```rust
fn run_resume(
    cli: Cli,
    spec: Option<String>,
    last: bool,
    prompt: Option<String>,
) -> Result<()> {
    let config = config::load_lenient(&cli)?;
    let workdir = config.workdir.clone();
    let store = yi_agent_app_server::thread_store::ThreadStore::new(&workdir);

    let thread_id = match (spec, last) {
        (Some(spec), _) => resume::resolve_thread_id(&store, &spec)?,
        (None, true) => resume::pick_last(&store, &workdir)
            .map(|m| m.thread_id)
            .ok_or_else(|| anyhow::anyhow!("当前目录没有可恢复的会话"))?,
        (None, false) => anyhow::bail!("需要会话 id，或加 `--last`"),
    };

    // cwd 已删必须明确报错，不得回退到当前 workdir——回退会让历史里的相对路径
    // 全部指向错误位置（spec §5 第 4 点）。`rebuild_meta` 重建出的老会话 cwd 是
    // 空串，那种情况视为「与当前目录相同」放行。
    let meta_cwd = store
        .load(&thread_id)?
        .map(|l| l.meta.cwd)
        .unwrap_or_default();
    if !meta_cwd.is_empty() && !std::path::Path::new(&meta_cwd).is_dir() {
        anyhow::bail!(
            "会话 {thread_id} 的工作目录已不存在：{meta_cwd}\n\
             如有需要，用 `yi-agent fork {thread_id}` 在新目录继续"
        );
    }

    // OpenError 要区分「被占用」与「不存在」：前者提示 fork，后者提示 threads。
    let session = crate::session_store::TuiSessionStore::open(&workdir, &thread_id)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let (messages, last_input_tokens) = session.load_seed()?;
    run_agent_with_seed(
        cli,
        Some(ResumeSeed {
            store: session,
            messages,
            last_input_tokens,
            prompt,
        }),
    )
}

fn run_threads(cli: Cli) -> Result<()> {
    let config = config::load_lenient(&cli)?;
    let store = yi_agent_app_server::thread_store::ThreadStore::new(&config.workdir);
    let mut all = store.list()?;
    if all.is_empty() {
        println!("当前目录没有会话");
        return Ok(());
    }
    all.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    println!("{:<40}  {:<26}  {}", "ID", "标题", "更新时间");
    for m in all {
        let title = m.title.unwrap_or_else(|| "(untitled)".to_string());
        println!(
            "{:<40}  {:<26}  {}",
            m.thread_id,
            title.chars().take(26).collect::<String>(),
            format_millis_local(m.updated_at)
        );
    }
    Ok(())
}

/// `updated_at` 是 Unix 毫秒。项目里没有既有的时间格式化函数（`control_agents`
/// 只打 task_id 与 state），所以这里给一个最小的本地时间渲染。
///
/// 用 `chrono` 需要有这个依赖；`yi-agent` 目前没有，所以走 `libc::localtime_r`——
/// `libc` 已是本 crate 的依赖（TUI 的 OSC 11 探测在用），不必新增依赖。
fn format_millis_local(ms: i64) -> String {
    let secs = ms / 1000;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time: libc::time_t = secs as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return format!("{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

fn run_fork(cli: Cli, spec: String, prompt: Option<String>) -> Result<()> {
    let config = config::load_lenient(&cli)?;
    let workdir = config.workdir.clone();
    let store = yi_agent_app_server::thread_store::ThreadStore::new(&workdir);
    let source = resume::resolve_thread_id(&store, &spec)?;

    let new_id = format!("thread-{}", uuid::Uuid::new_v4());
    let created = store.fork_thread(&source, &new_id)?;

    let session = crate::session_store::TuiSessionStore::open(&workdir, &created)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (messages, last_input_tokens) = session.load_seed()?;
    run_agent_with_seed(
        cli,
        Some(ResumeSeed {
            store: session,
            messages,
            last_input_tokens,
            prompt,
        }),
    )
}
```


- [ ] **Step 8: `run_tui_agent` 接受 seed 并种入 session**

签名末尾加 `seed: Option<ResumeSeed>`，在 TUI history 初始化之后（Task 9）、driver 启动之前：

```rust
    /// resume 时沿用已打开的 store；否则 Task 5 现建。
    let (session_store, initial_messages, initial_last_input, startup_prompt) = match seed {
        Some(seed) => (
            Some(Arc::new(seed.store)),
            seed.messages,
            seed.last_input_tokens,
            seed.prompt,
        ),
        None => (None, Vec::new(), None, None),
    };
```

把 Task 5 Step 1 里那段「`TuiSessionStore::create_new`」改成只在 `session_store.is_none()` 时执行：

```rust
    let session_store: Option<Arc<crate::session_store::TuiSessionStore>> =
        match (session_store, crate::session_store::TuiSessionStore::create_new(
            &workdir,
            &config.model,
            if config.yolo {
                yi_agent_app_server::thread_store::ThreadMode::Yolo
            } else {
                yi_agent_app_server::thread_store::ThreadMode::Normal
            },
        )) {
            (Some(store), _) => Some(store),
            (None, Ok(store)) => Some(Arc::new(store)),
            (None, Err(e)) => {
                eprintln!("[yi-agent] 本次会话不落盘: {e}");
                None
            }
        };
```

driver 闭包里种 session：

```rust
            let mut initial = yi_agent_core::Session::from_messages(initial_messages.clone());
            initial.set_last_input_tokens(initial_last_input);
            let session_handle = Arc::new(std::sync::Mutex::new(initial));
```

**这三行替换 Task 5 Step 2 里那行 `Session::new()`**，不是新增。替换后新会话路径（`initial_messages` 为空）等价于原来的空 session，行为零变化。

- [ ] **Step 9: 跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent --lib config::tests::resume
cargo test -p yi-agent --lib resume::
```

Expected: 2 passed + 若干 resume 测试。

- [ ] **Step 10: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/resume.rs yi-agent-rs/crates/yi-agent/src/main.rs && \
git commit -m "feat(cli): resume / fork / threads 子命令

clap 无法表达 'resume --last 的 positional 是提示词而非 id' 这种条件语义，
在 dispatch 里手工消歧，否则会报「找不到会话 修复这个 bug」。

id 支持唯一前缀匹配（类 git short hash），歧义时列候选并拒绝。"
```

---

## Task 7: `ThreadStore::fork_thread`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `load`、`create`、`append_turn`、`title_from`、`now_millis`、`acquire_thread_lock`
- Produces: `pub fn fork_thread(&self, from: &str, new_id: &str) -> io::Result<String>`

**为什么必须住在这个文件里：** fork 要用 `title_from` 做标题截断，而它是 `pub(crate)`。写在 CLI 侧只有两条烂路：提升为 `pub`（无谓扩大 API）或复制一份 30 字符截断（第二套规则，迟早漂移）。

- [ ] **Step 1: 写失败的测试**

```rust
#[test]
fn fork_copies_items_and_messages() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    let mut meta = sample_meta("thread-src", dir.path());
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
}

#[test]
fn fork_clears_fields_that_would_break_the_desktop() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    let mut meta = sample_meta("thread-src2", dir.path());
    meta.last_item_id = Some("stale-item".into());
    meta.last_read_at = 1_700_000_000_000;
    meta.last_completed_at = 1_700_000_000_000;
    meta.role = Some(crate::thread_store::LEAD_ROLE.to_string());
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
    assert_eq!(loaded.meta.role, None, "不能继承 lead，否则一个项目两个组长");
    assert_eq!(loaded.meta.content_epoch, 0, "新会话的内容世代从头算");
}

#[test]
fn fork_does_not_touch_the_source_thread() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    store.create(&sample_meta("thread-src3", dir.path())).unwrap();
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

    let log = dir.path().join(".yi-agent").join("threads").join("thread-src3.jsonl");
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
    store.create(&sample_meta("thread-src4", dir.path())).unwrap();
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
    assert_eq!(loaded.totals.input_tokens, 100, "累计用量必须跟着过去，否则面板归零");
    assert!(
        loaded.usage.is_some(),
        "usage 必须是 Some——rpc/thread.rs:662 的 `if let Some(u)` 会挡掉 None 的整个通知"
    );
}
```

`sample_meta` 辅助函数（若测试模块里还没有）：

```rust
fn sample_meta(id: &str, root: &Path) -> ThreadMeta {
    ThreadMeta {
        thread_id: id.to_string(),
        cwd: root.to_string_lossy().to_string(),
        model: "test-model".into(),
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
        forked_from: None,
        content_epoch: 0,
        last_item_id: None,
        anchor_sha: None,
        last_read_at: 0,
        last_completed_at: 0,
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_
```

Expected: 编译失败，`fork_thread` 与 `forked_from` 不存在。

- [ ] **Step 3: 确认 `forked_from` 已存在**

该字段在 Task 1b 已加好。这里只需确认：

```bash
cd yi-agent-rs && grep -n "pub forked_from" crates/yi-agent-app-server/src/thread_store.rs
```

Expected: 一行命中。若没有，回到 Task 1b。

- [ ] **Step 4: 实现 `fork_thread`**

```rust
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
        let _src_lock = acquire_thread_lock(&self.root, from)
            .map_err(|busy| io::Error::new(io::ErrorKind::WouldBlock, busy.to_string()))?;

        let loaded = self
            .load(from)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("thread {from}")))?;

        let now = now_millis();
        let title = loaded.meta.title.as_ref().map(|t| title_from(&format!("{t} (fork)")));

        let meta = ThreadMeta {
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
            // 历史提到的文件可能在这些根里，不继承则 fork 后工具访问不到
            extra_roots: loaded
                .meta
                .extra_roots
                .iter()
                .filter(|p| Path::new(p).is_dir())
                .cloned()
                .collect(),
            // 唯一组长不变量：一个项目只能有一个 lead
            role: None,
            forked_from: Some(from.to_string()),
            content_epoch: 0,
            // 锚点/已读/完成时刻都不能继承，见 spec §7.2
            last_item_id: None,
            anchor_sha: loaded.meta.anchor_sha.clone(),
            last_read_at: 0,
            last_completed_at: 0,
        };
        self.create(&meta)?;

        // 新会话从诞生起就归本进程
        let _new_lock = acquire_thread_lock(&self.root, new_id)
            .map_err(|busy| io::Error::new(io::ErrorKind::WouldBlock, busy.to_string()))?;

        // `usage` 与 `usage_total` 都必须给：`rpc/thread.rs:662` 的
        // `if let Some(u) = loaded.usage` 会挡掉 None 时的整个用量通知。
        let usage_total = loaded.totals;
        let total_as_turn = TurnUsage {
            model: loaded
                .usage
                .as_ref()
                .map(|u| u.model.clone())
                .unwrap_or_else(|| loaded.meta.model.clone()),
            input_tokens: saturate_u32(total_as_turn_input(&usage_total)),
            output_tokens: saturate_u32(usage_total.output_tokens),
            cache_creation_input_tokens: saturate_u32(usage_total.cache_creation_input_tokens),
            cache_read_input_tokens: saturate_u32(usage_total.cache_read_input_tokens),
        };
        self.append_turn(
            new_id,
            &TurnLine::Turn {
                items: loaded.items,
                usage: loaded.usage,
                usage_total: Some(total_as_turn),
                messages: loaded.messages,
            },
        )?;

        Ok(new_id.to_string())
    }
```

`total_as_turn_input` 不需要单独函数，直接用 `usage_total.input_tokens`：

```rust
            input_tokens: saturate_u32(usage_total.input_tokens),
```

`saturate_u32` 已在 `server.rs:4711` 存在。它是 `fn`（私有），需要改成 `pub(crate) fn saturate_u32` 才能从 `thread_store` 调；或者在本文件里放一个同名的私有副本——**不要副本**，改成 `pub(crate)`。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib fork_
```

Expected: 4 passed。

- [ ] **Step 6: 跑整个 crate，修 `ThreadMeta` 字面量的编译错误**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

Expected: 全 green。若有 `missing field forked_from` 错误，逐个补 `forked_from: None`。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs && \
git commit -m "feat(thread_store): fork_thread 物理复制会话

必须住在本模块：标题截断要用 pub(crate) 的 title_from。

三个时间戳字段（last_item_id / last_read_at / last_completed_at）不继承：
继承 last_item_id 会让桌面端锚点校验拒绝 fork 会话首轮。

usage 与 usage_total 都必须给。只给 usage_total 会被 rpc/thread.rs:662
的 if let Some(u) 挡掉，客户端仍显示 0。

fork 不碰源会话：load 已在内存里合并 partial，不需要 promote_partial。"
```

---

## Task 8: headless `run --resume` 与端到端测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`（`Command::Run` 加 `resume` 字段）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`run_headless` 接受 resume）
- Test: `yi-agent-rs/crates/yi-agent/tests/e2e_resume.rs`（新建）

**Interfaces:**
- Consumes: `TuiSessionStore::open`、`resolve_thread_id`
- Produces: `Command::Run { .., resume: Option<String> }`

- [ ] **Step 1: 写失败的端到端测试**

`tests/e2e_resume.rs`：

```rust
//! 端到端：落盘 → resume → 再 resume。
//!
//! TUI 是全屏应用、无法自动化，`run --resume` 是这条链路唯一的可测载体。

use std::process::Command;

fn temp_workdir() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
}

fn run_in(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_yi-agent"))
        .args(args)
        .current_dir(dir)
        .env("YI_AGENT_PROVIDER", "mock")
        .env("MODEL_API_URL", "http://127.0.0.1:1")
        .output()
        .expect("run yi-agent")
}

#[test]
#[ignore] // 需要 mock provider，由 just test-resume-e2e 单独跑
fn resume_restores_history_and_appends() {
    let dir = temp_workdir();
    let first = run_in(dir.path(), &["run", "你好"]);
    assert!(
        first.status.success(),
        "首轮失败: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    // 找到刚建的 thread
    let threads_dir = dir.path().join(".yi-agent").join("threads");
    let meta = std::fs::read_dir(&threads_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .find(|e| e.file_name().to_string_lossy().ends_with(".meta.json"))
        .expect("应有 meta 文件");
    let id = meta
        .file_name()
        .to_string_lossy()
        .trim_end_matches(".meta.json")
        .to_string();

    let second = run_in(dir.path(), &["run", "--resume", &id, "继续"]);
    assert!(
        second.status.success(),
        "resume 失败: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    // 第二轮必须也落盘
    let log = std::fs::read_to_string(threads_dir.join(format!("{id}.jsonl"))).unwrap();
    let turns = log.lines().filter(|l| l.contains("\"type\":\"turn\"")).count();
    assert!(turns >= 2, "两轮都该落盘，实际 {turns} 行 turn");

    // 锁文件必须存在（首次 resume 时创建）
    assert!(threads_dir.join(format!("{id}.lock")).exists());
}

#[test]
#[ignore]
fn resume_of_missing_thread_fails_loudly() {
    let dir = temp_workdir();
    let out = run_in(dir.path(), &["run", "--resume", "thread-doesnotexist", "x"]);
    assert!(!out.status.success(), "不存在的会话必须报错退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("找不到会话") || stderr.contains("thread-doesnotexist"),
        "错误信息要能指导用户，实际: {stderr}"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test e2e_resume -- --ignored
```

Expected: 编译失败或 `--resume` 是未知参数。

- [ ] **Step 3: 给 `Command::Run` 加字段**

```rust
        /// 恢复一个既有会话再跑。跑完的内容会追加进同一份日志。
        #[arg(long)]
        resume: Option<String>,
```

dispatch 里 `Some(Command::Run { .. })` 的解构要补 `resume`，并传给 `run_headless`。

- [ ] **Step 4: `run_headless` 接 resume**

在 `run_headless` 里、构造 `agent` 之前：

```rust
    // resume：先载入历史，再让 agent 从它继续。落盘走同一份 ThreadStore。
    let resume_session = match resume.as_deref() {
        Some(spec) => {
            let store = yi_agent_app_server::thread_store::ThreadStore::new(&config.workdir);
            let id = resume::resolve_thread_id(&store, spec)?;
            Some(crate::session_store::TuiSessionStore::open(&config.workdir, &id)
                .map_err(|e| anyhow::anyhow!("{e}"))?)
        }
        None => None,
    };
```

把 session 种给 agent：

```rust
    let mut session = yi_agent_core::Session::new();
    if let Some(store) = &resume_session {
        let loaded = /* 需要从 store 取 messages —— 见下 */;
    }
```

**这里需要一个 getter**：`TuiSessionStore` 加

```rust
    /// 载入的 messages 与用量，供 resume 种入 session。
    pub fn load_seed(&self) -> std::io::Result<(Vec<Message>, Option<u32>)> {
        let loaded = self
            .store
            .load(&self.thread_id)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "thread"))?;
        let seeded = yi_agent_core::Session::from_messages(loaded.messages);
        // 与 fork 子 agent 共用同一条截断规则
        let safe = yi_agent_core::forkable_messages(&seeded);
        Ok((safe, loaded.usage.map(|u| u.input_tokens)))
    }
```

然后：

```rust
    if let Some(store) = &resume_session {
        let (messages, last_input) = store.load_seed()?;
        session = yi_agent_core::Session::from_messages(messages);
        session.set_last_input_tokens(last_input);
    }
    let agent = agent.with_session_arc(Arc::new(std::sync::Mutex::new(session)));
```

**`pending_turn` 必须先升格。** 给 `TuiSessionStore` 加：

```rust
    /// 把崩溃残留的 checkpoint 升格进主日志。
    ///
    /// resume 必须做：采纳的 partial 是那一轮唯一的持久副本，不 append 就返回，
    /// 用户下一条消息的 turn-start checkpoint 会原子覆盖同一文件，该轮 items
    /// 永久丢失，而 session 上下文仍「记得」它们，造成 item 与上下文不一致。
    ///
    /// 返回 `true` 表示确实升格了（调用方据此决定是否补一条「上一轮已中断」提示）。
    pub fn promote_pending(&self) -> std::io::Result<bool> {
        self.store.promote_partial(&self.thread_id)
    }
```

调用顺序固定为「先 `promote_pending`，再 `load_seed`」——反过来会读到未合流的旧状态：

```rust
    let mut session = yi_agent_core::Session::new();
    if let Some(store) = &resume_session {
        match store.promote_pending() {
            Ok(_) => {}
            // 失败只记日志，不阻断：partial 不是唯一数据源，主日志仍在。
            Err(e) => eprintln!("[yi-agent] 未能升格未收尾的轮次: {e}"),
        }
        let (messages, last_input) = store.load_seed()?;
        session = yi_agent_core::Session::from_messages(messages);
        session.set_last_input_tokens(last_input);
    }
    let agent = agent.with_session_arc(Arc::new(std::sync::Mutex::new(session)));
```

`load_seed` 内部已经调了 `load`，而 `load` 会自己合流 partial（`thread_store.rs` 的 `read_partial` 分支）。所以即使 `promote_pending` 失败，消息不会丢——升格的意义是让**磁盘**也收尾，避免下一轮 checkpoint 覆盖。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test e2e_resume -- --ignored
```

Expected: 视 mock provider 配置而定。若项目没有 `YI_AGENT_PROVIDER=mock` 这个开关，改用 wiremock（参考 `crates/yi-agent/tests/common/mod.rs`）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/config.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/session_store.rs yi-agent-rs/crates/yi-agent/tests/e2e_resume.rs && \
git commit -m "feat(cli): run --resume 与端到端测试

TUI 是全屏应用、无法自动化，run --resume 是落盘→resume→再 resume
这条链路唯一可测的载体。

resume 载入走 load_seed：内部调 forkable_messages，与 fork 子 agent
共用同一条截断规则，并恢复 last_input_tokens 让 auto-compact 首轮生效。"
```

---

## Task 9: 回放接入 TUI 启动

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`run_tui_agent` 初始化 history 的位置）

**Interfaces:**
- Consumes: `items_to_cells`、`TuiSessionStore::open`
- Produces: 无（终端行为）

- [ ] **Step 1: 在 TUI 初始化 history 处注入回放 cells**

找到 `HistoryState::new()` / `with_theme(...)` 的构造点，在它之后：

```rust
        // resume 时把历史摊平成 cells。不注入的话上下文是对的但屏幕是空的，
        // 用户会以为历史丢了。
        if let Some(store) = resume_store.as_ref() {
            if let Ok(loaded) = store.load_for_replay() {
                for cell in crate::tui::replay::items_to_cells(&loaded.items) {
                    history.push(cell, width);
                }
                if loaded.pending_turn {
                    history.push(
                        crate::tui::cell::HistoryCell::Separator {
                            label: Some("上一轮已中断".into()),
                        },
                        width,
                    );
                }
            }
        }
```

需要 `TuiSessionStore::load_for_replay()`：

```rust
    /// 回放用：返回 items 与是否有崩溃残留的 turn。
    pub fn load_for_replay(
        &self,
    ) -> std::io::Result<yi_agent_app_server::thread_store::LoadedThread> {
        Ok(self
            .store
            .load(&self.thread_id)?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "thread"))?)
    }
```

- [ ] **Step 2: 确认 `enforce_history_cap` 会自动裁剪**

```bash
cd yi-agent-rs && grep -n "enforce_history_cap" crates/yi-agent/src/tui/history.rs
```

Expected: 在 `push` 内部被调用。若不是，在批量 push 循环结束后手动调一次——**不要**新写裁剪逻辑。

- [ ] **Step 3: 手动冒烟**

```bash
cd /tmp/resume-smoke && cargo run -p yi-agent -- resume --last
```

Expected: 屏幕上出现上次对话的内容，不是空白。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && \
git add yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent/src/session_store.rs && \
git commit -m "feat(tui): resume 时回放历史到屏幕

items_to_cells 把落盘 Item 摊平成 HistoryCell。裁剪交给既有
enforce_history_cap（MAX_HISTORY_CELLS=2000），不新写裁剪逻辑。

崩溃残留的 turn 补一条「上一轮已中断」分隔线。"
```

---

## Task 10: 文档与项目进度

**Files:**
- Modify: `docs/project-management/` 下对应模块文件
- Modify: `docs/project-management/README.md`（模块索引计数）

- [ ] **Step 1: 找到应更新的模块文件**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && \
ls docs/project-management/ && \
grep -rln "CLI\|TUI" docs/project-management/*.md | head
```

- [ ] **Step 2: 按既有格式加条目**

三态规则：只用 `[x]` / `[ ]` / `[-]`，**禁止 `[~]`**。每条带可验证判据（`file.rs:line` 或可执行命令）。

新增条目示例：

```markdown
- [x] CLI 会话 resume（`yi-agent resume <id>` / `--last`）
  判据：`cargo test -p yi-agent --test e2e_resume -- --ignored`
- [x] 会话 fork（`yi-agent fork <id>`）
  判据：`cargo test -p yi-agent-app-server --lib fork_`（4 条）
- [x] 跨进程会话写锁
  判据：`cargo test -p yi-agent-app-server --lib thread_lock`（3 条）
```

- [ ] **Step 3: 同步 README 计数**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && \
grep -n "完成 / 总计\|总计" docs/project-management/README.md | head
```

按实际条目数改「完成 / 总计」。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/ && \
git commit -m "docs: 记录 CLI resume/fork 完成状态与验据"
```

---

## 最终验收

全部任务完成后跑：

```bash
cd yi-agent-rs
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"   # 必须为空
cargo fmt --all --check
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent --lib
```

Expected: `fmt --check` 无输出；两个 crate 全 green。

再手验四条用户可见路径：

```bash
cd /tmp && rm -rf rs && mkdir rs && cd rs
cargo run -p yi-agent                    # 聊一句，退出
cargo run -p yi-agent -- threads         # 应看到刚才那条
cargo run -p yi-agent -- resume --last   # 历史应在屏幕上
cargo run -p yi-agent -- fork --help     # 参数齐全
```

**最容易写错的三处，实现时重点自查：**

1. **锁只防两个 writer**。若在 app-server 里无条件长期持锁，桌面开着会话时终端永远 resume 不了（Task 2 Step 5 的反例测试会红）。
2. **`fork` 不写源会话**。调了 `promote_partial(源)` 就会让 fork 变成隐式写操作（Task 7 的 `fork_does_not_touch_the_source_thread` 会红）。
3. **`/clear` 不新增日志变体**。复用 `truncate`；自造 `Clear` 边界记录会与既有语义重复并引入向后兼容负担。

---

## 执行记录：与计划的偏差（2026-11-01 实施时发现）

计划在执行中被验证出若干错误，逐条记录，避免后来者照着重犯。

### 1. 锁永不释放（严重，已修）
原设计让进程级去重表存 `Arc<ThreadWriterLock>`，而锁本身又持有表项的键。两者互相
持有构成自引用循环，`Drop` 永不触发 → `flock` 永不释放 + fd 泄漏。**改为存 `Weak`**，
并在查表时用 `Weak::upgrade` 顺带清掉失效项。

### 2. `serde(default)` 对 `Option<T>` 不是必需的
计划称「`forked_from` 缺 `default` 会让反序列化失败、`cwd` 被冲成空串」。实测：serde
对 `Option<T>` 缺字段自动填 `None`，去掉 `default` 仍通过。测试保留（它钉的是行为），
注释改成准确说法。

### 3. 子进程测试名必须带完整模块路径
`--exact tests::child_xxx` 匹配不到，必须写 `session_store::tests::child_holds_lock`
（或对应模块的完整路径）。

### 4. 同进程内 flock 无法模拟「另一个 writer」
flock 按打开文件描述加锁，同进程再 open 一次会先命中进程级去重表，根本走不到内核。
必须起真子进程。

### 5. `thread/start` 也必须加锁（计划只提了 `thread/resume`）
`ThreadSession` 有两处生产构造点（`server.rs` 的 `thread/start` 分支、
`rpc/thread.rs` 的 `thread/resume` 分支），只加一处等于留后门。

### 6. IO 失败不能报成 Busy
`thread/start` 里 `create_dir_all` 失败即报错会打断既有测试
`agent_message_without_a_runtime_is_an_error`（它故意用文件占住 `.yi-agent`）。改为
`ThreadLockError::{Busy, Io}`：`Busy` 拒绝，`Io` 退化为无锁并记 warn。

### 7. `ThreadStore::root()` 已存在
计划里的 `root_for_test()` 不需要新增。

### 8. `run_agent` 拆成两层后，两个源码结构断言测试要改
`run_agent_offers_onboarding_before_loading_config` 与
`the_env_migration_runs_before_config_loading` 都按 `fn run_agent(` 切片。`run_agent`
变成转调 `run_agent_with_seed` 的薄包装后，切片落在空壳上，断言空转失败。改为切
`run_agent_with_seed`（以及抽取后的 `prepare_environment`）。

### 9. headless `run` 此前完全不落盘（计划假设它已落盘）
Task 8 的 e2e 断言「两轮都落盘」，但 `run_headless` 原来没有任何存储代码，`resume`
也就无从谈起。补上落盘时顺手把「事件 → items → 按 session 净增长裁剪」抽成
`TurnLog`（`crates/yi-agent/src/turn_log.rs`），TUI driver 与 headless 共用同一份。
不抽的话这条链要复制三处易错逻辑（用户消息补齐、取消轮不发 Done、按净增长裁剪）。

### 10. 所有需要 config 的入口都必须先跑迁移
冒烟发现 `resume` 拿到的 `workdir` 可能来自旧读取路径：老格式 `.env` 里的设置在迁移
前读 `.env`、迁移后读 `settings.json`，两边不同就会去另一个目录找会话。抽出
`prepare_environment`（只做非交互的迁移，**不弹引导**——`threads` / headless 可能跑在
管道里，弹引导会挂死在 stdin），`resume` / `fork` / `threads` / headless / 新会话都先调它。
headless 此前完全没做迁移，是顺带补上的既有缺口。

### 11. e2e 不需要 wiremock
`yi-agent` 的 dev-dependencies 里没有 wiremock，而这条链路只需「回一句固定的话」。
用标准库 `TcpListener` 手写一个只会返回固定 Anthropic SSE 的服务器，几十行，不加依赖。

### 12. 新增的 `TurnLog` / 回放相关 clippy 处理
- `items_to_cells` 在 Task 9 前无人调用 → `#[allow(dead_code)]`（Task 9 接上后即可移除，
  当前保留以标注接入点）。
- `ThreadSession.writer_lock` 是纯 RAII 字段，永远没有读取方 → `#[allow(dead_code)]`
  并注明「值一旦 drop，flock 就释放。没有读取方是设计如此」。
- 循环外的三个轮次累加器（`last_usage` / `usage_total` / `base_len`）初值从未被读 →
  搬进循环体。
