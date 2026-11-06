# 互动延时：环境提示、cwd 反馈与搜索工具重写 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 agent 知道自己在哪里（system prompt 环境段 + bash 回显 cwd），并让 `grep`/`glob` 跳过构建产物、在 10 秒内返回且明确报告截断。

**Architecture:** 四处独立改动：环境段插进 `resolve_system_prompt` 的既有拼接顺序；`bash` 结果加一行 `cwd:`；新增 `fs/walk.rs` 作为 `grep`/`glob` 共用的 gitignore-aware walker（用 `ignore` crate，关闭 hidden 但显式排除 `.git`），两个工具的同步遍历移入 `spawn_blocking`；system prompt 追加一条侦察优先策略。

**Tech Stack:** Rust 2024 edition / tokio（`spawn_blocking`）/ `ignore 0.4`（`WalkBuilder`）/ `glob 0.3`（`Pattern::matches_with`）。

## Global Constraints

- 设计真源：[spec](../specs/2026-11-06-interaction-latency-and-search-design.md)。本计划的每一处行为都以该 spec 为准；spec 与本计划冲突时以 spec 为准。
- 工作目录必须是 worktree，不得在 `main` 上改代码或提交。分支沿用 `docs/latency-search-spec` 或新开 `feat/latency-search`。
- 每次 commit 前在 `yi-agent-rs/` 下执行 `cargo fmt --all`。
- 全部任务完成后在 `yi-agent-rs/` 下跑 `just ci`（20 道门禁）并全绿。特别关注 `msrv-check`（`rust-version = "1.89"`）与 `audit`。
- `ignore` 配置**必须**是：`hidden(false)`、`require_git(false)`、`git_global(false)`、`git_ignore(true)`、`git_exclude(true)`，外加 override `!.git`。五项加一条缺一不可，理由见 spec §3.3 与 §4.4。
- **walker 必须支持两套产出策略，不能合成一套**：`FILES`（只收文件、不跨链接）给 `grep`；`MATCH_TARGETS`（含目录、跨链接）给 `glob`。旧 `glob::glob()` 会返回目录并跟随符号链接，旧 `WalkDir` 不会；合并成一个策略会静默改变其中一个工具的结果集（实测证据见 Task 1 的 Interfaces 段）。
- `glob::Pattern` **没有** `is_absolute()`。绝对模式判断一律用 `Path::new(pattern).is_absolute()`。
- 以单个文件为 `base` 时，walker 必须把该文件本身作为结果产出（`grep` 的 `path` 可以是文件）。跳过 depth 0 只能针对「根是目录」。
- glob 匹配**必须**用 `glob::MatchOptions { case_sensitive: true, require_literal_separator: true, require_literal_leading_dot: false }`。`require_literal_separator` 若为 `false`，`*` 会跨越 `/`，静默扩大结果集。
- deadline 必须是 **10 秒**，且必须在阻塞闭包内部用 `Instant` 实现。不得用 `tokio::time::timeout` 包裹 `spawn_blocking` 冒充超时：那只放弃等待，不会停止任务。
- 截断必须**明确告知**用户。deadline 截断与既有条目截断是两条独立提示，可同时出现。
- 不改变文件工具的相对路径基准（仍是 `roots[0]`），不统一 bash 持久 cwd 与文件工具基准。
- 现有测试必须全部保持通过，除明确说明需要改写的断言外，不得删改。

---

## File Structure

| 文件 | 责任 |
|---|---|
| `yi-agent-rs/crates/yi-agent-tools/src/fs/walk.rs` | 新建。唯一的遍历策略所有者：ignore 配置、`.git` 排除、deadline、截断标记。grep 和 glob 都只能经由它遍历。 |
| `yi-agent-rs/crates/yi-agent-tools/src/fs/mod.rs` | 声明并导出 `walk`（crate 内可见即可）。 |
| `yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs` | 改为用 `walk::walk_with_deadline` + `spawn_blocking`；新增 deadline 截断提示；schema 描述补 root 说明。 |
| `yi-agent-rs/crates/yi-agent-tools/src/fs/glob.rs` | 改为用 `walk::walk_with_deadline` + `Pattern::matches_with` + `spawn_blocking`；保留 warning 与相对路径报告。 |
| `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs` | 结果前缀加 `cwd: <path>`（正常与非零退出共用一个分支；超时分支因 `ToolResult::error` 自带 `error: ` 前缀，`cwd:` 出现在 `error: ` 之后）；schema 描述补 cwd 语义。 |
| `yi-agent-rs/crates/yi-agent-tools/Cargo.toml` | 加 `ignore`、tokio `rt`；移除不再直接使用的 `walkdir`。 |
| `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` | `resolve_system_prompt` 插入 `## Environment` 段。 |
| `yi-agent-rs/crates/yi-agent-core/src/system_prompt.rs` | 追加「侦察优先」一条。 |

---

### Task 1: walker 模块（ignore 配置 + `.git` 排除 + deadline）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/fs/walk.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`

**Interfaces:**
- Consumes: 无（本任务不依赖其它任务）
- Produces:
  - `walk::SCAN_DEADLINE: Duration`（10 秒）
  - `walk::Truncated { No, Deadline }`（`#[derive(Default)]`，`No` 为 `#[default]`）
  - `walk::WalkOptions { include_dirs: bool, follow_links: bool }`
  - `walk::FILES: WalkOptions`（`{ false, false }`，grep 用）
  - `walk::MATCH_TARGETS: WalkOptions`（`{ true, true }`，glob 用）
  - `walk::WalkOutcome { paths: Vec<PathBuf>, truncated: Truncated }`
  - `walk::walk_with_deadline(base: &Path, options: WalkOptions, deadline: Option<Instant>) -> WalkOutcome`
  - `walk::deadline_from_now() -> Instant`（`Instant::now() + SCAN_DEADLINE`）

**为什么需要两个开关（本节结论全部来自实测，不是推测）：**

旧 `glob::glob()` 与 `WalkDir::new()` 在两点上行为不同，两条都必须逐项对齐：

| 行为 | 旧 `glob()` | 旧 `WalkDir`（grep） | 新 walker |
|---|---|---|---|
| 是否返回目录 | **是**。`**/*` 返回 `src`、`src/a.rs` | 否（`is_file()` 过滤） | `MATCH_TARGETS.include_dirs = true` |
| 是否跟随符号链接 | **是**。`**/*.rs` 能命中 `link.rs` | 否 | `MATCH_TARGETS.follow_links = true` |

实测（`/tmp` 一次性实验，`glob 0.3.4` + `ignore 0.4.23`）：

```
旧 glob `**/*`  -> ["src", "src/a.rs", "src/x", "top.rs"]     ← 含目录
只收文件的新实现 -> ["src/a.rs", "top.rs"]                     ← 漏掉两个目录
旧 glob `**/*.rs`（有符号链接）-> ["link.rs", "linkdir/real.rs", "plain.rs"]
follow_links(false) 的新实现 -> ["plain.rs"]                   ← 整条链接丢失
```

因此 `glob` 必须用 `MATCH_TARGETS`，`grep` 必须用 `FILES`，不能用同一套。
`follow_links(true)` 不会打转：`ignore` 自带祖先检测，实测自引用链接报
`File system loop found: ... points to an ancestor ...` 并跳过该条，遍历正常结束。

- [ ] **Step 1: 加依赖**

`yi-agent-rs/crates/yi-agent-tools/Cargo.toml` 中，把 tokio 那一行改为（只加 `rt`）：

```toml
tokio = { version = "1", features = ["process", "time", "fs", "io-util", "rt"] }
```

在 `# FS 工具` 段落下加 `ignore`（`walkdir` 暂时保留，Task 2 Step 8 再删）：

```toml
# FS 工具
glob = "0.3"
walkdir = "2"
regex = "1"
ignore = "0.4"
```

- [ ] **Step 2: 声明模块（必须先做，否则下一步的「红」不成立）**

`fs/mod.rs` 加一行模块声明（`pub mod read_document;` 之后、`pub mod view_image;` 之前，保持字母序）：

