# 卡片会话与 worktree 的显式回收 Design

日期：2026-10-08
状态：设计已确认（用户逐项拍板），待用户 review 本文件后进入 writing-plans。
范围：宿主 `yi-agent-rs/crates/yi-agent-app-server/`（`thread/delete` 分支与其辅助）+ 桌面端 `desktop/src/App.tsx` 的删除确认框（S1）。插件零改动。

## 1. 背景与问题

看板卡片会话与它的 worktree **没有任何专用回收**，只增不减：

- **会话**：唯一删除入口是人工 `thread/delete`（`server.rs:3567+`）；删除后**不通知看板**，卡的 `thread_id` 仍指向已删会话。
- **worktree**：从不删除。`ensure_worktree`（插件侧）只复用或新建；宿主侧无任何 worktree 删除路径。
- **`board/remove`**：只删队列（`lifecycle::remove_in`），**不动会话、不动 worktree**。
- **卡片「归档」**：是卡片层的 24h 隐藏，**与会话/worktree 无关**。

后果：侧栏「看板会话」小节与 `.worktrees/kanban/` 随历史无限堆积。实测本项目 `.worktrees/kanban/` 下已有 9 个卡片 worktree，`threads` 里仍有卡片会话 meta。

## 2. 目标与非目标

**目标**

1. 卡片会话被**显式删除**时，其 worktree **一并显式删除**——会话与 worktree 配对回收，不留下孤立的 worktree。
2. 破坏性动作走**两步确认**：第一次删除只回报「要销毁什么 / 什么被保留及为何」；客户端带确认标志重发才真的销毁。唯一例外是无损删除（§4.5）——那条路径静默完成，因为它不丢任何数据。
3. 绝不静默销毁可能有价值的工作：脏 worktree、分支未并入 base 时，默认**保留** worktree 并如实回报。
4. 连带删 worktree **只对卡片会话生效**。

**非目标**

- **不自动清理**：不由卡片终态、归档、`board/remove` 触发任何自动会话/worktree 删除。
- **不新增独立清理入口**：不做「按卡批量清理 worktree」的 CLI/RPC（见 §9 的代价）。
- 不改 `board/remove`（仍只删队列）、不改 `done`/`archive` 语义。
- 不为普通会话引入任何 worktree 概念。
- 不清理面板二层的 `merge` worktree（`<项目>/.worktrees/kanban-merge/<slug>`，若有）——本设计只处理卡片 worktree。

## 3. 设计决策汇总

| # | 决策 | 取值 |
|---|---|---|
| D1 | 触发点 | `thread/delete` 命中**卡片会话**（`ThreadMeta.card_id` 非空）时连带处理 worktree |
| D2 | 配对语义 | 会话与 worktree 捆绑：会话被显式删除 → worktree 一起被显式删除 |
| D3 | 确认机制 | 复用既有 `force` 语义，扩展为**统一确认**：一个 `force` 同时确认「杀活跃子代理」与「销毁 worktree」 |
| D4 | 未 force 且 worktree 确认后将销毁 | 返回 `needs_confirmation`，响应新增 `worktree` 字段（`action`/`reason`）；无代价则直接删，不打扰用户 |
| D5 | 安全判据 | 干净 worktree 且 `kanban/<slug>` 已并入 base → 可直接删；脏 或 未并入 → 未 force 时保留、force 时销毁 |
| D6 | 防呆 | 路径若不是**本仓库的 linked worktree**（主检出、非 git、仓库不匹配）→ **硬拒绝**，force 也不删 |
| D7 | 适用范围 | 仅卡片会话（`card_id` 非空）；普通会话的 cwd 是用户项目目录，**任何情况**不删 |
| D8 | 删除命令 | `git -C <board_project> worktree remove [--force] <path>` |
| D9 | 失败处置 | worktree 删除失败只记日志并如实回报；会话删除照常完成（不因 worktree 失败而回滚会话） |
| D10 | 卡状态 | 不改卡片状态，也不通知插件（`thread_id` 悬空由看板侧容忍，见 §7） |

