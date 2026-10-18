# 模型驱动的 Git 视图实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让模型决定 Git 视图展示什么（给命令），而不是软件写死取数逻辑；同时把默认基准改为会话锚点，并修掉 5 个已实测的缺陷。

**Architecture:** 工具 `show_git_diff` 从「只收 base」扩展为「收 git 命令 + 原语」；命令经 `SandboxPolicy(ReadOnly)` 执行，结果成 `ViewFrame` 按会话存内存，RPC `thread/view/read` 取整帧，前端按原语（`diff`/`log`/`text`）渲染并把命令与退出码显示在头部。无命令时走结构化默认（会话锚点 diff）。默认基准由 `thread/start` 记录的 `ThreadMeta.anchor_sha` 决定。

**Tech Stack:** Rust（app-server crate：`std::process::Command` + `yi-agent-tools` 的 `SandboxPolicy`、serde、tokio broadcast、ts-rs 生成 TS 类型）、React + TypeScript + Tailwind、Vitest + Testing Library。

## Global Constraints

- 比较基准优先级：**explicit → anchor → `refs/remotes/origin/HEAD` → `origin/main`/`origin/master` → `main`/`master` → 仅工作区**。
- `baseKind` 取值：`explicit` | `session` | `origin-head` | `origin-default` | `local-default` | `worktree-only`。
- diff 范围：含未提交改动 + 未跟踪新文件。
- 渲染原语只有三种：`diff` / `log` / `text`。`as` 省略时后端按内容判定；`as` 与内容不符时前端回退 `text`。
- 命令执行：`SandboxPolicy::new(SandboxMode::ReadOnly, cwd, vec![])`。**不写白名单**（模型已有 `bash`；防护靠沙箱 + 超时）。
- 命令超时 `VIEW_TIMEOUT = 10s`；输出上限 `MAX_VIEW_BYTES = 256 * 1024`（截断置 `truncated = true`）。
- 注入防护：所有拼接模型字符串的 git 调用加 `--end-of-options`；ref 走 `rev-parse --verify --quiet`，范围表达式按 `..` / `...` 拆分后两侧分别校验。
- 视图帧**不落盘**，存内存（`HashMap<String, ViewFrame>`）；刷新 = 重跑。
- 保留既有 RPC `thread/diff/read` 与 `ThreadDiffResult` / `DiffTextResult` 类型（单文件懒加载仍用它），**不删**。
- `show_git_diff` 的 `ToolSource` 改为 `Builtin`；工具**不**改成 skill。
- 通知方法名沿用 `ui/gitDiff/focus`（不再携带 `base`；帧已承载基准）。
- Rust 验证：`cargo test -p yi-agent-app-server -p yi-agent-skills`。前端验证：`cd desktop && npx vitest run <files> && npx tsc --noEmit`。
- 协议类型改动后**必须**跑 `cd yi-agent-rs && just bindings-check`（生成物已提交，漂移即 CI 失败）。
- 提交前跑 `cargo fmt --all`。

---

### Task 1: 会话锚点（默认基准）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:39`（`ThreadMeta` 加字段）、`:829`（`rebuild_meta`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5398`（建 meta 处取值）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs:69`（`resolve_base` 加参数）
- Test: `thread_store.rs` / `git_diff.rs` 各自 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `ThreadMeta.anchor_sha: Option<String>`；`pub fn resolve_base(repo: &Path, explicit: Option<&str>, anchor: Option<&str>) -> (Option<String>, &'static str)`（新增第三个参数）；`pub fn session_anchor(repo: &Path) -> Option<String>`（取 `rev-parse HEAD`）。

- [ ] **Step 1: 写失败测试（锚点优先级）**

在 `git_diff.rs` 的 `mod tests` 末尾加：

```rust
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

    #[test]
    fn unreachable_anchor_degrades_to_the_default_branch_chain_not_worktree_only() {
        let dir = repo();
        // 锚点已不可达（模拟 rebase / gc 掉）：不得降级为「仅工作区」，
        // 否则 diff 会突然只剩未提交改动，比报错更难理解。
        let (base, kind) = resolve_base(dir.path(), None, Some("deadbeefdeadbeef"));
        assert_eq!(kind, "worktree-only", "no other candidate exists here");
        assert_eq!(base, None);
    }

    #[test]
    fn session_anchor_reads_head() {
        let dir = repo();
        let sha = session_anchor(dir.path()).expect("a git repo has an anchor");
        assert_eq!(sha.trim(), git_ok(dir.path(), &["rev-parse", "HEAD"]).unwrap().trim());
    }

    #[test]
    fn session_anchor_is_none_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(session_anchor(dir.path()), None);
    }
```

在 `thread_store.rs` 的测试模块加：

```rust
    #[test]
    fn old_meta_without_anchor_reads_as_none() {
        let (_dir, s) = store();
        // 旧 meta 没有 anchor_sha 字段：必须零迁移地读成 None。
        let dir = _dir.path().join("thread-old");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".meta.json"),
            r#"{"thread_id":"thread-old","cwd":"/tmp","model":"m","created_at":1,"updated_at":1,
                "title":null,"permission_mode":"normal","pin_seq":null,"board_project":null,
                "card_id":null,"model_ref":null,"reasoning_effort":null,"tag":null,
                "preserve_thinking":true,"extra_roots":[],"role":null}"#,
        )
        .unwrap();
        let loaded = s.load("thread-old").unwrap().expect("meta must parse");
        assert_eq!(loaded.meta.anchor_sha, None);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server anchor 2>&1 | tail -20`
Expected: 编译失败，报 `resolve_base` 参数个数不符 / `session_anchor` 未定义。

- [ ] **Step 3: 实现**

`thread_store.rs` 的 `ThreadMeta` 加字段（放在 `role` 之后）：

```rust
    /// 会话起点：`thread/start` 时的 `HEAD` sha。默认 diff 基准用它。
    ///
    /// `None` = 建会话时不是 git 仓库，或该 meta 早于本字段（**不回填**：读操作
    /// 不产生写入，也不引入「首次打开时锚点 = 此刻」这种与「会话开始」不同的语义）。
    #[serde(default)]
    pub anchor_sha: Option<String>,
```