```rust
pub mod path_util;
pub mod read;
pub mod read_document;
pub(crate) mod walk;
pub mod view_image;
```

**这一步不能挪到 Step 5（实现）。** 没有 `mod walk;` 声明时 `walk.rs` 根本不参与编译，
`cargo test --lib fs::walk` 只会输出 `0 tests, filter didn't match`，看起来像
「红」但其实是「没编译」，会掩盖真正的编译错误。

- [ ] **Step 3: 写失败的测试**

在 `walk.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 测试专用：标准 deadline 的一步封装。生产代码不调用它（见上文注释）。
    fn walk(base: &Path, options: WalkOptions) -> WalkOutcome {
        walk_with_deadline(base, options, Some(deadline_from_now()))
    }

    fn rel(outcome: &WalkOutcome) -> Vec<String> {
        let mut v: Vec<String> = outcome
            .paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn skips_directories_listed_in_gitignore() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/artifact.rs"), "x").unwrap();
        fs::create_dir(tmp.path().join("node_modules")).unwrap();
        fs::write(tmp.path().join("node_modules/dep.js"), "x").unwrap();
        fs::write(tmp.path().join("keep.rs"), "x").unwrap();

        let files = rel(&walk(tmp.path(), FILES));

        // `.gitignore` 本身也占一个名额：它没有被任何规则忽略（实测如此）。
        // 只断言「产物不在、源码在」，不要断言精确数量。
        assert!(
            files.iter().any(|p| p.ends_with("keep.rs")),
            "the real source file must survive, got {files:?}"
        );
        assert!(
            !files.iter().any(|p| p.contains("/target/")),
            "gitignored target/ must be skipped, got {files:?}"
        );
        assert!(
            !files.iter().any(|p| p.contains("/node_modules/")),
            "gitignored node_modules/ must be skipped, got {files:?}"
        );
    }

    #[test]
    fn keeps_dot_directories_such_as_github() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        fs::write(tmp.path().join(".github/workflows/ci.yml"), "on: push\n").unwrap();

        let files = rel(&walk(tmp.path(), FILES));

        assert!(
            files.iter().any(|p| p.ends_with(".github/workflows/ci.yml")),
            ".github must stay visible, got {files:?}"
        );
    }

    #[test]
    fn always_skips_the_git_directory() {
        // hidden(false) 关掉了 ignore 唯一的点目录过滤，而 ignore 只把 .git
        // 当作「此处是 git 仓库」的探测标记，并不会跳过它。没有 override
        // 时 .git/ 会被完整遍历（本仓库 10345 个文件）。
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();
        fs::write(tmp.path().join(".git/objects/blob"), "x").unwrap();
        fs::write(tmp.path().join(".git/HEAD"), "ref: x").unwrap();
        fs::write(tmp.path().join("keep.rs"), "x").unwrap();

        let files = rel(&walk(tmp.path(), FILES));

        assert!(
            !files.iter().any(|p| p.contains("/.git/")),
            ".git must never be walked, got {files:?}"
        );
        assert_eq!(files.len(), 1, "got {files:?}");
    }

    #[test]
    fn does_not_read_the_global_gitignore() {
        // git_global(false)：用户 ~/.config/git/ignore 里的规则不得改写结果。
        // 本机那里有 `**/.claude/settings.local.json`，这里用一个同名路径
        // 断言它仍然出现。
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(tmp.path().join(".claude/settings.local.json"), "{}").unwrap();

        let files = rel(&walk(tmp.path(), FILES));

        assert!(
            files
                .iter()
                .any(|p| p.ends_with(".claude/settings.local.json")),
            "global gitignore must not apply, got {files:?}"
        );
    }

    #[test]
    fn reports_deadline_truncation_instead_of_running_forever() {
        let tmp = TempDir::new().unwrap();
        for i in 0..500 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "x").unwrap();
        }

        // 已经过期的 deadline：第一轮检查就应停止。
        // `checked_sub` 而非 `-`：后者在时钟读数极端靠前时会 panic。
        let expired = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("clock is at least 1s past boot");
        let outcome = walk_with_deadline(tmp.path(), FILES, Some(expired));

        assert!(
            matches!(outcome.truncated, Truncated::Deadline),
            "an expired deadline must report Deadline truncation"
        );
        assert!(
            outcome.paths.is_empty(),
            "an already-expired deadline must not collect files"
        );
    }

    #[test]
    fn a_fresh_deadline_is_not_truncated() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "x").unwrap();

        let outcome = walk(tmp.path(), FILES);

        assert!(matches!(outcome.truncated, Truncated::No));
        assert_eq!(outcome.paths.len(), 1);
    }

    /// `grep` 的 `path` 可以是单个文件（schema 写的是 "File or directory"）。
    /// 旧的 `WalkDir::new(file)` 会把这个文件作为 depth 0 的条目产出，所以新
    /// walker 也必须保留它——跳过 depth 0 只能针对「根是目录」的情形。
    #[test]
    fn a_file_root_is_yielded_as_the_only_hit() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("only.rs");
        fs::write(&file, "needle\n").unwrap();

        let files = rel(&walk(&file, FILES));

        assert_eq!(files.len(), 1, "got {files:?}");
        assert!(files[0].ends_with("only.rs"));
    }

    #[test]
    fn files_mode_yields_files_only_not_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/a.txt"), "x").unwrap();

        let outcome = walk(tmp.path(), FILES);

        assert!(outcome.paths.iter().all(|p| p.is_file()));
        assert_eq!(outcome.paths.len(), 1);
    }

    /// `glob` 依赖这一点：目录本身也必须进结果集，否则 `**/*` 会漏掉目录。
    #[test]
    fn match_targets_mode_also_yields_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/a.txt"), "x").unwrap();

        let outcome = walk(tmp.path(), MATCH_TARGETS);

        assert!(
            outcome.paths.iter().any(|p| p.ends_with("sub") && p.is_dir()),
            "directories must be included in match-targets mode, got {:?}",
            outcome.paths
        );
        assert_eq!(outcome.paths.len(), 2);
    }

    /// `glob` 也要能穿过符号链接，否则链接进来的源码整条消失。
    #[test]
    fn match_targets_mode_follows_symlinked_files() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("real.rs"), "").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("real.rs"),
            tmp.path().join("link.rs"),
        )
        .unwrap();

        let files = rel(&walk(tmp.path(), MATCH_TARGETS));

        assert!(
            files.iter().any(|p| p.ends_with("link.rs")),
            "symlinked files must be visited, got {files:?}"
        );
    }

    /// 自引用链接不得让遍历打转或报错退出。
    #[test]
    fn follow_links_survives_a_symlink_loop() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("a")).unwrap();
        fs::write(tmp.path().join("a/f.txt"), "x").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("a/back")).unwrap();

        let files = rel(&walk(tmp.path(), MATCH_TARGETS));

        assert!(
            files.iter().any(|p| p.ends_with("a/f.txt")),
            "the real file must still be found, got {files:?}"
        );
    }
}
```

- [ ] **Step 4: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::walk`

Expected: 编译失败，`cannot find function walk in module walk` / `cannot find value FILES`.
如果输出是 `0 tests, filter didn't match`，说明 Step 2 的 `mod walk;` 漏了。

- [ ] **Step 5: 实现 walker**

`yi-agent-rs/crates/yi-agent-tools/src/fs/walk.rs` 完整内容：

```rust
//! The one traversal policy shared by `grep` and `glob`.
//!
//! Both tools used to walk with `WalkDir` / `glob::glob` and no exclusions,
//! which meant a project-root query synchronously read every build artifact:
//! in this repository `yi-agent-rs/target` alone is 162 GB and the tree holds
//! 1.7M files, so a single root `grep` took 178 s.
//!
//! Three decisions live here and nowhere else:
//!
//! * `.gitignore` decides what is generated. `ignore` gives us that semantics
//!   for free, so the skip list is the project's own, not a hardcoded guess.
//! * Hidden paths are **not** skipped: `.github/` must stay searchable. That
//!   costs us the dot-directory filter, so `.git/` is excluded explicitly with
//!   an override — `ignore` only uses `.git` to detect a repository, it never
//!   skips it.
//! * The walk is bounded by a deadline checked on every entry. A caller that
//!   hits it gets `Truncated::Deadline` and must tell the user, rather than
//!   returning a partial result as if it were complete.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;

/// How long either search tool may spend walking before it gives up.
pub(crate) const SCAN_DEADLINE: Duration = Duration::from_secs(10);

/// Why a walk stopped early.
// The `#[default]` on a variant is what lets `WalkOutcome` derive `Default`;
// a hand-written `impl Default` would trip clippy's `derivable_impls` under
// `-D warnings`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Truncated {
    /// The walk finished on its own.
    #[default]
    No,
    /// The deadline fired; `paths` holds only what was reached in time.
    Deadline,
}

/// Which entries a walk yields, and whether it crosses symlinks.
///
/// `grep` and `glob` must not share one answer here: the old `glob()` returned
/// directories and followed links, the old `WalkDir` did neither. Collapsing
/// them into one policy silently changes one tool's result set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WalkOptions {
    /// Yield directories in addition to regular files.
    pub include_dirs: bool,
    /// Cross symbolic links (cycles are detected and skipped by `ignore`).
    pub follow_links: bool,
}

/// `grep`'s policy: regular files only, links not crossed.
pub(crate) const FILES: WalkOptions = WalkOptions {
    include_dirs: false,
    follow_links: false,
};

/// `glob`'s policy: directories count as matches and links are crossed, which
/// is what `glob::glob()` did before this rewrite.
pub(crate) const MATCH_TARGETS: WalkOptions = WalkOptions {
    include_dirs: true,
    follow_links: true,
};

/// What one walk produced.
#[derive(Debug, Default)]
pub(crate) struct WalkOutcome {
    pub paths: Vec<PathBuf>,
    pub truncated: Truncated,
}

/// The deadline a caller should pass to [`walk_with_deadline`].
pub(crate) fn deadline_from_now() -> Instant {
    Instant::now() + SCAN_DEADLINE
}

// 注意：这里**故意不提供** `walk(base, options)` 这种「自动取当前时间」的
// 便利封装。grep / glob 的 deadline 必须在 `spawn_blocking` 的闭包内部取，
// 否则排队等待时间会吃掉预算；而如果两边都只在闭包里调
// `walk_with_deadline(.., Some(deadline_from_now()))`，那个便利封装就只剩
// 测试在用，非测试构建里会被 `dead_code` + `-D warnings` 判死。
// 测试用局部辅助函数（见 `mod tests` 里的 `walk`）。

/// Walk `base`, yielding the entries `options` selects.
///
/// `deadline` is an absolute instant. `None` means unbounded and exists so
/// tests can exercise the "not truncated" path deterministically.
pub(crate) fn walk_with_deadline(
    base: &Path,
    options: WalkOptions,
    deadline: Option<Instant>,
) -> WalkOutcome {
    // `OverrideBuilder::new` returns the builder directly, not a `Result`.
    let mut overrides = OverrideBuilder::new(base);
    // `!` inverts the meaning: this says "ignore `.git`".
    if overrides.add("!.git").is_err() {
        return WalkOutcome::default();
    }
    let overrides = match overrides.build() {
        Ok(overrides) => overrides,
        // A base we cannot build overrides for is a caller error; walking
        // nothing is the safe answer, and both tools then report "no matches".
        Err(_) => return WalkOutcome::default(),
    };

    let mut builder = WalkBuilder::new(base);
    builder
        // Keep `.github/` and other dot directories searchable.
        .hidden(false)
        // Apply gitignore rules even outside a git repository, so a plain
        // directory still skips its build output.
        .require_git(false)
        // Never let the user's global gitignore rewrite our results.
        .git_global(false)
        // The actual win: target/, node_modules/, .worktrees/.
        .git_ignore(true)
        .git_exclude(true)
        .overrides(overrides)
        .follow_links(options.follow_links);

    let mut paths = Vec::new();
    for entry in builder.build() {
        if let Some(limit) = deadline
            && Instant::now() > limit
        {
            return WalkOutcome {
                paths,
                truncated: Truncated::Deadline,
            };
        }
        let Ok(entry) = entry else {
            // Unreadable directories and vanished paths are skipped, exactly
            // as the previous `filter_map(|e| e.ok())` did.
            continue;
        };
        // `depth() == 0` is `base` itself. Skip it **only when it is a
        // directory**: the old `glob()` never reported the search root, but it
        // did report `base` when the caller passed a single file as `base`
        // (grep's `path` may be a file). Verified: walking a file root yields
        // exactly that file at depth 0.
        let file_type = entry.file_type();
        if entry.depth() == 0 && file_type.is_some_and(|t| t.is_dir()) {
            continue;
        }
        let keep = if options.include_dirs {
            file_type.is_some_and(|t| t.is_file() || t.is_dir())
        } else {
            file_type.is_some_and(|t| t.is_file())
        };
        if keep {
            paths.push(entry.into_path());
        }
    }

    WalkOutcome {
        paths,
        truncated: Truncated::No,
    }
}
```

`fs/mod.rs` 的模块声明已在 Step 2 加好，此处不再重复。

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::walk`

Expected: PASS，12 个测试全绿（7 个策略/行为测试 + 2 个 deadline 测试 +
2 个 `MATCH_TARGETS` 测试 + 1 个 loop 测试）。

- [ ] **Step 7: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/fs/walk.rs crates/yi-agent-tools/src/fs/mod.rs crates/yi-agent-tools/Cargo.toml Cargo.lock
git commit -m "feat(tools): add a gitignore-aware walker with a 10s deadline"
```

---

### Task 2: grep 改用 walker，并报告 deadline 截断

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: `walk::walk_with_deadline`, `walk::FILES`, `walk::deadline_from_now`, `walk::Truncated`
- Produces: 无新增公开接口；`grep_search` 签名新增两个参数：`deadline: Option<Instant>`，返回 `Result<(String, bool), ToolsError>`（第二个值 = 是否因 deadline 截断）

- [ ] **Step 1: 写失败的测试**

在 `grep.rs` 的 `mod tests` 里追加：

```rust
    #[tokio::test]
    async fn grep_skips_gitignored_build_output() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/generated.rs"), "needle\n").unwrap();
        fs::write(tmp.path().join("src.rs"), "needle\n").unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}))
            .await;

        let text = text_result(&result);
        assert!(
            text.contains("src.rs"),
            "the real source file must still match, got: {text}"
        );
        assert!(
            !text.contains("target/generated.rs"),
            "gitignored build output must be skipped, got: {text}"
        );
    }

    #[tokio::test]
    async fn grep_finds_matches_inside_dot_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        fs::write(
            tmp.path().join(".github/workflows/ci.yml"),
            "run: cargo test\n",
        )
        .unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"pattern": "cargo test", "output_mode": "files_with_matches"}))
            .await;

        let text = text_result(&result);
        assert!(
            text.contains(".github/workflows/ci.yml"),
            "hidden(false) keeps dot directories searchable, got: {text}"
        );
    }

    #[tokio::test]
    async fn grep_reports_deadline_truncation() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let tool = make_tool(&tmp);

        // 正常 deadline 下不得 panic、不得误报截断。
        // 真正的「过期 deadline」路径由下面的同步测试精确覆盖。
        let result = tool
            .call(serde_json::json!({"pattern": "needle", "output_mode": "files_with_matches"}))
            .await;
        let text = text_result(&result);
        assert!(!result.is_error);
        assert!(
            !text.contains("[scan stopped after 10s:"),
            "a 200-file walk must finish well inside 10s, got: {text}"
        );
    }
```

同一 `mod tests` 里再加一个直接测 `grep_search` 的同步测试（可以精确控制 deadline）：

```rust
    #[test]
    fn grep_reports_deadline_truncation_with_expired_deadline() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.txt")), "needle\n").unwrap();
        }
        let re = regex::Regex::new("needle").unwrap();

        let (output, timed_out) = grep_search(
            tmp.path(),
            &re,
            None,
            OutputMode::FilesWithMatches,
            0,
            &[tmp.path().to_path_buf()],
            // `Instant` 由模块顶部的 `use std::time::Instant` 带入；
            // `Duration` 这里用全路径，避免测试模块再加一行 import。
            Some(
                Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .expect("clock is at least 1s past boot"),
            ),
        )
        .unwrap();

        assert!(timed_out, "an expired deadline must be reported to the caller");
        assert!(
            output.contains("[scan stopped after 10s:"),
            "the user must be told the scan was cut short, got: {output}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::grep`

Expected: 编译失败（`grep_search` 参数个数不匹配）、以及两个行为测试失败（`target/generated.rs` 仍出现在结果里）。

- [ ] **Step 3: 改 `grep_search` 签名为接受 deadline 并返回截断标记**

`grep.rs` 顶部 `use` 改成：

```rust
use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::walk;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
```

（删掉 `use walkdir::WalkDir;`）

新增常量，放在 `TRUNCATION_NOTICE` 之后：

```rust
const DEADLINE_NOTICE: &str =
    "[scan stopped after 10s: narrow path or glob, some matches were not reached]\n";
```

把 `grep_search` 的签名从

```rust
fn grep_search(
    base: &Path,
    re: &regex::Regex,
    glob_filter: Option<&glob::Pattern>,
    mode: OutputMode,
    context: usize,
    roots: &[PathBuf],
) -> Result<String, ToolsError> {
```

改为

```rust
fn grep_search(
    base: &Path,
    re: &regex::Regex,
    glob_filter: Option<&glob::Pattern>,
    mode: OutputMode,
    context: usize,
    roots: &[PathBuf],
    deadline: Option<Instant>,
) -> Result<(String, bool), ToolsError> {
```

- [ ] **Step 4: 把 `WalkDir` 循环换成 walker 的结果**

把原来的

```rust
    'walk: for entry in WalkDir::new(base).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
```

改为

```rust
    let walked = walk::walk_with_deadline(base, walk::FILES, deadline);
    let timed_out = matches!(walked.truncated, walk::Truncated::Deadline);

    'walk: for path in walked.paths.iter() {
        let path = path.as_path();
```

循环体内其余部分（glob 过滤、`is_binary`、`read_to_string`、匹配、`relative_to_roots`、
三种 `OutputMode` 的输出）**完全不变**。注意原来用 `break 'walk` 的地方保持不变，
`'walk` 标签继续有效。

循环结束后的收尾，从

```rust
    if truncated {
        output.push_str(TRUNCATION_NOTICE);
    }

    if !found_any {
        Ok(String::new())
    } else {
        Ok(output)
    }
}
```

改为

```rust
    if truncated {
        output.push_str(TRUNCATION_NOTICE);
    }
    if timed_out {
        output.push_str(DEADLINE_NOTICE);
    }

    if !found_any {
        // 即使一条都没找到，只要扫描被 deadline 截断，也必须说明，
        // 否则 "no matches" 会被误读为「确实没有」。
        Ok(if timed_out {
            (DEADLINE_NOTICE.to_string(), true)
        } else {
            (String::new(), false)
        })
    } else {
        Ok((output, timed_out))
    }
}
```

- [ ] **Step 5: 把 `call` 改成在 `spawn_blocking` 里跑**

`greptool` 的 `call` 里，把

```rust
        match grep_search(
            &base,
            &re,
            glob_pattern.as_ref(),
            mode,
            context,
            self.ctx.roots(),
        ) {
            Ok(output) => {
                if output.is_empty() {
                    ToolResult::text("no matches")
                } else {
                    ToolResult::text(output)
                }
            }
            Err(e) => e.into(),
        }
```

改为

```rust
        // The walk is synchronous and can touch thousands of files. Running it
        // on a tokio worker would park that thread for the whole scan and slow
        // down every other tool call in the same turn.
        let base_for_scan = base.clone();
        let roots = self.ctx.roots().to_vec();
        let glob_for_scan = glob_pattern.clone();
        let deadline = walk::deadline_from_now();

        let joined = tokio::task::spawn_blocking(move || {
            grep_search(
                &base_for_scan,
                &re,
                glob_for_scan.as_ref(),
                mode,
                context,
                &roots,
                Some(deadline),
            )
        })
        .await;

        match joined {
            Ok(Ok((output, _timed_out))) => {
                if output.is_empty() {
                    ToolResult::text("no matches")
                } else {
                    ToolResult::text(output)
                }
            }
            Ok(Err(e)) => e.into(),
            Err(e) => ToolResult::error(format!("grep worker failed: {e}")),
        }
```

注意：`regex::Regex` 是 `Clone + Send`，`glob::Pattern` 也是；`base` 需在移动前
`clone()`。若编译器提示 `re` 已被借用，把 `let re = ...` 改为在闭包下方
`let re_for_scan = re.clone();` 并移动那个副本。

- [ ] **Step 6: schema 描述补 root 说明**

把 `path` 的描述从

```rust
                "path": { "type": "string", "description": "File or directory, default root" },
```

改为

```rust
                "path": { "type": "string", "description": "File or directory, default root (the session working directory)" },
```

- [ ] **Step 7: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::grep`

Expected: 全部 PASS。原有的 8 个测试（content / files / count / glob 过滤 / 无匹配 /
多根相对路径 / 两个截断测试）必须仍然通过。

- [ ] **Step 8: 删除不再使用的 walkdir 直接依赖**

确认 `grep.rs` 里已无 `walkdir::`：`grep -n "walkdir" yi-agent-rs/crates/yi-agent-tools/src/fs/grep.rs` 应无输出。然后从 `Cargo.toml` 删除 `walkdir = "2"`。

Run: `cd yi-agent-rs && cargo build -p yi-agent-tools --offline`

Expected: 编译通过（`ignore` 自己仍然依赖 `walkdir`，不会从依赖图消失）。

- [ ] **Step 9: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/fs/grep.rs crates/yi-agent-tools/Cargo.toml Cargo.lock
git commit -m "fix(tools): make grep skip build output and run off the async pool"
```

---

### Task 3: glob 改用 walker，保持逐段展开语义

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/glob.rs`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/fs/glob.rs`（同文件 `mod tests`）

**Interfaces:**
- Consumes: `walk::walk_with_deadline`, `walk::MATCH_TARGETS`, `walk::deadline_from_now`, `walk::Truncated`
- Produces: `glob::MATCH: glob::MatchOptions`（crate 内常量）；`scan(base: &Path, pattern: &glob::Pattern, deadline: Option<Instant>) -> (Vec<PathBuf>, bool)`（返回**绝对**路径，展示换算留给 `call`，因为 `roots` 只在 `call` 里拿得到）

**三条关键约束（每一条写错都会静默改变结果集）：**

1. `require_literal_separator` 必须为 `true`（下面详述）。
2. walk 必须用 `walk::MATCH_TARGETS`，不能只用文件 —— 旧 `glob()` 返回目录。
3. `glob::Pattern` **没有** `is_absolute()`；判断绝对模式要用
   `Path::new(&args.pattern).is_absolute()`。

**约束 1（写错会静默扩大结果集）：**

`glob::glob()` 把模式按 `/` 切成组件逐层展开，所以组件内的 `*` **不跨** `/`。而
`Pattern::matches()` 默认 `require_literal_separator: false`，此时 `*` 会跨 `/`：

```
require_literal_separator: false → "*.rs" 匹配 "a/b.rs"   ← 错，会多返回一切
require_literal_separator: true  → "*.rs" 不匹配 "a/b.rs" ← 对，与 glob() 一致
```

- [ ] **Step 1: 写失败的测试**

**先加测试辅助函数**（`glob.rs` 的 `mod tests` 里没有 `text_result`，与
`grep.rs` 不同）。在 `mod tests` 的 `make_tool` 之后插入：

```rust
    fn text_result(result: &ToolResult) -> &str {
        match &result.content[0] {
            yi_agent_core::ContentBlock::Text(text) => text,
            _ => panic!("expected a text result"),
        }
    }
```

然后追加下面的测试。

```rust
    #[tokio::test]
    async fn glob_single_star_does_not_cross_directories() {
        // 与 glob() 的逐段展开一致：`*.rs` 只匹配根级。
        // 若 require_literal_separator 误设 false，这里会多出 src/main.rs。
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(tmp.path().join("top.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "*.rs"})).await;

        let text = text_result(&result);
        assert!(text.contains("top.rs"), "root-level file must match, got: {text}");
        assert!(
            !text.contains("src/main.rs"),
            "`*` must not cross `/`, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_double_star_matches_at_any_depth() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("a/b/c")).unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        fs::write(tmp.path().join("a/b/c/Cargo.toml"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/Cargo.toml"})).await;

        let text = text_result(&result);
        assert!(text.contains("Cargo.toml"), "zero depth must match, got: {text}");
        assert!(text.contains("a/b/c/Cargo.toml"), "deep match must appear, got: {text}");
    }

    #[tokio::test]
    async fn glob_still_matches_dot_files() {
        // require_literal_leading_dot: false 保持现行为：* 能命中 .gitignore。
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "*"})).await;

        let text = text_result(&result);
        assert!(text.contains(".gitignore"), "dot files must still match, got: {text}");
    }

    #[tokio::test]
    async fn glob_skips_gitignored_build_output() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        fs::create_dir(tmp.path().join("target")).unwrap();
        fs::write(tmp.path().join("target/out.rs"), "").unwrap();
        fs::write(tmp.path().join("src.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/*.rs"})).await;

        let text = text_result(&result);
        assert!(text.contains("src.rs"), "got: {text}");
        assert!(!text.contains("target/out.rs"), "got: {text}");
    }

    #[tokio::test]
    async fn glob_finds_matches_inside_dot_directories() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join(".github/workflows")).unwrap();
        fs::write(tmp.path().join(".github/workflows/ci.yml"), "").unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"pattern": "**/*.yml"})).await;

        let text = text_result(&result);
        assert!(
            text.contains(".github/workflows/ci.yml"),
            "hidden(false) keeps dot directories searchable, got: {text}"
        );
    }

    #[tokio::test]
    async fn glob_resolves_absolute_patterns() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("src")).unwrap();
        fs::write(tmp.path().join("src/main.rs"), "").unwrap();
        let tool = make_tool(&tmp);

        let pattern = format!("{}/src/*.rs", tmp.path().to_string_lossy());
        let result = tool.call(serde_json::json!({"pattern": pattern})).await;

        let text = text_result(&result);
        assert!(text.contains("src/main.rs"), "absolute patterns must work, got: {text}");
    }

    #[test]
    fn glob_reports_deadline_truncation() {
        let tmp = TempDir::new().unwrap();
        for i in 0..200 {
            fs::write(tmp.path().join(format!("f{i}.rs")), "").unwrap();
        }
        let pattern = glob::Pattern::new("**/*.rs").unwrap();

        let (matches, timed_out) = scan(
            tmp.path(),
            &pattern,
            // `Instant` 由模块顶部的 `use std::time::Instant` 带入；
            // `Duration` 这里用全路径，避免测试模块再加一行 import。
            Some(
                Instant::now()
                    .checked_sub(std::time::Duration::from_secs(1))
                    .expect("clock is at least 1s past boot"),
            ),
        );

        assert!(timed_out, "expired deadline must be reported");
        assert!(matches.is_empty(), "expired deadline must not collect paths");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::glob`

Expected: 编译失败，`cannot find function scan`.

- [ ] **Step 3: 加 MATCH 常量与 scan 函数**

在 `glob.rs` 的 `UNBOUNDED_ROOT_GLOB_WARNING` 之后加：

```rust
/// Matching options that reproduce `glob::glob()`'s component-by-component
/// expansion.
///
/// `require_literal_separator: true` is load-bearing: with the default
/// (`false`), `*` crosses `/`, so `{"pattern": "*.rs"}` would return every
/// `.rs` in the tree instead of the root-level ones.
pub(crate) const MATCH: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};
```

在 `is_unbounded_root_recursive_scan` 之后加：

```rust
/// Match `pattern` against every entry under `base`.
///
/// Returns the matched paths **as the walker produced them** (absolute), plus
/// whether the deadline cut the walk short. Turning them into display paths is
/// the caller's job: only it has the root set.
///
/// Uses `MATCH_TARGETS`, not `FILES`: the `glob()` this replaces returned
/// directories as matches and crossed symlinks. Both are load-bearing.
fn scan(
    base: &Path,
    pattern: &glob::Pattern,
    pattern_is_absolute: bool,
    deadline: Option<Instant>,
) -> (Vec<PathBuf>, bool) {
    let walked = walk::walk_with_deadline(base, walk::MATCH_TARGETS, deadline);
    let timed_out = matches!(walked.truncated, walk::Truncated::Deadline);

    let mut matches: Vec<PathBuf> = Vec::new();
    for path in walked.paths {
        // Absolute patterns match the absolute path; relative ones are matched
        // component-by-component against the path relative to `base`.
        let matched = if pattern_is_absolute {
            pattern.matches_path_with(&path, MATCH)
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            pattern.matches_path_with(rel, MATCH)
        };
        if matched {
            matches.push(path);
        }
    }
    (matches, timed_out)
}
```

`pattern_is_absolute` 作为参数传入而不是在 `scan` 里判断，是因为
`glob::Pattern` 没有 `is_absolute()`：`glob-0.3.4/src/lib.rs:556-563` 的
`Pattern` 只有 `original`／`tokens`／`is_recursive`／`has_metachars` 四个私有字段。
唯一可靠的判据是原始模式字符串：`Path::new(&args.pattern).is_absolute()`。

- [ ] **Step 4: 重写 `call` 使用 scan + spawn_blocking**

把 `call` 中从 `let full_pattern = base.join(&args.pattern);` 到 `matches.push(shown);`
的整段替换为：

```rust
        // `Pattern::join` semantics: an absolute pattern replaces the base
        // entirely, so a pattern outside `base` is legal. We therefore walk
        // from the pattern's static prefix when it is absolute, and from
        // `base` otherwise.
        let pattern = match glob::Pattern::new(&args.pattern) {
            Ok(p) => p,
            Err(e) => return ToolsError::Glob(e).into(),
        };
        // `glob::Pattern` has no `is_absolute()`; the raw pattern is the only
        // reliable source for this bit.
        let pattern_is_absolute = Path::new(&args.pattern).is_absolute();

        let walk_base = if pattern_is_absolute {
            static_prefix(&args.pattern).unwrap_or_else(|| base.clone())
        } else {
            base.clone()
        };

        let roots = self.ctx.roots().to_vec();
        let deadline = walk::deadline_from_now();
        let walk_base_for_scan = walk_base.clone();
        let pattern_for_scan = pattern.clone();

        let joined = tokio::task::spawn_blocking(move || {
            let (paths, timed_out) = scan(
                &walk_base_for_scan,
                &pattern_for_scan,
                pattern_is_absolute,
                Some(deadline),
            );
            let shown: Vec<String> = paths
                .iter()
                .map(|path| {
                    crate::context::relative_to_roots(&roots, path)
                        .unwrap_or_else(|| path.to_string_lossy().to_string())
                })
                .collect();
            (shown, timed_out)
        })
        .await;

        let (mut matches, timed_out) = match joined {
            Ok(v) => v,
            Err(e) => return ToolResult::error(format!("glob worker failed: {e}")),
        };
        matches.sort();
