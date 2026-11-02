# yi-agent-cli（CLI 二进制）

## 模块说明

`yi-agent-cli` 模块对应 `yi-agent-rs/crates/yi-agent/` 这个**二进制 crate**（产物名
`yi-agent`）。本文件只记录「CLI 自身」的跨领域关注点：顶层子命令语法、共享参数、
以及 shell 补全等面向终端用户的命令行体验。各子命令的**业务语义**仍归各自的领域
文档负责（如 `yi-agent run` → [yi-agent-run](./yi-agent-run.md)，app-server 子命令 →
[yi-agent-app-server](./yi-agent-app-server.md)，`agents`/`agent`/`daemon`/`schedule` →
[subagent-runtime](./subagent-runtime.md)）。

## 范围边界

**做什么：**
- clap 参数语法定义（`crates/yi-agent/src/config.rs` 的 `Cli` / `Command`）
- 顶层 dispatch（`crates/yi-agent/src/main.rs::main`）
- 面向用户命令行的辅助体验：shell 补全脚本生成

**不做什么：**
- 不做配置加载语义（由 `yi-agent-runtime` 拥有，见 [yi-agent-runtime](./yi-agent-runtime.md)）
- 不做各子命令的业务实现（归各自领域文档）
- 不做动态补全（补全运行时的模型名 / 会话 id / task id —— 当前未实现）

## Features

- [x] Shell 补全脚本生成 — `yi-agent completions <shell>` 子命令（`crates/yi-agent/src/config.rs::Command::Completions`，dispatch 在 `main.rs::print_completion`），基于 `clap_complete` 从 CLI 语法派生静态补全脚本，支持 `bash` / `elvish` / `fish` / `powershell` / `zsh`（由 `clap_complete::Shell` 提供）。补全内容覆盖顶层 flag（`--provider` / `--workdir` / `--sandbox` 等）、子命令及 `value_enum` 候选值（如 `--sandbox` 的 `read-only` / `workspace-write` / `danger-full-access`）。为保持脚本纯净,该子命令**跳过 tracing 初始化**（`main.rs::main` 中 `matches!(cli.command, Some(Command::Completions { .. }))` 时 `_trace_guard` 为 `None`），确保 stderr 无任何日志行。安装方式(以 zsh 为例)：`yi-agent completions zsh > "${fpath[1]}/_yi-agent"`；bash：`yi-agent completions bash > /usr/local/etc/bash_completion.d/yi-agent`。验证：`cargo test -p yi-agent --test cli_completions`

- [x] TUI 首启问答 + `yi-agent run` 未配置指引 — 新增 `crates/yi-agent/src/onboarding_prompt.rs`（`should_offer`（`yi-agent/src/onboarding_prompt.rs:10`，改看**清单**是否就绪） / `provider_defaults` / `run_onboarding_prompt`（`:49`，交互式收集 provider / api_key / model / api_url、可选连接测试、经共享导入器把条目落进 `~/.yi-agent/models.json`）），`run_agent` 首启自动弹问答；`yi-agent web` 与 `run_agent` 在启动时各跑一次存量 `.env` → 清单迁移；`yi-agent run`（headless）未配置时经 `unconfigured_guidance` 打印中文指引并以非零码退出（判定基于 `run` 实际加载出的配置，含全局 `.env` / 进程环境变量 / CLI 覆盖，不会误拦已配置机器）。验证 `cargo test -p yi-agent --bin yi-agent onboarding_prompt` 与 `cargo test -p yi-agent --bin yi-agent guidance` — [设计](../superpowers/specs/2026-10-09-first-run-onboarding-design.md)

**验证命令：** `cargo test -p yi-agent --test cli_completions`（4 个测试）

- [x] 启动期错误在类型上不可伪装成回合失败 + 提示携带版本号 — `attach_tui_runtime`（`main.rs`）签名由 `Result<Option<TuiRuntimeSession>>` 改为 `TuiRuntimeSession`。原签名的两个失败分支都是死的（5 处 return 全为 `Ok(Some(..))`，从未产生 `Ok(None)`），唯一能把错误送进 `Err` 的是 `socket_path_for(..)?`，它现在就地转成 `TuiRuntimeSession::Unavailable`；driver 调用点的 `Err` 臂（包 `AgentError::ProviderTurnAdmission`，即 `Error: provider turn admission failed: ...` 的唯一来源）随之删除。**这是本次 bug 的根除点**：启动失败没有「回合失败」这个通道可走，因为签名里不存在该通道。`attach_headless_runtime`（`run --subagents`）刻意保留 `Result`——显式要求委派的命令失败就该失败。另：`runtime_restart_notice_text()` 在重启提示后追加 `（当前版本 X）`（`tui/subagents.rs`），`render_runtime_notice_popup` 宽度改为按文本长度自适应（原固定 58 会裁掉版本号，`tui/app.rs`）。动机：2026-10-29 现场是「二进制比修复早 6 天」，旧提示与新提示一字不差，用户无从判断该重启还是该升级；离线实现，不做联网更新检查。`control_schedule` 同步改用 `load_lenient` + `resolve_effective` + 显式 `unconfigured_guidance`（原先走严格加载器，在只有 `models.json` 的机器上报 `API key required`）。验证：`cargo test -p yi-agent --bin yi-agent`（661，含 `attach_tui_runtime_is_infallible`、`only_the_lenient_loader_exists`、`the_notice_names_the_running_version`）；变异：签名改回 `Result<Option<..>>` → 6 处编译错误，`load(cli)` 调回来 → `E0425`，去掉版本号 → `the_notice_names_the_running_version` 红