`rebuild_meta` 里补 `anchor_sha: None,`（损坏回退无权编造锚点）。

`git_diff.rs` 的 `resolve_base` 改签名并插入锚点分支：

```rust
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
```

`thread_diff` 与 `file_diff` 的调用点补参（本任务先传 `None`，Task 2 接到真锚点）：

```rust
pub fn thread_diff(repo: &Path, explicit_base: Option<&str>, anchor: Option<&str>) -> ThreadDiff {
    let root = repo_root(repo);
    let (base, base_kind) = resolve_base(repo, explicit_base, anchor);
    // ...其余不变
```

`server.rs` 建 meta 处（`:5398`）加一行取值：

```rust
    let now = crate::thread_store::now_millis();
    // 会话起点：默认 diff 基准。必须在建 meta 时取，之后 HEAD 会随会话推进。
    let anchor_sha = crate::git_diff::session_anchor(std::path::Path::new(cwd));
    let meta = crate::thread_store::ThreadMeta {
        // ...既有字段...
        role: None,
        anchor_sha,
    };
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server anchor 2>&1 | tail -20 && cargo test -p yi-agent-app-server git_diff 2>&1 | tail -20`
Expected: 新测试全 PASS。`git_diff` 既有测试若因签名变化编译失败，按新签名补一个 `None` 实参（不改断言）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs
git commit -m "feat(app-server): anchor a session's diff to its starting commit"
```

---

### Task 2: L1 地基（沙箱执行 + 超时 + 注入防护）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `session_anchor`（Task 1）、`yi_agent_tools::{SandboxPolicy, SandboxMode}`（已可从 `yi_agent_tools` 根导入）。
- Produces:
  - `pub const VIEW_TIMEOUT: std::time::Duration`
  - `pub fn valid_rev(repo: &Path, spec: &str) -> bool`（单 ref 或范围表达式）
  - `pub fn run_view_command(repo: &Path, command: &str) -> ViewCommandOutput`
  - `pub struct ViewCommandOutput { pub exit_code: Option<i32>, pub stdout: String, pub stderr: String, pub truncated: bool, pub timed_out: bool }`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn valid_rev_accepts_refs_and_ranges_and_rejects_junk() {
        let dir = repo();
        assert!(valid_rev(dir.path(), "HEAD"));
        assert!(valid_rev(dir.path(), "trunk"));
        assert!(valid_rev(dir.path(), "HEAD~1..HEAD"));
        assert!(valid_rev(dir.path(), "trunk...HEAD"));
        assert!(!valid_rev(dir.path(), "bogus..HEAD"), "unresolvable side must fail");
        assert!(!valid_rev(dir.path(), ""), "empty is not a rev");
        assert!(!valid_rev(dir.path(), "--output=/tmp/x"), "options are not revs");
    }

    #[tokio::test]
    async fn view_command_rejects_an_output_option_that_would_write() {
        let dir = repo();
        let probe = dir.path().join("probe.txt");
        let out = run_view_command(
            dir.path(),
            &format!("git diff --no-color --numstat --output={}", probe.display()),
        )
        .await;
        assert!(!probe.exists(), "sandbox/end-of-options must prevent the write");
        assert!(out.exit_code != Some(0), "it must not report success: {out:?}");
    }

    #[tokio::test]
    async fn view_command_reports_read_output() {
        let dir = repo();
        let out = run_view_command(dir.path(), "git log --oneline -1").await;
        assert_eq!(out.exit_code, Some(0));
        assert!(out.stdout.contains("init"), "stdout: {}", out.stdout);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn view_command_denies_writes_in_read_only_mode() {
        let dir = repo();
        // 写 .git/index 必须被拒（ReadOnly 沙箱）。
        let out = run_view_command(dir.path(), "git add -A").await;
        assert!(out.exit_code != Some(0), "git add must be denied: {out:?}");
        // 仓库状态不变。
        assert_eq!(git_ok(dir.path(), &["log", "--oneline"]).unwrap().lines().count(), 1);
    }

    #[tokio::test]
    async fn view_command_times_out_instead_of_hanging() {
        let dir = repo();
        let out = run_view_command(dir.path(), "sleep 60").await;
        assert!(out.timed_out, "sleep must be killed by the timeout: {out:?}");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server view_command 2>&1 | tail -20`
Expected: 编译失败（`run_view_command` / `valid_rev` 未定义）。

- [ ] **Step 3: 实现**

在 `git_diff.rs` 顶部加导入与常量：

```rust
use std::path::Path;
use std::time::Duration;

/// 视图命令的超时。与 bash 的「预期运行时长」无关：这里是展示用命令，
/// 只读且应当很快；挂住比失败更糟（面板会一直转）。
pub const VIEW_TIMEOUT: Duration = Duration::from_secs(10);
```

实现（放在 `truncate` 附近）：

```rust
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
    if let Some((left, right)) = spec
        .split_once("...")
        .or_else(|| spec.split_once(".."))
    {
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
    let sandbox = yi_agent_tools::SandboxPolicy::new(
        yi_agent_tools::SandboxMode::ReadOnly,
        repo,
        Vec::new(),
    );
    let (program, args) = match sandbox.command(command, repo) {
        Ok(v) => v,
        Err(e) => {
            return ViewCommandOutput {
                exit_code: None,
                stdout: String::new(),
                stderr: format!("sandbox unavailable: {e}"),
                truncated: false,
                timed_out: false,
            }
        }
    };
    let mut child = match tokio::process::Command::new(&program)
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
            }
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
            }
        }
        Err(_) => {
            // `kill_on_drop` 已在 drop 时杀掉整个进程组；这里只需如实报 timeout。
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
```

`truncate` 的签名（`String) -> (String, bool)`）与此处字符串输出复用，无需改。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server view_command 2>&1 | tail -20 && cargo test -p yi-agent-app-server valid_rev 2>&1 | tail -10`
Expected: 全 PASS。若 `sleep 60` 在 ReadOnly 沙箱下被拒而非超时，改该测试断言为「未挂住」（超时或非 0 退出二者皆可），但**必须**保留「不会挂住」这一语义。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs yi-agent-rs/crates/yi-agent-app-server/Cargo.toml
git commit -m "feat(app-server): run view commands in a read-only sandbox with a timeout"
```

