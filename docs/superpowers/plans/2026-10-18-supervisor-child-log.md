# 被托管子进程的日志落盘 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让被托管子进程的 stderr 与 supervisor 自己的生命周期事件落到 `<workdir>/.yi-agent/logs/<name>.log`，结束"系统在报告、报告被丢进 `/dev/null`"的静默失效。

**Architecture:** 只改 `yi-agent-supervisors`（通用 crate，不认识具体插件）。新增私有 `ChildLog`（`Option<File>`，写失败即降级为不写），由 `RunningChild` 持有；`spawn` 时把子进程 stderr 从 `Stdio::null()` 换成 `Stdio::from(文件句柄)`，并在 `spawn_and_track` / `ensure_running` / `reconcile` / `stop` / `stop_all` 处写入生命周期事件。路径由 `Layout.workdir` + `manifest.name` 机械推导，**不新增清单字段**。

**Tech Stack:** Rust 1.85（edition 2024）、`std::process::Stdio`、`std::fs::OpenOptions`、新增 `chrono = { workspace = true }`（workspace 已 pin，`yi-agent-boards/src/registry.rs:62` 已在用）。

**Spec:** `docs/superpowers/specs/2026-10-18-supervisor-child-log-design.md`

## Global Constraints

- **best-effort 硬约束**（spec §3.2）：日志目录建不出、文件开不了，一律回落到 `Stdio::null()`，**绝不阻断 spawn**。守护子进程是本职，记日志是附加。
- **只重定向 stderr**（spec §2.2）：`stdout` 保持 `Stdio::null()`。核实依据——`run` 长驻路径全程 `eprintln!`，唯一的 `println!`（`main.rs:820`）是一次性命令的结果通道。
- **append 而非 truncate**（spec §3.2）：保留上一次崩溃的遗言。
- **必须区分「清单消失」与「开关关闭」**（spec §3.3）：二者在 `reconcile` 走不同分支，日志不得混成一句。
- **不加清单字段**（spec §2.2）：路径由 `manifest.name` 推导，`SupervisorManifest` 的 schema 零变化。
- **不改** wire 契约、协议文档、前端、`yi-agent-runtime` 的 `serve` 逻辑。
- **不做**日志轮转、二进制新鲜度检测、rejected 事件的桌面通知（spec §2.2）。
- **测试纪律**（spec §4）：用 marker 文件 + `wait_until` 有界轮询断言，**不用固定 sleep**；spawn 类测试复用既有 `spawn_serial_guard()`。
- **测试基线**：`cargo test -p yi-agent-supervisors`、`cargo test -p yi-agent-runtime --lib`。
- **不许并行跑 `cargo test --workspace`**（会 OOM）；按 crate 跑，跑前 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认无残留。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下）。
- 提交信息用 conventional commits、中文，**不写** `Co-Authored-By`。
- 本计划在 worktree `.worktrees/supervisor-log`（分支 `fix/supervisor-child-log`）内执行；**严禁**直接提交 `main`。

---

### Task 1: 日志写入器与路径推导

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`（文件顶部 `Layout` impl 附近新增 `logs_dir()`；新增 `ChildLog`）
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml`（`[dependencies]` 加 chrono）
- Test: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`（文件内新增 `mod child_log_tests`）

**Interfaces:**
- Consumes: `Layout`（`supervisor.rs:11`，已有 `workdir` / `state_dir` / `runtime_dir`）。
- Produces:
  - `impl Layout { pub fn logs_dir(&self) -> PathBuf }` —— 返回 `<workdir>/.yi-agent/logs`。
  - `struct ChildLog`（模块私有）+ `impl ChildLog`：
    - `fn open(logs_dir: &Path, name: &str) -> Self`
    - `fn write_line(&mut self, event: &str)`
    - `#[cfg(test)] fn is_active(&self) -> bool`

- [ ] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs` 的**文件末尾**追加：

```rust
#[cfg(test)]
mod child_log_tests {
    use super::*;

    #[test]
    fn the_logs_dir_sits_under_the_workdir() {
        let layout = Layout::for_workdir(Path::new("/tmp/project"));

        assert_eq!(
            layout.logs_dir(),
            PathBuf::from("/tmp/project/.yi-agent/logs"),
            "the log directory is derived from the workdir, not configured"
        );
    }