```

并在 `glob.rs` 里加辅助函数（放在 `scan` 之前）：

```rust
/// The literal directory prefix of `pattern`, up to the first component that
/// contains a wildcard. `None` when the pattern is not absolute or the first
/// component already has a wildcard.
fn static_prefix(pattern: &str) -> Option<PathBuf> {
    let path = Path::new(pattern);
    if !path.is_absolute() {
        return None;
    }
    let mut prefix = PathBuf::new();
    for component in path.components() {
        let as_str = component.as_os_str().to_string_lossy();
        if as_str.contains(['*', '?', '[']) {
            break;
        }
        prefix.push(component);
    }
    if prefix.as_os_str().is_empty() {
        None
    } else {
        Some(prefix)
    }
}
```

- [ ] **Step 5: 保留 warning 与输出组装**

`call` 末尾保持原有结构，把 `truncated` 的提示接上：

```rust
        let mut lines = Vec::new();
        if warn_unbounded_root_glob {
            lines.push(UNBOUNDED_ROOT_GLOB_WARNING.to_string());
        }
        if timed_out {
            lines.push(
                "[scan stopped after 10s: narrow path or glob, some matches were not reached]"
                    .to_string(),
            );
        }

        if matches.is_empty() {
            lines.push("no matches".to_string());
        } else {
            lines.extend(matches);
        }

        ToolResult::text(lines.join("\n"))
