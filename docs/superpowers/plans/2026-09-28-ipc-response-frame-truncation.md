# daemon 命令响应帧截断修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 daemon 的命令响应能完整传输到协议声明的 1 MiB 上限，而不是在内核发送缓冲（macOS 默认 8192 B）处被静默截断；并把「帧被截断」与「帧超限」拆成两个可区分的错误。

**Architecture:** 根因是连接从非阻塞 listener 继承了 `O_NONBLOCK`，而命令响应用裸 `write_all` 写出、遇 `WouldBlock` 不重试。修复分三层：写路径复用仓库已有的「分片 + 重试」范式（订阅路径的 `write_frame_chunk` 已验证可行）；读帧把「超限」与「截断」拆成两个 `IpcError` 变体并各自映射错误码；补一条走真实 `accept` 的集成测试堵住 macOS-only 盲区。

**Tech Stack:** Rust（`std::os::unix::net::UnixStream`；测试期用 `libc` 调 `setsockopt` 缩小 `SO_SNDBUF`）、cargo test、just。

**设计文档：** `docs/superpowers/specs/2026-09-28-ipc-response-frame-truncation-design.md`

**工作区：** 本计划在 worktree `.worktrees/fix/ipc-command-response-truncation`（分支 `fix/ipc-command-response-truncation`）内执行。

---

## Global Constraints

- **严禁在 `main` 上直接改动**。全部改动留在 `fix/ipc-command-response-truncation` 分支。
- Commit message 用 conventional commits，**不写 `Co-Authored-By`**，首行 ≤72 字符。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认无其他 cargo 进程（AGENTS.md 要求，避免锁竞争 / exit 137）。
- **不要**跑 `cargo test --workspace`；按 crate 跑（本计划只用 `-p yi-agent-store`）。
- **定位代码一律用符号名（函数名/变量名），不要硬编码行号**——编辑会让行号漂移。
- 不改协议常量 `MAX_FRAME_BYTES = 1024 * 1024`；不调 `SO_SNDBUF`（生产代码内）。
- 每次 commit 只暂存本任务涉及的文件。

**工作区已存在的输入：** `src/ipc.rs` 中有两条**未提交**的 red 测试，位于 `mod subscription_queue_tests` 内：
`a_truncated_response_is_reported_as_truncated_not_as_oversized` 与
`a_frame_over_the_cap_is_reported_as_too_large_not_truncated`。
它们引用尚未定义的 `IpcError::TruncatedFrame`，故当前 `cargo test -p yi-agent-store --lib --no-run` 编译失败（E0599）。Task 1 消费它们。

---

## File Structure

| 文件 | 职责 | 本计划改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | IPC 协议实现：错误枚举、读写帧、daemon 循环 | 新增 2 个错误变体；拆分读帧判定；新增 `write_frame_until` 并接入命令响应写路径；放宽写超时；新增 4 条单测 |
| `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` | daemon 端到端集成测试 | 新增 1 条走真实 `accept` 的大响应测试 |
| `docs/bug-list.md` | 项目缺陷清单 | 勾记该缺陷 + 遗留 CI 盲区 |

---

### Task 1: 拆分读帧错误语义（让两条 red 测试转绿）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（`IpcError` 枚举、`read_limited_frame`、`ipc_error_code`、`mod subscription_queue_tests`）
- Test: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（既有两条 red 测试 + 新增一条）

**Interfaces:**
- Consumes: 无（首个任务）
- Produces:
  - `IpcError::TruncatedFrame { received: usize }` — 读方向，对端在帧中途关闭
  - `IpcError::FrameWriteTimeout { written: usize, total: usize }` — 写方向；本任务只声明，Task 2 使用
  - `read_limited_frame` 新语义：超限 → `FrameTooLarge`；无换行结尾 → `TruncatedFrame`

