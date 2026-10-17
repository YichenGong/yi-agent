# 关联工作区根（工作流 1：索引 + 协议 + 多根工具层 + UI）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端可以把目录 B 关联到主根 A，并使以 A 为 `cwd` 的会话对 A 与 B 都能 read / write / edit / glob / grep / bash / process_start。

**Architecture:** 关联关系存在本机索引 `~/.yi-agent/workspaces.json` 的新 `links` 字段；`thread/start` 把根集快照进 `ThreadMeta.extra_roots`，`thread/resume` 从 meta 重建。根集经 thread-local 槽位传入 agent 工厂，同时落到沙箱层（`SandboxPolicy` 已有 `writable_roots: Vec<PathBuf>`）与 FS 工具层（`ToolsContext.root` 扩为 `roots`），权限检查器的前缀解析基点从"唯一 workdir"改为"命中的那个根"。沙箱层与 FS 工具层必须一起改：只改前者会让 `bash` 能写副目录而 `write`/`edit` 不能。

**Tech Stack:** Rust（app-server / tools / core crates）、React 19 + TypeScript + Vitest（desktop）。

**上游 spec:** `docs/superpowers/specs/2026-10-17-linked-workspace-roots-design.md`

## Global Constraints

- 关联只允许一层：主根不能同时是别人的附属根；无环；一个附属根全局只属于一个主根；主根 ≠ 附属根。
- `dirs` 必须继续包含附属根（`thread/listAll` 靠 `workspaces.list()` 发现冷会话）。"附属根不作为顶层组"是**显示层**行为。
- `ThreadMeta.extra_roots` 与索引 `links` 都必须 `#[serde(default)]`，旧数据零迁移。
- 空根集 = 单根会话，行为与改动前逐字节一致（这是回归的基线）。
- 相对路径永远只拼主根（`roots[0]`），不做多根猜测。
- 四条逃逸检查逐条保留：终 component canonicalize、父目录 canonicalize、词法归一、绝对路径原样。
- 根集**不得**塞进共享 `cfg` 后跨会话复用（daemon 的 worker factory 按项目目录建、被同目录多会话共享）。
- FS 层不复用 worker 的 `.git` 可写根。
- 每个任务结束时 `cargo fmt --all` 必须通过；提交信息用 conventional commits，不写 `Co-Authored-By`。
- 不要同时跑多个 `cargo test`（见 `CLAUDE.md`：cargo 元数据锁竞争会导致 exit 137）。

---

## File Structure

| 文件 | 职责 | 动作 |
| --- | --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs` | 关联关系的存取与不变量 | Modify |
| `yi-agent-rs/crates/yi-agent-tools/src/fs/path_util.rs` | 多根路径解析与包含检查 | Modify |
| `yi-agent-rs/crates/yi-agent-tools/src/context.rs` | `ToolsContext` 持有根集 | Modify |
| `yi-agent-rs/crates/yi-agent-tools/src/fs/{read,write,edit,glob,grep,view_image,read_document}.rs` | 用根集替代单根 | Modify |
| `yi-agent-rs/crates/yi-agent-tools/src/lib.rs` | 注册函数接收 FS 根集 | Modify |
| `yi-agent-rs/crates/yi-agent-core/src/permission.rs` | 前缀白名单按命中根取基点 | Modify |
| `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs` | 把 FS 根集传给工具注册 | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` | `ThreadMeta.extra_roots` | Modify |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 槽位、`thread/start`/`resume`、`workspace/link`/`unlink`/`list` | Modify |
| `desktop/src/lib/protocol.ts` | `Workspace.linked_to` | Modify |
| `desktop/src/lib/workspaceGroups.ts` | 附属根归拢纯函数 | Modify |
| `desktop/src/components/ThreadSidebar.tsx` | 菜单项与嵌套小节 | Modify |
| `desktop/src/App.tsx` | `onLinkFolder` / `onUnlinkFolder` 接线 | Modify |
| `docs/project-management/{desktop,yi-agent-app-server,yi-agent-tools}.md`、`README.md` | 进度登记 | Modify |

---