```

同时更新 `use` 行：

```rust
use crate::fs::walk;
use std::time::Instant;
```

- [ ] **Step 6: schema 描述补 root 说明**

```rust
                "path": { "type": "string", "description": "Base directory, default root (the session working directory)" },
```

- [ ] **Step 7: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::glob`

Expected: 全部 PASS。原有 4 个测试（递归 / 无匹配 / warning / 多根相对路径）必须仍然通过。

- [ ] **Step 8: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/fs/glob.rs
git commit -m "fix(tools): make glob gitignore-aware without widening its match set"
```

---

### Task 4: bash 结果回显 cwd

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/shell/bash.rs`

**Interfaces:**
- Consumes: `ToolsContext::cwd()`（既有）
- Produces: 无新增接口；`bash` 的两条结果分支都包含 `cwd: <path>`。

**注意 `ToolResult::error` 的格式**（`yi-agent-core/src/tool.rs:52-57`）：

```rust
pub fn error(text: impl Into<String>) -> Self {
    Self { content: vec![ContentBlock::Text(format!("error: {}", text.into()))], is_error: true }
}
```

它会在正文最前面再加一个 `error: `。所以超时分支的实际输出是
`error: cwd: /path\ncommand timeout after ...`，**不是**以 `cwd: ` 开头。
测试断言必须按这个真实形状写，不要写成 `starts_with("cwd: ")`。

