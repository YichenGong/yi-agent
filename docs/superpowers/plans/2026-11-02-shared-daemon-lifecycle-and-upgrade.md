# 共享项目 daemon 生命周期与确认升级 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 保留一项目一数据库，使后台工作随真实前端生命周期回收，并允许用户在了解全部影响后把共享 daemon 切换到当前 App 的配套构建。

**Architecture:** 每项目一个 managed guardian 持有前端租约、独立控制通道和受管进程句柄；一个 daemon 独占原数据库与 coordinator，按客户端隔离的 executor 承载 Agent worker。App 桌面壳和各 TUI 分别持有前端身份，退出只回收该身份的工作；确认升级通过同一作用域的准入屏障、协调锁和代际状态机执行。共享轻量协议包不链接 SQLite，桌面壳不引入 store／core／supervisors。

**Tech Stack:** Rust workspace（既有 rust-version 1.85，不为本需求升版）、Tokio、Serde、Unix domain socket、flock／POSIX process groups、现有 SQLite repository、Tauri 2、React 19、TypeScript、Vitest；测试默认 mock，不调用真实 LLM。

## Global Constraints

- 依据：`docs/superpowers/specs/2026-11-02-shared-daemon-lifecycle-and-upgrade-design.md`。本文是实施计划，不是开始修改实现或合并的授权。
- macOS 关闭窗口仍然隐藏窗口；只有 Cmd+Q／真正退出才释放 App 客户端。
- App 退出，必须停止 App 名下的后台工作；仍活跃的 TUI 的工作不受影响。
- 一项目一数据库、一个业务 daemon；guardian 和 executor 不创建第二份 runtime 数据库，不成为第二个 coordinator。
- 心跳每 5 秒；30 秒未续约判失效。首个客户端登记等待最多 30 秒。
- 正常退出所有项目并行共享 10 秒优雅预算，再最多 5 秒安全终止；崩溃租约失效后使用相同 10 + 5 秒回收预算。
- 控制协议 v1；每帧最多 16 KiB、单次控制请求 2 秒超时。读、写、锁等待均不得超过剩余总预算。
- 影响确认 token 一次性、60 秒过期；心跳、日志和 token 用量不改变影响 revision。
- 升级全部对象共用 10 + 5 秒停旧预算；控制层交接、启动与就绪验证合计 30 秒。
- socket 遵守现有 103-byte 限制和长路径 hash 回退，目录 0700、socket 0600；跨 exec 仅保留明确白名单 FD。
- 仅 managed runtime 提供退出保证；旧／手工 unmanaged daemon 的暂用必须由用户明确选择并持续显示例外。
- 不改变手工 `daemon start/serve` 公开语义，不杀 embedded daemon 的 TUI 宿主，不强杀其他用户／SSH 远端进程。
- 不自动重放 spawn、发消息、命令等非幂等业务；不静默降级；不删除数据库、交付、锁文件解决冲突。
- managed runtime 拒绝 `on_exit=keep`，不静默改写成 kill；不保证撤销外部副作用或追踪用户主动脱离托管的任意程序。
- 单任务结束与执行器进程退出不同；运行资源在实际执行停止后释放，中断记录使用 paused／recovery_required。
- 所有实现、测试、配置、文档修改都在非 main worktree。主工作区及其他 worktree 的原有修改不得包含在本需求提交中。
- 每个实现任务提交前执行 `cargo fmt --all`、`just ci`（在 `yi-agent-rs/`）；desktop 另跑其专用检查。每组需求同步模块进度和 README 计数。

---

## 0. 执行准备与计划边界

### 0.1 工作区与批准门禁

本计划写在已有 `.worktrees/docs-runtime-lifecycle`、分支 `docs/runtime-lifecycle`。不要再套一层 worktree，也不要在主工作区写文件。执行实现前先获得用户明确授权；用户要求继续写 plan 不等于已经同意开始代码改造。

若实现仍在这条分支进行，先保存 spec + plan 的文档基线；若另建 feature worktree，必须先使这两份文档在该 worktree 可读取，并记录基线 commit。不要丢弃原未跟踪文档。

- [ ] 确认 `git rev-parse --git-dir --git-common-dir` 表明 linked worktree，`git branch --show-current` 不是 main。
- [ ] 记录 `git status --short`、`git rev-parse HEAD`、`git worktree list`；确认没有其他人在同一 worktree 修改实现。
- [ ] 运行基线 `cd yi-agent-rs && just ci`，再 `just desktop-check`；已有失败先单独记录、诊断，不把它们算成本需求造成或跳过门禁。
- [ ] 若 cargo／node 不在 PATH，使用已安装工具链；不要安装或升级整个工具链来绕过失败。当前机器已发现 Rust stable 工具链在 `$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin`，Node／just 在 `/opt/homebrew/bin`。
- [ ] 桌面 Rust 测试若因 externalBin 缺失失败，先按项目规范在 worktree 内补 sidecar 链接，仅用于构建测试；打包必须使用 Task 2 新构建的真实 sidecar，不把旧链接当成升级目标。

### 0.2 不拆成独立发布的原因与里程碑

这些子系统共享“一个前端退出不影响另一个、最后退出后无受管工作”的不变量，不能把提示按钮单独视为本期完成。内部可分三段审查：

1. **M1／Tasks 1–9：** 协议、归属、执行隔离和回收成立；新入口尚不切换默认行为。
2. **M2／Tasks 10–16：** 旧安装迁移、前端连接、确认切换与本地控制面串通；仍不能仅凭单元测试启用。
3. **M3／Tasks 17–18：** UI、真实子进程集成、全量门禁和默认接入一次收口。

实现阶段新 managed 路径通过显式内部模式调用，原用户默认入口在 Task 18 完整验收后切换。不是新增永久设置开关，也不声称旧路径已具有新保证。

### 0.3 文件地图与依赖方向

路径均相对仓库根。`Create` 的文件不存在是预期；不要去找一个不存在的旧实现。

| 文件／目录 | 动作与单一职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-lifecycle/` | 新增轻量 crate；`types.rs` 线协议 DTO，`codec.rs` 有界帧，`client.rs` 控制客户端，`leases.rs` 纯租约状态机，`deadline.rs` 总预算；不依赖 store/core/tools |
| `yi-agent-rs/crates/yi-agent/build.rs`、`src/build_info.rs` | 产物构建身份，不将 wrapper 版本当 daemon 身份 |
| `desktop/scripts/build-sidecar.sh`、`desktop/src-tauri/build.rs` | sidecar 清单生成／校验，与打包链一致 |
| `yi-agent-rs/crates/yi-agent-store/src/lifecycle.rs` | root 所有权、客户端准入／维护屏障、回收状态与执行停止确认 |
| `yi-agent-rs/crates/yi-agent-store/src/schema_compat.rs` | 只读 schema 范围探测，早于所有恢复／迁移写入 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/` | 新模块；`scope.rs` canonical scope／协调锁，`selection.rs` 构建选择，`guardian.rs` 控制服务，`processes.rs` 子进程归属，`reclaim.rs` 回收，`preview.rs` 影响快照／token，`switch.rs` 切换，`handover.rs` exec 交接，`migration.rs` 旧安装处理 |
| `yi-agent-rs/crates/yi-agent-subagent/src/executor/` | 新模块；`protocol.rs` worker IPC，`proxy.rs` factory 代理，`serve.rs` 执行端；不另开 repository |
| `yi-agent-rs/crates/yi-agent-tools/src/managed_process.rs` | 受管工具 spawn 登记与 keep 策略，不把进程组清理藏在 Agent Drop 内 |
| `yi-agent-rs/crates/yi-agent/src/managed_runtime.rs` | 隐藏的 guardian／daemon／executor 子命令路由、TUI／headless 接入桥 |
| `desktop/src-tauri/src/runtime_lifecycle.rs` | 桌面壳身份、项目租约、本地预览／确认与退出屏障 |
| `yi-agent-rs/crates/yi-agent-app-server/src/runtime_status.rs` | 状态与项目使用通知；没有独立破坏性确认权限 |
| `desktop/src/lib/runtimeLifecycle.ts` | 本地 Tauri 控制适配器；远端返回只读／不支持能力 |
| `desktop/src/hooks/useRuntimeLifecycle.ts` | 按 scope／代际轮询并清理状态，不通过轮询隐式注册 |
| `desktop/src/components/RuntimeVersionBanner.tsx` | 提示、二次确认、进度及失败展示 |
| `yi-agent-rs/crates/yi-agent/tests/managed_runtime_lifecycle.rs` | 真正启动隔离进程、模拟 App + 多 TUI 的最终验收 |
| `docs/project-management/yi-agent-lifecycle.md` | 新 crate 的模块说明、可执行完成判据；同 PR 登记索引 |

依赖只向下：`desktop → lifecycle`；`tools → lifecycle`；`store → lifecycle`；`supervisors → store + lifecycle`；`subagent → supervisors + store + core + tools + lifecycle`；CLI／app-server 作为组装端。不得添加 `store → supervisors/subagent` 或 `lifecycle → store` 的环。

`guardian` 是 supervisors 的一个运行角色，不新增另一个独立 crate／数据库／launchd 服务。内部 executor 协议可传大消息，不能误套控制通道 16 KiB 上限到完整 WorkerStart／fork 上传；单独采用明确有界的业务传输及现有上传机制。

### 0.4 统一 TDD 与提交规程

每个 Task 的步骤按顺序执行；下文代码是待新增的具体测试或关键实现片段，不声称当前已有这些 API。新接口在所属 Task 的 Interfaces 块中定义；现有 core 类型以源码为准。

每任务最后一步都运行下列流程，不能只跑本任务过滤测试就提交：

```bash
# 从当前 worktree 根执行；根据该任务先跑其专用测试，再跑以下总门禁。
(cd yi-agent-rs && cargo fmt --all && just ci)
git diff --check
# git add 只列该任务 Files、对应进度文档和索引；不使用 git add -A。
# commit 首行用每个任务给出的 conventional message，不加 Co-Authored-By。
```

每个任务包含实际可验证的独立交付，但不意味着可以单独默认上线。若一个测试仅因接口未定义而变红，下一步先补最小可编译骨架并再次确认断言变红，避免把编译错误当作行为红灯。

## Task 1: 建立轻量身份、控制协议与有界传输

**Depends on:** 0。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-lifecycle/Cargo.toml`、`yi-agent-rs/crates/yi-agent-lifecycle/src/{lib,types,codec,client,deadline}.rs`、`yi-agent-rs/crates/yi-agent-lifecycle/tests/control_codec.rs`。
- Modify: `yi-agent-rs/Cargo.toml`、`yi-agent-rs/Cargo.lock`。
- Create: `docs/project-management/yi-agent-lifecycle.md`；Modify: `docs/project-management/README.md`。

**Interfaces:** 对外均位于 `yi_agent_lifecycle`，ID 在 wire 中是字符串，不是 PID。

新 crate manifest 使用以下依赖；桌面无需为了这个包链接数据库或模型实现：

```toml
[package]
name = "yi-agent-lifecycle"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json.workspace = true
getrandom = "0.3"
libc = "0.2"

[dev-dependencies]
tempfile = "3"
```

```rust
pub const CONTROL_VERSION: u16 = 1;
pub const MAX_CONTROL_FRAME: usize = 16 * 1024;
pub type ClientRunId = String;
pub type DaemonInstanceId = String;
pub type BuildId = String;
pub type OperationId = String;
pub type ConfirmationToken = String;
pub type RootId = String;
pub type ManagedProcessId = String;

