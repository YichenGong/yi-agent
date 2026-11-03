---
name: superpowers-kanban
description: "Use when the user works with the Superpowers kanban board instead of writing specs or plans: enqueue an existing spec+plan pair (\"加到看板\", \"排队\", \"enqueue this\"), record a pre-queued idea (\"先记着\", \"run it later\"), promote a pre-queued card once its spec and plan exist, ask for queue status or where a card stands, request the per-project merge slot from a card's own session (\"合并这张卡\", \"merge it into main\"), or settle and archive a card that was already merged. It only drives the superpowers-kanban CLI; it never writes specs, plans, or board files."
---

# Superpowers 看板

看板把需求排成队列，插件在限流窗口内按顺序自动开发。它是一条流水线，不是文档生成器：

- **正式入队**——spec 和 plan 都已写好，`add <spec> <plan>` 直接排队；
- **前植入队**——需求已定、两份文件还没写，先 `add-prequeued <title>` 记一张预排队卡
  （最左 `PRE-QUEUED` 列，不占并发槽、不会被起会话），等文件写好再
  `promote <card-id> <spec> <plan>`；
- **合并**——实现卡跑完后由用户确认、排进合并队列；**daemon 排到那张卡时会把合并
  这一轮送回这张卡的开发会话**（同一个会话、同一段上下文），不是另起一个会话；
- **收尾**——合并已落地后 `done` 登记，`archive` / `purge` 隐藏或删除。

看板是一个独立插件：装了就可用，卸了就消失。

## 你的职责边界

**只操作看板，不写上游产物。** 本 skill 的唯一动作是调用 `superpowers-kanban` 二进制。

- **不要**生成、修改或"顺手补全" spec 或 plan。
- **不要**替用户决定两条需求该不该合并成一张卡。合并与否由用户在界面上决定；
  你只负责如实展示状态。
- **不要**直接写 `board.json`、`inbox/` 下的文件或 `preferences.json`。
  这些是插件进程的落盘格式，绕过 CLI 写会破坏它的原子性假设。
- **不要**绕过合并名额自己 `git merge`（见 §6）。

## 前提：正式入队要 spec 和 plan；没写就先预排队

看板是 Superpowers SDD 流程的**下游**。上游是 `brainstorming` → 写 spec →
`writing-plans` 写 plan。正式入队（`add`）要求两份文件**都已经存在**。

需求已经定了、但 spec/plan 还没写时，**不要**把用户干等在外面，也不要凭空替它写
spec——先 `add-prequeued` 记一张预排队卡持续追踪，等文件齐了再 `promote` 提升。
用户说"这个还没做设计、但先记着"时，就是这条路径。

## 命令全景（唯一动作面）

`superpowers-kanban <verb>`；下表的动词就是全部能力，没有表外的隐藏命令。

| 动词 | 作用 | 何时用 |
|---|---|---|
| `superpowers-kanban add <spec> <plan>` | 投递一张正式排队卡 | 两份文件已存在 |
| `superpowers-kanban add-prequeued <title>` | 投递一张预排队卡 | 需求定了、文件没写 |
| `superpowers-kanban promote <id> <spec> <plan> [--title <t>]` | 预排队卡提升为正式排队 | 文件补齐后 |
| `superpowers-kanban add-merge <source> [--base <ref>]` | 为**没有实现卡**的分支投一张合并卡 | 手工建的分支要合 |
| `superpowers-kanban state <id> <state> [--detail <t>]` | **声明卡片状态**（见 §4） | 结束回合 / 等人决策 / 完成时 |
| `superpowers-kanban state --self <state> [--detail <t>]` | 同上，卡号取环境变量 | 卡片会话里最常用 |
| `superpowers-kanban merge-request <id>` | 把 `awaiting_merge` 的卡**排进**合并队列 | 用户确认要合并时 |
| `superpowers-kanban merge-finish <id>` | 释放合并名额（不判成败） | 会话外手工合并收尾；队列推进的回合由 daemon 自己回收 |
| `superpowers-kanban merge-cancel <id>` | 退出合并队列（仅在 `merge_queued`） | 用户反悔不想合了 |
| `superpowers-kanban done <id>` | 把 `awaiting_merge` 的卡结算为 `done` | 等价于 `state <id> done`，保留兼容 |
| `superpowers-kanban archive <id>｜--all-terminal` | 归档（隐藏，保留） | 想让卡从看板上消失 |
| `superpowers-kanban purge <id>｜--all-archived` | 真删（**仅限已归档**） | 连记录也不要了 |
| `superpowers-kanban list [--all]` | 查队列与状态 | 用户问"队列里有什么/跑到哪了" |
| `superpowers-kanban on` / `superpowers-kanban off` | 写**项目层**推进开关 | 开始/停止推进队列 |
| `superpowers-kanban workdir` | 报告状态目录对应的项目根 | 不确定 `--state-dir` 指向哪 |
| `superpowers-kanban manifest` | 打印本插件的 supervisor 清单 | 宿主 `plugin/install` 用；**不要**手调 |
| `superpowers-kanban run` | 推进循环 | 由监督器拉起；**不要**手调 |

