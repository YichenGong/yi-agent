### Task 1: Plugin query server uses `poll(2)` instead of a 200ms accept sleep

**Rationale:** `serve_with` binds non-blocking and, on `WouldBlock`, sleeps a full `ACCEPT_POLL` (200ms). A query that arrives just after the sleep starts waits ~200ms, so `list` latency is bimodal (0ms or ~271ms, measured). Replacing the sleep with `poll(POLLIN, ACCEPT_POLL)` returns the moment a connection arrives while keeping the `keep_going` check cadence identical — so both existing shutdown tests keep passing unchanged.

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs:36-40` (constants), `:88-123` (`serve_with`), `:124+` (tests)
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces: `serve_with(socket, dispatch, keep_going)` keeps its exact signature; adds private `fn wait_readable(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`.

- [ ] **Step 1: Add the `libc` dependency**

Edit `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml`, adding `libc` under `[dependencies]`:

```toml
[dependencies]
serde.workspace = true
sha2 = "0.10"
serde_json.workspace = true
libc = "0.2"
```

This needs no `[workspace.dependencies]` entry; the lock already contains `libc 0.2.189`.

- [ ] **Step 2: Write the failing test**

Add to `mod tests` in `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs` (place it after `a_client_that_connects_and_leaves_does_not_stop_serving`):

```rust
    /// 回归：accept 不再盲睡 200ms。三个**连续**查询的总耗时必须远小于
    /// `ACCEPT_POLL`——旧实现里每个查询都要等下一次轮询醒来(实测每个 ~200ms,
    /// 三个合计 ~600ms)，新实现每个 ~1ms。
    #[test]
    fn consecutive_queries_do_not_wait_for_the_accept_poll() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_path(dir.path()).expect("temp dir fits");
        let dispatch = fake();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let path = socket.clone();
        let handle = std::thread::spawn(move || {
            serve_with(&path, dispatch, || {
                !flag.load(std::sync::atomic::Ordering::SeqCst)
            })
            .unwrap()
        });

        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let start = std::time::Instant::now();
        for _ in 0..3 {
            let mut stream = UnixStream::connect(&socket).expect("must connect");
            stream
                .write_all(br#"{"type":"plugin.query","method":"list","params":{}}"#)
                .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            let reply: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(reply["type"], "plugin.result", "{reply}");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < ACCEPT_POLL,
            "three back-to-back queries took {elapsed:?}; the accept path is still sleeping"
        );

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        handle.join().unwrap();
    }
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc consecutive_queries_do_not_wait_for_the_accept_poll`
Expected: FAIL — `three back-to-back queries took ~600ms; the accept path is still sleeping`.

- [ ] **Step 4: Add the `wait_readable` helper**

In `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`, update the imports and constants block (currently lines 11-14 and 36-38):

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
```

Keep `CLIENT_READ_TIMEOUT` and `ACCEPT_POLL` as they are, and update `ACCEPT_POLL`'s doc comment:

```rust
/// `keep_going` 的检查粒度，兼作 accept 的等待上限。
///
/// 连接到达时 [`wait_readable`] 立刻返回，故这个值**不再**是每次查询的固有
/// 延迟；它只是「多久回头看一眼 `keep_going`」。
const ACCEPT_POLL: Duration = Duration::from_millis(200);
```

Add the helper immediately above `serve_with`:

```rust
/// 等到 listener 可读(有连接待 accept)，或超时。
///
/// 返回 `true` 表示可以 `accept`；`false` 表示超时——调用方借此保持
/// `keep_going` 的检查粒度，同时不给连接引入 `ACCEPT_POLL` 的固有延迟。
fn wait_readable(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: `descriptor` 是一个已初始化的 `pollfd`，`poll` 只读它的 `fd`/`events`
    // 并写 `revents`；`nfds = 1` 与之匹配。没有其它指针参数。
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    match ready {
        // 超时：没有连接。
        0 => Ok(false),
        // 一个待 accept 的连接（或挂断等情况，`accept` 会给出具体错误）。
        count if count > 0 => Ok(true),
        _ => Err(std::io::Error::last_os_error()),
    }
}
```

- [ ] **Step 5: Rewrite the accept loop in `serve_with`**

Replace the body from `listener.set_nonblocking(true)?;` through the end of the `while` loop with:

```rust
    // 仍设非阻塞：`accept` 只在 `poll` 说可读之后调用，但两者之间对端可能已
    // 断开，那时我们要拿到 `WouldBlock` 而不是卡住。
    listener.set_nonblocking(true)?;
    while keep_going() {
        // 连接一到就返回；超时才回到 `keep_going` 检查（粒度不变）。
        if !wait_readable(&listener, ACCEPT_POLL)? {
            continue;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // 单条连接的失败不得拖垮监听：记下就走，继续服务下一个。
                if let Err(error) = serve_connection(dispatch.as_ref(), stream) {
                    eprintln!("superpowers-kanban: query connection failed: {error}");
                }
            }
            // `poll` 与 `accept` 之间对端断开：等下一次事件即可。
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
```

- [ ] **Step 6: Run the plugin test suite**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc`
Expected: PASS, including `consecutive_queries_do_not_wait_for_the_accept_poll`, `the_socket_sits_where_the_manifest_says`, `a_query_is_forwarded_with_its_params_and_answered_verbatim`, `a_failed_dispatch_becomes_an_error_frame`, `malformed_frames_are_refused_instead_of_panicking`, and both `stop.store(true)` shutdown tests.

- [ ] **Step 7: Run the whole plugin workspace tests**

Run: `cd plugins/superpowers-kanban && cargo test`
Expected: PASS (all three crates).

- [ ] **Step 8: Format and commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs plugins/superpowers-kanban/Cargo.lock
git commit -m "perf(kanban-ipc): poll for connections instead of sleeping 200ms"
```

---

