# 测试与 CI 加固：堵住「绿着漏」的四个缺口

**状态：** 设计（待评审）
**日期：** 2026-11-01
**范围：** 桌面测试进 CI、nextest 迁移、macOS 门禁、隔离规则落文件

> **修订记录**
>
> **第一轮（初稿 → 二次核查）**：修正 6 处事实错误。关键两条：
> 1. 初稿称「doctest 会静默丢失、需单独保留」——**实测全仓 doctest 为 0**，该缺口不存在，相应阶段已删除。
> 2. 初稿用 `macos-14` runner——**该镜像于 2026-11-02（明日）退役**，已改为 `macos-15`。
>
> **第二轮（三轮复核，逐条回源验证）**：又修正 6 处。其中三条会让配置**静默失效**，是本轮最大收获：
> 1. `nextest.toml` 的继承键写成 `inherit`，正确拼写是 **`inherits`**（官方 reference 与 `default-config.toml` 双确认）。写错不会报错，整段 profile 配置被忽略。
> 2. 声称「`junit.path` 写绝对路径会双重拼接」——**源码是 `store_dir.join(path)`，`Path::join` 遇绝对路径直接替换，不会拼接**。已用 `rustc` 实测确认。
> 3. `terminate-after` 单位是**周期数**不是秒：`60s × 3` 是累计 180s 才终止，此前表述含糊。
>
> 另有几处计数/描述偏差：`TempDir` 97/93→**99/96**；文档代码块「12 处、只扫 `///`」→**6 块 / 12 行，含 3 个 `//!` 块**（只 grep `///` 会漏一半）；`--test-threads=1` 原写「无依赖全局串行」，实际有 5 处且全在 `test-real-*`（目的是限流）。
>
> **关于测量基准的一次自我纠正**：复核中段我曾以主仓库工作区为基准测量，得出「桌面测试 146 文件」「`rpc_tests.rs` 21277 行」等结论——那是错的。主仓库 `main` 上当时并行存在 thread-fork 的未提交改动（`git status` 有 `M`/`UU`），同一文件两次测得 21272 / 21434。**已全部回退，并在本 worktree 的 `df3cf1f2` 干净基线上重测**：140 文件 / 1851 声明 + `it.each`×8 ≈ 1859 用例、21272 行、10945 行。文末 #20 记录了这次教训。
>
> **第三轮（写 implementation plan 时逐条实跑）**：以「计划里每条命令都要能真跑」为标准复核，又修正 3 处，并**撤回了我自己在第三轮开头做出的 2 处误判**：
> 1. 阶段 3a-补 的失败清单从「3 个」改为「**5 个**」——连跑四轮，失败数在 1/4/2 之间浮动、失败集合每次不同，新增 `the_supervisor_loop_starts_a_manifest_when_its_switch_is_on`（`main.rs:4266` 固定 3s）与 `a_detached_child_leads_its_own_session_and_runs_in_the_project`（`daemon.rs:221` 固定 2s + 超时后 `unwrap` panic）。阶段 3 的完成判据因此改为**连跑四轮全绿**。
> 2. `refreshing_a_stale_manifest_...` 的 5s 超时不是测试侧问题，而是**生产常量** `DESCRIBE_TIMEOUT`（`refresh.rs:34`），必须单列一次「改生产代码」的提交并附代价分析。
> 3. `wait_until(impl Fn)` 包不住 `supervisor.reconcile()`（需要 `&mut`），签名须先改 `impl FnMut() -> bool`。
>
> 两处**误判的撤回**（原文已改回，教训记在 #21 / #24）：
> - 我曾因 `runner-images` README 只有 deprecated 徽章而断定「macOS 14 无退役日期」——回查附录 A 的 changelog 原文，`"will be retired on November 2, 2026"` 白纸黑字。**README 没写日期不等于没有出处。**
> - 我曾称「`node-version: "20"` 会让 vite 8 拒绝启动」——vite 的 engines 是 `^20.19.0 || >=22.12.0`，20 在允许范围内，`"20"` 取 20.x 最新（当日 v20.20.2）。选 Node 22 的真实理由是 v20 自 2026-03-24 起停止发版，而非「20 会挂」。
>
> **第四轮（合入 main 前重测，main `03149e99`）**：本分支基点 `32ec0db0` 之后 main 前进 62 个提交，把基线刷新后重测，**结论全部维持**，但所有绝对数字变化：
> - nextest 收集数 **3875 → 3954**；单轮实跑 `3953 passed, 1 failed`（失败的仍是 `a_childs_exit_is_recorded_once_not_once_per_tick`），5 个 flaky 测试**全部仍存在**，固定预算也一个没被改（`main.rs:4266` 的 3s、`supervisor.rs:785` 的 20 拍、`daemon.rs:221` 的 100×20ms、`refresh.rs:34` 的 5s）。
> - 桌面 **140 文件/1856 用例 → 146 文件/1932 用例**（main 新增了测试）。
> - `yi-agent` crate **829 → 865**；`yi-agent-supervisors` 仍 **87**；`export_bindings` 仍 **18**；power+tools+app-server 实测 **1520 passed / 7 skipped**。
> - 行号随之偏移，已逐条回源修正（`main.rs:4257→4266`、`supervisor.rs:780-790→784-789`、`supervisor.rs:35→36`、`daemon.rs:218-240→219-238`、`config.rs:544→554`、`ws.rs:605→609`、`supervisor.rs:37-44→36-39`）。
>
> 全部核查过程与依据见文末「核查记录」。

---

## 问题

`just ci` 是唯一的 Rust 侧 CI 入口，14 道门禁全绿才允许合并（见 `yi-agent-rs/justfile:36`；`CLAUDE.md` 写的「12 道」是旧的，实际是 14 道）。但当前有四个「写得多、验得少」的缝隙。

### 缺口 1：桌面端测试不在任何 CI 里

