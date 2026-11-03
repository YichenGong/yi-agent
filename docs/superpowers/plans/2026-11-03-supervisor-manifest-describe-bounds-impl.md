# 插件清单刷新限时与取消 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复插件清单自述的管道背压与退出后超时失效，并让项目监督停止能取消刷新、保留原清单且安全清理本次自述进程。

**Architecture:** 在 refresh 的私有子模块实现有界双流捕获、非阻塞轮询和独占回收权的 ChildSession；保持旧入口，新增取消入口，批次内部保留结构化失败分类。runtime 将刷新放入现有 catch_unwind，通过已有 stop AtomicBool 取消，仍走原项目路由清理路径。

**Tech Stack:** Rust 2024、std process/IO/Instant、现有 libc 0.2、serde_json、tracing、tempfile；macOS/Linux 现有 Unix 进程能力；假插件使用 CI 已使用的 Python 3，不增加 Cargo 依赖、生产解释器依赖或 Tokio。

## Global Constraints

- 唯一需求源：[修订 spec](../specs/2026-11-03-supervisor-manifest-describe-bounds-design.md)。初稿基线 `b4567a1c`，本次计划复核已与 `83a60f02` 对齐；实施时从执行宿主提供的最新基线重新核对目标接口。本文件是待执行计划，不是实现报告。
- `rust-version = "1.89"`；不升级依赖、工具链，不新增 Cargo 依赖。新 unsafe 块写清 FD/PID/输出结构的有效性理由。
- 生产 timeout **15 秒**，poll_interval **20 ms**，stdout_limit **1 MiB（1,048,576 字节）**，stderr_capture_limit **64 KiB（65,536 字节）**，read_chunk **8 KiB**，drain_budget_per_stream **64 KiB/轮**。
- stdout 恰好等于上限允许，下一字节失败；stderr 超限仅截断保存，继续排空。错误首行展示最多 **512 个字符**，截断标记计入此限值；stdout 内容不进日志。
- spawn 前建立单一 deadline；每轮和完成前检查取消/期限；同一检查点取消优先。spawn 与最终回收仍受 OS 不可中断状态约束，不引入无界用户态读取或 reader join。
- 直接 child 退出与 stdout/stderr 均 EOF 才可能成功。用 `waitid(P_PID, ..., WEXITED | WNOHANG | WNOWAIT)` 观察但不回收；组清理先于最终 wait，之后不再使用缓存 pgid 发信号。
- ECHILD 解除信号能力、关闭读端并返回所有权丢失；不支持外部 reap-all/SA_NOCLDWAIT。不保证清理逃逸或改变权限的恶意后代。
- 旧 `RefreshOutcome::{Current,Refreshed,Skipped}` 的公开形状与两个旧刷新入口不变；仅同名、可解析、自述成功且清理成功的清单允许原子替换。
- 只刷新启动前这一遍，正常对账仍每 500ms；不改正常 run 子进程、安装/卸载、开关来源、socket 回退规则、清单并发写入或全目录总预算。
- 取消后不启动后续插件；写回前再检查取消，但不撤销检查后已提交的 rename。取消日志由私有分类决定，禁止匹配 reason 子串。
- runtime 停止用 **2 秒宽 backstop** 验证取消，不把 20ms 当 CI 精确性能指标；真实 A 超时/B 恢复测试使用生产 15 秒预算、**至少 40 秒外层 backstop**，不新增跨 crate 的生产测试配置 API。
- 实施时使用执行宿主提供的隔离 worktree；看板卡片直接使用其 `.worktrees/kanban/...` 和 `kanban/...` 分支，不再创建固定名称的第二工作树。只有普通非看板会话才按 using-git-worktrees 创建 `fix/supervisor-manifest-refresh` 分支。不在 main 修改文件。每个交付提交前 `cargo fmt --all && just ci`；合并结果再次 `just ci`。默认 mock，不加 `--ignored`。

## 文件职责与接口边界

下文文件路径均从实施 worktree 根开始；命令明确进入 `yi-agent-rs`。将执行器限制在 **refresh 私有模块**，不建立通用进程框架。

| 文件 | 操作与职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh.rs` | 修改：旧入口包装、取消批次、解析/同名/原子写契约、结构化日志投影 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh/describe.rs` | 新建：DescribePolicy、DescribeError、Capture、双流状态机、最终限时检查 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh/describe/process.rs` | 新建：ChildSession、fcntl、waitid、不提前回收、信号与回收顺序、展开清理 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh/describe/tests.rs` | 新建：捕获/时钟/真实进程/注入失败测试；由 describe 的 cfg(test) 模块加载 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh/describe/fixture.rs` | 新建：只在 cfg(test) 使用的通用假插件；仅 std/tempfile/serde_json，无本 crate 类型引用 |
| `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs` | 修改：取消入口置于原 catch，保持清理；复用已有项目分区/flags 测试机制 |
| `docs/bug-list.md` | 更新本缺陷的实际证据与状态，不动其他问题 |
| `docs/project-management/yi-agent-runtime.md`、`README.md` | 每个交付更新完成判据；最终实现全通过才把该项改 [x]，按实际复选框重算计数 |

公共取消入口的确切形状（Task 2 产生，Task 3 使用）：

```rust
pub fn refresh_from_plugins_until(
    dir: &std::path::Path,
    should_stop: impl FnMut() -> bool,
) -> Vec<RefreshOutcome>;
pub fn refresh_and_log_until(
    dir: &std::path::Path,
    should_stop: impl FnMut() -> bool,
) -> Vec<String>;
```

上面为接口声明，不是在 Rust 模块顶层放无函数体的普通函数。取消谓词同步调用，不加 Send/'static 限制；runtime 的外层线程自己满足其捕获要求。

### 执行和提交约定

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:$PATH"
(cd yi-agent-rs && cargo test -p yi-agent-supervisors -- --list)
(cd yi-agent-rs && cargo test -p yi-agent-supervisors)
(cd yi-agent-rs && cargo test -p yi-agent-runtime --lib supervise)
```

每步先核对 `--list` 确有相应测试，避免过滤成 0 tests 也 exit 0。长运行的测试使用进程级 backstop或有界接收；失败也释放假插件根目录，子进程检测根消失即退出，禁止在失败时无限 join。

每个 Task 的提交步骤都包括下列门禁，长 CI 单独等待，不能伪装成 2–5 分钟编辑步骤：

```sh
(cd yi-agent-rs && cargo fmt --all && just ci) && git diff --check
```

不共享其他 worktree 的 target、不删 cargo 锁、不靠重新跑直到绿掩盖根因。过往 main 的 nextest 二进制曾嵌入已移除 worktree 的 fixture 路径：发生同类错误先查看二进制来源；可按现有 YI_FIXTURE_DIR 契约使用真实样例路径，但不能跳过失败项。两条 subagent_real_e2e runtime 用例过往也在完整 nextest 中失败、单独重编译运行通过；若本轮门禁仍失败，保留日志并停止合并，不扩展本计划去修这些无关模块。

---

## Task 1：完整私有自述执行器与资源护栏

**Files:** Create 上表 describe.rs、process.rs、tests.rs、fixture.rs；Modify/Test `src/refresh.rs:107–154` 与其中已有 tests；同步 runtime 模块未完成项的阶段证据。

**Interfaces:** Produces 下列私有接口，后续 Task 只使用 describe/error，不接管 FD/PID：

