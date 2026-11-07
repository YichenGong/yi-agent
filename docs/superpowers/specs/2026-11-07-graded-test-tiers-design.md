# 分级测试入口：按改动范围选择测试档

> 状态：设计已批准（2026-11-07）
> 日期：2026-11-07
> 前置：[2026-11-01-test-ci-hardening-design.md](./2026-11-01-test-ci-hardening-design.md)（nextest 迁移）、
> [2026-11-02-quality-hardening-wave2-design.md](./2026-11-02-quality-hardening-wave2-design.md)（变异测试 / patch coverage / nightly）
> 后续：[2026-07-26-graded-test-system-design.md](../plans/2026-07-26-graded-test-system-design.md) 定义了 Tier 1–3
> 真 LLM 分层，本文不修改那一套，只补「日常本地测试」这一层

## 问题

日常开发中「跑测试」只有一个入口：`just test`，即
`cargo nextest run --all-features --workspace --profile ci`。

2026-11-07 在本机（18 核 / 128 GB / macOS 26.5.1 / Rust 1.98.1）实测：

| 场景 | 实测 |
|---|---|
| 全量 workspace（**5709** 个测试被执行） | **821 秒（13.7 分钟）**，其中 36 个失败 |
| 单独 `-p yi-agent-supervisors` | **37 秒**，451 个全绿 |
| 改 `yi-agent-core` 后 `-p yi-agent-core --no-run` | **43 秒** |
| 改 `yi-agent-core` 后全 workspace `--no-run` | **超过 18 分钟未完成** |

> 口径说明（两个数字都原样保留，因为它们来自不同次观测，我不做无依据的归一）：
> nextest 终端报告 **5709** 个测试被执行；junit 文件里 `testcase` 元素合计 **5913** 个。
>
> 我尝试核对差异来源但**未能确认**：用单 crate 做对照
> （`-p yi-agent-flags`）时终端报 24 个、junit 也是 24 个、`skipped` 为 0，
> 说明 junit 计数与终端计数本应一致。因此 204 的差异**不能**简单归因于 skipped 条目。
> 最可能的解释是两次读到的不是同一次运行的产物（`target/nextest/ci/junit.xml`
> 是固定路径，会被其它会话的运行覆盖），但**我没有证据**，故不写成结论。
>
> 实操上这两个数的差异不影响任何设计判断：它们都是「数千个测试」的量级，
> 而全量耗时 821 秒是两个口径共同支持的观测。下文凡引占比处一律标明分母。

两个结论直接推动本文：

1. **全量的 36 个失败里有 34 个与调度延迟有关，而非被测逻辑错。** 判据有两条：
   (a) 失败耗时精确落在 20/30/45/60/90/120/180 秒等超时预算边界上，不是断言失败；
   (b) 单独跑对应 crate 时全部通过（`-p yi-agent-supervisors` 451/451 绿）。
   注意其中 6 个属 `supervisor` 二进制——它已在 `spawn-heavy` 组内仍失败，
   这条反常直接否掉了「补名单即可修好」的假设，详见 D3。
   本文不用「假红」作为已证的定性结论，只说「与调度延迟有关」。
2. **绝大多数改动不需要跑全量。** 单 crate 测试 37 秒，全量 821 秒。两者口径不同
   （前者热缓存、无并发争用；后者含 **124 个测试二进制**的 `--list` 探测与多会话争用），
   所以不写「差 N 倍」这种精确倍数，只记录量级差异：**分钟级 vs 十几分钟**。

> 124 的推导：`cargo metadata --no-deps` 中带 `test` kind 的 target 有 108 个（集成测试），
> 带 `lib`/`rlib`/`proc-macro` kind 的 16 个（lib 单元测试），
> 合计 16 + 108 = 124 个会被 nextest 探测的测试二进制。
> 集成测试**文件**是 115 个，但 target 是 108 个（部分文件是同 target 的辅助模块）。

