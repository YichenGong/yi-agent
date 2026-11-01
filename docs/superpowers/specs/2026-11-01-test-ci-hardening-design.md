# 测试与 CI 加固：堵住「绿着漏」的五个缺口

**状态：** 设计（待评审）
**日期：** 2026-11-01
**范围：** P0 五项缺口（五个缺口按依赖关系组织为四个可独立合并的阶段）—— 桌面测试进 CI、nextest 迁移、doctest 保留、macOS 门禁、隔离规则落文件

---

## 问题

`just ci` 是唯一的 CI 入口，12 道门禁全绿才允许合并。但当前有五个「写得多、验得少」的缝隙。

### 缺口 1：桌面端测试不在任何 CI 里

`desktop/src` 下有 140 个 `*.test.ts*`、约 1924 个 `it()` / `test()`，`desktop/package.json:10` 有 `"test": "vitest run"`。但：

- `yi-agent-rs/justfile:36` 的 `ci:` 依赖列表里没有它；
- `.github/workflows/ci.yml`、`.gitlab-ci.yml` 只调 `just ci`；
- 三个文件里 `vitest` / `npm test` 零命中。

结果：前端测试写得再多，删掉也没人知道。`tsc` 也只在 `npm run build` 里跑，同样不在门禁内。

### 缺口 2：测试运行器共享进程

`justfile:22-23` 是 `cargo test --all-features --workspace`，进程内多线程。共享状态有三处进程级静态：

| 静态 | 位置 | 保护方式 |
|---|---|---|
| `TEST_CONNECTOR` | `yi-agent-mcp/src/manager.rs:763` | `seam_guard()` 手动持锁到 await 之后 |
| `ENV_TEST_MUTEX` | `yi-agent-runtime/src/config.rs:544` | 42 处测试逐个 `lock()` |
| `SPAWN_LOCK` | `yi-agent-supervisors/tests/supervisor.rs:18` | 起子进程的测试串行 |

这些锁是有效补丁，但**约束只存在于代码里**，没有变成仓库规则；且 `cargo test` 下一个测试 panic 会带走同进程的其他测试。

已发生三次竞态事故：`4ed7b410`（seam 互抢 `TEST_CONNECTOR`）、`022a6d50`（把通知当响应）、`d1313805`（插件竞态），以及 `b3cc5c77` 附近的 `flock` 释放时序。

### 缺口 3：迁移 nextest 会静默丢掉文档测试

`nextest` 不执行 doctest（官方文档明确说明）。本仓库有 30 处文档代码块。直接替换运行器会让这部分覆盖**静默消失**——没有任何报错，只是不再跑。

### 缺口 4：macOS 专属代码在 PR 路径上没有验证

21 处 `#[cfg(target_os = "macos")]`：

| crate | 数量 | 内容 |
|---|---|---|
| `yi-agent-power` | 8 | 电源断言、IOKit 调用 |
| `yi-agent-app-server` | 7 | git diff 的 macOS 分支 |
| `yi-agent-tools` | 6 | sandbox 只读测试 |

而 `.github/workflows/ci.yml:11` 是 `runs-on: ubuntu-latest`，只有 push 到 GitLab 的 Mac mini 才在 macOS 上跑。**一个改坏 power 或 sandbox 的 PR 在 GitHub 上是绿的，合进 `main` 之后才暴露。**

### 缺口 5：已发现的真实 flake 隐患尚未归位

`yi-agent/tests/headless_signal_reap.rs:163`：

```rust
let port = 9000 + (signal as u16 % 500);
let pid_file = format!("/tmp/yi-headless-signal-tool-{signal}.pid");
```

两个测试分别用 SIGINT(2) 和 SIGTERM(15)，端口是 9002 和 9015，pid 文件都在固定的 `/tmp` 下。当前靠「端口不同」这份巧合避免冲突——任何第三个信号测试或端口复用都会撞车。这是 nextest `test-groups` 的直接适用场景，但需要显式声明，不能靠巧合。

## 目标

