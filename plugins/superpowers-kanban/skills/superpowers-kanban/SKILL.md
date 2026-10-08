---
name: superpowers-kanban
description: "Use when the user wants to add work to the Superpowers kanban queue - e.g. \"加到看板\", \"排队这个需求\", \"加入队列\", \"enqueue this\", \"run it later\", or asks what is queued. Enqueues an existing spec+plan pair into the board; it does not write specs or plans."
---

# Superpowers 看板入队

把一对**已经写好**的 spec + plan 投进看板队列，让插件在限流窗口内按顺序自动开发。
看板是一个独立插件：它不在主流程里，装了就可用，卸了就消失。

## 你的职责边界

**只入队。** 这个 skill 的唯一动作是调用 `superpowers-kanban` 二进制。
合并卡同样**只入队**：你只负责投递，执行与串行由插件进程负责。

- **不要**生成、修改或"顺手补全" spec 或 plan。
- **不要**替用户决定两条需求该不该合并成一张卡。合并与否由用户在界面上决定；
  你只负责如实展示状态。
- **不要**直接写 `board.json`、`inbox/` 下的文件或 `preferences.json`。
  这些是插件进程的落盘格式，绕过 CLI 写会破坏它的原子性假设。

## 前提：spec 和 plan 必须已经存在

看板是 Superpowers SDD 流程的**下游**。上游是 `brainstorming` → 写 spec →
`writing-plans` 写 plan。用户说"这个还没做设计"时，先把他引到 SDD 流程，
拿到两份文件之后再回来入队。

## 触发时机

出现以下意图时使用本 skill：

- 用户说"把这个加到看板 / 排队 / 入队 / enqueue / 待会儿跑"。
- 用户刚写完一份 spec 和 plan，说"那就让它跑吧"。
- 用户问"现在队列里有什么 / 跑到哪了"。

## 怎么做

### 1. 确认两份文件

`<spec>` 和 `<plan>` 是**文件路径**，不是内容。动手前先确认两者都真实存在：

```bash
test -f <spec> && test -f <plan> && echo ok
```

缺失任一份就停下来问用户，不要猜路径、不要新建占位文件。
两份必须是不同文件（同一份 spec+plan 不成对）。

### 2. 入队

在**项目根目录**下执行（默认状态目录就是 `<项目根>/.yi-agent/superpowers-kanban`）：

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

### 2b. 把一张实现卡合并进主线（主路径）

用户在同一张卡片的**原会话**里说「合并 / 把这条合进 main / 这张卡可以合了」时，
你**不再**自己直接 `git merge`，而是走名额：

1. **申请名额**：

   ```bash
   superpowers-kanban merge-request <card-id>
   ```

   - 打印 `granted`：记下它给的 `workdir`，继续第 2 步。
   - 打印 `busy: another merge is running in this project`：本项目已有合并在跑。
     **如实告诉用户「排队中」**，停下来等用户稍后再说一次。不要轮询重试。
   - 报 `denied: …`：把原因原样转述（多半是卡不在 `awaiting_merge`，或 source 分支不存在）。
     不要猜、不要绕。

2. **在名额给的 worktree 里合并**：

   ```bash
   git -C <workdir> merge --no-ff <source> -m "merge <source> into <base> (kanban)"
   ```

   `granted` 输出里的 `merge <source> into <base> in that worktree` 一句就是这条命令的参数来源。
   **必须在 `<workdir>` 里执行**——它是 base 分支被检出的地方（base 就是 main 时即主检出）。
   这会话的 cwd 是当初实现卡的工作区，**不是** `<workdir>`，所以不加 `-C` 会合错地方。

   - 干净合并：继续第 3 步。
   - 有冲突：就地解决（这是你被叫来的原因）。解决后 `git -C <workdir> add` 冲突文件并
     `git -C <workdir> commit --no-edit` 收尾合并提交。
   - 解决不了：**不要** `git merge --abort` 后就完事——照第 3 步如实回执，
     让卡停在待处理，并在回复里说清卡在哪些文件。

