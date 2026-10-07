# 会话 Git Diff Tab（变更可视化）设计

日期：2026-10-04
状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

桌面端的子 agent 面板（`desktop/src/components/SubagentTrace.tsx`）目前只展示**轨迹**：
摘要是「最近步骤」，展开是 `foldTraceRows` 折叠出的轨迹块。它**只在选中某个子 agent
时**才渲染（`desktop/src/App.tsx:1387` 的 `currentId && openSubagent`），数据来自 app-server
的 `agent/trace/read` / `agent/trace/watch`。

用户希望在这块面板里 review **整个 root thread 的改动**，并有一个接近 GitHub
"Files changed" / "Commits" 的可视化。两个诉求：

1. **看得见**：面板加一个 Tab，用类似 GitHub 的方式展示该会话产出的 git diff，
   供用户 review。
2. **被推送到眼前**：新增一个**系统 skill + 工具**，让模型结合当前上下文
   （在什么分支、改了什么、该跟什么比）主动把这个视图推到用户眼前。

### 关键事实（已核实）

- **root thread 直接在项目目录里跑，不建 worktree**：
  `yi-agent-store/src/runtime.rs:914`「A root never provisions a worktree: it runs in
  the project directory」。因此 root 的 diff 就是「当前仓库工作区相对某个 base 的差异」。
- 用户按 superpowers 工作流开发时**自己会创建 worktree / feature 分支**，所以
  `merge-base HEAD <默认分支>` 通常就是「本分支的分叉点」，是最贴合直觉的比较基准。
- 后端**已有子任务级**的 diff：daemon IPC `ReadTaskDiff`（`yi-agent-store/src/ipc.rs:3855`）
  → `coordinator.delivery_diff`（`runtime.rs:3107`），但它比的是**子 agent 交付的 commit**
  （`base_ref...commit`），且 app-server 尚未暴露，与服务端的「root thread 全量 diff」不是一回事。
- 模型 → UI 已有现成范式：`SetThemeTool`（`app-server/src/theme_tool.rs`）写共享句柄并广播，
  由 `ui/settings/updated` 通知推回前端。本设计沿用同一形态。
- **系统 skill** 指打进 `yi-agent-skills/src/assets/`、开机安装到
  `~/.yi-agent/skills/.system/` 的内置 skill（见 `yi-agent-runtime/src/bootstrap.rs:430`、
  `yi-agent-skills/src/system.rs`），现有 `skill-creator` / `skill-installer` 两例。

## 2. 决策记录（本次头脑风暴敲定）

| 问题 | 决定 |
| --- | --- |
| 要看的 diff 是什么 | **整个 root thread 的 git diff**（不是单个子 agent 的交付）。 |
| 比较基准 | **分支分叉点** = `git merge-base HEAD <默认分支>`。 |
| 默认分支判定 | `refs/remotes/origin/HEAD` → `origin/main` → `origin/master` → `main` → `master`；皆无则退化「仅工作区改动」并标注。 |
| diff 范围 | **含未提交改动 + 未跟踪新文件**。 |
| 视图形态 | **C：Commits 列表 + Files changed 聚合 diff 都要**。 |
| 布局 | **unified（单列）**，不做 side-by-side 双列。 |
| 面板入口 | **复用**子 agent 面板，加 Tab **[轨迹 | Git Diff]**；点击卡片 / 点 Tab / 模型推送都能切换。 |
| 模型控制面 | **B：打开 + 选基准**（工具可传 `base`，默认自动 merge-base）。 |
| diff 计算位置 | **app-server**（只读 git 呈现），不新增 daemon IPC。 |
| skill 名 | **`git-diff-review`**。 |

## 3. 目标与非目标

**目标**

- 子 agent 面板变为常驻可开合的 `ThreadDetailPanel`，内含 **[轨迹 | Git Diff]** 两个 Tab。
- Git Diff Tab 以 GitHub 式渲染：汇总行 + Commits 段 + Files changed 段（可折叠、带行号与增删配色）。
- 后端新增只读 RPC `thread/diff/read`，在 thread 的 cwd 上现算 diff，支持 base / commit / path 三种取数。
- 新增工具 `show_git_diff` + 系统 skill `git-diff-review`，让模型能主动推送到该 Tab 并指定基准。

**非目标**

- 不改动、不暴露 daemon 的 `ReadTaskDiff`（它服务的是子任务交付 review，语义不同）。
- 不做 side-by-side 双列 diff、不做行内 word-level 高亮、不做评论/行批注。
- 不做 diff 的落盘或缓存（每次现算，保证实时）。
- 不改子 agent 轨迹 Tab 的既有渲染与交互。
- 不新增状态栏按钮（沿用现有「子 agent 面板」图标作为开合入口）。

## 4. 架构

