# 会话 Git Diff Tab 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在桌面端子 agent 面板新增「Git Diff」Tab，以 GitHub 风格展示整个 root thread 相对分支分叉点的变更，并新增工具 + 系统 skill 让模型能主动把它推到用户眼前。

**Architecture:** diff 由 app-server 直接在 thread 的 cwd 上跑只读 git 命令现算（不依赖 daemon、不落盘）；新增 RPC `thread/diff/read` 供前端拉取；新增工具 `show_git_diff` + 系统 skill `git-diff-review` 走「句柄广播 → 通知 → 前端响应」范式（仿 `theme_tool.rs`）；前端把现有 `SubagentTrace` 包进一个带 Tab 条的常驻面板 `ThreadDetailPanel`，Tab 内容为「轨迹」与新的 `GitDiffView`。

**Tech Stack:** Rust（app-server crate，`std::process::Command` 调 git、serde、tokio broadcast）、React + TypeScript + Tailwind（desktop crate）、Vitest + Testing Library。

## Global Constraints

- 比较基准：分支分叉点 = `git merge-base HEAD <默认分支>`。默认分支判定顺序：`refs/remotes/origin/HEAD` → `origin/main` → `origin/master` → `main` → `master`；皆不可用则退化为「仅工作区改动」。
- diff 范围：**含未提交改动 + 未跟踪新文件**。
- 视图：**unified（单列）**，不做 side-by-side；Commits 列表 + Files changed 聚合 diff 都要。
- 模型控制面：工具可传 `base`，默认自动 merge-base。
- diff 计算位置：**app-server**（`yi-agent-rs/crates/yi-agent-app-server`），不新增 daemon IPC。
- diff 文本截断阈值：`MAX_DIFF_BYTES = 256 * 1024`，截断后 `truncated = true`；文件列表不受截断影响。
- skill 名：`git-diff-review`，作为系统 skill 打进 `yi-agent-rs/crates/yi-agent-skills/src/assets/`。
- 不新增状态栏按钮：沿用现有「子 agent 面板」图标作为面板开合入口。
- 不改子 agent 轨迹 Tab 的既有渲染与交互逻辑。
- Rust 验证命令：`cargo test -p yi-agent-app-server -p yi-agent-skills`。前端验证命令（在 `desktop/` 下）：`npm test`。

---

### Task 1: git diff 纯函数模块

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（新增 `pub(crate) mod git_diff;`）

**Interfaces:**
- Consumes: 无（本任务自包含）。
- Produces:
  - `pub struct CommitInfo { pub sha: String, pub short: String, pub subject: String, pub author: String, pub timestamp: i64 }`
  - `pub struct FileStat { pub path: String, pub status: String, pub additions: u64, pub deletions: u64, pub binary: bool }`
  - `pub struct ThreadDiff { pub base: Option<String>, pub base_kind: String, pub merge_base: Option<String>, pub commits: Vec<CommitInfo>, pub files: Vec<FileStat>, pub unified_diff: String, pub truncated: bool }`
  - `pub const MAX_DIFF_BYTES: usize = 256 * 1024;`
  - `pub fn resolve_base(repo: &Path, explicit: Option<&str>) -> (Option<String>, &'static str)`
  - `pub fn thread_diff(repo: &Path, explicit_base: Option<&str>) -> ThreadDiff`
  - `pub fn commit_diff(repo: &Path, sha: &str) -> (String, bool)`
  - `pub fn file_diff(repo: &Path, merge_base: Option<&str>, path: &str) -> (String, bool)`

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs`，先只写测试模块与一个临时 git 仓库辅助函数：

```rust
//! 在 thread 的工作目录上现算 git diff（只读，不落盘）。
//!
//! 比较基准是**分支分叉点**：`git merge-base HEAD <默认分支>`。这对应 GitHub
//! PR 的算法，也是「这个会话从分叉点以来改了什么」最贴切的语义。默认分支判定
//! 见 [`resolve_base`]；全不可用时退化为「仅工作区改动」。

use std::path::Path;

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
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-app-server git_diff 2>&1 | tail -30`
Expected: 编译失败，报 `cannot find function resolve_base` / `thread_diff` 未定义。

- [ ] **Step 3: 实现最小代码**

在 `git_diff.rs` 的测试模块上方补齐实现（`use std::path::Path;` 已在测试模块上方写过一次，**不要重复**）：

```rust
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