更深一层的原因是**测试档位缺失**：没有「改了 X 该跑 Y」的路由，于是每次都在
「跑全量（慢且脏）」和「只跑一个 crate（可能漏）」之间二选一。

### 附加实测：nextest 的 `--list` 探测是独立的一笔开销

这一条在诊断过程中才发现，且影响所有档位，必须单独记录。

nextest 为了枚举用例，会对**每个测试二进制**执行 `--list --format terse` 两次
（一次含 `--ignored`）。在 system load 高时这笔开销会失控。2026-11-07 实测：

| 场景 | 实测 |
|---|---|
| 单个 26MB 二进制**独立**执行 `--list` | **1 秒** |
| 同上二进制的 28 个实例在 nextest 下并发探测 | **超过 17 分钟仍未返回** |
| `cargo nextest run -p yi-agent-core --no-run`（热缓存） | **31 秒**，其中编译 0.09 秒 |
| 同上第二次（页缓存变热） | **0.72 秒** |
| `cargo test --no-run -p yi-agent-flags`（对照组，无 `--list`） | **17.6 秒** |

采样这些卡住进程的调用栈：停在 `_dyld_start`，`rss` 仅 32KB，CPU 0%——
即卡在**从磁盘加载可执行文件**这一步，尚未进入 `main`。

**结论**：`--no-run` 并不省掉探测开销。真正的变量是「多少个二进制同时被加载」，
而这被**多会话并发**放大：实测当时 load 17.5–21（18 核），有 4–5 个 agent 会话
同时在同一个 `target/` 上跑 cargo。

这条结论直接约束设计：
1. L0 用 `cargo check` 而非 nextest，正是为了不产生测试二进制、不触发探测
2. 各档位的收益评估必须承认「多会话并发」这个外部变量，不能把它算作档位的效果
3. 文档必须给出多会话并行的建议（独立 `CARGO_TARGET_DIR`）

## 目标

1. 新增 3 个分级入口（L0/L1/L2），覆盖「改单 crate」「改公共 crate」「保存即验证」三类场景
2. 让假红**可识别、不污染判断**：跑 L3 前检测同 target 的其它 cargo 进程并警告；
   失败时给出「用独占运行复跑该 crate」的判定路径。**不声称能修掉假红**——
   实测证明它是跨会话并发的症状，nextest 配置管不了（见 D3）
3. 保持 `just test` 与 `just ci` 语义不变（CI 三条路径都依赖它们）
4. L1 单跑与 L3 全量在独占运行时对同一份代码必须给出**一致**的通过/失败结论

## 非目标

- **不做**环境治理：`target/` 尺寸、Spotlight 排除、`[profile.dev]` debuginfo、`msrv-check` 的
  `CARGO_TARGET_DIR`、`just gc` 均由另一条独立工作线负责，本文不碰（见「与另一条工作线的边界」）
- **不做** CI 分片：`.github/workflows/ci.yml` 与 `.gitlab-ci.yml` 不改
- **不改** `just ci` 的命令与依赖列表；`just test` 的命令与退出码也不变
  （唯一改动是在 `test` 前加一段只读并发预检输出，见「并发检测」节）
- **不改** `.config/nextest.toml` 的任何现有内容（D4 已决定不改 `max-threads`；
  L0/L1/L2 是新增 recipe，不触碰既有配置）
- **不做**「按 git diff 自动推断受影响测试」的更深推断：只到 crate 粒度，不到测试用例粒度
- **不改** 任何测试的断言、超时预算数值（这意味着跨会话假红不会被真正消除，见 D3）

## 决策记录

### D1：档位划分按「反向依赖闭包」而非「测试数量」

判定用 `git diff` 命中的 crate 的**反向依赖闭包**（谁依赖它，传递闭包），而不是凭直觉。