    #[test]
    fn a_line_is_written_with_a_timestamp_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let logs = dir.path().join("logs");
        let mut log = ChildLog::open(&logs, "demo");

        log.write_line("started demo (pid 42)");

        let text = std::fs::read_to_string(logs.join("demo.log")).unwrap();
        assert!(
            text.ends_with("started demo (pid 42)\n"),
            "the event must be the tail of the line, got: {text:?}"
        );
        // 前缀是 `YYYY-MM-DDTHH:MM:SS `（20 字符 + 空格）。这次事故的核心难点正是
        // "什么时候开始的"，所以时间戳不是装饰。
        assert!(
            text.len() > "started demo (pid 42)\n".len(),
            "the line must carry a timestamp prefix, got: {text:?}"
        );
        let prefix = &text[..text.find(' ').expect("timestamp then a space")];
        assert_eq!(prefix.len(), 19, "expected YYYY-MM-DDTHH:MM:SS, got {prefix:?}");
        assert!(prefix.contains('T'), "date and time must be separated by T");
    }

    #[test]
    fn lines_append_rather_than_truncate() {
        let dir = tempfile::TempDir::new().unwrap();
        let logs = dir.path().join("logs");

        ChildLog::open(&logs, "demo").write_line("first");
        ChildLog::open(&logs, "demo").write_line("second");

        let text = std::fs::read_to_string(logs.join("demo.log")).unwrap();
        assert!(
            text.contains("first") && text.contains("second"),
            "a restart must not erase the previous run's last words, got: {text:?}"
        );
    }

    #[test]
    fn an_unwritable_log_degrades_instead_of_failing() {
        let dir = tempfile::TempDir::new().unwrap();
        // `logs` 是一个**文件**而不是目录：建目录必失败，写入器必须降级。
        let logs = dir.path().join("logs");
        std::fs::write(&logs, "not a directory").unwrap();

        let mut log = ChildLog::open(&logs, "demo");
        assert!(
            !log.is_active(),
            "an unusable log path must degrade to a no-op, not panic"
        );
        log.write_line("this must not panic");
    }
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --lib child_log_tests`

Expected: 编译失败，`cannot find method \`logs_dir\`` / `cannot find type \`ChildLog\``。

- [ ] **Step 3: 加依赖**

在 `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml` 的 `[dependencies]` 段末尾（`sha2 = "0.10"` 之后）加：

```toml
# 日志行的时间戳。workspace 已 pin，`yi-agent-boards` 同样在用；不手抄日历算法。
chrono = { workspace = true }
```

- [ ] **Step 4: 实现 `logs_dir` 与 `ChildLog`**

在 `supervisor.rs` 里 `Layout::manifests_dir`（`:28`）之后加：

```rust
    /// Where supervised children's logs land: one file per manifest name.
    ///
    /// Beside the other project-scoped state rather than in `state_dir`: this
    /// crate supervises whatever the manifests name and knows nothing about any
    /// particular plugin, so it must not write inside a directory whose name is
    /// owned by one.
    pub fn logs_dir(&self) -> PathBuf {
        self.workdir.join(".yi-agent").join("logs")
    }
```

并在 `RunningChild`（`:47`）**之前**加：

```rust
/// A supervised child's log file, or a no-op when the file cannot be opened.
///
/// Best effort on purpose: supervising the child is the job, recording it is a
/// bonus. A log that cannot be written must never stop the child from starting,
/// so every failure degrades to [`Stdio::null`] behaviour instead of propagating.
///
/// Once a write fails the file is dropped and the log goes quiet. `reconcile`
/// runs every 500ms; retrying a doomed write every tick would bury the real
/// failure under its own noise.
struct ChildLog {
    file: Option<std::fs::File>,
}

impl ChildLog {
    /// Opens `<logs_dir>/<name>.log`, creating the directory as needed.
    ///
    /// Appends rather than truncates: the interesting line is usually the last
    /// one the previous incarnation managed to write.
    fn open(logs_dir: &Path, name: &str) -> Self {
        let file = std::fs::create_dir_all(logs_dir)
            .ok()
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(logs_dir.join(format!("{name}.log")))
                    .ok()
            });
        Self { file }
    }

    /// Writes one timestamped line, going quiet if the file is gone.
    fn write_line(&mut self, event: &str) {
        use std::io::Write;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        if writeln!(file, "{stamp} {event}").is_err() {
            self.file = None;
        }
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.file.is_some()
    }
}
```

