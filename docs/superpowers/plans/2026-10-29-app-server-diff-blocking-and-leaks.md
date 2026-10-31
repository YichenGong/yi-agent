# app-server 的 diff 阻塞与内存泄漏修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修掉 `yi-agent-app-server` 里四条实测缺陷：`thread/diff/read` 阻塞主循环、`view_frames` 泄漏、`image/read` 重复全量读、`git_ok` 无超时。

**Architecture:** 三处「计算挪出 async 主循环」（套用 `thread/listAll` 已验证的 `tokio::spawn` + `spawn_blocking` 两层模式）+ 三处「给无界 IO 加上界」（未跟踪文件读前大小闸、`git_ok` 有界等待、`image/read` 区间读）+ 一处「补上遗漏的清理」。

**Tech Stack:** Rust 2021、tokio（`spawn` / `spawn_blocking` / `process` / `time`）、`tempfile`、`image` 0.25、serde_json。

## Global Constraints

- **只在 worktree 里改，禁止在 `main` 上提交。** worktree：`.worktrees/<branch>`，分支名 `fix/app-server-diff-blocking-and-leaks`。
- **每条修复先写失败测试并确认它真的红**（行为不符，不是编译错误）。没看到红的测试不算 TDD。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下执行），保证 `just fmt-check` 通过。
- **不要并行跑 `cargo test`。** 跑之前先 `ps aux | grep cargo` 确认没有残留进程。按 crate 跑：`cargo test -p yi-agent-app-server --lib`。
- **`cargo` 不在默认 PATH**，每次 bash 调用前需：
  `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"`
- Commit 用 conventional commits，首行 ≤72 字符，中文正文，**不写** `Co-Authored-By`。
- 线上契约字段名不得改：`ViewFrame` 的 `asKind` / `exitCode` / `timedOut` / `baseKind`，`image/read` 的 `data` / `nextOffset` / `mediaType` / `size`。
- 现有测试必须全绿：基线 1054 passed / 0 failed / 4 ignored。

## 文件结构

| 文件 | 职责 | 本计划改动 |
|---|---|---|
| `src/git_diff.rs` | 在 thread cwd 上现算 git diff（只读） | `git_ok` 加超时；未跟踪文件加上限读；抽 `read_untracked_capped` |
| `src/rpc/thread.rs` | `thread/*` 域 RPC | `thread/diff/read` 与 `thread/view/read` refresh 移出主循环；`thread/delete` 补清 `view_frames` |
| `src/rpc/image.rs` | `image/*` 域 RPC | `image/read` 改区间读 + 头部魔数校验 |
| `src/rpc_tests.rs` | RPC 层测试（Harness） | 新增 T1 / T3 / T5 三个集成测试 |
| `src/git_diff.rs` 的 `mod tests` | git_diff 单测 | 新增 T2 / T4 两个单测 |
| `Cargo.toml` | 依赖 | `tempfile` 从 dev-dependency 提为 dependency |

---

### Task 1: `thread/delete` 清理 `view_frames`（P2，最小、最独立，先做）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:1713-1720`（`thread/delete` 清理序列）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（新增用例）

**Interfaces:**
- Consumes：`state.view_frames: &'a ViewFrames`（`rpc/mod.rs:440`），
  `ViewFrames = Arc<tokio::sync::Mutex<HashMap<String, ViewFrame>>>`（`server.rs:390`）
- Produces：无新 API

- [ ] **Step 1: 写失败测试**

追加到 `src/rpc_tests.rs` 末尾：

```rust
/// `thread/delete` 必须把该会话的视图帧一并收走。
///
/// 帧表以 `thread_id` 为键、进程生命周期内常驻，且全仓库只有插没有删。
/// `thread/view/read` 有 `require_known_thread` 前挡，故残留不影响正确性,
/// 但每删一个看过 diff 的会话就永久留一份(单条上限 256 KiB)。
#[tokio::test(flavor = "multi_thread")]
async fn thread_delete_drops_the_view_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    {
        // 直接注入一帧，等价于 `pump_git_diff_notifications` 写帧。
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: Some("git log --oneline".to_string()),
                as_kind: "text".to_string(),
                exit_code: Some(0),
                stdout: "x".repeat(1024),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: None,
                base_kind: String::new(),
            },
        );
    }
    assert!(
        h.view_frames.lock().await.contains_key(&tid),
        "precondition: the frame must be present before delete"
    );

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":30,"method":"thread/delete","params":{{"threadId":"{tid}","force":true}}}}"#
    ))
    .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(30)))
        .await;

    assert!(
        !h.view_frames.lock().await.contains_key(&tid),
        "thread/delete must drop the view frame for {tid}"
    );
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认它红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_delete_drops_the_view_frame -- --nocapture
```

预期：`FAILED`，panicked at `thread/delete must drop the view frame for thread-…`。
若它是 PASS，说明测试没测到点上，回去改测试。

- [ ] **Step 3: 最小实现**

在 `src/rpc/thread.rs` 的 `thread/delete` 臂内，紧跟 `process_watches` 清理之后
（当前约 1717-1719 行）插入：

```rust
            // 视图帧与 thread 同生命周期:帧表按 thread_id 索引,不清理就永久驻留。
            // `thread/view/read` 有 `require_known_thread` 前置检查,故此处不影响
            // 正确性,只是不让已删会话的帧继续占内存。
            state.view_frames.lock().await.remove(&thread_id);
```