- [ ] **Step 1: 写失败的测试**

**先加测试辅助函数**（`bash.rs` 的 `mod tests` 里没有 `text_result`，现有测试
一律内联 `if let ContentBlock::Text(s)`）。在 `make_tool` 之后插入：

```rust
    fn text_result(result: &ToolResult) -> &str {
        match &result.content[0] {
            yi_agent_core::ContentBlock::Text(text) => text,
            _ => panic!("expected a text result"),
        }
    }
```

然后追加：

```rust
    #[tokio::test]
    async fn result_leads_with_the_current_directory() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"command": "true"})).await;

        let text = text_result(&result);
        // 不要 canonicalize：`ToolsContext::new` 存的是 `tmp.path()` 原样，
        // 而 macOS 上 TempDir 的 `/var/folders/...` 与 canonicalize 后的
        // `/private/var/folders/...` 不是同一个字符串，断言会必然失败。
        let expected = format!("cwd: {}\n", tmp.path().display());
        assert!(
            text.starts_with(&expected),
            "result must lead with the cwd line, expected prefix {expected:?}, got: {text}"
        );
        assert!(text.contains("exit: 0"), "exit line must survive, got: {text}");
    }

    #[tokio::test]
    async fn cwd_line_follows_a_successful_cd() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let tool = make_tool(&tmp);

        let after_cd = tool
            .call(serde_json::json!({"command": "cd sub"}))
            .await;
        let text = text_result(&after_cd);
        assert!(
            text.contains("cwd: ") && text.contains("/sub"),
            "the cd must be visible immediately, got: {text}"
        );

        let next = tool.call(serde_json::json!({"command": "pwd"})).await;
        let next_text = text_result(&next);
        assert!(
            next_text.contains("/sub"),
            "the directory must persist across calls, got: {next_text}"
        );
    }

    #[tokio::test]
    async fn cwd_line_is_present_on_timeout() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            // 与既有 `bash_timeout_kills` 同样的形状：sleep 10 + timeout 1。
            .call(serde_json::json!({"command": "sleep 10", "timeout": 1}))
            .await;

        let text = text_result(&result);
        // `ToolResult::error` 会再加一个 `error: ` 前缀，所以这里不是
        // `starts_with("cwd: ")`。
        assert!(
            text.starts_with("error: cwd: "),
            "the timeout result must still show the cwd, got: {text}"
        );
        assert!(
            text.contains("command timeout after 1s"),
            "got: {text}"
        );
    }

    #[tokio::test]
    async fn cwd_line_is_present_on_nonzero_exit() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        let result = tool.call(serde_json::json!({"command": "exit 1"})).await;

        let text = text_result(&result);
        assert!(text.starts_with("cwd: "), "got: {text}");
        assert!(text.contains("exit: 1"), "got: {text}");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib shell::bash::tests::result_leads_with_the_current_directory`