## 触发时机

出现以下意图时使用本 skill：

- 用户说"把这个加到看板 / 排队 / 入队 / enqueue / 待会儿跑"。
- 用户刚写完一份 spec 和 plan，说"那就让它跑吧"。
- 用户说"先记着 / 还没做设计 / 之后再做"，但需求已经定了。
- 用户问"现在队列里有什么 / 跑到哪了 / 这张卡什么状态"。
- 用户在一张实现卡的原会话里说"合并 / 把这条合进 main / 这张卡可以合了"。
- 用户说"我合并了这张卡 / 确认合并 / 更新看板进度 / 这张卡可以收了"。

## 怎么做

### 0. 先确认状态目录（最容易错的一步）

不带 `--state-dir` 时，状态目录取**当前工作目录**下的 `.yi-agent/superpowers-kanban`。

- 在项目根执行：默认就落在 `<项目根>/.yi-agent/superpowers-kanban`，无需参数。
- **卡片会话的 cwd 是实现用的 worktree，不是项目根。** 在卡片会话里裸跑 `add`/`list`
  会指向 worktree 下那个空目录（表现为"卡不见了"）。此时要么先 `cd` 回项目根，要么显式给
  `--state-dir <项目根>/.yi-agent/superpowers-kanban`。

不确定某个状态目录对应哪个项目时：

```bash
superpowers-kanban workdir --state-dir <dir>
```

### 1. 确认两份文件

`<spec>` 和 `<plan>` 是**文件路径**，不是内容。动手前先确认两者都真实存在：

```bash
test -f <spec> && test -f <plan> && echo ok
```

缺失任一份就停下来问用户，不要猜路径、不要新建占位文件。
两份必须是不同文件（同一份 spec+plan 不成对）。

### 2. 入队

在**项目根目录**下执行：

```bash
superpowers-kanban add <spec> <plan>
```

成功时打印：

```
delivered <card-id> to <项目根>/.yi-agent/superpowers-kanban/inbox
<spec>
<plan>
```

`<card-id>` 由两份文件名派生，可拿来跟用户复述"第几张卡"。

跨项目或状态目录不在默认位置时，显式指定：

```bash
superpowers-kanban add <spec> <plan> --state-dir <dir>
```

`add` **立刻**校验两份文件；校验失败会打印原因并以非零退出码结束。
把这条错误原样转述给用户，不要重试、不要自我修正路径。

### 2a. 预排队（spec/plan 还没写）

需求已定、两份文件还没写时，先记一张预排队卡：

```bash
superpowers-kanban add-prequeued "<需求标题>"
```

成功时打印投递的 `<card-id>`（由标题派生，可在桌面最左 `PRE-QUEUED` 列 / `list` 里
看到它，卡片状态为 `PreQueued`）。`<title>` 必须非空。

在会话里投递时，插件会自动把**当前会话**记为这张卡的来源（宿主经
`YI_AGENT_THREAD_ID` 注入），无需额外参数；桌面看板上可点击跳回那次讨论。

之后设计写完、`<spec>` 与 `<plan>` 落盘，再提升为正式排队卡：

```bash
superpowers-kanban promote <card-id> <spec> <plan> [--title "<新标题>"]
```

- 提升**不重算 id**（从预排队到 done 始终是同一张卡），并把卡重排到队尾；给了
  `--title` 就顺带改标题（不改 id）。
- `promote` 会先校验两份文件成对、且该卡确实还在预排队态，失败**不落盘**、原样报错。
- 时机：`add-prequeued` 后要等插件下一次 tick 把卡建出来，`promote` 才找得到它；
  刚投递就立刻提升会报 `unknown card`（先用 `list` 确认卡已在看板上）。

### 2b. 合并：用户确认后入队，daemon 排到才开跑（主路径）

合并**不是**「会话跑完就自动合」。真实流程只有两条边由你踩：

