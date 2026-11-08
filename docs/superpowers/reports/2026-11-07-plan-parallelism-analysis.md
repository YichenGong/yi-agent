# 单个 Plan 内部的并行空间分析（最近 10 份 Plan）

日期：2026-11-07
问题：**一份 plan 内部，它的 Task 之间有没有并行空间？**
范围：`docs/superpowers/plans/` 按修改时间最新的 10 份实现计划。

---

## 0. 结论

1. **有空间，但远小于任务数量给人的印象。** 10 份 plan 共 93 个 Task，按各自声明的依赖关系算出的**上限加速只有 1.0x–3.0x**，中位数约 1.2x。也就是说大多数 plan 的横向并行空间很小，**纵向串行链才是主体**。
2. **决定并行宽度的是"共享文件"，不是"任务数"。** 用 Files 清单做交集后，真正能同时开工的任务对极少：10 份 plan 里有 4 份（interaction-latency、foreground-root、remote-acceptance、side-conversation 前段）存在写集完全不相交的并行组，而 shared-http-client、graded-test-tiers、ios-qr、managed-runtime 四份的并行面被单一热点文件吃掉。
3. **最好的两份是 transcript-window（3.0x）和 interaction-latency（2.0x）。** 它们的共同点是：前半段是彼此独立的纯函数/新模块，后半段才汇合到组件或调用方。这正是任务级并行唯一可靠的形态。
4. **最大的并行机会浪费在 side-conversation。** 它的前端链（Task 1–7）与 TUI 链（Task 8–10）写集交集为 **0**，本可整体并行，但计划按 1→11 串行排列，上限 1.0x。
5. **一个被忽视的隐形串行化因素：测试文件。** shared-http-client 的 Task 2/3/4 三个任务都往同一个 `tests/shared_client.rs` 末尾追加内容。文件清单看起来是"每个任务各改各的"，实际全撞在同一个文件的同一段，这才是这类计划不能并行的真因。

---

## 1. 逐份 Plan 的并行空间

"上限加速"= Task 数 ÷ 关键路径长度，假设任务耗时相等、并行无额外成本。它是**理论天花板**，实际还会被下文的阻塞项进一步压低。

| # | Plan | Task | 关键路径 | 上限加速 | 可并行的任务组 | 主要限制 |
|---|---|---|---|---|---|---|
| 1 | shared-http-client | 5 | 3 | **1.67x** | 弱：2 / 3 / 4 都依赖 Task 1，互相只共享测试文件 | 三个任务全往 `tests/shared_client.rs` 追加 |
| 2 | interaction-latency | 6 | 3 | **2.00x** | **Task 4 + Task 5 可与 1→2→3 并行** | 无文件交集，仅共用 `Cargo.toml` |
| 3 | ios-qr | 5 | 5 | **1.00x** | 无 | `PairingScreen` 是全链热点 |
| 4 | graded-test-tiers | 9 | 7 | **1.29x** | 弱：Task 4（concurrency/check）与 Task 1/2/3 不相交 | Task 1–4 全改 `test-tiers.sh` 一个文件 |
| 5 | transcript-window | 12 | 4 | **3.00x** | **Task 2 ∥ 3 ∥ 6 ∥ 12 可同时开工；4 ∥ 5 已声明** | Task 7–10 汇合到 `ChatView` |
| 6 | session-subagent | 11 | 11 | **1.00x** | Task 9 ∥ Task 10（作者已声明）实测写集不相交 | 计划写死"按依赖串行 Task 1–8" |
| 7 | managed-runtime | 11 | 11 | **1.00x** | 无（明确声明"纵向修复，不是互不依赖的新子系统"） | 20 对任务共享文件，Task 1 的 DTO 是全局前置 |
| 8 | foreground-root | 11 | 9 | **1.22x** | **5 ∥ 6、8 ∥ 9（依赖图已写明）** | 计划把图写成串行检查表 |
| 9 | remote-acceptance | 6 | 5 | **1.20x** | **Task 4 独立**（作者已声明） | 只有 6 个任务，基数小 |
| 10 | side-conversation | 11 | 11 | **1.00x** | **前端链 1–7 ∥ TUI 链 8–10，写集交集 0** | 计划按一维顺序排列，没有分支结构 |