`cargo metadata --no-deps` + 闭包计算实测（17 个成员）：
| 改动 crate | 闭包大小 | 说明 |
|---|---|---|
| `yi-agent-core` | **12** | 8 个直接依赖 + 传递 |
| `yi-agent-plugins` | **10** | |
| `yi-agent-lifecycle` | **10** | |
| `yi-agent-store` | **9** | |
| `yi-agent-flags` | **8** | |
| `yi-agent-llm` | **7** | |
| `yi-agent-skills` | **7** | |
| `yi-agent-supervisors` | **7** | |
| `yi-agent-mcp` | **6** | |
| `yi-agent-tools` | **6** | |
| `yi-agent-runtime` | **5** | |
| `yi-agent-power` / `relay` / `subagent` | **3** | |
| `yi-agent-app-server` / `web` | **2** | |
| `yi-agent`（根 crate） | **1** | 唯一没有 dependents 的成员 |

所以「改 core 就该跑 12 个 crate」不是懒，是依赖图决定的。分级的价值在**两侧都有**：
改根 crate 只跑 1 个（5 分钟的 app-server 不必陪着跑），改 `power`/`relay` 只跑 3 个。

**必须算闭包，不能只取直接依赖。** 只取直接依赖会让 `yi-agent-core` 得到 8 个而非 12 个，
漏掉 `flags`/`supervisors`/`web`——它们通过 `store`/`runtime` 间接依赖 core。

**但 D1 不是实现依据，而是验收依据。** 实现交给 nextest 的 `rdeps()`（见「L2」节的选型实证），
本表只用来断言 `rdeps` 的输出条数对得上。若某天数字对不上，说明依赖图变了，
需人工复核是合理变化还是实现 bug。

### D2：L2 的改动范围默认对比 main，允许 `--since` 覆盖

不做「自动猜改动影响面」。理由：猜错的代价是**以为测了其实没测**，比多跑几个 crate 贵得多。
默认 `git diff main...HEAD`（三点，比两点干净：排除 main 上的新提交），
`--since <ref>` 可换成任意基准。

若 `git diff` 结果为空（例如在 main 上直接跑），则**报错退出**而不是静默跑 0 个测试。
静默跑 0 个测试是这个仓库已经出现过两次的失败模式（见 `justfile` 里 `eval` recipe
关于「跑了 0 条 case 且退出码 0」的注释）。

### D3：假红的根因是跨会话并发，不是 spawn-heavy 组的名单不全

**这一节推翻了本设计早期版本的判断，保留过程以便复核。**

早期判断是：`spawn-heavy` test-group 只串行化 3 个二进制，漏掉了 `refresh`、`daemon` 等，
把它们补进去即可。**该判断已被证伪**，两条证据：

**证据 1：已在组内的二进制同样失败。**
`supervisor` 二进制（`binary_id(yi-agent-supervisors::supervisor)`）**本来就在** `spawn-heavy`
组里（`max-threads = 1`），但它的 6 个测试仍以 20.014–20.053 秒失败。若「加入组」是解，
这些不该红。

**证据 2：源码注释直接说明了这一点。**
`crates/yi-agent-supervisors/tests/supervisor.rs:18-25` 写道：

> 实测（2026-11-01）：`binary(supervisor)` 全组跑在 nextest 下仍有测试超时，
> 锁帮不上忙——别再往这里加锁来「修」并发问题。

并明确点出它无法看到的负载来源是 `cargo test --workspace` 与 **CI 邻居**。

**真实因果链**（实测吻合）：

1. 我的 821 秒全量跑（23:33–23:47）期间，另一个会话在跑
   `cargo test -p yi-agent-supervisors`（23:25 启动，正是失败范围）
2. 两个进程同时 fork `/bin/sh`，机器上数百个测试进程竞争
3. 子进程在 20 秒 `CHILD_START_BUDGET` 内未被调度
4. 断言拿到 0 次而非 1 次 → 红

`spawn-heavy` 的 `max-threads = 1` 只约束**同一次 nextest 运行内**的并发，
跨进程（另一个 `cargo test`）完全无效。这是它修不了本次假红的原因。

