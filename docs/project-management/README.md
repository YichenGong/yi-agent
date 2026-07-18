# yi-agent 项目进度总览

## 状态图例
- [x] 已完成
- [~] 进行中
- [ ] 未完成
- [-] 已放弃

## 功能树

- **yi-agent-core** → [详情](./yi-agent-core.md)
  - [x] 消息模型 (Role, Message, ContentBlock)
  - [x] Tool trait 与 ToolRegistry
  - [x] Provider trait 与 ProviderEvent
  - [x] Agent loop、Session、AgentEvent（并行工具执行）
  - [ ] 流式输出与中断处理
  - [ ] Token 计数（扩展 AgentEvent::Done）
  - [ ] 图片工具（ContentBlock::Image 已留类型）
  - [ ] 插件系统（基于 ToolSource::Plugin）
- **ci-cd** → [详情](./ci-cd.md)
  - [x] CI/CD 设计文档
  - [x] CI/CD 实现计划
  - [ ] justfile 与 rust-toolchain.toml
  - [ ] npm 包结构（主包 + 平台子包）
  - [ ] Homebrew tap 自动更新脚本
  - [ ] GitLab CI 配置
  - [ ] GitHub Actions CI 配置（PR 触发）
  - [ ] GitHub Actions Release 配置（tag 触发）
  - [ ] Mac mini runner 配置
  - [ ] 首次端到端验证
  - [-] 覆盖率统计（codecov.io）— YAGNI，暂不做
  - [-] crates.io 发布 — YAGNI，暂不做