// wire DTO 派生 Clone/Serialize/Deserialize；不含凭据的类型可派生 Debug。
// ClientGrant、HandoverState 及含它们的 request 使用手写脱敏 Debug，禁止打印 credential。
pub struct RuntimeScope {
    pub key: String, pub runtime_dir: std::path::PathBuf,
    pub project_key: String, pub instance_home: std::path::PathBuf,
}
pub struct ArtifactIdentity {
    pub build_id: BuildId, pub display_version: String,
    pub executable: std::path::PathBuf, pub sha256: String,
    pub schema_min: u32, pub schema_max: u32, pub handover_version: u16,
    pub business_protocol_min: u32, pub business_protocol_max: u32,
    pub capabilities: Vec<String>,
}
pub struct RuntimeIdentity {
    pub scope_key: String, pub daemon_instance_id: Option<DaemonInstanceId>,
    pub guardian_build_id: Option<BuildId>, pub daemon_build_id: Option<BuildId>,
    pub mode: RuntimeMode, pub business_protocol: Option<u32>,
    pub daemon_display_version: Option<String>, pub guardian_display_version: Option<String>,
    pub capabilities: Vec<String>, pub schema_min: u32, pub schema_max: u32,
}
pub enum RuntimeMode { Managed, LegacyIndependent, LegacyEmbedded, Unknown }
pub enum ClientKind { App, Tui, Headless }
pub enum LeaseMode { Business, LeaseOnly, WaitingNextGeneration }
pub struct ClientGrant {
    pub client_run_id: ClientRunId, pub credential: String,
    pub epoch: u64, pub mode: LeaseMode,
}
pub struct OperationStatus {
    pub operation_id: OperationId, pub phase: SwitchPhase,
    pub detail: Option<String>, pub ready_identity: Option<RuntimeIdentity>,
}
pub struct ReclaimResult {
    pub client_run_id: ClientRunId, pub stopped: bool, pub failures: Vec<String>,
}
pub enum SwitchPhase {
    Idle, Preparing, Draining, Stopping, Starting, Verifying,
    Ready, StoppedNoClients, Failed, ManualActionRequired,
}
```

控制包第一步仅定义可独立编译的基础 tagged enum：`ControlRequest::{Identity, Register { client_run_id, kind, mode }, Resume { grant }, Heartbeat { grant }, Release { grant }, ReclaimStatus { grant, operation_id }, Operation { grant, operation_id }}`；`ControlResponse` 对应 identity／grant／ack／operation／结构化 error，释放路径另有 `ReleaseStarted { operation_id }`、`ReclaimStatus { operation_id, result: Option<ReclaimResult> }`。Tasks 14、16 分别新增预览／确认与状态 DTO 及 enum variants，不在 Task 1 引用尚未定义的类型。未知操作返回 Unsupported，不能默默成功。

基础授权另定义 `RootAuthorization { scope_key: String, daemon_instance_id: String, client_run_id: String, root_id: String, capability: String }`，手写脱敏 Debug。guardian 仅在已认证的 business lease 上签发，经私有通道向 coordinator 建立同一绑定；业务 attach 必须验证 capability、root、scope 和代际，Release 立即撤销。lease-only 不可取得它。它不含 lease credential，不可注册／续租，工具只取得更窄的 task／spawn capability。Task 5 落地校验、Task 13 接入壳与 sidecar。不同代际重新签发，不让旧 capability 复活。

状态通知统一采用前端独立只读 polling：使用中项目每 5 秒查询一次（切换操作进行时每 1 秒），按 scope／generation 丢弃过期结果，失败有界退避；停止使用即取消。轮询不登记、不续租、不占业务 drain 队列。TUI 也从此状态路径显示维护通知；不承诺尚未实现的 push／订阅协议。

- `codec::encode<T: Serialize>(&T) -> io::Result<Vec<u8>>`、`decode<T: DeserializeOwned>(&[u8]) -> io::Result<T>`：4-byte big-endian 长度头，长度仅指 JSON body；`MAX_CONTROL_FRAME` 包含头，解码前检查长度。
- `Deadline::after(Duration) -> Deadline`、`remaining() -> Duration`、`clamp(Duration) -> Duration`：单调时钟，总预算零时不能再开始 I/O。内部使用 `pub trait Clock: Send + Sync { fn now(&self) -> Duration; }`，生产 Clock 由单调时钟实现；提供测试专用 `Deadline::with_clock(Duration, Arc<dyn Clock>)`，fake clock 与 Tokio paused time 显式同步，不拿 wall-clock 或序列化 Instant 代替。Clock 本身无 Tokio／业务依赖。
- `ControlClient::connect(&Path, Deadline) -> io::Result<Self>`、`request(&mut self, ControlRequest, Deadline) -> io::Result<ControlResponse>`：设置读写超时为 min(2 秒, remaining)，拒绝零预算；控制断开不重发 Confirm。

- [ ] **Step 1:** 添加 codec 测试，包括拒绝超长、截断、未知协议、UTF-8 错误及 16 KiB 边界。使用以下真实断言作为起点：

```rust
#[test]
fn oversized_frame_is_rejected_before_body_allocation() {
    let input = ((16 * 1024) as u32).to_be_bytes();
    let error = yi_agent_lifecycle::codec::decode::<serde_json::Value>(&input).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-lifecycle --test control_codec`，补骨架后确认边界断言失败。
- [ ] **Step 3:** 用 `read_exact`／`write_all` 和剩余预算实现传输；frame 内带独立 control_version；默认拒绝不支持版本，不借用业务 IPC 的响应反序列化器。错误不输出凭据。
- [ ] **Step 4:** 运行 codec 测试和 `cargo tree -p yi-agent-lifecycle`，确认无 rusqlite、core、provider 依赖。
- [ ] **Step 5:** 登记新 crate 进度，执行 §0.4，提交 `feat: add lightweight runtime lifecycle control protocol`。

## Task 2: 可验证的 daemon 构建身份与配套 sidecar 清单

**Depends on:** 1。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/build.rs`、`src/build_info.rs`、`src/config.rs`、`src/main.rs`、`Cargo.toml`。
- Create: `yi-agent-rs/crates/yi-agent/build_identity.rs`、`tests/runtime_build_identity.rs`。
- Modify: `desktop/scripts/build-sidecar.sh`、`desktop/scripts/package-npm-app.sh`、`desktop/scripts/release-dmg.sh`、`scripts/rebuild.sh`、`.github/workflows/release.yml`、`desktop/src-tauri/build.rs`、`desktop/src-tauri/tauri.conf.json`、`desktop/src-tauri/Cargo.toml`。
- Create: `desktop/scripts/finalize-runtime-bundle.sh`、`desktop/scripts/test_runtime_build_identity.sh`；Modify: `docs/project-management/{ci-cd,desktop,yi-agent-cli,README}.md`。

**Interfaces:** `build_info::runtime_build_id() -> &'static str`；隐藏命令 `yi-agent internal-runtime identity --json` 只输出 ArtifactIdentity 的静态字段／文件 digest，不连接 runtime、不读写数据库。`build_identity::input_digest(inputs: &[(String, Vec<u8>)]) -> String` 供 build.rs 与测试共用。新增 portable `BundledArtifactManifest`：build ID、display version、相对 bundle 的 executable 路径、sha256、schema／handover 能力；禁止嵌入构建机器绝对路径。清单文件名 `runtime-sidecar.json`，运行时解析实际路径后才转成 Task 1 ArtifactIdentity。桌面编译时仅嵌入稳定 expected build ID／能力，不嵌入签名前的 sha256。

- [ ] **Step 1:** 在测试中直接包含纯 `build_identity.rs`，验证内容、编译配置及路径排序：

```rust
#[test]
fn changed_dirty_source_changes_identity() {
    let a = vec![("src/lib.rs".into(), b"const X: u8 = 1;".to_vec())];
    let b = vec![("src/lib.rs".into(), b"const X: u8 = 2;".to_vec())];
    assert_ne!(input_digest(&a), input_digest(&b));
    assert_eq!(input_digest(&a), input_digest(&a));
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent --test runtime_build_identity`，确认用旧 git SHA／dirty 方法不能通过。
- [ ] **Step 3:** build.rs 对工作区源码输入闭包、manifest／lockfile、目标 triple、rustc 版本、profile、features、RUSTFLAGS／构建配置按排序计算摘要并嵌入；逐个声明 rerun-if-changed／env-changed。遍历仅限已知 workspace source／build/config 目录，排除 target、node_modules、.git、.worktrees，不扫描用户 HOME。新增／删除源文件通过受限目录监听触发；环境只收构建相关项，不把密钥收进清单。身份不能仅依赖现有 build.rs 是否恰好重跑。
- [ ] **Step 4:** sidecar 构建后先生成稳定身份信息供桌面编译；最终 bundle 内 sidecar 最后一次签名后，由 `finalize-runtime-bundle.sh` 写最终资源清单 → 签 desktop（若需要）→ 签外层 app → `codesign --verify` 并复核 sidecar digest。外层签名不得再用会重签 sidecar 的 `--deep`。所有打包入口复用此顺序；DMG／updater 归档必须在 finalize 后生成，不能发布 finalize 前的旧归档。若 Tauri 原有一次构建自动产出 DMG，拆为 app 构建、finalize、再从该 app 生成归档，并保留既有 updater／公证要求。桌面运行时从实际 bundle resource 读取最终清单，核对清单 build ID=编译 expected ID 且文件 digest 匹配；开发模式明确使用 dev sidecar 清单路径，缺失标 unknown 并禁自动切换，不用 wrapper 号兜底。复制同一产物不改变 build ID。schema 范围从当前 migration 支持契约产生，Task 5 加迁移时同步更新；区分可只读识别与可迁移范围，不硬编码 15。
- [ ] **Step 5:** shell 测试使用临时假 cargo／identity／codesign／归档器，验证自定义 CARGO_TARGET_DIR、旧清单、被替换 sidecar、签名后 digest 更新、归档包含最终清单、无构建机绝对路径。运行 `bash desktop/scripts/test_runtime_build_identity.sh`、`bash desktop/scripts/test_scripts_target_dir.sh`；只读审查 rebuild／release 入口，测试中替换真实命令，禁止直接运行会停止用户 App／daemon 的 `scripts/rebuild.sh`。
- [ ] **Step 6:** 更新构建／打包文档判据，执行 §0.4，提交 `feat: identify bundled runtime artifacts independently of wrapper versions`。

## Task 3: canonical scope、固定 inode 协调锁与只读 schema 预检

**Depends on:** 1。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{mod,scope,selection}.rs`、`tests/runtime_scope.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/{Cargo.toml,src/lib.rs}`。
- Create: `yi-agent-rs/crates/yi-agent-store/src/schema_compat.rs`、`tests/schema_compat.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-store/{Cargo.toml,src/lib.rs,src/repository/mod.rs,src/ipc/daemon.rs}`。
- Modify: `docs/project-management/{yi-agent-store,subagent-runtime,README}.md`。

**Interfaces:**
- `scope::resolve(runtime: &Path, project: &Path, instance_home: &Path) -> io::Result<RuntimeScope>`；尚未创建目录时 canonicalize 最近存在祖先后拼接，创建后再次核对。
- `CoordinationGuard::acquire(&RuntimeScope, Deadline) -> io::Result<Self>`；RAII flock 固定文件，不 unlink。
- `SchemaRange { min: u32, max: u32 }`、`inspect_schema(&Path) -> Result<Option<u32>, RepositoryError>`、`require_compatible(&Path, SchemaRange) -> Result<(), RepositoryError>`；无库为 None，未来 schema 拒绝。
- `RuntimeSelection { target: ArtifactIdentity, last_operation: Option<OperationStatus> }`；`read_selection(&RuntimeScope) -> io::Result<Option<RuntimeSelection>>`、`write_selection(&CoordinationGuard, &RuntimeSelection) -> io::Result<()>`。

- [ ] **Step 1:** 建立实际 SQLite 临时库 v999，预检前后比较 bytes、mtime、WAL／journal 是否新增：

```rust
#[test]
fn future_schema_is_rejected_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runtime.sqlite");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE schema_migrations(version INTEGER); INSERT INTO schema_migrations VALUES(999);").unwrap();
    drop(conn);
    let before = std::fs::read(&db).unwrap();
    assert!(require_compatible(&db, SchemaRange { min: 0, max: 15 }).is_err());
    assert_eq!(before, std::fs::read(&db).unwrap());
    assert!(!db.with_file_name("runtime.sqlite-wal").exists());
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-store --test schema_compat`；scope 测试用真实 symlink 验证两个路径不能各获独占锁，同一覆盖目录不同项目／instance 返回冲突。
- [ ] **Step 3:** 用 SQLite read-only flags 探测，不调用普通 open/migrate，不设置写 PRAGMA；把检查放在 Daemon::start_with_factory 的恢复／回收／Coordinator::open 之前。正常新库迁移保留既有行为，但不能接受 `current_version > supported_max`。
- [ ] **Step 4:** 选择记录同目录临时文件 → fsync → rename → 目录 fsync，权限 0600；不写租约凭据。首启仅在无活实例、无选择且 schema 可用时建立记录；错误和超时不当作 daemon 不存在。验证进程抢锁、锁 inode 不变、旧 socket 的 dev/ino 清理条件不回退。
- [ ] **Step 5:** 运行 `cargo test -p yi-agent-supervisors --test runtime_scope`、`cargo test -p yi-agent-store --test schema_compat` 及既有 `runtime_ipc` 测试；执行 §0.4，提交 `feat: fence runtime scope and reject incompatible schemas before writes`。

## Task 4: 前端租约与连接 epoch 的纯状态机

**Depends on:** 1。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-lifecycle/src/leases.rs`、`tests/client_leases.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/lib.rs`、`docs/project-management/{yi-agent-lifecycle,README}.md`。

**Interfaces:** `LeaseTable` 不执行 I/O，时间参数为从服务启动单调时钟映射的 `Duration`。

```rust
pub struct LeaseTable; // 内部 map、已释放身份 tombstone、连接 epoch 与截止时间。
pub enum DisconnectOutcome { IgnoredStaleEpoch, AwaitReconnect }
pub enum ReleaseOutcome { AlreadyReleased, Reclaim(ClientRunId) }
impl LeaseTable {
    pub fn new() -> Self;
    pub fn register(&mut self, id: ClientRunId, kind: ClientKind,
        mode: LeaseMode, now: Duration) -> Result<ClientGrant, String>;
    pub fn resume(&mut self, grant: &ClientGrant, now: Duration) -> Result<ClientGrant, String>;
    pub fn heartbeat(&mut self, grant: &ClientGrant, now: Duration) -> Result<(), String>;
    pub fn disconnected(&mut self, id: &str, epoch: u64) -> DisconnectOutcome;
    pub fn release(&mut self, grant: &ClientGrant) -> Result<ReleaseOutcome, String>;
    pub fn expire(&mut self, now: Duration) -> Vec<ClientRunId>;
    pub fn after_system_resume(&mut self, now: Duration);
    pub fn active_count(&self) -> usize;
}
```

- [ ] **Step 1:** 添加以下用例及显式 release、凭据错误、released 后迟到 heartbeat、首客户端超时测试：

