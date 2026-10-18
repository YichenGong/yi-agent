# 模型驱动的 Git 视图（不写死展示内容）设计

日期：2026-10-18
状态：设计已定稿，待用户 review 后进入 writing-plans。
前置：`docs/superpowers/specs/2026-10-04-thread-git-diff-tab-design.md`（本设计取代其 §5 的「基准只有 merge-base」与 §8 的工具形态）。

## 1. 背景与问题

### 1.1 现象

桌面端会话详情栏的 Git Diff Tab 展示的东西过多。在本机实测（主仓库、`main` 上开会话）：

```
resolve_base 命中            refs/remotes/origin/HEAD   (baseKind=origin-head)
其 sha                       b09828b0  (= origin/main)
HEAD                         b497488f  (= 本地 main)
merge-base HEAD origin/HEAD  b09828b0
git rev-list --count origin/HEAD..HEAD   174
git diff --numstat | wc -l               169 files
unifiedDiff 原始大小                       2,335,164 字节 → 被 MAX_DIFF_BYTES(256KB) 截断
面板头部读成                 对比 refs/remotes/origin/HEAD · merge-base b09828b · 174 commits · 175 files
```

即展示的是「本地 main 相对 origin/main 的全部历史欠账」，而非「这次会话改了什么」。

### 1.2 根因

两层，缺一不可：

1. **没有会话锚点。** `ThreadMeta`（`thread_store.rs:39`）只有 `created_at`，没有任何「会话开始时 HEAD 是哪个 sha」的记录。默认基准只能退化为「跟默认分支比」。
2. **展示内容是写死的。** `show_git_diff` 只接受 `base`（单个 ref），`thread/diff/read` 只支持「基准 diff / 单 commit / 单文件」三种取数，前端只渲染 unified diff。模型无法表达「看最后三个提交」「看某目录的改动」。

### 1.3 附带查出的实现缺陷（本设计一并修）

| # | 缺陷 | 证据 |
| --- | --- | --- |
| D1 | 截断后文件列表与内容脱节：`files[]` 列出 175 个文件，`unifiedDiff` 只含 54 个文件正文，其余展开是空白块 | `GitDiffView.tsx` 对无正文文件走 `parsed.find(...) ?? { ...f, hunks: [] }`；前端从不发 `path` 参数 |
| D2 | 合并提交点开是空白：`commit_diff` 用 `git show --format= <sha>`，对 merge 输出 0 字节（本次 174 个提交里 35 个是 merge） | 实测 `git show --format= 77785538` = 0 字节；`--first-parent` = 39,284 字节 |
| D3 | `show_git_diff` 回执撒谎：按**入参**回报基准，而 `resolve_base` 会静默回退 | `git_diff_tool.rs:113`；传 `no-such-ref` 时回执说「against no-such-ref」，实际用在 `origin/HEAD` |
| D4 | 汇总行缺增删统计，且直接打印原始 ref | spec §6 要求 `N commits · M files · +X −Y`；实际只有 `N commits · M files`，base 显示 `refs/remotes/origin/HEAD` |
| D5 | **git 调用完全绕过沙箱，且可注入**：不是 bug 而是安全面 | 见 §4.1 实测 |

## 2. 决策记录

| 问题 | 决定 |
| --- | --- |
| 展示内容的决定权 | **模型**。模型给 git 命令，工具执行并把结果渲进视图。不写死取数逻辑。 |
| 工具接口 | `show_git_diff({ command?, as?, note? })`。`command` 省略 = 走结构化默认（会话锚点 diff）。 |
| 渲染原语 | 有限集合：`diff` / `log` / `text`。`text` 是保底，永不失手。**渲染原语必然是有限集合**——见 §3.2。 |
| 从自然语言到视图 | 模型把自然语言翻成 git 命令。软件不解析自然语言。 |
| 默认基准（无命令时） | **会话锚点**：`thread/start` 时记录的 HEAD sha。锚点缺失则落回既有的默认分支候选链。 |
| 命令执行位置 | app-server，经 `SandboxPolicy` 的 **ReadOnly** 模式（git 视图命令全是只读的）。 |
| 命令的权威性 | 命令与退出码**随帧下发**并显示在视图头部，用户可自行复核。 |
| 帧是否落盘 | **不落盘**。与既有 diff 一致，现算。「刷新」= 重跑同一条命令。 |
| 注入防护 | 所有拼接模型字符串的 git 调用加 `--end-of-options`，并对 ref/命令做校验。 |
| 旧会话锚点 | **不回填**（方案 1）。无锚点的会话维持既有行为；不回填、不读时写入。 |
| 合并提交的展示 | `commit_diff` 改用 `--first-parent`。 |