```rust
#[derive(Clone, Copy)]
pub(super) struct DescribePolicy {
    pub timeout: std::time::Duration,
    pub poll_interval: std::time::Duration,
    pub stdout_limit: usize,
    pub stderr_capture_limit: usize,
    pub read_chunk: usize,
    pub drain_budget_per_stream: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ObservedExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}
#[derive(Debug)]
pub(super) enum DescribeError {
    Cancelled,
    Timeout(std::time::Duration),
    StdoutLimit(usize),
    InvalidUtf8,
    NonZero { exit: ObservedExit, stderr: String },
    Io { stage: &'static str, source: std::io::Error },
    OwnershipLost,
    CleanupFailed(String),
    WithCleanup { primary: Box<DescribeError>, cleanup: String },
}
// Task 1 implements render(); Task 2 adds typed classification methods.
// All signatures below have bodies in the implementation.
// DescribePolicy::production() -> DescribePolicy
// ObservedExit::success(self) -> bool
// DescribeError::render(&self, command: &str) -> String
// describe(command: &str, policy: DescribePolicy,
//          should_stop: &mut dyn FnMut() -> bool) -> Result<String, DescribeError>
// check_limits(deadline: Instant, now: Instant, timeout: Duration,
//              cancelled: bool) -> Result<(), DescribeError>
// Capture::new(limit: usize, hard_limit: bool) -> Capture
// Capture::push(&mut self, bytes: &[u8]) -> Result<(), DescribeError>
// Capture::text(&self) -> Result<String, DescribeError>
// Capture::diagnostic(&self) -> String
// ChildSession::spawn(command: &str) -> Result<ChildSession, DescribeError>
// ChildSession::observe(&mut self) -> Result<Option<ObservedExit>, DescribeError>
// ChildSession::finish(&mut self) -> Result<(), DescribeError>
```

`ChildSession` 在process.rs声明为 `pub(super)`；stdout/stderr字段及spawn/observe/finish方法均为 `pub(super)`，字段类型为 `Option<ChildStdout>` / `Option<ChildStderr>`，只对父describe可见。其child、owns_child、may_signal_group、observed_exit、finished保持私有；Drop调用finish同一清理路径。describe.rs中 `DescribePolicy::production`、`DescribeError::{render,is_cancelled,cleanup_warning}` 必须显式声明 `pub(super)`，供父refresh.rs调用；后两方法由Task2增加。ObservedExit::success、Capture和时钟核心不扩大可见性。只导出类型而遗漏方法可见性会导致父模块编译失败，禁止用pub全公开来规避。

### Step 1：先造不会污染 run marker 的假插件与兼容旧 API 的失败测试

- [ ] 新建 fixture.rs。产生 `PluginFixture::new()`、`workdir() -> &Path`、`plugin(name: &str, mode: &str, payload_bytes: usize, exit_code: i32) -> FakePlugin`、`arm_flags() -> PathBuf`。
- [ ] `FakePlugin` 暴露测试所需的 `command/stored/body/manifest_marker/descendant_marker/run_marker/query_socket: PathBuf`，不暴露生产类型。每个项目一个 TempDir，可装 a、b 两插件；stored 清单的 `stop_when_disabled=false`，插件自述改为 true，command 一致，switch_key 为已注册 `superpowers_kanban`。root 析构时删除目录，使 hold/hang/continuous 后代在下一轮检查退出。
- [ ] launcher 用 shell 正确引用路径，再 `exec python3 helper.py config.json "$@"`。helper/config 文件用 UTF-8；Python 子进程为 exec 后的直接 child，不留中间 sh。

fixture helper 的实际内容（config 含 root、fresh、mode、exit_code 和各 marker/socket；由 serde_json::json! 写入）：

```python
# coding: utf-8
import json, os, pathlib, socket, subprocess, sys, time
cfg = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
root = pathlib.Path(cfg["root"])
def mark(key):
    p = pathlib.Path(cfg[key])
    t = p.with_name(p.name + ".tmp")
    t.write_text(str(os.getpid()), encoding="ascii")
    os.replace(t, p)
def hold():
    end = time.monotonic() + 60
    while root.exists() and time.monotonic() < end:
        time.sleep(0.01)
verb = sys.argv[2] if len(sys.argv) > 2 else "run"
if verb == "hold":
    mark("descendant_marker")
    hold()
    sys.exit(0)
if verb == "manifest":
    mark("manifest_marker")
    mode = cfg["mode"]
    if mode == "hang":
        hold()
        sys.exit(0)
    if mode in ("inherited", "inherited_closed"):
        sinks = {} if mode == "inherited" else {
            "stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL,
        }
        subprocess.Popen([sys.executable, __file__, sys.argv[1], "hold"], **sinks)
        until = time.monotonic() + 20
        while not pathlib.Path(cfg["descendant_marker"]).exists():
            if not root.exists() or time.monotonic() >= until:
                sys.exit(72)
            time.sleep(0.01)
    if mode == "eof_live":
        os.close(1)
        os.close(2)
        hold()
        sys.exit(0)
    if mode == "noisy":
        sys.stderr.buffer.write(b"x" * (2 * 1024 * 1024))
        sys.stderr.flush()
    if mode == "continuous":
        end = time.monotonic() + 60
        while root.exists() and time.monotonic() < end:
            sys.stderr.buffer.write(b"x" * 4096)
            sys.stderr.flush()
        sys.exit(0)
    if mode == "nonzero":
        sys.stderr.write("cancelled is only plugin text\nsecond line\n")
        sys.exit(cfg["exit_code"])
    if mode == "invalid_utf8":
        sys.stdout.buffer.write(b"\xff")
        sys.exit(0)
    print(cfg["fresh"], flush=True)
    sys.exit(cfg["exit_code"])
mark("run_marker")
s = socket.socket(socket.AF_UNIX)
s.bind(cfg["query_socket"])
s.listen(16)
s.settimeout(0.1)
try:
    end = time.monotonic() + 60
    while root.exists() and time.monotonic() < end:
        try:
            c, _ = s.accept()
        except socket.timeout:
            continue
        with c:
            c.settimeout(1)
            try:
                data = c.makefile("rb").readline()
                if data:
                    c.sendall(b'{"ok":true}\n')
            except (OSError, socket.timeout):
                pass
finally:
    s.close()
```

`fresh` 是完整 JSON **字符串**，不是二次编码对象：`print(cfg["fresh"])` 直接产生原清单正文。用 `args = ["run", query_socket, "x".repeat(payload_bytes)]` 产生 stdout 大数据；所有 marker 分清 manifest/run。query_socket 放在 fixture 项目根的短路径，并断言长度不超过现有 MAX_SOCKET_PATH_BYTES，不复制哈希回退算法。

兼容旧公开入口的行为红（放入 refresh.rs 已有 tests，不能只留下缺类型的编译红）：

```rust
#[test]
fn describe_drains_large_stdout_without_a_false_timeout() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "small", 256 * 1024, 0);
    let before = std::fs::read(&plugin.stored).unwrap();
    let result = refresh_from_plugins(&fixture.workdir().join(".yi-agent/supervisors"));
    assert_eq!(result, vec![RefreshOutcome::Refreshed("a".into())]);
    let after = std::fs::read(&plugin.stored).unwrap();
    assert_ne!(after, before);
    let parsed = SupervisorManifest::parse(std::str::from_utf8(&after).unwrap()).unwrap();
    assert!(parsed.stop_when_disabled);
    assert_eq!(parsed.args.last().unwrap().len(), 256 * 1024);
}
```

- [ ] 加入完整的 stderr背压行为测试：

