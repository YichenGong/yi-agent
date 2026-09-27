# 桌面 GUI 工作目录选择 实现计划

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 让桌面 GUI 支持「每个对话独立工作目录」+ 最近目录下拉 + 侧栏按目录分组。

**Architecture:** app-server 侧新增全局「最近目录」索引(`~/.yi-agent/workspaces.json`)与
`workspace/*` / `thread/listAll` RPC;`thread/start` 接受 `cwd`,agent 与 `ThreadStore`
改为按每个 thread 的 cwd 构建。桌面端加原生文件夹选择器,侧栏由扁平列表改为按目录分组。

**Tech Stack:** Rust(tokio, serde, anyhow)/ Tauri 2 + React 19 + TypeScript + Vitest + Tailwind v4。

**设计依据:** `docs/superpowers/specs/2026-09-27-desktop-workspace-selection-design.md`

**通用约定:**
- worktree 根:`WT=/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/desktop-workspace-selection`
- Rust 测试:`cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server ...`
- 提交前:`cd $WT/yi-agent-rs && cargo fmt --all`,再在 `$WT` 提交。
- 跑 Rust 测试前先 `ps aux | grep -c "[c]argo"` 确认无残留进程。
- 前端测试:`cd $WT/desktop && npm test`(vitest run)。
- commit message 不写 `Co-Authored-By` 行。

**Phase A 顺序:** Task 1(索引模块)→ Task 2(接线 + `workspace/*`)→
Task 3(按 cwd 构建 agent/store)→ Task 4(`thread/listAll`)。Task 3 依赖 Task 1、2 的
`find_thread_dir`;Task 2 依赖 Task 1。

---

## Phase A — 后端

### Task 1: `WorkspaceIndex` 模块（新）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`(注册 `pub mod workspace_index;`)

**Step 1: 写失败测试**

在 `workspace_index.rs` 末尾加:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn index() -> (TempDir, WorkspaceIndex) {
        let dir = TempDir::new().unwrap();
        let idx = WorkspaceIndex::new(dir.path().join("workspaces.json"));
        (dir, idx)
    }

    #[test]
    fn add_then_list_round_trips() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string(), "/tmp/a".to_string()]);
    }

    #[test]
    fn add_dedupes_and_moves_to_front() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.add(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
    }

    #[test]
    fn remove_deletes_entry() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.remove(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string()]);
    }

    #[test]
    fn remove_missing_is_ok() {
        let (_d, idx) = index();
        assert!(idx.remove(Path::new("/tmp/nope")).is_ok());
    }

    #[test]
    fn list_missing_file_is_empty() {
        let (_d, idx) = index();
        assert!(idx.list().is_empty());
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, b"{not json").unwrap();
        let idx = WorkspaceIndex::new(path);
        assert!(idx.list().is_empty());
    }
}
```

**Step 2: 运行,确认失败**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server --lib workspace_index`
Expected: 编译失败(`WorkspaceIndex` 未定义)。

**Step 3: 实现**

`workspace_index.rs` 完整内容:

```rust
//! 全局「最近目录」索引:`~/.yi-agent/workspaces.json`。
//!
//! 与 thread 数据分离——移除目录只动本文件,不碰 `<dir>/.yi-agent/`。
//! 读-改-写用进程内 `Mutex` 串行化,写入用 temp 文件 + rename 原子替换。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    dirs: Vec<String>,
}

pub struct WorkspaceIndex {
    path: PathBuf,
    lock: Mutex<()>,
}

/// 全局索引默认路径:`$HOME/.yi-agent/workspaces.json`。
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".yi-agent").join("workspaces.json")
}

impl WorkspaceIndex {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// 当前目录列表,最近的在前。
    pub fn list(&self) -> Vec<String> {
        self.read().dirs
    }

    /// 加入并置顶(去重)。path 应为已 canonicalize 的绝对路径。
    pub fn add(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        file.dirs.insert(0, entry);
        self.write(&file)
    }

    /// 从索引移除;不存在则幂等成功。
    pub fn remove(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        self.write(&file)
    }

    fn read(&self) -> WorkspacesFile {
        let Ok(raw) = std::fs::read_to_string(&self.path) else {
            return WorkspacesFile::default();
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            eprintln!("[app-server] ignoring corrupt workspaces index: {e}");
            WorkspacesFile::default()
        })
    }

    fn write(&self, file: &WorkspacesFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = self.path.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &self.path)
    }
}
```

