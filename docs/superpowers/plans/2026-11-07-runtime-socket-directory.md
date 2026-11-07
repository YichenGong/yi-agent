# 运行时 socket 统一目录 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 guardian / daemon 两类 unix socket 的路径长度与项目路径深度彻底解耦，统一落在 `<instance_home>/sockets/<sha256(runtime_dir)[..16]>/`，并删掉 `$TMPDIR` 兜底。

**Architecture:** 在 `yi-agent-lifecycle::scope` 新增纯函数的路径派生层（`socket_directory` / `guardian_socket` / `daemon_socket` / `instance_home_for_sockets`），把「路径怎么算」收敛成单一真相源；`control_path` / `control_socket` / `socket_path_for` 三个既有入口签名不变、内部改指新布局，因此 40+ 调用点无需改动。目录创建与 0700 权限由 bind 方负责，退出时删空目录。guardian 直接用 `RuntimeScope.instance_home`，不读环境变量。

**Tech Stack:** Rust（edition 2024）、`sha2`、`tmpfile`/`wiremock`（测试）、`just`（ci 入口）。

## Global Constraints

- 新布局固定为 `<instance_home>/sockets/<16 hex>/`，两类 socket 文件名不变：`guardian-control.sock`（`lifecycle::GUARDIAN_SOCKET_FILE`）、`runtime.sock`。
- `MAX_SOCKET_PATH_BYTES = 103`（`lifecycle/src/scope.rs:28`），不得改动。
- 哈希输入必须先 `canonicalize_allowing_missing`，否则 macOS `/var` 与 `/private/var` 会让同一项目分裂成两个 socket 目录。
- `<instance_home>` 长到 56 字节时触顶，超限返回错误，**不得**落回 `$TMPDIR`。
- `$TMPDIR` 兜底彻底删除：不保留「超长时换目录」的分支。
- 既有公开签名不得变更：`lifecycle::scope::control_path(&Path) -> PathBuf`、`control_socket(&Path) -> Result<PathBuf, SocketPathTooLong>`、`store::ipc::socket_path_for(&Path) -> Result<PathBuf, IpcError>`。
- 依赖方向单向：`store → lifecycle`，`lifecycle` 不得依赖 `store`。
- 每个 task 结束时 `cargo test -p <crate>` 必须全绿；最后一步跑 `just ci`。
- commit message 用 conventional commits，首行 ≤72 字符，不写 `Co-Authored-By`。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `crates/yi-agent-lifecycle/src/scope.rs` | 新增 `socket_directory` / `guardian_socket` / `daemon_socket` / `instance_home_for_sockets`；改写 `control_path` / `control_socket`；删 `$TMPDIR` 分支 |
| `crates/yi-agent-lifecycle/src/scope.rs`（测试模块） | 长度恒定、bind、`/var` 等价、隔离、超限、`instance_home` 三源等价、跨进程一致 |
| `crates/yi-agent-store/src/ipc/client.rs` | `socket_path_for` 内部改指 `daemon_socket`；删 `socket_file_name`；改写两条旧布局用例 |
| `crates/yi-agent-store/src/ipc/error.rs` | 新增 `InstanceHomeUnavailable`；改写 `SocketPathTooLong` 文案 |
| `crates/yi-agent-store/src/ipc/daemon.rs` | bind 前创建 `sockets/<hash>/` + 0700；退出时删空目录 |
| `crates/yi-agent-supervisors/src/lifecycle/guardian.rs` | `bind_control` 用 `scope.instance_home`、建目录、设 0700 |
| `crates/yi-agent-subagent/src/attach.rs` | `attach_to_answer` 不再丢弃 `instance_home`；managed 入口传 `Some` |
| `crates/yi-agent-store/tests/runtime_ipc.rs` | 断言指向新路径 |

---

### Task 1: `socket_directory` 与两个具名构造函数

**Files:**
- Modify: `crates/yi-agent-lifecycle/src/scope.rs`（在 `MAX_SOCKET_PATH_BYTES` 之后、`runtime_directory` 之前插入）
- Test: `crates/yi-agent-lifecycle/src/scope.rs`（文件底部 `mod tests`）

**Interfaces:**
- Consumes: `canonicalize_allowing_missing(&Path) -> io::Result<PathBuf>`（`scope.rs:56`，`pub`）、`crate::GUARDIAN_SOCKET_FILE`。
- Produces:
  - `pub fn socket_directory(runtime_dir: &Path, instance_home: &Path) -> PathBuf`
  - `pub fn guardian_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf`
  - `pub fn daemon_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf`
  - `const DAEMON_SOCKET_FILE: &str = "runtime.sock";`（`pub`）

- [x] **Step 1: 写失败测试**

