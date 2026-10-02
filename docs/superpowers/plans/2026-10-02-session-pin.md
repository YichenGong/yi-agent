# 会话置顶（Session Pin）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 macOS 桌面端能把会话置顶：置顶会话集中到侧栏顶部独立分区、跨工作目录可见、可手动拖拽排序，状态由服务端持久化。

**Architecture:** 服务端在 `ThreadMeta`（`.meta.json`）新增 `pin_seq: Option<i64>`（`Some` = 已置顶，数值越大越靠前），新增 `thread/setPinned` 与 `thread/reorderPinned` 两个 RPC；`thread/listAll` 除 `groups` 外新增顶层 `pinned` 数组（服务端排好序），`ThreadSummary` 新增 `pinned: bool`。桌面端 `App` 读出 `pinned` 传给侧栏，`ThreadSidebar` 顶部渲染 Pinned 分区、会话行加图钉按钮、分区内手写 HTML5 拖拽排序。

**Tech Stack:** Rust（`yi-agent-app-server`）、TypeScript/React 19 + Vitest/Testing Library（`desktop/`）。

**Spec:** `docs/superpowers/specs/2026-10-02-session-pin-design.md`

## Global Constraints

- 所有改动在 worktree `.worktrees/session-pin`（分支 `feat/session-pin`）内，**不碰 `main`**。
- Rust 命令在 `<worktree>/yi-agent-rs/` 下跑；`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`，`cargo --offline`。
- 前端命令在 `<worktree>/desktop/` 下跑；`export PATH="/opt/homebrew/bin:$PATH"`；vitest 需 `TMPDIR="$PWD/.tmpverify"`（该目录已存在）。
- 不要同时跑多个 `cargo test`（见 `CLAUDE.md`：锁竞争 / OOM）。按 crate 跑，别 `--workspace`。
- 提交前在 `yi-agent-rs/` 下 `cargo fmt --all`。
- 提交用 conventional commits，**不写 `Co-Authored-By` 行**；`git status` 核对后**精确 `git add` 具体文件，不要 `git add -A`**。
- 不新增任何 npm / Rust 依赖。
- 服务端 meta 写入一律走既有 `update_meta`（持 `meta_lock`）/ `write_atomic`。

---

### Task 1: 服务端存储 —— `pin_seq` 字段 + 排序纯函数 + `set_pin_seq`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`
  - `ThreadMeta` struct（约 26 行）
  - `rebuild_meta`（约 419 行）
  - `ThreadStore` 方法区（`set_permission_mode` 之后，约 267 行）
  - 文件级 `pub(crate) fn assign_pin_seqs`
  - 测试 `mod tests` 的 `meta()` helper（约 448 行）

**Interfaces:**
- Produces:
  - `ThreadMeta.pin_seq: Option<i64>`
  - `ThreadStore::set_pin_seq(&self, id: &str, seq: Option<i64>) -> io::Result<bool>`
  - `assign_pin_seqs(order: &[String], current: &HashMap<String, Option<i64>>) -> Vec<(String, i64)>`
- Consumes: 既有 `update_meta`、`now_millis`、`HashMap`。

- [ ] **Step 1: 写失败测试**

在 `thread_store.rs` 的 `mod tests` 内（`use super::*;` 已引入）追加。注意现有 `meta()` helper 需要先补上 `pin_seq` 字段（见 Step 3），测试才能编译；先加测试、后补字段会让 Step 2 编译失败——因此**本步同时**把 `meta()` 改成带 `pin_seq: None`（`..Default` 不适用于此 struct，手工加一行）。

