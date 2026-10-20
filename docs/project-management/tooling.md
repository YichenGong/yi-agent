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
- [x] 稳定版 / 开发版分道 — 分支模型为单一长期分支 `main`（发版是 `main` 上的动作，不另开 release 分支；给旧版本打补丁才从 tag 切短命分支）；stable 只经发布渠道（npm / Homebrew / dmg）安装，回滚 = 装回旧版本号；dev 构建由 `./scripts/rebuild.sh cli --profile dev` 产出并只装到 `~/.local/bin/yi-agent-dev`，不碰 `~/.cargo/bin/yi-agent` 与 `/opt/homebrew/bin/yi-agent`。判据：跑该命令前后 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变
- [x] `build.rs` 的 rerun 路径修正 — 原实现监听 `<repo>/yi-agent-rs/.git/HEAD`（不存在：真实 `.git` 在上一层，worktree 里 `.git` 还是文件），cargo 把不存在的路径当作永远变化，于是「每次重跑」这个正确行为变成了巧合；且**创建 tag 不改 HEAD/index**，一旦路径真的存在就会打了 tag 不重建、`--version` 继续报 `-dev`。现改为 `git rev-parse` 取真实目录：`refs/tags` 取 **common dir**（worktree 共享）、`HEAD`/`index` 取 per-worktree dir，只声明确实存在的路径，并监听 `Cargo.toml`（版本 bump 会改它）。判据：`cd yi-agent-rs && bash scripts/test_build_rerun_paths.sh`（读 cargo fingerprint 逐条确认路径存在且含 `refs/tags`）；端到端复核：打 tag 后自动重建为 release、再提交一次回到 dev
- [x] `rebuild.sh` 默认装开发版 — 默认 `PROFILE` 由 `stable` 改为 `dev`，即 `./scripts/rebuild.sh cli` 只写 `~/.local/bin/yi-agent-dev`；覆盖稳定版必须显式 `--profile stable`。理由：日常九成是编开发版，默认值反了等于「随手重编就把稳定版定义污染成某个下午的 main」。判据：不带参数跑一次后 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变
- [x] 临时开发实例并行测试 — `yi-agent-rs/scripts/dev-env.sh`（`id` / `up [--sync-skills]` / `run <cmd>` / `down [--purge]`）；实例根 `~/.yi-agent-instances/<slug>-<branch>-<hash6>/.yi-agent`，通过 `YI_AGENT_HOME` + `YI_AGENT_RUNTIME_DIR` + `YI_AGENT_DISABLE_WATCHMAN=1` 与 stable 完全隔离，同一 worktree 可同时跑两套。判据：`bash yi-agent-rs/scripts/test_dev_env.sh`（7 项全 ok）
- [x] 实例开箱可用 — `dev-env.sh` 的 `ensure_instance` 同时复制 `.env` 与 `models.json`；`down --purge` 删整个 `<id>/` 而不是 `<id>/.yi-agent`（否则 `~/.yi-agent-instances` 会积一堆空壳目录）。判据：`bash yi-agent-rs/scripts/test_dev_env.sh` 的 `models.json is copied into the instance` 与 `down --purge removes the instance dir`
- [x] 功能开关（feature flags）— 见 [yi-agent-flags](./yi-agent-flags.md)。`yi-agent-rs/crates/yi-agent-flags/` 是唯一判定点；注册表 `registry.rs` 定义全部 flag（必填 `expires_on`），`flags.json` 存实例级取值（跟 `paths::instance_home()` 走），取值链为「默认 → 文件 → 环境变量 `YI_AGENT_FLAG_*` → 构建类型硬闸」。**非 dev 构建一律全关**，无逃生口。两个入口共用同一份存储：`yi-agent flags list|set|status` 与 RPC `flags/list`、`flags/set`（stable 上返回 `-32601`）；设置页「实验功能」Tab 仅在 `build/info` 报 `dev` 时渲染。判据：`cargo test -p yi-agent-flags`（17 passed）；`yi-agent flags status <name>` 在 dev 报 `source: file`/`default`、在 release 报 `source: build-kind`