在 `crates/yi-agent-lifecycle/src/scope.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn socket_directory_is_independent_of_runtime_dir_depth() {
        let home = Path::new("/home/alice/.yi-agent");
        let short = Path::new("/tmp/p/.yi-agent/runtime");
        let deep = Path::new(&format!("/tmp/{}", "x".repeat(600)));

        let one = socket_directory(short, home);
        let two = socket_directory(deep, home);

        assert_eq!(
            one.parent(),
            two.parent(),
            "both hashes live under the same sockets/ directory"
        );
        assert_ne!(one, two, "different runtimes must not collide");
    }

    #[test]
    fn guardian_and_daemon_sockets_share_one_directory() {
        let runtime = Path::new("/tmp/p/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        let dir = socket_directory(runtime, home);

        assert_eq!(guardian_socket(runtime, home), dir.join(crate::GUARDIAN_SOCKET_FILE));
        assert_eq!(daemon_socket(runtime, home), dir.join(DAEMON_SOCKET_FILE));
    }

    #[test]
    fn the_hash_is_over_the_canonical_runtime_directory() {
        // Two spellings of one directory must hash the same, or the guardian binds
        // one socket and every client polls another. Create a real directory and
        // reach it through the symlinked spelling: on macOS `/var -> /private/var`,
        // and a `TempDir` lands under exactly that pair.
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let runtime = real.join("project/.yi-agent/runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        // The unserialised spelling is whatever the caller happened to hold. If the
        // real path starts with `/private`, the alias drops that prefix; otherwise
        // the two spellings are the same string and the test is vacuously true --
        // which is fine, it still runs on macOS where the bug is reachable.
        let alias = match real.to_str().and_then(|s| s.strip_prefix("/private")) {
            Some(rest) => PathBuf::from(rest),
            None => real.clone(),
        };
        let alias_runtime = alias.join("project/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        assert_eq!(
            socket_directory(&runtime, home),
            socket_directory(&alias_runtime, home),
            "both spellings of one directory must hash to one socket directory"
        );
    }

    #[test]
    fn a_deep_instance_home_produces_a_path_that_still_fits() {
        let runtime = Path::new("/tmp/p/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        for len in [200usize, 500, 900] {
            let deep = Path::new(&format!("/work/{}", "d".repeat(len)));
            let socket = guardian_socket(deep, home);
            assert!(
                socket.as_os_str().len() <= MAX_SOCKET_PATH_BYTES,
                "guardian socket must fit at workdir len {len}: {} bytes",
                socket.as_os_str().len()
            );
        }
    }
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib scope::tests
```

Expected: 编译失败，`cannot find function socket_directory in this scope`（以及 `guardian_socket` / `daemon_socket` / `DAEMON_SOCKET_FILE`）。

- [x] **Step 3: 实现**

在 `crates/yi-agent-lifecycle/src/scope.rs` 里，把 `MAX_SOCKET_PATH_BYTES` 常量之后插入：

```rust
/// The daemon's socket file name inside a socket directory.
pub const DAEMON_SOCKET_FILE: &str = "runtime.sock";

/// The directory holding both sockets for one runtime.
///
/// Always `<instance_home>/sockets/<16 hex>/`, so the path length depends only on
/// where the instance lives — never on how deep the project is. That is the whole
/// point: `sun_path` is 103 bytes here, and a deep checkout used to overflow it.
///
/// Both inputs are canonicalised because a hash over the raw spelling would make
/// one directory produce two sockets: macOS's `/var` and `/private/var` are the
/// same directory, and the launcher and the guardian do not always name it the
/// same way. Two spellings, two socket directories, guardian binds one and every
/// client polls the other — the exact timeout this module exists to remove.
pub fn socket_directory(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    canonical_or_self(instance_home)
        .join("sockets")
        .join(digest16(runtime_dir))
}

/// The guardian's control socket under the shared socket directory.
pub fn guardian_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    socket_directory(runtime_dir, instance_home).join(crate::GUARDIAN_SOCKET_FILE)
}

/// The business daemon's socket under the shared socket directory.
pub fn daemon_socket(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
    socket_directory(runtime_dir, instance_home).join(DAEMON_SOCKET_FILE)
}

/// Sixteen hex characters of the canonical path's SHA-256.
fn digest16(path: &Path) -> String {
    let digest = Sha256::digest(canonical_or_self(path).as_os_str().as_encoded_bytes());
    digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

/// Canonical when possible; the original path otherwise (the directory may not
/// exist yet on a first start, which is not an error here).
fn canonical_or_self(path: &Path) -> PathBuf {
    canonicalize_allowing_missing(path).unwrap_or_else(|_| path.to_path_buf())
}
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib scope::tests
```

