//! 在 thread 的工作目录上现算 git diff（只读，不落盘）。
//!
//! 比较基准是**分支分叉点**：`git merge-base HEAD <默认分支>`。这对应 GitHub
//! PR 的算法，也是「这个会话从分叉点以来改了什么」最贴切的语义。默认分支判定
//! 见 [`resolve_base`]；全不可用时退化为「仅工作区改动」。

use std::path::Path;
use std::time::Duration;

pub const MAX_DIFF_BYTES: usize = 256 * 1024;

/// 视图命令的超时。与 bash 的「预期运行时长」无关：这里是展示用命令，
/// 只读且应当很快；挂住比失败更糟（面板会一直转）。
pub const VIEW_TIMEOUT: Duration = Duration::from_secs(10);

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

/// 模型投递到前端的一帧视图。
///
/// 与 [`ThreadDiff`] 的区别：`ThreadDiff` 是「结构化取数」的结果（软件决定看什么），
/// 本帧是「模型给命令」的结果（模型决定看什么）。两者并存：前者服务单文件懒加载，
/// 后者服务模型驱动的展示。
///
/// 片段化字段名（`asKind` / `exitCode` / `timedOut` / `baseKind`）是前端（Task 7/8）
/// 逐字消费的线上契约，**不得**改名。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewFrame {
    /// 执行的命令；`None` = 结构化默认（会话锚点 diff）。
    pub command: Option<String>,
    /// `diff` | `log` | `text`。
    pub as_kind: String,
    /// `None` = 没跑起来（超时 / 沙箱不可用）。
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub note: Option<String>,
    /// 结构化默认时的实际基准。
    pub base: Option<String>,
    /// `session` | `explicit` | `origin-head` | `origin-default` | `local-default` | `worktree-only`
    pub base_kind: String,
}

/// 按内容判定渲染原语。判定不出即 `text`（保底永不失手）。
///
/// 判定只看内容，不看模型的声明：声明与内容不符时（Task 7 的 `resolveKind` 同样
/// 会回退），渲染必须落在 `text` 上——把任意文本当 diff 渲染只会把面板画坏。
pub fn classify(stdout: &str) -> &'static str {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return "text";
    };
    // `diff --git ` 允许出现在首行之外的任何位置（多段 diff 之间可能有噪声行）；
    // `--- ` 只认首行，否则普通文本里的分隔线会被误判成 diff。
    if stdout.contains("diff --git ") || first.starts_with("--- ") {
        return "diff";
    }
    // `git log --oneline` 一类：短 sha + 空格 + 非空描述。
    let looks_like_log = first.len() > 8
        && first.chars().take(7).all(|c| c.is_ascii_hexdigit())
        && first.chars().nth(7) == Some(' ')
        && first[8..].trim().len() > 0;
    if looks_like_log {
        return "log";
    }
    "text"
}

/// 结构化帧：不跑模型命令，直接按 `explicit`/`anchor` 算 diff。
///
/// `explicit` 与 `anchor` **必须分开传**：两者都走 `resolve_base`，但得到的
/// `base_kind` 不同（`explicit` vs `session`）。若把显式基准塞进 `anchor` 槽，
/// 帧会把「模型指定的基准」标成「本次会话起点」——回执就成了 D3 那种不实之词。
pub fn structured_frame(repo: &Path, explicit: Option<&str>, anchor: Option<&str>) -> ViewFrame {
    let d = thread_diff(repo, explicit, anchor);
    ViewFrame {
        command: None,
        as_kind: "diff".to_string(),
        exit_code: Some(0),
        stdout: d.unified_diff,
        stderr: String::new(),
        truncated: d.truncated,
        timed_out: false,
        note: None,
        base: d.base,
        base_kind: d.base_kind,
    }
}

