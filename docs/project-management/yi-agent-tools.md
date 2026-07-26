# yi-agent-tools

## 模块说明

yi-agent 的内置工具实现 crate，提供 coding agent 的 FS（文件系统）和 Shell 核心工具能力，通过实现 `yi-agent-core` 的 `Tool` trait 接入 agent。

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

- [x] FS 工具：Read/Write/Edit/Glob/Grep（单一 root 限制）— [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Shell 工具：Bash（sh -c + 黑名单 + timeout + 输出截断 + 流式增量）— [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] 工具注册 API：register_builtin_tools() — [设计](../plans/2026-07-19-yi-agent-tools-design.md)
- [x] Web 工具：WebFetch + WebSearch（Bocha）— [设计](../plans/2026-07-19-yi-agent-web-tools-design.md)
- [x] Skill 工具：SkillTool（调用 yi-agent-skills 服务发现的 skill）— [设计](../plans/2026-07-25-skills-design.md)
- [ ] Sandbox（跨平台进程隔离，Linux seccomp/bubblewrap + macOS sandbox-exec）— 延后单独设计