Expected: PASS（4 个新用例 + 原有用例全绿）。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-lifecycle/src/scope.rs
git commit -m "feat(lifecycle): derive sockets from a depth-independent directory"
```

---

### Task 2: `instance_home_for_sockets` 与 `.yi-agent` 归一化

**Files:**
- Modify: `crates/yi-agent-lifecycle/src/scope.rs`（紧接 Task 1 的 `canonical_or_self` 之后）
- Test: `crates/yi-agent-lifecycle/src/scope.rs`（`mod tests`）

**Interfaces:**
- Consumes: `std::env::{var_os, temp_dir}`，`std::ffi::OsStr`。
- Produces: `pub fn instance_home_for_sockets() -> Option<PathBuf>`

**背景（必读）：** `store::paths::instance_home()`（`paths.rs:20`）在 `YI_AGENT_HOME=/foo` 时返回 `/foo/.yi-agent`（自动补 `.yi-agent` 后缀）。`lifecycle` 不依赖 `store`，所以这段归一化必须在这里复刻一遍，语义逐字一致，否则两侧算出不同目录。`store` 侧的对应测试在 `paths.rs` 的 `mod tests`（`a_bare_override_gains_the_dot_yi_agent_suffix` 等），可对照阅读。

- [x] **Step 1: 写失败测试**

```rust
    #[test]
    fn a_bare_yi_agent_home_gains_the_dot_yi_agent_suffix() {
        // 必须与 `store::paths::instance_home()` 同义：`/foo` -> `/foo/.yi-agent`。
        assert_eq!(
            instance_home_from_env(Some(OsStr::new("/tmp/dev-a")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_instance_home_already_ending_in_dot_yi_agent_is_kept_verbatim() {
        assert_eq!(
            instance_home_from_env(Some(OsStr::new("/tmp/dev-a/.yi-agent")), None),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn no_override_and_no_home_yields_none() {
        assert_eq!(instance_home_from_env(None, None), None);
    }

    #[test]
    fn an_empty_override_falls_through_to_home() {
        assert_eq!(
            instance_home_from_env(Some(OsStr::new("")), Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.yi-agent"))
        );
    }
```

（`instance_home_from_env` 先做成可注入的纯函数，再让 `instance_home_for_sockets` 包一层真实 env —— 这样测试不需要 `set_var`，也就没有 edition 2024 的 `unsafe` 与并发问题。）

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib scope::tests
```

Expected: 编译失败，`cannot find function instance_home_from_env`。

- [x] **Step 3: 实现**

```rust
/// The instance home a socket path should be derived from.
///
/// Priority: `YI_AGENT_INSTANCE_HOME` (a process that was told which instance it
/// serves), then `YI_AGENT_HOME`, then `$HOME/.yi-agent`.
///
/// Why not just `YI_AGENT_HOME`: a guardian spawns its daemon with
/// `YI_AGENT_INSTANCE_HOME` set explicitly and `YI_AGENT_HOME` only inherited. If
/// the two ever disagreed, reading only `YI_AGENT_HOME` would put the daemon's
/// socket somewhere the frontend never looks.
pub fn instance_home_for_sockets() -> Option<PathBuf> {
    let instance = std::env::var_os("YI_AGENT_INSTANCE_HOME").filter(|v| !v.is_empty());
    if let Some(dir) = instance {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty());
    instance_home_from_env(override_value.as_deref(), home.as_deref())
}

/// `YI_AGENT_HOME` / `$HOME` -> instance home, including the `.yi-agent` suffix
/// normalisation.
///
/// This must stay semantically identical to `store::paths::instance_home()`
/// (`paths.rs:20`). It is duplicated here because `lifecycle` cannot depend on
/// `store` (the dependency runs the other way). A test in `store` pins the two
/// together, which is why this is `#[doc(hidden)] pub` rather than private —
/// Task 8 relies on that.
#[doc(hidden)]
pub fn instance_home_from_env(
    override_value: Option<&OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_value {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() {
            return Some(if dir.file_name() == Some(OsStr::new(".yi-agent")) {
                dir
            } else {
                dir.join(".yi-agent")
            });
        }
    }
    home.map(|home| home.join(".yi-agent"))
}
```

同时把 `use std::path::{Component, Path, PathBuf};` 附近补上 `use std::ffi::OsStr;`。

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib scope::tests
```

Expected: PASS。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-lifecycle/src/scope.rs
git commit -m "feat(lifecycle): resolve the instance home for socket paths"
```

---

### Task 3: `control_path` 改指新布局，删掉 `$TMPDIR` 兜底

**Files:**
- Modify: `crates/yi-agent-lifecycle/src/scope.rs:190-232`（`control_path` / `control_socket` 及其文档注释）
- Test: `crates/yi-agent-lifecycle/src/scope.rs`（`mod tests`）
- Test: `crates/yi-agent-store/src/ipc/client.rs`（后续 Task 5 处理，本任务先只看 lifecycle 侧）

**Interfaces:**
- Consumes: Task 1 的 `guardian_socket`，Task 2 的 `instance_home_for_sockets`。
- Produces: 签名不变的 `pub fn control_path(runtime_dir: &Path) -> PathBuf`、`pub fn control_socket(runtime_dir: &Path) -> Result<PathBuf, SocketPathTooLong>`。

- [x] **Step 1: 写失败测试**

把 `scope.rs` 的 `mod tests` 里这三条**替换**掉（它们断言的是旧布局）：

- `a_short_runtime_directory_keeps_the_control_socket_inside_it`
- `a_deep_runtime_directory_yields_a_control_socket_that_fits_the_limit`
- `the_runtime_directory_defaults_under_the_project` 里的 `assert_eq!(control_path(...), ...)` 那一段

替换为：

```rust
    #[test]
    fn the_control_socket_lives_in_the_shared_socket_directory() {
        let runtime = Path::new("/tmp/project/.yi-agent/runtime");
        let home = Path::new("/home/alice/.yi-agent");

        let socket = control_path_with_home(runtime, home);

        assert_eq!(socket, guardian_socket(runtime, home));
        assert!(
            socket.starts_with(home.join("sockets")),
            "must not follow the project directory any more: {}",
            socket.display()
        );
    }

    #[test]
    fn a_deep_runtime_directory_does_not_lengthen_the_control_socket() {
        let home = Path::new("/home/alice/.yi-agent");
        let shallow = control_path_with_home(Path::new("/tmp/p"), home);
        let deep = control_path_with_home(Path::new(&format!("/tmp/{}", "x".repeat(600))), home);

        assert_eq!(
            shallow.as_os_str().len(),
            deep.as_os_str().len(),
            "length must depend only on instance_home, never on project depth"
        );
    }

    #[test]
    fn the_control_socket_is_actually_bindable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let runtime = dir.path().join("project/.yi-agent/runtime");
        std::fs::create_dir_all(&runtime).unwrap();

        let socket = control_path_with_home(&runtime, home);
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();

        let listener = std::os::unix::net::UnixListener::bind(&socket)
            .unwrap_or_else(|error| panic!("binding {}: {error}", socket.display()));
        drop(listener);
    }

    #[test]
    fn an_over_long_instance_home_is_refused_not_redirected() {
        let runtime = Path::new("/tmp/project/.yi-agent/runtime");
        let home = PathBuf::from(format!("/{}", "h".repeat(80)));

        let refused = control_socket_with_home(runtime, &home);

        assert!(
            refused.is_err(),
            "a path that cannot fit must be an error, not a silent move to $TMPDIR"
        );
        let message = refused.unwrap_err().to_string();
        assert!(!message.contains("falling back to the temp"), "stale wording: {message}");
        assert!(!message.contains("TMPDIR"), "stale wording: {message}");
        assert!(message.contains("103"), "must state the limit: {message}");
    }
```

（`control_path_with_home` / `control_socket_with_home` 是 `#[cfg(test)]` 的薄封装，见 Step 3。）

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib scope::tests
```

Expected: 编译失败，`cannot find function control_path_with_home`。

- [x] **Step 3: 实现**

把 `control_path` 与 `control_socket` 整段（含文档注释）替换为：

```rust
/// The guardian's control socket for a runtime directory.
///
/// This is the **only** place the path is computed. Every component — the guardian
/// that binds it, the frontends that lease from it, the desktop shell that reads
/// status through it — must resolve it here, because a disagreement means one side
/// listens where nobody connects.
///
/// The path is `<instance_home>/sockets/<hash>/guardian-control.sock`, so its length
/// depends on where the instance lives and never on how deep the project is. The
/// previous layout put it beside the runtime directory, which overflowed
/// `sun_path` (103 bytes here) on a deep checkout; the guardian's stderr is
/// discarded, so that failure surfaced only as a 30-second stall followed by
/// delegation being switched off.
///
/// Callers that do not know the instance home (most of them) get it from
/// [`instance_home_for_sockets`]. Callers that *do* know it — the guardian, which
/// carries a `RuntimeScope` — should call [`guardian_socket`] directly instead.
pub fn control_path(runtime_dir: &Path) -> PathBuf {
    let home = instance_home_for_sockets().unwrap_or_else(|| {
        // Diagnostic placeholder only. All three sources missing is unreachable on a
        // normal host (launchd, containers and SSH all set HOME). A path that cannot
        // exist fails `bind` loudly instead of quietly landing somewhere unexpected.
        //
        // `eprintln!` rather than `tracing`: `lifecycle` deliberately carries no
        // logging stack (it is linked by the desktop shell and must stay light).
        eprintln!(
            "yi-agent: instance home is unknown (no YI_AGENT_INSTANCE_HOME, \
             YI_AGENT_HOME or HOME); the socket path is a placeholder"
        );
        PathBuf::from("<unknown-instance-home>")
    });
    guardian_socket(runtime_dir, &home)
}

/// [`control_path`], but refusing when the derived path cannot be bound.
///
/// Callers that bind — in practice the guardian — use this so an over-long instance
/// home is reported instead of inferred from a timeout.
pub fn control_socket(runtime_dir: &Path) -> Result<PathBuf, SocketPathTooLong> {
    reject_if_too_long(control_path(runtime_dir))
}

fn reject_if_too_long(path: PathBuf) -> Result<PathBuf, SocketPathTooLong> {
    if path.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: path.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(path)
}
```

在 `mod tests` 开头加两个测试用薄封装：

```rust
    fn control_path_with_home(runtime_dir: &Path, instance_home: &Path) -> PathBuf {
        guardian_socket(runtime_dir, instance_home)
    }

    fn control_socket_with_home(
        runtime_dir: &Path,
        instance_home: &Path,
    ) -> Result<PathBuf, SocketPathTooLong> {
        reject_if_too_long(guardian_socket(runtime_dir, instance_home))
    }
```

并把 `SocketPathTooLong` 的 `Display`（`scope.rs:241-248` 附近）改成不带「兜底」字样：

```rust
impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the guardian control socket would be {} bytes, over the {}-byte sun_path \
             limit: {} -- shorten the instance home (YI_AGENT_HOME)",
            self.path.len(),
            self.limit,
            self.path
        )
    }
}
```

同时把该 struct 的 doc comment（`scope.rs:231-234`，写着 "Reachable only when `TMPDIR` itself is absurdly deep"）改为：

```rust
/// A socket path longer than `sockaddr_un.sun_path` accepts.
///
/// Reachable only when the instance home itself is very deep — a project's depth no
/// longer contributes, so this reports the one input a user can actually change.
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-lifecycle --lib
```

Expected: PASS。若 `a_short_runtime_directory_keeps_the_control_socket_inside_it` 仍残留，会红 —— 那是本步骤要替换掉的那条，确认已删除。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-lifecycle/src/scope.rs
git commit -m "refactor(lifecycle): derive the control socket from the instance home"
```

---

### Task 4: `socket_path_for` 改指新布局，删 `socket_file_name`

**Files:**
- Modify: `crates/yi-agent-store/src/ipc/client.rs:60-96`
- Modify: `crates/yi-agent-store/src/ipc/error.rs:33-38`
- Test: `crates/yi-agent-store/src/ipc/client.rs`（`mod tests`）

**Interfaces:**
- Consumes: `yi_agent_lifecycle::scope::{daemon_socket, instance_home_for_sockets}`（Task 1、2）。
- Produces: 签名不变的 `pub fn socket_path_for(runtime_dir: &Path) -> Result<PathBuf, IpcError>`；新增 `IpcError::InstanceHomeUnavailable`。

- [x] **Step 1: 写失败测试**

在 `crates/yi-agent-store/src/ipc/client.rs` 的 `mod tests` 里，**替换**这两条（旧布局语义已失效）：

- `a_short_runtime_directory_keeps_the_socket_inside_it`
- `a_long_runtime_directory_yields_a_socket_that_fits_the_platform_limit`

替换为：

```rust
    #[test]
    fn the_socket_lives_in_the_shared_directory_not_the_runtime_directory() {
        let runtime_dir = Path::new("/tmp/project/.yi-agent/runtime");

        let socket = socket_path_for(runtime_dir).expect("resolves");

        assert!(
            socket.to_string_lossy().contains("/sockets/"),
            "socket must move to the shared directory: {}",
            socket.display()
        );
        assert!(
            !socket.starts_with(runtime_dir),
            "socket must no longer sit inside the runtime directory: {}",
            socket.display()
        );
        assert_eq!(socket.file_name().unwrap(), "runtime.sock");
    }

    #[test]
    fn a_deep_runtime_directory_does_not_lengthen_the_socket() {
        let shallow = socket_path_for(Path::new("/tmp/p/.yi-agent/runtime")).expect("resolves");
        let deep =
            socket_path_for(Path::new(&format!("/tmp/{}", "x".repeat(600)))).expect("resolves");

        assert_eq!(
            shallow.as_os_str().len(),
            deep.as_os_str().len(),
            "length must depend only on the instance home"
        );
        assert!(deep.as_os_str().len() <= MAX_SOCKET_PATH_BYTES);
    }
```

并**删除** `the_fallback_is_deterministic_for_one_runtime_directory` 里对 fallback 措辞的引用（断言本身保留：同一 runtime 两次解析相等，仍然成立）。

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib ipc::client::tests
```

Expected: 失败，`assertion failed: socket.to_string_lossy().contains("/sockets/")`（当前实现仍返回 `<runtime_dir>/runtime.sock`）。

- [x] **Step 3: 实现**

把 `client.rs:64-99`（`socket_path_for` 与 `socket_file_name`）整体替换为：

```rust
/// The business daemon's socket for a runtime directory.
///
/// `<instance_home>/sockets/<hash>/runtime.sock` — the length depends on where the
/// instance lives, never on how deep the project is. The previous layout put it
/// beside the runtime directory, which overflowed `sun_path` on a deep checkout.
///
/// The instance home is resolved from the environment because most callers only
/// hold a `runtime_dir`. A caller that already knows it should pass it explicitly
/// rather than relying on this.
pub fn socket_path_for(runtime_dir: &Path) -> Result<PathBuf, IpcError> {
    let home = yi_agent_lifecycle::scope::instance_home_for_sockets()
        .ok_or(IpcError::InstanceHomeUnavailable)?;
    let socket = yi_agent_lifecycle::scope::daemon_socket(runtime_dir, &home);
    if socket.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(IpcError::SocketPathTooLong {
            path: socket.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(socket)
}
```

删掉 `socket_file_name` 整个函数，以及 `Sha256` / `Digest` 相关的 `use`（若没有其它使用者，`cargo` 会提示未使用）。

在 `crates/yi-agent-store/src/ipc/error.rs` 的 `IpcError` 里，`SocketPathTooLong` 之后加：

```rust
    /// No `YI_AGENT_INSTANCE_HOME`, `YI_AGENT_HOME` or `HOME` is set, so the socket
    /// directory cannot be derived. Reported instead of falling back to a temp
    /// directory: a socket nobody can find is worse than a clear error.
    #[error("cannot derive the instance home: none of YI_AGENT_INSTANCE_HOME, YI_AGENT_HOME or HOME is set")]
    InstanceHomeUnavailable,
```

并把 `SocketPathTooLong` 的文案改为：

```rust
    #[error(
        "socket path {path} exceeds this platform's {limit}-byte limit; \
         shorten the instance home (YI_AGENT_HOME)"
    )]
    SocketPathTooLong { path: String, limit: usize },
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib
```

Expected: PASS。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-store/src/ipc/client.rs crates/yi-agent-store/src/ipc/error.rs
git commit -m "refactor(store): move the daemon socket under the instance home"
```

---

### Task 5: daemon 建目录、设权限、退出时删空目录

**Files:**
- Modify: `crates/yi-agent-store/src/ipc/daemon.rs:292-300`（启动）
- Modify: `crates/yi-agent-store/src/ipc/daemon.rs:520-528`（退出清理）
- Test: `crates/yi-agent-store/tests/runtime_ipc.rs`

**Interfaces:**
- Consumes: `socket_path_for`（Task 4）。
- Produces: 无新公开接口；行为变更：`sockets/<hash>/` 自动创建为 `0700`，退出时若为空则删除。

- [x] **Step 1: 写失败测试**

在 `crates/yi-agent-store/tests/runtime_ipc.rs` 追加（与既有用例风格一致，用 `TempDir`）：

```rust
#[test]
fn the_socket_directory_is_created_with_owner_only_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let daemon = Daemon::start(&runtime, &database).unwrap();

    let socket = daemon.socket_path();
    let socket_dir = socket.parent().expect("socket has a directory");

    let mode = std::fs::metadata(socket_dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "socket directory must be owner-only");

    // The runtime directory is still created too: the daemon watches it to notice
    // that its project was deleted, and that check is independent of the socket.
    assert!(runtime.exists(), "the runtime directory is still needed");
}

#[test]
fn the_socket_directory_is_removed_on_shutdown_when_empty() {
    let directory = tempfile::TempDir::new().unwrap();
    let runtime = directory.path().join("runtime");
    let database = directory.path().join("runtime.sqlite");
    let mut daemon = Daemon::start(&runtime, &database).unwrap();

    let socket_dir = daemon.socket_path().parent().unwrap().to_path_buf();
    assert!(socket_dir.exists(), "created on start");

    daemon.stop().unwrap();
    // 给监听线程一个收尾窗口，实现里是 join 完再删，这里留余量
    std::thread::sleep(std::time::Duration::from_millis(200));

    assert!(
        !socket_dir.exists(),
        "an empty socket directory left behind turns into garbage in the developer's home: {}",
        socket_dir.display()
    );
}
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc the_socket_directory
```

Expected: 第一条约 0o755 而非 0o700 或目录不存在（取决于 umask）；第二条目录仍存在。

- [x] **Step 3: 实现**

`crates/yi-agent-store/src/ipc/daemon.rs` 的 `start` 里，紧接原有的
`let socket_path = socket_path_for(runtime_dir)?;`（`:297`）之后插入下面这段
（不要重复声明 `socket_path`）：

```rust
        // The socket no longer lives beside the runtime directory, so its own
        // directory has to be created here — and locked down, because it is under
        // the instance home rather than inside the project.
        if let Some(socket_dir) = socket_path.parent() {
            fs::create_dir_all(socket_dir)?;
            fs::set_permissions(socket_dir, fs::Permissions::from_mode(0o700))?;
        }
```

在退出清理处（`remove_socket_if_same_inode(&cleanup_socket, cleanup_socket_identity);` 之后）加：

```rust
            remove_socket_if_same_inode(&cleanup_socket, cleanup_socket_identity);
            // Drop the directory too when nothing else is using it. The guardian may
            // share this directory, in which case `remove_dir` fails with ENOTEMPTY
            // and that is the right outcome -- never remove a directory with a live
            // socket in it.
            if let Some(dir) = cleanup_socket.parent() {
                let _ = std::fs::remove_dir(dir);
            }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_ipc
```

Expected: PASS（含新加两条与既有全部用例）。

若既有用例 `a_daemon_stops_once_its_runtime_directory_is_gone`（`:2893`）或 `:2981` 的 `!runtime.join("runtime.sock").exists()` 变红，说明断言指向旧路径——把它们改成 `!daemon_socket(...)` 的等价断言（`runtime.sock` 现在不在 `runtime` 里，该断言恒真，改成对 `socket_path()` 的断言才有意义）。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-store/src/ipc/daemon.rs crates/yi-agent-store/tests/runtime_ipc.rs
git commit -m "feat(store): create and reap the daemon socket directory"
```

---

### Task 6: guardian 用 `scope.instance_home` 绑定

**Files:**
- Modify: `crates/yi-agent-supervisors/src/lifecycle/guardian.rs:2130-2200`（`bind_control`）

**Interfaces:**
- Consumes: `lifecycle::scope::{guardian_socket, MAX_SOCKET_PATH_BYTES}`（Task 1），`RuntimeScope.instance_home`（已有）。
- Produces: 无签名变更；`bind_control` 不再经过 `control_path`、不读环境变量。

- [x] **Step 1: 写失败测试**

在 `crates/yi-agent-supervisors/tests/managed_bringup.rs` 追加：

```rust
#[test]
fn the_guardian_binds_under_the_scope_instance_home() {
    let directory = tempfile::TempDir::new().unwrap();
    let instance_home = directory.path().join("instance");
    std::fs::create_dir_all(&instance_home).unwrap();
    let runtime_dir = directory.path().join("project/.yi-agent/runtime");
    std::fs::create_dir_all(&runtime_dir).unwrap();

    let scope = yi_agent_lifecycle::scope::resolve(&runtime_dir, directory.path(), &instance_home)
        .unwrap();
    let expected = yi_agent_lifecycle::scope::guardian_socket(&scope.runtime_dir, &scope.instance_home);

    // Binding is what proves the path is usable; the length assertion only proves
    // the arithmetic. Both matter, but bind is the regression that was actually hit.
    let listener = std::os::unix::net::UnixListener::bind(&expected)
        .unwrap_or_else(|error| panic!("binding {}: {error}", expected.display()));
    drop(listener);

    assert!(
        expected.starts_with(instance_home.join("sockets")),
        "socket must live under the scope's instance home: {}",
        expected.display()
    );
}
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-supervisors --test managed_bringup the_guardian_binds
```

Expected: 失败——`expected` 的父目录 `sockets/<hash>/` 不存在，`bind` 报 `No such file or directory`（这正是 spec §3.7 指出的「新布局比旧故障更糟」的风险点）。

- [x] **Step 3: 实现**

把 `bind_control` 的开头（`use std::os::unix::fs::FileTypeExt;` 到 `std::fs::create_dir_all(&self.scope.runtime_dir)?;`）改为：

```rust
        use std::os::unix::fs::FileTypeExt;
        // Derived from the scope's own instance home, not from the environment: this
        // process was told which instance it serves (`YI_AGENT_INSTANCE_HOME`), and
        // re-deriving it from a different variable is how the two sides would end up
        // binding and polling different paths.
        let path = yi_agent_lifecycle::scope::guardian_socket(
            &self.scope.runtime_dir,
            &self.scope.instance_home,
        );
        if path.as_os_str().len() > yi_agent_lifecycle::scope::MAX_SOCKET_PATH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "the guardian control socket would be {} bytes, over the {}-byte \
                     limit: {} -- shorten the instance home (YI_AGENT_HOME)",
                    path.as_os_str().len(),
                    yi_agent_lifecycle::scope::MAX_SOCKET_PATH_BYTES,
                    path.display()
                ),
            ));
        }
        std::fs::create_dir_all(&self.scope.runtime_dir)?;
        // The socket is no longer inside the runtime directory, so its directory has
        // to be created here. Owner-only: it sits under the instance home.
        if let Some(socket_dir) = path.parent() {
            std::fs::create_dir_all(socket_dir)?;
            std::fs::set_permissions(
                socket_dir,
                std::os::unix::fs::PermissionsExt::from_mode(0o700),
            )?;
        }