`desktop/src` 下有 146 个 `*.test.ts*`，运行时 **1932 条用例**（`npx vitest run` 实测，12.47s；main `03149e99` 基线），`desktop/package.json:10` 有 `"test": "vitest run"`。但：

- `yi-agent-rs/justfile:36` 的 `ci:` 依赖列表里没有它；
- `.github/workflows/ci.yml`、`.gitlab-ci.yml` 只调 `just ci`；
- 三个文件里 `vitest` / `npm test` 零命中。

结果：前端测试写得再多，删掉也没人知道。类型检查同样缺席——`tsc` 只在 `npm run build` 里跑，不在门禁内。

### 缺口 2：测试运行器共享进程

`justfile:22-23` 是 `cargo test --all-features --workspace`，进程内多线程。共享状态有三处进程级静态：

| 静态 | 位置 | 保护方式 |
|---|---|---|
| `TEST_CONNECTOR` | `yi-agent-mcp/src/manager.rs:763` | `seam_guard()` 手动持锁到 await 之后 |
| `ENV_TEST_MUTEX` | `yi-agent-runtime/src/config.rs:554` | 42 处 `lock()` 调用 |
| `SPAWN_LOCK` | `yi-agent-supervisors/tests/supervisor.rs:18` | 起子进程的测试串行 |

这些锁是有效补丁，但**约束只存在于代码里**，没有变成仓库规则；且 `cargo test` 下一个测试 panic 会带走同进程的其他测试。

已发生四次竞态相关修复（提交哈希均已核对）：

- `4ed7b410` — mcp seam 测试互抢 `TEST_CONNECTOR`
- `022a6d50` — second_turn 测试把通知当响应
- `d1313805` — 停用插件用例的竞态
- `32ec0db0` — session_store 重开测试的 `flock` 释放时序偶发

### 缺口 3：macOS 专属代码在 PR 路径上没有验证

21 处 `#[cfg(target_os = "macos")]`：

| crate | 数量 |
|---|---|
| `yi-agent-power` | 8 |
| `yi-agent-app-server` | 7 |
| `yi-agent-tools` | 6 |

而 `.github/workflows/ci.yml:11` 是 `runs-on: ubuntu-latest`，只有 push 到 GitLab 的 Mac mini 才在 macOS 上跑。**一个改坏 power 或 sandbox 的 PR 在 GitHub 上是绿的，合进 `main` 之后才暴露。**

### 缺口 4：一处靠巧合避让的固定端口

`yi-agent/tests/headless_signal_reap.rs:163`：

```rust
let port = 9000 + (signal as u16 % 500);
let pid_file = format!("/tmp/yi-headless-signal-tool-{signal}.pid");
```

两个测试用 SIGINT(2) 和 SIGTERM(15)，端口是 9002 和 9015。当前靠「端口恰好不同」避让，pid 文件则都写在固定的 `/tmp` 下。任何第三个信号测试或端口复用都会撞车。

这是下一阶段（isolation）需要显式处理的一处，不能继续靠巧合。

## 目标

1. 前端测试与 Rust 测试一起进入合并门禁，任一失败都阻塞合并。
2. 测试间不再共享进程级可变状态；崩溃的测试不带走同批其他测试。
3. macOS 专属代码在 PR 阶段就有信号。
4. 已识别的共享状态（三处静态 + 一处固定端口）能对应到可执行的规则。

## 非目标

- 不引入覆盖率门禁、变异测试、属性测试、快照测试。
- 不做测试影响分析。
- 不改动任何现有测试的断言内容。
- 不重构测试以消除进程级静态（那是另一个量级的改动）。
- 不补 doctest（现状为 0，补 doctest 是独立议题，见附录 C）。

## 总体设计

四个阶段，按依赖顺序：

```
阶段 1  桌面测试进 CI           （独立）
阶段 2  nextest 迁移            （独立；阶段 3 依赖它）
阶段 3  隔离规则落文件           （依赖阶段 2）
阶段 4  macOS 门禁              （独立）
```

**核心约束：每一步都必须可回退。** 四个阶段都不删除测试、不改断言，任一阶段出问题都可单独 revert。

---

## 阶段 1：桌面测试进 CI

### 设计

新增 `yi-agent-rs/justfile` recipe：

```make
# 桌面端类型检查 + 单元测试。需要 Node，故不进 `ci` 依赖列表
# （GitLab MR 的 rust:latest 镜像没有 Node，见 `magpie-agent-check` 的同类说明）。
desktop-check:
    cd ../desktop && npm ci && npx tsc --noEmit && npx vitest run
```

三个决定的理由：

- **用 `tsc` 而非 `npm run build`**：门禁的目标是类型正确。`desktop/tsconfig.json` 已设 `"noEmit": true`，所以 `tsc` 本身不产出文件；产出 `dist/` 的是 `vite build`，那属于发布步骤。`--noEmit` 是显式冗余声明，保留是为了让读者一眼看出意图。
- **用 `npx vitest run` 而非 `npm test`**：两者等价（`package.json:10` 就是 `vitest run`），但显式写出来避免以后 `test` 脚本被改成 watch 模式时门禁静默挂死。
- **`npm ci` 而非 `npm install`**：`npm ci` 严格按 `desktop/package-lock.json`（216 KB，已提交）安装且不修改它。门禁不该有写仓库的能力。

命令注释里写「不进 `ci`」而不是「不进 CI」：`just ci` 是 Rust 侧 recipe，桌面测试仍要进**流水线**，只是走独立 job。

### 接入方式

**不放进 `just ci`。** 依据是本仓库已有的先例（`justfile:38-41`）：`magpie-agent-check` 刻意不进 `ci`，因为 GitLab MR 的 `rust:latest` 镜像没有 Node，塞进去会拖垮整条流水线。桌面测试同理。

改为独立门禁：

