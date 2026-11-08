# 统一测试分级：两个正交轴

日期：2026-11-08
状态：待评审

## 问题

仓库里现在有**五套并行的测试入口**，各自独立演化，彼此不知道对方存在：

| 入口 | 内容 | 实测耗时 |
|---|---|---|
| `just test` | nextest `--workspace --all-features` | 821 秒（当时 5709 个测试；2026-11-08 复查已增至 6020 个） |
| `just runtime-lifecycle-check` | 23 个 target，`cargo test` 串行 | 34.1 秒（263 个测试） |
| `just test-real-*`（4 个 + `all`） | 真实 LLM，`#[ignore]` 门控 | 分钟级，花钱 |
| `just desktop-check` / `plugin-*-check` | tsc + vitest + 插件独立 workspace | 分钟级 |
| `just mutants` / `ci-patch` / `eval` | 变异测试 / 覆盖率 / 评测 | 8-10 分钟 / 7.2GB |

症状是「每次测试都要测很久」。根因不是测试太多，而是**该少的没少、该省的没省**。

### 具体病因（均为实测，非推测）

**病因 1：`just test` 无条件带上全部真进程测试。**

263 个真进程测试（真的 fork guardian / daemon / 子进程，个别等真实 10 秒优雅窗口）
不带 `#[ignore]`，因此 nextest `--workspace` 会执行它们。改一个字符串和改运行时
调度付一样的代价。

**病因 2：CI 的 macOS job 把同一批测试跑了两遍。**

`.github/workflows/ci.yml` 的 macos job 先用 nextest 跑 8 个 crate（含全部
lifecycle target），再用 `just runtime-lifecycle-check`（`cargo test`）跑同样的
263 个测试。

**病因 3：病因 2 的理由是错的。**

该 job 的注释称「必须用 `cargo test`，因为 `managed_runtime_lifecycle` 用
`current_exe()` + `--exact runtime_process_helper --ignored` 拉起自己的 OS
fixture，nextest 的进程模型下 `--exact` 行为不同」。

实测否证：

| 验证 | 结果 |
|---|---|
| `runtime-lifecycle-check` 全部 263 个测试在 nextest 下 | **263 passed, 4 skipped, 34.05s** |
| `managed_channel`（真的用 `current_exe()` + `--exact --ignored` 拉 fixture）| **7 passed** |
| `managed_runtime_lifecycle` 里是否存在 `current_exe` / `--exact` | **不存在**（该文件只有环境变量门控的 helper） |

即注释点名的文件根本不是使用该机制的载体，而真正使用它的文件在 nextest 下通过。
注释已陈旧，但没人敢删——删错了会让 CI 假绿。

**病因 4：真实 LLM 测试的 key 检查有三套不一致的逻辑。**

- `test-real-llm`：检查 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MODEL_API_KEY` 三者
- `test-real-e2e` / `test-real-complex`：只检查 `MODEL_API_KEY`
- `test-real-subagent`：**完全不检查**

`scripts/export-model-env.sh` 在无 key 时 `exit 0` 且不导出变量，因此
`test-real-subagent` 会**执行 0 个测试后成功**。报告写「通过」，实际什么都没验证。
这是假绿。

**病因 5：有两个 live 测试二进制没有任何入口覆盖。**

`crates/yi-agent/tests/model_catalog_live_reload_e2e.rs` 有 2 个 `#[ignore]` 的
live 测试，`justfile` 里没有任何 recipe 指向它。它会静默地永远不跑。

**病因 6：命名冲突。**

`CLAUDE.md` 现在有两节都叫「分级」：

- `## 分级测试入口(L0/L1/L2)`（按改动范围）
- `## 分级测试系统`（按是否需要真实 LLM，Tier 0-3）

两者是不同维度，但同名导致必须靠一段引用块声明「别混」——那是补丁，不是整合。

## 目标

1. **一套命名**：仓库里只有一个「分级」概念，不出现两个同名不同义的章节。
2. **两个正交轴**：范围（改了多少代码）与保真度（要多真实）分别表达，可自由组合。
3. **消除重复**：同一批测试在一次 CI 运行里只跑一遍。
4. **消除假绿**：无前置条件时必须是显式失败或显式跳过，不能「跑 0 个测试然后成功」。
5. **保留唯一例外**：只允许有明确理由、且写进文档的例外（本设计中为零）。