```

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-supervisors
```

Expected: PASS，含 `managed_bringup.rs:217` 的既有断言（文件名不变，应仍绿）。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-supervisors/src/lifecycle/guardian.rs crates/yi-agent-supervisors/tests/managed_bringup.rs
git commit -m "feat(supervisors): bind the control socket under the instance home"
```

---

### Task 7: `attach_to_answer` 用上它已有的 `instance_home`

**Files:**
- Modify: `crates/yi-agent-subagent/src/attach.rs:1344-1372`（两个调用方）、`:1380-1395`、`:1421`
- Test: `crates/yi-agent-subagent/src/attach.rs`（`mod tests`）

**Interfaces:**
- Consumes: `lifecycle::scope::daemon_socket`（Task 1）、`store::paths`/`lifecycle::scope::instance_home_for_sockets`（Task 2）。
- Produces: `attach_to_answer` 不再丢弃参数；`attach_managed_project_runtime` 传 `Some(instance_home)`。

- [x] **Step 1: 写失败测试**

在 `attach.rs` 的 `mod tests` 里加（若该模块尚无 `mod tests`，在文件末尾新建）：

```rust
    #[test]
    fn the_attach_socket_follows_the_supplied_instance_home_not_the_environment() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("project/.yi-agent/runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let explicit = dir.path().join("instance-a");
        std::fs::create_dir_all(&explicit).unwrap();

        let from_arg = attach_socket_path(&runtime, Some(&explicit));

        let expected = yi_agent_lifecycle::scope::daemon_socket(&runtime, &explicit);
        assert_eq!(
            from_arg, expected,
            "an explicit instance home must win over whatever the environment says"
        );
    }
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib attach::tests
```

Expected: 编译失败，`cannot find function attach_socket_path`。

- [x] **Step 3: 实现**

在 `attach.rs`（`attach_to_answer` 之前）加一个小函数，作为唯一的路径决策点：

```rust
/// The daemon socket for an attach, preferring a caller-supplied instance home.
///
/// The managed path already knows which instance it serves, so it must not go back
/// to the environment for the answer: `YI_AGENT_INSTANCE_HOME` and `YI_AGENT_HOME`
/// can disagree, and then the daemon's socket would be somewhere the caller never
/// looked. Only when the caller genuinely has no value (`attach_authorized_root`
/// holds a capability, not an instance) do we fall back to the environment.
fn attach_socket_path(
    runtime_dir: &Path,
    instance_home: Option<&Path>,
) -> Result<PathBuf, AttachFailure> {
    let home = match instance_home {
        Some(path) => path.to_path_buf(),
        // `IpcError` is already imported in this module (`:12`).
        None => yi_agent_lifecycle::scope::instance_home_for_sockets()
            .ok_or(IpcError::InstanceHomeUnavailable)
            .map_err(|error| AttachFailure::new("runtime directory", error))?,
    };
    Ok(yi_agent_lifecycle::scope::daemon_socket(runtime_dir, &home))
}
```

把 `attach_to_answer` 里

```rust
    let socket_path = yi_agent_store::ipc::socket_path_for(runtime_dir)
        .map_err(|error| AttachFailure::new("runtime directory", error))?;