1. 前端测试与 Rust 测试一起进入合并门禁，任一失败都阻塞合并。
2. 测试间不再共享进程级可变状态；崩溃的测试不带走同批其他测试。
3. 迁移后 doctest 覆盖不下降（30 处文档示例仍被编译执行）。
4. macOS 专属代码在 PR 阶段就有信号。
5. 已识别的共享状态（三处静态 + 一处固定端口）能对应到配置文件里可执行的规则。

## 非目标

- 不引入覆盖率门禁、变异测试、属性测试、快照测试。
- 不做测试影响分析。
- 不改动任何现有测试的断言内容。
- 不重构测试以消除进程级静态（那是另一个量级的改动）。

## 总体设计

四个阶段，按依赖顺序：

```
阶段 1  桌面测试进 CI           （独立）
阶段 2  nextest 迁移 + doctest  （独立；阶段 3 依赖它）
阶段 3  隔离规则落 .config      （依赖阶段 2）
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
    cd ../desktop && npm ci && npx tsc --noEmit && npm test
```

两个决定的理由：

- **`tsc --noEmit` 而非 `npm run build`**：门禁只需类型正确，产出 `dist/` 是发布步骤的事。`--noEmit` 更快且不写仓库。
- **`npm ci` 而非 `npm install`**：`npm ci` 严格按 lockfile 安装且不修改 `package-lock.json`。门禁不该有写仓库的能力。

### 接入方式

**不放进 `just ci`。** 依据是本仓库已有的先例（`justfile:38-41`）：`magpie-agent-check` 刻意不进 `ci`，因为 `rust:latest` 没有 Node，塞进去会拖垮整条流水线。桌面测试同理。

改为两条独立门禁：

- `.github/workflows/ci.yml`：新增 `desktop` job，`runs-on: ubuntu-latest`，`actions/setup-node@v4` + `node-version: "20"`（与 `magpie-agent.yml:34`、`release.yml:174` 一致），跑 `npm ci && npx tsc --noEmit && npm test`。
- `.gitlab-ci.yml`：
  - MR 路径新增 `ci-desktop` job，`image: node:20`，走云端 runner（不进 Mac mini，与既有安全策略一致）。
  - push 路径在 Mac mini 上已有 Node，可复用同一脚本。

两条都是**合并门禁的一部分**——分支保护里必须把它们列为 required check，否则等于没有。

### 前置摸底（必须先做）

`desktop/vite.config.ts` 里没有 `test` 段，测试文件用 `/** @vitest-environment jsdom */` 文件级注释声明环境。**需要先本地跑一次 `npm test` 确认是否全绿。**

如果不绿：修复测试是阶段 1 的前置任务。**不能通过放宽门禁（skip 用例、降级断言）来让它变绿**，否则这次改造就变成了给已知问题盖盖子。

如果总耗时超过 5 分钟：用 vitest 的 `--shard` 拆分，或按目录拆成多个 job。这个决定等实测数据。

---

## 阶段 2：nextest 迁移与 doctest 保留

### 设计

```make
# 跑全部测试。nextest 提供进程隔离、per-test 超时和 JUnit 输出；
# 但它不跑 doctest（官方明确说明），所以 doc 测试单独一条，缺一不可。
test:
    cargo nextest --version
    cargo nextest run --all-features --workspace
    cargo test --all-features --workspace --doc

# 配置漂移门禁。nextest 的 filterset 中 `test()` 默认是「包含」匹配，
# 与 cargo test 的位置参数子串语义一致，故此处写法不变。
bindings-check:
    cargo nextest run --all-features -p yi-agent-app-server export_bindings
    git diff --exit-code -- ../desktop/src/lib/generated
```

`cargo nextest --version` 单独一行是刻意的：附录 A 指出，新版配置项在旧版 nextest 上会被**静默忽略**（例如 `junit` 不生效且不报错）。把版本打到日志里，失败时能第一时间排除工具链问题。

新增 `.config/nextest.toml`（workspace 根，即 `yi-agent-rs/.config/nextest.toml`）：

```toml
[profile.default]
slow-timeout = { period = "60s", terminate-after = 3 }
fail-fast = false

[profile.ci]
inherit = "default"
retries = 0

[profile.ci.junit]
path = "target/nextest/ci/junit.xml"
```