并在 `supervisor.rs` 顶部 `use` 区（`:1-5`）把 `std::fs::OpenOptions` 引入：

```rust
use std::fs::OpenOptions;
```

- [ ] **Step 5: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --lib child_log_tests`

Expected: PASS（4 条）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml yi-agent-rs/Cargo.lock
git commit -m "feat(supervisors): 新增被托管子进程的日志写入器"
```

---

### Task 2: 把子进程 stderr 接进日志文件

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`（`RunningChild:47`、`spawn:221`、`spawn_and_track:206`）
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs`

**Interfaces:**
- Consumes: Task 1 的 `ChildLog`、`Layout::logs_dir`。
- Produces: `RunningChild` 新增字段 `log: ChildLog`；`spawn` 的签名不变，但 stderr 改道。

- [ ] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs` 中，紧跟 `turning_the_switch_off_stops_a_running_child`（`:197`）之后追加：

```rust
#[test]
fn a_childs_stderr_lands_in_its_log_file() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    // 子进程往 **stderr** 写一个独特标记，另往 stdout 写一个，用来证明只有
    // stderr 被接进日志（stdout 仍被吞掉）。
    let child = workdir.join("noisy.sh");
    std::fs::write(
        &child,
        "#!/bin/sh\nprintf 'child-stdout-marker\\n'\nprintf 'child-stderr-marker\\n' 1>&2\nsleep 30\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
    }
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("child-stderr-marker"))
                .unwrap_or(false),
            "the child's stderr line to reach its log file"
        ),
        "a child's stderr must be recorded, not discarded"
    );

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("child-stdout-marker"),
        "stdout must stay swallowed: only stderr is a reporting channel, got: {text:?}"
    );
    supervisor.stop_all();
}

#[test]
fn the_supervisor_records_when_it_starts_a_child() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("started demo") && text.contains("pid"))
                .unwrap_or(false),
            "the supervisor's start event to reach the log"
        ),
        "the log must record the start, including the pid"
    );
    supervisor.stop_all();
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor a_childs_stderr_lands_in_its_log_file`

Expected: FAIL，`the child's stderr line to reach its log file`（当前 stderr 进 `/dev/null`，文件里没有该标记）。

- [ ] **Step 3: 实现**

**(a)** 给 `RunningChild`（`:47`）加字段：

```rust
struct RunningChild {
    child: std::process::Child,
    last_started: Instant,
    /// 当前重启退避。反复失败时 ×2 增长，至清单的 `restart_backoff_max_ms` 封顶。
    backoff: Duration,
    /// 这个子进程的日志。随它存亡，`stop` 时先写事件再 kill。
    log: ChildLog,
}
```

**(b)** 改 `spawn`（`:221`），把 stderr 接到日志、stdout 保持吞掉：

```rust
    fn spawn(&mut self, manifest: &SupervisorManifest) -> std::io::Result<()> {
        let args = manifest.expand_args(
            &self.layout.workdir,
            &self.layout.state_dir,
            &self.layout.runtime_dir,
        );
        // The log is best effort: `ChildLog::open` never fails outright, it just
        // yields an inactive log when the path is unusable.
        let log = ChildLog::open(&self.layout.logs_dir(), &manifest.name);
        let stderr = log
            .file
            .as_ref()
            .and_then(|file| file.try_clone().ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);
        let child = Command::new(&manifest.command)
            .args(args)
            .current_dir(&self.layout.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()?;
        self.spawned = Some(child);
        self.spawned_log = Some(log);
        Ok(())
    }
```

**(c)** 在 `Supervisor`（`:54`）加暂存槽，与既有 `spawned` 并列：

```rust
pub struct Supervisor {
    layout: Layout,
    running: BTreeMap<String, RunningChild>,
    /// 一次 `spawn` 的暂存槽：先在无借用冲突处起进程，再搬进 `running`。
    spawned: Option<Child>,
    /// 与 `spawned` 配对：起进程时建好的日志，搬进 `running` 时一并带上。
    spawned_log: Option<ChildLog>,
}
```

并在 `Supervisor::new`（`:63`）里初始化为 `spawned_log: None`。