- `.github/workflows/ci.yml`：新增 `desktop` job，`runs-on: ubuntu-latest`，`actions/setup-node@v4` + `node-version: "22"`（现有 `magpie-agent.yml:34/49`、`release.yml:174` 用的是 `"20"`，那是 20.x 最新即 v20.20.2、仍满足 vite 的 `^20.19.0`；此处取 22 是因为 v20 自 2026-03-24 起未再发版，而 v22 仍在发 LTS），执行 `desktop-check` 的三条命令。
- `.gitlab-ci.yml`：新增 `ci-desktop` job，`image: node:22`。注意 MR 路径的现有 job `ci-mr` 用的是 `image: rust:latest`（`.gitlab-ci.yml` 的 `ci-mr` 段），这个镜像没有 Node——这是「不能把桌面测试塞进 `just ci`」的**确凿依据**，不只是类比 `magpie-agent-check`。**MR 与 push 两条路径都走这个镜像**，不要依赖 Mac mini 自带 Node——`release-dmg.sh:26-29` 明确对 `node npm jq shasum` 做前置检查并提示「runner 上缺 node 时请执行 brew install node jq」，说明 Mac mini 的 Node 是**人工保证**而非声明式保证。

两条都是**合并门禁的一部分**——分支保护里必须把它们列为 required check，否则等于没有。

### 前置摸底（必须先做）

`desktop/vite.config.ts` 里没有 `test` 段，测试文件靠 `/** @vitest-environment jsdom */` 文件级注释声明环境（见 `desktop/src/App.test.tsx:1`）。

**已实测（2026-11-01，`npm ci` 后）：**

```
$ npx tsc --noEmit      -> exit 0（无输出）
$ npx vitest run        -> Test Files 140 passed (140)
                           Tests 1932 passed (1932)
                           Duration 11.66s
```

**结论：桌面测试当前全绿，阶段 1 没有前置修复任务。** 用例数以运行时 1932 为准（静态数 `it()` 是一个近似值，`it.each` 与参数化会改变实际条数）。

若将来因新增改动变红：修复测试是阶段 1 的前置任务。**不能通过放宽门禁（skip 用例、降级断言）来让它变绿**，否则这次改造就变成了给已知问题盖盖子。

---

## 阶段 2：nextest 迁移

### 设计

```make
# 跑全部测试。`--profile ci` 不能省：JUnit 只在被选中的 profile 下生成。
# nextest 不跑 doctest，但本仓库 doctest 数为 0（已实测），故无需补第二条命令。
test:
    cargo nextest --version
    cargo nextest run --all-features --workspace --profile ci

# 配置漂移门禁。nextest 的 filterset 中 `test()` 默认是「包含」匹配，
# 与 cargo test 的位置参数子串语义一致，故此处写法不变。
bindings-check:
    cargo nextest run --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

新增 `.config/nextest.toml`（workspace 根，即 `yi-agent-rs/.config/nextest.toml`）：

```toml
# `terminate-after` 的单位是「周期数」，不是秒：period=60s 且 terminate-after=3
# 表示第 3 个 60s 周期结束（即累计 180s）才终止，第 1、2 个周期只标 SLOW。
[profile.default]
slow-timeout = { period = "60s", terminate-after = 3 }
fail-fast = false

[profile.ci]
inherits = "default"
retries = 0

[profile.ci.junit]
# 相对路径会落到 target/nextest/ci/junit.xml（nextest 自动按 profile 分组）。
path = "junit.xml"
```

五处必须注意的地方，全部已对照官方文档核实：

1. **`--profile ci` 是必需的。** 官方原文：「If `--profile ci` is selected on the command line, a JUnit report will be written out to `target/nextest/ci/junit.xml`」。不传这个参数，`[profile.ci]` 整段不生效，JUnit 静默不生成。
2. **`path` 写相对路径 `"junit.xml"`。** 源码里是 `path: store_dir.join(path)`（`nextest-runner/src/config/elements/junit.rs:47`），而 `store_dir = workspace_root/store.dir/<profile-name>`（`config/core/imp.rs:824-825`），因此相对路径落到 `target/nextest/ci/junit.xml`。若写成绝对路径，`Path::join` 会**直接替换**掉 `store_dir`（不是报错、也不是双重拼接），报告会跑到指定位置——可用但会让 CI 上传路径依赖平台，故不采用。
3. **`retries = 0` 是刻意决定。** 多份来源一致认为「先加重试」会训练团队忽略红灯（附录 A）。需要重试的只有被显式隔离的测试——那是后续议题，不在本阶段。
4. **`fail-fast = false` 是显式覆盖默认值。** nextest 内建默认是 `true`（`default-config.toml:58`），即首个失败后不再启动新测试。合并门禁要知道**一次 PR 里到底坏了几个地方**，否则修一个推一次、来回排队。本地想要快速失败可用 `--fail-fast` / `--ff`（0.9.92+ 才有，故钉版本时不能低于它）。若日后觉得全量失败太吵，改成 `fail-fast = { max-fail = N }` 即可，不必再加配置层。
5. **`cargo nextest --version` 单独一行。** 新版配置项在旧版 nextest 上会被**静默忽略**（附录 A），把版本打到日志里，失败时能第一时间排除工具链问题。

### 去掉 doctest 保护的前提

初稿曾要求「`test` recipe 里补一条 `cargo test --doc` 兜底」。实测推翻了这个前提：

```
$ cargo test --all-features --workspace --doc
   Doc-tests yi_agent_app_server
running 0 tests
   ... （14 个 crate 全部 running 0 tests，总耗时 2.89s）
```

**全仓 doctest 数为 0。** 原因：文档里共 6 个代码块、12 行围栏，语言标记只有 `text`(3)、`jsonc`(2)、`json`(1)——没有一个是 `rust` 或无标记，而 rustdoc 只把 `rust` 或无标记的围栏当作可执行示例。注意必须同时扫 `///` 和 `//!`：`///` 有 3 块（`protocol.rs:359`、`ws.rs:339`、`openai/error.rs:40`），`//!` 另有 3 块（`group_store.rs:5`、`relay/tests/bridge.rs:7`、`relay/src/client.rs:6`），只 grep `///` 会漏掉一半。

