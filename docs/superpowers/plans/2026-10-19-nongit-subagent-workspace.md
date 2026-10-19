# 子 agent 非 Git 降级实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 子 agent 的 `workdir` 不在 git 仓库内时不再让 spawn 失败，而是就地工作，并在 spawn 回执、任务列表、交付报告三处标注"改动没有版本控制保护"。

**Architecture:** 三层。(1) core 的 `WorkerWorkspace` 增加显式 `vcs` 标记（`Git` / `None`），默认 `Git` 保证旧 IPC 帧仍能解码；(2) 交付结构增加 `DeliveryKind::{Git, Unverified}`，`validate_for` 按 kind 分支；(3) 运行期按 `vcs` 分支：git 路径逐字不动，非 git 路径生成 `Unverified` 交付并附 `known_limitations`。

**Tech Stack:** Rust（`yi-agent-core`、`yi-agent-subagent`、`yi-agent-store`、`yi-agent-app-server`）、React + TypeScript（`desktop`）。

## Global Constraints

- 不得在 `main` 分支上提交；所有改动在 `.worktrees/<branch>` 内完成。
- 提交前 `cd yi-agent-rs && cargo fmt --all`。
- 提交信息用 conventional commits，不写 `Co-Authored-By`。
- 测试按 crate 分别跑，不要 `cargo test --workspace`（见 AGENTS.md 的 OOM 说明）。
- **git 环境下的行为必须逐字不变**：worktree 语义、dirty 重试提示、交付 commit 校验都不改。
- 不自动 `git init`，不做文件系统快照回滚（设计非目标）。
- `WorkerWorkspace` 会跨 IPC 序列化（`ipc.rs:476`），**不得**用"字段全空"推断非 git：`read_only_workspace` / `in_place_workspace` 在 git 仓库里也全是空 branch / base_commit。

---

### Task 1: `WorkerWorkspace` 增加显式 `vcs` 标记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs:22-40`
- Test: `yi-agent-rs/crates/yi-agent-core/src/subagent/worker.rs`（内联）

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceVcs {
    #[default]
    Git,
    None,
}

pub struct WorkerWorkspace {
    pub lease_id: WorkspaceLeaseId,
    pub repository_root: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    pub parent_branch: String,
    pub base_commit: String,
    /// 该位置是否有版本控制保护。`None` = 就地目录，无法提交/比较。
    /// `#[serde(default)]` 让只可能是 git 的旧报文仍解码为 `Git`。
    #[serde(default)]
    pub vcs: WorkspaceVcs,
}
```

- [ ] **Step 1: 写会失败的测试**

```rust
/// 旧帧（没有 `vcs` 字段）必须解码成 `Git`：跨版本重连时不能把 git 位置
/// 误判成"无版本控制"，那会让交付从 git 校验退化成 unverified。
#[test]
fn a_legacy_frame_without_vcs_decodes_as_git() {
    let json = r#"{
        "lease_id": "lease-1",
        "repository_root": "/repo",
        "path": "/repo",
        "branch": "main",
        "parent_branch": "",
        "base_commit": "abc123"
    }"#;
    let workspace: WorkerWorkspace = serde_json::from_str(json).unwrap();
    assert_eq!(workspace.vcs, WorkspaceVcs::Git);
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib a_legacy_frame_without_vcs_decodes_as_git`
Expected: 编译失败（`WorkspaceVcs` 不存在）。

- [ ] **Step 3: 实现**

按上面的 `Interfaces` 加入枚举与字段。`WorkspaceLeaseId` 的 serde 形态以现有定义为准；若它不接受裸字符串，用一条真实序列化（`serde_json::to_string(&WorkerWorkspace { .. })`）再反序列化做往返断言，而不是手写 JSON。

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent`
Expected: PASS

- [ ] **Step 5: 补齐构造点**

Run: `cd yi-agent-rs && cargo check -p yi-agent-core -p yi-agent-subagent -p yi-agent-store 2>&1 | grep -E "^error|missing field" | head -50`

按编译器提示给每个 `WorkerWorkspace { .. }` 补 `vcs`。生产点（数量少，逐个判断）：

