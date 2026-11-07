# 互动延时：环境提示、cwd 反馈与搜索工具重写设计

日期：2026-11-06
分支：`docs/latency-search-spec`
状态：设计已经用户逐节确认（4 节全部通过），待用户审阅本 spec 后进入实施计划。

## 1. 背景与证据

用户反馈「每次互动的速度比较慢」，要求剖析各模块延时。剖析已完成并落盘在
`docs/research/2026-11-06-interaction-latency-profile.md`，可复现脚本
`scripts/latency_profile.py`。结论：单轮墙钟时间 90% 以上是 provider 首字节
等待（TTFB），我方 agent 循环 p50 = 1ms；真正的可优化项是**每任务的 provider
往返次数**与**文件搜索工具的同步全树扫描**。

本设计针对其中四个已确证的浪费来源。所有数字取自本机 trace
（`~/.yi-agent/trace/session-2026*.jsonl`）与真实命令计时，采集于 2026-11-05
与 2026-11-06。

### 1.1 证据 A：agent 不知道自己的工作目录

| 观测 | 数值 |
|---|---|
| 今日 `bash` 调用总数 | 1,476 |
| 命令以 `cd /绝对路径` 开头 | 1,137（75.7%） |
| 其中 `cd` 到的正是当前所在目录（纯 no-op） | 665 / 1,191 次 `cd`（55.8%） |
| 命令中出现机器绝对路径 | 1,232（82.1%） |
| 显式 `pwd` 探测 | 10 次 |

`yi-agent-core/src/system_prompt.rs` 的 `DEFAULT_SYSTEM_PROMPT` 全文未提及当前
工作目录。`shell/bash.rs` 的返回值只有 `exit: N\nstdout:\n...`，不回显 cwd，因此
即使 `ctx.set_cwd` 已跨调用保留了目录（`shell/bash.rs:547`），模型也看不到，
只能每轮重新 `cd` 自我保险。

trace 中还存在路径臆造：`cd /Users/gongyichen/dev/yi-agent 2>/dev/null || cd
~/dev/yi-agent 2>/dev/null`。真实路径是
`~/Documents/TechnicalStuff/projects/personalProjects/yi-agent`。

### 1.2 证据 B：搜索工具的同步全树扫描

| 观测 | 数值 |
|---|---|
| `yi-agent-rs/target` | 162 GB |
| 项目根文件总数 | 1,704,405（`.worktrees/` 占 769,914） |
| `.git` | 134 MB / 10,345 文件 |
| 2026-11-05 `grep path=yi-agent-rs` | 126.4 s |
| 2026-11-05 `glob **/Cargo.toml` | 14.8 s |
| 2026-11-06 两次根目录 `grep` | 各 178 s |

同一目录下 `find` 对照实测：

```
find . -name "*.rs"                     含 target      → 60 s 后仍未跑完
find . -name "*.rs" -prune target/.git  排除后         → 0.033 s，568 个文件
```

相差约 1800 倍，而实际目标只有 674 个 `.rs` 文件。

根因两条，均已核对源码：

1. `fs/grep.rs:76` 的 `call` 是 `async fn`，内部同步调用 `grep_search`
   （`:103`），用 `WalkDir` + `std::fs::read_to_string`（`:168`、`:186`）；
   `fs/glob.rs:73` 同理使用同步的 `glob::glob()`。**均无 `spawn_blocking`**，
   遍历期间占住 tokio worker。
2. 两者均不跳过 `target/`、`.worktrees/`、`node_modules/`、`.git/`。

### 1.3 证据 C：搜索工具的调用方式

| 观测 | 数值 |
|---|---|
| `grep` + `glob` 调用总数 | 2,087 |
| 带 `path` 参数 | 1,798 |
| 不带 `path`（等价全仓扫描） | 289 |
| 显式指向 `target` / `node_modules` | 0 |

模型从不主动扫描构建产物；长尾完全由默认遍历行为造成。

### 1.4 证据 D：探索轮次过长

| 观测 | 数值 |
|---|---|
| 只发 1 个 tool call 的轮 | 82.4% |
| 其中是 `bash` | 81.8% |
| 连续单工具轮链中位数 | 3 轮 |
| 最长连续单工具轮链 | 172 轮 |
| 单轮 TTFB p50 | 2.36 s |

一个 172 轮的串行链，仅 provider 往返就约 7 分钟。

