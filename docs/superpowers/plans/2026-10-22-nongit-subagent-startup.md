# 子 Agent 非 Git 目录启动策略实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `read_only` 子 agent 能在非 git 目录启动，同时把非 git 目录下 `coding` 子 agent 的拒绝变成父 LLM 可解释、可询问用户、可照做的结构化错误。

**Architecture:** 三层改动。(1) 仓库层的恢复上下文校验按 `checkpoint_json.kind` 放宽，允许 read-only 在无 git 位置准入；(2) 运行期在写任务行之前对 coding 做"必须有 git HEAD"的前置检查，read-only 跳过 workdir 的 git 校验；(3) 工具层把内部错误翻译成带 `remedy` 与 `ask_user` 的结构化 JSON，并同步模型提示词。

**Tech Stack:** Rust（`yi-agent-core`、`yi-agent-store`、`yi-agent-subagent`、`yi-agent`）、`rusqlite`、`serde_json`。

## Global Constraints

- 不得在 `main` 分支上提交；所有改动在 `.worktrees/feat-nongit-subagent` 内完成。
- 提交前 `cd yi-agent-rs && cargo fmt --all`。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。
- 测试按 crate 分别跑，不要 `cargo test --workspace`（见 AGENTS.md 的 OOM 说明）。
- **git 环境下的行为必须逐字不变**：worktree 语义、dirty 重试提示、交付 commit 校验都不改。
- 不自动 `git init`：创建动作由用户同意后由 LLM 通过既有 `bash` 工具发起。
- 不新增 `spawn_agent` 参数，不新增工具，不做 daemon 侧持久化的"用户已拒绝"状态。
- `WorkerWorkspace` 与 `WorkerRecoveryContext` 不新增字段（IPC 报文不扩），判据走 `checkpoint_json`。
- coding 的 git 判据必须是"`--show-toplevel` 成功 **且** `HEAD` 解析成功"；只判其一会让空仓库跑到交付阶段才失败（已在 `/tmp` 实测确认）。
- 常量 `CODING_REQUIRES_GIT_PREFIX` 只有一个定义点：`yi-agent-rs/crates/yi-agent-store/src/runtime.rs`，值为 `"coding_requires_git:"`。

---

### Task 1: 新增 `CodingRequiresGit` 错误与 IPC 映射

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:69-98`（`RuntimeCoordinatorError`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:2943`（`ipc_error_message`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:2964`（`ipc_error_code`）
- Test: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（新增内联 `mod`，文件末尾）

**Interfaces:**
- Produces:
  - `pub const CODING_REQUIRES_GIT_PREFIX: &str = "coding_requires_git:";`（`runtime` 模块）
  - `RuntimeCoordinatorError::CodingRequiresGit { workdir: PathBuf, reason: CodingRequiresGitReason }`
  - `pub enum CodingRequiresGitReason { NotARepository, NoInitialCommit }`，`impl CodingRequiresGitReason { pub fn as_str(&self) -> &'static str }` 返回 `"not_a_repository"` / `"no_initial_commit"`

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` 末尾追加：

```rust
#[cfg(test)]
mod coding_requires_git_mapping_tests {
    use super::*;
    use crate::runtime::{CodingRequiresGitReason, RuntimeCoordinatorError};

    #[test]
    fn coding_requires_git_is_a_validation_error_with_a_stable_prefix() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit {
            workdir: std::path::PathBuf::from("/tmp/notes"),
            reason: CodingRequiresGitReason::NotARepository,
        });

        // The client must be able to tell "your environment is not ready" apart
        // from "the daemon is broken": Internal would send users chasing a
        // crashed daemon that is running fine.
        assert_eq!(ipc_error_code(&error), IpcErrorCode::Validation);

        let message = ipc_error_message(&error).expect("a validation rejection carries a message");
        assert!(
            message.starts_with(crate::runtime::CODING_REQUIRES_GIT_PREFIX),
            "the tool layer keys off this prefix, got {message:?}"
        );
        assert!(message.contains("not_a_repository"), "{message}");
        assert!(message.contains("/tmp/notes"), "{message}");
    }

    #[test]
    fn the_missing_commit_reason_is_distinguishable_at_the_wire() {
        let error = IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit {
            workdir: std::path::PathBuf::from("/tmp/notes"),
            reason: CodingRequiresGitReason::NoInitialCommit,
        });
        let message = ipc_error_message(&error).expect("message");
        assert!(message.contains("no_initial_commit"), "{message}");
        assert!(
            !message.contains("not_a_repository"),
            "the two reasons must not be conflated: {message}"
        );
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib coding_requires_git_mapping_tests
```

Expected: 编译失败，`CodingRequiresGitReason` / `CODING_REQUIRES_GIT_PREFIX` / `CodingRequiresGit` 均不存在。

- [ ] **Step 3: 实现**

在 `runtime.rs` 的 `RuntimeCoordinatorError` 之后追加：

```rust
/// Stable wire prefix for "a coding child needs a git worktree with a HEAD".
///
/// The daemon cannot hand the model a JSON object through `IpcResponse::Error`
/// without changing the protocol, so the rejection carries this prefix and the
/// tool layer rebuilds the structured payload from it. One definition, one test.
pub const CODING_REQUIRES_GIT_PREFIX: &str = "coding_requires_git:";

/// Which git precondition a coding spawn failed.
///
/// Kept apart so the remedy can omit `git init` for a repository that exists but
/// has no commit: telling a user to re-initialize would be both wrong and
/// destructive-sounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingRequiresGitReason {
    /// The directory is not inside any git repository.
    NotARepository,
    /// The repository exists but has no commit, so `rev-parse HEAD` fails and
    /// there is no base the runtime could recover into.
    NoInitialCommit,
}

