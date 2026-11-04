# yi-agent-tools

## 防休眠加固交叉引用（已实现）

显式后台进程保活意图与读回见 [spec](../superpowers/specs/2026-11-03-idle-sleep-hardening-design.md) §7、[plan](../superpowers/plans/2026-11-03-idle-sleep-hardening-impl.md) Tasks 5–6。需求 P4 统一计入 [yi-agent-power](./yi-agent-power.md)，本模块不重复计数。

完成判据（自动化已通过，详见 [verification](../research/2026-11-03-idle-sleep-hardening-verification.md)）：
`cargo test -p yi-agent-tools --lib sleep_hardening` 6 项（B1–B4），加
`cargo test -p yi-agent-tools --lib process::` 32 项无回归。
实现位置：`crates/yi-agent-tools/src/process/tools.rs`（`ProcessStartArgs` 的
`#[serde(default)] prevent_idle_sleep: bool` + schema `type: boolean` /
`default: false`、不在 `required` 里；`"true"` / `null` / `1` 被 serde 拒绝）与
`crates/yi-agent-tools/src/process/manager.rs`
（`ProcessStartOptions` / `ManagedProcess` / snapshot / 结果 DTO 都携带意图，
只在构造时写入、不按状态改写；`has_sleep_preventing_processes()` 只遍历内存表，
`Starting | Running | Ready` 算活跃，`Ready` 不是终态）。

desktop 侧同一意图由 `ProcessInfo` / `ProcessReadResult` 的可选字段承载，
`desktop/src/lib/processes.ts` 的 `requestsIdleSleepPrevention` 把「缺值按 false」
固化成唯一入口（当前无 UI 消费者）；`npx vitest run` 2051 passed。

变异证据：schema 改 `type: "string"` → `sleep_hardening_schema_exposes_explicit_boolean` 红；
谓词去掉 `Ready` → `..._predicate_covers_every_status` 与
`..._gated_process_reports_the_intent_end_to_end` 红（"status Ready with
prevent_idle_sleep=true must yield true"）。

遗留的既有跳转（`JumpToProcess`）与 old `App` 老流程无关，不受影响。

## 模块说明

yi-agent 的内置工具实现 crate，提供 coding agent 的 FS（文件系统）、Shell、Web、Skill 核心工具能力，通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

## 范围边界

**做什么：**
- 实现 FS 工具（Read/Write/Edit/Glob/Grep）
- 实现图片读取工具（ViewImage：png/jpeg/gif/webp 读取、缩放、base64 编码）
- 实现文档读取工具（ReadDocument：PDF 文本层 / DOCX / HTML / 纯文本，字符修复、按页、预算）
- 实现 Shell 工具（Bash 命令执行，支持流式输出增量推送）
- 实现 Web 工具（WebFetch + WebSearch）
- 实现 Skill 工具（加载并执行 yi-agent-skills 发现的 skill）
- 路径安全（单一 root 限制，canonicalize + starts_with）
- 工具注册 API（`register_builtin_tools`）
- Shell sandbox（Codex-compatible sandbox mode names + platform enforcement）

**不做什么：**
- 不做 MCP 协议工具（由 yi-agent-mcp 负责）
- 不做插件系统（基于 ToolSource::Plugin，后续）
- 不实现 Windows restricted-token backend（不支持的平台会拒绝受限命令，不会静默裸跑）

## Features

- [x] FS 工具：Read/Write/Edit/Glob/Grep — `crates/yi-agent-tools/src/fs/` 五个 tool 文件 + 单一 root 校验；`grep.rs` 对所有输出模式限制为 200 条渲染记录或 32 KiB 并返回截断提示；验证：`cargo test -p yi-agent-tools grep_` — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] 图片读取工具：view_image — `crates/yi-agent-tools/src/fs/view_image.rs` 读取 png/jpeg/gif/webp（`image::guess_format` 识别，20 MiB 上限），`detail: high` 最长边缩到 2048px、`original` 缩到 6000px，超限时重编码为 PNG，否则保留原字节；返回 `ToolResult`（Text 标签 + `ContentBlock::Image`）；只读、只读会话也注册；验证：`cargo test -p yi-agent-tools --lib fs::view_image` — [设计](../superpowers/specs/2026-09-26-view-image-tool-design.md)
  - 编码后字节预算：返回的 base64 受 `MAX_ENCODED_BASE64_BYTES`（默认 4 MiB，`YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES` 覆盖）约束；超限按 `源格式无损 → JPEG q85/70/55/40 → ×0.75 降采样(下限512)` 降级，降级注记在 label；仍不可达则返回 `image omitted` 占位文字（非报错）。验证同上。