## 2. 目标与非目标

### 目标

- 让 agent 在会话开始就知道自己的绝对工作目录，以及各文件工具与 `bash` 的路径
  基准，从而不再逐轮 `cd` 或猜测路径。
- 让 `bash` 每次执行后回显当前工作目录，形成「位置已确认」的反馈闭环。
- 让 `grep` / `glob` 默认跳过构建产物目录，并在有界时间内返回，且**明确告知
  是否发生了截断**。
- 引导 agent 在动手前一次性并行侦察，减少串行探索轮次。

### 非目标

- 不改变 provider、模型或网关参数。本设计不触碰 TTFB 本身。
- 不改变文件工具的相对路径基准。`roots[0]` 依旧是解析基准，与
  `cfg.workdir` 相同（`bootstrap.rs:441`），因此注入 prompt 的目录与工具基准
  天然一致，无需行为变更。
- 不引入 `git ls-files` 白名单、不引入每轮可变内容到 system prompt。
- 不重构 `side_read` 的既有有界 walker（其与 `SideReadAccess` 句柄授权强耦合）。
- 不调整 skills catalog 体积：实测固定前缀 5,038 token 中缓存命中 4,992，
  那部分几乎免费，不在本次范围。

## 3. 方案比较

### 3.1 工作目录信息的注入位置

| 方案 | 收益 | 代价 | 选择 |
|---|---|---|---|
| 注入 system prompt，会话开始组装一次 | 前缀稳定，prompt cache 继续命中 | 目录变更后提示不更新 | **采用** |
| 每轮在 user 消息里注入 | 目录总是最新 | 每轮前缀变化，破坏 cache，TTFB 从 2.4 s 推向 4 s 以上 | 不采用 |
| 只在工具描述里写 | 无额外 token | 描述适用于所有项目，无法给出具体绝对路径 | 不采用（但与方案 1 互补，见 4.3） |

### 3.2 搜索遍历的实现

| 方案 | 收益 | 代价 | 选择 |
|---|---|---|---|
| `ignore` crate，`hidden(false)` + `git_ignore(true)` | 复用 `.gitignore` 语义，零硬编码；162 GB 自动排除 | 新增依赖（本机缓存齐全，可离线编译） | **采用** |
| `walkdir::filter_entry` 手写跳过四个目录 | 无新依赖 | 对其他项目不通用；需维护列表 | 不采用 |
| 复用 `side_read::walk_candidates` | 已有 deadline 与候选上限 | 与 `SideReadAccess` 授权句柄强耦合，主 `grep` 无该对象，改造量远大于收益 | 不采用 |

### 3.3 `ignore` 默认值的取舍

`ignore-0.4.23/src/dir.rs` 的 `IgnoreBuilder::new()` 默认值：`hidden: true`、
`parents: true`、`git_global: true`、`git_ignore: true`、`git_exclude: true`、
`require_git: true`。

| 默认值 | 后果（本仓库实测） | 决定 |
|---|---|---|
| `hidden: true` | `.github/` 下 6 个文件（含 `workflows/ci.yml`、`release.yml`）完全搜不到 | 关闭 → `hidden(false)` |
| `git_global: true` | 本机 `~/.config/git/ignore` 含 `**/.claude/settings.local.json`，会静默改写结果 | 关闭 → `git_global(false)` |
| `require_git: true` | 非 git 目录下全部规则失效 | 关闭 → `require_git(false)` |
| `git_ignore: true` | 99% 收益来源（`target/`、`.worktrees/`、`node_modules/`） | 保留 |
| `git_exclude: true` | 尊重 `.git/info/exclude` | 保留 |
| `parents: true` | 与 `git_ignore` 配合读取上层规则 | 保留（默认） |

**关闭 `hidden` 带来一个必须补偿的副作用**：`hidden` 是 `ignore` 唯一的点目录
过滤机制（`walk.rs:476` 第五步），关掉后 `.git/` 也会被遍历。`ignore` 并不内建
跳过 `.git`，只用它作为「此处是 git 仓库」的探测标记（`dir.rs:215`、`:250`）。
本仓库 `.git` 下有 10,345 个文件、134 MB，因此必须显式加 override `!.git`
（见 4.4）。`.` 与 `..` 不受影响：`ignore` 的遍历基于 `walkdir`，不会产出这两个
条目。