```rust
#[test]
fn new_meta_is_not_pinned() {
    let (_d, s) = store();
    s.create(&meta("thread-a")).unwrap();
    assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
}

#[test]
fn set_pin_seq_sets_value_without_bumping_updated_at() {
    let (_d, s) = store();
    let mut m = meta("thread-a");
    m.updated_at = 7;
    s.create(&m).unwrap();
    assert!(s.set_pin_seq("thread-a", Some(42)).unwrap());
    let got = s.load("thread-a").unwrap().unwrap().meta;
    assert_eq!(got.pin_seq, Some(42));
    assert_eq!(got.updated_at, 7, "置顶不得刷新 updated_at");
}

#[test]
fn set_pin_seq_none_clears_it() {
    let (_d, s) = store();
    s.create(&meta("thread-a")).unwrap();
    s.set_pin_seq("thread-a", Some(1)).unwrap();
    assert!(s.set_pin_seq("thread-a", None).unwrap());
    assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
}

#[test]
fn set_pin_seq_unknown_id_returns_false() {
    let (_d, s) = store();
    assert!(!s.set_pin_seq("nope", Some(1)).unwrap());
}

#[test]
fn legacy_meta_without_pin_seq_deserializes_unpinned() {
    let (_d, s) = store();
    std::fs::create_dir_all(&s.root).unwrap();
    // 旧格式：没有 pin_seq 字段。
    let legacy = r#"{"thread_id":"thread-a","cwd":"/tmp","model":"m",
        "created_at":1,"updated_at":1,"title":null,"permission_mode":"normal"}"#;
    std::fs::write(s.meta_path("thread-a"), legacy).unwrap();
    assert_eq!(s.load("thread-a").unwrap().unwrap().meta.pin_seq, None);
}

#[test]
fn assign_pin_seqs_reproduces_requested_order_top_to_bottom() {
    let ids = vec!["a".to_string(), "b".to_string(), "c".to_string()];
    let mut cur = HashMap::new();
    cur.insert("a".to_string(), Some(10));
    cur.insert("b".to_string(), Some(20));
    cur.insert("c".to_string(), Some(30));
    // 请求顺序：c, a, b（从顶到底）。
    let order = vec!["c".to_string(), "a".to_string(), "b".to_string()];
    let out = assign_pin_seqs(&order, &cur);
    // 还原顺序：按 seq 降序读出 id，应等于 order。
    let mut pairs: Vec<(&String, i64)> = out.iter().map(|(i, s)| (i, *s)).collect();
    pairs.sort_by(|x, y| y.1.cmp(&x.1));
    let got: Vec<&String> = pairs.iter().map(|(i, _)| *i).collect();
    assert_eq!(got, vec![&"c".to_string(), &"a".to_string(), &"b".to_string()]);
    // 互异。
    let mut seqs: Vec<i64> = out.iter().map(|(_, s)| *s).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 3, "不得产生重复 seq");
    let _ = ids;
}

#[test]
fn assign_pin_seqs_pads_none_values_and_stays_unique() {
    let order = vec!["a".to_string(), "b".to_string()];
    let mut cur = HashMap::new();
    cur.insert("a".to_string(), None); // 异常态：声称置顶但无 seq
    cur.insert("b".to_string(), Some(5));
    let out = assign_pin_seqs(&order, &cur);
    let mut seqs: Vec<i64> = out.iter().map(|(_, s)| *s).collect();
    seqs.sort_unstable();
    seqs.dedup();
    assert_eq!(seqs.len(), 2, "补号后仍须互异");
    // a 在最顶 → 其 seq 更大。
    let a = out.iter().find(|(i, _)| i == "a").unwrap().1;
    let b = out.iter().find(|(i, _)| i == "b").unwrap().1;
    assert!(a > b);
}

#[test]
fn assign_pin_seqs_is_idempotent() {
    let order = vec!["x".to_string(), "y".to_string()];
    let mut cur = HashMap::new();
    cur.insert("x".to_string(), Some(100));
    cur.insert("y".to_string(), Some(50));
    let first = assign_pin_seqs(&order, &cur);
    let cur2: HashMap<String, Option<i64>> =
        first.iter().map(|(i, s)| (i.clone(), Some(*s))).collect();
    let second = assign_pin_seqs(&order, &cur2);
    let mut a: Vec<i64> = first.iter().map(|(_, s)| *s).collect();
    let mut b: Vec<i64> = second.iter().map(|(_, s)| *s).collect();
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b, "以当前顺序再算一次应稳定");
}
```

- [ ] **Step 2: 跑测试确认失败（编译失败）**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::new_meta_is_not_pinned`
Expected: 编译错误（`pin_seq` 字段 / `set_pin_seq` / `assign_pin_seqs` 不存在）。

- [ ] **Step 3: 实现**

在 `ThreadMeta` 里加字段（放在 `permission_mode` 之后）：

```rust
    /// 置顶顺序键：`Some` 表示已置顶（数值越大越靠前），`None` 表示未置顶。
    /// 旧 meta 缺字段时默认 `None`。
    #[serde(default)]
    pub pin_seq: Option<i64>,
```

`rebuild_meta` 的 `ThreadMeta { ... }` 字面量补 `pin_seq: None,`。
`mod tests` 的 `meta()` 字面量补 `pin_seq: None,`。

在 `ThreadStore` 的 `set_permission_mode` 之后加方法：

```rust
    /// 写入置顶顺序键：`Some(seq)` = 置顶，`None` = 取消置顶。
    ///
    /// 有意**不**刷新 `updated_at`：置顶属于设置变更而非 thread 活动，不应影响
    /// 按 `updated_at` 排序的普通列表顺序（与 `set_permission_mode` 同理）。
    /// 返回 false 表示 thread 不存在或 meta 不可读。
    pub fn set_pin_seq(&self, id: &str, seq: Option<i64>) -> io::Result<bool> {
        Ok(self.update_meta(id, |meta| meta.pin_seq = seq)?.is_some())
    }
```

在文件顶部 `use std::path::{Path, PathBuf};` 旁加 `use std::collections::HashMap;`，并在 `now_millis` 之后（模块级，非 `impl` 内）加纯函数：

```rust
/// 计算重排后的 `pin_seq` 赋值。`order` 为**从顶到底**的完整有序 id 列表，
/// `current` 为这些 id 当前的 `pin_seq`。
///
/// 算法：取 `current` 中现有 `pin_seq` 的**互异**值升序得 `seqs`；把 `order`
/// 从顶到底依次赋值为 `seqs` 的从大到小（`order[0]` 拿最大）。复用既有互异
/// 数值做双射，永不产生新的重复值，且与「数值越大越靠前」的排序契约一致。
/// 若互异值不够（有 `None` 或历史重复值），从 `max(seqs)+1` 起补足。
pub(crate) fn assign_pin_seqs(
    order: &[String],
    current: &HashMap<String, Option<i64>>,
) -> Vec<(String, i64)> {
    let mut seqs: Vec<i64> = current.values().filter_map(|v| *v).collect();
    seqs.sort_unstable();
    seqs.dedup();
    if seqs.len() < order.len() {
        let mut next = seqs.last().map(|m| m.saturating_add(1)).unwrap_or_else(now_millis);
        while seqs.len() < order.len() {
            seqs.push(next);
            next = next.saturating_add(1);
        }
    }
    order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.clone(), seqs[seqs.len() - 1 - i]))
        .collect()
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::`
Expected: PASS（含既有 `thread_store::tests::*` 全部）。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): persist a thread pin order key"
```

---

### Task 2: 服务端 RPC —— `thread/setPinned` / `thread/reorderPinned` / `pinned` 字段

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  - 新增模块级 helper `collect_pinned` + `thread_summary_json`
  - `thread/list` 的映射（约 1000 行）
  - `thread/listAll` 的映射与响应（约 1030-1070 行）
  - 新增两个 RPC 分支（放在 `thread/setPermissionMode` 分支之后，约 1510 行）
  - `mod tests` 追加集成测试