### 1.1 关键路径是怎么算的

依据是每份 plan 自带的 `Consumes:` 字段、显式的 `依赖图:` 段，以及 `顺序与依赖` 章节。举例：

- **transcript-window** 的 Task 1/2/6/12 都写着 `Consumes: 无`，属于零依赖起点，可以同时开工；Task 7 之后才收口到 `ChatView`。所以关键路径是 `1/2/3 → 5 → 7 → 8/9`，长度 4，12 个任务能压到 4 层。
- **foreground-root** 的依赖图 `1→2→3→4；4→5、6；6→7；2/6/7→8；3/6/7→9；8/9→10；→11` 是这批里唯一写成显式图的，最长链是 `1→2→3→4→6→7→8→10→11`，长度 9。
- **session-subagent / managed-runtime / side-conversation** 没有给依赖图，只给了任务顺序，我按声明顺序当作链式处理，所以是 1.00x。这三份的实际并行空间可能大于 1.00x（见 §2.3 的 side-conversation），但**计划本身没有表达出来**。

---

## 2. 四个真实的并行点，逐个拆开

### 2.1 transcript-window —— 并行度最高（3.0x）

```dot
Task 1 (nextWindow)   ┐
Task 2 (HeightBook)   ├─→ Task 4 (searchIndex) ─→ Task 11 ─┐
Task 3 (itemText)     ┘                                    │
Task 5 (hook) ──────────────────────────────────┐          ├─→ Task 7 ─→ 8 ─→ 9 ─→ 10
Task 6 (compensation) ──────────────────────────┴──────────┘
Task 12 (perf script) 独立
```

理由：Task 1/2/6/12 的 `Consumes` 全是"无"；Task 3 只消费既有 `Item` 类型。这四个任务各自新建独立文件（`transcriptWindow.ts`、`heightBook.ts`、`itemText.ts`、`scrollCompensation.ts`），写集互斥。

计划自己只声明了"4 与 5 可并行"，**漏掉了开头的 1/2/3/6 四路并行**，这是这批 plan 里最可惜的一处：12 个任务本可以 4 层跑完。

限制：从 Task 7 起全部汇合到 `ChatView.tsx` 与 `ChatView.test.tsx`（7/8/9/10 两两共享测试文件），后半段必须严格串行。

### 2.2 interaction-latency —— 干净的 2.0x

```dot
Task 1 (walk) ─→ Task 2 (grep) ─┐
                Task 3 (glob) ──┴─→ Task 6 (e2e)
Task 4 (bash cwd) ───────────────┘
Task 5 (system prompt) ──────────┘
```

Task 4 只改 `shell/bash.rs`，Task 5 只改 `bootstrap.rs` 与 `system_prompt.rs`，与 walker 链**零文件交集**。Task 1 头部写明 `Consumes: 无`。

限制：三者共用 `yi-agent-tools/Cargo.toml`（Task 1 加 `ignore`、tokio rt，Task 4 改 schema 描述）。这是**同文件不同行**的冲突：并行时需约定由 Task 1 独享 `Cargo.toml` 的修改权，Task 4/5 不碰。

### 2.3 side-conversation —— 被顺序排列掩盖的 0 交集

```
前端链  Task 1 → 2 → 3 → 4 → 5 → 6 → 7     （19 个文件，全在 desktop/）
TUI 链  Task 8 → 9 → 10                     （3 个文件，全在 crates/yi-agent/）
汇合    Task 11
```

实测两条链的文件交集为 **0**，前端链没有任何 `.rs`，TUI 链没有任何 `desktop/` 路径。按依赖关系，两条链可以从第 1 分钟就同时开工，把 11 个任务压到 5 层（6.5 个任务的前端链为关键路径）。计划写成一维的 1→11，实际执行也是串行的。