**(d)** 改 `spawn_and_track`（`:206`），记录成功/失败并带走日志：

```rust
    /// spawn 并把新 child 连同退避值记进 `running`。
    fn spawn_and_track(&mut self, manifest: &SupervisorManifest, backoff: Duration) {
        match self.spawn(manifest) {
            Ok(()) => {
                let mut log = self.spawned_log.take().unwrap_or(ChildLog { file: None });
                if let Some(child) = self.spawned.take() {
                    log.write_line(&format!("started {} (pid {})", manifest.name, child.id()));
                    self.running.insert(
                        manifest.name.clone(),
                        RunningChild {
                            child,
                            last_started: Instant::now(),
                            backoff,
                            log,
                        },
                    );
                }
            }
            Err(error) => {
                let mut log = self.spawned_log.take().unwrap_or(ChildLog { file: None });
                log.write_line(&format!("could not start {}: {error}", manifest.name));
            }
        }
    }
```

- [ ] **Step 4: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor a_childs_stderr_lands_in_its_log_file`

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor the_supervisor_records_when_it_starts_a_child`

Expected: 均 PASS。

- [ ] **Step 5: 跑既有 supervisor 测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors`

Expected: PASS（既有 6 条 + Task 1 的 4 条 lib 用例 + 本任务 2 条）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs
git commit -m "feat(supervisors): 子进程 stderr 与启动事件落盘"
```

---

### Task 3: 记录停止原因与退避重启

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`（`ensure_running:177`、`reconcile:91`、`stop:238`、`stop_all:246`）
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs`

**Interfaces:**
- Consumes: Task 2 的 `RunningChild.log`。
- Produces: `stop` 签名变更 —— `fn stop(&mut self, name: &str, reason: &str)`；`stop_all` 内部传 `"supervisor shutting down"`。**`stop` 是私有方法，无外部调用者**；crate 内有 **3** 个调用点（实现时已核实）：`stop_all`、`reconcile` 的 stale 分支（`"manifest removed"`）、`reconcile` 的 `stop_when_disabled` 分支（`"switch off"`）。三处都改了才编得过。

- [ ] **Step 1: 写失败的测试**

在 `tests/supervisor.rs` 中，紧跟 Task 2 的用例之后追加：

```rust
#[test]
fn removing_a_manifest_records_why_the_child_stopped() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "child should start");

    // 清单消失（插件被卸载）→ 必须记下是**这个**原因，而不是"开关关闭"。
    std::fs::remove_file(layout.manifests_dir().join("demo.json")).unwrap();
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("manifest removed"))
                .unwrap_or(false),
            "the stop reason to reach the log"
        ),
        "a stop caused by a removed manifest must say so"
    );
}

#[test]
fn switching_off_records_a_different_reason_than_removing_the_manifest() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    let prefs = workdir.join(".yi-agent/preferences.json");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "child should start");

    std::fs::write(&prefs, r#"{"demo_on":false}"#).unwrap();
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("switch off"))
                .unwrap_or(false),
            "the switch-off reason to reach the log"
        ),
        "a stop caused by the switch must be distinguishable from a removed manifest"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("manifest removed"),
        "the manifest is still there; the reason must not be misattributed: {text:?}"
    );
}
```

- [ ] **Step 2: 运行测试确认它失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor removing_a_manifest_records_why_the_child_stopped`

Expected: FAIL（当前 `stop` 无日志、无 reason）。

- [ ] **Step 3: 实现**

**(a)** 改 `stop`（`:238`）接受原因，并**先写事件再 kill**：

```rust
    /// 停掉一个子进程并记下原因。
    ///
    /// 先写日志再 kill：否则子进程的临终输出可能排在被杀之后丢失。
    fn stop(&mut self, name: &str, reason: &str) {
        if let Some(mut running) = self.running.remove(name) {
            running.log.write_line(&format!("stopped {name} ({reason})"));
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }
```

**(b)** 改 `stop_all`（`:246`）：

```rust
    /// daemon 退出时回收全部子进程。
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.running.keys().cloned().collect();
        for name in names {
            self.stop(&name, "supervisor shutting down");
        }
    }
```

**(c)** 改 `reconcile`（`:91`）的两处调用，分别给出原因：

```rust
        // 清单被删除 → 停止并移除。
        let stale: Vec<String> = self
            .running
            .keys()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            self.stop(&name, "manifest removed");
        }
