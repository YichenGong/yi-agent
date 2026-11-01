# 测试与 CI 加固 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把四个「写得多、验得少」的缺口堵上：桌面测试进 CI、Rust 测试迁移到 nextest（进程隔离）、修掉 nextest 暴露出的时间预算型脆弱测试、macOS 专属代码进 PR 门禁。

**Architecture:** 全部围绕现有 `justfile` 作为单一日志入口扩展，不新建构建系统。桌面门禁走独立 CI job（MR 路径用的 `rust:latest` 镜像没有 Node，塞进 `just ci` 会拖垮流水线）；Rust 测试从 `cargo test` 换成 `cargo nextest`，配置放 `yi-agent-rs/.config/nextest.toml`；macOS 门禁单独一个 GitHub job 只跑三个 crate。

**Tech Stack:** just、cargo-nextest 0.9.150、npm / vite 8 / vitest 3.2.7 / typescript 6.0.3、GitHub Actions、GitLab CI。

---

## Global Constraints

以下约束来自 spec，**每个任务隐含包含**，不再逐条重复：

- 工作目录：`.worktrees/docs-test-ci-hardening`，分支 `docs/test-ci-hardening`。不在 `main` 上提交。
- `cargo` 不在默认 PATH。每条 Rust 命令前先：
  `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin"`
- **不要并行跑多个 `cargo`**。仓库里有多个 worktree，别人的编译会抢锁；跑前 `ps aux | grep -v grep | grep -cE "[r]ustc|[c]argo "` 必须为 0。等待时不要空转超过 10 分钟。
- 测试按 crate 跑，不要 `cargo test --workspace` 全量（容易 OOM）。
- 提交前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- 提交信息用 conventional commits 风格，首行 ≤72 字符，**不写 `Co-Authored-By`**。
- 不删除任何测试、不放宽任何断言来换取绿色。测试失败要么修根因，要么记入 `bug-list.md` 后单独处理。
- 每个任务必须可单独 revert。
- nextest 版本钉 **0.9.150**。低于 0.9.115 没有 `inherits`。

### 实测基线（2026-11-01，本 worktree）

写这份计划时全部实跑过，执行者可直接对照，**不必重新试探**：

| 项目 | 实测值 |
|---|---|
| nextest 收集到的测试总数 | `3875`（`cargo nextest list --all-features --workspace \| grep -c "::"`） |
| 全量 nextest 跑（含 27 skipped） | 40~51s，**失败数随负载在 1~4 之间浮动**，非固定值 |
| doctest 总数 | **0**（14 个 crate 全 0，`cargo test --doc` 实测） |
| `cargo test -p yi-agent-supervisors --test supervisor` | `23 passed` |
| `cargo nextest run -p yi-agent-app-server export_bindings` | `18 tests run: 18 passed`，exit 0 |
| 桌面 `npx tsc --noEmit` | exit 0，无输出 |
| 桌面 `npx vitest run` | `140 files / 1856 tests passed`，12.57s |
| JUnit 产物 | `yi-agent-rs/target/nextest/ci/junit.xml`，688KB |

### 已实测确认的 5 个脆弱测试（这是 Task 3 的完整输入）

全量跑时**每次失败组合不同**，说明是负载竞态而非确定性 bug。逐个根因如下：

| # | 测试 | 位置 | 根因（实测栈） |
|---|---|---|---|
| 1 | `yi-agent::bin/yi-agent tests::the_supervisor_loop_starts_a_manifest_when_its_switch_is_on` | `crates/yi-agent/src/main.rs:4257` | `let deadline = Instant::now() + Duration::from_secs(3)` 固定 3s；单跑 0.52s 过，全量 3.03s 超时 |
| 2 | `yi-agent-supervisors::supervisor a_childs_exit_is_recorded_once_not_once_per_tick` | `crates/yi-agent-supervisors/tests/supervisor.rs:780-790` | `for _ in 0..20 { reconcile(); sleep(50ms) }` 固定 1s 观测窗；断言拿到 `exited` 计数 0（应为 1） |
| 3 | `yi-agent-supervisors daemon::tests::a_detached_child_leads_its_own_session_and_runs_in_the_project` | `crates/yi-agent-supervisors/src/daemon.rs:221` | `for _ in 0..100 { ...; sleep(20ms) }` 固定 2s；超时后 `text` 为空，`get()` 里 `unwrap()` 在 `None` 上 panic（`daemon.rs:236`） |
| 4 | `yi-agent-supervisors::supervisor refreshing_a_stale_manifest_makes_switch_off_actually_stop_the_child` | 生产常量 `crates/yi-agent-supervisors/src/refresh.rs:34` | `DESCRIBE_TIMEOUT = 5s`；实测报 `` `describer.sh manifest` did not answer within 5s `` → `RefreshOutcome::Skipped` |
| 5 | `yi-agent-runtime supervise::tests::stopping_clears_the_forwarding_table` | `crates/yi-agent-runtime/src/supervise.rs:350` | `wait_for(&marker, Duration::from_secs(3))` 固定 3s。**写计划时未复现**，按同一类隐患一并处理 |

**注意 #4 是生产代码常量**，修它会改变线上行为，不能像前三个那样只改测试。

---

## File Structure

| 文件 | 职责 | 本次改动 |
|---|---|---|
| `yi-agent-rs/justfile` | 所有构建/测试/门禁 recipe 的单一真源 | 新增 `desktop-check`；`test`/`bindings-check` 换 nextest |
| `yi-agent-rs/.config/nextest.toml` | nextest 仓库级配置 | **新建** |
| `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs` | supervisor 集成测试（23 个） | `wait_until` 改 `FnMut`；修 #2 |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | #1 所在 | 3s 固定预算 → 共享宽预算 |
| `yi-agent-rs/crates/yi-agent-supervisors/src/daemon.rs` | #3 所在 | 2s 轮询 → 宽预算 + 去掉 `unwrap` |
| `yi-agent-rs/crates/yi-agent-supervisors/src/refresh.rs` | #4 的生产常量 | `DESCRIBE_TIMEOUT` 5s → 15s |
| `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs` | #5 所在 | 3s 固定预算 → 宽预算 |
| `yi-agent-rs/crates/yi-agent-mcp/src/manager.rs` | `TEST_CONNECTOR` 静态 | 仅注释（说明 nextest 下无意义） |
| `yi-agent-rs/crates/yi-agent-runtime/src/config.rs` | `ENV_TEST_MUTEX` | 仅注释 |
| `yi-agent-rs/crates/yi-agent/src/config.rs` | `ENV_TEST_MUTEX` | 仅注释 |
| `.github/workflows/ci.yml` | GitHub PR 门禁 | 新增 `desktop` job、`macos` job |
| `.gitlab-ci.yml` | GitLab 门禁 | 新增 `ci-desktop` job（云端 runner，node 镜像） |
| `docs/project-management/ci-cd.md` | 模块进度与判据 | 补本次 5 项 feature |

**依赖关系（务必按此顺序）：**

