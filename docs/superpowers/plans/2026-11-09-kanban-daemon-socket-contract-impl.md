# Kanban Daemon Socket Contract Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复看板插件与新宿主业务 daemon 地址错配，并使 Status 探活有界、可诊断、受跨端门禁保护。

**Architecture:** 插件在独立 IPC crate 中同步宿主路径算法，公共 fixture 与跨进程真实函数对照防止再次漂移，不链接宿主 crate。仅 Status 使用非阻塞且共享 deadline 的 IO；runner 保持三次连续失败策略，提前报告解析错误。查询 socket 和卡片状态机不改。

**Tech Stack:** Rust edition 2024 / 插件 rust-version 1.85；现有 std、sha2、serde/serde_json、libc、tempfile；Bash、Python 3 unittest、just；Linux 与 macOS。

**Revision:** rev2（2026-11-09 plan 专项复审）；[复审与验证记录](../reviews/2026-11-09-kanban-daemon-socket-plan-review.md)。当前未发现未决计划阻断项，不代表实现或部署已通过。

## 审查/执行清单

- [x] 接口、可见性、示例编译与源码一致性复核。
- [x] 测试身份、时间预算、私有接缝和信号负例复核。
- [x] gate失败传播、零用例、环境及CI依赖复核。
- [x] 逐项修订后重走Task1–8/A01–A14覆盖与文档验证。
- [ ] 执行任何实现任务或部署验收（本轮未授权）。

## Global Constraints

- 权威 spec：[rev3](../specs/2026-11-09-kanban-daemon-socket-contract-design.md)，基线 `dcfad0d6`；本计划未执行。
- 宿主与插件不得新增相互的生产或 dev-dependency；公共 fixture 是数据，不反向 include 插件源码。
- 业务地址 `<instance_home>/sockets/<canonical-runtime-path-sha256-first-16-hex>/runtime.sock`；路径上限 103 个 Unix 原始字节。
- 实例优先级：非空 YI_AGENT_INSTANCE_HOME（原样）→非空 YI_AGENT_HOME（末级不是 .yi-agent 时追加）→非空 HOME/.yi-agent→明确报错。
- 单次 Status 连接/写/读共享 2 秒不可续期预算；合法匹配响应含业务 Error 均证明通信；连续失败阈值 3。
- 不 bump 协议 3；不改查询布局，不回退旧 daemon 路径、不改 PATH/授权、控制客户端、查询服务端或看板数据格式。
- 不读生产 ~/.yi-agent、不投递真实卡、不调用付费 API；正式安装/重启必须另获用户许可。
- 禁止在 main 编辑。文档分支保留；执行时从含本 spec/plan 的提交创建 `fix/kanban-daemon-socket` worktree，按 using-git-worktrees 流程，不自动 merge/push/入队。
- 代码提交前在宿主跑 cargo fmt --all 与 just ci；插件亦格式检查和 plugin-check。合并获准后复跑。每个任务 commit 同步模块证据，功能未全面验收保持 `[ ]`。
- 所有命令工作目录为执行 worktree 根；本机 PATH 使用 `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"`。CI 使用配置的 Rust 工具链，不写本机路径。

- 私有接缝只位于IPC crate：`exchange` 用 `pub(super)` 供兄弟client模块调用；`connect_before` / `wait_ready` / `exchange_connected` / `ProbeIo` 留在status_io私有范围，其直接测试和signal worker放同模块 `#[cfg(test)] mod tests`，外部integration只调用公开Status入口，不扩大生产API。
- TDD口径：新接口缺失可用编译失败作首次红证据；已经正确实现的行为测试第一次可以绿，不能写“必须先红”并伪造结果。每一关键防回归行为用受控变异取得真实红证据；所有红绿命令记录确切测试名/退出码。
- 进程与日志口径：stub的请求计数只在解码完整RequestEnvelope且command为Status后增加，不把accept/EOF视为请求；每条失败诊断从stderr稳定文件解析，不能用成功响应数量代替失败次数。

---

## 文件职责与任务顺序

| 文件 | 职责 | 任务 |
|---|---|---|
| plugins/contracts/fixtures/daemon-socket.json | 中立路径数据契约 | 1 |
| plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/daemon_socket.rs | 环境、规范化、地址与业务错误 | 1 |
| 同 crate src/socket.rs / src/client.rs / src/lib.rs | 保留查询规则，切换业务入口、导出模块 | 1、3、4 |
| 双侧 tests/daemon_socket_contract.rs | 本侧 fixture、插件跨端生产函数对照 | 1、2 |
| yi-agent-rs/crates/yi-agent-lifecycle/examples/daemon_socket_contract_probe.rs | 无业务知识的测试探针 | 2 |
| IPC src/status_io.rs | 首次 connect 前非阻塞、预算、完整帧 | 3、4 |
| runner src/client.rs / src/single_instance.rs / src/main.rs | probe、计数、诊断、启动顺序 | 5 |
| runner tests/daemon_liveness.rs / tests/support/daemon.rs | 隔离 CLI 子进程与只读桩 | 6 |
| scripts/plugins/check-daemon-socket-contract.sh / check-daemon-socket-wiring.py / test_check_daemon_socket_contract.py | 必执行 gate、静态接线与负例 | 7 |
| yi-agent-rs/justfile / .github/workflows/ci.yml / .gitlab-ci.yml | 两平台调用链与Python依赖 | 7 |
| plugins/contracts/README.md / plugins/superpowers-kanban/INSTALL.md / docs/project-management/{superpowers-kanban.md,README.md} | 契约、排障、证据 | 各任务、8 |
| docs/superpowers/reports/2026-11-09-kanban-daemon-socket-validation.md | 实现后的脱敏实测，不预填通过 | 8 |

依赖：1→2；1→3→4→5→6；2+6→7→8。跨端与 IO 可由独立 reviewer 分别核查，涉及同一 client.rs 的编辑串行整合。

## Task 1: 公共路径契约与插件业务地址解析（A01–A09）

**Files:**
- Create: `plugins/contracts/fixtures/daemon-socket.json`
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/daemon_socket.rs`
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/tests/daemon_socket_contract.rs`
- Create: `yi-agent-rs/crates/yi-agent-lifecycle/tests/daemon_socket_contract.rs`
- Modify: IPC `src/lib.rs`（模块表）、`src/socket.rs::daemon_socket_for`（只替换业务分支）、`src/client.rs::socket_path` 及旧布局单测。
- Modify: `plugins/contracts/README.md`、`docs/project-management/superpowers-kanban.md`。

**Interfaces:**
```rust
// daemon_socket.rs，公开纯入口及环境包装；不与查询 SocketPathError 共用。
#[derive(Debug)]
pub enum DaemonSocketError {
    InstanceHomeUnavailable,
    PathTooLong { bytes: usize, limit: usize, path: std::path::PathBuf },
}
pub fn instance_home_from_values(
    instance: Option<&std::ffi::OsStr>,
    override_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<std::path::PathBuf, DaemonSocketError>;
pub fn daemon_socket_with_home(
    runtime: &std::path::Path, home: &std::path::Path,
) -> Result<std::path::PathBuf, DaemonSocketError>;
pub fn daemon_socket_for(runtime: &std::path::Path)
    -> Result<std::path::PathBuf, DaemonSocketError>;
// socket::daemon_socket_for 与 client::socket_path 兼容名称转调，返回上述业务错误。
```
实现 std::error::Error / Display；超长显示候选地址与 bytes/103，缺 home 说明三个环境来源均不可用。不修改宿主算法。