```

以及 `desired_on` 为假的分支：

```rust
                self.stop(&manifest.name, "switch off");
                continue;
```

**(d)** 在 `ensure_running`（`:177`）的"已退出、尚在退避"处记一行：

```rust
                Ok(Some(_)) | Err(_) => {
                    // 已退出：未到当前退避就先等，到了才重试并把退避翻倍。
                    if running.last_started.elapsed() < running.backoff {
                        // 同一个退出事实只记一次——退避窗口内每 tick 都观察到它。
                        if !running.logged_exit {
                            running.log.write_line(&format!(
                                "exited {}; retrying in {:?}",
                                manifest.name, running.backoff
                            ));
                            running.logged_exit = true;
                        }
                        return;
                    }
                    running.backoff.saturating_mul(2).min(max)
                }
```

注意：`running` 在此处是 `&mut` 借用，`manifest.name` 是另一处借用，二者不冲突（`running` 来自 `self.running.get_mut`）。若借用检查器报错，把 `manifest.name.clone()` 绑到局部变量再用。

- [ ] **Step 4: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor removing_a_manifest_records_why_the_child_stopped`

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor switching_off_records_a_different_reason_than_removing_the_manifest`

Expected: 均 PASS。

- [ ] **Step 5: 跑全 crate 测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors`

Expected: PASS（既有 6 条 + 新增用例全绿）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs
git commit -m "feat(supervisors): 记录停止原因与退避重启"
```

---

### Task 4: 日志不可写时不得阻断 spawn

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs`

**Interfaces:**
- Consumes: Task 1–3 的全部改动。
- Produces: 无新 API。这是一条**反脆弱断言**，钉住 Global Constraints 里那条"best-effort"。

- [ ] **Step 1: 写测试**

追加到 `tests/supervisor.rs`：

```rust
#[test]
fn an_unwritable_log_does_not_stop_the_child_from_starting() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    // 让日志路径不可用：`logs` 处放一个**文件**而不是目录，`create_dir_all` 必失败。
    // 守护子进程是本职，记日志是附加——这条路径绝不能阻断 spawn。
    std::fs::write(layout.logs_dir(), "not a directory").unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();

    assert!(
        wait_for_pid_marker(&marker),
        "an unusable log path must never stop the child from starting"
    );
    supervisor.stop_all();
}
```

- [ ] **Step 2: 运行测试确认它通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor an_unwritable_log_does_not_stop_the_child_from_starting`

Expected: PASS。（这条在 Task 1 的 `ChildLog::open` 降级逻辑下就应通过；若 FAIL，说明 best-effort 约束被破坏，必须修实现而不是改测试。）

- [ ] **Step 3: 验证它在"没有降级"时会失败（反脆弱性检查）**

临时把 `ChildLog::open` 改为在失败时 `panic!`（或用 `.expect`），跑该测试确认 FAIL，然后**还原**。

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor an_unwritable_log_does_not_stop_the_child_from_starting`

Expected: 改坏时 FAIL；还原后 PASS。这一步证明该测试真的在守护"best-effort"这条约束，而不是永远为真。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs
git commit -m "test(supervisors): 日志不可写不得阻断子进程启动"
```

---

### Task 5: INSTALL.md 升级注意事项与日志指引

**Files:**
- Modify: `plugins/superpowers-kanban/INSTALL.md`

**Interfaces:**
- Consumes: 无。
- Produces: 无。

- [ ] **Step 1: 定位锚点**

Run: `grep -n "装完不需要重启 daemon" plugins/superpowers-kanban/INSTALL.md`

Expected: 打印第 20 行附近一处。**保留该句**（它对"新装"场景正确）。

- [ ] **Step 2: 新增升级小节**

在"## 2. 安装二进制到 PATH"（或紧邻"第 2 步"之后）插入新小节，**不删改既有内容**：

```markdown
### 2a. 升级：覆盖二进制后，让 `run` 进程重启

`install` 覆盖二进制会写入**新的 inode**；而已经在跑的 `superpowers-kanban run`
仍持有旧 inode，会继续用**旧代码**运行——监督器只检查「查询 socket 是否有人应答」，
不会发现二进制换了（unix 语义下这也是合法的）。

症状：升级后投递被静默拒绝，卡片不出现。