在 `lib.rs` 加 `pub mod workspace_index;`(先 Read `lib.rs` 看现有模块声明风格再插入)。

**Step 4: 运行,确认通过**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server --lib workspace_index`
Expected: 6 passed。

**Step 5: Commit**

```bash
cd $WT/yi-agent-rs && cargo fmt --all
cd $WT && git add yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): 新增全局最近目录索引"
```

---

### Task 2: 接线 `WorkspaceIndex` + `workspace/*` RPC

本任务让 app-server 持有索引、暴露 `workspace/list|add|remove`,并新增两个供后续复用的
helper。**不改** `thread/start` 行为(仍用 `cfg.workdir`)。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - `run_with` 签名(约 :73-120)、`run`(约 :48-66)
  - handler 分派(`match method.as_str()`,约 :195-631)
  - 测试 harness `with_config`(约 :1203-1215)

**Step 1: `run_with` 增加 index 参数**

`use` 顶部加 `use crate::workspace_index::WorkspaceIndex;` 与 `use std::path::{Path, PathBuf};`。

`run_with` 签名在 `permission_timeout: Duration` 之后加:

```rust
    workspaces: Arc<WorkspaceIndex>,
```

`run()` 里构造并传入:

```rust
let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
run_with(reader, writer, cfg, PERMISSION_TIMEOUT, workspaces, move |session| { ... })
```

**Step 2: 测试 harness 构造临时索引**

`Harness` 结构体加字段 `_index_dir: tempfile::TempDir`,并在 `with_config` 里:

```rust
let index_dir = tempfile::TempDir::new().unwrap();
let workspaces = Arc::new(WorkspaceIndex::new(index_dir.path().join("workspaces.json")));
let handle = tokio::spawn(run_with(server_r, server_w, cfg, permission_timeout, workspaces, build));
Self { client_w, client_r: BufReader::new(client_r), handle, _index_dir: index_dir }
```

**Step 3: 加 `workspace/*` handlers**

在 `match method.as_str()` 内加:

```rust
"workspace/list" => {
    let list: Vec<serde_json::Value> = workspaces
        .list()
        .into_iter()
        .map(|p| json!({ "path": p, "exists": Path::new(&p).is_dir() }))
        .collect();
    write_response(&writer, ok_response(id, json!({ "workspaces": list }))).await?;
}
"workspace/add" => {
    let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match std::fs::canonicalize(raw) {
        Ok(p) if p.is_dir() => {
            let value = p.to_string_lossy().to_string();
            match workspaces.add(&p) {
                Ok(()) => {
                    write_response(&writer, ok_response(id, json!({ "path": value }))).await?
                }
                Err(e) => {
                    write_response(&writer, err_response(id, RpcError::internal(e.to_string())))
                        .await?
                }
            }
        }
        _ => {
            write_response(
                &writer,
                err_response(id, RpcError::invalid_params("path is not a directory")),
            )
            .await?;
        }
    }
}
"workspace/remove" => {
    let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
    match workspaces.remove(Path::new(raw)) {
        Ok(()) => write_response(&writer, ok_response(id, json!({}))).await?,
        Err(e) => {
            write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?
        }
    }
}
```

**Step 4: 加 helper（供 Task 3 复用）**

放在 `require_thread_id` 附近:

```rust
/// 在全局索引的目录里定位 `thread_id` 所属目录。
fn find_thread_dir(workspaces: &WorkspaceIndex, thread_id: &str) -> Option<PathBuf> {
    workspaces
        .list()
        .into_iter()
        .map(PathBuf::from)
        .find(|d| crate::thread_store::ThreadStore::new(d).exists(thread_id))
}

/// 按 thread 定位其 store;索引找不到时回退 `cfg.workdir`。
fn store_for(
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    let dir = find_thread_dir(workspaces, thread_id).unwrap_or_else(|| cfg.workdir.clone());
    Arc::new(crate::thread_store::ThreadStore::new(&dir))
}
```

> 两个 helper 在 Task 2 暂未被调用;若 clippy 报 `dead_code`,加
> `#[allow(dead_code)]` 并在 Task 3 移除。

**Step 5: 测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn workspace_add_list_remove_round_trip() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    let mut h = Harness::new();
    initialize(&mut h).await;

    let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&add.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#).await;
    let v = h.read_value().await;
    assert_eq!(v["result"]["workspaces"][0]["path"], cwd.to_string_lossy().to_string());
    assert_eq!(v["result"]["workspaces"][0]["exists"], true);

    let rm = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"workspace/remove",
        "params":{"path": cwd.to_string_lossy()}});
    h.send(&rm.to_string()).await;
    let v = h.read_value().await;
    assert!(v.get("result").is_some());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_add_rejects_non_directory() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"workspace/add","params":{"path":"/nope/nope"}}"#).await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}
```

**Step 6: 运行**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全部通过(含新增 2 个)。

**Step 7: Commit**

```bash
cd $WT/yi-agent-rs && cargo fmt --all
cd $WT && git add -A yi-agent-rs/crates/yi-agent-app-server
git commit -m "feat(app-server): 接入最近目录索引与 workspace/* RPC"
```

---

### Task 3: agent 工厂接受 cwd + per-thread store + `thread/start {cwd}`

核心改动:把「全局 cfg.workdir」换成「每 thread 的 cwd」,并让 `thread/start` 接受 `cwd`。

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - `run_with` 泛型约束(约 :83)与 `run()` 闭包(约 :53-64)
  - 删 `server.rs:109` 全局 store
  - `thread/start`(:242-325)、`thread/resume`(:327-467)、`thread/rename`(:468-508)、`thread/delete`(:509-537)
  - 新增 `resolve_thread_cwd` 辅助
  - 测试 harness 约束(:1195/:1203)与 4 个 factory(:1117/1132/1147/1930 附近)、:1722/:1768 直调点

**Step 1: 工厂签名加 cwd**

`run_with` 泛型约束由

```rust
F: Fn(Option<yi_agent_core::Session>) -> anyhow::Result<BuiltAgent> + Send + 'static,
```

改为

```rust
F: Fn(Option<yi_agent_core::Session>, &Path) -> anyhow::Result<BuiltAgent> + Send + 'static,
```

`run()` 闭包(约 :53-64)改为:

```rust
let cfg_for_factory = cfg.clone();
run_with(reader, writer, cfg, PERMISSION_TIMEOUT, workspaces, move |session, cwd| {
    let mut thread_cfg = cfg_for_factory.clone();
    thread_cfg.workdir = cwd.to_path_buf();
    let built = yi_agent_runtime::bootstrap::bootstrap_agent(
        &thread_cfg,
        yi_agent_runtime::bootstrap::PermissionMode::Interactive,
    )?;
    Ok(BuiltAgent {
        agent: apply_session(built.agent, session),
        decision_tx: built.decision_tx,
        catalog: built.catalog,
    })
})
.await
```

**Step 2: 删全局 store**

删除 `server.rs:109` 的 `let store = Arc::new(crate::thread_store::ThreadStore::new(&cfg.workdir));`。
(所有使用点改走 per-thread 或 `store_for`。)

**Step 3: 新增 `resolve_thread_cwd`**

```rust
/// 解析 `thread/start` 的目标目录:params.cwd 优先,缺省用 cfg.workdir。
/// canonicalize + 校验是目录;失败写 `-32602` 并返回 Ok(None)。
async fn resolve_thread_cwd<W: tokio::io::AsyncWrite + Unpin>(
    params: &serde_json::Value,
    cfg: &RuntimeConfig,
    writer: &MessageWriter<W>,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    let raw = match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => PathBuf::from(s),
        _ => cfg.workdir.clone(),
    };
    match std::fs::canonicalize(&raw) {
        Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
        _ => {
            write_response(
                writer,
                err_response(id, RpcError::invalid_params("cwd is not a valid directory")),
            )
            .await?;
            Ok(None)
        }
    }
}
```

**Step 4: `thread/start` 用 cwd**

handler 开头:

```rust
let cwd = match resolve_thread_cwd(&req.params, &cfg, &writer, id.clone()).await? {
    Some(c) => c,
    None => continue,
};
let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

let BuiltAgent { agent, decision_tx, catalog } = match build_agent(None, Path::new(&cwd)) {
```

删掉原 `let cwd = cfg.workdir.display().to_string();`;`store.create(&meta)` → `thread_store.create(&meta)`;
`run_thread_driver(... Arc::clone(&store))` → `Arc::clone(&thread_store)`。创建后记录索引:

```rust
if let Err(e) = workspaces.add(Path::new(&cwd)) {
    eprintln!("[app-server] failed to record workspace {cwd}: {e}");
}
```

**Step 5: `thread/resume` 按索引定位 store 与 cwd**

分支开头改为:

```rust
let thread_store = store_for(&workspaces, &cfg, &thread_id);
let loaded = match thread_store.load(&thread_id) { ... };
```

之后 `cwd` / `model` 解析不变(仍从 `loaded.meta`),但 agent 与 driver 用:
`let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));`
(此时 cwd 已 canonical,与 `store_for` 解析出的一致)
`build_agent(Some(session), Path::new(&cwd))`,driver 传 `Arc::clone(&thread_store)`。

**Step 6: `thread/rename` / `thread/delete` 按索引定位**

两处把 `store` 替换为 `store_for(&workspaces, &cfg, &thread_id)`。删除分支里的
`store.exists(&thread_id)` / `store.delete(&thread_id)` 同样改用该 store。

**Step 7: 更新测试工厂签名**

4 个 factory(`build_test_agent` :1117、`build_slow_agent` :1132、`build_delayed_agent` :1147、
`build_permission_agent` :1930 附近)各加第二参 `_cwd: &std::path::Path`。
harness 的 `with_factory` / `with_config` 泛型约束同步改为:

```rust
F: Fn(Option<yi_agent_core::Session>, &std::path::Path) -> anyhow::Result<BuiltAgent> + Send + 'static,
```

直调点 `build_test_agent(None)`(:1722、:1768)改为
`build_test_agent(None, std::path::Path::new("/tmp"))`。

**Step 8: 运行**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全部通过(与改动前同数;若 Task 2 的 helper 加了 `#[allow(dead_code)]` 此时移除)。

**Step 9: Commit**

```bash
cd $WT/yi-agent-rs && cargo fmt --all
cd $WT && git add -A yi-agent-rs/crates/yi-agent-app-server
git commit -m "feat(app-server): thread/start 支持 cwd,agent/store 按 thread 构建"
```

---

### Task 4: `thread/start` cwd 行为测试 + `thread/listAll`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Step 1: 写 cwd 行为测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn thread_start_with_cwd_writes_meta_cwd() {
    let dir = tempfile::TempDir::new().unwrap();
    let cwd = dir.path().canonicalize().unwrap();

    let mut cfg = test_config();
    cfg.workdir = cwd.clone();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    initialize(&mut h).await;

    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": cwd.to_string_lossy() }});
    h.send(&req.to_string()).await;

    let mut resp = None;
    for _ in 0..4 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(2)) { resp = Some(v); }
    }
    let resp = resp.expect("thread/start response");
    assert_eq!(resp["result"]["cwd"].as_str().unwrap(), cwd.to_string_lossy().to_string());

    let store = crate::thread_store::ThreadStore::new(&cwd);
    let metas = store.list().unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(metas[0].cwd, cwd.to_string_lossy().to_string());
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_start_with_bad_cwd_returns_invalid_params() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
        "params":{ "cwd": "/nonexistent/definitely/not/here" }});
    h.send(&req.to_string()).await;
    let v = h.read_value().await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}
```

**Step 2: 写 `thread/listAll` 测试**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn thread_list_all_groups_by_workspace() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let a = dir_a.path().canonicalize().unwrap();
    let b = dir_b.path().canonicalize().unwrap();

    let mut h = Harness::new();
    initialize(&mut h).await;

    for (id, path) in [(2, &a), (3, &b)] {
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start",
            "params":{"cwd": path.to_string_lossy()}});
        h.send(&req.to_string()).await;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) { break; }
        }
    }

    h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#).await;
    let v = h.read_value().await;
    assert_eq!(v["id"], 9);
    let groups = v["result"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "两个目录 → 两个分组: {v}");
    h.shutdown().await;
}
```

**Step 3: 运行,确认失败**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_list_all`
Expected: FAIL(method not found,-32601)。

**Step 4: 实现 `thread/listAll`**

在分派里加:

```rust
"thread/listAll" => {
    let mut groups: Vec<serde_json::Value> = Vec::new();
    for dir in workspaces.list() {
        let path = Path::new(&dir);
        let exists = path.is_dir();
        let threads: Vec<serde_json::Value> = if exists {
            crate::thread_store::ThreadStore::new(path)
                .list()
                .unwrap_or_default()
                .into_iter()
                .map(|m| json!({
                    "thread_id": m.thread_id,
                    "cwd": m.cwd,
                    "model": m.model,
                    "created_at": m.created_at,
                    "updated_at": m.updated_at,
                    "title": m.title,
                }))
                .collect()
        } else {
            Vec::new()
        };
        groups.push(json!({ "workspace": dir, "exists": exists, "threads": threads }));
    }
    write_response(&writer, ok_response(id, json!({ "groups": groups }))).await?;
}
```

**Step 5: 运行**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全部通过(含新增 3 个)。

**Step 6: Commit**

```bash
cd $WT/yi-agent-rs && cargo fmt --all
cd $WT && git add -A yi-agent-rs/crates/yi-agent-app-server
git commit -m "feat(app-server): 覆盖 thread/start cwd 并新增 thread/listAll"
```

---

## Phase B — 前端

### Task 5: `protocol.ts` 新类型

**Files:**
- Modify: `desktop/src/lib/protocol.ts`
- Test: `desktop/src/lib/protocol.test.ts`(新)

**Step 1: 写失败测试**

```ts
import { describe, expect, it } from "vitest";
import type { Workspace, WorkspaceGroup } from "./protocol";

describe("workspace types", () => {
  it("shapes compile", () => {
    const w: Workspace = { path: "/tmp/a", exists: true };
    const g: WorkspaceGroup = { workspace: "/tmp/a", exists: true, threads: [] };
    expect(w.path).toBe("/tmp/a");
    expect(g.threads).toEqual([]);
  });
});
```

**Step 2: 运行,确认失败**

Run: `cd $WT/desktop && npm test -- protocol`
Expected: FAIL(类型不存在)。

**Step 3: 加类型**

`protocol.ts` 末尾加:

```ts
/** `workspace/list` 的单项。 */
export interface Workspace {
  path: string;
  exists: boolean;
}

