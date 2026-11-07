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
| 全量 `-p` workspace（5709 个测试） | **821 秒（13.7 分钟）**，其中 36 个失败 |
| 单独 `-p yi-agent-supervisors` | **37 秒**，451 个全绿 |
| 改 `yi-agent-core` 后 `-p yi-agent-core --no-run` | **43 秒** |
| 改 `yi-agent-core` 后全 workspace `--no-run` | **超过 18 分钟未完成** |

两个结论直接推动本文：

1. **全量的 36 个失败里有 34 个是假红。** 同一批测试单独跑 451/451 全绿。失败耗时精确落在
   15/20/30/45/60/90/120/180 秒，是超时预算边界而非断言失败——典型信号是子进程没被调度到，
   不是被测逻辑错。
2. **绝大多数改动不需要跑全量。** 单 crate 测试 37 秒，全量 821 秒，差 22 倍。

更深一层的原因是**测试档位缺失**：没有「改了 X 该跑 Y」的路由，于是每次都在
「跑全量（慢且脏）」和「只跑一个 crate（可能漏）」之间二选一。

## 目标

1. 新增 3 个分级入口（L0/L1/L2），覆盖「改单 crate」「改公共 crate」「保存即验证」三类场景
2. 修掉 34 个假红，让「绿灯」重新可信——**这是前提**，假红存在时任何档位的绿灯都不算数
3. 保持 `just test` 与 `just ci` 语义不变（CI 三条路径都依赖它们）
4. L1 单跑与 L3 全量对同一份代码必须给出**一致**的通过/失败结论

## 非目标

- **不做**环境治理：`target/` 尺寸、Spotlight 排除、`[profile.dev]` debuginfo、`msrv-check` 的
  `CARGO_TARGET_DIR`、`just gc` 均由另一条独立工作线负责，本文不碰（见「与另一条工作线的边界」）
- **不做** CI 分片：`.github/workflows/ci.yml` 与 `.gitlab-ci.yml` 不改
- **不改** `just test` / `just ci` 的命令与依赖列表
- **不做**「按 git diff 自动推断受影响测试」的更深推断：只到 crate 粒度，不到测试用例粒度
- **不改** 任何测试的断言、超时预算数值

## 决策记录

### D1：档位划分按「编译扇出」而非「测试数量」

`cargo metadata --no-deps` 实测反向依赖：

| crate | 被谁依赖 | 改动后需跑的 crate 数 |
|---|---|---|
| `yi-agent-core` | app-server, llm, mcp, runtime, store, subagent, tools, yi-agent | **9** |
| `yi-agent-store` | app-server, flags, runtime, subagent, supervisors, web, yi-agent | **8** |
| `yi-agent-tools` | app-server, runtime, subagent, yi-agent | **5** |
| `yi-agent-supervisors` | app-server, runtime, subagent, tools, yi-agent | **6** |
| `yi-agent-relay` | 无 | **1** |

所以「改 core 就该跑 9 个 crate」不是懒，是依赖图决定的。分级的价值在于：
改叶子 crate（relay / flags / power / skills / web / mcp）时能从 17 个 crate 收敛到 1–2 个。

### D2：L2 的改动范围默认对比 main，允许 `--since` 覆盖

不做「自动猜改动影响面」。理由：猜错的代价是**以为测了其实没测**，比多跑几个 crate 贵得多。
默认 `git diff main...HEAD`（三点，比两点干净：排除 main 上的新提交），
`--since <ref>` 可换成任意基准。

若 `git diff` 结果为空（例如在 main 上直接跑），则**报错退出**而不是静默跑 0 个测试。
静默跑 0 个测试是这个仓库已经出现过两次的失败模式（见 `justfile` 里 `eval` recipe
关于「跑了 0 条 case 且退出码 0」的注释）。

### D3：假红修复用整 crate 匹配，不用逐个二进制名

现状 `filter` 只列了 3 个二进制：

```
binary_id(yi-agent-supervisors::supervisor)
+ binary_id(yi-agent-supervisors::managed_launch)
+ binary_id(yi-agent::plugins_maintenance_daemon)
```

漏掉的是 `refresh`、`refresh::describe`、`daemon`、`managed_bringup`、`managed_channel`、
`runtime_switch`、`client_reclamation`、`execution_admission`、`guardian_*`、`runtime_*` 等，
它们同样 fork 子进程并等待 10–30 秒预算。这次 34 个假红正是这些二进制贡献的。

逐个补名字必然再漏，改为按 crate 匹配：`package(yi-agent-supervisors) + package(yi-agent-runtime)`。