- `yi-agent-subagent/src/lib.rs:485`（`register_workdir`）：见 Task 2。
- `yi-agent-subagent/src/lib.rs:546`（`read_only_workspace`）：**继承父位置的 vcs**，不能硬写 `Git`（见 Task 2）。
- `yi-agent-subagent/src/lib.rs:565`（`in_place_workspace`）：`repository_root` 的 git 探测在这里做（见 Task 2）。
- `yi-agent-store/src/runtime.rs:256`（`in_place_workspace_at`，read-only 分支的祖先位置）：**必须探测** `path` 是否在 git 里，不能硬写 `Git`。它是生产函数（`runtime.rs:2083` 调用），非 git 项目里的 read-only 子任务会经过它。
- `yi-agent-store/src/runtime.rs` 的测试构造点（`:272` `recorded_workspace`）与 `yi-agent-subagent/src/lib.rs`、`thread_root.rs` 的测试构造点一律显式写 `vcs: WorkspaceVcs::Git`（测试夹具模拟的是 git 语义）。

`in_place_workspace_at` 的改法：

```rust
fn in_place_workspace_at(
    repository_root: std::path::PathBuf,
    path: std::path::PathBuf,
) -> WorkerWorkspace {
    // read-only 子任务的位置来自祖先的 workdir，那里可能是非 git 项目：
    // 用实际路径判定，而不是沿用服务实例的假设。
    let vcs = if std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&path)
        .output()
        .is_ok_and(|out| out.status.success())
    {
        WorkspaceVcs::Git
    } else {
        WorkspaceVcs::None
    };
    WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root,
        path,
        branch: String::new(),
        parent_branch: String::new(),
        base_commit: String::new(),
        vcs,
    }
}
```

（若 `yi-agent-store` 里已有 git 探测辅助函数，直接复用，别在这里再写一份 `Command`。）

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/subagent/worker.rs
git commit -m "feat(subagent): WorkerWorkspace 显式标注版本控制状态"
```

---

### Task 2: `register_workdir` 与两个就地位置正确设置 vcs

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:456-580`
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（内联，约 `:3824`）

**Interfaces:**
- Consumes: Task 1 的 `WorkspaceVcs`。
- Produces: 非 git 目录 `register_workdir` 返回 `Ok`；`vcs == None`；`repository_root == canonical`。

**注意三个易错点（设计文档没展开，但代码里必须处理）:**

1. `in_place_workspace` 的 `repository_root` 用的是 `DaemonWorkspaceService::new` 时探测的根（`:440-445`）。若那个根不是 git，它会等于传入路径；但若服务在 git 目录上构造、而项目子目录不是 git，该字段会指向错误的根。**必须在 `in_place_workspace` 里对 `self.repository_root` 做一次 git 探测**，探测失败则 `vcs: None` 且 `repository_root = self.repository_root.clone()`。
2. `read_only_workspace` 必须**继承**父位置的 `vcs`，否则 read-only 子 agent 在非 git 项目里会被标成 `Git`，交付/徽章会给出错误信号。
3. `register_workdir` 的"workdir 不存在"仍是硬错误（用户错误），只有"目录在但不在 git 里"降级。

- [ ] **Step 1: 写会失败的测试**

在既有 `observe_workdir_resolves_a_prepared_directory_and_rejects_a_missing_one`（`:3824`）旁边加：

```rust
#[test]
fn a_non_git_directory_is_accepted_as_an_unverified_position() {
    let dir = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(dir.path().to_path_buf());
    let workspace = service.register_workdir(dir.path()).unwrap();
    assert_eq!(workspace.vcs, WorkspaceVcs::None);
    assert_eq!(workspace.repository_root, dir.path().canonicalize().unwrap());
    assert!(workspace.branch.is_empty());
    assert!(workspace.base_commit.is_empty());
}

#[test]
fn a_missing_directory_is_still_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(dir.path().to_path_buf());
    let missing = dir.path().join("nope");
    assert!(service.register_workdir(&missing).is_err());
}

#[test]
fn a_read_only_position_inherits_the_parents_vcs() {
    // 父位置非 git → read-only 子位置也必须是 None，否则徽章会说"受版本控制保护"。
    let dir = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(dir.path().to_path_buf());
    let parent = service.register_workdir(dir.path()).unwrap();
    let task = /* 用既有测试里的 TaskId 构造方式 */;
    let child = service.read_only_workspace(Some(&parent), &task).unwrap();
    assert_eq!(child.vcs, WorkspaceVcs::None);
}
```