/// 把模型给的命令跑成帧。`as_hint` 是模型声明的原语（`None` = 自动判定）。
///
/// 声明合法时信声明、不覆盖（模型可能知道内容的样子，如带 `--stat` 的 diff）；
/// 声明非法或缺失时才按内容判定。
pub async fn command_frame(
    repo: &Path,
    command: &str,
    as_hint: Option<&str>,
    note: Option<String>,
) -> ViewFrame {
    let out = run_view_command(repo, command).await;
    let as_kind = match as_hint {
        Some("diff") | Some("log") | Some("text") => as_hint.unwrap_or_default().to_string(),
        _ => classify(&out.stdout).to_string(),
    };
    ViewFrame {
        command: Some(command.to_string()),
        as_kind,
        exit_code: out.exit_code,
        stdout: out.stdout,
        stderr: out.stderr,
        truncated: out.truncated,
        timed_out: out.timed_out,
        note,
        base: None,
        // 命令帧没有「结构化基准」这一说：基准由模型在命令里自己给出。
        base_kind: String::new(),
    }
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

/// 取会话锚点：当前 `HEAD`。非 git 仓库返回 `None`。
pub fn session_anchor(repo: &Path) -> Option<String> {
    git_ok(repo, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string())
}

/// 判定比较基准。显式传入优先；其次是会话锚点；再次按候选顺序找默认分支；都没有则 `None`。
///
/// 锚点不可达（rebase / gc 掉）时**不**降级为「仅工作区」——那会让 diff 突然只剩
/// 未提交改动，比报错更难理解；此处直接落到默认分支候选链。
pub fn resolve_base(
    repo: &Path,
    explicit: Option<&str>,
    anchor: Option<&str>,
) -> (Option<String>, &'static str) {
    if let Some(name) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        if ref_exists(repo, name) {
            return (Some(name.to_string()), "explicit");
        }
    }
    if let Some(name) = anchor.map(str::trim).filter(|s| !s.is_empty()) {
        if ref_exists(repo, name) {
            return (Some(name.to_string()), "session");
        }
    }
    // 以下为既有候选链，逐字保留。
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
        &[
            "log",
            "--format=%H%x1f%h%x1f%s%x1f%an%x1f%at%x1e",
            &format!("{merge_base}..HEAD"),
        ],
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
            Some(CommitInfo {
                sha,
                short,
                subject,
                author,
                timestamp,
            })
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
    git_ok(
        repo,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--full-name",
            "-z",
        ],
    )
    .map(|raw| {
        raw.split('\0')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect()
    })
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
        let additions = std::fs::read(&full)
            .map(|b| b.iter().filter(|&&c| c == b'\n').count() as u64)
            .unwrap_or(0);
        let binary = is_binary(&full);
        out.push(FileStat {
            path,
            status: "A".into(),
            additions,
            deletions: 0,
            binary,
        });
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
            let status = if deletions == 0 && additions > 0 {
                "A"
            } else if additions == 0 && deletions > 0 {
                "D"
            } else {
                "M"
            };
            (status.to_string(), path.to_string())
        };
        out.push(FileStat {
            path,
            status,
            additions,
            deletions,
            binary,
        });
    }
    out
}

fn is_binary(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// 未跟踪文件合成一份「整文件新增」的 unified diff（避免触碰用户索引）。
///
/// `paths` 与 `root` 都是仓库根相对，故合成头 `diff --git a/.. b/..` 与 git 自己
/// 产出的头共用同一基准。
fn untracked_diff(root: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        let lines: Vec<&str> = content.lines().collect();
        out.push_str(&format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
        ));
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

/// 一次视图命令的结果。退出码是 `None` 表示**没跑起来**（如沙箱不可用）。
#[derive(Debug, Clone)]
pub struct ViewCommandOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
}