### D4：`max-threads` 从 1 提到 3，用实测定，不靠推测

串行不是负担的实测依据：`-p yi-agent-supervisors` 451 个测试串行执行 37 秒。
同时串行化整个 crate（不只 3 个二进制）后，全量跑的墙钟时间会上升，`max-threads = 3` 是
在「不再饿死子进程」和「不把 wall clock 拉太长」之间的折中，**必须先实测确认 0 假红**。
若 3 仍出现调度延迟假红，退回 1；若有实测证据支持更高值，可提。

**验收方式**：连续跑 3 次全量 `just test`，要求 0 失败。一次不够——这类调度延迟是概率性的。

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

为什么是 `--all-targets` 而不是 `cargo check`：默认只检查 lib/bin，不检查 `tests/` 和
`benches/`。本仓 106 个集成测试文件占测试总量的大头，漏掉它们等于没检查。

为什么不用 `cargo clippy`：clippy 比 check 慢数倍，L0 的定位是「保存即跑」。
clippy 已经由 `just lint` 承担且进 CI。

### L1：`test-crate <crate>`

```
cargo nextest run --all-features -p {{crate}} --profile ci
```

参数名用 `crate` 但接受 `yi-agent-core` 这种带横线的 crate 名（just 的位置参数）。

**必须保留 `--all-features`**：不加会得到与 CI 不同的 feature 组合，导致依赖在两种变体间
反复重建（实测 `-p` 与 `--workspace` 的 feature 统一结果不同，改 core 后连 `tokio` 都重编）。

### L2：`test-affected [--since]`

三步：

1. `git diff --name-only <base>...HEAD` 取改动文件
2. 由路径前缀映射到 workspace crate（`crates/<name>/src/...` → `<name>`）
3. 用 `cargo metadata --no-deps` 算反向依赖闭包，`-p` 一起跑

判定顺序是**固定**的，三种结果互斥：

1. **全部改动都是文档**（`docs/`、`*.md`）→ 按仓库既有规则（`CLAUDE.md`：纯文档合并免跑 CI
   和单测）报「无需测试」并 **exit 0**，同时打印判定依据。此条优先于下面的错误分支。
2. **存在改动但都映射不到 crate**（例如只改了 `desktop/`、`plugins/`）→ 打印未映射路径清单并
   **exit 1**。这些不在本 recipe 覆盖范围，静默 exit 0 会让人误以为测过了。
3. **部分映射到 crate** → 跑命中的 crate，并**额外打印**未映射路径，提示这部分未被覆盖。

第 3 种是常见情况（同时改 Rust 与桌面端），此时既要跑命中的 crate，也要让人看见漏了什么。

### 与 `just test` / `just ci` 的关系

不改动。`just test` 保持 `--all-features --workspace --profile ci`；`just ci` 依赖列表不变。
新增 recipe 是**纯增量**，任何一条出错都能单独 revert 而不影响 CI。

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

本文的改动是**构建配置 + recipe**，没有可单测的 Rust 逻辑。验证方式是实测：

| 验证项 | 方法 | 通过判据 |
|---|---|---|
| 假红已修 | 连续 3 次 `just test` | 每次 0 失败 |
| L1 与 L3 一致 | 同一份代码跑 `-p yi-agent-supervisors` 与全量，对比该 crate 的用例结果 | 完全一致 |
| L2 路由正确 | 改 `yi-agent-core` 一个文件后跑 `just test-affected --since main` | 命中 9 个 crate，打印清单 |
| L2 空 diff | 在 main 上跑 `just test-affected` | exit 1 并说明原因 |
| L0 覆盖 tests/ | 故意在 `tests/` 放一个类型错误 | `check-fast` 报错（证明 `--all-targets` 生效） |
| 纯文档改动 | 只改 `docs/` 后跑 L2 | exit 0 并报「无需测试」 |

## 与另一条工作线的边界

项目所有者另有并发会话负责「环境治理」，本文**明确不碰**以下文件：

| 文件 | 该工作线可能做 |
|---|---|
| `yi-agent-rs/Cargo.toml`（`[profile.dev]`） | debuginfo / split-debuginfo |
| `yi-agent-rs/scripts/check-msrv.sh` | 独立 `CARGO_TARGET_DIR` |
| `justfile` | 新增 `gc` recipe（回收入口） |
| `target/.metadata_never_index` | Spotlight 排除 |

**冲突点**：`justfile` 两边都会改。本文遵守「只新增 recipe、不改既有 rule」，
合并时若冲突，后合者 rebase。