- [ ] **Step 1: 锁公共数据及独立固定向量。** JSON 顶层字段固定为 `algorithm: "sha256-canonical-runtime-first8"`、`instance_priority`（三个变量名数组）、`override_suffix: ".yi-agent"`、`socket_directory: "sockets"`、`socket_file: "runtime.sock"`、`max_socket_path_bytes: 103`、`vectors`、`case_ids`。向量至少包含 `{ "runtime_hex": "2f", "digest16": "8a5edab282632443" }`（根目录在两平台稳定）。case_ids：`short,deep,instance_priority,override_suffix,empty,relative,symlink,dot_segments,symlink_dotdot,missing_tail,isolated,tmpdir,non_utf8,deleted_cwd,limit_103,limit_104`。fixture 读入类型用 serde 的 deny_unknown_fields，拒绝重复/缺 case ID；不是 Node checker 自动支持新 fixture 的假设。

- [ ] **Step 2: 写第一条红测试并运行。** 在插件集成测试写：
```rust
#[test]
fn short_and_deep_use_instance_socket_directory() {
    use superpowers_kanban_ipc::daemon_socket::daemon_socket_with_home;
    let home = std::path::Path::new("/");
    assert_eq!(daemon_socket_with_home(std::path::Path::new("/"), home).unwrap(),
        std::path::PathBuf::from("/sockets/8a5edab282632443/runtime.sock"));
    let root = tempfile::Builder::new().prefix("kd").tempdir_in("/tmp").unwrap();
    let h = root.path().join("i");
    let mut deep = root.path().to_path_buf();
    for _ in 0..40 { deep.push("deep"); }
    for runtime in [root.path().join("r"), deep.join("r")] {
        let path = daemon_socket_with_home(&runtime, &h).unwrap();
        assert!(path.starts_with(std::fs::canonicalize(root.path()).unwrap().join("i/sockets")));
        assert!(path.ends_with("runtime.sock"));
        assert!(path.as_os_str().as_encoded_bytes().len() <= 103);
    }
}
```
运行 `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract short_and_deep_use_instance_socket_directory -- --exact`；先因新模块缺失失败。将 tempdir 构造统一改为 `tempfile::Builder::new().prefix("kd").tempdir_in("/tmp")` 并 canonicalize 实际根；断言候选字节<=103，fixture 不硬编码 /tmp hash。

- [ ] **Step 3: 实现纯环境优先级、规范化及包装。** 核心代码：
```rust
fn nonempty(value: Option<&std::ffi::OsStr>) -> Option<&std::ffi::OsStr> {
    value.filter(|s| !s.is_empty())
}
if let Some(value) = nonempty(instance) { return Ok(value.into()); }
if let Some(value) = nonempty(override_home) {
    let p = std::path::PathBuf::from(value);
    return Ok(if p.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
        p
    } else { p.join(".yi-agent") });
}
nonempty(home).map(|h| std::path::PathBuf::from(h).join(".yi-agent"))
    .ok_or(DaemonSocketError::InstanceHomeUnavailable)
```
`canonicalize_allowing_missing` 按宿主 scope.rs 顺序实现（绝对化→词法点段→canonicalize 全路径→寻找已存在祖先追加尾部）；内部返回 io::Result，调用点 `.unwrap_or_else(|_| original.to_path_buf())`。用 Sha256::digest(canonical_runtime.as_os_str().as_encoded_bytes())，前8字节 lowercase hex；canonical_home.join("sockets").join(hash).join("runtime.sock")，最终原始 bytes >103 报错。生产包装读取三个 var_os 后借用传纯函数，不全局 set_var。原 socket 查询分支一个字不改。

- [ ] **Step 4: 逐项补红绿路径测试。** 每新增一组测试先运行该名称记录真实结果；若已绿，不伪造红，用仅破坏该行为的变异确认它会拦截回归，再恢复绿。fixture Case IDs 必须有消费断言。
  - `instance_priority_and_suffix_are_exact`：显式三值、空 INSTANCE、空 override、仅 HOME、全空；INSTANCE 即使未带后缀也不补。
  - `canonical_spelling_and_missing_tail_match`：runtime/home 各自 symlink、相对+点段、symlink+.. 先词法处理、symlink祖先下缺失尾部创建前后一致；相对/cwd专用子进程见Task2，不在本测试进程 chdir。
  - `instances_and_runtimes_are_isolated`：换 runtime/home 各变地址；TMPDIR影响只在环境子进程测。
  - `non_utf8_and_length_are_raw_bytes`：OsStringExt::from_vec 将0xff放目录名；长home生成候选恰好103与104，home绝对且不存在末级，先canonicalize父根；104断言 bytes=104，不用 display 长度。
  - 双侧 `root_vector_and_fixture_schema_are_pinned`：读取同一 JSON 调真实 daemon_socket，根目录固定输出；宿主在本侧用 scope::instance_home_from_env 验证低层后缀，全优先级留Task2。插件 include_str 路径 `../../../../contracts/fixtures/daemon-socket.json`；宿主 `../../../../plugins/contracts/fixtures/daemon-socket.json`（均从 tests 文件位置计算）。

- [ ] **Step 5: 清除所有旧 daemon 布局断言并复跑查询回归。** 原 `client.rs::the_socket_lives_beside_its_runtime_directory` 不能再用继承 HOME 测，改为纯入口新布局；`socket.rs::the_daemon_rule_matches_the_host` / `a_short_runtime_dir_keeps_the_socket_inside_it` 替换，查询原 fixture / 深路径 bind 保持。同步socket.rs模块表与兼容wrapper、client::socket_path注释，删除旧daemon直通/TMPDIR地址描述；仅更新文案，不改变查询实现/SocketPathError/原fixture。
```bash
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle --test daemon_socket_contract
```
预期全绿，无“仅因函数存在就跳过”的分支。

- [ ] **Step 6: 文档证据和提交。** 模块记录Task1命令、测试名和实际结果但不勾功能。运行两个 workspace fmt、宿主 just ci、插件 plugin-check；`git diff --check` 后只 add 本任务文件与模块，提交 `fix: align kanban daemon socket resolution with host`。完整门禁失败先诊断，不能以单测绿代替提交前规则。

## Task 2: 跨进程真实环境/路径对照（A02–A08、A14）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-lifecycle/examples/daemon_socket_contract_probe.rs`
- Modify/Test: 双侧 `tests/daemon_socket_contract.rs`，插件此文件添加环境 worker。
- Modify: `docs/project-management/superpowers-kanban.md`。