- [x] 文档读取工具 `read_document` — `crates/yi-agent-tools/src/fs/read_document.rs:34`（`ReadDocumentTool`）/ `crates/yi-agent-tools/src/lib.rs:90`（随 `register_builtin_tools` 注册，只读、不需确认：`metadata()` `read_document.rs:399`）。**格式覆盖**：`.pdf` 走 `lopdf` + 单页切片 + `pdf-extract`（`extract_pdf` `read_document.rs:149`）/ `.docx` 走 zip + `quick-xml`，标题按 `w:pStyle` 出 `#`、表格出 markdown 表（`extract_docx` `read_document.rs:223`；`<w:pStyle .../>` 是自闭合标签须接 `Event::Empty`，quick-xml 0.38 用 `BytesText::xml_content()`——spike §4 两个陷阱）/ `.html`·`.htm` 走 `html2md` / 其余扩展名按 UTF-8 纯文本（非 UTF-8 报错，不静默乱码）。**字符修复**：`repair_cjk` `read_document.rs:118` 做 NFKC + UCD `EquivalentUnifiedIdeograph` 两步——NFKC 只折叠康熙部首区（U+2F00–U+2FDF），不折叠 CJK 部首补充区（`⻓` U+2ED3 等），故必须再叠加 UCD 表；区间行（`2E8C..2E8D ; 5C0F`，表内共 5 条）**必须展开**，否则整段映射被静默丢弃（缺陷与 `⻓` 同类）。**按页**：`pages` 参数接受 `"3"` 或 `"1-5"`（1-based 闭区间，仅 PDF 生效），非法值报 `invalid pages spec`、越界页报 `page N does not exist`（不得伪装成扫描件）。**预算**：文件 50 MiB 上限（`MAX_DOCUMENT_BYTES`），正文默认 100 000 字符（`YI_AGENT_READ_DOCUMENT_MAX_CHARS` 覆盖），超限截断并在正文尾部说明 `partial` 用量；PDF 抽到空白文本层时返回「无文本层，疑似扫描件」提示而非空串。验证：`cargo test -p yi-agent-tools read_document`（17 例：字符修复 3 + 页参数 2 + 中文 PDF 修字符 + 按页只取该页 + 越界页报错 + 扫描件识别 + DOCX 标题与表格 + HTML + 纯文本 + 非 UTF-8 + 截断 + 越出 root 拒绝 + 注册齐全 + name/metadata）— [设计](../superpowers/specs/2026-10-07-document-attachments-design.md)、[spike](../research/2026-10-07-pdf-text-extraction-spike.md)
  - 已知限制：扫描版 PDF / 整页渲染不支持（返回「疑似扫描件」提示）；`.rtf` 无专门解析，按原始文本返回（含 RTF 标记）。