impl CodingRequiresGitReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotARepository => "not_a_repository",
            Self::NoInitialCommit => "no_initial_commit",
        }
    }
}
```

在 `RuntimeCoordinatorError` 的最后一个变体（`ForkUpload`）之后追加：

```rust
    #[error(
        "{prefix}{reason} at {workdir}",
        prefix = CODING_REQUIRES_GIT_PREFIX,
        reason = reason.as_str(),
        workdir = workdir.display()
    )]
    CodingRequiresGit {
        workdir: std::path::PathBuf,
        reason: CodingRequiresGitReason,
    },
```

在 `ipc.rs` 的 `ipc_error_message` 的 `_ => None` 之前追加：

```rust
        // Carries a stable prefix plus the reason and the path, so the tool layer
        // can rebuild a structured rejection the model can act on.
        IpcError::Runtime(error @ RuntimeCoordinatorError::CodingRequiresGit { .. }) => {
            Some(error.to_string())
        }
```

在 `ipc_error_code` 的 `ForkTooLarge | ForkUpload => Validation` 分支里加入新变体：

```rust
        IpcError::Runtime(RuntimeCoordinatorError::CodingRequiresGit { .. })
        | IpcError::Runtime(RuntimeCoordinatorError::ForkTooLarge { .. })
        | IpcError::Runtime(RuntimeCoordinatorError::ForkUpload(_)) => IpcErrorCode::Validation,
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib coding_requires_git_mapping_tests
```

Expected: 2 passed。

- [ ] **Step 5: 确认没有引入全量回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib
```

Expected: 全绿（新增 2 个测试，其余不变）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(store): 新增 coding_requires_git 拒绝与 IPC 映射"
```

---

### Task 2: coding 的 git 前置检查（写任务行之前）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:1490-1530`（`spawn_child_with_objective` 开头）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（新增私有函数 `coding_git_precondition`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: Task 1 的 `RuntimeCoordinatorError::CodingRequiresGit`、`CodingRequiresGitReason`。
- Produces: `fn coding_git_precondition(workdir: &Path) -> Result<(), CodingRequiresGitReason>`（`runtime.rs` 私有自由函数，供 Task 5 的 `register_workdir` 之外的路径复用）

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs` 末尾追加（该文件已有 `TempDir`、`RuntimeCoordinator::open`、`MessageRecordingFactory`、`RuntimeRepository::open` 的既有用法，照抄邻近测试的 setup 风格）：

```rust
/// A coding child in a plain directory must be refused before anything durable
/// exists: a queued row the caller cannot see is a ghost task that never runs
/// and never gets cleaned up.
#[tokio::test]
async fn non_git_coding_child_is_refused_without_a_queued_row() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let plain = directory.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let service = Arc::new(GitWorkspaceService::new(repository_root.clone()));
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(service.clone() as Arc<dyn WorkerWorkspaceProvider>),
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let before = RuntimeRepository::open(&database).unwrap().queued_task_count().unwrap();
    let error = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "change the plain directory".into(),
            ChildWriteMode::Coding,
            None,
            Some(plain.clone()),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();
    let after = RuntimeRepository::open(&database).unwrap().queued_task_count().unwrap();

    match error {
        RuntimeCoordinatorError::CodingRequiresGit { workdir, reason } => {
            assert_eq!(workdir, plain.canonicalize().unwrap_or(plain));
            assert_eq!(reason, CodingRequiresGitReason::NotARepository);
        }
        other => panic!("expected a coding git rejection, got {other}"),
    }
    assert_eq!(after, before, "a refused spawn must not leave a queued row");
    assert!(
        factory.starts.lock().unwrap().is_empty()
            || factory.starts.lock().unwrap().len() == 1,
        "only the root may have started"
    );
}