- [x] **Step 1: 确认两条 red 测试当前确实失败（red 基线）**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store --lib --no-run 2>&1 | tail -20
```
Expected: 编译失败，含
`error[E0599]: no variant named TruncatedFrame found for enum ipc::IpcError`。
这是 red 基线——记录下来，不要跳过。

- [x] **Step 2: 在 `IpcError` 中新增两个变体**

定位 `ipc.rs` 中的 `pub enum IpcError {`，在 `FrameTooLarge` 变体之后插入：

```rust
    #[error("IPC response frame is truncated after {received} bytes (the peer closed mid-frame)")]
    TruncatedFrame { received: usize },
    #[error("timed out writing response frame after {written} of {total} bytes")]
    FrameWriteTimeout { written: usize, total: usize },
```

- [x] **Step 3: 拆分 `read_limited_frame` 的两个判定**

定位 `fn read_limited_frame`，把合并的判定：

```rust
    if frame.len() > MAX_FRAME_BYTES || !frame.ends_with(b"\n") {
        return Err(IpcError::FrameTooLarge);
    }
```

替换为：

```rust
    if frame.len() > MAX_FRAME_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    if !frame.ends_with(b"\n") {
        return Err(IpcError::TruncatedFrame {
            received: frame.len(),
        });
    }
```

- [x] **Step 4: 在 `ipc_error_code` 中显式映射两个新变体**

定位 `fn ipc_error_code`，把：

```rust
        IpcError::Json(_) | IpcError::FrameTooLarge => IpcErrorCode::Validation,
        _ => IpcErrorCode::Internal,
```

替换为：

```rust
        // Transport-layer failures stay Validation: a truncated or timed-out
        // response is not a daemon fault, and reporting it as Internal sends
        // operators hunting for a crashed daemon that is running fine.
        IpcError::Json(_)
        | IpcError::FrameTooLarge
        | IpcError::TruncatedFrame { .. }
        | IpcError::FrameWriteTimeout { .. } => IpcErrorCode::Validation,
        _ => IpcErrorCode::Internal,
```

- [x] **Step 5: 新增单测，锁死错误码映射（防回落 `_` 分支）**

定位 `mod subscription_queue_tests` 中的
`fn a_frame_over_the_cap_is_reported_as_too_large_not_truncated()`，在其**之后**插入：

```rust
    #[test]
    fn truncation_and_write_timeout_map_to_validation_not_internal() {
        // Regression guard: without an explicit arm these fall through to
        // `_ => Internal`, which tells the caller the daemon broke rather than
        // that the transport did.
        assert_eq!(
            ipc_error_code(&IpcError::TruncatedFrame { received: 12 }),
            IpcErrorCode::Validation
        );
        assert_eq!(
            ipc_error_code(&IpcError::FrameWriteTimeout {
                written: 8192,
                total: 26000,
            }),
            IpcErrorCode::Validation
        );
    }
```

- [x] **Step 6: 跑单测，确认全部通过**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store --lib 2>&1 | tail -25
```
Expected: PASS。特别确认这四条：
`a_truncated_response_is_reported_as_truncated_not_as_oversized`、
`a_frame_over_the_cap_is_reported_as_too_large_not_truncated`、
`truncation_and_write_timeout_map_to_validation_not_internal`

- [x] **Step 7: 格式化并提交**

```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs
git commit -m "fix(store): split truncated frames from oversized frames

read_limited_frame collapsed two unrelated failures into FrameTooLarge:
a genuine size violation, and a peer closing mid-frame. The latter is what
actually happens when a response outgrows the socket send buffer, so the
error told operators to look for a 1 MiB payload that was never sent.

TruncatedFrame now carries the received byte count, FrameWriteTimeout
carries write progress, and both map to Validation instead of falling
through to Internal."
```

---

### Task 2: 命令响应写路径根治（分片写 + 放宽写超时）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（连接配置、`write_envelope_frame`、新增 `write_frame_until`、新增单测）

**Interfaces:**
- Consumes: Task 1 的 `IpcError::FrameWriteTimeout { written, total }`
- Produces:
  - `fn write_frame_until(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<(), IpcError>`
  - `const COMMAND_WRITE_DEADLINE: Duration = Duration::from_secs(30)`
  - `write_envelope_frame` 不再使用裸 `write_all`

- [x] **Step 1: 写失败测试——大帧在非阻塞连接上必须完整写出**

先确认 `mod subscription_queue_tests` 中已有辅助函数
`fn set_send_buffer(stream: &UnixStream, bytes: libc::c_int)`（用于缩小 `SO_SNDBUF` 以逼出 `WouldBlock`；已有测试 `partial_event_frame_finishes_before_resync_frame` 在用）。在该函数**之后**插入：

```rust
    #[test]
    fn a_frame_larger_than_the_send_buffer_is_written_in_full() {
        // Reproduces the production path: the listener is non-blocking, so an
        // accepted connection inherits O_NONBLOCK and a bare write_all gives up
        // at the first WouldBlock. Shrinking SO_SNDBUF makes the truncation
        // deterministic on every platform, not just macOS.
        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        set_send_buffer(&writer, 4 * 1024);

        let payload = vec![b'x'; 64 * 1024];
        let total = payload.len();
        let deadline = Instant::now() + Duration::from_secs(10);

        // Capture the length before the move: the drain loop needs it, and
        // `payload` itself must stay here for write_frame_until.
        let pump = std::thread::spawn(move || {
            let mut drained = 0usize;
            let mut scratch = [0u8; 8192];
            while drained < total {
                match reader.read(&mut scratch) {
                    Ok(0) => break,
                    Ok(n) => drained += n,
                    Err(_) => break,
                }
            }
        });

        write_frame_until(&mut writer, &payload, deadline).unwrap();
        write_frame_until(&mut writer, b"\n", deadline).unwrap();
        writer.flush().unwrap();
        drop(writer);
        pump.join().unwrap();
    }
```

- [x] **Step 2: 跑测试确认失败**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store --lib a_frame_larger_than_the_send_buffer_is_written_in_full 2>&1 | tail -20
```
Expected: FAIL — 编译错误 `cannot find function write_frame_until in this scope`。

- [x] **Step 3: 新增 `COMMAND_WRITE_DEADLINE` 与 `write_frame_until`**

定位顶部常量 `const CONFIRMATION_TTL: Duration = Duration::from_secs(60);`，在其后插入：

```rust
// A command response may legitimately reach MAX_FRAME_BYTES (1 MiB). Writing
// that through a socket whose peer reads slowly takes far longer than the 1s
// timeout used to detect half-open readers, so the write side gets its own,
// much looser budget.
const COMMAND_WRITE_DEADLINE: Duration = Duration::from_secs(30);
```

定位 `fn write_envelope_frame(`，在其**之前**插入：

```rust
/// Writes a whole frame, tolerating partial writes and backpressure.
///
/// The accepted connection inherits `O_NONBLOCK` from the non-blocking
/// listener, so a single `write` consumes only what fits in the socket send
/// buffer (8192 bytes on macOS) and then reports `WouldBlock`. `write_all`
/// treats that as fatal and abandons the response mid-frame; this loops
/// instead, sleeping briefly until the peer drains the buffer or the deadline
/// expires.
fn write_frame_until(
    stream: &mut UnixStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), IpcError> {
    let mut written = 0usize;
    while written < bytes.len() {
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(IpcError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write response frame",
                )));
            }
            Ok(step) => written += step,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(IpcError::FrameWriteTimeout {
                        written,
                        total: bytes.len(),
                    });
                }
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(IpcError::Io(error)),
        }
    }
    Ok(())
}
```

- [x] **Step 4: 让 `write_envelope_frame` 使用 `write_frame_until`**

定位 `fn write_envelope_frame` 函数体的末尾，把这三行：

```rust
    stream.write_all(&frame)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