/// 未跟踪文件的路径集合（相对仓库根）。
fn untracked_paths(repo: &Path) -> Vec<String> {
    git_ok(repo, &["ls-files", "--others", "--exclude-standard", "-z"])
        .map(|raw| raw.split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// 文件清单 = 已跟踪改动（numstat）+ 未跟踪（A）。
///
/// 两来源互斥：numstat 覆盖的是已跟踪文件（含已暂存的新增），未跟踪走
/// `ls-files --others`，故**不会重复计入**同一个路径。
fn files(repo: &Path, merge_base: Option<&str>) -> Vec<FileStat> {
    let mut out: Vec<FileStat> = Vec::new();
    if let Some(mb) = merge_base {
        if let Some(raw) = git_ok(repo, &["diff", "--numstat", "-z", mb]) {
            out.extend(parse_numstat(&raw));
        }
    }
    for path in untracked_paths(repo) {
        let additions = std::fs::read(repo.join(&path))
            .map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64)
            .unwrap_or(0);
        let binary = is_binary(&repo.join(&path));
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
fn untracked_diff(repo: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(repo.join(path)) else { continue };
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
    let (base, base_kind) = resolve_base(repo, explicit_base);
    let mb = base.as_deref().and_then(|b| merge_base(repo, b));
    let commits = mb.as_deref().map(|m| commits(repo, m)).unwrap_or_default();
    let files = files(repo, mb.as_deref());

    let mut diff = String::new();
    if let Some(mb) = mb.as_deref() {
        if let Some(raw) = git_ok(repo, &["diff", "--no-color", mb]) {
            diff.push_str(&raw);
        }
    }
    // 未跟踪不在 `git diff <mb>` 的输出里，单独合成并入；它们不在 numstat 中，
    // 故不会与已跟踪改动重复。
    diff.push_str(&untracked_diff(repo, &untracked_paths(repo)));
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
    let raw = match merge_base {
        Some(mb) => git_ok(repo, &["diff", "--no-color", mb, "--", path]).unwrap_or_default(),
        None => git_ok(repo, &["diff", "--no-color", "--", path]).unwrap_or_default(),
    };
    truncate(raw)
}
```

在 `lib.rs` 的模块声明区加入：

```rust
pub(crate) mod git_diff;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-app-server git_diff 2>&1 | tail -30`
Expected: 4 个 `git_diff::tests::*` 全 PASS。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): compute a thread's git diff against its fork point"
```

---

### Task 2: `thread/diff/read` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（在 RPC `match` 里新增分支，位置紧邻 `agent/trace/read` 附近；并新增一个取 thread cwd 的小函数）
- Test: 同文件 `#[cfg(test)] mod tests`（沿用既有 `Harness`）

**Interfaces:**
- Consumes: `git_diff::{thread_diff, commit_diff, file_diff, ThreadDiff}`（Task 1）。
- Produces: RPC `thread/diff/read`，params `{ threadId: string, base?: string, commit?: string, path?: string }`；响应字段 `{ base, baseKind, mergeBase, commits[], files[], unifiedDiff, truncated }`，`commit` 分支响应 `{ unifiedDiff, truncated }`，`path` 分支同样 `{ unifiedDiff, truncated }`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `pub(crate) mod tests` 里加（沿用既有 `Harness` / `test_config` 的真实 API；注意 `thread/diff/read` 需要 thread 的 cwd，故用自定义 `cfg.workdir` 指向临时 git 仓库）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn thread_diff_read_reports_base_and_files() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    // init_git_repo 只建空提交；补一个文件再改它，制造「已提交 + 未提交」两段改动。
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    for args in [vec!["add", "."], vec!["commit", "-q", "-m", "add a"]] {
        assert!(std::process::Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap(); // 未提交改动

    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"thread/diff/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    // thread/start 之后流里可能还夹着通知（如 process/updated），故读到 id 7 为止。
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(7)) {
        v = h.read_value().await;
    }
    assert_eq!(v["result"]["files"][0]["path"], "a.txt");
    assert!(v["result"]["unifiedDiff"].as_str().unwrap().contains("+y"));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_diff_read_rejects_unknown_thread() {
    let mut h = Harness::new().await;
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":8,"method":"thread/diff/read","params":{"threadId":"thread-nope"}}"#)
        .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(8)) {
        v = h.read_value().await;
    }
    assert_eq!(v["error"]["code"], -32011, "unknown thread uses the shared code");
}
```

> 说明：`init_git_repo` / `Harness::with_cfg` / `start_thread` / `initialize` 都是本文件既有测试辅助（见 `server.rs:7547`、`:8247`、`:8486`、`:8503`）。若 `dir.path()` 所在分支恰好是 `main`/`master`，`baseKind` 会是 `local-default`；测试只断言文件与 diff 文本，不锁 baseKind，避免绑定具体分支名。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-app-server thread_diff_read 2>&1 | tail -30`
Expected: 响应为 method-not-found 或断言失败（RPC 尚未实现）。

- [ ] **Step 3: 实现 RPC**

在 `server.rs` 的请求 `match` 里，`"agent/trace/read" => { ... }` 分支之后加入（`crate::git_diff` 的完整路径引用；`Path` 已在本文件顶部导入）：

```rust
"thread/diff/read" => {
    let Some(thread_id) = require_thread_id(&hub, &client, &req.params, id.clone()).await? else {
        continue;
    };
    if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone()).await? {
        continue;
    }
    // cwd 是 diff 的唯一来源：thread 的 cwd，缺省兜底 cfg.workdir。
    let cwd = threads
        .get(&thread_id)
        .map(|t| t.cwd.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| cfg.workdir.display().to_string());
    let repo = Path::new(&cwd);
    let base = req.params.get("base").and_then(|v| v.as_str());
    let result = if let Some(commit) = req.params.get("commit").and_then(|v| v.as_str()) {
        let (unified_diff, truncated) = crate::git_diff::commit_diff(repo, commit);
        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
    } else if let Some(path) = req.params.get("path").and_then(|v| v.as_str()) {
        let d = crate::git_diff::thread_diff(repo, base);
        let (unified_diff, truncated) =
            crate::git_diff::file_diff(repo, d.merge_base.as_deref(), path);
        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
    } else {
        serde_json::to_value(crate::git_diff::thread_diff(repo, base)).unwrap_or(json!(null))
    };
    write_response(&hub, &client, ok_response(id, result)).await?;
}
```

`ThreadDiff` / `FileStat` / `CommitInfo` 已在 Task 1 加了 `#[serde(rename_all = "camelCase")]`，故响应字段是 `baseKind` / `mergeBase` / `unifiedDiff` / `truncated`，与前端类型一致。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-app-server thread_diff_read 2>&1 | tail -30`
Expected: 两个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs
git commit -m "feat(app-server): expose thread/diff/read over RPC"
```

---

### Task 3: `show_git_diff` 工具 + `ui/gitDiff/focus` 通知

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff_tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（`pub mod git_diff_tool;`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（新增 `Notification::UiGitDiffFocus`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`build_runtime_tooling` 注册工具；`serve` 里起 watcher；`RuntimeAttachments` 加句柄）
- Test: 各文件内测试模块

**Interfaces:**
- Consumes: `crate::git_diff`（仅用于回执里回报采用的 base —— 这里不重算，直接回报参数）。
- Produces:
  - `pub struct GitDiffHandle` + `pub struct GitDiffFocus { pub thread_id: Option<String>, pub base: Option<String>, pub note: Option<String> }`
  - `GitDiffHandle::broadcast(&self, focus: GitDiffFocus)`、`GitDiffHandle::subscribe(&self) -> broadcast::Receiver<GitDiffFocus>`
  - `pub struct ShowGitDiffTool`
  - 通知：`{ "method": "ui/gitDiff/focus", "params": { "threadId": string|null, "base": string|null, "note": string|null } }`

- [ ] **Step 1: 写失败测试**

创建 `git_diff_tool.rs`：

```rust
//! 「把 Git Diff 视图推到用户眼前」的句柄与工具。
//!
//! 与 `theme_tool.rs` 同构：工具只负责广播一个焦点意图，真正渲染是前端的事；
//! app-server 的 watcher 把广播翻译成 `ui/gitDiff/focus` 通知扇出。diff 文本本身
//! 一律由 git 计算，工具不生成、不传递 diff。

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::broadcast;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

/// 一次「请用户看 diff」的意图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiffFocus {
    /// 哪个会话的视图；`None` = 客户端按当前会话处理。
    pub thread_id: Option<String>,
    /// 显式指定的比较基准；`None` = 客户端用默认（分叉点）规则。
    pub base: Option<String>,
    /// 面向用户的说明，随通知带过去。
    pub note: Option<String>,
}

#[derive(Clone)]
pub struct GitDiffHandle {
    tx: broadcast::Sender<GitDiffFocus>,
}

impl GitDiffHandle {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(16);
        Self { tx }
    }

    pub fn broadcast(&self, focus: GitDiffFocus) {
        let _ = self.tx.send(focus);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<GitDiffFocus> {
        self.tx.subscribe()
    }
}

impl Default for GitDiffHandle {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ShowGitDiffTool {
    handle: GitDiffHandle,
    thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ShowGitDiffArgs {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

impl ShowGitDiffTool {
    /// `thread_id` 绑定时，焦点推给该会话；未绑定时前端按当前会话处理。
    pub fn new(handle: GitDiffHandle, thread_id: Option<String>) -> Self {
        Self { handle, thread_id }
    }
}

#[async_trait::async_trait]
impl Tool for ShowGitDiffTool {
    fn name(&self) -> &str {
        "show_git_diff"
    }

    fn description(&self) -> &str {
        "Open the desktop app's Git Diff tab for this conversation, showing the change since the branch forked. Use when a review of the code you changed would help the user. Pass `base` only when you know the intended comparison (e.g. \"origin/main\"); omit it to use the default merge-base with the repository's default branch."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "base": {
                    "type": "string",
                    "description": "Git ref to compare against (branch, tag, or sha). Omit to let the app use the repository default branch."
                },
                "note": {
                    "type": "string",
                    "description": "One short sentence shown to the user above the diff."
                }
            }
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: ShowGitDiffArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let base = parsed.base.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        self.handle.broadcast(GitDiffFocus {
            thread_id: self.thread_id.clone(),
            base: base.clone(),
            note: parsed.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
        });
        match base {
            Some(b) => ToolResult::text(format!("showing git diff against {b}")),
            None => ToolResult::text("showing git diff against the repository default branch"),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin { name: "ui".to_string() },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn broadcasting_an_explicit_base_reports_it_back() {
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(handle.clone(), Some("thread-1".into()));
        let result = tool.call(serde_json::json!({"base": "origin/main", "note": "review"})).await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(focus.base.as_deref(), Some("origin/main"));
        assert_eq!(focus.note.as_deref(), Some("review"));
    }

    #[tokio::test]
    async fn omitting_base_still_broadcasts() {
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(handle.clone(), None);
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.base, None);
        assert_eq!(focus.thread_id, None);
    }

    #[test]
    fn tool_advertises_its_name_and_schema() {
        let tool = ShowGitDiffTool::new(GitDiffHandle::new(), None);
        assert_eq!(tool.name(), "show_git_diff");
        assert!(tool.schema()["properties"]["base"].is_object());
    }
}
```

在 `protocol.rs` 的通知枚举里，紧邻 `UiSettingsUpdated` 加入：

```rust
    /// 模型（`show_git_diff` 工具）请求桌面端聚焦 Git Diff 视图。
    #[serde(rename = "ui/gitDiff/focus", rename_all = "camelCase")]
    UiGitDiffFocus {
        thread_id: Option<String>,
        base: Option<String>,
        note: Option<String>,
    },
```

并在 `Notification::thread_key` 的匹配里，把 `UiGitDiffFocus { .. }` 归入**带 thread 的帧**（若 `thread_id` 为 `None` 则返回 `None`）：

```rust
            Notification::UiGitDiffFocus { thread_id, .. } => thread_id.as_deref(),
```

> 若既有 `thread_key` 是 `match` 单一返回 `Some/None` 的形态，按 `thread_id.as_deref()` 适配即可。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-app-server git_diff_tool 2>&1 | tail -30`
Expected: 编译失败（`git_diff_tool` 未在 `lib.rs` 声明）。

- [ ] **Step 3: 接线（lib.rs / server.rs）**

`lib.rs` 加入：

```rust
pub mod git_diff_tool;
```

`server.rs`：

1）`RuntimeAttachments` 增加字段（与 `theme` 并列）：

```rust
    /// Git Diff 焦点句柄：`show_git_diff` 工具持它广播，watcher 翻译成
    /// `ui/gitDiff/focus` 通知。每个 thread 的工具与 watcher 共用同一实例。
    pub(crate) git_diff: crate::git_diff_tool::GitDiffHandle,
```

2）在 `serve` 里、`pump_theme_notifications` 旁起 Git Diff watcher：

```rust
    // Git Diff 焦点 → ui/gitDiff/focus。与主题守望者同构：Lagged 继续，
    // 只有 Closed 才结束。
    tokio::spawn(pump_git_diff_notifications(
        git_diff_handle.subscribe(),
        Arc::clone(&hub),
    ));
```

其中 `git_diff_handle` 来自 `attachments` 解构（在 `let RuntimeAttachments { ... } = attachments;` 里加入 `git_diff: git_diff_handle,`）。并在 `serve` 的同文件处新增：

```rust
async fn pump_git_diff_notifications(
    mut rx: tokio::sync::broadcast::Receiver<crate::git_diff_tool::GitDiffFocus>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    loop {
        let focus = match rx.recv().await {
            Ok(focus) => focus,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                tracing::warn!(missed, "git diff watcher lagged; continuing");
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        let n = Notification::UiGitDiffFocus {
            thread_id: focus.thread_id,
            base: focus.base,
            note: focus.note,
        };
        if write_notification(&hub, &n).await.is_err() {
            return;
        }
    }
}
```

3）`build_runtime_tooling` 增加参数并在 `register_theme_tool` 旁注册（该函数已有 `thread_id`）：

```rust
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
) -> Result<RuntimeTooling, String> {
    ...
    register_theme_tool(&mut registry, theme);
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
    )));
    ...
}
```

4）`build_runtime_tooling` 加参数并注册（该函数已有 `thread_id`，工具据此绑定会话）：

```rust
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
) -> Result<RuntimeTooling, String> {
    ...
    register_theme_tool(&mut registry, theme);
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
    )));
    ...
}
```

5）**把 `git_diff` 句柄沿着 `theme` 的同一条路径传下去**：凡是当前携带 `theme`
的地方，都要同样携带 `git_diff`。逐一对照（用 `grep -n 'theme' server.rs` 定位）：

- `RuntimeAttachments` 加字段 `pub(crate) git_diff: crate::git_diff_tool::GitDiffHandle`（见上）。
- `serve` 的 `let RuntimeAttachments { ... } = attachments;` 解构里加 `git_diff: git_diff_handle,`。
- `serve` 里 `theme_handle` 旁加 `let git_diff_handle = git_diff_handle.clone();`（watcher 用）。
- `start_thread_core`（`server.rs:5672`）加参数 `git_diff: &crate::git_diff_tool::GitDiffHandle`，并传给 `attach_delegation`。
- `attach_delegation`（`server.rs:253`）加参数 `git_diff: &crate::git_diff_tool::GitDiffHandle`，并传给 `build_runtime_tooling`。
- `card_scheduler_tick`（`server.rs:5375`）与其调用点（主循环 `card_ticker.tick()` 分支）加该参数。
- `ServeLauncher` 结构体加字段 `git_diff: crate::git_diff_tool::GitDiffHandle`，`launch_inner`（`server.rs:5509`）把它传给 `start_thread_core`；其构造点（`run` / `serve_stdio` 里装配 `ServeLauncher` 的地方）从 `attachments` 取句柄填入。
- 主循环 `thread/start` 分支（`server.rs:2732` 附近）与任何其它 `start_thread_core(` 调用点，一并补参。

6）所有生产构造 `RuntimeAttachments` 的地方（`run` / `serve_stdio` / `serve_stdio_with_relay` 的装配点）加入：

```rust
        git_diff: crate::git_diff_tool::GitDiffHandle::new(),
```

**测试夹具**：`Harness`（`server.rs:8427` 附近的 `RuntimeAttachments { ... }`）也要加 `git_diff: crate::git_diff_tool::GitDiffHandle::new(),`。`build_runtime_tooling` 的 3 处测试调用（`:7594` / `:7671` / `:7712`）各补一个 `crate::git_diff_tool::GitDiffHandle::new()` 实参。委派降级路径（非 git 目录）**不**经过 `build_runtime_tooling`，故那里没有 `show_git_diff` —— 这与 `set_theme` 的既有行为一致（那条路径的 registry 由 `registry_with_theme_tool` 构造，只补主题工具）；skill 的「工具不可用时自行跑 git」降级说明正是为此。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-app-server git_diff_tool 2>&1 | tail -30 && cargo test -p yi-agent-app-server 2>&1 | tail -5`
Expected: 新增 3 个测试 PASS，且该 crate 原有测试全绿（证明调用点改动无回归）。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff_tool.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/lib.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): let the model focus the git diff view via show_git_diff"
```

---

### Task 4: 系统 skill `git-diff-review`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-skills/src/assets/git-diff-review/SKILL.md`
- Test: `yi-agent-rs/crates/yi-agent-skills/src/system.rs`（在既有测试模块加一条）