所以不需要第二条命令。**如果将来引入了可执行 doctest，必须重新加回这一条**；届时也可以在 `nextest.toml` 里加注释提醒。

### `--ignored` 的等价写法（已核实）

现有 `just test-real-*` 用 `cargo test ... -- --ignored`。nextest 的等价写法是 `--run-ignored only`。

**本阶段不改这些 recipe。** 真实 LLM 测试不进 CI，改与不改都不影响门禁；等阶段 3 定了隔离规则再一起处理，避免一次改动波及太多面。

### 迁移风险与对策

| 风险 | 对策 |
|---|---|
| `bindings-check` 的过滤语义变化导致漏跑 | 迁移后改一处 `protocol.rs`，确认生成物真的被重写（`git status` 有 diff 才算跑过） |
| nextest 版本过旧导致配置被静默忽略 | recipe 里先 `--version`；CI 安装时钉一个明确版本 |
| 迁移后首次出现「Linux 上通过、nextest 下失败」 | 这通常说明测试间有隐性共享状态，是被隔离暴露出来的真实问题，不是迁移引入的 bug。记录后单独修 |
| 依赖 `--test-threads=1` 的测试跑法变了 | 实际有 5 处（`justfile:129/132/146/159/167`），**全部在 `test-real-*`**，且目的是给真实 LLM API 限流、不是规避共享状态。这些 recipe 不进 CI，本阶段不动；nextest 侧对应 `-j 1`，迁移时随 `--run-ignored only` 一起改 |

### 安装方式

CI 里安装 nextest 用 `taiki-e/install-action@v2`（`tool: nextest`），比 `cargo install --locked` 快一个数量级。版本显式钉住，不跟随「最新」。

---

## 阶段 3：隔离规则落文件

### 现状核实

我逐项核对了实际的共享状态，结论比最初设想的乐观：

| 资源 | 实际状况 | 是否需要 test-groups |
|---|---|---|
| TCP 端口 | 15 处已绑定 `127.0.0.1:0`（`relay/tests/bridge.rs:70`、`llm/tests/integration.rs:638`、`app-server/src/ws.rs:609` 等） | 否 |
| daemon socket | 每个测试一个 `TempDir`（`runtime_ipc.rs` 99 处、`runtime_coordinator.rs` 96 处） | 否 |
| sqlite | 每 daemon 一条连接 + `Mutex`，文件名由 `TempDir` 决定 | 否 |
| **`headless_signal_reap` 的端口 + /tmp pid 文件** | **固定值，靠信号不同这份巧合避让** | **是** |
| `TEST_CONNECTOR` | 进程级静态 | 进程隔离后自然消失，锁保留兜底 |
| `ENV_TEST_MUTEX` | 进程级静态 | 同上 |
| `SPAWN_LOCK` | 进程级静态 | 同上 |

**这个结论改变了设计。** 原先设想「用 test-groups 串行一大类测试」，实际只有一处真正需要。所以阶段 3 的重点不是大规模分组，而是：

1. **修掉那一处固定端口**（首选）；
2. 为无法改成动态分配的那一类显式声明分组（兜底）；
3. 把三处静态的约束写进注释，指向配置文件。

### 3a：修掉固定端口（首选方案）

nextest 官方对端口冲突有明确建议：**不要用 test-groups 解决，应该绑定端口 0 让内核分配**。理由是 test-groups 只是逻辑互斥，仍可能和系统上已有的服务撞车，而且会让测试变慢。

好消息是改造量很小。`spawn_mock_gateway`（`headless_signal_reap.rs:28`）目前是：

```rust
fn spawn_mock_gateway(port: u16, probe_cmd: &str) -> std::thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock gateway");
    ...
}
```

它**自己持有 listener**，所以不需要 `drop` 再重绑，也不存在 TOCTOU 窗口。改成让调用方传入已绑定的 listener、由 gateway 返回实际端口即可：

```rust
// 内核分配端口；listener 直接移交给 gateway，中间不存在「先释放再重绑」的窗口。
fn spawn_mock_gateway(listener: TcpListener, probe_cmd: &str) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || { for stream in listener.incoming() { ... } })
}

// 调用侧
let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock gateway");
let port = listener.local_addr().expect("local addr").port();
let _gateway = spawn_mock_gateway(listener, &cmd);
```

另一处要改的是 pid 文件：**挪进 `TempDir`，不要写死 `/tmp`**。注意现有代码有个取舍值得保留——`start_run_with_live_tool` 用 `std::mem::forget(workdir)` 让 `--workdir` 目录活到进程结束。pid 文件的 TempDir 如果用同样的技巧，会泄漏目录；更简单的做法是把 pid 文件放进同一目录、同样 forget，或直接沿用现有的 per-signal 命名但换成 `std::env::temp_dir()` 加进程 id 前缀。

### 3a-补：实测发现的真实阻塞（2026-11-01，nextest 0.9.150）

首次全量 `cargo nextest run --all-features --workspace --profile ci` 结果（**历史记录**，基线 `df3cf1f2`；合入前在 main `03149e99` 上重测为 3954 个测试、单轮 1 failed，见修订记录第四轮）：

```
Summary [46.968s] 3875 tests run: 3872 passed, 3 failed, 27 skipped
FAIL yi-agent-runtime  supervise::tests::stopping_clears_the_forwarding_table
FAIL yi-agent-supervisors::supervisor a_childs_exit_is_recorded_once_not_once_per_tick
FAIL yi-agent-supervisors::supervisor refreshing_a_stale_manifest_makes_switch_off_actually_stop_the_child
```

复跑后只有 `a_childs_exit_is_recorded_once_not_once_per_tick` **稳定失败**（连续两次全量复现，1.091s 位置一致）。逐层定位：