3. **回执让插件复核**：

   ```bash
   superpowers-kanban merge-finish <card-id>
   ```

   - `done`：git 复核确认 `source` 已并入 `base`。告诉用户已合并。
   - `needs_you`：复核不通过（合并没真正落地）。**如实说**，并把卡的现状回报。
   - `cleared`：卡已不在合并中（多半已被别处推进）。不要自行改状态。

**绝不绕过名额直接 `git merge`。** 名额（每项目一把 `merge.lock`）保证同一项目同一时刻
只有一个合并，绕过去会让两条分支同时改主检出。

### 2c. 投递一张独立的合并卡（旧路径，保留）

需要为一个**没有实现卡**的分支（例如手工建的分支）单独立卡时：

```bash
superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <dir>]
```

`--base` 缺省取仓库默认分支（`origin/HEAD`，退化到 `main`）。命令**立刻**校验 refs 与
source 分支存在；失败原样转述，不要猜分支名、不要重试。

### 3. 必要时确认状态

```bash
superpowers-kanban list
```

`list` 会打印：

- 排队中的卡（按顺序编号）——看板会依次启动它们；
- 其他状态的卡（`Running` / `NeedsYou` / `AwaitingMerge` / `Done` / `Failed`），
  带 worktree 路径；
- `pending (waiting for the next tick)` —— 刚投递、还没被插件消费的卡。
  刚 `add` 完看到这条是**正常**的，不是失败。

**如实展示，不要粉饰。** 卡片失败或停在 `NeedsYou` 时照实说。

### 4. 开关

看板的推进由开关控制（项目层覆盖全局层，默认关闭）：

```bash
superpowers-kanban on      # 开始推进队列
superpowers-kanban off     # 停止推进（已在跑的会话不会被取消）
```

`on`/`off` 只写**项目层**（`<项目根>/.yi-agent/preferences.json`）。
想改全局开关请让用户自己改 `~/.yi-agent/preferences.json`，不要代劳。

### 5. 确认合并（更新看板进度）

**优先走 `### 2b`**（会话内申请名额并实际执行合并）。本节只用于「用户已在别处
手工合并完、只想让看板收尾」的情形。

用户说「我合并了这张卡 / 确认合并 / 更新看板进度 / 这张卡可以收了」时：

1. 先确认这张卡确实在 `awaiting_merge`（`superpowers-kanban list`）。
2. 调 `superpowers-kanban done <card-id>`，把打印的核对结果**原样转述**：
   - `verified`：推导出的 `kanban/<slug>` 分支存在且已并入默认分支；
   - `branch-missing`：分支已不存在（多半已合并后删除），按人工确认放行；
   - `not-merged`：分支存在但**尚未**并入 —— 醒目提示用户再确认。
3. 说明「卡片已放行，24h 后自动归档；要立刻隐藏可 `superpowers-kanban archive <card-id>`」。

**不要**代替用户执行 `git merge`——合并权归人，这里只登记「已合并」这一事实。

### 6. 找项目根

不确定 `--state-dir` 对应哪个项目根时：

```bash
superpowers-kanban workdir --state-dir <dir>
```

## 并发不是你能调的东西

每个时段的并发上限由文件名带日期的日历文件（`superpowers-kanban.toml`）决定，
典型配置是限流窗口 3 个任务、非限流窗口 10 个任务。
**不要**为了提高吞吐去调并发——限流是服务端约束，超了只会一起失败。
用户想改并发时，告诉他改日历文件，而不是你替他决定。

## 失败时

| 现象 | 含义 | 你怎么做 |
|---|---|---|
| `superpowers-kanban: command not found` | 插件没装 | 按插件的 `INSTALL.md` 装，或告诉用户未安装 |
| `spec file does not exist: …` | 路径不对 | 把路径回给用户确认，不要猜 |
| `spec and plan must be different files` | 传了同一份文件 | 让用户给出一对文件 |
| `list` 长时间只有 `pending` | 插件进程没跑，或开关是关的 | 先用 `on` 打开开关；仍不动则查插件是否在运行 |

## 卸载

看板是可卸载的。用户说"不要这个看板了"时，走 `INSTALL.md` 的卸载章节，
不要留下半装状态（二进制在、skill 没了，或反过来）。
