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
  - [x] ProviderRequest / AgentConfig 加 model 字段（请求级模型选择）
  - [ ] 流式输出与中断处理
  - [ ] Token 计数（扩展 AgentEvent::Done）
  - [ ] 图片工具（ContentBlock::Image 已留类型）
  - [ ] 插件系统（基于 ToolSource::Plugin）
- **yi-agent-llm** → [详情](./yi-agent-llm.md)
  - [x] AnthropicProvider 设计
  - [x] AnthropicProvider 实现（types + stream + client + 测试）
  - [ ] OpenAI provider
  - [ ] 本地模型 (Ollama) provider
  - [ ] Bedrock / Vertex AI 适配
  - [ ] 重试与流断连重连
- **yi-agent-tools** → [详情](./yi-agent-tools.md)
  - [ ] FS 工具：Read/Write/Edit/Glob/Grep（单一 root 限制）
  - [ ] Shell 工具：Bash（sh -c + 黑名单 + timeout + 输出截断）
  - [ ] 工具注册 API：register_builtin_tools()
  - [ ] Sandbox（跨平台进程隔离）— 延后单独设计
  - [ ] Web 工具（搜索/抓取）— 延后单独一轮
- **ci-cd** → [详情](./ci-cd.md)
  - [x] CI/CD 设计文档
  - [x] CI/CD 实现计划
  - [x] justfile 与 rust-toolchain.toml
  - [x] npm 包结构（主包 + 平台子包）
  - [x] Homebrew tap 自动更新脚本
  - [x] GitLab CI 配置
  - [x] GitHub Actions CI 配置（PR 触发）
  - [x] GitHub Actions Release 配置（tag 触发）
  - [x] Mac mini runner 配置
  - [x] 首次端到端验证
  - [-] 覆盖率统计（codecov.io）— YAGNI，暂不做
  - [-] crates.io 发布 — YAGNI，暂不做
- **tooling** → [详情](./tooling.md)
  - [x] 项目进度追踪系统（docs/project-management/）
  - [x] 进度同步 Hook（Stop 事件触发，检测代码改动后提醒更新进度表格）