```rust
#[test]
fn late_eof_does_not_remove_resumed_client() {
    let mut table = LeaseTable::new();
    let first = table.register("app".into(), ClientKind::App, LeaseMode::Business, Duration::ZERO).unwrap();
    let resumed = table.resume(&first, Duration::from_secs(8)).unwrap();
    table.disconnected("app", first.epoch);
    table.heartbeat(&resumed, Duration::from_secs(29)).unwrap();
    assert!(table.expire(Duration::from_secs(31)).is_empty());
    assert_eq!(table.active_count(), 1);
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-lifecycle --test client_leases`，确认断言先失败。
- [ ] **Step 3:** CSPRNG 签发至少 256-bit 凭据，凭据只在当前 guardian 内存／匿名交接 FD；明确失活不可被迟到 heartbeat 复活。EOF 不续租，不立即回收；显式 release 和已核实宿主死亡直接发回收事件。复用 ID 必须 resume，不能重新 register 抢占。
- [ ] **Step 4:** 增加休眠测试：显式 OS resume 事件重新给仍登记连接 30 秒续租窗口；不能仅靠 wall-clock 跳变。macOS wake 通知由桌面／guardian 平台适配接入，其他 Unix 记录 suspend-aware 与 suspend-excluding 时钟差值作恢复线索；可疑恢复先进入续租窗口，不把普通事件循环阻塞误当用户退出。已 release／核实死亡不恢复。
- [ ] **Step 5:** 测试 table 的 active_count 包含显式 lease-only／waiting 登记，不包含查询；heartbeat 不发影响 revision 事件。执行 §0.4，提交 `feat: track frontend leases with authenticated reconnect epochs`。

## Task 5: coordinator 的客户端归属、原子准入与停止确认

**Depends on:** 1、3、4。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/src/lifecycle.rs`、`tests/runtime_client_ownership.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-store/src/{lib,runtime}.rs`、`src/ipc/{daemon,protocol}.rs`、`src/repository/mod.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/{worker,supervisor}.rs`、`yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`、`docs/project-management/{yi-agent-store,yi-agent-core,subagent-runtime,README}.md`。

**Interfaces:** store 生命周期管理对象为 `RuntimeClients`，由 coordinator 的同一串行状态入口调用，不另造一个和 spawn 不同步的 mutex。
- `bind_root(&mut self, client: &str, root: &str, generation: &str) -> Result<(), String>`。
- `authorize(&self, client: &str, root: &str, generation: &str) -> Result<(), String>`。
- `close_client_admission(&mut self, client: &str) -> Vec<RootId>`；重复调用幂等。
- `begin_maintenance(&mut self, expected_coordinator_revision: u64) -> Result<(), String>`；Task 14 引入双分量快照时此处只比较 coordinator 分量。
- `execution_stopped(&mut self, task: &str, attempt: &str, generation: &str) -> Result<(), String>`：拒绝旧代际、旧 attempt、重复释放。
- core `WorkerHandle` 新增 `execution_stopped() -> bool` 与独立停止确认句柄；`WorkerEvent::Completed/Paused` 本身不设置物理停止信号。确认句柄只能由持有执行单元 join 的 observer 完成：正常返回、panic／abort 后 join 已完成都可确认，丢失 observer／通道 EOF 不能确认。更新现有 application factory、core supervisor、store runtime 及其 fake factories，禁止默认把终态当停止或默认永远 stopped。该改动同 Task 5 完成全体调用方迁移，不等到 Task 7 才使旧测试恢复。

- [ ] **Step 1:** 在 RuntimeClients 单元测试里先构造两个客户端的 root，再验证释放 A 不关闭 B：

```rust
#[test]
fn closing_app_admission_preserves_tui_root() {
    let mut clients = RuntimeClients::default();
    clients.bind_root("app", "thread:a", "g1").unwrap();
    clients.bind_root("tui", "thread:b", "g1").unwrap();
    assert_eq!(clients.close_client_admission("app"), vec!["thread:a".to_string()]);
    assert!(clients.authorize("app", "thread:a", "g1").is_err());
    assert!(clients.authorize("tui", "thread:b", "g1").is_ok());
}
```

- [ ] **Step 2:** 用 barrier 驱动真实 coordinator 集成测试，竞争 client-release 与子／孙 spawn；运行 `cargo test -p yi-agent-store --test runtime_client_ownership`，确认旧实现可逃逸的派生被测试抓住。
- [ ] **Step 3:** root 绑定、spawn／重试／调度／插件派生、队列 dequeue 共用准入检查；所有权向全部后代传递。业务 root 授权必须由 guardian 已认证连接经私有内部通道签发，不能信任 RPC 自报 client ID。schema 在当前 15 基础上加下一迁移版本保存 root 最近所有者及退出原因；执行时重新核对当前最新版本后递增，不覆盖别人的迁移。
- [ ] **Step 4:** 退出先关闭客户端准入，再请求 checkpoint，物理停止前不释放运行 lease；非正常结束使用 paused／recovery_required，历史交付和 workspace 保留策略不变。活跃 root 不可凭 thread ID 被新 client 抢走。全局 drain 与 client drain 分开。
- [ ] **Step 5:** 给假报终态但执行仍存活的 worker 加测试；只有独立停止证据才允许 release。运行受影响 core worker／store runtime 测试及 §0.4，提交 `feat: bind runtime roots and admission to frontend ownership`。

## Task 6: guardian 的进程归属与可验证整组回收

**Depends on:** 1、3、4。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{guardian,processes}.rs`、`tests/managed_process_groups.rs`、`tests/support/process_fixture.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/{Cargo.toml,src/lifecycle/mod.rs}`、`docs/project-management/{subagent-runtime,README}.md`。

**Interfaces:**
- `ProcessOwner = Frontend(ClientRunId) | SharedPlugin(String) | Daemon(DaemonInstanceId)`。
- `TaskExecutionKey { generation: String, task: String, attempt: String }`；`ManagedCommand { executable: PathBuf, args: Vec<String>, env: Vec<(String,String)>, cwd: PathBuf, owner: ProcessOwner, root: Option<RootId>, execution: Option<TaskExecutionKey> }`；敏感 env 不进日志。工具的 root／execution 由 owner-bound capability 确定，不能由工具自报以冒充别的 attempt。
- `ProcessRegistry::spawn(&mut self, ManagedCommand, Deadline) -> io::Result<ManagedProcessId>`；`terminate_owner(&mut self, &ProcessOwner, Deadline) -> io::Result<()>`；`owner_is_stopped(&mut self, &ProcessOwner) -> io::Result<bool>`；`task_tools_are_stopped(&mut self, &TaskExecutionKey) -> io::Result<bool>`。后者只计该 task 的工具组，不要求承载同 owner 其他任务的 executor 退出。
- 受管进程登记以 0600 原子写入非机密 journal：scope／代际、PID／PGID／出生标识、产物身份和 owner／root／task／attempt，登记落稳后才放行 ready barrier；确认退出后标记回收。不得保存 env、密钥或客户端凭据。guardian 崩溃后下一控制者依据 journal 核实残留；journal 不完整或身份复用时禁止自动 kill／调度，提示手动处理，不能仅看到锁释放就认为安全。
- `Guardian::serve(scope: RuntimeScope, selected: ArtifactIdentity) -> io::Result<()>`；持有 LeaseTable、Control listener、ProcessRegistry，不打开 repository。

- [ ] **Step 1:** process_fixture 用独立测试 helper 进程生成 parent + grandchild；通过 pipe 返回 PID／PGID／ready，不使用固定 sleep 等待。测试先调用 registry.spawn，再使父进程退出并验证整组仍需回收：

```rust
#[test]
fn owner_reclamation_waits_for_the_entire_group() {
    let mut registry = ProcessRegistry::default();
    let owner = ProcessOwner::Frontend("app".into());
    let command = process_fixture::parent_exits_leaving_child(owner.clone());
    registry.spawn(command, Deadline::after(Duration::from_secs(2))).unwrap();
    assert!(!registry.owner_is_stopped(&owner).unwrap());
    registry.terminate_owner(&owner, Deadline::after(Duration::from_secs(5))).unwrap();
    assert!(registry.owner_is_stopped(&owner).unwrap());
}
```

fixture 函数在 `tests/support/process_fixture.rs` 实现为启动当前 integration test 可执行文件（`std::env::current_exe()`）的 `--exact process_fixture_helper --ignored --nocapture` 固定角色。该 helper 测试标记 `#[ignore]`，必须有父测试传入的私有 ready FD 才执行；没有 FD 明确返回，不把普通 `--ignored` 当作启动授权。不猜其他 test target 的哈希路径，也不扫描 target。helper 无 LLM／数据库访问，握手建立后才开始写临时 marker；所有 parent Child 和 pipe 由 cleanup guard 持有。

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test managed_process_groups`，确认只 kill/wait 直接 child 不能过测试。
- [ ] **Step 3:** spawn 前建立受管 process group、注册归属和 ready barrier，登记失败不得开始执行。持有 child 句柄、组身份与出生标识，拒绝 stale PID；macOS 用系统进程 start time，Linux 用 /proc starttime／可用 pidfd，不引入按名字 kill。parent reap 前先确认所属组清空，不能在 group 身份失效后向复用 PGID 发信号。
- [ ] **Step 4:** daemon 异常死亡回收所有托管组；受控切换的退出不误触异常关 guardian。guardian 死亡由 daemon／executor 独立监控清理其可可靠控制的组后退出；不能依赖 Agent runtime 调度该监控。spawn ACK 前，guardian 必须把该组身份／owner 同步给存活的 owner host／daemon 独立监控者，取得 ACK 后才释放执行 barrier；前台 root 工具也有对应 host 监控，shared plugin 由 daemon 监控兜底。监控用独立 lifeline FD 的关闭／已核实 guardian 死亡触发，避免业务 RPC 卡死使失联检测永远不发生。监控者只持回收登记，不持续租权。guardian 在 spawn 与登记间崩溃，helper 因 barrier EOF 退出且未执行用户程序。组无法核实时记录故障并阻止下一代调度，不假报归零。
- [ ] **Step 5:** 测试非目标 FD 不继承、跨用户／远端不提供 terminate、B 客户端 marker 继续、未知孤儿拒绝自动复活；运行 §0.4，提交 `feat: supervise runtime process groups outside the business daemon`。

## Task 7: 按客户端隔离 Agent worker executor

**Depends on:** 5、6。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/executor/{mod,protocol,proxy,serve}.rs`、`tests/executor_isolation.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-subagent/{Cargo.toml,src/lib.rs}`、`yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`。
- Modify: `yi-agent-rs/crates/yi-agent/src/{config,main}.rs`、`yi-agent-rs/crates/yi-agent/Cargo.toml`；Create: `yi-agent-rs/crates/yi-agent/src/managed_runtime.rs`。
- Modify: `docs/project-management/{yi-agent-subagent,yi-agent-core,subagent-runtime,README}.md`。

**Interfaces:**
- `ProcessWorkerFactory` 实现现有 `AgentWorkerFactory::start(WorkerStart) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>` 与 `start_with_provider_turn_gate(WorkerStart, Option<Arc<dyn ProviderTurnGate>>) -> BoxFuture<'static, Result<WorkerHandle, WorkerError>>`。workspace service／registry、project matching、recovery context／preflight、availability、capacity、provider profile 留在 daemon 代理层的原有 application policy，不跨进程序列化 trait object；恢复 preflight 不启动 Agent／provider。
- `WorkerStartWire` 精确保留 `WorkerStart` 的 task_id、attempt_id、root_session_id、workspace_lease_id、workspace、workspace_mode、inherited_sandbox、message_capability、initial_user_messages、objective、fork_messages、model、extra_roots；现有可序列化 core 类型直接复用，其他类型新增显式 wire 转换。cancellation 映射本地 token＋Cancel 消息；root 有效配置／模型与权限覆盖作为明确的 `RootExecutionConfig` 随 Start 传递，不采用第一个 root 的配置污染同 executor 后续任务。敏感内容只经私有 FD 传输，Debug 脱敏。
- 内部 `ExecutorRequest::{Start { command_id: String, generation: String, owner: String, payload: WorkerStartWire }, Cancel { task: String, attempt: String }, Pause { task: String, attempt: String }, Message { task: String, attempt: String, id: String, body: String }}`。
- `ExecutorEvent::{Worker { task: String, attempt: String, payload: WorkerEventWire }, Watchdog { task: String, attempt: String, payload: WorkerWatchdogEventWire }, Trace { task: String, attempt: String, payload: TraceFact }, Stopped { task: String, attempt: String }, Rejected { command_id: String, reason: String }}`。WorkerEventWire／WorkerWatchdogEventWire 逐 variant 映射现有 core enum（含 message-consumed、delivery、budget-exhausted、recovery-conflict），不以任意 JSON 代替契约。
- `ExecutorProxy::send(&mut self, ExecutorRequest, Deadline) -> io::Result<()>`、`recv(&mut self, Deadline) -> io::Result<ExecutorEvent>`。双向 provider gate 子协议 `AcquireTurn { request_id, task, attempt, profile }`／`TurnGranted { request_id, permit_id }`／`ReleaseTurn { permit_id }`：daemon 调用原 gate 并持真实 permit，executor 只持代理 permit；drop／取消／执行器死亡幂等释放，断连时拒绝新 turn，不能换成本地计数器绕过全局限流。gate 队列不能阻塞取消和停止事件；迟到授权在 attempt 结束时立即释放。
- `serve_executor(control_fd: OwnedFd, owner: ClientRunId) -> anyhow::Result<()>`；隐藏 `internal-runtime executor` 只通过继承的私有 FD 接收配置，不从公共 RPC 接受任意执行命令。