- [x] Glob 无边界扫描提示 — `crates/yi-agent-tools/src/fs/glob.rs` 对 `path` 为 root 且 `pattern` 为 `**/*` 的调用返回非失败 warning；验证：`cargo test -p yi-agent-tools --lib fs::glob::tests::glob_warns_on_unbounded_root_recursive_scan -- --exact`
- [x] Shell 工具：Bash — `crates/yi-agent-tools/src/shell/bash.rs` 实现 sh -c + 黑名单 + timeout + 输出截断 + 流式增量 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Shell 进程组隔离与异常整组回收 — `crates/yi-agent-tools/src/process_group.rs` 承载机制（`configure_process_group`、`signal_process_group`、`SIGKILL` 常量），策略留在调用点：`shell/bash.rs` spawn 加 `.process_group(0)`（子进程自任组长，pgid == 自身 pid，与 yi-agent 隔离），并以文件内 `ProcessGroupGuard` 兜底——**超时/取消**时对整组 SIGKILL 并 reap，**正常结束**显式 `disarm()` 保留后台进程（跨调用长驻）；`process/manager.rs` 仅改为复用共享机制、行为不变。验证：`cargo test -p yi-agent-tools --lib bash_`（含 `bash_child_runs_in_its_own_process_group`、`bash_timeout_kills_the_whole_process_group`、`bash_cancel_reaps_the_whole_process_group`、`bash_normal_exit_keeps_background_process_alive`、`bash_self_kill_by_process_group_does_not_kill_the_caller`）、`cargo test -p yi-agent-tools --test bash_stream bash_tool_does_not_leak_busy_loop_after_timeout` — [设计](../superpowers/specs/2026-09-28-bash-tool-process-group-isolation-design.md)、[计划](../superpowers/plans/2026-09-28-bash-tool-process-group-isolation-impl.md)
- [x] 工具注册 API — `crates/yi-agent-tools/src/lib.rs::register_builtin_tools()` 注册全部内置工具 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Web 工具：WebFetch + WebSearch — `crates/yi-agent-tools/src/web/` 目录；WebSearch 按 `~/.yi-agent/websearch.json` 决定注册形式（关闭则不注册；无 key 注册 `UnconfiguredProvider` 并在调用时说明原因；有 key 注册 Bocha provider，端点/条数/时效可配，见 `web/config.rs` 与 `lib.rs::register_web_search_from`）。桌面设置页「网页搜索」Tab 经 `websearch/get|set|test` 读写该文件。wiremock 测试专用 reqwest client 禁用环境代理，生产 client 保持代理支持；验证：`cargo test -p yi-agent-tools --lib`、`cargo test -p yi-agent-app-server websearch` — [设计](../superpowers/specs/2026-10-25-websearch-settings-design.md)、[计划](../superpowers/plans/2026-10-25-websearch-settings.md)
- [x] Skill 工具：SkillTool — `crates/yi-agent-tools/src/skill_tool.rs` 实现 `Tool` trait — [设计](../superpowers/plans/2026-07-25-skills-design.md)
- [x] Sandbox（进程隔离）— `crates/yi-agent-tools/src/sandbox.rs` 实现 `read-only` / `workspace-write` / `danger-full-access`；macOS 通过 Seatbelt、Linux 通过 Bubblewrap 强制文件写入与网络策略；运行 `cargo test -p yi-agent-tools workspace_sandbox read_only_sandbox` 验证受限模式
- [x] read-only 沙箱放行 `/dev/null` — `crates/yi-agent-tools/src/sandbox.rs` 的 macOS 策略在 `ReadOnly` 下原先只写 `(deny file-write*)`，而 Git 每次调用都以 `O_RDWR` 打开 `/dev/null`（含 `git log` / `git status` 这类纯读命令），于是 read-only 子 agent 的任何 `git` 调用都直接失败：`fatal: could not open '/dev/null' for reading and writing: Operation not permitted`（exit 128）。`workspace-write` 早就有这条 literal 放行，`ReadOnly` 漏了。改为在两种模式下统一追加 `(allow file-write* (literal "/dev/null"))`，并放在模式专属 deny 之后（SBPL 后者优先）；`/dev/null` 不是仓库状态，放行不扩大任何持久化写入面。验证：`cargo test -p yi-agent-tools --test sandbox_readonly`（修复前 `read_only_sandbox_can_run_git` 复现上述 exit 128，修复后通过；同文件 `read_only_sandbox_still_denies_workspace_writes` 守住 read-only 仍拒绝写入 workspace）
- [x] YOLO full bypass default — `crates/yi-agent/src/config.rs` 在未显式指定 `--sandbox` 或 `YI_AGENT_SANDBOX` 时为 `--yolo` 选择 `danger-full-access`；运行 `cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag` 验证
- [x] Managed process 数据模型与输出 ring buffer — `crates/yi-agent-tools/src/process/manager.rs` 定义 `ManagedProcessSnapshot` / `ProcessReadResult` / `StreamRingBuffer`；验证：`cargo test -p yi-agent-tools --lib process::manager::tests::stream_ring_buffer_ -- --nocapture`
- [x] Managed process tools — `crates/yi-agent-tools/src/process/` provides `ProcessManager` plus `process_start` / `process_list` / `process_read` / `process_kill`, with bounded stdout/stderr buffers, readiness matching, unique names, and managed kill; `process_read` / `process_kill` schemas use a top-level object without composition keywords for OpenAI-compatible providers; verification: `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schemas_are_openai_compatible -- --exact`
- [x] `process_kill` 整组回收（SIGTERM → 宽限 → 整组 SIGKILL）— 修复「报 killed 但子孙仍活着」：`ProcessManager::kill` 只做状态迁移，终止收敛到 `terminate_group`（`crates/yi-agent-tools/src/process/manager.rs:587`）；`wait_until_gone` 同时要求直接子进程被收割且整组无存活成员，组探针是 `process_group::group_exists`（`process_group.rs:48`，信号 0 探组）；`KILL_GRACE` 3s / `KILL_REAP_WAIT` 2s；`terminate_child`（`:1003`）在同步上下文改为整组 TERM 后立刻补 SIGKILL。验证：`cargo test -p yi-agent-tools --lib process`（35，含 `manager_kill_reaps_grandchildren_that_ignore_sigterm` / `manager_kill_lets_the_child_shut_down_gracefully_before_killing` / `manager_kill_does_not_return_until_the_process_is_gone`）
- [x] `process_read` 分页语义修正 + 槽位回收 — 修复三处独立丢数据：`StreamRingBuffer::read` 按 `max_bytes` 只保留**尾部**却把缓冲末尾当 `next_cursor`（游标与尾部之间的字节任何后续读取都拿不到，且 `truncated=false`）；`truncated` 只表示"游标早于保留窗口"、因 `max_bytes` 截断时不报告；stdout/stderr 共用一条字节游标，游标可能越过较慢的流、且两流各自 earliest 与共享游标相比较必然误报。现在游标读按共享时间轴的**窗口** `[cursor, cursor+N)` 从两条流同时返回（循环读不重不漏，`manager.rs` 的 `read` / `StreamPage` / `loss_floor` / `lost_before` / `read_window`），无游标读仍是尾部快照并如实报 `truncated`；窗口不会切在多字节字符中间，单个比 `max_bytes` 更宽的字符整字返回而非让游标停住。另在 `start` 时 `retain` 掉终端态进程（`manager.rs:303` 附近的槽位检查前），避免 16 个已退出的短命令永久耗尽槽位（`process_kill` 对它们本就是 no-op）。桌面详情面板随之改为**无游标**快照轮询（`desktop/src/hooks/useProcessState.ts`），否则替换式渲染只剩最近 500ms。验证：`cargo test -p yi-agent-tools --lib process::`（新增 `stream_ring_buffer_incremental_read_pages_without_skipping` / `stream_ring_buffer_snapshot_returns_tail_and_reports_dropped_prefix` / `stream_ring_buffer_window_ends_on_a_char_boundary` / `stream_ring_buffer_tail_does_not_split_a_multibyte_char` / `manager_read_pages_incrementally_without_gaps` / `manager_read_does_not_mark_truncated_for_fresh_cursor` / `manager_read_keeps_stderr_after_stdout_exceeds_cap` / `manager_read_advances_past_a_char_wider_than_max_bytes` / `manager_read_always_advances_the_cursor` / `manager_start_reclaims_slots_from_exited_processes`）；前端回归 `desktop/src/App.test.tsx` 的 `polls process/read as a snapshot`
- [x] 运行期可变沙箱提权（共享 `SandboxController`）— `crates/yi-agent-tools/src/sandbox.rs:30`（`SandboxController { switch, base, promotable }`）；`effective()` `:47` 仅当 `switch.get() && promotable` 提升到 `DangerFullAccess` 否则回 `base`；`new()` `:37` 在 `base == ReadOnly` 时强制不可提权（安全兜底，不可绕过）；`SandboxPolicy::with_controller` `:74` / `allows_writes()` `:103`（写工具面跟随 `base`，不随开关撤销）；`register_builtin_tools_with_controller`（`crates/yi-agent-tools/src/lib.rs:75`）让 bash 工具与 process manager 共享同一 controller，一次翻转同时改两条执行路径；验证：`cargo test -p yi-agent-tools --lib sandbox::` — [设计](../plans/2026-09-27-desktop-yolo-mode-design.md)
- [x] 图片预处理公共模块 `image_prep` — `crates/yi-agent-tools/src/image_prep.rs` 从 `view_image` 抽出可复用管线（`view_image` 改为调用它，见 `fs/view_image.rs:94`）：`MAX_IMAGE_BYTES=20MiB`（`:15`，解码前拦内存）、`DEFAULT_BASE64_BUDGET` / `MAX_ENCODED_BASE64_BYTES=4MiB`（`:17`/`:19`）、`resolve_budget`（`:40`，读 `YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES`，非法值 warn 后回退默认）、`prepare_image_bytes`（`:232`）/ `prepare_image_file`（`:265`，超限/非图片等在此拒绝）、`PreparedImage`（`:217`）与由 `ImageDetail` 驱动的缩放 + 无损→JPEG 降级（`HIGH_MAX_DIMENSION=2048` / `ORIGINAL_MAX_DIMENSION=6000` / `MIN_DIMENSION=512` / `JPEG_QUALITIES=[85,70,55,40]`）。app-server 的图片注入与分片上传共用同一条管线（`yi-agent-app-server/src/image_upload.rs:491`、`server.rs:7045`）。验证：`cd yi-agent-rs && cargo test -p yi-agent-tools --lib image_prep`