**Interfaces:**
- Consumes: 无。
- Produces: 打包进 `assets/` 的系统 skill，开机安装到 `~/.yi-agent/skills/.system/git-diff-review/SKILL.md`。

- [ ] **Step 1: 写失败测试**

在 `system.rs` 测试模块加入：

```rust
    #[test]
    fn installs_the_git_diff_review_skill() {
        let tmp = tempfile::tempdir().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let path = tmp.path().join("git-diff-review/SKILL.md");
        assert!(path.is_file());
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("show_git_diff"), "skill must name the tool");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-skills installs_the_git_diff_review_skill 2>&1 | tail -20`
Expected: FAIL（文件不存在）。注：`include_dir!` 是编译期嵌入，新目录需触发重编；`cargo` 会自动处理。

- [ ] **Step 3: 写 skill**

创建 `assets/git-diff-review/SKILL.md`：

```markdown
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
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-skills 2>&1 | tail -20`
Expected: 新测试 PASS，其余全绿。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/assets/git-diff-review/SKILL.md \
        yi-agent-rs/crates/yi-agent-skills/src/system.rs
git commit -m "feat(skills): add git-diff-review system skill"
```

---

### Task 5: 前端 diff 解析器 + 协议类型

**Files:**
- Create: `desktop/src/lib/gitDiff.ts`
- Create: `desktop/src/lib/gitDiff.test.ts`
- Modify: `desktop/src/lib/protocol.ts`（新增 `ThreadDiffResult` 等类型与通知分支）

**Interfaces:**
- Consumes: 无。
- Produces:
  - `export interface DiffLine { kind: "add" | "del" | "ctx"; oldNo: number | null; newNo: number | null; text: string }`
  - `export interface Hunk { header: string; lines: DiffLine[] }`
  - `export interface FileDiff { path: string; status: string; additions: number; deletions: number; binary: boolean; hunks: Hunk[] }`
  - `export function parseUnifiedDiff(text: string): FileDiff[]`
  - `interface CommitInfo { sha: string; short: string; subject: string; author: string; timestamp: number }`
  - `interface FileStat { path: string; status: string; additions: number; deletions: number; binary: boolean }`
  - `interface ThreadDiffResult { base: string | null; baseKind: string; mergeBase: string | null; commits: CommitInfo[]; files: FileStat[]; unifiedDiff: string; truncated: boolean }`
  - `interface DiffTextResult { unifiedDiff: string; truncated: boolean }`

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/lib/gitDiff.test.ts`：

