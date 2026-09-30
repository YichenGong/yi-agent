# Kanban 看板插件设计

**日期：** 2026-09-30
**状态：** 设计已确认，待评审
**模块：** `yi-agent-kanban`（新增）+ `kanban` system skill + 两端 UI

---

## 1. 背景与问题

用户是管理者，不能持续盯守编码环境。已使用的大模型服务随时段变化限流：

- 工作日 09:00–24:00：800 req/h（≈ 最多 3 个任务并发）
- 工作日 00:00–09:00：不限流
- 周六、周日：不限流

诉求：把**已决策完成的需求**（一对 spec + plan 文件）排成队列，按时段并发上限自动往下
执行，尽量打满机器；用户不需要监控每一个任务，只在真正需要决策时才介入。

因为该方案高度个性化，它必须：

1. 作为插件存在，不与主流程耦合，保证主流程干净；
2. 因是特化方案，允许与 Superpowers 的业务流程（SDD）强绑定。

---

## 2. 已确认的设计决策

| # | 决策点 | 结论 |
|---|--------|------|
| 1 | 卡片粒度 | 一张卡片 = 一个需求 = 一对 spec + plan 文件 |
| 2 | 执行单元 | 一个 "Thread"（会话），不是受监督的子代理 |
| 3 | 承载者 | 常驻 daemon（方案 A）：关闭界面后仍继续跑 |
| 4 | 并发语义 | **任务级**并发，不做事级/请求级计数 |
| 5 | 并发上限 | 工作日 09:00–24:00 → 3；工作日 00:00–09:00 → 10；周末全天 → 10 |
| 6 | 合并决定权 | 始终留给用户；执行者**绝不自动合并** |
| 7 | 停等策略 | 方案 B：能自己跑完就跑完；需要用户时才停下等确认 |
| 8 | 降级规则 | BLOCKED / plan 冲突 / 预算耗尽 → 标 `needs_you` 并**立即让出槽位**，队列不空转 |
| 9 | 卡片来源 | 方案 A：从普通会话"提升"（讨论与看板解耦） |
| 10 | 执行体归属 | 方案 X：卡片 = daemon 托管的自主会话，前端经 IPC 只读展示其时间线 |
| 11 | TUI 查看 | 第一版降级：看板可见状态/进度/证据，**不做**跳转查看完整会话时间线 |
| 12 | 合并动作 | 方案 Q7-a：看板只展示证据并给入口，用户进会话由 `finishing-a-development-branch` 走正规菜单 |
| 13 | 跨项目 | 不做。看板是 per-project 的（运行时目录本就按项目划分） |

---

## 3. 范围边界

**做什么：**

- 新增 crate `yi-agent-kanban`：卡片模型、队列状态机、时段并发日历、队列推进决策。
- 在 daemon 增加"看板队列"存储（SQLite，schema v12）与分钟 tick 上的队列推进。
- 补上缺失的 **root worker 自动启动驱动**（见 §5）。
- 新增 IPC 请求族：卡片增删改查、入队/插队/暂停/取消、只读订阅某张卡片会话的事件流。
- 新增 `kanban` system skill：约束执行者按 Superpowers 规范自建 worktree、执行 spec+plan、
  呈现合并菜单、卡住时报告 BLOCKED。
- desktop 看板视图；TUI `/kanban` popup。

**不做什么：**

- 不做请求级限流计数（沿用任务级并发；429 由既有退避兜底）。
- 不做自动合并、不替用户执行 `git merge`。
- 不做跨项目看板。
- 不做看板内的需求讨论/草稿态（方案 A：提升时两文件已存在）。
- 不改既有任务树、审核、交付、worktree 回收语义。
- 不做 TUI 的会话时间线跳转（第一版）。

---

## 4. 架构

```
┌──────────────── 用户会话（讨论需求） ────────────────┐
│  普通 Thread：brainstorming → spec + plan 文件       │
│  动作：「加入看板」→ 校验两文件成对 → 入队            │
└───────────────────────────┬──────────────────────────┘
                            │ IPC: EnqueueCard
                            ▼
┌──────────────────── 常驻 daemon ─────────────────────┐
│  kanban 队列（SQLite）                                │
│    ├─ 卡片状态机（§6）                                │
│    ├─ 时段并发日历（§7）                              │
│    └─ 分钟 tick 推进：                               │
│         选队首 → 建 root session(Coding) → 启动 worker│
│                                                       │
│  现有能力（复用，不改语义）：                          │
│    任务树 / 状态机 / WatchdogLimits / 资源租约         │
│    事件流 / worktree 回收 / 审核                       │
└───────────────────────────┬──────────────────────────┘
                            │ IPC: 只读订阅事件流
              ┌─────────────┴─────────────┐
              ▼                           ▼
      desktop 看板视图              TUI /kanban popup
      （可点开看时间线）            （状态/进度/证据，只读）
```

