---
name: git-diff-review
description: Use when the user would benefit from reviewing the code you changed in this conversation - e.g. you finished implementing a feature or fixing a bug and want them to review the diff. Opens the desktop app's Git Diff tab and drives the view with a read-only shell command you choose, such as git diff or git log.
---

# Git Diff Review

`show_git_diff` 把用户切到桌面端的 **Git Diff Tab**。视图内容不再由工具替你猜——
**你写一条只读命令**，命令在 read-only 沙箱里跑，它的 stdout 就是对方看到的东西。
所以这个技能的实质是：**挑一条合适的只读 git 命令 + 选对渲染原语**。

## 何时用

- 你刚完成一段实现或修复，改动**值得对方看一眼**再继续。
- 对方问「你改了什么 / 给我看看 diff / 让我 review 一下」。
- 一次交付前，想让对方确认改动范围。

不要在纯问答、只读探查、或改动还没成形时调用——那时打开空视图只会打扰。

## 调用长什么样

```
show_git_diff({ "command": "git diff --stat", "note": "先给你看改动规模" })
```

参数：

- `command` — 一个 shell 命令字符串，在**只读沙箱**里执行，stdout 即视图。这是主接口。
- `as` — 可选渲染原语：`diff` | `log` | `text`。省略（或给一个不在三值里的值）时
  后端按内容自动分类；给了一个**合法但与内容不符**的值时后端照信，
  前端会退回纯文本渲染。所以**不确定就省略**，让后端分类。
- `base` — 比较基准，**只在不传 `command` 时**才有意义（默认取本会话起点，
  即开会话时的 HEAD；拿不到时退回默认分支候选）。
  传了 `command` 时它不起作用，比较对象由你自己的命令决定。
- `note` — 给用户看的一句短说明。

## 命令样例（可直接抄）

| 目的 | 命令 |
| --- | --- |
| 先探规模，别一上来糊一大坨 | `"git diff --stat"` |
| 看最近的提交在做什么 | `"git log --oneline -10"` |
| 看**还没提交**的工作区改动 | `"git diff HEAD"` |
| 只看某个目录/文件 | `"git diff <base> -- <path>"` |
| 看某个提交本身 | `"git show <sha>"` |
| 看工作区有哪些新文件 | `"git status --short"` |

> **合并提交要加 `--first-parent`**：`git show <merge>` 对干净合并输出为空、
> 对带冲突解决的合并输出组合 diff（`diff --cc`），面板都显示不出东西。
> 看合并提交用 `"git show --first-parent --format= <sha>"`。

组合示例：

- 完成一个功能后：`show_git_diff({ "command": "git diff --stat", "note": "改了这三处，麻烦 review" })`
- 对方说「还没提交的先看看」：`show_git_diff({ "command": "git diff HEAD" })`
- 对方想看历史脉络：`show_git_diff({ "command": "git log --oneline -10", "as": "log" })`
- 只想聚焦一个子目录：`show_git_diff({ "command": "git diff HEAD -- crates/foo/src" })`

改动很大时**先给统计**（`git diff --stat`），再按对方关注的文件出一次 `git diff <base> -- <path>`；
一次糊上整份超长 diff 只会淹掉重点。

## 怎么选 as

- 输出是 unified diff（`git diff`、`git diff HEAD`、`git show`）→ `"as": "diff"`。
- 输出是提交历史（`git log`）→ `"as": "log"`。
- 其它（`git status`、统计表、自定义输出）→ `"as": "text"`。

不确定就**省略 `as`**，交给后端分类，比猜错降级成纯文本更好。

## 输出的边界

视图有上限，命令输出过大时会被变形：

- 正文超过 **256 KB** 会被截断（面板上会标「截断」）；
- 命令超过 **10 秒**会被判超时并杀掉。

所以别用 `git log -p`、`git diff` 这类可能一次吐几十万行的命令去糊满屏；
先 `git diff --stat` 看规模，再按文件/目录逐个看。

## 只读沙箱（重要）

`command` 在 **read-only 沙箱**里执行，它**无法修改工作树**。
任何会改状态的 git 命令——`git stash push`、`git checkout`、`git switch`、`git commit`、
`git reset`、`git clean`、`git add`——都会被沙箱挡住（命令非零退出），写了只会白跑一趟；
它不会改出半个状态，所以也别指望用这种命令「探路」。

- 想看未提交改动，直接 `"git diff HEAD"`，不要试图先 stash 再看。
- 想换 base 比较，用 `"git diff <ref>"` 指定 ref，不要 checkout 过去。
- 需要写文件/改状态时，用别的工具，别塞进 `command`。

## 工具不可用时

若 `show_git_diff` 不在你的工具集里（例如会话不在 git 项目内、或委派不可用），
就**自己跑 git** 并把结果用文字汇报，**不要贴整份超长 diff**：

- `git diff --stat` 拿文件列表和改动规模，先报给对方；
- `git diff <base> -- <path>` 看某个文件，只摘关键 hunk；
- `git log --oneline -10` 说明最近的提交脉络。