已知代价：`hidden(false)` 会让 `.env` 等敏感文件也进入搜索结果。用户已明确
接受「搜到」，理由是搜索工具应当忠实反映目录内容；不因此增加隐式过滤。

被 `.gitignore` 命中但 Git 实际跟踪的文件会漏报。本仓库当前不存在这种文件
（`git ls-files | git check-ignore --stdin` 输出为空），属已知残余风险，
记入第 7 节。

## 4. 详细设计

### 4.1 环境段注入

位置：`yi-agent-runtime/src/bootstrap.rs` 的 `resolve_system_prompt(user, workdir)`。

现有顺序为「默认 prompt → User-provided instructions → Project instructions
(AGENTS.md) → Current date」。新段插在 **AGENTS.md 之后、`Current date` 之前**，
与既有顺序契约测试兼容（`bootstrap.rs:1258`
`resolve_system_prompt_appends_root_agents_md_after_custom_instructions`）。

注入文本（`workdir` 取 `display()`，不做 canonicalize，保持与用户配置一致）：

```
## Environment

Working directory: <workdir>
All file tools (read/edit/write/grep/glob) resolve relative paths against this directory.
The bash tool starts in this directory and keeps its working directory across calls.
Run `pwd` once when you need to confirm a location after `cd`.

Do not probe for the project location. If this directory looks wrong, say so instead of
guessing other paths.
```

要点：

- 篇幅约 6 行。实测表明 system prompt 中稳定部分的增量几乎不影响 TTFB（缓存命中
  4,992/5,038），因此这段的收益来自「消除试探」而非节省 token。
- 「与当前目录是否一致」不写入，因为 system prompt 只在会话开始组装
  （`agent-core/src/agent.rs:1158`），会话中途 `cd` 不应改写它。
- 该段为纯文本新增，不含用户可控内容以外的变量；`workdir` 由运行时选定，不引入
  注入面。

### 4.2 bash 回显 cwd

位置：`yi-agent-tools/src/shell/bash.rs` 的返回值组装（`exit: ...` 之前）。

改为：

```
cwd: <ctx.cwd() 调用后的值>
exit: <code>
stdout:
...
stderr:
...
```

- 使用**执行后**的 `ctx.cwd()`，因此成功 `cd` 的下一轮即可看到新目录。
- 超时分支同样带上 `cwd:` 行，保证两种结果形态一致。
- 现有断言 `s.contains("exit: 0")`、`s.contains("exit: 1")`
  （`bash.rs:758`、`:813`）为子串匹配，加前缀不影响。
- 单次增量约 40~90 字符，进入工具结果而非 system prompt，不破坏缓存前缀。

### 4.3 工具描述补基准说明

位置：`yi-agent-tools/src/shell/bash.rs` 的 schema `command` 描述，以及
`fs/grep.rs`、`fs/glob.rs` 的 `path` 描述。

`bash`：

```
The command to execute. Runs in the session working directory, which persists
across calls (a successful standalone `cd` changes it for later calls).
```

`grep` / `glob` 的 `path`：

```
File or directory, default root (the session working directory).
```

会话中途 `cd` 之后，文件工具的相对路径仍以 `roots[0]` 为基准（非 4.1 中 bash 的
持久 cwd）。上述措辞只声明「默认 root」，不宣称与 bash cwd 联动，避免给出错误
承诺；该一致性缺口记入第 7 节。

### 4.4 统一 walker 模块

新增 `yi-agent-tools/src/fs/walk.rs`，供 `grep` 与 `glob` 共用：

```rust
/// What a walk yields, and whether it crosses symlinks.
///
/// 两个工具**不能**共用一套：旧 `glob()` 返回目录且跟随符号链接，旧 `WalkDir`
/// 两者都不做。合并成一个策略会静默改变其中一个工具的结果集（实测见下）。
pub(crate) struct WalkOptions {
    /// 除常规文件外，是否也产出目录。
    pub include_dirs: bool,
    /// 是否跨符号链接（环由 `ignore` 检测并跳过）。
    pub follow_links: bool,
}

/// `grep` 的策略。
pub(crate) const FILES: WalkOptions = WalkOptions { include_dirs: false, follow_links: false };

/// `glob` 的策略：目录算匹配，且跨链接——这正是重写前 `glob::glob()` 的行为。
pub(crate) const MATCH_TARGETS: WalkOptions =
    WalkOptions { include_dirs: true, follow_links: true };

/// What a walk produced.
pub(crate) struct WalkOutcome {
    pub paths: Vec<PathBuf>,
    pub truncated: Truncated,
}

pub(crate) enum Truncated {
    No,
    Deadline,
}
```