Expected: FAIL，`result must lead with the cwd line`.

- [ ] **Step 3: 在两条结果分支里加 cwd 行**

**不要新增一个 `cwd_before_update` 变量。** 函数开头已有
`let cwd = self.ctx.cwd();`（`bash.rs:158`），它此后只被按引用传递
（`sandbox.command(&args.command, &cwd)`、`spawner.spawn(..., &cwd)`、
`.current_dir(&cwd)`、`parse_standalone_cd_target(&args.command, &cwd)`），
到结果组装处仍然有效。超时分支里 `set_cwd` 尚未执行，`cwd` 就是当前目录。

把超时分支改为：

```rust
        if timed_out {
            ToolResult::error(format!(
                "cwd: {}\ncommand timeout after {}s\nstdout:\n{}\nstderr:\n{}",
                cwd.display(),
                args.timeout.unwrap_or(DEFAULT_TIMEOUT),
                stdout_text,
                stderr_text,
            ))
        } else {
```

把成功分支改为（注意：`set_cwd` 之后重新读取，才能反映本次 `cd`）：

```rust
            ToolResult::text(format!(
                "cwd: {}\nexit: {}\nstdout:\n{}\nstderr:\n{}",
                self.ctx.cwd().display(),
                exit,
                stdout_text,
                stderr_text,
            ))
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib shell::bash`

Expected: 全部 PASS，包括原有的 `exit: 0` / `exit: 1` 断言（它们是 `contains`，不受前缀影响）。

- [ ] **Step 5: 补 schema 描述**

把 `command` 的描述从

```rust
                "command": { "type": "string", "description": "The bash command to execute" },
```

改为

```rust
                "command": { "type": "string", "description": "The command to execute. Runs in the session working directory, which persists across calls (a successful standalone `cd` changes it for later calls). Every result starts with a `cwd:` line showing the directory after this command." },
```

- [ ] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/src/shell/bash.rs
git commit -m "feat(tools): echo the working directory in every bash result"
```

---

### Task 5: system prompt 环境段与侦察优先策略

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/system_prompt.rs`

**Interfaces:**
- Consumes: `resolve_system_prompt(user, workdir)`（既有签名，不变）
- Produces: system prompt 新增 `## Environment` 段与一条策略条目

- [ ] **Step 1: 写失败的测试（环境段）**

在 `bootstrap.rs` 的 `mod tests` 里追加：

