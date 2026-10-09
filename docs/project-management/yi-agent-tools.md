# yi-agent-tools

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
- [x] Web 工具：WebFetch + WebSearch — `crates/yi-agent-tools/src/web/` 目录，WebSearch 在有 `BOCHA_API_KEY` 时注册；wiremock 测试专用 reqwest client 禁用环境代理，生产 client 保持代理支持；验证：`cargo test -p yi-agent-tools --lib` — [设计](../plans/2026-07-19-yi-agent-web-tools-design.md)
- [x] Skill 工具：SkillTool — `crates/yi-agent-tools/src/skill_tool.rs` 实现 `Tool` trait — [设计](../superpowers/plans/2026-07-25-skills-design.md)
- [x] Sandbox（进程隔离）— `crates/yi-agent-tools/src/sandbox.rs` 实现 `read-only` / `workspace-write` / `danger-full-access`；macOS 通过 Seatbelt、Linux 通过 Bubblewrap 强制文件写入与网络策略；运行 `cargo test -p yi-agent-tools workspace_sandbox read_only_sandbox` 验证受限模式
- [x] read-only 沙箱放行 `/dev/null` — `crates/yi-agent-tools/src/sandbox.rs` 的 macOS 策略在 `ReadOnly` 下原先只写 `(deny file-write*)`，而 Git 每次调用都以 `O_RDWR` 打开 `/dev/null`（含 `git log` / `git status` 这类纯读命令），于是 read-only 子 agent 的任何 `git` 调用都直接失败：`fatal: could not open '/dev/null' for reading and writing: Operation not permitted`（exit 128）。`workspace-write` 早就有这条 literal 放行，`ReadOnly` 漏了。改为在两种模式下统一追加 `(allow file-write* (literal "/dev/null"))`，并放在模式专属 deny 之后（SBPL 后者优先）；`/dev/null` 不是仓库状态，放行不扩大任何持久化写入面。验证：`cargo test -p yi-agent-tools --test sandbox_readonly`（修复前 `read_only_sandbox_can_run_git` 复现上述 exit 128，修复后通过；同文件 `read_only_sandbox_still_denies_workspace_writes` 守住 read-only 仍拒绝写入 workspace）
- [x] YOLO full bypass default — `crates/yi-agent/src/config.rs` 在未显式指定 `--sandbox` 或 `YI_AGENT_SANDBOX` 时为 `--yolo` 选择 `danger-full-access`；运行 `cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag` 验证
- [x] Managed process 数据模型与输出 ring buffer — `crates/yi-agent-tools/src/process/manager.rs` 定义 `ManagedProcessSnapshot` / `ProcessReadResult` / `StreamRingBuffer`；验证：`cargo test -p yi-agent-tools --lib process::manager::tests::stream_ring_buffer_ -- --nocapture`
- [x] Managed process tools — `crates/yi-agent-tools/src/process/` provides `ProcessManager` plus `process_start` / `process_list` / `process_read` / `process_kill`, with bounded stdout/stderr buffers, readiness matching, unique names, and managed kill; `process_read` / `process_kill` schemas use a top-level object without composition keywords for OpenAI-compatible providers; verification: `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schemas_are_openai_compatible -- --exact`
- [x] 运行期可变沙箱提权（共享 `SandboxController`）— `crates/yi-agent-tools/src/sandbox.rs:30`（`SandboxController { switch, base, promotable }`）；`effective()` `:47` 仅当 `switch.get() && promotable` 提升到 `DangerFullAccess` 否则回 `base`；`new()` `:37` 在 `base == ReadOnly` 时强制不可提权（安全兜底，不可绕过）；`SandboxPolicy::with_controller` `:74` / `allows_writes()` `:103`（写工具面跟随 `base`，不随开关撤销）；`register_builtin_tools_with_controller`（`crates/yi-agent-tools/src/lib.rs:75`）让 bash 工具与 process manager 共享同一 controller，一次翻转同时改两条执行路径；验证：`cargo test -p yi-agent-tools --lib sandbox::` — [设计](../plans/2026-09-27-desktop-yolo-mode-design.md)
- [x] 图片预处理公共模块 `image_prep` — `crates/yi-agent-tools/src/image_prep.rs` 从 `view_image` 抽出可复用管线（`view_image` 改为调用它，见 `fs/view_image.rs:94`）：`MAX_IMAGE_BYTES=20MiB`（`:15`，解码前拦内存）、`DEFAULT_BASE64_BUDGET` / `MAX_ENCODED_BASE64_BYTES=4MiB`（`:17`/`:19`）、`resolve_budget`（`:40`，读 `YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES`，非法值 warn 后回退默认）、`prepare_image_bytes`（`:232`）/ `prepare_image_file`（`:265`，超限/非图片等在此拒绝）、`PreparedImage`（`:217`）与由 `ImageDetail` 驱动的缩放 + 无损→JPEG 降级（`HIGH_MAX_DIMENSION=2048` / `ORIGINAL_MAX_DIMENSION=6000` / `MIN_DIMENSION=512` / `JPEG_QUALITIES=[85,70,55,40]`）。app-server 的图片注入与分片上传共用同一条管线（`yi-agent-app-server/src/image_upload.rs:491`、`server.rs:7045`）。验证：`cd yi-agent-rs && cargo test -p yi-agent-tools --lib image_prep`

**验证命令：** `cargo test -p yi-agent-tools`（270 个测试：lib 247 + 集成 23，均全绿）