/// `git init` alone is not enough: `rev-parse --show-toplevel` succeeds while
/// `rev-parse HEAD` fails, so the child would start and only fail later at
/// delivery. Catch it at the spawn instead.
#[tokio::test]
async fn an_empty_repository_is_refused_for_missing_head() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let repository_root = directory.path().join("repo");
    std::fs::create_dir(&repository_root).unwrap();
    initialize_git_repository(&repository_root);
    let empty = directory.path().join("empty-repo");
    std::fs::create_dir(&empty).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&empty)
        .status()
        .unwrap()
        .success());
    let service = Arc::new(GitWorkspaceService::new(repository_root.clone()));
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(service.clone() as Arc<dyn WorkerWorkspaceProvider>),
        workspace_registry: Some(service as Arc<dyn WorkerWorkspaceRegistry>),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let error = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "change the empty repository".into(),
            ChildWriteMode::Coding,
            None,
            Some(empty),
            None,
            None,
            None,
        )
        .await
        .unwrap_err();

    match error {
        RuntimeCoordinatorError::CodingRequiresGit { reason, .. } => {
            assert_eq!(reason, CodingRequiresGitReason::NoInitialCommit);
        }
        other => panic!("expected no_initial_commit, got {other}"),
    }
}
```

同时在该文件的 `use` 区确认已导入 `ChildWriteMode`、`RuntimeCoordinatorError`、`CodingRequiresGitReason`。`ChildWriteMode` 已在 `yi_agent_core::subagent::task` 的导入列表中（`:11-15`），无需改动；`RuntimeCoordinatorError` 目前只以全路径使用过一次（`:1986`），在文件顶部加一行：

```rust
use yi_agent_store::runtime::RuntimeCoordinatorError;
```

`CodingRequiresGitReason` 同样加在文件顶部：

```rust
use yi_agent_store::runtime::CodingRequiresGitReason;
```

注意第二个测试里 `git init -q` 之后**不要**提交：它就是要构造"有仓库、无 HEAD"的状态。同时该测试对 `git commit` 不设身份，因为不提交；若在 CI 上 `git init` 因缺少默认分支名而告警，加 `-b main` 与既有 `initialize_git_repository` 保持一致：

```rust
        .args(["init", "-q", "-b", "main"])
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator non_git_coding_child_is_refused_without_a_queued_row
```

Expected: 失败——当前实现会先写行，然后因 `observe_workdir` 报 `workdir is not inside a git worktree` 而返回 `Supervisor`，既有 queued 行残留，`after > before`。

- [ ] **Step 3: 实现前置检查**

在 `runtime.rs` 的自由函数区（`git_capture` 附近，`:4234`）追加：

```rust
/// Whether `workdir` can host a coding child: it must be inside a git worktree
/// *and* that worktree must already have a commit.
///
/// The second half is not redundant. `git init` alone makes `--show-toplevel`
/// succeed while `rev-parse HEAD` still fails, so a check on the toplevel only
/// would admit the child, let it run its whole task, and then fail it at
/// delivery with an opaque `Git workspace error`. Checking HEAD here turns that
/// into an immediate, actionable refusal.
fn coding_git_precondition(workdir: &std::path::Path) -> Result<(), CodingRequiresGitReason> {
    let run = |args: &[&str]| -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(workdir)
            .output()
            .is_ok_and(|output| output.status.success())
    };
    if !run(&["rev-parse", "--show-toplevel"]) {
        return Err(CodingRequiresGitReason::NotARepository);
    }
    if !run(&["rev-parse", "HEAD"]) {
        return Err(CodingRequiresGitReason::NoInitialCommit);
    }
    Ok(())
}
```

在 `spawn_child_with_objective` 中，`ensure_admitting()` 之后、写入任务行之前插入检查。位置必须紧跟 `workdir` 解析之后：

```rust
        // Resolved before the check so a relative workdir is judged against the
        // parent's position, exactly as it will be stored.
        let workdir = workdir.map(|workdir| self.resolve_parent_workdir(parent, &workdir));
        // A coding child needs a recoverable git position. Checked before the
        // task row is written: a refusal must not leave a queued ghost task.
        if workspace_mode == ChildWriteMode::Coding {
            let target = workdir
                .clone()
                .or_else(|| self.remembered_task_position(parent).map(|p| p.path));
            if let Some(target) = target {
                coding_git_precondition(&target).map_err(|reason| {
                    RuntimeCoordinatorError::CodingRequiresGit {
                        workdir: target,
                        reason,
                    }
                })?;
            }
        }
```

注意：`workdir` 的解析原本就存在（原代码在 `let supervisor = self.supervisor(session)?;` 之后）。把这段移动到解析之后、`thread_id` 解析之前即可，不改变解析本身。

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator refused
```

Expected: 2 passed。

- [ ] **Step 5: 确认既有 coding 路径未回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator
```

Expected: 全绿。若 `coding_worktree_is_prepared_and_delivered` 一类既有测试失败，说明前置检查把合法 worktree 也拒了，检查 `coding_git_precondition` 的 `git` 调用是否用了 `current_dir`。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(store): coding 子任务在写任务行前校验 git HEAD"
```

---

### Task 3: `register_workdir` 的判据改为"必须有 HEAD"

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:456-498`（`register_workdir`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（既有 `mod tests`）

**Interfaces:**
- Consumes: 无（该 crate 不依赖 `yi-agent-store` 的 runtime 常量，避免循环依赖；前缀与 reason 字符串在此重复定义一次并由测试钉住一致性）。
- Produces: `register_workdir` 在两类失败上返回 `WorkerError::Startup("coding_requires_git:<reason> at <path>")`。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 的 `mod tests` 中，紧邻 `observe_workdir_resolves_a_prepared_directory_and_rejects_a_missing_one`（`:3824`）追加：

```rust
    /// The tool layer keys off this prefix to rebuild a structured rejection, so
    /// it must survive here even though this crate cannot import the store's
    /// constant (that would be a dependency cycle).
    const CODING_REQUIRES_GIT_PREFIX: &str = "coding_requires_git:";

    #[test]
    fn register_workdir_reports_a_plain_directory_as_not_a_repository() {
        let directory = tempfile::TempDir::new().unwrap();
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let plain = directory.path().join("plain");
        std::fs::create_dir(&plain).unwrap();

        let error = service.register_workdir(&plain).unwrap_err().to_string();
        assert!(error.starts_with(CODING_REQUIRES_GIT_PREFIX), "{error}");
        assert!(error.contains("not_a_repository"), "{error}");
    }

    /// `git init` without a commit leaves `--show-toplevel` working while
    /// `rev-parse HEAD` fails. Admitting it here would defer the failure to the
    /// delivery step, after the child already did all its work.
    #[test]
    fn register_workdir_reports_an_empty_repository_as_missing_a_commit() {
        let directory = tempfile::TempDir::new().unwrap();
        let service = DaemonWorkspaceService::new(directory.path().to_path_buf());
        let empty = directory.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        // `-b main` matches this file's `initialize_git_repository` helper. No
        // commit is made: that is exactly the state under test.
        assert!(std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(&empty)
            .status()
            .unwrap()
            .success());

        let error = service.register_workdir(&empty).unwrap_err().to_string();
        assert!(error.starts_with(CODING_REQUIRES_GIT_PREFIX), "{error}");
        assert!(error.contains("no_initial_commit"), "{error}");
    }
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib register_workdir_reports_
```

Expected: 两个测试都失败——当前错误是 `workdir is not inside a git worktree`，不含前缀。

- [ ] **Step 3: 实现**

把 `register_workdir` 的 git 判据段落（`lib.rs:478-486`）替换为：

```rust
        // A coding position must be a git worktree *with a HEAD*. `git init`
        // alone passes `--show-toplevel` while `rev-parse HEAD` fails, so
        // accepting it would defer the failure to delivery, after the child has
        // already run its whole task.
        if git_output(&canonical, &["rev-parse", "--show-toplevel"]).is_none() {
            return Err(WorkerError::Startup(format!(
                "coding_requires_git:not_a_repository at {}",
                workdir.display()
            )));
        }
        if git_output(&canonical, &["rev-parse", "HEAD"]).is_none() {
            return Err(WorkerError::Startup(format!(
                "coding_requires_git:no_initial_commit at {}",
                workdir.display()
            )));
        }