两层，各司其职；diff 文本一律由 git 计算，模型只决定「看什么」。

```
模型：调 show_git_diff(base?)  ──►  GitDiffHandle 广播
                                        │
                                        ▼  watcher 转成通知
                               ui/gitDiff/focus { threadId, base }
                                        │
用户：点 Tab / 状态栏图标 ───────────────┤
                                        ▼
                     前端：打开面板 + 切到 Git Diff Tab
                           ── 发起 thread/diff/read { threadId, base? } ──►
                                        │
                     app-server：在 thread cwd 上跑 git（merge-base / diff / log / numstat）
                                        │
                                        ▼
                     返回 { base, baseKind, mergeBase, commits[], files[], unifiedDiff, truncated }
                                        │
                                        ▼
                     前端 lib/gitDiff.ts 解析 → GitDiffView 渲染
```

新增/改动单元一览：

| 单元 | 位置 | 职责 |
| --- | --- | --- |
| `git_diff.rs` | `yi-agent-app-server/src/` | 纯函数：base 解析、merge-base、commits、numstat、unified diff、未跟踪纳入、截断。 |
| `thread/diff/read` | `yi-agent-app-server/src/server.rs` | RPC：校验 threadId → 取 cwd → 调 `git_diff` → 返回。 |
| `git_diff_tool.rs` | `yi-agent-app-server/src/` | `show_git_diff` 工具 + `GitDiffHandle`（仿 `theme_tool.rs`）。 |
| `ui/gitDiff/focus` | `yi-agent-app-server/src/` | 通知：句柄广播 → watcher → hub 扇出。 |
| `git-diff-review` | `yi-agent-skills/src/assets/` | 系统 skill：教模型何时/如何请用户 review。 |
| `lib/gitDiff.ts` | `desktop/src/lib/` | 纯函数：unified diff 解析 → `FileDiff[]`。 |
| `components/GitDiffView.tsx` | `desktop/src/components/` | GitHub 式渲染：汇总 + Commits + Files。 |
| `components/ThreadDetailPanel.tsx` | `desktop/src/components/` | 常驻面板 + Tab 条，容纳轨迹与 Git Diff。 |
| `App.tsx` | `desktop/src/` | 面板可见性、Tab 状态、通知处理、RPC 调用接线。 |

## 5. 后端：diff 计算

放在 **app-server**，不放 daemon。理由：这是对「thread 工作目录」的只读呈现，而 cwd 本就由
app-server 掌握（`ThreadMeta.cwd`，`thread_store.rs:27`；`resolve_thread_cwd`，`server.rs:6022`）；
走 daemon 需新增 IPC + 转发，纯增负担、且 daemon 的 `delivery_diff` 语义也不是这里要的。

**新增模块 `git_diff.rs`**，只做纯函数 + 一个 `git_capture` 封装（复用 `runtime.rs:4921` 的形态）：

- `resolve_base(repo) -> BaseKind`：
  - 优先级：工具显式传入（校验 `rev-parse --verify` 通过）→ `refs/remotes/origin/HEAD`
    → `origin/main` → `origin/master` → `main` → `master`；
  - 全部不可用 → `BaseKind::WorktreeOnly`（只在工作区改动内 diff）。
- `merge_base(repo, base) -> Option<sha>`：`git merge-base HEAD <base>`。
- `commits(repo, merge_base) -> Vec<CommitInfo>`：`git log --format=... <merge_base>..HEAD`
  （短 sha、subject、author、时间戳），对应 GitHub 的 "Commits"。
- `files(repo, merge_base) -> Vec<FileStat>`：`git diff --numstat <merge_base>` 得状态
  (A/M/D/R) 与 +/- 行数；再并入 `git ls-files --others --exclude-standard` 的未跟踪文件。
- `unified_diff(repo, merge_base) -> String`：`git diff --no-color <merge_base>` +
  未跟踪文件以「新增文件全文」并入。
- `truncate_diff`：超过 `MAX_DIFF_BYTES = 256 * 1024` 截断并置 `truncated=true`
  （文件列表不受截断影响）。

**新增 RPC `thread/diff/read`**，参数 `{ threadId, base?, commit?, path? }`：

- 无 `commit` / `path` → 返回
  `{ base, baseKind, mergeBase, commits[], files[], unifiedDiff, truncated }`。
- 有 `commit` → 该提交单独 diff（`git show --no-color <sha>`，对应点开某条 commit）。
- 有 `path` → 单文件 diff（大 diff 时前端按文件懒加载）。
- threadId 校验沿用 `require_thread_id` / `require_known_thread`（`server.rs`）；cwd 从
  `thread_store` 的 meta 取，缺失时按既有兜底（`cfg.workdir`）。