/** `thread/listAll` 的一个目录分组。 */
export interface WorkspaceGroup {
  workspace: string;
  exists: boolean;
  threads: ThreadSummary[];
}
```

**Step 4: 运行**

Run: `cd $WT/desktop && npm test -- protocol`
Expected: PASS。

**Step 5: Commit**

```bash
cd $WT && git add desktop/src/lib/protocol.ts desktop/src/lib/protocol.test.ts
git commit -m "feat(desktop): 补充 workspace 协议类型"
```

---

### Task 6: 侧栏分组纯函数 + 组件

**Files:**
- Create: `desktop/src/lib/workspaceGroups.ts`
- Test: `desktop/src/lib/workspaceGroups.test.ts`
- Modify: `desktop/src/components/ThreadSidebar.tsx`

**Step 1: 写失败测试**

```ts
import { describe, expect, it } from "vitest";
import { basename, groupCount } from "./workspaceGroups";
import type { WorkspaceGroup } from "./protocol";

const mk = (ws: string, ids: string[]): WorkspaceGroup => ({
  workspace: ws,
  exists: true,
  threads: ids.map((id, i) => ({
    thread_id: id, cwd: ws, model: "m", created_at: i, updated_at: i, title: null,
  })),
});

describe("groupCount", () => {
  it("counts total threads across groups", () => {
    expect(groupCount([mk("/a", ["1", "2"]), mk("/b", ["3"])])).toBe(3);
  });
  it("is zero for no groups", () => {
    expect(groupCount([])).toBe(0);
  });
});

