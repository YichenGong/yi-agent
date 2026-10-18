# yi-agent-subagent（共享子 Agent 运行时装配）

## 模块说明

`yi-agent-subagent` 是「应用侧」子 Agent runtime 的共享 crate：它把项目 daemon 的
worker 工厂、daemon 客户端（attach / activate / detach）与六个委派工具
（`spawn_agent` / `send_message` / `wait_agent` / `inspect_agent` / `cancel_agent` /
`review_agent`）实现一次，供 CLI（`yi-agent`）与桌面 app-server
（`yi-agent-app-server`）共用。

它从 `crates/yi-agent/src/subagent_runtime.rs` 整体平移而来（该文件已删除）。抽取的
动机是**停止漂移**：此前 app-server 完全不知道 daemon 的存在，桌面端因此根本没有
`spawn_agent`，而 TUI 有——同一份能力在两个前端里存在与否，取决于各自的装配代码。

## 范围边界

**做什么：**
- daemon 侧 worker 工厂 `DaemonAgentWorkerFactory`（provider、skills-only registry、
  sandbox、项目 workspace、per-child model）
- daemon 客户端：`attach_project_runtime` / `activate_root` / `detach_root`
  （`AttachFailure` 带失败阶段，供降级路径记录）
- 运行时的探活与归属决策：`probe_runtime`（只读 `Status` 判 `Healthy` / `Wedged` /
  `Dead` / `Unknown`）、`ensure_owned_runtime`（策略 B：健康复用、死或卡死才接管）、
  `retire_if_wedged`（只退休回 `internal` 的 daemon）
- 活绑定 `RuntimeBinding`：工具不再把 socket 与 root 三元组冻结进自身，而是每次调用
  解析当届句柄；连接失效（`is_liveness_error`）时单飞 `repair` 并重试一次
- 六个委派工具的注册：`register_attached_root_tools`（application root 版）与
  `register_application_subagent_tools`
- `AttachedRoot`（session id / root task id / capability / workspace）
- 项目 runtime 目录解析 `project_runtime_directory`（读 `YI_AGENT_RUNTIME_DIR`，
  否则 `<workdir>/.yi-agent/runtime`）与 `ignore_project_local_runtime_state`

**不做什么：**
- 不做 daemon 本身（`yi-agent-store` 负责持久化、IPC、调度）
- 不做「当前 root」的单例：TUI 用进程级单例（一进程一 root），app-server 用 per-cwd
  缓存（一进程多项目），二者刻意不同，故由各应用自己持有
- 不做 attach 的决策（是否启用、何时询问用户）：TUI 由
  `tui/runtime_prefs.rs` 的三态偏好决定，app-server 直接尝试并降级

## Features

- [x] crate 骨架 + workspace 注册 — `yi-agent-rs/crates/yi-agent-subagent/`；
  `yi-agent-rs/Cargo.toml` 的 members 与 `workspace.dependencies` 均含本 crate
- [x] daemon 工厂与六个委派工具整体平移 — `src/lib.rs`（`DaemonAgentWorkerFactory`
  `src/lib.rs:42`、`DaemonWorkspaceService` `src/lib.rs:217`、`register_attached_root_tools`
  `src/lib.rs:1062`）；平移前该文件在 `crates/yi-agent/src/subagent_runtime.rs`，无
  `crate::` 内部引用，故可整块搬移；验证：`cargo test -p yi-agent-subagent`
- [x] 共享 attach 客户端 — `src/attach.rs`：`attach_project_runtime(cfg, runtime_dir)`
  （收 **runtime 目录**而非 socket 路径，测试因此能隔离单个项目的 runtime 而不动进程级
  环境变量）、`activate_root`（幂等）、`detach_root`（best-effort，失败只记 trace）；
  `AttachFailure` 的 `stage`（`runtime directory` / `worker factory` / `daemon start`
  / `attach`）让降级路径能说清是哪一步断了；`project_runtime_directory` 的默认位置由本
  crate 的单测钉住（daemon、TUI、app-server 必须落在同一处）；验证：
  `cargo test -p yi-agent-subagent --test attach_delegation`