- [ ] **Step 1:** 为 wire DTO 添加往返测试，覆盖 coding/read_only、workdir、sandbox、恢复证据、fork 快照、消息、permission、pause／watchdog／trace。不能使用真实 key 做快照。关键协议断言：

```rust
#[test]
fn completed_event_does_not_imply_stopped_event() {
    let completed = ExecutorEvent::Worker {
        task: "t1".into(), attempt: "a1".into(),
        payload: WorkerEventWire::Completed { report: "done".into() },
    };
    assert!(!matches!(completed, ExecutorEvent::Stopped { .. }));
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-subagent --test executor_isolation`，用两个 fake executor 通过就绪 pipe 后，一个忽略 cancel、另一个持续 marker；确认旧线程实现不能独立强停。
- [ ] **Step 3:** coordinator 仍在 daemon 唯一分配任务／workspace／资源；每客户端一个 executor，首次任务懒启动，可承载该客户端多个任务。Agent 配置与 provider 在 executor 内按原实例配置重建，会话覆盖随 Start 传递；禁止跨 client 凭据／权限／模型预算混用。executor 不持 runtime.sqlite，子代理工具通过 daemon 业务 IPC 回 coordinator。每次 Start 在实际执行前通过 owner-bound 私有 guardian 通道登记 `(generation, owner, task, attempt)` 并取得本次执行准入 ACK；guardian 维护／client-release 后拒绝新 ACK，旧 daemon 不能凭已有 executor FD 绕开屏障。前台 host 的 root-turn／工具入口与插件派生同样服从代际屏障，不能仅靠 5 秒状态轮询最终阻止执行；该窄通道无续租／任意 spawn 权限。
- [ ] **Step 4:** proxy 将取消、暂停、信箱、watchdog 和 trace 在内部通道桥接；非幂等 `(generation, owner, task, attempt, command_id)` 只接受一次，连接中断不得重放 Start。独立 observer 等真实 join 完成才发送 Stopped，Agent 自报 Stopped／假终态不足为证；executor 整体死亡由 guardian 的退出证据确认其全部 attempt 停止。正常一任务停止不退出其他任务，客户端回收超时才杀该客户端全部 executor 组。加入 gate 跨两个 executor 仍守全局上限、排队取消不泄漏 permit、executor 死亡／迟到 grant 释放 permit 的测试。
- [ ] **Step 5:** 实测大 fork payload 不受 16 KiB 控制帧误限；采用既有分块上传，内部非上传单帧上限明确为 1 MiB、超限显式错误且启动前失败。运行既有 attach_delegation、store fork 测试、core worker 测试和 §0.4，提交 `feat: execute managed workers in per-client process boundaries`。

## Task 8: 工具、插件与自动触发的归属链

**Depends on:** 5、6、7。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/managed_process.rs`、`tests/managed_process_policy.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-tools/{Cargo.toml,src/lib.rs,src/shell/bash.rs,src/process/manager.rs,src/process/tools.rs,src/process_group.rs}`。
- Modify: `yi-agent-rs/crates/yi-agent-mcp/{Cargo.toml,src/manager.rs}`；Create: `yi-agent-rs/crates/yi-agent-mcp/tests/managed_stdio_process.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/{bootstrap.rs,supervise.rs}`、`yi-agent-rs/crates/yi-agent-supervisors/src/{supervisor,watch}.rs`、`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`。
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/demands.rs`；Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/mod.rs`、`yi-agent-rs/crates/yi-agent-lifecycle/src/{types,client}.rs`。
- Modify: `docs/project-management/{yi-agent-tools,yi-agent-runtime,subagent-runtime,README}.md`。

**Interfaces:**
- `ManagedProcessPolicy::{Unmanaged, Frontend { client_run_id: String }}`；`validate_on_exit(&ManagedProcessPolicy, &str) -> Result<(), String>`。
- `ManagedSpawnSpec { executable: PathBuf, args: Vec<String>, env: Vec<(String, String)>, cwd: PathBuf }`、`ManagedSpawnReceipt { process_id: ManagedProcessId, pid: u32, pgid: i32 }` 位于轻量 types；`ManagedSpawnClient::connect(fd: OwnedFd) -> io::Result<Self>`、`spawn(&mut self, spec: ManagedSpawnSpec, stdio: [OwnedFd; 3], deadline: Deadline) -> io::Result<ManagedSpawnReceipt>`、`terminate(&mut self, process_id: &str, deadline: Deadline) -> io::Result<()>` 位于轻量 client。只使用 guardian 签发的私有 owner-bound capability，owner 由服务端连接确定而非入参自报；工具不获得 register／续租权限，不把 spawn 加入公共控制操作。stdin／stdout／stderr 用独立 FD 传递，receipt 不承诺已完成进程退出；输出不挤进 16 KiB 控制帧。
- `ManagedExit { code: Option<i32>, signal: Option<i32>, group_stopped: bool }`；`ManagedChild::wait(&mut self, deadline: Deadline) -> io::Result<ManagedExit>`、`try_wait(&mut self) -> io::Result<Option<ManagedExit>>`、`terminate_group(&mut self, deadline: Deadline) -> io::Result<ManagedExit>`。ManagedSpawnClient 的 receipt 转成 ManagedChild，状态由 guardian 的 waitpid／组核验产生；请求绑定 owner、代际、process_id，重复 wait 可读缓存但不能跨 owner 控制。stdio 独立管道保持 stdout／stderr 分离、背压与 EOF；bash 原硬超时／idle 超时、cancel、drop 清组、sandbox.command 与 cwd／session_env 语义不变，process manager 原输出 cursor／ready 检测／exit code 语义不变。drop 只发尽力终止，guardian 的 owner 回收负责兜底，不假装 drop 已同步回收。
- `PluginDemandTable::acquire(client: &str, plugin: &str)`、`release(client: &str, plugin: &str)`、`owners(plugin: &str) -> Vec<ClientRunId>`、`release_client(client: &str)`；定义于 supervisors 的新 `lifecycle/demands.rs` 并导出。

- [ ] **Step 1:** 添加 keep 测试：

```rust
#[test]
fn managed_keep_is_rejected_without_silent_conversion() {
    let policy = ManagedProcessPolicy::Frontend { client_run_id: "app".into() };
    assert!(validate_on_exit(&policy, "keep").is_err());
    assert!(validate_on_exit(&policy, "kill").is_ok());
    assert!(validate_on_exit(&ManagedProcessPolicy::Unmanaged, "keep").is_ok());
}
```

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-tools --test managed_process_policy`；为两个 demand owners 添加一个 release 不停共享查询、最后 release 停服务、重新 acquire 可再启动的测试。
- [ ] **Step 3:** 接入 shell/bash、process_start 及插件 supervisor 的全部 spawn；Task 6 registrar 必须收到工具实际 PGID／归属并 ACK 后工具执行；guardian 能回收前台 TUI／app-server root 工具，而不仅是 daemon 孙任务。直接 spawn 的现有 setpgid helper 不得形成未登记的新组。受限 helper 先等待 ready barrier，再 exec 原 sandbox 包装程序；保留原沙箱策略，不因 guardian cwd／权限不同扩大权限。为 wait／signal、混合 stdout／stderr、EOF、超时、取消、丢 transport、组内父先退的真实子进程逐项测试。MCP `manager.rs` 的 `TokioChildProcess::new(cmd)`（包含探测与 reconnect）改用同一 owner-bound spawn＋stdio transport 适配；若 rmcp 不接收该 Child，则使用其通用读写 transport 承接管道，不能偷偷改回直接 spawn。HTTP MCP 不杀外部服务；本地 stdio 子孙必须登记。managed bootstrap 不在获得 owner 前执行 MCP probe；为 probe／正常调用／重连分别验证不残留进程。
- [ ] **Step 4:** 每次插件自动派生必须显式选择唯一仍活跃需求 owner；多个候选使用本次触发的显式来源，无来源则不执行而非随机移交。插件持久 enabled／resident 登记不算 demand，离开插件面且无待处理工作释放；schedule 保留既有 skip／显式 catch-up-once，不在无客户端期执行。
- [ ] **Step 5:** 验证 A 退出不会把任务转给 B、插件父先退出仍清孙进程；单任务 join 后再向 guardian 验证该 TaskExecutionKey 的全部工具组停止，二者均成立才调用 store execution_stopped。进程管理工具不因“允许在 Agent 多轮间后台运行”而取得跨 task／owner 的 keep 权，task 结束的回收范围明确且不得影响同 executor 其他任务。运行 tools／MCP／runtime／supervisors 对应测试、§0.4，提交 `feat: scope managed tools and plugin demand to frontend owners`。

## Task 9: 有界客户端回收与最后客户端 drain

**Depends on:** 4–8。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/reclaim.rs`、`tests/client_reclamation.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{mod,guardian}.rs`、`yi-agent-rs/crates/yi-agent-store/src/{lifecycle,runtime}.rs`。
- Modify: `docs/project-management/{subagent-runtime,yi-agent-store,README}.md`。

**Interfaces:** 使用 Task 1 的 `ReclaimResult`；`Reclaimer::release(&mut self, grant: ClientGrant, deadline: Deadline) -> io::Result<OperationId>` 异步启动、立即返回；`result(&self, operation: &str) -> Option<ReclaimResult>`。Release 后立刻撤销业务、续租与该 run 的重新登记权限，但在独立的完成记录中保留该 owner 凭据对本次 operation 的只读查询权最多 60 秒；ReclaimStatus 不计客户端、不续租、不阻止最后退出。guardian 退出前把脱敏最终结果按 operation ID 存在同 scope，正常退出的壳可读取自己的结果，不能因 token 已释放就无法确认收尾。结果包含 scope／daemon generation／operation／owner，0600 原子写入且不含 credential；前端只认自己 Release 返回的 operation ID 和 scope，不凭任意文件名判成功。结果 60 秒后在下次受控访问时清除，不为清理保活 guardian；released tombstone 在本 guardian 代内保留，最终退出后旧代际 credential 本身无效。

- [ ] **Step 1:** 为 A+B 测试显式 release 立即关闭 A 准入，B 不进入 global drain；最后 release 对整个 scope drain。新 `tests/client_reclamation.rs` 中定义 `ReclaimFixture`，使用真实 LeaseTable／Reclaimer／RuntimeClients，注入可控 Clock 与记录式 ProcessBackend；提供 `with_two_clients() -> Self`、`release(&mut self, client: &str)`、`can_admit(&self, client: &str) -> bool`、`global_drain_started(&self) -> bool`。测试实际回收决策，不新增仅为测试布尔值的生产函数：

```rust
#[test]
fn another_tui_prevents_project_wide_drain() {
    let mut f = ReclaimFixture::with_two_clients();
    f.release("app");
    assert!(!f.can_admit("app"));
    assert!(f.can_admit("tui"));
    assert!(!f.global_drain_started());
    f.release("tui");
    assert!(f.global_drain_started());
}
```

该 fixture 的 backend 仅替换 OS／时钟，不自行实现一份回收算法；Task 18 再验证真实任务／进程。

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test client_reclamation`；假时钟推进至 30 秒验证异常 EOF 与明确 Release 的差异，不固定 sleep 30 秒。
- [ ] **Step 3:** 使用一个绝对优雅截止时间并行请求 foreground host、daemon root、executor、工具／插件 checkpoint；10 秒后对该 owner 的受管组发终止，后 5 秒确认停止。RPC 读写共用剩余预算；control／lease 服务与业务 drain 分开调度，不被卡住的 coordinator 请求阻塞。
- [ ] **Step 4:** 最后退出转 global drain，拒绝新业务 attach；新客户端注册得到 WaitingNextGeneration，旧 daemon 和执行器确认退出且运行锁释放后才允许 bring-up。单个退出只 client drain。不可中断 OS 进程记录失败且保留 scope 执行屏障，不虚报成功。
- [ ] **Step 5:** 测试运行资源停止后才释放、历史报告保留、lease-only 离开、首客户端从未到来 30 秒退出；运行 §0.4，提交 `feat: reclaim departed client work within a shared shutdown budget`。

## Task 10: 旧 watchman 与 unmanaged runtime 的安全迁移助手