**为什么必须动 daemon（诚实说明）：** 当前 daemon 只有在被显式调用
`StartWorker` / `spawn` / `retry` 时才会启动 worker（`runtime.rs:1268/1772/1989/2414`）；
定时任务只创建 root 会话记录（`evaluate_schedules`），**之后没有任何东西把它跑起来**——
`IpcRequest::StartWorker` 在生产代码里零调用方。因此"排队 → 自动启动"这件事在现有机制里
**不存在**，是本插件必须新增的核心一环；而队列的骨架（会话/任务/attempt/状态机/worktree
回收）全部现成。

---

## 5. 执行体的精确语义（方案 X）

一张卡片在队首启动时：

1. 以 Coding 模式创建 root session：复用
   `RuntimeCoordinator::create_session_with_objective_and_mode`。
2. root 的 objective 由插件模板生成（§8），指向 spec/plan 文件。
3. daemon 启动该 root 的 worker 并驱动到底（新增的驱动，见下）。
4. 线程结束后，卡片进入 `awaiting_merge` 或 `needs_you`。

**与 desktop thread 的区别（必须在文档里说清）：**

- desktop 侧边栏的 thread 由 **app-server 进程**托管，App 退出即不再被驱动。
- 卡片对应的执行会话由 **daemon** 托管，无人值守时仍继续跑。
- 因此**两者是两套持久化**，卡片会话不会自动出现在侧边栏；它只通过看板视图可见。

**驱动实现要点：** 队列推进在 daemon 既有分钟 tick
（`ipc.rs` 的 `evaluate_schedules` 同位）上执行，新增一步：
对有可用槽位时的队首卡片，创建会话并调用 `start_worker`。该动作必须幂等——
同一卡片同一次启动不得创建两个 worker（复用 `has_worker` 检查与持久化状态作为权威）。

---

## 6. 卡片状态机

```
queued ──▶ running ──▶ awaiting_merge ──▶ done
              │  ▲
              ▼  │
          needs_you
              │
              ▼
           failed
```

| 状态 | 含义 | 占用槽位 |
|------|------|----------|
| `queued` | 排队中，等槽位 | 否 |
| `running` | 正在执行 | **是** |
| `needs_you` | 需要用户决策（BLOCKED / plan 冲突 / 预算耗尽） | 否（立即释放） |
| `awaiting_merge` | 执行完成，分支上有 commit，等用户决定集成 | 否 |
| `failed` | 终止性失败 | 否 |
| `done` | 已集成 | 否 |

**不变量：**

- 任意时刻 `running` 卡片数 ≤ 当前时段上限。
- 任何非 `running` 状态都不占用槽位。
- 卡片进入 `needs_you` 或 `awaiting_merge` 后，队列立即推进下一张。

---

## 7. 时段并发日历

插件配置（拟 `<workdir>/.yi-agent/kanban.toml`）：

```toml
default_max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "09:00"
end = "24:00"
max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "00:00"
end = "09:00"
max_tasks = 10

[[window]]
days = "Sat,Sun"
all_day = true
max_tasks = 10
```

**解析规则：**

- 给定本地时刻 → 命中第一个匹配窗口 → 取 `max_tasks`；无命中取 `default_max_tasks`。
- 边界以本地时区为准；`end` 为开区间（`24:00` 表示到当日 23:59:59）。
- 配置缺失/损坏 → 回退 `default_max_tasks = 3`（保守），并记 warning；绝不因配置错误停摆。
- 时段切换（如 09:00 从 10 降到 3）**不打断**正在运行的卡片；只影响新启动，直到运行数
  自然回落到新上限以下。

**为什么不做请求级计数：** 用户明确按任务级并发（"大概同时 3 个任务在运行"）。请求级计数
是最易出错、最脏的部分；且现有 `max_llm_requests_per_provider_key` 策略字段并未真正接到
调度器上（容量是硬编码常量 `DEFAULT_LLM_PER_PROVIDER_KEY`），依赖它反而更不可靠。若某时段
3 个任务仍打出 429，由既有 `RetryFailure::ProviderRateLimited` 指数退避 + 抖动兜底。

---

## 8. 执行者约束：kanban skill