## Task 1: `WorkspaceIndex` 支持关联

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs`

**Interfaces:**
- Consumes: 无（首个任务）。
- Produces:
  - `pub struct LinkError(pub String)`（`Display`）
  - `WorkspaceIndex::link(&self, primary: &Path, child: &Path) -> Result<(), LinkError>`
  - `WorkspaceIndex::unlink(&self, child: &Path) -> std::io::Result<()>`
  - `WorkspaceIndex::linked_to(&self, path: &Path) -> Option<String>`
  - `WorkspaceIndex::children(&self, primary: &Path) -> Vec<String>`
  - 文件结构 `{"dirs": [...], "links": {"<primary>": ["<child>", ...]}}`

- [ ] **Step 1: 写失败测试**

在 `workspace_index.rs` 的 `mod tests` 内追加（沿用该文件现有的 `index()` helper）：

```rust
    #[test]
    fn link_records_child_under_primary() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-a").unwrap();
        std::fs::create_dir_all("/tmp/wsidx-b").unwrap();
        idx.link(Path::new("/tmp/wsidx-a"), Path::new("/tmp/wsidx-b"))
            .unwrap();
        assert_eq!(idx.children(Path::new("/tmp/wsidx-a")), vec!["/tmp/wsidx-b".to_string()]);
        assert_eq!(
            idx.linked_to(Path::new("/tmp/wsidx-b")),
            Some("/tmp/wsidx-a".to_string())
        );
        assert_eq!(idx.linked_to(Path::new("/tmp/wsidx-a")), None);
    }

    #[test]
    fn link_rejects_self() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-self").unwrap();
        let err = idx
            .link(Path::new("/tmp/wsidx-self"), Path::new("/tmp/wsidx-self"))
            .unwrap_err();
        assert!(err.0.contains("itself"), "unexpected: {}", err.0);
    }

    #[test]
    fn link_rejects_chain() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-p", "/tmp/wsidx-c", "/tmp/wsidx-g"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-p"), Path::new("/tmp/wsidx-c")).unwrap();
        // c 已是附属根，不能再当 g 的主根（禁链式）。
        let err = idx
            .link(Path::new("/tmp/wsidx-c"), Path::new("/tmp/wsidx-g"))
            .unwrap_err();
        assert!(err.0.contains("linked"), "unexpected: {}", err.0);
    }

    #[test]
    fn link_rejects_cycle() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-x", "/tmp/wsidx-y"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-x"), Path::new("/tmp/wsidx-y")).unwrap();
        // y 是附属根，当主根即链式；反过来 x 当 y 的附属根即环。
        let err = idx
            .link(Path::new("/tmp/wsidx-y"), Path::new("/tmp/wsidx-x"))
            .unwrap_err();
        assert!(err.0.contains("linked") || err.0.contains("already"), "unexpected: {}", err.0);
    }

    #[test]
    fn link_reparents_child_to_new_primary() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-p1", "/tmp/wsidx-p2", "/tmp/wsidx-kid"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-p1"), Path::new("/tmp/wsidx-kid")).unwrap();
        idx.link(Path::new("/tmp/wsidx-p2"), Path::new("/tmp/wsidx-kid")).unwrap();
        assert!(idx.children(Path::new("/tmp/wsidx-p1")).is_empty());
        assert_eq!(idx.children(Path::new("/tmp/wsidx-p2")), vec!["/tmp/wsidx-kid".to_string()]);
    }

    #[test]
    fn unlink_is_idempotent() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-u").unwrap();
        idx.unlink(Path::new("/tmp/wsidx-u")).unwrap();
        idx.link(Path::new("/tmp/wsidx-u"), Path::new("/tmp/wsidx-u2")).unwrap_err(); // u2 不存在
        idx.unlink(Path::new("/tmp/wsidx-u2")).unwrap();
    }

    #[test]
    fn legacy_file_without_links_still_reads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, br#"{"dirs":["/tmp/legacy"]}"#).unwrap();
        let idx = WorkspaceIndex::new(path);
        assert_eq!(idx.list(), vec!["/tmp/legacy".to_string()]);
        assert_eq!(idx.linked_to(Path::new("/tmp/legacy")), None);
    }

    #[test]
    fn links_persist_json_shape() {
        let (d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-ja").unwrap();
        std::fs::create_dir_all("/tmp/wsidx-jb").unwrap();
        idx.add(Path::new("/tmp/wsidx-ja")).unwrap();
        idx.link(Path::new("/tmp/wsidx-ja"), Path::new("/tmp/wsidx-jb")).unwrap();
        let raw = std::fs::read_to_string(d.path().join("workspaces.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(v["links"]["/tmp/wsidx-ja"][0], "/tmp/wsidx-jb");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib workspace_index`
Expected: 编译失败（`link` / `unlink` / `linked_to` / `children` 不存在）。

- [ ] **Step 3: 实现**

把 `WorkspacesFile` 与 `WorkspaceIndex` 改为：

```rust
#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    dirs: Vec<String>,
    /// 主根 → 附属根（均为 canonical 绝对路径）。旧文件无此字段，默认空。
    #[serde(default)]
    links: std::collections::BTreeMap<String, Vec<String>>,
}

/// 关联被拒绝的原因。文案直接面向用户（UI 原样显示），故写清"为什么"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(pub String);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
```

并给 `WorkspaceIndex` 追加四个方法：

```rust
    /// 把 `child` 关联到 `primary` 之下。两边都必须是已存在的目录。
    ///
    /// 不变量：禁链式（主根不能是别人的附属根）、无环、一个附属根只属于一个主根、
    /// 主根 ≠ 附属根。`child` 若已有主根则改挂（重新父级），不报错。
    pub fn link(&self, primary: &Path, child: &Path) -> Result<(), LinkError> {
        let primary = canonical_dir(primary)?;
        let child = canonical_dir(child)?;
        if primary == child {
            return Err(LinkError("a folder cannot be linked to itself".into()));
        }
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        // 禁链式：primary 自己不能是别人的附属根。
        if file.links.values().any(|kids| kids.iter().any(|k| k == &primary)) {
            return Err(LinkError(format!(
                "{primary} is already linked into another project"
            )));
        }
        // 环：child 已有附属根时，那个附属根不能是 primary 的后代。
        if let Some(existing) = file.links.get(&primary) {
            if existing.iter().any(|k| k == &child) {
                return Ok(()); // 幂等
            }
        }
        if child_is_primary_of(&file, &child, &primary) {
            return Err(LinkError(format!(
                "linking {child} under {primary} would create a cycle"
            )));
        }
        // 一个附属根只属于一个主根：先摘掉旧父级。
        for kids in file.links.values_mut() {
            kids.retain(|k| k != &child);
        }
        file.links.retain(|_, kids| !kids.is_empty());
        file.links.entry(primary).or_default().push(child);
        self.write(&file).map_err(|e| LinkError(e.to_string()))
    }

    /// 取消 `child` 的关联；不存在则幂等成功。
    pub fn unlink(&self, child: &Path) -> std::io::Result<()> {
        let entry = path_string(child);
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        let mut changed = false;
        for kids in file.links.values_mut() {
            let before = kids.len();
            kids.retain(|k| k != &entry);
            changed |= kids.len() != before;
        }
        if !changed {
            return Ok(());
        }
        file.links.retain(|_, kids| !kids.is_empty());
        self.write(&file)
    }

    /// `path` 的主根（若它是附属根）。
    pub fn linked_to(&self, path: &Path) -> Option<String> {
        let entry = path_string(path);
        self.read()
            .links
            .iter()
            .find(|(_, kids)| kids.iter().any(|k| k == &entry))
            .map(|(primary, _)| primary.clone())
    }

    /// `primary` 的附属根列表。
    pub fn children(&self, primary: &Path) -> Vec<String> {
        let entry = path_string(primary);
        self.read().links.get(&entry).cloned().unwrap_or_default()
    }
```

文件级 helper（放在 `impl WorkspaceIndex` 之外）：

```rust
/// canonicalize 并确认是目录。失败时给出面向用户的文案。
fn canonical_dir(path: &Path) -> Result<String, LinkError> {
    match std::fs::canonicalize(path) {
        Ok(p) if p.is_dir() => Ok(path_string(&p)),
        _ => Err(LinkError(format!("not a folder: {}", path.display()))),
    }
}

/// 与 `add` / `remove` 同一口径：canonicalize 不成功时回退原始串。
fn path_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// `child` 是否是 `primary` 某个祖先的主根位置（即把 child 挂到 primary 下会成环）。
fn child_is_primary_of(file: &WorkspacesFile, child: &str, primary: &str) -> bool {
    let mut cursor = primary.to_string();
    // 只允许一层，但用循环防御畸形文件里的深链。
    let mut guard = 0;
    while guard < 64 {
        guard += 1;
        let Some(kids) = file.links.get(&cursor) else {
            return false;
        };
        if kids.iter().any(|k| k == child) {
            return true;
        }
        match kids.first() {
            Some(next) => cursor = next.clone(),
            None => return false,
        }
    }
    false
}
```

注意：现有 `read` / `write` 方法只处理 `dirs`，`links` 字段随结构体一起读写即可；`add` / `remove` 不动 `links`（"移除只是视图操作、不删数据"口径）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib workspace_index`
Expected: PASS（含 9 个既有测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs
git commit -m "feat(app-server): link workspaces into one project root set"
```

---

## Task 2: `path_util` 多根解析

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/path_util.rs`

**Interfaces:**
- Consumes: 无。
- Produces:
  - `pub fn canonicalize_roots(roots: &[PathBuf]) -> Vec<PathBuf>`
  - `pub fn resolve_and_check_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError>`
  - `pub fn resolve_for_write_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError>`
  - 既有 `resolve_and_check(root, path)` / `resolve_for_write(root, path)` 保留为单根包装（现有测试与调用点不破）。

- [ ] **Step 1: 写失败测试**

追加到 `path_util.rs` 的 `mod tests`：

```rust
    #[test]
    fn roots_resolve_relative_against_primary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        fs::write(a.path().join("file.txt"), "hi").unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let resolved = resolve_and_check_roots(&roots, "file.txt").unwrap();
        assert_eq!(resolved, a.path().join("file.txt").canonicalize().unwrap());
    }

    #[test]
    fn roots_accept_absolute_path_inside_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        fs::write(b.path().join("sib.txt"), "hi").unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = b.path().join("sib.txt").to_string_lossy().to_string();
        let resolved = resolve_and_check_roots(&roots, &target).unwrap();
        assert_eq!(resolved, b.path().join("sib.txt").canonicalize().unwrap());
    }

    #[test]
    fn roots_reject_dotdot_escape_out_of_all_roots() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result = resolve_and_check_roots(&roots, "../escape.txt");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_reject_absolute_outside_all_roots() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result = resolve_and_check_roots(&roots, "/etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_reject_symlink_escape_from_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let secret = TempDir::new().unwrap();
        fs::write(secret.path().join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(secret.path(), b.path().join("evil")).unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result = resolve_and_check_roots(&roots, &format!("{}/evil/secret.txt", b.path().display()));
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_write_allows_nonexistent_parent_in_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = b.path().join("new/dir/file.txt").to_string_lossy().to_string();
        let resolved = resolve_for_write_roots(&roots, &target).unwrap();
        assert_eq!(resolved, b.path().join("new/dir/file.txt"));
    }

    #[test]
    fn roots_reject_symlinked_parent_escape_on_write() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), b.path().join("link")).unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = format!("{}/link/passwd", b.path().display());
        let result = resolve_for_write_roots(&roots, &target);
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn empty_roots_is_an_error() {
        let result = resolve_and_check_roots(&[], "file.txt");
        assert!(result.is_err());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib path_util`
Expected: 编译失败（`canonicalize_roots` / `resolve_and_check_roots` / `resolve_for_write_roots` 不存在）。

- [ ] **Step 3: 实现**

在 `path_util.rs` 顶部加入：

```rust
/// canonicalize 根集：能 canonicalize 的用规范路径，顺序保持（`[0]` 仍是主根），
/// 相同项去重。根不存在时回退原始路径——与 `SandboxPolicy::canonicalize_root` 同口径，
/// 保证"删除中的根"不会让所有路径解析直接报 IO 错。
pub fn canonicalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

/// 路径是否落在根集中任一根之内。
fn contained_in_any(candidate: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| candidate.starts_with(root))
}
```

然后新增多根版本（把现有单根函数体里所有 `canonical_root` 用法替换掉）：

```rust
/// 多根版 [`resolve_and_check`]：相对路径拼**主根**（`roots[0]`），
/// 包含检查要求落在**任一根**之内。
pub fn resolve_and_check_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError> {
    let roots = canonicalize_roots(roots);
    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| ToolsError::PathEscapesRoot(PathBuf::from(path)))?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        primary.join(path)
    };

    let (parent, file_name) = match candidate.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => (parent, candidate.file_name()),
        _ => {
            if let Ok(full_canonical) = candidate.canonicalize() {
                if !contained_in_any(&full_canonical, &roots) {
                    return Err(ToolsError::PathEscapesRoot(candidate));
                }
                return Ok(full_canonical);
            }
            return Ok(candidate);
        }
    };

    let canonical_parent = match parent.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let normalized = lexical_normalize(&candidate);
            if !contained_in_any(&normalized, &roots) {
                return Err(ToolsError::PathEscapesRoot(candidate));
            }
            return Err(ToolsError::NotFound(candidate));
        }
        Err(e) => return Err(ToolsError::Io(e)),
    };

    if !contained_in_any(&canonical_parent, &roots) {
        return Err(ToolsError::PathEscapesRoot(candidate));
    }

    let resolved = match file_name {
        Some(name) => canonical_parent.join(name),
        None => canonical_parent,
    };

    if let Ok(full_canonical) = resolved.canonicalize() {
        if !contained_in_any(&full_canonical, &roots) {
            return Err(ToolsError::PathEscapesRoot(candidate));
        }
        return Ok(full_canonical);
    }

    Ok(resolved)
}