（`TempDir` 目录在 macOS 上通常不在 git 仓库内；若 CI 恰好把它放在 git 仓库里，先在该目录下 `std::fs::create_dir` 一个子目录并确认 `git rev-parse` 失败，或用既有测试里"造一个非 git 目录"的辅助函数。**不要**依赖"临时目录一定不是 git"这个隐含前提而不验证。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_non_git_directory_is_accepted`
Expected: FAIL（当前返回 `Err("workdir is not inside a git worktree")`）。

- [ ] **Step 3: 实现**

`register_workdir` 的 git 探测改为分支：

```rust
let toplevel = git_output(&canonical, &["rev-parse", "--show-toplevel"]);
let (repository_root, vcs, branch, base_commit) = match toplevel {
    Some(root) => (
        PathBuf::from(root),
        WorkspaceVcs::Git,
        current_git_branch(&canonical).unwrap_or_default(),
        git_output(&canonical, &["rev-parse", "HEAD"]).unwrap_or_default(),
    ),
    // 不是 git 仓库不是用户错误：就地工作，但没有版本控制保护。
    // `repository_root` 用 canonical 路径，让重连匹配有可比对的值。
    None => (canonical.clone(), WorkspaceVcs::None, String::new(), String::new()),
};
let workspace = WorkerWorkspace {
    lease_id: WorkspaceLeaseId::new(),
    repository_root,
    path: canonical.clone(),
    branch,
    parent_branch: String::new(),
    base_commit,
    vcs,
};
```

把函数文档改成"目录不存在才拒绝；不在 git 里则就地工作"。

`read_only_workspace` 加：

```rust
let vcs = parent.map(|w| w.vcs).unwrap_or(WorkspaceVcs::Git);
```

并在构造里用 `vcs`（`repository_root` 保持 `self.repository_root`）。

`in_place_workspace` 加：

```rust
// 服务实例的根可能是 git，但这个项目目录不一定：以实际路径为准。
let (repository_root, vcs) =
    match git_output(&self.repository_root, &["rev-parse", "--show-toplevel"]) {
        Some(_) => (self.repository_root.clone(), WorkspaceVcs::Git),
        None => (self.repository_root.clone(), WorkspaceVcs::None),
    };
```

（`repository_root` 在两种情况下都用 `self.repository_root`，因为就地位置就在那里；`vcs` 才是区分点。）

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib workspace`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): 非 git workdir 降级为就地位置"
```

---

### Task 3: 交付结构支持 `Unverified`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs:245-300`
- Test: `yi-agent-rs/crates/yi-agent-core/src/subagent/task.rs`（内联）

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    #[default]
    Git,
    /// 未受版本控制保护的就地交付：没有 commit、没有 base ref。
    Unverified,
}

impl DeliveryReport {
    pub fn unverified(
        workspace: WorkspaceLeaseId,
        evidence: impl Into<String>,
    ) -> Self;
    pub fn with_kind(mut self, kind: DeliveryKind) -> Self;
}
```

- [ ] **Step 1: 写会失败的测试**

```rust
#[test]
fn an_unverified_delivery_needs_evidence_but_not_a_commit() {
    let task = /* 既有测试里的 task 构造，workspace 与下面一致 */;
    let report = DeliveryReport::unverified(task.workspace.clone().unwrap(), "scan");
    assert!(report.validate_for(&task).is_ok());
    assert_eq!(report.kind, DeliveryKind::Unverified);
    assert!(report.commit.is_empty());
}

#[test]
fn a_git_delivery_still_requires_a_commit() {
    let task = /* 同上 */;
    let report = DeliveryReport::coding("", "main", task.workspace.clone().unwrap(), "e");
    assert!(report.validate_for(&task).is_err());
}

/// 存量 `delivery_json` 没有 `kind` 字段，必须解码成 Git。
#[test]
fn a_legacy_delivery_without_kind_decodes_as_git() {
    let report: DeliveryReport = serde_json::from_str(
        r#"{"id":"d1","commit":"abc","base_ref":"main","workspace":"lease-1","evidence":"e"}"#,
    )
    .unwrap();
    assert_eq!(report.kind, DeliveryKind::Git);
}
```

（`DeliveryId` / `WorkspaceLeaseId` 的 serde 形态按现有定义；若上面的裸字符串不成立，改用一次真实序列化再反序列化。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib an_unverified_delivery`
Expected: 编译失败（`unverified` 不存在）。

- [ ] **Step 3: 实现**

加枚举与字段（`#[serde(default)] pub kind: DeliveryKind`）。`unverified` 构造：

```rust
pub fn unverified(workspace: WorkspaceLeaseId, evidence: impl Into<String>) -> Self {
    Self {
        id: DeliveryId::new(),
        commit: String::new(),
        base_ref: String::new(),
        workspace,
        evidence: evidence.into(),
        changed_files: Vec::new(),
        known_limitations: Vec::new(),
        kind: DeliveryKind::Unverified,
    }
}
```