## 非目标

1. **不修那两个已知失败的 live 测试**（`real_openai_*` 打 `https://api.openai.com`
   而实际 provider 走自定义 URL；`e2e_auto_compact_triggers` 依赖 token 计数假设）。
   它们是真 bug，属于另一条工作线。本设计只保证「不会因为并发而假红」。
2. **不动 `just ci` 的门禁清单**。那是另一条工作线。
3. **不修改任何测试断言**。只改「怎么调起它们」。
4. **不承诺秒级反馈**。全量编译本身 43 秒，这个承诺给不了。
5. **不重写已有的 `2026-11-07-graded-test-tiers-design.md`**。该文档定义 L0-L2 的
   实现细节（映射逻辑、`rdeps()` 选型、`affected-check` 断言），仍然有效，本文档
   在其之上补足保真度轴并完成整合。

## 决策记录

### D1：两个正交轴，不是一条阶梯

**决策**：范围用 `L0-L3` 表达，保真度用 `mock / proc / live` 单独表达。

**理由**：一条一维阶梯无法表达横切需求。

- 「只改了一个字符串，但仍要跑全量 mock 测试」——阶梯表达不了。
- 「没改代码，只想验证真实 API 还能通」——阶梯表达不了。

**被否的替代方案**：

- *单一阶梯*（L0→L5 一维到底）：简单，但「只跑 live」需要跳过中间档，语义不自然。
- *场景别名优先*（只暴露「刚改完 / 要合并 / 要发版」）：贴合使用，但不透明——
  出问题时不知道该补跑什么。

**已知代价**：用户需要记两个维度。缓解方式是入口命令只暴露 5 个，且每个命令的
名称直接说明它覆盖什么。

### D2：`proc` 不是独立档位，包含在 `mock+proc` 里

**决策**：`just test-crate` / `just test-affected` / `just test` 都跑 `mock+proc`。
不提供「只跑 mock，不跑 proc」的入口。

**这不是在回避病因 1，而是换了解法。** 病因 1 的痛点是「改一个字符串也要等 13 分钟」，
但耗时的大头不是 proc，而是**全量的编译与 6020 个测试本身**。切出 proc 只能省 34 秒，
而 `just test-crate`（37 秒）与 `just test-affected`（只跑闭包）能省掉的是分钟级。
所以病因 1 的正确解法是**加范围维度的分层**（L1/L2），不是把 proc 单独切出来。

**理由**：263 个 proc 测试实测 34.1 秒，只占全量 821 秒的 4.1%（数字口径：34.1/821）。
单独再切一层，用户要多做一次「我该不该跑 proc」的判断，收益远小于认知成本。

**实测依据**（2026-11-08，nextest 0.9.150）：

| 验证 | 结果 |
|---|---|
| `runtime-lifecycle-check` 的 23 个 target、263 个测试在 nextest 下 | **263 passed, 4 skipped, 34.05s**（wall 66s） |
| 全 workspace 测试总数 | 6020 |

**边界**：如果将来 proc 测试增长到显著占比（例如超过全量耗时的 20%），应重新评估
是否切分。当前 4.1% 不支持这个复杂度。

### D3：live 迁移到 nextest，并发 3

**决策**：`test-live` 使用 nextest `--run-ignored only`，用 test-group 把并发压到 3。

**理由**：

1. **统一 runner**。两个 runner 就有两套超时语义、两套并发模型，假红必然回归。
2. **实测可行**：`cargo nextest run --all-features -p yi-agent --test e2e_real
   --run-ignored only` 在默认并发下 4 passed / 1 failed，失败项与串行时相同。
3. **并发 3 已验证安全**：见下方「并发安全性实证」。
4. **并发收益确定**：`e2e_real` 串行 90.01 秒 → 并发 3 的 38.17 秒。

**关于串行约束的变更**：原设计坚持 `--test-threads=1`，理由是「真实 API 有速率
限制」。该约束经确认可放松到并发 3。

**并发安全性实证**（2026-11-08，本机 18 核 / 128 GB / macOS）：