```

改成

```rust
    let socket_path = attach_socket_path(runtime_dir, instance_home)?;
```

并删掉 `let _ = instance_home;`（`:1421`）。

把 `attach_managed_project_runtime`（`:1371`）里传的 `None` 改成 `Some(instance_home)`。

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent
```

Expected: PASS。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-subagent/src/attach.rs
git commit -m "fix(subagent): use the instance home the attach already carries"
```

---

### Task 8: 跨进程一致性测试与收尾

**Files:**
- Test: `crates/yi-agent-lifecycle/src/scope.rs`（`mod tests`）
- Test: `crates/yi-agent-store/src/paths.rs`（`mod tests`，等价性断言）

**Interfaces:**
- Consumes: Task 2 的 `instance_home_for_sockets` / `instance_home_from_env`。
- Produces: 无新接口，只加断言。

- [x] **Step 1: 写测试**

`crates/yi-agent-store/src/paths.rs` 的 `mod tests` 里加（钉住两处归一化逐字等价）：

```rust
    /// `lifecycle::scope` 复刻了一份 `.yi-agent` 归一化（它不能依赖本 crate）。
    /// 这条断言让两份实现不会漂移。
    #[test]
    fn the_lifecycle_copy_of_the_instance_home_rule_agrees_with_this_one() {
        for (override_value, home) in [
            (Some(OsStr::new("/tmp/dev-a")), Some(Path::new("/Users/me"))),
            (Some(OsStr::new("/tmp/dev-a/.yi-agent")), Some(Path::new("/Users/me"))),
            (Some(OsStr::new("")), Some(Path::new("/Users/me"))),
            (None, Some(Path::new("/Users/me"))),
            (None, None),
        ] {
            assert_eq!(
                from(override_value, home),
                yi_agent_lifecycle::scope::instance_home_from_env(override_value, home),
                "override={override_value:?} home={home:?}"
            );
        }
    }