`validate_for` 按 kind 分支（`Git` 分支逐字保留现有四条；`Unverified` 只校验 evidence 与 workspace）：

```rust
fn validate_for(&self, task: &AgentTask) -> Result<(), &'static str> {
    match self.kind {
        DeliveryKind::Git => {
            if self.commit.trim().is_empty() {
                return Err("commit is required for coding delivery");
            }
            if self.base_ref.trim().is_empty() {
                return Err("base ref is required for coding delivery");
            }
        }
        DeliveryKind::Unverified => {}
    }
    if self.evidence.trim().is_empty() {
        return Err("delivery evidence is required");
    }
    if task.workspace.as_ref() != Some(&self.workspace) {
        return Err("delivery workspace does not match task workspace");
    }
    Ok(())
}
```

`coding(...)` 保持 `kind: DeliveryKind::Git`。

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib subagent`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/subagent/task.rs
git commit -m "feat(core): 交付报告区分 git 与未受版本控制保护"
```

---

### Task 4: 非 git 下的交付生成与运行循环分支

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:500-530`（`inspect_delivery_report`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1040-1090`（收尾逻辑）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（内联）

**Interfaces:**
- Consumes: Task 1 的 `vcs`、Task 3 的 `DeliveryReport::unverified`。
- Produces: 非 git 的 coding 子任务以 `Completed` + `Unverified` 收尾。

**关键顺序问题:** 现有收尾逻辑先调 `inspect_delivery`，再按 `Err` 分支区分"dirty"与"空交付"。非 git 下 `inspect_workdir` 必然返回 `Err("Git workspace error: ...")`，会先被 `is_dirty_delivery_error` / 其它 `Err` 分支吃掉。因此**分支必须放在调用 `inspect_delivery` 之前**，按 `workspace.vcs` 判断，而不是在 `Err` 里补救。

- [ ] **Step 1: 写会失败的测试**

```rust
#[test]
fn a_non_git_position_produces_an_unverified_delivery() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("out.txt"), "hello").unwrap();
    let service = DaemonWorkspaceService::new(dir.path().to_path_buf());
    let workspace = service.register_workdir(dir.path()).unwrap();

    let report = service.inspect_delivery(&workspace).unwrap();
    assert_eq!(report.kind, DeliveryKind::Unverified);
    assert!(report.commit.is_empty());
    assert!(report.changed_files.iter().any(|f| f.ends_with("out.txt")));
    assert!(
        report
            .known_limitations
            .iter()
            .any(|l| l.contains("不是 Git 仓库")),
        "got {:?}",
        report.known_limitations
    );
}
```

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_non_git_position_produces`
Expected: FAIL（当前返回 `Err("Git workspace error: ...")`）。

- [ ] **Step 3: 实现 `inspect_delivery_report`**

```rust
fn inspect_delivery_report(
    &self,
    workspace: &WorkerWorkspace,
) -> Result<DeliveryReport, WorkerError> {
    if workspace.vcs == WorkspaceVcs::None {
        return Ok(unverified_delivery(workspace));
    }
    // 以下逐字保留现有 git 路径。
    ...
}
```

`unverified_delivery` 的实现（放同文件私有函数）：

```rust
/// 非 git 位置的交付：只有一份可核对的改动清单，没有 commit。
///
/// 扫描是有界的：跳过 `.yi-agent/` 与符号链接，最多 1000 条；超过时只留
/// 计数，避免在一个大目录上把收尾拖成分钟级。**不**计算内容哈希、**不**
/// 区分"本次新增"与"改动前已存在"——没有基线可比，这一点必须写在
/// `known_limitations` 里，否则用户会误以为清单等于本次改动。
fn unverified_delivery(workspace: &WorkerWorkspace) -> DeliveryReport {
    const LIMIT: usize = 1000;
    let mut files = Vec::new();
    let mut truncated = false;
    if let Ok(entries) = walk_unversioned(&workspace.path, LIMIT, &mut files) {
        truncated = entries;
    }
    let evidence = serde_json::json!({
        "kind": "unverified_delivery",
        "reason": "no_git_repository",
        "path": workspace.path.display().to_string(),
        "truncated": truncated,
    })
    .to_string();
    let mut limitations = vec![
        "该目录不是 Git 仓库：改动未经版本控制保护，无法生成 commit，也无法用 diff 审阅；\
         如需回滚请手动备份。"
            .to_string(),
        "文件清单无法区分本次改动与此前已存在的文件。".to_string(),
    ];
    if truncated {
        limitations.push(format!("改动文件过多（超过 {LIMIT} 条），未逐一列举。"));
    }
    DeliveryReport::unverified(workspace.lease_id.clone(), evidence)
        .with_changed_files(if truncated { Vec::new() } else { files })
        .with_known_limitations(limitations)
}
```

`walk_unversioned` 用 `std::fs::read_dir` 手写 BFS（不要引入 `walkdir`）：跳过 `.yi-agent`、`std::fs::symlink_metadata` 判 `is_symlink` 后不跟随、返回 `bool` 表示"是否超限"。

- [ ] **Step 4: 改运行循环**

在 `workspace_mode == ChildWriteMode::Coding` 的两处收尾（`:1026` 的最大轮次分支与 `:1053` 的正常结束分支）**之前**加一个 `vcs` 判断。最省重复的做法是在进入 `'run` 循环前算一次：