不预留 `max_depth`：现状 `grep`/`glob` 都没有深度限制，`glob` 的深度由模式本身
决定（`**` 递归、固定组件数固定）。加一个没人用的字段只会让人以为有档位可调。

行为：

- 内部使用 `ignore::WalkBuilder::new(base)`，配置为
  `hidden(false)`、`require_git(false)`、`git_global(false)`、
  `git_ignore(true)`、`git_exclude(true)`。
- **必须显式排除 `.git/`**。`hidden(false)` 关掉了 `ignore` 唯一的点目录过滤
  （`walk.rs:476` 的第五步），而 `ignore` **没有**针对 `.git` 的内建跳过：它只用
  `.git` 作为「此处是 git 仓库」的探测标记（`dir.rs:215`、`:250`）。因此
  不加约束时 `.git/` 会被完整遍历（本仓库 10,345 个文件）。做法是用 override
  显式忽略：

  ```rust
  // `OverrideBuilder::new` 直接返回 builder，不是 `Result`（`overrides.rs:122`）。
  let mut overrides = OverrideBuilder::new(base);
  overrides.add("!.git")?;   // override 里 `!` 表示忽略
  builder.overrides(overrides.build()?);
  ```

  override 的优先级高于一切其它规则（`dir.rs:380-389`），因此不受 `.gitignore`
  内容影响。`!.git` 只匹配名为 `.git` 的目录组件，不会牵连 `.github/`。
- 产出什么由 `WalkOptions` 决定（见上）。`grep` 传 `FILES`，`glob` 传 `MATCH_TARGETS`。
- 遍历错误（权限、消失的目录）一律跳过，与现状 `filter_map(|e| e.ok())` 一致。
- `deadline` 用 `Instant` 判定，检查点在每个条目迭代处；超时立即停止并置
  `Truncated::Deadline`。
- 跳过 depth 0（搜索根本身）**仅当它是目录**。`grep` 的 `path` 可以是单个文件，
  旧 `WalkDir::new(file)` 会把该文件作为 depth 0 条目产出；无条件跳过 depth 0
  会让「对单个文件 grep」返回空。
- 不产出搜索根目录本身。

**`glob` 必须用 `MATCH_TARGETS`，这是 spec 早期版本的错误，此处更正。** 早先写的
「不产出目录、不跟随符号链接」是按 `WalkDir` 的行为推的，与 `glob()` 的实际行为
不符。实测（`glob 0.3.4` + `ignore 0.4.23`，同一棵临时目录）：

```
旧 glob `**/*`                    -> ["src", "src/a.rs", "src/x", "top.rs"]   含目录
只收文件的 walker                 -> ["src/a.rs", "top.rs"]                   少两个目录
旧 glob `**/*.rs`（含符号链接）   -> ["link.rs", "linkdir/real.rs", "plain.rs"]
follow_links(false) 的 walker     -> ["plain.rs"]                            整条链接丢失
follow_links(true) 的 walker      -> ["link.rs", "linkdir/real.rs", "plain.rs"]  与旧行为一致
```

`follow_links(true)` 不会打转：`ignore` 自带祖先检测，实测自引用链接报
`File system loop found: ... points to an ancestor ...` 并跳过该条，遍历正常结束。

### 4.5 grep 改造

- `call` 内把整个 `grep_search` 移入 `tokio::task::spawn_blocking`。
- `grep_search` 的 `WalkDir::new(base)` 替换为
  `walk::walk_with_deadline(base, walk::FILES, deadline)`（deadline 从 `call`
  传入，因为取时间点必须在 `spawn_blocking` 闭包内部，见 §4.9）。
- 保留既有输出契约：`MAX_OUTPUT_ENTRIES = 200`、`MAX_OUTPUT_BYTES = 32 KiB`、
  `TRUNCATION_NOTICE` 文案不变。
- 新增 deadline 截断提示，与既有条目截断提示可同时出现，各占一行：

  ```
  [truncated: narrow path, glob, or pattern; use files_with_matches or count]
  [scan stopped after 10s: narrow path or glob, some matches were not reached]
  ```