插件为卡片 root 注入 objective 模板，约束执行者遵循 Superpowers 规范：

1. 用 `superpowers:using-git-worktrees` 自建隔离 worktree（`.worktrees/<branch>`），
   分支名 `kanban/<卡号>-<slug>`；
2. 用 `superpowers:executing-plans` / `superpowers:subagent-driven-development` 执行
   spec + plan；
3. 结束时用 `superpowers:finishing-a-development-branch` **呈现选项**，**绝不自动合并**；
4. 遇到无法解决的阻塞 → 报告 BLOCKED（插件将其映射为 `needs_you`）。

**worktree 由执行者自建**（符合 Superpowers 规范与用户要求），插件不代劳。

**进度可观测（不需要询问模型）：**

- plan 文件的 task 勾选状态；
- SDD 账本 `<repo>/.superpowers/sdd/<plan-basename>/progress.md` 中的
  `Task <N>: complete` 行与 fix round 行。

两者结合得出 "3/7 tasks" 之类的进度。

---

## 9. 创建卡片（提升）

流程：

1. 用户在普通会话中讨论需求，产出 spec + plan 文件（沿用项目既有规范路径：
   `docs/superpowers/specs/` 与 `docs/superpowers/plans/`）。
2. 用户执行「加入看板」（slash command / 界面动作），指定 spec 路径与 plan 路径。
3. 插件**校验两文件成对存在**（判据是文件存在，而非模型声称完成）。
4. 校验通过 → 入队为 `queued`；失败 → 拒绝并给出明确原因。

讨论与看板彻底解耦：讨论是高度交互的过程，且**不占用任何队列资源**。

---

## 10. 控制面

### desktop

- 看板视图（与 `ThreadSidebar` 并列，不替换）。
- 卡片列表：状态、进度、所属项目、等待时长。
- 卡片详情：spec/plan 链接、commit 列表、变更统计、验证结论、分支名。
- 操作：插队、调整顺序、暂停/恢复、取消、"打开执行会话"（只读订阅事件流）。

### TUI

- `/kanban` slash command → popup（复用 `/agents` 的 `ListPopup` / `DetailPopup` 范式）。
- 可见：状态、进度、证据（commit / 验证结论）。
- **降级：** 不做跳转查看完整会话时间线（TUI 目前只有单一根会话，无多会话读路径）。

### 合并（Q7-a）

看板**不执行**合并。卡片进入 `awaiting_merge` 时展示证据并提供入口；用户进会话，由
`finishing-a-development-branch` 走正规菜单（合并 / 提 PR / 保留）。

---

## 11. 测试策略

- **并发日历单测：** 给定本地时刻 → 正确上限；覆盖工作日跨时段边界（08:59 / 09:00 /
  23:59）、周末、配置缺失与损坏回退。
- **队列状态机单测：** 槽位获取/释放/补位；`needs_you` 不阻塞队列；FIFO 与插队；
  暂停/恢复/取消；状态转换不变式（`running` 数 ≤ 上限）。
- **驱动幂等单测：** 同一卡片重复推进不产生第二个 worker。
- **提升校验单测：** spec/plan 缺失或不成对时拒绝入队。
- **daemon 端到端：** 临时 SQLite + socket，验证"入队 → 到点自动启动 → 完成 →
  `awaiting_merge`"与"阻塞 → `needs_you` → 队列继续"。不调用真实 LLM。

---

## 12. 未决 / 后续

- TUI 的会话时间线跳转（第二版，需先有 daemon 会话的 TUI 读路径）。
- 跨项目看板。
- 卡片优先级/权重（当前仅 FIFO + 手动插队）。

---

## 13. 插件形态的诚实说明

`yi-agent` 目前**没有动态插件机制**（无 dylib/dlopen/加载器）。可用的扩展面只有：

| 扩展面 | 性质 | 本方案用途 |
|--------|------|-----------|
| skill | 真正的插件（纯提示词，零编译） | `kanban` system skill |
| MCP server | 外部进程，工具级 | 不使用 |
| crate + workspace 成员 | **编译期模块**，非插件 | `yi-agent-kanban` |

因此本方案是**双层结构**：

- **模块层**（不可避免，因需接入 daemon tick 与新增 IPC）：`yi-agent-kanban` crate。
- **插件层**（真正可替换、可演进）：`kanban` skill 提示词。

对"主流程干净"的保证方式是**边界约束**：新增能力只以新 crate + 新 IPC 请求族 + 新 skill
的形式存在，不改既有任务树/审核/交付/worktree 回收的任何语义。