```

**整体替换**为：

```rust
    let deadline = Instant::now() + COMMAND_WRITE_DEADLINE;
    write_frame_until(stream, &frame, deadline)?;
    write_frame_until(stream, b"\n", deadline)?;
    stream.flush()?;
    Ok(())
```

不要保留任何 `write_all` 调用（`write_frame_until` 已覆盖该职责）。

- [x] **Step 5: 放宽命令连接的写超时（读超时保持不变）**

定位 `handle_client` 中这两行：

```rust
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
```

替换为：

```rust
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    // The read timeout guards against a half-open peer and stays tight. The
    // write timeout must span a full 1 MiB response to a slow reader, so it
    // tracks COMMAND_WRITE_DEADLINE rather than the read budget.
    stream.set_write_timeout(Some(COMMAND_WRITE_DEADLINE))?;
```

- [x] **Step 6: 跑单测确认通过**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store --lib 2>&1 | tail -25
```
Expected: PASS，含 `a_frame_larger_than_the_send_buffer_is_written_in_full`。

- [x] **Step 7: 格式化并提交**

```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs
git commit -m "fix(store): write command responses through backpressure

An accepted connection inherits O_NONBLOCK from the non-blocking listener,
so write_envelope_frame's bare write_all stopped at the first WouldBlock --
8192 bytes into the response on macOS, silently truncating anything longer.
That is why wait_agent could not return a delivery report past ~8 KB.

write_frame_until loops on partial writes and WouldBlock against a deadline,
mirroring the retry the subscription path already used. The connection write
timeout widens to match, since a full 1 MiB reply to a slow reader cannot
finish inside the 1s read budget."
```

---

### Task 3: 集成测试——走真实 `accept` 的大响应防护

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: Task 2 的 `write_frame_until` 行为（经真实 daemon socket 体现）
- Produces: 新增测试 `a_response_payload_larger_than_the_socket_send_buffer_arrives_intact`；无新导出符号