## 4. 触发与两步确认

### 4.1 现有 `force` 语义（复用，不新造）

`thread/delete` 已有一个两段确认：存在活跃子代理时，未带 `force` 即返回

```json
{ "status": "needs_confirmation", "active_children": <n> }
```

（`server.rs:3591-3611`），客户端在用户确认后带 `force: true` 重发。本设计**扩展**这一处，不引入第二种确认协议。

### 4.2 统一确认（D3）

`force` 成为「我确认要付出破坏性代价」的统一标志，同时覆盖两类代价：

1. 杀掉该会话的活跃子代理（既有行为）；
2. 连带删除该卡 worktree（新行为，仅当删除会丢失可能的工作时才有代价）。

**理由**：两者语义相同（都是"确认破坏性、不可逆"），继续给两个独立标志会让客户端与用户面对两个确认框，而实际决策是同一个。代价：只关心删会话、不希望动 worktree 的用户也必须理解这个标志——由 D4 的响应字段把话说清楚。

### 4.3 未 force 时的响应

当未带 `force` 且（有活跃子代理 **或** worktree 处置属"销毁/安全保留"）时，返回：

```json
{
  "status": "needs_confirmation",
  "active_children": <n>,
  "worktree": {
    "path": "<worktree 绝对路径>",
    "action": "remove" | "keep",
    "reason": "<为什么销毁 / 为什么保留>"
  }
}
```

- `active_children` 保持既有字段与含义（`0` 表示无子代理）。**注意**：本设计下 `needs_confirmation`
  也可能在 `active_children == 0` 时出现（仅因 worktree 处置需用户知情）——桌面必须容忍这一点（§11）。
- `worktree` 是**新增**字段，仅卡片会话出现；普通会话不返回该字段。
- **`action` 的准确定义：这一轮带 `force` 重发时，worktree 的最终动作**。取值：
  - `"remove"`：确认后 worktree 会被删除，且**会永久丢弃内容**（脏/未并入/分支缺失）——
    `reason` 必须把"丢什么"说清；
  - `"keep"`：**即便 force 也不删**（仅 D6 硬拒绝）——`reason` 说明为何不删。
  这个定义排除了"keep 既可表示不需删、也可表示硬拒绝"的二义。
- `reason` **总是**存在且可读，桌面两个分支都要展示它（§11）。

### 4.4 带 force 时的行为

按序执行（顺序有意）：

1. 既有：取消活跃子代理（`cancel_thread_children(.., force=true)`）。
2. 既有：中断并等待落盘、从内存摘除、停止各类 watch。
3. 既有：删会话文件（`thread_store.delete`）、回收附件。
4. 新增：按 §5 判据删/留 worktree。
5. 响应既有成功形状 `ok_response(id, json!({}))`——**空对象**，与今天一致（不新增成功字段）。

**顺序理由**：该会话的 cwd 就是这块 worktree。步骤 1–3 先做完（取消子代理、等 driver 落盘收尾、摘除内存会话、停 watch），才能保证**没有任何进程还持有或写入**该 worktree，随后步骤 4 的 `git worktree remove` 才安全、才有意义。反过来若先删 worktree，driver 可能仍在其上落盘。注意步骤 3 删的是 `<cwd>/.yi-agent/threads/*` 这类 **gitignored** 文件，不影响步骤 4 对"干净"的判定。

### 4.5 不带 force 且无需确认时的行为

若没有活跃子代理，且 worktree 处置为「**可直接删**」（§5.2：干净且已并入 base），则**无需确认**：
本次调用直接走 §4.4 的 1–4 步（删会话 + 删 worktree），响应 `ok_response(id, json!({}))`。
这是唯一"删 worktree 却不打扰用户"的情形——被删的 worktree 内容已完整存在于已并入的分支里，删除不丢任何东西。