describe("basename", () => {
  it("extracts the last path segment", () => {
    expect(basename("/Users/x/projectA")).toBe("projectA");
  });
  it("ignores a trailing slash", () => {
    expect(basename("/Users/x/")).toBe("x");
  });
});
```

**Step 2: 运行,确认失败**

Run: `cd $WT/desktop && npm test -- workspaceGroups`
Expected: FAIL(模块不存在)。

**Step 3: 实现**

`desktop/src/lib/workspaceGroups.ts`:

```ts
import type { WorkspaceGroup } from "./protocol";

export function groupCount(groups: WorkspaceGroup[]): number {
  return groups.reduce((n, g) => n + g.threads.length, 0);
}

export function basename(path: string): string {
  const trimmed = path.replace(/\/+$/, "");
  const idx = trimmed.lastIndexOf("/");
  return idx >= 0 ? trimmed.slice(idx + 1) : trimmed;
}
```

**Step 4: 运行**

Run: `cd $WT/desktop && npm test -- workspaceGroups`
Expected: PASS。

**Step 5: 改 `ThreadSidebar.tsx`**

- props 由 `threads: ThreadSummary[]` 改为:
  `groups: WorkspaceGroup[]`、`onNew(cwd?: string): void`、`onRemoveWorkspace(cwd: string): void`。
- 把现有单条 thread 的渲染(含双击重命名、删除按钮、相对时间)抽成内部函数
  `renderThread(t)`,组内复用。
- 顶层「+ New thread」按钮 `onClick={() => onNew()}`(不带 cwd → 上层弹选择器)。
- 遍历 `groups`:每组渲染组头 + 折叠区。
  - `const [collapsed, setCollapsed] = useState<Set<string>>(new Set())`;
    caret 点击 toggle 该 `workspace`。
  - 组头显示 `basename(g.workspace)`(完整路径放 `title`);`g.exists === false` 时
    路径加 `text-neutral-600` 并追加 ` (missing)`。
  - 组头 `onContextMenu`(preventDefault)打开自绘小菜单:两项
    `New thread here`(`onNew(g.workspace)`)、`Remove from list`(`onRemoveWorkspace(g.workspace)`)。
    用 `useState<string | null>` 记录打开的组,点击外部关闭。
- `groups` 为空时显示空态文案「选择一个目录开始」。

> **实现时核实**:保留现有 `busy` / `editingId` 的禁用逻辑;改完跑
> `cd $WT/desktop && npx tsc --noEmit`。

**Step 6: 校验**

Run: `cd $WT/desktop && npx tsc --noEmit && npm test`
Expected: 无类型错误、测试全绿。

**Step 7: Commit**

```bash
cd $WT && git add desktop/src/lib/workspaceGroups.ts desktop/src/lib/workspaceGroups.test.ts desktop/src/components/ThreadSidebar.tsx
git commit -m "feat(desktop): 侧栏按工作目录分组"
```

---

### Task 7: Tauri dialog 插件 + App 接线

**Files:**
- Modify: `desktop/src-tauri/Cargo.toml`
- Modify: `desktop/src-tauri/src/lib.rs`
- Modify: `desktop/src-tauri/capabilities/default.json`
- Modify: `desktop/package.json`
- Modify: `desktop/src/App.tsx`

**Step 1: 加依赖**

`Cargo.toml` `[dependencies]` 加 `tauri-plugin-dialog = "2"`。
`package.json` `dependencies` 加 `"@tauri-apps/plugin-dialog": "^2"`。

**Step 2: 注册插件**

`lib.rs` builder 链加 `.plugin(tauri_plugin_dialog::init())`。

**Step 3: 加权限**

`capabilities/default.json` `permissions` 数组加 `"dialog:allow-open"`。

**Step 4: 安装依赖**

Run: `cd $WT/desktop && npm install`
Expected: `@tauri-apps/plugin-dialog` 进入 `node_modules`。

**Step 5: `App.tsx` 接线**

- import:`import type { WorkspaceGroup } from "./lib/protocol";` 与
  `import { groupCount } from "./lib/workspaceGroups";`。
- state:`const [groups, setGroups] = useState<WorkspaceGroup[]>([])`(替换 `threads`)。
- `refreshThreads`:

```ts
const refreshThreads = async () => {
  const c = clientRef.current;
  if (!c) return;
  try {
    const r = await c.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
    setGroups(r.groups);
  } catch {
    // 刷新失败不打断对话
  }
};
```

- `newThread(cwd?: string)`:`request("thread/start", cwd ? { cwd } : {})`。
- 新增目录选择与添加:

```ts
const pickDirectory = async (): Promise<string | null> => {
  const { open } = await import("@tauri-apps/plugin-dialog");
  const picked = await open({ directory: true, multiple: false });
  return typeof picked === "string" ? picked : null;
};

