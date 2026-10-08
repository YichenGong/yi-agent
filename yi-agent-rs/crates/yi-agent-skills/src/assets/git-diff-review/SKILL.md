---
name: git-diff-review
description: Use when the user would benefit from reviewing the code you changed in this conversation - e.g. you finished implementing a feature or fixing a bug and want them to review the diff. Opens the desktop app's Git Diff tab, optionally choosing the comparison base.
---

# Git Diff Review

请用户 review 你改动的代码时，用 `show_git_diff` 工具把他切到桌面端的
**Git Diff Tab**——那里用接近 GitHub 的方式展示本会话相对**分支分叉点**的
全部改动（已提交 + 未提交 + 新文件）。你自己不要抄 diff 文本：diff 由 git 现算。

## 何时用

- 你刚完成一段实现或修复，改动**值得对方看一眼**再继续。
- 对方问「你改了什么 / 给我看看 diff / 让我 review 一下」。
- 一次交付前，想让对方确认改动范围。

不要在纯问答、只读探查、或改动还没成形时调用——那时打开空 diff 只会打扰。

## 怎么选 base（比较基准）

默认（不传 `base`）就已经是正确选择：应用会算
`git merge-base HEAD <默认分支>`，也就是**本分支从默认分支分叉出来的那个点**，
diff 即「分叉以来的全部改动」。

只有在你**明知**该跟别的东西比时才显式传 `base`：

- 你新开了一条分支且分叉点不是默认分支 → 传分叉前的那个分支名或 sha。
- 对方明确说「跟某个 tag / 某个提交比」→ 传那个 ref。
- 对方只想看**还没提交**的部分 → 传 `HEAD`（此时 diff 只剩工作区改动）。

不确定就别传：默认规则比你猜得更准。

## 举例

- 实现完一个功能：
  `show_git_diff({ "note": "这个功能改了三处，麻烦 review 一下" })`
- 对方说「跟我上次的提交比」：
  `show_git_diff({ "base": "<那个提交的 sha>" })`
- 对方说「还没提交的先看看」：
  `show_git_diff({ "base": "HEAD" })`

## 工具不可用时

若 `show_git_diff` 不在你的工具集里（例如会话不在 git 项目内、或委派不可用），
就**自己跑 git** 并把结果用文字汇报：

- `git merge-base HEAD main` 找分叉点，`git diff <分叉点>` 看改动；
- 把文件列表和关键 hunk 摘给对方，不要贴整份超长 diff。