```rust
    #[test]
    fn resolve_system_prompt_states_the_working_directory() {
        let project = tempfile::TempDir::new().unwrap();
        // 不 canonicalize：`resolve_system_prompt` 用 `workdir.display()` 原样输出，
        // macOS 上 TempDir 的 `/var/folders/...` 与 `/private/var/...` 字符串不同。
        let workdir = project.path().to_path_buf();

        let prompt = resolve_system_prompt(None, &workdir).expect("prompt");

        assert!(
            prompt.contains("## Environment"),
            "prompt must name the environment section, got: {prompt}"
        );
        assert!(
            prompt.contains(&format!("Working directory: {}", workdir.display())),
            "prompt must state the absolute working directory, got: {prompt}"
        );
    }

    /// 顺序契约：环境段在项目指令之后、日期之前。
    #[test]
    fn environment_section_sits_between_project_instructions_and_the_date() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "Always run focused tests.")
            .expect("write AGENTS.md");

        let prompt = resolve_system_prompt(Some("Be brief.".into()), project.path()).expect("prompt");

        let custom = prompt.find("User-provided instructions:").unwrap();
        let project_rules = prompt.find("Project instructions (AGENTS.md):").unwrap();
        let environment = prompt.find("## Environment").unwrap();
        let date = prompt.find("Current date:").unwrap();

        assert!(custom < project_rules, "got: {prompt}");
        assert!(project_rules < environment, "environment follows project rules, got: {prompt}");
        assert!(environment < date, "environment precedes the date, got: {prompt}");
    }

    /// 没有 AGENTS.md 时环境段照常出现。
    #[test]
    fn environment_section_appears_without_project_instructions() {
        let project = tempfile::TempDir::new().unwrap();
        let prompt = resolve_system_prompt(None, project.path()).expect("prompt");

        assert!(prompt.contains("## Environment"));
        assert!(!prompt.contains("Project instructions"));
    }
```

- [ ] **Step 2: 写失败的测试（侦察优先策略）**

在 `agent.rs` 的 `mod tests` 里追加：

```rust
    #[test]
    fn default_system_prompt_asks_for_a_single_reconnaissance_pass() {
        let prompt = AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("Reconnoitre before editing"),
            "prompt must name the reconnaissance rule so a reviewer can find it"
        );
        assert!(
            prompt.contains("Do not explore one file per turn"),
            "prompt must forbid the one-file-per-turn pattern it is replacing"
        );
    }
```

- [ ] **Step 3: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core default_system_prompt_asks_for_a_single_reconnaissance_pass`
Expected: FAIL.

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime resolve_system_prompt_states_the_working_directory`
Expected: FAIL.

- [ ] **Step 4: 实现环境段**

`bootstrap.rs` 的 `resolve_system_prompt`，把

```rust
    let today = chrono::Local::now().format("%Y-%m-%d");
    Some(format!("{base}\n\nCurrent date: {today}"))
```

改为

```rust
    // 位置信息只在会话开始拼一次。放进 system prompt 而不是每轮 user 消息，
    // 是为了让前缀稳定、provider 的 prompt cache 继续命中（实测缓存命中
    // 4992/5038 token）。
    base.push_str("\n\n## Environment\n\n");
    base.push_str(&format!("Working directory: {}\n", workdir.display()));
    base.push_str(
        "All file tools (read/edit/write/grep/glob) resolve relative paths against this directory.\n\
         The bash tool starts in this directory and keeps its working directory across calls.\n\
         Run `pwd` once when you need to confirm a location after `cd`.\n\n\
         Do not probe for the project location. If this directory looks wrong, say so instead of\n\
         guessing other paths.",
    );

    let today = chrono::Local::now().format("%Y-%m-%d");
    Some(format!("{base}\n\nCurrent date: {today}"))
```

- [ ] **Step 5: 加入侦察优先策略**

`system_prompt.rs` 的 `Tool use strategy` 段末尾（`- Only split work across turns when a later
step genuinely depends on the RESULT of an earlier step.` 之后、空行与 `Example:` 之前）
插入：

```text
- Reconnoitre before editing: when a task spans code you have not read yet, spend
  your first response gathering the whole picture in parallel — one glob for the
  file layout, one grep for the entry point, read the one or two key files. Then
  act. Do not explore one file per turn.
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core system_prompt`
Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime resolve_system_prompt`

Expected: 全部 PASS。特别确认既有的
`resolve_system_prompt_appends_root_agents_md_after_custom_instructions` 仍然通过
（环境段插在它的 `Current date` 断言之前，不影响该测试的顺序断言）。

- [ ] **Step 7: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-runtime/src/bootstrap.rs crates/yi-agent-core/src/system_prompt.rs crates/yi-agent-core/src/agent.rs
git commit -m "feat(prompt): state the working directory and ask for one reconnaissance pass"
```

---

### Task 6: 端到端验证与文档同步

**Files:**
- Add: `docs/research/2026-11-06-interaction-latency-profile.md`（从主 worktree 带进来）
- Modify: `docs/project-management/yi-agent-tools.md`
- Modify: `docs/project-management/yi-agent-core.md`

**Interfaces:**
- Consumes: Task 1–5 的全部产出
- Produces: 无代码接口

- [ ] **Step 1: 全量测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-core && cargo test -p yi-agent-runtime`

Expected: 三个 crate 全绿。

- [ ] **Step 2: 加一条忽略的真实仓库实测（不进 CI，不花 API 费用）**

在 `walk.rs` 的 `mod tests` 末尾追加。它直接测真实仓库，不需要 provider：

```rust
    /// 真实仓库实测：不进 CI（`#[ignore]`），手动跑 `--ignored`。
    ///
    /// 判据是机制性的：走完整个仓库根不超过 deadline，且结果里一个
    /// `target/` 路径都没有。它不依赖模型行为，所以可以作为回归基线。
    #[test]
    #[ignore = "measures the real repository; run with --ignored"]
    fn real_repository_walk_skips_build_output_within_the_deadline() {
        // crates/yi-agent-tools -> yi-agent-rs -> 仓库根
        let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("repo root above crates/yi-agent-tools")
            .to_path_buf();

        let started = Instant::now();
        let outcome = walk(&repo_root, FILES);
        let elapsed = started.elapsed();

        assert!(
            matches!(outcome.truncated, Truncated::No),
            "the whole repository must finish inside the 10s deadline, took {elapsed:?}"
        );
        assert!(
            !outcome
                .paths
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == "target")),
            "target/ must never be walked ({} paths, {elapsed:?})",
            outcome.paths.len()
        );
        assert!(
            !outcome
                .paths
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == ".git")),
            ".git/ must never be walked"
        );
        assert!(
            outcome
                .paths
                .iter()
                .any(|p| p.ends_with("yi-agent-rs/Cargo.toml")),
            "real source files must still be found"
        );

        eprintln!(
            "walk {} -> {} files in {elapsed:?}",
            repo_root.display(),
            outcome.paths.len()
        );
    }
```

- [ ] **Step 3: 跑实测并记录数字**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib fs::walk::tests::real_repository_walk -- --ignored --nocapture`

Expected: PASS，并且 stderr 有一行 `walk <repo> -> <n> files in <elapsed>`。
把这一行的真实 `<n>` 与 `<elapsed>` 记下来，Step 7 要填进剖析报告。对照基线：
改造前根目录 `grep` 178 s、`glob **/Cargo.toml` 14.8 s，全树 1,704,405 个文件。

若这条不通过，说明 `!git` override 或 `git_ignore(true)` 没生效，回到 Task 1 排查，
不要靠调大 `SCAN_DEADLINE` 掩盖。

- [ ] **Step 4: 确认 system prompt 真的带上工作目录（不走 provider）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime resolve_system_prompt_states_the_working_directory`

Expected: PASS。这条不看模型输出，只断言拼装结果，所以是机制性判据。
另跑一次全 crate 的 prompt 测试，确认既有顺序契约没被破坏：

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime resolve_system_prompt && cargo test -p yi-agent-core system_prompt`

Expected: 全部 PASS。

- [ ] **Step 5: 跑全量门禁**

Run: `cd yi-agent-rs && just ci`

Expected: 20 道门禁全绿。若 `audit` 对新增依赖报公告，按 `yi-agent-rs/.cargo/audit.toml`
既有流程评估；若 `msrv-check` 失败，说明 `ignore` 依赖树里某个包的下限高于 1.89，
需在 `Cargo.toml` 中下调该依赖版本或提升 `rust-version`（后者需要单独评估）。