- [x] 删除 `yi-agent boards` 别名（有意的破坏性变更）— 看板面从宿主删净后，`yi-agent boards watch` / `boards watch --once` 不再存在；`Cli::try_parse_from(["yi-agent", "boards", "watch"])` 必须报错（`main.rs` 的 `the_legacy_boards_alias_is_gone` 钉住，注释说明「别把 board 这个名字加回来」）。替代物是通用的 `yi-agent resident watch [--interval-secs]` / `yi-agent resident list`（`yi-agent-supervisors`），plist 由 `plugin/install` best-effort 安装（`ai.yi-agent.resident-daemon-watchman`）。仍跑旧 plist 的机器会在下次登录时解析失败——这是刻意的，因为旧的看板专用入口已无实现可调。验证：`cargo test -p yi-agent --bin yi-agent the_legacy_boards_alias_is_gone`、`cargo test -p yi-agent --bin yi-agent resident` — [设计](../superpowers/specs/2026-10-17-plugin-lifecycle-decoupling-design.md)

- [x] CLI 级会话 resume / fork / threads — 三个新子命令：`yi-agent resume <id|前缀> [提示词]`、`resume --last [提示词]`、`yi-agent fork <id> [提示词]`、`yi-agent threads`（`crates/yi-agent/src/config.rs` 的 `Command::{Resume,Fork,Threads}`，解析在 `crates/yi-agent/src/resume.rs`，编排在 `main.rs::run_resume` / `run_fork` / `run_threads`）。id 支持完整 uuid 或**唯一前缀**；前缀有歧义时列出候选而不是随便挑一个（挑错会把历史接在另一个话题上，用户直到看到上下文不对才发现）。`resume --last "x"` 的 positional 被 clap 绑到 `thread_id`，语义上却是提示词——clap 表达不了这种条件语义，dispatch 层换位。`run --resume <id>` 是同一链路的 headless 载体（TUI 全屏、测不了）。三个入口都先跑 `prepare_environment`（`.env` → `settings.json` 迁移）：迁移会改变 `workdir` 的读取来源，跳过它 `resume` 会去另一个目录的 threads 里找会话。验证：`cargo test -p yi-agent --bin yi-agent config::tests::resume`、`cargo test -p yi-agent --bin yi-agent resume::`、`cargo test -p yi-agent --test e2e_resume` — [设计](../superpowers/specs/2026-11-01-cli-session-resume-design.md)

- [x] 会话 fork 的存储语义（`ThreadStore::fork_thread`） — `crates/yi-agent-app-server/src/thread_store.rs`。必须住在这个模块：标题截断要用 `pub(crate)` 的 `title_from`，写在 CLI 侧只能扩大 API 或复制一份规则。复制 `items` / `messages` / 累计用量；**清空** `last_item_id`（继承会让桌面端锚点校验拒绝 fork 会话首轮）、`last_read_at`（新会话必须未读）、`last_completed_at`（从未完成过）、`role`（否则一个项目两个组长）、`content_epoch`；只设 `forked_from`。`usage` 与 `usage_total` 都必须给——`rpc/thread.rs` 的 `if let Some(u)` 会挡掉 `None` 时的整个用量通知，只给 `usage_total` 客户端仍显示 0。**只读源会话**（不调 `promote_partial`：`load` 已在内存里合流 partial，写源会让「fork 一个进行中的会话」意外改变源会话状态）。验证：`cargo test -p yi-agent-app-server --lib fork_`（5 条，含源日志 mtime/长度不变）

- [x] 跨进程会话写锁 — `thread_store.rs::acquire_thread_lock`（`flock(LOCK_EX|LOCK_NB)` + 进程级 `Weak` 去重表）。表项存 `Weak` 而非 `Arc`：存 `Arc` 会让锁与表互相持有成自引用循环，`Drop` 永不触发，`flock` 永不释放且 fd 泄漏。`ThreadLockError` 区分 `Busy`（另一个 writer，调用方提示去 `fork`）与 `Io`（建目录/打开失败，退化为无锁并记 warn）——把 IO 故障报成「会话被占用」会让用户永远查不出原因。app-server 只在启动 driver 时取锁（`thread/start` 与 `thread/resume` 两处生产构造点都要），覆盖多客户端订阅场景。验证：`cargo test -p yi-agent-app-server --lib writer_lock`、`cargo test -p yi-agent --bin yi-agent session_store::tests::second_open_of_same_thread_is_busy`（用真子进程——同进程内 flock 会先命中去重表，走不到内核）
- [x] 隐藏的 `internal-runtime identity` 子命令（机器可读的 daemon 构建身份） — `crates/yi-agent/src/config.rs` 的 `InternalRuntime`（`#[command(hide = true)]`）+ `main.rs::run_internal_identity`。输出 `build_id`（32 位十六进制，来自 build script 对源码/编译配置/依赖闭包算出的摘要）、display version、可执行文件路径、schema 范围（取自 `schema_compat` 的活契约，不硬编码）、handover version、business 协议范围与能力列表。**不连接 runtime、不读写数据库**：打包脚本与桌面壳的 `build.rs` 都需要在无 daemon 的情况下问「这个二进制是谁」。`--help` 里看不到它（打包链路专用，不是用户命令）。判据：`./target/debug/yi-agent internal-runtime identity --json` 输出 `"build_id"` 为 32 位十六进制；`./target/debug/yi-agent --help | grep -c internal-runtime` 为 0；改一个源文件后 id 变化、还原后逐字符复原（实测 `1123c23c…` → `6f36f0bb…` → `1123c23c…`）