---

### Task 3: 视图帧与 RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs`（`ViewFrame` + 内容判定）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:454`（通知去掉 `base`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（帧表 + `thread/view/read` + watcher）
- Test: `git_diff.rs` 单测 + `server.rs` RPC 测试

**Interfaces:**
- Consumes: Task 1 的 `resolve_base` / `session_anchor`、Task 2 的 `run_view_command` / `valid_rev`。
- Produces:
  - `pub struct ViewFrame`（字段见下，`serde(rename_all = "camelCase")`）
  - `pub fn classify(stdout: &str) -> &'static str`（内容 → `diff`/`log`/`text`）
  - `pub fn structured_frame(repo: &Path, anchor: Option<&str>) -> ViewFrame`
  - RPC `thread/view/read { threadId, refresh? }`
  - 通知 `ui/gitDiff/focus { threadId, note }`（**去掉 `base`**）

- [ ] **Step 1: 写失败测试**

`git_diff.rs`：

```rust
    #[test]
    fn classify_detects_diff_log_and_text() {
        assert_eq!(classify("diff --git a/x b/x\n--- a/x\n"), "diff");
        assert_eq!(classify("abc1234 fix a thing\ndef5678 add another\n"), "log");
        assert_eq!(classify("random words\n"), "text");
        assert_eq!(classify(""), "text");
    }

    #[test]
    fn structured_frame_reports_the_session_anchor() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        let f = structured_frame(dir.path(), Some("trunk"));
        assert_eq!(f.command, None, "structured default has no command");
        assert_eq!(f.as_kind, "diff");
        assert_eq!(f.base_kind, "session");
        assert!(f.stdout.contains("+two"), "stdout: {}", f.stdout);
    }
```

`server.rs` 的 RPC 测试实际在 `rpc_tests.rs`（`server.rs:7720` 以 `#[path = "rpc_tests.rs"] pub(crate) mod tests;` 挂载），故新测试加在 **`rpc_tests.rs`**，紧邻既有的 `thread_diff_read_reports_base_and_files`（`:2121`），并沿用同一套夹具（`Harness::with_cfg` / `initialize` / `start_thread` / `h.shutdown()`）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_returns_null_before_any_frame() {
    let mut cfg = test_config();
    cfg.workdir = tempfile::TempDir::new().unwrap().path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/view/read","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(9)) {
        v = h.read_value().await;
    }
    assert!(v["result"].is_null(), "no frame yet must read back as null: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_refresh_produces_a_structured_frame() {
    let dir = tempfile::TempDir::new().unwrap();
    init_git_repo(dir.path());
    std::fs::write(dir.path().join("a.txt"), "x\ny\n").unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":10,"method":"thread/view/read","params":{{"threadId":"{tid}","refresh":true}}}}"#
    ))
    .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(10)) {
        v = h.read_value().await;
    }
    // 无帧时 refresh 走结构化默认：应是 diff 原语、基准是会话锚点、含未跟踪内容。
    assert_eq!(v["result"]["asKind"], "diff", "frame: {v}");
    assert!(v["result"]["stdout"].as_str().unwrap().contains("+y"), "frame: {v}");
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_view_read_rejects_unknown_thread() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":11,"method":"thread/view/read","params":{"threadId":"thread-nope"}}"#)
        .await;
    let mut v = h.read_value().await;
    while v.get("id") != Some(&serde_json::json!(11)) {
        v = h.read_value().await;
    }
    assert_eq!(v["error"]["code"], -32011, "unknown thread uses the shared code");
    h.shutdown().await;
}
```

> 这三条测试都走真实 RPC 路径，**不新增**测试专用生产 API。`refresh` 分支同时覆盖了「无帧时重算结构化默认」，故 Task 3 不必依赖 Task 4 的工具。`server.rs` 里另需的 `Harness` 帧表访问：若 Task 9 需要直接注入帧，用 `RuntimeAttachments` 里共享的 `view_frames`（`rpc_tests.rs:1587` 等构造点各自加一条 `view_frames: Arc::new(...)`）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server view_frame 2>&1 | tail -20`
Expected: 编译失败（`ViewFrame` / `classify` / `structured_frame` 未定义）。

- [ ] **Step 3: 实现帧与内容判定**

`git_diff.rs`：

```rust
/// 模型投递到前端的一帧视图。
///
/// 与 `ThreadDiff` 的区别：`ThreadDiff` 是「结构化取数」的结果（软件决定看什么），
/// 本帧是「模型给命令」的结果（模型决定看什么）。两者并存：前者服务单文件懒加载，
/// 后者服务模型驱动的展示。
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
pub fn classify(stdout: &str) -> &'static str {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return "text";
    };
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

/// 结构化默认帧：不跑模型命令，直接算会话锚点 diff。
pub fn structured_frame(repo: &Path, anchor: Option<&str>) -> ViewFrame {
    let d = thread_diff(repo, None, anchor);
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
```

- [ ] **Step 4: 实现慢性命令帧**

```rust
/// 把模型给的命令跑成帧。`as_hint` 是模型声明的原语（`None` = 自动判定）。
pub async fn command_frame(
    repo: &Path,
    command: &str,
    as_hint: Option<&str>,
    note: Option<String>,
) -> ViewFrame {
    let out = run_view_command(repo, command).await;
    let as_kind = match as_hint {
        Some("diff") | Some("log") | Some("text") => as_hint.unwrap().to_string(),
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
        base_kind: String::new(),
    }
}
```

- [ ] **Step 5: 协议：通知去掉 `base`**

`protocol.rs:456` 的 `UiGitDiffFocus` 改成：

```rust
    /// 模型（`show_git_diff` 工具）请求桌面端聚焦 Git Diff 视图。
    ///
    /// 不再携带 `base`：基准现在由**帧**承载（`thread/view/read` 读回），
    /// 前端不必自己解析基准。
    #[serde(rename = "ui/gitDiff/focus", rename_all = "camelCase")]
    UiGitDiffFocus {
        thread_id: Option<String>,
        note: Option<String>,
    },
```