```

保留其上方的 `if !canonical.is_dir()` 分支不变（目录不存在仍是独立的用户错误）。

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib register_workdir_reports_
```

Expected: 2 passed。

- [ ] **Step 5: 确认既有 subagent 行为未回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: 全绿。特别关注 `observe_workdir_resolves_a_prepared_directory_and_rejects_a_missing_one` 仍通过（它断言"目录不存在"路径，与本次改动的两个 git 分支无关）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(subagent): 工作目录校验要求 git HEAD 并输出可识别拒绝"
```

---

### Task 4: read-only 在非 git 位置准入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs:3767-3798`（`validate_recovery_context`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:409-428`（`recovery_context_for_workspace`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:704-718`（`recovery_context` / `recovery_context_for` 传 mode）
- Test: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`（新增内联 `mod`）

**Interfaces:**
- Consumes: 无。
- Produces:
  - `recovery_context_for_workspace(&self, workspace: PathBuf, workspace_mode: ChildWriteMode) -> WorkerRecoveryContext`
  - 非 git 的 read-only 上下文：`worktree_lease: None`，`checkpoint_json.kind == "read_only_checkpoint"`。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/src/repository.rs` 末尾追加：

```rust
#[cfg(test)]
mod read_only_checkpoint_tests {
    use super::*;

    fn context(kind: &str) -> WorkerRecoveryContext {
        WorkerRecoveryContext {
            workspace_lease_id: Some("workspace:/tmp/plain".into()),
            worktree_lease: None,
            checkpoint_json: format!(r#"{{"kind":"{kind}"}}"#),
            tool_state_json: r#"{"state":"available"}"#.into(),
        }
    }

    /// A read-only worker runs in place, so it has no worktree to recover into.
    /// The checkpoint kind is what says so; an absent lease alone must not be
    /// read as permission, or a coding worker that forgot its lease would slip
    /// through.
    #[test]
    fn a_read_only_checkpoint_admits_a_worker_without_a_worktree_lease() {
        validate_recovery_context(&context("read_only_checkpoint"))
            .expect("a read-only position is a legitimate place to run");
    }

    #[test]
    fn an_unknown_checkpoint_kind_still_requires_a_worktree_lease() {
        let error = validate_recovery_context(&context("worker_admission"))
            .expect_err("an ordinary admission must still name a worktree");
        assert!(
            error.to_string().contains("worktree lease is missing"),
            "{error}"
        );
    }
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib read_only_checkpoint_tests
```

Expected: 第一个测试失败（`worktree lease is missing`），第二个通过。

- [ ] **Step 3: 放宽仓库层校验**

把 `validate_recovery_context`（`repository.rs:3767`）开头到 `worktree` 校验的段落改为：

```rust
fn validate_recovery_context(context: &WorkerRecoveryContext) -> Result<(), RepositoryError> {
    let workspace = context
        .workspace_lease_id
        .as_deref()
        .filter(|lease| lease.starts_with("workspace:") && lease.len() > "workspace:".len())
        .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
            reason: "workspace lease is missing".into(),
        })?;
    // A read-only worker runs in place, so there is no worktree to recover into.
    // The checkpoint kind is the explicit permission: it is never inferred from
    // an absent lease, because a coding worker that lost its lease would then
    // be admitted as if it were read-only.
    let checkpoint: serde_json::Value =
        serde_json::from_str(&context.checkpoint_json).map_err(|error| {
            RepositoryError::InvalidWorkerRecoveryContext {
                reason: format!("checkpoint evidence is invalid JSON: {error}"),
            }
        })?;
    let is_read_only = checkpoint.get("kind").and_then(serde_json::Value::as_str)
        == Some("read_only_checkpoint");
    if !is_read_only {
        let worktree = context
            .worktree_lease
            .as_deref()
            .filter(|lease| lease.starts_with("worktree:") && lease.len() > "worktree:".len())
            .ok_or_else(|| RepositoryError::InvalidWorkerRecoveryContext {
                reason: "worktree lease is missing".into(),
            })?;
        if worktree == "worktree:" {
            return Err(RepositoryError::InvalidWorkerRecoveryContext {
                reason: "resource lease key is empty".into(),
            });
        }
    }
    if workspace == "workspace:" {
        return Err(RepositoryError::InvalidWorkerRecoveryContext {
            reason: "resource lease key is empty".into(),
        });
    }
    serde_json::from_str::<serde_json::Value>(&context.tool_state_json).map_err(|error| {
        RepositoryError::InvalidWorkerRecoveryContext {
            reason: format!("tool-state evidence is invalid JSON: {error}"),
        }
    })?;
    Ok(())
}
```

注意：原函数末尾对 `checkpoint_json` 的重复 `serde_json::from_str` 校验已被上面的 `checkpoint` 解析取代，删除重复那一行。

- [ ] **Step 4: 让工厂按 mode 生成 read-only 上下文**

把 `recovery_context_for_workspace`（`lib.rs:409`）签名与实现改为：