| 测试集 | 串行 | 并发 3 | 判定 |
|---|---|---|---|
| `real_integration`（anthropic 系） | pass | pass | 并发无害 |
| `real_integration`（openai 系，3 个） | **3 fail** | **3 fail** | 失败与并发无关 |
| `e2e_real`（5 个可跑） | 4 pass / 1 fail | 4 pass / 1 fail | 失败与并发无关 |

两处失败串行也复现，故**不是并发引入**，而是环境/断言问题（见非目标 1）。

**关于 `YI_AGENT_HOME` 隔离**：原本担心并发时多个子进程共享真实 `~/.yi-agent`
（`run_agent_with_timeout` 只传 `--workdir`，不设 `YI_AGENT_HOME`）。实测下并发 3
全部通过，且运行时 socket/db 已按 workdir 隔离（`project_runtime_directory(workdir)`），
每个测试用独立 `TempDir`。**结论：不需要补隔离**，避免一处测试代码改动。

**被否的替代方案**：

- *保留 `cargo test --test-threads=3`*：零风险，但两个 runner 并存，超时语义不一致。
- *迁移但保持串行*：放弃已确认可行的并发收益（90s → 38s）。

### D4：live 需要独立的超时预算

**决策**：为 live 增加独立 profile，`slow-timeout = { period = 300s, terminate-after = 2 }`。

**理由**：这是 D3 引入的**必须修**的新问题。现状 `[profile.default]` 是
`slow-timeout = { period = 60s, terminate-after = 3 }`，即 **180 秒终止**。而 live
测试自身预算是 **300 秒**：

| 常量 | 位置 | 值 |
|---|---|---|
| `E2E_TIMEOUT` | `crates/yi-agent/tests/e2e_real.rs:14` | 300s |
| `COMPLEX_TIMEOUT` | `crates/yi-agent/tests/common/mod.rs:20` | 300s |
| `real_integration` 的 client timeout | `crates/yi-agent-llm/tests/real_integration.rs` | 30s |
| `subagent_real_e2e` 的 deadline | `crates/yi-agent/tests/subagent_real_e2e.rs:219,281` | 300s |

若不抬预算，300 秒预算的合法测试会在 180 秒被 nextest 杀掉，报一个看似「不稳定」
的假红。

**数值推导**：`period = 300s, terminate-after = 2` → 累计 600 秒终止，是 300 秒
自身预算的 2 倍，留足余量；第 1 个周期只标 SLOW，不会误杀。

**实测确认（2026-11-08，nextest 0.9.150）**：`slow-timeout` **可以**写在
`[[profile.default.overrides]]` 里，无需独立 profile。但格式必须带单位：

```toml
slow-timeout = { period = "300s", terminate-after = 2 }   # 正确
slow-timeout = { period = 300, terminate-after = 2 }      # 报错：expected a duration
```

`terminate-after` 是整数（周期数），`period` 是字符串（带时长单位）。两者格式不同，
写错会在解析阶段直接失败而非静默降级——这点是安全的。

### D5：统一 key 检查，消除三套逻辑

**决策**：所有 live 入口走同一个前置检查函数，且检查项为
`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `MODEL_API_KEY` 三者的并集。

**理由**：`test-real-subagent` 当前无检查 → 假绿（病因 4）。统一后所有入口在无
可用 key 时打印 `SKIPPED: ...` 并 `exit 0`（沿用 `export-model-env.sh` 既有约定：
缺工具是环境问题，不伪装成测试失败）。

**注意**：`exit 0` 跳过仍是「成功」，但它有显式输出。这与「静默跑 0 个测试然后
成功」的区别是**可观测性**——日志里能看见跳过原因。

### D6：`live` 永不进入 `just test`

**决策**：`just test`（L3, mock+proc）不包含任何 live 测试。live 只能由
`just test-live` 主动调用。

**理由**：live 花钱、有速率限制、需要密钥、结果依赖外部服务。它必须是**主动调用**。
`#[ignore]` 已经是天然的保险，但见下方风险 R1——不能只依赖它。

### D7：删除 macOS job 的重复步骤

**决策**：删除 `.github/workflows/ci.yml` macos job 的
`Run shared runtime lifecycle tests` 一步，其覆盖已包含在上一步的 nextest 中。

**理由**：实测 263 个测试在 nextest 下全绿（病因 3），重复跑无收益。同时修正那条
错误的注释，避免后人再被误导。

