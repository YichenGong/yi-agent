# permission

## 模块说明

yi-agent 的权限管理系统。参考 codex 的 `--yolo` 和 claude 的 `--dangerously-skip-permissions`，对需要授权的工具调用（bash、write、edit）进行确认。支持分层白名单和黑名单，通过 `.yi-agent/permissions.toml` 持久化。

## 范围边界

**做什么：**
- PermissionsConfig 数据结构（serde 持久化到 TOML）
- PermissionChecker（分层白名单：工具类型 + 命令前缀/路径模式）
- 黑名单命令（默认拒绝，用户可主动确认执行）
- Agent 集成（AgentEvent::PermissionRequest / PermissionResolved）
- CLI flag（`--yolo` / `--dangerously-skip-permissions` 跳过所有确认）
- PrefixExtractor trait（从工具调用中提取命令前缀/路径）

**不做什么：**
- 不做全局权限配置（仅项目级 `.yi-agent/permissions.toml`）
- 不做权限版本迁移（YAGNI）
- 不做多用户权限共享

## Features

- [x] 权限管理设计 — [设计](../plans/2026-07-25-permission-management-design.md)
- [~] PermissionsConfig 数据结构（serde 持久化）
- [~] PermissionChecker（分层白名单 + 黑名单）
- [~] Agent 集成（AgentEvent::PermissionRequest/PermissionResolved）
- [~] CLI flag（--yolo / --dangerously-skip-permissions）
- [ ] TUI 确认 UI