```rust
    fn recovery_context_for_workspace(
        &self,
        workspace: PathBuf,
        workspace_mode: ChildWriteMode,
    ) -> WorkerRecoveryContext {
        let git_root = git_output(&workspace, &["rev-parse", "--show-toplevel"]);
        let git_head = git_output(&workspace, &["rev-parse", "HEAD"]);
        let git_status = git_command_output(&workspace, &["status", "--porcelain"]).ok();
        let tool_names = self.worker_tool_names_for_workspace(workspace.clone());
        // A read-only worker runs in place: when the position is not a git
        // worktree there is nothing to recover into, and the checkpoint says so
        // explicitly rather than leaving an empty lease to be misread.
        let (worktree_lease, checkpoint_kind) = match (git_root, workspace_mode) {
            (Some(directory), _) => (Some(format!("worktree:{directory}")), "worker_admission"),
            (None, ChildWriteMode::ReadOnly) => (None, "read_only_checkpoint"),
            (None, ChildWriteMode::Coding) => (None, "worker_admission"),
        };
        WorkerRecoveryContext {
            workspace_lease_id: Some(format!("workspace:{}", workspace.display())),
            worktree_lease,
            checkpoint_json: json!({
                "kind": checkpoint_kind,
                "git_head": git_head,
                "git_status": git_status,
            })
            .to_string(),
            tool_state_json: json!({
                "state": "available",
                "registered_tools": tool_names,
            })
            .to_string(),
        }
    }
```

同步修改两个调用点（`lib.rs:706` 与 `:714`）：

```rust
    fn recovery_context(&self) -> WorkerRecoveryContext {
        self.recovery_workspace
            .clone()
            .map(|workspace| {
                self.recovery_context_for_workspace(workspace, ChildWriteMode::Coding)
            })
            .unwrap_or_default()
    }

    fn recovery_context_for(&self, request: &WorkerStart) -> WorkerRecoveryContext {
        request
            .workspace
            .as_ref()
            .map(|workspace| {
                self.recovery_context_for_workspace(
                    workspace.path.clone(),
                    request.workspace_mode,
                )
            })
            .unwrap_or_else(|| self.recovery_context())
    }
```

`recovery_context()` 传 `Coding` 的理由要写进注释：它是无请求参数时的兜底，保持与今天完全一致（今天它总是按 git 探测产出 `worker_admission`）。

- [ ] **Step 5: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib read_only_checkpoint_tests && cargo test -p yi-agent-subagent --lib
```

Expected: 2 passed + subagent 全绿。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat: read-only 子任务允许在非 git 位置准入"
```

---

### Task 5: read-only 跳过 workdir 的 git 校验，端到端放行

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:1621-1639`（`observe_workdir` 调用点）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs`

**Interfaces:**
- Consumes: Task 4 的 read-only 恢复上下文。
- Produces: 无新接口；`spawn_child_with_objective` 对 `ReadOnly` 不再调用 `observe_workdir`。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs` 中，紧邻 `read_only_child_runs_in_place_without_a_workspace_row`（`:3163`）追加：

```rust
/// A read-only child in a plain directory must start and finish. It never
/// writes, so there is nothing to recover or deliver, and the git requirement
/// that gates a coding child does not apply to it.
#[tokio::test]
async fn non_git_read_only_child_starts_and_completes() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let plain = directory.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(NonGitWorkspaceService {
            repository_root: plain.clone(),
        })),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory.clone()).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    let child = coordinator.spawn_child(&session, &root).await.unwrap();
    coordinator
        .start_worker(&session, &child)
        .await
        .expect("a read-only child has no worktree to recover into");

    // Reporting a plain text result is how a read-only worker finishes; the
    // supervisor turns it into `Completed`, never `failed`. The handle list is
    // the file's established seam (see
    // `one_supervisors_failure_does_not_discard_another_sessions_completion`).
    let handles = factory.handles.lock().unwrap().clone();
    assert_eq!(handles.len(), 2, "root plus the read-only child");
    handles[1].report_completed("the investigation finished");
    coordinator.reconcile_worker_events().await.unwrap();

    let state = RuntimeRepository::open(&database)
        .unwrap()
        .task_state(&child)
        .unwrap();
    assert_eq!(state, "completed", "a read-only child must not fail");
}
```

若 `handles[1]` 的顺序不稳定，按 `task_id` 匹配：`factory.starts` 里每次 `WorkerStart` 都带 `task_id`，用 `factory.starts.lock().unwrap().iter().position(|s| s.task_id == child)` 找到下标再取对应 handle。这是测试接缝，不改变生产代码。

同时追加第二个测试，钉住"read-only 传了非 git workdir 也不失败"：

```rust
#[tokio::test]
async fn non_git_read_only_child_accepts_an_explicit_workdir() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let plain = directory.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    let factory = Arc::new(MessageRecordingFactory {
        workspace_service: Some(Arc::new(NonGitWorkspaceService {
            repository_root: plain.clone(),
        })),
        ..Default::default()
    });
    let coordinator = RuntimeCoordinator::open(&database, factory).unwrap();
    let session = coordinator.create_session().unwrap();
    let root = coordinator.root_task_id(&session).unwrap();
    coordinator.start_worker(&session, &root).await.unwrap();

    // Today this fails inside `observe_workdir` even though a read-only task
    // never uses the workdir it was handed.
    let child = coordinator
        .spawn_child_with_objective(
            &session,
            &root,
            "read the plain directory".into(),
            ChildWriteMode::ReadOnly,
            None,
            Some(plain),
            None,
            None,
            None,
        )
        .await
        .expect("a read-only child must not be gated on git");
    coordinator.start_worker(&session, &child).await.unwrap();
}
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator non_git_read_only_child
```

Expected: 两个都失败——第一个在 `start_worker` 处报 `worktree lease is missing`，第二个在 `spawn_child_with_objective` 处报 `workdir is not inside a git worktree`。

- [ ] **Step 3: 实现**

把 `spawn_child_with_objective` 中的 `observe_workdir` 段（`runtime.rs:1621-1639`）改为：

```rust
        // Auto-registration: the parent prepares the directory (`git worktree
        // add`) and hands over its path; the daemon only records that position.
        // A bad path fails the spawn rather than deferring to worker start, so
        // the parent learns immediately that its workdir was not usable.
        //
        // Read-only children are exempt: `prepare_task_workspace` ignores a
        // read-only task's `workdir` entirely and inherits the nearest
        // ancestor's position, so validating a directory that will never be
        // used only manufactures a failure unrelated to what the task does.
        if workspace_mode == ChildWriteMode::Coding
            && let Some(workdir) = workdir.as_deref()
        {
            let registry = self.factory.worker_workspace_registry().ok_or_else(|| {
                RuntimeCoordinatorError::Supervisor(
                    "coding child requires a prepared-workspace registry".into(),
                )
            })?;
            let observed = registry
                .observe_workdir(workdir)
                .map_err(|error| RuntimeCoordinatorError::Supervisor(error.to_string()))?;
            registry.register_prepared(&observed);
        }
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator non_git_read_only_child
```

Expected: 2 passed。

- [ ] **Step 5: 确认 git 路径未回归**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator
```

