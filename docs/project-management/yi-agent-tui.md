# yi-agent-tui

## 模块说明

yi-agent 的终端用户界面（TUI），基于 ratatui 实现全屏布局。提供结构化对话历史展示、输入编辑、slash 命令弹窗、状态栏、bash 弹窗等功能。从早期 InlineRenderer（reedline 流式打印）迁移而来，现已设为默认 TUI 模式。

## 范围边界

**做什么：**
- ratatui 全屏布局（history 区 + popup 区 + input 区 + 状态栏）
- 结构化对话历史（HistoryCell: UserMessage / AssistantMessage / ToolCall / ToolResult / Separator / Markdown / Usage）
- Markdown 渲染（pulldown-cmark，标题/代码块/粗体/斜体/引用/链接/表格）
- 输入行编辑器（自实现，不依赖 reedline）
- 多行自动换行（unicode-width + CJK 宽度感知）
- Slash 命令弹窗（自动补全 + 中文描述 + Up/Down/Tab/Enter 导航）
- 两步退出确认（仅 Ctrl+C 两次退出）
- 输入排队（agent 运行期间粘贴的输入进队列，结束后回放）
- 状态栏（实时 token 计数 + 模型名 + 运行中任务指示）
- Bash 全屏弹窗（Ctrl+P 查看运行中/已完成 bash 实时输出 + exit code）

**不做什么：**
- 不做 InlineRenderer 的功能扩展（已 deprecated，待移除）
- 不做 syntax-highlight（可选 feature，默认关闭）
- 不做侧边栏 / 模态框（YAGNI）
- 不做 spinner / 进度条（YAGNI）

## Features

- [x] ratatui 全屏 TUI 架构 — `tui/app.rs` 实现事件循环 + 布局 — [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] 结构化对话历史 — `tui/cell.rs` 定义 `HistoryCell` 枚举 + `tui/history.rs` 管理 — [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] Markdown 渲染 — `tui/markdown.rs` 用 pulldown-cmark + 表格 Unicode box drawing — [设计](../plans/2026-07-25-tui-history-redesign.md)
- [x] LaTeX 终端渲染 — `tui/markdown.rs` 支持 `$...$`、`$$...$$`、`\\(...\\)`、`\\[...\\]` 并按终端宽度换行；验证：`cargo test -p yi-agent tui::markdown::tests -- --nocapture`
- [x] 输入框多行自动换行 — `tui/input.rs` 实现 CJK 宽度感知换行
- [x] 两步退出确认 — `tui/app.rs` Ctrl+C 两次才退出；Esc 只打断运行中的 agent 或命令，不退出进程；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::repeated_esc_does_not_quit` — [设计](../plans/2026-07-24-yi-agent-tui-features-design.md)
- [x] Slash 命令弹窗 — `tui/slash.rs` 实现自动补全 + 中文描述 — [设计](../plans/2026-07-25-tui-slash-commands-design.md)
- [x] 输入框光标可见 — `tui/input.rs` 反色显示（白底黑字）
- [x] 输入排队 — `tui/queued.rs::QueuedInput` 在 agent 运行期间缓存输入 — [设计](../plans/2026-07-25-tui-queued-input-design.md)
- [x] 状态栏 — `tui/statusbar.rs` 显示实时 token + 模型名 + 运行中任务 — [设计](../plans/2026-07-25-task-perception-design.md)
- [x] Bash 全屏弹窗 — `tui/bash_popup.rs` Ctrl+P 打开，显示实时输出 + exit code — [设计](../plans/2026-07-25-task-perception-design.md)
- [x] Ctrl+P managed process tab — `tui/process_popup.rs` and `tui/app.rs` add a `Processes` tab beside Bash Tasks, showing managed process status/output and kill confirmation; verification: `cargo test -p yi-agent --bin yi-agent tui::process_popup::tests` and `cargo test -p yi-agent --bin yi-agent runtime_popup`
- [x] Ctrl+P 仅显示 Bash 任务 — `tui/app.rs::route_event` 只注册 `bash` 工具调用，避免其他工具显示空详情；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::test_route_event_tracks_only_bash_tool_calls`
- [x] Bash 详情内容换行 — `tui/bash_popup.rs` 按终端显示宽度折行 Ctrl+P 详情中的命令、stdout 和 stderr，完整内容可通过上下滚动查看；验证：`cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests`
- [x] `/cost` 命令 — `tui/cost.rs::CostTracker` 按模型累计 token + 调用次数 — [设计](../plans/2026-07-26-tui-cost-command-design.md)
- [x] `/yolo` `/model` `/compact` `/clear` `/help` `/exit` slash 命令 — `tui/slash.rs` + `tui/app.rs` 路由
- [x] 压缩状态闭环 — `/compact` 的 pending 行由 `ManualCompacted` / `ManualCompactFailed` 原地更新，`AutoCompacting` 追加完成行；验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests::manual_compaction_` 和 `cargo test -p yi-agent --bin yi-agent tui::history::tests::auto_compaction_appends_completed_status`
- [x] Markdown 表格渲染 — commit `2e9da9e` 用 Unicode box drawing 修复
- [x] 终端原生复制与 bracketed paste — `tui/app.rs` 不启用 mouse capture，并路由 `Event::Paste`; 验证：`cargo test -p yi-agent tui::app::tests::paste_`
- [x] 对话历史滚动与滚动条 — `tui/history.rs` 的 `HistoryState` / `HistoryView` 处理当前宽度重排、锚点位置保持和右侧滚动条；`tui/app.rs` 路由键盘与鼠标滚动及本地插入后的最终宽度锚点恢复；未修饰 `Up` / `Down` 每次滚动 3 行，适配终端将触控板滚动转换为方向键的行为；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::history_anchor_survives_local_user_insertion_at_scrollbar_width`、`cargo test -p yi-agent --bin yi-agent tui::app::tests`、`cargo test -p yi-agent --bin yi-agent tui::history::tests`、`cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection`
- [x] 语义化对话留白 — `tui/history.rs` 在用户输入后、工具结果后的首段模型回复前各保留一行空白，工具调用/结果连续显示；验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests`
- [x] 多层绝对路径输入转发 — `tui/app.rs` 在首个空白符前的 token 含至少两个 `/` 时将完整输入发送给 agent；单层 `/tmp` 仍显示 `未知命令`；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_` 和 `cargo test -p yi-agent --bin yi-agent tui::app::tests::unknown_slash_command_shows_error`
- [x] 项目 AGENTS.md 提示词加载 — `main.rs::load_project_instructions()` 读取 `<workdir>/AGENTS.md` 并由 `resolve_system_prompt_with_skills()` 注入正常 TUI/run 会话；`--naked` 保持不加载；验证：`cargo test -p yi-agent --bin yi-agent resolve_system_prompt_`
- [x] 启动不污染项目目录 — `config.rs::load()` 只读取已存在的 `<workdir>/.yi-agent/.env`，不在 fallback 启动时创建 `<workdir>/.yi-agent`；验证：`cargo test -p yi-agent --bin yi-agent config::tests::load_does_not_create_local_yi_agent_dir_in_fallback_mode`
- [ ] InlineRenderer 退役 — `tui/` 仍保留 deprecated 的 InlineRenderer 代码，待删除