| 运行方式 | 结果 |
|---|---|
| `cargo test -p yi-agent-supervisors --test supervisor` | **23/23 通过** |
| `cargo nextest run -p yi-agent-supervisors --test supervisor`（只集成测试） | **23/23 通过** |
| `cargo nextest run -p yi-agent-supervisors --lib`（只单元测试） | **64/64 通过** |
| `cargo nextest run -p yi-agent-supervisors`（两个二进制一起） | **86/87，稳定失败** |
| 同上 + `-j 1` | **87/87 通过** |
| 同上 + `test-groups` 限 `binary(supervisor)` 为 max-threads=1 | **仍失败**（配置已确认生效，`show-config test-groups` 列出了 23 个成员） |

**结论（推翻 3b 兜底方案）：**

1. **`test-groups` 解决不了这一类问题。** 它只能限制**同组内**的并发；失败测试被跨二进制并发拖慢，同一个二进制内的并发并不是原因。实测：把 `binary(supervisor)` 整组限成 1 仍然失败，说明干扰来自**另一个测试二进制**（`--lib` 的 64 个单元测试）。
2. **`SPAWN_LOCK` 在 nextest 下双重失效。** 它是进程级静态，nextest 进程隔离后本就无法跨测试生效；而原本它保护的正是「多测试同时 fork 子进程导致调度延迟」。所以这不是锁写错了，而是**该锁的语义在 nextest 下不存在**——`cargo test` 靠它变稳，nextest 下无人接替。
3. **失败测试是时间预算型，不是逻辑错误。** 它写死「20 拍 × 50ms ≈ 1s」的观测窗口，断言日志里 `exited demo` 命中 1 次；高负载下子进程尚未退出，断言得到 0。单独跑 1.08s 稳定通过，全量跑 1.09s 就失败——差在负载，不在逻辑。

**因此阶段 3 的首要工作从「修端口」改为「修这个测试的时间预算」**：把固定 `20 × 50ms` 观测窗口改为 `wait_until` 轮询（该文件已有现成的 `wait_until` + 20s 预算约定，见 `supervisor.rs:36-39`）。这样它在任何运行器、任何并发度下都稳定，而不是靠 test-groups 掩盖。

**待办真伪确认：** 另两个失败（`stopping_clears_the_forwarding_table`、`refreshing_a_stale_manifest_makes_switch_off_actually_stop_the_child`）只出现一次、复跑即过，属于同类时间预算问题，一并纳入同一修法排查。

### 3b：为剩余场景显式分组（兜底）

如果 3a 因接口限制无法完成（**注意：实测已证明 test-groups 不能解决「跨二进制并发拖慢」这一类**，见 3a-补。它只对「同组内进程互斥」有效，仍适用于真的共享固定端口的场景）：

```toml
[test-groups]
port-bound = { max-threads = 1 }

[[profile.default.overrides]]
filter = 'test(headless_signal_reap::)'
test-group = "port-bound"

[[profile.ci.overrides]]
filter = 'test(headless_signal_reap::)'
test-group = "port-bound"
```

`[profile.*]` 两段都要写：`inherits` 是否传递 overrides 取决于 nextest 版本，显式重复没有代价。

### 3c：三处静态的处理

nextest 是 process-per-test，**每个测试独立进程，进程级静态天然隔离**：

- `TEST_CONNECTOR`、`ENV_TEST_MUTEX` 在 nextest 下失去存在的必要（它们防的是**进程内**争用）；
- 但 `cargo test` 仍要能跑（本地方便单跑某个测试），锁必须保留。

**一处必须修正的认知：`SPAWN_LOCK` 不属于「自然消失」那一类。** 实测（见 3a-补）显示，它防的是「多个测试同时 fork 子进程导致调度延迟」，nextest 下锁失效后这个压力依然存在，只是换成跨进程并发。所以它对应的测试必须改用**时间预算**兜底（`wait_until` 轮询），不能指望进程隔离让它变稳。这是唯一一个「进程隔离反而暴露问题」的静态。

所以设计是**测试自身健壮性为主防线，锁只在 `cargo test` 下兜底**。把三处的注释升级为指向配置文件，例如：

```rust
/// 进程级 seam，碰它的测试必须拿 `seam_guard()`。
/// nextest 下测试进程独立，争用自然消失（见 `.config/nextest.toml`）；
/// `cargo test` 下仍靠这把锁，两种运行器都必须安全。
static TEST_CONNECTOR: Mutex<Option<TestConnector>> = Mutex::new(None);
```

### 不做什么

- **不引入 `serial_test` crate**。nextest 官方明确说明它不支持进程内互斥（因为 process-per-test），应改用 test-groups。加依赖是重复。
- **不试图消除 `ENV_TEST_MUTEX`**。`config.rs` 里 42 处 `.lock()` 对应的测试必须改环境变量，串行就是这类测试的正确做法，重构为依赖注入是另一个量级的改动。

---

## 阶段 4：macOS 门禁

### 设计

`.github/workflows/ci.yml` 新增 job：

```yaml
  macos:
    runs-on: macos-15          # Apple Silicon；macos-14 已于 2026-11-02 退役
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with: { workspaces: yi-agent-rs }
      - uses: taiki-e/install-action@v2
        with: { tool: nextest }
      - name: Run macOS-gated tests
        working-directory: yi-agent-rs
        run: |
          cargo nextest run --all-features \
            -p yi-agent-power -p yi-agent-tools -p yi-agent-app-server
```

四个决定：