Expected: 全绿，特别是 `read_only_child_runs_in_place_without_a_workspace_row` 与所有 coding workdir 测试。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat: read-only 子任务不再校验 workdir 的 git 归属"
```

---

### Task 6: 工具层把拒绝翻译成结构化 JSON

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`spawn_outcome` 及其新助手）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`mod tests`）

**Interfaces:**
- Consumes: 稳定前缀 `coding_requires_git:`（Task 1/3）。
- Produces: `fn coding_requires_git_payload(message: &str) -> Option<String>`，返回可直接交给模型的 JSON 字符串。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-subagent/src/lib.rs` 的 `mod tests` 中追加：

```rust
    /// The model must be able to act on a refusal: what is wrong, what to run,
    /// and what to ask the user. A bare "validation error" makes it guess.
    #[test]
    fn a_coding_requires_git_rejection_becomes_an_actionable_payload() {
        let payload = coding_requires_git_payload(
            "coding_requires_git:not_a_repository at /tmp/notes",
        )
        .expect("the prefix marks a rejectable refusal");

        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["code"], "coding_requires_git");
        assert_eq!(value["workdir"], "/tmp/notes");
        assert_eq!(value["reason"], "not_a_repository");
        assert_eq!(value["remedy"]["requires_initial_commit"], true);
        let commands = value["remedy"]["commands"].as_array().unwrap();
        assert!(commands.iter().any(|c| c == "git init"));
        assert!(
            value["ask_user"]
                .as_str()
                .is_some_and(|text| !text.trim().is_empty()),
            "the model needs a sentence it can put to the user"
        );
    }

    /// A repository that exists but has no commit must not be told to
    /// re-initialize: that would be wrong and sounds destructive.
    #[test]
    fn a_missing_commit_omits_git_init_from_the_remedy() {
        let payload = coding_requires_git_payload(
            "coding_requires_git:no_initial_commit at /tmp/notes",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["reason"], "no_initial_commit");
        let commands = value["remedy"]["commands"].as_array().unwrap();
        assert!(
            !commands.iter().any(|c| c == "git init"),
            "an existing repository must not be re-initialized: {commands:?}"
        );
        assert!(commands.iter().any(|c| c == "git commit -m \"initial commit\""));
    }

    #[test]
    fn an_unrelated_rejection_is_left_alone() {
        assert!(coding_requires_git_payload("daemon rejected spawn request: conflict").is_none());
    }
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: 编译失败，`coding_requires_git_payload` 不存在。

- [ ] **Step 3: 实现**

在 `spawn_outcome` 之前追加：

```rust
/// The stable prefix the daemon puts on a git-precondition refusal. Mirrors
/// `yi_agent_store::runtime::CODING_REQUIRES_GIT_PREFIX`; this crate cannot
/// import it without a dependency cycle, so a test pins the spelling.
const CODING_REQUIRES_GIT_PREFIX: &str = "coding_requires_git:";

/// Rebuilds a structured rejection from the daemon's prefixed message.
///
/// The IPC protocol carries only `code` and `message`, so the model would
/// otherwise see `validation: coding_requires_git:...` and have to parse a
/// sentence. This turns it into the facts a model can act on: which directory,
/// why, what to run, and what to ask the user.
fn coding_requires_git_payload(message: &str) -> Option<String> {
    let rest = message.strip_prefix(CODING_REQUIRES_GIT_PREFIX)?;
    // Shape: "<reason> at <path>". The path is last so a directory containing
    // " at " cannot be mistaken for part of the reason.
    let (reason, workdir) = match rest.rsplit_once(" at ") {
        Some((reason, workdir)) => (reason.trim(), workdir.trim()),
        None => return None,
    };
    let reason = match reason {
        "not_a_repository" | "no_initial_commit" => reason,
        _ => return None,
    };
    // `git init` only makes sense when there is no repository yet. Telling a
    // user to re-initialize an existing repo is wrong and sounds destructive.
    let commands: Vec<&str> = if reason == "not_a_repository" {
        vec![
            "git init",
            "git add -A",
            "git commit -m \"initial commit\"",
        ]
    } else {
        vec!["git add -A", "git commit -m \"initial commit\""]
    };
    Some(
        json!({
            "code": "coding_requires_git",
            "workdir": workdir,
            "reason": reason,
            "remedy": {
                "commands": commands,
                // Stated explicitly because `git init` alone still fails: with
                // no commit, `rev-parse HEAD` has nothing to point at.
                "requires_initial_commit": true,
                "note": "a repository with no commits still fails: the runtime needs a HEAD to recover into",
            },
            "ask_user": format!(
                "The coding subagent needs version control in {workdir}. I can create a git repository there and make an initial commit. Do you want me to?"
            ),
        })
        .to_string(),
    )
}
```

修改 `spawn_outcome`，把两条拒绝分支都过一遍翻译：

```rust
        Ok(other) => {
            abort_unconsumed_fork(socket, fork_token);
            let message = format_ipc_rejection("spawn request", &other);
            ToolResult::error(
                coding_requires_git_payload(&message).unwrap_or(message),
            )
        }
        Err(error) => {
            abort_unconsumed_fork(socket, fork_token);
            let message = format!("daemon is unavailable: {error}");
            ToolResult::error(
                coding_requires_git_payload(&message).unwrap_or(message),
            )
        }