```
Task 1  桌面摸底复核 ── 独立
Task 2  nextest 配置落地 ── 独立，但 Task 3/4/5 都依赖它
Task 3  修 5 个脆弱测试 ── 依赖 Task 2（要先能稳定复现）
Task 4  headless 端口/临时文件加固 ── 依赖 Task 2
Task 5  justfile 切到 nextest ── 依赖 Task 3（否则门禁一上线就是红的）
Task 6  桌面测试进 CI ── 依赖 Task 1
Task 7  macOS 门禁 ── 依赖 Task 2、Task 5
Task 8  文档与进度收尾 ── 依赖全部
```

---

### Task 1: 复核桌面测试基线（不绿则先修）

写计划时已实测全绿（140 files / 1856 tests / 12.57s）。这个任务只做确认，**任何修复都不许通过 skip 或降级断言完成**。

**Files:**
- 无代码改动（除非失败）
- 可能改：`desktop/src/**/*.test.tsx`

**Interfaces:**
- Consumes: 无
- Produces: 确认过的绿色基线。Task 6 依赖「桌面测试是绿的」这个前提。

- [ ] **Step 1: 安装依赖（用 `npm ci`，不用 `npm install`）**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npm ci
```

Expected: exit 0。`npm ci` 严格按 `package-lock.json` 安装且不改它。

- [ ] **Step 2: 类型检查**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx tsc --noEmit; echo "exit=$?"
```

Expected: `exit=0`，无输出。

- [ ] **Step 3: 跑单元测试**

```bash
cd desktop && export PATH="/opt/homebrew/bin:$PATH" && npx vitest run 2>&1 | tail -6
```

Expected: `Test Files 140 passed (140)`、`Tests 1856 passed (1856)`。

- [ ] **Step 4: 若失败，按根因修**

判定顺序（**不要跳步**）：

1. 读完整报错，确认是断言失败、超时、还是环境缺失（jsdom / `@testing-library`）。
2. 若只是超时：先确认不是机器负载（`ps aux | grep -v grep | grep -c "[n]ode"`）。
3. 修复必须落在测试或被测量代码的真实缺陷上。

**禁止的「修复」方式**（任一条出现即视为本任务失败）：加 `it.skip` / `describe.skip`、放宽断言、调大 `vi.setTimeout` 掩盖死等、删除用例。

- [ ] **Step 5: 记录实测数字**

把 Step 2、3 的实际输出（用例数、耗时）记下来，Task 8 要写进 `ci-cd.md`。

- [ ] **Step 6: 提交（仅当 Step 4 有改动）**

```bash
cd desktop && git add -A src && git commit -m "fix(desktop): 修复阻塞 CI 门禁的测试失败"
```

若 Step 2/3 本来就全绿，**跳过提交**。

---

### Task 2: 落地 `.config/nextest.toml`

只加配置，不改 recipe。目的是先让 nextest 独立跑起来、暴露问题，再决定改哪些测试。

**Files:**
- Create: `yi-agent-rs/.config/nextest.toml`

**Interfaces:**
- Consumes: 无
- Produces: profile `ci`（含 JUnit）。Task 3/4/5/7 通过 `cargo nextest run --profile ci` 使用。JUnit 固定产出在 `yi-agent-rs/target/nextest/ci/junit.xml`。

- [ ] **Step 1: 创建配置文件**

创建 `yi-agent-rs/.config/nextest.toml`：

```toml
# 仓库级 nextest 配置。必须与 workspace 的 Cargo.toml 同级（即 yi-agent-rs/）。
#
# `terminate-after` 的单位是「周期数」，不是秒：period = 60s 且 terminate-after = 3
# 表示第 3 个 60s 周期结束（累计 180s）才终止；第 1、2 个周期只标 SLOW。
# 写成 terminate-after = "180s" 是不行的——它只接受整数。

[profile.default]
slow-timeout = { period = "60s", terminate-after = 3 }
# 显式覆盖内建默认值 true：合并门禁需要知道一次 PR 里到底坏了几个地方，
# 首个失败就停会让修一个推一次、来回排队。本地想要快速失败用 `--fail-fast`。
fail-fast = false

[profile.ci]
# 键名是 inherits（带 s）。已实测：写成 inherit 不报错退出，只在 stderr 打一行
#   warning: ... ignoring unknown configuration key: profile.ci.inherit
# 后果是 slow-timeout 等 default 段配置**静默不继承**（JUnit 仍会生成，因为
# profile.ci.junit 是同级键，不依赖继承）。所以一定要按 Step 2 卡住这个警告。
inherits = "default"
# 刻意不重试：先加重试会训练团队忽略红灯。需要重试的是被显式隔离的测试，
# 那是后续议题。
retries = 0

[profile.ci.junit]
# 相对路径解析到 target/nextest/<profile>/ 下，即 target/nextest/ci/junit.xml。
path = "junit.xml"
```

- [ ] **Step 2: 验证配置能被解析，且没有「未知键」警告**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo nextest show-config test-groups 2>&1 | grep -i "unknown configuration key"; echo "grep-exit=$?"
```

Expected: `grep-exit=1`（即**没有**匹配到）。

若出现 `ignoring unknown configuration key`，说明键名写错了（最常见是 `inherit` 少了 s），必须修到没有为止。**这一步是本任务的核心**，因为该错误不会让命令失败。

- [ ] **Step 3: 跑一次全量，记录失败清单**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" \
  && command -v cargo-nextest >/dev/null || (cd /tmp && curl -sL "https://get.nexte.st/0.9.150/mac" -o nx.tar.gz && mkdir -p nx && tar -xzf nx.tar.gz -C nx)
```

（若本机没装 nextest，上面第二条命令把它装到 `/tmp/nx`，后续 PATH 里带上 `/tmp/nx`。）

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" \
  && cargo nextest run --all-features --workspace --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u
```

Expected：`3875 tests run`，失败数 **1~4 个，且组合每次不同**。写计划时的四轮实测：

| 轮次 | 失败数 | 失败集合（编号见「5 个脆弱测试」表） |
|---|---|---|
| 第 1 轮 | 1 | #2 |
| 第 2 轮 | 4 | #1 #2 #3 #4 |
| 第 3 轮 | 2 | #1 #2 |
| 第 4 轮（单 crate） | 2~3 | #2 #4（#3 在其中一轮出现） |

**这不是本任务的失败**，而是 Task 3 的输入。把实际 FAIL 清单抄进任务记录。若出现上表 5 个之外的测试，**停下来**先判断是不是环境问题（别人的 worktree 在编译），跑前务必确认 `ps aux | grep -v grep | grep -cE "[r]ustc|[c]argo "` 为 0。

- [ ] **Step 4: 确认 JUnit 真的生成了**

```bash
cd yi-agent-rs && ls -l target/nextest/ci/junit.xml && head -2 target/nextest/ci/junit.xml
```

Expected: 文件存在（写计划时 688897 字节），首行 `<?xml version="1.0" encoding="UTF-8"?>`。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && git add .config/nextest.toml && git commit -m "test: 新增 nextest 配置（ci profile + JUnit + 慢测试超时）"
```

---

### Task 3: 修 5 个时间预算型脆弱测试

**这是整个计划最关键、也最容易做错的一步。**