处理：让 `run` 进程退出一次，监督器会按清单的 `restart_backoff_ms` 拉起新的：

```bash
pkill -f "superpowers-kanban run"   # 监督器 1s 内拉起新进程
```

确认新进程用的是磁盘上的二进制：

```bash
lsof -p "$(pgrep -f 'superpowers-kanban run' | head -1)" | awk '$4=="txt" && /superpowers-kanban/ {print $7}'
stat -f %i "$(command -v superpowers-kanban)"
```

两个 inode 一致即已同步。

（第 0 节那句「装完不需要重启 daemon」说的是**新装**：daemon 会发现该守护的新清单。
升级的场景不同，需要重启的是 `run` 子进程，不是 daemon。）
```

- [ ] **Step 3: 新增日志指引**

在排障相关章节（若已有"排障"小节则追加其内）加：

```markdown
### 插件被拒绝的投递去哪了

插件的报告（入队成功、投递被拒）写在项目日志里：

```
<项目根>/.yi-agent/logs/superpowers-kanban.log
```

例：`2026-10-18T21:03:11 superpowers-kanban: rejected my-card (…原因…)`。

被拒绝的投递文件本身留在 `<项目根>/.yi-agent/superpowers-kanban/inbox/rejected/`。
看到 `rejected` 时先读日志里的原因，再看那个文件的实际内容。
```

- [ ] **Step 4: 提交**

```bash
git add plugins/superpowers-kanban/INSTALL.md
git commit -m "docs(kanban): 升级需重启 run 进程，并说明日志位置"
```

---

### Task 6: 全量验证与登记

**Files:**
- Modify: `docs/bug-list.md`（未修复条目区，`- [ ] 当前一些测试需要手工测试验证` 一带）

**Interfaces:**
- Consumes: 无。
- Produces: 无。

- [ ] **Step 1: 追加 bug-list 条目**

在 `docs/bug-list.md` 未修复条目区插入一行，登记为已修复：

```markdown
- [x] 插件升级后静默失效：`install` 覆盖二进制只写新 inode，已在跑的 `superpowers-kanban run` 继续用旧代码，而 supervisor 只看「查询 socket 是否有人应答」（`yi-agent-supervisors/src/supervisor.rs:177` `ensure_running`），不比对二进制新鲜度（全仓搜 `mtime`/`inode`/`modified` 零结果）；旧代码拒绝新格式投递（如缺 `title` 字段的预排队卡）后 `eprintln!("rejected …")`，而 supervisor spawn 时 `.stderr(Stdio::null())`（`supervisor.rs:227-233`）把它丢进 `/dev/null` —— 「系统在报告、报告被丢弃」。实测：`add-prequeued` 返回成功并打印 card-id，卡片却未上板、无任何错误，磁盘上留下 `inbox/rejected/*.json`。修复（本次只修可见性，二进制新鲜度检测另立）：被托管子进程的 stderr 落盘到 `<workdir>/.yi-agent/logs/<manifest.name>.log`（`Layout::logs_dir` + 私有 `ChildLog`，append 模式、写失败即降级为不写），并把 supervisor 生命周期事件（`started`/`could not start`/`exited … retrying in`/`stopped … (manifest removed|switch off|supervisor shutting down)`）写进同一文件、每行带 `YYYY-MM-DDTHH:MM:SS` 时间戳；**只重定向 stderr**，stdout 保持吞掉（`run` 长驻路径全程 `eprintln!`）。best-effort 硬约束：日志目录建不出/文件开不了绝不阻断 spawn。INSTALL.md 补升级需重启 `run` 子进程与日志位置。验证：`cargo test -p yi-agent-supervisors`（含 `child_log_tests` 4 例、`a_childs_stderr_lands_in_its_log_file`、`the_supervisor_records_when_it_starts_a_child`、`removing_a_manifest_records_why_the_child_stopped`、`switching_off_records_a_different_reason_than_removing_the_manifest`、`an_unwritable_log_does_not_stop_the_child_from_starting`（经反脆弱验证：把降级改成 panic 即红））— [设计](../superpowers/specs/2026-10-18-supervisor-child-log-design.md)、[计划](../superpowers/plans/2026-10-18-supervisor-child-log.md)
```

- [ ] **Step 2: 跑两个 crate 的全量测试**

Run: `ps aux | grep -v grep | grep -E "cargo|rustc"`（确认无残留）→ 然后

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors && cargo test -p yi-agent-runtime --lib`