**所以「修掉 34 个假红」这个目标要重新定义**：

- **不能**靠改 nextest 配置达成——配置管不了其它会话
- 可行的做法是**降低对调度延迟的敏感度**：这些测试已用条件轮询 + 宽预算，
  但预算写死在 20 秒等数值上；把它改成可配置的系数（如按 `available_parallelism` 缩放）
  属于「改超时预算数值」，被本文非目标排除
- **本文的结论**：假红是多会话并发的**外部症状**，不是仓库配置缺陷。
  故目标 2 从「修掉 34 个假红」改为「**让假红可识别、不污染判断**」：
  1. 全量跑失败时，文档给出「用独占运行复跑该 crate」的判定路径
  2. 保留 `spawn-heavy` 现状不改（它在本进程内仍有价值），
     但**不再声称它能修跨会话假红**
  3. 新增 doctor 式提示：跑 L3 前检测同 target 的其它 cargo 进程并警告

这实质上把「修复」换成「识别 + 隔离」，因为在不改测试断言与非目标约束下，
让它真正消失不可能。

### D4：`max-threads` 维持 1，不调整

原计划提到 3，依据是「串行不是负担（451 个测试 37 秒）」。但 D3 修正后，
提高并发**不会**解决跨会话假红，只会让本进程内的资源竞争更重。

因此**不改** `max-threads`。这一条从「需要实测调参」降级为「维持现状」，
为本设计减少一项无依据的风险面。

### D5：多会话并发是外部变量，不把它算进档位收益

实测当时 system load 17.5–21（18 核），4–5 个 agent 会话同时在同一个
`yi-agent-rs/target/` 上跑 cargo。后果是 nextest 的 `--list` 探测从 1 秒膨胀到 17 分钟以上。

这不是本设计能修的东西（`target/` 共享是仓库既有约定，`CLAUDE.md` 已记录该现象），
但必须写进文档，否则会犯两类错：

1. **夸大档位收益**：把「并发争用消失」误记成「L1/L2 让测试变快」
2. **误判验证结果**：共享 target 时跑出来的超时失败会被误认为「L1 与 L3 不一致」或「档位有 bug」

因此：
- 各档位的收益陈述必须注明「独占运行时」这个前提
- `just test` 的注释与 `CLAUDE.md` 的 cargo 并发小节都要补一句：多会话并行时用独立
  `CARGO_TARGET_DIR`（本设计不改 `CLAUDE.md`，只在 spec 记录该建议，
  由后续工作线决定是否落入文档）

## 架构

### 档位

```
L0  just check-fast              类型检查，不产生测试二进制
L1  just test-crate <crate>      单个 crate 的测试
L2  just test-affected [--since] 改动 crate + 反向依赖
L3  just test                    全 workspace（现状，不改）
```

不进 L0–L3 的独立档（会 spawn 真进程 / 花钱 / 慢）：

```
just runtime-lifecycle-check   真进程生命周期（现状，不改）
just test-real-llm / -e2e / -complex / -subagent   真 LLM（现状，不改）
just mutants / eval / ci-patch 长跑质量度量（现状，不改）
```

### L0：`check-fast`

```
cargo check --all-targets --all-features
```

为什么是 `--all-targets` 而不是 `cargo check`：默认只检查 lib/bin，**不检查 `tests/`**。
本仓有 **108 个集成测试 target**（`cargo metadata --no-deps` 中 kind 含 `test`，
分布在 13 个 crate 的 `tests/` 目录下），`--all-targets` 才覆盖它们。

**这一点已用对照实验实证**（2026-11-07）：

```
# 在 crates/yi-agent-web/tests/zz_tmp_probe.rs 写入一个类型错误
cargo check -p yi-agent-web --all-features              → 0 个 error   # 漏检
cargo check -p yi-agent-web --all-features --all-targets → error[E0308]  # 检出
```

同上实验在 `--all-targets` 下精确报出
`could not compile 'yi-agent-web' (test "zz_tmp_probe")`，证明覆盖生效。