`server.rs` 的 `pump_git_diff_notifications` 去掉 `base: focus.base`。

`git_diff_tool.rs` 的 `GitDiffFocus` 去掉 `base` 字段（Task 4 会同时补 `command`）。

- [ ] **Step 6: 服务端：帧表 + RPC**

`server.rs` 的 `RuntimeAttachments` 加帧表（与 `git_diff` 句柄并列）：

```rust
    /// 模型驱动的视图帧：每会话一帧，内存态，不落盘。
    pub(crate) view_frames: Arc<tokio::sync::Mutex<std::collections::HashMap<String, crate::git_diff::ViewFrame>>>,
```

**全部构造点都要补这一条字段（共 10 处）**：生产与测试各半，漏掉任一处都是编译错误（结构体字面量不做默认填充）——
`server.rs:1563`、`server.rs:1644`、`server.rs:2366`（解构，需加 `view_frames,`）、`ws.rs:114`、`rpc_tests.rs:1587`、`:2562`、`:2641`、`:2722`、`:14599`。
测试夹具各自新建一份 `Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()))`；`serve` 里同理。

在 `thread/view/read` RPC 分支实现：

```rust
                    "thread/view/read" => {
                        let Some(thread_id) =
                            require_thread_id(&hub, &client, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        if !require_known_thread(&hub, &client, &threads, &thread_id, id.clone())
                            .await?
                        {
                            continue;
                        }
                        let refresh = req.params.get("refresh").and_then(|v| v.as_bool()).unwrap_or(false);
                        let mut frames = view_frames.lock().await;
                        if refresh {
                            // 重跑：结构化默认用锚点重算，命令帧重跑同一条命令。
                            let cwd = threads
                                .get(&thread_id)
                                .map(|t| t.cwd.clone())
                                .filter(|c| !c.is_empty())
                                .unwrap_or_else(|| cfg.workdir.display().to_string());
                            let repo = Path::new(&cwd);
                            let anchor = threads
                                .get(&thread_id)
                                .map(|t| t.store.load(&thread_id).ok().flatten())
                                .flatten()
                                .and_then(|l| l.meta.anchor_sha);
                            let next = match frames.get(&thread_id).and_then(|f| f.command.clone()) {
                                Some(cmd) => crate::git_diff::command_frame(repo, &cmd, None, None).await,
                                None => crate::git_diff::structured_frame(repo, anchor.as_deref()),
                            };
                            frames.insert(thread_id.clone(), next);
                        }
                        let result = frames
                            .get(&thread_id)
                            .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                            .unwrap_or(json!(null));
                        drop(frames);
                        write_response(&hub, &client, ok_response(id, result)).await?;
                    }
```

- [ ] **Step 7: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server 2>&1 | tail -20`
Expected: 全绿（含既有全部测试，证明 10 处 `RuntimeAttachments` 构造点与监听点改动无回归）。

- [ ] **Step 8: 重新生成 TS 绑定**

Run: `cd yi-agent-rs && just bindings-check 2>&1 | tail -10`
Expected: 生成物更新（`Notification.ts` 里 `gitDiff/focus` 少了 `base`），且 `git diff --exit-code` 通过。

- [ ] **Step 9: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A yi-agent-rs/crates/yi-agent-app-server/src desktop/src/lib/generated
git commit -m "feat(app-server): carry model-driven view frames over thread/view/read"
```

---

### Task 4: 工具接受命令，回执如实（D3）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff_tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（注册处传 repo/anchor/帧表）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`GitDiffFocus` 加 `command`）
- Test: `git_diff_tool.rs` 单测

**Interfaces:**
- Consumes: Task 2/3 的 `command_frame` / `structured_frame` / `valid_rev` / `ViewFrame`。
- Produces: `show_git_diff({ command?, as?, note? })`；`ToolSource::Builtin`。

- [ ] **Step 1: 写失败测试**

先在 `git_diff_tool.rs` 的 `mod tests` 加一个取文本的本地 helper（`ToolResult` 只有 `content: Vec<ContentBlock>` 字段，**没有** `.text()` 访问器，故按既有方式自取）：

```rust
    /// 取 `ToolResult` 的第一段文本。`ToolResult` 没有文本访问器，
    /// 其 `content` 是 `Vec<ContentBlock>`（`yi_agent_core::ContentBlock::Text`）。
    fn text_of(result: &ToolResult) -> String {
        match result.content.first() {
            Some(yi_agent_core::ContentBlock::Text(t)) => t.clone(),
            _ => String::new(),
        }
    }

    /// 临时仓库 + 一次 init 提交（与 `git_diff.rs` 的 `repo()` 同形的最小版）。
    fn init_repo(dir: &std::path::Path) {
        for args in [
            vec!["init", "-q", "-b", "trunk"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "T"],
        ] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        for args in [vec!["add", "."], vec!["commit", "-q", "-m", "init"]] {
            assert!(std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success());
        }
    }
```

再追加三条测试：

```rust
    #[tokio::test]
    async fn receipt_reports_the_effective_base_not_the_requested_one() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let handle = GitDiffHandle::new();
        let tool = ShowGitDiffTool::new(
            handle.clone(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool.call(serde_json::json!({ "base": "no-such-ref" })).await;
        assert!(result.is_error, "an unusable rev must fail loudly: {result:?}");
        let text = text_of(&result);
        assert!(
            text.contains("no-such-ref"),
            "the receipt should say which rev was rejected: {text}"
        );
    }

    #[tokio::test]
    async fn a_command_is_echoed_in_the_receipt_and_broadcast() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let handle = GitDiffHandle::new();
        let mut rx = handle.subscribe();
        let tool = ShowGitDiffTool::new(
            handle.clone(),
            Some("thread-1".into()),
            dir.path().to_path_buf(),
            None,
        );
        let result = tool
            .call(serde_json::json!({ "command": "git log --oneline -1" }))
            .await;
        assert!(!result.is_error, "a read-only command must succeed: {result:?}");
        assert!(text_of(&result).contains("git log"));
        let focus = rx.recv().await.unwrap();
        assert_eq!(focus.command.as_deref(), Some("git log --oneline -1"));
        assert!(focus.frame.is_some(), "the frame must ride along");
    }

    #[tokio::test]
    async fn the_receipt_names_the_session_base_when_no_base_was_given() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let anchor = crate::git_diff::session_anchor(dir.path());
        let tool = ShowGitDiffTool::new(
            GitDiffHandle::new(),
            None,
            dir.path().to_path_buf(),
            anchor,
        );
        let result = tool.call(serde_json::json!({})).await;
        assert!(!result.is_error);
        let text = text_of(&result);
        assert!(
            text.contains("session") || text.contains("会话"),
            "must name the session base: {text}"
        );
    }
```