**`retries = 0` 是刻意决定。** 多份来源一致认为「先加重试」会训练团队忽略红灯（附录 A）。需要重试的只有被显式隔离的测试——那是后续议题，不在本阶段。

### `--ignored` 的等价写法（已核实）

现有 `just test-real-*` 用 `cargo test ... -- --ignored`。nextest 的等价写法是 `--run-ignored only`（`--run-ignored all` 等价于 `--include-ignored`）。

**但本阶段不改这些 recipe。** 真实 LLM 测试不进 CI，改与不改都不影响门禁；等阶段 3 定了隔离规则再一起处理，避免一次改动波及太多面。

### 迁移风险与对策

| 风险 | 对策 |
|---|---|
| `bindings-check` 的过滤语义变化导致漏跑 | 迁移后改一处 `protocol.rs`，确认生成物真的被重写（`git status` 有 diff 才算跑过） |
| doctest 静默消失 | `test` recipe 里第二条命令兜底；验证方式见「验证方式」表 |
| nextest 版本过旧导致配置被静默忽略 | recipe 里先 `--version`；CI 安装时钉一个明确版本 |
| 依赖 `--test-threads=1` 的测试跑法变了 | nextest 用 `test-groups` 表达；现有测试里没有依赖全局串行的，逐条确认后可归入阶段 3 |
| 迁移后首次出现「Linux 上通过、nextest 下失败」 | 这通常说明测试间有隐性共享状态，是被隔离暴露出来的真实问题，不是迁移引入的 bug。记录后单独修 |

### 安装方式

CI 里安装 nextest 用 `taiki-e/install-action@v2`（`tool: nextest`），比 `cargo install --locked` 快一个数量级，且是社区通用做法。版本要显式钉住，不跟随「最新」。

---

## 阶段 3：隔离规则落文件

### 现状核实

我核对了实际的共享状态，结论比最初设想的乐观：

| 资源 | 实际状况 | 是否需要 test-groups |
|---|---|---|
| TCP 端口 | 绝大多数测试已绑定 `127.0.0.1:0`（`relay/tests/bridge.rs:70`、`llm/tests/integration.rs:638`、`app-server/src/ws.rs:605` 等 15 处） | 否 |
| daemon socket | 每个测试一个 `TempDir`（`runtime_ipc.rs` 97 处、`runtime_coordinator.rs` 93 处） | 否 |
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

nextest 官方文档对此有明确建议：**不要用 test-groups 解决端口冲突，应该绑定端口 0 让内核分配**。理由是 test-groups 只是逻辑互斥，仍可能和系统上已有的服务撞车，而且会让测试变慢。

`headless_signal_reap.rs` 的端口是传给子进程的 mock gateway 用的，改造方式：

```rust
// 先绑 0 拿到内核分配的端口，再交给子进程。
let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
let port = listener.local_addr()?.port();
drop(listener);            // 让出端口给 gateway 用
let pid_file = tempdir.path().join(format!("tool-{signal}.pid"));
```

两个要点：
- **pid 文件挪进 `TempDir`**，不要写死 `/tmp`。`/tmp` 下的固定文件名在并发或残留进程时会互相覆盖。
- `drop(listener)` 与 gateway 重新 bind 之间有极小的 TOCTOU 窗口。更稳的做法是让 gateway 接受一个已绑定好的 listener（`TcpListener::from_std`），彻底消除窗口。**推荐后者**。

### 3b：为剩余场景显式分组（兜底）

如果 3a 因 mock gateway 的接口限制无法完成，则退回到 test-groups：

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

注意 `[profile.*.overrides]` 要分别写，因为 `inherit` 不会自动继承 overrides（按 nextest 的 override 合并语义，显式重复更安全）。

### 3c：三处静态的处理

nextest 是 process-per-test，**每个测试独立进程，进程级静态天然隔离**。这意味着：

- `TEST_CONNECTOR`、`ENV_TEST_MUTEX`、`SPAWN_LOCK` 在 nextest 下几乎失去存在的必要；
- 但 `cargo test` 仍要能跑（本地方便单跑某个测试），锁必须保留。

所以设计是**两条路径都正确，配置文件是主防线，锁是兜底**。把三处的注释升级为指向配置文件，例如：