（早先版本引用过「集成测试占 5913 个 testcase 的 37.0%、占执行时间 39.3%」这类百分比，
但 junit 文件是固定路径、已被其它会话覆盖，**事后无法复核**，故删除这些数字。
`--all-targets` 的必要性不依赖它们，由上面对照实验直接支撑。）

为什么不用 `cargo clippy`：clippy 比 check 慢数倍，L0 的定位是「保存即跑」。
clippy 已经由 `just lint` 承担且进 CI。
（注意 `just lint` 用的正是 `--all-targets --all-features`，可交叉印证这两个参数是必要的。）

**L0 不承诺很快。** 实测 `cargo check --all-targets --all-features` 在
system load 17（18 核，多个会话并发跑测试）时超过 40 分钟未完成。L0 的价值是
「不产出测试二进制」从而不触发 `--list` 探测开销，不是「秒级返回」。
期望值应表述为「比 L1 快」，而不是「秒级」。

若将来确需秒级反馈，那是 `rust-analyzer` 的职责（IDE 内即时诊断），
不是 CLI 档位能提供的。

### L1：`test-crate <crate>`

```
cargo nextest run --all-features -p {{crate}} --profile ci
```

参数名用 `crate` 但接受 `yi-agent-core` 这种带横线的 crate 名（just 的位置参数）。

**必须保留 `--all-features`，这不是可选项，实测不加会直接编译失败**：

`yi-agent-store` 的 `foreground-test-support` feature 门控了测试辅助方法
（`crates/yi-agent-store/src/runtime.rs:2210` 等 8 处 `#[cfg(any(test, feature = "foreground-test-support"))]`），
而 `yi-agent-subagent` 以 `features = ["foreground-test-support"]` 依赖 store。
实测 `cargo check -p yi-agent-store --all-targets`（**不带** `--all-features`）：

```
error[E0599]: no method named `execute_test_scalar_text` found for struct `RuntimeRepository`
error[E0599]: no method named `execute_test_scalar` found for struct `RuntimeRepository`
```

所以 L1 漏掉 `--all-features` 时，至少 `yi-agent-store` 无法编译，直接红。
这条同时说明 L1 与 L3 的 feature 组合必须一致，否则「L1 绿、L3 红」这种假不一致会出现。

### L2：`test-affected [--since]`

四步：

1. `git diff --name-only <base>...HEAD` 取改动文件
2. 由路径前缀映射到 workspace crate（`crates/<name>/src/...` → `<name>`）；
   成员列表由 `cargo metadata --no-deps` 读一次得到
3. 把这些 crate 名拼成 `-E 'rdeps(a) | rdeps(b) | ...'`
4. `cargo nextest run --all-features --profile ci -E '<上面那条>'`

第 3 步用 `rdeps()` 而非手工算闭包，选型实证见本节末尾。

判定顺序是**固定**的，三种结果互斥：

1. **全部改动都是文档**（`docs/`、`*.md`）→ 按仓库既有规则（`CLAUDE.md`：纯文档合并免跑 CI
   和单测）报「无需测试」并 **exit 0**，同时打印判定依据。此条优先于下面的错误分支。
2. **存在改动但都映射不到 crate**（例如只改了 `desktop/`、`plugins/`）→ 打印未映射路径清单并
   **exit 1**。这些不在本 recipe 覆盖范围，静默 exit 0 会让人误以为测过了。
3. **部分映射到 crate** → 跑命中的 crate，并**额外打印**未映射路径，提示这部分未被覆盖。

第 3 种是常见情况（同时改 Rust 与桌面端），此时既要跑命中的 crate，也要让人看见漏了什么。

### L2 的选型实证：用 `rdeps()`，不手工算闭包

`cargo nextest help filterset` 确认 `rdeps(name-matcher)` 的语义是
"Include all tests in crates matching name-matcher, and all the crates that
(**possibly transitively**) depend on name-matcher"，正是需要的。