```ts
import { describe, it, expect } from "vitest";
import { parseUnifiedDiff } from "./gitDiff";

describe("parseUnifiedDiff", () => {
  it("splits files and assigns old/new line numbers", () => {
    const text = [
      "diff --git a/a.txt b/a.txt",
      "index 111..222 100644",
      "--- a/a.txt",
      "+++ b/a.txt",
      "@@ -1,3 +1,3 @@",
      " one",
      "-two",
      "+TWO",
      " three",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files).toHaveLength(1);
    expect(files[0].path).toBe("a.txt");
    expect(files[0].hunks[0].lines).toEqual([
      { kind: "ctx", oldNo: 1, newNo: 1, text: "one" },
      { kind: "del", oldNo: 2, newNo: null, text: "two" },
      { kind: "add", oldNo: null, newNo: 2, text: "TWO" },
      { kind: "ctx", oldNo: 3, newNo: 3, text: "three" },
    ]);
  });

  it("marks new files and ignores the no-newline marker", () => {
    const text = [
      "diff --git a/n.txt b/n.txt",
      "new file mode 100644",
      "--- /dev/null",
      "+++ b/n.txt",
      "@@ -0,0 +1,1 @@",
      "+hi",
      "\\ No newline at end of file",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files[0].status).toBe("A");
    expect(files[0].additions).toBe(1);
    expect(files[0].hunks[0].lines).toEqual([
      { kind: "add", oldNo: null, newNo: 1, text: "hi" },
    ]);
  });

  it("marks binary files and tolerates an empty diff", () => {
    expect(parseUnifiedDiff("")).toEqual([]);
    const text = [
      "diff --git a/img.png b/img.png",
      "Binary files a/img.png and b/img.png differ",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files[0].binary).toBe(true);
    expect(files[0].hunks).toEqual([]);
  });

  it("parses multiple files in one diff", () => {
    const text = [
      "diff --git a/a b/a",
      "--- a/a",
      "+++ b/a",
      "@@ -1 +1 @@",
      "-x",
      "+y",
      "diff --git a/b b/b",
      "--- a/b",
      "+++ b/b",
      "@@ -1 +1 @@",
      "-p",
      "+q",
      "",
    ].join("\n");
    const files = parseUnifiedDiff(text);
    expect(files.map((f) => f.path)).toEqual(["a", "b"]);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run（在 `desktop/`）: `npm test -- gitDiff 2>&1 | tail -20`
Expected: FAIL，`parseUnifiedDiff` 未导出。

- [ ] **Step 3: 实现解析器与类型**

创建 `desktop/src/lib/gitDiff.ts`：

```ts
/**
 * unified diff → 结构化文件列表。
 *
 * 纯函数：把 git 输出的文本折成可渲染的 `FileDiff[]`。渲染层只消费结构，
 * 不重新解析文本，故解析规则可脱离 DOM 单测。
 */

export interface DiffLine {
  kind: "add" | "del" | "ctx";
  oldNo: number | null;
  newNo: number | null;
  text: string;
}

export interface Hunk {
  header: string;
  lines: DiffLine[];
}

export interface FileDiff {
  path: string;
  status: string;
  additions: number;
  deletions: number;
  binary: boolean;
  hunks: Hunk[];
}

/** 从 `diff --git a/x b/y` 里取新路径；新文件取 `+++ b/<path>`。 */
function pathFromGitLine(line: string): string | null {
  const m = /^diff --git a\/(.+) b\/(.+)$/.exec(line);
  return m ? m[2] : null;
}