- [x] FS 工具的多根会话（根集替代单根）— `ToolsContext` 从**单一 root** 改为**根集**（`crates/yi-agent-tools/src/context.rs:21` `with_roots` / `:38` `roots()`，`roots[0]` 恒为主根；`:18` 单根构造等价于单元素根集，既有调用点语义不变），`relative_to_roots`（`:45` / `:61`）在命中根上算相对路径供 glob/grep 渲染；越界检查改为**逐根** `crates/yi-agent-tools/src/fs/path_util.rs:64`（`resolve_and_check_roots`：相对路径拼**主根**，绝对路径必须落在**任一**根内，`:137` `resolve_for_write_roots` 同理并额外允许尚不存在的父目录）；逃逸检查强度与单根时**一致**（`..`、根外绝对路径、末段符号链接、中间目录符号链接四类 × 每个根）。空根集是错误而非"放行一切"（`:434` / `:455`）。验证 `cd yi-agent-rs && cargo test -p yi-agent-tools`（`fs::path_util` 多根 10 例：`roots_resolve_relative_against_primary` / `roots_accept_absolute_path_inside_secondary` / `roots_reject_dotdot_escape_out_of_all_roots` / `roots_reject_absolute_outside_all_roots` / `roots_reject_symlink_escape_from_secondary` / `roots_reject_symlinked_parent_escape_on_write` / `roots_write_rejects_symlink_escape_from_secondary` / `roots_write_allows_nonexistent_parent_in_secondary` / `empty_roots_is_an_error`（读与写各一）等；`context.rs` 的 `with_roots_keeps_primary_first_and_shares_paths` / `relative_to_roots_reports_the_matching_root`）— [设计](../superpowers/specs/2026-10-17-linked-workspace-roots-design.md)
- [x] 注册 API 的**根集**参数语义 + git 内部根只进沙箱 — `register_builtin_tools_with_sandbox`（`crates/yi-agent-tools/src/lib.rs:71`）与 `register_builtin_tools_with_controller`（`lib.rs:90`）收**两个刻意分开**的根列表：`roots` 是 FS 工具的多根集（`roots[0]` 恒为主根，`crates/yi-agent-tools/src/context.rs:21` 的 `with_roots`；`read`/`write`/`edit`/`glob`/`grep` 的逃逸检查与相对路径解析以整集为准，见 `fs/path_util.rs:64`），`sandbox_writable_roots` 是 **shell 沙箱**的额外可写根（`lib.rs:75` → `SandboxPolicy::with_controller(ctx.root(), sandbox_writable_roots, controller)` `lib.rs:99`）。单根入口 `register_builtin_tools`（`lib.rs:53`）等价于 `roots = [root]` + 空沙箱可写根，既有调用点语义不变。**约束**：`git_writable_roots_for_worktree` 产出的 git 内部目录（`yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1239`）只能作为 `sandbox_writable_roots` 传入、**绝不**并入 `roots`，否则 `write` 能直写 `.git`；该 worker 侧的分离由 `cargo test -p yi-agent-subagent --lib fs_roots_stay_clear_of_the_git_roots_the_shell_sandbox_needs` 钉住。验证：`cargo test -p yi-agent-tools --lib with_roots_keeps_primary_first_and_shares_paths`、`cargo test -p yi-agent-tools --lib relative_to_roots_reports_the_matching_root`（`crates/yi-agent-tools/src/context.rs:89` / `:100`）— [设计](../superpowers/specs/2026-10-17-linked-workspace-roots-design.md)
- [x] 注入会话环境变量到 shell 子进程（`YI_AGENT_THREAD_ID`）— `ToolsContext` 新增 `session_env` 字段（`crates/yi-agent-tools/src/context.rs:15`，默认空 → 子进程行为与今天逐字节一致），构造器 `with_roots_and_env`（`context.rs:29`）与读取器 `session_env()`（`context.rs:39`），既有 `with_roots`（`context.rs:24`）委托传空集；`BashTool` spawn 时 `.envs(self.ctx.session_env().iter().cloned())`（`crates/yi-agent-tools/src/shell/bash.rs:171`）——让会话内经 bash 调用的命令知道自己是哪个会话（如 `superpowers-kanban add-prequeued` 用它记下来源会话）。新注册入口 `register_builtin_tools_with_controller_and_env`（`crates/yi-agent-tools/src/lib.rs:107`）带第 5 参 `session_env`；既有 `register_builtin_tools_with_controller`（`lib.rs:90`）签名不变、内部委托传空集，故 7 个既有调用点无需改动。验证：`cargo test -p yi-agent-tools`（`context::tests::a_context_carries_its_session_env`、`shell::bash::tests::bash_injects_the_session_env_into_the_child`、`lib::tests::the_env_carrying_registration_reaches_the_bash_tool`——真实调用 bash 断言子进程读到注入值）
- [x] 受管进程客户端的工具侧（`managed_process.rs` + `register_builtin_tools_managed`） —
  `ManagedChild` 不再是「一把锁住 socket 的结构体」：它只持三条管道，连接交给一个专属线程
  （`serve_child`）按帧**串行**处理 `Ask { request, timeout, reply }`。理由是被取消的 caller
  future 会连同它持有的 `&mut` 一起消失，而 socket 里可能正停在半帧上——由线程独占连接后，取消只丢掉
  一个 `oneshot`，协议不会错位。方法因此都是 `&self`：`wait` / `try_wait` / `terminate_group` /
  `retain_group`，另给后台 exit watcher 一个轻量的 `ManagedWatcher`。**正常结束必须调
  `retain_group()`**：`Drop` 的默认动作是尽力终止整组，而 `nohup ... &` 这类有意留下的后台进程正需要
  「连接关了但组还活着」。注册入口 `register_builtin_tools_managed`（`crates/yi-agent-tools/src/lib.rs`）
  把 `(ManagedProcessPolicy, ManagedSpawner)` 同时接到 bash 与 `ProcessManager::into_managed`；
  `resolve_program` 转 `pub` 供 runtime 的 MCP 启动器复用。判据：
  `cargo test -p yi-agent-tools`（295 lib passed）、`cargo test -p yi-agent-tools --test managed_bash`
  （3 passed，含 `a_managed_command_with_slow_output_does_not_stall_the_wait_loop`——10 个 tick ×
  100ms 期间 stderr 必须持续到达）、`cargo test -p yi-agent-tools --test managed_process_policy`
  （2 passed）