**Interfaces:**
- Host example CLI：`daemon_socket_contract_probe <runtime_hex> [--delete-cwd]`。十六进制解码为 Unix OsString，不接受奇数/非法 hex。可选开关在 child 已启动后仅 remove_dir(current_dir)，测试传入专用空目录；绝不删父项目目录。
- 输出单行 JSON：成功 `{"socket_hex":"2f736f636b6574732f386135656461623238323633323434332f72756e74696d652e736f636b","error":null}`（root向量示例）；缺home `{"socket_hex":null,"error":"instance_home_unavailable"}`；超限再含 `bytes`/`limit`/候选 `socket_hex`，`error:"path_too_long"`。业务解析错误作为输出正常 exit0，CLI/IO/非法输入 exit1；测试显式比较 error，不能只比较退出码。
- 插件 worker 测试名 `plugin_environment_worker`；输入 `KANBAN_CONTRACT_RUNTIME_HEX`、可选 `KANBAN_CONTRACT_DELETE_CWD=1`，调用真实 daemon_socket_for，stdout 唯一标记 `KANBAN_CONTRACT_RESULT=<一行JSON>`。没有worker输入时正常返回仅供常规 cargo test（不算跨端成功）。worker遵守与host同样的空目录删除约束。
- 必执行跨端测试名 `cross_process_daemon_socket_contract`，ignored原因 `requires separately built host probe`；消耗 `KANBAN_HOST_SOCKET_PROBE`（绝对文件路径），缺失必须 panic。

- [ ] **Step 1: 写缺helper和真实对照红测试。**
```rust
#[test]
#[ignore = "requires separately built host probe"]
fn cross_process_daemon_socket_contract() {
    let probe = std::env::var_os("KANBAN_HOST_SOCKET_PROBE")
        .expect("KANBAN_HOST_SOCKET_PROBE is required; no skip permitted");
    assert!(std::path::Path::new(&probe).is_file(), "host probe missing");
    // 测试随后遍历 fixture 每个case；调用下述run_contract_child并比较完整JSON投影。
    let mut command = std::process::Command::new(&probe);
    command.arg("2f").env_clear().env("YI_AGENT_INSTANCE_HOME", "/");
    let output = run_contract_child(command, std::time::Duration::from_secs(5));
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["socket_hex"], serde_json::Value::String(
        "/sockets/8a5edab282632443/runtime.sock".as_bytes().iter()
            .map(|b| format!("{b:02x}")).collect()));
}
```
`cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract cross_process_daemon_socket_contract -- --ignored --exact`：未传helper必须失败；不得把它改成可选test。

- [ ] **Step 2: 实现host probe与worker。** Host 使用 `scope::instance_home_for_sockets()`（Option）与 `scope::daemon_socket(runtime,home)`（PathBuf）；再按 `scope::MAX_SOCKET_PATH_BYTES` 校验原始字节，不能声称调用了store长度wrapper。hex编解码在各测试程序内局部实现，用 chunks_exact(2)/u8::from_str_radix，避免新增依赖。worker从current_exe起 `--exact plugin_environment_worker --nocapture`，解析标记行且只允许一条标记；parent断言test退出成功且标记存在，不拿libtest其他输出当JSON。

- [ ] **Step 3: 添加有界child helper并覆盖所有Case IDs。** `run_contract_child(mut command: std::process::Command, timeout: Duration) -> std::process::Output`：stdout/stderr重定向独立tempfile（防pipe填满），spawn后每10ms try_wait；超过5秒kill+wait，读取文件后panic含stderr；正常退出wait收尸。RAII guard在assert panic时亦kill/wait。
```rust
let mut command = std::process::Command::new(std::env::current_exe().unwrap());
command.args(["--exact", "plugin_environment_worker", "--nocapture"])
    .env_clear().env("KANBAN_CONTRACT_RUNTIME_HEX", runtime_hex)
    .env("YI_AGENT_INSTANCE_HOME", instance_value)
    .current_dir(&dedicated_child_cwd);
```
每个case使用同一个canonical cwd，host与插件child顺序运行，不能用两个不同目录假称同一runtime。删除cwdcase使用相同相对runtime字节：创建专用空cwd→host删除自身cwd并调用生产入口→parent在相同路径重建空cwd→插件child删除并调用；两child结束后才清理。非删除case也共用同一cwd，relative home/override 的语义才能真实比较。实例home绝对，两child相对runtime回退原输入的hash必须一致。其他case提供相同canonical短根环境，遍历INSTANCE/override/HOME、空值、TMPDIR变化、dot/symlink/非UTF8及103/104；比较address原始hex或明确错误类别/bytes，不仅比较成功root示例；断言已消费case_ids集合完全相等。

- [ ] **Step 4: 独立构建探针，再执行cross test。**
```bash
cargo build --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle --example daemon_socket_contract_probe
HOST_TARGET=$(cargo metadata --manifest-path yi-agent-rs/Cargo.toml --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
KANBAN_HOST_SOCKET_PROBE="$HOST_TARGET/debug/examples/daemon_socket_contract_probe" cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract cross_process_daemon_socket_contract -- --ignored --exact --nocapture
```
预期明确1 passed/0 ignored；改成假helper（exit0输出 `{}`）必须失败。进程测试不调用cargo build，构建只由外部脚本负责。

- [ ] **Step 5: 审查依赖边界并提交。** 两边 Cargo.toml/Cargo.lock无新增跨端依赖；跑plugin-boundary-check及Task1提交前完整验证序列，模块附Task2证据，提交 `test: compare daemon socket contracts across processes`。

## Task 3: 首次connect之前非阻塞且有界（A08、A12）

**Files:**
- Create: IPC `src/status_io.rs`。
- Modify: IPC `src/lib.rs`（私有 mod status_io）、`src/client.rs::ClientError`。
- Test: `src/status_io.rs` 内unit tests；不改宿主client.rs。

**Interfaces:**
```rust
// client.rs公开，供runner诊断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeStage { Connect, Write, Read }
// 在既有ClientError增加 Timeout { stage: ProbeStage }，其他variants保持。
// status_io.rs私有；fd由OwnedFd负责关闭。
fn connect_before(socket: &std::path::Path, deadline: std::time::Instant)
    -> Result<std::os::fd::OwnedFd, crate::client::ClientError>;
fn wait_ready(fd: std::os::fd::RawFd, events: libc::c_short,
    deadline: std::time::Instant, stage: crate::client::ProbeStage)
    -> Result<libc::c_short, crate::client::ClientError>;
```

- [ ] **Step 1: 写红connect预算及NUL测试。** `nul_is_rejected_before_connect` 使用 OsStringExt::from_vec(b"/tmp/a\0b".to_vec())，断言 Io.kind InvalidInput；`connect_backlog_obeys_deadline` 创建短根 listener，用非阻塞原生connect填满accept backlog，deadline=100ms，再调用connect_before，断言Timeout Connect且elapsed<1秒。积压构造最多1秒，未形成EAGAIN/EINPROGRESS必须报失败不是skip。Linux验证EAGAIN，macOS验证实际pending errno；同步测试接缝额外证明有界重试，不能替代真实积压覆盖或以平台差异放行gate。Run `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc status_io::tests -- --test-threads=1`，先因模块/方法缺失红。

