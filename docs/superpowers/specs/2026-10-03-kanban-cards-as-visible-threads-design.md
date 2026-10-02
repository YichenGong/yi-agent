# 看板卡片改为可见会话（Thread）

**日期：** 2026-10-03
**状态：** 设计已确认，待评审
**模块：** `superpowers-kanban` 插件（状态机/槽位）+ `yi-agent-app-server`（卡片调度器）+ 桌面会话界面（复用）

---

## 1. 背景与问题

看板卡片的执行单元，原始设计（`2026-09-30-kanban-plugin-design.md` §2 决策 #2/#10）定为
「一个 Thread（会话），不是受监督的子代理」「卡片 = daemon 托管的自主会话，前端只读展示」。
但实现漂移了：插件用 `CreateAutonomousSession` 起了一个 daemon 内的后台 root 任务，**没有
创建桌面可见的 Thread**。

后果（已在真实 E2E 复现）：

- 卡片跑起来后，桌面侧栏里根本没有这个会话，用户**看不到、点不进、无法验收**；
- 该 root 任务即使跑完并产出提交，也因运行时不允许 root 提交父交付
  （`yi-agent-core/src/subagent/supervisor.rs:723`「root tasks cannot submit parent delivery」）
  而被卡在 `running`，卡片永远到不了 `awaiting_merge`。

本设计把执行单元改回原意：**卡片 = 一个用户可见、可点开、可继续追问的桌面会话（Thread）**。

## 2. 目标与非目标

**目标**

- 卡片被触发时，起一个正常的桌面 Thread（出现在侧栏），并**自动发首轮**执行。
- 全过程都在这个会话里（允许在会话内使用 subagent），用户可点开看全过程。
- 会话结束、且产出提交 → 卡片进 `awaiting_merge`；用户可点开会话**继续追问**，追问期间
  卡片回到 `running`，该轮再结束回到 `awaiting_merge`。
- 关掉界面后卡片仍继续跑（执行在 app-server 进程，不在前端）。

**非目标**

- 不改会话界面本身（复用现有侧栏/对话 UI，不新增卡片专用视图）。
- 不做卡片内联的「合并 / 丢弃」按钮（验收与集成仍走既有流程）。
- 不保留后台 root 任务这条执行路径。
- 不在本设计里修 `supervisor.rs` 的 root-交付缺陷（见 §10）。

## 3. 架构与职责

**插件（`superpowers-kanban`，独立进程）—— 看板状态与槽位所有者**

- 继续负责：inbox 消费、看板状态机、并发/日历/开关、`ensure_worktree`（建隔离 worktree）。
- 不再创建会话：移除 `CreateAutonomousSession` 那条 launch 路径。
- 新增/调整查询方法供 app-server 驱动（§5）。
- 槽位（全局租约）与卡片状态由插件统一记账：`next_launch` 原子占用，`mark_terminal`/`release` 归还。
  插件内部把「看板状态 + 槽位」收敛到一把锁，供推进循环与查询分派共享，使 `next_launch`
  相对推进循环是原子的。

**app-server —— 卡片调度器（新增 tokio 任务）**

- 周期性轮询「有看板且开关打开」的项目：经现有 `plugin_query` 调用插件方法取可启动卡、
  起 Thread、回写状态。
- 复用现有能力：`thread/start`、`turn/start`、`thread/resume`、`thread/list`；`plugin_query(path:879)`
  已是现成的插件客户端，无需新协议。

**桌面 UI —— 不改协议**

- 卡片会话自然出现在侧栏（普通 Thread）。
- 卡片面板继续显示状态；可选：显示卡片对应的 thread 以便一键打开（§5 的 `list` 已带 `thread_id`）。

## 4. 卡片生命周期与状态

```
queued ──▶ launching(持久化) ──▶ running(带 thread_id) ──▶ awaiting_merge ──▶ done
                                              │  ▲
                              (追问一轮) ──────┘  └────── (该轮结束)
```

- `launching`：插件已占用槽位、建好 worktree、等待 app-server 起会话的短暂状态。**持久化**
  （以便崩溃后能对账），桌面渲染为「启动中」。
- `running`：会话正在跑，**占一个并发名额**。
- `awaiting_merge` / `done` / `failed` / `needs_you`：**不占名额**。
- 追问：用户在 `awaiting_merge` 的会话里再发一轮 → 卡片回 `running`（重新占名额），该轮结束再回
  `awaiting_merge`。
- 异常：会话失败 → `failed`；会话等待授权/被阻塞 → `needs_you`；产出为空的「完成」→ `needs_you`
  （原因 `completed without changes`）。

## 5. 插件 ↔ app-server 接口（复用 `plugin_query`，无新协议）

