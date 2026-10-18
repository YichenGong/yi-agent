### Task 3: Daemon accept loop polls instead of sleeping 10ms

**Rationale:** The daemon's listener sets non-blocking and sleeps 10ms on `WouldBlock`, adding 0-10ms of baseline latency and ~100 wakeups/sec to every request. Same `poll(2)` treatment as Task 1.

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:858-859` (`set_nonblocking`), `:915-948` (accept match), and the `listener.accept` error arm
- Test: covered structurally by Task 2's tests plus the existing daemon tests; no new timing test.

**Interfaces:**
- Consumes: `RequestContext` from Task 2.
- Produces: private `fn daemon_accept_wait(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool>`.

- [ ] **Step 1: Add the wait helper**

Add above `fn handle_client(` in `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`:

```rust
/// 等 listener 可读，或超时（粒度同 [`DAEMON_ACCEPT_POLL`]）。
///
/// 与插件侧同款：用 `poll(2)` 取代「非阻塞 + sleep」，连接一到就返回，避免给每个
/// 请求叠加 0-10ms 的盲等，同时把 `thread_stop` 的检查保持在固定节奏。
fn daemon_accept_wait(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as libc::c_int;
    // SAFETY: 单个已初始化的 `pollfd`，`nfds = 1` 与之一致；无其它指针参数。
    let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    match ready {
        0 => Ok(false),
        count if count > 0 => Ok(true),
        _ => Err(std::io::Error::last_os_error()),
    }
}

/// accept 等待粒度：只决定 `thread_stop` 多久被看一次，不再是请求延迟。
const DAEMON_ACCEPT_POLL: Duration = Duration::from_millis(10);
```

Ensure `use std::os::unix::io::AsRawFd;` is imported in this file. No new dependency is needed: `yi-agent-store/Cargo.toml` already declares `libc = "0.2"` (`:27`).

- [ ] **Step 2: Replace the sleep with the poll gate**

In the daemon accept loop, replace the `match listener.accept()` block's wait behaviour. **Note:** the enclosing `thread::spawn(move || { ... })` closure returns `()` and contains no `?` — so the wait result must be handled explicitly, not with `?`:

```rust
                let ready = match daemon_accept_wait(&listener, DAEMON_ACCEPT_POLL) {
                    Ok(ready) => ready,
                    Err(error) => {
                        tracing::error!(error = %error, "daemon accept poll failed; retrying");
                        continue;
                    }
                };
                if !ready {
                    continue;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        // ...existing handler spawn, unchanged...
                    }
                    // `poll` 与 `accept` 之间对端断开：等下一次事件即可。
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
```

Delete the old `Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { thread::sleep(Duration::from_millis(10)); }` arm. Keep `listener.set_nonblocking(true)?;` as-is (it prevents `accept` from blocking between the poll and the accept).

- [ ] **Step 3: Run the crate's tests**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib`
Expected: PASS.

- [ ] **Step 4: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/ipc-responsiveness
git add yi-agent-rs/crates/yi-agent-store/src/ipc.rs
git commit -m "perf(daemon): poll for connections instead of sleeping 10ms"
```

---