/// 该校验该字符串能否作为 git rev / 范围表达式使用。
///
/// 单个 ref 走 `rev-parse --verify --quiet`；范围（含 `..` / `...`）两侧分别校验
/// ——因为 `rev-parse --verify` **不认**范围表达式（实测 `A..B` 直接失败）。
/// 以 `-` 开头的一律拒绝：那是选项，不是 rev，放过去就给了 `--output=` 之类
/// 的注入面。
pub fn valid_rev(repo: &Path, spec: &str) -> bool {
    let spec = spec.trim();
    if spec.is_empty() || spec.starts_with('-') {
        return false;
    }
    // `...` 先于 `..` 匹配，否则三点会被拆成两段错误的结果。
    if let Some((left, right)) = spec.split_once("...").or_else(|| spec.split_once("..")) {
        return ref_exists(repo, left.trim()) && ref_exists(repo, right.trim());
    }
    ref_exists(repo, spec)
}

/// 在只读沙箱内跑一条视图命令。
///
/// 两道防护互补，不是冗余：
/// - 沙箱挡住**任何**实际写入（含 shell 重定向 `> f`，那是 `--end-of-options` 管不到的）；
/// - `bash -lc` 前不做拼接，命令整条交给 `sh -c`，故 `--end-of-options` 的职责
///   落在命令**内部**（模型自己写的 git 参数由 git 解析，写不进去因为沙箱）。
///
/// 沙箱不可用（如 Linux 无 bwrap）当作失败返回，而不是无沙箱裸跑。
pub async fn run_view_command(repo: &Path, command: &str) -> ViewCommandOutput {
    let sandbox =
        yi_agent_tools::SandboxPolicy::new(yi_agent_tools::SandboxMode::ReadOnly, repo, Vec::new());
    let (program, args) = match sandbox.command(command, repo) {
        Ok(v) => v,
        Err(e) => {
            return ViewCommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("sandbox unavailable: {e}"),
                truncated: false,
                timed_out: false,
            };
        }
    };
    let child = match tokio::process::Command::new(&program)
        .args(&args)
        .current_dir(repo)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ViewCommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to spawn: {e}"),
                truncated: false,
                timed_out: false,
            };
        }
    };

    let output = match tokio::time::timeout(VIEW_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return ViewCommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("failed to read output: {e}"),
                truncated: false,
                timed_out: false,
            };
        }
        Err(_) => {
            // `kill_on_drop` 在 drop 时只回收直接 spawn 的那个子进程
            // （`sandbox-exec` / `sh`），不保证杀掉它再派生出的后代；
            // 这里只需如实报 timeout。
            return ViewCommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("command timed out after {}s", VIEW_TIMEOUT.as_secs()),
                truncated: false,
                timed_out: true,
            };
        }
    };

    let stdout_raw = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr_raw = String::from_utf8_lossy(&output.stderr).to_string();
    let (stdout, truncated) = truncate(stdout_raw);
    ViewCommandOutput {
        exit_code: output.status.code(),
        stdout,
        stderr: stderr_raw,
        truncated,
        timed_out: false,
    }
}

