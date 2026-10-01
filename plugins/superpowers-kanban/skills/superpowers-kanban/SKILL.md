---
name: superpowers-kanban
description: "Use when the user wants to add work to the Superpowers kanban queue - e.g. \"加到看板\", \"排队这个需求\", \"加入队列\", \"enqueue this\", \"run it later\", or asks what is queued. Enqueues an existing spec+plan pair into the board; it does not write specs or plans."
---

# Superpowers 看板入队

把一对**已经写好**的 spec + plan 投进看板队列，让插件在限流窗口内按顺序自动开发。
看板是一个独立插件：它不在主流程里，装了就可用，卸了就消失。

## 你的职责边界

**只入队。** 这个 skill 的唯一动作是调用 `superpowers-kanban` 二进制。

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

### 5. 找项目根

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