- [ ] **Step 2: 实现地址、fd与readiness。** 实现 std::fmt::Display：ProbeStage显示小写 `connect` / `write` / `read`，ClientError::Timeout显示 `ipc probe timeout during {stage}`，保留既有所有错误variants的Display分支。使用zeroed sockaddr_un、检查原始bytes<=103且不含0；复制bytes且末尾NUL，socklen=`offset_of!(libc::sockaddr_un,sun_path)+bytes.len()+1`，macOS设置sun_len。socket syscall返回负fd先报last_os_error，不能对-1构造OwnedFd；合法fd立即OwnedFd::from_raw_fd；fcntl F_GETFL/F_SETFL O_NONBLOCK、F_GETFD/F_SETFD FD_CLOEXEC，任一失败drop。macOS SO_NOSIGPIPE在首次send前配置。所有unsafe块注明地址初始化、指针寿命与fd唯一所有者。
```rust
let remaining = deadline.saturating_duration_since(std::time::Instant::now());
if remaining.is_zero() { return Err(ClientError::Timeout { stage }); }
let ms = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
let mut item = libc::pollfd { fd, events, revents: 0 };
let rc = unsafe { libc::poll(&mut item, 1, ms) };
```
poll前/返回后重查同deadline；EINTR重试，不延长；POLLNVAL报InvalidInput/EBADF，HUP/ERR返回revents给连接/读写处理，不先丢数据。rc=0重查deadline，不对剩余时间做无界block。

- [ ] **Step 3: 实现connect状态机。** 0立即成功（返回前再查deadline）；EINPROGRESS/EALREADY按pending处理，poll后getsockopt(SO_ERROR)，0才成功，非0保留errno。EINTR先重查deadline，关闭当前fd并重新创建/连接，不能将可能未建立连接的fd直接按SO_ERROR=0认作成功。Linux EAGAIN/EWOULDBLOCK为积压：drop此fd，在剩余deadline内sleep最多10ms后新fd重试；不能poll一个未连fd并把SO_ERROR0当成功。EISCONN成功；其他errno立即返回。deadline每次新fd前后及poll后都检查。

- [ ] **Step 4: 红绿验证并添加确定性中断接缝。** `connect_retries_keep_original_deadline` 将“新fd尝试”的私有闭包输入精确到 `(deadline: Instant) -> io::Result<OwnedFd>`，测试返回EAGAIN并记录调用deadline相同；`poll_eintr_keeps_deadline` 窄poll syscall封装接缝连续EINTR直到原deadline，超时Connect；`failed_attempts_close_descriptors` 优先用可注入syscall封装记录每次open/close的配对与失败路径（socket失败不创建guard、fcntl/connect/poll/timeout失败恰好关闭一次）；真实OwnedFd的EBADF检查只在独占测试且关闭后不再open任何fd时做，不能用进程全局fd数量或已被复用的fd号作为证据。真实积压与fake并用，不能只测假clock。

- [ ] **Step 5: 完整验证及提交。** Task1/2的路径和跨端test回归，两个workspace fmt+宿主just ci+插件plugin-check；模块记录connect测试证据，提交 `fix: bound status socket connection before first connect`。

## Task 4: 完整帧读写共享deadline（A11、A12）

**Files:**
- Modify: IPC `src/status_io.rs`、`src/client.rs`。
- Create/Test: IPC `tests/status_budget.rs`。
- Modify: `docs/project-management/superpowers-kanban.md`。

**Interfaces:**
```rust
// client.rs公开，只允许Status，不接任意Command。
pub fn send_status_with_budget(socket: &std::path::Path, budget: std::time::Duration)
    -> Result<crate::wire::Reply, ClientError>;
// status_io.rs私有，消耗Task3 connect_before；client负责ID与编解码。
pub(super) fn exchange(socket: &std::path::Path, request: &[u8], deadline: std::time::Instant)
    -> Result<Vec<u8>, crate::client::ClientError>;
trait ProbeIo {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<usize>;
    fn recv(&mut self, bytes: &mut [u8]) -> std::io::Result<usize>;
    fn wait(&mut self, write: bool, deadline: std::time::Instant)
        -> Result<(), crate::client::ClientError>;
}
fn exchange_connected(io: &mut impl ProbeIo, request: &[u8], deadline: std::time::Instant)
    -> Result<Vec<u8>, crate::client::ClientError>;
```
真实实现持OwnedFd，recv/send非阻塞；fake仅在unit tests，不暴露SDK。

- [ ] **Step 1: 写沉默桩红测试。** 测试server线程accept后读一行并等待shutdown channel，不无限sleep；listener也非阻塞且有accept截止时间，Drop发stop并join。`silent_peer_times_out_during_read` 调预算100ms，断言Timeout Read且elapsed<1秒；`drip_feed_cannot_renew_budget` 每20ms发1字节超过100ms，仍在原deadline失败。Run `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test status_budget`：新公开函数缺失红。

- [ ] **Step 2: 实现Status入口和完整帧执行器。**
```rust
let started = std::time::Instant::now();
let deadline = started.checked_add(budget).ok_or_else(|| ClientError::Io(
    std::io::Error::new(std::io::ErrorKind::InvalidInput, "probe budget overflow")))?;
let request_id = next_request_id();
let request = encode_request(&request_id, Command::Status)?;
let response = crate::status_io::exchange(socket, &request, deadline)?;
decode_response(&response, &request_id)
```
write offset直到request长度，0→WriteZero；WouldBlock→wait(write=true)，Interrupted→重查，其他errno透传。read只读最多剩余MAX_FRAME_BYTES+1，查首LF，返回完整首frame；EOF无LF→Truncated；达到上限且无LF→FrameTooLarge；如LF恰在MAX_FRAME_BYTES位置则接受（JSON字节上限不含LF）。每次IO前后检查deadline，缓冲不超过MAX+1，不用read_until无界累积。Linux send MSG_NOSIGNAL，macOS沿Task3 SO_NOSIGPIPE，不能全局signal忽略。

- [ ] **Step 3: fake IO红绿补部分写与中断。** `short_writes_and_interrupted_io_complete_one_frame`：fake send先Interrupted，再每次1字节，recv先WouldBlock再2字节，wait记录同一deadline，最终request字节与encode_request完全一致；`write_stall_times_out_at_write_stage`：fake永远WouldBlock，wait到deadline返回Timeout Write；`hup_with_complete_frame_is_not_truncated`：真实peer write完整帧后shutdown，仍成功；`broken_peer_does_not_raise_sigpipe` 位于 `status_io::tests`：用current_exe带 `--exact status_io::tests::sigpipe_worker --nocapture --test-threads=1` 只跑worker（CONNECTED必须即时可见，不能被libtest捕获），env_clear传专用socket路径；worker进入后先 `unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }`（只改child信号，Rust启动默认忽略不能作为测试条件），调用connect_before拿fd，stdout发并flush `CONNECTED`，等待父进程stdin一个GO字节。parent有5秒watchdog，观察CONNECTED后accept peer、shutdown(Both)+drop，再写GO；worker通过exchange_connected发送大frame直到出错，断言Io BrokenPipe/ConnectionReset，正常退出而非signal13。worker输入缺失时正常返回，不计负例；parent必须看到CONNECTED及正常退出。将局部防SIGPIPE去掉并独占运行此测试时child必须signal13失败，再恢复绿。仅Status公开入口小frame，短写/写背压必须通过私有执行器接缝，不捏造Status一定填满缓冲。

