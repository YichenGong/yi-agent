# Bug 列表

- [ ] bash 执行过程中，如何停止当前的 bash 进程方式不明确
- [ ] 当前一些测试需要手工测试验证，无法自动化验证
- [ ] 显示内容太密集，user 和 system 的内容之间加空行
- [ ] bash 目前没有后台模式
- [ ] 命令行需要输入密码的话，TUI会出现显示故障。
- [ ] 排队user request加入对话的逻辑不是很清晰。
- [ ] 我希望在运行的时候能够切换模型。目前看起来没什么选择
- [ ] 确认是否支持图片读取。
- [ ] 如果输入框输入的是一个路径开始的内容。系统会把他当成slash command，然后会反馈说“未知命令”
- [ ] 当遇到一系列的待确认项的时候，最好有进度条。
- [ ] 自动压缩后，Prefill的数字好像不会自动更新了。
- [ ] 出现多次连续自动压缩的情况
- [ ] subagent 如果一直不停。怎么办。
- [ ] 当前启动subagent runtime 就会创建worktree。太多了怎么清理。
- [x] `daemon serve` 用 `state.sqlite`、内嵌 TUI/headless runtime 用 `runtime.sqlite`，同一项目任务历史分裂（修复：`main.rs` `runtime_database_path` 统一为 `runtime.sqlite`）
- [x] resident lease 释放与 resource coordinator 锁序反转，reconcile 热路径可死锁（修复：`runtime.rs` `release_resident_lease` 不再同时持两把锁）