**Interfaces:**
- Consumes: `ThreadStore::set_pin_seq`、`assign_pin_seqs`（Task 1）；既有 `store_lookup`、`store_for`、`require_thread_id`、`thread_status`、`RpcError::*`、`Harness`。
- Produces: RPC `thread/setPinned`、`thread/reorderPinned`；`ThreadSummary.pinned`；`thread/listAll` 顶层 `pinned`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 内追加（沿用 `Harness::new()` / `initialize` / `read_thread_start_response` / `read_value`）。`Harness::new()` 用 `test_config()`，其 `workdir` 是固定值，多线程会互相干扰；因此**用 `Harness::with_config` 把 `workdir` 指到 `TempDir`**，测试才能拿到确定的 workspace 目录。

```rust
    fn pin_test_config(dir: &std::path::Path) -> RuntimeConfig {
        let mut c = test_config();
        c.workdir = dir.to_path_buf();
        c
    }

    /// 读一个指定 id 的响应，跳过其间所有通知。
    async fn read_response(h: &mut Harness, want: u64) -> serde_json::Value {
        for _ in 0..12 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(want)) {
                return v;
            }
        }
        panic!("no response with id {want}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_puts_thread_on_top_of_list_all_pinned() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, |s, p, m| build_test_agent(s, p, m), Duration::from_secs(5));
        initialize(&mut h).await;

        let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
        h.send(&start.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        let pin = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/setPinned",
            "params":{"threadId": tid, "pinned": true}});
        h.send(&pin.to_string()).await;
        let resp = read_response(&mut h, 3).await;
        assert!(resp.get("error").is_none(), "setPinned must succeed: {resp}");

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/listAll","params":{}}"#).await;
        let v = read_response(&mut h, 4).await;
        let pinned = v["result"]["pinned"].as_array().expect("顶层 pinned 数组");
        assert_eq!(pinned.len(), 1, "恰一个置顶: {v}");
        assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tid);
        assert_eq!(pinned[0]["pinned"], true);

        // 仍在原分组内,且带 pinned:true。
        let all: Vec<&serde_json::Value> = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .collect();
        let me = all.iter().find(|t| t["thread_id"].as_str() == Some(tid.as_str())).expect("in group");
        assert_eq!(me["pinned"], true);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_false_removes_from_pinned_list() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, |s, p, m| build_test_agent(s, p, m), Duration::from_secs(5));
        initialize(&mut h).await;
        let start = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}});
        h.send(&start.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        for (id, pinned) in [(3u64, true), (4u64, false)] {
            let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/setPinned",
                "params":{"threadId": tid, "pinned": pinned}});
            h.send(&req.to_string()).await;
            let r = read_response(&mut h, id).await;
            assert!(r.get("error").is_none(), "{r}");
        }
        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/listAll","params":{}}"#).await;
        let v = read_response(&mut h, 5).await;
        assert_eq!(v["result"]["pinned"].as_array().unwrap().len(), 0);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn set_pinned_rejects_non_boolean_and_unknown_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, |s, p, m| build_test_agent(s, p, m), Duration::from_secs(5));
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":"yes"}}"#).await;
        let v = read_response(&mut h, 2).await;
        assert_eq!(v["error"]["code"], -32602, "非布尔 → invalid_params: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/setPinned","params":{"threadId":"thread-x","pinned":true}}"#).await;
        let v = read_response(&mut h, 3).await;
        assert_eq!(v["error"]["code"], -32011, "未知 id → unknown_thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reorder_pinned_rewrites_order_and_rejects_bad_input() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = pin_test_config(dir.path());
        let mut h = Harness::with_config(cfg, |s, p, m| build_test_agent(s, p, m), Duration::from_secs(5));
        initialize(&mut h).await;

        let mut tids = Vec::new();
        for id in [2u64, 3u64] {
            let start = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start","params":{}});
            h.send(&start.to_string()).await;
            tids.push(read_thread_start_response(&mut h, id).await);
        }
        // 两个都置顶(后置顶的 tids[1] 在顶)。
        for (i, tid) in tids.iter().enumerate() {
            let req = serde_json::json!({"jsonrpc":"2.0","id":10+i as u64,"method":"thread/setPinned",
                "params":{"threadId": tid, "pinned": true}});
            h.send(&req.to_string()).await;
            let r = read_response(&mut h, 10+i as u64).await;
            assert!(r.get("error").is_none(), "{r}");
        }
        // 反转顺序：tids[0] 放到最顶。
        let rev = serde_json::json!({"jsonrpc":"2.0","id":20,"method":"thread/reorderPinned",
            "params":{"threadIds": tids}});
        h.send(&rev.to_string()).await;
        let r = read_response(&mut h, 20).await;
        assert!(r.get("error").is_none(), "reorder must succeed: {r}");

        h.send(r#"{"jsonrpc":"2.0","id":21,"method":"thread/listAll","params":{}}"#).await;
        let v = read_response(&mut h, 21).await;
        let pinned = v["result"]["pinned"].as_array().unwrap();
        assert_eq!(pinned[0]["thread_id"].as_str().unwrap(), tids[0]);

        // 含未置顶 id → invalid_params。
        let bad = serde_json::json!({"jsonrpc":"2.0","id":22,"method":"thread/reorderPinned",
            "params":{"threadIds": ["thread-nope"]}});
        h.send(&bad.to_string()).await;
        let v = read_response(&mut h, 22).await;
        assert_eq!(v["error"]["code"], -32602, "未置顶 id → invalid_params: {v}");

        // 重复 id → invalid_params。
        let dup = serde_json::json!({"jsonrpc":"2.0","id":23,"method":"thread/reorderPinned",
            "params":{"threadIds": [tids[0].clone(), tids[0].clone()]}});
        h.send(&dup.to_string()).await;
        let v = read_response(&mut h, 23).await;
        assert_eq!(v["error"]["code"], -32602, "重复 id → invalid_params: {v}");
        h.shutdown().await;
    }
```