export function parseUnifiedDiff(text: string): FileDiff[] {
  const files: FileDiff[] = [];
  let current: FileDiff | null = null;
  let hunk: Hunk | null = null;
  let oldNo = 0;
  let newNo = 0;

  const startFile = (path: string) => {
    current = { path, status: "M", additions: 0, deletions: 0, binary: false, hunks: [] };
    files.push(current);
    hunk = null;
  };

  for (const raw of text.split("\n")) {
    if (raw.startsWith("diff --git ")) {
      const path = pathFromGitLine(raw);
      if (path) startFile(path);
      continue;
    }
    if (!current) continue;

    if (raw.startsWith("new file mode")) {
      current.status = "A";
      continue;
    }
    if (raw.startsWith("deleted file mode")) {
      current.status = "D";
      continue;
    }
    if (raw.startsWith("rename from ") || raw.startsWith("rename to ")) {
      current.status = "R";
      continue;
    }
    if (raw.startsWith("Binary files ") || raw.startsWith("GIT binary patch")) {
      current.binary = true;
      continue;
    }
    if (raw.startsWith("+++ ")) {
      // 用 +++ 的新路径补正（对 /dev/null 应保持 new file 语义，跳过）。
      const p = raw.slice(4).trim();
      if (p !== "/dev/null" && current.hunks.length === 0) current.path = p.replace(/^b\//, "");
      continue;
    }
    if (raw.startsWith("--- ") || raw.startsWith("index ") || raw.startsWith("similarity ")) {
      continue;
    }
    if (raw.startsWith("@@")) {
      const m = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
      oldNo = m ? Number(m[1]) : 0;
      newNo = m ? Number(m[2]) : 0;
      hunk = { header: raw, lines: [] };
      current.hunks.push(hunk);
      continue;
    }
    if (raw.startsWith("\\ No newline")) continue;

    if (hunk === null) continue;
    const marker = raw[0];
    const body = raw.length > 0 ? raw.slice(1) : "";
    if (marker === "+") {
      hunk.lines.push({ kind: "add", oldNo: null, newNo: newNo++, text: body });
      current.additions++;
    } else if (marker === "-") {
      hunk.lines.push({ kind: "del", oldNo: oldNo++, newNo: null, text: body });
      current.deletions++;
    } else if (marker === " ") {
      hunk.lines.push({ kind: "ctx", oldNo: oldNo++, newNo: newNo++, text: body });
    } else if (raw === "") {
      // 尾随空行，忽略。
    }
  }
  // 二进制文件时不保留空 hunk 的伪造头。
  for (const f of files) if (f.binary) f.hunks = [];
  return files;
}
```

在 `desktop/src/lib/protocol.ts` 追加类型，并扩展 `Notification` 联合：

```ts
export interface CommitInfo {
  sha: string;
  short: string;
  subject: string;
  author: string;
  timestamp: number;
}

export interface FileStat {
  path: string;
  status: string;
  additions: number;
  deletions: number;
  binary: boolean;
}

/** `thread/diff/read` 的默认响应。 */
export interface ThreadDiffResult {
  base: string | null;
  baseKind: string;
  mergeBase: string | null;
  commits: CommitInfo[];
  files: FileStat[];
  unifiedDiff: string;
  truncated: boolean;
}

/** `thread/diff/read` 的 `commit` / `path` 分支响应。 */
export interface DiffTextResult {
  unifiedDiff: string;
  truncated: boolean;
}
```

在 `Notification` 联合里加入：

```ts
  | {
      method: "ui/gitDiff/focus";
      params: { threadId: string | null; base: string | null; note: string | null };
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run（在 `desktop/`）: `npm test -- gitDiff 2>&1 | tail -20`
Expected: 4 个 `parseUnifiedDiff` 测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/gitDiff.ts desktop/src/lib/gitDiff.test.ts desktop/src/lib/protocol.ts
git commit -m "feat(desktop): parse unified diffs and type the thread diff RPC"
```

---

### Task 6: `GitDiffView` 组件

**Files:**
- Create: `desktop/src/components/GitDiffView.tsx`
- Create: `desktop/src/components/GitDiffView.test.tsx`

**Interfaces:**
- Consumes: `parseUnifiedDiff` / `FileDiff`（Task 5）、`ThreadDiffResult` / `CommitInfo` / `FileStat`（Task 5）。
- Produces: `export function GitDiffView(props)`，props：
  - `diff: ThreadDiffResult | null`
  - `loading: boolean`
  - `error: string | null`
  - `activeCommit: { sha: string; text: string } | null`
  - `onRefresh: () => void`
  - `onOpenCommit: (sha: string) => void`
  - `onCloseCommit: () => void`
  - `note: string | null`

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/GitDiffView.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, vi, afterEach } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { GitDiffView } from "./GitDiffView";
import type { ThreadDiffResult } from "../lib/protocol";

afterEach(cleanup);

const diff: ThreadDiffResult = {
  base: "origin/main",
  baseKind: "origin-default",
  mergeBase: "abc1234",
  commits: [
    { sha: "c1", short: "c1", subject: "add feature", author: "T", timestamp: 1700000000 },
  ],
  files: [
    { path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false },
    { path: "n.txt", status: "A", additions: 1, deletions: 0, binary: false },
  ],
  unifiedDiff: [
    "diff --git a/a.txt b/a.txt",
    "--- a/a.txt",
    "+++ b/a.txt",
    "@@ -1 +1 @@",
    "-old",
    "+new",
    "",
  ].join("\n"),
  truncated: false,
};

const baseProps = {
  loading: false,
  error: null,
  activeCommit: null,
  onRefresh: () => {},
  onOpenCommit: () => {},
  onCloseCommit: () => {},
  note: null,
};

describe("GitDiffView", () => {
  it("summarises the base, commits and files", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    expect(screen.getByText(/origin\/main/)).toBeTruthy();
    expect(screen.getByText("add feature")).toBeTruthy();
    expect(screen.getByText("a.txt")).toBeTruthy();
  });

  it("renders hunks by default and collapses them on click", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    // a.txt 是第 0 个文件，落在 DEFAULT_EXPANDED（=3）内，默认应已展开。
    expect(screen.getByText("new")).toBeTruthy();
    expect(screen.getByText("old")).toBeTruthy();
    // 点一下折叠它：hunk 文本随之消失。
    fireEvent.click(screen.getByRole("button", { name: /a\.txt/ }));
    expect(screen.queryByText("new")).toBeNull();
  });

  it("calls onOpenCommit with the clicked sha", () => {
    const onOpenCommit = vi.fn();
    render(<GitDiffView {...baseProps} diff={diff} onOpenCommit={onOpenCommit} />);
    fireEvent.click(screen.getByText("add feature"));
    expect(onOpenCommit).toHaveBeenCalledWith("c1");
  });

  it("shows explicit empty and error states", () => {
    render(<GitDiffView {...baseProps} diff={null} error="not a git repository" />);
    expect(screen.getByText(/not a git repository/)).toBeTruthy();
    render(<GitDiffView {...baseProps} diff={{ ...diff, files: [], commits: [], unifiedDiff: "" }} />);
    expect(screen.getByText(/没有改动/)).toBeTruthy();
  });

  it("flags a truncated diff", () => {
    render(<GitDiffView {...baseProps} diff={{ ...diff, truncated: true }} />);
    expect(screen.getByText(/截断/)).toBeTruthy();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run（在 `desktop/`）: `npm test -- GitDiffView 2>&1 | tail -20`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 实现组件**

创建 `desktop/src/components/GitDiffView.tsx`：

```tsx
/**
 * Git Diff 视图：接近 GitHub 的「Commits + Files changed」。
 *
 * 只消费结构化数据，不自己解析 diff 文本（解析在 `lib/gitDiff.ts`，有独立单测）。
 * 默认只展开前若干个文件：长 diff 一次渲染成百上千行会拖慢整块面板。
 */
import { memo, useState } from "react";
import type { CommitInfo, FileStat, ThreadDiffResult } from "../lib/protocol";
import { parseUnifiedDiff, type FileDiff } from "../lib/gitDiff";

const DEFAULT_EXPANDED = 3;

function statusBadge(status: string): string {
  switch (status) {
    case "A":
      return "text-emerald-400";
    case "D":
      return "text-red-400";
    case "R":
      return "text-amber-400";
    default:
      return "text-fg-muted";
  }
}

function relativeTime(ts: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (secs < 60) return "刚刚";
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} 小时前`;
  return `${Math.floor(secs / 86400)} 天前`;
}

const FileBlock = memo(function FileBlock({ file, open }: { file: FileDiff; open: boolean }) {
  return (
    <div className="mt-1 overflow-hidden">
      {open &&
        file.hunks.map((hunk, hi) => (
          <div key={hi} className="font-mono text-xs">
            <div className="bg-panel px-2 py-0.5 text-fg-faint">{hunk.header}</div>
            {hunk.lines.map((line, li) => (
              <div
                key={li}
                className={
                  line.kind === "add"
                    ? "bg-emerald-950/40 text-emerald-200"
                    : line.kind === "del"
                      ? "bg-red-950/40 text-red-200"
                      : "text-fg-subtle"
                }
              >
                <span className="inline-block w-10 select-none pr-2 text-right text-fg-faint">
                  {line.oldNo ?? ""}
                </span>
                <span className="inline-block w-10 select-none pr-2 text-right text-fg-faint">
                  {line.newNo ?? ""}
                </span>
                <span className="whitespace-pre-wrap">
                  {line.kind === "add" ? "+" : line.kind === "del" ? "-" : " "}
                  {line.text}
                </span>
              </div>
            ))}
          </div>
        ))}
    </div>
  );
});

export function GitDiffView({
  diff,
  loading,
  error,
  activeCommit,
  onRefresh,
  onOpenCommit,
  onCloseCommit,
  note,
}: {
  diff: ThreadDiffResult | null;
  loading: boolean;
  error: string | null;
  activeCommit: { sha: string; text: string } | null;
  onRefresh: () => void;
  onOpenCommit: (sha: string) => void;
  onCloseCommit: () => void;
  note: string | null;
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const parsed = diff ? parseUnifiedDiff(diff.unifiedDiff) : [];

  if (activeCommit) {
    const commitFiles = parseUnifiedDiff(activeCommit.text);
    return (
      <div className="flex min-h-0 flex-1 flex-col px-3 py-2">
        <div className="flex items-center gap-2">
          <button type="button" className="text-xs text-sky-300 hover:text-sky-200" onClick={onCloseCommit}>
            ← 返回全部改动
          </button>
          <span className="font-mono text-xs text-fg-muted">{activeCommit.sha.slice(0, 8)}</span>
        </div>
        <div className="min-h-0 flex-1 overflow-y-auto">
          {commitFiles.map((f) => (
            <FileBlock key={f.path} file={f} open />
          ))}
        </div>
      </div>
    );
  }

  if (error) {
    return <p className="px-3 py-2 text-sm text-red-300">{error}</p>;
  }
  if (loading && !diff) {
    return <p className="px-3 py-2 text-sm text-fg-subtle">正在计算 diff…</p>;
  }
  if (!diff) {
    return <p className="px-3 py-2 text-sm text-fg-subtle">尚无 diff 数据</p>;
  }
  const empty = diff.files.length === 0 && diff.commits.length === 0;
  if (empty) {
    return (
      <div className="px-3 py-2 text-sm text-fg-muted">
        <p>没有改动</p>
        <p className="mt-1 text-xs text-fg-faint">
          基准：{diff.base ?? "（无默认分支，仅看工作区）"}
        </p>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center justify-between px-3 py-2">
        <span className="truncate text-xs text-fg-muted">
          对比 {diff.base ?? "工作区"}
          {diff.mergeBase && ` · merge-base ${diff.mergeBase.slice(0, 7)}`}
          {` · ${diff.commits.length} commits · ${diff.files.length} files`}
        </span>
        <button type="button" className="text-xs text-fg-subtle hover:text-fg-muted" onClick={onRefresh}>
          刷新
        </button>
      </div>
      {note && <p className="px-3 pb-1 text-xs text-sky-300">{note}</p>}
      {diff.truncated && (
        <p className="px-3 pb-1 text-xs text-amber-300">diff 过长已截断，仅显示前一部分</p>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-3 pb-3">
        {diff.commits.length > 0 && (
          <div className="mb-2">
            <p className="text-xs text-fg-subtle">Commits</p>
            <ul>
              {diff.commits.map((c: CommitInfo) => (
                <li key={c.sha}>
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 rounded px-1 py-0.5 text-left hover:bg-raised/50"
                    onClick={() => onOpenCommit(c.sha)}
                  >
                    <span className="font-mono text-xs text-fg-faint">{c.short}</span>
                    <span className="min-w-0 flex-1 truncate text-sm text-fg">{c.subject}</span>
                    <span className="text-xs text-fg-faint">{relativeTime(c.timestamp)}</span>
                  </button>
                </li>
              ))}
            </ul>
          </div>
        )}

        <p className="text-xs text-fg-subtle">Files changed</p>
        <ul>
          {diff.files.map((f: FileStat, i: number) => {
            const open = expanded[f.path] ?? i < DEFAULT_EXPANDED;
            return (
              <li key={f.path} className="border-b border-line/50">
                <button
                  type="button"
                  className="flex w-full items-center gap-2 px-1 py-1 text-left"
                  aria-expanded={open}
                  onClick={() => setExpanded((e) => ({ ...e, [f.path]: !open }))}
                >
                  <span className={`w-4 text-center font-mono text-xs ${statusBadge(f.status)}`}>
                    {f.status}
                  </span>
                  <span className="min-w-0 flex-1 truncate font-mono text-xs text-fg">{f.path}</span>
                  <span className="text-xs text-emerald-400">+{f.additions}</span>
                  <span className="text-xs text-red-400">-{f.deletions}</span>
                </button>
                {f.binary ? (
                  <p className="px-1 pb-1 text-xs text-fg-faint">二进制文件，不显示内容</p>
                ) : (
                  <FileBlock
                    file={parsed.find((p) => p.path === f.path) ?? { ...f, hunks: [] }}
                    open={open}
                  />
                )}
              </li>
            );
          })}
        </ul>
      </div>
    </div>
  );
}
```

- [ ] **Step 4: 运行测试确认通过**

Run（在 `desktop/`）: `npm test -- GitDiffView 2>&1 | tail -20`
Expected: 5 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/GitDiffView.tsx desktop/src/components/GitDiffView.test.tsx
git commit -m "feat(desktop): render a GitHub-style git diff view"
```

---

### Task 7: `ThreadDetailPanel` 常驻面板 + Tab + App 接线

**Files:**
- Create: `desktop/src/components/ThreadDetailPanel.tsx`
- Create: `desktop/src/components/ThreadDetailPanel.test.tsx`
- Modify: `desktop/src/App.tsx`（面板可见性、Tab 状态、通知处理、RPC 调用；替换原来直接渲染 `SubagentTrace` 的片段）

**Interfaces:**
- Consumes: `SubagentTrace`（既有）、`GitDiffView`（Task 6）、`ThreadDiffResult` / `DiffTextResult`（Task 5）。
- Produces: `export function ThreadDetailPanel(props)`，props：
  - `tab: "trace" | "diff"`
  - `onTabChange: (t: "trace" | "diff") => void`
  - `onClose: () => void`
  - `traceProps: SubagentTraceProps | null`（选中子 agent 时为轨迹 props，否则 null）
  - `diffProps: GitDiffViewProps`（Task 6 的 props）

- [ ] **Step 1: 写失败测试**

创建 `desktop/src/components/ThreadDetailPanel.test.tsx`：

```tsx
/** @vitest-environment jsdom */
import { describe, it, expect, afterEach, vi } from "vitest";
import { render, fireEvent, cleanup, screen } from "@testing-library/react";
import { ThreadDetailPanel } from "./ThreadDetailPanel";

afterEach(cleanup);

const traceProps = {
  taskId: "t1",
  row: null,
  children: [],
  rows: [],
  onClose: () => {},
  onDrill: () => {},
  onMessage: async () => {},
  onCancel: async () => ({ confirmationToken: "x", taskIds: [], expiresInSecs: 30 }),
  onConfirmCancel: async () => {},
};

const diffProps = {
  diff: null,
  loading: false,
  error: null,
  activeCommit: null,
  onRefresh: () => {},
  onOpenCommit: () => {},
  onCloseCommit: () => {},
  note: null,
};

describe("ThreadDetailPanel", () => {
  it("renders both tabs and switches on click", () => {
    const onTabChange = vi.fn();
    render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={onTabChange}
        onClose={() => {}}
        traceProps={traceProps}
        diffProps={diffProps}
      />,
    );
    expect(screen.getByRole("tab", { name: "轨迹" })).toBeTruthy();
    fireEvent.click(screen.getByRole("tab", { name: "Git Diff" }));
    expect(onTabChange).toHaveBeenCalledWith("diff");
  });

  it("shows an empty trace state when no subagent is selected", () => {
    render(
      <ThreadDetailPanel
        tab="trace"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={diffProps}
      />,
    );
    expect(screen.getByText(/未选择子 agent/)).toBeTruthy();
  });

  it("shows the diff body on the diff tab", () => {
    render(
      <ThreadDetailPanel
        tab="diff"
        onTabChange={() => {}}
        onClose={() => {}}
        traceProps={null}
        diffProps={{ ...diffProps, error: "boom" }}
      />,
    );
    expect(screen.getByText("boom")).toBeTruthy();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run（在 `desktop/`）: `npm test -- ThreadDetailPanel 2>&1 | tail -20`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 实现面板**

创建 `desktop/src/components/ThreadDetailPanel.tsx`：

```tsx
/**
 * 主对话下方的常驻详情面板：轨迹与 Git Diff 两个 Tab。
 *
 * 面板常驻（而非只在选中子 agent 时出现），因为 Git Diff 是会话级的、与选了谁无关；
 * 「轨迹」Tab 在未选子 agent 时给空态。Tab 可点，也可由外部（点卡片 / 模型推送）
 * 切换：`tab` 是受控 prop，切换由 `onTabChange` 上抛。
 */
import { SubagentTrace } from "./SubagentTrace";
import { GitDiffView } from "./GitDiffView";
import type { ComponentProps } from "react";

type Tab = "trace" | "diff";

export function ThreadDetailPanel({
  tab,
  onTabChange,
  onClose,
  traceProps,
  diffProps,
}: {
  tab: Tab;
  onTabChange: (t: Tab) => void;
  onClose: () => void;
  traceProps: ComponentProps<typeof SubagentTrace> | null;
  diffProps: ComponentProps<typeof GitDiffView>;
}) {
  const tabClass = (t: Tab) =>
    `rounded px-2 py-0.5 text-xs ${
      tab === t ? "bg-panel text-fg" : "text-fg-muted hover:text-fg"
    }`;
  return (
    <section
      aria-label="会话详情"
      className="flex min-h-0 flex-1 flex-col border-t border-line bg-surface"
    >
      <div className="flex items-center justify-between px-3 py-1.5">
        <div role="tablist" className="flex items-center gap-1">
          <button type="button" role="tab" aria-selected={tab === "trace"} className={tabClass("trace")} onClick={() => onTabChange("trace")}>
            轨迹
          </button>
          <button type="button" role="tab" aria-selected={tab === "diff"} className={tabClass("diff")} onClick={() => onTabChange("diff")}>
            Git Diff
          </button>
        </div>
        <button type="button" aria-label="关闭详情" className="text-xs text-fg-subtle hover:text-fg-muted" onClick={onClose}>
          关闭
        </button>
      </div>

      <div className="flex min-h-0 flex-1 flex-col">
        {tab === "trace" ? (
          traceProps ? (
            <SubagentTrace {...traceProps} />
          ) : (
            <p className="px-3 py-2 text-sm text-fg-subtle">未选择子 agent</p>
          )
        ) : (
          <GitDiffView {...diffProps} />
        )}
      </div>
    </section>
  );
}
```

> 注意：`SubagentTrace` 目前自带 `<section>` 外框与页眉（含「关闭」按钮）。为纳入 Tab，请把 `SubagentTrace` 的最外层 `<section>` 与页眉里的「关闭」按钮**去掉**（改由 `ThreadDetailPanel` 提供外框与关闭），保留其主体（状态行、展开/收起轨迹、子任务、发消息/取消）。若改动会影响 `SubagentTrace.test.tsx` 的既有断言（例如它断言 `关闭详情` 按钮），同步调整该测试为对着 `ThreadDetailPanel` 断言，勿削弱覆盖。

- [ ] **Step 4: App 接线**

在 `desktop/src/App.tsx`：

0）补齐 import（`ThreadDiffResult` / `DiffTextResult` 来自 Task 5 的 `./lib/protocol`；`ThreadDetailPanel` 来自新组件）：

```tsx
import { ThreadDetailPanel } from "./components/ThreadDetailPanel";
import type { ThreadDiffResult, DiffTextResult } from "./lib/protocol";
```

1）新增状态：

```tsx
  const [panelOpen, setPanelOpen] = useState(false);
  const [detailTab, setDetailTab] = useState<"trace" | "diff">("trace");
  const [threadDiff, setThreadDiff] = useState<ThreadDiffResult | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [diffBase, setDiffBase] = useState<string | null>(null);
  const [diffNote, setDiffNote] = useState<string | null>(null);
  const [activeCommit, setActiveCommit] = useState<{ sha: string; text: string } | null>(null);
```

2）加载函数：

```tsx
  const loadThreadDiff = async (threadId: string, base?: string | null) => {
    const c = clientRef.current;
    if (!c) return;
    setDiffLoading(true);
    setDiffError(null);
    setActiveCommit(null);
    try {
      const r = await c.request<ThreadDiffResult>("thread/diff/read", {
        threadId,
        ...(base ? { base } : {}),
      });
      setThreadDiff(r);
    } catch (e) {
      setDiffError(formatError(e));
    } finally {
      setDiffLoading(false);
    }
  };

  const openCommit = async (threadId: string, sha: string) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      const r = await c.request<DiffTextResult>("thread/diff/read", { threadId, commit: sha });
      setActiveCommit({ sha, text: r.unifiedDiff });
    } catch (e) {
      setDiffError(formatError(e));
    }
  };
```

3）通知处理（在 `wireClient` 的 `onNotification` 里，`ui/settings/updated` 分支旁）：

```tsx
        if (n.method === "ui/gitDiff/focus") {
          const target = n.params.threadId ?? currentIdRef.current;
          if (target) {
            setDetailTab("diff");
            setPanelOpen(true);
            setDiffNote(n.params.note ?? null);
            setDiffBase(n.params.base ?? null);
            void loadThreadDiff(target, n.params.base);
          }
          return;
        }
```

> 若现有 `App.tsx` 没有 `currentIdRef`，加一个 `const currentIdRef = useRef<string | null>(null);` 并在 `useEffect` 里随 `currentId` 更新，或直接读渲染期的 `currentId`（通知回调在 `wireClient` 闭包里，可能读到旧值，故用 ref 更稳）。

4）点卡片时切回轨迹并打开面板（修改 `openDetail`）：

```tsx
  const openDetail = async (threadId: string, taskId: string) => {
    setPanelOpen(true);
    setDetailTab("trace");
    ...  // 既有 trace read/watch 逻辑不变
  };
```

5）状态栏图标改为开合面板（`aria-expanded={panelOpen}`、`onClick={() => setPanelOpen((v) => !v)}`），并让面板在 `panelOpen` 时渲染：

```tsx
          {currentId && panelOpen && !isMobile && (
            <ThreadDetailPanel
              tab={detailTab}
              onTabChange={(t) => {
                setDetailTab(t);
                if (t === "diff") void loadThreadDiff(currentId, diffBase);
              }}
              onClose={() => {
                setPanelOpen(false);
                void closeDetail(currentId);
              }}
              traceProps={
                openSubagent
                  ? {
                      taskId: openSubagent,
                      row: railStore.get(currentId).find((r) => r.taskId === openSubagent) ?? null,
                      children: childrenOf(railStore.get(currentId), openSubagent),
                      rows: traceRows,
                      onClose: () => void closeDetail(currentId),
                      onDrill: (taskId) => void openDetail(currentId, taskId),
                      onMessage: async (taskId, message) => {
                        await clientRef.current?.request("agent/message", { threadId: currentId, taskId, message });
                      },
                      onCancel: async (taskId) =>
                        await clientRef.current!.request<AgentCancelPreviewResult>("agent/cancel/preview", { threadId: currentId, taskId }),
                      onConfirmCancel: async (taskId, token) => {
                        await clientRef.current?.request("agent/cancel", { threadId: currentId, taskId, confirmationToken: token });
                      },
                    }
                  : null
              }
              diffProps={{
                diff: threadDiff,
                loading: diffLoading,
                error: diffError,
                activeCommit,
                note: diffNote,
                onRefresh: () => void loadThreadDiff(currentId, diffBase),
                onOpenCommit: (sha) => void openCommit(currentId, sha),
                onCloseCommit: () => setActiveCommit(null),
              }}
            />
          )}
```

删除原来直接渲染 `<SubagentTrace .../>` 的整段（`App.tsx:1387-1418` 附近），其职责已由面板承接。

6）切会话时清空 diff 状态（在 `selectThread` 里）：

```tsx
    setPanelOpen(false);
    setThreadDiff(null);
    setActiveCommit(null);
    setDiffError(null);
```

- [ ] **Step 5: 运行前端测试**

Run（在 `desktop/`）: `npm test 2>&1 | tail -30`
Expected: 新增的 `ThreadDetailPanel` 3 个测试 PASS；`SubagentTrace.test.tsx` 与 `App.test.tsx` 若有断言因外框迁移而失败，按其真实语义更新断言后全绿。

- [ ] **Step 6: 类型检查与构建**

Run（在 `desktop/`）: `npx tsc --noEmit 2>&1 | tail -20`
Expected: 无类型错误。

- [ ] **Step 7: 提交**

```bash
git add desktop/src/components/ThreadDetailPanel.tsx \
        desktop/src/components/ThreadDetailPanel.test.tsx \
        desktop/src/components/SubagentTrace.tsx \
        desktop/src/components/SubagentTrace.test.tsx \
        desktop/src/App.tsx
git commit -m "feat(desktop): make the detail panel a tabbed trace + git diff surface"
```

---

### Task 8: 端到端核对（工具 → 通知 → 前端）

**Files:**
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（新增一条集成测试，验证工具注册在委派路径上）

**Interfaces:**
- Consumes: Task 3 的工具、Task 4 的 skill、Task 5–7 的前端。
- Produces: 一条「该会话的工具集里确实有 `show_git_diff`，且它注册在委派路径」的断言，锁住接线不回退。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 测试模块，紧邻 `a_git_project_gets_the_delegation_tools`（`server.rs:7580` 附近）加——沿用同一条真实路径（`attach_project_runtime` → `build_runtime_tooling` → `registry.names()`）：

```rust
/// 会话的工具集里必须有 `show_git_diff`：模型没有它就无法把 diff 推到用户眼前。
///
/// 与 `a_git_project_gets_the_delegation_tools` 走同一条 setup，故两者一起失败时
/// 说明是共享接线断了，而不是单点回归。
#[test]
fn a_git_project_gets_the_show_git_diff_tool() {
    let repo = tempfile::TempDir::new().unwrap();
    let runtime = tempfile::TempDir::new().unwrap();
    init_git_repo(repo.path());
    let mut cfg = test_config();
    cfg.workdir = repo.path().to_path_buf();

    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(&cfg, runtime.path().to_path_buf())
            .expect("a clean git repo must attach"),
    );
    let binding =
        RuntimeBinding::managed(&cfg, runtime.path().to_path_buf(), Arc::clone(&attached));
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());

    let names = build_runtime_tooling(
        &cfg,
        &root,
        "thread-test",
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
    )
    .expect("tooling")
    .registry
    .names();

    assert!(
        names.contains(&"show_git_diff".to_string()),
        "an attached root must expose show_git_diff, got {names:?}"
    );
}
```

> 依赖：本测试要求 Task 3 已完成（`build_runtime_tooling` 已加 `git_diff` 参数）。若按顺序执行则应已满足。

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-app-server show_git_diff_tool 2>&1 | tail -20`
Expected: FAIL（工具尚未出现在该路径）。

- [ ] **Step 3: 确认/修正接线**

核对 `build_runtime_tooling` 里已注册 `ShowGitDiffTool`（Task 3 Step 3）。若测试仍失败，说明委派降级路径未注册：按 Task 3 的说明确保 `attach_delegation` 无论 attach 成功与否都经过同一条注册逻辑。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p yi-agent-app-server 2>&1 | tail -5 && cargo test -p yi-agent-skills 2>&1 | tail -5`
Expected: 全绿。

- [ ] **Step 5: 全量验证**

Run: `cargo test -p yi-agent-app-server -p yi-agent-skills 2>&1 | tail -5`
Run（在 `desktop/`）: `npm test 2>&1 | tail -5 && npx tsc --noEmit 2>&1 | tail -5`
Expected: 全部通过。

- [ ] **Step 6: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "test(app-server): lock show_git_diff into the conversation tool set"
```

---

## 自查记录

**Spec 覆盖：**

| Spec 章节 | 落地任务 |
| --- | --- |
| §4 架构 / 数据流 | Task 1–8 全链路 |
| §5 后端 diff 计算（base / merge-base / commits / files / 未跟踪 / 截断 / RPC） | Task 1、Task 2 |
| §6 GitHub 式渲染（汇总 / Commits / Files / 空错截断态） | Task 5、Task 6 |
| §7 面板 Tab 化（常驻 / 三路切换 / 轨迹空态 / 复用状态栏入口） | Task 7 |
| §8 工具 + 系统 skill | Task 3、Task 4、Task 8 |
| §9 错误处理与测试 | 各任务测试步骤 + Task 8 全量验证 |

**占位符扫描：** 无 TBD/TODO；每个代码步骤都给了可照抄的代码或明确文件路径。

**类型一致性：** Rust `ThreadDiff`（camelCase serde）↔ TS `ThreadDiffResult`；`CommitInfo` / `FileStat` 两侧同名同形；`GitDiffView`/`ThreadDetailPanel` 的 props 名与 Task 7 的传参一致；RPC `thread/diff/read` 的 `commit` / `path` 分支都返回 `{ unifiedDiff, truncated }`，与 TS `DiffTextResult` 一致。
