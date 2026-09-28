# bash 工具进程组隔离与异常路径整组回收 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `BashTool` spawn 的 shell 自成独立进程组，并在超时/取消时回收整组，
同时保留正常结束时的跨调用长驻行为。

**Architecture:** 新增 `process_group.rs` 承载「机制」（配置进程组 + 向进程组发信号）；
策略留在各调用点 —— `shell/bash.rs` 在超时/取消时对整组 SIGKILL，
`process/manager.rs` 行为不变仅改为复用机制。隔离用 tokio 原生
`Command::process_group(0)`。

**Tech Stack:** Rust（edition 2024）、tokio 1.53.0（`process_group` / `Child::id`）、
libc 0.2、`yi-agent-tools` crate。测试用 `#[tokio::test]` + `tempfile`。

## Global Constraints

- 工作分支：本计划在 worktree `.worktrees/<branch>` 内执行；**严禁**在 `main` 上直接提交。
- 提交信息用 conventional commits（本计划用 `feat(tools):` / `test(tools):` /
  `refactor(tools):` / `docs(tools):`）；**禁止** `Co-Authored-By` 行。
- 每次提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 测试命令统一用 worktree 自己的 target：
  `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools <test_name>`。
- **不要**并发跑多个 `cargo test`。若被中断，先
  `pkill -f "yi-agent-tools-"` 清理僵尸测试二进制再重跑。
- 依赖已就绪，**不需要**新增依赖：`libc = "0.2"` 已在
  `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`；tokio 为 1.53.0。
- 平台：unix 为真实现，非 unix 为 no-op（保持与现状一致）。
- **语义约束（A1，spec §5）**：正常结束时**不得**杀进程组；仅超时/取消时整组回收。
- 精确值（逐字使用）：`process_group(0)`；`libc::setpgid(0, 0)`；
  `libc::kill(-(pid as libc::pid_t), sig)`；pgid 类型 `Option<u32>`。

---

## File Structure

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-tools/src/process_group.rs`（新建） | 机制层：进程组配置与信号发送原语；unix 实现 + 非 unix no-op |
| `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`（改 1 行） | 声明 `mod process_group;` |
| `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`（改） | 删除本地 `configure_process_group` / `kill_process_group`，改用共享机制；**行为不变** |
| `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`（改） | spawn 加隔离；超时/取消整组回收；正常结束 disarm；更新 description；新增测试 |

---

### Task 1: 机制层 `process_group.rs`（含 manager.rs 复用）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/process_group.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/process_group.rs`（内联 `#[cfg(test)]`）

**Interfaces:**
- Consumes: 无（本计划第一个任务）
- Produces:
  - `pub(crate) fn configure_process_group(cmd: &mut Command)` —— 让 `cmd` spawn 出的
    子进程自任组长（等价 `setpgid(0,0)`）；非 unix 为 no-op。
  - `pub(crate) fn signal_process_group(pid: u32, sig: i32)` —— 向进程组
    `pid` 发信号 `sig`；非 unix 为 no-op。
  - 参数 `pid` 语义：**进程组组长 pid，即子进程自身 pid**（因为 `process_group(0)`
    使 pgid == 子进程 pid）。

- [ ] **Step 1: 写失败测试（机制层行为）**