**Depends on:** 3、6、9。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/migration.rs`、`tests/runtime_migration.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/{watchman,install,watch}.rs`、`src/lifecycle/mod.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{server,settings_store}.rs`、`src/server/watchman_install_tests.rs`。
- Modify: `docs/project-management/{subagent-runtime,yi-agent-app-server,README}.md`。

**Interfaces:**
- `LegacyProbe { mode: RuntimeMode, identity_verified: bool, influence_complete: bool, can_retire_without_host_exit: bool, self_heal_frozen: bool }`。
- `MigrationDisposition::{ManagedReady, ExplicitTemporaryUse, ManualActionRequired}`。
- `MigrationPlan::inspect(scope: &RuntimeScope, probe: LegacyProbe) -> Self` 只读；`execute(&mut self, confirmation: &str, deadline: Deadline) -> io::Result<MigrationDisposition>` 只在用户同意后做破坏性迁移。
- `LaunchctlBackend` trait：`loaded_job(&str) -> io::Result<Option<serde_json::Value>>`、`bootout(&str) -> io::Result<()>`；测试 fake 注入，不调用用户 launchctl。

- [ ] **Step 1:** 测试 loaded job 与 plist 归属不一致拒绝卸载，且旧 TUI 不支持退休时不能杀宿主：

```rust
#[test]
fn legacy_embedded_runtime_requires_host_cooperation() {
    let probe = LegacyProbe {
        mode: RuntimeMode::LegacyEmbedded, identity_verified: true,
        influence_complete: false, can_retire_without_host_exit: false,
        self_heal_frozen: false,
    };
    let plan = MigrationPlan::inspect(&test_scope(), probe);
    assert!(matches!(plan.disposition(), MigrationDisposition::ManualActionRequired));
}
```

`test_scope() -> RuntimeScope` 在同测试文件内用 TempDir 构造并保留目录句柄到测试结束；`MigrationPlan::disposition(&self) -> MigrationDisposition` 在本任务定义。

- [ ] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test runtime_migration` 和 app-server watchman 测试，先确认旧“已安装”判据不能通过。
- [ ] **Step 3:** 分别读取 `ai.yi-agent.resident-daemon-watchman` 与 `ai.yi-agent.board-watchman` 的 loaded job／plist；按实例与可执行身份核对，存在歧义就停止并提示。匹配时 bootout、删除该 plist，保留 resident 配置；失败时禁自动切换。
- [ ] **Step 4:** 新 managed 路径移除 plugin/install、设置开关和 resident loop 自动重装／无人复活行为；旧 app-server 或旧 watchman 若无法冻结，要求用户退出。不能仅靠新协调锁假设任意旧二进制会合作。旧独立实例只有能核实安全停止和全组回收时才可自动迁移，其他情况只给手动指引。
- [ ] **Step 5:** 测试其他实例／用户不受影响、明确暂用保持 migration pending、迁移不读写真实 LaunchAgent；运行 §0.4，提交 `fix: retire legacy self-healing paths before managed runtime migration`。

## Task 11: 唯一 managed bring-up 与私有运行角色

**Depends on:** 2–10。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/bringup.rs`、`tests/managed_bringup.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/{daemon,lifecycle/mod}.rs`、`yi-agent-rs/crates/yi-agent-subagent/src/{attach,binding}.rs`。
- Modify: `yi-agent-rs/crates/yi-agent/src/{config,main,managed_runtime}.rs`、`yi-agent-rs/crates/yi-agent-subagent/Cargo.toml`、`yi-agent-rs/crates/yi-agent-lifecycle/src/{types,client}.rs`。
- Modify: `docs/project-management/{yi-agent-cli,yi-agent-subagent,subagent-runtime,README}.md`。

**Interfaces:**
- `BringUpIntent::{Business, LeaseOnly, Inspect}`；Inspect 从不登记／启动。
- `BringUpResult::{Connected { identity: RuntimeIdentity, control_path: PathBuf }, VersionChoiceRequired { identity: RuntimeIdentity }, ManualActionRequired { reason: String }}`。
- `ensure_managed(scope: &RuntimeScope, expected: &ArtifactIdentity, intent: BringUpIntent, deadline: Deadline) -> io::Result<BringUpResult>`。
- 隐藏命令 `internal-runtime guardian`、`internal-runtime daemon`、Task 7 executor；公开 daemon Start／Serve 保持旧显式模式。受管角色无有效继承 bootstrap FD 时拒绝运行。
- 本任务在轻量 types 定义 `Compatibility::{SameBuild, CompatibleMismatch, ControlOnly, LegacyTemporaryChoiceRequired, Unknown}` 与 `classify_compatibility(identity: &RuntimeIdentity, expected: &ArtifactIdentity, required_capabilities: &[String]) -> Compatibility`，Task 16 只包装状态／展示。按业务协议范围与功能能力交集验证；业务可用但 lifecycle 缺失必须显式暂用，unknown 禁任务。Task 12 的早期通知使用 Identity／Operation，Task 16 补充 counts／完整客户端概览，不能在 Task 11 调用 Task 16 尚未存在的 API。

- [ ] **Step 1:** 并发启动测试记录唯一运行锁持有者；Error Status、权限错误、control 超时不能被当成无实例：

```rust
#[test]
fn inspection_cannot_create_runtime() {
    let (scope, artifact, _dir) = empty_scope_and_artifact();
    let result = ensure_managed(&scope, &artifact, BringUpIntent::Inspect,
        Deadline::after(Duration::from_secs(2))).unwrap();
    assert!(matches!(result, BringUpResult::ManualActionRequired { .. }));
    assert!(!scope.runtime_dir.join("runtime.sqlite").exists());
}
```

`empty_scope_and_artifact() -> (RuntimeScope, ArtifactIdentity, TempDir)` 在本测试文件定义，artifact 指向只输出静态身份的测试 helper，不运行真实 LLM。

- [x] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test managed_bringup` 和 `cargo test -p yi-agent-subagent --test attach_delegation`。
  实测 22 passed 与 16 passed（2026-11-04 复核）。
- [ ] **Step 3:** 协调锁内探测 control、核对 scope／instance／selection／schema；首次启动 guardian，guardian 再启动唯一 daemon，未登记期间禁止恢复执行。多个 attach 等同一启动结果，不按项目别名起新 daemon。startup 默认总预算 30 秒，Inspect 每次不超过 2 秒。
- [ ] **Step 4:** 保留 YI_AGENT_HOME、模型目录与基础配置，root 覆盖显式下发。Task 2 目标产物以受控私有目录中的校验副本固定字节、0700 目录／可执行只对当前用户；selection 指向该受控副本并保留 digest，避免 bundle 被替换后旧客户端改用 PATH。复制前后复核 digest／build identity，启动握手再次核对；失败不回退。副本是程序产物缓存，不是新 runtime 数据库。已选定且摘要正确的缓存即正式目标，原 bundle 被删不使它自动失效；新 App 点击切换仍必须先验证其当前 bundle 清单并生成目标缓存。选定缓存丢失／被替换则报错，不偷偷从旧 bundle／PATH 修复。
- [ ] **Step 5:** 替换 attach 中 embedded 自动路径为 managed 分支，但最终默认开关留 Task 18；binding 代际退休保留 SingleShot 不重放。手工 daemon、兼容旧版暂用走显式 legacy 分支；unknown 不放行业务。
- [ ] **Step 6:** 验证 bundle 变更、未来 schema、daemon 未 bind 暂态、不同实例覆盖冲突；运行 §0.4，提交 `feat: unify managed runtime startup under versioned scope coordination`。

## Task 12: TUI 与 headless run 的生命周期接入

**Depends on:** 9、11。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`、`src/tui/{app,state,subagents,statusbar}.rs`、`yi-agent-rs/crates/yi-agent-lifecycle/src/client.rs`。
- Create: `yi-agent-rs/crates/yi-agent/tests/frontend_runtime_lease.rs`。
- Modify: `docs/project-management/{yi-agent-tui,yi-agent-run,README}.md`。

**Interfaces:** lightweight crate client 模块新增 `FrontendLease::register(scope: &RuntimeScope, id: ClientRunId, kind: ClientKind, mode: LeaseMode, deadline: Deadline) -> io::Result<Self>`、`grant(&self) -> &ClientGrant`、`release(&mut self, deadline: Deadline) -> io::Result<OperationId>`；心跳在拥有者前端线程／runtime 内运行，Drop 仅尽力关闭连接，不阻塞 15 秒。

- [x] **Step 1:** 为 TUI 正常退出、Ctrl+C、headless 成功／失败、只读命令加回收测试；关键 shell 角色断言写入新 integration target：

```rust
#[test]
fn internal_worker_is_not_a_frontend_kind() {
    let kinds = [ClientKind::App, ClientKind::Tui, ClientKind::Headless];
    assert_eq!(kinds.len(), 3);
    assert!(serde_json::from_str::<ClientKind>("\"Worker\"").is_err());
}
```

同时真实启动 headless fake provider，退出后从 guardian Identity 观察该 identity 已释放；不能仅依赖枚举测试。

- [x] **Step 2:** 运行 `cargo test -p yi-agent --test frontend_runtime_lease`。
  实测 9 passed（`a_headless_run_registers_and_releases_its_lease`、`a_read_only_command_never_registers_a_frontend`、
  `one_frontend_leaving_does_not_reclaim_anothers_work`、`a_tui_exit_releases_its_lease` 等），
  其中新增的 `a_headless_runs_tool_processes_are_owned_by_that_run` 是 Task 8 Step 3 的端到端判据。
- [x] **Step 3:** TUI 启动生成一次 client_run_id，连接的所有会话共享该身份；退出先关闭自己的前台 Agent／受管工具，再 release 并有界等结果。headless run 结束无论成功失败都走同一路径，signal handler 只置退出信号，不在 handler 做锁／IPC。
  实现：`frontend_lease::new_client_run_id("tui"/"run")` 每次进程启动生成一次；TUI 的 `lease_handle` 在 attach 成功后才交给 driver，
  失败分支显式 `release()`（不能靠 Drop——Drop 只停心跳，租约要等 30s 过期，期间该前端的活会被当作无主）。
- [x] **Step 4:** lease heartbeat 不交给 executor／工具；通知显示构建差异、切换、任务中断及不兼容需升级。新版停机后旧期望构建不能覆盖 selection。只读 daemon status／内部调用不登记，不从 history 扫描项目常驻。
  实现：心跳线程由 `FrontendLeaseHandle` 自己持有（`crates/yi-agent/src/frontend_lease.rs`），不进 executor、不交给工具；
  只读命令走 `attach_tui_runtime` 之外的分支，不 register。App 端由桌面壳持连接（Task 13）。
  判据：`cargo test -p yi-agent --test frontend_runtime_lease`（9 passed，含 `a_read_only_command_never_registers_a_frontend`）。
- [x] **Step 5:** 多 TUI＋App 角色集成验证一个退出其他继续；执行 §0.4，提交 `feat: tie tui and headless runtime work to frontend lifetimes`。
  实现：`one_frontend_leaving_does_not_reclaim_anothers_work` 与 supervisor 侧 `client_reclamation` 的 A+B 用例共同覆盖；
  第四个前端只会让 scope 继续服务，最后一个离开才 drain。
  判据：`cargo test -p yi-agent --test frontend_runtime_lease`、`cargo test -p yi-agent-supervisors --test client_reclamation`。

## Task 13: 桌面壳拥有身份、侧车恢复与真正退出屏障

**Depends on:** 1、9、11、12。

**Files:**
- Create: `desktop/src-tauri/src/runtime_lifecycle.rs`。
- Modify: `desktop/src-tauri/{Cargo.toml,Cargo.lock,src/lib.rs,src/bridge.rs,src/window.rs}`。
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{server,protocol}.rs`。
- Modify: `docs/project-management/{desktop,yi-agent-app-server,README}.md`。

**Interfaces:**
- `DesktopRuntimeManager::new(client_run_id: String) -> Self`；异步方法 `acquire_project(&self, scope: RuntimeScope, mode: LeaseMode) -> io::Result<()>`、`release_project(&self, scope_key: &str, deadline: Deadline) -> io::Result<()>`、`shutdown(&self, deadline: Deadline) -> Vec<ReclaimResult>` 均为 `async fn`。底层 Task 1 阻塞 socket client 仅在受控独立 I/O worker 使用，不能阻塞 Tauri 主循环／Tokio executor；剩余 deadline 传入 worker，不让 spawn_blocking 形成无法取消的无限等待。
- desktop 使用 Task 1 轻量 `types.rs` 中的 ReclaimResult；supervisors 只 re-export，不复制结构，也不让桌面依赖 supervisors。
- app-server 只可请求桌面壳 acquire／detach 会话接线；壳通过现有本地 stdio 的初始化消息传入不具续租能力、绑定 root／代际的业务授权，不把客户端原始凭据送进 sidecar。该消息只由本地 bridge 生成，不能从 relay initialize 原样转发。App client credential 不经 argv、环境、WebSocket／relay 或工具继承。

- [ ] **Step 1:** 在 runtime_lifecycle.rs 同模块定义 FakeControl（记录 release 到达的项目／时间，等待可控 barrier 后返回结果），通过测试构造器 `DesktopRuntimeManager::with_control(id, control)` 注入，提供 `control.await_all_releases(3)` 和 `control.finish_all()`。使用 Tokio paused clock 测试三个项目并行共享同一总 deadline：

```rust
#[tokio::test(start_paused = true)]
async fn multi_project_quit_releases_in_parallel_under_one_deadline() {
    let (manager, control) = three_project_manager();
    let task = tokio::spawn(async move {
        manager.shutdown(Deadline::after(Duration::from_secs(15))).await
    });
    control.await_all_releases(3).await;
    tokio::time::advance(Duration::from_secs(10)).await;
    control.finish_all();
    let results = task.await.unwrap();
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|result| result.stopped));
}
```

`three_project_manager() -> (DesktopRuntimeManager, FakeControl)` 在同模块构造三个实际 manager 项目登记，不以独立 map 假冒 manager。新增 timeout 用例使三个项目永不返回，推进 15 秒后 manager 必须结束并有三份失败结果；若实现串行，await_all_releases 即不能满足且测试受外层超时保护。Deadline 的 Clock 在测试可注入，与 paused clock 同步；不能拿真实 std::Instant 配 paused Tokio 假装测过总预算。