- deadline 常量 `SCAN_DEADLINE = Duration::from_secs(10)`，置于 `fs/walk.rs`，
  两个工具共用。
- 注意 `is_binary`（`fs/grep.rs:132`）每个候选文件都要 `open` 读 1 KiB 判 NUL。
  在跳过大目录后其绝对调用量已从百万级降到千级，保持现状即可，不做额外优化。
- `is_binary` 的 NUL 首字节探测、`context` 行展开、`output_mode` 三种模式、
  `relative_to_roots` 的相对路径报告方式全部不变。

### 4.6 glob 改造

- `call` 内把遍历移入 `spawn_blocking`。
- 现有实现依赖 `glob::glob(full_pattern)` 的**模式内嵌目录展开**：它把模式按
  分隔符切成逐级组件，一层层 `read_dir` 匹配（`glob-0.3.4/src/lib.rs:250-252` 切分组件、
  `:889-942` 逐层 `read_dir`）。改为：用 walker 遍历 `base` 下所有文件，再用 `glob::Pattern`
  对路径整体匹配。

**语义对齐（本节是本设计最容易写错的地方，结论来自核对 `glob` 源码）：**

`glob::glob()` 逐段展开，所以组件内的 `*` 天然**不跨越** `/`。而
`Pattern::matches()` 的默认 `MatchOptions` 是 `require_literal_separator: false`
（`lib.rs:48`），此时 `*` 会跨越 `/`。二者不等价：

```
现有 glob({"pattern":"*.rs"})           → 只返回根级 .rs
若用 Pattern 默认选项匹配整条相对路径    → 会多返回 src/main.rs（行为变宽）
```

`glob-0.3.4` 自己的测试钉住了这个区分（`lib.rs:1394-1428`）：

```
require_literal_separator: true  → "abc*def" 不匹配 "abc/def"
require_literal_separator: false → "abc*def" 匹配   "abc/def"
```

因此新实现**必须**使用：

```rust
const MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,   // 等价于 glob() 的逐段展开
    require_literal_leading_dot: false, // 保留现行为：* 可匹配 .gitignore 这类点文件
};
```

用 `glob::Pattern::matches_with(rel, MATCH)` 匹配「相对 `base` 的路径」。这样
**不需要新增 `globset` 直接依赖**：`glob` 已是既有依赖，且直接用同一引擎，语义
差异风险最小。

上述常量已用一次性实验程序在 `glob 0.3.4` 上实测确认（11 条断言全通过）：

```
require_literal_separator: true
**/Cargo.toml  vs Cargo.toml              => true    （** 零层）
**/Cargo.toml  vs a/b/c/Cargo.toml        => true
*.rs           vs a.rs                    => true
*.rs           vs a/b.rs                  => false   ← 与 glob() 逐段展开一致
src/**/*.rs    vs src/a.rs                => true
src/**/*.rs    vs src/x/y/a.rs            => true
*              vs .gitignore              => true    （点文件仍可命中）
**/Cargo.toml  vs yi-agent-rs/Cargo.toml  => true
```

对照：同一组用例在 **默认** `MatchOptions` 下 `*.rs vs a/b.rs => true`，即会把
根级 glob 的匹配范围扩大成整棵树。这正是必须显式设 `true` 的原因。

`require_literal_leading_dot: false` 是保持现行为所必需的。核对
`glob-0.3.4/src/lib.rs:963-966`：该选项为 `false` 时，`glob()` **不**过滤点
开头的目录项，即**当前 `glob` 确实会遍历点目录**（`.worktrees/`、`.github/`
都在内）。`**/Cargo.toml` 花 14.8 s 与「下探 `.worktrees/` 的 769,914 个文件」
一致。

- `**` 的零层匹配已由上游测试钉住（`lib.rs:1252-1256`：`**/test` 匹配 `test`、
  `one/test`、`one/two/test`），与 `glob()` 一致，无需额外处理。
- **绝对路径模式**：现实现 `full_pattern = base.join(pattern)`，`Pattern::join`
  遇绝对路径会整段替换，因此绝对模式可以从任意位置开始展开。新实现需要先算
  「模式中第一个含通配符的组件之前的静态前缀」，以该前缀为 walk base，再用
  `matches_with(abs_path, MATCH)` 匹配。这样绝对模式仍能命中 base 之外的位置。
  静态前缀逻辑与既有 `is_unbounded_root_recursive_scan` 的模式分析属同一类，
  复用在 `fs/walk.rs`。