- [ ] **Step 4: 跑测试确认它绿，且全量仍绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_delete_drops_the_view_frame
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：单测 PASS；全量 `1055 passed; 0 failed; 4 ignored`（基线 1054 + 1）。

- [ ] **Step 5: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix: drop view frames when a thread is deleted

thread/delete 清理了 threads / pending_activation / thread_roots /
trace_watches / children_watches / process_watches,唯独漏了 view_frames。
帧表以 thread_id 为键、只有 insert 没有 remove,于是每删一个看过 diff 的
会话就永久留下一条(单条上限 256 KiB)。

thread/clear 与 thread/compact 不清:它们只清对话历史,会话仍在,
thread/view/read 仍应能读回该会话的帧。"
```

---

### Task 2: `image/read` 改区间读取（P3）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/image.rs:60-215`（`image/read` 臂）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（新增用例）

**Interfaces:**
- Consumes：`std::fs::File` + `Seek` + `Read`
- Produces：模块内私有函数
  ```rust
  const IMAGE_HEADER_BYTES: usize = 16;
  fn read_range(path: &Path, offset: u64, max_bytes: u64) -> std::io::Result<Vec<u8>>;
  fn read_header(path: &Path) -> std::io::Result<Vec<u8>>;
  ```

- [ ] **Step 1: 写失败测试**

追加到 `src/rpc_tests.rs` 末尾：

```rust
/// `image/read` 只应触碰被请求的那一段，而不是把整个文件读进内存再切片。
///
/// 判据用**相对比较**：同一个 20 MiB 文件，取最后一页 1 字节的耗时不应显著
/// 高于取首页 1 字节。全量读实现下两者都是「读满 20 MiB」，比值接近 1 但
/// **绝对开销与文件大小成正比**;区间读实现下两者都只碰 16 字节头 + 1 字节。
/// 故这里改用更锐利的判据：页内字节数相同,比较 20 MiB 与 1 KiB 两个文件。
#[tokio::test(flavor = "multi_thread")]
async fn image_read_does_not_scale_with_file_size() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 两张「png」：魔数正确即可通过 guess_format，其余填充随机性无关。
    let png_magic: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let big_path = dir.path().join("big.png");
    let mut big = png_magic.to_vec();
    big.resize(20 * 1024 * 1024, 0x41);
    std::fs::write(&big_path, &big).unwrap();

    let small_path = dir.path().join("small.png");
    let mut small = png_magic.to_vec();
    small.resize(1024, 0x41);
    std::fs::write(&small_path, &small).unwrap();

    // 取各自**最后一页**的第一字节：两者读取量都是 1 字节。
    let big_offset = big.len() as u64 - 1;
    let small_offset = small.len() as u64 - 1;

    // 自由函数而不是闭包：闭包捕获 &mut h 后无法在循环里连续 await 两次。
    async fn time_one_read(
        h: &mut Harness,
        tid: &str,
        path: &str,
        offset: u64,
        id: u64,
    ) -> std::time::Duration {
        let started = std::time::Instant::now();
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"image/read","params":{{"threadId":"{tid}","path":"{path}","offset":{offset},"maxBytes":1}}}}"#
        ))
        .await;
        let v = h
            .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
            .await;
        assert_eq!(v["result"]["nextOffset"], serde_json::Value::Null, "{v}");
        started.elapsed()
    }

    // 各跑三次取最小值，压掉文件系统缓存冷启动的抖动。串行 await，
    // 不能用 join!/select!——那会交错两次读取，测不出单次开销。
    let mut big_best = std::time::Duration::MAX;
    let mut small_best = std::time::Duration::MAX;
    let mut id = 80u64;
    for _ in 0..3 {
        big_best = big_best
            .min(time_one_read(&mut h, &tid, "big.png", big_offset, id).await);
        id += 1;
        small_best = small_best
            .min(time_one_read(&mut h, &tid, "small.png", small_offset, id).await);
        id += 1;
    }
    assert!(
        big_best < small_best * 4,
        "one 1-byte read must not scale with file size: \
         20MiB={big_best:?} 1KiB={small_best:?}"
    );
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认它红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_read_does_not_scale_with_file_size -- --nocapture
```

预期：`FAILED`，消息形如
`one 1-byte read must not scale with file size: 20MiB=2.6ms 1KiB=220µs`
（实测比值约 12 倍）。

- [ ] **Step 3: 实现区间读**

在 `src/rpc/image.rs` 顶部常量区加入：

```rust
/// 判定图片格式所需的头部字节数。
///
/// `image::guess_format` 只对 `MAGIC_BYTES` 逐条 `starts_with`
/// (`image-0.25.10/src/io/free_functions.rs:145-158`)。本 crate 启用的
/// png/jpeg/gif/webp 四个 feature 的签名分别是 8/3/6/12 字节,16 覆盖最长者。
const IMAGE_HEADER_BYTES: usize = 16;

/// 只读文件的 `[offset, offset + max_bytes)` 区间。
///
/// 与「全量读再切片」的区别是本 RPC 的核心修复:`image/read` 由客户端逐页
/// 拉取(`desktop/src/lib/useImageData.ts:137`),原实现每页都把整个文件读进
/// 内存,读一张 20 MiB 的图等于读 40 次 20 MiB。
fn read_range(path: &std::path::Path, offset: u64, max_bytes: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; max_bytes as usize];
    let mut filled = 0usize;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 读文件头部 `IMAGE_HEADER_BYTES` 字节,供 `guess_format` 判格式。
fn read_header(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    read_range(path, 0, IMAGE_HEADER_BYTES as u64)
}
```