- [x] 运行时归属与自愈原语 — `src/attach.rs` 的 `probe_runtime` / `ensure_owned_runtime` /
  `retire_if_wedged`，与 `src/binding.rs` 的 `RuntimeBinding`（`managed` / `fixed`）：
  委派工具持 `Arc<RuntimeBinding>`，每次调用经 `current_or_repair` 解析当届 socket 与
  root，`send` 遇 liveness 失败时先 `repair`（单飞，避免并发起两个 daemon）再重试一次；
  `activate` 走同一条通道。判据：启动该 runtime 的进程退出后，下一次委派调用仍能自愈。
  `fixed` 绑定（TUI 整个会话自己持有 daemon）行为与过去完全一致。验证：
  `cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent-app-server --lib a_dead_runtime_is_repaired`
- [x] 端到端装配契约 — `tests/attach_delegation.rs`：干净 git 项目 attach → 激活两次
  （证明幂等）→ 注册的委派工具存在 → `SpawnApplicationChild` 被 daemon 接纳并返回
  child task id → detach；这是 TUI 与 app-server 共用的那条链路的可执行说明
- [x] `spawn_agent` 的 `fork` 参数与调用者上下文 fork — 两个 spawn 工具新增 `fork`
  （`boolean`，`default:false`，`required` 仍只有 `task`，显式非布尔值报错而非忽略）；
  `CallerContext`（`src/lib.rs:1296`）持调用者会话的**活 `Arc<Mutex<Session>>`**，
  每次调用经 `snapshot()` 读取「此刻」的 `forkable_messages`（协议有效性裁剪，末位悬空
  `tool_use` 丢弃），故 Agent 重建/compact 不会让工具读到陈旧转录；`fork:true` 时
  `upload_forked_context`（`src/lib.rs:1564`）把 JSON 历史按 256 KiB 原始切片（`FORK_CHUNK_BYTES`）
  base64 后逐块 `BeginForkUpload`/`AppendForkChunk`，返回的 `fork_token` 填进
  `SpawnApplicationChild`；`fork:false`（或省略）逐字节走原路径、不建上传。任何
  begin 之后的失败都 `abort_fork_upload` 回收 token（避免 daemon 进程内泄漏载荷）；
  无绑定 caller 却要 fork 时显式报错 `fork requested but no caller context available`，
  绝不静默回退。worker 侧 `start_with_provider_turn_gate`（`src/lib.rs:658`）在
  `with_session(Session::from_messages(fork))` **之后** `worker_caller.bind(session_handle())`，
  把子 worker 自己的委派工具绑到其实时会话（顺带支持子 agent fork 自己）。验证：
  `cargo test -p yi-agent-subagent --lib`、`cargo test -p yi-agent --test subagent_fork_e2e`
  （后者经**真实注册工具 + 真实 daemon + 真实 worker 工厂**，断言子 worker 首个 provider
  请求含父历史且 objective 居后，省略 `fork` 时只含 objective）— [设计](../superpowers/specs/2026-10-02-subagent-context-fork-design.md)
- [x] worker 工厂按 `subagent_model` 解析 provider/model — `worker_factory`（`src/attach.rs:88`）从**调用方注入的**机器级清单路径（新增 `models_path: &Path` 形参，经 `load_catalog_from` 读取；不再自取 `$HOME`）取 `subagent_entry`（缺省回退全局默认、再无则回退进程 cfg），经 `resolve_effective`（`src/attach.rs:113`）得到该条目的 provider/api_url/api_key/model 去 `build_provider`；改完 `subagent_model` 下次项目 daemon 重启生效。该路径经 `attach_project_runtime` / `ensure_owned_runtime` / `RuntimeBinding::managed` 从 app-server 的 `RuntimeAttachments.models_path` 与 CLI 的 `yi_agent_runtime::models::models_path()` 一路传入，使委派链路可被隔离测试（见 `docs/bug-list.md`「委派 worker 工厂重读 $HOME」条）。验证 `cargo test -p yi-agent-subagent --lib the_worker_uses_the_subagent_model_entry`、`cargo test -p yi-agent-subagent --lib the_worker_falls_back_to_the_global_default`、`cargo test -p yi-agent-subagent --lib the_public_worker_factory_reads_the_injected_catalog the_public_worker_factory_never_falls_back_to_home`