> 注：`build_test_agent` 是 `mod tests` 里既有的默认 agent 工厂（`Harness::new()` 用的就是它，签名 `(Option<Session>, &Path, ThreadMode) -> anyhow::Result<BuiltAgent>`）。这里通过 `Harness::with_config` 只覆写 `cfg.workdir`，工厂仍用默认的那个。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::set_pinned_puts_thread_on_top_of_list_all_pinned`
Expected: FAIL —— `thread/setPinned` 未实现，返回 `-32601 method not found`；或顶层 `pinned` 不存在（`expect` panic）。

- [ ] **Step 3: 实现**

在 `server.rs` 的 `thread_status` 附近加模块级 helper（需要 `WorkspaceIndex` / `ThreadMeta` / `HashMap` 已在文件作用域）：

```rust
/// 跨工作目录收集已置顶的 thread meta，按置顶分区契约排序：
/// `pin_seq` 降序（越大越靠前），`updated_at` 降序，`thread_id` 升序。
fn collect_pinned(workspaces: &WorkspaceIndex) -> Vec<crate::thread_store::ThreadMeta> {
    let mut out = Vec::new();
    for dir in workspaces.list() {
        let path = Path::new(&dir);
        if !path.is_dir() {
            continue;
        }
        match crate::thread_store::ThreadStore::new(path).list() {
            Ok(metas) => out.extend(metas.into_iter().filter(|m| m.pin_seq.is_some())),
            Err(e) => eprintln!("[app-server] collect_pinned failed to list {dir}: {e}"),
        }
    }
    out.sort_by(|a, b| {
        b.pin_seq
            .cmp(&a.pin_seq)
            .then_with(|| b.updated_at.cmp(&a.updated_at))
            .then_with(|| a.thread_id.cmp(&b.thread_id))
    });
    out
}

/// 把 thread meta 渲染成 wire 上的 ThreadSummary（含 `pinned`）。
fn thread_summary_json(
    m: &crate::thread_store::ThreadMeta,
    threads: &HashMap<String, ThreadSession>,
) -> serde_json::Value {
    json!({
        "thread_id": m.thread_id,
        "cwd": m.cwd,
        "model": m.model,
        "created_at": m.created_at,
        "updated_at": m.updated_at,
        "title": m.title,
        "permission_mode": m.permission_mode,
        "pinned": m.pin_seq.is_some(),
        "status": thread_status(threads, &m.thread_id),
    })
}
```

把 `thread/list` 与 `thread/listAll` 里内联的 `json!({ ... "status": ... })` 映射替换为 `thread_summary_json(&m, &threads)`（两处 `metas.into_iter().map(|m| { json!({...}) })` 改为 `.map(|m| thread_summary_json(&m, &threads))`）。

`thread/listAll` 的响应补顶层 `pinned`：

```rust
                        let pinned: Vec<serde_json::Value> = collect_pinned(&workspaces)
                            .iter()
                            .map(|m| thread_summary_json(m, &threads))
                            .collect();
                        write_response(
                            &writer,
                            ok_response(id, json!({ "groups": groups, "pinned": pinned })),
                        )
                        .await?;