- [x] **Step 1: 写测试**

在 `tests/runtime_ipc.rs` 末尾追加：

```rust
#[test]
fn a_response_payload_larger_than_the_socket_send_buffer_arrives_intact() {
    // Every other read-side test builds its connection with UnixStream::pair,
    // which is blocking and therefore cannot reproduce the production defect:
    // a connection accepted by the non-blocking listener inherits O_NONBLOCK.
    // Only a real daemon exercises the accepted-connection path, and only a
    // payload past the 8192-byte send buffer makes truncation observable.
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(directory.path().join("runtime"), &database).unwrap();

    let IpcResponse::SessionCreated {
        session_id,
        root_task_id,
    } = send_request(daemon.socket_path(), IpcRequest::CreateSession).unwrap()
    else {
        panic!("expected a created session");
    };

    // The objective is persisted verbatim as the task's delivery_json, so a
    // large objective produces a large InspectTask response.
    let objective = "x".repeat(64 * 1024);
    let IpcResponse::TaskSpawned { task_id } = send_request(
        daemon.socket_path(),
        IpcRequest::SpawnChild {
            session_id,
            parent_task_id: root_task_id,
            objective: objective.clone(),
            mode: None,
        },
    )
    .unwrap()
    else {
        panic!("expected a spawned child task");
    };

    // Read raw bytes rather than JSON: a truncated frame is only detectable by
    // its missing terminator, and serde would report a parse error instead.
    let request = json!({
        "protocol_version": 1,
        "request_id": "large-response",
        "command": { "type": "InspectTask", "task_id": task_id },
    });
    let mut stream = UnixStream::connect(daemon.socket_path()).unwrap();
    writeln!(stream, "{request}").unwrap();
    stream.flush().unwrap();

    let mut line = Vec::new();
    BufReader::new(stream).read_until(b'\n', &mut line).unwrap();

    assert!(
        line.ends_with(b"\n"),
        "response was truncated at {} bytes without a terminator",
        line.len()
    );
    assert!(
        line.len() > 8 * 1024,
        "expected a response past the send buffer, got {} bytes",
        line.len()
    );

    let envelope: Value = serde_json::from_slice(&line).unwrap();
    let detail = &envelope["result"];
    assert_eq!(detail["type"], "TaskDetail");
    let delivery = detail["delivery_json"].as_str().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(delivery).unwrap()["objective"],
        Value::String(objective),
        "the delivered objective must survive the round trip intact"
    );
}
```

- [x] **Step 2: 跑测试确认通过**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store --test runtime_ipc a_response_payload_larger_than_the_socket_send_buffer_arrives_intact 2>&1 | tail -20
```
Expected: PASS。

- [x] **Step 3: 关键——证明该测试能抓住这个 bug（临时回退验证）**

把 `write_envelope_frame` 中的 `write_frame_until(stream, &frame, deadline)?;` 临时改回
`stream.write_all(&frame)?;`，重跑 Step 2 的命令。

Expected: FAIL，断言 `response was truncated at ... bytes without a terminator`。

确认失败后**立即还原**为 `write_frame_until(...)`，重跑 Step 2 确认重新 PASS。

> **平台说明：** 在 Linux 上回退后可能仍 PASS（Linux 的 `accept` 不继承 `O_NONBLOCK`）。若本机是 Linux，请在 commit message 中如实注明该测试在本机无法区分修复前后，防护价值仅体现在 macOS/BSD。

- [x] **Step 4: 跑整个 store crate 的测试**

Run:
```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo test -p yi-agent-store 2>&1 | tail -25
```
Expected: PASS（lib + 全部集成测试）。

- [x] **Step 5: 格式化并提交**

```bash
cd .worktrees/fix/ipc-command-response-truncation/yi-agent-rs
cargo fmt --all
git add crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "test(store): cover a response past the send buffer on a real accept

