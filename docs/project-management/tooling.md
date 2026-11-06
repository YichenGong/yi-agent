# tooling

## 模块说明

yi-agent 项目的辅助工具与开发流程配置，包括进度追踪、hook 自动化等非业务代码设施。

## 范围边界

**做什么：**
- 项目进度追踪机制（docs/project-management/）
- Claude Code hook 自动化（.claude/）
- 开发流程辅助脚本

**不做什么：**
- 不做 CI/CD 流水线（由 ci-cd 模块负责）
- 不做业务代码（由各核心 crate 负责）

## Features

- [x] 项目进度追踪系统 — `docs/project-management/` 目录 + 模块文件齐全 — [设计](../plans/2026-07-19-project-management-design.md)
- [x] 进度同步 Hook — `.claude/` Stop 事件 hook 检测代码改动后提醒更新进度 — [设计](../plans/2026-07-19-progress-sync-hook-design.md)
- [x] Git worktree 工作流 — `.worktrees/` 目录用于 feature 分支隔离开发
- [x] 稳定版 / 开发版分道 — 分支模型为单一长期分支 `main`（发版是 `main` 上的动作，不另开 release 分支；给旧版本打补丁才从 tag 切短命分支）；stable 只经发布渠道（npm / Homebrew / dmg）安装，回滚 = 装回旧版本号；dev 构建由 `./scripts/rebuild.sh cli --profile dev` 产出并只装到 `~/.local/bin/yi-agent-dev`，不碰 `~/.cargo/bin/yi-agent`（全局软链 `/opt/homebrew/bin/yi-agent` 自 2026-11-01 起由两个 profile 都接管，见下条）。判据：跑该命令前后 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变
- [x] `build.rs` 的 rerun 路径修正 — 原实现监听 `<repo>/yi-agent-rs/.git/HEAD`（不存在：真实 `.git` 在上一层，worktree 里 `.git` 还是文件），cargo 把不存在的路径当作永远变化，于是「每次重跑」这个正确行为变成了巧合；且**创建 tag 不改 HEAD/index**，一旦路径真的存在就会打了 tag 不重建、`--version` 继续报 `-dev`。现改为 `git rev-parse` 取真实目录：`refs/tags` 取 **common dir**（worktree 共享）、`HEAD`/`index` 取 per-worktree dir，只声明确实存在的路径，并监听 `Cargo.toml`（版本 bump 会改它）。判据：`cd yi-agent-rs && bash scripts/test_build_rerun_paths.sh`（读 cargo fingerprint 逐条确认路径存在且含 `refs/tags`）；端到端复核：打 tag 后自动重建为 release、再提交一次回到 dev
- [x] `rebuild.sh` 默认装开发版 — 默认 `PROFILE` 由 `stable` 改为 `dev`，即 `./scripts/rebuild.sh cli` 只写 `~/.local/bin/yi-agent-dev`；覆盖稳定版二进制必须显式 `--profile stable`。理由：日常九成是编开发版，默认值反了等于「随手重编就把稳定版定义污染成某个下午的 main」。判据：不带参数跑一次后 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变
- [x] `rebuild.sh` 装完把全局路径接过去 — 此前 dev 构建只写 `~/.local/bin/yi-agent-dev`，而登录 shell 的 PATH 里 `/opt/homebrew/bin` 排在 `~/.local/bin` 与 `~/.cargo/bin` 之前（实测 `which -a yi-agent` 首选它），于是「编译安装成功」与「终端里敲 `yi-agent` 用上新版本」是两件事，重建看起来没生效。现 `link_global` 让两个 profile 都接管 `/opt/homebrew/bin/yi-agent` 这个软链：dev 指 `~/.local/bin/yi-agent-dev`、stable 指 `~/.cargo/bin/yi-agent`，两者都不覆盖对方的二进制文件（稳定版 shasum 在 dev 重建前后不变）。真实文件（brew 装的）拒绝覆盖并列出现状与出路；父目录不存在则 warn 跳过、不中断整条构建（没有 Homebrew 的机器不该因此编不完），存在但不可写则失败（路径真实存在，静默跳过会误导）。安装段与编译解耦为 `install_cli_bin`，三个落点可用 `REBUILD_LOCAL_BIN_DIR` / `REBUILD_CARGO_BIN_DIR` / `REBUILD_GLOBAL_BIN` 注入，脚本被 source 时不执行 `main`。判据：`bash scripts/test_rebuild_global_link.sh`（25 passed）＋真机 `zsh -lc 'which yi-agent'` 命中 `/opt/homebrew/bin/yi-agent` → `~/.local/bin/yi-agent-dev`，且 `shasum -a 256 ~/.cargo/bin/yi-agent` 不变 — [设计](../superpowers/specs/2026-11-01-rebuild-install-global-design.md)
- [x] 临时开发实例并行测试 — `yi-agent-rs/scripts/dev-env.sh`（`id` / `up [--sync-skills]` / `run <cmd>` / `down [--purge]`）；实例根 `~/.yi-agent-instances/<slug>-<branch>-<hash6>/.yi-agent`，通过 `YI_AGENT_HOME` + `YI_AGENT_RUNTIME_DIR` + `YI_AGENT_DISABLE_WATCHMAN=1` 与 stable 完全隔离，同一 worktree 可同时跑两套。判据：`bash yi-agent-rs/scripts/test_dev_env.sh`（7 项全 ok）
- [x] 实例开箱可用 — `dev-env.sh` 的 `ensure_instance` 同时复制 `.env` 与 `models.json`；`down --purge` 删整个 `<id>/` 而不是 `<id>/.yi-agent`（否则 `~/.yi-agent-instances` 会积一堆空壳目录）。判据：`bash yi-agent-rs/scripts/test_dev_env.sh` 的 `models.json is copied into the instance` 与 `down --purge removes the instance dir`
- [x] 功能开关（feature flags）— 见 [yi-agent-flags](./yi-agent-flags.md)。`yi-agent-rs/crates/yi-agent-flags/` 是唯一判定点；注册表 `registry.rs` 定义全部 flag（必填 `expires_on`），`flags.json` 存实例级取值（跟 `paths::instance_home()` 走），取值链为「默认 → 文件 → 环境变量 `YI_AGENT_FLAG_*` → 构建类型硬闸」。**非 dev 构建一律全关**，无逃生口。两个入口共用同一份存储：`yi-agent flags list|set|status` 与 RPC `flags/list`、`flags/set`（stable 上返回 `-32601`）；设置页「实验功能」Tab 仅在 `build/info` 报 `dev` 时渲染。判据：`cargo test -p yi-agent-flags`（17 passed）；`yi-agent flags status <name>` 在 dev 报 `source: file`/`default`、在 release 报 `source: build-kind`
- [x] cargo 并发测试锁机制调研与文档化 — 原 CLAUDE.md 断言「不要在多个 shell / worktree 同时跑 `cargo test`，即便 target 目录独立，cargo 对 workspace 元数据文件仍有锁竞争，可能导致编译卡死或 exit 137」，并把瞬时卡顿归因于「僵尸测试二进制进程仍持有 target 目录锁」。结论：**锁确实存在但只在编译阶段**——全局 `~/.cargo/.package-cache(-mutate)` 与单 target 的 `<target>/*/.cargo-lock`，冲突时 cargo 打印 `Blocking waiting for file lock on ...` 并排队而非死锁（实测同 target 并发 4 个 `cargo test` 全部 rc=0，1.0~6.4s 完成）。真正看起来像死锁的只有三种情形：持锁进程被 `SIGSTOP`/Ctrl-Z 暂停（锁随 fd 持有直到进程退出）、前一个 cargo 在跑十几分钟的全量 workspace 编译、多个 worktree 显式共用同一个 `CARGO_TARGET_DIR`。**僵尸测试二进制不持有 `.cargo-lock`**（kill -9 cargo 后残留进程仍在跑，下一个 `cargo test --no-run` 仍 0 秒完成），删 incremental 下的 `*.lock` 也解不开构建锁。已在 `CLAUDE.md` 的「cargo test 执行」补充锁模型表、诊断命令（`ps ... stat` 找 `T` 态进程 + `lsof` 找持锁者）与三种并行方案，并纠正上述两条错误归因。判据：`grep -n "cargo 每次运行" CLAUDE.md` 命中锁模型段落；诊断命令输出与实测一致（`lsof <target>/debug/.cargo-lock` 在测试执行阶段为空）
- [x] `rebuild.sh` 清扫测试残留分两类 — `stop_dependents` 在停生产进程之前先清测试留下的东西，两类
  成因不同、规则也必须不同：**(1) helper 子进程**，即被测代码在 `pre_exec` 里 `setsid()` 出去的进程
  （`crates/yi-agent-supervisors/tests/support/process_fixture.rs`）。它们自成会话，测试二进制退出或被
  Ctrl-C 后收不到任何信号，直接挂到 launchd 名下。匹配条件刻意三条同时成立——路径在
  `target/{debug,release}/deps/`、带 `--exact`、带 `--ignored`——正常构建产物与正在跑的 `cargo test`
  主进程都不匹配。**(2) 孤儿测试二进制本体**，即 `ppid=1` 且名字形如 `deps/<crate>-<16位hex>` 的进程；
  典型成因是某条读路径没超时，服务端不回帧就永远挂着，进程连同端口、临时目录、子进程一起活到下次开机
  （实测一个 `yi_agent_app_server` 测试二进制跑了 3 小时 32 分、CPU 0%、21 个线程，旧规则一条都不匹配）。
  只认 `ppid=1` 是关键：`cargo test` 的子进程父进程是 cargo，终端直接起的是 shell，都不会被选中，
  因此在跑的测试不受影响。两类都先 `SIGCONT` 再 `SIGTERM`——被 `SIGSTOP` 挂起（`stat=T`）的进程收
  SIGTERM 只会把信号挂起，永不退出。**不放宽 `is_target_command`**：那条规则按 basename 精确匹配正是
  为了防误杀，放宽成「路径含 `deps/`」会命中任何在 `target/` 下跑的东西。
  判据：`bash -n scripts/rebuild.sh`；构造 `ppid=1` 的 `deps/yi_agent_fake-<16hex>` 进程后
  `orphaned_test_binary_pids` 能选中并清理，同时 `deps/notatest`（形态不符）被排除
