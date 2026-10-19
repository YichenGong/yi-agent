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
- [x] 稳定版 / 开发版分道 — 分支模型为 `main`（开发）+ `release/X.Y`（发布准备）；stable 只经发布渠道（npm / Homebrew / dmg）安装，回滚 = 装回旧版本号；dev 构建由 `./scripts/rebuild.sh cli --profile dev` 产出并只装到 `~/.local/bin/yi-agent-dev`，不碰 `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/yi-agent`。判据：跑该命令前后 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变
- [x] 临时开发实例并行测试 — `yi-agent-rs/scripts/dev-env.sh`（`id` / `up [--sync-skills]` / `run <cmd>` / `down [--purge]`）；实例根 `~/.yi-agent-instances/<slug>-<branch>-<hash6>/.yi-agent`，通过 `YI_AGENT_HOME` + `YI_AGENT_RUNTIME_DIR` + `YI_AGENT_DISABLE_WATCHMAN=1` 与 stable 完全隔离，同一 worktree 可同时跑两套。判据：`bash yi-agent-rs/scripts/test_dev_env.sh`（6 passed）