**风险控制**：该改动只影响 CI，本地 `just runtime-lifecycle-check` **保留**，因为
它作为独立入口仍有价值（单独跑真进程测试时不必等全量编译）。

## 架构

### 两个轴

```
轴 1 · 范围 (scope)              轴 2 · 保真度 (fidelity)
─────────────────────           ─────────────────────────────
L0  check        编译即止        mock   无真实进程，全并行
L1  crate        单 crate        proc   真进程 / 真 OS 资源，全并行
L2  affected     反向依赖闭包     live   真实 LLM API，并发 3
L3  workspace    全量
```

**为什么是两个轴而不是一个矩阵的 12 格**：`L0` 与保真度无关（不产出测试二进制），
`live` 与范围无关（它是一组固定的 `#[ignore]` 测试）。实际有意义的组合只有 5 个，
就是下面这张表。

### 入口

| 命令 | 范围 | 保真度 | 用途 | 实测/预期耗时 |
|---|---|---|---|---|
| `just check-fast` | L0 | — | 改完看能否编过 | 分钟级（编译为主） |
| `just test-crate <c>` | L1 | mock+proc | 只改了一个 crate | 约 37 秒（supervisors 451 个） |
| `just test-affected` | L2 | mock+proc | 改了几处，想知道牵动谁 | 视闭包大小 |
| `just test` | L3 | mock+proc | 收尾 / 合并前 | 约 821 秒 |
| `just test-live [suite]` | — | live | 主动验证真实 API | 分钟级，花钱 |

### `test-live` 的 suite 参数

收敛现有 4 个 recipe + 补上缺失的 1 个：

| suite | 目标二进制 | `#[ignore]` 数 | 现有覆盖 |
|---|---|---|---|
| `llm` | `yi-agent-llm/tests/real_integration.rs` | 7 | `test-real-llm` |
| `e2e` | `yi-agent/tests/e2e_real.rs` | 6 | `test-real-e2e` |
| `complex` | `yi-agent/tests/e2e_complex.rs` | 5 | `test-real-complex` |
| `subagent` | `yi-agent/tests/subagent_real_e2e.rs` | 2 | `test-real-subagent` |
| `catalog` | `yi-agent/tests/model_catalog_live_reload_e2e.rs` | 2 | **无（病因 5）** |
| （无参数） | 以上全部 | 22 | `test-real-all` |

### nextest 配置

```toml
# live：真实 LLM API。并发 3（已实测安全），超时抬到 600s。
[test-groups.live-api]
max-threads = 3

[[profile.default.overrides]]
filter = 'binary_id(yi-agent-llm::real_integration) + binary_id(yi-agent::e2e_real) + binary_id(yi-agent::e2e_complex) + binary_id(yi-agent::subagent_real_e2e) + binary_id(yi-agent::model_catalog_live_reload_e2e)'
test-group = 'live-api'
slow-timeout = { period = "300s", terminate-after = 2 }
```

注：`overrides` 支持 `slow-timeout` 已实测确认（nextest 0.9.150），无需独立 profile。
`period` 必须带单位字符串，`terminate-after` 为整数周期数。

### 数据流（`test-live`）

```
just test-live [suite]
  └─> 统一前置检查（key 并集；无 key 则打印 SKIPPED 并 exit 0）
      └─> cargo nextest run --all-features \
            --run-ignored only \
            --profile live \            # 或 default + overrides
            -p <crate> --test <binary>
```

### 错误处理

| 情况 | 行为 |
|---|---|
| 无可用 key | 打印 `SKIPPED: ...`，`exit 0`（沿用既有约定） |
| nextest 零匹配 | nextest 返回 rc=4，原样透传并附说明（沿用 `test-tiers.sh` 既有约定） |
| live 测试超时 | 自身 300s 预算先触发；nextest 600s 兜底 |
| 未知 suite 名 | `exit 1` 并列出合法取值 |

## 测试

本设计自身的验收：

1. **`test-live` 的 key 检查**：在无 key 环境下跑每个 suite，必须打印 `SKIPPED`
   且 `exit 0`；在有 key 时能真实执行。
   - 关键断言：**不能出现「无输出、跑 0 个测试、rc=0」**（病因 4 的形态）。