> 注意第一条测试的语义改成「拒绝并**说明**是哪个 rev 不可用」，而不是「悄悄回退并撒谎」——这正是 D3 的要点。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server git_diff_tool 2>&1 | tail -20`
Expected: 编译失败（`ShowGitDiffTool::new` 参数变多、`focus.command` 未定义）。

- [ ] **Step 3: 实现工具**

`git_diff_tool.rs`：

```rust
#[derive(Debug, Deserialize)]
struct ShowGitDiffArgs {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default, rename = "as")]
    as_kind: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

pub struct ShowGitDiffTool {
    handle: GitDiffHandle,
    thread_id: Option<String>,
    /// 本会话的仓库路径：执行命令与解析基准都在这里发生。
    repo: std::path::PathBuf,
    /// 本会话起点（默认基准）。工具要能回答「实际用了哪个基准」。
    anchor: Option<String>,
}
```

`call` 的三条分支：

```rust
    async fn call(&self, args: Value) -> ToolResult {
        let parsed: ShowGitDiffArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        let note = parsed.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());

        // 1) 给了命令：跑它。
        if let Some(command) = parsed.command.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) {
            let frame = crate::git_diff::command_frame(
                &self.repo,
                &command,
                parsed.as_kind.as_deref(),
                note.clone(),
            )
            .await;
            // 帧写进会话槽由 watcher 侧负责（见 server.rs），工具只广播意图。
            self.handle.broadcast(GitDiffFocus {
                thread_id: self.thread_id.clone(),
                command: Some(command.clone()),
                base: None,
                note,
                frame: Some(Box::new(frame.clone())),
            });
            if let Some(code) = frame.exit_code {
                if code != 0 {
                    return ToolResult::error(format!(
                        "`{command}` exited {code}: {}",
                        frame.stderr.trim()
                    ));
                }
            }
            return ToolResult::text(format!(
                "showing the output of `{command}` ({} bytes{})",
                frame.stdout.len(),
                if frame.truncated { ", truncated" } else { "" }
            ));
        }

        // 2) 给了 base：先校验，再按它算一帧。校验失败**明确报错**，不静默回退
        //    （D3：旧实现回退后仍回报入参，等于对模型撒谎）。
        let explicit = parsed.base.map(|b| b.trim().to_string()).filter(|b| !b.is_empty());
        if let Some(ref rev) = explicit {
            if !crate::git_diff::valid_rev(&self.repo, rev) {
                return ToolResult::error(format!("`{rev}` is not a usable git revision"));
            }
        }
        let frame =
            crate::git_diff::structured_frame(&self.repo, explicit.as_deref().or(self.anchor.as_deref()));
        self.handle.broadcast(GitDiffFocus {
            thread_id: self.thread_id.clone(),
            command: None,
            base: explicit.clone(),
            note,
            frame: Some(Box::new(frame.clone())),
        });
        // 回执报**实际**生效的基准，不是入参。
        match (&frame.base, frame.base_kind.as_str()) {
            (Some(b), "session") => ToolResult::text(format!(
                "showing the diff since this session started (base {b})"
            )),
            (Some(b), "explicit") => ToolResult::text(format!("showing the diff against {b}")),
            (Some(b), kind) => ToolResult::text(format!("showing the diff against {b} ({kind})")),
            (None, _) => ToolResult::text("showing uncommitted changes only (no base ref found)"),
        }
    }
```

`description` 与 `schema` 同步改成新参数集（`command` / `as` / `base` / `note`），描述里点明「命令在只读沙箱内执行」。`metadata` 的 `source` 改 `ToolSource::Builtin`。

- [ ] **Step 4: 协议与 watcher 携带帧**

`GitDiffFocus` 加两个字段：

```rust
pub struct GitDiffFocus {
    pub thread_id: Option<String>,
    /// 模型请求的基准（`None` = 默认）。仅用于回执之外的展示语义。
    pub base: Option<String>,
    /// 模型请求的命令（`None` = 结构化默认）。
    pub command: Option<String>,
    pub note: Option<String>,
    /// 已算好的帧。工具算一次、watcher 写进会话槽，避免在两处各算一遍
    /// （两次读之间仓库可能在变，那会让「回执说的」与「面板显示」不一致）。
    pub frame: Option<Box<crate::git_diff::ViewFrame>>,
}
```

`pump_git_diff_notifications` 增加写帧表（需要把帧表传进来），并把通知改成不带 `base`：

```rust
        if let (Some(focus_thread), Some(frame)) = (focus.thread_id.clone(), focus.frame) {
            view_frames.lock().await.insert(focus_thread, *frame);
        }
        let n = Notification::UiGitDiffFocus {
            thread_id: focus.thread_id,
            note: focus.note,
        };
```

`build_runtime_tooling` 的注册处补两个实参（repo 用 `cfg.workdir`，它与 thread cwd 同源；anchor 从 `thread_store` 读）：

```rust
    registry.register(Arc::new(crate::git_diff_tool::ShowGitDiffTool::new(
        git_diff,
        Some(thread_id.to_string()),
        cfg.workdir.clone(),
        anchor_sha,
    )));
