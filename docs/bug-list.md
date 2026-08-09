# Bug 列表

- [ ] bash 执行过程中，如何停止当前的 bash 进程方式不明确
- [ ] 当前一些测试需要手工测试验证，无法自动化验证
- [ ] 显示内容太密集，user 和 system 的内容之间加空行
- [ ] bash 目前没有后台模式
- [ ] sandbox没有，目前命令执行危险。
- [ ] 命令行需要输入密码的话，TUI会出现显示故障。
- [x] 上下scroll速度过慢 — `tui/app.rs` 让未修饰 `Up` / `Down` 每次滚动 3 行，以支持终端将触控板滚动转换为方向键的模式；验证：`cargo test -p yi-agent --bin yi-agent tui::app::tests::normal_navigation_keys_route_to_history_without_affecting_shift_selection`
- [ ] 排队user request加入对话的逻辑不是很清晰。
- [x] 长输出流在约 4000 Token 时出现 `error decoding response body` — Provider 默认总超时已从 60 秒提高到 5 分钟：`yi-agent-rs/crates/yi-agent-llm/src/{anthropic,openai}/client.rs`；验证：`cargo test -p yi-agent-llm --lib default_stream_timeout_is_five_minutes`
- [ ] bash执行结果现在显示的只有一行，多给几行结果，会更好。
- [ ] 每次大模型的一个调用间的TUI显示，最好都有空格。
- [ ] 我希望在运行的时候能够切换模型。目前看起来没什么选择
- [ ] 确认是否支持图片读取。
- [ ] 如果输入框输入的是一个路径开始的内容。系统会把他当成slash command，然后会反馈说“未知命令”
- [ ] 当遇到一系列的待确认项的时候，最好有进度条。
- [ ] 两次ESC不应该直接退出Agent的进程。ESC可以打断命令执行，可以打断对话，但是不应该退出整体进程。
- [x] `--yolo` 条件下，`/dev/null` 受 sandbox 限制阻断 — `crates/yi-agent/src/config.rs` 在未显式指定 `--sandbox` 或 `YI_AGENT_SANDBOX` 时选择 `danger-full-access`，从而不经 sandbox wrapper 执行；验证：`cargo test -p yi-agent --bin yi-agent config::tests::load_yolo_from_cli_flag`。