```rust
let deliverable_without_git = workspace_for_delivery.vcs == WorkspaceVcs::None;
```

然后在两处 coding 收尾的开头插入：

```rust
if deliverable_without_git {
    let delivery = service
        .inspect_delivery(&workspace_for_delivery)
        .unwrap_or_else(|error| {
            // 扫描失败也要以 completed 收尾：用户明确要求"不影响正常使用"。
            // 失败原因写进 evidence，让父 agent 能解释这份交付为何为空。
            let mut report = DeliveryReport::unverified(
                workspace_for_delivery.lease_id.clone(),
                format!("unverified delivery scan failed: {error}"),
            );
            report.known_limitations.push(
                "无法扫描目录内容；改动清单为空，不能据此判断改了什么。".to_string(),
            );
            report
        });
    end_trace(&mut aggregator, "completed");
    reporter.report_delivery(delivery);
    break 'run;
}
```

（`workspace_service` 为 `None` 时保持现有 `report_completed` 分支不变——那意味着没有注册表，也就没有可标注的位置。）

`is_dirty_delivery_error` / `is_empty_delivery_error` 不改：它们只在 git 分支被引用。

- [ ] **Step 5: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib`
Expected: PASS（含既有 git 交付测试，语义不变）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): 非 git 位置以 unverified 交付收尾"
```

---

### Task 5: 重连匹配按 vcs 分支

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:693-701`
- Test: 同文件内联

- [ ] **Step 1: 写会失败的测试**

```rust
#[test]
fn a_non_git_position_matches_by_canonical_path() {
    let dir = tempfile::TempDir::new().unwrap();
    let service = DaemonWorkspaceService::new(dir.path().to_path_buf());
    let recorded = service.register_workdir(dir.path()).unwrap();
    assert_eq!(recorded.vcs, WorkspaceVcs::None);
    assert!(service.project_workspace_matches(dir.path(), &recorded));
    assert!(!service.project_workspace_matches(&dir.path().join("other"), &recorded));
}
```

- [ ] **Step 2: 运行测试，确认失败**

Expected: FAIL（非 git 下 `git rev-parse` 恒失败 → 恒 `false`）。

- [ ] **Step 3: 实现**

```rust
fn project_workspace_matches(
    &self,
    workspace: &std::path::Path,
    recorded: &WorkerWorkspace,
) -> bool {
    match recorded.vcs {
        WorkspaceVcs::Git => git_output(workspace, &["rev-parse", "--show-toplevel"])
            .is_some_and(|repository_root| {
                recorded.repository_root == std::path::Path::new(&repository_root)
            }),
        WorkspaceVcs::None => {
            let canonical = workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_path_buf());
            recorded.repository_root == canonical
        }
    }
}
```

- [ ] **Step 4: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib project_workspace_matches`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs
git commit -m "fix(subagent): 非 git 位置按 canonical 路径重连匹配"
```

---

### Task 6: spawn 回执带 `unverified` 标记

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:487-490`（`IpcResponse::TaskSpawned`）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs:3489-3530`（两处构造）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs:2138-2152`（只读访问器）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1856-1881`（`spawn_outcome`）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:4644,4698`（既有模式需改 `..`）

**Interfaces:**
- Produces:

