# yi-agent-tools

## 模块说明

yi-agent 的内置工具实现 crate，提供 coding agent 的 FS（文件系统）、Shell、Web、Skill 核心工具能力，通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

## 范围边界

**做什么：**
- 实现 FS 工具（Read/Write/Edit/Glob/Grep）
- 实现图片读取工具（ViewImage：png/jpeg/gif/webp 读取、缩放、base64 编码）
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
- [x] Glob 无边界扫描提示 — `crates/yi-agent-tools/src/fs/glob.rs` 对 `path` 为 root 且 `pattern` 为 `**/*` 的调用返回非失败 warning；验证：`cargo test -p yi-agent-tools --lib fs::glob::tests::glob_warns_on_unbounded_root_recursive_scan -- --exact`
- [x] Shell 工具：Bash — `crates/yi-agent-tools/src/shell/bash.rs` 实现 sh -c + 黑名单 + timeout + 输出截断 + 流式增量 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] 工具注册 API — `crates/yi-agent-tools/src/lib.rs::register_builtin_tools()` 注册全部内置工具 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Web 工具：WebFetch + WebSearch — `crates/yi-agent-tools/src/web/` 目录，WebSearch 在有 `BOCHA_API_KEY` 时注册；wiremock 测试专用 reqwest client 禁用环境代理，生产 client 保持代理支持；验证：`cargo test -p yi-agent-tools --lib` — [设计](../plans/2026-07-19-yi-agent-web-tools-design.md)
- [x] Skill 工具：SkillTool — `crates/yi-agent-tools/src/skill_tool.rs` 实现 `Tool` trait — [设计](../superpowers/plans/2026-07-25-skills-design.md)
- [x] Sandbox（进程隔离）— `crates/yi-agent-tools/src/sandbox.rs` 实现 `read-only` / `workspace-write` / `danger-full-access`；macOS 通过 Seatbelt、Linux 通过 Bubblewrap 强制文件写入与网络策略；运行 `cargo test -p yi-agent-tools workspace_sandbox read_only_sandbox` 验证受限模式
- [x] YOLO full bypass default — `crates/yi-agent/src/config.rs` 在未显式指定 `--sandbox` 或 `YI_AGENT_SANDBOX` 时为 `--yolo` 选择 `danger-full-access`；运行 `cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag` 验证
- [x] Managed process 数据模型与输出 ring buffer — `crates/yi-agent-tools/src/process/manager.rs` 定义 `ManagedProcessSnapshot` / `ProcessReadResult` / `StreamRingBuffer`；验证：`cargo test -p yi-agent-tools --lib process::manager::tests::stream_ring_buffer_ -- --nocapture`
- [x] Managed process tools — `crates/yi-agent-tools/src/process/` provides `ProcessManager` plus `process_start` / `process_list` / `process_read` / `process_kill`, with bounded stdout/stderr buffers, readiness matching, unique names, and managed kill; `process_read` / `process_kill` schemas use a top-level object without composition keywords for OpenAI-compatible providers; verification: `cargo test -p yi-agent-tools --lib process::tools::tests::process_selector_schemas_are_openai_compatible -- --exact`