- [ ] **Step 4: 补负帧和成功语义。** 测试名 `valid_status_and_business_error_are_replies`、`truncated_frame_is_rejected`、`malformed_frame_is_rejected`、`oversized_stream_is_bounded`、`protocol_and_request_id_mismatches_are_rejected`、`frame_limit_excludes_newline`。桩读取RequestEnvelope取真实ID，按PROTOCOL_VERSION构造ResponseEnvelope；合法Error保留Reply::Error而非探活失败。MAX/ MAX+1体积用event padding或未知Reply生成合法JSON，不只用invalid repeated x测长度；无LF超限也测。gate中每测试错误variant精确断言。

- [ ] **Step 5: 全回归和提交。**
```bash
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc
```
Task2 cross gate明确运行；两个workspace fmt、宿主just ci、插件plugin-check，模块证据提交 `fix: enforce one deadline for status frame io`。

## Task 5: Runner诊断与无副作用解析失败（A10–A13）

**Files:**
- Modify: runner `src/client.rs::BoardDaemon`、`src/single_instance.rs::Liveness`、`src/main.rs::run_daemon`。
- Test: runner client/single_instance unit tests；进程验收Task6。
- Modify: `docs/project-management/superpowers-kanban.md`。

**Interfaces:**
```rust
impl BoardDaemon {
    pub fn probe(&self) -> Result<(), superpowers_kanban_ipc::client::ClientError>;
    pub fn socket(&self) -> &std::path::Path;
}
impl Liveness { pub fn misses(&self) -> u32; }
```
删除is_alive并调整全部实际调用点；create_session/task_state仍使用原send，不改其预算/返回语义。

- [ ] **Step 1: 红测试计数和probe投影。**
```rust
#[test]
fn misses_accessor_tracks_only_consecutive_failures() {
    let mut l = Liveness::default();
    assert!(!l.observe(false, 3)); assert_eq!(l.misses(), 1);
    assert!(!l.observe(true, 3)); assert_eq!(l.misses(), 0);
    assert!(!l.observe(false, 3)); assert!(!l.observe(false, 3));
    assert!(l.observe(false, 3)); assert_eq!(l.misses(), 3);
}
```
runner client unit `probe_preserves_error_and_accepts_business_error` 不引用Task4 crate的私有测试桩，而在本 `client::tests` 内构造独立短根UnixListener：非阻塞accept最多1秒；accept后设stream blocking且read/write timeout各1秒；BufReader读取受控Status一行、反序列化RequestEnvelope，生成匹配ID的合法Reply::Error后write_all。核心响应代码如下（server线程持stream与request，父test持同一短根socket）：
```rust
let reply = superpowers_kanban_ipc::wire::ResponseEnvelope {
    protocol_version: superpowers_kanban_ipc::wire::PROTOCOL_VERSION,
    request_id: request.request_id,
    event_id: None, event: None,
    result: superpowers_kanban_ipc::wire::Reply::Error {
        code: "TestOnly".into(), message: None,
    },
};
let mut bytes = serde_json::to_vec(&reply).unwrap();
bytes.push(b'\n');
std::io::Write::write_all(&mut stream, &bytes).unwrap();
```
父test断言BoardDaemon::new(socket).probe()为Ok、join有界线程；另用不存在socket断言Io ConnectionRefused/NotFound。调用 `cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner`，新methods缺失红。

- [ ] **Step 2: 实现窄方法并重排run。**
```rust
pub fn probe(&self) -> Result<(), ClientError> {
    client::send_status_with_budget(&self.socket, std::time::Duration::from_secs(2)).map(|_| ())
}
pub fn socket(&self) -> &Path { &self.socket }
```
Liveness::misses直接返回现有field。check_managed_context后立即match socket_path；Err打印runtime+socket unresolved或超长候选及错误→exit1；Ok打印daemon socket at，构造BoardDaemon。之后保留calendar、锁、service、迁移、query顺序。start_query_server已有查询日志保持query名字，不使用daemon路径sweep。

- [ ] **Step 3: 用一个计数源接诊断。**
```rust
let result = daemon.probe();
let should_exit = liveness.observe(result.is_ok(), 3);
if let Err(error) = result {
    eprintln!("superpowers-kanban: daemon probe failed runtime={} socket={} misses={}/3: {}{}",
        args.runtime_dir.display(), daemon.socket().display(), liveness.misses(), error,
        if should_exit { "; consecutive probe failures; exiting to release instance lock" } else { "" });
}
if should_exit { std::process::exit(0); }
```
探活仍在开关之前；成功只清零，不逐拍打印。进入loop前仅一次打印 `daemon probe loop starting runtime={} socket={}`，位置必须在迁移/查询启动之后、首次probe之前，供进程测试启动计时；不得每拍输出或用首次错误日志替代。不保留“daemon is gone”无证据断言。注释退出预算改为spec P/I/W口径，不再写3×interval硬上界。

- [ ] **Step 4: 回归、证据与提交。** 跑runner单测与IPC/cross tests，完整fmt/just ci/plugin-check，模块记录Task5尚待进程验收，提交 `fix: report kanban probe failures without hiding socket errors`。

## Task 6: 隔离CLI进程回归（A10–A13）

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/tests/daemon_liveness.rs`
- Create: 同crate `tests/support/daemon.rs`（由integration test用path module引用，不新增lib公开API）。
- Modify: `docs/project-management/superpowers-kanban.md`。

**Interfaces:**
- `TestLayout::new() -> TestLayout`：用 `tempfile::Builder::new().prefix("kd").tempdir_in("/tmp")` 持短根并canonicalize，home=`root/i`、runtime=`root/r`、project=`root/p`、state=`project/.yi-agent/superpowers-kanban`。只预建home、home/tmp、runtime、project；state和其配置父目录由各用例显式创建，缺home/超长零副作用用例可保持state不存在。
- `spawn_runner(layout: &TestLayout, instance_home: Option<&Path>) -> ChildGuard`；Some传入可替换的实际INSTANCE并始终给HOME隔离目录，None同时不设置INSTANCE/override/HOME；因此缺home与超长home均能复用，无含糊bool参数。用env!("CARGO_BIN_EXE_superpowers-kanban")而非假设target/debug固定路径。
- `StatusStub::start(path: &Path, replies: Vec<StubReply>) -> StatusStub`：只对本TestLayout下socket创建parent目录后bind；StubReply为Status/BusinessError/Close/Silent，AtomicUsize请求数，stop flag，Drop停止join。非空replies按连接依次取，耗尽重复最后一个；主accept线程不被沉默连接阻塞，每条连接独立有界处理并可由stop中断，Drop join所有handler。
- layout.state固定 `<project>/.yi-agent/superpowers-kanban`；创建默认测试开关使用真实 `superpowers_kanban_core::switch::write_layer(&superpowers_kanban_core::layout::project_preferences_path(&layout.state), SwitchValue::Enabled)`；只有关闭开关用例写Disabled，缺home/超长零副作用用例不调用写开关（避免先创建state父目录）。
- ChildGuard日志tempfile、`wait_before(deadline: Instant) -> ExitStatus`、`stderr() -> String`、Drop kill/wait；不用pipe导致无法read至EOF或阻塞子进程。

- [ ] **Step 1: 写真正会在旧路径变红的进程测试。** `new_socket_keeps_same_pid_and_old_sockets_are_untouched`：只在新地址提供合法Status，旧短地址和TMPDIR hashed旧地址各bind监听但不答复；不删除现存socket，只用临时根。runner env_clear、HOME隔离、YI_AGENT_INSTANCE_HOME=home、TMPDIR独立目录，CLI如下：
```rust
std::process::Command::new(env!("CARGO_BIN_EXE_superpowers-kanban"))
    .args(["run", "--interval-secs", "1"])
    .arg("--runtime-dir").arg(&layout.runtime)
    .arg("--state-dir").arg(&layout.state)
    .arg("--project-root").arg(&layout.project)
    .env_clear().env("HOME", &layout.home)
    .env("YI_AGENT_INSTANCE_HOME", &layout.home)
    .env("TMPDIR", layout.home.join("tmp"));