```

（`instance_home_from_env` 在 Task 2 里已经是 `#[doc(hidden)] pub`，这里不需要再改可见性。）

- [x] **Step 2: 跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib paths::tests && cargo test -p yi-agent-lifecycle --lib
```

Expected: PASS。

- [x] **Step 3: 全仓检查**

```bash
cd yi-agent-rs && cargo fmt --all && just ci
```

Expected: 16 道门禁全绿。若 `secret-check` / `msrv-check` 之外有红，按输出逐条修。

- [x] **Step 4: 真机验收**

```bash
cargo build -p yi-agent 2>&1 | tail -3
ls -la ~/.yi-agent/sockets/ 2>/dev/null
```

启动 yi-agent 后确认 `~/.yi-agent/sockets/<16hex>/` 里出现 `guardian-control.sock` 与 `runtime.sock`，`/agents` 可用。

- [x] **Step 5: Commit**

```bash
cd yi-agent-rs && git add -A
git commit -m "test: pin the two instance-home implementations together"
```

---

## Self-Review

**1. Spec coverage**

| Spec 章节 | 对应 task |
|---|---|
| §3.1 单一真相源（`socket_directory` / 两个构造函数 / 双入参规范化） | Task 1 |
| §3.2 删 `$TMPDIR` 兜底、56 字节阈值、删 `socket_file_name` | Task 3、4 |
| §3.3 `control_path` 签名不变、占位路径处理 | Task 3 |
| §3.4 `socket_path_for` 签名不变、`InstanceHomeUnavailable` | Task 4 |
| §3.5 跨进程一致（情况 A guardian 用 scope、情况 B env 优先级、`.yi-agent` 归一化） | Task 2、6、8 |
| §3.5.1 `attach.rs` 不再丢 `instance_home` | Task 7 |
| §3.6 测试侧影响（空目录清理） | Task 5 |
| §3.7 目录创建与 0700 | Task 5、6 |
| §3.8 旧位置清理（不做） | 不实现，符合 spec |
| §4 错误处理（两处文案） | Task 3（`SocketPathTooLong`）、Task 4（`InstanceHomeUnavailable` + 文案） |
| §5 第 1-12 条 | Task 1（1、2、4、6）、Task 2（7）、Task 3（5、9）、Task 5（8、10）、Task 6（3 的一部分）、Task 8（11、12） |
| §6 风险表 | 由各 task 的实现与测试覆盖 |
| §6.1 四个既有用例 | Task 3（2 条）、Task 4（2 条）、Task 5（`runtime_ipc.rs:2981`）、Task 6（`managed_bringup.rs:217`） |
| §8 完成判据 | Task 8 Step 3-4 |

缺口：§5 第 3 条要求「launcher / guardian / admission / thread_root / frontend_lease 五处两两相等」。Task 6 只验证了 guardian 一处。补一个 Task 9。

**2. Placeholder scan**：无 TBD / TODO / 「类似 Task N」。

**3. Type consistency**：`socket_directory` / `guardian_socket` / `daemon_socket` 三处签名均为 `(&Path, &Path) -> PathBuf`，Task 1 定义、Task 3/4/6/7 使用一致。`instance_home_for_sockets() -> Option<PathBuf>` 在 Task 2 定义，Task 3/4/7 均按 `Option` 处理。`IpcError::InstanceHomeUnavailable` 在 Task 4 定义，Task 7 使用同名。

---

### Task 9: 五处调用点同源断言

**Files:**
- Test: `crates/yi-agent-supervisors/tests/managed_bringup.rs` 或新建 `crates/yi-agent-supervisors/tests/socket_agreement.rs`

**Interfaces:**
- Consumes: Task 1 的 `guardian_socket`；各处既有入口 `control_path`。

- [x] **Step 1: 写测试**

新建 `crates/yi-agent-supervisors/tests/socket_agreement.rs`：

```rust
//! 回归：所有解析控制 socket 的地方必须算出同一个路径。
//!
//! 分成多个 crate 的调用方（app-server、subagent、前端）各自持有 `runtime_dir`，
//! 一旦有人绕过 `control_path` 直接 join，就会出现「guardian 绑一个、客户端连
//! 另一个」——症状正是 30 秒超时。