- 相对模式：walk `base`（默认 `roots[0]`，或 `args.path`），用
  `matches_with(rel_to_base, MATCH)` 匹配。
- 保留 `is_unbounded_root_recursive_scan` 的警告与 `UNBOUNDED_ROOT_GLOB_WARNING`
  文案。
- 保留 `relative_to_roots` 的报告方式。

### 4.7 侦察优先的提示

位置：`yi-agent-core/src/system_prompt.rs` 的 `Tool use strategy` 段，追加一条：

```
- Reconnoitre before editing: when a task spans code you have not read yet, spend
  your first response gathering the whole picture in parallel — one glob for the
  file layout, one grep for the entry point, read the one or two key files. Then
  act. Do not explore one file per turn.
```

与既有「并行 / 用 `&&` 合并依赖步骤」的规则并列，不重复其表述。

### 4.8 依赖变更

`yi-agent-tools/Cargo.toml`：

```toml
ignore = "0.4"
tokio = { version = "1", features = ["process", "time", "fs", "io-util", "rt"] }
```

- **不新增 `globset` 直接依赖**：glob 的匹配改用既有的 `glob::Pattern` +
  `matches_with`，语义与 `glob()` 逐段展开等价（见 4.6）。
- `rt` 是 `spawn_blocking` 所需。workspace 已声明 `tokio = { features = ["full"] }`，
  因此不引入新版本。

**版本与 lock 影响（已核对 Cargo.toml 与本地 cache）：**

`ignore` 会引入 `globset`、`bstr`、`crossbeam-deque`、`memchr`、`log`、
`regex-automata`、`same-file`、`walkdir` 等传递依赖。当前 `Cargo.lock` 中
**没有** `ignore`、`globset`、`bstr`，因此 lock 一定会新增这三个包。

选版本时的关键约束：

| 包 | 约束 | 结论 |
|---|---|---|
| `ignore` | 0.4.23 要求 `globset >= 0.4.15`；0.4.25 要求 `>= 0.4.18` | 用 `ignore = "0.4"` 即可 |
| `globset` | 0.4.18 要求 `regex-automata >= 0.4.0`；0.4.20 要求 `>= 0.4.18` | 解析到 0.4.18 可复用 lock 里已有的 `regex-automata 0.4.16` |
| `bstr` | 非 optional，必然引入 | lock 新增一项 |

本地 cache 同时存在 `ignore-0.4.23/0.4.25`、`globset-0.4.16/0.4.18/0.4.20`、
`regex-automata-0.4.9/0.4.13/0.4.14/0.4.16/0.4.18`、`bstr-1.11.3/1.12.1/1.13.1`，
**可以离线解析**。实施时若解析出 `globset 0.4.20`，会连带把 `regex-automata`
升到 0.4.18；那也是 cache 内已有版本，可接受。

**MSRV 核对**：仓库 `rust-version = "1.89"`。`ignore 0.4.23` 未声明
`rust-version`，`globset 0.4.18` 未声明，`bstr 1.13.1` 声明 1.65，
`crossbeam-deque 0.8.8` 声明 1.61，`regex-automata 0.4.14` 声明 1.65 ——
均低于 1.89，`msrv-check` 不会因此失败。实施后须实际跑一次 `just msrv-check`
确认，因为 `rust-version` 只声明不校验。
- `glob` 依赖是否保留：`glob::Pattern` 仍在 `grep` 的 `glob` 过滤参数中使用，
  因此**保留** `glob = "0.3"`。
- `walkdir` 依赖：`fs/grep.rs` 改造后不再直接使用；`side_read/file_tools.rs`
  的注释提及但未引用该 crate。若确认无其他使用点，从 `yi-agent-tools` 移除
  `walkdir`；`ignore` 自身依赖 `walkdir`，不会从依赖图中消失。

### 4.9 并发与取消

- 两个工具的同步遍历统一走 `spawn_blocking`。`Tool::call` 当前签名不接收取消
  令牌（`yi-agent-core/src/tool.rs:104`）。
- **deadline 必须在阻塞闭包内部实现**（walker 用 `Instant` 自检），不能靠
  `tokio::time::timeout` 包裹 `spawn_blocking`：后者只放弃等待，不会停止已在运行的
  任务，线程会继续扫完。前者保证线程在 10 s 内自行退出。