**这是本批 10 份里最大的一处并行浪费。**

### 2.4 foreground-root 与 remote-acceptance —— 依赖图已经写好了，只是没人用

- foreground-root：`Task 4 → 5、6` 与 `Task 8 ∥ 9` 都是分叉，图在计划第 74 行明写。计划的实际执行纪律却是一张 1→11 的顺序勾选表。
- remote-acceptance：第 135 行明写"Task 4 独立，可与 1–3 并行"。

这两份不需要重新设计，**只要把已有的依赖图直接当成并行批次执行**。

---

## 3. 为什么并行空间比任务数小得多

### 3.1 单一热点文件吃掉整段并行

| Plan | 热点文件 | 被吃掉的任务 |
|---|---|---|
| ios-qr | `PairingScreen.tsx` / `.test.tsx` | Task 2、3、4 两两共享测试文件 |
| graded-test-tiers | `scripts/test-tiers.sh` | Task 1–4 全改这一个脚本 |
| transcript-window | `ChatView.tsx` / `ChatView.test.tsx` | Task 7–10 |
| side-conversation | `desktop/src/App.tsx` | Task 1、5、7 |
| managed-runtime | `local_management.rs` / `runtime_management.rs` | Task 2、4、6、7 |

模式很清楚：**只要计划把工作按"功能切片"分任务（每个任务贯穿层），热点文件必然被反复改；按"层"或"模块"分任务才有并行空间。** transcript-window 前段之所以能并行，正是因为它是按模块切的（每个纯函数一个文件）；后半段按功能切（窗口渲染）就串行了。

### 3.2 "追加到同一测试文件" 是最隐蔽的冲突

shared-http-client 的 Task 2、3、4 的 Files 清单看起来只有 Task 1 一个共同依赖：

- Task 2：`Modify tests/shared_client.rs`
- Task 3：`Modify src/openai/client.rs`、`src/anthropic/client.rs`（但 Step 1 写着"在 `tests/shared_client.rs` 追加"）
- Task 4：`Modify src/openai/channel.rs`、`src/openai/client.rs`、`tests/shared_client.rs`

Task 3 的 Files 里**没有列测试文件，但正文要求在测试文件里追加测试**。这种"清单与实际改动的错位"会让基于文件清单做的并行判断失效。三个任务全往文件末尾追加，git 合并冲突率接近 100%。

**判断并行时必须用正文里的 Step 命令，不能只用 Files 清单。**

### 3.3 逐任务全量门禁把可并行的任务也排成串行

6 份 plan 要求每个任务收尾跑 `just ci`（`justfile` 里 23 项依赖，含全量 nextest，设计文档记录约 821 秒）。两个写集完全不相交的任务，也必须在一个 worktree 里排队等各自的 `just ci`。side-conversation 计划里写得很直白："Rust 使用 worktree 自己的 target，**不并发抢同一个 target 构建锁**"。

这一条不改变依赖关系，但**单独就足以让 1.67x 的上限退回 1.0x**。

### 3.4 执行流程本身禁止并行派发

`superpowers:subagent-driven-development` 写着 "Never dispatch multiple implementation subagents in parallel (conflicts)"。所以无论 plan 写得多么可并行，只要走 SDD 单会话流程，执行层永远只有一个写代理。**任务级并行在现有流程里的正确落地方式是：把可并行的任务组拆成独立的 worktree + 独立会话，而不是在一个会话里开两个 implementer。**

---

## 4. 任务级并行的判定方法

### 4.1 四条判据，必须全满足

| # | 判据 | 怎么查 | 只满足前三条的反例 |
|---|---|---|---|
| 1 | 上游契约已提交，不是"即将新增" | 两个任务都 `Consumes` 同一个**已存在**的类型 | shared-http-client Task 1 未提交时，2/3 会各自猜 `ClientKey` 字段 |
| 2 | 写集互斥（用正文 Step，不用 Files 清单） | 提取每个 Step 的 `Create`/`Modify` 与追加位置 | Task 3 的 Files 没写测试文件，但正文要追加 → 假互斥 |
| 3 | 无共享可变状态 | 不并发改同一份迁移/schema/serialization 契约 | session-subagent 的 v19 迁移与持久 CAS |
| 4 | 门禁可分段 | 各自能跑 `just test-crate <crate>` | 任何要求每任务全量 `just ci` 的计划 |

