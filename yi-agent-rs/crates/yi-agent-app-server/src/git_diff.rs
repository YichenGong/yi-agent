//! 在 thread 的工作目录上现算 git diff（只读，不落盘）。
//!
//! 比较基准是**分支分叉点**：`git merge-base HEAD <默认分支>`。这对应 GitHub
//! PR 的算法，也是「这个会话从分叉点以来改了什么」最贴切的语义。默认分支判定
//! 见 [`resolve_base`]；全不可用时退化为「仅工作区改动」。

use std::io::Read;
use std::path::Path;
use std::time::Duration;

pub const MAX_DIFF_BYTES: usize = 256 * 1024;

/// 未跟踪文件参与 diff 合成的单文件上限。
///
/// 超过它的文件在清单里标 `binary: true`、正文给一行说明,**不读内容**。
/// 为什么是 1 MiB:`MAX_DIFF_BYTES` 是 256 KiB,一个超过 1 MiB 的未跟踪文件
/// 无论如何都不可能完整出现在最终 diff 里(前面还有 `truncate`);继续读它
/// 只是白白占用时间与内存。前端对 `binary: true` 已有占位渲染
/// (`desktop/src/components/GitDiffView.tsx:187`)。
pub const MAX_UNTRACKED_FILE_BYTES: u64 = 1024 * 1024;

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
        && !first[8..].trim().is_empty();
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

/// `git_ok` 的单次时间上限。
///
/// 与 `VIEW_TIMEOUT` 分开:那个约束「模型给的展示命令」(走沙箱、允许较慢),
/// 这个约束「我们自己发的只读内省命令」(`rev-parse` / `diff` / `log`,
/// 正常都是毫秒级)。给 5s 是因为大仓库首次 `git diff` 可能真慢;但
/// 「`index.lock` 残留」或「仓库在网络挂载点上」应当失败而不是无限等待。
pub const GIT_OK_TIMEOUT: Duration = Duration::from_secs(5);

/// `git_ok` 轮询子进程状态的间隔。git 正常返回是毫秒级,10 ms 只多等一轮。
const GIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

thread_local! {
    /// 本线程最近一次 diff 计算里，是否有 `git_ok` 因超时被放弃。
    ///
    /// 用 thread-local 而不是给 `git_ok` 加参数：它有十余个调用点，散布在
    /// `resolve_base` / `files` / `commits` / `untracked_paths` / `file_diff` /
    /// `commit_diff` 里，逐个加 `&mut bool` 只会把噪声传到每一层。
    /// 一次 diff 计算在单个 blocking 线程里同步跑完，故这一位不会跨请求串。
    static GIT_TIMED_OUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 清掉「本线程有 git 超时」标记。计算入口在开始前调它。
pub fn reset_git_timeout() {
    GIT_TIMED_OUT.with(|c| c.set(false));
}

/// 取走「本线程有 git 超时」标记（读过即清）。
///
/// 为什么必须区分这一位：`git_ok` 的 `None` 同时表示「git 说没有」与
/// 「git 没在 [`GIT_OK_TIMEOUT`] 内返回」。前者是合法的空 diff，后者是失败
/// （典型成因是残留的 `index.lock`）。不区分就会把挂住渲染成「没有改动」。
pub fn take_git_timeout() -> bool {
    GIT_TIMED_OUT.with(|c| c.replace(false))
}

/// 跑一个子进程,在 `timeout` 内返回它的 stdout(成功时),否则 `None`。
///
/// stdout 走**临时文件**而不是管道:管道模式下子进程写满内核缓冲区
/// (macOS 64 KiB)就会阻塞在 write 上,而我们正在轮询 `try_wait`、没有任何人
/// 读管道——这是死锁(子进程等我们读,我们等子进程退出)。`git diff` 的输出
/// 远超 64 KiB 是常态,故管道方案对本项目不可用。
fn run_git_bounded(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Option<String> {
    let out_file = tempfile::NamedTempFile::new().ok()?;
    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::from(out_file.reopen().ok()?))
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                return std::fs::read_to_string(out_file.path()).ok();
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    // 必须 wait:否则留下僵尸进程。
                    let _ = child.wait();
                    // 只有这一支是「超时」：非零退出码与 spawn 失败都是 git
                    // 给出的真实结论，仍保留「没有」的语义。
                    GIT_TIMED_OUT.with(|c| c.set(true));
                    return None;
                }
                std::thread::sleep(GIT_POLL_INTERVAL);
            }
            Err(_) => return None,
        }
    }
}