pub fn thread_diff(repo: &Path, explicit_base: Option<&str>, anchor: Option<&str>) -> ThreadDiff {
    let root = repo_root(repo);
    let (base, base_kind) = resolve_base(repo, explicit_base, anchor);
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

/// 单个提交的 diff。
///
/// `--first-parent`：对**合并提交**这一条是必需的——`git show --format= <merge>`
/// 按设计输出为空（实测 0 字节），面板会是一片空白；`--first-parent` 给出一份
/// 连贯的「这次合并带进来了什么」。不采用 `-m`：它把两个父提交的 diff 拼接在
/// 一起（同一改动列两遍），读起来像重复列举。对非合并提交，两者与裸 `show`
/// 完全等价。
pub fn commit_diff(repo: &Path, sha: &str) -> (String, bool) {
    let raw = git_ok(
        repo,
        &["show", "--no-color", "--first-parent", "--format=", sha],
    )
    .unwrap_or_default();
    truncate(raw)
}

/// 单文件 diff。
///
/// 未跟踪文件不在任何 `git diff` 的输出里，需单独合成——否则 `path` 分支对它
/// 返回 0 字节，前端懒加载出来是空白（D1）。
pub fn file_diff(repo: &Path, merge_base: Option<&str>, path: &str) -> (String, bool) {
    // `None` 分支与 `files` / `thread_diff` 保持一致：`HEAD` 覆盖已暂存 + 未暂存，
    // 裸 `git diff` 会漏掉已暂存的改动。
    let raw = match merge_base {
        Some(mb) => git_ok(repo, &["diff", "--no-color", mb, "--", path]).unwrap_or_default(),
        None => git_ok(repo, &["diff", "--no-color", "HEAD", "--", path]).unwrap_or_default(),
    };
    // 只有「diff 为空」且该路径**确在未跟踪清单里**时才合成「整文件新增」。
    // 「已跟踪但无改动」也返回空，但它不在未跟踪清单里，故不会被捏造出内容。
    if raw.trim().is_empty() && untracked_paths(repo).iter().any(|p| p == path) {
        return truncate(untracked_diff(&repo_root(repo), &[path.to_string()]));
    }
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
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-q", "-m", "init"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        dir
    }

    #[test]
    fn worktree_only_when_no_default_branch_exists() {
        let dir = repo();
        let (base, kind) = resolve_base(dir.path(), None, None);
        assert_eq!(base, None);
        assert_eq!(kind, "worktree-only");
    }

    #[test]
    fn explicit_base_wins_and_reports_kind() {
        let dir = repo();
        let (base, kind) = resolve_base(dir.path(), Some("trunk"), None);
        assert_eq!(base.as_deref(), Some("trunk"));
        assert_eq!(kind, "explicit");
    }

    #[test]
    fn untracked_and_uncommitted_are_both_included() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap(); // 未提交改动
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap(); // 未跟踪
        let d = thread_diff(dir.path(), Some("trunk"), None);
        // 在 trunk 上 merge-base == HEAD（自己与自己），diff 只含工作区
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"a.txt"),
            "tracked edit must appear: {paths:?}"
        );
        assert!(
            paths.contains(&"new.txt"),
            "untracked file must appear: {paths:?}"
        );
        assert!(d.unified_diff.contains("+two"), "diff: {}", d.unified_diff);
        assert!(
            d.unified_diff.contains("+hello"),
            "diff: {}",
            d.unified_diff
        );
    }

    /// 回归 #1：无默认分支（`worktree-only`）时也必须报出已跟踪文件的未提交改动。
    ///
    /// 修复前 `files` / `unified_diff` 都只在 `Some(mb)` 分支里跑 git diff，
    /// 于是 `worktree-only` 只剩未跟踪文件，已跟踪文件的未提交编辑整体丢失。
    #[test]
    fn worktree_only_includes_tracked_uncommitted_edits() {
        let dir = repo(); // 分支 `trunk`，无 remote ⇒ resolve_base 给出 worktree-only
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap(); // 已跟踪文件的未提交改动
        let d = thread_diff(dir.path(), None, None);
        assert_eq!(d.base_kind, "worktree-only");
        assert_eq!(d.base, None);
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"a.txt"),
            "tracked uncommitted edit must appear: {paths:?}"
        );
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(
            (a.additions, a.deletions),
            (1, 0),
            "numstat must be read: {a:?}"
        );
        assert!(
            d.unified_diff.contains("+two"),
            "tracked edit must be in the diff: {}",
            d.unified_diff
        );
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
        assert!(
            std::process::Command::new("git")
                .args(["add", "a.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        // 只暂存、不再改动：裸 `git diff` 在这里输出为空。
        let d = thread_diff(dir.path(), None, None);
        assert_eq!(d.base_kind, "worktree-only");
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"a.txt"),
            "staged-only edit must appear: {paths:?}"
        );
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(
            (a.additions, a.deletions),
            (1, 0),
            "numstat must be read: {a:?}"
        );
        assert!(
            d.unified_diff.contains("+two"),
            "staged-only edit must be in the diff: {}",
            d.unified_diff
        );
    }

    /// 回归 #1c：已暂存的**新文件**不能被漏掉。
    ///
    /// 它已在索引里，故 `ls-files --others` 不会报它；又因「索引 == 工作区」，
    /// 裸 `git diff` 也不会报它——`git diff HEAD` 是唯一能看见它的范围。
    #[test]
    fn worktree_only_includes_staged_new_files() {
        let dir = repo();
        std::fs::write(dir.path().join("n.txt"), "brand new\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "n.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let d = thread_diff(dir.path(), None, None);
        assert_eq!(d.base_kind, "worktree-only");
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            ["n.txt"],
            "staged new file must appear exactly once: {paths:?}"
        );
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
        assert!(
            std::process::Command::new("git")
                .args(["add", "a.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap(); // 暂存后再改
        let d = thread_diff(dir.path(), None, None);
        let a = d.files.iter().find(|f| f.path == "a.txt").unwrap();
        assert_eq!(
            (a.additions, a.deletions),
            (2, 0),
            "both deltas must be counted: {a:?}"
        );
        assert!(
            d.unified_diff.contains("+two"),
            "staged delta missing: {}",
            d.unified_diff
        );
        assert!(
            d.unified_diff.contains("+three"),
            "unstaged delta missing: {}",
            d.unified_diff
        );
    }

    /// 回归 #1e：`file_diff` 的 `None` 分支与 `thread_diff` 同源，同样必须看见已暂存改动。
    #[test]
    fn file_diff_without_base_includes_staged_only_edits() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "a.txt"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let (diff, truncated) = file_diff(dir.path(), None, "a.txt");
        assert!(!truncated);
        assert!(
            diff.contains("+two"),
            "staged-only edit must be in file_diff: {diff}"
        );
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
            assert!(
                std::process::Command::new("git")
                    .args(&args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(sub.join("t.txt"), "t\nedit\n").unwrap(); // 已提交文件的未提交改动
        std::fs::write(sub.join("u.txt"), "hello\n").unwrap(); // 子目录里的未跟踪文件

        let d = thread_diff(&sub, None, None); // thread 的 cwd 是子目录（Task 2 的调用方式）
        let paths: Vec<_> = d.files.iter().map(|f| f.path.as_str()).collect();
        assert!(
            paths.contains(&"sub/t.txt"),
            "tracked edit must be root-relative: {paths:?}"
        );
        assert!(
            paths.contains(&"sub/u.txt"),
            "untracked must be root-relative: {paths:?}"
        );
        assert!(
            !paths.contains(&"u.txt") && !paths.contains(&"t.txt"),
            "no cwd-relative paths: {paths:?}"
        );
        assert!(
            d.unified_diff
                .contains("diff --git a/sub/u.txt b/sub/u.txt"),
            "synthesized header must be root-relative: {}",
            d.unified_diff
        );
        assert!(
            d.unified_diff.contains("+hello"),
            "untracked content must be in the diff: {}",
            d.unified_diff
        );
    }

    #[test]
    fn commits_are_the_branch_introduced_ones() {
        let dir = repo();
        // 造一个分叉：从 trunk 起分支并加一个提交
        assert!(
            std::process::Command::new("git")
                .args(["checkout", "-q", "-b", "feature"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join("a.txt"), "feature\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-qam", "feature work"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        // 以 trunk 为 base 时，feature 引入的提交应被列出
        let d = thread_diff(dir.path(), Some("trunk"), None);
        assert_eq!(d.commits.len(), 1, "commits: {:?}", d.commits);
        assert_eq!(d.commits[0].subject, "feature work");
        assert!(!d.commits[0].short.is_empty());
    }

    #[test]
    fn anchor_beats_the_default_branch_candidates() {
        let dir = repo();
        std::fs::create_dir_all(dir.path().join(".git/refs/remotes/origin")).unwrap();
        // 造一个「默认分支」候选，确认锚点仍然压过它。
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(
            dir.path().join(".git/refs/remotes/origin/main"),
            format!("{}\n", head.trim()),
        )
        .unwrap();
        let (base, kind) = resolve_base(dir.path(), None, Some("trunk"));
        assert_eq!(base.as_deref(), Some("trunk"), "anchor must win");
        assert_eq!(kind, "session");
    }

    #[test]
    fn explicit_beats_the_anchor() {
        let dir = repo();
        let (base, kind) = resolve_base(dir.path(), Some("HEAD"), Some("trunk"));
        assert_eq!(base.as_deref(), Some("HEAD"));
        assert_eq!(kind, "explicit", "explicit still wins over the anchor");
    }

    #[test]
    fn missing_anchor_falls_back_to_the_existing_chain() {
        let dir = repo();
        // 无锚点、无默认分支：落回 worktree-only（旧会话的行为不变）。
        let (base, kind) = resolve_base(dir.path(), None, None);
        assert_eq!(base, None);
        assert_eq!(kind, "worktree-only");
    }

    /// 锚点不可达（rebase / gc 掉）时必须落到默认分支候选链——**不得**降级为
    /// 「仅工作区」，否则 diff 会突然只剩未提交改动，比报错更难理解。
    ///
    /// 前提是仓库里真的有候选可落：故先造一个 `origin/main`。否则本测试只能断言
    /// 「没有候选时是 worktree-only」，而那正是「误降级」与「正确落链」共有的输出，
    /// 断言不出被测性质。
    #[test]
    fn unreachable_anchor_falls_through_to_the_default_branch_candidate() {
        let dir = repo();
        std::fs::create_dir_all(dir.path().join(".git/refs/remotes/origin")).unwrap();
        // 造一个默认分支候选，让「落链」这一步真正可观测。
        let head = git_ok(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(
            dir.path().join(".git/refs/remotes/origin/main"),
            format!("{}\n", head.trim()),
        )
        .unwrap();
        let (base, kind) = resolve_base(dir.path(), None, Some("deadbeefdeadbeef"));
        assert_eq!(
            base.as_deref(),
            Some("origin/main"),
            "unreachable anchor must fall through to the candidate chain"
        );
        assert_eq!(kind, "origin-default");
    }

    #[test]
    fn session_anchor_reads_head() {
        let dir = repo();
        let sha = session_anchor(dir.path()).expect("a git repo has an anchor");
        // 不拿 git 的同一条命令自比（那只是 f(x) == f(x)，实现返回错的 ref 也照样绿）；
        // 改断言返回值自身的性质：一个完整的 40 位十六进制 sha。
        assert_eq!(sha.len(), 40, "anchor must be a full sha: {sha:?}");
        assert!(
            sha.bytes().all(|b| b.is_ascii_hexdigit()),
            "anchor must be hex: {sha:?}"
        );
        // 再确认 git 能把它当 committish 解析出来——下游 `merge_base` 正是这么用它。
        assert!(
            git_ok(
                dir.path(),
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{sha}^{{commit}}")
                ]
            )
            .is_some(),
            "anchor must resolve as a committish: {sha:?}"
        );
    }

    #[test]
    fn session_anchor_is_none_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(session_anchor(dir.path()), None);
    }

    #[test]
    fn valid_rev_accepts_refs_and_ranges_and_rejects_junk() {
        let dir = repo();
        // `HEAD~1` 要有两个提交才存在，而 `repo()` 只有一个（别的测试依赖那个
        // 「只有一条 log」的前提，故不改 fixture，只在本测试里补一个提交）。
        // 少了这一步，左侧就解不开，也就测不出「范围两侧都要校验」这一性质。
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        assert!(git_ok(dir.path(), &["add", "."]).is_some());
        assert!(git_ok(dir.path(), &["commit", "-q", "-m", "second"]).is_some());
        assert!(valid_rev(dir.path(), "HEAD"));
        assert!(valid_rev(dir.path(), "trunk"));
        assert!(valid_rev(dir.path(), "HEAD~1..HEAD"));
        assert!(valid_rev(dir.path(), "trunk...HEAD"));
        assert!(
            !valid_rev(dir.path(), "bogus..HEAD"),
            "unresolvable side must fail"
        );
        assert!(!valid_rev(dir.path(), ""), "empty is not a rev");
        assert!(
            !valid_rev(dir.path(), "--output=/tmp/x"),
            "options are not revs"
        );
    }

    // 以下四个测试会真正把命令交给沙箱去跑，故只在 macOS 上编译。
    // Linux 上 ReadOnly 走 `bwrap`，而 CI runner（ubuntu-latest）默认没装
    // bubblewrap：`platform_command` 直接返回 `SandboxUnavailable`，
    // `run_view_command` fail closed（`exit_code: None`、`timed_out: false`）、
    // 一个字节都不执行。在那里断言「跑出什么」是没有意义的——既测不到超时，
    // 也测不到写入被拒。这与仓库既有做法一致：`yi-agent-tools` 里沙箱测试都是
    // `#[cfg(target_os = "macos")]`（见 `shell/bash.rs`、`tests/sandbox_readonly.rs`）。
    // 注意 `valid_rev_*` 不在此列：它只走纯 git，任何平台都得跑。
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn view_command_rejects_an_output_option_that_would_write() {
        let dir = repo();
        let probe = dir.path().join("probe.txt");
        let out = run_view_command(
            dir.path(),
            &format!("git diff --no-color --numstat --output={}", probe.display()),
        )
        .await;
        assert!(
            !probe.exists(),
            "sandbox/end-of-options must prevent the write"
        );
        assert!(
            out.exit_code != Some(0),
            "it must not report success: {out:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn view_command_reports_read_output() {
        let dir = repo();
        let out = run_view_command(dir.path(), "git log --oneline -1").await;
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("init"), "stdout: {}", out.stdout);
        assert!(!out.timed_out);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn view_command_denies_writes_in_read_only_mode() {
        let dir = repo();
        // 写 .git/index 必须被拒（ReadOnly 沙箱）。
        let out = run_view_command(dir.path(), "git add -A").await;
        assert!(out.exit_code != Some(0), "git add must be denied: {out:?}");
        // 仓库状态不变。
        assert_eq!(
            git_ok(dir.path(), &["log", "--oneline"])
                .unwrap()
                .lines()
                .count(),
            1
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn view_command_times_out_instead_of_hanging() {
        let dir = repo();
        let out = run_view_command(dir.path(), "sleep 60").await;
        assert!(
            out.timed_out,
            "sleep must be killed by the timeout: {out:?}"
        );
    }

    #[test]
    fn classify_detects_diff_log_and_text() {
        assert_eq!(classify("diff --git a/x b/x\n--- a/x\n"), "diff");
        assert_eq!(
            classify("abc1234 fix a thing\ndef5678 add another\n"),
            "log"
        );
        assert_eq!(classify("random words\n"), "text");
        assert_eq!(classify(""), "text");
    }

    #[test]
    fn structured_frame_reports_the_session_anchor() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let f = structured_frame(dir.path(), None, Some("trunk"));
        assert_eq!(f.command, None, "structured default has no command");
        assert_eq!(f.as_kind, "diff");
        assert_eq!(f.base_kind, "session");
        assert_eq!(f.base.as_deref(), Some("trunk"));
        assert!(f.stdout.contains("+two"), "stdout: {}", f.stdout);
    }

    /// 帧的线协议字段名是前端（Task 7/8）逐字消费的契约：少一个 `asKind`、
    /// 多一个蛇形名都会让面板读空。这里钉死整份驼峰形状。
    #[test]
    fn view_frame_serializes_as_camel_case() {
        let f = ViewFrame {
            command: Some("git log --oneline -1".into()),
            as_kind: "log".into(),
            exit_code: Some(0),
            stdout: "abc1234 x\n".into(),
            stderr: String::new(),
            truncated: false,
            timed_out: true,
            note: Some("n".into()),
            base: Some("trunk".into()),
            base_kind: "session".into(),
        };
        let v = serde_json::to_value(&f).unwrap();
        let obj = v.as_object().unwrap();
        for key in [
            "command",
            "asKind",
            "exitCode",
            "stdout",
            "stderr",
            "truncated",
            "timedOut",
            "note",
            "base",
            "baseKind",
        ] {
            assert!(obj.contains_key(key), "missing `{key}` in {v}");
        }
        assert_eq!(obj.len(), 10, "no extra/renamed keys: {v}");
    }

    /// D2：合并提交的 diff 不能是空白。
    ///
    /// `git show --format= <merge>` 按设计输出 0 字节（实测），面板会是一片空白。
    /// `--first-parent` 给出一份连贯的「这次合并带进来了什么」；`-m` 会把两个父
    /// 提交的 diff 拼在一起（同一改动列两遍），故不采用。
    #[test]
    fn merge_commits_render_a_first_parent_diff() {
        let dir = repo();
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir.path())
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        run(&["checkout", "-q", "-b", "side"]);
        std::fs::write(dir.path().join("b.txt"), "side\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "side work"]);
        run(&["checkout", "-q", "trunk"]);
        std::fs::write(dir.path().join("a.txt"), "main edit\n").unwrap();
        run(&["commit", "-qam", "main work"]);
        run(&["merge", "-q", "--no-ff", "side", "-m", "merge side"]);

        let merge = git_ok(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        let (diff, _) = commit_diff(dir.path(), merge.trim());
        assert!(
            !diff.trim().is_empty(),
            "a merge commit must not render blank (that was the bug)"
        );
        assert!(
            diff.contains("b.txt"),
            "the merged-in change must show: {diff}"
        );
        // `-m` 会把同一改动对两个父各列一遍；`--first-parent` 只列一遍。
        assert_eq!(
            diff.matches("diff --git a/b.txt").count(),
            1,
            "the merge must be rendered against the first parent only: {diff}"
        );
    }

    /// 非合并提交的行为**不得**因加 `--first-parent` 而改变。
    #[test]
    fn non_merge_commits_are_unchanged() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["commit", "-qam", "edit a"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let (diff, _) = commit_diff(dir.path(), "HEAD");
        assert!(
            diff.contains("+two"),
            "non-merge behaviour must not change: {diff}"
        );
    }

    /// D1：未跟踪文件不在任何 `git diff` 的输出里，单文件视图必须自己合成，
    /// 否则点开它是一片空白（`path` 分支返回 0 字节）。
    #[test]
    fn file_diff_covers_untracked_files() {
        let dir = repo();
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        let (diff, _) = file_diff(dir.path(), Some("trunk"), "new.txt");
        assert!(
            diff.contains("+hello"),
            "an untracked file's content must be synthesized (the path branch returned 0 bytes): {diff}"
        );
        // 未跟踪不应被误判成「已跟踪但无改动」的空白：必须带 new file 头。
        assert!(
            diff.contains("new file mode"),
            "the synthesized diff must look like a new file: {diff}"
        );
    }

    /// 合成只针对**未跟踪**路径：已跟踪但无改动的文件仍应返回空，不得凭空捏造。
    #[test]
    fn file_diff_leaves_a_clean_tracked_file_empty() {
        let dir = repo();
        let (diff, _) = file_diff(dir.path(), Some("trunk"), "a.txt");
        assert!(
            diff.trim().is_empty(),
            "a clean tracked file has no diff; nothing may be synthesized: {diff}"
        );
    }
}
