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
            files
                .iter()
                .any(|p| p.ends_with(".github/workflows/ci.yml")),
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
        // git_global(false)：用户全局 git 配置里的 excludes 不得改写结果。
        //
        // 本用例自带一份全局配置，不依赖本机 `~/.config/git/ignore` 的内容：
        // `GIT_CONFIG_GLOBAL` 指向临时 gitconfig，其 `core.excludesFile` 指向
        // 一份会隐藏下面那个文件的 ignore 文件。`ignore` 在同一次 walk 里现读
        // 该变量（无缓存），所以这份配置就是本次 walk 看到的「用户全局规则」。
        // 谁把 `git_global` 改成 `true`，walk 就会吞掉该文件，断言随即失败——
        // 在任何机器上都一样，包括没有那条全局规则的 CI runner。
        let root = TempDir::new().unwrap();
        fs::create_dir_all(root.path().join(".claude")).unwrap();
        fs::write(root.path().join(".claude/settings.local.json"), "{}").unwrap();

        // 配置放在 walk root 之外，免得它自己进结果集。
        //
        // 规则刻意用不带 `/` 的裸文件名：`ignore` 把全局 gitignore 的根设在
        // **进程 CWD**（`GlobalGitignore::build_with_cwd` 传的是
        // `current_dir()`），带 `/` 的规则会锚定到 CWD 下面，而 walk root 是
        // 一个与 CWD 无关的临时目录，锚定规则永远匹配不上——那样断言就又变成
        // 空转。裸文件名按 git 的语义匹配任意层级，故与 walk root 位置无关。
        let config = TempDir::new().unwrap();
        let excludes = config.path().join("global-ignore");
        fs::write(&excludes, "settings.local.json\n").unwrap();
        let gitconfig = config.path().join("gitconfig");
        fs::write(
            &gitconfig,
            format!("[core]\n\texcludesFile = \"{}\"\n", excludes.display()),
        )
        .unwrap();

        // 只在这个块里 `GIT_CONFIG_GLOBAL` 生效；guard 在块尾恢复原值，所以
        // 即使断言失败也不会把变量泄漏给其它测试。
        let files = {
            struct Restore(Option<std::ffi::OsString>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    // SAFETY: 本用例独占该变量，drop 时立刻恢复原值。
                    match self.0.take() {
                        Some(v) => unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", v) },
                        None => unsafe { std::env::remove_var("GIT_CONFIG_GLOBAL") },
                    }
                }
            }
            // SAFETY: 同上——先存旧值再改，由 guard 负责恢复。
            let _restore = Restore(std::env::var_os("GIT_CONFIG_GLOBAL"));
            unsafe { std::env::set_var("GIT_CONFIG_GLOBAL", &gitconfig) };
            rel(&walk(root.path(), FILES))
        };

        assert!(
            files
                .iter()
                .any(|p| p.ends_with(".claude/settings.local.json")),
            "git_global(false) must ignore the user's global gitconfig excludes; \
             if this fails, the walker is honouring the global excludes. \
             got {files:?}"
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
            outcome
                .paths
                .iter()
                .any(|p| p.ends_with("sub") && p.is_dir()),
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
        std::os::unix::fs::symlink(outside.path().join("real.rs"), tmp.path().join("link.rs"))
            .unwrap();

        let files = rel(&walk(tmp.path(), MATCH_TARGETS));

        assert!(
            files.iter().any(|p| p.ends_with("link.rs")),
            "symlinked files must be visited, got {files:?}"
        );
    }

    /// `FILES` 是 `grep` 的策略：不跟随符号链接。旧 `WalkDir` 也不跟随，
    /// 所以这是 parity，而不是新行为；这条测试钉住它，免得有人把两个策略
    /// 合成一个后 grep 开始跟着链接读到根目录之外。
    #[test]
    fn files_mode_does_not_follow_symlinks() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        fs::write(outside.path().join("real.rs"), "needle\n").unwrap();
        std::os::unix::fs::symlink(outside.path().join("real.rs"), tmp.path().join("link.rs"))
            .unwrap();
        fs::write(tmp.path().join("plain.rs"), "needle\n").unwrap();

        let files = rel(&walk(tmp.path(), FILES));

        assert!(
            files.iter().any(|p| p.ends_with("plain.rs")),
            "real files must still be found, got {files:?}"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("link.rs")),
            "FILES must not follow symlinks, got {files:?}"
        );
    }

    /// 已知 parity 缺口：旧 `glob()` 会把断链当作一条匹配返回，而
    /// `MATCH_TARGETS`（follow_links = true）解析链接时 `fs::metadata` 失败，
    /// 该条目以 `Err` 形式产出，被 walker 跳过。这里把「跳过且不中断整趟遍历」
    /// 钉成明确契约：断链本身不可读，报告它只会让调用方读到错误。
    #[test]
    fn a_broken_symlink_is_skipped_without_aborting_the_walk() {
        let tmp = TempDir::new().unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("does-not-exist"),
            tmp.path().join("dangling.rs"),
        )
        .unwrap();
        fs::write(tmp.path().join("real.rs"), "x").unwrap();

        let files = rel(&walk(tmp.path(), MATCH_TARGETS));

        assert!(
            files.iter().any(|p| p.ends_with("real.rs")),
            "a dangling link must not abort the walk, got {files:?}"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("dangling.rs")),
            "a dangling link is not yieldable, got {files:?}"
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
