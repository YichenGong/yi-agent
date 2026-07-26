# yi-agent-skills

## 模块说明

yi-agent 的 skills 系统，提供可发现的 skill 目录（类似 Claude Code 的 skills 概念）。`yi-agent-skills` crate 负责 skill 的发现、加载与 catalog 渲染；`yi-agent-tools` 中的 `SkillTool` 通过该服务让 agent 能执行 skill。

## 范围边界

**做什么：**
- Skill 元数据模型（`SkillMetadata`、`SkillScope`：Project / User / System）
- 多根目录 skill 发现（项目 `.yi-agent/skills/`、用户 `~/.yi-agent/skills/`、系统内置）
- YAML skill 定义加载与校验
- `SkillsService`：根目录管理、snapshot、catalog 渲染（按预算截断）
- 系统级 skill 内置安装（`install_system_skills()`）
- `SkillTool`：把 skill 暴露为 agent 可调用的 Tool
- 系统提示词注入 skill catalog（受 `--skills-catalog-budget` 控制）

**不做什么：**
- 不做 skill 的在线市场或远程拉取
- 不做 skill 版本管理
- 不做 skill 沙箱隔离（skill 在 agent 进程内执行）

## Features

- [x] Skills 系统设计 — 设计与实现文档齐全 — [设计](../plans/2026-07-25-skills-design.md) · [实现](../plans/2026-07-25-skills-impl.md)
- [x] Skill 元数据与作用域 — `crates/yi-agent-skills/src/model.rs` 定义 `SkillMetadata` + `SkillScope`（Project/User/System）
- [x] 多根目录发现与 YAML 加载 — `discovery.rs::discover_skills()` + `loader.rs` 实现 YAML 解析校验
- [x] `SkillsService` — `service.rs::SkillsService` 提供 `snapshot()` / `render_catalog()` / `full_catalog_size()`
- [x] 系统内置 skill 安装 — `system.rs::install_system_skills()` + `assets/` 目录含内置 skill 定义
- [x] `SkillTool` 注册到 agent — `crates/yi-agent-tools/src/skill_tool.rs` 实现 `Tool` trait，`main.rs:105` 注册
- [x] 系统提示词注入 skill catalog — `main.rs::resolve_system_prompt_with_skills()` 按 `--skills-catalog-budget` 截断注入