```

注意：`format_ipc_rejection` 产出的字符串形如 `daemon rejected spawn request: validation: coding_requires_git:not_a_repository at /x`，前缀不在开头。因此 `coding_requires_git_payload` 需要能处理前缀不在首位的情形。改为按"包含前缀"定位：

```rust
fn coding_requires_git_payload(message: &str) -> Option<String> {
    let start = message.find(CODING_REQUIRES_GIT_PREFIX)?;
    let rest = &message[start + CODING_REQUIRES_GIT_PREFIX.len()..];
    // ...其余不变
}
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: 3 passed。

- [ ] **Step 5: 确认 subagent 全绿**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib
```

Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "feat(subagent): spawn_agent 把 git 前置拒绝翻成可执行的结构化错误"
```

---

### Task 7: 同步模型提示词（工具文案 + 系统提示词）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1949`（`spawn_agent_description`）、`:1957-1983`（`spawn_agent_schema`）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:218-229`（`default_system_prompt`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（`spawn_agent_schema_pins_the_wire_contract`）、`yi-agent-rs/crates/yi-agent/src/main.rs`（系统提示词断言）

**Interfaces:**
- Consumes: 拒绝码 `coding_requires_git`（Task 6）。
- Produces: 无代码接口；纯文案契约。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-subagent/src/lib.rs` 的 `spawn_agent_schema_pins_the_wire_contract`（`:2420`）中，先**替换**既有的精确字符串断言：

```rust
        // 旧断言：assert_eq!(spawn_agent_description(), "Create an asynchronously scheduled direct child agent.");
```

因为 Task 7 会扩展这段文案，精确相等会失败。改用"包含"断言并补新契约：

```rust
        let description = spawn_agent_description();
        assert!(
            description.starts_with("Create an asynchronously scheduled direct child agent."),
            "the first sentence is the tool's identity and must survive: {description}"
        );
        assert!(
            description.contains("coding_requires_git"),
            "the model knows this code only if the tool description names it: {description}"
        );
        let workdir = schema["properties"]["workdir"]["description"]
            .as_str()
            .expect("workdir description");
        assert!(
            workdir.contains("git worktree") || workdir.contains("git repository"),
            "the model must know coding needs git: {workdir}"
        );
        assert!(
            workdir.contains("commit"),
            "a repository with no commit still fails, so the description must say so: {workdir}"
        );
```

在 `yi-agent-rs/crates/yi-agent/src/main.rs` 的既有测试 `default_system_prompt_requires_a_workdir_for_isolation`（`:3586`）中追加：

```rust
        assert!(
            prompt.contains("coding_requires_git"),
            "the system prompt must tell the model to ask the user when a coding child is refused"
        );
        assert!(
            prompt.contains("read_only"),
            "the fallback after a refusal is a read-only child, which must be discoverable here"
        );
```

- [ ] **Step 2: 运行测试，确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib spawn_agent_schema_pins_the_wire_contract && cargo test -p yi-agent --bin yi-agent default_system_prompt_requires_a_workdir_for_isolation
```

Expected: 两个都失败（文案尚未包含 `coding_requires_git`）。

- [ ] **Step 3: 实现**

把 `spawn_agent_description`（`lib.rs:1949`）改为：

```rust
pub(crate) fn spawn_agent_description() -> &'static str {
    "Create an asynchronously scheduled direct child agent. A 'coding' child \
     needs a git worktree with at least one commit; when the spawn is refused \
     with 'coding_requires_git', ask the user whether to create one there, and \
     do not retry a coding spawn in this session once they decline."
}
```

把 `spawn_agent_schema` 中 `workdir` 的描述改为：

```rust
            "workdir": {
                "type": "string",
                "description": "Directory the child works in. Required for 'coding': it must be inside a git worktree that already has a commit. Create it yourself with `git worktree add <path> -b <branch>` first, or after the user agrees to initialize the project with `git init` plus an initial commit."
            },
```

在 `default_system_prompt()` 的 "Subagent integration" 段（`agent.rs:218`）末尾追加一条：

```markdown
- A 'coding' child needs a git worktree with at least one commit. When
  `spawn_agent` is refused with `coding_requires_git`, do not retry blindly:
  explain the situation to the user and ask whether you should create a git
  repository there. If they agree, run the commands under `remedy.commands`
  with `bash` and then retry once. If they decline, do not start another
  'coding' child in this session: use a `read_only` child, or do the work
  yourself in this conversation.
```

- [ ] **Step 4: 运行测试，确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-subagent --lib spawn_agent_schema_pins_the_wire_contract && cargo test -p yi-agent --bin yi-agent default_system_prompt_requires_a_workdir_for_isolation
```

Expected: 2 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "docs: spawn_agent 文案与系统提示词说明 coding 的 git 前置条件"
```

---