- [ ] **Step 2:** 运行 `cargo test --manifest-path desktop/src-tauri/Cargo.toml runtime_lifecycle`，确认 Cmd+Q 当前未挂退出时测试失败。该任务先在 desktop Cargo.toml 的 dev-dependencies 启用 Tokio `test-util`，不等 Task 18 才修测试编译；增加 fake clock 的一致性测试。
- [ ] **Step 3:** Sidecar 的 desired 状态先设 shutting_down，禁止 respawn；`RunEvent::ExitRequested` prevent_exit 一次，由独立任务并行 shutdown 全部实际登记项目，完成／超时后调用最终退出且避免再次拦截。macOS CloseRequested 仍 hide，不 release；其他平台真实退出也接入。
- [ ] **Step 4:** 桌面壳直接持有生命周期连接与心跳，app-server 重启保持 id；旧 sidecar detach 不能注销 App。没有使用需求的历史项目不 acquire，离开且无工作／plugin demand 时 release；实际后台工作继续时维持相应项目租约。
- [ ] **Step 5:** 前台 Agent 停止与受管 root 工具回收包含在同一全局预算；deadline 到后记录各项目结果，结束壳不虚报成功。用 mock Tauri Runtime 测试隐藏、真正退出、退出重入、sidecar crash／respawn。
- [ ] **Step 6:** 验证 `cargo tree --manifest-path desktop/src-tauri/Cargo.toml` 未通过 lifecycle 引入 rusqlite；执行桌面 Rust 测试、§0.4，提交 `feat: own app runtime leases in the desktop shell until real quit`。

## Task 14: 只读影响快照、一次性确认与原子维护屏障

**Depends on:** 3、5、9、10、13。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/preview.rs`、`tests/runtime_restart_preview.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/{types,leases}.rs`、`yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{mod,guardian}.rs`、`yi-agent-rs/crates/yi-agent-store/src/lifecycle.rs`。
- Modify: `docs/project-management/{yi-agent-lifecycle,subagent-runtime,README}.md`。

**Interfaces:**
- `ImpactCounts { clients: u64, pending_clients: u64, running: u64, queued: u64, plugins: u64, complete: bool }`；`ImpactEntry { kind: String, id: String, owner: Option<String> }`。在轻量 types 定义，下列 DTO 同属 Task 14，不在后续 task 重定义。
- `ImpactRevision { guardian: u64, coordinator: Option<u64> }`，派生 Eq；两个权威各单调递增，不用未证明无碰撞的 hash／简单相加。coordinator=None 只表示业务无响应、影响未知，绝不解释成 0／无任务。
- `RestartPreview { snapshot_id: String, token: ConfirmationToken, revision: ImpactRevision, counts: ImpactCounts, entries: Vec<ImpactEntry>, next_cursor: Option<String>, requires_second_confirmation: bool, can_restart: bool, blocked_reason: Option<String> }`。
- `ConfirmationContext { scope_key: String, daemon_instance_id: String, target: ArtifactIdentity, revision: ImpactRevision }`；`PreviewStore::preview(client: &ClientGrant, context: &ConfirmationContext, impact: Vec<ImpactEntry>, now: Duration) -> RestartPreview`；`confirm(client: &ClientGrant, token: &str, operation_id: &str, current: &ConfirmationContext, now: Duration) -> Result<OperationId, String>`；`page(client: &ClientGrant, snapshot: &str, cursor: &str, current: &ConfirmationContext) -> Result<RestartPreview, String>`。分页验证 owner 与完整 context，不能凭 cursor 越权读其他预览。
- Task 14 在 ControlRequest 新增 `Preview { grant, target }`、`PreviewPage { grant, snapshot_id, cursor }`、`Confirm { grant, token, operation_id }`，ControlResponse 对应上述 DTO。operation_id 是前端在点击确认前生成的随机 ID，服务端在任何破坏性动作前绑定 token 并记录；同 operation 只可查询，重复 Confirm 返回已消费／已有 operation，绝不重新执行。Operation 查询可用该 ID，即便确认响应丢失也不需第二次确认。
- 正常快照收集采用读取 guardian revision → coordinator 完整影响／revision → 再读 guardian revision，一致才签 token；收集超时不得拼接半张快照：转为 `coordinator=None, counts.complete=false` 的明确 unknown 预览。只有 managed guardian 仍持有目标 daemon／executor／工具全组身份、所有旧代际执行入口可独立封闭，且旧前端具备维护契约时，才可提供“影响未知、将中断整个 scope”的二次确认；缺一条件则不可自动重启，给手动指引。unknown 预览列已核实的客户端／进程下界，不冒充完整任务数。
- 正常确认先取得协调权、在 guardian 串行入口验证其分量并建立短时 admission reservation，再在 coordinator 同一入口比较 coordinator 的 Some 分量且关闭全部业务准入。reservation 阶段暂停可增加影响的登记／demand／spawn，**不得冻结心跳、Release、死亡监控**；这些减少影响的变化递增 revision 并使未提交确认失败。协调完成后复核 guardian 分量，成功才一次消费 token／记录 operation／开始 drain。任何跨边界失败均撤销未提交 reservation；coordinator 已关闭但尚未 drain 时仅在原实例仍健康且 scope 身份一致下撤销屏障，否则 fail-closed。每步 2 秒请求并受剩余预算约束，不能把业务锁持有到整个切换结束。屏障提交后新登记只到 WaitingNextGeneration，退出继续生效。
- 无响应路径在 guardian 验证 unknown token 后，独立撤销旧代际全部执行准入／capability，并由 executor、工具与前台 host 的私有控制面确认屏障；不等待 coordinator 应答才封闭执行。随后按 Task 15 的 10 + 5 秒预算尽力 checkpoint／核实强停，锁或任一组未确认停止就失败，绝不启动新版。若确认前 coordinator 恢复、范围／revision 变更，返回 stale，重新完整预览而不自动改变用户已确认的范围。增加 `unresponsive_managed_daemon_can_be_confirmed_with_explicit_unknown_impact` 与 `unknown_impact_without_independent_fence_requires_manual_action` 测试。

- [x] **Step 1:** 添加重复确认、过期、代际／目标变化、分页 revision 变化测试；关键纯函数用例：

```rust
#[test]
fn heartbeat_does_not_change_impact_revision() {
    let mut table = LeaseTable::new();
    let grant = table.register("app".into(), ClientKind::App, LeaseMode::Business, Duration::ZERO).unwrap();
    let before = table.impact_revision();
    table.heartbeat(&grant, Duration::from_secs(5)).unwrap();
    assert_eq!(before, table.impact_revision());
}
```

本任务给 Task 4 LeaseTable 增加 `impact_revision(&self) -> u64`，只在登记／释放及实际影响变化时递增，单纯重连 socket 不新增 client。

- [x] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test runtime_restart_preview`；变更集合与 confirm 同时竞争，断言不可能“旧 token 成功但新任务逃过确认”。
- [x] **Step 3:** token 绑定发起者、scope、代际、目标固定副本身份、完整 revision，CSPRNG、60 秒、一次消费；预览无 drain 无维护锁，取消无副作用。确认校验与维护准入原子化；重复点击返回已消费／已有 operation，不开始第二次切换。
- [x] **Step 4:** 页内容按 16 KiB 实际序列化长度切分，完整分类计数不截断。其他客户端／活跃或排队任务／自动工作／unknown 都需要二次确认；只有本 App 空闲可直接消费预览 token。旧实例 token 在壳管理入口本地维护，不能要求旧 daemon实现新协议；未知不等于可强杀。
- [x] **Step 5:** 测试控制面兼容但业务不兼容时 lease-only 仍能预览；执行 §0.4，提交 `feat: require revision-bound confirmation for runtime replacement`。

## Task 15: 有界版本切换、guardian exec 交接与失败恢复

**Depends on:** 2、3、6、9、10、11、14。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{switch,handover}.rs`、`tests/runtime_switch.rs`、`tests/guardian_handover.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/{mod,guardian,selection}.rs`、`yi-agent-rs/crates/yi-agent/src/managed_runtime.rs`。
- Modify: `docs/project-management/{subagent-runtime,README}.md`。

**Interfaces:**
- `SwitchController::start(operation: OperationId, target: ArtifactIdentity, coordination: CoordinationGuard, deadline: Deadline) -> io::Result<()>`；`status(&self, operation: &str) -> Option<OperationStatus>`；start 不在控制 request handler 阻塞等待结束。Task 14 成功确认时把已持协调权移交后台 operation，不重新获取同一把锁；启动后台 operation 失败保留失败记录并按准入 reservation 规则恢复／fail-closed。
- `HandoverState { version: u16, scope_key: String, operation: OperationStatus, consumed_tokens: Vec<String>, captured_monotonic_ns: u64, remaining_leases_ms: Vec<(ClientGrant, u64)>, remaining_start_budget_ms: u64 }`；真实实现还保存 LeaseTable 的 released tombstone／epoch，不能让交接复活已释放身份。captured_monotonic_ns 使用同机跨进程可比较的 OS 单调时钟，验证时减去交接耗时；不能把进程本地 Instant 直接序列化或重发完整预算。
- `handover::encode_state(&HandoverState) -> io::Result<OwnedFd>`；`validate_state(fd: BorrowedFd, scope: &RuntimeScope, target: &ArtifactIdentity) -> io::Result<HandoverState>`；`exec_target(target: &ArtifactIdentity, state: OwnedFd, preserve: &[RawFd]) -> io::Result<Infallible>`。HandoverState 还显式携带 selected target、guardian revision、operation/token 绑定、释放结果查询记录、各保留 FD 的角色／epoch 与帧缓冲状态；仅保存 ClientGrant 列表不足以恢复控制服务。旧请求 handler 在有界 quiesce 后交接：不得截断已接收的 Release／Heartbeat，也不得在新程序重执行已消费 Confirm；未完整帧保留 parser 缓冲或在明示可重连的连接上关闭，不能假装仅继承 socket FD 即可无损。exec 交接状态体有独立上限 4 MiB，超过则在停旧前拒绝自动 handover并给手动指引，不套用 16 KiB 控制帧上限。

- [x] **Step 1:** fake 后端记录严格切换顺序并注入每阶段失败；纯状态约束测试作为起点：

```rust
#[test]
fn no_clients_after_stop_is_not_ready() {
    let status = OperationStatus {
        operation_id: "op".into(), phase: SwitchPhase::StoppedNoClients,
        detail: None, ready_identity: None,
    };
    assert!(status.ready_identity.is_none());
    assert!(!matches!(status.phase, SwitchPhase::Ready));
}
```

集成测试必须验证真进程 stopped-no-clients 不启动新 daemon，而非仅验证 enum。

- [x] **Step 2:** 运行 `cargo test -p yi-agent-supervisors --test runtime_switch`、`cargo test -p yi-agent-supervisors --test guardian_handover`。
- [x] **Step 3:** 维护期持协调锁，冻结旧自愈与全部准入，通知客户端；10 秒并行 checkpoint、5 秒回收已核实 daemon／executor／工具组；确认全部退出和运行锁释放，再原子写目标 selection。任一失败停止，不删锁，不杀 TUI，不降级，不开始新版调度。
- [x] **Step 4:** 目标构建／schema／handover 能力在停旧前只读预检，磁盘不足或不支持交接即提示手动退出，不先停旧才发现无法接管。旧对象停止后，若 guardian 构建不同，用私有匿名 FD 交接控制监听、登记连接、协调锁及操作状态；其余 FD 关闭。在单线程／安全 exec 边界交接，不在多线程 Rust fork 后执行分配器逻辑。验证 FD 类型、数量、scope、owner、版本、目标 build，失败不得恢复运行旧任务。
- [x] **Step 5:** 剩余租约扣除交接耗时，不能重新发完整 30 秒。已消费 token 不可重用，凭据不落磁盘。30 秒内启动新 daemon，核对 guardian＋daemon build、代际、schema和两条通道 ready；timeout 回收本次受管进程。客户端重新 attach 签新业务授权，只读操作可重试，SingleShot 不重放。
- [x] **Step 6:** 全部客户端退出则完成停旧并保存选择，状态 StoppedNoClients；有等待者仅授权下一代。发起者消失不取消已批准停旧；guardian 消失后下一控制者只能核实并隔离残留，不能继承强杀 token。测试启动／迁移失败保留数据库与错误阶段，bundle 中途替换、旧 PID 复用均不误杀。
- [x] **Step 7:** 运行受影响 supervisors／store／subagent 测试和 §0.4，提交 `feat: switch managed runtime generations with safe guardian handover`。

## Task 16: 只读状态、兼容矩阵与本地桌面控制命令

**Depends on:** 11–15。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/runtime_status.rs`。
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/{lib,server,protocol}.rs`、`src/rpc/mod.rs`、`yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`。
- Modify: `desktop/src-tauri/src/{runtime_lifecycle,lib,bridge}.rs`、`desktop/src/lib/generated/`、`yi-agent-rs/crates/yi-agent-lifecycle/src/{types,client}.rs`。
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/runtime_status_tests.rs`。
- Modify: `docs/project-management/{yi-agent-app-server,desktop,README}.md`。