`needs_confirmation` 因此在两种情况出现：
1. 有活跃子代理；或
2. worktree 处置为「**销毁**」（脏/未并入/分支缺失，确认后会丢数据）或「**安全保留**」（D6 硬拒绝）。

换言之：凡是**会丢数据**或**该保留**的事，都要让用户看见；可无损删除则静默完成。

## 5. worktree 判定与删除

### 5.1 取路径与项目根

- 路径：沿用删除分支既有推导——`thread_store.root().parent().parent()`（`server.rs:3665` 已用同一式样定位附件，即 `<cwd>/.yi-agent/threads` 上溯两级得 `<cwd>`）。**cwd 即该卡片会话当初的 worktree**（调度器起会话时 `thread/start` 的 cwd 就是 `ensure_worktree` 的产物）。
- 项目根：读该会话 meta 的 `board_project`（`ThreadMeta.board_project`，卡片会话非空，`thread_store.rs:47` 附近）。它是 `git -C` 的作用域，也是"是否本仓库 worktree"的判据来源。
- 卡 id：`ThreadMeta.card_id`，非空才进入本流程（D1/D7）。

### 5.2 判定表（D5/D6）

| 情形 | 未 force（首次响应） | force=true（重发后实际动作） |
|---|---|---|
| 干净 worktree 且 `kanban/<slug>` 已并入 base | **无需确认**：直接删（§4.5） | 删 |
| 有未提交改动，或 `kanban/<slug>` 未并入 base | `needs_confirmation`，`action:"remove"` + 破坏性 `reason` | 删（`--force`，永久丢弃） |
| 路径不是本仓库的看板 worktree（D6） | `needs_confirmation`，`action:"keep"` + 原因（让用户知道工作被保留、为何） | **保留**（硬拒绝，force 也不删） |
| 分支不存在（无法判定并入） | `needs_confirmation`，`action:"remove"` + `reason`「分支缺失，无法确认已并入」 | 删（用户已确认接受风险） |

**D6 的判据细化**：仅"`cwd` 出现在 `<board_project>` 的 `git worktree list` 里且非主检出"**不足以**证明它是卡片 worktree——同一个项目里可能有多个卡片会话，各自 cwd 不同；也可能有别的来源的 linked worktree。故本设计**同时**要求 `card_id` 非空（D1/D7）**且** `cwd` 落在 `<board_project>/.worktrees/kanban/` 之下**且**该路径出现在 `worktree list` 中。三者皆真才允许删除；否则一律 `keep`（硬拒绝的泛化：宁可不删，不可错删）。

判据来源（全部用现成 git 事实，不新增"自述"）：

- 干净：`git -C <cwd> status --porcelain` 为空。
- 已并入：`git -C <project> merge-base --is-ancestor kanban/<slug> <base>`；`<base>` 取项目默认分支（`origin/HEAD` 退化 `main`）。
- 是 linked worktree：`git -C <project> worktree list --porcelain` 里存在 `worktree <cwd>` 且 `<project>` 是同一个仓库（由此排除主检出、排除登记在别的仓库下的同名路径）。

### 5.3 删除命令（D8）

```
git -C <board_project> worktree remove [--force] <path>
```

- 未 force 且判定为"可直接删"（§5.2 第一行：干净且已并入）时，走 §4.5 无需确认，用不带 `--force` 的删除；
  git 若仍拒绝（例如残留文件），按 D9 记日志、保留并如实回报，**不**自动升级为 `--force`。
- force 且判定为"确认后删"（§5.2 第二、四行）时，用 `--force`（用户已确认接受销毁）。

## 6. 职责切分