先明确一件事：**不要用 `test-groups` 掩盖它**。实测把 `binary(supervisor)` 整组限成 `max-threads = 1` 之后**仍然失败**（`23 tests run: 22 passed, 1 failed`），因为干扰来自另一个测试二进制。必须修测试本身。

三条硬性要求：

1. **不要新建轮询工具函数。** `supervisor.rs:39` 已有 `wait_until`，`daemon.rs`、`supervise.rs` 各有自己的 `wait_for`。就地复用。
2. **先改 `wait_until` 的签名。** 它现在是 `impl Fn() -> bool`，无法接受需要可变借用 `supervisor` 的闭包（#2 必须这么写）。改成 `impl FnMut() -> bool`。已实测这样可以编译：
   ```rust
   fn wait_until(mut condition: impl FnMut() -> bool, what: &str) -> bool
   ```
   只加 `mut`、只改 `Fn` → `FnMut`，函数体不动。**不改这一步，#2 的修法编译不过。**
3. **#4 是生产常量。** 单独提交，并在 PR 描述里点明行为变化。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs:39`（`wait_until` 签名）
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs:780-790`（#2）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:4257`（#1）
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/daemon.rs:218-240`（#3）
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/refresh.rs:34`（#4）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs:303-315, 350`（#5）

**Interfaces:**
- Consumes: `wait_until`（`supervisor.rs:39`）、`CHILD_START_BUDGET`（`supervisor.rs:35`，20s）、`wait_for`（`daemon.rs` / `supervise.rs` 各自私有）
- Produces: 5 个测试在任何并发度下稳定。Task 5 依赖这一点。

#### 3a. 先改 `wait_until` 签名（一次编译验证）

- [ ] **Step 1: 改签名**

把 `crates/yi-agent-supervisors/tests/supervisor.rs:39`：

```rust
fn wait_until(condition: impl Fn() -> bool, what: &str) -> bool {
```

改为：

```rust
/// `FnMut` 而非 `Fn`：有的调用方要在闭包里驱动状态机（例如反复调用
/// `supervisor.reconcile()` 再读日志），那需要可变借用。
fn wait_until(mut condition: impl FnMut() -> bool, what: &str) -> bool {
```

- [ ] **Step 2: 编译并全 crate 跑，确认签名改动没破坏现有 4 个调用点**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo test -p yi-agent-supervisors --test supervisor --no-run 2>&1 | tail -3
```

Expected: 编译成功（`supervisor.rs` 里 4 处 `wait_until(...)` 都是 `Fn` 闭包，`Fn` 自动满足 `FnMut`，无需改动）。

#### 3b. 修 #2：`a_childs_exit_is_recorded_once_not_once_per_tick`

- [ ] **Step 3: 复现（两个方向都要对上）**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && echo "--- 单跑 ---" && cargo nextest run -p yi-agent-supervisors --profile ci -E 'test(a_childs_exit_is_recorded_once_not_once_per_tick)' 2>&1 | grep -E "Summary" && echo "--- 全 crate ---" && cargo nextest run -p yi-agent-supervisors --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u
```

Expected: 单跑 `1 passed`；全 crate 里这条 FAIL，报 `left: 0, right: 1`，日志内容形如 `"...started demo (pid 63999)\n"`——**只有 started，没有 exited**，证明子进程还没退出观测窗就结束了。

- [ ] **Step 4: 改测试**

`crates/yi-agent-supervisors/tests/supervisor.rs:780-790` 当前形态：

```rust
    let mut supervisor = supervisor_for(workdir, "dev");
    // 20 拍 x 50ms 约 1s，全程 < 60s 退避：同一退出事实被观察约 20 次。
    for _ in 0..20 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(50));
    }

    let log = layout.logs_dir().join("demo.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let count = text.matches("exited demo").count();
```

改为（先轮询到退出被记录，再固定拍数验证不重复）：

```rust
    let mut supervisor = supervisor_for(workdir, "dev");
    let log = layout.logs_dir().join("demo.log");
    // 先轮询到「reconcile 真的观察到了退出事实」，再进入计数阶段。
    // 原写法直接固定 20 拍，隐含「1s 内子进程一定退完」这个负载假设；全量并发下
    // 该假设不成立（实测：单跑 1.1s 通过，全量 1.09s 失败，日志里只有 started），
    // 断言于是拿到 0 次而不是 1 次。轮询消除这个时间假设。
    assert!(
        wait_until(
            || {
                supervisor.reconcile();
                std::fs::read_to_string(&log)
                    .map(|text| text.contains("exited demo"))
                    .unwrap_or(false)
            },
            "the child's exit to be recorded at least once"
        ),
        "the supervisor never recorded the child's exit"
    );

    // 退出已被记录；再拍 20 次，验证它不会被重复写进日志。
    // 这一段保留固定拍数是刻意的：退避是 60s，远大于 20 x 50ms 的墙钟，
    // 所以这里测的是「拍多少次都不会重复记录」，与调度快慢无关。
    for _ in 0..20 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(50));
    }

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let count = text.matches("exited demo").count();
```

**注意闭包里的 `supervisor.reconcile()` 需要 `&mut supervisor`，这正是 Step 1 改 `FnMut` 的原因。**