### 4.2 每份 plan 的并行批次建议（可直接套用）

| Plan | 建议批次 | 预计关键路径 |
|---|---|---|
| transcript-window | B1: 1 ∥ 2 ∥ 3 ∥ 6 ∥ 12；B2: 4 ∥ 5；B3: 7；B4: 8；B5: 9；B6: 10；B7: 11 | 4（现状 4，但前段实际可 4 路并发） |
| interaction-latency | B1: (1→2→3) ∥ 4 ∥ 5；B2: 6 | 3 |
| side-conversation | B1: (1→2→3→4→5→6→7) ∥ (8→9→10)；B2: 11 | 7（现状 11） |
| foreground-root | B1: 1→2→3→4；B2: 5 ∥ 6；B3: 7；B4: 8 ∥ 9；B5: 10；B6: 11 | 9（结构已写好，直接照做） |
| remote-acceptance | B1: (1→2→3) ∥ 4；B2: 5；B3: 6 | 5 |
| session-subagent | B1: 1→…→8；B2: 9 ∥ 10；B3: 11 | 10 |
| shared-http-client | B1: 1；B2: 2 ∥ 3 ∥ 4（**需先拆分 `tests/shared_client.rs` 为每任务一个文件**）；B3: 5 | 3 |
| graded-test-tiers | B1: (1→2→3) ∥ 4；B2: 5；B3: 6；B4: 7；B5: 8→9 | 6 |
| ios-qr | 保持串行 | 5 |
| managed-runtime | 保持串行 | 11 |

---

## 5. 要让 Task 级并行真正落地，只需改三件事

1. **测试文件按任务拆分**（成本最低、收益最确定）。把 `tests/shared_client.rs` 拆成 `shared_client_key.rs` / `shared_client_provider.rs` / `shared_client_channel.rs`，shared-http-client 就能从 1.67x 真正达到 1.67x，而不是因为冲突退回 1.0x。
2. **每任务门禁降为 `just test-crate`，汇合点与合并前各跑一次 `just ci`。** CLAUDE.md 已有的 L0/L1/L2 分级入口正是为此设计的，只是计划模板没采用。
3. **Plan 里加"并行批次"一节**，格式直接沿用 foreground-root 那种 `Task N → M、N` 的写法，并标注每批的写集。有这一节，执行时才不用每次重新推依赖。

---

## 6. 局限

- 第 1 节的"上限加速"是按任务等权估算的天花板，实际受任务体量差异影响很大（例如 transcript-window 的 Task 1 有 560 行，Task 12 只有 60 行，等权会高估并行收益）。
- 文件交集只统计计划文本里出现的路径；实际执行时的"顺手改动"（新增 mock 文件、`lib.rs` 导出）不在统计内，真实冲突面比表中更大。
- session-subagent、managed-runtime、side-conversation 三份没有显式依赖图，我按声明顺序当作链式处理。它们的真实依赖散落在各任务的 `Consumes` 里，可能有比 1.00x 更好的结构（side-conversation 已确认有 2 条独立链）。
- 10 份中有 2 份（managed-runtime 未实施、session-subagent 部分完成）的并行判断是纯静态分析，无执行观测佐证。

---

## 7. 开发方式：一棵主 worker tree + 若干子 worker tree，最后合并

### 7.1 结论

**方向对，但"一棵主树 + 子树最后合并"这个模型要改一处：不能有"主树"。**

如果设一棵"主 worker tree"当基准分支，其余子树最后都合回它，那么这棵主树在别人没合完之前**不能发布、不能跑全量 CI 的最终结论**，它就成了串行瓶颈；而且子树的 base 是主树的某个中间提交，主树自己继续往前推时，子树就得不断 rebase。这正是当前 `.worktrees/` 里 21 棵树、其中一棵 target 已经涨到 23G 的现状来源。