**错误与边界**：非 git 仓库、找不到 base（退化 WorktreeOnly）、base 不可达（commit 级回退到
其父提交并标注）、超长截断、二进制文件（标注不显示主体）、重命名/删除的状态呈现。

## 6. 前端：GitHub 式渲染

**`lib/gitDiff.ts`（纯函数 + 类型）**

- 输入 unified diff 文本，输出
  `FileDiff[] = { path, status, additions, deletions, binary, hunks: Hunk[] }`；
- `Hunk = { header, lines: DiffLine[] }`，`DiffLine = { kind:'add'|'del'|'ctx', oldNo?, newNo?, text }`；
- 处理多文件、`\ No newline at end of file`、二进制、重命名、无 hunk 的文件头。
- 全部纯逻辑，单测覆盖。

**`components/GitDiffView.tsx`**

- **头部**：base 显示（如「对比 origin/main · merge-base `abc1234`」）+ 刷新按钮 +
  汇总（`N commits · M files · +X −Y`）。
- **Commits 段**：每行短 sha、subject、作者、相对时间；点击 → 请求 `commit` 取该提交 diff 并展示。
- **Files 段**：每文件可折叠，头部给状态徽标 + 路径 + `+/-` 统计；展开为带行号 gutter 的 hunk，
  `+` 绿 / `−` 红 / 上下文灰。默认只展开前若干文件，避免长列表一次渲染过多节点。
- **空/错误态**：非 git 目录、无改动、无基准、被截断，各有明确文案。

## 7. 面板 Tab 化

- 新增容器 `ThreadDetailPanel`，内部 Tab 条 **[轨迹 | Git Diff]**：
  - 「轨迹」= 现有 `SubagentTrace` 的主体（摘要 / 展开轨迹 / 子任务下钻 / 发消息 / 取消基本不动）；
  - 「Git Diff」= 新 `GitDiffView`。
- 面板可见性：`App.tsx` 引入 `panelOpen` 状态；**任一**路径都使 `panelOpen=true`：
  - 点子 agent 卡片 → 打开面板并切到「轨迹」（同时选中该 task）；
  - 点 Tab 条 → 切换 Tab；
  - 状态栏现有「子 agent 面板」图标（`App.tsx:1365`）→ 开合面板；
  - 收到 `ui/gitDiff/focus` → 打开面板 + 切「Git Diff」+ 以通知里的 base 发起 `thread/diff/read`。
- 「轨迹」Tab 在未选子 agent 时显示空态（「未选择子 agent」），而非让整个面板消失。

## 8. 系统 skill + 工具

**工具 `show_git_diff`**（新模块 `git_diff_tool.rs`，仿 `theme_tool.rs`）：

- 参数 `{ base?: string, note?: string }`；`note` 可选，作为一句面向用户的说明随通知带过去。
- 调 `GitDiffHandle`：广播 `(threadId, base, note)`；返回一句短回执（含**实际采用的 base**），
  让模型知道「现在展示的是什么」。
- 注册点与 `SetThemeTool` 完全对称：在 `register_theme_tool`（`server.rs:837`）与
  `registry_with_theme_tool`（`server.rs:848`）旁各加一处，保证委派与非委派两条路径都有。
- `GitDiffHandle` 需带 threadId（工具在哪个 thread 里被调，就把焦点推给哪个 thread）。

**系统 skill `git-diff-review`**（打进 `yi-agent-skills/src/assets/git-diff-review/SKILL.md`）：

- 教模型：何时该请用户 review（交付了一段可 review 的改动）；
- 如何依据上下文选 `base`（自己刚建分支 → 显式传 `origin/main` 或分叉前分支；已提交且想
  整体 review → 用默认；只关心工作区 → 传 `HEAD`）；
- 明确「工具只负责打开视图，diff 由 git 算」，不要自己抄 diff 文本；
- 工具不可用时的降级：自行跑 git 并以文字汇报。

## 9. 错误处理与测试

**Rust 测试**

- `git_diff`：base 解析优先级（显式 > origin/HEAD > origin/main ... > WorktreeOnly）；merge-base
  正确；commits 列表只含 `merge_base..HEAD`；numstat 状态与计数；未跟踪文件纳入；截断标志；
  非 git 目录返回明确结果；二进制与重命名。
- RPC：缺 `threadId` / 未知 thread 的错误码与既有约定一致；`commit` / `path` 分支取数。
- 工具：调用 `show_git_diff` 触发一次广播，且回执含实际 base；注册在两条路径都生效。

**TS 测试**

- `gitDiff.ts`：多文件、增删、`\ No newline`、二进制、重命名、无 hunk 文件头。
- `GitDiffView`：折叠、commit 列表、空/错误/截断态渲染。
- `App`：Tab 切换；收到 `ui/gitDiff/focus` 打开面板并切 Tab；点子 agent 卡片切回「轨迹」。