- **`server.rs` 的 `thread/delete` 分支**：串起流程；新增一个小的判定/执行辅助（如 `fn plan_worktree_reclaim(...) -> WorktreeReclaim` 与 `fn run_worktree_reclaim(...)`），使分支本体保持可读。
- **新辅助模块或既有文件内的函数**：git 判定（干净/已并入/是 linked worktree）与 `worktree remove`。倾向放在 `yi-agent-app-server` 内一个独立小模块（如 `worktree_reclaim.rs`），与 `git_diff.rs` 同级——**不为一个功能去动 `yi-agent-store` 的公共面**。
- **与插件解耦**：本设计只读 `board_project`/`card_id`（会话自带的 meta），**不**调用任何 `board.*` RPC，也不改 `board.json`。插件侧零改动。

## 7. 与看板的边界（D10）

会话被删后，卡片记录的 `thread_id` 会悬空。这是**既有行为**（今天的 `thread/delete` 就已经如此），本设计**不**新增通知或状态改写：

- 看板调度器对"本进程看不到的会话"一概不动（`card_scheduler.rs:441-444` 的既有口径）。
- 卡片若停在 `awaiting_merge`/`done` 等非 `running` 态，`thread_id` 悬空对它无影响。
- 若卡片仍在 `running` 而会话被删：`recover_orphan_cards`（`server.rs:6050+`）与重启兜底路径处理这类情形（置 `needs_you`），无需本设计介入。

## 8. 测试与验收

**测试（`server.rs` 既有 `thread/delete` 测试族，`#[tokio::test]` + 假 daemon 模式）**

- 卡片会话（meta 带 `card_id` + `board_project`）+ 干净且已并入的 worktree：未 force 时**无需确认**，
  直接删除会话与 worktree（响应为空对象，目录消失）——`action` 语义在此不适用（本就不进确认路径）。
- 卡片会话 + 脏 worktree：未 force 返回 `needs_confirmation` 且 `worktree.action="remove"`（原因含"未提交"）；
  **会话仍在**。force 后 worktree 消失、会话消失。
- 卡片会话 + 分支未并入：同上（原因含"未并入"）。
- 普通会话（`card_id=None`）：响应**不含** `worktree` 字段，且其 cwd 目录在删除前后**原样存在**（D7 的钉子）。
- 防呆（D6）：把会话 cwd 指向主检出（或非看板 worktree 目录），未 force 返回 `needs_confirmation`
  且 `action="keep"`（原因说清）；force 重发后**仍保留**（目录仍在）。
- 既有用例零回归：更新断言时**只**为"新增字段"放宽，既有 `needs_confirmation`/`active_children`
  断言保持（这些用例在 `server.rs:13482/13559/13574/13602/14322` 一带）。

**验收口径（人可验证）**

1. 在一张卡片会话上（例如本项目 `2026-10-08-awaiting-merge-convergence-...` 那张）执行删除：
   第一次弹确认框，框里**同时**说明子代理数与 worktree 将删/将留及原因；同意后，
   会话与它的 `.worktrees/kanban/<slug>` 一并消失。
2. 对一张**未合并**的卡片（例如停在 `awaiting_merge` 的旧卡）删会话：确认框写明"worktree 将保留：<原因>"，
   实际也**保留**——工作没有静默丢失。
3. 删一个**手工会话**（在项目根开的）：不出现 worktree 相关文案；项目根目录**毫发无损**。

## 9. 已知代价与风险

| 项 | 说明 |
|---|---|
| **孤儿 worktree（H1 的显式代价）** | 会话先被删（或卡从未起过会话就进终态），其 worktree 无人能删。本设计不给它出路——独立清理入口被明确否掉。用户可手工 `git worktree remove`。 |
| 统一 `force` 的粗粒度 | 只想删会话、不想动 worktree 的用户没有更细的开关；靠 D4 的响应字段把后果说清。 |
| 删 worktree 需读 `board.json` 侧信息 | 仅用会话自带的 `board_project`/`card_id`，不调插件 RPC，故不引入对插件在线的依赖。 |
| 分支名推导耦合 | `kanban/<slug>` 的 slug 规则需与插件侧 `worktree::slugify` 一致；若插件改了规则而宿主不同步，"已并入"判定会退化为"分支缺失 → 保留"（安全侧失败，不会误删）。 |
| 删前重算判据 | 未 force 与 force 是两次独立调用（间隔可能很长，用户可能中途解除了脏状态）。force 那一次**重新**执行 §5 判定，不缓存第一次的结论——客户端展示的预告可能与最终动作不同（只会更保守）。 |