**Interfaces:**
- app-server `runtime/status` 只读返回 `RuntimeStatusView { identity: RuntimeIdentity, expected_build_id: Option<String>, expected_display_version: Option<String>, compatibility: Compatibility, lifecycle_managed: bool, counts: ImpactCounts, clients: Vec<ClientSummary>, active_operation: Option<OperationStatus>, local_restart_available: bool, message: Option<String> }`。`Compatibility` 复用 Task 11 的 enum；`ClientSummary { client_run_id: String, kind: ClientKind, mode: LeaseMode }`，没有正文／凭据。guardian 提供同一轻量状态源 `Status`，完整客户端列表按同快照分页，RuntimeStatusView 的 clients 仅当前页，另带 `clients_next_cursor: Option<String>`，counts 始终全量且 complete 明确。身份最小响应不夹带无界客户端数组。前端按能力交集决定业务操作，不能单一 bool 抹掉 control-only 与 legacy。状态、expected display version 由当前前端配套清单产生，不以 wrapper 号兜底。调用这个 RPC 不续租／启动。
- Tauri 命令 `runtime_status(scope_key)`、`runtime_preview(scope_key)`、`runtime_confirm(scope_key, token, operation_id)`、`runtime_operation(scope_key, operation_id)`；它们从壳状态取当前配套 ArtifactIdentity，不接受 WebView 提交任意 executable／build target。桌面壳持有待查 operation ID，即使 WebView 刷新也能从 status 找回已有操作。
- 跨 websocket／relay 的 app-server 不新增 Confirm／Kill RPC；远端展示状态但 `local_restart_available=false`。

- [x] **Step 1:** 在 app-server 测试中遍历同构建、不同兼容、业务不兼容／控制兼容、unknown、legacy 的矩阵；已有业务 Error 响应不能被识别成 healthy。禁止 RPC 路由断言：

```rust
#[test]
fn destructive_restart_is_not_a_remote_rpc_method() {
    let method = "runtime/confirmRestart";
    assert!(!runtime_status::is_read_only_runtime_method(method));
    assert!(runtime_status::is_read_only_runtime_method("runtime/status"));
}
```

`is_read_only_runtime_method(&str) -> bool` 在本任务实现；另用真实 dispatcher 断言该破坏性 method 返回 MethodNotFound，不能只测字符串函数。

- [x] **Step 2:** 运行 `cargo test -p yi-agent-app-server runtime_status`，确认旧 dispatcher／类型无新能力。
- [x] **Step 3:** app-server 只传项目接线与状态，不持 App 续租凭据。桌面控制命令只接受本机已解析 scope，target 固定当前 sidecar；本地 legacy preview 不绕过迁移阻断，不把 LLM 未注册当作同 UID OS 沙箱。
- [x] **Step 4:** DTO 沿用 ts-rs 生成约定；在 app-server protocol 定义轻量 view 转换，`RuntimeRestartPreviewView`、`RuntimeOperationStatusView` 以 ts rename 分别导出 `RestartPreview`、`OperationStatus`，状态 DTO 导出 `RuntimeStatusView`。字段逐项对应 lifecycle DTO（包含 revision 双分量、完整 counts、分页 cursor 和 active operation），桌面命令返回同形序列化 view，不向 app-server dispatcher 新增破坏性 RPC。运行 `cargo test -p yi-agent-app-server export_bindings` 后只提交生成物，不手写另一套 TS 类型。lifecycle 不引入 ts-rs／app-server 依赖；新增 roundtrip 测试防止 view 丢字段。
- [x] **Step 5:** 测试通知去凭据、API key、消息正文；状态请求没有隐式 register／heartbeat；运行 `just bindings-check`、桌面 Rust 测试及 §0.4，提交 `feat: expose runtime compatibility status and local restart controls`。

## Task 17: App 持续版本提示、二次确认与操作状态 UI

**Depends on:** 13、14、15、16。

**Files:**
- Create: `desktop/src/lib/runtimeLifecycle.ts`、`runtimeLifecycle.test.ts`、`desktop/src/hooks/useRuntimeLifecycle.ts`。
- Create: `desktop/src/components/RuntimeVersionBanner.tsx`、`RuntimeVersionBanner.test.tsx`。
- Modify: `desktop/src/{App.tsx,App.test.tsx,tauriTransport.ts,transportFactory.ts}`。
- Modify: `docs/project-management/{desktop,README}.md`。

**Interfaces:**

```typescript
// DTO 从已生成类型导入；以下接口不扩充通用 RPC 的自动重试路径。
export interface RuntimeControl {
  status(scopeKey: string): Promise<RuntimeStatusView>;
  preview(scopeKey: string): Promise<RestartPreview>;
  confirm(scopeKey: string, token: string, operationId: string): Promise<OperationStatus>;
  operation(scopeKey: string, operationId: string): Promise<OperationStatus>;
}
export function localRuntimeControl(): RuntimeControl;
// 远端使用只读 status，preview/confirm 返回明确 UnsupportedLocalControl。
export function useRuntimeLifecycle(scopeKey: string | null, control: RuntimeControl): {
  status: RuntimeStatusView | null;
  preview: RestartPreview | null;
  operation: OperationStatus | null;
  error: string | null;
  prepareRestart(): Promise<void>;
  confirmRestart(): Promise<void>;
  cancelPreview(): void;
};
```

`RuntimeVersionBanner` props 为 `{ scopeKey: string; control: RuntimeControl }`。组件使用 hook，二次确认时保留 snapshot／token 与 scope。首个不同版本提示不是阻塞弹窗。

- [x] **Step 1:** 用 fake RuntimeControl 添加 Vitest 测试，测试文件声明 jsdom 环境；关键旧请求防重复：

```tsx
it("does not confirm when the user cancels the impact dialog", async () => {
  const control = makeControl({ otherClients: 2, running: 3 });
  render(<RuntimeVersionBanner scopeKey="project-a" control={control} />);
  fireEvent.click(await screen.findByRole("button", { name: "重启为当前版本" }));
  fireEvent.click(await screen.findByRole("button", { name: "取消" }));
  expect(control.confirm).not.toHaveBeenCalled();
});
```

`makeControl({otherClients, running})` 在同测试文件内返回完整 status／preview DTO 和 `vi.fn` 的 RuntimeControl；ready status 和 error 不允许返回空对象欺骗类型检查。

- [x] **Step 2:** 运行 `cd desktop && npx vitest run src/components/RuntimeVersionBanner.test.tsx src/lib/runtimeLifecycle.test.ts`，确认先红。
- [x] **Step 3:** 提示显示实际／期望版本、客户端和任务／插件数；不同构建不随意称旧版。暂不重启只折叠／保持可发现状态，不吞掉未来代际提示。legacy 明示“后台退出管理尚未生效”，unknown 不假装兼容。存在未知或其他影响时展示完整二次确认；无能力时禁按钮并给手动指引。
- [x] **Step 4:** 点击期间禁重复操作；token stale 重新预览并重新确认，不能自动同意。点击时先生成 operation ID 并交给壳保存，再发送一次 Confirm；网络断开只按该 ID 查询，不自动重发，不重新预览后自动授权第二次。NotFound 不代表一定未执行：查询 status／等待原请求的 2 秒边界，仍不能确认则显示结果未知、要求用户手动刷新状态，禁止自动生成新 token／新 operation。取消不调用 Stop／kill。
- [x] **Step 5:** 状态轮询按 scope + generation 隔离，项目切换／卸载取消异步结果与计时器，不让旧项目 token 在新项目确认；独立控制路径不走 RpcClient 自动恢复重放。按 Task 1 的 5 秒／操作期 1 秒只读轮询，心跳使用独立连接／队列，不能以轮询替代心跳。测试切项目、超时、迁移失败、StoppedNoClients、远端不可重启、状态刷新。
- [x] **Step 6:** 运行 `cd desktop && npx tsc --noEmit && npx vitest run`、§0.4，提交 `feat: show runtime version mismatch and confirmed restart progress`。

## Task 18: 真进程验收、默认接入与文档收口