```rust
#[test]
fn describe_drains_stderr_after_its_capture_limit() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "noisy", 0, 0);
    let before = std::fs::read(&plugin.stored).unwrap();
    let result = refresh_from_plugins(&fixture.workdir().join(".yi-agent/supervisors"));
    assert_eq!(result, vec![RefreshOutcome::Refreshed("a".into())]);
    let after = std::fs::read(&plugin.stored).unwrap();
    assert_ne!(after, before);
    let parsed = SupervisorManifest::parse(std::str::from_utf8(&after).unwrap()).unwrap();
    assert!(parsed.stop_when_disabled);
    assert_eq!(parsed.name, "a");
}
```
- [ ] 在 refresh.rs 用下列声明加载 fixture；describe.rs 用 `#[cfg(test)] mod tests;` 加载 describe/tests.rs。fixture 本身不引用 crate::manifest，便于 Task 3 源码级测试复用。describe tests 通过 `crate::refresh::fixture::PluginFixture` 访问，不重复定义 fixture。

```rust
#[cfg(test)]
#[allow(dead_code)] // Shared fixture: each test crate uses a different subset of fields.
#[path = "refresh/describe/fixture.rs"]
mod fixture;
```

fixture 的 Rust 构造代码见本计划附录 B，Python helper 使用本步全文。

### Step 2：运行旧行为，记录红

- [ ] `cargo test -p yi-agent-supervisors --lib describe_drains_ -- --list` 核对上述两名。
- [ ] `cargo test -p yi-agent-supervisors --lib describe_drains_ -- --nocapture`；旧逻辑两个场景约到 15s 返回 Skipped，Refreshed 断言红。外层 runner 至少 40s；保存输出，不先改全局超时。

### Step 3：实现有界捕获、错误和纯时间判据

- [ ] 按 Interfaces 写类型与 production 常量。`ObservedExit::success` 只认 `code == Some(0) && signal.is_none()`。
- [ ] Capture 用 `Vec::with_capacity(limit)`，字段 `bytes: Vec<u8>, limit: usize, hard_limit: bool, truncated: bool`。下面 append 逻辑同时服务 stdout/stderr，不把软上限变成停止读取：

```rust
fn push(&mut self, chunk: &[u8]) -> Result<(), DescribeError> {
    let remaining = self.limit.saturating_sub(self.bytes.len());
    if self.hard_limit && chunk.len() > remaining {
        return Err(DescribeError::StdoutLimit(self.limit));
    }
    let keep = remaining.min(chunk.len());
    self.bytes.extend_from_slice(&chunk[..keep]);
    self.truncated |= keep < chunk.len();
    Ok(())
}
fn check_limits(
    deadline: std::time::Instant,
    now: std::time::Instant,
    timeout: std::time::Duration,
    cancelled: bool,
) -> Result<(), DescribeError> {
    if cancelled { return Err(DescribeError::Cancelled); }
    if now >= deadline { return Err(DescribeError::Timeout(timeout)); }
    Ok(())
}
```

- [ ] `text()` 严格 UTF-8，不用 lossy；`diagnostic()` 对 stderr 才使用 lossy，取首行 trim，按 chars 截断。常量标记 `" [truncated]"` 的字符数从 512 中扣除，capture 已截断也加标记，最终总字符数仍 ≤512。
- [ ] render 覆盖所有枚举：命令执行阶段、15s/注入 timeout、stdout 限值、退出 code/signal、非 UTF-8、所有权丢失、清理失败；WithCleanup 保留主因后追加清理说明。不输出 stdout。
- [ ] 写单元边界表：stdout 恰好 1,048,576、下一字节 Err；stderr 写到 2 MiB 后保存仅 65,536 且仍可 push；中文首行超长、无换行、空 stderr、非法 UTF-8；到期恰好 `now == deadline` 为 timeout，取消与到期同时成立为 Cancelled。

### Step 4：实现不回收的退出观察和非阻塞 FD

- [ ] process.rs 的 `spawn` 使用下列 Command 设置；successful spawn 后**立刻**把 child 放入 guard，然后 take 两个 pipe；对每个 FD 先 F_GETFL，再 F_SETFL `flags | O_NONBLOCK`。只有 spawn 自身失败时无 child可清理。接管/设flags失败时，显式 `finish()` 并通过 combine 保留初始化主错及清理附错，再返回；不能只依赖不返回诊断的 Drop。Drop仅兜底展开，清理失败时另行warn，且不覆盖panic或二次panic。

```rust
use std::os::unix::process::CommandExt;
let child = std::process::Command::new(command)
    .arg("manifest")
    .process_group(0)
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .map_err(|source| DescribeError::Io { stage: "spawn", source })?;
```

接管后为负组号转换使用 `libc::pid_t::try_from(child.id())`，不得未经检查把u32变负值。guard构造字段初始化不执行用户代码；初始化错误与正常操作共用combine规则。
- [ ] `observe_exit(pid: libc::pid_t) -> std::io::Result<Option<ObservedExit>>` 使用以下主体：

```rust
let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
// SAFETY: positive PID belongs to this ChildSession; info is writable for waitid.
let rc = unsafe {
    libc::waitid(libc::P_PID, pid as libc::id_t, &mut info,
                 libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
};
if rc != 0 { return Err(std::io::Error::last_os_error()); }
// SAFETY: waitid returned a initialized siginfo_t; examine si_pid before event data.
if unsafe { info.si_pid() } == 0 { return Ok(None); }
let status = unsafe { info.si_status() };
match info.si_code {
    libc::CLD_EXITED => Ok(Some(ObservedExit { code: Some(status), signal: None })),
    libc::CLD_KILLED | libc::CLD_DUMPED =>
        Ok(Some(ObservedExit { code: None, signal: Some(status) })),
    _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                               "waitid returned a non-exit event")),
}
```

- [ ] ChildSession::observe 缓存 ObservedExit，但不调用 try_wait。Interrupted 原样交给外层限时循环重查；ECHILD 先清 owns_child/may_signal_group，take 读端，再返回 OwnershipLost；其他 Io 附上 waitid 阶段。
- [ ] 写 `exit_observation_keeps_the_leader_waitable`：spawn `sh -c 'exit 7'` 后，宽预算轮询 observe 为 Some；第二次 observe 仍 Some(7)，finish 才回收；回收后的原生 waitid 为 ECHILD。另测 running 的观察为 None。

### Step 5：实现一次性的组清理与非阻塞异常降级

- [ ] session 每次 finish 都使用同一顺序：若 owns_child=false，仅关闭 pipe、解除能力并结束；否则在 child 未回收时向负 pgid 发 SIGKILL。保存结果后关闭 pipe；未观察退出时向正 child PID 尝试 SIGKILL。然后解除 may_signal_group，再回收。
- [ ] 已观察退出或 child 的 kill 成功：`child.wait()` 回收，Interrupted 重试；信号失败且未确认退出：仅 `child.try_wait()` 一次，不能无条件 wait。组信号 ESRCH 本身是可接受的“组不存在”，不单独制造清理失败，但也不当作child死亡证明：仍按上述规则尝试正PID信号/一次try_wait。正PID信号 ESRCH 只有在一次try_wait确认已退出时才能消解；否则仍报告无法确认回收。其他错误收集为 CleanupFailed。finish所有出口都置finished，解除缓存组信号能力，Drop不得对旧pgid重试。
- [ ] 为决策分支抽一个私有纯函数，不造通用 mock 进程框架：