1. **用户确认要合并时，把卡排进队列**：

   ```bash
   superpowers-kanban merge-request <card-id>
   ```

   只有卡处于 `awaiting_merge`（你上一轮跑完计划的产物）才排得进去。
   - 打印 `queued` + `position: N` → 告诉用户「已排队，前面还有 N 张」。
   - 报 `denied: …` 原样转述，**不要猜、不要绕**。

   **这一步只是排队，不会立刻开跑。** 本项目同一时刻只跑一个合并。排到队时
   daemon 会把合并这一轮送回**这张卡自己的开发会话**（`thread_id` 不变），由那个
   会话里的模型执行合并；**不会**另起一个新会话去顶替它。

   这意味着两件事：
   - 合并轮的上下文就是这个会话的历史，模型不需要重新了解"这是什么卡、做到哪了"；
   - 如果那个会话已经不在内存里（进程重启过），宿主会按原 `thread_id` 与开发
     worktree 的持久历史**冷恢复**它。恢复失败（开发树被删、日志损坏、锁被别的
     进程占着）时卡片转 `needs_you`，**不会**退化成"换一个会话继续合并"。

2. **用户反悔时退队**：

   ```bash
   superpowers-kanban merge-cancel <card-id>
   ```

   只在 `merge_queued` 上成立，退回 `awaiting_merge`。卡已经排到队、合并回合
   开跑之后就不能退了——那会与一轮在跑的合并打架。

那轮合并拿到的是**已持有**的合并名额，它按 daemon 给出的提示词合并，然后由模型
自己声明结果：`state <card-id> done`（成功）或 `state <card-id> needs_you`
（冲突解决不了）。注意合并轮的提示词用的是**显式卡号 + 绝对 `--state-dir`**，
不依赖 `--self`——冷恢复出来的会话不一定有 `SUPERPOWERS_KANBAN_CARD`。

释放名额的是 **daemon**，不是会话：它要看到「这一轮的结果已经回报、并且那个会话
在**之后**的一拍被观测为空闲」才放门。名额是 daemon 进程里的 `flock`，别的进程
放不掉，所以会话里那句 `merge-finish` 只会回 `cleared`，写不写都不影响，
合并轮的提示词也明确要求**不要**调用它。

**你不要**替它判断成败，也不要在别的会话里再跑一遍 `git merge`。

**绝不绕过名额直接 `git merge`。** 名额（每项目一把 `merge.lock`）由 daemon 在
开跑那一拍拿到，绕过去就会两条分支同改主检出。

### 2c. 投递一张独立的合并卡（旧路径，保留）

需要为一个**没有实现卡**的分支（例如手工建的分支）单独立卡时：

```bash
superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <dir>]
```

- `--base` 缺省取仓库默认分支（`origin/HEAD`，退化到 `main`）。命令**立刻**校验 refs 与
  source 分支存在；失败原样转述，不要猜分支名、不要重试。
- 这张卡**一出生就在合并队列里**（状态 `MergeQueued`），不用也不该再调
  `merge-request`；排到队时 daemon 自己开一轮合并会话去合并。你只负责投递，
  并如实回报它的 id。
- 实现卡要合并请走 §2b，不要用 `add-merge` 复制一张卡。

### 2d. 维护卡片状态（**模型负责**）

看板**不会**替你判断状态。daemon 只写三件事：排队、启动成功、启动失败。其余全部
由你声明——结束回合前、需要用户决策时、完成时：

```bash
superpowers-kanban state --self <state> [--detail "<原因>"]
```

| 情境 | `<state>` |
|---|---|
| 计划跑完、有产出等验收 | `awaiting_merge` |
| 用户已确认合并、正在排队等名额 | `merge_queued`（只由 `merge-request` 写入） |
| 需要用户决策才能继续 | `needs_you` |
| 已合并、用户确认完成 | `done` |
| 环境阻塞（缺依赖、无权限） | `failed` + `--detail` |
| 用户明确放弃 | `cancelled` |

规则：

- **不要**为了「让看板好看」提前写 `done`。卡片状态是给用户的承诺。
- 一轮里问了用户一句话 → `needs_you`。会话空闲不等于活儿干完了，但「我在等人」正是
  `needs_you` 的语义。
- 用户回话后你继续干活 → `running`。
- 会话被杀了、你没能写状态时卡会停在 `running`，看板显示 `[stale] 当前宿主未加载`
  或 `[observation unknown] 当前宿主未观测到`。两者都**只说明当前宿主的热表里没有它**，
  不说明会话的历史已经没了——历史可能仍在磁盘上，只是当前进程没加载。
  这不是插件的错，是这里没写。
- 终态（`done` / `failed` / `cancelled`）不可复活。要重做，让用户新建一张卡。
- `--self` 依赖 `SUPERPOWERS_KANBAN_CARD`：插件给卡片会话的 shell 注入。缺失就说明
  当前不在卡片会话里，别硬猜 id。