**验证命令：** `cargo test -p yi-agent-tools`（lib + 集成全绿；新增 `session_env` 3 例）

- [x] 临时旁问受限文件访问 — `SideReadAccess`（`side_read/mod.rs:99`）用侧用接口覆盖 read/read_document/view_image/glob/grep：每次调用重查动态根授权、打开前后校验路径未被换掉，附件映射同样走这套检查不能借它绕过撤权；有界阻塞 IO 由 `SideBlockingPool`（`side_read/blocking.rs:56`，2 线程 / 每任务 1 并发 / 8 队列 / 4 秒超时）承担，旁支不会饿死父的线程池。验收：`cargo test -p yi-agent-tools --lib side_` → 40 通过（含 `side_read_access_rechecks_roots_and_blocks_path_races`）— [三端 spec](../superpowers/specs/2026-11-03-side-conversation-design.md) · [实施计划](../superpowers/plans/2026-11-03-side-conversation-impl.md) §6、§12.1。
- [x] 托管进程选择器公开参数契约修复 — C1–C7 全部验收。C1：`process_selector_schema()` 显式 `required: []`，工具与字段说明写明「exactly one / 省略另一字段 / 不用空字符串占位」（`crates/yi-agent-tools/src/process/tools.rs`），`cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schema_contract`（先红于缺失 `required`）。C2/C3：`process_selector_argument_contract` 覆盖 4 种合法单选择器与 8 组非法输入，拒绝不改变 status/pid/ready；3 种突变均被捕获。C4：`crates/yi-agent-tools/tests/process_selector_request_contract.rs` 让真实 `ToolRegistry` schema 穿过 Anthropic / Chat / Responses 三条真实 provider 路径。C5/C6：**隔离实例（`YI_AGENT_HOME=/tmp/yi-iso-selector`）+ 修复版二进制 + 真实 HORIZON-DeepSeek + 真实网关 `llmapi.horizon.auto`（chat 通道）**实测：模型仅传 `name` 读取成功、仅传 `process_id` 读取成功、双有效字段返回 exactly-one 错误；用户在用实例未触碰。C7：`just ci` exit 0（16 道门禁）。取证与判别实验见 [取证记录](../research/2026-11-03-process-selector-contract-evidence.md) §11–§13，其中「缺失 required 即全必填」的根因假设已由实验证伪（`process_read` 另有 cursor/max_bytes，与报告现象不符）。**遗留（不影响本条目验收）**：①已部署实例仍为未修复版 `308ccaa4`，线上生效需按发布流程替换；②首个变形边界仍未定位，但修复后的定义在真实网关下已验证可用。fixture：`docs/research/fixtures/2026-11-03-process-selector-{transmitted-definition,live-acceptance}.json`。
- [x] `ProcessManager` 可在运行中就地升级为受管归属 — attach 成功前工具集已经建好（进程管理器也已注册），
  而 managed owner 要等这一会话拿到租约才存在。`adopt_managed(policy, spawner)` 在**同一个** manager 上
  就地替换 `Ownership`（policy 与 spawner 放在同一个 `Mutex` 里，避免观察到两者不一致），已经跑着的子进程
  句柄因此不丢；用重建 manager 的做法会让正在运行的进程失去归属且无法被停。判据：
  `cargo test -p yi-agent-tools`（347 lib + 集成全绿，含 `managed_bash` 3 passed）；
  `cargo test -p yi-agent --test frontend_runtime_lease`（9 passed）覆盖 TUI/headless 两条真实路径