- [ ] **Step 5: 验证**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent-supervisors --profile ci -E 'binary(supervisor)' 2>&1 | grep -E "Summary"; done
```

Expected: 三次都是 `23 tests run: 23 passed`。**必须跑 `binary(supervisor)` 而不是单条**——单条本来就过，证明不了修复。

#### 3c. 修 #1：`the_supervisor_loop_starts_a_manifest_when_its_switch_is_on`

- [ ] **Step 6: 改固定 3s 预算**

`crates/yi-agent/src/main.rs:4257` 附近当前形态：

```rust
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut started = false;
        while Instant::now() < deadline {
            if marker.exists() {
                started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(started, "the supervisor loop should have started the child");
```

改为：

```rust
        // 宽预算：本测试断言的是「监督器会拉起子进程」，不是「它 3 秒内拉起」。
        // 实测全量并发下这一拍要 3.03s（单跑 0.52s），固定 3s 只是把机器负载
        // 写进了断言。30s 是 backstop，不是期望值——正常情况下 1s 内就返回。
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut started = false;
        while Instant::now() < deadline {
            if marker.exists() {
                started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(started, "the supervisor loop should have started the child");
```

- [ ] **Step 7: 验证**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent --profile ci 2>&1 | grep -E "^     Summary"; done
```

Expected: 三次都是 `829 tests run: 829 passed`（13 skipped）。

#### 3d. 修 #3：`a_detached_child_leads_its_own_session_and_runs_in_the_project`

- [ ] **Step 8: 改轮询预算，并去掉超时后的 `unwrap` panic**

`crates/yi-agent-supervisors/src/daemon.rs:218-240` 当前形态（注意两个缺陷：固定 2s、超时后 `get()` 在空串上 `unwrap()`）：

```rust
        let text = {
            let mut text = String::new();
            for _ in 0..100 {
                text = std::fs::read_to_string(&marker).unwrap_or_default();
                if ["pid=", "cwd=", "pgid="]
                    .iter()
                    .all(|key| text.contains(key))
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            text
        };
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .unwrap()
                .to_string()
        };
```

改为：

```rust
        // 宽预算：100 x 20ms 的 2s 上限把机器负载写进了断言（实测并发下会超）。
        // 20s 是 backstop，不是期望值。
        let budget = std::time::Duration::from_secs(20);
        let deadline = std::time::Instant::now() + budget;
        let mut text = String::new();
        loop {
            text = std::fs::read_to_string(&marker).unwrap_or_default();
            if ["pid=", "cwd=", "pgid="]
                .iter()
                .all(|key| text.contains(key))
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // 超时后不要在各字段的 `unwrap()` 上 panic——那会把「子进程没起来」报
        // 成「Option::unwrap on None」，丢掉真正的原因。先给出可读诊断。
        assert!(
            ["pid=", "cwd=", "pgid="]
                .iter()
                .all(|key| text.contains(key)),
            "the detached child never reported its identity within {budget:?}; \
             marker file held: {text:?}"
        );
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .expect("the diagnostic assert above guarantees every key is present")
                .to_string()
        };
```

确认 `daemon.rs` 里已导入 `Instant`；若没有，用上面写法的全限定路径 `std::time::Instant`（上面已用全限定，无需改 import）。

- [ ] **Step 9: 验证**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent-supervisors --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u; echo "---"; done
```

Expected: 三次都 `87 passed`，无 FAIL。

- [ ] **Step 10: 提交 3a-3d（不含 #4，#4 单独提交）**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo fmt --all && git add -A crates && git commit -m "test: 用条件轮询替换 4 处固定观测窗口，消除负载竞态"
```

#### 3d-补. 把三处进程级静态的注释指向 running-mode 语义（spec 阶段 3c）

这三把锁在 `cargo test` 下是必需的，在 nextest 下语义消失（每个测试独立进程）。代码行为一律不动，**只改注释**，否则下一个人读到这里会以为它们仍然有效而继续依赖它。

- [ ] **Step 10a: `SPAWN_LOCK`（`crates/yi-agent-supervisors/tests/supervisor.rs:18`）**

在 `static SPAWN_LOCK` 上方注释末尾（「CI neighbours).」之后）追加：

```rust
///
/// **两种运行器下的语义不同：**
/// - `cargo test`：同进程内并行，这把锁真的在防「多测试同时 fork 子进程」。
/// - `cargo nextest`：每个测试独立进程，锁跨不过进程边界，等于没有。
///   所以真正托底的是各测试自己的条件轮询 + 宽预算，不是锁。
///   实测（2026-11-01）：`binary(supervisor)` 全组跑在 nextest 下仍有测试
///   超时，锁帮不上忙——别再往这里加锁来「修」并发问题。
```

- [ ] **Step 10b: `ENV_TEST_MUTEX` 两处**

`crates/yi-agent-runtime/src/config.rs:544` 与 `crates/yi-agent/src/config.rs:468`，把

```rust
/// 测试用互斥锁:涉及环境变量的测试必须串行执行,避免并行干扰。
```

改为：

```rust
/// 测试用互斥锁:涉及环境变量的测试必须串行执行,避免并行干扰。
///
/// `cargo nextest` 下每个测试独立进程，环境变量天然不共享，这把锁退化为
/// 无害的空操作；`cargo test` 下它仍是必需的。两种运行器都必须保留。
```

- [ ] **Step 10c: `TEST_CONNECTOR`（`crates/yi-agent-mcp/src/manager.rs:763`）**

在 `pub(crate) static TEST_CONNECTOR` 上方追加：

```rust
/// 进程级 seam。**只在 `cargo test` 下有意义**：nextest 每测试一个进程，
/// 进程级静态天然隔离，不必也争不到锁。保留它是为了让人仍能
/// `cargo test -p yi-agent-mcp --lib` 单跑。
```

- [ ] **Step 10d: 验证只是注释，行为零变化**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && git diff --stat crates/yi-agent-supervisors/tests/supervisor.rs crates/yi-agent-runtime/src/config.rs crates/yi-agent/src/config.rs crates/yi-agent-mcp/src/manager.rs
```

Expected: 四个文件有改动，且 `git diff` 里**只有 `+` 的注释行**，没有任何可执行语句被改。用这条确认：

```bash
cd yi-agent-rs && git diff -U0 crates/ | grep -E "^[+-]" | grep -vE "^[+-]{3}" | grep -vE "^[+-]\s*(//|///)" ; echo "grep-exit=$? (1 = 全是注释)"
```

Expected: `grep-exit=1`。

`SPAWN_LOCK` 的改动并入 Step 10 那次 `test:` 提交；另外三个文件并入 Step 14 之后的下一次提交（或单独一次 `docs:` 注释提交）。

#### 3e. 修 #4：`refresh.rs` 的 `DESCRIBE_TIMEOUT`（生产代码，单独提交）

实测失败信息：`` Skipped { name: "demo", reason: "`.../describer.sh manifest` did not answer within 5s" } ``。

这是**生产常量**：`crates/yi-agent-supervisors/src/refresh.rs:34` `const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);`。插件 `manifest` 子命令在冷进程启动 + 高负载下可以超过 5s，于是校准被判成 `Skipped`，测试拿到的不是 `Refreshed`。

- [ ] **Step 11: 先确认它确实只在这条路径上失败，且单跑能过**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && echo "--- 单跑 ---" && cargo nextest run -p yi-agent-supervisors --profile ci -E 'test(refreshing_a_stale_manifest)' 2>&1 | grep -E "Summary" && echo "--- 相关的 refresh 单元测试 ---" && cargo nextest run -p yi-agent-supervisors --profile ci -E 'test(/refresh/)' 2>&1 | grep -E "Summary"
```

Expected: 都通过。也就是说失败只在并发下出现。

- [ ] **Step 12: 提高超时，并在注释里写清代价**

```rust
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(5);
```

改为：

```rust
/// 等插件 `manifest` 子命令自述的超时。
///
/// 5s 在冷进程启动 + CI 高并发下会被击穿（实测：并发跑 87 个测试时，一个
/// 只做 `cat` 的 shell 脚本也能超过 5s，校准被误判成 `Skipped`）。代价分析：
/// 这个超时只在**插件自己坏掉**时才会走满，走满的结果是回退到磁盘上的旧清单
/// （`RefreshOutcome::Skipped`），不影响 reconcile 循环，也不影响启动延迟之外
/// 的任何路径。因此把它放宽到 15s，用「坏插件多等 10s」换「好插件不被误判」。
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(15);
```

- [ ] **Step 13: 验证（这是本任务唯一一次「改生产代码」，要跑足）**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent-supervisors --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u; echo "---"; done && echo "=== refresh 全模块 ===" && cargo nextest run -p yi-agent-supervisors --profile ci -E 'test(/refresh/)' 2>&1 | grep -E "Summary"
```

Expected: 三次都 `87 passed`；refresh 模块全过。若 `garbage_output_keeps_the_stored_manifest` 这类测试依赖「5s 内失败」的语义而变慢或变红，**停下来**看它是否在断言超时值本身——若在，把断言改成不依赖具体数值（例如只断言 `Skipped`）。

- [ ] **Step 14: 提交**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo fmt --all && git add crates/yi-agent-supervisors/src/refresh.rs && git commit -m "fix(supervisors): 放宽插件自述超时，避免高负载下误判为跳过"
```

#### 3f. 修 #5：`stopping_clears_the_forwarding_table`（写计划时未复现）

- [ ] **Step 15: 先复现它是否仍存在**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent-runtime --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u; echo "---"; done
```

- **若失败**：`crates/yi-agent-runtime/src/supervise.rs:350` 的 `wait_for(&marker, Duration::from_secs(3))` 是同一个模式，把 `3` 提到 20；同文件 `335`、`410`、`443` 的 `from_secs(3)` 一并处理（同一类固定预算）。
- **若不复现**：在同文件这几行加一句注释说明预算含义，并在 `docs/project-management/ci-cd.md` 记「已加固但未复现」。**不要假装它不存在。**

- [ ] **Step 16: 全仓验证（Task 3 的完成判据）**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3 4; do cargo nextest run --all-features --workspace --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u; echo "--- run $i end ---"; done
```

Expected: **四轮全绿**（`3875 tests run: 3875 passed, 27 skipped`）。四轮而不是一轮，是因为原问题的表现就是「每次失败组合不同」；一轮绿说明不了什么。

- [ ] **Step 17: 确认 `cargo test` 路径也没坏**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo test -p yi-agent-supervisors --test supervisor 2>&1 | tail -3 && cargo test -p yi-agent-supervisors --lib 2>&1 | tail -3
```

Expected: `23 passed` / 全部通过。Task 5 之前两条路径都要能走通，方便回退。

---

### Task 4: headless 测试的端口与临时文件加固

**这是加固，不是修 bug。** 现状实测是过的（`cargo nextest run -p yi-agent --profile ci` 全 crate 通过），所以不要声称「修复了失败」。要解决的是两个**潜在**问题：

1. `let port = 9000 + (signal as u16 % 500);` 用信号值推导端口（SIGINT=2 → 9002，SIGTERM=15 → 9015）。这只是「两个信号碰巧不撞」，不是设计保证：换一个信号号、或在同一台机器上并发跑两份测试，就会撞。
2. pid 文件写死 `/tmp/yi-headless-signal-tool-{signal}.pid`。`/tmp` 是全局共享的，同名文件跨测试进程、跨用户复用。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/tests/headless_signal_reap.rs:28`（`spawn_mock_gateway` 签名）
- Modify: `yi-agent-rs/crates/yi-agent/tests/headless_signal_reap.rs:162-167`（端口与 pid 文件）

**Interfaces:**
- Consumes: `std::net::TcpListener`、`tempfile`（已是该 crate 的 dev-dependency，`Cargo.toml:62`）
- Produces: `spawn_mock_gateway(listener: TcpListener, probe_cmd: &str) -> JoinHandle<()>`（**签名改变**，唯一调用方同步改）。`start_run_with_live_tool(port: u16, pid_file: &str)` 签名**不变**。

- [ ] **Step 1: 记录加固前的状态**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo nextest run -p yi-agent --test headless_signal_reap --profile ci 2>&1 | grep -E "^     Summary"
```

Expected: 通过。把它记进任务记录，作为「加固前后功能等价」的对照。

- [ ] **Step 2: 改 `spawn_mock_gateway` 接收已绑定的 listener**

`crates/yi-agent/tests/headless_signal_reap.rs:28` 当前：

```rust
fn spawn_mock_gateway(port: u16, probe_cmd: &str) -> std::thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock gateway");
    let cmd = probe_cmd.to_string();
    std::thread::spawn(move || {
```

改为：

```rust
/// 接收**已绑定**的 listener，不自己 bind。
///
/// 端口由内核分配（调用方传 `127.0.0.1:0` 再读 `local_addr()`），因此不存在
/// 「先算一个端口再抢」的假设：多个测试并行、或同一台机器上跑两份测试，都不会
/// 撞。
fn spawn_mock_gateway(listener: TcpListener, probe_cmd: &str) -> std::thread::JoinHandle<()> {
    let cmd = probe_cmd.to_string();
    std::thread::spawn(move || {
```

闭包里 `listener.incoming()` 那段不动。

- [ ] **Step 3: 改调用侧**

`crates/yi-agent/tests/headless_signal_reap.rs:162-167` 当前：

```rust
fn signal_and_expect_group_reaped(signal: i32) {
    let port = 9000 + (signal as u16 % 500);
    // One pid file per signal so the two tests can run in parallel.
    let pid_file = format!("/tmp/yi-headless-signal-tool-{signal}.pid");
    let cmd = format!("echo $$ > {pid_file}; sleep 60");
    let _gateway = spawn_mock_gateway(port, &cmd);
```

改为：

```rust
fn signal_and_expect_group_reaped(signal: i32) {
    // 端口交给内核分配。原来用 `9000 + signal % 500` 靠「两个信号号恰好不同」
    // 避让，那是巧合不是设计：换信号号或并发跑两份测试就会撞。
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
    let port = listener.local_addr().expect("local addr").port();
    // pid 文件放进进程专属临时目录，不再写死 /tmp（全局共享，同名会互相覆盖）。
    let pid_dir = tempfile::tempdir().expect("tempdir for pid file");
    let pid_file = pid_dir
        .path()
        .join(format!("headless-signal-tool-{signal}.pid"))
        .to_string_lossy()
        .into_owned();
    let cmd = format!("echo $$ > {pid_file}; sleep 60");
    let _gateway = spawn_mock_gateway(listener, &cmd);
```

**注意 `pid_dir` 必须活到测试结束**：它绑在 `signal_and_expect_group_reaped` 的栈上，函数返回时才 drop，正好覆盖整个测试体。不要改成 `tempfile::TempDir::new()` 后 `mem::forget`（那是 `start_run_with_live_tool` 里为 workdir 做的权衡，这里不需要）。

- [ ] **Step 4: 编译验证签名一致**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo test -p yi-agent --test headless_signal_reap --no-run 2>&1 | tail -5
```

Expected: 编译成功，无 `error[E0061]`（参数个数不符）。

- [ ] **Step 5: 并行跑三次**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && for i in 1 2 3; do cargo nextest run -p yi-agent --test headless_signal_reap --profile ci 2>&1 | grep -E "^     Summary|^        FAIL"; done
```

Expected: 三次全过（`2 passed`）。与 Step 1 对照，行为等价。

- [ ] **Step 6: 验证端口确实是内核分配的（不能只看绿灯）**

```bash
cd yi-agent-rs && printf '%s\n' "--- 确认代码里已无硬编码端口推导 ---" && grep -n "9000 +" crates/yi-agent/tests/headless_signal_reap.rs; echo "grep-exit=$? (1 = 已移除)" && printf '%s\n' "--- 确认没有 /tmp 硬编码 ---" && grep -n '"/tmp/' crates/yi-agent/tests/headless_signal_reap.rs; echo "grep-exit=$? (1 = 已移除)"
```

Expected: 两个 `grep-exit=1`。

- [ ] **Step 7: `cargo test` 路径也过**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo test -p yi-agent --test headless_signal_reap 2>&1 | tail -3
```

Expected: `test result: ok. 2 passed`。

- [ ] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/usr/bin:/bin" && cargo fmt --all && git add crates/yi-agent/tests/headless_signal_reap.rs && git commit -m "test: headless 信号测试改用内核分配端口与进程专属临时目录"
```

---

### Task 5: justfile 切到 nextest

**前置条件：Task 3 已完成且四轮全绿。** 否则这一步之后 `just test` 就是红的。

**Files:**
- Modify: `yi-agent-rs/justfile:21-23`（`test`）
- Modify: `yi-agent-rs/justfile:45-49`（`bindings-check`）

**Interfaces:**
- Consumes: `.config/nextest.toml` 的 `ci` profile（Task 2）
- Produces: `test` / `bindings-check` 内部改用 nextest。`ci:` 的依赖列表**不变**（实测 14 项）。

- [ ] **Step 1: 确保 nextest 可用**

```bash
export PATH="$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo nextest --version || (cd /tmp && curl -sL "https://get.nexte.st/0.9.150/mac" -o nx.tar.gz && mkdir -p nx && tar -xzf nx.tar.gz -C nx && /tmp/nx/cargo-nextest --version)
```

Expected: `cargo-nextest 0.9.150`。

- [ ] **Step 2: 改 `test` recipe**

`justfile:21-23` 当前：

```make
# 跑全部测试
test:
    cargo test --all-features --workspace
```

改为：

```make
# 跑全部测试（nextest：每个测试独立进程，一个 panic 不带走同批其他测试，
# 也不会因为共享进程内的静态量互相干扰）。
#
# `--profile ci` 不能省：JUnit 只在被选中的 profile 下生成。
# nextest 不跑 doctest，但本仓库 doctest 数为 0（2026-11-01 实测 14 个 crate 全 0），
# 故无需补 `cargo test --doc`。若将来引入可执行 doctest，必须加回。
test:
    cargo nextest --version
    cargo nextest run --all-features --workspace --profile ci
```

- [ ] **Step 3: 改 `bindings-check` recipe**

`justfile:45-49` 当前：

```make
bindings-check:
    cargo test --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

改为：

```make
bindings-check:
    cargo nextest run --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

`export_bindings` 作为位置参数传给 nextest，匹配的是**测试名子串**，与 `cargo test` 的位置参数语义一致。

- [ ] **Step 4: 验证过滤确实生效（不能只看绿灯）**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo nextest run --all-features -p yi-agent-app-server export_bindings 2>&1 | grep -E "^     Summary"; echo "exit=$?"
```

Expected: `18 tests run: 18 passed`（写计划时实测 18、0.147s）。

补充一条实测事实，用于判断这个 recipe 是否可能假绿：**nextest 在「0 个测试被选中」时返回 exit 4 并打印 `error: no tests to run`**（实测：`-E 'test(nonexistent_zzz)'` → `0 tests run` + exit 4）。所以过滤写错不会被静默放过——`just` 会在第一步就失败。这一条与 `cargo test` 的行为相同。

- [ ] **Step 5: 验证 `just test` 端到端**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && just test 2>&1 | tail -6
```

Expected: 先打印 `cargo-nextest 0.9.150`，然后 `Summary [..] 3875 tests run: 3875 passed, 27 skipped`。

- [ ] **Step 6: 验证 JUnit 产物是这次生成的**

```bash
cd yi-agent-rs && ls -l target/nextest/ci/junit.xml && date
```

Expected: mtime 是刚才。

- [ ] **Step 7: 跑完整 `just ci`**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && just ci 2>&1 | tail -15
```

Expected: 末尾 `CI passed`。

**若某道门禁红了**：先判断是不是 nextest 引起的（对比 `git stash` 前后）。不要为了变绿而跳过门禁。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && git add justfile && git commit -m "ci: test 与 bindings-check 迁移到 nextest"
```

---

### Task 6: 桌面测试进 CI

**Files:**
- Modify: `yi-agent-rs/justfile`（`magpie-agent-check` 之后新增 `desktop-check`）
- Modify: `.github/workflows/ci.yml`（新增 `desktop` job）
- Modify: `.gitlab-ci.yml`（新增 `ci-desktop` job）

**Interfaces:**
- Consumes: Task 1 确认过的绿色基线
- Produces: `just desktop-check`；CI job 名：GitHub `desktop`、GitLab `ci-desktop`

- [ ] **Step 1: 新增 `desktop-check` recipe**

在 `justfile` 的 `magpie-agent-check`（`justfile:42-43`）之后插入：

```make
# 桌面端类型检查 + 单元测试。需要 Node，故**不进** `ci` 依赖列表：
# MR 路径的 `ci-mr` 用的是 `rust:latest` 镜像，没有 Node；塞进去会把整条
# 流水线拖垮。与 `magpie-agent-check` 同类，由独立 job 跑。
#
# 用 `tsc --noEmit` 而不是 `npm run build`：门禁要的是类型正确，产出 `dist/`
# 是发布步骤。用 `npx vitest run` 而不是 `npm test`：避免以后 test 脚本被改成
# watch 模式后门禁挂死。用 `npm ci` 而不是 `npm install`：严格按 lock 安装且不改它。
desktop-check:
    cd ../desktop && npm ci
    cd ../desktop && npx tsc --noEmit
    cd ../desktop && npx vitest run
```

- [ ] **Step 2: 本地验证这条 recipe 能用**

```bash
cd yi-agent-rs && export PATH="/opt/homebrew/bin:$PATH" && just desktop-check 2>&1 | tail -8
```

Expected: tsc 无输出；vitest 末尾 `Test Files 140 passed (140)`、`Tests 1856 passed (1856)`。

- [ ] **Step 3: 加 GitHub job**

在 `.github/workflows/ci.yml` 的 `jobs:` 下、`ci:` job 之后追加（缩进与 `ci:` 同级）：

```yaml
  desktop:
    # 独立 job：`rust:latest` 系镜像没有 Node，桌面测试不能塞进 just ci。
    #
    # Node 22：vite 8 的 engines 是 `^20.19.0 || >=22.12.0`
    # （实测 node_modules/vite/package.json），Node 20 其实也在允许范围内
    # （`"20"` 取 20.x 最新，当日为 v20.20.2）。选 22 的理由是维护活跃度：
    # v20 自 2026-03-24 起未再发版，v22 最新为 v22.23.4（2026-10-20，LTS Jod）。
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Setup Node.js
        uses: actions/setup-node@v4
        with:
          node-version: "22"

      - name: Install just
        uses: extractions/setup-just@v2

      - name: Desktop type check + unit tests
        working-directory: yi-agent-rs
        run: just desktop-check
```

- [ ] **Step 4: 加 GitLab job（云端 runner，不是 Mac mini）**

`.gitlab-ci.yml` 顶部注释写明「MR 走云端 runner(不进 Mac mini,安全考虑)」，且 `ci-mr` 没有 `tags`（落在云端 docker executor）。新 job 必须遵守同一条原则：

在 `ci-mr` job 之后追加：

```yaml
# 桌面端类型检查 + 单元测试。独立 job + node 镜像：MR 路径的 ci-mr 用的是
# rust:latest，没有 Node。与 ci-mr 一样跑在云端 runner（不加 tags，不进
# Mac mini），MR 与 push 两条路径统一走这里，不依赖 runner 上预装 node。
#
# 用 node:22：vite 8 允许 `^20.19.0 || >=22.12.0`，Node 20 也可行，
# 取 22 是因为 v20 已停止发版（最后 v20.20.2 / 2026-03-24），v22 仍是活跃 LTS。
ci-desktop:
  stage: check
  image: node:22
  rules:
    - if: $CI_PIPELINE_SOURCE == "push"
    - if: $CI_PIPELINE_SOURCE == "merge_request_event"
  before_script:
    - apt-get update && apt-get install -y --no-install-recommends curl ca-certificates >/dev/null
    - curl -sSf https://just.systems/install.sh | sh -s -- --to /usr/local/bin
  script:
    - cd yi-agent-rs
    - just desktop-check
```

- [ ] **Step 5: 校验两份 YAML 语法**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && python3 -c "
import yaml
a=yaml.safe_load(open('.github/workflows/ci.yml'))
print('github jobs:', list(a['jobs'].keys()))
print('desktop node:', a['jobs']['desktop']['steps'][1]['with']['node-version'])
b=yaml.safe_load(open('.gitlab-ci.yml'))
print('gitlab jobs:', [k for k in b if not k.startswith('.') and k not in ('stages','variables','cache')])
print('ci-desktop image:', b['ci-desktop']['image'], '| tags:', b['ci-desktop'].get('tags','<none>'))
"
```

Expected: `github jobs:` 含 `ci`、`desktop`；`desktop node: 22`；`gitlab jobs:` 含 `ci-push`、`ci-mr`、`ci-desktop`、`release:macos-dmg`；`ci-desktop image: node:22`、`tags: <none>`。

- [ ] **Step 6: 确认 `just ci` 的 14 道门禁没被改动**

```bash
cd yi-agent-rs && sed -n '/^ci:/p' justfile | tr ' ' '\n' | grep -v '^ci:$' | grep -v '^$' | wc -l
```

Expected: `14`。

- [ ] **Step 7: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && git add yi-agent-rs/justfile .github/workflows/ci.yml .gitlab-ci.yml && git commit -m "ci: 桌面端类型检查与单元测试进流水线（独立 job）"
```

- [ ] **Step 8: 推送后的人工确认（代码做不到）**

分支保护的 required check 必须网页端配置：

- GitHub：Settings → Branches → `main` protection → 勾 `desktop`。
- GitLab：Settings → Merge requests → 确认 `ci-desktop` 参与合并检查。

在执行记录里写明「已配置」或「待所有者配置」。

---

### Task 7: macOS 门禁

**前置条件：Task 2、Task 5 完成。**

**Files:**
- Modify: `.github/workflows/ci.yml`（新增 `macos` job）

**Interfaces:**
- Consumes: `.config/nextest.toml` 的 `ci` profile
- Produces: GitHub job 名 `macos`，只跑 `yi-agent-power`、`yi-agent-tools`、`yi-agent-app-server`

- [ ] **Step 1: 确认三个 crate 的 macOS 代码量**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && for c in yi-agent-power yi-agent-tools yi-agent-app-server; do printf "%-24s %s\n" "$c" "$(grep -rn --include='*.rs' 'cfg(target_os = "macos")' yi-agent-rs/crates/$c | wc -l)"; done
```

Expected: `8 / 6 / 7`，合计 21。

- [ ] **Step 2: 本地先验证这三个 crate 在 macOS 上能跑**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && cargo nextest run --all-features -p yi-agent-power -p yi-agent-tools -p yi-agent-app-server --profile ci 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u
```

Expected: 全部通过、无 FAIL。**先确认「本地绿」，否则新 job 一上线就是红的。**

- [ ] **Step 3: 加 `macos` job**

```yaml
  macos:
    # macOS 专属代码在 PR 阶段就该有信号：21 处 cfg(target_os = "macos")
    # （power 8 / tools 6 / app-server 7）在 ubuntu runner 上根本不会被编译。
    #
    # 镜像选择（2026-11-01 核对 actions/runner-images README）：
    #   - macos-14 已于 2026-11-02 退役（官方 changelog 2026-10-01，issue #13518），不用
    #   - macos-latest 现在指向 macOS 26，会随 GitHub 调整漂移，门禁要用固定标签
    #   - macos-15 是当前可用的 arm64 固定标签，可用
    # 只跑三个 crate：目标是覆盖 macOS 特有路径，不是重复 Linux 已跑过的验证。
    runs-on: macos-15
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust toolchain
        uses: dtolnay/rust-toolchain@stable

      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
        with:
          workspaces: yi-agent-rs

      - name: Install nextest
        uses: taiki-e/install-action@v2
        with:
          tool: nextest@0.9.150

      - name: Run macOS-gated tests
        working-directory: yi-agent-rs
        run: |
          cargo nextest run --all-features \
            -p yi-agent-power -p yi-agent-tools -p yi-agent-app-server \
            --profile ci
```

- [ ] **Step 4: 校验 YAML**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && python3 -c "
import yaml
d=yaml.safe_load(open('.github/workflows/ci.yml'))
print('jobs:', list(d['jobs'].keys()))
print('macos runs-on:', d['jobs']['macos']['runs-on'])
"
```

Expected: jobs 含 `ci`、`desktop`、`macos`；`runs-on: macos-15`。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && git add .github/workflows/ci.yml && git commit -m "ci: 新增 macOS job 覆盖 cfg(target_os = macos) 路径"
```

- [ ] **Step 6: 记录残余风险**

即使跑通，也要在 `ci-cd.md` 写明：`macos-15` 是 GitHub 托管 runner，与真实用户机器（本机 Mac mini）不是同一环境；电源断言这类行为只能靠它给出**信号**，不能替代真机验证。Task 8 落地。

---

### Task 8: 文档与进度收尾

**Files:**
- Modify: `docs/project-management/ci-cd.md`
- Modify: `CLAUDE.md`（门禁数量 12 → 14）
- Modify: `docs/project-management/README.md`（若计数有变）

**Interfaces:**
- Consumes: Task 1-7 的全部实测数字
- Produces: 可追溯的进度记录

- [ ] **Step 1: 更新 `ci-cd.md` 的 Features 段**

在 `## Features` 下追加（每条带可验证判据，这是仓库规范）。**数字必须用你自己跑出来的，不要照抄这里**：

```markdown
- [x] 桌面端类型检查与单元测试进流水线 — `just desktop-check`（`tsc --noEmit` + `vitest run`）；GitHub `desktop` job、GitLab `ci-desktop` job（node:22）各自独立，不进 `just ci`（`rust:latest` 无 Node）。判据：`cd desktop && npx vitest run` 输出 `Tests 1856 passed`；故意改坏一条断言后 push，对应 job 必须红
- [x] Rust 测试迁移到 nextest — `yi-agent-rs/.config/nextest.toml`（profile `ci`，`inherits = "default"`，JUnit 到 `target/nextest/ci/junit.xml`）；`justfile` 的 `test` / `bindings-check` 改调 `cargo nextest`。判据：`just test` 打印 `cargo-nextest 0.9.150` 且 `3875 tests run: 3875 passed`；`ls target/nextest/ci/junit.xml` 存在且 mtime 为本次
- [x] 时间预算型脆弱测试消除 — 4 处固定观测窗口改为条件轮询 / 宽 backstop：`supervisor.rs`（`wait_until` 改 `FnMut`）、`main.rs:4257`、`daemon.rs:221`、`supervise.rs`；另有 `refresh.rs` 的 `DESCRIBE_TIMEOUT` 5s→15s。判据：`cargo nextest run --all-features --workspace --profile ci` **连跑四轮全绿**
- [x] headless 测试端口与临时文件加固 — `headless_signal_reap.rs` 的 `spawn_mock_gateway` 改为接收已绑定 listener（`127.0.0.1:0` 内核分配）；pid 文件挪进 `tempfile::tempdir()`。判据：`grep -n "9000 +" crates/yi-agent/tests/headless_signal_reap.rs` 无输出；`cargo nextest run -p yi-agent --test headless_signal_reap --profile ci` 连跑三次通过
- [x] 进程级静态的语义说明 — `SPAWN_LOCK`、`ENV_TEST_MUTEX`（两处）、`TEST_CONNECTOR` 的注释注明「`cargo test` 下有效、nextest 下跨不过进程边界」。判据：`git diff -U0 crates/ | grep -E "^[+-]" | grep -vE "^[+-]{3}" | grep -vE "^[+-]\s*(//|///)"` 无输出（即只动了注释）
- [x] macOS 门禁 — `.github/workflows/ci.yml` 的 `macos` job，`runs-on: macos-15`，只跑 power/tools/app-server。判据：临时删掉一处 power 的 macOS 断言后该 job 必须红；`macos-14` 已 deprecated（runner-images #13518），不可回退
```

- [ ] **Step 2: 在 `ci-cd.md` 的「不做什么」里补一条**

```markdown
- 不做覆盖率硬阈值 / 变异测试 / 属性测试 / 测试影响分析（理由见 spec 附录 B，已逐条记录）
```

- [ ] **Step 3: 修正 `CLAUDE.md` 的门禁数量**

找到 `## Commit 规范` 里「涵盖 12 道门禁」那段，把 `12 道门禁` 改为 `14 道门禁`，并在句末补：

```
  实际清单以 `yi-agent-rs/justfile` 的 `ci:` 行为准（共 14 项依赖）。
```

- [ ] **Step 4: 检查 `README.md` 的计数**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && grep -n "ci-cd" docs/project-management/README.md
```

若索引表里 `ci-cd` 那行有「完成 / 总计」计数，按新增 6 条同步更新。没有就跳过。

- [ ] **Step 5: 全量验证**

```bash
cd yi-agent-rs && export PATH="/tmp/nx:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/usr/bin:/bin" && just ci 2>&1 | tail -5 && just desktop-check 2>&1 | tail -4
```

Expected: `CI passed` + `Tests 1856 passed`。

- [ ] **Step 6: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/docs-test-ci-hardening && git add docs/project-management CLAUDE.md && git commit -m "docs: 记录测试与 CI 加固的完成项与判据"
```

---

## 验证清单（合并前必须全绿）

```bash
# 1. 配置无未知键（应无输出，grep-exit=1）
cd yi-agent-rs && cargo nextest show-config test-groups 2>&1 | grep -i "unknown configuration key"; echo $?

# 2. Rust 全量测试，连跑 4 轮（原问题是「每次失败组合不同」，一轮不算数）
for i in 1 2 3 4; do cargo nextest run --all-features --workspace --profile ci --no-fail-fast 2>&1 | grep -E "^     Summary|^        FAIL" | sort -u; done

# 3. JUnit 产物存在且是新的
ls -l yi-agent-rs/target/nextest/ci/junit.xml

# 4. 桌面测试
cd desktop && npx tsc --noEmit && npx vitest run

# 5. 完整门禁
cd yi-agent-rs && just ci

# 6. 门禁数量未变（应为 14）
sed -n '/^ci:/p' justfile | tr ' ' '\n' | grep -v '^ci:$' | grep -v '^$' | wc -l
```

**「门禁的价值在于它会红」**：Task 6、Task 7 的完成判据都必须包含一次**故意破坏后变红**的验证，只看到绿灯不算完成。

---

## 风险与应对

| 风险 | 症状 | 应对 |
|---|---|---|
| `inherits` 拼错 | 只打一行 warning 后继续；`default` 段配置静默不继承 | Task 2 Step 2 用 `grep -i "unknown configuration key"` 卡住 |
| 脆弱测试只修了一半 | 单 crate 绿、全仓仍偶发红 | Task 3 Step 16 要求**连跑四轮全绿** |
| `wait_until` 没改 `FnMut` | #2 的修法编译报「cannot borrow as mutable」 | Task 3 Step 1 先改签名再写闭包 |
| `DESCRIBE_TIMEOUT` 改动影响生产 | 坏插件多等 10s 才回退 | 只影响「插件坏了」的路径，`Skipped` 语义不变；Task 3 Step 13 要跑 refresh 全模块 |
| GitLab 新 job 被塞进 Mac mini | 违反「MR 不进 Mac mini」的安全约定 | Task 6 Step 4 用 `image: node:22` 且**不加 `tags`**；Step 5 显式校验 `tags: <none>` |
| Node 版本线老化 | v20 自 2026-03-24 起停止发版（最后 v20.20.2） | 固定 `node-version: "22"` / `image: node:22`；vite 本身 20/22 都支持 |
| 桌面测试在 CI 首次红 | 本地绿、CI 红 | Task 1 的基线只在本地证明；CI 首跑当验证，不绿就修测试而非放宽 |
| `macos-15` 排队久 | PR 等待变长 | 只跑 3 个 crate；实测后再决定是否分片 |
| nextest 版本漂移 | 低版本静默忽略新配置项 | 钉 0.9.150；`test` recipe 第一行打印 `--version` |
| 别人 worktree 在编译 | cargo 抢锁 / 我这边 exit 137 | 跑前 `ps aux \| grep -v grep \| grep -cE "[r]ustc\|[c]argo "` 为 0 |

---

## 明确不做（避免执行者自作主张）

- 不引入 `serial_test`、不引入覆盖率工具、不做变异/属性/快照测试。
- 不用 `test-groups` 掩盖并发问题（实测对跨二进制干扰无效，见 Task 3 开头）。
- 不重构三处进程级静态（`TEST_CONNECTOR` / `ENV_TEST_MUTEX` / `SPAWN_LOCK`）的实现。
- 不改 `test-real-*` 系列 recipe（`--ignored` → `--run-ignored only` 是后续议题）。
- 不加 CI 分片、不加 JUnit 上传步骤（spec 附录 C 第 8 项）。