**Depends on:** 1–17；M1/M2 审查无未处理阻断问题。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/managed_runtime_lifecycle.rs`、`tests/common/runtime_fixture.rs`。
- Create: `yi-agent-rs/crates/yi-agent/tests/common/runtime_process_helper.rs`（由 `managed_runtime_lifecycle.rs` 声明固定 `#[ignore]` helper test，使用**当前同一** integration test 可执行文件启动，不跨 test target 猜路径）。
- Modify: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`、`yi-agent-rs/crates/yi-agent/src/{main,managed_runtime}.rs`、`yi-agent-rs/crates/yi-agent-app-server/src/server.rs`。
- Modify: `yi-agent-rs/justfile`、`desktop/scripts/test_runtime_build_identity.sh`、`.github/workflows/ci.yml`、`.gitlab-ci.yml`、`yi-agent-rs/.config/nextest.toml`。
- Modify: `desktop/src-tauri/Cargo.toml`（同模块异步 fake-clock 测试所需的 test-util 仅作 dev feature，若 Task 13 尚未添加则此处补核）。
- Modify: `docs/project-management/{README,yi-agent-lifecycle,yi-agent-store,yi-agent-subagent,yi-agent-core,yi-agent-tools,yi-agent-runtime,yi-agent-cli,yi-agent-run,yi-agent-tui,yi-agent-app-server,desktop,subagent-runtime,ci-cd}.md`，只更新确实涉及的完成项。

**Interfaces:** 仅测试 harness，不进入产品 API。

```rust
struct RuntimeHarness; // 持有 TempDir、所有 Child、IPC连接、测试日志及 cleanup guard。
impl RuntimeHarness {
    fn start() -> anyhow::Result<Self>;
    fn client(&mut self, kind: ClientKind) -> anyhow::Result<String>;
    fn start_marker_worker(&mut self, client: &str, ignore_cancel: bool) -> anyhow::Result<String>;
    fn release(&mut self, client: &str) -> anyhow::Result<()>;
    fn worker_is_stopped(&mut self, worker: &str) -> anyhow::Result<bool>;
    fn await_worker_stopped(&mut self, worker: &str, deadline: Deadline) -> anyhow::Result<()>;
    fn assert_worker_progresses(&mut self, worker: &str) -> anyhow::Result<()>;
    fn quit_client_process(&mut self, client: &str) -> anyhow::Result<()>;
    fn assert_runtime_exits(&mut self, deadline: Deadline) -> anyhow::Result<()>;
    fn database_exists(&self) -> bool;
}
```

真实 guardian／daemon／executor 一律使用 `env!("CARGO_BIN_EXE_yi-agent")`，它是普通非 cfg(test) CLI：不向产品加入 fake worker 注入 FD、测试角色或 fake-worker 参数。使用 loopback wiremock HTTP provider 和临时配置（假 key）驱动真实 Agent／工具；marker／忽略 signal／fork group 的 OS fixture 来自本 integration test 可执行文件，通过 `current_exe()` + `--exact runtime_process_helper --ignored --nocapture` 固定角色启动，由服务端返回的工具调用运行它。直接启动的假前端 helper 验证父测试继承的私有 ready FD；经生产工具链启动的 OS helper 不要求继承该测试 FD（生产 guardian 必须关闭非白名单 FD），改为连接 0700 临时目录内的测试 Unix socket、校验一次性 fixture nonce、握手后才执行。两者没有有效临时 scope／控制握手时都不工作，普通 `--ignored`／nextest 单独运行必须安全返回；不可拿生产 bootstrap FD 当测试授权。多个假前端以这种独立宿主进程持有各自 lease；它们模拟生命周期客户端，不冒充已测试真实 Tauri Cmd+Q。真实桌面退出另外由 Task 13 mock runtime＋macOS 受控冒烟覆盖。使用 ready barrier、文件变化通知／有界条件轮询与可控时钟；Drop 必须回收全部测试子进程，即使 panic。卡死 executor 用 supervisor 层真实 helper／SIGSTOP 独立进程故障注入覆盖，CLI 端用阻塞工具＋真实回收链覆盖；不能仅靠 mock provider 挂起就声称测到不可取消的 Rust 线程。

- [ ] **Step 1:** 实现 harness 并写关键验收用例，先在未接通默认 managed 路径的状态下确认失败：

```rust
#[test]
fn quitting_app_reclaims_stuck_app_worker_but_preserves_two_tuis() -> anyhow::Result<()> {
    let mut h = RuntimeHarness::start()?;
    let app = h.client(ClientKind::App)?;
    let t1 = h.client(ClientKind::Tui)?;
    let t2 = h.client(ClientKind::Tui)?;
    let a = h.start_marker_worker(&app, true)?;
    let b = h.start_marker_worker(&t1, false)?;
    let c = h.start_marker_worker(&t2, false)?;
    h.release(&app)?;
    h.await_worker_stopped(&a, Deadline::after(Duration::from_secs(15)))?;
    h.assert_worker_progresses(&b)?;
    h.assert_worker_progresses(&c)?;
    h.release(&t1)?;
    h.assert_worker_progresses(&c)?;
    h.release(&t2)?;
    h.assert_runtime_exits(Deadline::after(Duration::from_secs(15)))?;
    assert!(h.database_exists());
    Ok(())
}
```

`assert_worker_progresses` 必须观察释放后的新 marker 序号，不能仅检查 PID 存在。`await_worker_stopped` 必须核实执行器与工具组，不只检查任务终态。

- [ ] **Step 2:** 扩展 spec §10 每行的测试，按下方覆盖表核对；必须包括真 SIGKILL daemon／guardian、前台宿主死亡、App-server respawn、exec 交接和异常残留阻断。数据库复用用已保存报告＋交付＋中断任务，断言无外部副作用重播；trace 仅在保留期内断言。
- [ ] **Step 3:** 接通 TUI／App／headless／插件实际默认自动入口到 ensure_managed，移除本期临时内部测试路径的启用限制；保留手工 unmanaged CLI 分支及旧版明确暂用。逐一查找自动启动／ensure_running／embedded 路径，确认没有绕过迁移和选择策略的默认入口。
  **2026-11-04 状态：TUI 与 headless 已接（工具 spawn 走受管通道），App/sidecar 与默认开关未接。**
  TUI（`main.rs` 的 attach 分支）与 headless（`build_headless_root_tools`）现在通过
  `FrontendLease::managed_owner()` 拿到 owner，bash／`process_start`／MCP 的 spawn 都经 guardian 签发的私有通道；
  判据是 `a_headless_runs_tool_processes_are_owned_by_that_run`（去掉 guardian 后失败）。
  **App/sidecar 仍未接**：`yi-agent-app-server` 里没有 `FrontendLease`（`ClientKind::App` 只出现在
  `runtime_status.rs` 的 DTO 映射里），而 `build_runtime_tooling` 仍调 `build_tool_setup_with_controller`（内部传 `None`）。
  sidecar 不能借用桌面壳的 `ClientGrant`：租约按**进程**计，把壳的凭据交给子进程会造出「一条租约两个进程」，
  让「最后一个前端离开」既算不准也回收不了。它需要自己的客户端身份，而 sidecar 刻意不持可再生凭据
  （见 `shell_authorization` 设计），所以这一步要么新增一条壳→sidecar 的授权路径，要么让 sidecar 自己登记租约——
  两者都改变会话模型，属 Task 13 的设计决策，本轮不擅自落地。
  **默认开关仍为 `Embedded`**（`yi-agent-subagent/src/attach.rs::default_for_build`）：daemon 侧
  `DaemonAgentWorkerFactory` 仍是线程实现、`ExecutorConnector` 仍只有测试实现，此时把默认切到 managed
  等于给每个用户一个 worker 隔离尚未接线的运行时。
- [ ] **Step 4:** 添加 `just runtime-lifecycle-check` recipe，运行 CLI 新集成目标与 lifecycle／supervisors／subagent 的 lease／process-group／switch／handover／executor 集成测试；将适合当前 CI OS 的必需测试纳入 `just ci` 的既有 test 入口。更新 `.github/workflows/ci.yml` 的既有 macos job（当前只跑 power／tools／app-server），加入 lifecycle／store／supervisors／subagent／CLI 的指定集成目标，并补 Node／just 与隔离 sidecar 测试资源，运行桌面 Rust 生命周期测试；`.gitlab-ci.yml` 的 macOS push job 同步执行相同 macOS recipe，不把云端 Linux MR 当 macOS 验证。更新 nextest filter 确保 helper 不被默认单独执行、故障注入目标有界超时且 panic 清理有效。平台缺能力不能静默显示通过；真实 wake／Tauri run-event 在 CI 无 GUI 能力时只验证适配／mock，并明确另有本机隔离 App 的手动冒烟门禁，不能声称无头测试已覆盖系统 Cmd+Q。
- [ ] **Step 5:** 在 worktree 跑完整验证，命令如下；所有失败先诊断修复，不能依赖“纯文档计划已通过”代替实现门禁。

```bash
(cd yi-agent-rs && cargo fmt --all && just runtime-lifecycle-check && just ci)
(cd yi-agent-rs && just desktop-check)
cargo test --manifest-path desktop/src-tauri/Cargo.toml
bash desktop/scripts/test_runtime_build_identity.sh
bash desktop/scripts/test_scripts_target_dir.sh
git diff --check
```

- [ ] **Step 6:** 同 commit 更新各模块进度与索引计数；新增生命周期模块用命令作完成判据。spec/plan 状态按实际实现更新，记录已验证平台和剩余限制，不将计划复选框当成功证据。执行 §0.4，提交 `test: verify shared runtime lifecycle and enable managed defaults`。
- [ ] **Step 7:** 使用 requesting-code-review skill 审查，修复阻断问题并重跑相关测试／总门禁；再用 finishing-a-development-branch skill 收尾。仅在获准合并后回 main 做 `git merge --no-ff <implementation-branch>`，在合并结果上再次 `just ci` 和 desktop-check；不包含主工作区及其他 worktree 的原有修改。确认完成后移除已合并 worktree／分支，按项目磁盘规范先检查在用进程和 dry-run，再清不需增量的构建产物。

## 19. Spec → Task → 测试覆盖表

下列测试名称是要求新增的精确用例名；文件／crate 归属见对应 Task。执行者不得把“有一行矩阵”视作已测试。

| Spec 场景／约束 | Tasks | 必须落下的测试名 |
|---|---|---|
| 单 App Cmd+Q、关闭窗口只隐藏 | 9,13,18 | `last_app_quit_stops_runtime_preserving_database`, `closing_window_does_not_release_app` |
| App + 两 TUI、逐个退出 | 5–9,12,18 | `quitting_app_reclaims_stuck_app_worker_but_preserves_two_tuis` |
| 宿主强杀、EOF、迟到心跳／EOF | 4,9,12,13,18 | `frontend_death_expires_without_sidecar_renewal`, `late_eof_does_not_remove_resumed_client` |
| App-server 重启保持 App 身份 | 13,18 | `sidecar_respawn_does_not_release_app_lease` |
| 休眠恢复与真实失活 | 4,13,18 | `system_resume_grants_renewal_window_without_reviving_released_client` |
| 多项目退出总预算 | 9,13,18 | `multi_project_quit_releases_in_parallel_under_one_deadline` |
| 空闲项目释放、headless／内部调用 | 11–13,18 | `history_and_inspection_do_not_acquire_leases`, `headless_failure_releases_frontend` |
| 冷启动无人、并发 attach | 3,9,11,18 | `first_client_timeout_leaves_no_runtime`, `concurrent_attach_starts_one_coordinator` |
| 路径别名、覆盖目录、实例冲突 | 3,11 | `runtime_aliases_share_lock_inode`, `override_rejects_other_project_or_instance` |
| 数据库复用、历史、交付、副作用 | 3,5,15,18 | `reopen_preserves_delivery_without_replaying_interrupted_attempt` |
| 构建兼容矩阵、wrapper 不等于 sidecar | 1,2,11,16,17 | `changed_dirty_source_changes_identity`, `runtime_compatibility_matrix` |
| 业务不兼容但控制兼容 | 1,4,14,16,18 | `lease_only_can_preview_but_cannot_spawn` |
| 暂不重启、取消、token 过期 | 14,17,18 | `cancel_preview_has_no_runtime_side_effects`, `expired_confirmation_cannot_drain` |
| revision／代际变化、分页 | 14,17 | `changed_impact_rejects_confirmation`, `preview_pages_preserve_complete_snapshot` |
| 心跳／日志不改 revision | 4,14 | `heartbeat_does_not_change_impact_revision`, `trace_output_does_not_invalidate_preview` |
| 同执行器内单任务停止 | 5,7,8,18 | `one_stopped_task_does_not_exit_client_executor`, `task_join_without_tool_group_exit_retains_execution_lease` |
| 跨 executor provider-turn 限流／取消 | 7,18 | `executor_turn_gate_preserves_global_limit`, `executor_death_releases_provider_permit` |
| shell／process／MCP 进程语义 | 6,8,18 | `managed_child_preserves_exit_signal_and_streams`, `mcp_probe_and_reconnect_leave_no_unregistered_child` |
| 确认响应丢失／状态查询不续租 | 1,14,16,17,18 | `lost_confirm_response_queries_preallocated_operation`, `status_polling_does_not_extend_frontend_lease` |
| 最终签名产物／可搬移清单 | 2,11,18 | `final_bundle_manifest_matches_signed_sidecar`, `relocated_bundle_resolves_relative_sidecar` |
| 重复点击、竞争升级、维护期新连接 | 14,15,17,18 | `concurrent_confirm_starts_one_switch`, `maintenance_registration_waits_for_next_generation` |
| 有 TUI 确认重启、不能抢回旧构建 | 11,12,14,15,18 | `confirmed_switch_reconnects_compatible_tuis_without_downgrade` |
| embedded、无新契约旧 TUI、手工 daemon | 10,11,15,18 | `legacy_embedded_runtime_requires_host_cooperation`, `manual_daemon_requires_explicit_temporary_use` |
| daemon 无响应、卡死 worker、假终态、keep | 5–9,14,15,18 | `completed_event_does_not_imply_stopped_event`, `managed_keep_is_rejected_without_silent_conversion`, `unresponsive_managed_daemon_can_be_confirmed_with_explicit_unknown_impact`, `unknown_impact_without_independent_fence_requires_manual_action` |
| SIGKILL daemon、guardian 失联、插件孙进程 | 6–9,15,18 | `daemon_death_reclaims_all_registered_groups`, `guardian_loss_fails_closed`, `owner_reclamation_waits_for_the_entire_group` |
| schema 超前、迁移／启动失败 | 3,11,15,18 | `future_schema_is_rejected_without_writing`, `failed_start_does_not_fall_back_to_old_binary` |
| 目标被删、bundle 中途替换 | 2,11,15,18 | `changed_artifact_is_rejected_before_spawn`, `missing_selection_target_does_not_use_path` |
| 控制者崩溃、guardian 自更新 | 6,14,15,18 | `handover_preserves_remaining_lease_and_consumed_tokens`, `controller_crash_does_not_transfer_kill_authorization` |
| 切换中全退出 | 9,15,18 | `no_clients_after_stop_is_not_ready` |
| 旧 launchd 与插件安装 | 10,18 | `migration_checks_loaded_job_and_plist_owner`, `plugin_install_does_not_reinstall_watchman` |
| 其他项目／用户／实例／远端 | 3,6,10,16,17,18 | `replacement_is_scoped_to_one_local_runtime`, `remote_restart_is_not_exposed` |
| 插件需求重建、离线调度 | 8–10,18 | `plugin_demand_reacquires_after_client_return`, `offline_schedule_preserves_skip_and_catch_up_once` |

## 20. 计划自检与执行交接

### 自检清单

- [ ] 实施前再核对 spec 未发生变化；用户如收缩强制回收保证，应先同步 spec／本计划，不静默省略 Tasks 6–9。
- [ ] 新 crate 注册、依赖无环、桌面不链接 SQLite 均有明确任务及验证命令。
- [ ] 客户端租约与业务授权分离，guardian 接管不复活过期身份，非幂等请求不自动重放。
- [ ] 工具包括前台 root，不把只清 daemon worker 当整棵工作回收；失败关闭有可观察结果。
- [ ] 构建清单覆盖真实 sidecar／签名后产物，不将 git dirty 或 wrapper version 当唯一身份。
- [ ] 用户确认在本地桌面壳，远端只有状态，不新增 LLM kill 工具。
- [ ] 验收覆盖表每行都有真实测试，所有成功声明有刚运行的输出证据。

本文编写阶段只检查文件路径、任务依赖、接口命名与 spec 覆盖；以上复选框为实施跟踪，不表示实现已完成。本计划与 spec 都留在独立 worktree，未授权改实现、打包替换正在使用的 App、停止真实 daemon 或操作用户 LaunchAgent。

macOS 本机受控冒烟（实现获批后执行，不属于本轮文档审查）：按项目独立 App 规范构建不同 bundle ID，使用绝对路径 `YI_AGENT_HOME` 和临时项目；直接从命令行启动该测试 App，连接同一测试 scope 的两个新版 TUI。先关闭窗口验证任务继续，再 Cmd+Q 验证仅 App marker 停止、两 TUI marker 递增；逐个退出 TUI 验证最后退出停止 guardian／daemon／工具组；重开读到原报告且不自动重播任务。再验证当前／不同构建提示、取消无副作用、显式切换和失败提示。不得对正式 App 或真实用户 scope 做这些操作；保存命令、产物 build ID、测试 PID／组证据和结果。

### 2026-11-02 计划复核记录

本轮按以下顺序反复审阅并修订，完成后再跑文档静态校验：

1. spec 逐项覆盖：真正退出、多 TUI、单库复用、未知／旧 runtime、迁移与确认边界。
2. 接口／任务顺序：移除前向未定义 DTO、对齐协议宽度与授权、分离业务配置、补齐 provider-turn gate 和 task 级停止证据。
3. 源码可落地性：核对实际 factory、bash／process／MCP spawn、构建／签名／归档入口及 macOS CI。
4. 失败／竞态与验证质量：双分量 revision、确认 operation ID、锁移交、Release 不被冻结、exec 状态、真实 helper／mock provider 与测试隔离。

静态校验只证明计划结构、路径和已列约束一致，不证明实现正确。本轮没有修改实现、执行功能测试、停止真实进程、操作 LaunchAgent、提交或合并。后续实施仍需用户授权；如果源码／spec 在此后变化，执行前必须重做基线检查。

执行可选：使用 subagent-driven-development 每任务实现并逐项审查；或使用 executing-plans 在同会话按里程碑分批执行。无论哪种，开始实现前先获得授权，不能以“继续写 plan”作为默认开工依据。