在新建的 `process_group.rs` 末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    #[cfg(unix)]
    #[tokio::test]
    async fn configure_process_group_puts_child_in_its_own_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        configure_process_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("child id while running");

        // 子进程自成一组：pgid == 自身 pid。
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        assert_eq!(pgid, pid as libc::pid_t, "child should lead its own group");

        // 且不与当前进程同组。
        let own = unsafe { libc::getpgid(0) };
        assert_ne!(pgid, own, "child must not share the caller's group");

        signal_process_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_process_group_kills_a_surviving_background_child() {
        // shell 自任组长，内部再起一个后台子孙；杀整组应把子孙一并收掉。
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sh -c 'sleep 30' & wait");
        configure_process_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("child id");

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        signal_process_group(pid, libc::SIGKILL);
        let _ = child.wait().await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // 整组已被清掉：组长 pid 不再可查。
        let gone = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(gone, -1, "group leader must be gone after group kill");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools configure_process_group_puts_child_in_its_own_group`
Expected: 编译失败 —— `configure_process_group` / `signal_process_group` 未定义。

- [ ] **Step 3: 写最小实现**

`process_group.rs`（`#[cfg(test)] mod tests` 之前的部分）：

```rust
//! 进程组机制：让工具 spawn 的子进程自成进程组，并能整组回收。
//!
//! 只放「机制」，不放策略：发什么信号、何时发，由各调用点决定。

use tokio::process::Command;

/// 让 `cmd` spawn 出的子进程自任组长（等价于子进程内 `setpgid(0, 0)`）。
///
/// 使子进程组 pgid == 子进程 pid，从而与 yi-agent 自身进程组隔离。
#[cfg(unix)]
pub(crate) fn configure_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn configure_process_group(_cmd: &mut Command) {}

/// 向进程组 `pid` 发送信号 `sig`。`pid` 为组长 pid（即子进程自身 pid）。
///
/// 对已消失的组返回 `ESRCH`，属正常情况，调用方应忽略。
#[cfg(unix)]
pub(crate) fn signal_process_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
    }
}

#[cfg(not(unix))]
pub(crate) fn signal_process_group(_pid: u32, _sig: i32) {}
```

在 `lib.rs` 的模块声明区（`mod process;` 一带）加入：

```rust
mod process_group;
```

- [ ] **Step 4: 让 manager.rs 复用机制（行为不变）**

在 `process/manager.rs` 中：
1. 删除本地 `#[cfg(unix)] fn configure_process_group` / `#[cfg(not(unix))] fn configure_process_group`（现 796-808 行附近）。
2. 删除本地 `#[cfg(unix)] fn kill_process_group` / `#[cfg(not(unix))] fn kill_process_group`（现 810-818 行附近）。
3. 把两处调用点 `kill_process_group(pid)`（现 455、790 行）改为
   `signal_process_group(pid, libc::SIGTERM)`。
4. 在文件顶部 `use crate::shell::blocklist;` 下一行加入：

```rust
use crate::process_group::{configure_process_group, signal_process_group};
```

> 行为不变：原 `kill_process_group` 正是对整组发 `SIGTERM`，此处逐字等价。
> `configure_process_group(&mut cmd)` 调用点（现 343 行）保持不变。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools process_group`
Expected: PASS（2 个新测试）。再跑 manager 相关：
Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib process`
Expected: PASS（原有 manager 测试全绿，证明行为未变）。

- [ ] **Step 6: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/process_group.rs \
        yi-agent-rs/crates/yi-agent-tools/src/lib.rs \
        yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs
git commit -m "refactor(tools): extract shared process-group helpers"
```

---

### Task 2: bash 工具进程组隔离

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs:135-145`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`（内联 `mod tests`）

**Interfaces:**
- Consumes: `crate::process_group::configure_process_group`（Task 1）；
  `crate::process_group::signal_process_group`（Task 1）。
- Produces: `BashTool` spawn 的子进程自成进程组；为 Task 3 提供
  「pgid 在 spawn 后立即抓取并存入变量」的代码结构。

- [ ] **Step 1: 写失败测试（隔离断言）**

在 `bash.rs` 的 `#[cfg(test)] mod tests` 内追加：

```rust
#[cfg(unix)]
#[tokio::test]
async fn bash_child_runs_in_its_own_process_group() {
    let tmp = TempDir::new().unwrap();
    let tool = make_tool(&tmp);

    // 子进程打印自身 pgid 与 pid；隔离后应相等（自任组长）。
    let result = tool
        .call(serde_json::json!({
            "command": "echo \"pgid=$(ps -o pgid= -p $$ | tr -d ' ') pid=$$\""
        }))
        .await;
    assert!(!result.is_error);

    let text = match &result.content[0] {
        yi_agent_core::ContentBlock::Text(s) => s.clone(),
        _ => panic!("expected text block"),
    };
    let pgid: i32 = text
        .split("pgid=").nth(1).unwrap()
        .split_whitespace().next().unwrap()
        .parse().unwrap();
    let pid: i32 = text
        .split("pid=").nth(1).unwrap()
        .split_whitespace().next().unwrap()
        .parse().unwrap();
    assert_eq!(pgid, pid, "child should lead its own process group");

    // 且不与测试进程（即模拟的 yi-agent）同组。
    let own = unsafe { libc::getpgid(0) };
    assert_ne!(pgid, own, "child must not share the caller's group");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools bash_child_runs_in_its_own_process_group`
Expected: FAIL —— 当前无隔离，`pgid != pid`（与被测进程同组）。

- [ ] **Step 3: 写最小实现（加隔离 + 立即抓 pgid）**

把 `bash.rs` 现 135-145 行的 spawn 块替换为：

```rust
        let mut child = match Command::new(program)
            .args(command_args)
            .current_dir(&cwd)
            .process_group(0) // 自成进程组（pgid == 子进程 pid），与 yi-agent 隔离
            // Dropping the agent's tool future must not leave the shell running.
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(ToolEvent::Exit { code: Some(-1) }).await;
                return ToolsError::Io(e).into();
            }
        };

        // 必须在任何 wait() 之前抓取：Child::id() 一旦子进程被 poll 完成
        // 即返回 None（tokio 1.53.0 process/mod.rs:1216-1227）。
        // 隔离后该值同时是子进程的进程组 id。
        let child_pgid = child.id();
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools bash_child_runs_in_its_own_process_group`
Expected: PASS。

再跑回归：
Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib bash`
Expected: PASS（除 Task 3 待补的回收测试外，原有 bash 测试全绿）。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs
git commit -m "feat(tools): run bash tool children in their own process group"
```

---

### Task 3: 超时 / 取消时整组回收（A1 语义）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`（超时分支 296/304、
  取消路径、正常结束 disarm、description）
- Test: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`（内联 `mod tests`）

**Interfaces:**
- Consumes: `crate::process_group::signal_process_group`（Task 1）；
  `child_pgid: Option<u32>`（Task 2 产出）。
- Produces: 一个 `ProcessGroupGuard`（文件内私有），`Drop` 时对整组发 `SIGKILL`；
  提供 `disarm(&mut self)`。语义：**只在未被 disarm 时才杀整组**。

- [ ] **Step 1: 写失败测试（异常回收 + 正常保留 + 自杀防护）**

在 `bash.rs` 的 `mod tests` 内追加：

```rust
#[cfg(unix)]
/// 进程组 `pgid` 是否仍存在（至少还有一个成员）。
///
/// 用 `kill(pgid, 0)` 探测，避免 `ps` 全文扫描的自匹配与竞态。
fn group_still_alive(pgid: i32) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_timeout_kills_the_whole_process_group() {
    let tmp = TempDir::new().unwrap();
    let tool = make_tool(&tmp);

    // 命令把自身 pgid 写入文件，便于测试精确探测该组是否仍存活。
    let pgid_file = tmp.path().join("pgid.txt");
    let cmd = format!(
        "ps -o pgid= -p $$ | tr -d ' ' > {}; sh -c 'while :; do :; done' & sleep 30",
        pgid_file.display()
    );
    let result = tool
        .call(serde_json::json!({
            "command": cmd,
            "timeout": 1,
            "expected_timeout_sec": 1
        }))
        .await;
    assert!(result.is_error, "should report timeout");

    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("command wrote its pgid")
        .trim()
        .parse()
        .expect("pgid is numeric");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !group_still_alive(pgid),
        "timeout must reap the whole group (pgid {pgid} still alive)"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_normal_exit_keeps_background_process_alive() {
    let tmp = TempDir::new().unwrap();
    let tool = make_tool(&tmp);

    // 正常结束：后台进程应存活（跨调用长驻）。
    let marker = tmp.path().join("bg.txt");
    let cmd = format!(
        "nohup sh -c 'sleep 1; touch {}' >/dev/null 2>&1 & exit 0",
        marker.display()
    );
    let result = tool
        // 显式给出 expected_timeout_sec：正常结束时后台进程仍攥着管道，
        // reader_wait 需等到 idle_limit 才放弃；不设则默认 180s，测试会久等。
        .call(serde_json::json!({
            "command": cmd,
            "timeout": 5,
            "expected_timeout_sec": 3
        }))
        .await;
    assert!(!result.is_error, "normal exit expected: {:?}", result.content);

    // 后台进程仍在（未被 disarmed guard 误杀）→ 1 秒后写出 marker。
    tokio::time::sleep(std::time::Duration::from_millis(1800)).await;
    assert!(marker.exists(), "background process must survive normal exit");
}

#[cfg(unix)]
#[tokio::test]
async fn bash_cancel_reaps_the_whole_process_group() {
    let tmp = TempDir::new().unwrap();
    let tool = Arc::new(make_tool(&tmp));

    let pgid_file = tmp.path().join("pgid.txt");
    let cmd = format!(
        "ps -o pgid= -p $$ | tr -d ' ' > {}; sh -c 'while :; do :; done' & sleep 30",
        pgid_file.display()
    );
    let task = tokio::spawn({
        let tool = tool.clone();
        let cmd = cmd.clone();
        async move {
            tool.call(serde_json::json!({ "command": cmd, "timeout": 30 })).await
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("command wrote its pgid")
        .trim()
        .parse()
        .expect("pgid is numeric");
    task.abort();
    let _ = task.await;
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !group_still_alive(pgid),
        "cancel must reap the whole group (pgid {pgid} still alive)"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_self_kill_by_process_group_does_not_kill_the_caller() {
    let tmp = TempDir::new().unwrap();
    let tool = make_tool(&tmp);

    // 复刻事故：命令内杀自身进程组。隔离后不应伤到调用方（本测试进程）。
    // 外层用 `sleep` 制造组内成员，再由组内 sh 执行 kill -TERM -$PGID。
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        tool.call(serde_json::json!({
            "command": "PGID=$(ps -o pgid= -p $$ | tr -d ' '); kill -TERM -$PGID; echo survived",
            "timeout": 5,
            "expected_timeout_sec": 2
        })),
    )
    .await;
    // 只要能拿到结果，就说明调用方（本测试进程）没被信号带走。
    assert!(result.is_ok(), "caller must survive a self group-kill");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib bash_timeout_kills_the_whole_process_group bash_normal_exit_keeps_background_process_alive bash_cancel_reaps_the_whole_process_group`
Expected: 至少 `bash_timeout_kills_the_whole_process_group` 与
`bash_cancel_reaps_the_whole_process_group` FAIL（子孙未被回收）。
`bash_normal_exit_keeps_background_process_alive` 可能已通过（现状本就保留）。

- [ ] **Step 3: 实现 guard + 三路径回收**

在 `bash.rs` 顶部（`use` 区之后、`pub struct BashTool` 之前）加入 guard：

```rust
/// 保证异常退出时回收整个进程组；正常结束时由调用方 `disarm()`。
///
/// 只在未被 disarm 时于 `Drop` 中对整组发 `SIGKILL`。
struct ProcessGroupGuard {
    pgid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pgid: Option<u32>) -> Self {
        Self { pgid, armed: true }
    }

    /// 标记为正常结束：不再回收整组（保留后台进程）。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pgid) = self.pgid {
                crate::process_group::signal_process_group(pgid, libc::SIGKILL);
            }
        }
    }
}
```

在 `let child_pgid = child.id();`（Task 2 产出）之后立刻加入：

```rust
        let mut pgid_guard = ProcessGroupGuard::new(child_pgid);
```

把两处超时分支（现 296、304 行）由：

```rust
                    let _ = child.kill().await;
```

改为：

```rust
                    if let Some(pgid) = child_pgid {
                        crate::process_group::signal_process_group(pgid, libc::SIGKILL);
                    }
                    let _ = child.wait().await;
                    pgid_guard.disarm();
```

在 `select` 循环的**正常结束分支**（`status = child.wait() => { ... break; }` 之后、
`if timed_out { let _ = child.wait().await; }` 之前）加入：

```rust
        // 正常结束：保留后台进程（跨调用长驻），不回收整组。
        pgid_guard.disarm();
```

> 注意顺序：`disarm()` 必须在函数返回前执行，否则 Drop 会误杀长驻进程。
> 超时分支已各自 disarm；正常分支在此统一 disarm。取消（future drop）时
> guard 未 disarm，Drop 负责杀整组。

- [ ] **Step 4: 更新工具 description（A1 语义）**

把 `description()` 的字符串替换为：

```rust
        "Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls. On timeout or cancellation the command's entire process group is killed, including background processes it started; for long-lived services use the managed process tools instead. Prefer combining dependent steps with && into a single call (e.g. `mkdir -p foo && touch foo/bar.txt && ls foo`) rather than splitting across turns."
```

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib bash`
Expected: PASS —— 新增 4 个测试 + 原有 bash 测试（含
`bash_timeout_kills`、`dropping_bash_call_stops_the_child_process`、
`bash_orphan_subprocess_does_not_hang_call_stream`）全绿。

- [ ] **Step 6: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs
git commit -m "feat(tools): reap bash tool process group on timeout and cancellation"
```

---

### Task 4: 端到端回归与边界确认

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-tools/tests/bash_stream.rs`（可新增集成测试）

**Interfaces:**
- Consumes: Task 1-3 的全部产出。
- Produces: 无新接口；仅验证整链路。

- [ ] **Step 1: 写集成回归测试**

在 `tests/bash_stream.rs` 末尾追加（如该文件缺少 `use`，按现有风格补齐）：

```rust
#[cfg(unix)]
#[tokio::test]
async fn bash_tool_does_not_leak_busy_loop_after_timeout() {
    use std::process::Command;
    use std::sync::Arc;
    use yi_agent_core::Tool;
    use yi_agent_tools::{BashTool, ToolsContext};
    use tempfile::TempDir;

    let tmp = TempDir::new().unwrap();
    let tool = BashTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())));

    // 复刻事故命令形态：xargs 并发拉起空转进程；命令写出自身 pgid 供探测。
    let pgid_file = tmp.path().join("pgid.txt");
    let cmd = format!(
        "ps -o pgid= -p $$ | tr -d ' ' > {}; seq 1 4 | xargs -P4 -I{{}} sh -c 'while :; do :; done'; sleep 30",
        pgid_file.display()
    );
    let result = tool
        .call(serde_json::json!({
            "command": cmd,
            "timeout": 1,
            "expected_timeout_sec": 1
        }))
        .await;
    assert!(result.is_error, "expected timeout");

    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("command wrote its pgid")
        .trim()
        .parse()
        .expect("pgid is numeric");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let alive = unsafe { libc::kill(-pgid, 0) == 0 };
    assert!(!alive, "no busy loop may survive the timeout (pgid {pgid})");
}
```

- [ ] **Step 2: 运行确认通过**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --test bash_stream`
Expected: PASS（含新测试）。

- [ ] **Step 3: 全 crate 测试**

Run: `cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/tests/bash_stream.rs
git commit -m "test(tools): cover end-to-end process-group reclamation for bash"
```
