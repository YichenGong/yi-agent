# yi-agent-tui

## 模块说明

yi-agent 的终端用户界面（TUI），基于 ratatui 实现全屏布局。提供结构化对话历史展示、输入编辑、slash 命令弹窗等功能。从早期 InlineRenderer（reedline 流式打印）迁移而来，现已设为默认 TUI 模式。

## 范围边界

**做什么：**
- ratatui 全屏布局（history 区 + popup 区 + input 区）
- 结构化对话历史（HistoryCell: UserMessage / AssistantMessage / ToolCall / ToolResult / Separator）
- Markdown 渲染（pulldown-cmark，标题/代码块/粗体/斜体/引用/链接）
- 输入行编辑器（自实现，不依赖 reedline）
- 多行自动换行（unicode-width + CJK 宽度感知）
- Slash 命令弹窗（自动补全 + 中文描述 + Up/Down/Tab/Enter 导航）
- 两步退出确认（Ctrl+C / Esc 两次退出）
- 输入框光标可见（反色显示：白底黑字）
- 可折叠工具调用/结果（Ctrl+O 切换）
- 历史区滚动（Shift+↑/↓ 选中 cell，Ctrl+U/D 半屏滚动）

**不做什么：**
- 不做 InlineRenderer 的功能扩展（已 deprecated，仅保留兼容）
- 不做 syntax-highlight（可选 feature，默认关闭）
- 不做侧边栏 / 模态框（YAGNI）
- 不做 spinner / 进度条（YAGNI）

## Features

- [x] ratatui 全屏 TUI 架构（HistoryView + InputLine + 事件循环）— [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] 结构化对话历史（HistoryCell + 折叠/展开）— [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] Markdown 渲染（pulldown-cmark，含代码块 + 表格 Unicode box drawing）— [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] 输入框多行自动换行（CJK 宽度感知）
- [x] 两步退出确认（Ctrl+C / Esc）— [设计](../plans/2026-07-24-yi-agent-tui-features-design.md)
- [x] Slash 命令弹窗（自动补全 + 中文描述）— [设计](../plans/2026-07-25-tui-slash-commands-design.md)
- [x] 输入框光标可见（反色显示）
- [x] 输入排队（agent 运行期间粘贴的输入进入队列，结束后回放）— [设计](../plans/2026-07-25-tui-queued-input-design.md)
- [x] 状态栏（实时 token 计数 + 模型名 + 运行中任务指示）— [设计](../plans/2026-07-25-task-perception-design.md)
- [x] Bash 全屏弹窗（Ctrl+P 查看运行中/已完成 bash 的实时输出与 exit code）— [设计](../plans/2026-07-25-task-perception-design.md)
- [x] `/cost` 命令（按模型累计 input/output/cache token 与调用次数）— [设计](../plans/2026-07-26-tui-cost-command-design.md)
- [x] `/yolo` `/model` `/compact` `/clear` `/help` `/exit` slash 命令
- [ ] InlineRenderer 退役（已标记 deprecated，待移除）