```

在 `thread/setPermissionMode` 分支之后（`thread/delete` 之前）加两个分支：

```rust
                    "thread/setPinned" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 参数校验先于 thread 存在性:非布尔一律 `-32602`。
                        let Some(pinned) = req.params.get("pinned").and_then(|v| v.as_bool())
                        else {
                            write_response(
                                &writer,
                                err_response(
                                    id,
                                    RpcError::invalid_params("pinned must be a boolean"),
                                ),
                            )
                            .await?;
                            continue;
                        };
                        // 降序契约:now_millis 是当前最大值 → 新置顶项天然在最顶。
                        let seq = if pinned {
                            Some(crate::thread_store::now_millis())
                        } else {
                            None
                        };
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .set_pin_seq(&thread_id, seq)
                        {
                            Ok(true) => {
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/reorderPinned" => {
                        let arr = match req.params.get("threadIds").and_then(|v| v.as_array()) {
                            Some(a) => a,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "threadIds must be an array of strings",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 全部必须是字符串,否则拒绝(不允许静默丢弃非字符串项)。
                        let ids: Vec<String> = match arr
                            .iter()
                            .map(|v| v.as_str().map(|s| s.to_string()))
                            .collect::<Option<Vec<_>>>()
                        {
                            Some(v) => v,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "threadIds must be an array of strings",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 校验:与「当前全部置顶集合」完全一致,且无重复。
                        let pinned_now = collect_pinned(&workspaces);
                        let current: std::collections::HashSet<&str> =
                            pinned_now.iter().map(|m| m.thread_id.as_str()).collect();
                        let unique: std::collections::HashSet<&str> =
                            ids.iter().map(|s| s.as_str()).collect();
                        let valid = unique.len() == ids.len()
                            && ids.len() == current.len()
                            && ids.iter().all(|i| current.contains(i.as_str()));
                        if !valid {
                            write_response(
                                &writer,
                                err_response(
                                    id,
                                    RpcError::invalid_params(
                                        "threadIds must list exactly the pinned threads, no duplicates",
                                    ),
                                ),
                            )
                            .await?;
                            continue;
                        }
                        let current_seq: HashMap<String, Option<i64>> = pinned_now
                            .iter()
                            .map(|m| (m.thread_id.clone(), m.pin_seq))
                            .collect();
                        let assignments =
                            crate::thread_store::assign_pin_seqs(&ids, &current_seq);
                        let mut err: Option<RpcError> = None;
                        for (tid, seq) in assignments {
                            // 活跃线程复用共享 meta_lock;冷线程回退 store_for。
                            let store = store_lookup(&threads, &workspaces, &cfg, &tid);
                            match store.set_pin_seq(&tid, Some(seq)) {
                                Ok(true) => {}
                                Ok(false) => {
                                    err = Some(RpcError::unknown_thread(&tid));
                                    break;
                                }
                                Err(e) => {
                                    err = Some(RpcError::internal(e.to_string()));
                                    break;
                                }
                            }
                        }
                        match err {
                            None => {
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Some(e) => {
                                write_response(&writer, err_response(id, e)).await?;
                            }
                        }
                    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib server::tests::`
Expected: PASS（含既有 server 测试）。若 `cargo` 卡住，按 `CLAUDE.md` 用 `sample` 定位或先 `ps aux | grep cargo` 清残留。

- [ ] **Step 5: fmt + 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add setPinned and reorderPinned RPCs"
```

---

### Task 3: 桌面端数据层 —— 类型 + App 读取/回调

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（`ThreadSummary`，约 164 行）
- Modify: `desktop/src/App.tsx`（state、`refreshThreads`、首屏、`modeForThread`、两个回调、侧栏 props）

**Interfaces:**
- Consumes: 服务端 `thread/listAll`（含 `pinned`）、`thread/setPinned`、`thread/reorderPinned`。
- Produces: 传给 `ThreadSidebar` 的 `pinned: ThreadSummary[]`、`onTogglePin`、`onReorderPinned`。

- [ ] **Step 1: 加类型字段**

`desktop/src/lib/protocol.ts` 的 `ThreadSummary` 内（`status?` 之后）加：

```ts
  /** 是否置于侧栏顶部的 Pinned 分区。旧服务端缺省视为 false。 */
  pinned?: boolean;
```

- [ ] **Step 2: 写失败测试（App 层）**

在 `desktop/src/App.test.tsx` 追加。先扩展 fake client 的 `thread/listAll`，使其可返回 `pinned`，并记录 `thread/setPinned` 参数。

在 `state` 里加字段：

```ts
    // 顶层 pinned 列表（thread_id 顺序即渲染顺序）。
    pinnedIds: [] as string[],
    // 让 thread/reorderPinned 可被强制失败。
```

`thread/listAll` 分支的返回值改为（保留现有 groups）：

```ts
        return {
          groups: [
            {
              workspace: "/w",
              exists: true,
              threads: seeds.map((t) => ({
                thread_id: t.thread_id,
                cwd: "/w",
                model: "m",
                created_at: 0,
                updated_at: 0,
                title: t.title,
                permission_mode: t.permission_mode,
                status: state.listStatus,
                pinned: state.pinnedIds.includes(t.thread_id),
              })),
            },
          ],
          pinned: state.pinnedIds.map((id) => ({
            thread_id: id,
            cwd: "/w",
            model: "m",
            created_at: 0,
            updated_at: 0,
            title: id,
            pinned: true,
          })),
        };
```

`beforeEach` 里重置 `state.pinnedIds = [];`。

新增测试：

```ts
it("renders a pinned thread in the Pinned section from thread/listAll", async () => {
  state.threads = [
    { thread_id: "t1", title: "one", permission_mode: "normal" },
    { thread_id: "t2", title: "two", permission_mode: "normal" },
  ];
  state.pinnedIds = ["t2"];
  const { container } = render(<App />);
  await waitFor(() => expect(screen.getAllByText("two").length).toBeGreaterThan(0));
  // Pinned 分区标题存在。
  expect(container.textContent).toContain("Pinned");
});

it("sends thread/setPinned and re-lists when the pin button is clicked", async () => {
  state.threads = [{ thread_id: "t1", title: "one", permission_mode: "normal" }];
  const { container } = render(<App />);
  await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
  const before = clients[0].requests.filter((r) => r.method === "thread/listAll").length;
  fireEvent.click(container.querySelector('[aria-label="Pin thread"]')!);
  await waitFor(() => {
    expect(clients[0].requests.some((r) => r.method === "thread/setPinned")).toBe(true);
  });
  await waitFor(() => {
    const after = clients[0].requests.filter((r) => r.method === "thread/listAll").length;
    expect(after).toBeGreaterThan(before);
  });
});
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/App.test.tsx`
Expected: FAIL —— 无 `Pinned` 文本 / 无 `[aria-label="Pin thread"]`。

- [ ] **Step 4: 实现 App.tsx**

加 state（`groups` 旁）：

```ts
  const [pinned, setPinned] = useState<ThreadSummary[]>([]);
```

`refreshThreads` 改为读取并使用 `pinned`：

```ts
  const refreshThreads = async (): Promise<WorkspaceGroup[] | null> => {
    const c = clientRef.current;
    if (!c) return null;
    try {
      const r = await c.request<{ groups: WorkspaceGroup[]; pinned?: ThreadSummary[] }>(
        "thread/listAll",
        {},
      );
      setGroups(r.groups);
      setPinned(r.pinned ?? []);
      store.seed(r.groups.flatMap((g) => g.threads));
      return r.groups;
    } catch {
      return null;
    }
  };
```

首屏初始化（约 420 行）同样改：

```ts
      const list = await client.request<{ groups: WorkspaceGroup[]; pinned?: ThreadSummary[] }>(
        "thread/listAll",
        {},
      );
      setGroups(list.groups);
      setPinned(list.pinned ?? []);
      store.seed(list.groups.flatMap((g) => g.threads));
      const first = (list.pinned ?? [])[0] ?? list.groups.flatMap((g) => g.threads)[0];
```

`modeForThread` 改为同时扫 pinned：

```ts
function modeForThread(groups: WorkspaceGroup[], pinned: ThreadSummary[], id: string): ThreadMode | null {
  const t =
    pinned.find((th) => th.thread_id === id) ??
    groups.flatMap((g) => g.threads).find((th) => th.thread_id === id);
  return (t?.permission_mode as ThreadMode | undefined) ?? null;
}
```

并更新其两处调用点（约 232、258 行）传入 `pinned`。

新增两个回调（放在 `renameThread` 等回调附近，需 `ThreadSummary` 已 import）：

```ts
  /** 置顶 / 取消置顶；成功后重拉列表，以服务端顺序为权威。 */
  const onTogglePin = async (id: string, next: boolean) => {
    const c = clientRef.current;
    if (!c) return;
    try {
      await c.request("thread/setPinned", { threadId: id, pinned: next });
      await refreshThreads();
      force((v) => v + 1);
    } catch (e) {
      setCurrentError(formatError(e));
      await refreshThreads();
      force((v) => v + 1);
    }
  };

  /** 置顶分区内拖拽排序落盘；乐观更新 + 失败回滚重拉。 */
  const onReorderPinned = async (orderedIds: string[]) => {
    const c = clientRef.current;
    if (!c) return;
    const prev = pinned;
    const byId = new Map(prev.map((t) => [t.thread_id, t]));
    setPinned(orderedIds.map((id) => byId.get(id)).filter((t): t is ThreadSummary => !!t));
    force((v) => v + 1);
    try {
      await c.request("thread/reorderPinned", { threadIds: orderedIds });
    } catch (e) {
      setCurrentError(formatError(e));
      setPinned(prev);
      await refreshThreads();
      force((v) => v + 1);
    }
  };
```

传给侧栏：

```tsx
        <ThreadSidebar
          groups={groups}
          workspaces={workspaces}
          currentId={currentId}
          pinned={pinned}
          statuses={statuses}
          unread={unread}
          onSelect={selectThread}
          onRename={renameThread}
          onDelete={deleteThread}
          onTogglePin={onTogglePin}
          onReorderPinned={onReorderPinned}
          onNew={onNew}
          onRemoveWorkspace={removeWorkspace}
          onBrowse={onBrowse}
        />
```

> `ThreadSummary` 目前**未**被 `App.tsx` import（现有 import 列表见文件头 `from "./lib/protocol"` 的 type 块）。实现时需把 `ThreadSummary` 加入该 import 列表。

- [ ] **Step 5: 跑测试确认通过（Task 4 未做前会因缺 props 类型报错——见下）**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/App.test.tsx`
Expected: 本步会因 `ThreadSidebar` 尚未声明新 props 而**类型/运行报错**。这是预期：Task 4 补齐侧栏。若需独立验证 App 层，可临时把新 props 视为可选；**但最终以 Task 4 完成后为准**，本步不单独提交。

---

### Task 4: 桌面端侧栏 —— Pinned 分区 + 图钉按钮 + 拖拽排序

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Modify: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: Task 3 的 `pinned` / `onTogglePin` / `onReorderPinned`；`ThreadSummary.pinned`。
- Produces: 无（叶子 UI）。

- [ ] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 的 `sidebarProps` 默认值里加：

```ts
    pinned: [],
    onTogglePin: vi.fn(),
    onReorderPinned: vi.fn(),
```

追加测试（`thread()` helper 需支持 `pinned`，见下）：

```ts
function pinnedThread(id: string, title: string): ThreadSummary {
  return { thread_id: id, cwd: "/work/projA", model: "m", created_at: 0, updated_at: 0, title, pinned: true };
}

describe("ThreadSidebar pinned section", () => {
  it("renders pinned threads in the Pinned section, in the given order", () => {
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "pinned-nine"), pinnedThread("8", "pinned-eight")] });
    expect(container.textContent).toContain("Pinned");
    const rows = Array.from(container.querySelectorAll("[data-pinned-row]"));
    expect(rows.map((r) => r.textContent)).toEqual([
      expect.stringContaining("pinned-nine"),
      expect.stringContaining("pinned-eight"),
    ]);
  });

  it("hides pinned threads from their workspace group", () => {
    const groups: WorkspaceGroup[] = [
      { workspace: "/work/projA", exists: true, threads: [thread("1", "alpha"), pinnedThread("9", "pin-a")] },
    ];
    const { container } = renderSidebar({ groups, pinned: [pinnedThread("9", "pin-a")] });
    const groupRows = Array.from(container.querySelectorAll("[data-group-row]")).map((r) => r.textContent!);
    expect(groupRows.some((t) => t.includes("pin-a"))).toBe(false);
    expect(groupRows.some((t) => t.includes("alpha"))).toBe(true);
  });

  it("does not render a Pinned section when there are no pinned threads", () => {
    const { container } = renderSidebar({ pinned: [] });
    expect(container.textContent).not.toContain("Pinned");
  });

  it("toggles pin on button click without selecting the thread", () => {
    const onTogglePin = vi.fn();
    const onSelect = vi.fn();
    const { container } = renderSidebar({ groups, onTogglePin, onSelect });
    const btn = container.querySelector<HTMLButtonElement>('[aria-label="Pin thread"]')!;
    fireEvent.click(btn);
    expect(onTogglePin).toHaveBeenCalledWith("1", true);
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("shows a lit pin button on an already-pinned row", () => {
    const onTogglePin = vi.fn();
    const { container } = renderSidebar({ pinned: [pinnedThread("9", "pin-a")], onTogglePin });
    const btn = container.querySelector<HTMLButtonElement>('[aria-label="Unpin thread"]')!;
    expect(btn.getAttribute("aria-pressed")).toBe("true");
    fireEvent.click(btn);
    expect(onTogglePin).toHaveBeenCalledWith("9", false);
  });

  it("reorders pinned threads on drop", () => {
    const onReorderPinned = vi.fn();
    const { container } = renderSidebar({
      pinned: [pinnedThread("9", "nine"), pinnedThread("8", "eight")],
      onReorderPinned,
    });
    const rows = container.querySelectorAll<HTMLElement>("[data-pinned-row]");
    // 把第一行拖到第二行位置。
    fireEvent.dragStart(rows[0]);
    fireEvent.dragOver(rows[1]);
    fireEvent.drop(rows[1]);
    expect(onReorderPinned).toHaveBeenCalledWith(["8", "9"]);
  });

  it("does not make unpinned group rows draggable", () => {
    const { container } = renderSidebar();
    const notDraggable = Array.from(container.querySelectorAll<HTMLElement>("[data-group-row]")).every(
      (r) => r.getAttribute("draggable") !== "true",
    );
    expect(notDraggable).toBe(true);
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: FAIL —— `pinned` / `onTogglePin` / `onReorderPinned` 未声明，`data-pinned-row` 不存在。

- [ ] **Step 3: 实现 ThreadSidebar**

签名新增 props：

```tsx
  pinned,
  onTogglePin,
  onReorderPinned,
}: {
  groups: WorkspaceGroup[];
  workspaces: Workspace[];
  currentId: string | null;
  pinned: ThreadSummary[];
  statuses: Map<string, ThreadStatus>;
  unread: Map<string, TurnStatus>;
  onSelect: (id: string) => void;
  onRename: (id: string, title: string) => void;
  onDelete: (id: string) => void;
  onTogglePin: (id: string, pinned: boolean) => void;
  onReorderPinned: (orderedIds: string[]) => void;
  onNew: (cwd?: string) => void;
  onRemoveWorkspace: (cwd: string) => void;
  onBrowse: () => void;
}) {
```

新增 state：

```tsx
  const [pinnedCollapsed, setPinnedCollapsed] = useState(false);
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropIndex, setDropIndex] = useState<number | null>(null);
```

`renderThread` 增加可选拖拽参数，并渲染 `data-*-row`、图钉按钮：

```tsx
  const renderThread = (
    t: ThreadSummary,
    opts: { rowAttr?: "group" | "pinned"; dragIndex?: number } = {},
  ) => {
    const active = t.thread_id === currentId;
    const st = statuses.get(t.thread_id) ?? "idle";
    const un = unread.get(t.thread_id);
    const isPinned = t.pinned ?? false;
    const dragging = opts.rowAttr === "pinned";
    const dataAttr =
      opts.rowAttr === "pinned" ? { "data-pinned-row": "" } : { "data-group-row": "" };
    return (
      <div
        key={t.thread_id}
        {...dataAttr}
        draggable={dragging && editingId !== t.thread_id}
        onDragStart={dragging ? () => setDragId(t.thread_id) : undefined}
        onDragOver={
          dragging
            ? (e) => {
                if (dragId === null) return;
                e.preventDefault();
                setDropIndex(opts.dragIndex ?? null);
              }
            : undefined
        }
        onDrop={
          dragging
            ? (e) => {
                e.preventDefault();
                commitPinDrop(opts.dragIndex ?? 0);
              }
            : undefined
        }
        onDragEnd={
          dragging
            ? () => {
                setDragId(null);
                setDropIndex(null);
              }
            : undefined
        }
        onClick={/* 保持现有逻辑不变 */}
        className={/* 保持现有逻辑,拖拽中略加 opacity */ `group flex items-center justify-between gap-1 px-3 py-2 text-sm ${
          dragId === t.thread_id ? "opacity-50" : ""
        } ${active ? "bg-neutral-800 text-neutral-100" : "text-neutral-400 hover:bg-neutral-800/50"} cursor-pointer`}
      >
        {/* 插入指示线 */}
        {dragging && dropIndex === opts.dragIndex && dragId !== null && (
          <div className="-mx-3 h-0.5 w-full bg-amber-400" />
        )}
        {/* ... 标记/编辑框/标题/时间 部分保持现状 ... */}
        <button
          type="button"
          aria-label={isPinned ? "Unpin thread" : "Pin thread"}
          aria-pressed={isPinned}
          title={isPinned ? "Unpin" : "Pin"}
          onClick={(e) => {
            e.stopPropagation();
            onTogglePin(t.thread_id, !isPinned);
          }}
          className={`shrink-0 rounded px-1 ${
            isPinned
              ? "text-amber-400 hover:text-amber-300"
              : "pointer-events-none text-neutral-500 opacity-0 group-hover:pointer-events-auto group-hover:opacity-100 hover:text-amber-300 focus-visible:pointer-events-auto focus-visible:opacity-100"
          }`}
        >
          <svg viewBox="0 0 16 16" className="size-3.5" fill="currentColor" aria-hidden="true">
            <path d="M9.5 1 15 6.5l-2.6.6-1 3.1-2.6-2.6L4.3 12 3 13l1-4.4L6.6 6 4 3.4l3.1-1L9.5 1Z" />
          </svg>
        </button>
        {/* ... × 删除按钮保持现状 ... */}
      </div>
    );
  };
```

> 实现细节：图钉按钮与 `×` 都放在标题右侧、`un` 未读点之后。编辑态（`editingId === t.thread_id`）分支保持现状，不渲染图钉。

`commitPinDrop`：

```tsx
  const commitPinDrop = (index: number) => {
    const from = pinned.findIndex((t) => t.thread_id === dragId);
    setDragId(null);
    setDropIndex(null);
    if (from < 0) return;
    const ids = pinned.map((t) => t.thread_id);
    const [moved] = ids.splice(from, 1);
    const to = from < index ? index - 1 : index;
    ids.splice(to, 0, moved);
    if (ids.join(",") !== pinned.map((t) => t.thread_id).join(",")) onReorderPinned(ids);
  };
```

渲染 Pinned 分区（放在滚动容器 `flex-1 overflow-y-auto` 内、`groups.map` 之前）：

```tsx
        {pinned.length > 0 && (
          <div>
            <div className="flex items-center gap-1 px-2 py-1.5 text-xs text-neutral-500">
              <button
                type="button"
                onClick={() => setPinnedCollapsed((v) => !v)}
                aria-label={pinnedCollapsed ? "Expand pinned" : "Collapse pinned"}
                title={pinnedCollapsed ? "Expand" : "Collapse"}
                className="shrink-0 px-0.5 text-neutral-500 hover:text-neutral-300"
              >
                {pinnedCollapsed ? "▸" : "▾"}
              </button>
              <div className="min-w-0 flex-1 truncate text-neutral-400">Pinned</div>
            </div>
            {!pinnedCollapsed &&
              pinned.map((t, i) => renderThread(t, { rowAttr: "pinned", dragIndex: i }))}
          </div>
        )}
```

分组渲染里过滤置顶项：

```tsx
              {!isCollapsed && g.threads.filter((t) => !t.pinned).map((t) => renderThread(t, { rowAttr: "group" }))}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS。

- [ ] **Step 5: 跑 App 层测试 + 类型检查**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npx vitest run src/App.test.tsx && npx tsc --noEmit`
Expected: PASS，无类型错误。

- [ ] **Step 6: 全量前端测试**

Run: `cd desktop && TMPDIR="$PWD/.tmpverify" npm test`
Expected: PASS（无回归）。

- [ ] **Step 7: 提交**

```bash
cd desktop
git add src/lib/protocol.ts src/App.tsx src/components/ThreadSidebar.tsx src/components/ThreadSidebar.test.tsx src/App.test.tsx
git commit -m "feat(desktop): pin sessions in the sidebar"
```

---

### Task 5: 文档同步（project-management + README）

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `README.md`（模块索引表计数，约 198 行）

- [ ] **Step 1: 读现有文档，按规则补条目**

阅读 `docs/project-management/README.md` 末尾「维护规则」与 `desktop.md` / `yi-agent-app-server.md` 现有格式。各加一条 `[x]` 记录，**带可验证判据**：

- `desktop.md`：新增「会话置顶（侧栏 Pinned 分区 + 图钉 + 拖拽排序）」，判据指向
  `desktop/src/components/ThreadSidebar.tsx` 与 `desktop/src/App.tsx` 的
  `onTogglePin` / `onReorderPinned`，验证命令
  `cd desktop && TMPDIR="$PWD/.tmpverify" npm test`。
- `yi-agent-app-server.md`：新增「thread 置顶持久化与 RPC」，判据指向
  `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` 的 `pin_seq` /
  `assign_pin_seqs` 与 `server.rs` 的 `thread/setPinned` / `thread/reorderPinned`，
  验证命令 `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`。

- [ ] **Step 2: 更新 README 计数**

把 `README.md` 模块索引表里 `desktop` 行与 `yi-agent-app-server` 行的「完成」计数按新增条目 +1，同步「完成 / 总计」与模块总数（若总计不变则只改完成数）。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md README.md
git commit -m "docs: record session pinning in the module indexes"
```

---

## Self-Review

**Spec coverage：**
- §4.1 存储字段 `pin_seq` → Task 1 Step 3。
- §4.2 排序纯函数 `assign_pin_seqs` + `set_pin_seq` → Task 1。
- §4.3 `thread/setPinned` / `thread/reorderPinned` / `ThreadSummary.pinned` / 顶层 `pinned` → Task 2。
- §5.1 类型 → Task 3 Step 1。§5.2 数据流（state/refresh/首屏/modeForThread/两回调）→ Task 3。§5.3 侧栏（分区/隐藏/图钉/拖拽）→ Task 4。
- §6 错误处理 → Task 2（unknown/invalid）、Task 3（回滚重拉）。§7 测试 → Task 1/2/3/4 测试步骤。§8 约束 → Global Constraints。

**类型一致性：** `pin_seq`（Rust）/`pinned`（wire 与 TS）命名前后一致；`assign_pin_seqs` 签名在 Task 1 定义、Task 2 使用一致；`set_pin_seq(id, Option<i64>)` 一致；侧栏 props `pinned/onTogglePin/onReorderPinned` 在 Task 3 传递、Task 4 声明一致。

**已知实施风险（执行者注意）：**
1. Task 2 测试通过 `Harness::with_config` + `build_test_agent` 把 `workdir` 指到 `TempDir`；`build_test_agent` 的可见性若为 `mod tests` 私有，测试同在该模块内，可直接引用。
2. `thread/list`/`thread/listAll` 的映射重构后，务必确认 `permission_mode` 等既有字段一字不差（既有测试 `thread_list_all_includes_permission_mode` 会兜底，但优先自查）。
3. Task 3 Step 5 单独跑 App 测试会因侧栏 props 未齐而报错——这是 Task 3/4 的边界，最终以 Task 4 Step 5 的联合验证为准。