### 3. 确认状态

```bash
superpowers-kanban list        # 默认不列归档卡
superpowers-kanban list --all  # 连归档卡一起列（行尾标 [archived]）
```

`list` 的输出有三种行：

- `1. <id> queued` —— 排队中，按顺序编号，看板会依次启动它们；
- `<id> <State> [@ <worktree>] [archived]` —— 其他状态的卡。`<State>` 是 Rust 枚举名：
  `PreQueued` / `Queued` / `Running` / `NeedsYou` / `AwaitingMerge` / `MergeQueued` /
  `Done` / `Failed` / `Cancelled`。行尾可能还有：
  - `[stale] 当前宿主未加载（截至 <RFC3339>；历史可能仍存在）`——宿主**明确**报告
    热表里没有这张卡的会话；
  - `[observation unknown] 当前宿主未观测到（历史可能仍存在）`——没有有效证据
    （旧格式观测、查询失败、这一拍没问），**不是**「已经没了」；
  - `[merging]`——本项目此刻的合并名额在它手里。
- `<id> pending (waiting for the next tick)` —— 刚投递、还没被插件消费的卡。
  刚 `add` 完看到这条是**正常**的，不是失败。

只有待消费投递、还没有任何卡落在 `board.json` 时，第一行会是
`no cards on the board`，后面照样跟 `pending` 行——这是"插件还没 tick 到"，
不是"什么都没投成功"。

**如实展示，不要粉饰。** 卡片失败、停在 `NeedsYou` 或 `AwaitingMerge` 时照实说。

### 4. 结算：done / archive / purge

三条命令职责不同，按用户意图选，不要混用：

| 用户说 | 命令 | 前提 |
|---|---|---|
| "我合并了 / 更新看板进度" | `superpowers-kanban done <id>` | 卡在 `awaiting_merge` |
| "从看板上隐藏它" | `superpowers-kanban archive <id>`（或 `archive --all-terminal`） | 卡不是活跃态 |
| "彻底删掉这条记录" | `superpowers-kanban purge <id>`（或 `purge --all-archived`） | 卡**已归档** |

- `done` 会核对推导出的 `kanban/<slug>` 分支是否已并入默认分支，打印
  `card <id> -> done (verified|branch-missing|not-merged); auto-archives in 24h`，
  但**不据此阻断**。三种判词都原样转述，`not-merged` 要醒目提示用户再确认。
  卡不在 `awaiting_merge` 会报 `card <id> is <State>, not awaiting_merge`。
- `archive` 拒绝**活跃卡**（`Queued` / `Running`），
  报 `illegal transition: <State> -> <State>`。别硬来，先让用户处理那张卡。
  归档只是隐藏，`board.json` 里还在，`list --all` 可见。
- `purge` 不可逆，且**只接受已归档卡**（未归档同样报 `illegal transition`）。
  用户没说"真要删掉"就不要主动提议。
- 进入 `Done`/`Failed`/`Cancelled` 的卡超过宽限期（`archive_grace_hours`，默认 24h）
  会自动归档；`needs_you` / `awaiting_merge` 永不自动归档。

### 5. 开关

看板的推进由开关控制（项目层覆盖全局层，默认关闭）：

```bash
superpowers-kanban on      # 开始推进队列
superpowers-kanban off     # 停止推进（已在跑的会话不会被取消）
```

`on`/`off` 只写**项目层**（`<项目根>/.yi-agent/preferences.json`），打印
`superpowers_kanban = true`、写入路径与 `now enabled|disabled`。
想改全局开关请让用户自己改 `~/.yi-agent/preferences.json`，不要代劳。

## 6. 与其他 skill 的冲突：谁执行 merge

`finishing-a-development-branch` 的"Option 1: Merge back to <base-branch> locally"
会直接 `git checkout <base> && git merge <feature>`。**在卡片会话里不要照做**——
那会绕过本插件的 `merge.lock`，与别的合并同时改主检出。

裁决顺序：

1. 会话跑的是**看板卡片的 worktree**（路径含 `.worktrees/kanban/…`）→ 走 §2b：
   用户确认后 `merge-request` 入队；daemon 排到后会**回到这个会话**再执行合并。
   **你自己**（这一轮）不执行 `git merge`。
2. 用户明确要在看板之外手工合并 → 让用户自己执行，再走 §4 的 `done` 登记。
3. 不涉及看板卡片的普通分支 → 按 `finishing-a-development-branch` 走，本 skill 不插手。

## 并发不是你能调的东西