## 10. 涉及文件

- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/delete` 分支 + 判定/执行接线）
- `yi-agent-rs/crates/yi-agent-app-server/src/worktree_reclaim.rs`（新增：git 判定与 `worktree remove`）
- `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（注册 `pub(crate) mod worktree_reclaim;`，与 `git_diff` 同款可见性）
- 同 crate 测试：`thread/delete` 族新增用例 + 既有断言按 §8 微调
- `desktop/src/App.tsx`（`deleteThread`：确认框渲染 `worktree.action`/`reason`，含空消息兜底）
- `desktop/src/App.test.tsx`（新增两条 worktree 确认用例；既有两条按 §11 保持语义）

## 11. 桌面端（S1：本 spec 含）

**现状**：`desktop/src/App.tsx:845-867` 的 `deleteThread` 已实现两步确认——请求 `thread/delete`，
若 `status === "needs_confirmation"` 则读 `active_children`（`:853-856`）、用 `window.confirm` 问一句、
同意后带 `force: true` 重发（`:859-863`）。其测试在 `desktop/src/App.test.tsx:1850`（同意→force）与
`:1876`（拒绝→不 force）。

**本设计带来的变化**：后端现在会在**只有 worktree 需处理**时也返回 `needs_confirmation`
（此时 `active_children` 可能为 `0`）。故桌面不能只按子代理数拼话术，必须同时表达 worktree 后果。

**改动（`App.tsx` 的 `deleteThread`）**：

1. 首次请求的响应类型扩为：
   ```ts
   const first = await c.request<{
     status?: string;
     active_children?: number;
     worktree?: { path: string; action: "remove" | "keep"; reason: string };
   }>("thread/delete", { threadId: id });
   ```
2. `needs_confirmation` 分支改为按**两段**拼消息（各自的缺失都不影响另一段）：
   - 子代理段（`count > 0` 才加）：`这个会话还有 ${count} 个子代理正在运行，删除会一并终止它们。`
   - worktree 段（`first.worktree` 存在才加）：
     - `action === "remove"` → `将一并删除卡片 worktree：${worktree.path}`
     - `action === "keep"` → `卡片 worktree 将保留：${worktree.reason}`
   - 两段用 `\n` 连接，末尾统一加 `继续？`
   - **兜底**：两段都为空（后端返回了 `needs_confirmation` 却没给可读原因）时，仍须弹一句
     非空确认文案（如 `删除不可逆。继续？`）——绝不弹空框；这是"协议字段缺失也不能让确认变成盲点"。3. 其余流程不变（`window.confirm` 同意后带 `force` 重发；非 `needs_confirmation` 直落后续清理）。

**测试（`App.test.tsx`，沿用既有 `dataSources["thread/delete"]` 桩法）**：

- 新增：`active_children: 0` + `worktree.action:"remove"` + `path` → `window.confirm` 的消息含该 path；
  同意后发出带 `force: true` 的重发（钉住"仅 worktree 也走确认"）。
- 新增：`worktree.action:"keep"` + `reason` → 消息含该 reason，且**不含**"将一并删除"。
- 既有两条（`:1850`/`:1876`）保持通过：它们只给 `active_children`、不给 `worktree`，新逻辑的子代理段
  与兜底都不破坏原断言（若消息拼接影响断言字符串，只做必要放宽，不放宽"是否发 force"这一语义）。