Existing read-side tests pair() their sockets, which is blocking, so none
reproduced the inherited-O_NONBLOCK truncation. This drives a real daemon
and asserts a >8 KB response arrives with its terminator and payload intact."
```

---

### Task 4: 收尾——记录缺陷与 CI 平台盲区

**Files:**
- Modify: `docs/bug-list.md`

**Interfaces:**
- Consumes: Task 1–3 的修复结论
- Produces: 无

- [x] **Step 1: 在 `docs/bug-list.md` 勾记该缺陷**

定位文件中已有的条目：

```markdown
- [ ] app-server `render_content` 不截断工具输出，超大工具结果可能超过 `MAX_FRAME_BYTES`（1MB）导致客户端拒收该帧
```

在其**之后**插入两行：

```markdown
- [x] daemon 命令响应超过内核发送缓冲（macOS 默认 8192 B）即被静默截断，客户端误报为 `IPC frame exceeds 1048576 bytes`，使 `wait_agent` 无法回传超过约 8 KB 的子 agent 交付报告（修复：连接从非阻塞 listener 继承 `O_NONBLOCK`，命令响应却用裸 `write_all`，一遇 `WouldBlock` 即放弃；改为新增 `write_frame_until` 分片写 + `WouldBlock` 重试（与订阅路径同一范式），写超时放宽至 30s、读超时不变；读帧把「超限」与「截断」拆为 `FrameTooLarge` 与新增的 `TruncatedFrame`/`FrameWriteTimeout`，三者均映射 `Validation` 而非落入 `Internal`。见 [设计](../superpowers/specs/2026-09-28-ipc-response-frame-truncation-design.md)、[计划](../superpowers/plans/2026-09-28-ipc-response-frame-truncation.md)。验证：`cargo test -p yi-agent-store`（含 `a_frame_larger_than_the_send_buffer_is_written_in_full`、`a_truncated_response_is_reported_as_truncated_not_as_oversized`、`a_frame_over_the_cap_is_reported_as_too_large_not_truncated`、`truncation_and_write_timeout_map_to_validation_not_internal`、`a_response_payload_larger_than_the_socket_send_buffer_arrives_intact`））
- [ ] IPC 集成测试在 Linux CI 上无法覆盖 `accept` 继承 `O_NONBLOCK` 的平台差异（`ci.yml` 跑 `ubuntu-latest`，而 `release.yml` 已有 `self-hosted` Mac runner）；此类平台相关缺陷会继续漏网
```

- [x] **Step 2: 提交**

```bash
cd .worktrees/fix/ipc-command-response-truncation
git add docs/bug-list.md
git commit -m "docs: log the daemon response truncation fix and its CI blind spot

The defect was platform-specific: accept() inherits O_NONBLOCK on macOS/BSD
but not Linux, so the Linux CI could never catch it. Record the fix, and
leave the CI coverage gap on the list rather than losing it."
```

---

## Self-Review

**1. Spec coverage**

| spec 章节 | 对应任务 |
|---|---|
| §3.1 A1 分片写复用已有范式 | Task 2 Step 3–4 |
| §3.1 A2 放宽写超时、读超时不变 | Task 2 Step 5 |
| §3.1 A3 备选（阻塞语义） | 未采用——spec 明确 A1 更鲁棒 |
| §3.2 拆分错误语义 + 新增两个变体 | Task 1 Step 2–3 |
| §3.2 `ipc_error_code` 显式映射（防落 `_`） | Task 1 Step 4–5 |
| §3.3 C1 走真实 `accept` 的集成测试 | Task 3 |
| §3.3 C2 两条既有 red 测试转绿 | Task 1 Step 1、6 |
| §5 手工复现 | Task 3 Step 3（回退验证等价手动复现） |
| §6 更新 bug-list | Task 4 Step 1 |
| §6 评估 CI 平台覆盖 | Task 4 Step 1（记为遗留条目） |
| §4 非目标（不调 SO_SNDBUF、不改常量、不做分块重构） | Global Constraints 明令禁止 |

无缺口。

**2. Placeholder scan**

无 TBD/TODO/"add error handling" 之类占位。所有代码块均为可直接粘贴的完整内容。

**3. Type consistency**

- `IpcError::TruncatedFrame { received: usize }`：Task 1 Step 2 定义 → Step 3 构造 → Step 5 断言，一致。
- `IpcError::FrameWriteTimeout { written: usize, total: usize }`：Task 1 Step 2 定义 → Step 5 断言 → Task 2 Step 3 构造，字段名一致。
- `write_frame_until(stream: &mut UnixStream, bytes: &[u8], deadline: Instant) -> Result<(), IpcError>`：Task 2 Step 3 定义 → Step 1 测试调用 → Step 4 生产调用，三处签名一致。
- `COMMAND_WRITE_DEADLINE: Duration`：Task 2 Step 3 定义 → Step 4、Step 5 使用，一致。
- 测试名 `a_frame_larger_than_the_send_buffer_is_written_in_full`（Task 2）与 `a_response_payload_larger_than_the_socket_send_buffer_arrives_intact`（Task 3）名称不同，各自在对应命令中完整引用，无混淆。