```
等待stub>=6请求（最多12秒），期间try_wait None、id保持初始值，旧两桩count0。先临时恢复旧业务解析器运行该单例，确认红，再还原新实现绿；此task新测试在已修代码上预期绿，不能伪称先前运行过红。

- [ ] **Step 2: 失联/开关/重置用例。** `three_misses_exit_and_release_lock`：无daemon，空inbox/no archive，进程从首次probe前的 `daemon probe loop starting` 标记开始计时，12秒内exit0；标记等待单独最多5秒startup watchdog，绝不从首条失败诊断开始计时漏掉第一次超时。，日志次数1/2/3恰好三行、runtime/socket/errno清晰，single_instance::acquire成功；标记之前的初始化不计入失联窗口（启动watchdog已单独覆盖）。`disabled_switch_still_probes`：用项目board switch API先写关闭再启动，不只是改全局flag；三次一样退出。`success_resets_the_streak`：桩Close,Close,Status,Close,Close,Close，断言计数1,2,1,2,3（成功不打印）且至少6请求；合法Error替代Status再跑一次。

- [ ] **Step 3: 沉默进程仍有界退出。** `silent_status_peer_exits_under_empty_loop_budget`：三次accept后保持沉默直到client关闭，每次read Timeout诊断；从同一首次probe前标记计时12秒内exit0、三条misses、释放锁。测试不能通过服务端主动close代替沉默。child/source工作与loop空载条件在报告写明，不外推全工作负载硬上界。

- [ ] **Step 4: 解析失败零副作用。** `missing_instance_home_fails_before_state_side_effects`：child env_clear且不设置HOME/INSTANCE/override，预先state放最小board/inbox快照；exit1，包含socket unresolved/实例错误，plugin.lock不存在、query新旧候选不存在，快照逐字节不变；state原本不存在的另一个case保持不存在。`long_instance_home_fails_before_state_side_effects`：超限候选显示bytes/limit，其他相同。运行态保全测试 `bound_card_state_is_not_changed_by_probing`：在启用开关的layout写含已绑定thread的Running、Queued、NeedsYou三卡，enqueued_at使用当前chrono RFC3339、next_order=4，各卡order=1/2/3；让合法Status桩应答>=6次后读取board投影比较ID/order/state/thread。创建数据：
```rust
let cards: Vec<serde_json::Value> = ["running", "queued", "needs_you"].iter()
    .enumerate().map(|(i, state)| serde_json::json!({
        "id": format!("card-{i}"), "state": state, "order": i + 1,
        "spec_path": "docs/spec.md", "plan_path": "docs/plan.md",
        "enqueued_at": chrono::Local::now().to_rfc3339(),
        "thread_id": format!("isolated-thread-{i}")
    })).collect();
std::fs::create_dir_all(&layout.state).unwrap();
std::fs::write(layout.state.join("board.json"),
    serde_json::to_vec(&serde_json::json!({"cards": cards, "next_order": 4})).unwrap()).unwrap();
```
避免触发既有legacy迁移和到期归档，不能给旧逻辑加禁令，也不能默认关闭开关使状态保全测试空过。

- [ ] **Step 5: 有界清理、回归和提交。**
```bash
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner --test daemon_liveness -- --test-threads=1
cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-runner
```
所有child即使panic也kill/wait；所有桩有stop+join，有界accept/IO，不遗留服务。fmt/just ci/plugin-check全绿后模块记录实测，提交 `test: cover isolated kanban daemon liveness and startup errors`。

## Task 7: 必执行跨端gate与Linux/macOS接线（A14）

**Files:**
- Create: `scripts/plugins/check-daemon-socket-contract.sh`
- Create: `scripts/plugins/check-daemon-socket-wiring.py`
- Create/Test: `scripts/plugins/test_check_daemon_socket_contract.py`
- Modify: `yi-agent-rs/justfile::plugin-contracts-check`，新增 `plugin-daemon-socket-check`。
- Modify: `.github/workflows/ci.yml::macos`（Linux plugins已有调用链不重复加）；`.gitlab-ci.yml::ci-plugins.before_script` 显式安装 `python3`，后续script保留现有contracts调用。
- Modify: `plugins/contracts/README.md`、`docs/project-management/superpowers-kanban.md`。

**Interfaces:**
- gate脚本从自身绝对位置解析repo root；默认只host example +两个fixture测试+ignored cross+IPC/runner预算/进程测试，无Node。
- 从cargo metadata取host target_directory；支持绝对/相对CARGO_TARGET_DIR，build与metadata统一cd根，避免两个workspace解释相对路径不同。
- Python `check(root: pathlib.Path) -> list[str]` 静态守卫读取具体recipe和workflow jobs，不全局grep碰巧出现字符串。对shell的明确接线识别固定 `run_one cross_process_daemon_socket_contract ignored` 调用、run_one函数非空 `flags=(--exact)` 与ignored分支 `flags+=(--ignored)` 的实际参数组合，不能要求它们相邻而误判具名函数封装；自测使用临时root文件，变异不能编辑真实main。

- [ ] **Step 1: 写gate负例红测试。** Python unittest名 `test_valid_gate_executes_required_tests`、`test_missing_probe_is_not_success`、`test_zero_cross_tests_is_rejected`、`test_missing_required_io_test_is_rejected`、`test_stale_or_cross_target_probe_is_rejected`、`test_invalid_metadata_is_rejected`、`test_missing_recipe_dependency_is_rejected`、`test_linux_gitlab_and_macos_call_chains_are_required`。临时root复制待测shell并放在scripts/plugins，创建必要manifest占位文件及临时PATH假cargo，所有构建/test操作由fake分派，不访问真实cargo。fake记录每次argv到文件：metadata返回target_directory，build返回compiler-artifact JSON/executable；正常case为所需test输出精确running/成功名称/summary，显式用 `/bin/bash` 执行（macOS为Bash3.2），预期exit0且8个阶段实际被调用；非空flags数组不可改为空数组再展开（Bash3.2 set -u会报unbound且可能错误exit0，调用清单必须拦住）。负例只改变一个阶段，其他保持有效。build exit0但缺example→非0；build指向不同target的产物→非0；非法metadata→非0；cross输出0test→非0；删一条关键IO成功记录→非0。不得只跑负例来放行永远exit1的脚本。

- [ ] **Step 2: 实现shell gate并校验实际产物/测试执行。** 脚本主体固定如下；run_one拒绝0test/ignored跳过，完整包回归日志再核查关键测试名称。stderr/stdout不依赖继承的capture/color环境。probe来源同时核对cargo metadata目录与本次compiler-artifact JSON，拒绝build.target配置或陈旧native路径错配，而非从磁盘碰巧找到旧example就继续。
```bash
#!/usr/bin/env bash
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd -P)
cd "$ROOT"
command -v python3 >/dev/null
if [[ -n ${CARGO_BUILD_TARGET:-} ]]; then
  echo 'cross-target execution is not supported' >&2
  exit 1