### 2.1 为什么「不写死」不等于「前端什么都能渲染」

前端只能把字节流渲成三种原语之一。这不是偷懒，是渲染的定义。因此真实的自由度是：
**模型决定跑什么命令 + 用哪个原语展示**；原语清单由前端定义并可校验。任何「模型给什么都能渲染」的承诺都是无法实现的，设计里必须显式承认这个边界。

## 3. 目标与非目标

**目标**

- `show_git_diff` 接受任意 git 命令；模型据此决定「展示什么」，软件不再写死取数逻辑。
- 默认路径（用户自己点开 Tab、模型不指定命令）用会话锚点，展示「这次会话改了什么」。
- 命令在只读沙箱内执行，有超时与输出上限；命令与退出码随帧可见。
- 修掉 D1–D5。
- 旧会话、非 git 目录、命令失败、输出超长等边界各有明确表现。

**非目标**

- 不做 git 命令的白名单。模型已有 `bash`；白名单只能制造「看起来安全」的假象，真实防护靠沙箱与超时。
- 不解析自然语言（那是模型的事）。
- 不落盘视图帧，不做 diff 缓存。
- 不做 side-by-side、word-level 高亮、行批注。
- 不改「子 agent」「后台进程」两个 Tab 的既有行为。
- 不做会话内「按 turn 划分改动」。锚点是会话级近似，多任务会话仍会混在一起（§6 已列为已知限制）。

## 4. 架构

### 4.1 现状安全面（D5 实测）

现有 `git_diff.rs` 用**裸 `std::process::Command`** 调 git，不经 `SandboxPolicy`（而 `bash` 工具是经沙箱的，`bash.rs:14`）。且模型可控的字符串直接拼进 git：

```
git diff --numstat "--output=/tmp/pwn_probe.txt" HEAD~1   → /tmp/pwn_probe.txt 被创建
git show --no-color --format= "--output=/tmp/pwn3.txt" HEAD → /tmp/pwn3.txt 被创建
```

`base` 参数因过 `rev-parse --verify` 校验而被挡住：

```
git rev-parse --verify --quiet "--output=/tmp/pwn4.txt"  → 失败，文件未创建
```

挡板实测有效且不影响范围表达式：

```
git diff --no-color --end-of-options "--output=/tmp/pwn5.txt"  → fatal，文件未创建
git diff --no-color --end-of-options HEAD~1..HEAD              → 正常输出 diff
```

**说明**：模型经 `show_git_diff` 只能传 `base`（有校验）；`commit` 无校验而拼进 `git show`，但它的来源是 RPC 客户端（桌面端点 commit 列表传 sha），不是模型。本设计会扩大这个面（模型将能传命令），故 D5 必须先修。

### 4.2 数据流

```
模型：show_git_diff({ command?, as?, note? })
        │
        ├─ 无 command ─► 结构化默认：会话锚点 diff（A 路径）
        │
        └─ 有 command ─► 校验 → SandboxPolicy(ReadOnly) 执行（超时 + 上限）
                          │
                          ▼
                   ViewFrame { command, as, exitCode, stdout, truncated, note }
                   按 threadId 存进内存（每会话一帧）
                          │
                          ▼ 广播
                   ui/gitDiff/focus { threadId, note }
                          │
用户：点 Tab / 收到推送 ───┤
                          ▼
        前端：打开面板 + 切 Tab ──► thread/view/read { threadId } ──► 取整帧
                          │
                          ▼
        frontend：按 as 选原语渲染（diff / log / text），命令与退出码显示在头部
```

与既有「进程」Tab 同构：通知只说「变了」，RPC 给权威数据。

### 4.3 新增/改动单元

| 单元 | 位置 | 职责 |
| --- | --- | --- |
| `session_anchor` | `git_diff.rs` | 取 `rev-parse HEAD`;校验 ref/范围;`--end-of-options` 封装;超时执行 |
| `ViewFrame` | `git_diff.rs` | 帧类型 + 按 threadId 的内存快照 |
| `thread/view/read` | `server.rs` | RPC：取该会话的当前帧 |
| `show_git_diff` 扩展 | `git_diff_tool.rs` | 接受 `command`/`as`;执行并回执**实际**结果 |
| `ui/gitDiff/focus` | `protocol.rs` + `server.rs` | 通知（沿用既有方法名，去掉 `base` 字段——帧已承载基准） |
| 渲染原语 | `desktop/src/lib/viewFrame.ts` | 原语判定与解析（`diff` 复用 `gitDiff.ts`,`log` 新增,`text` 纯展示） |
| `GitDiffView` 扩展 | `desktop/src/components/` | 按原语渲染 + 命令条 + 懒加载（D1 修复） |
| 锚点字段 | `thread_store.rs` + `server.rs` | `ThreadMeta.anchor_sha`;`thread/start` 写入 |