```rust
TaskSpawned {
    task_id: String,
    /// 该位置不在 git 仓库内：改动没有版本控制保护。
    /// `#[serde(default)]` 保证旧帧解码为 `false`。
    #[serde(default)]
    unverified: bool,
}
```

**为什么不用改 spawn 函数签名:** 设计文档建议把 `spawn_child_with_objective` 等的返回值扩成 `SpawnedChild`。那会波及 25 处调用（其中 16 处是测试）。更小的等价做法是：coordinator 在 spawn 成功时已经通过 `remember_task_position` 记下了 workspace（`runtime.rs:2117,2133`），IPC 层只需在构造响应前查一次。

- [ ] **Step 1: 写会失败的测试**

在 `yi-agent-store` 的测试里加一条序列化契约测试：

```rust
#[test]
fn a_spawn_response_without_unverified_decodes_as_false() {
    let response: IpcResponse = serde_json::from_str(
        r#"{"type":"TaskSpawned","task_id":"t1"}"#,
    )
    .unwrap();
    match response {
        IpcResponse::TaskSpawned { unverified, .. } => assert!(!unverified),
        other => panic!("got {other:?}"),
    }
}
```

（`IpcResponse` 的 serde 表示以现有定义为准备；若它用 `#[serde(tag = "type")]` 的变体外，按实际形态调整 JSON。）

- [ ] **Step 2: 运行测试，确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib a_spawn_response_without_unverified`
Expected: 编译/断言失败。

- [ ] **Step 3: 实现**

`ipc.rs` 响应变体加字段；两处构造改为：

```rust
let unverified = coordinator
    .task_runs_without_vcs(&task_id)
    .unwrap_or(false);
Ok(IpcResponse::TaskSpawned {
    task_id: task_id.to_string(),
    unverified,
})
```

在 `runtime.rs` 加一个只读访问器（`remembered_task_position` 当前是私有；新增 pub(crate) 包装，或在同模块加）：

```rust
/// spawn 回执要带"是否无版本控制保护"：位置已在 spawn 时记录，这里只读，
/// 不重新探测（重新探测会在目录被改动后给出与本次 spawn 不一致的结论）。
pub(crate) fn task_runs_without_vcs(&self, task: &TaskId) -> Option<bool> {
    self.remembered_task_position(task)
        .map(|workspace| workspace.vcs == WorkspaceVcs::None)
}
```

`spawn_outcome` 改为：

```rust
Ok(yi_agent_store::ipc::IpcResponse::TaskSpawned {
    task_id,
    unverified,
}) => {
    let mut value = json!({
        "task_id": task_id,
        "objective": objective,
        "status": "queued",
        "forked": fork_token.is_some(),
        "unverified": unverified,
    });
    if unverified {
        value["note"] = json!(
            "该目录不是 Git 仓库，子任务的改动没有版本控制保护：无法生成 commit，也无法用 diff 审阅。"
        );
    }
    ToolResult::text(value.to_string())
}
```

- [ ] **Step 4: 修正既有模式匹配**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib 2>&1 | grep -E "TaskSpawned" | head`

把 `TaskSpawned { task_id }` 的匹配改成 `TaskSpawned { task_id, .. }`（测试里）或解构出 `unverified`（需要断言的）。`yi-agent-store` 的测试同样处理。

- [ ] **Step 5: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --lib && cargo test -p yi-agent-subagent --lib`
Expected: PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/ipc.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): spawn 回执标注无版本控制保护"
```

---

### Task 7: 端到端测试：非 git workdir 的 coding 子任务

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/runtime_coordinator.rs:3495-3560`

- [ ] **Step 1: 改写既有测试**

把 `an_autonomous_session_refuses_a_workdir_that_is_not_a_git_worktree` 改名为 `an_autonomous_session_downgrades_a_non_git_workdir`，断言：

```rust
// spawn 成功，任务最终 Completed，交付是 Unverified 且带风险说明。
assert_eq!(task.state, TaskState::Completed);
let delivery = task.delivery.expect("a completed coding task must deliver");
assert_eq!(delivery.kind, DeliveryKind::Unverified);
assert!(!delivery.known_limitations.is_empty());
assert!(
    delivery
        .known_limitations
        .iter()
        .any(|l| l.contains("不是 Git 仓库"))
);
```

- [ ] **Step 2: 运行测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator an_autonomous_session_downgrades_a_non_git_workdir`
Expected: PASS

- [ ] **Step 3: 跑该测试文件全量**