然后把 `image/read` 臂内的读取逻辑改成：

```rust
            // 头部足够判格式(见 `IMAGE_HEADER_BYTES` 的说明),不必读全文。
            let header = match read_header(&canonical) {
                Ok(h) => h,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("cannot read image: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 只送可解码的图片:以魔数判定,不看扩展名。
            let format = match image::guess_format(&header) {
                Ok(f) => f,
                Err(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("not a supported image")),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            let offset = req
                .params
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let max_bytes = req
                .params
                .get("maxBytes")
                .and_then(|v| v.as_u64())
                .filter(|n| *n > 0)
                .unwrap_or(IMAGE_READ_CHUNK_BYTES)
                .min(IMAGE_READ_CHUNK_BYTES);
            // 越界 offset 落在文件尾之后:返回空块且 `nextOffset: null`,
            // 与「全量读后 `start.min(len)`」的旧行为一致(客户端据此结束循环)。
            let start = offset.min(declared_len);
            let want = max_bytes.min(declared_len.saturating_sub(start));
            let bytes = match read_range(&canonical, start, want) {
                Ok(b) => b,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("cannot read image: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            let end = start + bytes.len() as u64;
            let next = if end < declared_len {
                Some(end)
            } else {
                None
            };
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "data": data,
                        "nextOffset": next,
                        "mediaType": image_media_type(format),
                        "size": declared_len,
                    }),
                ),
            )
            .await?;
```

删除原来的 `let bytes = std::fs::read(&canonical) { ... }` 整块与后续的
`bytes.len() > IMAGE_READ_MAX_BYTES` 复核（`declared_len` 已在 metadata 阶段挡过；
区间读不会读超，复核失去意义）。`start` / `end` / `next` 的计算全部基于 `declared_len`。

- [ ] **Step 4: 跑测试确认它绿，且既有 image 测试仍绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image_read
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib image
```

预期：新用例 PASS；既有 `image_*` 用例（含分片边界 `a_chunk_must_stay_within_...`）全绿。

- [ ] **Step 5: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/image.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix: read only the requested range in image/read

原实现每次分片请求都把整个文件 std::fs::read 进内存再切片,而客户端逐页
拉取,读一张 20 MiB 的图等于读 40 次 20 MiB。实测取 1 字节的耗时是 1 KiB
文件的 12 倍,且全发生在 async 主循环上。

改为 seek + 读取请求区间;格式判定只需头部 16 字节(image crate 的
guess_format 只做魔数 starts_with 比较,最长签名是 webp 的 12 字节)。
线上契约 data/nextOffset/mediaType/size 语义不变。"
```

---

### Task 3: `git_ok` 加有界等待（P4）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs:160-176`（`git_ok`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`（`tempfile` 提为 dependency）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs` 的 `mod tests`

**Interfaces:**
- Consumes：`tempfile::NamedTempFile`
- Produces：
  ```rust
  pub const GIT_OK_TIMEOUT: Duration = Duration::from_secs(5);
  const GIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
  fn git_ok(directory: &Path, args: &[&str]) -> Option<String>;  // 签名不变
  ```

- [ ] **Step 1: 改 Cargo.toml 提依赖**

`src/git_diff.rs` 的 `git_ok` 要用 `tempfile`。它在 `[dev-dependencies]` 里
（`Cargo.toml:59`），需挪到 `[dependencies]`：

```toml
# `git_ok` 把 git 的 stdout 写进临时文件而不是管道:管道会在输出超过内核
# 缓冲区(64 KiB)时死锁——我们在轮询 try_wait、没人读管道,`git diff` 的输出
# 又常态超过 64 KiB。
tempfile = "3"
```

并从 `[dev-dependencies]` 段删掉重复的 `tempfile = "3"`。

- [ ] **Step 2: 写失败测试**

追加到 `src/git_diff.rs` 的 `mod tests` 内（`#[cfg(test)]` 块里）：

```rust
    /// `run_git_bounded` 必须在上界内放弃,并回收子进程。
    ///
    /// 用 `/bin/sh -c 'sleep 600'` 冒充挂住的 git:它确实是「一个不会自己退出
    /// 的子进程」,这正是残留 `index.lock` 或网络挂载点上的 git 的表现。
    /// 不走 PATH 注入,故不会污染并行测试。
    #[test]
    fn run_git_bounded_gives_up_after_the_timeout() {
        let started = std::time::Instant::now();
        let out = run_git_bounded(
            "/bin/sh",
            &["-c".to_string(), "sleep 600".to_string()],
            std::path::Path::new("/tmp"),
            Duration::from_millis(200),
        );
        let elapsed = started.elapsed();
        assert!(out.is_none(), "a hung child must yield None");
        assert!(
            elapsed < Duration::from_secs(5),
            "must give up near the bound, took {elapsed:?}"
        );
    }

    /// 正常返回的子进程要照常给出 stdout,且不受上界影响。
    #[test]
    fn run_git_bounded_returns_stdout_on_success() {
        let out = run_git_bounded(
            "/bin/sh",
            &["-c".to_string(), "printf 'hi\\n'".to_string()],
            std::path::Path::new("/tmp"),
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("hi\n"));
    }
```