- **必须用 `macos-15`，不能用 `macos-14`。** GitHub 宣布 `macos-14` / `macos-14-large` / `macos-14-xlarge` 于 **2026-11-02 退役**（本文写作时为 2026-11-01），10 月已有八次 brownout 让相关 job 直接失败。迁移目标标签是 `macos-15` 或 `macos-latest`（当前指向 `macos-26`）。**不要写 `macos-latest`**，它的指向会随 GitHub 调整而变，门禁环境应当稳定。
- **只跑三个 crate**：目标是覆盖 21 处 `cfg(target_os = "macos")` 路径，不是重复 Linux 已跑过的验证。全量跑会让 PR 等待时间翻倍。
- **用 GitHub 托管 runner 而非现有 Mac mini**：`ci.yml:3` 的注释写明「PR 触发,跑在云端 ubuntu runner(不进 Mac mini,安全考虑)」。引 Mac mini 跑 PR 会破坏这条边界。
- **不用 `--partition`**：三个 crate 的测试量不大，先观察实际耗时再决定。

### 降级选项（不推荐，但不该不写）

如果 macOS runner 排期或成本不可接受，退一步方案是在 ubuntu 上做交叉编译检查：

```bash
cargo check --target aarch64-apple-darwin -p yi-agent-power -p yi-agent-tools -p yi-agent-app-server
```

**这只能验证编译通过，不能验证行为。** `yi-agent-power` 的 8 处 macOS 代码实际调用电源断言，行为测试才有价值。所以交叉编译只应作为降级，且要在 `docs/project-management/ci-cd.md` 里如实记录残余风险。

---

## 验证方式

每个阶段的「完成」都必须有可执行判据。

| 阶段 | 判据 |
|---|---|
| 1 | 故意改坏一个前端测试的断言并推 PR，`desktop` job 必须红；另：故意引入类型错误，`tsc` 步骤必须红 |
| 2 | 改 `protocol.rs` 后 `just bindings-check` 必须报 diff；`cargo nextest run --profile ci` 后必须存在 `target/nextest/ci/junit.xml`（不存在说明 profile 没生效，这是最容易静默失败的一处） |
| 3 | 把 `port-bound` 组的 `max-threads` 改成 4（或临时恢复固定端口），并发跑 `headless_signal_reap` 必须出现失败；改回必须绿 |
| 4 | 临时删掉一处 `yi-agent-power` 的 macOS 断言，`macos` job 必须红 |

阶段 2 和 3 的验证方式值得强调：**门禁的价值在于它会红**。一个配置改动后什么都没变，说明配置没生效——这时要怀疑配置本身，而不是「问题已经不存在」。

---

## 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 桌面测试当前不是全绿 | 阶段 1 被阻塞 | 先摸底；不绿则修复为前置任务，不以放宽门禁收场 |
| nextest 迁移后首次跑出失败 | 阻塞合并 | 阶段 2 单独 PR，可干净 revert；先本地全量跑 |
| 迁移暴露隐性共享状态 | 需要额外修测试 | 这是净收益（暴露了真问题），但要预留时间 |
| macOS runner 拉长 PR 时间 | 开发体验下降 | 只跑三个 crate；实测后再决定 |
| `--run-ignored` 语义搞混 | 真实测试被误跑或漏跑 | 本阶段不动 `test-real-*`；阶段 3 再统一处理 |
| `--profile ci` 漏传 | JUnit 静默不生成，可观测性落空 | 验证方式里写死「必须有 junit.xml」 |

---

## 附录 A：外部依据

- nextest filterset 语法：`test()` 默认 contains 匹配，正则需 `/.../`：<https://www.nexte.st/docs/filtersets/reference>
- **JUnit 必须 `--profile ci` 才生成、路径相对 `target/nextest/<profile>/`**：<https://nexte.st/docs/machine-readable/junit/>
- test-groups 官方建议：端口冲突应绑 0，而非用 test-groups 串行：<https://nexte.st/docs/configuration/test-groups/>
- 配置项在旧版 nextest 上被静默忽略：<https://qaskills.sh/blog/cargo-nextest-rust-testing-guide>
- **`macos-14` runner 于 2026-11-02 退役**：<https://github.blog/changelog/2026-10-01-github-actions-macos-14-runner-image-retirement>
- nextest 实测加速 1.37×–3.38×，官方措辞 "up to 3×"：<https://reasoncore.dev/post/cargo-nextest-benchmarks-measured-speedups-isolation-tradeoffs-and-ci-features>
- 重试掩盖真实回归、flaky 应走「诊断→隔离→定责→限期修」：<https://flaviocopes.com/courses/github-actions-cicd/treat-flaky-tests-as-defects/>、<https://testingexperience.com/test-automation/flaky-tests>
- `--run-ignored only` 等价 `-- --ignored`：<https://jorgeortiz.dev/posts/rust_unit_testing_tools_add_ons/>

## 附录 B：明确排除的方案

| 方案 | 排除理由 |
|---|---|
| Gherkin / cucumber-rust | 无业务方读者（读者是本人与 AI）；每个场景要维护一个 step 定义层；断言能力弱于直接 Rust |
| loom | 模型检查器，只适合手写无锁结构或自定义同步原语。本项目大量用 `Mutex`/`RwLock`，`unsafe` 极少 |
| trybuild | 收益真实，但 `.stderr` 是黄金快照，编译器升级会大面积变红；优先度低于属性测试 |
| test impact analysis | `cargo-rail` / `dokono-test` 成熟度参差，漏测代价高于收益；先做确定性分片 |
| 全仓变异测试 | 成本约为「变异体数 × 单次测试耗时 ÷ 并行度」，全仓是数千 CPU 小时级 |
| 总覆盖率硬阈值 | 诱发为指标写测试 |
| `serial_test` crate | nextest 官方明确不支持进程内互斥，应用 test-groups 替代 |

## 附录 C：本设计未覆盖、需要后续处理的事项

按优先级排列：