/// 跑 git 并返回成功时的 stdout 文本。
///
/// 只认 success：`rev-parse --verify` / `merge-base` / `log` 这类命令失败时
/// 本就该当作「没有」，而 diff 类命令的退出码差异由 HTML 生成后的内容决定，
/// 故所有调用点都只需「有内容」或「没有」两态。
///
/// 有界等待见 [`GIT_OK_TIMEOUT`]：无上界时,一个挂住的 git 会把调用方
/// 永久占住(修复前是主循环)。
fn git_ok(directory: &Path, args: &[&str]) -> Option<String> {
    let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    run_git_bounded("git", &owned, directory, GIT_OK_TIMEOUT)
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
        // 一次读取服务两件事：数换行、判二进制。原实现为此各做一次
        // 全量 `std::fs::read`，纯粹是重复 IO。
        let bytes = read_untracked_capped(&full);
        let (additions, binary) = match bytes.as_deref() {
            Some(b) => (
                b.iter().filter(|&&c| c == b'\n').count() as u64,
                looks_binary(b),
            ),
            // 超上限 / 非普通文件 / 读不到：视为二进制，(0 行新增)。
            None => (0, true),
        };
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

/// git 自己的二进制判据：前 8000 字节里出现 NUL。与 `files` 里进清单的
/// `binary` 标志同源，故「清单说它是什么」与「点开后是什么」不会分叉。
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// 读未跟踪文件；超过 [`MAX_UNTRACKED_FILE_BYTES`] 或读失败返回 `None`。
///
/// `None` 的语义 = 「不参与内容合成」：调用方据此标 binary / 跳过正文。
///
/// 两道闸，各管一个失效模式：
/// - 读之前按 `metadata` 挡下超限与非普通文件；
/// - 读的时候再按上限截断（`.take`），故超大文件最多只被读进 1 MiB + 1 字节。
///
/// 只有闸不行：`metadata` 看到的是**那一刻**的长度，文件在 stat 与 read 之间
/// 长大（或本就是个增长中的日志）时 `read` 仍会按新长度全量分配。只有 `.take`
/// 也不行：非普通文件的 `len` 不可信，见下一条。
///
/// 非普通文件（FIFO / 设备 / socket）的 `len` 常为 0，能骗过尺寸闸；而读
/// FIFO 在无写端时会一直阻塞，故与 `rpc/image.rs` 同一条判据直接拒绝。
fn read_untracked_capped(full: &Path) -> Option<Vec<u8>> {
    let meta = std::fs::metadata(full).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(full)
        .ok()?
        .take(MAX_UNTRACKED_FILE_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_UNTRACKED_FILE_BYTES {
        return None;
    }
    Some(buf)
}

/// 未跟踪文件合成一份「整文件新增」的 unified diff（避免触碰用户索引）。
///
/// `paths` 与 `root` 都是仓库根相对，故合成头 `diff --git a/.. b/..` 与 git 自己
/// 产出的头共用同一基准。
fn untracked_diff(root: &Path, paths: &[String]) -> String {
    let mut out = String::new();
    for path in paths {
        let Some(bytes) = read_untracked_capped(&root.join(path)) else {
            // 超上限 / 非普通文件 / 读不到：给一行明确说明，而不是留下一个
            // 空白条目 (D1 的另一半：清单里有、点开却是空的)。措辞不写
            // 「too large」——拒绝的原因也可能是 FIFO/设备，见 `read_untracked_capped`。
            out.push_str(&format!(
                "diff --git a/{path} b/{path}\nnew file mode 100644\n\
                 Binary files /dev/null and b/{path} differ (not shown: too large \
                 or not a regular file)\n"
            ));
            continue;
        };
        out.push_str(&format!(
            "diff --git a/{path} b/{path}\nnew file mode 100644\n"
        ));
        // 二进制按 git 自己的约定给一行说明。此前这里是 `read_to_string` 失败就
        // `continue`，结果是面板把该文件列为 `A`、点开却是空白——D1 的另一半。
        // 判据与 `files` 里的清单标志共用 `looks_binary`，否则会出现「清单标
        // binary、正文却当文本渲染」这类自相矛盾。
        if looks_binary(&bytes) {
            out.push_str(&format!("Binary files /dev/null and b/{path} differ\n"));
            continue;
        }
        // 非 UTF-8 但不是二进制（无 NUL）的罕见文件用有损转换渲染，宁可有乱码
        // 也不留空白。git 自身在这种情况下也是按文本处理的。
        let text = String::from_utf8_lossy(&bytes);
        out.push_str(&format!("--- /dev/null\n+++ b/{path}\n"));
        let lines: Vec<&str> = text.lines().collect();
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
/// `--first-parent`：对**合并提交**是必需的。`git show --format= <merge>` 给出的是
/// 合并提交本身的 diff——干净合并输出为空，有冲突解决的合并则输出**组合 diff**
/// （`diff --cc` / `@@@`）。两者在面板上都等于空白：前者的文本为空，后者没有任何
/// `diff --git` 头（前端的 `parseUnifiedDiff` 也不认 `diff --cc`）。`--first-parent`
/// 给出一份连贯的「这次合并带进来了什么」，且正是解析器认得的普通格式。
/// 不采用 `-m`：它把两个父提交的 diff 拼接在一起（同一改动列两遍，实测头数是
/// `--first-parent` 的三倍），读起来像重复列举。对非合并提交，`--first-parent`
/// 与裸 `show` 输出逐字节相同（实测）。
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

    /// `run_git_bounded` 必须在上界内放弃,并回收子进程。
    ///
    /// 用 `/bin/sh -c 'sleep 600'` 冒充挂住的 git:它确实是「一个不会自己退出
    /// 的子进程」,这正是残留 `index.lock` 或网络挂载点上的 git 的表现。
    /// 不走 PATH 注入,故不会污染并行测试。
    #[test]
    fn run_git_bounded_gives_up_after_the_timeout() {
        let started = std::time::Instant::now();
        let out = run_git_bounded(
            "/bin/sh",
            &["-c".to_string(), "sleep 600".to_string()],
            std::path::Path::new("/tmp"),
            Duration::from_millis(200),
        );
        let elapsed = started.elapsed();
        assert!(out.is_none(), "a hung child must yield None");
        assert!(
            elapsed < Duration::from_secs(5),
            "must give up near the bound, took {elapsed:?}"
        );
    }

    /// 正常返回的子进程要照常给出 stdout,且不受上界影响。
    #[test]
    fn run_git_bounded_returns_stdout_on_success() {
        let out = run_git_bounded(
            "/bin/sh",
            &["-c".to_string(), "printf 'hi\\n'".to_string()],
            std::path::Path::new("/tmp"),
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Some("hi\n"));
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

    /// 未跟踪的**二进制/非 UTF-8** 文件同样不能是空白：面板会把它列为 `A`，
    /// 点开却什么都没有就是 D1 的另一半。按 git 自己的约定给一行说明
    /// （前端 `parseUnifiedDiff` 认得 `Binary files ` 前缀）。
    #[test]
    fn an_untracked_binary_file_is_announced_not_blank() {
        let dir = repo();
        std::fs::write(dir.path().join("bin.dat"), b"a\0b").unwrap();
        let (diff, _) = file_diff(dir.path(), Some("trunk"), "bin.dat");
        assert!(
            !diff.trim().is_empty(),
            "a binary untracked file must not render blank: {diff:?}"
        );
        assert!(
            diff.contains("Binary files"),
            "it must carry git's own binary notice: {diff:?}"
        );
        assert!(
            diff.contains("bin.dat"),
            "the notice must name the file: {diff:?}"
        );
    }

    /// 「清单说它是二进制」与「点开后的正文」必须一致，且都不能是空白。
    ///
    /// 判据若两处不同（清单只看 NUL，正文却把非法 UTF-8 也当二进制），
    /// 就会出现清单标 text、正文说 binary 这类自相矛盾。这里同时钉住两种输入的
    /// 归属：含 NUL 的走二进制说明；非法 UTF-8 但无 NUL 的按文本有损渲染。
    #[test]
    fn the_file_list_and_the_body_agree_on_binary_ness() {
        let dir = repo();
        // 含 NUL：清单标 binary，正文给二进制说明。
        std::fs::write(dir.path().join("nul.dat"), b"a\0b").unwrap();
        // 非法 UTF-8、无 NUL：清单不标 binary，正文按文本有损渲染（不留空白）。
        std::fs::write(dir.path().join("latin.dat"), b"caf\xe9\n").unwrap();

        for (path, binary) in [("nul.dat", true), ("latin.dat", false)] {
            let file = files(dir.path(), dir.path(), Some("trunk"))
                .into_iter()
                .find(|f| f.path == path)
                .unwrap_or_else(|| panic!("{path} must be listed"));
            assert_eq!(file.binary, binary, "list flag for {path}: {file:?}");

            let (diff, _) = file_diff(dir.path(), Some("trunk"), path);
            assert!(
                !diff.trim().is_empty(),
                "{path} must never open onto a blank body: {diff:?}"
            );
            assert_eq!(
                diff.contains("Binary files"),
                binary,
                "body agrees with the list flag for {path}: {diff:?}"
            );
        }
    }

    /// 超过上限的未跟踪文件不读内容:清单标 binary,正文给一行说明。
    ///
    /// 原实现对每个未跟踪文件做三次全量 `std::fs::read`(数换行 / 判二进制 /
    /// 合成 unified diff),没有大小上限。截断只在最后做,故 `MAX_DIFF_BYTES`
    /// 只约束输出、不约束输入——一个 512 MiB 的未跟踪文件会被完整读进内存。
    #[test]
    fn untracked_file_over_the_cap_is_not_read_into_the_diff() {
        let dir = repo();
        let big = dir.path().join("huge.log");
        // 2 MiB,超过 MAX_UNTRACKED_FILE_BYTES(1 MiB)。
        let chunk = "x".repeat(1024 * 1024);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&big).unwrap();
            f.write_all(chunk.as_bytes()).unwrap();
            f.write_all(chunk.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        let d = thread_diff(dir.path(), Some("trunk"), None);
        let stat = d
            .files
            .iter()
            .find(|f| f.path == "huge.log")
            .expect("the oversized untracked file must still be listed");
        assert!(
            stat.binary,
            "an oversized untracked file must be reported as binary, not read as text"
        );
        assert!(
            d.unified_diff
                .contains("not shown: too large or not a regular file"),
            "the body must say why it is not rendered: {}",
            &d.unified_diff[..d.unified_diff.len().min(400)]
        );
        // 正文里绝不能出现那 2 MiB 的内容。
        assert!(
            !d.unified_diff.contains(&"x".repeat(4096)),
            "the file content must not be read"
        );
    }

    /// 超时必须被记下来，且读取即清（否则会污染同线程的下一次计算）。
    ///
    /// 这一条只证明**标记机制**本身：真的让 `git` 挂住需要构造仓库级故障
    /// （如占住 `index.lock`），故 `git_ok` 的端到端超时上报仍无自动化覆盖。
    #[test]
    fn run_git_bounded_records_a_timeout() {
        let dir = tempfile::tempdir().unwrap();
        reset_git_timeout();
        assert!(!take_git_timeout(), "a fresh thread must start clean");

        let out = run_git_bounded(
            "/bin/sh",
            &["-c".to_string(), "sleep 30".to_string()],
            dir.path(),
            Duration::from_millis(100),
        );
        assert!(out.is_none(), "a timed-out command yields no stdout");
        assert!(take_git_timeout(), "the timeout branch must raise the flag");
        assert!(!take_git_timeout(), "taking the flag must clear it");

        // 正常退出不能置位：只有超时才算失败。
        GIT_TIMED_OUT.with(|c| c.set(true));
        reset_git_timeout();
        assert!(!take_git_timeout(), "reset must clear a stale flag");
        assert!(
            run_git_bounded(
                "/bin/sh",
                &["-c".to_string(), "exit 0".to_string()],
                dir.path(),
                Duration::from_secs(5)
            )
            .is_some()
        );
        assert!(
            !take_git_timeout(),
            "a successful run must not raise the flag"
        );
        assert!(
            run_git_bounded(
                "/bin/sh",
                &["-c".to_string(), "exit 3".to_string()],
                dir.path(),
                Duration::from_secs(5)
            )
            .is_none()
        );
        assert!(
            !take_git_timeout(),
            "a non-zero exit is a real failure, not a timeout"
        );
    }

    /// 上限边界：正好 `MAX_UNTRACKED_FILE_BYTES` 字节仍接受，多 1 字节即拒绝。
    ///
    /// 关键是把 `>` 与 `>=` 的区别钉住：上限语义是「可以等于」。
    #[test]
    fn read_untracked_capped_stops_reading_at_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let exact = dir.path().join("exact.txt");
        std::fs::write(&exact, vec![b'x'; MAX_UNTRACKED_FILE_BYTES as usize]).unwrap();
        let got = read_untracked_capped(&exact).expect("exactly at the cap must be accepted");
        assert_eq!(got.len() as u64, MAX_UNTRACKED_FILE_BYTES);

        let over = dir.path().join("over.txt");
        std::fs::write(&over, vec![b'x'; MAX_UNTRACKED_FILE_BYTES as usize + 1]).unwrap();
        assert!(
            read_untracked_capped(&over).is_none(),
            "one byte over the cap must be rejected"
        );
    }

    /// 非普通文件一律拒绝：设备 / FIFO 的 `len` 不可信（常为 0），读 FIFO 在
    /// 无写端时会一直阻塞。
    ///
    /// 承重的是 `/dev/null`，不是目录：目录的 `metadata().len()` 虽也非 0（本机实测
    /// 64），但真正拦下它的是随后的 `read`——macOS 上 `File::open(dir)` 成功而
    /// `read_to_end` 返回 `EISDIR`，`.ok()?` 一样给出 `None`。所以拿目录当样本，
    /// 删掉 `!meta.is_file()` 这道闸测试仍然绿，没有回归价值。
    ///
    /// `/dev/null` 没有这个退路：`size=0`（过尺寸闸）、`crw-rw-rw-`
    /// （`is_file() == false`）、且 `open` 后 `read` 成功返回 0 字节。于是
    /// 旧代码（丢掉 `is_file()` 检查）对它会走 `metadata` → 尺寸闸通过 →
    /// `fs::read` 返回 `Ok(vec![])` → `Some(vec![])`，新代码返回 `None`。
    /// 这才是真 RED——实测：删掉 `is_file()` 后本测试失败，且探到的旧返回值
    /// 就是 `Some([])`；同样的删除下「只测目录」的临时用例仍然绿。
    #[test]
    fn read_untracked_capped_refuses_a_non_regular_file() {
        let dev_null = Path::new("/dev/null");
        let meta = std::fs::metadata(dev_null).expect("/dev/null must exist");
        assert_eq!(
            meta.len(),
            0,
            "premise: a device reports len 0, so it slips past the byte cap"
        );
        assert!(
            !meta.is_file(),
            "premise: a char device must not look like a regular file"
        );
        assert_eq!(
            std::fs::read(dev_null).expect("reading /dev/null must succeed"),
            Vec::<u8>::new(),
            "premise: the old code's unbounded read returns Ok(empty) here, \
             so only the !is_file() guard can reject it"
        );
        assert!(
            read_untracked_capped(dev_null).is_none(),
            "/dev/null must never be read as file content"
        );

        // 目录也拒绝，但它单独不能证明这道闸（见上面的理由）。
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        assert!(
            read_untracked_capped(&sub).is_none(),
            "a directory must never be read as file content"
        );
    }

    /// 正常大小的未跟踪文件仍按文本合成 diff（不能被上限误伤）。
    #[test]
    fn untracked_file_under_the_cap_is_still_rendered_as_text() {
        let dir = repo();
        std::fs::write(dir.path().join("note.txt"), "hello\nworld\n").unwrap();
        let d = thread_diff(dir.path(), Some("trunk"), None);
        let stat = d.files.iter().find(|f| f.path == "note.txt").unwrap();
        assert!(!stat.binary, "a small text file must not be marked binary");
        assert!(
            d.unified_diff.contains("+hello"),
            "small untracked text must still be rendered: {}",
            d.unified_diff
        );
    }
}