- [ ] **Step 3: 跑测试确认它红**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib run_git_bounded
```

预期：**编译错误** `cannot find function run_git_bounded`。
这一步的红是编译错误——因为函数还不存在，这是 TDD 里可接受的「红」，
但**Step 4 之后必须重跑**，确认它变成「行为通过」。

- [ ] **Step 4: 实现**

在 `src/git_diff.rs` 中，`git_ok` 上方加入：

```rust
/// `git_ok` 的单次时间上限。
///
/// 与 `VIEW_TIMEOUT` 分开:那个约束「模型给的展示命令」(走沙箱、允许较慢),
/// 这个约束「我们自己发的只读内省命令」(`rev-parse` / `diff` / `log`,
/// 正常都是毫秒级)。给 5s 是因为大仓库首次 `git diff` 可能真慢;但
/// 「`index.lock` 残留」或「仓库在网络挂载点上」应当失败而不是无限等待。
pub const GIT_OK_TIMEOUT: Duration = Duration::from_secs(5);

/// `git_ok` 轮询子进程状态的间隔。git 正常返回是毫秒级,10 ms 只多等一轮。
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 跑一个子进程,在 `timeout` 内返回它的 stdout(成功时),否则 `None`。
///
/// stdout 走**临时文件**而不是管道:管道模式下子进程写满内核缓冲区
/// (macOS 64 KiB)就会阻塞在 write 上,而我们正在轮询 `try_wait`、没有任何人
/// 读管道——这是死锁(子进程等我们读,我们等子进程退出)。`git diff` 的输出
/// 远超 64 KiB 是常态,故管道方案对本项目不可用。
fn run_git_bounded(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Option<String> {
    let out_file = tempfile::NamedTempFile::new().ok()?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::from(out_file.reopen().ok()?))
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                return std::fs::read_to_string(out_file.path()).ok();
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    // 必须 wait:否则留下僵尸进程。
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(GIT_POLL_INTERVAL);
            }
            Err(_) => return None,
        }
    }
}
```

然后把 `git_ok` 改为委托：

```rust
/// 跑 git 并返回成功时的 stdout 文本。
///
/// 只认 success：`rev-parse --verify` / `merge-base` / `log` 这类命令失败时
/// 本就该当作「没有」，而 diff 类命令的退出码差异由 HTML 生成后的内容决定，
/// 故所有调用点都只需「有内容」或「没有」两态。
///
/// 有界等待见 [`GIT_OK_TIMEOUT`]：无上界时,一个挂住的 git 会把调用方
/// 永久占住(修复前是主循环)。
fn git_ok(directory: &Path, args: &[&str]) -> Option<String> {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    run_git_bounded("git", &owned, directory, GIT_OK_TIMEOUT)
}
```

- [ ] **Step 5: 跑测试确认全绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib run_git_bounded
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib git_diff
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：两个新用例 PASS；既有 `git_diff` 用例全绿（它们全走真 `git`，超时不影响）；
全量 `1057 passed`（1055 + 2）。

- [ ] **Step 6: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs \
              yi-agent-rs/crates/yi-agent-app-server/Cargo.toml \
              yi-agent-rs/Cargo.lock
git commit -m "fix: bound git_ok with a timeout

git_ok 用 Command::output() 阻塞等待,没有超时,却被 thread_diff 在同步
路径上连续调用五次以上。index.lock 残留或仓库位于网络挂载点时,git 会
挂起且没有任何上界能打断它。

改为 try_wait 轮询 + 超时 kill + wait 回收。stdout 走临时文件而非管道:
管道模式下子进程写满 64 KiB 内核缓冲区就阻塞在 write,而我们正在轮询
try_wait、没人读管道,是死锁。tempfile 从 dev-dependency 提为 dependency。"
```

---

### Task 4: 未跟踪文件加上限，合并重复读取（P1 的一半）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs`（`files` / `is_binary` / `untracked_diff`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs` 的 `mod tests`

**Interfaces:**
- Consumes：`std::fs::metadata`
- Produces：
  ```rust
  pub const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;
  /// `None` = 不参与内容合成（过大或读失败）。
  fn read_untracked_capped(full: &Path) -> Option<Vec<u8>>;
  ```

- [ ] **Step 1: 写失败测试**

追加到 `mod tests`：

```rust
    /// 超过上限的未跟踪文件不读内容:清单标 binary,正文给一行说明。
    ///
    /// 原实现对每个未跟踪文件做三次全量 `std::fs::read`(数换行 / 判二进制 /
    /// 合成 unified diff),没有大小上限。截断只在最后做,故 `MAX_DIFF_BYTES`
    /// 只约束输出、不约束输入——一个 512 MiB 的未跟踪文件会被完整读进内存。
    #[test]
    fn untracked_file_over_the_cap_is_not_read_into_the_diff() {
        let dir = repo();
        let big = dir.path().join("huge.log");
        // 2 MiB,超过 MAX_UNTRACKED_FILE_BYTES(1 MiB)。
        let chunk = "x".repeat(1024 * 1024);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            f.write_all(chunk.as_bytes()).unwrap();
            f.write_all(chunk.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        let d = thread_diff(dir.path(), Some("trunk"), None);
        let stat = d
            .files
            .iter()
            .find(|f| f.path == "huge.log")
            .expect("the oversized untracked file must still be listed");
        assert!(
            stat.binary,
            "an oversized untracked file must be reported as binary, not read as text"
        );
        assert!(
            d.unified_diff.contains("too large to render"),
            "the body must say why it is not rendered: {}",
            &d.unified_diff[..d.unified_diff.len().min(400)]
        );
        // 正文里绝不能出现那 2 MiB 的内容。
        assert!(
            !d.unified_diff.contains(&"x".repeat(4096)),
            "the file content must not be read"
        );
    }

    /// 正常大小的未跟踪文件仍按文本合成 diff（不能被上限误伤）。
    #[test]
    fn untracked_file_under_the_cap_is_still_rendered_as_text() {
        let dir = repo();
        std::fs::write(dir.path().join("note.txt"), "hello\nworld\n").unwrap();
        let d = thread_diff(dir.path(), Some("trunk"), None);
        let stat = d.files.iter().find(|f| f.path == "note.txt").unwrap();
        assert!(!stat.binary, "a small text file must not be marked binary");
        assert!(
            d.unified_diff.contains("+hello"),
            "small untracked text must still be rendered: {}",
            d.unified_diff
        );
    }
```