新增看板方法：

| 方法 | 参数 | 行为 | 返回 |
|------|------|------|------|
| `board.next_launch` | `{}` | 若开关开、有名额、日历放行、队首有 `queued` 卡：原子占槽 → `ensure_worktree` → 置 `launching` | `{card_id, workdir, title}` 或 `null` |
| `board.mark_running` | `{card_id, thread_id}` | `launching → running`，记下 thread_id | `{ok}` |
| `board.mark_terminal` | `{card_id, outcome, detail?}` | `running → awaiting_merge\|failed\|needs_you`，释放槽位 | `{ok}` |
| `board.release` | `{card_id}` | 启动失败时归还槽位，卡片置 `failed`(detail) | `{ok}` |

- `outcome ∈ {awaiting_merge, failed, needs_you}`。
- 扩展 `list`：每张卡附 `thread_id`（可空）、`state`、`workdir`，供桌面/调度器共用。
- 现有 `enqueue` / `switch.read` / `switch.write` 不变。

## 6. 调度器行为

轮询周期 **3s**（并在插件可用性恢复时立即触发一次）。

- **启动**：对每个有看板且 `switch.read` 为开的项目，循环调用 `board.next_launch`：
  - 取到 `{card_id, workdir, title}` → `thread/start`(cwd=workdir, title, Yolo) → `turn/start`(objective)
    → `board.mark_running {card_id, thread_id}`；
  - `thread/start` / `turn/start` 失败 → `board.release {card_id, detail}`。
- **对账**：对每个 `running` 且已有 thread_id 的卡片，依据调度器内存中该 Thread 的
  `active_turn_id`/状态判断是否**无活跃 turn（空闲）**：
  - 有新提交（worktree HEAD ≠ 启动时记录的 base）→ `board.mark_terminal {awaiting_merge}`；
  - 无提交 → `board.mark_terminal {needs_you, "completed without changes"}`；
  - 线程失败 → `board.mark_terminal {failed}`；等待授权/阻塞 → `needs_you`。
  （「空闲」的判据是调度器自己持有的 Thread 会话状态，不做额外 RPC 轮询。）
- **恢复**：app-server 启动时，对仍 `running` 且有 thread_id 的卡片 `thread/resume` 重新接管，
  随后立即对账一次。
- **多项目**：调度器遍历有看板的项目；跨项目并发由插件的全局槽位统一约束。

## 7. 会话设置

- **cwd**：卡片的隔离 worktree。
- **标题**：`看板 · <spec 文件名去扩展名>`（可被用户重命名）。
- **权限模式**：Yolo（自动执行必需；沿用原后台会话的自主权级别）。
- **首轮 objective**：沿用现有 `objective_for` 文案（遵循 SDD、只在本 worktree、用
  `finishing-a-development-branch` 给出集成选项、**绝不自行合并**、被阻塞则报 BLOCKED）。
- **启动时记录 base commit**：在 `next_launch` 里取 worktree HEAD 存入卡片，供对账判断「有无新提交」。

## 8. 错误处理

- `next_launch` 成功但起会话失败 → `board.release`：归还槽位、卡片 `failed`（不做无声重试，避免
  死循环；下轮队列继续放行）。
- 会话启动后立即失败 → 对账置 `failed`。
- 插件不可用 / 查询超时 → 调度器跳过本轮，不改任何卡片状态（不可达不等于失败）。
- 建 worktree 失败 → 插件侧直接置 `failed`，且不占用槽位。

## 9. 测试

- **插件（Rust 单测）**：`next_launch` 的开关/名额/日历门控与原子占用；`mark_running` /
  `mark_terminal` / `release` 的状态迁移与槽位释放；`list` 附带 `thread_id`；无提交完成的映射。
- **app-server（Rust 集成）**：以 fake 插件 socket 驱动调度器，验证
  「取卡 → `thread/start`+`turn/start` → `mark_running`」、「空闲 → `mark_terminal`」、
  「起会话失败 → release + failed」、「重启 → resume 并对账」。
- **桌面**：卡片面板展示 thread（若加该字段）。

## 10. 迁移与兼容

- 移除插件的 `CreateAutonomousSession` launch 路径；对旧的、仍是 `running` 但**没有 thread_id**
  的历史卡片，一次性置 `needs_you`（原因 `legacy run without a thread`），避免永久卡住。
- `supervisor.rs:723` 的 root-交付缺陷在本设计下不再阻塞看板（不再用后台 root），另开独立小修。

## 11. 范围外

- 卡片内联合并/丢弃按钮。
- 对账的推 vs 拉：本设计用轮询（3s）；事件驱动的即时回写留作后续优化。
- 会话侧栏里对「看板会话」的特殊视觉标记。