```

`build_runtime_tooling` 需要多一个 `anchor_sha: Option<String>` 参数；其 3 处测试调用补 `None`。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server 2>&1 | tail -20`
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add -A yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): let show_git_diff take a command and report what it actually did"
```

---

### Task 5: D2 合并提交 + D1 未跟踪文件的单文件 diff

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs:314`（`commit_diff`）、`:319`（`file_diff`）
- Test: 同文件

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn merge_commits_render_a_first_parent_diff() {
        let dir = repo();
        assert!(std::process::Command::new("git")
            .args(["checkout", "-q", "-b", "side"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join("b.txt"), "side\n").unwrap();
        for a in [vec!["add", "."], vec!["commit", "-q", "-m", "side work"]] {
            assert!(std::process::Command::new("git")
                .args(&a)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }
        assert!(std::process::Command::new("git")
            .args(["checkout", "-q", "trunk"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join("a.txt"), "main edit\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["commit", "-qam", "main work"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(std::process::Command::new("git")
            .args(["merge", "-q", "--no-ff", "side", "-m", "merge side"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());

        let merge = git_ok(dir.path(), &["rev-parse", "HEAD"]).unwrap();
        let (diff, _) = commit_diff(dir.path(), merge.trim());
        assert!(
            !diff.trim().is_empty(),
            "a merge commit must not render blank (that was the bug)"
        );
        assert!(diff.contains("b.txt"), "the merged-in change must show: {diff}");
    }

    #[test]
    fn non_merge_commits_are_unchanged() {
        let dir = repo();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        assert!(std::process::Command::new("git")
            .args(["commit", "-qam", "edit a"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let (diff, _) = commit_diff(dir.path(), "HEAD");
        assert!(diff.contains("+two"), "non-merge behaviour must not change: {diff}");
    }

    #[test]
    fn file_diff_covers_untracked_files() {
        let dir = repo();
        std::fs::write(dir.path().join("new.txt"), "hello\n").unwrap();
        let (diff, _) = file_diff(dir.path(), Some("trunk"), "new.txt");
        assert!(
            diff.contains("+hello"),
            "an untracked file's content must be synthesized (the path branch returned 0 bytes): {diff}"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server merge_commits_render 2>&1 | tail -20`
Expected: FAIL —— merge 的 `commit_diff` 返回空串；未跟踪的 `file_diff` 返回空串。

- [ ] **Step 3: 实现**

```rust
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

/// 单文件 diff。未跟踪文件不在任何 `git diff` 的输出里，需单独合成
/// （否则 `path` 分支对它返回 0 字节，前端懒加载出来是空白）。
pub fn file_diff(repo: &Path, merge_base: Option<&str>, path: &str) -> (String, bool) {
    let root = repo_root(repo);
    let raw = match merge_base {
        Some(mb) => git_ok(repo, &["diff", "--no-color", mb, "--", path]).unwrap_or_default(),
        None => git_ok(repo, &["diff", "--no-color", "HEAD", "--", path]).unwrap_or_default(),
    };
    // 空结果 + 该路径确在未跟踪清单里 → 合成「整文件新增」。
    if raw.trim().is_empty() {
        let untracked = untracked_paths(repo);
        if untracked.iter().any(|p| p == path) {
            let synthesized = untracked_diff(&root, &[path.to_string()]);
            return truncate(synthesized);
        }
    }
    truncate(raw)
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server 2>&1 | tail -20`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/git_diff.rs
git commit -m "fix(app-server): render merge commits and untracked files instead of blank"
```

---

### Task 6: skill 大改（判断知识移出工具）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/assets/git-diff-review/SKILL.md`
- Test: `yi-agent-rs/crates/yi-agent-skills/src/system.rs`

- [ ] **Step 1: 写失败测试**

在 `system.rs` 的测试模块加：

```rust
    #[test]
    fn the_git_diff_skill_teaches_commands_not_just_a_base() {
        let tmp = tempfile::tempdir().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let body = std::fs::read_to_string(tmp.path().join("git-diff-review/SKILL.md")).unwrap();
        assert!(body.contains("show_git_diff"), "skill must name the tool");
        assert!(body.contains("command"), "the tool now takes a command: {body}");
        assert!(body.contains("git log"), "skill must give a log example");
        assert!(body.contains("read-only") || body.contains("只读"), "skill must warn about the sandbox");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-skills the_git_diff_skill 2>&1 | tail -20`
Expected: FAIL（现有 SKILL.md 只讲 `base`）。

- [ ] **Step 3: 重写 skill**

把 `SKILL.md` 换成围绕「写命令 + 选原语」的版本：何时请人 review、命令样例集（`git diff --stat` 探规模、`git log --oneline -10` 看提交、`git diff HEAD` 看未提交、`git diff <base> -- <path>` 看某目录）、`as` 的选择、**只读沙箱**的告警（`git stash push` 一类会改状态的命令不能写，写了只会被拒）、以及工具不可用时的降级（自己跑 git 并用文字汇报，不要贴整份超长 diff）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-skills 2>&1 | tail -20`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/assets/git-diff-review/SKILL.md yi-agent-rs/crates/yi-agent-skills/src/system.rs
git commit -m "feat(skills): teach git-diff-review to drive the view with commands"
```

---

### Task 7: 前端原语解析（`viewFrame.ts`）

**Files:**
- Create: `desktop/src/lib/viewFrame.ts`
- Create: `desktop/src/lib/viewFrame.test.ts`
- Test: 同上

**Interfaces:**
- Produces: `export type ViewKind = "diff" | "log" | "text"`；`export interface ViewFrame { command: string | null; asKind: string; exitCode: number | null; stdout: string; stderr: string; truncated: boolean; timedOut: boolean; note: string | null; base: string | null; baseKind: string }`；`export function resolveKind(frame: ViewFrame): ViewKind`；`export interface LogEntry { sha: string; subject: string }`；`export function parseLog(text: string): LogEntry[]`。

- [ ] **Step 1: 写失败测试**

```ts
import { describe, it, expect } from "vitest";
import { resolveKind, parseLog } from "./viewFrame";

const frame = (over: Partial<Parameters<typeof resolveKind>[0]> = {}) => ({
  command: null,
  asKind: "text",
  exitCode: 0,
  stdout: "",
  stderr: "",
  truncated: false,
  timedOut: false,
  note: null,
  base: null,
  baseKind: "",
  ...over,
});

describe("resolveKind", () => {
  it("trusts a declared kind that matches the content", () => {
    expect(resolveKind(frame({ asKind: "diff", stdout: "diff --git a/x b/x\n" }))).toBe("diff");
  });

  it("falls back to text when the declared kind mismatches the content", () => {
    // 模型说是 diff，但内容不是：回退 text（保底永不失手）。
    expect(resolveKind(frame({ asKind: "diff", stdout: "just words\n" }))).toBe("text");
  });

  it("keeps the declared kind for text regardless of content", () => {
    expect(resolveKind(frame({ asKind: "text", stdout: "diff --git a/x b/x\n" }))).toBe("text");
  });
});

describe("parseLog", () => {
  it("splits oneline log entries", () => {
    expect(parseLog("abc1234 fix a thing\ndef5678 add another\n")).toEqual([
      { sha: "abc1234", subject: "fix a thing" },
      { sha: "def5678", subject: "add another" },
    ]);
  });

  it("tolerates a decorated log and empty input", () => {
    expect(parseLog("")).toEqual([]);
    expect(parseLog("no sha here\n")).toEqual([]);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/viewFrame.test.ts 2>&1 | tail -20`
Expected: FAIL（模块不存在）。

- [ ] **Step 3: 实现**

`viewFrame.ts`：`resolveKind` 按「声明与内容一致性」判定——`diff` 需内容含 `diff --git`；`log` 需首行匹配短 sha + 空格；`text` 原样接受；不匹配即 `text`。`parseLog` 按 `^([0-9a-f]{7,40})\s+(.*)$` 提取，未匹配的行丢弃。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/viewFrame.test.ts 2>&1 | tail -20`
Expected: 5 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/viewFrame.ts desktop/src/lib/viewFrame.test.ts
git commit -m "feat(desktop): resolve a view frame's rendering primitive"
```

---

### Task 8: 前端渲染（命令条 + 三种原语 + D1/D4）

**Files:**
- Modify: `desktop/src/components/GitDiffView.tsx`
- Modify: `desktop/src/components/GitDiffView.test.tsx`
- Modify: `desktop/src/lib/protocol.ts`（加 `ViewFrame` 类型）

- [ ] **Step 1: 写失败测试**

在 `GitDiffView.test.tsx` 追加：

```tsx
const frame = (over = {}) => ({
  command: null,
  asKind: "diff",
  exitCode: 0,
  stdout: "",
  stderr: "",
  truncated: false,
  timedOut: false,
  note: null,
  base: null,
  baseKind: "",
  ...over,
});

describe("GitDiffView 模型帧", () => {
  it("把命令与退出码显示在头部", () => {
    render(
      <GitDiffView
        {...baseProps}
        diff={diff}
        frame={frame({ command: "git log --oneline -1", asKind: "text", stdout: "abc1234 x\n" })}
      />,
    );
    expect(screen.getByText(/git log --oneline -1/)).toBeTruthy();
  });

  it("log 原语渲染成提交列表", () => {
    render(
      <GitDiffView
        {...baseProps}
        diff={diff}
        frame={frame({ command: "git log --oneline -2", asKind: "log", stdout: "abc1234 fix a\ndef5678 add b\n" })}
      />,
    );
    expect(screen.getByText("fix a")).toBeTruthy();
    expect(screen.getByText("add b")).toBeTruthy();
  });

  it("汇总行给出增删统计（D4）", () => {
    render(<GitDiffView {...baseProps} diff={diff} />);
    expect(screen.getByText(/\+2/)).toBeTruthy();
    expect(screen.getByText(/−1|-\s*1/)).toBeTruthy();
  });

  it("session 基准显示为「本次会话」而不是原始 ref（D4）", () => {
    render(
      <GitDiffView {...baseProps} diff={{ ...diff, base: "deadbeef", baseKind: "session" }} />,
    );
    expect(screen.getByText(/本次会话/)).toBeTruthy();
  });

  it("截断且无正文的文件被明确标注，而不是空白（D1）", () => {
    render(
      <GitDiffView
        {...baseProps}
        diff={{ ...diff, truncated: true, files: [diff.files[0]], unifiedDiff: "" }}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /a\.txt/ }));
    expect(screen.getByText(/正文已截断|未加载/)).toBeTruthy();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/GitDiffView.test.tsx 2>&1 | tail -20`
Expected: FAIL（`frame` prop 不存在；无统计；无 `session` 文案）。

- [ ] **Step 3: 实现**

`GitDiffView` 增加可选 `frame` prop：有帧且帧有内容时，头部显示命令 + 退出码，正文按 `resolveKind` 分派（`diff` → 复用 `parseUnifiedDiff`；`log` → `parseLog` 列表；`text` → 等宽文本）。无帧时保持既有 `diff` 渲染路径（向后兼容）。
汇总行补 `+X −Y`（`files` 求和）。`baseKind === "session"` 显示「本次会话」。
文件无正文时（`truncated` 且该 path 不在解析结果里）显示「正文已截断，点击重新加载」并把点击接到单文件懒加载回调（新增可选 prop `onLoadFile?: (path: string) => void`）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/GitDiffView.test.tsx 2>&1 | tail -20 && npx tsc --noEmit 2>&1 | tail -10`
Expected: 全 PASS，无类型错误。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/GitDiffView.tsx desktop/src/components/GitDiffView.test.tsx desktop/src/lib/protocol.ts
git commit -m "feat(desktop): render model-driven frames and fix the diff summary"
```

---

### Task 9: 前端接线（通知 → 取帧；D1 懒加载）

**Files:**
- Modify: `desktop/src/hooks/useDiffState.ts`（加帧状态与 `loadFrame`）
- Modify: `desktop/src/App.tsx:1181`（focus 分支改读帧）
- Modify: `desktop/src/hooks/useSessionState.ts`（复位）
- Test: `desktop/src/App.test.tsx`

- [ ] **Step 1: 写失败测试**

在 `App.test.tsx` 的 diff 相关 describe 里加（沿用既有夹具：`state.notifHandlers` / `state.dataSources` / `clients[0].requests`，照 `ui/gitDiff/focus 打开面板并切到 Git Diff，读出该会话的 diff` 那条测试的写法）：

```tsx
  it("ui/gitDiff/focus 打开面板并读出该会话的视图帧", async () => {
    state.dataSources["thread/view/read"] = () => ({
      command: "git log --oneline -1",
      asKind: "text",
      exitCode: 0,
      stdout: "abc1234 x\n",
      stderr: "",
      truncated: false,
      timedOut: false,
      note: null,
      base: null,
      baseKind: "",
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");

    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: "看这里" } });
      }
    });

    expect(await screen.findByLabelText("会话详情")).toBeTruthy();
    expect(screen.getByRole("tab", { name: "Git Diff" }).getAttribute("aria-selected")).toBe("true");
    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/view/read",
        params: { threadId: "t1" },
      }),
    );
  });

  it("单文件懒加载在文件正文缺失时补一次带 path 的 thread/diff/read", async () => {
    // 文件列在 files[] 里但正文被截断：展开它必须为该 path 取一次正文。
    // 组件级先把 onLoadFile 的契约钉死（带正确 path），App 级再钉「它真的发了请求」。
    state.dataSources["thread/diff/read"] = () => ({
      base: "origin/main",
      baseKind: "origin-default",
      mergeBase: "abc1234",
      commits: [],
      files: [{ path: "a.txt", status: "M", additions: 1, deletions: 1, binary: false }],
      unifiedDiff: "", // 正文缺失（截断的后果）
      truncated: true,
    });
    render(<App />);
    await screen.findByLabelText("会话详情面板");
    act(() => {
      for (const cb of state.notifHandlers) {
        cb({ method: "ui/gitDiff/focus", params: { threadId: "t1", note: null } });
      }
    });
    await screen.findByLabelText("会话详情");

    // 展开该文件（它落在默认展开窗口内时无需点击，故兼容两种情形）。
    const fileButton = screen.getByRole("button", { name: /a\.txt/ });
    fireEvent.click(fileButton); // 折叠
    fireEvent.click(fileButton); // 再展开，触发懒加载

    await waitFor(() =>
      expect(clients[0].requests).toContainEqual({
        method: "thread/diff/read",
        params: { threadId: "t1", path: "a.txt" },
      }),
    );
  });
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx 2>&1 | tail -20`
Expected: FAIL（focus 分支仍发 `thread/diff/read`，没有 `thread/view/read`）。

- [ ] **Step 3: 实现接线**

`useDiffState` 加 `viewFrame` 状态与 `loadViewFrame(threadId, refresh?)`（归属门与既有两道门同形：`diffReqRef` + `diffOwnerRef`）。`App.tsx` 的 focus 分支把 `diff.loadThreadDiff(target, n.params.base)` 换成 `diff.loadViewFrame(target)`。切会话时 `resetThreadDiff` 一并清帧。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx 2>&1 | tail -20 && npx tsc --noEmit 2>&1 | tail -10`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/hooks/useDiffState.ts desktop/src/App.tsx desktop/src/hooks/useSessionState.ts desktop/src/App.test.tsx
git commit -m "feat(desktop): wire the model-driven view frame into the panel"
```

---

### Task 10: 全量验证 + 项目文档

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`（仅当计数变化）

- [ ] **Step 1: 全量 Rust 与前端验证**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server -p yi-agent-skills 2>&1 | tail -5 && just bindings-check 2>&1 | tail -5 && cargo fmt --all --check`
Run: `cd desktop && npx vitest run 2>&1 | tail -5 && npx tsc --noEmit 2>&1 | tail -5`
Expected: 全绿；`bindings-check` 无漂移。

- [ ] **Step 2: 手动核对主仓库的真实效果**

在主仓库跑一次真实取数，确认量级从「174 commits / 175 files」降到「本次会话的改动」：

Run: `cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git log --oneline $(git rev-parse HEAD)..HEAD | wc -l`
Expected: 0（锚点 = HEAD 时无提交），即面板只显示未提交改动——这正是「这次会话改了什么」的语义。

- [ ] **Step 3: 更新项目文档**

在 `docs/project-management/desktop.md` 的 Git Diff 条目补：模型驱动视图（工具 `show_git_diff({command?, as?, note?})`、原语 `diff`/`log`/`text`、RPC `thread/view/read`、只读沙箱、会话锚点基准），每条带判据（命令或 `file.rs:line`）。同步 `README.md` 索引表计数（若有变化）。

- [ ] **Step 4: 提交**

```bash
git add docs/project-management
git commit -m "docs(project-management): record the model-driven git view"
```

---

## 自查记录

**Spec 覆盖：**

| Spec 章节 | 落地任务 |
| --- | --- |
| §5.1 会话锚点 | Task 1 |
| §5.2 L1 地基（沙箱/超时/注入） | Task 2 |
| §5.3 命令即接口 | Task 4 |
| §5.4 帧与 RPC | Task 3 |
| §5.5 回执（D3） | Task 4 |
| §5.6 D1 截断/单文件 | Task 5（后端）+ Task 8（前端） |
| §5.7 D2 合并提交 | Task 5 |
| §5.8 D4 汇总与文案 | Task 8 |
| §5.9 工具与 skill 分工 | Task 4（`Builtin`）+ Task 6（skill） |
| §6 错误与边界 | Task 2（超时/沙箱）、Task 4（回执）、Task 8（文案） |
| §7 测试 | 每个任务的测试步骤 + Task 10 |

**类型一致性：** Rust `ViewFrame`（camelCase serde）↔ TS `ViewFrame`（`asKind` / `exitCode` / `timedOut` / `baseKind`）；`resolve_base(repo, explicit, anchor)` 三参在 Task 1 定义、Task 3/4 一致使用；`run_view_command` 返回 `ViewCommandOutput`，`command_frame` 消费它产 `ViewFrame`；`GitDiffFocus` 的 `command` / `frame` 字段在 Task 3 定义、Task 4 使用。

**风险点（执行者留意）：** Task 3 的 RPC 测试与 Task 4 的 `ShowGitDiffTool::new` 签名都依赖 `Harness` 与 `build_runtime_tooling` 的既有形状——以仓库现状为准，若夹具 API 与计划不符，按其真实 API 适配而**不**新增测试专用生产 API。Task 9 第二条测试若 jsdom 交互不可达，退化为组件级断言 + App 侧请求断言（已在步骤里说明）。