- [ ] **Step 2: 跑测试确认它红**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib untracked_file_over_the_cap
```

预期：`FAILED` at `an oversized untracked file must be reported as binary`
（原实现标 `binary: false`、正文含 256 KiB 的 x）。

- [ ] **Step 3: 实现**

在 `src/git_diff.rs` 常量区加：

```rust
/// 未跟踪文件参与 diff 合成的单文件上限。
///
/// 超过它的文件在清单里标 `binary: true`、正文给一行说明,**不读内容**。
/// 为什么是 1 MiB:`MAX_DIFF_BYTES` 是 256 KiB,一个超过 1 MiB 的未跟踪文件
/// 无论如何都不可能完整出现在最终 diff 里(前面还有 `truncate`);继续读它
/// 只是白白占用时间与内存。前端对 `binary: true` 已有占位渲染
/// (`desktop/src/components/GitDiffView.tsx:187`)。
pub const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;

/// 读未跟踪文件；超过 [`MAX_UNTRACKED_FILE_BYTES`] 或读失败返回 `None`。
///
/// `None` 的语义 = 「不参与内容合成」：调用方据此标 binary / 跳过正文。
/// 先看 `metadata` 再读，故超大文件不会被读进内存。
fn read_untracked_capped(full: &Path) -> Option<Vec<u8>> {
    let len = std::fs::metadata(full).ok()?.len();
    if len > MAX_UNTRACKED_FILE_BYTES {
        return None;
    }
    std::fs::read(full).ok()
}
```

把 `files()` 的未跟踪循环改成「读一次、判一次」：

```rust
    for path in untracked_paths(repo) {
        let full = root.join(&path);
        // 一次读取服务两件事：数换行、判二进制。原实现分别 `std::fs::read`
        // 一次(+ `is_binary` 再读一次)，纯粹是重复 IO。
        let bytes = read_untracked_capped(&full);
        let (additions, binary) = match bytes.as_deref() {
            Some(b) => (
                b.iter().filter(|&&c| c == b'\n').count() as u64,
                looks_binary(b),
            ),
            // 超上限或读不到：视为二进制，(0 行新增)。
            None => (0, true),
        };
        out.push(FileStat {
            path,
            status: "A".into(),
            additions,
            deletions: 0,
            binary,
        });
    }