- 因此超时路径的契约是：`spawn_blocking` 返回 `WalkOutcome`，其中
  `truncated: Truncated::Deadline`；工具照常构造「部分结果 + 截断提示」，不返回错误。
- 不在本次引入协作式取消：那需要扩展 `Tool` trait，超出本设计范围，记入第 7 节。

## 5. 测试

### 5.1 环境段

- `resolve_system_prompt` 生成的 prompt 含 `## Environment` 与
  `Working directory: <workdir>`。
- 段落顺序：`User-provided instructions` < `Project instructions` <
  `## Environment` < `Current date`。
- 无 `AGENTS.md` 时环境段照常出现。
- 自定义 `system_prompt` 时，默认 prompt 与环境段仍在（与既有
  `resolve_system_prompt_custom_keeps_base_instructions` 一致）。

### 5.2 bash cwd 回显

- 普通命令输出以 `cwd: <期望路径>\nexit: 0` 开头。
- `cd <子目录>` 成功后，下一次调用的 `cwd:` 为新目录。
- 非零退出码结果同样带 `cwd:` 行。
- 超时结果同样带 `cwd:` 行。

### 5.3 walker

- `.gitignore` 中的目录（`target/`、`node_modules/`）被跳过。
- 点目录（`.github/`）**不**被跳过。
- **`.git/` 必须被跳过**：在 `.git/` 内放置一个可被 pattern 命中的文件，断言它
  不出现在 walk 结果里。这是 `hidden(false)` 的必配约束，缺了它搜索会重新变慢
  （本仓库 `.git` 下 10,345 个文件）。
- 全局 gitignore 不生效（用一个临时 HOME 或显式断言行为）。
- 非 git 仓库目录下 `git_ignore` 规则仍生效（`require_git(false)`）。
- deadline：传入**已过期**的 `Instant`（`Instant::now().checked_sub(1s)`），断言返回
  `Truncated::Deadline` 且 `paths` 为空。写死「已过期」而不是「1ms」是因为后者在
  磁盘缓存命中时可能整棵树都读完了，测试会间歇性失败。
- 以单个文件为 base 时，该文件本身必须出现在结果里（`grep` 的 `path` 可为文件）。
- `glob` 专用：以 `MATCH_TARGETS` 走一遍，`**/*` 必须同时返回目录与文件；
  符号链接目标也必须被访问到。

### 5.4 grep

- 既有 8 个测试全部保持通过（content / files / count / glob 过滤 / 无匹配 /
  多根相对路径）。
- `target/` 下放置命中文件，断言结果中不出现。
- `.github/` 下放置命中文件，断言结果中出现。
- 已过期的 deadline 下断言出现 `[scan stopped after 10s:` 提示，且返回的
  `timed_out` 为 `true`。

### 5.5 glob

- 既有 4 个测试保持通过。
- `**/*.rs` 仍能匹配多级子目录。
- **`*.rs` 不得匹配 `src/main.rs`**（验证 `require_literal_separator: true`
  生效，与改造前的逐段展开语义一致）。
- `*` 仍可匹配点开头的文件（如 `.gitignore` 被 `*` 命中），验证
  `require_literal_leading_dot: false` 保持了现行为。
- `**/Cargo.toml` 能匹配根级 `Cargo.toml` 与多级嵌套（`**` 零层匹配）。
- `target/` 下放置匹配文件，断言结果中不出现。
- `.github/` 下放置匹配文件，断言结果中**出现**（`hidden(false)` 生效）。
- 绝对路径模式（如 `<abs>/src/*.rs`）能正常命中。
- `UNBOUNDED_ROOT_GLOB_WARNING` 仍在根目录无界 glob 时出现。

### 5.6 验证命令

```
cd yi-agent-rs && cargo test -p yi-agent-tools
cd yi-agent-rs && cargo test -p yi-agent-core
cd yi-agent-rs && cargo test -p yi-agent-runtime
cd yi-agent-rs && cargo fmt --all && just ci
```

`just ci` 里必须特别关注 `msrv-check` 与 `audit`：前者验证 `ignore` 依赖树在
`rust-version = "1.89"` 下真能编译，后者对新增依赖做漏洞检查（豁免清单在
`yi-agent-rs/.cargo/audit.toml`）。若 `audit` 报出新依赖的公告，按既有流程评估
是否加豁免，不得直接忽略。