/// 多根版 [`resolve_for_write`]。
pub fn resolve_for_write_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError> {
    let roots = canonicalize_roots(roots);
    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| ToolsError::PathEscapesRoot(PathBuf::from(path)))?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        primary.join(path)
    };

    if let Ok(full_canonical) = candidate.canonicalize() {
        if !contained_in_any(&full_canonical, &roots) {
            return Err(ToolsError::PathEscapesRoot(candidate));
        }
        return Ok(full_canonical);
    }

    if let Some(parent) = candidate.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if !contained_in_any(&canonical_parent, &roots) {
                    return Err(ToolsError::PathEscapesRoot(candidate));
                }
                let file_name = candidate.file_name().unwrap_or_default();
                return Ok(canonical_parent.join(file_name));
            }
        }
    }

    let normalized = lexical_normalize(&candidate);
    if !contained_in_any(&normalized, &roots) {
        return Err(ToolsError::PathEscapesRoot(candidate));
    }
    Ok(normalized)
}
```

把既有两个单根函数改为包装（保持现有测试与调用点可用）：

```rust
pub fn resolve_and_check(root: &Path, path: &str) -> Result<PathBuf, ToolsError> {
    resolve_and_check_roots(&[root.to_path_buf()], path)
}

pub fn resolve_for_write(root: &Path, path: &str) -> Result<PathBuf, ToolsError> {
    resolve_for_write_roots(&[root.to_path_buf()], path)
}
```

**注意**：原单根 `resolve_for_write` 的最后一步是 `Ok(candidate)`（未归一化），上面多根版返回 `normalized`。两者对"父目录不存在"的返回路径等价（`candidate` 无 `..` 时 `lexical_normalize` 即其本身），且多根版更严格（归一化后仍须在根内）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib path_util`
Expected: PASS（含 8 个既有测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/fs/path_util.rs
git commit -m "feat(tools): resolve fs paths against a root set"
```

---

## Task 3: `ToolsContext` 持有根集，FS 工具改用根集

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/context.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/{read,write,edit,glob,grep,view_image,read_document}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:77-105`

**Interfaces:**
- Consumes: `path_util::{resolve_and_check_roots, resolve_for_write_roots, canonicalize_roots}`（Task 2）。
- Produces:
  - `ToolsContext::new(root: PathBuf)`（保留；等价于 `with_roots(vec![root])`）
  - `ToolsContext::with_roots(roots: Vec<PathBuf>) -> Self`
  - `ToolsContext::root(&self) -> &Path`（= `roots[0]`）
  - `ToolsContext::roots(&self) -> &[PathBuf]`
  - `ToolsContext::relative_to_roots(&self, path: &Path) -> Option<String>`

- [ ] **Step 1: 写失败测试**

追加到 `context.rs` 的 `mod tests`：

```rust
    #[test]
    fn with_roots_keeps_primary_first_and_shares_paths() {
        let ctx = ToolsContext::with_roots(vec![
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/b"),
        ]);
        assert_eq!(ctx.root(), Path::new("/tmp/a"));
        assert_eq!(ctx.roots(), &[PathBuf::from("/tmp/a"), PathBuf::from("/tmp/b")]);
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/a"));
    }

    #[test]
    fn relative_to_roots_reports_the_matching_root() {
        let ctx = ToolsContext::with_roots(vec![
            PathBuf::from("/tmp/a"),
            PathBuf::from("/tmp/b"),
        ]);
        assert_eq!(
            ctx.relative_to_roots(Path::new("/tmp/b/src/main.rs")),
            Some("src/main.rs".to_string())
        );
        assert_eq!(ctx.relative_to_roots(Path::new("/tmp/elsewhere/x")), None);
    }
```

追加到 `fs/glob.rs` 的 `mod tests`：

```rust
    #[tokio::test]
    async fn glob_reports_relative_path_against_the_matching_root() {
        let primary = tempfile::TempDir::new().unwrap();
        let secondary = tempfile::TempDir::new().unwrap();
        std::fs::write(secondary.path().join("sib.rs"), "fn main() {}").unwrap();
        let ctx = Arc::new(ToolsContext::with_roots(vec![
            primary.path().to_path_buf(),
            secondary.path().to_path_buf(),
        ]));
        let tool = GlobTool::new(ctx);
        let out = tool
            .call(serde_json::json!({
                "pattern": "sib.rs",
                "path": secondary.path().to_string_lossy(),
            }))
            .await;
        assert!(format!("{out:?}").contains("sib.rs"));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib context`
Expected: 编译失败（`with_roots` / `roots` / `relative_to_roots` 不存在）。

- [ ] **Step 3: 实现 `context.rs`**

```rust
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Shared context for all builtin tools.
///
/// `roots` is the session's root set: `roots[0]` is the primary root and is the
/// base for every relative path; later entries are linked roots the session may
/// also read and write. `cwd` persists across BashTool calls and is always
/// relative to the primary root.
pub struct ToolsContext {
    roots: Vec<PathBuf>,
    cwd: Mutex<PathBuf>,
}

impl ToolsContext {
    /// Single-root constructor. Equivalent to [`Self::with_roots`] with one root.
    pub fn new(root: PathBuf) -> Self {
        Self::with_roots(vec![root])
    }

    pub fn with_roots(roots: Vec<PathBuf>) -> Self {
        let cwd = roots.first().cloned().unwrap_or_default();
        Self {
            roots,
            cwd: Mutex::new(cwd),
        }
    }

    /// The primary root: the base for relative paths and for `cwd`.
    pub fn root(&self) -> &Path {
        self.roots.first().map(PathBuf::as_path).unwrap_or(Path::new(""))
    }

    /// The whole root set, primary first.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// `path` expressed relative to whichever root contains it; `None` when it
    /// is outside every root. Used by glob/grep so reported matches stay relative
    /// to the root they actually came from.
    pub fn relative_to_roots(&self, path: &Path) -> Option<String> {
        self.roots
            .iter()
            .find_map(|root| path.strip_prefix(root).ok())
            .map(|rel| rel.to_string_lossy().to_string())
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    pub fn set_cwd(&self, path: PathBuf) {
        *self.cwd.lock().unwrap() = path;
    }
}
```

（`use std::path::{Path, PathBuf};` 需要补 `Path`。）

- [ ] **Step 4: 实现 FS 工具改用根集**

逐个把 `self.ctx.root()` 换成根集调用：

`fs/read.rs:68`、`fs/edit.rs:68`、`fs/view_image.rs:87`、`fs/read_document.rs:377`：

```rust
        let resolved = match resolve_and_check_roots(self.ctx.roots(), &args.path) {
```

（`use crate::fs::path_util::resolve_and_check_roots;` 替换原 import。）

`fs/write.rs:53`：