2026-11-07 四条实证：

1. **语法可用**：`cargo nextest list --all-features -E 'rdeps(yi-agent-flags)'` 正常返回，8.14 秒
2. **闭包正确**：不加 `-p` 时它自行扩展，列出的 crate 恰好是
   flags / runtime / tools / supervisors / subagent / web / app-server / yi-agent 共 **8 个**，
   与 D1 表一致
3. **不滥编无关 crate**：改 `crates/yi-agent-flags/src/lib.rs` 后跑同一条命令，
   `Compiling` 行**只有上述 8 个**，没有 core/store/llm/mcp/skills 等闭包外成员
4. **不比手工慢**：该次 67 秒（含 30 秒编译），远低于全量

选 `rdeps()` 而非手工拼 `-p` 列表的理由：手工要维护 `cargo metadata` 解析脚本，
新增 crate 时易漏；`rdeps` 由 nextest 自己维护依赖图，不会漂移。

**因此 D1 的闭包表是验收依据、不是实现依据**——用来断言 `rdeps` 输出条数对得上
（改 core 得 12、改 relay 得 3）。数字对不上就人工复核是依赖图变了还是实现 bug。

**仍需手工做的部分**：第 2 步「路径 → crate 名」要 workspace 成员列表
（`cargo metadata --no-deps` 读一次），否则无法把 `crates/foo/src/x.rs` 认成 crate `foo`。
这一步与 `rdeps` 无关，必须实现。


### 并发检测：`just test` 的前置警告（替代「修假红」）

D3 的结论是假红无法在配置层修掉，因此改为**让它在发生时被看见**。

**检测方式已实证选型**（2026-11-07 实测）：

| 候选 | 结果 | 结论 |
|---|---|---|
| `lsof +D target/debug` | 无输出、rc=0 | **不可用**：目录遍历慢，且只在编译期持锁时才有条目 |
| `pgrep -f cargo` | 命中 6 条，其中多数是 `sh -c` 包装 | **不可用**：误报严重 |
| `pgrep -x cargo` / `pgrep -x cargo-nextest` | 精确列出真实进程（pid 64125 cargo） | **采用** |

所以实现用 `pgrep -x cargo` 与 `pgrep -x cargo-nextest`（可再加 `rustc`），
用进程名精确匹配而非命令行模糊匹配，避免把 shell 包装当成并发会话。

命中后打印警告并列出 PID 与命令，**但不阻断**（阻断会让 CI 与既有习惯失效）。

警告文案必须包含两个信息：
1. 有 N 个其它 cargo 进程存在于本机
2. 若本次出现超时类失败，先用独占运行复跑对应 crate 再判断

**已知局限（必须在文案里体现，不能假装精确）**：`pgrep -x` 匹配的是**整机**的 cargo 进程，
无法判断对方的 `CARGO_TARGET_DIR` 是否就是本目录。因此文案应说「本机上另有 N 个 cargo 进程」，
而不是「它们正在占用同一个 target/」。后者需要 `lsof <target>/{debug,release}/.cargo-lock`，
但该锁只在编译期持有、测试执行期为空，会漏报（上表已实测）。

「不阻断」是有意的：CI 上本来就只有自己一个进程，警告不会触发；本地多会话是常态，
阻断会让人养成加 `--no-verify` 式的绕过习惯。

**这一项是本设计里唯一「改既有行为」的改动**（在 `test` recipe 前加一段预检）。
若评审认为风险偏高，可退化为独立 recipe `just test-guarded`，把选择权交给使用者。

### 与 `just test` / `just ci` 的关系

`just test` 的命令与依赖不变；唯一变化是执行前多一段并发预检输出（见上）。
`just ci` 完全不变——CI 上不存在多会话，预检不会命中。

## 错误处理

