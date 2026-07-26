# Bug 列表

- [x] web-fetch 存在 render 失败问题（markdown 渲染改进后修复）
- [x] bash 执行过程时间统计没有停止（`tui/state.rs` 在 exit/timeout/abort 时冻结 `end_time`）
- [ ] bash 执行过程中，如何停止当前的 bash 进程方式不明确
- [x] TUI render markdown 表格格式失败（`tui/markdown.rs` Unicode box drawing 已实现）
- [ ] 当前一些测试需要手工测试验证，无法自动化验证
- [ ] 显示内容太密集，user 和 system 的内容之间加空行