Expected: 全绿。`yi-agent-runtime` 的 `serve` 测试尤其重要——它是 supervisor 唯一的生产调用方。

- [ ] **Step 3: 确认格式**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`

Expected: 无输出（通过）。

- [ ] **Step 4: 提交并合回 main**

```bash
cd .. && git add docs/bug-list.md docs/superpowers/specs/2026-10-18-supervisor-child-log-design.md docs/superpowers/plans/2026-10-18-supervisor-child-log.md
git commit -m "docs: 登记插件升级静默失效 bug，附 spec 与 plan"
```

然后按项目规范：回 `main` → `git merge --no-ff fix/supervisor-child-log` → 删分支 + 移除 worktree。

---

## Self-Review

**1. Spec coverage**

| Spec 小节 | 对应 Task |
|---|---|
| §3.1 职责边界（路径由 name 推导、不给清单加字段） | Task 1（`logs_dir`）+ Global Constraints |
| §3.2 路径与 append 打开方式 | Task 1 |
| §3.2 best-effort 硬约束 | Task 1 Step 4（`ChildLog::open` 降级）+ Task 4 |
| §3.3 子进程 stderr 落盘 | Task 2 |
| §3.3 supervisor 生命周期 6 类事件 | Task 2（started/could not start）+ Task 3（exited/stopped×3） |
| §3.3 区分 manifest removed vs switch off | Task 3 |
| §3.3 时间戳 | Task 1（`a_line_is_written_with_a_timestamp_prefix`） |
| §3.4 `ChildLog` 降级语义 | Task 1 + Task 4 |
| §3.4 chrono 依赖 | Task 1 Step 3 |
| §4 测试 1（stderr 到文件） | Task 2 |
| §4 测试 2（生命周期事件） | Task 2 |
| §4 测试 3（stop 原因可区分） | Task 3 |
| §4 测试 4（stdout 仍被吞） | Task 2（同一用例内断言） |
| §4 测试 5（best-effort 不阻断） | Task 4 |
| §4 测试 6（无回归） | Task 2 Step 5、Task 3 Step 5、Task 6 Step 2 |
| §5 文档同步（升级+日志指引） | Task 5 |
| §6 落点清单 | Task 1–5 |
| §2.2 非目标 | Global Constraints |

无遗漏。

**2. Placeholder scan**

无 TBD / TODO / "类似 Task N"；每个代码步骤都给了可直接落盘的完整代码块。

**3. Type consistency**

- `ChildLog { file: Option<File> }`：Task 1 定义；Task 2 在 `spawn` 构造、在 `spawn_and_track` 用 `ChildLog { file: None }` 兜底（同模块私有字段，可构造）。
- `ChildLog::open(logs_dir: &Path, name: &str) -> Self`、`write_line(&mut self, event: &str)`、`is_active(&self) -> bool`：定义与全部调用点一致。
- `Layout::logs_dir(&self) -> PathBuf`：Task 1 定义；Task 2/3/4 调用一致。
- `RunningChild` 字段 `log: ChildLog`：Task 2 加入；Task 3 在 `stop` 与 `ensure_running` 用它。
- `Supervisor.spawned_log: Option<ChildLog>`：Task 2 加入并在 `new` 初始化。
- `stop(&mut self, name: &str, reason: &str)`：Task 3 改签名；`stop_all` 与 `reconcile` 两处调用同步更新（无其它调用者，已核实）。

**4. 已知执行风险**

- Task 3 Step 3(d) 的 `running` 与 `manifest.name` 借用可能冲突；plan 已给出 `manifest.name.clone()` 的退路。
- Task 2 Step 3(b) 的 `try_clone().expect(...)`：`ChildLog` 的文件刚打开，clone fd 在实践中不会失败；但若代码评审认为 `expect` 不可接受，可改为 `try_clone().ok().map(Stdio::from).unwrap_or_else(Stdio::null)`（同样满足 best-effort）。**实现时采用后者更稳妥**——已在 Task 2 代码中标注这一点供执行者选择。
- Task 4 Step 3 的反脆弱验证需临时改坏实现再还原，执行者务必确认最终状态是还原后的。