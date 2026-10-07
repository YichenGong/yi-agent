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
- 中途追加投递（agent 运行期间粘贴的输入立即折进当前轮次，下一次请求前生效）
- 状态栏（实时 token 计数 + 模型名 + 运行中任务指示）
- 运行时全屏弹窗（Ctrl+P 打开，Bash Tasks / Processes 两个 tab；Bash tab 查看运行中/已完成 bash 实时输出 + exit code，Processes tab 查看托管进程状态并支持 kill 确认）

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
- [x] 中途追加投递 — `PendingQueue` 更名 `DeliveredInterjections`（`tui/queued.rs`）：忙时提交**立即投出**（`input_tx.try_send`），由 driver 在运行中调 `agent.interject` 折进当前轮次，不再攒到轮次结束才发；该类型只做**记账**（已投递、未见回执），预览标题据此改为 `⌛ 已送达，待生效 (N)`；`InterjectionAccepted` 经 `on_receipt` 冲销最旧一条，`InterjectionsReturned` 经 `take_returned` 把文本退回输入框并写提示行（`tui/app.rs::apply_interjection_event`，在回合结束分支**之前**调用）；开启轮次的那条 prompt 不入账（它走 `run()`，永远没有回执）；投递失败（通道满 / `inbox_handle()` 为 `None` 的窗口）仍退回输入框，沿用 `Rejected` 语义 — [设计](../superpowers/specs/2026-09-30-mid-turn-user-interjection-design.md)（取代 [排队设计](../superpowers/specs/2026-09-27-tui-pending-queue-design.md)）
- [x] 状态栏 — `tui/statusbar.rs` 显示实时 token + 模型名 + 运行中任务 — [设计](../plans/2026-07-25-task-perception-design.md)
- [x] Bash 全屏弹窗 — `tui/app.rs::RuntimePopup` Ctrl+P 打开默认 Bash tab，`tui/bash_popup.rs` 显示实时输出 + exit code — [设计](../plans/2026-07-25-task-perception-design.md)
- [x] Ctrl+P managed process tab — `tui/process_popup.rs` and `tui/app.rs` add a `Processes` tab beside Bash Tasks, showing managed process status/output and kill confirmation; verification: `cargo test -p yi-agent --bin yi-agent tui::process_popup::tests` and `cargo test -p yi-agent --bin yi-agent runtime_popup`
- [x] 托管后台进程状态栏计数 — `tui/app.rs::is_active_managed_process` 统计 `Starting` / `Running` / `Ready` 快照，`tui/statusbar.rs::render_statusbar` 显示无耗时的 `N proc running`；验证：`cargo test -p yi-agent --bin yi-agent tui::statusbar::tests` 和 `cargo test -p yi-agent --bin yi-agent active_managed_process_statuses_exclude_terminal_states`
- [x] Ctrl+P 仅显示 Bash 任务 — `tui/app.rs::route_event` 只注册 `bash` 工具调用，避免其他工具显示空详情；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::test_route_event_tracks_only_bash_tool_calls`
- [x] Bash 详情内容换行 — `tui/bash_popup.rs` 按终端显示宽度折行 Ctrl+P 详情中的命令、stdout 和 stderr，完整内容可通过上下滚动查看；验证：`cargo test -p yi-agent --bin yi-agent tui::bash_popup::tests`
- [x] `/cost` 命令 — `tui/cost.rs::CostTracker` 按模型累计 token + 调用次数 — [设计](../plans/2026-07-26-tui-cost-command-design.md)
- [x] slash 命令目录与路由 — `tui/slash.rs::SlashCommand` 定义 29 个命令（`/quit` `/clear` `/model` `/cost` `/compact` `/config` `/help` `/agents` `/agent` `/events` `/diff` `/mailbox` `/message` `/pause` `/resume` `/cancel` `/retry` `/priority` `/approve` `/deny` `/review` `/accept` `/rework` `/reject` `/budget` `/daemon` `/mcp` `/runtime` `/kanban`），`?` 是 `/help` 的别名（`slash.rs::from_name`）；`tui/app.rs` 经 `SlashCommand::from_name` 路由，未命中时在转录里回显「未知命令」。**注意 `/yolo` 不是 slash 命令**——跳过权限确认只有启动参数 `--yolo` / `--dangerously-skip-permissions`（`crates/yi-agent/src/config.rs`），运行期无对应命令；`/runtime [ask|always|never]` 管的是子 agent 本地运行时的启动偏好（`tui/runtime_prefs.rs`），与权限无关。验证：`cargo test -p yi-agent --bin yi-agent tui::slash::`、`cargo test -p yi-agent --bin yi-agent unknown_slash_command_shows_error`、`cargo test -p yi-agent --bin yi-agent submit_single_segment_absolute_path_shows_unknown_command`
- [x] slash 命令补齐：3 个落地 + 4 个隐藏 — `/config` 现渲染真实会话配置快照（`tui/slash.rs::TuiConfigSnapshot::render`，startup 组装、不含任何密钥）；`/model <name>` 运行期切换（driver 以 `ControlCommand::SetModel` 重建 agent、保留 session，回 `AgentEvent::ModelChanged` 更新 `current_model` 与状态栏）；`/daemon [status|stop]` 走既有 IPC（`IpcRequest::Status` / `Stop`）。**无后端支撑的 4 个命令**（`/approve` `/deny` `/budget` `/priority`）**已从弹窗与全量 `/help` 隐藏，但仍保留识别**：`SlashCommand::all()` 是完整目录（供 `from_name` 解析），新增 `SlashCommand::completable()` 供弹窗与 `/help` 全量列表；显式敲这 4 个命令时给出理由（`SlashCommand::unavailable_reason()`，形如 `/approve 暂不支持：<原因>`），不再误报为未知命令。该 4 项无 core/store/IPC 后端，如需另行立项。验证：`cargo test -p yi-agent --bin yi-agent tui::`、`cargo test -p yi-agent --bin yi-agent -- manual_compaction`、`cargo test -p yi-agent --bin yi-agent -- control_command`、`cargo test -p yi-agent-core --lib model_changed`
- [x] 压缩状态闭环 — `/compact` 的 pending 行由 `ManualCompacted` / `ManualCompactFailed` 原地更新，`AutoCompacting` 追加完成行；driver 侧把三种结果统一经 `manual_compaction_outcome_event` 转成上述事件回给 TUI（手动压缩无论成功/无历史/失败都会原地收尾 pending 行，不再只报 `AgentEvent::Error`）。验证：`cargo test -p yi-agent --bin yi-agent -- manual_compaction` 和 `cargo test -p yi-agent --bin yi-agent tui::history::tests::auto_compaction_appends_completed_status`
- [x] 自动压缩后 Prefill 数字跟随下降 — `tui/statusbar.rs::tick` 经 `interpolate` 双向收敛显示值到 target（原先单向 `saturating_sub`，压缩后卡在旧值）；验证：`cargo test -p yi-agent --bin yi-agent tui::statusbar::tests::test_prefill_follows_estimate_down_after_compaction`
- [x] Markdown 表格渲染 — commit `2e9da9e` 用 Unicode box drawing 修复；`tui/markdown.rs` `flush_table()` 现按终端宽度收缩列宽并折行单元格（对齐保持），放不下最小盒子时退化为竖排记录，消除右侧永久裁剪 — [设计](../superpowers/specs/2026-09-27-tui-table-width-design.md)
- [x] 终端原生复制与 bracketed paste — `tui/app.rs` 不启用 mouse capture，并路由 `Event::Paste`; 验证：`cargo test -p yi-agent tui::app::tests::paste_`
- [x] 对话历史滚动与滚动条 — `tui/history.rs` 的 `HistoryState` / `HistoryView` 处理当前宽度重排、锚点位置保持和右侧滚动条；`tui/app.rs` 路由键盘与鼠标滚动及本地插入后的最终宽度锚点恢复；未修饰 `Up` / `Down` 每次滚动 3 行，适配终端将触控板滚动转换为方向键的行为；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::history_anchor_survives_local_user_insertion_at_scrollbar_width`、`cargo test -p yi-agent --bin yi-agent tui::app::tests`、`cargo test -p yi-agent --bin yi-agent tui::history::tests`、`cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection`
- [x] 语义化对话留白 — `tui/history.rs` 在用户输入后、工具结果后的首段模型回复前各保留一行空白，工具调用/结果连续显示；验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests`
- [x] 多层绝对路径输入转发 — `tui/app.rs` 在首个空白符前的 token 含至少两个 `/` 时将完整输入发送给 agent；单层 `/tmp` 仍显示 `未知命令`；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::submit_` 和 `cargo test -p yi-agent --bin yi-agent tui::app::tests::unknown_slash_command_shows_error`
- [x] Ctrl+P 子 agent 页签与只读轨迹 — Ctrl+P 现在三态循环 Bash 任务 / 托管进程 / 子 agent；子 agent 页签列出当前对话（`ListTaskSummaries` + `thread_id`）的子任务并按状态着色，`Enter` 打开只读轨迹详情：`read_task_trace` 回灌历史后订阅全 kind 实时追加，正文渲染复用主转录的 `HistoryView`（折叠与滚动与主会话同一套），行由 `TraceFeed` 折叠（连续 `assistant_text` 合并为一段，`tool_call` / `tool_result` / `state_note` 各自成块）；详情页列出直接子任务可继续 `Enter` 进入，`Esc` 逐级返回，栈空回列表；同一时刻至多一条订阅（`sync_trace_streams` 在关闭弹窗时显式丢弃流，重开同一任务会重新回灌而不是复用旧流）；`/agent <task-id>` 改为直接打开同一详情视图。代码：`yi-agent-rs/crates/yi-agent/src/tui/trace.rs:88`（`TraceAction`）、`trace.rs:108`（`TraceFeed`）、`trace.rs:255`（`TraceDetailPopup`）、`trace.rs:436`（`render_subagent_detail`）、`trace.rs:670`（`TraceStreams`）、`yi-agent-rs/crates/yi-agent/src/tui/app.rs:1186`（`open_agent_detail`）、`app.rs:1196`（`handle_agents_key`）、`app.rs:1377`（`sync_trace_streams`）、`app.rs:3112`（`subagent_children_at`）、`yi-agent-rs/crates/yi-agent/src/tui/process_popup.rs`（`RuntimeTab::Agents`）；验证：`cargo test -p yi-agent --bin yi-agent tui::trace`（17 个）、`cargo test -p yi-agent --bin yi-agent tui::`
- [x] 子 agent 轨迹页签的两个干预动作 — 详情页 `m` 进入发消息输入（`Enter` 提交，走既有 `SendUserMessage`；`Esc` 放弃），`k` 触发取消——**先** `PreviewCancel` 拿 `confirmation_token` 并在页脚显示"继续按 y 确认"，再按 `y` 走 `ConfirmCancel`，任何其它键放弃；不绕过既有的二次确认路径。代码：`yi-agent-rs/crates/yi-agent/src/tui/trace.rs:88`（`TraceAction::Cancel { confirmation_token }`）、`yi-agent-rs/crates/yi-agent/src/tui/app.rs:1196`（`handle_agents_key` 的 `awaiting_cancel` 分支）、`app.rs`（`execute_trace_action` / `preview_cancel_token_at`）；验证：`cargo test -p yi-agent --bin yi-agent tui::trace`
- [ ] 项目 AGENTS.md 提示词加载 — 全仓未找到 `load_project_instructions` / `AGENTS.md` 的代码实现（仅文档提及），需重新实现或确认已放弃。提示词装配本身已迁至 `yi-agent-runtime/src/bootstrap.rs::build_prompt_setup`（默认 prompt + 当前日期 + skills catalog），`--naked` 保持不加载；验证：`cargo test -p yi-agent-runtime --lib resolve_system_prompt_`
- [x] 启动不污染项目目录 — `yi-agent-runtime/src/config.rs::RuntimeConfig::load()` 只读取已存在的 `<workdir>/.yi-agent/.env`，不在 fallback 启动时创建 `<workdir>/.yi-agent`；验证：`cargo test -p yi-agent-runtime --lib config::tests::load_does_not_create_local_yi_agent_dir_in_fallback_mode`
- [ ] InlineRenderer 退役 — `tui/` 仍保留 deprecated 的 InlineRenderer 代码，待删除
- [x] 全宽内容不被右侧截断 — 所有渲染生产点（代码块 `tui/markdown.rs::push_wrapped`、工具调用/结果与分隔线 `tui/cell.rs` 按显示宽度折行、排队预览 `tui/queued.rs`、输入框换行 `tui/app.rs::wrap_input_buffer` 支持 `\n` 且 `compute_input_height` 与渲染共用同一折行、状态栏 `tui/statusbar.rs` 按宽度预算裁剪、popup `tui/process_popup.rs` / `tui/bash_popup.rs`）都保证行宽 <= 终端宽度，`tui/history.rs::HistoryView::render` 另加 `debug_assert` 兜底；根因是 ratatui 对超宽 `Line` 只从右侧截断（`Line::render_with_alignment`），而 history 每行只占一格高、无换行机会。见 [设计](../superpowers/specs/2026-09-27-tui-line-clipping-design.md)。验证：`cargo test -p yi-agent --bin yi-agent -- tui::`、`cargo test -p yi-agent --bin yi-agent -- tui::history::tests::rendered_history_never_exceeds_terminal_width`
- [x] 长会话渲染缓存 — `tui/history.rs` 的 `HistoryState::cache` 按宽度缓存每个 cell 的渲染行，`ensure_cache` 快路径让空闲帧 O(1)（不再遍历 cell），`content_generation` + `fingerprint` 兜住原地修改（流式追加 / 折叠 / 权限展开），`text_width` 判定结果按内容版本 memo 以免探测宽度与渲染宽度互相翻转导致每帧重折，`render` 通过 `part_offsets` 二分只取可见行且借用不 clone；重构前空闲帧全量遍历 7 次、成本随 transcript 线性增长（400 回合约 19ms release / 190ms debug），重构后稳态 400 回合 0.09ms、每帧 cell 重渲染 0 次。见 [设计](../superpowers/specs/2026-09-30-tui-history-render-cache-design.md)。验证：`cargo test -p yi-agent --bin yi-agent tui::history::tests::settled_frame_does_not_re_render_every_cell`、`cargo test -p yi-agent --bin yi-agent tui::history::tests::fitting_frame_work_is_bounded_regardless_of_size`、`cargo test -p yi-agent --bin yi-agent tui::history::tests::cached_flatten_matches_naive_render`、`cargo test -p yi-agent --bin yi-agent tui::history::tests::memoized_output_matches_naive_render_over_long_sequence`、`cargo test -p yi-agent --bin yi-agent tui::`
- [x] 权限确认弹窗完整可见 — `tui/wrap.rs::wrap_by_display_width` 优先空白断行、逐字符保留原文；`tui/cell.rs` 折叠 4 行 + `[e]` 展开；`tui/app.rs` 待确认时放行 `e` 与滚动键；验证：`cargo test -p yi-agent --bin yi-agent tui::wrap::` 和 `cargo test -p yi-agent --bin yi-agent tui::app::tests::permission_key_e_toggles_expanded`
- [x] `/model` 清单与切换命令 — 无参列清单（显示名 + 全局默认 + 当前会话「← 当前」标记），带参把**当前会话**切到该显示名（走 `ControlCommand::SetModel`，模型名换成解析后的 cfg），`/model default` 清会话覆盖；TUI 直接读 `yi-agent-runtime::models` 清单、不经 app-server，会话覆盖只活在进程内（不写 thread meta）。代码：`yi-agent-rs/crates/yi-agent/src/tui/app.rs:2231`（`render_model_list`）、`app.rs:2272`（`handle_model_command`）、`app.rs:340`（`current_model_ref` 选中显示名）、`app.rs:1784`（`KeyOutcome::ModelSelected`）；验证 `cargo test -p yi-agent --bin yi-agent tui::app::tests::model_command_`