```rust
/// 进程级 seam，碰它的测试必须拿 `seam_guard()`。
/// nextest 下测试进程独立，争用自然消失（见 `.config/nextest.toml`）；
/// `cargo test` 下仍靠这把锁，两种运行器都必须安全。
static TEST_CONNECTOR: Mutex<Option<TestConnector>> = Mutex::new(None);
```

### 不做什么

- **不引入 `serial_test` crate**。nextest 的 `test-groups` 已表达同一件事；nextest 官方也说明它不支持进程内互斥，用 test-groups 替代。加依赖是重复。
- **不试图消除 `ENV_TEST_MUTEX`**。`config.rs` 里 42 处测试必须改环境变量，串行就是这类测试的正确做法，重构为依赖注入是另一个量级的改动。

---

## 阶段 4：macOS 门禁

### 设计

`.github/workflows/ci.yml` 新增 job：

```yaml
  macos:
    runs-on: macos-14          # Apple Silicon
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

三个决定：

- **只跑三个 crate**：目标是覆盖 `cfg(target_os = "macos")` 的 21 处代码路径，不是重复 Linux 已跑过的验证。全量跑会让 PR 等待时间翻倍。
- **用 GitHub `macos-14` 而非现有 Mac mini**：`ci.yml` 的注释写明「PR 走云端 runner（不进 Mac mini,安全考虑）」。引 Mac mini 跑 PR 会破坏这条边界。
- **不用 `--partition`**：三个 crate 的测试量不大，先观察实际耗时再决定。

### 降级选项（不推荐，但不该不写）

如果 macOS runner 排期或成本不可接受，退一步方案是在 ubuntu 上做交叉编译检查：

```bash
cargo check --target aarch64-apple-darwin -p yi-agent-power -p yi-agent-tools -p yi-agent-app-server
```

**这只能验证编译通过，不能验证行为。** `yi-agent-power` 的 8 处 macOS 代码实际调用电源断言，行为测试才有价值。所以交叉编译只应作为「mac runner 批不下来」时的降级，且要在 `docs/project-management/ci-cd.md` 里如实记录这个降级带来的残余风险。

---

## 验证方式

每个阶段的「完成」都必须有可执行判据，不接受主观描述。

| 阶段 | 判据 |
|---|---|
| 1 | 故意改坏一个前端测试的断言并推 PR，`desktop` job 必须红；改回必须绿。另：`npx tsc --noEmit` 在故意引入类型错误时必须红 |
| 2 | 迁移前后对比 doctest 数量：`cargo test --doc -p <crate> 2>&1 \| grep -c '^test '` 每个 crate 一致。另：改 `protocol.rs` 后 `just bindings-check` 必须报 diff |
| 3 | 把 `port-bound` 组的 `max-threads` 改成 4（或临时恢复固定端口），并发跑 `headless_signal_reap` 必须出现失败；改回必须绿 |
| 4 | 临时删掉一处 `yi-agent-power` 的 macOS 断言，`macos` job 必须红 |

阶段 3 的验证方式值得强调：**门禁的价值在于它会红**。一个配置改动后什么都没变，说明配置没生效——这时要怀疑的是配置本身，而不是「问题已经不存在」。

---

## 风险

| 风险 | 影响 | 缓解 |
|---|---|---|
| 桌面测试当前不是全绿 | 阶段 1 被阻塞 | 先摸底；不绿则修复为前置任务，不以放宽门禁收场 |
| nextest 迁移后首次跑出失败 | 阻塞合并 | 阶段 2 单独 PR，可干净 revert；先本地全量跑 |
| 迁移暴露隐性共享状态 | 需要额外修测试 | 这是净收益（暴露了真问题），但要预留时间；不要把「nextest 下有失败」误判为迁移 bug |
| macOS runner 拉长 PR 时间 | 开发体验下降 | 只跑三个 crate；实测后再决定 |
| `--run-ignored` 语义搞混 | 真实测试被误跑或漏跑 | 本阶段不动 `test-real-*`；阶段 3 再统一处理 |

---

## 附录 A：外部依据

- nextest 每测试独立进程、不跑 doctest、filterset 语法：<https://nexte.st/>、<https://www.nexte.st/docs/filtersets/reference>（`test()` 默认 contains 匹配，正则需 `/.../`）
- test-groups 官方建议：端口冲突应绑 0，而非用 test-groups 串行：<https://nexte.st/docs/configuration/test-groups/>
- nextest 实测加速 1.37×–3.38×，官方措辞 "up to 3×"：<https://reasoncore.dev/post/cargo-nextest-benchmarks-measured-speedups-isolation-tradeoffs-and-ci-features>
- 配置项在旧版 nextest 上被静默忽略：<https://qaskills.sh/blog/cargo-nextest-rust-testing-guide>
- 重试掩盖真实回归、flaky 应走「诊断→隔离→定责→限期修」：<https://flaviocopes.com/courses/github-actions-cicd/treat-flaky-tests-as-defects/>、<https://testingexperience.com/test-automation/flaky-tests>
- `--run-ignored only` 等价 `-- --ignored`：<https://jorgeortiz.dev/posts/rust_unit_testing_tools_add_ons/>

## 附录 B：明确排除的方案

| 方案 | 排除理由 |
|---|---|
| Gherkin / cucumber-rust | 无业务方读者（读者是本人与 AI）；每个场景要维护一个 step 定义层；断言能力弱于直接 Rust。多家来源指出「没有三方协作就只付成本不拿收益」 |
| loom | 模型检查器，只适合手写无锁结构或自定义同步原语。本项目大量用 `Mutex`/`RwLock`，`unsafe` 极少 |
| trybuild | 收益真实（可钉住「必须编译失败」），但 `.stderr` 是黄金快照，编译器升级会大面积变红；优先度低于属性测试 |
| test impact analysis | `cargo-rail` / `dokono-test` 成熟度参差，漏测代价高于收益；先做确定性分片 |
| 全仓变异测试 | 成本约为「变异体数 × 单次测试耗时 ÷ 并行度」，全仓是数千 CPU 小时级 |
| 总覆盖率硬阈值 | 诱发为指标写测试 |
| `serial_test` crate | nextest 的 `test-groups` 已覆盖；官方明确 nextest 不支持进程内互斥 |

## 附录 C：本设计未覆盖、需要后续处理的事项

按优先级排列，供后续排期参考：

| # | 事项 | 说明 |
|---|---|---|
| 1 | patch coverage 门禁 | 只卡 PR 新增行，不设总阈值。基线不可解析时必须判失败，不能静默假绿 |
| 2 | 变异测试（限定模块） | 权限判断、bash 黑名单、store 幂等三处；只对快单元测试，不对 IPC 集成测试 |
| 3 | 属性测试（proptest） | `tui/markdown.rs`、`tui/slash.rs`、unicode width 三个 parser |
| 4 | Agent golden-set eval | 30–50 条起步，每例跑 3–5 次，评轨迹而非只看结果。这是当前最大的「测对东西」缺口 |
| 5 | 定时任务（nightly） | real-LLM 三层 + 变异测试 + 依赖审计。新披露 CVE 针对的是没改过的依赖，只在 PR 跑会漏 |
| 6 | insta 快照 | TUI 渲染与错误文案；CI 检测 `*.snap.new` 存在即失败 |
| 7 | 拆分巨型测试文件 | `rpc_tests.rs` 21272 行、`tui/app.rs` 11755 行、`server.rs` 10945 行 |
| 8 | JUnit 上传 + 慢测试 Top10 | 失败可诊断性；阶段 2 已生成 JUnit，但还没上传 |
| 9 | cargo-audit / cargo-deny | 有 npm 分发、桌面 App、relay 网络组件，供应链面比两年前大 |
| 10 | 桌面端真实浏览器验证 | 扩展现有 `desktop/scripts/browser/session-view-check.mjs` 的 CDP 路径，而不是引入 WebDriverIO（Tauri 用系统 WebView，Chromium 结果不可信） |
| 11 | MSRV 门禁 | `Cargo.toml` 声明 `rust-version = "1.85"`，但 CI 没有 job 用 1.85 编译 |
| 12 | CI 分片 | 先用实测数据判断是否必要；分片是确定性保证，优先于影响分析 |
