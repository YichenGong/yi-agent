# CLAUDE.md

项目级 Claude Code 指令,会被自动加载到 Claude Code 的上下文。

## Commit 规范

- **不要**在 commit message 里写 `Co-Authored-By: Claude ...` 行
- Commit message 用 conventional commits 风格(`feat:`, `fix:`, `ci:`, `docs:` 等)
- 首行简短(<=72 字符),空行后可选写正文说明"为什么"
- **提交前必须跑 `cargo fmt --all`**(在 `yi-agent-rs/` 下),保证代码格式
  通过 `just fmt-check`。`git commit` 前的 hook 不自动跑 fmt,所以需要手动
  执行或在 commit 前先 `cargo fmt --all && git add ...`。

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
- **被中断的 `cargo test` 会留下僵尸测试二进制进程**(进程名形如
  `yi_agent_tools-<hash>`),即使 cargo 本身已退出,这些子进程仍在后台持有
  target 目录锁,导致后续 `cargo test` 立即卡死或 exit 137。复现步骤:
  跑 `cargo test -p yi-agent-tools --lib`,若被中断,下次再跑会立即卡住。
  修复:`ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent"`
  找出残留进程并 `kill <pid>`,必要时 `find target -name "*.lock" -type f -delete`
  清理 incremental lock 文件(空的 mutex 文件,删了 cargo 会重建)。
- **定位卡住的测试**:不要靠猜,直接用 macOS `sample` 工具抓取卡住进程的
  调用栈。步骤:(1) `ps aux | grep -v grep | grep yi_agent` 找到卡住的测试
  二进制 PID;(2) `sample <pid> 2` 抓 2 秒样本;(3) 看 `Call graph` 里的线程名
  和栈顶函数——线程名通常会显示 `shell::bash::tests::<test_name>`,直接定位
  是哪个测试死锁。比逐个 `--test <name>` 排查快得多。
- **避免 `cargo test --workspace` 全量跑**:workspace 全量编译 + 全量测试
  会同时启动多个 crate 的测试二进制,容易触发 OOM(exit 137)或死锁级联。
  优先按 crate 跑:`cargo test -p yi-agent`、`cargo test -p yi-agent-core`、
  `cargo test -p yi-agent-tools`。确需全量跑时,用 `--jobs 2` 降低并行度,
  并先 `ps aux | grep cargo` 确认没有残留进程。
- **编译与运行分离排查**:`cargo test --no-run` 只编译不跑,秒级完成说明
  编译没问题,卡住的是测试运行;`target/debug/deps/<test_binary>` 直接跑
  测试二进制可以绕过 cargo 的进程管理,配合 `sample` 定位死锁测试。