2. **suite 覆盖完整性**：断言上表 5 个二进制都被某个 suite 映射到。
   这条断言防的是病因 5（`model_catalog_live_reload_e2e` 无人覆盖）。
3. **nextest 能跑 proc 测试**：`runtime-lifecycle-check` 的 263 个测试在 nextest 下
   全绿（已实测），删除 CI 重复步骤前必须再确认一次。
4. **超时不误杀**：live profile 的 `slow-timeout` 累计值必须 > 300s（最长自身预算）。
   可以写一条断言读取配置并比较，纳入 `just affected-check` 同类自测。
5. **并发 3 的配置生效**：断言 `test-groups.live-api` 的 `max-threads == 3`。

## 风险

### R1：`--run-ignored all` 会误拉起 live 测试

`[profile.default].default-filter` 只排除 `test(runtime_process_helper)`，不排除
live 测试。它们靠 `#[ignore]` 默认不跑。若有人输入 `--run-ignored all`（排查
「为什么某个测试从没跑过」时最容易输入的命令），live 测试会被拉起并产生真实 API
调用与费用。

**缓解**：在 `nextest.toml` 的 `default-filter` 中加入排除项，把这 5 个二进制的
ignored 测试显式排除。这与既有的 `runtime_process_helper` 排除是同一思路——
**用 filter 而不是靠约定**。

### R2：live 测试的结果依赖外部服务

网络抖动、provider 限流、模型行为变化都会导致失败。
**缓解**：这是固有属性，不试图消除。文档中明确 live 测试不纳入任何自动门禁。

### R3：删除 CI 步骤可能漏掉真实覆盖差异

若 ubuntu 与 macOS 上 nextest 行为不同，删除重复步骤会漏测。
**缓解**：该 job 本就是 macOS-only；删除的是**同一 job 内**的重复，不跨 OS。

### R4：`test-tiers.sh` 与新入口的职责边界

现有 `scripts/test-tiers.sh` 负责 L0-L2 的映射与执行。`test-live` 不属于范围轴，
应放独立的 `scripts/test-live.sh` 或 justfile 内联，**不塞进 `test-tiers.sh`**，
避免把两个轴的逻辑混在一个文件里。

## 与已有文档的关系

| 文档 | 关系 |
|---|---|
| `docs/superpowers/specs/2026-11-07-graded-test-tiers-design.md` | **仍然有效**。定义 L0-L2 的实现细节（映射、`rdeps()` 选型、`affected-check` 断言）。本文档在其之上补保真度轴并整合。 |
| `docs/superpowers/plans/2026-11-07-graded-test-tiers.md` | 已完成。L0-L2 已实装并合并。 |
| `CLAUDE.md` 的 `## 分级测试入口(L0/L1/L2)` | 需更新：合并两个轴，删除与 `## 分级测试系统` 的命名冲突。 |
| `CLAUDE.md` 的 `## 分级测试系统` | 需重写：Tier 0-3 的表述并入保真度轴。 |

## 实施顺序

必须按此顺序，因为后者依赖前者的验证结果：

1. ~~验证 nextest 能否在 override 中设置 `slow-timeout`~~ — **已实测完成（见 D4）**：
   可以，`period` 需带单位字符串。
2. 新增 `[test-groups.live-api]` 与超时配置（R1 的 `default-filter` 排除项一并加）。
3. 新增 `just test-live [suite]`，内含统一 key 检查。
4. 删除 4 个 `test-real-*` recipe（`test-real-all` 由 `just test-live` 无参替代）。
5. 删除 CI macos job 的重复步骤 + 修正错误注释。
6. 更新 `CLAUDE.md`：合并两个轴，消除命名冲突。
7. 更新 `docs/project-management/tooling.md`。

## 验收

- [ ] `just test-live` 无 key 时每个 suite 都打印 `SKIPPED` 并 `exit 0`
- [ ] `just test-live llm` 在有 key 时真实执行
- [ ] 上表 5 个 live 二进制全部被某个 suite 覆盖（含 `catalog`）
- [ ] `just runtime-lifecycle-check` 与 nextest 跑同一批 263 个测试结果一致
- [ ] CI macOS job 中 263 个测试只跑一遍
- [ ] `CLAUDE.md` 里只有一个「分级」概念，无两个同名章节
- [ ] `just test` 仍然只跑 mock+proc，不触发任何真实 API 调用
