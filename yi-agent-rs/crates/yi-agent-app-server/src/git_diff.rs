//! 在 thread 的工作目录上现算 git diff（只读，不落盘）。
//!
//! 比较基准是**分支分叉点**：`git merge-base HEAD <默认分支>`。这对应 GitHub
//! PR 的算法，也是「这个会话从分叉点以来改了什么」最贴切的语义。默认分支判定
//! 见 [`resolve_base`]；全不可用时退化为「仅工作区改动」。

use std::path::Path;

pub const MAX_DIFF_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitInfo {
    pub sha: String,
    pub short: String,
    pub subject: String,
    pub author: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileStat {
    pub path: String,
    /// 单字母状态：A 新增 / M 修改 / D 删除 / R 重命名。
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadDiff {
    /// 参与比较的 ref；`None` = 无基准（仅工作区）。
    pub base: Option<String>,
    /// `explicit` | `origin-head` | `origin-default` | `local-default` | `worktree-only`
    pub base_kind: String,
    pub merge_base: Option<String>,
    pub commits: Vec<CommitInfo>,
    pub files: Vec<FileStat>,
    pub unified_diff: String,
    pub truncated: bool,
}

/// 跑 git 并返回成功时的 stdout 文本。
///
/// 只认 success：`rev-parse --verify` / `merge-base` / `log` 这类命令失败时
/// 本就该当作「没有」，而 diff 类命令的退出码差异由 HTML 生成后的内容决定，
/// 故所有调用点都只需「有内容」或「没有」两态。
fn git_ok(directory: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(directory)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// 该 ref 能否在本仓库解析。
fn ref_exists(repo: &Path, r: &str) -> bool {
    git_ok(repo, &["rev-parse", "--verify", "--quiet", r]).is_some()
}

/// 判定比较基准。显式传入优先；否则按候选顺序找默认分支；都没有则 `None`。
pub fn resolve_base(repo: &Path, explicit: Option<&str>) -> (Option<String>, &'static str) {
    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if ref_exists(repo, name) {
            return (Some(name.to_string()), "explicit");
        }
    }
    // `refs/remotes/origin/HEAD` 是「远端默认分支」的权威指针。
    if ref_exists(repo, "refs/remotes/origin/HEAD") {
        return (Some("refs/remotes/origin/HEAD".to_string()), "origin-head");
    }
    for candidate in ["origin/main", "origin/master"] {
        if ref_exists(repo, candidate) {
            return (Some(candidate.to_string()), "origin-default");
        }
    }
    for candidate in ["main", "master"] {
        if ref_exists(repo, candidate) {
            return (Some(candidate.to_string()), "local-default");
        }
    }
    (None, "worktree-only")
}

pub fn merge_base(repo: &Path, base: &str) -> Option<String> {
    git_ok(repo, &["merge-base", "HEAD", base]).map(|s| s.trim().to_string())
}

fn commits(repo: &Path, merge_base: &str) -> Vec<CommitInfo> {
    // 单元分隔符用 ASCII 0x1f，记录分隔符用 0x1e，避免 subject 里的空白歧义。
    let Some(raw) = git_ok(
        repo,
        &["log", "--format=%H%x1f%h%x1f%s%x1f%an%x1f%at%x1e", &format!("{merge_base}..HEAD")],
    ) else {
        return Vec::new();
    };
    raw.split('\u{1e}')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            if record.trim().is_empty() {
                return None;
            }
            let mut parts = record.split('\u{1f}');
            let sha = parts.next()?.to_string();
            let short = parts.next()?.to_string();
            let subject = parts.next()?.to_string();
            let author = parts.next()?.to_string();
            let timestamp = parts.next()?.trim().parse::<i64>().ok()?;
            Some(CommitInfo { sha, short, subject, author, timestamp })
        })
        .collect()
}

/// `repo`（thread 的 cwd）可能只是工作树的一个子目录。所有 git 输出（numstat、
/// 合成头）都以仓库根为基准，故先解析根，再让文件内容也相对根读取，避免同一个
/// 结果里混入 cwd 相对路径。`rev-parse` 失败时退回 `repo`（原行为）。
fn repo_root(repo: &Path) -> std::path::PathBuf {
    git_ok(repo, &["rev-parse", "--show-toplevel"])
        .map(|s| std::path::PathBuf::from(s.trim()))
        .unwrap_or_else(|| repo.to_path_buf())
}

