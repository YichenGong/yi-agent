# tooling

## 模块说明

yi-agent 项目的辅助工具与开发流程配置，包括进度追踪、hook 自动化等非业务代码设施。

## 范围边界

**做什么：**
- 项目进度追踪机制（docs/project-management/）
- Claude Code hook 自动化（.claude/）
- 开发流程辅助脚本

**不做什么：**
- 不做 CI/CD 流水线（由 ci-cd 模块负责）
- 不做业务代码（由各核心 crate 负责）

## Features

- [x] 项目进度追踪系统 — `docs/project-management/` 目录 + 模块文件齐全 — [设计](../plans/2026-07-19-project-management-design.md)
- [x] 进度同步 Hook — `.claude/` Stop 事件 hook 检测代码改动后提醒更新进度 — [设计](../plans/2026-07-19-progress-sync-hook-design.md)
- [x] Git worktree 工作流 — `.worktrees/` 目录用于 feature 分支隔离开发