```rust
        let resolved = match resolve_for_write_roots(self.ctx.roots(), &args.path) {
```

`fs/glob.rs:58-76` 的 base 与相对路径：

```rust
        let base = match &args.path {
            Some(p) if Path::new(p).is_absolute() => PathBuf::from(p),
            Some(p) => self.ctx.root().join(p),
            None => self.ctx.root().to_path_buf(),
        };
        // …glob 执行不变…
        let mut matches: Vec<String> = Vec::new();
        for entry in paths_iter {
            match entry {
                Ok(path) => {
                    // 命中哪个根就相对哪个根报，避免副目录的匹配被报成主根下的路径。
                    let shown = self
                        .ctx
                        .relative_to_roots(&path)
                        .unwrap_or_else(|| path.to_string_lossy().to_string());
                    matches.push(shown);
                }
                Err(_) => continue,
            }
        }
```

`fs/grep.rs:87-106` 同款：`base` 用绝对路径直通 / 相对拼主根，末尾把 `self.ctx.root()` 那个"相对化基点"参数换成 `self.ctx.roots()`，并在 `grep_search` 内用 `ctx.relative_to_roots` 同款逻辑相对化输出。

**若 `grep_search` 签名是「收一个 `&Path` 作为展示基点」**：改为收 `roots: &[PathBuf]`，其内部把每个命中路径相对化时改用"命中哪个根就相对哪个根"（与 glob 同一规则），而不是 `strip_prefix(单一基点)`。

`fs/lib.rs:83` 的 `ToolsContext::new(root)` 调用改为按新参数（见 Task 5）。

- [ ] **Step 5: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib`
Expected: PASS（含 `fs::*` 既有测试；`glob` / `grep` / `read` / `write` / `edit` 全部绿）。

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/context.rs yi-agent-rs/crates/yi-agent-tools/src/fs
git commit -m "feat(tools): let fs tools operate on the whole root set"
```

---

