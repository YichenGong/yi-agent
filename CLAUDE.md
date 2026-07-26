# CLAUDE.md

项目级 Claude Code 指令,会被自动加载到 Claude Code 的上下文。

## 分支与 worktree 规范

- **严禁在 `main` 分支上直接修改代码或提交 commit。** 所有变更——无论代码、
  文档、配置——都必须先建 worktree、在新分支上改,再合回 `main`。
- 建 worktree 用 `git worktree add .worktrees/<branch> -b <type>/<name>`,
  `.worktrees/` 已在 `.gitignore` 中。`<type>` 用 conventional commits
  前缀(`fix/`、`feat/`、`docs/`、`ci/` 等)。
- 改完在 worktree 里跑测试 + `cargo fmt --all`,确认通过后再 commit,
  最后回 `main` 做 `git merge --no-ff <branch>` 合并,合并后删分支 + 移除
  worktree。流程详见 `superpowers:using-git-worktrees` 和
  `superpowers:finishing-a-development-branch` skill。
- 唯一例外:紧急回滚 / hotfix 也走 worktree 流程,不允许在 `main` 上直接
  `git revert` 或改文件。

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

## 真实 LLM 测试

- 默认 `cargo test` / `just test` / `just ci` 只跑 mock 测试(wiremock 模拟),
  不调用真实 LLM API,无需 API key。
- 真实 LLM 测试用 `#[ignore]` 标记 gate,默认跳过,需手动加 `--ignored`:
  - provider 层冒烟:`cargo test -p yi-agent-llm --test real_integration -- --ignored`
  - 端到端(经 `yi-agent run`):`cargo test -p yi-agent --test e2e_real -- --ignored`
- 推荐用 justfile recipe,无 API key 时自动跳过(exit 0,非失败):
  - `just test-real-llm`:跑 provider 层
  - `just test-real-e2e`:跑端到端
  - `just test-real-all`:两者都跑
- 真实测试需要环境变量 `ANTHROPIC_API_KEY` 或 `OPENAI_API_KEY`。测试代码开头
  会检查 env var,无 key 时 `eprintln!("skip")` 并 return(双保险,即使误加
  `--ignored` 也安全跳过)。
- CI **不跑**真实 LLM 测试(避免成本与密钥泄露),只跑 mock 测试。
- `yi-agent run` 子命令是端到端测试的载体:非交互式 drain `AgentEvent` 到
  stdout/stderr,`--json` 切换为 JSONL 供测试断言。详见
  `docs/plans/2026-07-26-real-llm-testing-design.md`。

## 分级测试系统

- **Tier 0 (Mock)**: `cargo test` / `just test` — wiremock,总是跑,无 API key
- **Tier 1 (Provider smoke)**: `just test-real-llm` — SSE 解析、鉴权
- **Tier 2 (Simple e2e)**: `just test-real-e2e` — 单轮文本、单工具调用
- **Tier 3 (Complex one-shot)**: `just test-real-complex` — 多步骤生成任务
  (个人网站、Python 脚本、数据转换、bug 修复)
- `just test-real-all` 跑 Tier 1 + 2 + 3
- 复杂测试用 `tempfile::TempDir` 隔离,300s 超时,结构性断言(文件存在/大小/标记)
- 复杂测试同样是 `#[ignore]` gate,CI 不跑
- 测试文件: `yi-agent-rs/crates/yi-agent/tests/e2e_complex.rs`
- 共享 helper: `yi-agent-rs/crates/yi-agent/tests/common/mod.rs`

## 项目进度维护

- **每完成一组需求,必须同步更新 `docs/project-management/` 下对应模块文件**,
  并在同一个 commit 或 PR 内提交。不要事后补。
- 状态只有三态:**`[x]` 已完成**、**`[ ]` 未完成**、**`[-]` 已放弃**。
  **禁止使用 `[~]` 进行中**——要么完成要么没完成,"进行中"对读者没信息量且
  容易成为黑洞(进去就出不来)。
- 每条 feature 必须带**可验证的完成判据**:代码位置(`file.rs:line`)或
  可执行命令,不要写"已实现权限管理"这种主观描述。
- 更新模块文件后,同步更新 `README.md` 模块索引表的"完成 / 总计"计数。
- **新增 crate 时**,同一 PR 内必须:
  1. 在 `docs/project-management/` 下创建对应模块文件
  2. 在 `README.md` 模块索引表登记一行
- `bug-list.md` 同理:只有 `[x]` 已修复 / `[ ]` 未修复两态,不用 `[~]`。
- 维护规则详见 `docs/project-management/README.md` 末尾"维护规则"小节。