```rust
#[derive(Debug, PartialEq, Eq)]
enum ReapMode { None, Blocking, NonBlocking }
fn reap_mode(owns_child: bool, observed_exit: bool, child_kill_ok: bool) -> ReapMode {
    if !owns_child { ReapMode::None }
    else if observed_exit || child_kill_ok { ReapMode::Blocking }
    else { ReapMode::NonBlocking }
}
```

生产 finish 必须真正调用该函数。表驱动断言 `(false,false,false)->None`、`(false,true,true)->None`、`(true,true,false)->Blocking`、`(true,false,true)->Blocking`、`(true,false,false)->NonBlocking`。
- [ ] 增加 cfg(test) 的事件观察/错误注入点（仅该模块）：记录 `signal_group/close_pipes/signal_child/disarm_group/reap` 次序；模拟 ECHILD、fcntl 失败、组信号失败、直接信号失败、Interrupted。注入点不增加公开 API，不在生产路径按 env 切行为。
- [ ] 真实 fixture 组清理覆盖：inherited（超时）、inherited+marker触发取消、inherited_closed（成功；后代先关两流但仍活着）。helper在parent退出前等待后代marker，避免还未spawn就断言“后代已死”。读取marker中的PID只用于观察，使用有界 `ps -p <pid> -o stat=` 条件轮询：不存在或Z为已终止；普通活状态为未终止。不得按marker PID盲kill，避免复用误杀；PID复用最多导致测试安全失败。每条测试在fixture目录仍存在时检查后代终止，不能先drop根目录让后代自行退出来伪造产品清理成功。失败退出才用root消失和60s helper backstop收口。
- [ ] 用 `catch_unwind` 故意在持有 session 时 panic，检查 guard 走同一清理次序。成功 finish 后再次 Drop 不产生第二次组信号。

### Step 6：实现双流公平轮询和全阶段限时