**边界（两端口一致，故记在这里）：** attach 与工具注册**不看**项目是不是 git 仓库（非
git 目录原地成 root，`workspace_root == project_root`），真正需要仓库的是后续 `spawn_agent`
——daemon 的 worker 准入要求可恢复的 `worktree:` lease。因此非 git 项目下「工具在、调用被拒」。
app-server 侧的钉法是 `cargo test -p yi-agent-app-server
a_non_git_cwd_attaches_in_place_with_the_delegation_tools`。

- [x] daemon 工厂按指纹跟随模型清单（运行期热重载）— `DaemonAgentWorkerFactory::with_catalog_watcher(cfg, watcher)`（`src/lib.rs:231`）装一个 `WorkerRefresh`（`:121`）；worker start 时先用 `EffectiveKey::of`（`:98`，`resolve_effective` 覆盖的 6 字段）比对缓存，命中则复用，未命中则 `WorkerParts::rebuild`（`:55`）按当前清单重建 provider/tools/config/sandbox（装配口径与 `attach::build_worker_factory` 一致：skills-only 注册表 + 同一组 `bootstrap` 构造函数）；重建失败保留上一次可用派生件。**`new()` 签名逐字节不变**，未装刷新器的 22 处既有构造点行为不变（零回归）；`worker_config_model` 仅在无刷新器时生效，有刷新器时模型名来自当前清单条目。生产装配在 `attach.rs:156`（`build_worker_factory` 新增 `models_path` 形参后 `.with_catalog_watcher(cfg.clone(), CatalogWatcher::new(models_path))`）。验证：`cargo test -p yi-agent-subagent`（65 lib + 5 e2e，含 `a_watcher_backed_factory_follows_a_changed_subagent_model`、`a_factory_without_a_watcher_keeps_its_static_config`、`the_attached_worker_factory_follows_a_changed_catalog`、`the_attached_worker_factory_survives_a_corrupt_catalog`）；真机三步判据见 `yi-agent/tests/model_catalog_live_reload_e2e.rs` — [设计](../superpowers/specs/2026-10-17-model-catalog-live-reload-design.md)

- [x] worker 工具注册的根集参数 + 「FS 根集 ≠ 沙箱可写根」 — `DaemonAgentWorkerFactory::worker_tool_registry`（`src/lib.rs:306`）/ `worker_tool_registry_with_base`（`src/lib.rs:328`）新增第 4 个参数 `inherited_roots: Vec<PathBuf>`（父会话下发的关联根，主根除外；空集即今天的单根 worker），把它并进 **FS 根集** `fs_roots = [worktree] + inherited_roots`（`src/lib.rs:339`）。**两个根集刻意分开**：`fs_roots` 进 FS 工具（`read`/`write`/`edit`/`glob`/`grep` 的多根校验），而 `git_writable_roots_for_worktree`（`src/lib.rs:1239`）产出的 git 内部目录**只**并入 shell 沙箱的额外可写根 `writable_roots`（`src/lib.rs:345`），**绝不**进 FS 根集——否则 `write` 能直写 `.git`（该约束写在 `src/lib.rs:355` 的注释里）。接线：`WorkerStart.extra_roots`（`yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:97`）在 `start_with_provider_turn_gate` 里被取出并下传给注册函数（`src/lib.rs:766`）。ReadOnly 子任务只扩读、不注册写工具。验证：`cargo test -p yi-agent-subagent --lib coding_worker_gets_fs_tools_over_the_inherited_roots`、`--lib read_only_worker_inherits_roots_without_write_tools`、`--lib coding_worker_shell_sandbox_covers_the_inherited_roots`、`--lib fs_roots_stay_clear_of_the_git_roots_the_shell_sandbox_needs`、`--lib worker_start_hands_the_inherited_roots_to_its_fs_tools`（分别钉住 FS 工具覆盖关联根、只读不扩写、沙箱覆盖关联根、两集不混、`WorkerStart` 真到达 FS 工具）— [设计](../superpowers/specs/2026-10-17-linked-workspace-roots-design.md)

**验证命令：** `cargo test -p yi-agent-subagent`（含 `binding::` 单测与
`application_tools_share_one_binding`；依赖真 socket 的 `probe_reports_healthy_on_status`
需在可 bind Unix socket 的环境运行）