## 5. 逐项设计

### 5.1 A：会话锚点作为默认基准

- `ThreadMeta` 新增 `anchor_sha: Option<String>`，带 `#[serde(default)]`（零迁移，旧 meta 读成 `None`）。`rebuild_meta`（`thread_store.rs:829`）也补 `None`——损坏回退无权编造锚点。
- 在 `start_thread_core` 建 meta 处（`server.rs:5398`）同 `now` 一起取值：跑 `git -C <cwd> rev-parse HEAD`，git 仓库则 `Some(sha)`，否则 `None`。现有 `already_persisted` 守卫保证 resume 不覆盖（已核实）。
- `resolve_base(repo, explicit, anchor)`：优先级 **explicit → anchor → `refs/remotes/origin/HEAD` → `origin/main`/`origin/master` → `main`/`master` → 仅工作区**。新增 `baseKind = "session"`；`"explicit"` 仍只在调用方显式传入时出现。
- 无锚点（全部既有会话）→ 落回今天的行为。**不回填**：读操作不产生写入，也不引入「首次打开时锚点=此刻」这种语义分歧。

效果：锚点 ≈ 分叉点（superpowers 标准流程下与旧 spec 等价），而在长命分支上给出「本次会话的改动」。

### 5.2 L1：地基（安全与有界）

- **`--end-of-options` 覆盖**所有拼接模型字符串的 git 调用。
- **ref / 范围校验**：单 ref 走 `rev-parse --verify --quiet`；范围表达式（含 `..` / `...`）走 `rev-parse --verify` 的**范围形式校验**（实测 `rev-parse --verify 'A..B'` 会失败，故范围需按 `..` 拆成两端分别校验，两侧都必须可解析）。
- **超时**：默认 10s（`VIEW_TIMEOUT`）。超时即失败帧，回执如实报 timeout。
- **输出上限**：`MAX_VIEW_BYTES = 256 * 1024`（沿用现值），超出置 `truncated`。
- **沙箱**：`SandboxPolicy::new(SandboxMode::ReadOnly, cwd, vec![])`，`app-server` 已依赖 `yi-agent-tools`，直接复用。ReadOnly 下写操作被拒——模型即使给恶意命令也落不了盘。

### 5.3 L2 + L3：命令即接口

工具签名：

```jsonc
{
  "command": "git log --oneline -10",   // 可选。省略 = 结构化默认（会话锚点 diff）
  "as": "diff" | "log" | "text",        // 可选。省略 = 由内容判定（auto）
  "note": "…"                           // 可选，一句面向用户的说明
}
```

- `command` 省略 → 走 §5.1 的结构化默认，等价于今天的行为（但基准换成锚点）。这是「用户自己点开 Tab」和「模型只说『看看 diff』」的路径。
- `command` 给出 → 在只读沙箱内执行，取 stdout 成帧。
- `as` 省略 → 后端按内容判定：含 `diff --git` 判为 `diff`；匹配 commit 行格式判为 `log`；否则 `text`。
- `as` 给出但与内容不符 → 前端回退 `text`（保底永不失手）。

这样「基准范围 / 看 log / 只看某路径 / 看任意 git 输出」全部由模型表达，软件不再枚举。

### 5.4 帧与 RPC

```rust
pub struct ViewFrame {
    pub command: Option<String>,  // None = 结构化默认
    pub as_kind: String,          // diff | log | text
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub note: Option<String>,
    pub base: Option<String>,     // 结构化默认时的实际基准
    pub base_kind: String,        // session | explicit | origin-head | …
}
```

- **命令的执行时机是 `show_git_diff` 调用时，不是 `thread/view/read` 时。** 这样模型写错命令能立刻从回执看到失败（§5.5），而刷新按钮按需重跑。
- 快照按 `threadId` 存在内存（`HashMap<String, ViewFrame>`），与 `pending_activation` 同类。
- RPC `thread/view/read { threadId }` → 返回该帧（无帧则 `null`，前端显示空态）。`{ threadId, refresh: true }` → 重跑帧里的 `command`（或重算结构化默认）后返回新帧。
- 「刷新」= 重跑，**不落盘**。
- **既有 RPC 保留**：`thread/diff/read` 与其响应类型（`ThreadDiffResult` / `DiffTextResult`）**不删**——§5.6 的单文件懒加载仍用它，前端 diff 解析路径也不动。新帧承载「模型给的命令」，旧 RPC 继续承载「会话级 diff 的结构化取数」。
- **状态归属**：帧与既有 diff 快照一样是「有主」的——切会话必须复位，迟到的响应不得画进另一条会话的视图。沿用 `diffOwnerRef` / `diffReqRef` 的既有两道门（`App.tsx:326`）。