```

把 `untracked_diff()` 改成用同一个 helper：

```rust
fn untracked_diff(root: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let Some(bytes) = read_untracked_capped(&root.join(path)) else {
            // 超上限 / 读不到：给一行明确说明，而不是留下一个空白条目
            // (D1 的另一半：清单里有、点开却是空的)。
            out.push_str(&format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\n\
                 Binary files /dev/null and b/{path} differ (too large to render)\n"
            ));
            continue;
        };
        out.push_str(&format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n"
        ));
        if looks_binary(&bytes) {
            out.push_str(&format!("Binary files /dev/null and b/{path} differ\n"));
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
        let lines: Vec<&str> = text.lines().collect();
        out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
        for line in &lines {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
```

删除已无调用点的 `is_binary`（若 `grep -n "is_binary" src/` 确认无其它调用），
否则保留并标注仅供测试。

- [ ] **Step 4: 跑测试确认全绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib untracked_file
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib git_diff
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：两个新用例 PASS；既有 `untracked_*` / `file_diff_covers_untracked_files` 全绿；
全量 `1059 passed`（1057 + 2）。

- [ ] **Step 5: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs \
              yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix: cap untracked file reads and stop reading them twice

files/is_binary/untracked_diff 对每个未跟踪文件各做一次全量 std::fs::read,
没有大小上限,而截断只在最后做——MAX_DIFF_BYTES 只约束输出、不约束输入。
实测一个 512 MiB 的未跟踪文件被完整读进内存,耗时 4.23s,最终只输出 262 KB。

改为先看 metadata,超过 1 MiB 就不读内容,清单标 binary、正文给一行
too large to render 说明(前端对 binary 已有占位渲染)。同时把数换行与判
二进制合并到同一次读取。"
```

---

### Task 5: `thread/diff/read` 移出主循环（P1 的主体）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:1904-1929`（`thread/diff/read` 臂）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`（新增用例）

**Interfaces:**
- Consumes：`tokio::spawn`、`tokio::task::spawn_blocking`、
  `crate::server::write_response`、`crate::server::ok_response`、
  `crate::broadcast::ClientId: Clone`
- Produces：无新公开 API

- [ ] **Step 1: 写失败测试**

追加到 `src/rpc_tests.rs`：

```rust
/// `thread/diff/read` 不得把主循环占住。
///
/// 该臂在 async handler 里同步跑 git(5+ 次子进程)并全量读所有未跟踪文件。
/// 判据:一条需要扫大量未跟踪文件的 diff 读取之后,紧随其后的 `thread/list`
/// 必须很快应答——它证明主循环已经回到 `select!`。
#[tokio::test(flavor = "multi_thread")]
async fn diff_read_does_not_block_the_main_loop() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    // 一个没写进 .gitignore 的产物目录：60 个 8 MiB 未跟踪文件。
    let junk = dir.path().join("dist");
    std::fs::create_dir_all(&junk).unwrap();
    let chunk = "y".repeat(1024 * 1024);
    for n in 0..60 {
        std::fs::write(junk.join(format!("chunk{n}.log")), &chunk).unwrap();
    }

    // 先量基线：空载下一条 thread/list 的往返。
    let t0 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":70,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(70)))
        .await;
    let baseline = t0.elapsed();

    // 发一条 diff 读取，紧接着发 list：后者必须不被前者挡住。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":71,"method":"thread/diff/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let t1 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":72,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(72)))
        .await;
    let blocked = t1.elapsed();

    // 相对判据：同一进程同一磁盘上，被挡住时是 1000 倍量级（实测 1101x）。
    // 用 50 倍留足 CI 抖动余量，同时远小于阻塞量级。
    assert!(
        blocked < baseline * 50 + std::time::Duration::from_millis(100),
        "a diff read must not block the main loop: \
         baseline={baseline:?} list_behind_diff={blocked:?}"
    );
    // 两条响应都得回来，确认 diff 本身也没被弄坏。
    let v = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(71)))
        .await;
    assert!(v["result"].is_object(), "diff/read must still answer: {v}");
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认它红**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib diff_read_does_not_block_the_main_loop -- --nocapture
```

预期：`FAILED` at `a diff read must not block the main loop:
baseline=553µs list_behind_diff=609ms`（实测 1101x）。

- [ ] **Step 3: 实现**

把 `src/rpc/thread.rs` 中 `thread/diff/read` 的 1904-1929 行整段替换：

```rust
            // cwd 是 diff 的唯一来源：thread 的 cwd，缺省兜底 cfg.workdir。
            let cwd = state
                .threads
                .get(&thread_id)
                .map(|t| t.cwd.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| state.cfg.workdir.display().to_string());
            // 计算搬到**独立 task** 里做,而不是在本臂内联等待。理由与
            // `thread/listAll` 逐字相同:这一臂要跑 5+ 次 git 子进程,并全量读
            // 每个未跟踪文件(数换行 / 判二进制 / 合成 unified diff)。盘慢、
            // 目录多时这段同步 IO 会把主循环整个占住,期间收到的
            // `turn/interrupt` 只能干等,用户看到的是「点了 Stop 没反应」。
            //
            // 主 runtime 是多线程的(`#[tokio::main(flavor = "multi_thread")]`),
            // 把计算挪到别的 worker 后,主循环立刻回到 `select!`。
            //
            // 代价是计算期间不能借 `AppState`:故先把参数快照成纯值。
            let repo = std::path::PathBuf::from(&cwd);
            let base = req.params.get("base").and_then(|v| v.as_str()).map(str::to_string);
            let commit = req
                .params
                .get("commit")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let path = req
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let hub = Arc::clone(state.hub);
            let client = state.client.clone();
            tokio::spawn(async move {
                let computed = tokio::task::spawn_blocking(move || {
                    if let Some(commit) = commit {
                        let (unified_diff, truncated) =
                            crate::git_diff::commit_diff(&repo, &commit);
                        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                    } else if let Some(path) = path {
                        // 锚点由 Task 2 从 thread meta 接上；本任务先传 `None`（行为不变）。
                        let d = crate::git_diff::thread_diff(&repo, base.as_deref(), None);
                        let (unified_diff, truncated) =
                            crate::git_diff::file_diff(&repo, d.merge_base.as_deref(), &path);
                        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                    } else {
                        serde_json::to_value(crate::git_diff::thread_diff(
                            &repo,
                            base.as_deref(),
                            None,
                        ))
                        .unwrap_or(json!(null))
                    }
                })
                .await;
                let body = match computed {
                    Ok(body) => body,
                    Err(error) => {
                        // 只在 panic 时到这里。回一个合法形状的空结果,而不是
                        // 静默丢响应——后者会让客户端一直等。
                        eprintln!("[app-server] thread/diff/read task failed: {error}");
                        json!({ "unifiedDiff": "", "truncated": false })
                    }
                };
                let _ = write_response(&hub, &client, ok_response(id, body)).await;
            });
            return Ok(Flow::Continue);
```

注意 `id` 是 `RequestId`（本臂开始时已被 move 进来），直接 move 进 spawned task。

- [ ] **Step 4: 跑测试确认全绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib diff_read_does_not_block_the_main_loop
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_diff
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：新用例 PASS（`blocked` 降到与 `baseline` 同量级）；既有 `thread/diff/read`
相关用例全绿；全量 `1060 passed`（1059 + 1）。

- [ ] **Step 5: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs \
              yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix: run thread/diff/read off the main loop

该臂在 async handler 里同步跑 git(5+ 次子进程,无超时)并全量读每个
未跟踪文件,而 serve() 直接 await 它。实测 60 个 8 MiB 未跟踪文件下一次
diff 读取让紧随其后的 thread/list 延迟 1101 倍,期间所有客户端请求与
所有 thread 的事件转发全部排队。

套用 thread/listAll 已验证的两层模式:快照纯值参数 -> tokio::spawn ->
spawn_blocking 计算 -> 在 task 内写响应。注意必须保留 tokio::spawn 那一层:
就地 await spawn_blocking 只把计算挪到 blocking 池,future 仍挂在主循环
task 上,select! 不会转起来。"
```

---

### Task 6: `thread/view/read` 的 refresh 分支移出主循环

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:1930-2020`（`thread/view/read` 的 `if refresh` 分支）
- Test: 复用 Task 5 的机制，新增一条针对 refresh 的用例

**Interfaces:**
- Consumes：`crate::git_diff::command_frame`（async）、`crate::git_diff::structured_frame`（sync）
- Produces：无新公开 API

- [ ] **Step 1: 写失败测试**

追加到 `src/rpc_tests.rs`：

```rust
/// `thread/view/read?refresh=true` 同样不得占住主循环。
///
/// 它的 `structured_frame` 分支同步跑 thread_diff(5+ 次 git);命令帧分支虽然
/// 是 async,但 `command_frame` 开头同步构造 SandboxPolicy,后者调
/// `Path::canonicalize`(在已卸载的挂载点上会卡住),也不经过任何 await。
#[tokio::test(flavor = "multi_thread")]
async fn view_read_refresh_does_not_block_the_main_loop() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;

    let junk = dir.path().join("dist");
    std::fs::create_dir_all(&junk).unwrap();
    let chunk = "y".repeat(1024 * 1024);
    for n in 0..60 {
        std::fs::write(junk.join(format!("chunk{n}.log")), &chunk).unwrap();
    }
    {
        // 结构化帧(无 command)：refresh 会走 structured_frame -> thread_diff。
        let mut frames = h.view_frames.lock().await;
        frames.insert(
            tid.clone(),
            crate::git_diff::ViewFrame {
                command: None,
                as_kind: "diff".to_string(),
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                truncated: false,
                timed_out: false,
                note: None,
                base: None,
                base_kind: String::new(),
            },
        );
    }

    let t0 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":90,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(90)))
        .await;
    let baseline = t0.elapsed();

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":91,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let t1 = std::time::Instant::now();
    h.send(r#"{"jsonrpc":"2.0","id":92,"method":"thread/list","params":{}}"#)
        .await;
    let _ = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(92)))
        .await;
    let blocked = t1.elapsed();

    assert!(
        blocked < baseline * 50 + std::time::Duration::from_millis(100),
        "a view refresh must not block the main loop: \
         baseline={baseline:?} list_behind_refresh={blocked:?}"
    );
    let v = h
        .read_matching(256, |v| v.get("id") == Some(&serde_json::json!(91)))
        .await;
    assert!(v["result"].is_object(), "view/read must still answer: {v}");
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认它红**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib view_read_refresh_does_not_block_the_main_loop -- --nocapture
```

预期：`FAILED`，`list_behind_refresh` 在数百 ms 量级。

- [ ] **Step 3: 实现**

把 `thread/view/read` 的整个 `if refresh { ... }` 分支（当前 1954-2020 行）替换为：

```rust
            if refresh {
                // 重算这一帧：**分叉**在「这一帧原本是命令帧还是结构化默认」上。
                // 命令帧重跑同一条命令（模型可重复观察同一视角）；结构化默认则
                // 重算锚点 diff（工作区在这期间变了，重读必须反映新内容）。
                //
                // 与 `thread/diff/read` 同一理由搬进独立 task：结构化分支同步跑
                // thread_diff(5+ 次 git),命令分支虽 async 但开头同步
                // `Path::canonicalize`(挂载点失效时会卡住),都不该压在主循环上。
                let cwd = state
                    .threads
                    .get(&thread_id)
                    .map(|t| t.cwd.clone())
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| state.cfg.workdir.display().to_string());
                let repo = std::path::PathBuf::from(&cwd);
                let anchor = state
                    .threads
                    .get(&thread_id)
                    .and_then(|t| t.store.load(&thread_id).ok().flatten())
                    .and_then(|l| l.meta.anchor_sha);
                // 分两次短暂持锁，**不**把 guard 跨过 await：`serve()` 跑在
                // `tokio::spawn` 里，guard 过 await 会让整个 future 非 `Send`。
                // 快照连**基准来历**一起取：结构化帧可能是「模型指定的基准」
                // (`base_kind == "explicit"`)，重算时必须原样再用那个 base——
                // 只传 `explicit=None` 会让它退化成会话锚点。
                let snapshot = state
                    .view_frames
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|f| (f.command.clone(), f.base.clone(), f.base_kind.clone()));
                let command = snapshot.as_ref().and_then(|(c, _, _)| c.clone());
                let explicit = snapshot.as_ref().and_then(|(_, base, kind)| {
                    (kind == "explicit").then(|| base.clone()).flatten()
                });
                let hub = Arc::clone(state.hub);
                let client = state.client.clone();
                let view_frames = Arc::clone(state.view_frames);
                let tid_for_task = thread_id.clone();
                tokio::spawn(async move {
                    let next = match command.as_deref() {
                        // 命令帧：`command_frame` 是 async(tokio::process + 超时)，
                        // 放进本 task 即已脱离主循环。
                        Some(command) => {
                            crate::git_diff::command_frame(&repo, command, None, None).await
                        }
                        // 结构化帧：全同步(5+ 次 git_ok)，必须进 blocking 池。
                        None => {
                            let repo = repo.clone();
                            let explicit = explicit.clone();
                            let anchor = anchor.clone();
                            match tokio::task::spawn_blocking(move || {
                                crate::git_diff::structured_frame(
                                    &repo,
                                    explicit.as_deref(),
                                    anchor.as_deref(),
                                )
                            })
                            .await
                            {
                                Ok(frame) => frame,
                                Err(error) => {
                                    eprintln!(
                                        "[app-server] view refresh task failed: {error}"
                                    );
                                    // 算不出来就回读已有帧（CAS 阶段会读到它），
                                    // 不写坏帧、不丢响应。
                                    let existing = view_frames.lock().await;
                                    match existing.get(&tid_for_task) {
                                        Some(f) => f.clone(),
                                        None => return,
                                    }
                                }
                            }
                        }
                    };
                    // 写回用 CAS，不是无条件覆盖：「读缓存命令」与「写回新帧」之间
                    // 隔了一次 await（最多 VIEW_TIMEOUT）。期间别的写入者（另一次
                    // refresh、或 Task 4 的 watcher）可能已经写了更新的帧；若无条件
                    // 覆盖，面板就会显示与「刚刚请求的命令」不符的内容。
                    // 判定：槽里的命令仍等于我们读到的快照才写。
                    // 边界：判据只有 `command`，故它只对**命令帧**有区分力——两条
                    // 都是结构化帧时区分不出先后，后果仅是多算一遍，不产生错标。
                    let mut frames = view_frames.lock().await;
                    let unchanged = frames.get(&tid_for_task).and_then(|f| f.command.as_deref())
                        == command.as_deref();
                    if unchanged {
                        frames.insert(tid_for_task.clone(), next);
                    }
                    let result = frames
                        .get(&tid_for_task)
                        .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                        .unwrap_or(json!(null));
                    drop(frames);
                    let _ = write_response(&hub, &client, ok_response(id, result)).await;
                });
                return Ok(Flow::Continue);
            }
            let result = state
                .view_frames
                .lock()
                .await
                .get(&thread_id)
                .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                .unwrap_or(json!(null));
            write_response(state.hub, state.client, ok_response(id, result)).await?;