| # | 事项 | 说明 |
|---|---|---|
| 1 | Agent golden-set eval | 30–50 条起步，每例跑 3–5 次，评轨迹而非只看结果。这是当前最大的「测对东西」缺口 |
| 2 | 变异测试（限定模块） | 权限判断、bash 黑名单、store 幂等三处；只对快单元测试，不对 IPC 集成测试 |
| 3 | 属性测试（proptest） | `tui/markdown.rs`、`tui/slash.rs`、unicode width 三个 parser |
| 4 | patch coverage 门禁 | 只卡 PR 新增行，不设总阈值。基线不可解析时必须判失败 |
| 5 | 定时任务（nightly） | real-LLM 三层 + 变异测试 + 依赖审计。新披露 CVE 针对的是没改过的依赖 |
| 6 | insta 快照 | TUI 渲染与错误文案；CI 检测 `*.snap.new` 存在即失败 |
| 7 | 拆分巨型测试文件 | Rust：`rpc_tests.rs` 21272 行、`tui/app.rs` 11755 行、`server.rs` 10945 行；前端：`App.test.tsx` **6210 行**、`ThreadSidebar.test.tsx` 2420 行 |
| 8 | JUnit 上传 + 慢测试 Top10 | 阶段 2 生成 JUnit，但本阶段没有上传步骤 |
| 9 | cargo-audit / cargo-deny | 有 npm 分发、桌面 App、relay 网络组件 |
| 10 | 桌面端真实浏览器验证 | 扩展现有 `desktop/scripts/browser/session-view-check.mjs` 的 CDP 路径，而非引入 WebDriverIO |
| 11 | MSRV 门禁 | `Cargo.toml` 声明 `rust-version = "1.85"`，但 CI 没有 job 用 1.85 编译 |
| 12 | CI 分片 | 先用实测数据判断是否必要 |
| 13 | **补真 doctest** | 现在为 0。文档里 6 个代码块（`text`/`jsonc`/`json`，12 行围栏）从没被编译验证过 |
| 14 | **`CLAUDE.md` 门禁数量纠正** | 写的是「12 道」，实际 14 道 |

## 核查记录

初稿（commit `df3cf1f2`）经二次核查，修正如下：