正确的形态是：**所有 worker tree 平行地从同一个主干提交分叉，各自独立跑测试，最后由一个专门的"合入"动作依次合回主干。**主干只接收已完整的成果，不长在任何一个 worker tree 里。

### 7.2 这套方式对本项目的实际加速比

上限由 §1 算出（计划的横向并行度），实际还要乘上一个**编译开销折损**：

| 因素 | 实测/推算 | 影响 |
|---|---|---|
| 每棵树独立 `target/` | 主仓库 10G，已有一棵树 23G | 每多一棵树，多一次全量编译，约十几分钟 |
| 共享 target 是否可行 | 项目无 `.cargo/config.toml` 统一 target，`just ci` 含 `build --workspace`、`nextest --workspace` | 两棵树共用 target 会撞 `.cargo-lock`，只能串行排队，等于没并行 |
| 机器能力 | 18 核 / 137G 内存 | 内存和 CPU 够跑 3 棵树编译 |
| 磁盘 | 剩余 163G，`.worktrees/` 已占 141G | **这是当前最紧的约束**，3 棵树 × 20G 已经吃满 |

所以实际加速 ≠ 1.67x 或 3.0x，而是：

```
实际加速 = 计划并行度 ÷ (1 + 每棵子树的全量编译开销 / 该分支任务总耗时)
```

以 side-conversation 为例：前端链 19 个文件、TUI 链 3 个文件，两棵树并行。前端链本来就以 vitest 为主（秒级），第二棵树省下的墙钟时间会被"这棵树要从零 `cargo build --workspace`"吃掉一大半。**纯前端或纯脚本的分支才真正划算；两条链都重 Rust 时，并行收益会明显缩水。**

### 7.3 值得用这套方式的三个场景（按收益排序）

| 场景 | 为什么划算 | 对应 plan |
|---|---|---|
| 两条**技术栈不同**的链 | 一棵跑 vitest/tsc、一棵跑 cargo，几乎没有编译争用 | side-conversation 前端链 ∥ TUI 链 |
| 前段是**独立纯函数/新模块** | 每棵树只编译一个 crate（`cargo test -p`），不是全 workspace | transcript-window 的 Task 1/2/3/6/12 |
| 一棵树做**长时间门禁**、另一棵做**实现** | 门禁的 821 秒被藏进实现时间里 | 任意 |

不值得用：ios-qr（5 个任务全在 `PairingScreen`）、graded-test-tiers（全在 `test-tiers.sh`）、managed-runtime（20 对任务共享文件）。这三份拆树只会增加合并冲突和磁盘占用。

### 7.4 落地时的四条硬约束

1. **子树必须从主干同一提交分叉，不能从另一棵树分叉。** 否则你需要"主树"当锚点，就回到了 7.1 说的串行瓶颈。
2. **每棵树只跑 `just test-crate <受影响的 crate>`，不要跑 `just ci`。** 全量 `just ci` 每棵树十几分钟且互不共享编译产物，是这套方式最大的成本项。
3. **合并顺序按写集冲突数排，把冲突最多的那棵放到最后合。** 例如 side-conversation 的两条链交集为 0，谁先合都行；但只要有交集（如 `App.tsx`），就让改动面大的先合，小的后合去解冲突。
4. **合完在主干上再跑一次 `just ci`，这次的结果才是可信的。** 每棵树自己的绿灯只证明"这棵树是绿的"，不证明"合并结果是绿的"。

### 7.5 磁盘前提

当前 `.worktrees/` 已占 141G、磁盘只剩 163G。在做"多棵树并行"之前，先清理已完成/已废弃的 worktree（`git worktree remove`），否则加两三棵树就会把磁盘吃满，编译失败的原因会和并行开发本身混淆。

判断标准很简单：一棵树的 `yi-agent-rs/target` 单独就有 10–23G，**并行 N 棵树就要预留 N × 20G**。
