# 项目管理追踪系统设计文档

**日期**: 2026-07-19
**状态**: 已确认
**范围**: `docs/project-management/` 目录结构与文件格式

---

## 1. 目标

为 yi-agent 项目建立项目级进度追踪机制，记录"什么做了、什么没做、什么应该做"，用树形 markdown 结构呈现，每完成一个 feature 就划掉。

## 2. 设计决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 范围 | 项目级 | 覆盖所有模块与子项目（yi-agent-core、CI/CD、未来模块） |
| 文件组织 | 索引 + 分文件 | 顶层 README.md 显示树形总览，每个模块单独文件记录细节 |
| 状态分类 | 四态 | `[x]` 已完成 / `[~]` 进行中 / `[ ]` 未完成 / `[-]` 已放弃 |
| 与 plans 文档关系 | 软链接 | 追踪文件只列状态和一句话描述，细节链接到 `docs/plans/` 下对应文档 |
| 模块文件内容 | 最简 + 背景 | 模块说明 + 范围边界 + feature 列表（含 plans 链接） |

## 3. 目录结构

```
docs/
  plans/                      # 现有，不动
  project-management/         # 新建
    README.md                 # 顶层索引，树形总览
    yi-agent-core.md          # 模块文件
    ci-cd.md                  # 模块文件
```

## 4. 索引文件格式 (`README.md`)

```markdown
# yi-agent 项目进度总览

## 状态图例
- [x] 已完成
- [~] 进行中
- [ ] 未完成
- [-] 已放弃

## 功能树

- **模块名** → [详情](./模块名.md)
  - [状态] feature 一句话描述
  - [状态] feature 一句话描述
```

每个顶层条目是模块，链接到对应模块文件；模块下缩进列出 features。

## 5. 模块文件格式

```markdown
# 模块名

## 模块说明

一两句话讲清这个模块是什么。

## 范围边界

**做什么：**
- 列表

**不做什么：**
- 列表

## Features

- [状态] feature 名称 — [对应文档](../plans/路径.md)
- [状态] feature 名称
```

## 6. 初始内容

### 6.1 yi-agent-core 模块

基于 git 历史（commits 52ef678、35f00a9、698cf04、949b7fd、d115f95）：

**已完成 `[x]`：**
- 消息模型 (Role, Message, ContentBlock) — 链接设计文档
- Tool trait 与 ToolRegistry — 链接设计文档
- Provider trait 与 ProviderEvent — 链接设计文档
- Agent loop、Session、AgentEvent（并行工具执行）— 链接实现文档

**未完成 `[ ]`：**
- 流式输出与中断处理
- Token 计数（扩展 AgentEvent::Done）
- 图片工具（ContentBlock::Image 已留类型）
- 插件系统（基于 ToolSource::Plugin）

### 6.2 ci-cd 模块

基于 git 历史（commit b89eaed、74b1633）和文件检查（`.gitlab-ci.yml`、`.github/workflows/`、`justfile` 均不存在）：

**已完成 `[x]`：**
- CI/CD 设计文档 — 链接设计文档
- CI/CD 实现计划 — 链接实现文档

**未完成 `[ ]`：**
- justfile 与 rust-toolchain.toml
- npm 包结构（主包 + 平台子包）
- Homebrew tap 自动更新脚本
- GitLab CI 配置
- GitHub Actions CI 配置（PR 触发）
- GitHub Actions Release 配置（tag 触发）
- Mac mini runner 配置
- 首次端到端验证

**已放弃 `[-]`：**
- 覆盖率统计（codecov.io）— YAGNI，暂不做
- crates.io 发布 — YAGNI，暂不做

## 7. 维护方式

- 完成一个 feature：改对应模块文件里的状态 `[ ]` → `[x]`，同时更新 `README.md` 索引
- 新增 feature：在模块文件和 `README.md` 索引都加一行 `[ ]`
- 放弃 feature：改为 `[-]`，并在行尾注明原因
- 开始做 feature：改为 `[~]`
- 新增模块：创建模块文件 + 在 `README.md` 索引添加顶层条目