### Task 8: 更新契约测试与项目文档

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs:909`（`a_non_git_cwd_attaches_in_place_with_the_delegation_tools`）
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs:3525`（`an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`）
- Modify: `docs/project-management/subagent-runtime.md:60`
- Modify: `docs/project-management/yi-agent-subagent.md:97`（"边界"段）
- Modify: `docs/project-management/desktop.md:64`（已知限制段）

**Interfaces:**
- Consumes: Task 2/3/5 的行为。
- Produces: 无。

- [ ] **Step 1: 改写 app-server 契约测试**

把 `a_non_git_cwd_attaches_in_place_with_the_delegation_tools`（`rpc_tests.rs:909`）中"delegating is where a plain directory stops"之后的断言段替换为：

```rust
    // Read-only delegation works in a plain directory: it writes nothing, so
    // there is no worktree to recover into and no delivery to validate.
    let read_only = yi_agent_store::ipc::send_request(
        &attached.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: attached.attached_root.session_id.clone(),
            parent_task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
            objective: "a read-only investigation".into(),
            mode: Some("read_only".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("the runtime socket answers");
    assert!(
        matches!(read_only, yi_agent_store::ipc::IpcResponse::TaskSpawned { .. }),
        "a plain directory must still admit a read-only child, got {read_only:?}"
    );

    // Coding is where it stops: a commit is the recovery boundary a coding
    // child cannot work without. The refusal must be recognizable, not a bare
    // internal error, so the model can explain it and offer to initialize.
    let coding = yi_agent_store::ipc::send_request(
        &attached.socket_path,
        yi_agent_store::ipc::IpcRequest::SpawnApplicationChild {
            session_id: attached.attached_root.session_id.clone(),
            parent_task_id: attached.attached_root.task_id.clone(),
            capability: attached.attached_root.capability.clone(),
            objective: "change the plain directory".into(),
            mode: Some("coding".into()),
            model: None,
            workdir: None,
            thread_id: None,
            sandbox: None,
            fork_token: None,
        },
    )
    .expect("the runtime socket answers even when it refuses");
    let yi_agent_store::ipc::IpcResponse::Error { code, message } = coding else {
        panic!("a plain directory must refuse a coding child, got {coding:?}");
    };
    assert_eq!(code, yi_agent_store::ipc::IpcErrorCode::Validation);
    assert!(
        message
            .as_deref()
            .is_some_and(|text| text.contains("coding_requires_git")),
        "the refusal must be actionable, got {message:?}"
    );
```

- [ ] **Step 2: 更新 store 的既有拒绝测试**

`an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree`（`runtime_coordinator.rs:3525`）保留拒绝断言，把错误文案断言改为可操作信息：

```rust
    assert!(
        error.to_string().contains("coding_requires_git")
            && error.to_string().contains("not_a_repository"),
        "the refusal must name the problem in a form the model can act on: {error}"
    );
```

- [ ] **Step 3: 更新项目文档**

- `docs/project-management/subagent-runtime.md:60`：把"非 git session 降级为只读原地模式，coding child 以 `coding_requires_git_repository` 失败"改成当前真实行为：read-only 子任务在非 git 目录可启动并原地完成；coding 子任务在非 git 或空仓库下以 `coding_requires_git` 拒绝，工具层翻译为含 `remedy.commands` 与 `ask_user` 的结构化 JSON；用户拒绝后由模型保证本会话不再尝试（无 daemon 侧状态）。附验证命令：`cargo test -p yi-agent-store --test runtime_coordinator non_git_read_only_child && cargo test -p yi-agent-subagent --lib coding_requires_git`。
- `docs/project-management/yi-agent-subagent.md:97`（"边界"段）：删除"非 git 项目下工具在、调用被拒"的笼统说法，改为按 mode 分述，并写明"coding 需要 `git init` + 一次初始提交；只 init 不 commit 仍会被拒"这一实测结论。
- `docs/project-management/desktop.md:64`：把"此时 `spawn_agent` 会被 daemon 拒绝"改为"`read_only` 可正常委派；`coding` 会被拒绝并给出可执行的修复建议"，保留"建议在 GUI 里显式选一个 git 项目目录"的建议。

- [ ] **Step 4: 运行受影响的测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server a_non_git_cwd_attaches_in_place_with_the_delegation_tools && cargo test -p yi-agent-store --test runtime_coordinator an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree
```

Expected: 2 passed。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. && git add -A && git commit -m "test+docs: 非 git 目录下按 mode 分述委派行为"
```

---

### Task 9: 全量验证

**Files:** 无改动。

- [ ] **Step 1: 按 crate 跑受影响测试（不要 workspace 全量）**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib \
  && cargo test -p yi-agent-subagent --lib \
  && cargo test -p yi-agent-core --lib
```

Expected: 全绿。

- [ ] **Step 2: 跑集成测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator \
  && cargo test -p yi-agent-store --test runtime_ipc \
  && cargo test -p yi-agent-subagent --test attach_delegation
```

Expected: 全绿。

- [ ] **Step 3: 跑 app-server 与 CLI 的委派链路**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server a_git_project_gets_the_delegation_tools \
  && cargo test -p yi-agent-app-server a_non_git_cwd_attaches_in_place_with_the_delegation_tools \
  && cargo test -p yi-agent --bin yi-agent default_system_prompt
```

Expected: 全绿。

- [ ] **Step 4: 格式与 lint**

```bash
cd yi-agent-rs && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
```

Expected: 无输出、exit 0。

- [ ] **Step 5: 检查没有残留僵尸任务逻辑**

用一次真实调用确认：在非 git 临时目录跑 `spawn_agent` 的 coding 分支后，`runtime.sqlite` 的 `tasks` 表没有新增 `queued` 子任务行。可由 Task 2 的测试覆盖，此处只做人工复核：

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator non_git_coding_child_is_refused_without_a_queued_row -- --nocapture
```

Expected: PASS，且无残留行断言失败。