```

注意：`next` 由 `match` 两支产生，两支的类型都是 `ViewFrame`；
`ViewFrame` 需 `Clone`（它已 `#[derive(Debug, Clone, serde::Serialize)]`）。

- [ ] **Step 4: 跑测试确认全绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib view_read
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：新用例 PASS；既有 `thread_view_read_*` 全绿（尤其
`thread_view_read_refresh_reruns_a_command_frame` 的 CAS 语义）；
全量 `1061 passed`（1060 + 1）。

- [ ] **Step 5: `cargo fmt --all` 后提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs \
              yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "fix: run the view refresh off the main loop

thread/view/read?refresh=true 的 structured_frame 分支同步跑 thread_diff,
命令帧分支虽 async 但开头同步构造 SandboxPolicy(其中 Path::canonicalize
在已卸载的挂载点上会卡住),都不该压在主循环上。

命令帧走本 task 的 await(tokio::process 本身不阻塞),结构化帧进
spawn_blocking。CAS 写回语义逐字保留。"
```

---

### Task 7: 全量回归与验收

**Files:** 无改动（纯验证）

- [ ] **Step 1: 确认没有残留 cargo 进程**

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent_app_server" || echo "clean"
```

- [ ] **Step 2: 全量测试**

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib
```

预期：`1061 passed; 0 failed; 4 ignored`（基线 1054 + 7 个新用例：
T1、T2-two、T3、T4-two、T5）。

- [ ] **Step 3: 格式与 lint**

```bash
cd yi-agent-rs && cargo fmt --all -- --check && echo "fmt ok"
cd yi-agent-rs && cargo clippy -p yi-agent-app-server --all-targets 2>&1 | tail -20
```

预期：`fmt ok`；clippy 不新增 warning（与 `main` 的基线比）。

- [ ] **Step 4: 桌面端不受影响**

```bash
cd desktop && npm test 2>&1 | tail -20
```

预期：全绿（`gitDiff.test.ts` / `useImageData.test.ts` 的既有用例不依赖服务端实现细节）。

- [ ] **Step 5: 逐条核对 spec §5 的测试表**

确认 spec §5 的五个测试项（共 7 个用例，T2/T4 各含成功与失败两条）都存在，
且都在实现前红过：

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib -- \
  thread_delete_drops_the_view_frame \
  untracked_file_over_the_cap_is_not_read_into_the_diff \
  diff_read_does_not_block_the_main_loop \
  run_git_bounded_gives_up_after_the_timeout \
  run_git_bounded_returns_stdout_on_success \
  untracked_file_under_the_cap_is_still_rendered_as_text \
  image_read_does_not_scale_with_file_size
```

预期：7 个全 PASS。

---

## 附：任务顺序的理由

按「风险从低到高、依赖从少到多」排：

1. **Task 1（view_frames）** 只加一行，与其余全部无关，先落地能立刻确认流程（分支、
   fmt、提交、全量测试）是通的。
2. **Task 2（image/read）** 独立文件、独立测试，不碰 git_diff。
3. **Task 3（git_ok 超时）** 改 `git_diff` 的地基，但签名不变，先做完它能让
   Task 4 / 5 的测试不受「偶发挂住」干扰。
4. **Task 4（未跟踪上限）** 依赖 Task 3 的 `git_ok` 已稳定。
5. **Task 5（diff/read 移出主循环）** 依赖 Task 4 的读上限——否则测试要造 480 MiB
   文件才能看出阻塞，慢且占盘。
6. **Task 6（view refresh 移出）** 复用 Task 5 的模式，最后做。
7. **Task 7** 纯验证。