每个时段的并发上限由文件名带日期的日历文件（`superpowers-kanban.toml`）决定，
典型配置是限流窗口 3 个任务、非限流窗口 10 个任务。
**不要**为了提高吞吐去调并发——限流是服务端约束，超了只会一起失败。
用户想改并发时，告诉他改日历文件（`<项目根>/.yi-agent/superpowers-kanban/superpowers-kanban.toml`，
或在桌面「设置 → 插件 → superpowers-kanban」里改），而不是你替他决定。

## 失败时

| 现象 | 含义 | 你怎么做 |
|---|---|---|
| `superpowers-kanban: command not found` | 插件没装 | 按插件的 `INSTALL.md` 装，或告诉用户未安装 |
| `usage: superpowers-kanban …` 且退出码 2 | 子命令缺失或拼错 | 对照 §命令全景，不要猜 |
| `spec file does not exist: …` / `plan file does not exist: …` | 路径不对 | 把路径回给用户确认，不要猜 |
| `spec and plan must be different files` | 传了同一份文件 | 让用户给出一对文件 |
| `unknown card: <id>` | 卡还没被 tick 建出来，或 id 打错 | 先 `list` 确认；预排队卡需等下一次 tick |
| `card <id> is not pre-queued (state: <State>)` | 卡已被提升过或根本不是预排队卡 | 如实回报状态，不要重试 |
| `card <id> is <State>, not awaiting_merge` | `done` 用错了状态 | 让用户确认卡是否真的合并完 |
| `busy: another merge is running…` | 本项目已有合并在跑 | 告诉用户排队中，等用户再说一次 |
| `等待恢复` / 卡转 `needs_you` 且 detail 提到 `could not be resumed` | 原开发会话无法冷恢复（开发树被删、日志损坏、锁被占） | 让用户按 INSTALL 的「人工恢复」处理；**不要**另开一张卡或新开会话重跑 |
| `denied: …` | 名额准入拒绝 | 原样转述原因，不要绕过 |
| `illegal transition: <A> -> <B>` | `archive`/`purge` 的前提不满足 | 按 §4 的顺序处理（先归档再 purge） |
| `list` 长时间只有 `pending` | 插件进程没跑，或开关是关的 | 先用 `on` 打开开关；仍不动则查插件是否在运行 |

### 观测角标的升级矩阵

「当前宿主是否加载着这个会话」这条信息要**宿主、插件、UI 三者都新**才完整。混装时
按下面理解，别把降级当故障：

| 组合 | 你会看到 | 含义 |
|---|---|---|
| 新 UI + 旧 runner | 只有旧文案（可能说「会话已不在」/「最后活动」） | UI 的修正没生效，**不是**看板在撒谎；升级 runner |
| 新 runner + 旧宿主 | 角标只有 `[observation unknown]`，热发现的卡仍可能被标未加载 | 旧宿主不发完整热表快照，插件无法区分「没加载」与「没问」；升级宿主 + 重启 daemon |
| 旧 UI + 新 runner/宿主 | 旧硬编码文案 | 数据已经对了，只是渲染没更新；升级 UI |
| 三者都新 | `[stale] 当前宿主未加载（截至 <时间>）` / `[observation unknown] …` / 无角标 | 完整验收 |

升级顺序与重启要求（旧 daemon 持有旧 inode、skill 是文件副本只对新会话生效）见
`INSTALL.md` 第 9.1、9.6 节。**不要**为了让角标消失而自动执行 `on`/`off`/`state`/`restart`：
观测只描述事实，状态永远由模型或用户写明。

## 安装与升级

安装由三块组成（二进制、本 skill、supervisor 清单），完整步骤与每步的预期输出见
`INSTALL.md`。两条容易踩的线：

- **新装**：宿主通用 `plugin/install` 会让插件自己报清单
  （`superpowers-kanban manifest`），落盘并打开开关；daemon 每 500ms 对账，
  新装无需重启。全新目录也可以先用 TUI `/superpowers-kanban create` 建盘。
- **升级**：`install` 覆盖二进制后，已经在跑的 `run` 进程仍持有**旧 inode**，
  会继续用旧代码——表现为投递被静默拒绝、卡片不出现。必须让 `run` 退出一次
  （`pkill -f "superpowers-kanban run"`，监督器会拉起新进程），再核对 inode 一致。
  **skill 本体也要重新复制**到 `~/.yi-agent/skills/superpowers-kanban/SKILL.md`，
  且新会话才生效；旧的已开会话不会重新扫描。

## 卸载

看板是可卸载的。用户说"不要这个看板了"时，走 `INSTALL.md` 的卸载章节，
**三块都删**（清单、skill、二进制），不要留下半装状态（二进制在、skill 没了，或反过来）。