- [ ] describe.rs 内声明 `mod process;`。定义 `DrainProgress { bytes: usize, eof: bool, interrupted: bool }`，`drain_once<R: std::io::Read>(reader: &mut R, capture: &mut Capture, policy: &DescribePolicy) -> Result<DrainProgress, DescribeError>` 使用固定8KiB buffer；每次只传 `&mut buf[..policy.read_chunk.min(buf.len()).min(remaining_budget)]` 的预计算长度片段，读至本轮policy.drain_budget_per_stream。私有policy构造保证两个值大于0且read_chunk<=8KiB；生产值仍为8/64KiB。read=0 EOF；WouldBlock返回；Interrupted返回interrupted；其他读错返回Io；每块调用Capture::push，软cap不停止读。
- [ ] `describe` 的操作结果放入内部闭包，再无条件 finish，以便所有 ? 出口都合并清理诊断。骨架如下，里面的函数均由本 Task 定义：

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Checkpoint {
    Start,
    Loop,
    AfterIo { observed_exit: bool, stdout_eof: bool, stderr_eof: bool, bytes_read: usize },
    BeforeSleep,
    Final,
}
fn describe_with_clock(
    command: &str,
    policy: DescribePolicy,
    should_stop: &mut dyn FnMut() -> bool,
    now: &mut dyn FnMut(Checkpoint) -> std::time::Instant,
) -> Result<String, DescribeError> {
    let deadline = now(Checkpoint::Start) + policy.timeout;
    check_limits(deadline, now(Checkpoint::Loop), policy.timeout, should_stop())?;
    let mut session = ChildSession::spawn(command)?;
    let mut out = Capture::new(policy.stdout_limit, true);
    let mut err = Capture::new(policy.stderr_capture_limit, false);
    let mut out_eof = false;
    let mut err_eof = false;
    let operation = (|| -> Result<String, DescribeError> {
        loop {
            check_limits(deadline, now(Checkpoint::Loop), policy.timeout, should_stop())?;
            let mut progress = 0;
            if !out_eof {
                let d = drain_once(session.stdout.as_mut().expect("stdout piped"), &mut out, &policy)?;
                out_eof = d.eof;
                progress += d.bytes;
                if d.interrupted { continue; }
            }
            if !err_eof {
                let d = drain_once(session.stderr.as_mut().expect("stderr piped"), &mut err, &policy)?;
                err_eof = d.eof;
                progress += d.bytes;
                if d.interrupted { continue; }
            }
            let exit = match session.observe() {
                Err(DescribeError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            check_limits(deadline, now(Checkpoint::AfterIo {
                observed_exit: exit.is_some(), stdout_eof: out_eof,
                stderr_eof: err_eof, bytes_read: progress,
            }), policy.timeout, should_stop())?;
            if let Some(exit) = exit {
                if out_eof && err_eof {
                    if !exit.success() {
                        return Err(DescribeError::NonZero { exit, stderr: err.diagnostic() });
                    }
                    return out.text();
                }
            }
            if progress == 0 {
                let remain = deadline.saturating_duration_since(now(Checkpoint::BeforeSleep));
                std::thread::sleep(policy.poll_interval.min(remain));
            }
        }
    })();
    let cleanup = session.finish();
    let result = combine(operation, cleanup);
    if result.is_ok() {
        check_limits(deadline, now(Checkpoint::Final), policy.timeout, should_stop())?;
    }
    result
}
```

- [ ] `combine<T>` 的四种情况写全：Ok+Ok 保留值；Err+Ok 保留主错；Ok+Err 返回清理错；Err+Err 返回 WithCleanup。它不识别 error 字符串。
- [ ] 上述 Checkpoint 与 describe_with_clock 是唯一实现真源，所有执行预算判据调用同一个now；AfterIo携带本轮真实观察，不增加cfg(test)可变全局观察器。生产包装的完整函数如下：

```rust
pub(super) fn describe(
    command: &str,
    policy: DescribePolicy,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<String, DescribeError> {
    describe_with_clock(command, policy, should_stop, &mut |_| std::time::Instant::now())
}
```

可控时钟测试在目标状态前**冻结为base**，不能返回真实Instant::now来消耗短预算。达到AfterIo携带的目标状态后推进到base+timeout并保持；另用真实20s backstop强制结束并断言目标阶段确已达到，防止冻结时钟掩盖卡死。继承管道测试的完整时钟闭包如下（调用本Task定义的executor，不用任何跨crate接口）：

```rust
let base = std::time::Instant::now();
let wall_start = std::time::Instant::now();
let mut reached = false;
let mut expired = false;
let mut clock = |point| {
    if matches!(point, Checkpoint::AfterIo {
        observed_exit: true, stdout_eof, stderr_eof, ..
    } if !stdout_eof || !stderr_eof) && plugin.descendant_marker.exists() {
        reached = true;
        expired = true;
    }
    if wall_start.elapsed() >= std::time::Duration::from_secs(20) { expired = true; }
    if expired { base + policy.timeout } else { base }
};
let result = describe_with_clock(plugin.command.to_str().unwrap(), policy, &mut || false, &mut clock);
assert!(reached, "never reached exited parent with inherited open pipes");
assert!(matches!(result, Err(DescribeError::Timeout(_))));
```

continuous stderr在bytes_read>0的AfterIo推进；eof_live在两个EOF且observed_exit=false时推进；完成后到期仅在Final推进。各测试都断言目标状态布尔值，禁止仅断言最终Timeout。生产时钟从不冻结。
- [ ] 原 refresh.rs 的 describe 替换为兼容包装：调用私有 executor、production policy、永不取消谓词，然后 error.render(command)。生产立即使用新执行器，不能只让测试使用新路径。

### Step 7：验证行为绿、限时红的鉴别力与提交

- [ ] 运行 Task 1 两条旧 API 行为红，必须转绿。
- [ ] 新增 `describe_deadline_covers_inherited_pipes_after_exit`、`describe_times_out_while_output_keeps_arriving`、`describe_rejects_stdout_over_limit_without_rewriting`、`describe_cleanup_stops_its_process_group`、`describe_keeps_bounded_failure_diagnostics`。每条既验证具体 error 类型，也验证原清单未改、capture 上限或真实阶段，不只看耗时。
- [ ] 有界 EOF 场景全部测：两 EOF 但 child 仍活着不能成功；child 已退出但一流未 EOF 不成功；晚完成在最终检查恰好到期不成功；非零 child 也受期限，不进入阻塞读。
- [ ] 变异三项逐个还原再跑：先等退出后读→两条背压测试红；退出后不查 deadline→继承管道测试红；直接 kill child、跳过组→组清理测试红。不能把编译错误当作这些行为变异的红。
- [ ] 删除旧阻塞 wait_with_output 实现与旧 DESCRIBE_TIMEOUT 重复真源，保留其历史注释所需的说明并指向 DescribePolicy::production。新 private module 无 unused/dead_code 警告。
- [ ] 同步 runtime 未完成项中的执行器阶段判据，**仍为 [ ]**：批次和 runtime 取消尚未完成。`cargo fmt --all && just ci` 通过后仅 add Task 1 文件和对应文档，commit `fix: bound plugin manifest description and child cleanup`。

---

## Task 2：取消批次、兼容入口和结构化日志

**Files:** Modify/Test `yi-agent-rs/crates/yi-agent-supervisors/src/refresh.rs`、`refresh/describe.rs`；复用 Task 1 fixture；同步 runtime 未完成项与 bug-list 的阶段证据。

**Interfaces:** Consumes Task 1 `describe`/DescribePolicy/DescribeError；Produces 上文两个公开 until 入口，以及下列私有批次接口：

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshPhase { BeforeLoad, BeforeItem, DescribeCheck, BeforeCompare, BeforeWrite, AfterItem }
struct RefreshRecord {
    outcome: RefreshOutcome,
    cancelled: bool,
    cleanup_warning: Option<String>,
}
// DescribeError::is_cancelled(&self) -> bool
// DescribeError::cleanup_warning(&self) -> Option<String>
// refresh_records(dir: &Path, policy: DescribePolicy,
//                 should_stop: &mut dyn FnMut(RefreshPhase) -> bool) -> Vec<RefreshRecord>
// refresh_one(dir: &Path, stored: &SupervisorManifest, policy: DescribePolicy,
//             should_stop: &mut dyn FnMut(RefreshPhase) -> bool) -> RefreshRecord
// log_records(records: Vec<RefreshRecord>) -> Vec<String>
```

### Step 1：先写取消与不写盘测试

- [ ] `cancelled_refresh_does_not_start_the_next_plugin`：a=hang、b=small，在取消谓词观察 a 的 manifest marker 后返回 true；断言只有 a 的 cancelled Skipped、a 字节未改、b marker 不存在。先前成功 c 项需另用排序更靠前的名称 `0_done`，断言已 Refreshed 的文件保留，b 未开始。

```rust
#[test]
fn cancelled_refresh_does_not_start_the_next_plugin() {
    let fixture = PluginFixture::new();
    let a = fixture.plugin("a", "hang", 0, 0);
    let b = fixture.plugin("b", "small", 0, 0);
    let before = std::fs::read(&a.stored).unwrap();
    let result = refresh_from_plugins_until(
        &fixture.workdir().join(".yi-agent/supervisors"),
        || a.manifest_marker.exists(),
    );
    assert_eq!(result.len(), 1);
    assert!(matches!(&result[0], RefreshOutcome::Skipped { name, reason }
        if name == "a" && reason.contains("cancelled")));
    assert_eq!(std::fs::read(&a.stored).unwrap(), before);
    assert!(!b.manifest_marker.exists());
}
```

测试检查 reason 只是公开结果的断言，不作为生产日志分类。内部另断言 DescribeError 的 Cancelled 类型。
- [ ] 开始即取消→空vector、所有marker不存在。私有RefreshPhase用于稳定观察写回闸门，不新增公开API或cfg(test)全局hook。写回前取消的测试正文如下；Current由已有测试保持不写盘：

```rust
#[test]
fn cancellation_at_the_write_gate_preserves_stored_bytes() {
    let fixture = PluginFixture::new();
    let a = fixture.plugin("a", "small", 0, 0);
    let before = std::fs::read(&a.stored).unwrap();
    let mut reached_write = false;
    let records = refresh_records(
        &fixture.workdir().join(".yi-agent/supervisors"), DescribePolicy::production(),
        &mut |phase| {
            if phase == RefreshPhase::BeforeWrite { reached_write = true; true } else { false }
        },
    );
    assert!(reached_write);
    assert_eq!(records.len(), 1);
    assert!(records[0].cancelled);
    assert!(matches!(&records[0].outcome, RefreshOutcome::Skipped { name, .. } if name == "a"));
    assert_eq!(std::fs::read(&a.stored).unwrap(), before);
}
```
- [ ] 原有六类刷新测试全部保留：Current 不写、同名 stale 更新、命令失败、名称错位、坏 JSON、临时文件清理；不把大 stdout 测试改成仅验证 parse 报错。
- [ ] 先运行并记录缺 API 的编译红；API 补齐后将批次取消检查暂时移除，确认取消/marker 断言的行为红。

### Step 2：实施批次投影与写回闸门

- [ ] error classification 使用递归枚举，而不是 reason：

```rust
pub(super) fn is_cancelled(&self) -> bool {
    match self {
        Self::Cancelled => true,
        Self::WithCleanup { primary, .. } => primary.is_cancelled(),
        _ => false,
    }
}
```

`cleanup_warning` 对 WithCleanup 返回 cleanup 的副本，对单独 CleanupFailed 返回其说明，其他返回 None。
- [ ] refresh_one从executor的typed error构造record；向describe传 `&mut || should_stop(RefreshPhase::DescribeCheck)`。parse/名称校验/写入IO错作为非取消Skipped。比较fresh与stored前查BeforeCompare，write_atomic前查BeforeWrite；已启动项取消也产出record。
- [ ] refresh_records首先查BeforeLoad，为true立即返回空vector，不读目录；随后按load_manifests现有稳定顺序循环，BeforeItem检查、处理后push；若record.cancelled或AfterItem检查为true则break，保留先前成功项，不伪造尚未启动项的Skipped。
- [ ] 两个until入口使用production policy，将公开FnMut()取消谓词适配为私有 `&mut |_| should_stop()`；RefreshPhase不export。旧refresh_from_plugins(dir)等价 `refresh_from_plugins_until(dir, || false)`；旧refresh_and_log同理。移除Task1临时兼容describe包装，所有入口走同一executor。

### Step 3：实现不会被 stderr 伪造的日志

- [ ] log_records 先对 cleanup_warning 单独 warn（即使主因是正常取消也不能掩盖清理失败）；Current 不写日志；Refreshed info 并收集名称；真正 cancelled Skipped debug；其他 Skipped warn。不要从 reason 字串推断级别。
- [ ] 不加 tracing-subscriber 依赖，使用现有 tracing::Subscriber 自定义测试接收器，记录 event metadata.level。完整所需 trait 方法如下：

```rust
#[derive(Clone)]
struct Levels(std::sync::Arc<std::sync::Mutex<Vec<tracing::Level>>>);
impl tracing::Subscriber for Levels {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool { true }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        self.0.lock().unwrap().push(*event.metadata().level());
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}
```

- [ ] fixture nonzero 的 stderr 第一行含 cancelled；with_default 中调用真实 refresh_and_log_until，断言 WARN 且不是仅 DEBUG。正常取消只有 debug（除非清理异常），WithCleanup(Cancelled)另有 WARN，正文超512 chars/原 stdout不进日志。该接收器只断言级别；字段/文本上限由 error/capture 单元测试断言。

### Step 4：验证与提交

- [ ] `cargo test -p yi-agent-supervisors --lib refresh::` 与整个 crate，核对取消前/中/写前、先前成功项保留、后续项未启动、旧结果形状均覆盖。
- [ ] 变异：日志分类改为 `reason.contains("cancelled")`→nonzero fixture 用例红；去掉 write 前 stop 闸门→写回取消用例红；去掉批次 break→b marker 用例红。
- [ ] 同步 docs 中批次/日志判据，仍 [ ] 等待 runtime 接线。门禁通过后 commit `fix: cancel manifest refresh batches without losing stored state`。

---

## Task 3：runtime 真实刷新入口、停机和项目路由收口

**Files:** Modify/Test `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs:161–215` 与其 tests；测试复用 fixture 源文件，不增加 supervisors 的公开测试配置 API；同步 runtime 项与 bug-list。

**Interfaces:** Consumes Task 2 `refresh_and_log_until(dir, should_stop)`；使用已有 serve_with_environment、SuperviseHandle、stop AtomicBool、scope/flags/registered flag 机制。所有已有公共 serve 签名不变。

### Step 1：真实入口的红测试

- [ ] 在 supervise.rs 模块根添加 cfg(test) 源码级 fixture 引用，不添加运行时依赖：

```rust
#[cfg(test)]
#[allow(dead_code)] // Shared fixture: each test crate uses a different subset of fields.
#[path = "../../yi-agent-supervisors/src/refresh/describe/fixture.rs"]
mod refresh_fixture;
```

相对于 `yi-agent-runtime/src/` 回退两层到 crates；fixture 只用 std/tempfile/serde_json，两 crate 已有这些依赖。新 runtime tests 从 `super::refresh_fixture` 取得 PluginFixture。
- [ ] `stopping_during_manifest_refresh_joins_promptly` 从真实 `serve_with_environment` 起 a=hang，传 fixture.arm_flags()、Some("dev")、真实 scope；条件等待 manifest_marker 完整 PID（用现有 SETTLE_BUDGET），然后停止。必须先证明 child 已进入自述，不能用 serve_without_manifest_refresh。

```rust
fn assert_refresh_shutdown(use_drop: bool) {
    let _guard = exclusive();
    let fixture = super::refresh_fixture::PluginFixture::new();
    let a = fixture.plugin("a", "hang", 0, 0);
    let b = fixture.plugin("b", "small", 0, 0);
    let flags = fixture.arm_flags();
    let before = std::fs::read(&a.stored).unwrap();
    let scope = scope_of(fixture.workdir());
    yi_agent_store::ipc::register_plugin_sockets(
        &scope, vec![("a".into(), a.query_socket.clone())],
    );
    let handle = serve_with_environment(
        fixture.workdir(), &scope, Some(flags), Some("dev"),
    );
    assert!(wait_for(&a.manifest_marker, SETTLE_BUDGET));
    let (tx, rx) = std::sync::mpsc::channel();
    let join = std::thread::spawn(move || {
        if use_drop { drop(handle); } else { handle.stop(); }
        let _ = tx.send(());
    });
    let stopped = rx.recv_timeout(std::time::Duration::from_secs(2));
    if stopped.is_err() {
        drop(fixture); // Release helper backstop, never join indefinitely on failure.
        panic!("shutdown did not cancel a running manifest description");
    }
    join.join().unwrap();
    assert_eq!(std::fs::read(&a.stored).unwrap(), before);
    assert!(yi_agent_store::ipc::plugin_socket_for(&scope, "a").is_none());
    assert!(!b.manifest_marker.exists());
    assert!(!b.run_marker.exists());
}
#[test]
fn stopping_during_manifest_refresh_joins_promptly() { assert_refresh_shutdown(false); }
#[test]
fn dropping_during_manifest_refresh_joins_promptly() { assert_refresh_shutdown(true); }
```

成功后断言 a 原 bytes 不变、本项目 plugin_socket_for 为空、b run/manifest marker均未出现。失败分支不先 join；fixture 消失是测试 child 的回收 backstop，不是产品取消的替代。
- [ ] 上述两个测试经同一辅助函数分别验证stop和Drop，均须在--list中出现，不删掉任一分支。
- [ ] `a_slow_description_does_not_block_later_supervision` 同项目 a=hang、b=small，真实刷新等待 a 的生产 15 秒 deadline，b 自述结束并进入正常 run。用至少40s条件轮询证明 b.run_marker、b路由均出现；a.store 原bytes不变，b.store为fresh。
- [ ] b 的路由从 `plugin_socket_for(&scope_of(workdir), "b")` 取得，UnixStream 连该路径并发送 `{"method":"ping"}\n`，读取响应解析后 `ok == true`。设 read/write timeout，不能只靠 socket 文件存在或marker伪造可查询。
- [ ] 新测试与既有 TABLE 锁共用现有 exclusive()，但不把此锁当 nextest 跨进程同步。
- [ ] 先跑 --list；基线 stop 测试红在2s接收，而不是整个测试无限挂起。保留这份输出。

### Step 2：最小生产接线

- [ ] 只把刷新移动到既有 catch 的开头，使用已有 flag；别在 stop 后直接 return，仍落入 cleanup：

```rust
let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    if refresh_manifests {
        yi_agent_supervisors::refresh::refresh_and_log_until(
            &layout.manifests_dir(), || flag.load(Ordering::SeqCst),
        );
    }
    let mut supervisor = Supervisor::new(layout, kind);
    if let Some(path) = flags_path { supervisor = supervisor.with_flags_path(path); }
    while !flag.load(Ordering::SeqCst) {
        supervisor.reconcile();
        yi_agent_store::ipc::register_plugin_sockets(&scope, supervisor.query_sockets());
        std::thread::sleep(RECONCILE_INTERVAL);
    }
    yi_agent_store::ipc::clear_plugin_sockets(&scope);
    supervisor.stop_all();
}));
if outcome.is_err() {
    tracing::error!("plugin supervision loop panicked; plugins are no longer supervised");
    yi_agent_store::ipc::clear_plugin_sockets(&scope);
}
```

外层 stop/join 结构不变，for_daemon_ownership、serve_without_manifest_refresh 的分流不变。
- [ ] 抽出私有 `drive_supervision`（完整代码见附录 C），将本步 catch/循环原样放入其中；`serve_inner` 的 thread closure 调用该驱动，并传入真实刷新闭包。测试直接给同一驱动传一次 panic 闭包，检查本项目清空、其他项目保留。不得新增公开 API、可变全局 hook 或 env 行为分支。真实 stop/Drop 和 A/B 两测试仍经过 serve_with_environment，不用这个注入驱动替代真实接线。

### Step 3：验证、变异和提交

- [ ] `cargo test -p yi-agent-runtime --lib supervise -- --list`，确认两个主要名称及 Drop/路由 panic 用例。
- [ ] 运行整个 supervise 测试和 supervisors crate；去掉 until stop接线→2s停机用例红；改回刷新在catch外→刷新panic路由用例红；stop后early return→已预登记本项目路由清理用例红。
- [ ] 不降低 a 的生产deadline，不新增 pub测试policy，不把新用例改成 serve_without_manifest_refresh。
- [ ] 此 Task 产品接线完成后，先在文档记实际通过的 crate命令；只有 Task4的完整验收都通过才整体 [x]。门禁通过后 commit `fix: interrupt project supervision during manifest refresh`。

---

## Task 4：要求覆盖审计、完整门禁与最终文档

**Files:** Test 上述两个 crate；Modify `docs/bug-list.md`、`docs/project-management/yi-agent-runtime.md`、`docs/project-management/README.md`、本 spec 的实现状态（只引用真实代码/命令，不重写历史证据）。

**Interfaces:** Consumes Tasks 1–3 的测试名与公开until入口；Produces 可核验的最终证据、完成状态与合并提交，不增加产品 API。

### Step 1：逐项对照 spec 验收，不把绿灯代替覆盖

- [ ] 用下表核对 --list 和实际结果，列出缺项；缺项回到对应 Task补测，不用只通过总体计数代替。

| spec §6 验收目标 | 实施责任 | 额外必须核实 |
|---|---|---|
| describe_drains_large_stdout_without_a_false_timeout | Task1 | 256 KiB低于限值，旧入口实际 Refreshed，字段完整 |
| describe_drains_stderr_after_its_capture_limit | Task1 | 2 MiB stderr未阻断stdout，不因capture cap终止 |
| describe_deadline_covers_inherited_pipes_after_exit | Task1 | 已观察parent退出且一流未EOF的到期分支 |
| describe_times_out_while_output_keeps_arriving | Task1 | 持续输出不饿死检查；两流每轮读预算 |
| describe_rejects_stdout_over_limit_without_rewriting | Task1 | 恰好1MiB/超一字节；原stored不被截断覆盖 |
| describe_cleanup_stops_its_process_group | Task1 | 正常/失败/展开；WNOWAIT；ECHILD不信号；kill失败非阻塞 |
| describe_keeps_bounded_failure_diagnostics | Tasks1–2 | UTF-8、512chars含标记、主因保留、stderr取消文字不伪造分类 |
| cancelled_refresh_does_not_start_the_next_plugin | Task2 | 开始/进行中/写前，历史成功项保留，b未开始 |
| stopping_during_manifest_refresh_joins_promptly | Task3 | 真实入口、stop/Drop、2s backstop、项目路由清理 |
| a_slow_description_does_not_block_later_supervision | Task3 | 生产15s，B真实run与响应，40s外层budget |

### Step 2：运行真实实现和平台边界

- [ ] macOS 本机跑下面全部命令；Linux CI跑相同Rust gate，并核实 libc访问器与waitid行为，不能凭macOS C探针冒称Linux机制已验证：

```sh
cd yi-agent-rs
cargo fmt --all
cargo test -p yi-agent-supervisors
cargo test -p yi-agent-runtime --lib supervise
cargo clippy -p yi-agent-supervisors -p yi-agent-runtime --all-targets -- -D warnings
just ci
```

- [ ] 运行正常子进程退出、信号退出、运行中None、WNOWAIT重复观察、最后ECHILD五态；每个spawn都拥有一次可核验的收口。检查新unsafe块的注释和真实FD非阻塞状态。
- [ ] 在目标检查点推进测试时钟：轮询入口、两流读后、退出观察后、成功cleanup后。确认 final check才是成功线性化点，没有清理结束后的到期漏判。
- [ ] 单项变异通过后还原全部生产代码，再跑完整测试；diff里不留下测试注入、日志诊断打印或放宽超时。

### Step 3：按真实结果更新状态并提交

- [ ] bug-list 中本缺陷改 [x]，加准确代码位置与两crate命令、所跑平台、变异结果；所有权异常/逃逸后代仍按spec降级描述，不能写“完全没有孤儿”。
- [ ] runtime 模块该项改 [x]，留下可执行判据；重算README的runtime完成/总数。禁止 [~]；如果门禁失败仍保留 [ ]，不把完成定义降低为“已写代码”。
- [ ] spec头部改为实现已验收，链接交付提交；本plan复选框只在各步实际完成后打勾。历史复现、历史CI限制保留其日期和性质。
- [ ] 文档提交前仍 cargo fmt、just ci、git diff --check；commit `docs: record bounded manifest refresh verification`。

### Step 4：合并与清理

- [ ] 独立审查Tasks1–3和最终diff，所有阻塞意见解决。若在看板卡片会话，按superpowers-kanban声明 `state --self awaiting_merge`，**停止并等待用户批准**；批准后执行 `merge-request <card-id>`，由持有合并名额的回合合并，当前实现回合不得直接git merge。普通非看板分支才按finishing-a-development-branch向用户确认后合并；不得预设本计划授权自动合并。
- [ ] 后续持有合并名额的合并回合在**合并后的树**执行 `cd yi-agent-rs && just ci`。失败则停止发布/清理，保存失败日志和worktree，不用“分支绿过”代替合并结果。合并成功由该回合更新看板done；实现回合不能提前done。
- [ ] 合并回合全绿且这轮开发结束后，先 `cargo clean --dry-run`、`du -sh`、`df -h`，确认本target无人使用，再按工作树归属清理本轮target与分支。看板仍管理或会话仍使用的worktree不得在实现回合自行删除；遵守宿主回收流程。不要操作他人工作区/正式app实例。

## Plan 自检与执行门槛

- spec覆盖：上表10项逐一映射，补充边界、ECHILD、信号失败、非UTF-8、日志分类、展开和项目路由，不漏取消写回或历史成功项。
- 接口一致性：Task1定义 describe/ChildSession；Task2添加分类方法与until入口；Task3消费确切入口，不给runtime注入私有policy。fixture源码共享只发生在cfg(test)，没有生产测试后门。
- 架构边界：新增三个执行器/测试私有文件层级仍属于refresh；不触及安装/卸载或正常run，不新增依赖。
- 未开始实施；本轮只修订spec并写plan。计划自身的结构/命令/链接/接口一致性检查，与将来产品测试验收严格分开。

## 附录 B：测试 fixture 的 Rust 构造

Task1 将上文 Python 全文保存为同一测试模块中的 `const HELPER: &str`（Rust raw string），然后使用以下构造。这些代码为待实现内容，本轮没有把它们写入 Rust 源文件或宣称已编译。fixture 工作目录使用系统临时目录而非深 worktree；套接字若超过103字节即明确测试失败，不改生产回退算法。

```rust
use std::path::{Path, PathBuf};
use std::os::unix::fs::PermissionsExt;

pub(crate) struct PluginFixture { root: tempfile::TempDir }
pub(crate) struct FakePlugin {
    pub command: PathBuf,
    pub stored: PathBuf,
    pub body: PathBuf,
    pub manifest_marker: PathBuf,
    pub descendant_marker: PathBuf,
    pub run_marker: PathBuf,
    pub query_socket: PathBuf,
}
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_str().expect("fixture path UTF-8").replace('\'', "'\\''"))
}
impl PluginFixture {
    pub(crate) fn new() -> Self {
        Self { root: tempfile::tempdir().expect("fixture tempdir") }
    }
    pub(crate) fn workdir(&self) -> &Path { self.root.path() }
    pub(crate) fn arm_flags(&self) -> PathBuf {
        let path = self.workdir().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"superpowers_kanban":true}}"#).unwrap();
        path
    }
    pub(crate) fn plugin(
        &self, name: &str, mode: &str, payload_bytes: usize, exit_code: i32,
    ) -> FakePlugin {
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        let root = self.workdir();
        let manifests = root.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        let p = FakePlugin {
            command: root.join(format!("{name}.sh")),
            stored: manifests.join(format!("{name}.json")),
            body: root.join(format!("{name}.fresh.json")),
            manifest_marker: root.join(format!("{name}.manifest.pid")),
            descendant_marker: root.join(format!("{name}.descendant.pid")),
            run_marker: root.join(format!("{name}.run.pid")),
            query_socket: root.join(format!("{name}.sock")),
        };
        assert!(p.query_socket.as_os_str().len() <= 103, "fixture socket too long");
        let fresh = serde_json::json!({
            "name": name, "command": p.command,
            "args": ["run", p.query_socket.to_str().unwrap(), "x".repeat(payload_bytes)],
            "switch_key": "superpowers_kanban", "stop_when_disabled": true,
            "restart_backoff_ms": 50, "restart_backoff_max_ms": 200,
            "query_socket": p.query_socket,
        });
        let mut stored = fresh.clone();
        stored["stop_when_disabled"] = serde_json::json!(false);
        std::fs::write(&p.stored, serde_json::to_vec(&stored).unwrap()).unwrap();
        std::fs::write(&p.body, serde_json::to_vec(&fresh).unwrap()).unwrap();
        let helper = root.join(format!("{name}.py"));
        let config = root.join(format!("{name}.config.json"));
        std::fs::write(&helper, HELPER).unwrap();
        let cfg = serde_json::json!({
            "root": root, "fresh": serde_json::to_string(&fresh).unwrap(),
            "mode": mode, "exit_code": exit_code,
            "manifest_marker": p.manifest_marker,
            "descendant_marker": p.descendant_marker,
            "run_marker": p.run_marker, "query_socket": p.query_socket,
        });
        std::fs::write(&config, serde_json::to_vec(&cfg).unwrap()).unwrap();
        std::fs::write(&p.command, format!(
            "#!/bin/sh\nexec python3 {} {} \"$@\"\n", shell_quote(&helper), shell_quote(&config)
        )).unwrap();
        std::fs::set_permissions(&p.command, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
}
```

字节限值集成测试用 body 文件补空白到精确大小，然后重新读 config JSON、替换 fresh 为新正文字符串。JSON 尾部空白合法，不把 args 值大小误当整个 stdout 大小。Python helper 的 `print` 还会追加一个换行：补齐正文到 limit-1 才是恰好 limit；limit 正文字节数则输出 limit+1。Capture 单元测试同时直接覆盖这两个字节边界。

## 附录 C：runtime 刷新 panic 的确定性驱动

Task3 的 `serve_inner` 保留创建 stop/layout/scope/kind 的代码，将原 thread closure 换为：

```rust
let join = std::thread::spawn(move || {
    drive_supervision(layout, &scope, kind, flags_path, &flag, |dir, stop| {
        if refresh_manifests {
            yi_agent_supervisors::refresh::refresh_and_log_until(
                dir, || stop.load(Ordering::SeqCst),
            );
        }
    });
});
```

新增以下普通私有函数；panic 测试传入 panic 闭包、stop已经为true，无需启动监督插件。真实生产刷新始终由上述闭包调用，catch的位置不在测试分支里：

```rust
fn drive_supervision(
    layout: Layout,
    scope: &Path,
    kind: String,
    flags_path: Option<std::path::PathBuf>,
    flag: &AtomicBool,
    refresh: impl FnOnce(&Path, &AtomicBool),
) {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        refresh(&layout.manifests_dir(), flag);
        let mut supervisor = Supervisor::new(layout, kind);
        if let Some(path) = flags_path { supervisor = supervisor.with_flags_path(path); }
        while !flag.load(Ordering::SeqCst) {
            supervisor.reconcile();
            yi_agent_store::ipc::register_plugin_sockets(scope, supervisor.query_sockets());
            std::thread::sleep(RECONCILE_INTERVAL);
        }
        yi_agent_store::ipc::clear_plugin_sockets(scope);
        supervisor.stop_all();
    }));
    if outcome.is_err() {
        tracing::error!("plugin supervision loop panicked; plugins are no longer supervised");
        yi_agent_store::ipc::clear_plugin_sockets(scope);
    }
}
```

测试正文放入 supervise::tests，复用 exclusive/scope_of：

```rust
#[test]
fn refresh_panic_clears_only_its_project_routes() {
    let _guard = exclusive();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let a_scope = scope_of(a.path());
    let b_scope = scope_of(b.path());
    yi_agent_store::ipc::register_plugin_sockets(
        &a_scope, [("a".to_owned(), a.path().join("a.sock"))].into_iter().collect(),
    );
    yi_agent_store::ipc::register_plugin_sockets(
        &b_scope, [("b".to_owned(), b.path().join("b.sock"))].into_iter().collect(),
    );
    drive_supervision(
        Layout::for_workdir(a.path()), &a_scope, "dev".into(), None,
        &AtomicBool::new(true), |_, _| panic!("injected refresh panic"),
    );
    assert!(yi_agent_store::ipc::plugin_socket_for(&a_scope, "a").is_none());
    assert_eq!(yi_agent_store::ipc::plugin_socket_for(&b_scope, "b"), Some(b.path().join("b.sock")));
    yi_agent_store::ipc::clear_plugin_sockets(&b_scope);
}
```

另一个停止清理用例传空刷新闭包而非panic，同样预登记两个项目、stop为true，验证无对账时仍清本项目。此用例只检验收口分支，不替代真实入口的2秒停机验收。

## 本轮文档检查记录（非实现验收）

本轮仅修订spec/plan并更新原未完成项链接，未实现生产修复。此前现有测试supervisors为64单元+23集成通过，runtime supervise为7通过；原始完整CI曾有2条subagent_real_e2e失败，故上一轮暂缓提交，该历史结果不删除。

本次用户要求的追加复核已完成三轮：

1. 源码/接口/交付流程：确认与83a60f02的目标源码兼容；修正看板自带worktree与合并名额，不在卡片回合直接merge或删除宿主工作树。
2. 错误与清理：修正FD初始化失败诊断、组ESRCH降级、成功后残留后代的可观测夹具、共享测试fixture的局部dead_code豁免。
3. 测试可执行性：冻结注入时钟，AfterIo携带实际观测；增加私有RefreshPhase用于写回前取消；补齐stderr背压测试代码。链接、十项验收覆盖、接口命名、状态计数24/27与diff空白检查通过。

将附录B的Rust构造、Python helper及附录C的runtime驱动/路由panic测试抽取到临时探针：Rust编译成功，small/noisy/inherited_closed/nonzero/invalid_utf8、自述与run marker隔离、真实Unix socket响应、root删除backstop及panic路由隔离均运行通过。未实现的ChildSession/执行器并没有因此被验证为正确。

更新基线后的 `cargo fmt --all && just ci` **exit0**：4079项测试全通过、28跳过，MSRV1.89与剩余门禁通过。设置NEXTEST_TEST_THREADS=4只降低并发，不过滤测试、不更改超时、不启用重试。完整日志在文档worktree的plan-ci-review.log；原失败日志plan-ci.log保留为历史。不把文档CI全绿写成产品修复完成；需求状态保持 [ ]。
