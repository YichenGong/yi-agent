# CLAUDE.md

项目级 Claude Code 指令,会被自动加载到 Claude Code 的上下文。

## Commit 规范

- **不要**在 commit message 里写 `Co-Authored-By: Claude ...` 行
- Commit message 用 conventional commits 风格(`feat:`, `fix:`, `ci:`, `docs:` 等)
- 首行简短(<=72 字符),空行后可选写正文说明"为什么"

## cargo test 执行

- 在 worktree 里跑测试用 `cd <worktree>/yi-agent-rs && cargo test ...`,或在
  worktree 根目录用 `cargo test --manifest-path yi-agent-rs/Cargo.toml ...`。
  两种方式都用 worktree 自己的 `yi-agent-rs/target/`,与主仓库互不干扰。
- **不要**在多个 shell / worktree 同时跑 `cargo test`:即便 target 目录独立,
  cargo 对 workspace 元数据文件仍有锁竞争,可能导致编译卡死或 exit 137
  (SIGKILL,通常被 OOM 杀掉)。跑测试前先 `ps aux | grep cargo` 确认没有其他
  cargo 进程。
- 如果 `cargo test` 一直卡住(尤其是 cancel/interrupt 类测试),先怀疑测试代码
  死锁而不是编译卡住:检查测试是否在 `agent.run()` 之前捕获 cancel token,
  但 `Agent::run()` 会重置 token(`agent.rs` Fix 1),导致旧 token cancel 无效。
  正确写法是在 `run()` 之后捕获 token。
- 单独跑一个卡住的测试名(如 `cargo test -p yi-agent-core --lib <test_name>`)
  通常秒级返回;如果一个测试卡但其它测试不卡,基本是测试代码问题而非环境问题。