use std::path::Path;

#[test]
fn every_resolver_agrees_on_one_control_socket() {
    let runtime_dir = Path::new("/tmp/agreement-project/.yi-agent/runtime");
    let home = Path::new("/home/alice/.yi-agent");

    let canonical = yi_agent_lifecycle::scope::guardian_socket(runtime_dir, home);

    // 1. supervisors 的门面（`bringup::control_path` 走 `RuntimeScope`）。
    let project = Path::new("/tmp/agreement-project");
    let scope = yi_agent_lifecycle::scope::resolve(runtime_dir, project, home).unwrap();
    assert_eq!(
        yi_agent_supervisors::lifecycle::bringup::control_path(&scope),
        canonical
    );

    // 2. admission / legacy_evidence / runtime.rs / thread_root / frontend_lease 都是
    //    薄转发，最终落在 `scope::control_path`。它读环境变量，所以只断言"形状一致"，
    //    不比对具体哈希 —— 比对具体值要改进程环境，那在 edition 2024 下是 unsafe，
    //    且会和并行用例互相干扰。
    let via_env = yi_agent_lifecycle::scope::control_path(runtime_dir);
    assert_eq!(via_env.file_name(), canonical.file_name());
    assert_eq!(
        via_env.parent().unwrap().parent().unwrap().file_name(),
        Some(std::ffi::OsStr::new("sockets")),
        "every resolver must agree on the socket shape: {}",
        via_env.display()
    );
}
```

说明：`bringup::control_path(&scope)` 与 `scope::control_path(runtime_dir)` 只差在
instance_home 的来源（前者用 scope，后者读 env）。用 scope 的那条是 guardian 实际走
的路，也是这次的真回归点，所以精确比对；走 env 的那条只比对形状，避免用例依赖宿主
的 `YI_AGENT_HOME`。

- [x] **Step 2: 跑测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-supervisors --test socket_agreement
```

Expected: PASS。

- [x] **Step 3: Commit**

```bash
cd yi-agent-rs && cargo fmt --all && git add crates/yi-agent-supervisors/tests/socket_agreement.rs
git commit -m "test(supervisors): pin every control-socket resolver together"
```