/// 未跟踪文件的路径集合（仓库根相对）。
///
/// `--full-name` 保证即使 cwd 是子目录，输出仍是仓库根相对——与 `diff --numstat`
/// 的基准一致；cwd 本就是仓库根时该选项不改变输出。
fn untracked_paths(repo: &Path) -> Vec<String> {
    git_ok(repo, &["ls-files", "--others", "--exclude-standard", "--full-name", "-z"])
        .map(|raw| raw.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// 文件清单 = 已跟踪改动（numstat）+ 未跟踪（A）。
///
/// 两来源互斥：numstat 覆盖的是已跟踪文件（含已暂存的新增），未跟踪走
/// `ls-files --others`，故**不会重复计入**同一个路径。
///
/// `merge_base` 为 `None`（无默认分支，退化为「仅工作区改动」）时改用 `git diff
/// --numstat HEAD`：裸 `git diff` 比的是「工作区 vs 索引」，看不见**已暂存**
/// （`git add` 过）的改动，而 `HEAD` 同时覆盖已暂存与未暂存。未跟踪文件的内容相对
/// 仓库根 `root` 读取，与其根相对路径对应。
fn files(repo: &Path, root: &Path, merge_base: Option<&str>) -> Vec<FileStat> {
    let mut out: Vec<FileStat> = Vec::new();
    let numstat = match merge_base {
        Some(mb) => git_ok(repo, &["diff", "--numstat", "-z", mb]),
        None => git_ok(repo, &["diff", "--numstat", "-z", "HEAD"]),
    };
    if let Some(raw) = numstat {
        out.extend(parse_numstat(&raw));
    }
    for path in untracked_paths(repo) {
        let full = root.join(&path);
        let additions =
            std::fs::read(&full).map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64).unwrap_or(0);
        let binary = is_binary(&full);
        out.push(FileStat { path, status: "A".into(), additions, deletions: 0, binary });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// 解析 `--numstat -z`：`<add>\t<del>\t<path>\0`；重命名时 path 字段含旧名与新名。
fn parse_numstat(raw: &str) -> Vec<FileStat> {
    let mut out = Vec::new();
    let mut fields = raw.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        // entry = "<add>\t<del>\t<path>"；重命名时下一段是旧名。
        let mut it = entry.splitn(3, '\t');
        let (Some(add), Some(del), Some(path)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let binary = add == "-" || del == "-";
        let additions = add.parse().unwrap_or(0);
        let deletions = del.parse().unwrap_or(0);
        let (status, path) = if path.is_empty() {
            // 重命名：当前 entry 的 path 为空，旧名在下一段，新名在再下一段。
            let _old = fields.next().unwrap_or("");
            let new = fields.next().unwrap_or("");
            ("R".to_string(), new.to_string())
        } else {
            let status =
                if deletions == 0 && additions > 0 { "A" } else if additions == 0 && deletions > 0 { "D" } else { "M" };
            (status.to_string(), path.to_string())
        };
        out.push(FileStat { path, status, additions, deletions, binary });
    }
    out
}

fn is_binary(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else { return false };
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// 未跟踪文件合成一份「整文件新增」的 unified diff（避免触碰用户索引）。
///
/// `paths` 与 `root` 都是仓库根相对，故合成头 `diff --git a/.. b/..` 与 git 自己
/// 产出的头共用同一基准。
fn untracked_diff(root: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(root.join(path)) else { continue };
        let lines: Vec<&str> = content.lines().collect();
        out.push_str(&format!("diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"));
        out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
        for line in &lines {
            out.push('+');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn truncate(diff: String) -> (String, bool) {
    if diff.len() <= MAX_DIFF_BYTES {
        return (diff, false);
    }
    let mut boundary = MAX_DIFF_BYTES;
    while boundary > 0 && !diff.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = diff[..boundary].to_owned();
    truncated.push_str("\n... diff truncated\n");
    (truncated, true)
}

pub fn thread_diff(repo: &Path, explicit_base: Option<&str>) -> ThreadDiff {
    let root = repo_root(repo);
    let (base, base_kind) = resolve_base(repo, explicit_base);
    let mb = base.as_deref().and_then(|b| merge_base(repo, b));
    let commits = mb.as_deref().map(|m| commits(repo, m)).unwrap_or_default();
    let files = files(repo, &root, mb.as_deref());

    // 无基准时同样要报「仅工作区改动」：`git diff HEAD` 覆盖已跟踪文件的
    // 已暂存 + 未暂存改动（裸 `git diff` 只看工作区 vs 索引，会漏掉已暂存部分），
    // 与 `files` 的 `None` 分支同源。
    let tracked = match mb.as_deref() {
        Some(mb) => git_ok(repo, &["diff", "--no-color", mb]),
        None => git_ok(repo, &["diff", "--no-color", "HEAD"]),
    };
    let mut diff = tracked.unwrap_or_default();
    // 未跟踪不在 `git diff` 的输出里，单独合成并入；它们不在 numstat 中，
    // 故不会与已跟踪改动重复。
    diff.push_str(&untracked_diff(&root, &untracked_paths(repo)));
    let (unified_diff, truncated) = truncate(diff);

    ThreadDiff {
        base,
        base_kind: base_kind.to_string(),
        merge_base: mb,
        commits,
        files,
        unified_diff,
        truncated,
    }
}

pub fn commit_diff(repo: &Path, sha: &str) -> (String, bool) {
    let raw = git_ok(repo, &["show", "--no-color", "--format=", sha]).unwrap_or_default();
    truncate(raw)
}

pub fn file_diff(repo: &Path, merge_base: Option<&str>, path: &str) -> (String, bool) {
    // `None` 分支与 `files` / `thread_diff` 保持一致：`HEAD` 覆盖已暂存 + 未暂存，
    // 裸 `git diff` 会漏掉已暂存的改动。
    let raw = match merge_base {
        Some(mb) => git_ok(repo, &["diff", "--no-color", mb, "--", path]).unwrap_or_default(),
        None => git_ok(repo, &["diff", "--no-color", "HEAD", "--", path]).unwrap_or_default(),
    };
    truncate(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个临时 git 仓库，初始提交 `init` 后停在 `trunk` 上。
    ///
    /// 分支名故意不叫 `main`/`master`，也没有 remote——这样「无默认分支」才是
    /// 真的无默认分支，`resolve_base` 的 `worktree-only` 分支才有测试覆盖。
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-q", "-b", "trunk"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        dir
    }

    #[test]
    fn worktree_only_when_no_default_branch_exists() {
        let dir = repo();
        let (base, kind) = resolve_base(dir.path(), None);
        assert_eq!(base, None);
        assert_eq!(kind, "worktree-only");
    }

    #[test]
    fn explicit_base_wins_and_reports_kind() {
        let dir = repo();
        let (base, kind) = resolve_base(dir.path(), Some("trunk"));
        assert_eq!(base.as_deref(), Some("trunk"));
        assert_eq!(kind, "explicit");
    }

    #[test]
    fn untracked_and_uncommitted_are_both_included() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap(); // 未提交改动
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap(); // 未跟踪
        let d = thread_diff(dir.path(), Some("trunk"));
        // 在 trunk 上 merge-base == HEAD（自己与自己），diff 只含工作区
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"), "tracked edit must appear: {paths:?}");
        assert!(paths.contains(&"new.txt"), "untracked file must appear: {paths:?}");
        assert!(d.unified_diff.contains("+two"), "diff: {}", d.unified_diff);
        assert!(d.unified_diff.contains("+hello"), "diff: {}", d.unified_diff);
    }

    /// 回归 #1：无默认分支（`worktree-only`）时也必须报出已跟踪文件的未提交改动。
    ///
    /// 修复前 `files` / `unified_diff` 都只在 `Some(mb)` 分支里跑 git diff，
    /// 于是 `worktree-only` 只剩未跟踪文件，已跟踪文件的未提交编辑整体丢失。
    #[test]
    fn worktree_only_includes_tracked_uncommitted_edits() {
        let dir = repo(); // 分支 `trunk`，无 remote ⇒ resolve_base 给出 worktree-only
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap(); // 已跟踪文件的未提交改动
        let d = thread_diff(dir.path(), None);
        assert_eq!(d.base_kind, "worktree-only");
        assert_eq!(d.base, None);
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"), "tracked uncommitted edit must appear: {paths:?}");
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.additions, a.deletions), (1, 0), "numstat must be read: {a:?}");
        assert!(d.unified_diff.contains("+two"), "tracked edit must be in the diff: {}", d.unified_diff);
    }

    /// 回归 #1b：`worktree-only` 下**已暂存**（`git add` 过）的改动同样要报出来。
    ///
    /// 裸 `git diff` 比的是「工作区 vs 索引」，`git add` 之后两者相同，于是已暂存
    /// 的改动在 `files` 与 `unified_diff` 里双双消失；`git diff HEAD` 才同时覆盖
    /// 已暂存与未暂存改动。
    #[test]
    fn worktree_only_includes_staged_only_edits() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        // 只暂存、不再改动：裸 `git diff` 在这里输出为空。
        let d = thread_diff(dir.path(), None);
        assert_eq!(d.base_kind, "worktree-only");
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"a.txt"), "staged-only edit must appear: {paths:?}");
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.additions, a.deletions), (1, 0), "numstat must be read: {a:?}");
        assert!(d.unified_diff.contains("+two"), "staged-only edit must be in the diff: {}", d.unified_diff);
    }

    /// 回归 #1c：已暂存的**新文件**不能被漏掉。
    ///
    /// 它已在索引里，故 `ls-files --others` 不会报它；又因「索引 == 工作区」，
    /// 裸 `git diff` 也不会报它——`git diff HEAD` 是唯一能看见它的范围。
    #[test]
    fn worktree_only_includes_staged_new_files() {
        let dir = repo();
        std::fs::write(dir.path().join("n.txt"), "brand new\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "n.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let d = thread_diff(dir.path(), None);
        assert_eq!(d.base_kind, "worktree-only");
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["n.txt"], "staged new file must appear exactly once: {paths:?}");
        assert!(
            d.unified_diff.contains("+brand new"),
            "staged new file content must be in the diff: {}",
            d.unified_diff
        );
    }

    /// 回归 #1d：已暂存 + 未暂存的混合改动必须**完整**报出（两段增量都要有）。
    #[test]
    fn worktree_only_reports_staged_and_unstaged_deltas_together() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap(); // 暂存后再改
        let d = thread_diff(dir.path(), None);
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!((a.additions, a.deletions), (2, 0), "both deltas must be counted: {a:?}");
        assert!(d.unified_diff.contains("+two"), "staged delta missing: {}", d.unified_diff);
        assert!(d.unified_diff.contains("+three"), "unstaged delta missing: {}", d.unified_diff);
    }

    /// 回归 #1e：`file_diff` 的 `None` 分支与 `thread_diff` 同源，同样必须看见已暂存改动。
    #[test]
    fn file_diff_without_base_includes_staged_only_edits() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let (diff, truncated) = file_diff(dir.path(), None, "a.txt");
        assert!(!truncated);
        assert!(diff.contains("+two"), "staged-only edit must be in file_diff: {diff}");
    }

    /// 回归 #2：`repo` 是工作树子目录时，所有路径必须统一为仓库根相对。
    ///
    /// 修复前 `ls-files --others` 给的是 cwd 相对路径（`u.txt`），而
    /// `diff --numstat` 给的是仓库根相对路径（`sub/t.txt`），同一次结果里
    /// 混了两套基准；合成的 `diff --git a/.. b/..` 头也跟着错。
    #[test]
    fn subdirectory_cwd_reports_root_relative_paths() {
        let dir = repo();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("t.txt"), "t\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "add sub"]] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(sub.join("t.txt"), "t\nedit\n").unwrap(); // 已提交文件的未提交改动
        std::fs::write(sub.join("u.txt"), "hello\n").unwrap(); // 子目录里的未跟踪文件

        let d = thread_diff(&sub, None); // thread 的 cwd 是子目录（Task 2 的调用方式）
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"sub/t.txt"), "tracked edit must be root-relative: {paths:?}");
        assert!(paths.contains(&"sub/u.txt"), "untracked must be root-relative: {paths:?}");
        assert!(!paths.contains(&"u.txt") && !paths.contains(&"t.txt"), "no cwd-relative paths: {paths:?}");
        assert!(
            d.unified_diff.contains("diff --git a/sub/u.txt b/sub/u.txt"),
            "synthesized header must be root-relative: {}",
            d.unified_diff
        );
        assert!(d.unified_diff.contains("+hello"), "untracked content must be in the diff: {}", d.unified_diff);
    }

    #[test]
    fn commits_are_the_branch_introduced_ones() {
        let dir = repo();
        // 造一个分叉：从 trunk 起分支并加一个提交
        assert!(std::process::Command::new("git")
            .args(["checkout", "-q", "-b", "feature"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join("a.txt"), "feature\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["commit", "-qam", "feature work"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        // 以 trunk 为 base 时，feature 引入的提交应被列出
        let d = thread_diff(dir.path(), Some("trunk"));
        assert_eq!(d.commits.len(), 1, "commits: {:?}", d.commits);
        assert_eq!(d.commits[0].subject, "feature work");
        assert!(!d.commits[0].short.is_empty());
    }
}