fi
unset RUST_TEST_NOCAPTURE KANBAN_CONTRACT_RUNTIME_HEX KANBAN_CONTRACT_DELETE_CWD
export CARGO_TERM_COLOR=never
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
HOST_TARGET=$(cargo metadata --manifest-path yi-agent-rs/Cargo.toml --no-deps --format-version 1 |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
cargo build --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle \
  --example daemon_socket_contract_probe --message-format=json >"$TMP/build.json"
PROBE=$(python3 - "$TMP/build.json" "$HOST_TARGET" <<'PY'
import json, os, sys
rows = [json.loads(line) for line in open(sys.argv[1]) if line.strip()]
paths = [r['executable'] for r in rows
         if r.get('reason') == 'compiler-artifact'
         and r.get('target', {}).get('name') == 'daemon_socket_contract_probe'
         and 'example' in r.get('target', {}).get('kind', [])
         and r.get('executable')]
assert len(paths) == 1, 'missing or ambiguous host probe artifact'
expected = os.path.join(sys.argv[2], 'debug', 'examples', 'daemon_socket_contract_probe')
assert os.path.realpath(paths[0]) == os.path.realpath(expected), 'cross-target or stale artifact path'
assert os.path.isfile(paths[0]) and os.access(paths[0], os.X_OK), 'host probe is missing'
print(os.path.abspath(paths[0]))
PY
)
export KANBAN_HOST_SOCKET_PROBE="$PROBE"
run_one() {
  local name=$1 mode=$2
  shift 2
  local flags=(--exact)
  if [[ $mode == ignored ]]; then flags+=(--ignored); fi
  local log="$TMP/one.log"
  if ! cargo test "$@" "$name" -- "${flags[@]}" --format pretty --color never >"$log" 2>&1; then
    cat "$log" >&2; return 1
  fi
  cat "$log"
  grep -Fqx 'running 1 test' "$log"
  grep -Fqx "test $name ... ok" "$log"
  grep -Eq '^test result: ok\. 1 passed; 0 failed; 0 ignored;' "$log"
}
run_one root_vector_and_fixture_schema_are_pinned normal \
  --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-lifecycle --test daemon_socket_contract
run_one root_vector_and_fixture_schema_are_pinned normal \
  --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract
run_one cross_process_daemon_socket_contract ignored \
  --manifest-path plugins/superpowers-kanban/Cargo.toml -p superpowers-kanban-ipc --test daemon_socket_contract
if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-ipc -- --format pretty --color never >"$TMP/ipc.log" 2>&1; then
  cat "$TMP/ipc.log" >&2; exit 1
fi
cat "$TMP/ipc.log"
for name in status_io::tests::nul_is_rejected_before_connect \
  status_io::tests::connect_backlog_obeys_deadline \
  status_io::tests::connect_retries_keep_original_deadline \
  status_io::tests::poll_eintr_keeps_deadline \
  status_io::tests::failed_attempts_close_descriptors \
  status_io::tests::short_writes_and_interrupted_io_complete_one_frame \
  status_io::tests::write_stall_times_out_at_write_stage \
  status_io::tests::broken_peer_does_not_raise_sigpipe \
  silent_peer_times_out_during_read drip_feed_cannot_renew_budget \
  hup_with_complete_frame_is_not_truncated \
  valid_status_and_business_error_are_replies truncated_frame_is_rejected \
  malformed_frame_is_rejected oversized_stream_is_bounded \
  protocol_and_request_id_mismatches_are_rejected frame_limit_excludes_newline; do
  grep -Fqx "test $name ... ok" "$TMP/ipc.log"
done
if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-runner --test daemon_liveness \
  -- --test-threads=1 --format pretty --color never >"$TMP/runner.log" 2>&1; then
  cat "$TMP/runner.log" >&2; exit 1
fi
cat "$TMP/runner.log"
for name in new_socket_keeps_same_pid_and_old_sockets_are_untouched \
  three_misses_exit_and_release_lock disabled_switch_still_probes \
  success_resets_the_streak silent_status_peer_exits_under_empty_loop_budget \
  missing_instance_home_fails_before_state_side_effects \
  long_instance_home_fails_before_state_side_effects bound_card_state_is_not_changed_by_probing; do
  grep -Fqx "test $name ... ok" "$TMP/runner.log"
done
if ! cargo test --manifest-path plugins/superpowers-kanban/Cargo.toml \
  -p superpowers-kanban-runner --lib -- --format pretty --color never >"$TMP/runner-lib.log" 2>&1; then
  cat "$TMP/runner-lib.log" >&2; exit 1
fi
cat "$TMP/runner-lib.log"
for name in client::tests::probe_preserves_error_and_accepts_business_error \
  single_instance::tests::misses_accessor_tracks_only_consecutive_failures; do
  grep -Fqx "test $name ... ok" "$TMP/runner-lib.log"
done
```
`hup_with_complete_frame_is_not_truncated` 放 `tests/status_budget.rs`；fixture的环境/路径用例由IPC整包回归执行。Rust-only gate已执行runner --lib，确保macOS覆盖probe/计数单测且不重复daemon_liveness。Task7自测严格模拟每阶段argv与成功记录；变更测试名时同commit同步列表，遗漏执行记录即失败。

- [ ] **Step 2a: 接线守卫自身实现/自测。** Python仅用pathlib/re/sys/unittest，不新增PyYAML。`check(root)`按固定缩进切出just顶层recipe、GitHub二级job、GitLab顶层job，忽略注释和空行；检查plugin-contracts-check依赖plugin-daemon-socket-check、两recipe主体与script显式cross名称/ignored/exact参数、Linux/GitLab调用contracts、macOS调用Rust-onlygate及安装just、GitLab安装python3。动态/不认识的写法返回非空错误而不是猜测放行；自测在临时root先构造全部合法块断言无错，再分别删依赖/调用/安装或改测试名断言点名错误。main打印每条错误到stderr并exit1，无错exit0。

- [ ] **Step 3: 接recipe与macOS。**
```just
plugin-daemon-socket-check:
    bash ../scripts/plugins/check-daemon-socket-contract.sh

plugin-contracts-check: plugin-daemon-socket-check
    python3 ../scripts/plugins/check-daemon-socket-wiring.py
    python3 -m unittest discover -s ../scripts/plugins -p 'test_check_daemon_socket_contract.py'
    node ../scripts/plugins/check-contracts.mjs
    node --test ../scripts/plugins/check-contracts.test.mjs
```
macos job新增Install just步骤 `uses: taiki-e/install-action@v2` / `tool: just`，然后 `working-directory: yi-agent-rs` / `run: just plugin-daemon-socket-check`。GitLab ci-plugins首条apt安装列表加 `python3`：`apt-get update && apt-get install -y --no-install-recommends curl ca-certificates gnupg git python3 >/dev/null`；不改变job权限/rules/runner。不能假设macOS已装just，也不跑需要Node的plugin-contracts-check。静态守卫检查Linux plugins/GitLab ci-plugins调用plugin-contracts-check且recipe依赖存在，macos固定macos-15有Rust-onlygate调用。

- [ ] **Step 4: 独占四类变异，必须红。** 在执行worktree逐项临时变异→单命令→记录rc/testname→立即还原；从clean checkpoint开始，只还原本轮变异文件，不能git reset --hard丢他人工作。
  1. 旧daemon布局：Task6 `new_socket_keeps_same_pid_and_old_sockets_are_untouched` 必红。
  2. INSTANCE/override交换：Task2cross与Task1优先级至少一个红。
  3. 去canonicalization：Task2symlink/relative/missing cases红。
  4. 删除plugin-contracts-check依赖：`python3 scripts/plugins/check-daemon-socket-wiring.py` 红，并对应unittest绿（它验证错误被捕获）。
缺helper与零测试负例也须各实测红。所有变异恢复后同一完整gate绿；报告不得只说“设计上会失败”。

- [ ] **Step 5: 全门禁及提交。**
```bash
python3 -m unittest discover -s scripts/plugins -p 'test_check_daemon_socket_contract.py'
(cd yi-agent-rs && just plugin-daemon-socket-check && just plugin-check && just ci)
```
GitHub Linux/macOS及GitLab实际运行链接在MR/PR可用时归档；本机不能声称已跑云端CI。fmt后提交 `ci: enforce daemon socket cross-process contracts on unix`，模块附A14命令、负例和变异证据。

## Task 8: 安装契约、隔离验收及交付（spec §8–9）

**Files:**
- Modify: `plugins/superpowers-kanban/INSTALL.md`（现有升级/二进制与skill一致性段落追加地址契约，不取代原排空流程）。
- Create: `docs/superpowers/reports/2026-11-09-kanban-daemon-socket-validation.md`。
- Modify: `docs/project-management/superpowers-kanban.md`、`docs/project-management/README.md`。

**Interfaces:** 不新增代码接口。报告每项含OS、版本/摘要、runtime/home字节身份、命令、脱敏结果、exit code、PID/时间；状态仅pass/fail/not_run，not_run不能写已恢复。

- [ ] **Step 1: 写可操作安装/排障文档并检查。** 明确新布局与实例环境优先级，示例区分daemon socket/query socket；连接拒绝先查实际地址/lsof与环境，不盲删socket或pkill。旧host混装不兼容，不fallback。记录生产路径从现场获取，不把诊断快照PID当当前目标。文档测试用 `git diff --check` + 本地链接检查，非代码功能验收。

- [ ] **Step 2: 重跑完整自动矩阵并建证据表。** 用下方A映射逐项记录测试名/数量/rc/平台，运行两个workspace fmt和 `just ci`、`just plugin-check`；缺某项写not_run，禁止用cargo输出0test代替。code review按requesting-code-review，发现问题回相应任务修复及复测。

- [ ] **Step 3: 隔离真宿主/插件验证。** 使用独立home/runtime/state和独立app bundle id，按项目CLAUDE.md独立app规程构建/签名，不用生产sqlite/relay。先用该build的 `yi-agent --help` / daemon子命令help确认参数，不发明启动flag。直接启动独立desktop，检查sidecar环境/参数，lsof地址与插件日志一致；只读Status；至少120秒、>=6探活同PID；经真宿主转发12次小switch.read，记录每次结果，遇既有query完整帧问题写fail+独立阻塞而非修该模块。先停隔离desktop监督父进程再停其daemon，分别跟踪旧插件PID三次失联/锁释放，清理child。没有GUI/构建产物则not_run，不能用桩替代此项。

- [ ] **Step 4: 正式部署检查点。** 提交隔离报告并询问是否安装/重启正式插件；未经允许到此停止部署动作并写正式待确认。获准后只针对该实例排空/版本安装/监督恢复，复核inode、环境、地址、120秒稳定及12次查询；卡片ID/order/thread关联无新增变化，既有legacy迁移/到期归档单独记。绝不投递真实任务“看看能不能跑”。

- [ ] **Step 5: 状态与提交/合并。** 所有自动、隔离和获准正式项通过才将feature `[ ]`→`[x]`、计数19/23→20/23；只有实现绿则保持19/23并写“代码完成、正式待确认”。模块与索引、INSTALL、报告同commit。代码提交规则完整fmt/CI/插件gate后 `docs: record kanban socket repair validation`；合并另按finishing-a-development-branch及用户选择，获准merge后再跑just ci和plugin-check，不自动push。开发产物回收只预演just reclaim，未完成编译阶段不删target。

## A01–A14 覆盖表

| ID | 实施任务 | 核心测试/证据 |
|---|---|---|
| A01 | 1、6 | short_and_deep_use_instance_socket_directory；新址CLI/旧桩零请求 |
| A02 | 1、2 | instance_priority_and_suffix_are_exact；cross_process_daemon_socket_contract |
| A03 | 1、2、6 | 显式空值/缺失；missing_instance_home_fails_before_state_side_effects |
| A04 | 1、2 | canonical_spelling_and_missing_tail_match；两child实际环境+cwd |
| A05 | 1、2 | symlink祖先缺失尾部创建前后case |
| A06 | 1、2 | instances_and_runtimes_are_isolated；TMPDIR两次child对照 |
| A07 | 1、2 | root_vector_and_fixture_schema_are_pinned；非UTF8/删除cwd child |
| A08 | 1、2、3、6 | non_utf8_and_length_are_raw_bytes；nul_is_rejected_before_connect；超长CLI |
| A09 | 1 | 原plugin-socket fixture/深路径bind继续通过 |
| A10 | 5、6 | new_socket_keeps_same_pid_and_old_sockets_are_untouched >=6探活 |
| A11 | 4、5、6 | misses accessor、Error应答、三次退出、成功重置、开关关闭 |
| A12 | 3、4、6 | connect backlog/EINTR、沉默/滴灌、short IO、SIGPIPE、帧负例/限额 |
| A13 | 5、6 | 无副作用解析失败、单计数日志、锁释放与卡片关联保全 |
| A14 | 2、7 | 双侧fixture、ignored实际1test、helper/零test拒绝、四类变异、两平台gate |

## 计划自检与执行边界

- 需求覆盖：spec §1–3→全局边界/Task1；§4→Task1/2/5/6；§5→Task3–6；§6→Task2/7；§7→覆盖表；§8–9→Task8。
- 类型一致：业务错误独立于查询错误；probe只返Result<(),ClientError>，Status入口返Reply；host路径函数返PathBuf、env返Option；跨端比较原始hex/错误类别。
- 实测红绿、四类变异、Linux/macOS运行和真机结果均由实施者记录，本计划没有预先填任何通过结果。
- 本轮plan专项复审已有最小Rust示例编译与内嵌gate语法/mock正反例证据，见[plan复审记录](../reviews/2026-11-09-kanban-daemon-socket-plan-review.md)；不等同原项目单测、跨端真实函数或真机通过。
- 执行选择：subagent-driven-development逐任务双阶段审查，或executing-plans在当前会话分批执行。用户本轮只授权反复检查plan，不开始实现或部署。