| # | 初稿说法 | 实测结果 | 处理 |
|---|---|---|---|
| 1 | 「30 处文档代码块」会被 nextest 丢掉 | `cargo test --doc` 全仓 **0 个** doctest（14 个 crate 逐个 `running 0 tests`，passed 合计 0）；30 是全文匹配 `` ``` `` 行数，含测试代码里的字符串常量；实际文档块为 **6 个 / 12 行围栏**（`///` 3 块 + `//!` 3 块），语言为 `text`/`jsonc`/`json` | 删除「doctest 缺口」，删掉 `cargo test --doc` 兜底命令 |
| 2 | `runs-on: macos-14` | 该镜像 **2026-11-02 退役** | 改 `macos-15` |
| 3 | `junit.path = "target/nextest/ci/junit.xml"` | 该配置会拼成 `target/nextest/ci/target/nextest/ci/junit.xml` | 改 `path = "junit.xml"` |
| 4 | `cargo nextest run ...` 会生成 JUnit | 必须 `--profile ci`，否则整段 `[profile.ci]` 不生效 | recipe 加 `--profile ci` |
| 5 | 「12 道门禁」 | 实际 14 道 | 改为 14 并注明 `CLAUDE.md` 是旧的 |
| 6 | 「`drop(listener)` 与重新 bind 之间有 TOCTOU 窗口，建议 `from_std`」 | `spawn_mock_gateway` **自己 bind 并持有 listener**，不存在该窗口 | 改为「移交 listener」，删除 TOCTOU 论述 |
| 7 | 「`b3cc5c77` 附近的 `flock` 释放时序」 | `b3cc5c77` 是 spec 测试补齐；真正的 flock 修复是 `32ec0db0` | 换成正确哈希 |
| 8 | 「push 路径在 Mac mini 上已有 Node，可复用同一脚本」 | `release-dmg.sh:26-29` 显式检查并提示「缺 node 时请 brew install」，是人工保证 | 改为两条路径都用 `node:22` 镜像 |
| 9 | 桌面测试 133 个文件 / 1918 用例 | 基线实测 **140** 个 `*.test.ts*`；**1851** 个 `it()`/`test()` 声明 + 1 个 `it.each`(×8) ≈ **1859** 用例 | 修正为 140 / ≈1859。注：`grep 'test('` 会误中正则 `.test(`，统计须用 `(?<![\w.$-])(it|test)\s*\(` |
| 10 | `ENV_TEST_MUTEX`「12 处测试」 | 该文件里 43 次出现 = 1 处定义 + **42 处 `.lock()` 调用** | 修正 |
| 11 | `[profile.ci] inherit = "default"` | 正确键名是 **`inherits`**（[reference](https://nexte.st/docs/configuration/reference/) `profile.<name>.inherits`，0.9.115+；`default-config.toml` 同样用 `inherits`） | 改为 `inherits`，并在正文标注「写错不报错、整段静默失效」 |
| 12 | 「`junit.path` 写绝对路径会变成 `target/nextest/ci/target/nextest/ci/junit.xml`」 | 源码 `JunitConfig::new` 是 `store_dir.join(path)`；`Path::join` 遇绝对路径**直接替换**。`rustc` 实测：`base.join("/tmp/out/junit.xml")` → `/tmp/out/junit.xml` | 改写为「相对路径落到 `target/nextest/<profile>/`；绝对路径会替换 `store_dir`，可用但平台相关，故不用」 |
| 13 | `slow-timeout` 的 `terminate-after` 语义未说明 | `terminate-after` 单位是**周期数**，`period=60s, terminate-after=3` 为累计 180s 终止 | 在 toml 示例上方加注释说明 |
| 14 | 「`[profile.*]` 两段都要写：`inherit` 是否传递 overrides…」 | 同上，键名错 | 改为 `inherits` |
| 15 | `TempDir` 计数「`runtime_ipc.rs` 97 处、`runtime_coordinator.rs` 93 处」 | 基线实测 **99 / 96**（`grep -c TempDir`） | 修正 |
| 16 | 「文档里 12 处代码块，语言 `text`/`json`/`jsonc`，没有 rust 或无标记」 | 精确解析（开闭配对）为 **6 个块 / 12 行围栏**：`///` 3 块 + `//!` 3 块；语言 `text`×3、`jsonc`×2、`json`×1；**可执行块 0**，doctest 为 0 的结论成立 | 补全 `//!` 块清单，指明只 grep `///` 会漏一半 |
| 17 | 「现有测试里没有依赖全局串行的」 | `--test-threads=1` 实际有 **5 处**（`justfile:129/132/146/159/167`），全在 `test-real-*`，目的是 LLM API 限流 | 改写为限流说明，并给出 nextest 对应写法 `-j 1` |
| 18 | `package-lock.json` 「221 KB」 | 实际 **216 KB** | 修正 |
| 19 | 总体设计称「三个阶段」，实际列了四个 | 阶段编号 1-4，验证表也是 4 行 | 改为「四个阶段」 |
| 20 | 复核期间以**主仓库工作区**为基准测量 | 主仓库 `main` 上并行存在 thread-fork 的未提交改动（`git status` 有 `M`/`UU`），`rpc_tests.rs` 已 +157 行，导致同一文件两次测得 21272 与 21434 | 全部计数改在**本 worktree 的 `df3cf1f2` 干净基线**上重测；已回退由污染数据得出的错误「修正」 |

第三次核查（2026-11-01，写 implementation plan 时逐条实跑）：

| # | 前一次的说法 | 实测结果 | 处理 |
|---|---|---|---|
| 21 | （本条是我第三次核查时的**误判**，已撤回）我曾据 `actions/runner-images` README 只有 deprecated 徽章而断定「未给退役日期」 | 回查 spec 附录 A 给出的 changelog 原文（`github.blog/changelog/2026-10-01-github-actions-macos-14-runner-image-retirement`，当日抓取）：`"The macOS 14 runner image will be retired on November 2, 2026."` —— **日期确实有官方出处**，且附有 brownout 安排 | 维持原文「2026-11-02 退役」；`macos-15` 的结论不变。教训：README 没有日期 ≠ 无出处，附录已给链接就该先点开
| 22 | 「首轮 nextest 全量 3 failed：`stopping_clears_the_forwarding_table` + `a_childs_exit...` + `refreshing_a_stale_manifest...`」 | 连跑四轮，失败数在 **1/4/2** 之间浮动，**失败集合每次不同**，涉及 **5 个**测试（新增 `yi-agent::bin/yi-agent tests::the_supervisor_loop_starts_a_manifest_when_its_switch_is_on` 与 `yi-agent-supervisors daemon::tests::a_detached_child_leads_its_own_session_and_runs_in_the_project`）。已拿到每个的根因栈 | 阶段 3 的修复清单从 3 个扩到 5 个；完成判据改为**连跑四轮全绿**（一轮绿说明不了任何事） |
| 23 | 「`inherits` 写错整段静默失效」 | 更准确：写错只打一行 `warning: ... ignoring unknown configuration key: profile.ci.inherit`，退出码仍 0；**JUnit 仍会生成**（`profile.ci.junit` 是同级键，不依赖继承），失效的是 `slow-timeout` 等来自 `default` 的配置 | 修正表述；验证手段定为 `grep -i "unknown configuration key"` 必须无输出 |
| 24 | （本条也是**误判**，已修正）我曾称「vite 8 要求 `>=22.12.0`，`node-version: "20"` 会让 vite 拒绝启动」 | `vite` 的 engines 是 `^20.19.0 \|\| >=22.12.0`，**20 也在允许范围内**；`actions/setup-node` 的 `"20"` 指「20.x 最新」，当日 dist 索引为 **v20.20.2**，满足 `^20.19.0`。真正的事实依据是发布活跃度：v20.20.2 发布于 **2026-03-24** 之后无新版本（维护线已冻结），而 v22 最新为 **v22.23.4（2026-10-20，LTS Jod）** | 仍选 **Node 22**，但理由改为「跟随仍在发版的 LTS 线」，不再声称 Node 20 会失败
| 25 | 「GitLab 新 job 跑在 mac-mini」 | `.gitlab-ci.yml` 头注释与 `ci-mr` 都表明 **MR 走云端 runner（不进 Mac mini，安全考虑）**；`ci-mr` 无 `tags` | 新 job 用 `image: node:22` 且**不加 `tags`**，把「不许落 Mac mini」写成可校验的判据 |
| 26 | 「nextest 过滤写错不会被静默放过」 | 方向对，但机制需说清：nextest 在选中 0 个测试时 **exit 4** 并打印 `error: no tests to run`（实测）；`bindings-check` 的 `export_bindings` 位置参数实测选中 **18** 个 | 写进 plan，避免执行者把「0 tests run」当成功 |
| 27 | 「`stopping_clears_the_forwarding_table` 属于同类时间预算问题」 | 三个方向的 `wait_for(..., 3s)` 固定预算确实存在（`supervise.rs:335/350/410/443`），但本环境**未复现失败** | 按「同隐患一并加固、未复现不谎报」处理，并在 plan 里显式要求先复现再改 |
| 28 | `refresh.rs` 的 5s 超时被当成测试侧问题 | 它是**生产常量** `DESCRIBE_TIMEOUT`（`refresh.rs:34`）；实测报错 `` `describer.sh manifest` did not answer within 5s `` → `RefreshOutcome::Skipped` | 单列为一次「改生产代码」的提交，附代价分析（只影响插件已坏的路径），不改测试去迁就 |
| 29 | 计划初稿假设 `wait_until(impl Fn)` 可直接包住 `supervisor.reconcile()` | `Fn` 不允许可变借用，闭包内 `&mut supervisor` 编译不过 | 签名改 `impl FnMut() -> bool`（已用 `rustc` 实测可编译），列为修 #2 的前置步骤 |