## 6. 文档更新

- `docs/project-management/yi-agent-tools.md`：登记 walker 模块、deadline、
  `ignore` 依赖、grep/glob 行为变更，附 `file.rs:line` 判据。
- `docs/project-management/yi-agent-core.md`：登记环境段与侦察优先提示。
- `docs/project-management/README.md`：同步模块索引计数（若计数变化）。
- `docs/research/2026-11-06-interaction-latency-profile.md`：把「建议 2」的
  同步遍历缺陷标注为已修复，并补修复后的实测对比。

## 7. 已知限制与风险

1. **被 gitignore 命中但被 Git 跟踪的文件会漏报**。当前仓库无此情况，但这是
   `git_ignore(true)` 的固有语义。若日后出现，用 Overrides 白名单补救；本次不做。
2. **`hidden(false)` 会让 `.env` 等敏感文件进入搜索结果**。已获用户明确同意。
3. **`hidden(false)` 必须配 `.git` 显式排除**（见 4.4）。若实现时漏掉这条，
   `.git/` 下的上万文件会重新进入遍历，收益大幅缩水且不易察觉——因此 5.3 有专门
   的回归测试钉住它。
4. **无协作式取消**。10 s deadline 是唯一上界；一次超长扫描最坏占用
   `spawn_blocking` 线程 10 s（deadline 在阻塞闭包内实现，见 4.9）。
5. **bash 持久 cwd 与文件工具 `roots[0]` 不一致**：`cd` 之后 `grep`/`glob`/
   `read` 的相对路径仍以 `roots[0]` 为基准。本设计只如实描述，不做统一；这是一处
   既有的语义缺口，统一它属于独立的行为变更。
6. **环境段不反映会话中途的 `cd`**。system prompt 只在会话开始组装一次；中途
   位置变化靠 4.2 的 `cwd:` 回显传递。
7. **`glob` 的绝对路径退化路径**：`pattern` 为绝对路径时，新实现需要从静态前缀
   开始 walk，而不是像 `glob()` 那样从根展开。若静态前缀计算有误，绝对模式可能
   漏匹配；必须在测试中覆盖绝对路径形式（见 5.5）。
8. **`glob` 的匹配语义由本设计显式钉住**：`require_literal_separator: true` 与
   `require_literal_leading_dot: false`。前者与 `glob()` 的逐段展开等价（已实测
   11 条断言，见 4.6），后者保持「`*` 可命中点文件」。若实现者改动这两个开关，
   会静默改变 glob 结果集。
9. **`grep` 的 `glob` 过滤参数不受 walker 影响**：它仍用
   `glob::Pattern::new(g).matches(file_name)` 只匹配**文件名**（`fs/grep.rs:174-179`），
   不匹配路径。本次不改这一语义。
10. **新增依赖会改 `Cargo.lock`**：`ignore`、`globset`、`bstr` 三项新增，可能连带
    升级 `regex-automata` 到 0.4.18（见 4.8）。这是引入 gitignore 语义的必要代价。

## 8. 验收判据

机制性判据（可直接验证，不依赖模型行为）：

- `cargo test -p yi-agent-tools`、`-p yi-agent-core`、`-p yi-agent-runtime` 全绿。
- `just ci` 全绿（16 道门禁）。
- 在 `yi-agent-rs` 下执行 `grep path=yi-agent-rs pattern=Cargo`，返回时间
  `< 1 s`（对照改造前 126.4 s）。
- 在上游仓库根执行 `glob **/Cargo.toml`，返回时间 `< 1 s`（对照 14.8 s）。
- `grep` / `glob` 在 `target/` 下设置了命中文件时，结果中不出现该文件。
- `.github/` 下设置了命中文件时，结果中出现该文件。
- 新会话的 system prompt 含 `Working directory: <abs workdir>`。
- 每次 `bash` 输出以 `cwd: <path>` 开头；`cd` 后下一次调用显示新路径。

行为性判据（需真实会话观测，作为后续复核而非本 design 的完成条件）：

- 用同一套 trace 统计脚本复核「`cd` 到当前目录的 no-op 比例」是否下降。改造前
  基线为 55.8%（665/1,191）。该指标受模型行为影响，不作为门禁。