Run: `cd yi-agent-rs && cargo test -p yi-agent-store --test runtime_coordinator`
Expected: PASS（git 路径的既有用例全部不变）

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/tests/runtime_coordinator.rs
git commit -m "test(subagent): 非 git workdir 端到端降级"
```

---

### Task 8: 任务列表与子 agent 卡片的 `workspace_vcs`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/repository.rs`（`tasks` 表加列）
- Modify: `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`（任务摘要带 vcs）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:571-583`（`AgentChild`）
- Modify: `desktop/src/lib/generated/AgentChild.ts`（ts-rs 重新生成）
- Modify: `desktop/src/lib/subagents.ts`（`SubagentRow`）
- Modify: `desktop/src/components/SubagentRail.tsx`
- Test: `desktop/src/components/SubagentRail.test.tsx`

**做法:** 复用既有 `workspace_mode` 的迁移风格（`repository.rs:4052-4065`）：

```sql
ALTER TABLE tasks ADD COLUMN workspace_vcs TEXT NOT NULL DEFAULT 'git';
```

写入点与 `workspace_mode` 相同（`repository.rs:954-983` 与 `1052-1070` 两处 insert）。读取用 `COALESCE(workspace_vcs, 'git')`，保证旧库在迁移前也不会崩。

- [ ] **Step 1: 写会失败的 Rust 测试**

```rust
#[test]
fn a_task_row_records_the_workspace_vcs() {
    // 造一条 vcs=None 的 coding 任务，读回 summary 必须是 "none"。
}
```

（按 `repository.rs` 既有测试的建库/建任务辅助函数写，避免手写 SQL。）

- [ ] **Step 2: 实现服务端**

迁移 + 两处 insert + summary 字段。`AgentChild` 加：

```rust
/// `"git"` / `"none"`。缺省按 `"git"`：旧侧车不会发这个字段。
#[serde(default = "default_workspace_vcs")]
pub workspace_vcs: String,
```

- [ ] **Step 3: 写会失败的前端测试**

```tsx
it("marks a child that runs outside version control", async () => {
  renderRail({ rows: [row({ workspace_vcs: "none" })] });
  expect(await screen.findByText("未受版本控制保护")).toBeTruthy();
});

it("does not mark a git child", async () => {
  renderRail({ rows: [row({ workspace_vcs: "git" })] });
  expect(screen.queryByText("未受版本控制保护")).toBeNull();
});
```

- [ ] **Step 4: 实现前端**

`SubagentRow` 加 `workspace_vcs?: "git" | "none"`；`SubagentRail` 在 `=== "none"` 时渲染一个小徽章。生成器（ts-rs）重新生成 `AgentChild.ts` 后，确认 `npx tsc --noEmit` 无错。

- [ ] **Step 5: 运行测试**

```bash
cd yi-agent-rs && cargo test -p yi-agent-store --lib && cargo test -p yi-agent-app-server --lib
cd desktop && npx vitest run src/components/SubagentRail.test.tsx src/lib/subagents.test.ts && npx tsc --noEmit
```

Expected: PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-store/src/repository.rs crates/yi-agent-store/src/runtime.rs crates/yi-agent-app-server/src/protocol.rs crates/yi-agent-store/tests/
git add ../desktop/src/lib/generated/AgentChild.ts ../desktop/src/lib/subagents.ts ../desktop/src/components/SubagentRail.tsx ../desktop/src/components/SubagentRail.test.tsx
git commit -m "feat(subagent): 任务列表标注无版本控制保护"
```

---

### Task 9: 文案与工具描述对齐

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs:1957-1975`（`spawn_agent` 的 `workdir` 描述）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:219-228`（系统提示）

- [ ] **Step 1: 改描述**

`workdir` 描述改为：

```
The directory the child works in. For a git repository, create a worktree
yourself (`git worktree add`) so the change is isolated. A directory outside
git is allowed, but the child's change is unversioned and cannot be reviewed
as a diff.
```

系统提示同步改为："在 git 仓库内优先使用 worktree；非 git 目录允许就地工作，但要在最终报告里说明改动未经版本控制保护。"

- [ ] **Step 2: 跑受影响的测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib && cargo test -p yi-agent-core --lib`
Expected: PASS（若有断言旧文案的测试，按新语义更新——这是刻意的行为变更）

- [ ] **Step 3: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-subagent/src/lib.rs crates/yi-agent-core/src/agent.rs
git commit -m "docs(subagent): 工具描述说明非 git 位置的风险"
```

---

### Task 10: diff 面板对非 git 位置的说明

**Files:**
- Modify: `desktop/src/`（diff 面板空态文案）
- Test: 对应组件测试

- [ ] **Step 1: 定位并改文案**

`git_diff.rs` 在非 git 下产出空 diff（既有行为，不新增服务端降级代码）。前端在 `workspace_vcs === "none"` 且 diff 为空时显示：