## Task 4: 权限检查器按命中根取基点

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/permission.rs`

**Interfaces:**
- Consumes: 无。
- Produces:
  - `PermissionChecker::with_roots(config, yolo, roots: Vec<PathBuf>, blocklist_fn)`（新增）
  - `PermissionChecker::new(config, yolo, workdir: PathBuf, blocklist_fn)`（保留；转调 `with_roots(vec![workdir])`）

- [ ] **Step 1: 写失败测试**

追加到 `permission.rs` 的 `mod tests`：

```rust
    #[test]
    fn path_whitelist_resolves_relative_to_the_matching_root() {
        let primary = std::path::PathBuf::from("/tmp/perm-primary");
        let secondary = std::path::PathBuf::from("/tmp/perm-secondary");
        let cfg = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                write: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = PermissionChecker::with_roots(
            cfg,
            crate::autonomy::YoloSwitch::new(false),
            vec![primary.clone(), secondary.clone()],
            Arc::new(|_| None),
        );
        // 副目录里的 src/** 也应命中白名单（关联即"B 属于这个项目"）。
        let input = serde_json::json!({ "path": "/tmp/perm-secondary/src/main.rs" });
        assert!(matches!(checker.check("write", &input), CheckResult::Allow));
    }

    #[test]
    fn path_whitelist_still_rejects_outside_every_root() {
        let cfg = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                write: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = PermissionChecker::with_roots(
            cfg,
            crate::autonomy::YoloSwitch::new(false),
            vec![std::path::PathBuf::from("/tmp/perm-a")],
            Arc::new(|_| None),
        );
        let input = serde_json::json!({ "path": "/etc/passwd" });
        assert!(matches!(checker.check("write", &input), CheckResult::NeedConfirm(_)));
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib permission`
Expected: 编译失败（`with_roots` 不存在）。

- [ ] **Step 3: 实现**

```rust
pub struct PermissionChecker {
    config: Mutex<PermissionsConfig>,
    yolo: crate::autonomy::YoloSwitch,
    /// 会话根集，`roots[0]` 为主根。相对路径白名单按**命中的那个根**解析。
    roots: Vec<std::path::PathBuf>,
    blocklist_fn: BlocklistFn,
    next_request_id: AtomicU64,
}

impl PermissionChecker {
    /// 单根构造（既有调用点不破）。
    pub fn new(
        config: PermissionsConfig,
        yolo: crate::autonomy::YoloSwitch,
        workdir: std::path::PathBuf,
        blocklist_fn: BlocklistFn,
    ) -> Self {
        Self::with_roots(config, yolo, vec![workdir], blocklist_fn)
    }

    pub fn with_roots(
        config: PermissionsConfig,
        yolo: crate::autonomy::YoloSwitch,
        roots: Vec<std::path::PathBuf>,
        blocklist_fn: BlocklistFn,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            yolo,
            roots,
            blocklist_fn,
            next_request_id: AtomicU64::new(1),
        }
    }
```

`check` 里把 `&self.workdir` 换成 `&self.roots`，`prefix_level_allows` 改为：

```rust
    fn prefix_level_allows(
        config: &PermissionsConfig,
        tool_name: &str,
        tool_input: &serde_json::Value,
        roots: &[Path],
    ) -> bool {
        match tool_name {
            "bash" | "process_start" => { /* 不变 */ }
            "write" | "edit" => {
                let Some(path) = tool_input.get("path").and_then(|v| v.as_str()) else {
                    return false;
                };
                let primary = match roots.first() {
                    Some(root) => root,
                    None => return false,
                };
                let abs = if Path::new(path).is_absolute() {
                    Path::new(path).to_path_buf()
                } else {
                    primary.join(path)
                };
                let patterns = match tool_name {
                    "write" => &config.prefix_level.write.paths,
                    "edit" => &config.prefix_level.edit.paths,
                    _ => return false,
                };
                // 按**命中哪个根**取相对路径：副目录里的 src/** 也能命中白名单。
                // 必须遍历全部根——只对主根判断会让关联目录的白名单永远不生效。
                roots.iter().any(|root| match abs.strip_prefix(root) {
                    Ok(rel) => {
                        let rel_str = rel.to_string_lossy();
                        patterns.iter().any(|pattern| glob_match(pattern, &rel_str))
                    }
                    Err(_) => false,
                })
            }
            _ => false,
        }
    }
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-core --lib permission`
Expected: PASS（含既有 permission 测试）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-core/src/permission.rs
git commit -m "feat(core): resolve path permissions against the session root set"
```

---

## Task 5: 注册路径接收 FS 根集，app-server 按会话下发

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:58-105`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs:176-235,319-340`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（thread-local 槽位、`production_factory`、`build_runtime_tooling`）

**Interfaces:**
- Consumes: `ToolsContext::with_roots`（Task 3）、`PermissionChecker::with_roots`（Task 4）。
- Produces:
  - `register_builtin_tools_with_controller(registry, roots: Vec<PathBuf>, controller, sandbox_writable_roots: Vec<PathBuf>)`
  - `register_builtin_tools_with_sandbox(registry, roots: Vec<PathBuf>, sandbox_mode, sandbox_writable_roots)`
  - `register_builtin_tools(registry, root: PathBuf)`（保留：单根 + 空额外汇总项）
  - `build_tool_setup_with_controller(cfg, naked, roots: &[PathBuf], controller)`
  - `server::with_session_extra_roots<T>(roots: Vec<PathBuf>, f) -> T`
  - `server::current_session_extra_roots() -> Vec<PathBuf>`

- [ ] **Step 1: 写失败测试**

追加到 `yi-agent-tools/src/lib.rs` 的测试模块：

```rust
    #[test]
    fn builtin_registration_accepts_a_root_set() {
        let primary = tempfile::TempDir::new().unwrap();
        let secondary = tempfile::TempDir::new().unwrap();
        let mut registry = yi_agent_core::ToolRegistry::new();
        register_builtin_tools_with_sandbox(
            &mut registry,
            vec![primary.path().to_path_buf(), secondary.path().to_path_buf()],
            SandboxMode::WorkspaceWrite,
            vec![secondary.path().to_path_buf()],
        );
        let names: Vec<String> = registry
            .schemas()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(names.contains(&"write".to_string()));
    }
```

追加到 `server.rs` 的测试模块：

```rust
    #[test]
    fn session_extra_roots_slot_scopes_and_restores() {
        assert!(current_session_extra_roots().is_empty());
        let roots = vec![PathBuf::from("/tmp/slot-a"), PathBuf::from("/tmp/slot-b")];
        let seen = with_session_extra_roots(roots.clone(), current_session_extra_roots);
        assert_eq!(seen, roots);
        assert!(current_session_extra_roots().is_empty());
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools --lib builtin_registration_accepts_a_root_set`
Expected: 编译失败（签名不匹配）。

- [ ] **Step 3: 实现注册函数**

`yi-agent-tools/src/lib.rs`：

```rust
/// Register all builtin tools into the given registry.
///
/// `root` constrains FS tool operations and shell writes to the given directory.
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    register_builtin_tools_with_sandbox(registry, vec![root], SandboxMode::WorkspaceWrite, Vec::new());
}

/// Register builtin tools with an explicit shell sandbox policy.
///
/// `roots` is the session root set (`[0]` = primary); it scopes FS tools.
/// `sandbox_writable_roots` is the **shell** sandbox's extra write list and is
/// deliberately separate: worker paths put git internals there, and those must
/// not become FS-tool write roots.
pub fn register_builtin_tools_with_sandbox(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    sandbox_mode: SandboxMode,
    sandbox_writable_roots: Vec<PathBuf>,
) {
    let controller = SandboxController::new(
        yi_agent_core::autonomy::YoloSwitch::new(false),
        sandbox_mode,
        false,
    );
    register_builtin_tools_with_controller(registry, roots, controller, sandbox_writable_roots);
}

pub fn register_builtin_tools_with_controller(
    registry: &mut ToolRegistry,
    roots: Vec<PathBuf>,
    controller: SandboxController,
    sandbox_writable_roots: Vec<PathBuf>,
) {
    let ctx = Arc::new(ToolsContext::with_roots(roots));
    let sandbox = SandboxPolicy::with_controller(ctx.root(), sandbox_writable_roots, controller);
    // …以下现有注册逻辑不变…
}
```

- [ ] **Step 4: 实现 bootstrap 传递**

`yi-agent-runtime/src/bootstrap.rs` 的 `build_tool_setup_with_controller`：

```rust
pub fn build_tool_setup_with_controller(
    cfg: &RuntimeConfig,
    naked: bool,
    roots: &[PathBuf],
    controller: yi_agent_tools::SandboxController,
) -> Result<ToolSetup> {
    // …naked 分支不变…
    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("a session needs at least one workspace root"))?;

    // FS 根集含全部关联根；shell 沙箱的可写根只取"关联根 + cfg 配置的额外汇总项"。
    let mut sandbox_writable = roots[1..].to_vec();
    sandbox_writable.extend(cfg.sandbox_writable_roots.iter().cloned());
    sandbox_writable.sort();
    sandbox_writable.dedup();

    yi_agent_tools::register_builtin_tools_with_controller(
        &mut registry,
        roots.to_vec(),
        controller.clone(),
        sandbox_writable.clone(),
    );

    let process_manager = yi_agent_tools::ProcessManager::with_controller(
        primary,
        controller,
        sandbox_writable,
    );
    // …其余不变…
}
```

`build_tool_setup_in` / `build_tool_setup_with_switch` 同步改为收 `roots: &[PathBuf]`：

```rust
pub fn build_tool_setup(cfg: &RuntimeConfig, naked: bool) -> Result<ToolSetup> {
    build_tool_setup_in(cfg, naked, std::slice::from_ref(&cfg.workdir))
}
```

`bootstrap_agent`：把根集改为"主根 + `cfg.sandbox_writable_roots`"（对主线程而言后者就是关联根）：

```rust
    let mut roots = vec![cfg.workdir.clone()];
    roots.extend(cfg.sandbox_writable_roots.iter().cloned());
    let setup = build_tool_setup_with_switch(cfg, false, &roots, switch.clone())?;
    let checker = load_permission_checker_with_roots(&roots, switch.clone())?;
```

`load_permission_checker_with_switch` 保留（单根转调），新增：

```rust
pub fn load_permission_checker_with_roots(
    roots: &[PathBuf],
    switch: yi_agent_core::autonomy::YoloSwitch,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    let roots = roots.to_vec();
    let permissions = std::thread::scope(|scope| {
        let probe = roots.clone();
        scope
            .spawn(move || {
                let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
                rt.block_on(yi_agent_core::permission::PermissionChecker::load(
                    probe.first().map(PathBuf::as_path).unwrap_or(Path::new(".")),
                ))
            })
            .join()
    })
    .map_err(|_| anyhow::anyhow!("permission loader thread panicked"))?
    .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?;

    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    Ok(Arc::new(yi_agent_core::permission::PermissionChecker::with_roots(
        permissions,
        switch,
        roots,
        blocklist_fn,
    )))
}
```

- [ ] **Step 5: 实现 app-server 槽位与工厂**

在 `server.rs` 的 `SESSION_REASONING_EFFORT` 之后追加：

```rust
thread_local! {
    /// 本次 `build_agent` 调用所属会话的关联根（主根之外的附属根）。与
    /// [`SESSION_MODEL_REF`] 同一理由：工厂闭包签名带不进逐会话参数。
    ///
    /// **必须逐调用传**：daemon 的 worker factory 按项目目录构建、被同目录多个
    /// 会话共享，把根集写进共享 `cfg` 会让 A 会话的关联泄漏给同目录的其它会话。
    static SESSION_EXTRA_ROOTS: std::cell::RefCell<Vec<PathBuf>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 在调用工厂前设定本次会话的关联根；返回后恢复，故嵌套调用不泄漏。
pub(crate) fn with_session_extra_roots<T>(roots: Vec<PathBuf>, f: impl FnOnce() -> T) -> T {
    SESSION_EXTRA_ROOTS.with(|slot| {
        let prev = slot.replace(roots);
        let out = f();
        slot.replace(prev);
        out
    })
}

fn current_session_extra_roots() -> Vec<PathBuf> {
    SESSION_EXTRA_ROOTS.with(|slot| slot.borrow().clone())
}
```

`production_factory` 内：

```rust
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        // 本会话的关联根（逐调用传入，不共享）。空 = 单根会话，与改动前一致。
        thread_cfg.sandbox_writable_roots = current_session_extra_roots();
        // …resolve_effective / effort 覆盖不变…
```

`build_runtime_tooling`（委派路径）同样把根集交给工具集：

```rust
fn build_runtime_tooling(
    cfg: &RuntimeConfig,
    root: &Arc<ThreadRoot>,
    thread_id: &str,
    yolo: yi_agent_core::autonomy::YoloSwitch,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
    extra_roots: Vec<PathBuf>,
) -> Result<RuntimeTooling, String> {
    let workspace_root = root.handle()?.workspace_root;
    let mut roots = vec![workspace_root.clone()];
    roots.extend(extra_roots.iter().cloned());
    let mut sandbox_extras = extra_roots.clone();
    sandbox_extras.extend(cfg.sandbox_writable_roots.iter().cloned());

    let controller = yi_agent_tools::SandboxController::new(yolo.clone(), cfg.sandbox, cfg.sandbox_promotable);
    let setup = yi_agent_runtime::bootstrap::build_tool_setup_with_controller(cfg, false, &roots, controller.clone())
        .map_err(|error| error.to_string())?;
    // …register_attached_root_tools_in_thread 等不变…
    let permission = yi_agent_runtime::bootstrap::load_permission_checker_with_roots(&roots, yolo)
        .map_err(|error| error.to_string())?;
```

`attach_delegation` 增加 `extra_roots: Vec<PathBuf>` 参数并透传给 `build_runtime_tooling`（两个调用点：`start_thread_core` 与 `thread/resume` 分支）。两个调用点的实参都来自 `current_session_extra_roots()`——它们已经在 `with_session_extra_roots` 之内（见 Task 6）。

- [ ] **Step 6: 补既有构造点的空根集**

`crates/yi-agent/src/main.rs:948,1245` 与 `yi-agent-subagent/src/lib.rs:291,335` 的 `register_builtin_tools_with_sandbox` 调用点改为新签名（TUI/CLI 单根：`vec![root]`，可写根用原值；subagent 用 `vec![workspace.path.clone()]` 并把原来的 `writable_roots` 原样作 sandbox 额外汇总项）。**工作流 2 会再把继承根集接进这些点。**

- [ ] **Step 7: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-core && cargo test -p yi-agent-runtime`
Expected: 全绿。

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-tools/src/lib.rs yi-agent-rs/crates/yi-agent-runtime/src/bootstrap.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat: thread the session root set into tools, sandbox and permissions"
```

---

## Task 6: `thread/start` 落 meta、`thread/resume` 重建、`workspace/link|unlink`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:27-72,758-775`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`thread/start`、`thread/resume`、`workspace/*` 分支、`start_thread_core`）

**Interfaces:**
- Consumes: `WorkspaceIndex::{link,unlink,children,linked_to}`（Task 1）、`with_session_extra_roots`（Task 5）。
- Produces:
  - `ThreadMeta.extra_roots: Vec<String>`
  - 判定函数 `fn linked_extra_roots(workspaces: &WorkspaceIndex, cwd: &str) -> Vec<PathBuf>`
  - RPC `workspace/link` `{primary, child}` / `workspace/unlink` `{child}`
  - RPC `workspace/list` 每项新增 `linked_to: Option<String>`

- [ ] **Step 1: 写失败测试**

追加到 `server.rs` 的测试模块：

```rust
    #[tokio::test]
    async fn thread_start_records_linked_roots_and_resume_rebuilds_them() {
        let dir = tempfile::TempDir::new().unwrap();
        let primary = dir.path().join("primary");
        let secondary = dir.path().join("secondary");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&secondary).unwrap();

        let mut h = harness_with_home(dir.path()).await;
        // 关联：primary ← secondary
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"workspace/link","params":{{"primary":"{}","child":"{}"}}}}"#,
            primary.display(),
            secondary.display()
        ))
        .await;
        let linked = h.read_value().await;
        assert_eq!(linked["result"], serde_json::json!({}));

        // 在 primary 建会话 → meta 应记下 secondary
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{"cwd": primary.to_string_lossy()}
        });
        h.send(&req.to_string()).await;
        let started = h.read_value().await;
        let tid = started["result"]["threadId"].as_str().unwrap().to_string();

        let meta_raw = std::fs::read_to_string(
            primary.join(".yi-agent").join("threads").join(&tid).join(".meta.json"),
        )
        .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap();
        assert_eq!(meta["extra_roots"], serde_json::json!([
            std::fs::canonicalize(&secondary).unwrap().to_string_lossy()
        ]));
    }

    #[tokio::test]
    async fn linked_roots_do_not_leak_across_threads_in_one_cwd() {
        // 同一 cwd 两个会话、关联不同：各自只拿到自己的根集。
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().join("proj");
        let other = dir.path().join("other");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut h = harness_with_home(dir.path()).await;
        // 不关联：该 cwd 的会话根集必须为空。
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"thread/start",
            "params":{"cwd": cwd.to_string_lossy()}
        });
        h.send(&req.to_string()).await;
        let started = h.read_value().await;
        let tid = started["result"]["threadId"].as_str().unwrap().to_string();
        let meta_raw = std::fs::read_to_string(
            cwd.join(".yi-agent").join("threads").join(&tid).join(".meta.json"),
        )
        .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap();
        assert_eq!(meta["extra_roots"], serde_json::json!([]));
        let _ = other;
    }

    #[tokio::test]
    async fn workspace_link_rejects_self_and_unknown_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir_all(&a).unwrap();
        let mut h = harness_with_home(dir.path()).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"workspace/link","params":{{"primary":"{p}","child":"{p}"}}}}"#,
            p = a.display()
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"workspace/link","params":{{"primary":"{p}","child":"{n}"}}}}"#,
            p = a.display(),
            n = dir.path().join("nope").display()
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn workspace_list_reports_linked_to() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut h = harness_with_home(dir.path()).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"workspace/link","params":{{"primary":"{a}","child":"{b}"}}}}"#,
            a = a.display(),
            b = b.display()
        ))
        .await;
        h.read_value().await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"workspace/list","params":{}}"#).await;
        let v = h.read_value().await;
        let list = v["result"]["workspaces"].as_array().unwrap();
        let b_canon = std::fs::canonicalize(&b).unwrap().to_string_lossy().to_string();
        let entry = list
            .iter()
            .find(|w| w["path"] == serde_json::Value::String(b_canon.clone()))
            .expect("linked dir must be listed");
        assert_eq!(
            entry["linked_to"],
            serde_json::Value::String(std::fs::canonicalize(&a).unwrap().to_string_lossy().to_string())
        );
    }
```

**注意**：`harness_with_home` 若不存在，用该文件既有 harness 构造（`RuntimeAttachments` 注入隔离 tempdir 的既有写法，参见 `test_config` / harness 定义），把 `workspaces` 指向 `dir/workspaces.json`。若既有 harness 已支持注入 HOME，直接复用其构造而不要新造。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib workspace_link_rejects_self_and_unknown_dir`
Expected: FAIL（`workspace/link` 未实现，回 method-not-found 或 `extra_roots` 缺失）。

- [ ] **Step 3: 实现 `ThreadMeta.extra_roots`**

```rust
    /// 该会话的关联根（主根之外的附属根，canonical 绝对路径）。
    /// 创建会话时从本机索引快照，运行期不变；旧 meta 缺字段时为空 = 单根会话。
    #[serde(default)]
    pub extra_roots: Vec<String>,
```

`rebuild_meta` 补 `extra_roots: Vec::new()`；其余 16 处 `ThreadMeta { .. }` 字面量按编译器提示补 `extra_roots: Vec::new()`（测试构造点用空集即可）。

- [ ] **Step 4: 实现根集解析与 RPC**

`server.rs` 增加：

```rust
/// 解析某主根当前关联的附属根：取索引里的记录，丢掉已不存在的目录。
///
/// 静默丢弃（而非报错）是有意的：附属根是能力增量，它消失不该让会话打不开；
/// `cwd` 本身已删仍照既有行为报错，因为那是权威基点。
fn linked_extra_roots(workspaces: &WorkspaceIndex, cwd: &str) -> Vec<PathBuf> {
    workspaces
        .children(Path::new(cwd))
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .collect()
}
```

`workspace/list` 分支：

```rust
                    "workspace/list" => {
                        let dirs = workspaces.list();
                        let list: Vec<serde_json::Value> = dirs
                            .iter()
                            .map(|p| {
                                // 只在主根仍登记时上报 linked_to：主根被「Remove from
                                // list」后，附属根自动回到顶层显示（见 spec §7）。
                                let linked_to = workspaces
                                    .linked_to(Path::new(p))
                                    .filter(|primary| dirs.iter().any(|d| d == primary));
                                json!({
                                    "path": p,
                                    "exists": Path::new(p).is_dir(),
                                    "linked_to": linked_to,
                                })
                            })
                            .collect();
                        write_response(&hub, &client, ok_response(id, json!({ "workspaces": list })))
                            .await?;
                    }
                    "workspace/link" => {
                        let primary = req.params.get("primary").and_then(|v| v.as_str()).unwrap_or("");
                        let child = req.params.get("child").and_then(|v| v.as_str()).unwrap_or("");
                        match workspaces.link(Path::new(primary), Path::new(child)) {
                            Ok(()) => {
                                // 附属根也进最近目录：`thread/listAll` 靠它发现冷会话。
                                let canonical =
                                    std::fs::canonicalize(child).unwrap_or_else(|_| PathBuf::from(child));
                                if let Err(e) = workspaces.add(&canonical) {
                                    eprintln!("[app-server] failed to index linked workspace: {e}");
                                }
                                write_response(&hub, &client, ok_response(id, json!({}))).await?
                            }
                            Err(e) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::invalid_params(e.0)),
                                )
                                .await?
                            }
                        }
                    }
                    "workspace/unlink" => {
                        let child = req.params.get("child").and_then(|v| v.as_str()).unwrap_or("");
                        let key = std::fs::canonicalize(child).unwrap_or_else(|_| PathBuf::from(child));
                        match workspaces.unlink(&key) {
                            Ok(()) => write_response(&hub, &client, ok_response(id, json!({}))).await?,
                            Err(e) => {
                                write_response(
                                    &hub,
                                    &client,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?
                            }
                        }
                    }
```

- [ ] **Step 5: 实现 `thread/start` 与 `thread/resume` 的接线**

`thread/start` 分支：

```rust
                        let extra_roots = linked_extra_roots(&workspaces, &cwd);
                        let built = match with_session_model_ref(model_ref.clone(), || {
                            with_session_extra_roots(extra_roots.clone(), || {
                                with_session_reasoning_effort(None, || {
                                    build_agent(None, Path::new(&cwd), mode)
                                })
                            })
                        }) {
```

并把 `extra_roots` 传给 `start_thread_core`（新增参数 `extra_roots: Vec<PathBuf>`），后者用它对 `attach_delegation` 传入 `extra_roots.clone()`、对 meta 写 `extra_roots: extra_roots.iter().map(|p| p.to_string_lossy().to_string()).collect()`。

`thread/resume` 分支：

```rust
                        // 附属根已删则静默丢弃；cwd 已删仍走上方既有报错。
                        let extra_roots: Vec<PathBuf> = loaded
                            .meta
                            .extra_roots
                            .iter()
                            .map(PathBuf::from)
                            .filter(|p| p.is_dir())
                            .collect();
                        let built = match with_session_model_ref(loaded.meta.model_ref.clone(), || {
                            with_session_extra_roots(extra_roots.clone(), || {
                                with_session_reasoning_effort(loaded.meta.reasoning_effort.clone(), || {
                                    build_agent(Some(session), Path::new(&cwd), mode)
                                })
                            })
                        }) {
```

并同样把 `extra_roots` 传给 resume 侧的 `attach_delegation`。

**关键**：两处 `attach_delegation` 调用都必须在 `with_session_extra_roots` 之外或之内**读取同一个值**——实现上用局部变量 `extra_roots`（上面已捕获），不要在 `attach_delegation` 里再读槽位。

- [ ] **Step 6: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿（397+ 既有测试 + 新增 4 个）。

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): snapshot linked roots per thread and add link/unlink RPCs"
```

---

## Task 7: 桌面协议与附属根归拢纯函数

**Files:**
- Modify: `desktop/src/lib/protocol.ts:280-286`
- Modify: `desktop/src/lib/workspaceGroups.ts`
- Test: `desktop/src/lib/workspaceGroups.test.ts`

**Interfaces:**
- Consumes: `WorkspaceGroup` / `ThreadSummary`（既有）。
- Produces:
  - `Workspace.linked_to?: string | null`
  - `nestLinkedGroups(groups: WorkspaceGroup[], workspaces: Workspace[]): NestedGroups`，其中 `NestedGroups = { top: NestedGroup[] }`、`NestedGroup = { group: WorkspaceGroup; children: WorkspaceGroup[] }`

- [ ] **Step 1: 写失败测试**

追加到 `workspaceGroups.test.ts`：

```ts
import { nestLinkedGroups } from "./workspaceGroups";
import type { Workspace } from "./protocol";

const ws = (path: string, linked_to: string | null = null): Workspace => ({
  path,
  exists: true,
  linked_to,
});

describe("nestLinkedGroups", () => {
  it("hides a linked child from the top level and nests it under its primary", () => {
    const groups = [mk("/a", ["1"]), mk("/b", ["2"])];
    const out = nestLinkedGroups(groups, [ws("/a"), ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/a"]);
    expect(out.top[0].children.map((c) => c.workspace)).toEqual(["/b"]);
  });

  it("keeps an unlinked dir at the top level", () => {
    const out = nestLinkedGroups([mk("/a", ["1"])], [ws("/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/a"]);
    expect(out.top[0].children).toEqual([]);
  });

  it("promotes a child back to the top when its primary is absent", () => {
    // 主根被 Remove from list 后，服务端不再上报 linked_to，但即便上报了、
    // 主根不在 groups 里也不能把子目录藏起来。
    const out = nestLinkedGroups([mk("/b", ["2"])], [ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/b"]);
    expect(out.top[0].children).toEqual([]);
  });

  it("keeps group order stable", () => {
    const groups = [mk("/z", ["1"]), mk("/a", ["2"]), mk("/b", ["3"])];
    const out = nestLinkedGroups(groups, [ws("/z"), ws("/a"), ws("/b", "/a")]);
    expect(out.top.map((n) => n.group.workspace)).toEqual(["/z", "/a"]);
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/lib/workspaceGroups.test.ts`
Expected: FAIL（`nestLinkedGroups` 未导出）。

- [ ] **Step 3: 实现**

`protocol.ts` 的 `Workspace`：

```ts
/** `workspace/list` 的单项。 */
export interface Workspace {
  path: string;
  exists: boolean;
  /** 该目录被关联到的主根；null / 缺省 = 顶层目录。 */
  linked_to?: string | null;
}
```

`workspaceGroups.ts` 追加：

```ts
/** 一个顶层组及其附属根小节。 */
export interface NestedGroup {
  group: WorkspaceGroup;
  children: WorkspaceGroup[];
}

export interface NestedGroups {
  top: NestedGroup[];
}

/**
 * 按 `linked_to` 把附属根从顶层摘下来，挂到主根的 `children` 里。
 *
 * 规则（与 spec §7 一致）：只有"主根也在本批 groups 里"时才嵌套，否则附属根
 * 留在顶层——否则主根被 Remove from list 后子目录会凭空消失。
 *
 * 在 `filterThreadTree` **之后**调用：搜索、置顶、看板会话三个既有分支都不受影响。
 */
export function nestLinkedGroups(
  groups: WorkspaceGroup[],
  workspaces: Workspace[],
): NestedGroups {
  const primaryOf = new Map<string, string>();
  for (const w of workspaces) {
    if (w.linked_to) primaryOf.set(w.path, w.linked_to);
  }
  const present = new Set(groups.map((g) => g.workspace));
  const childrenOf = new Map<string, WorkspaceGroup[]>();
  const top: NestedGroup[] = [];
  for (const g of groups) {
    const primary = primaryOf.get(g.workspace);
    if (primary && present.has(primary)) {
      const list = childrenOf.get(primary) ?? [];
      list.push(g);
      childrenOf.set(primary, list);
    } else {
      top.push({ group: g, children: [] });
    }
  }
  for (const node of top) {
    node.children = childrenOf.get(node.group.workspace) ?? [];
  }
  return { top };
}
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/lib/workspaceGroups.test.ts && npx tsc --noEmit`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/workspaceGroups.ts desktop/src/lib/workspaceGroups.test.ts
git commit -m "feat(desktop): nest linked workspaces under their primary group"
```

---

## Task 8: 侧栏菜单项与嵌套渲染

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: `nestLinkedGroups`（Task 7）。
- Produces: 新增 props `onLinkFolder: (primary: string) => void`、`onUnlinkFolder: (child: string) => void`。

- [ ] **Step 1: 写失败测试**

追加到 `ThreadSidebar.test.tsx`：

```tsx
function renderSidebar(overrides: Partial<ComponentProps<typeof ThreadSidebar>> = {}) {
  const props = {
    groups: [{ workspace: "/a", exists: true, threads: [thread("1", "one", "/a")] },
              { workspace: "/b", exists: true, threads: [thread("2", "two", "/b")] }],
    workspaces: [
      { path: "/a", exists: true, linked_to: null },
      { path: "/b", exists: true, linked_to: "/a" },
    ],
    currentId: null,
    pinned: [],
    statuses: new Map(),
    unread: new Map(),
    onSelect: vi.fn(),
    onRename: vi.fn(),
    onDelete: vi.fn(),
    onTogglePin: vi.fn(),
    onReorderPinned: vi.fn(),
    onNew: vi.fn(),
    onRemoveWorkspace: vi.fn(),
    onBrowse: vi.fn(),
    boards: [],
    boardSummaries: {},
    selectedBoard: null,
    onCreateBoard: vi.fn(),
    onRemoveBoard: vi.fn(),
    onOpenBoard: vi.fn(),
    onOpenSettings: vi.fn(),
    onLinkFolder: vi.fn(),
    onUnlinkFolder: vi.fn(),
    ...overrides,
  };
  render(<ThreadSidebar {...props} />);
  return props;
}

it("nests a linked folder under its primary and hides it from the top level", () => {
  renderSidebar();
  // 顶层只有 /a 一个组头。
  expect(screen.getAllByText("a")).toHaveLength(1);
  // /b 作为附属根小节出现。
  expect(screen.getByText("b")).toBeTruthy();
  expect(screen.getByText("b").closest("[data-linked-child]")).toBeTruthy();
});

it("offers 关联文件夹 on the group menu and calls onLinkFolder", () => {
  const props = renderSidebar();
  fireEvent.contextMenu(screen.getByText("a").closest("[data-ws-header]")!);
  fireEvent.click(screen.getByText("关联文件夹…"));
  expect(props.onLinkFolder).toHaveBeenCalledWith("/a");
});

it("offers 取消关联 on a linked child and calls onUnlinkFolder", () => {
  const props = renderSidebar();
  fireEvent.contextMenu(screen.getByText("b").closest("[data-linked-child]")!);
  fireEvent.click(screen.getByText("取消关联"));
  expect(props.onUnlinkFolder).toHaveBeenCalledWith("/b");
});

it("does not offer New thread here for a linked child", () => {
  renderSidebar();
  fireEvent.contextMenu(screen.getByText("b").closest("[data-linked-child]")!);
  expect(screen.queryByText("New thread here")).toBeNull();
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL（新 props 未接受 / 菜单项不存在）。

- [ ] **Step 3: 实现**

- props 增加 `onLinkFolder` / `onUnlinkFolder`，并加进类型声明。
- 菜单 state 的联合类型扩展为 `{ kind: "group"; ws: string } | { kind: "child"; ws: string } | { kind: "thread"; id: string }`。
- 组头菜单（现有三项之后）追加：

```tsx
                      <button
                        type="button"
                        role="menuitem"
                        onClick={() => {
                          closeMenus();
                          onLinkFolder(g.workspace);
                        }}
                        className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
                      >
                        关联文件夹…
                      </button>
```

- 渲染改为先归拢再渲染：

```tsx
  const nested = nestLinkedGroups(filtered.groups, workspaces);
  // …
  {nested.top.map(({ group: g, children }) => (
    <div key={g.workspace} className="relative">
      {/* …现有组头与菜单不变… */}
      {children.map((child) => (
        <LinkedChildSection
          key={child.workspace}
          group={child}
          menuOpen={menu?.kind === "child" && menu.ws === child.workspace}
          onOpenMenu={() => setMenu({ kind: "child", ws: child.workspace })}
          onCloseMenu={closeMenus}
          onUnlink={() => onUnlinkFolder(child.workspace)}
          renderThread={renderThread}
        />
      ))}
      {/* …现有看板与会话行… */}
    </div>
  ))}
```

- `LinkedChildSection`（同文件内的局部组件）：外层 `data-linked-child={group.workspace}`、`pl-3` 缩进、caret 折叠、行内 `basename(group.workspace)`（`exists === false` 时照现有口径显示 ` (missing)`）、右键菜单只有「取消关联」。**不渲染「New thread here」**（附属根不能作 cwd）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx && npx tsc --noEmit`
Expected: PASS（含既有 ThreadSidebar 全量测试）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/ThreadSidebar.tsx desktop/src/components/ThreadSidebar.test.tsx
git commit -m "feat(desktop): link and unlink folders from the sidebar menu"
```

---

## Task 9: `App` 接线与进度登记

**Files:**
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/App.test.tsx`
- Modify: `docs/project-management/{desktop,yi-agent-app-server,yi-agent-tools}.md`、`README.md`

**Interfaces:**
- Consumes: `ThreadSidebar` 的 `onLinkFolder` / `onUnlinkFolder`（Task 8）、`workspace/link|unlink`（Task 6）。
- Produces: 无新导出。

- [ ] **Step 1: 写失败测试**

追加到 `App.test.tsx`（沿用该文件既有 fake client 写法）：

```tsx
  it("links a picked folder to the group it was invoked from", async () => {
    pickDirectoryMock.mockResolvedValue("/tmp/child");
    const { client } = await renderApp();
    await client.emitMenuLinkFolder("/tmp/primary");
    expect(client.requests).toContainEqual({
      method: "workspace/link",
      params: { primary: "/tmp/primary", child: "/tmp/child" },
    });
  });

  it("unlinks a folder and refreshes the sidebar", async () => {
    const { client } = await renderApp();
    await client.emitMenuUnlinkFolder("/tmp/child");
    expect(client.requests).toContainEqual({
      method: "workspace/unlink",
      params: { child: "/tmp/child" },
    });
  });
```

**注意**：`emitMenuLinkFolder` / `pickDirectoryMock` 若与该文件既有 harness 命名不符，用既有 harness 暴露的等价钩子（该文件已有 `pickDirectory` 的 mock 与"模拟侧栏回调"的写法，照抄其形态，不要新造 harness）。

- [ ] **Step 2: 运行测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

```tsx
  /** 关联文件夹：选目录 → workspace/link → 刷新分组与最近目录。 */
  const linkFolder = async (primary: string) => {
    try {
      const child = await pickDirectory();
      if (!child) return;
      await clientRef.current?.request("workspace/link", { primary, child });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return;
    }
    await refreshThreads();
    await refreshWorkspaces();
  };

  const unlinkFolder = async (child: string) => {
    try {
      await clientRef.current?.request("workspace/unlink", { child });
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
    }
    // 失败也刷新：保证 UI 与服务端一致（与 removeWorkspace 同口径）。
    await refreshThreads();
    await refreshWorkspaces();
  };
```

接到 `<ThreadSidebar … onLinkFolder={linkFolder} onUnlinkFolder={unlinkFolder} />`。

- [ ] **Step 4: 进度登记**

- `docs/project-management/desktop.md`：在侧栏条目附近新增 `[x]` 条目，判据写具体位置（`desktop/src/components/ThreadSidebar.tsx` 的组头菜单第四项与 `LinkedChildSection`、`desktop/src/lib/workspaceGroups.ts` 的 `nestLinkedGroups`、`desktop/src/App.tsx` 的 `linkFolder`/`unlinkFolder`）+ 验证命令 `cd desktop && npx tsc --noEmit && npm test`。
- `docs/project-management/yi-agent-app-server.md`：登记 `workspace/link` / `workspace/unlink` / `workspace/list.linked_to` / `ThreadMeta.extra_roots`，判据指向 `workspace_index.rs` 的不变量测试与 `server.rs` 的 RPC 测试。
- `docs/project-management/yi-agent-tools.md`：登记根集（`ToolsContext::roots` / `path_util::*_roots`），判据指向新增逃逸测试。
- `README.md`：更新模块索引表的"完成 / 总计"计数。

- [ ] **Step 5: 全量验证**

Run:
```
cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-core && cargo test -p yi-agent-app-server && cargo fmt --all --check && cd ..
cd desktop && npx tsc --noEmit && npm test
```
Expected: 全绿。

- [ ] **Step 6: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx docs/project-management README.md
git commit -m "feat(desktop): wire folder linking into the app shell"
```

---

## Self-Review

- **Spec 覆盖**：§1 索引 → Task 1；§2 meta 快照 → Task 6；§3 协议 → Task 6 + 7；§4 传递 → Task 5；§5 工具层 → Task 2/3/4；§6 子 agent 继承 → **工作流 2 计划**（本计划非目标）；§7 UI → Task 7/8/9；§8 错误处理 → Task 1（不变量）+ Task 6（幂等与静默丢弃）；§9 测试 → 各任务测试 + Task 9 Step 5。
- **占位符扫描**：无 TBD / TODO / 「类似 Task N」/ 空泛「加错误处理」。每个代码步骤都给出可粘贴内容。
- **类型一致性**：`resolve_and_check_roots` / `resolve_for_write_roots`（Task 2）→ FS 工具（Task 3）；`ToolsContext::roots()`（Task 3）→ glob/grep 相对化；`with_session_extra_roots` / `current_session_extra_roots`（Task 5）→ Task 6 接线；`nestLinkedGroups` 返回 `NestedGroups`（Task 7）→ Task 8 消费；`onLinkFolder` / `onUnlinkFolder`（Task 8）→ Task 9 接线。
- **已知的现场对齐点**（非占位，是既有设施的引用）：Task 3 Step 4 的 `grep_search` 签名、Task 6 的 `harness_with_home`、Task 8 的 `renderSidebar`、Task 9 的 `pickDirectory` mock 与侧栏回调钩子——实现者须先 `grep` 确认真实名称再落笔，这些点在步骤里都已注明。