### 5.5 回执（D3 修复）

工具返回**实际发生的事**，而不是复述入参：

- 实际执行的命令（或「结构化默认」）、实际采用的 base 与 baseKind；
- 退出码；stdout 字节数与是否截断；超时/沙箱拒绝等失败原因。

模型据此能如实向用户描述，且命令写错时**立刻**从回执看到失败，而不是等用户发现空白面板。

### 5.6 D1：截断不再产生空白文件

- 前端：文件已展开但无正文时，发 `thread/diff/read { threadId, path }` 拉该文件，按 path 缓存后解析；确实无 hunk 的文件显式标注「无内容变化」，而不是看起来像坏了。
- 后端：`file_diff` 对**未跟踪文件**必须合成正文——实测现在返回 0 字节（`git diff <mb> -- untracked.txt` 为空，因为它不在任何 diff 里）。复用既有 `untracked_diff` 助手。
- 这同时把 `file_diff` / `path` 分支从死代码变成真正的加载路径。

### 5.7 D2：合并提交

`commit_diff` 改用 `git show --no-color --first-parent --format= <sha>`。实测：对非 merge 与 `--format=` 完全等价；对 merge 给出一份连贯 diff。不采用 `-m`——它把两个父提交的 diff 拼接（实测同一改动列两遍），读起来像重复列举。
代价：放弃「相对第二个父提交的冲突解决增量」。对 review 场景，「这次合并带进来了什么」才是有用的问题。

### 5.8 D4：汇总与文案

- 头部补 `+X −Y`（从 `files[]` 求和，不额外调 git）。
- `baseKind` 渲染为可读标签（`session` → 「本次会话」），不再直接打印原始 ref。
- 有 `command` 时头部显示命令原文与退出码。

## 6. 错误与边界

| 情形 | 表现 |
| --- | --- |
| 非 git 目录 | 结构化默认返回 `worktree-only`；命令执行返回退出码非 0 的失败帧 |
| 锚点不可达（rebase / gc 掉） | 降级到默认分支候选链，**不**降级到「仅工作区」（后者会让 diff 突然只剩未提交改动，比报错更难理解） |
| 命令超时 | 失败帧，回执报 timeout |
| 沙箱拒绝写 | 命令失败，退出码非 0，stderr 随帧可见 |
| 输出超长 | 截断 + `truncated`，文件列表不受影响 |
| 原语判定错 | 回退 `text` |
| 无帧 | 空态文案 |
| 会话内多个任务 | **已知限制**：锚点是会话级，多任务会话的改动会混在一起。不做按 turn 划分。 |

## 7. 测试

**Rust**

- `resolve_base`：锚点优先级（explicit > anchor > 候选链 > worktree-only）；缺 `anchor_sha` 的旧 meta；锚点经 resume 存活。
- 注入：`--output=` 类参数被挡；范围表达式仍正常工作。
- 校验：合法范围通过；`bogus..HEAD` 被拒。
- 超时：超时命令得到失败帧而非挂住。
- `file_diff`：未跟踪文件有正文。
- `commit_diff`：merge 有正文，非 merge 与旧行为等价。
- 回执：报实际 base，而非入参。
- 帧：每个会话一帧；`thread/view/read` 取到；无帧返回 null。

**TS**

- `viewFrame.ts`：三种原语的判定与解析；`as` 与内容不符时回退 `text`。
- `GitDiffView`：命令条（命令 + 退出码）；汇总含 `+X −Y`；`session` 文案；懒加载单文件；已加载但无 hunk 的文案。
- 端到端：`ui/gitDiff/focus` → 打开面板 + 切 Tab + 发起 `thread/view/read`。

每个缺陷（D1–D5）配一条回归测试——现有 14 个绿灯正是因为完全没覆盖这些点。

## 8. 已知限制（明确不解决）

1. **锚点是近似**：用「会话边界」近似「变更边界」。多任务会话混在一起；锚点被 rebase 掉后降级到默认分支。
2. **渲染原语有限**：模型给的内容必须是 diff / log / text 之一，否则退化 `text`。
3. **可测试性下降**：输入从结构化变成任意字符串，测试只能覆盖样例集，不能穷举。
4. **模型不可靠**：命令写错 → 空或错内容。缓解是命令可见 + 退出码随帧 + `text` 保底，但不消除。
5. **旧会话仍难看**：无锚点的既有会话维持旧行为。
6. **放宽了命令面**：模型能跑 git 命令，防护靠只读沙箱 + 超时 + 输出上限，而非白名单。