- [ ] **Step 6: 把剖析报告带进本分支**

剖析报告目前只在主 worktree 里且未跟踪（`docs/research/2026-11-06-interaction-latency-profile.md`），
本分支没有它。直接复制过来：

```bash
cd <主仓库根>
cp docs/research/2026-11-06-interaction-latency-profile.md \
   .worktrees/docs-latency-search-spec/docs/research/
cp scripts/latency_profile.py \
   .worktrees/docs-latency-search-spec/scripts/
```

预期：`ls .worktrees/docs-latency-search-spec/docs/research/2026-11-06-interaction-latency-profile.md`
存在。若主 worktree 里已被撤销，则跳过本步，并在 Step 7 中删掉对它的修改。

- [ ] **Step 7: 更新项目文档**

`docs/project-management/yi-agent-tools.md`：在 tools 模块清单里登记三条并可验证：

- `[x]` gitignore-aware walker，带 10s deadline —— 判据：
  `cargo test -p yi-agent-tools --lib fs::walk` 全绿；实现在
  `crates/yi-agent-tools/src/fs/walk.rs`。
- `[x]` `grep`/`glob` 跳过构建产物、在 `spawn_blocking` 中运行、明确报告截断 ——
  判据：`cargo test -p yi-agent-tools --lib fs::grep` 与 `fs::glob` 全绿。
- `[x]` `bash` 结果首行回显 cwd —— 判据：
  `cargo test -p yi-agent-tools --lib shell::bash::tests::result_leads_with_the_current_directory`。

`docs/project-management/yi-agent-core.md`：登记 system prompt 的环境段与侦察优先
策略，判据指向 Task 5 的两个测试名。

`docs/research/2026-11-06-interaction-latency-profile.md`：把 §3.1「真缺陷」段落标注
为已修复，并补一行改造后实测耗时（把 Step 2/3 的真实数字填进去，不要留占位符）。

- [ ] **Step 8: 更新模块索引计数**

若 `docs/project-management/README.md` 的模块索引表里 `yi-agent-tools` 的
「完成 / 总计」计数发生变化，同步更新。

- [ ] **Step 9: 提交**

```bash
git add docs/project-management/yi-agent-tools.md docs/project-management/yi-agent-core.md docs/research/2026-11-06-interaction-latency-profile.md docs/project-management/README.md scripts/latency_profile.py
git commit -m "docs: record the search-tool and prompt latency fixes"
```

---

## Self-Review

**1. Spec coverage**

| spec 章节 | 对应任务 |
|---|---|
| 4.1 环境段注入 | Task 5 Step 4 |
| 4.2 bash 回显 cwd | Task 4 |
| 4.3 工具描述补基准说明 | Task 2 Step 6、Task 3 Step 6、Task 4 Step 5 |
| 4.4 统一 walker（含 `.git` 排除） | Task 1 |
| 4.5 grep 改造（spawn_blocking + deadline 提示） | Task 2 |
| 4.6 glob 改造（`require_literal_separator: true`） | Task 3 |
| 4.7 侦察优先提示 | Task 5 Step 5 |
| 4.8 依赖变更（ignore、tokio rt、移除 walkdir） | Task 1 Step 1、Task 2 Step 8 |
| 4.9 并发与取消（deadline 在闭包内） | Task 1 Step 5、Task 2 Step 5、Task 3 Step 4 |
| 5.1–5.5 测试 | Task 1–5 各自的 Step 1 |
| 5.6 验证命令 | Task 6 Step 5 |
| 6 文档更新 | Task 6 Step 7–8 |
| 8 验收判据 | Task 6 Step 1–5 |

无遗漏。

**2. Placeholder scan**

已检查：无 `TBD`/`TODO`/「类似 Task N」/「添加适当错误处理」。
Task 6 Step 3 要求记录实测数字并在 Step 7 填进剖析报告，这是实施时才存在的
真实数据，不是留给读者猜的占位符。

**3. Type consistency**

- `walk::walk_with_deadline` / `walk::FILES` /
  `walk::MATCH_TARGETS` / `walk::deadline_from_now` / `walk::Truncated` /
  `walk::WalkOutcome` / `walk::SCAN_DEADLINE` 在 Task 1 定义，Task 2、3 按同名
  同签名使用。
- `grep_search(... , deadline: Option<Instant>) -> Result<(String, bool), ToolsError>`
  在 Task 2 定义并使用；Task 2 的同步测试与 `call` 都按 7 个参数、2 元组返回调用。
- `scan(base, pattern, pattern_is_absolute: bool, deadline) -> (Vec<PathBuf>, bool)`
  在 Task 3 定义；调用与测试都传 4 个参数。
- `walk::Truncated` 的两个变体 `No` / `Deadline` 在 Task 2、3 都以 `matches!(...)`
  比对，名称一致。
- `MATCH` 在 Task 3 定义为 `pub(crate) const`，同文件内使用。
- `glob::Pattern` 没有 `is_absolute()`（已核对 `glob-0.3.4/src/lib.rs:556-563`），
  所有绝对模式判断都走 `Path::new(...).is_absolute()` 或传入的 `pattern_is_absolute`。

**4. 依赖方向**

Task 3 的 `scan` 返回 `Vec<PathBuf>`（不是 `Vec<String>`），因为 `call` 里还需要
再经 `relative_to_roots` 换算成展示路径。计划中 Task 3 Step 3 的签名与 Step 1 的
测试解构方式都已按此对齐。

**5. 本次自审修掉的 10 处缺陷（每条都在 `/tmp` 用一次性程序实测过）**

| # | 缺陷 | 后果 | 修正 |
|---|---|---|---|
| 1 | `glob::Pattern::is_absolute()` 不存在 | 编译失败 | 改用 `Path::new(..).is_absolute()` 并作为参数传入 `scan` |
| 2 | `OverrideBuilder::new()` 返回 `OverrideBuilder` 而非 `Result` | 编译失败 | 去掉 `match ... Ok/Err` |
| 3 | 手写 `impl Default for Truncated` + `#[derive(Default)]` | `clippy -D warnings` 报 `derivable_impls` | 改为 `#[derive(Default)]` + `#[default]` 变体 |
| 4 | `walk_files(tmp.path())` 断言 `len() == 1` | `.gitignore` 自身也被遍历出来，实测 len=2 → 测试必失败 | 改为断言「产物不在、源码在」，不锁精确数量 |
| 5 | 只用一套 walker 策略 | 旧 `glob()` 返回目录且跟随符号链接，实测 `**/*` 少两个目录、`**/*.rs` 丢整条链接 | 新增 `WalkOptions` + `FILES` / `MATCH_TARGETS` |
| 6 | Task 1 的「红」步骤在 `mod walk;` 之前 | 0 tests 而非编译失败，掩盖真实错误 | 把模块声明提前为 Step 2 |
| 7 | `glob.rs` / `bash.rs` 的测试模块没有 `text_result` | 编译失败 | 计划里明确要求先加该辅助函数 |
| 8 | 测试里 `canonicalize()` | macOS 上 `/var/...` 与 `/private/var/...` 不同 → 断言必失败 | 改用 `tmp.path()` 原样 |
| 9 | `Instant::now() - Duration` | 极端情况 underflow panic | 改用 `checked_sub` |
| 10 | Task 4 新增冗余 `cwd_before_update` | 与函数开头已有的 `cwd` 重复 | 复用 `cwd` |

另外修正：Task 6 原 Step 2/3 用 `cargo run ... run --json` 调真实 provider，
依赖 API key 且产生费用；改为 `#[ignore]` 的真实仓库遍历测试（机制性判据，
不花 API 费用）。原 Step 6 引用的剖析报告不在本分支上，补了「先复制进来」一步。

**4. 一处需要实施者注意的依赖方向**

Task 3 的 `scan` 返回 `Vec<PathBuf>`（不是 `Vec<String>`），因为 `call` 里还需要
再经 `relative_to_roots` 换算成展示路径。计划中 Task 3 Step 3 的签名与 Step 1 的
测试解构方式都已按此对齐。