const addWorkspace = async (path: string) => {
  await clientRef.current?.request("workspace/add", { path });
};

const removeWorkspace = async (path: string) => {
  await clientRef.current?.request("workspace/remove", { path });
  await refreshThreads();
};

const newThreadWithPicker = async () => {
  const dir = await pickDirectory();
  if (!dir) return;
  await addWorkspace(dir);
  await newThread(dir);
};
```

- 启动流程改为:

```ts
await client.request("initialize", {});
const list = await client.request<{ groups: WorkspaceGroup[] }>("thread/listAll", {});
setGroups(list.groups);
const first = list.groups.flatMap((g) => g.threads)[0];
if (first) {
  await resumeThread(first.thread_id);
}
// 否则保持空态,等用户选目录新建
setStatus("connected");
```

- `ThreadSidebar` props:`groups={groups}`、`onNew`、`onRemoveWorkspace={removeWorkspace}`。
  - Sidebar 的顶层「+ New thread」调用 `onNew()`(无 cwd → 弹选择器);
    组头「New thread here」调用 `onNew(g.workspace)`(带 cwd → 直接在该目录建)。
  - App 侧实现单一入口:

```ts
const onNew = async (cwd?: string) => {
  if (cwd) {
    await newThread(cwd);
  } else {
    await newThreadWithPicker();
  }
};
```
- `deleteThread` / `renameThread` 里对 `threads` 的乐观更新改为按 groups 展开更新
  (或删除后仅 `refreshThreads()` 简化)。`threadInfo`/`StatusBar` 不变。

> 简化取舍:乐观更新改造较大,可先只保留 `refreshThreads()`(删除/重命名后刷新),
> 保证行为正确优先。

**Step 6: 校验**

Run: `cd $WT/desktop && npx tsc --noEmit && npm test`
Expected: 通过。

**Step 7: 手动验收(可选)**

Run: `cd $WT/desktop && npm run tauri dev`
Expected: 「New thread」弹系统文件夹选择器;选两个目录各建对话;重启后侧栏仍按目录分组。

**Step 8: Commit**

```bash
cd $WT && git add desktop/src-tauri desktop/package.json desktop/package-lock.json desktop/src/App.tsx
git commit -m "feat(desktop): 工作目录选择与侧栏接线"
```

---

## Phase C — 文档与收尾

### Task 8: 更新项目进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`(工作区切换 → `[x]` + 判据)
- Modify: `docs/project-management/yi-agent-app-server.md`(登记新 RPC)
- Modify: `README.md`(如计数变化)

**Step 1: 更新**

- `desktop.md`(原 :63 工作区切换):标 `[x]`,判据写
  `desktop/src/App.tsx:newThread`、`ThreadSidebar.tsx` 分组 + `server.rs` 的 `workspace/*`。
- `yi-agent-app-server.md`:新增 `workspace/list|add|remove`、`thread/listAll`、
  `thread/start {cwd}` 行,判据指向 `server.rs` 对应 handler。
- `README.md`:同步模块索引「完成 / 总计」。

**Step 2: Commit**

```bash
cd $WT && git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md README.md
git commit -m "docs: 登记桌面工作目录选择完成"
```

---

### Task 9: 全量回归 + 合并

**Step 1: 后端**

Run: `cd $WT/yi-agent-rs && cargo test -p yi-agent-app-server && cargo fmt --all -- --check`
Expected: 全绿、fmt 通过。

**Step 2: 前端**

Run: `cd $WT/desktop && npm test && npx tsc --noEmit`
Expected: 全绿。

**Step 3: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff feat/desktop-workspace-selection -m "Merge branch 'feat/desktop-workspace-selection'"
git worktree remove .worktrees/desktop-workspace-selection
git branch -d feat/desktop-workspace-selection
```

Expected: 合并成功,worktree 清理。
