# yi-agent-tools

## 模块说明

yi-agent 的内置工具实现 crate，提供 coding agent 的 FS（文件系统）、Shell、Web、Skill 核心工具能力，通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

## 范围边界

**做什么：**
- 实现 FS 工具（Read/Write/Edit/Glob/Grep）
- 实现 Shell 工具（Bash 命令执行，支持流式输出增量推送）
- 实现 Web 工具（WebFetch + WebSearch）
- 实现 Skill 工具（加载并执行 yi-agent-skills 发现的 skill）
- 路径安全（单一 root 限制，canonicalize + starts_with）
- 工具注册 API（`register_builtin_tools`）

**不做什么：**
- 不做 MCP 协议工具（由 yi-agent-mcp 负责）
- 不做插件系统（基于 ToolSource::Plugin，后续）
- 不做 sandbox（跨平台方案，单独一轮设计）

## Features

- [x] FS 工具：Read/Write/Edit/Glob/Grep — `crates/yi-agent-tools/src/fs/` 五个 tool 文件 + 单一 root 校验 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Shell 工具：Bash — `crates/yi-agent-tools/src/shell/bash.rs` 实现 sh -c + 黑名单 + timeout + 输出截断 + 流式增量 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] 工具注册 API — `crates/yi-agent-tools/src/lib.rs::register_builtin_tools()` 注册全部内置工具 — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Web 工具：WebFetch + WebSearch — `crates/yi-agent-tools/src/web/` 目录，WebSearch 在有 `BOCHA_API_KEY` 时注册 — [设计](../plans/2026-07-19-yi-agent-web-tools-design.md)
- [x] Skill 工具：SkillTool — `crates/yi-agent-tools/src/skill_tool.rs` 实现 `Tool` trait — [设计](../plans/2026-07-25-skills-design.md)
- [ ] Sandbox（跨平台进程隔离）— 无 sandbox 模块