```
该目录不是 Git 仓库，无法生成 diff。改动请查看交付报告里的文件清单。
```

- [ ] **Step 2: 写测试并运行**

```bash
cd desktop && npx vitest run src/components/  # 定位到实际承载 diff 空态的组件
cd desktop && npx tsc --noEmit
```

Expected: PASS

- [ ] **Step 3: 提交**

```bash
git add desktop/src
git commit -m "feat(desktop): 非 git 位置说明无法生成 diff"
```

---

### Task 11: 全量验证与文档登记

- [ ] **Step 1: 分 crate 跑测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib
cargo test -p yi-agent-subagent --lib
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-store --test runtime_coordinator
cargo test -p yi-agent-app-server --lib
```

Expected: 全绿。**不要** `cargo test --workspace`（见 AGENTS.md）。

- [ ] **Step 2: 前端验证**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```

- [ ] **Step 3: 登记进度**

在 `docs/project-management/` 对应模块文件里追加 `[x]` 条目（按既有格式），判据写成可执行命令，例如：

```markdown
- [x] 子 agent 非 Git 降级（静默 + 三处风险标注）— `WorkerWorkspace.vcs`、`DeliveryKind::Unverified`、spawn 回执 `unverified`、任务列表 `workspace_vcs` 徽章。判据：`cargo test -p yi-agent-subagent --lib non_git && cargo test -p yi-agent-store --test runtime_coordinator an_autonomous_session_downgrades_a_non_git_workdir` — [设计](../superpowers/specs/2026-10-19-nongit-subagent-workspace-design.md)
```

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/
git commit -m "docs(project-management): 登记子 agent 非 git 降级完成"
```

---

## 自检记录

**Spec 覆盖检查：**
- 设计 §1（`WorkspaceVcs`）→ Task 1、2。
- 设计 §2（`DeliveryKind` + `validate_for`）→ Task 3。
- 设计 §3（运行循环分支 + 有界扫描）→ Task 4。
- 设计 §4（重连匹配）→ Task 5。
- 设计 §5.1（spawn 回执）→ Task 6；§5.2（列表徽章）→ Task 8；§5.3（交付报告/`delivery_diff`）→ Task 3、4、10。
- 设计 §6（文案）→ Task 9。
- 设计「错误处理与边界」8 行 → Task 2（不存在仍拒绝）、Task 4（扫描失败仍 completed）、Task 6（回执）、Task 5（重连）。
- 设计「测试与验收」9 条 → Task 1–11。

**对设计的两处必要修正（实施时按本计划，不按 spec 原文）：**
1. spec §5.1 建议把 `spawn_child_with_objective` 等返回值扩成 `SpawnedChild`。那会改动 25 处调用（16 处测试）。本计划改为：coordinator 用 spawn 时已记录的 `task_positions` 只读查询，只给 `IpcResponse::TaskSpawned` 加一个 `#[serde(default)] unverified` 字段。语义等价，改动面小一个数量级。
2. spec §1 说约 53 处构造点要补字段。实际编译错误以 `cargo check` 为准：生产点为 `lib.rs:485/546/565`、`runtime.rs:260`（`in_place_workspace_at`）、`runtime.rs:273`（测试夹具）、`thread_root.rs:283`（测试夹具），其余是测试，统一写 `vcs: WorkspaceVcs::Git`。

**spec 未覆盖、但代码里必须处理的三点（已写进 Task 2 与 Task 4）：**
- `read_only_workspace` 必须继承父位置 vcs，否则 read-only 子 agent 会被误标为受版本控制保护。
- `in_place_workspace` 的 `repository_root` 来自服务实例构造时的根，必须对实际路径重新判定 vcs。
- 非 git 分支必须放在 `inspect_delivery` 调用**之前**：现有逻辑会先匹配 `Err` 的 dirty/empty 分支，git 探测失败会先被吃掉。

**占位符扫描：** 无 TBD/TODO。Task 8 与 Task 10 需要按实际组件名定位（计划里给了检索方式），因为这两个前端文件的确切承载点需要在实施时用 `grep` 确认；它们都不影响核心降级逻辑。

**已知风险：**
- 非 git 下多个 coding 子 agent 共享同一目录会互相覆盖（spec 明确接受，不引入锁）。
- `cargo check` 会在 Task 1 Step 5 给出大量 `missing field vcs` 错误；这是机械修复，按提示逐个补。
- `Repository` 迁移需要兼容既有库；`NOT NULL DEFAULT 'git'` 让旧行自动成为 git，符合"旧数据只可能来自 git"的事实。