| 情况 | 行为 |
|---|---|
| L1 crate 名不存在 | 先校验 workspace 成员，报错并列出可用 crate 名，exit 1 |
| L2 `git diff` 为空 | 报错说明「没有可推断的改动」并提示用 `--since`，exit 1 |
| L2 改动**全是文档** | 报「无需测试」并打印判定依据，**exit 0**（优先于下面的错误分支） |
| L2 有改动但全映射不到 crate | 打印未映射路径清单，exit 1（不静默跑 0 个测试） |
| L2 部分映射到 crate | 跑命中的 crate，并额外打印未映射路径 |
| `cargo metadata` 失败 | 直接透传失败，不吞 |

## 测试

本文的改动是**构建配置 + recipe**，没有可单测的 Rust 逻辑。验证方式是实测。

**所有验证项必须在独占运行时执行**（无其它会话在同一 `target/` 上跑 cargo），
否则测到的是外部争用（D5）。

| 验证项 | 方法 | 通过判据 |
|---|---|---|
| L1 在独占下全绿 | 独占时跑 `just test-crate yi-agent-supervisors` | 451 全绿（已实测过一次，作为基线） |
| L1 与 L3 一致 | 独占下分别跑 L1 与 L3，对比该 crate 的用例结果 | 完全一致 |
| L2 闭包正确 | 改 `yi-agent-core` 一个文件后跑 `--dry-run`/`list` | 输出 crate 集合为 **12** 个，清单打印 |
| L2 反向验证 | 改 `yi-agent-relay` 后跑同一命令 | 命中 **3** 个（relay + app-server + yi-agent） |
| L2 不滥编 | 改 `flags` 后观察 `Compiling` 行 | 只有 8 个闭包成员，无 core/llm/mcp 等 |
| L2 空 diff | 在 main 上跑 `just test-affected` | exit 1 并说明原因 |
| L2 无法映射 | 只改 `desktop/` 后跑 | exit 1 并列出未映射路径 |
| L0 覆盖 tests/ | 在 `crates/yi-agent-web/tests/` 放一个类型错误 | 不带 `--all-targets` 报 0 错、带上报错（**已实测通过**） |
| 纯文档改动 | 只改 `docs/` 后跑 L2 | exit 0 并报「无需测试」 |
| 并发检测生效 | 另起一个 `cargo test -p yi-agent-flags` 后跑 L3 | 启动时打印「本机另有 N 个 cargo 进程」 |
| 不改现有配置 | `git diff main -- .config/nextest.toml` | 输出为空 |

**不再有的验证项**（随 D3/D4 修正删除）：
- ~~连续 3 次全量 0 失败~~——在无法独占的机器上不可控，且本设计不声称修假红
- ~~nextest 配置合法（改配置后）~~——本设计不改 `.config/nextest.toml`

## 与另一条工作线的边界

项目所有者另有并发会话负责「环境治理」，本文**明确不碰**以下文件：

| 文件 | 该工作线可能做 |
|---|---|
| `yi-agent-rs/Cargo.toml`（`[profile.dev]`） | debuginfo / split-debuginfo |
| `yi-agent-rs/scripts/check-msrv.sh` | 独立 `CARGO_TARGET_DIR` |
| `justfile` | 新增 `gc` recipe（回收入口） |
| `target/.metadata_never_index` | Spotlight 排除 |

**冲突点**：`justfile` 两边都会改。本文对 `justfile` 的改动分两类，必须区分：

1. **纯新增 recipe**（`check-fast`、`test-crate`、`test-affected`）——零冲突风险
2. **在 `test` recipe 前加一段只读并发预检**——这是本文唯一触碰既有 rule 的地方，
   且只增加输出、**不改命令与退出码**。合并时若冲突，后合者 rebase

`test` recipe 的预检如果不被接受，退化为独立 `just test-guarded`（见「并发检测」节），
此时本文对 `justfile` 的改动回落为第 1 类纯增量。

**其它需要同步的位置**：`just --list` 的分组注释、`CLAUDE.md` 的「cargo test 执行」小节
若要点明档位入口，也属可选增强，不阻塞本设计。
