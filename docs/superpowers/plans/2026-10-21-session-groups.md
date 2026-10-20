# 会话分组（Session Groups）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在同一 workspace 内把多条会话收进有名字和描述的可折叠框里，侧栏中连续显示，分组关系存服务端并在桌面与手机端同步。

**Architecture:** 服务端新增 `group_store`（每 workspace 一个 `groups.json`，读-改-写 + 原子 rename），四个 `thread/group/*` RPC 加一条 `thread/groupsChanged` 通知；`thread/listAll` 的每个 workspace 分组多带 `groups` 字段。客户端加一个纯函数模块 `sessionGroups.ts` 负责「组框 + 未归组会话」的行序布局，`ThreadSidebar` 只做渲染，`App` 做 RPC 调用与通知处理。

**Tech Stack:** Rust（axum/JSON-RPC sidecar，cargo test）、React + TypeScript（vitest）、ts-rs 生成协议类型。

## Global Constraints

- 分组是纯视图层：不改变会话 `cwd`、模型、提示词、运行时行为；组名/描述**不得**进入模型上下文。
- 一个组只能属于一个 workspace；组内成员的 `cwd` 必须等于该 workspace 路径，跨 workspace 的成员读取时静默剔除。
- 组名去首尾空白后 1..=24 个字符（按 `char` 计）；描述 0..=120 个字符；成员数不限。
- 组名可重复；`id` 由服务端生成（`grp-` + uuid v4），创建后不变。
- 置顶会话**仍**在顶部 Pinned 区渲染一次，同时在组框内保留（本功能刻意引入的例外）；组内会话在其 workspace 分组里不再重复出现。
- 组内排序沿用现有「最近更新倒序」，不引入手动排序。
- 成员删空即自动解散（在**写路径**删除，读路径只过滤不下发）。
- 错误码：参数类（含 `groupId` 不存在）一律 `-32602`，`message` 形如 `unknown group: {id}`；不新增业务错误码。
- 变更 RPC 一律要求 `Scope::Control`（与 `thread/rename`、`thread/setTag` 同档）。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`；改了 `protocol.rs` 必须重新生成 ts-rs bindings，否则 `just bindings-check` 会红。
- 不要在 `main` 上直接改代码；本计划全部在 `.worktrees/feat-session-groups`（分支 `feat/session-groups`）内完成。

## 文件结构

**服务端**

| 文件 | 职责 |
| --- | --- |
| `yi-agent-rs/crates/yi-agent-app-server/src/group_store.rs`（新建） | `Group` 类型、`load`/`save`/`mutate`/`remove_thread`、长度与成员校验、原子写 |
| `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` | 新增 `Notification::GroupsChanged`；`thread_key()` 与 `Delivery` 分支 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs` | 四个 RPC 臂；`build_list_all_response` 带出 `groups`；`thread/delete` 联动清理 |
| `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` | 注册 `pub mod group_store;` |

**客户端**

| 文件 | 职责 |
| --- | --- |
| `desktop/src/lib/protocol.ts` | `WorkspaceGroup` 加 `groups?: SessionGroup[]`；`SessionGroup` 类型 |
| `desktop/src/lib/sessionGroups.ts`（新建） | 纯函数：`layoutWorkspace` / `groupMembers` / `groupByThread` |
| `desktop/src/lib/sessionGroups.test.ts`（新建） | 上述纯函数的单测 |
| `desktop/src/hooks/useSessionState.ts` | 加 `createGroup` / `updateGroup` / `setGroupMembers` / `deleteGroup` 四个动作 |
| `desktop/src/App.tsx` | 接线这四个动作；处理 `thread/groupsChanged` 通知 |
| `desktop/src/components/ThreadSidebar.tsx` | 组框渲染、多选、新建/重命名/描述就地编辑、组菜单 |
| `desktop/src/components/ThreadSidebar.test.tsx` | 组框与多选的渲染测试 |

---

### Task 1: `group_store` 存储层

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/group_store.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`（加 `pub mod group_store;`，跟随该文件既有 `pub mod` 的字母序位置）

**Interfaces:**
- Produces:
  - `pub struct Group { pub id: String, pub name: String, pub description: String, pub thread_ids: Vec<String> }`（`Serialize`/`Deserialize`，`thread_ids` 走 snake_case，与 `ThreadMeta` 一致）
  - `pub const NAME_MAX_CHARS: usize = 24;`
  - `pub const DESCRIPTION_MAX_CHARS: usize = 120;`
  - `pub fn load(workspace: &Path) -> Vec<Group>`
  - `pub fn save(workspace: &Path, groups: &[Group]) -> io::Result<()>`
  - `pub fn mutate(workspace: &Path, f: impl FnOnce(&mut Vec<Group>)) -> io::Result<()>`
  - `pub fn validate_name(raw: &str) -> Result<String, String>` — 去首尾空白后非空且 `chars().count() <= NAME_MAX_CHARS`，返回规整后的名字；错误信息为给 `-32602` 用的英文串
  - `pub fn validate_description(raw: &str) -> Result<String, String>`
  - `pub fn new_id() -> String` — `format!("grp-{}", uuid::Uuid::new_v4())`
  - `pub fn remove_thread(workspace: &Path, thread_id: &str) -> io::Result<()>` — 从所有组移除该 id，删空的组整组移除
  - `pub fn prune(groups: &mut Vec<Group>, is_member: impl Fn(&str) -> bool)` — 剔除不满足谓词的成员，删除空组（`build_list_all_response` 与 `remove_thread` 共用）

- [x] **Step 1: 写失败测试**

在 `group_store.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, name: &str, ids: &[&str]) -> Group {
        Group {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            thread_ids: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let groups = vec![group("grp-a", "登录模块", &["thread-a", "thread-b"])];
        save(dir.path(), &groups).unwrap();
        let back = load(dir.path());
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "grp-a");
        assert_eq!(back[0].name, "登录模块");
        assert_eq!(back[0].thread_ids, vec!["thread-a", "thread-b"]);
    }

    /// 落盘形状是磁盘契约：`thread_ids` 必须是 snake_case，未知键必须能读进来。
    #[test]
    fn on_disk_shape_uses_snake_case_and_tolerates_unknown_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(
            dir_path.join("groups.json"),
            r#"{"version":1,"groups":[{"id":"grp-a","name":"x","description":"","thread_ids":["thread-a"],"future_key":1}]}"#,
        )
        .unwrap();
        let groups = load(dir.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].thread_ids, vec!["thread-a"]);
    }

    #[test]
    fn a_corrupt_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(dir_path.join("groups.json"), "{not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn mutate_reads_then_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), &[group("grp-a", "a", &["thread-a"])]).unwrap();
        mutate(dir.path(), |groups| {
            groups[0].name = "b".to_string();
        })
        .unwrap();
        assert_eq!(load(dir.path())[0].name, "b");
    }

    #[test]
    fn remove_thread_drops_the_member_and_any_emptied_group() {
        let dir = tempfile::TempDir::new().unwrap();
        save(
            dir.path(),
            &[
                group("grp-a", "a", &["thread-a", "thread-b"]),
                group("grp-b", "b", &["thread-a"]),
            ],
        )
        .unwrap();
        remove_thread(dir.path(), "thread-a").unwrap();
        let back = load(dir.path());
        assert_eq!(back.len(), 1, "只剩非空组");
        assert_eq!(back[0].id, "grp-a");
        assert_eq!(back[0].thread_ids, vec!["thread-b"]);
    }

    #[test]
    fn prune_drops_non_members_and_emptied_groups() {
        let mut groups = vec![
            group("grp-a", "a", &["thread-a", "thread-x"]),
            group("grp-b", "b", &["thread-x"]),
        ];
        prune(&mut groups, |id| id != "thread-x");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].thread_ids, vec!["thread-a"]);
    }

    #[test]
    fn name_validation_trims_and_bounds_by_chars() {
        assert_eq!(validate_name("  登录  ").unwrap(), "登录");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"字".repeat(NAME_MAX_CHARS)).is_ok());
        assert!(validate_name(&"字".repeat(NAME_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn description_validation_allows_empty_and_bounds_by_chars() {
        assert_eq!(validate_description("").unwrap(), "");
        assert!(validate_description(&"字".repeat(DESCRIPTION_MAX_CHARS)).is_ok());
        assert!(validate_description(&"字".repeat(DESCRIPTION_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn new_id_is_prefixed_and_unique() {
        let a = new_id();
        assert!(a.starts_with("grp-"));
        assert_ne!(a, new_id());
    }
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib group_store`
Expected: 编译失败（`Group`、`load` 等未定义）。

- [x] **Step 3: 写最小实现**

```rust
//! 会话分组（session groups）的持久化。
//!
//! 每个 workspace 一个文件 `<workspace>/.yi-agent/groups.json`：
//!
//! ```jsonc
//! { "version": 1, "groups": [ { "id": "grp-…", "name": "登录模块",
//!   "description": "…", "thread_ids": ["thread-a"] } ] }
//! ```
//!
//! 选一个文件而不是写进各 thread 的 `.meta.json`：一个文件就能列出某组的全部
//! 成员，删会话时只需改一处。**分组不是归属**——权威归属仍是每个 thread 的
//! `cwd`，故成员表在读取时会被按 workspace 过滤（见 `rpc::thread` 的
//! `build_list_all_response`）。
//!
//! 写路径与 `settings_store` 同款：读-改-写 + 「临时文件 + rename」原子替换。
//! 缺文件是首次使用的正常情形，不打日志；文件存在却读不出/解析失败则
//! `tracing::warn!` 并回退空表——坏分组绝不阻断侧栏。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 组名上限（按 `char` 计，非字节）。
pub const NAME_MAX_CHARS: usize = 24;

/// 描述上限（按 `char` 计）。
pub const DESCRIPTION_MAX_CHARS: usize = 120;

/// 磁盘文件的版本号。读到未知版本按空表处理。
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thread_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    groups: Vec<Group>,
}

fn groups_path(workspace: &Path) -> PathBuf {
    workspace.join(".yi-agent").join("groups.json")
}

/// 读该 workspace 的全部分组。缺文件 / 不可读 / 损坏 / 未知版本一律空表。
pub fn load(workspace: &Path) -> Vec<Group> {
    let path = groups_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read session groups; treating as none"
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<File>(&text) {
        Ok(file) if file.version == VERSION => file.groups,
        Ok(file) => {
            tracing::warn!(
                version = file.version,
                path = %path.display(),
                "unknown session-groups file version; treating as none"
            );
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not parse session groups; treating as none"
            );
            Vec::new()
        }
    }
}

/// 整体重写该 workspace 的分组表。
pub fn save(workspace: &Path, groups: &[Group]) -> io::Result<()> {
    let dir = workspace.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(&File {
        version: VERSION,
        groups: groups.to_vec(),
    })
    .map_err(io::Error::other)?;
    crate::thread_store::write_atomic(&groups_path(workspace), text.as_bytes())
}

/// 读-改-写。进程内以全局 `Mutex` 串行化并发写。
///
/// 读与写都在锁内：只锁写会让「读-改-写」中间被另一个写者插进来，后写者用旧
/// 快照覆盖前者的结果。
pub fn mutate(workspace: &Path, f: impl FnOnce(&mut Vec<Group>)) -> io::Result<()> {
    let _guard = write_lock();
    let mut groups = load(workspace);
    f(&mut groups);
    save(workspace, &groups)
}

/// 该 workspace 的写锁。
///
/// 分组写操作很少（用户手动建/改组），一把进程级全局锁足够：**不**为每个目录
/// 各建一把，否则要么把锁表永远留在内存里、要么得在 `MutexGuard<'static>` 上
/// 用 `Box::leak`（那是内存泄漏）。锁序只有一把，也就不存在死锁。
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// 组名校验：去首尾空白，非空且不超过 [`NAME_MAX_CHARS`] 个字符。
pub fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("group name must not be empty".to_string());
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(format!(
            "group name must be at most {NAME_MAX_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

/// 描述校验：允许空串，不超过 [`DESCRIPTION_MAX_CHARS`] 个字符。
pub fn validate_description(raw: &str) -> Result<String, String> {
    if raw.chars().count() > DESCRIPTION_MAX_CHARS {
        return Err(format!(
            "group description must be at most {DESCRIPTION_MAX_CHARS} characters"
        ));
    }
    Ok(raw.to_string())
}

/// 新组的 id。
pub fn new_id() -> String {
    format!("grp-{}", uuid::Uuid::new_v4())
}

/// 剔除不满足 `is_member` 的成员，并删除因此变空的组。
///
/// 顺手去重并保持首次出现顺序：`Vec::dedup` 只合并**相邻**重复，而文件是手改
/// 或旧版本写下的，重复项未必相邻。
pub fn prune(groups: &mut Vec<Group>, is_member: impl Fn(&str) -> bool) {
    for group in groups.iter_mut() {
        let mut seen = std::collections::HashSet::new();
        group
            .thread_ids
            .retain(|id| is_member(id) && seen.insert(id.clone()));
    }
    groups.retain(|group| !group.thread_ids.is_empty());
}

/// 从所有组里移除某个 thread，删空的组整组移除。
///
/// 由 `thread/delete` 调用：会话文件已删，成员表不能留悬空 id。
pub fn remove_thread(workspace: &Path, thread_id: &str) -> io::Result<()> {
    mutate(workspace, |groups| prune(groups, |id| id != thread_id))
}
```

- [x] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib group_store`
Expected: PASS（10 个测试）。

- [x] **Step 5: 注册模块**

在 `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs` 里，按现有 `pub mod` 的字母序
插入一行：

```rust
pub mod group_store;
```

（`lib.rs` 里若无 `pub mod` 列表而只有 `mod` + `pub use`，按该文件既有风格加对应的一行；
模块必须对整个 crate 可见，`rpc/thread.rs` 与 `rpc_tests.rs` 都要用。）

- [x] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-app-server/src/group_store.rs \
             yi-agent-rs/crates/yi-agent-app-server/src/lib.rs \
  && git commit -m "feat(app-server): 会话分组的存储层 group_store"
```

---

### Task 2: 广播通知 + `thread/listAll` 带出分组

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（`Notification` 枚举、`thread_key()`、`Delivery`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（`build_list_all_response`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（既有 `#[cfg(test)] mod tests`）

**Interfaces:**
- Consumes: `group_store::{load, prune}`（Task 1）
- Produces:
  - `Notification::GroupsChanged { workspace: String }`，线上 `thread/groupsChanged`，`thread_key()` 返回 `None`，`Delivery::List`
  - `thread/listAll` 每个 workspace 分组多一个 `"groups"` 数组，元素形如 `{"id","name","description","threadIds"}`（**注意 wire 上是 camelCase 的 `threadIds`**，与既有 `WorkspaceGroup.threads` 同一个 hand-built JSON 里 camelCase 约定一致）

- [x] **Step 1: 写失败测试**

在 `protocol.rs` 的 `mod tests` 里加：

```rust
    /// 分组变更通知的 wire 形状：`thread/groupsChanged`，参数 snake_case。
    /// 它不带 thread_id（分组属于目录不属于某条会话），故 `thread_key()` 为 None
    /// 且走列表层传播——与 `ui/tagNames/updated` 的全局帧不同，它只关一个目录。
    #[test]
    fn groups_changed_notification_carries_the_workspace() {
        let v = serde_json::to_value(Notification::GroupsChanged {
            workspace: "/p".into(),
        })
        .unwrap();
        assert_eq!(v["method"], "thread/groupsChanged");
        assert_eq!(v["params"]["workspace"], "/p");
        assert!(Notification::GroupsChanged { workspace: "/p".into() }
            .thread_key()
            .is_none());
    }
```

在 `rpc_tests.rs` 末尾加（该文件已 `use` 了 harness 与 `pin_test_config`）：

```rust
#[tokio::test(flavor = "multi_thread")]
async fn list_all_carries_groups_and_hides_stale_members() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let cwd = dir.path().canonicalize().unwrap();

    // 直接落盘：一个含真实成员 + 一个悬空 id 的组。
    crate::group_store::save(
        &cwd,
        &[crate::group_store::Group {
            id: "grp-a".into(),
            name: "登录模块".into(),
            description: String::new(),
            thread_ids: vec![tid.clone(), "thread-gone".into()],
        }],
    )
    .unwrap();

    h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 3).await;
    let group = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["workspace"].as_str().is_some_and(|w| {
            std::path::Path::new(w).canonicalize().ok().as_deref() == Some(cwd.as_path())
        }))
        .expect("workspace group present");
    let groups = group["groups"].as_array().expect("groups key present");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["name"], "登录模块");
    assert_eq!(groups[0]["threadIds"], serde_json::json!([tid]));
    h.shutdown().await;
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib groups_changed && cargo test -p yi-agent-app-server --lib list_all_carries_groups`
Expected: 编译失败（`Notification::GroupsChanged` 未定义）。

- [x] **Step 3: 加通知变体**

在 `protocol.rs` 的 `Notification` 枚举里，紧跟 `TagChanged` 之后加：

```rust
    /// 某 workspace 的会话分组已变（`thread/group/*`）。
    ///
    /// 与 `thread/tagChanged` **不同族**：分组属于一个目录而不是某条会话，故
    /// `thread_key()` 为 `None`。同时也不带分组正文——一个目录的组表可能很大，
    /// 客户端收到后重拉 `thread/listAll` 更省事；而 `Delivery::List` 恒推，
    /// 恰好匹配「列表要实时」。
    #[serde(rename = "thread/groupsChanged")]
    GroupsChanged { workspace: String },
```

在 `thread_key()` 的 `None` 分支里加 `| Notification::GroupsChanged { .. }`（与 `TagNamesUpdated` 排在一起）。

在 `Delivery` 的判定函数（`impl Notification { pub(crate) fn delivery }`，紧跟 `thread_key` 之后那个）里把 `GroupsChanged` 归入 `Delivery::List`。若该函数是 `match` 式穷举，加一臂：

```rust
            Notification::GroupsChanged { .. } => Delivery::List,
```

- [x] **Step 4: `build_list_all_response` 带出 groups**

在 `rpc/thread.rs` 里加一个辅助函数（放在 `build_list_all_response` 之后）：

```rust
/// 某目录的分组表，供 `thread/listAll` 随 d 下发。
///
/// **只读**：读路径不写盘。过滤掉的悬空成员要等到下一次写操作才真正从文件里
/// 消失（见 spec「列表」一节）——列表请求是高频路径，不该因为它触发磁盘写。
///
/// `live_ids` 是本次扫描出的、属于该目录的全部 thread id：既保证「成员仍存在」，
/// 也保证「成员仍属于这个目录」（跨目录的脏数据在这里被剔除）。
fn groups_json(workspace: &str, live_ids: &[String]) -> serde_json::Value {
    let live: HashSet<&str> = live_ids.iter().map(String::as_str).collect();
    let mut groups = crate::group_store::load(Path::new(workspace));
    crate::group_store::prune(&mut groups, |id| live.contains(id));
    serde_json::Value::Array(
        groups
            .into_iter()
            .map(|g| {
                json!({
                    "id": g.id,
                    "name": g.name,
                    "description": g.description,
                    "threadIds": g.thread_ids,
                })
            })
            .collect(),
    )
}
```

然后改 `build_list_all_response` 里那段 `.map(|ws| { ... json!({...}) })`：

```rust
    let groups: Vec<serde_json::Value> = order
        .iter()
        .map(|ws| {
            let mut merged = plain.remove(ws).unwrap_or_default();
            if let Some(mut folded) = cards.remove(ws) {
                merged.append(&mut folded);
            }
            // 先收集 id 再 `json!`：`merged` 在 `json!` 里被移动，之后就拿不到了。
            let live_ids: Vec<String> = merged
                .iter()
                .filter_map(|t| t["thread_id"].as_str().map(str::to_string))
                .collect();
            json!({
                "workspace": ws,
                "exists": exists_of.get(ws).copied().unwrap_or(false),
                "threads": merged,
                "groups": groups_json(ws, &live_ids),
            })
        })
        .collect();
```

- [x] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib groups_changed && cargo test -p yi-agent-app-server --lib list_all_carries_groups`
Expected: PASS。

- [x] **Step 6: 重新生成 ts-rs bindings 并一起提交**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test --all-features -p yi-agent-app-server export_bindings \
  && cd .. \
  && git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs \
             yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs \
             yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs \
             desktop/src/lib/generated \
  && git commit -m "feat(app-server): 分组随 thread/listAll 下发并广播变更"
```

---

### Task 3: 四个分组 RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（四个 `match` 臂）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（`thread/delete` 删除后联动 `group_store::remove_thread`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `group_store::{load, mutate, new_id, validate_name, validate_description, Group}`（Task 1）、`Notification::GroupsChanged`（Task 2）
- Produces（线上契约，客户端 Task 5/6 依赖）：
  - `thread/group/create` params `{workspace, name, description?, threadIds}` → `{group: {id,name,description,threadIds}}`
  - `thread/group/update` params `{workspace, groupId, name?, description?}` → `{}`
  - `thread/group/setMembers` params `{workspace, groupId, threadIds}` → `{}`
  - `thread/group/delete` params `{workspace, groupId}` → `{}`
  - 全部在成功后就地广播 `thread/groupsChanged {workspace}`

- [x] **Step 1: 写失败测试**

在 `rpc_tests.rs` 末尾加（四条，覆盖建/改/删/成员与错误路径）：

```rust
/// 建组：成功后 `thread/listAll` 能读到，且成员顺序与请求一致；跨目录成员被拒。
#[tokio::test(flavor = "multi_thread")]
async fn group_create_persists_and_validates_workspace() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "登录模块", "description": "三条线",
                  "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["group"]["name"], "登录模块");
    let gid = v["result"]["group"]["id"].as_str().unwrap().to_string();
    assert!(gid.starts_with("grp-"));

    // 陌生成员：伪造一个不在本目录的 id → invalid_params。
    let stranger = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "x", "threadIds":["thread-from-elsewhere"]}});
    h.send(&stranger.to_string()).await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(v["error"]["code"], -32602, "跨目录成员必须被拒: {v}");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not in this workspace"));

    // 未索引目录：分组是目录级概念，未知目录一律拒绝。
    let unindexed = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"thread/group/create",
        "params":{"workspace": "/nonexistent-dir-xyz", "name": "x", "threadIds":[tid]}});
    h.send(&unindexed.to_string()).await;
    let v = read_response(&mut h, 5).await;
    assert_eq!(v["error"]["code"], -32602, "未索引目录必须被拒: {v}");

    let empty_name = serde_json::json!({"jsonrpc":"2.0","id":6,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "   ", "threadIds":[tid]}});
    h.send(&empty_name.to_string()).await;
    let v = read_response(&mut h, 6).await;
    assert_eq!(v["error"]["code"], -32602);
    h.shutdown().await;
}

/// 更新名/描述、换成员、删除；删到空时组自动消失。
#[tokio::test(flavor = "multi_thread")]
async fn group_update_set_members_and_delete() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "a", "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let gid = read_response(&mut h, 3).await["result"]["group"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let update = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"thread/group/update",
        "params":{"workspace": ws, "groupId": gid, "name": "b", "description": "d"}});
    h.send(&update.to_string()).await;
    assert!(read_response(&mut h, 4).await.get("error").is_none());

    // 空成员表 = 解散。
    let clear = serde_json::json!({"jsonrpc":"2.0","id":5,"method":"thread/group/setMembers",
        "params":{"workspace": ws, "groupId": gid, "threadIds":[]}});
    h.send(&clear.to_string()).await;
    assert!(read_response(&mut h, 5).await.get("error").is_none());

    h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
        .await;
    let v = read_response(&mut h, 6).await;
    let all: Vec<_> = v["result"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["groups"].as_array().cloned().unwrap_or_default())
        .collect();
    assert!(all.is_empty(), "空组不下发: {all:?}");

    // 未知 groupId → -32602。
    let unknown = serde_json::json!({"jsonrpc":"2.0","id":7,"method":"thread/group/delete",
        "params":{"workspace": ws, "groupId": "grp-nope"}});
    h.send(&unknown.to_string()).await;
    let v = read_response(&mut h, 7).await;
    assert_eq!(v["error"]["code"], -32602);
    assert!(v["error"]["message"].as_str().unwrap().contains("unknown group"));
    h.shutdown().await;
}

/// 删会话时成员表跟着清；成员删空后组消失。
#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_thread_prunes_it_from_groups() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();
    crate::group_store::save(
        &ws,
        &[crate::group_store::Group {
            id: "grp-a".into(),
            name: "a".into(),
            description: String::new(),
            thread_ids: vec![tid.clone()],
        }],
    )
    .unwrap();

    let del = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/delete",
        "params":{"threadId": tid, "force": true}});
    h.send(&del.to_string()).await;
    let _ = read_response(&mut h, 3).await;

    assert!(crate::group_store::load(&ws).is_empty(), "成员删空即解散");
    h.shutdown().await;
}

/// 变更广播只带 workspace，不带分组正文。
#[tokio::test(flavor = "multi_thread")]
async fn group_mutations_broadcast_groups_changed() {
    let dir = tempfile::TempDir::new().unwrap();
    let cfg = pin_test_config(dir.path());
    let mut h = Harness::with_config(cfg, build_test_agent, Duration::from_secs(5));
    initialize(&mut h).await;

    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let tid = read_thread_start_response(&mut h, 2).await;
    let ws = dir.path().canonicalize().unwrap();

    let create = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"thread/group/create",
        "params":{"workspace": ws, "name": "a", "threadIds":[tid]}});
    h.send(&create.to_string()).await;
    let _ = read_response(&mut h, 3).await;

    let n = h.expect_notification("thread/groupsChanged", 4).await;
    assert_eq!(n["params"]["workspace"], serde_json::Value::String(ws.display().to_string()));
    assert!(n["params"].get("groups").is_none(), "只带 workspace: {n}");
    h.shutdown().await;
}
```

> 注：`Harness` 若无 `expect_notification` 辅助方法，用该文件既有的「读帧直到 method 匹配」写法（参考 `set_tag_broadcasts_tag_changed` 附近的实现），照抄其结构。

- [x] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib group_`
Expected: 编译失败或 `method not found`。

- [x] **Step 3: 加四个 RPC 臂**

在 `rpc/thread.rs` 的 `match` 里、`"thread/setTag"` 臂之后加：

```rust
        // 会话分组的四个写操作。与 `thread/setTag` 同档（Control 起），参数校验
        // 一律先于存在性：非法输入只可能是客户端 bug，越早拒绝越不会留下半状态。
        // 四个臂共用同一段实现与同一条收尾：写成功后广播 `thread/groupsChanged`。
        "thread/group/create" | "thread/group/update" | "thread/group/setMembers"
        | "thread/group/delete" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let dirs = state.workspaces.list();
            match apply_group_mutation(&req.method, &req.params, &dirs) {
                Ok((workspace, result)) => {
                    write_response(state.hub, state.client, ok_response(id, result)).await?;
                    let _ = write_notification(
                        state.hub,
                        &Notification::GroupsChanged { workspace },
                    )
                    .await;
                }
                Err(e) => {
                    write_response(state.hub, state.client, err_response(id, e)).await?;
                }
            }
        }
```

同文件里加这段实现（放在 `thread/delete` 臂之外，靠文件末尾的辅助函数区）：

```rust
/// 四个分组写操作的公共实现。
///
/// 成功返回 `(workspace, 该方法的应答体)`：`create` 需要回传新建的组（客户端
/// 用它做后续编辑），其余三个回空对象。失败一律 `RpcError`——参数类错误走
/// `invalid_params`（-32602），含 `unknown group: {id}`；IO 失败走 `internal`。
fn apply_group_mutation(
    method: &str,
    params: &serde_json::Value,
    dirs: &[String],
) -> Result<(String, serde_json::Value), RpcError> {
    let workspace = params
        .get("workspace")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RpcError::invalid_params("missing workspace"))?;
    // workspace 必须是已索引目录：分组是目录级概念，写进未知目录等于制造一个
    // 永远显示不出来的组。
    if !dirs.iter().any(|d| d == workspace) {
        return Err(RpcError::invalid_params(format!(
            "workspace is not indexed: {workspace}"
        )));
    }
    let ws = Path::new(workspace);

    // `threadIds` 必须是字符串数组且无重复。重复 id 在成员表里没有意义，
    // 且会让 `prune` 之后的计数看起来像两条会话。
    let thread_ids = |params: &serde_json::Value| -> Result<Vec<String>, RpcError> {
        let raw = params
            .get("threadIds")
            .cloned()
            .unwrap_or(serde_json::Value::Array(Vec::new()));
        let ids: Vec<String> = serde_json::from_value(raw)
            .map_err(|e| RpcError::invalid_params(format!("threadIds must be strings: {e}")))?;
        let mut seen = HashSet::new();
        for id in &ids {
            if !seen.insert(id.clone()) {
                return Err(RpcError::invalid_params(format!("duplicate threadId: {id}")));
            }
        }
        Ok(ids)
    };

    // 成员必须真的在这个目录里。用目录的 meta 列表判定，与
    // `build_list_all_response` 的过滤口径一致（同一个 `ThreadStore::list`）。
    let members_ok = |ids: &[String]| -> Result<(), RpcError> {
        let live: HashSet<String> = crate::thread_store::ThreadStore::new(ws)
            .list()
            .map_err(|e| RpcError::internal(e.to_string()))?
            .into_iter()
            .map(|m| m.thread_id)
            .collect();
        if let Some(bad) = ids.iter().find(|id| !live.contains(*id)) {
            return Err(RpcError::invalid_params(format!(
                "thread is not in this workspace: {bad}"
            )));
        }
        Ok(())
    };

    match method {
        "thread/group/create" => {
            let name = crate::group_store::validate_name(
                params.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            )
            .map_err(RpcError::invalid_params)?;
            let description = crate::group_store::validate_description(
                params
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            )
            .map_err(RpcError::invalid_params)?;
            let ids = thread_ids(params)?;
            if ids.is_empty() {
                return Err(RpcError::invalid_params("threadIds must not be empty"));
            }
            members_ok(&ids)?;
            let group = crate::group_store::Group {
                id: crate::group_store::new_id(),
                name,
                description,
                thread_ids: ids,
            };
            let created = group.clone();
            crate::group_store::mutate(ws, |groups| groups.push(group))
                .map_err(|e| RpcError::internal(e.to_string()))?;
            Ok((
                workspace.to_string(),
                json!({
                    "group": {
                        "id": created.id,
                        "name": created.name,
                        "description": created.description,
                        "threadIds": created.thread_ids,
                    }
                }),
            ))
        }
        "thread/group/update" => {
            let group_id = require_group_id(params)?;
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .map(crate::group_store::validate_name)
                .transpose()
                .map_err(RpcError::invalid_params)?;
            let description = params
                .get("description")
                .and_then(|v| v.as_str())
                .map(crate::group_store::validate_description)
                .transpose()
                .map_err(RpcError::invalid_params)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
                    found = true;
                    if let Some(name) = &name {
                        group.name = name.clone();
                    }
                    if let Some(description) = &description {
                        group.description = description.clone();
                    }
                }
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        "thread/group/setMembers" => {
            let group_id = require_group_id(params)?;
            let ids = thread_ids(params)?;
            members_ok(&ids)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                if let Some(group) = groups.iter_mut().find(|g| g.id == group_id) {
                    found = true;
                    group.thread_ids = ids.clone();
                }
                // 空成员表 = 解散（spec「不变量 3」）。这一步必须在写路径做：
                // 读路径只过滤不下发，磁盘条目要等一次写才真正消失。
                groups.retain(|g| !g.thread_ids.is_empty());
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        "thread/group/delete" => {
            let group_id = require_group_id(params)?;
            let mut found = false;
            crate::group_store::mutate(ws, |groups| {
                let before = groups.len();
                groups.retain(|g| g.id != group_id);
                found = groups.len() != before;
            })
            .map_err(|e| RpcError::internal(e.to_string()))?;
            if !found {
                return Err(RpcError::invalid_params(format!(
                    "unknown group: {group_id}"
                )));
            }
            Ok((workspace.to_string(), json!({})))
        }
        other => Err(RpcError::internal(format!(
            "unhandled group mutation: {other}"
        ))),
    }
}

/// `groupId` 是 update / setMembers / delete 的必填项。
fn require_group_id(params: &serde_json::Value) -> Result<String, RpcError> {
    params
        .get("groupId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| RpcError::invalid_params("missing groupId"))
}
```

`Group` 需要 `Clone`（Task 1 已给）。`Path` 与 `HashSet` 在 `rpc/thread.rs` 顶部都已
import（该文件既有的 `scan_workspace_metas` 用了 `Path`，`build_list_all_response`
用了 `HashSet`），无需新加。`scan_workspace_metas` 不需要提升可见性。

- [x] **Step 4: `thread/delete` 联动清理**

在 `thread/delete` 臂里 `thread_store.delete(&thread_id)` 之后（同一 `if let Some(cwd)` 块之前）加：

```rust
            // 会话已删：成员表不能留悬空 id（spec「不变量 2」）。失败只记日志——
            // 会话删除本身已经成功，不该因为一次分组清理写失败而回滚。
            if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                if let Err(e) = crate::group_store::remove_thread(cwd, &thread_id) {
                    eprintln!("[app-server] could not prune {thread_id} from groups: {e}");
                }
            }
```

- [x] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib group_ && cargo test -p yi-agent-app-server --lib deleting_a_thread_prunes`
Expected: PASS。

- [x] **Step 6: 格式化并提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd .. \
  && git add yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs \
             yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs \
  && git commit -m "feat(app-server): 分组增删改与成员管理 RPC"
```

---

### Task 4: 客户端协议类型与纯函数布局

**Files:**
- Modify: `desktop/src/lib/protocol.ts`
- Create: `desktop/src/lib/sessionGroups.ts`
- Create: `desktop/src/lib/sessionGroups.test.ts`

**Interfaces:**
- Consumes: `WorkspaceGroup`（既有）
- Produces:
  - `export interface SessionGroup { id: string; name: string; description: string; threadIds: string[] }`
  - `WorkspaceGroup.groups?: SessionGroup[]`（旧服务端缺省视为 `[]`）
  - `export type SidebarRow = { kind: "thread"; thread: ThreadSummary } | { kind: "group"; group: SessionGroup; members: ThreadSummary[] }`
  - `export function layoutWorkspace(g: WorkspaceGroup): SidebarRow[]`
  - `export function groupMembers(group: SessionGroup, threads: ThreadSummary[]): ThreadSummary[]`
  - `export function groupByThread(rows: SidebarRow[]): Map<string, SessionGroup>`

- [x] **Step 1: 写失败测试**

`desktop/src/lib/sessionGroups.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import type { ThreadSummary, WorkspaceGroup } from "./protocol";
import { groupByThread, groupMembers, layoutWorkspace } from "./sessionGroups";

function thread(id: string, updated_at: number, extra: Partial<ThreadSummary> = {}): ThreadSummary {
  return {
    thread_id: id,
    cwd: "/p",
    model: "m",
    created_at: 0,
    updated_at,
    title: id,
    ...extra,
  };
}

function ws(threads: ThreadSummary[], groups?: WorkspaceGroup["groups"]): WorkspaceGroup {
  return { workspace: "/p", exists: true, threads, groups };
}

describe("groupMembers", () => {
  it("按侧栏既有顺序（threads 原序）取成员，忽略组内 id 顺序", () => {
    const a = thread("a", 30);
    const b = thread("b", 20);
    const members = groupMembers({ id: "g", name: "g", description: "", threadIds: ["b", "a"] }, [
      a,
      b,
    ]);
    expect(members.map((t) => t.thread_id)).toEqual(["a", "b"]);
  });

  it("剔除不存在的成员", () => {
    const members = groupMembers(
      { id: "g", name: "g", description: "", threadIds: ["a", "gone"] },
      [thread("a", 1)],
    );
    expect(members.map((t) => t.thread_id)).toEqual(["a"]);
  });
});

describe("layoutWorkspace", () => {
  it("无分组时退化为逐条会话行", () => {
    const rows = layoutWorkspace(ws([thread("a", 30), thread("b", 20)]));
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : r.group.id))).toEqual([
      "a",
      "b",
    ]);
  });

  it("归组会话只出现一次，组框落在最靠前成员的位置", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 30), thread("b", 20), thread("c", 10)], [
        { id: "g", name: "g", description: "", threadIds: ["c", "b"] },
      ]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : `[${r.group.id}]`))).toEqual([
      "a",
      "[g]",
    ]);
    const box = rows[1];
    expect(box.kind).toBe("group");
    if (box.kind === "group") {
      expect(box.members.map((t) => t.thread_id)).toEqual(["b", "c"]);
    }
  });

  it("多个组按各自最靠前成员的位置交错，组内成员不再单列", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 50), thread("b", 40), thread("c", 30), thread("d", 20)], [
        { id: "g1", name: "g1", description: "", threadIds: ["b", "d"] },
        { id: "g2", name: "g2", description: "", threadIds: ["c"] },
      ]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : `[${r.group.id}]`))).toEqual([
      "a",
      "[g1]",
      "[g2]",
    ]);
  });

  it("置顶会话仍留在组框内（spec 刻意例外），且不因 pinned 而消失", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 10, { pinned: true }), thread("b", 20)], [
        { id: "g", name: "g", description: "", threadIds: ["a"] },
      ]),
    );
    const box = rows.find((r) => r.kind === "group");
    expect(box).toBeDefined();
    if (box && box.kind === "group") {
      expect(box.members.map((t) => t.thread_id)).toEqual(["a"]);
    }
  });

  it("成员全都不在 threads 里时整框不渲染", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 10)], [{ id: "g", name: "g", description: "", threadIds: ["gone"] }]),
    );
    expect(rows.map((r) => (r.kind === "thread" ? r.thread.thread_id : r.group.id))).toEqual(["a"]);
  });
});

describe("groupByThread", () => {
  it("每个归组会话都能反查到它所在的组（多组时取第一个）", () => {
    const rows = layoutWorkspace(
      ws([thread("a", 20), thread("b", 10)], [
        { id: "g1", name: "g1", description: "", threadIds: ["a"] },
        { id: "g2", name: "g2", description: "", threadIds: ["a"] },
      ]),
    );
    const map = groupByThread(rows);
    expect(map.get("a")?.id).toBe("g1");
  });
});
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/sessionGroups.test.ts`
Expected: 模块不存在，导入失败。

- [x] **Step 3: 加协议类型**

`desktop/src/lib/protocol.ts`，在 `WorkspaceGroup` 之前加：

```ts
/**
 * 一个 workspace 内的会话分组（纯视图层：不改归属、不进模型上下文）。
 *
 * `threadIds` 是**成员表**而非归属声明——会话的权威归属仍是它自己的 `cwd`。
 * 旧服务端不下发分组，故 `WorkspaceGroup.groups` 可缺省，读取时按 `[]` 处理。
 */
export interface SessionGroup {
  id: string;
  name: string;
  description: string;
  threadIds: string[];
}
```

并把 `WorkspaceGroup` 改为：

```ts
export interface WorkspaceGroup {
  workspace: string;
  exists: boolean;
  threads: ThreadSummary[];
  /** 本目录的分组；旧服务端缺省视为无分组。 */
  groups?: SessionGroup[];
}
```

- [x] **Step 4: 写实现**

`desktop/src/lib/sessionGroups.ts`：

```ts
/**
 * 会话分组的布局纯函数。不碰 React、不碰 RPC。
 *
 * 侧栏的既有行序由服务端给定（`threads` 已按 `updated_at` 倒序）。分组只改变
 * 「哪些行被收进框里」，**不重排**：框落在它最靠前成员原来的位置上，框内成员
 * 按同一顺序排列。这样用户下次找会话时，位置相对稳定。
 */
import type { SessionGroup, ThreadSummary, WorkspaceGroup } from "./protocol";

export type SidebarRow =
  | { kind: "thread"; thread: ThreadSummary }
  | { kind: "group"; group: SessionGroup; members: ThreadSummary[] };

/** 组的成员，按 `threads` 的既有顺序；剔除已不存在的 id。 */
export function groupMembers(group: SessionGroup, threads: ThreadSummary[]): ThreadSummary[] {
  const wanted = new Set(group.threadIds);
  return threads.filter((t) => wanted.has(t.thread_id));
}

/**
 * 把一个 workspace 分组展开成侧栏行序。
 *
 * - 归组会话只在框内出现一次（不再单列）。
 * - 框的位置 = 它最靠前成员在 `threads` 里的下标。
 * - 空框（成员全部消失）不渲染。
 */
export function layoutWorkspace(g: WorkspaceGroup): SidebarRow[] {
  const groups = g.groups ?? [];
  const indexOf = new Map<string, number>();
  g.threads.forEach((t, i) => indexOf.set(t.thread_id, i));

  const boxes: { group: SessionGroup; members: ThreadSummary[]; at: number }[] = [];
  for (const group of groups) {
    const members = groupMembers(group, g.threads);
    if (members.length === 0) continue;
    const at = Math.min(...members.map((t) => indexOf.get(t.thread_id) ?? Number.MAX_SAFE_INTEGER));
    boxes.push({ group, members, at });
  }
  // 同一起点时保持服务端下发的组顺序（稳定排序）。
  boxes.sort((a, b) => a.at - b.at);

  const boxAt = new Map<number, (typeof boxes)[number]>();
  for (const box of boxes) boxAt.set(box.at, box);
  const grouped = new Set(boxes.flatMap((b) => b.members.map((t) => t.thread_id)));

  const rows: SidebarRow[] = [];
  g.threads.forEach((thread, i) => {
    const box = boxAt.get(i);
    if (box) {
      rows.push({ kind: "group", group: box.group, members: box.members });
      return;
    }
    if (grouped.has(thread.thread_id)) return;
    rows.push({ kind: "thread", thread });
  });
  return rows;
}

/** thread_id → 它所在的组（同一会话属于多组时取行序最靠前的那个）。 */
export function groupByThread(rows: SidebarRow[]): Map<string, SessionGroup> {
  const map = new Map<string, SessionGroup>();
  for (const row of rows) {
    if (row.kind !== "group") continue;
    for (const t of row.members) {
      if (!map.has(t.thread_id)) map.set(t.thread_id, row.group);
    }
  }
  return map;
}
```

- [x] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/sessionGroups.test.ts`
Expected: PASS（9 个测试）。

- [x] **Step 6: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/sessionGroups.ts desktop/src/lib/sessionGroups.test.ts \
  && git commit -m "feat(desktop): 会话分组的协议类型与布局纯函数"
```

---

### Task 5: 客户端动作与通知接线

**Files:**
- Modify: `desktop/src/hooks/useSessionState.ts`
- Modify: `desktop/src/App.tsx`
- Test: 复用 `desktop/src/components/ThreadSidebar.test.tsx`（Task 6）与 `App.test.tsx` 的既有 harness；本任务只加一个纯函数测试 `desktop/src/lib/sessionGroups.test.ts` 之外的接线断言（见 Step 1）

**Interfaces:**
- Consumes: `thread/group/*` RPC（Task 3）、`thread/groupsChanged`（Task 2）、`SessionGroup`（Task 4）
- Produces（`useSessionState` 返回值新增四个动作，`App` 传给 `ThreadSidebar`）：
  - `createGroup(workspace: string, name: string, description: string, threadIds: string[]): Promise<boolean>`
  - `updateGroup(workspace: string, groupId: string, patch: { name?: string; description?: string }): Promise<boolean>`
  - `setGroupMembers(workspace: string, groupId: string, threadIds: string[]): Promise<boolean>`
  - `deleteGroup(workspace: string, groupId: string): Promise<boolean>`
  - 四个都返回「是否成功」；失败时把错误写进 `setCurrentError` 并在返回前 `force()`
  - `App` 另把 `setCurrentError` 作为 `onError` 传给侧栏（跨目录建组的提示走这条）

- [x] **Step 1: 写失败测试**

在 `desktop/src/App.test.tsx` 里加一条（沿用该文件既有的 fake client harness，参考既有 `tagChanged` 用例的结构）：断言收到 `thread/groupsChanged` 后重拉了 `thread/listAll`：

```tsx
it("thread/groupsChanged 触发列表重拉", async () => {
  // 用既有 harness：连接、初始化、收到通知。
  const client = makeClient();
  render(<App client={client} />);
  await client.ready();
  await client.notify({ method: "thread/groupsChanged", params: { workspace: "/p" } });
  expect(client.calls.filter((c) => c.method === "thread/listAll").length).toBeGreaterThan(0);
});
```

> 若 `App.test.tsx` 的 harness 名称不同（`fakeRpc` / `TestClient` 等），按该文件既有约定改写；断言不变。

- [x] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "groupsChanged"`
Expected: FAIL（没有对应分支，`thread/listAll` 未被再次调用）。

- [x] **Step 3: 加四个动作**

在 `useSessionState.ts` 的 `return` 之前加（`rpc()` 为 null 时静默返回 `false`，与既有动作同款）：

```ts
  /**
   * 分组写操作。四个动作的失败处理一致：写进会话错误并强制重渲染，返回
   * `false` 让调用方（侧栏的就地编辑器）知道该保留输入框。
   *
   * 成功路径**不**就地改本地 `groups`：服务端紧接着广播 `thread/groupsChanged`，
   * `App` 收到后重拉列表。就地改一份、再被重拉覆盖，会让输入框在两次数据之间
   * 闪一下。
   */
  const groupCall = async (
    method: string,
    params: Record<string, unknown>,
  ): Promise<boolean> => {
    const c = rpc();
    if (!c) return false;
    try {
      await c.request(method, params);
      return true;
    } catch (e) {
      setCurrentError(formatError(e));
      force((v) => v + 1);
      return false;
    }
  };

  const createGroup = (
    workspace: string,
    name: string,
    description: string,
    threadIds: string[],
  ) => groupCall("thread/group/create", { workspace, name, description, threadIds });

  const updateGroup = (
    workspace: string,
    groupId: string,
    patch: { name?: string; description?: string },
  ) => groupCall("thread/group/update", { workspace, groupId, ...patch });

  const setGroupMembers = (workspace: string, groupId: string, threadIds: string[]) =>
    groupCall("thread/group/setMembers", { workspace, groupId, threadIds });

  const deleteGroup = (workspace: string, groupId: string) =>
    groupCall("thread/group/delete", { workspace, groupId });
```

并加进 `return` 的对象：

```ts
    createGroup,
    updateGroup,
    setGroupMembers,
    deleteGroup,
```

- [x] **Step 4: App 接线**

在 `App.tsx` 的通知处理链里，紧跟 `thread/tagChanged` 分支之后加：

```ts
        if (n.method === "thread/groupsChanged") {
          // 分组变更：通知只带 workspace（组表可能很大），重拉列表即可。
          // 与 tag/title 的「就地落笔」不同——这里没有可比正文小的权威值。
          void sess.refreshThreads();
          return;
        }
```

把四个动作透传给侧栏（`App` 里渲染 `<ThreadSidebar ... />` 处）：

```tsx
          onCreateGroup={sess.createGroup}
          onUpdateGroup={sess.updateGroup}
          onSetGroupMembers={sess.setGroupMembers}
          onDeleteGroup={sess.deleteGroup}
          onError={setCurrentError}
```

（`ThreadSidebar` 的 props 在 Task 6 里定义；本步只加传参，编译会红，属预期，Task 6 补齐。）

- [x] **Step 5: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx`
Expected: PASS。

- [x] **Step 6: 提交**

```bash
git add desktop/src/hooks/useSessionState.ts desktop/src/App.tsx desktop/src/App.test.tsx \
  && git commit -m "feat(desktop): 分组 RPC 动作与 groupsChanged 通知接线"
```

---

### Task 6: 侧栏组框渲染、多选与就地编辑

**Files:**
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: `layoutWorkspace` / `groupMembers` / `groupByThread`（Task 4）、四个动作（Task 5）
- Produces: `ThreadSidebar` 新增 props：
  - `onCreateGroup: (workspace: string, name: string, description: string, threadIds: string[]) => Promise<boolean>`
  - `onUpdateGroup: (workspace: string, groupId: string, patch: { name?: string; description?: string }) => Promise<boolean>`
  - `onSetGroupMembers: (workspace: string, groupId: string, threadIds: string[]) => Promise<boolean>`
  - `onDeleteGroup: (workspace: string, groupId: string) => Promise<boolean>`

- [x] **Step 1: 写失败测试**

在 `ThreadSidebar.test.tsx` 末尾加：

```tsx
describe("会话分组框", () => {
  const threads = [
    { ...baseThread("a"), updated_at: 30, title: "A" },
    { ...baseThread("b"), updated_at: 20, title: "B" },
    { ...baseThread("c"), updated_at: 10, title: "C" },
  ];
  const groups: WorkspaceGroup[] = [
    {
      workspace: "/p",
      exists: true,
      threads,
      groups: [{ id: "grp-1", name: "登录模块", description: "三条线", threadIds: ["b", "c"] }],
    },
  ];

  it("组名常显，描述只在展开时出现，成员不再单列", () => {
    const { container, getByText, queryByText } = render(
      <ThreadSidebar {...defaultProps({ groups })} />,
    );
    expect(getByText("登录模块")).toBeTruthy();
    expect(queryByText("三条线")).toBeNull();
    expect(container.querySelectorAll('[data-thread-row="b"]').length).toBe(1);
    expect(container.querySelectorAll('[data-thread-row="c"]').length).toBe(1);
    const rows = Array.from(container.querySelectorAll("[data-thread-row]")).map(
      (el) => el.getAttribute("data-thread-row"),
    );
    expect(rows).toContain("a");
  });

  it("展开后显示描述，折叠后隐藏，成员仍在框内", () => {
    const { getByLabelText, getByText, queryByText } = render(
      <ThreadSidebar {...defaultProps({ groups })} />,
    );
    fireEvent.click(getByLabelText("展开分组 登录模块"));
    expect(getByText("三条线")).toBeTruthy();
    fireEvent.click(getByLabelText("折叠分组 登录模块"));
    expect(queryByText("三条线")).toBeNull();
  });

  it("多选两个会话后点「新建组」发出正确的 RPC 参数", async () => {
    const onCreateGroup = vi.fn().mockResolvedValue(true);
    const props = defaultProps({ groups: [{ workspace: "/p", exists: true, threads }] });
    const { container, getByText } = render(
      <ThreadSidebar {...props} onCreateGroup={onCreateGroup} />,
    );
    fireEvent.click(container.querySelector('[data-thread-row="a"]')!, { ctrlKey: true });
    fireEvent.click(container.querySelector('[data-thread-row="b"]')!, { ctrlKey: true });
    fireEvent.click(getByText("新建组（2）"));
    const input = container.querySelector<HTMLInputElement>("[data-new-group-name]")!;
    fireEvent.change(input, { target: { value: "登录模块" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() =>
      expect(onCreateGroup).toHaveBeenCalledWith("/p", "登录模块", "", ["a", "b"]),
    );
  });

  it("点击组名就地编辑，Enter 保存", async () => {
    const onUpdateGroup = vi.fn().mockResolvedValue(true);
    const props = defaultProps({ groups });
    const { getByText } = render(<ThreadSidebar {...props} onUpdateGroup={onUpdateGroup} />);
    fireEvent.click(getByText("登录模块"));
    const input = document.querySelector<HTMLInputElement>("[data-group-name-input]")!;
    fireEvent.change(input, { target: { value: "扫码登录" } });
    fireEvent.keyDown(input, { key: "Enter" });
    await waitFor(() =>
      expect(onUpdateGroup).toHaveBeenCalledWith("/p", "grp-1", { name: "扫码登录" }),
    );
  });

  it("组菜单里「删除组」调用 onDeleteGroup", async () => {
    const onDeleteGroup = vi.fn().mockResolvedValue(true);
    const props = defaultProps({ groups });
    const { getByText } = render(<ThreadSidebar {...props} onDeleteGroup={onDeleteGroup} />);
    fireEvent.contextMenu(getByText("登录模块"));
    fireEvent.click(getByText("删除组"));
    await waitFor(() => expect(onDeleteGroup).toHaveBeenCalledWith("/p", "grp-1"));
  });
});
```

> `baseThread` / `defaultProps` 若该文件里没有同名 helper，用该文件既有的 `groups` 构造与 `renderSidebar(...)` 约定改写；断言不变。同时把 `fireEvent` / `vi` / `waitFor` 加进该文件的 import（已存在则复用）。

- [x] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx -t "会话分组框"`
Expected: FAIL（无组框渲染）。

- [x] **Step 3: 加 props 与选择态**

`ThreadSidebar.tsx` 的 props 类型里加：

```tsx
  /** 用选中的会话建组。返回 false 表示失败（保留编辑器）。 */
  onCreateGroup: (workspace: string, name: string, description: string, threadIds: string[]) => Promise<boolean>;
  /** 改组名 / 描述。`patch` 只带要改的字段。 */
  onUpdateGroup: (
    workspace: string,
    groupId: string,
    patch: { name?: string; description?: string },
  ) => Promise<boolean>;
  onSetGroupMembers: (workspace: string, groupId: string, threadIds: string[]) => Promise<boolean>;
  onDeleteGroup: (workspace: string, groupId: string) => Promise<boolean>;
  /** 侧栏自身的动作错误（如跨目录建组），落进会话错误区。 */
  onError: (message: string) => void;
```

组件顶部状态里加：

```tsx
  // 组框的展开集合（键为 group id）：与 workspace 折叠分开，折叠目录不等于折叠组。
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set());
  // 多选：会话行的选择集。Ctrl/Cmd+点击进出，Shift+点击按当前行序区间选择。
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [selectionAnchor, setSelectionAnchor] = useState<string | null>(null);
  // 就地编辑：正在编辑的组字段与草稿。
  const [editingGroup, setEditingGroup] = useState<
    { workspace: string; groupId: string; field: "name" | "description" } | null
  >(null);
  const [groupDraft, setGroupDraft] = useState("");
  // 新建组的输入态：框属于哪个 workspace、字段与草稿。
  const [newGroup, setNewGroup] = useState<
    { workspace: string; field: "name" | "description"; name: string; description: string } | null
  >(null);
  // 组菜单（与既有的 group/child/thread 菜单同一槽位，互斥）。
```

把 `menu` 联合类型加两个成员：

```tsx
    | { kind: "groupBox"; ws: string; groupId: string }
```

- [x] **Step 4: 会话行支持多选**

`renderThread` 的 `onClick` 里、`onSelect` 之前加：

```tsx
          // 多选：Ctrl/Cmd 切换单选，Shift 按当前行序区间选择。没有修饰键时
          // 照旧打开会话——选择态是「临时搭建的动作上下文」，不该改变单击的含义。
          if (e.metaKey || e.ctrlKey) {
            e.preventDefault();
            setSelected((prev) => {
              const next = new Set(prev);
              if (next.has(t.thread_id)) next.delete(t.thread_id);
              else next.add(t.thread_id);
              return next;
            });
            setSelectionAnchor(t.thread_id);
            return;
          }
          if (e.shiftKey && selectionAnchor) {
            e.preventDefault();
            const ids = visibleThreadIds;
            const from = ids.indexOf(selectionAnchor);
            const to = ids.indexOf(t.thread_id);
            if (from >= 0 && to >= 0) {
              const [lo, hi] = from < to ? [from, to] : [to, from];
              setSelected(new Set(ids.slice(lo, hi + 1)));
            }
            return;
          }
          if (selected.size > 0) {
            // 有选择时点空白行 = 退出多选。
            setSelected(new Set());
            setSelectionAnchor(null);
            return;
          }
```

`visibleThreadIds` 在 `renderThread` 之上算出：当前 workspace 分组里实际渲染的行序（`layoutWorkspace` 的 thread 行 + 所有框成员，按出现顺序摊平）。实现为组件内 `useMemo`：

```tsx
  // 多选区间的锚点顺序：与侧栏实际渲染顺序一致（组框成员摊平在其位置）。
  const visibleThreadIds = useMemo(
    () =>
      groups.flatMap((g) =>
        layoutWorkspace(g).flatMap((row) =>
          row.kind === "thread" ? [row.thread.thread_id] : row.members.map((t) => t.thread_id),
        ),
      ),
    [groups],
  );
```

行 className 里加选中态（与 `active` 并列）：

```tsx
        } ${selected.has(t.thread_id) ? "bg-raised ring-1 ring-line-strong" : ""} ${
```

- [x] **Step 5: 渲染组框**

在 `renderGroupNode` 里，把原来兜底的 `g.threads.filter(...).map(...)` 换成 `layoutWorkspace` 驱动：

```tsx
        {!isCollapsed &&
          layoutWorkspace(g)
            .filter((row) =>
              row.kind === "thread"
                ? row.thread.thread_id !== leadId && !row.thread.driver_ref
                : true,
            )
            .map((row) =>
              row.kind === "thread" ? (
                renderThread(row.thread, { rowAttr: "group", indentLevel: isChild ? 2 : 0 })
              ) : (
                renderGroupBox(g.workspace, row.group, row.members, isChild ? 2 : 0)
              ),
            )}
```

> 注意：`layoutWorkspace` 已经把置顶会话留在框内。原来的 `.filter((t) => !t.pinned && ...)`
> 在渲染前把置顶会话剔掉——换成行序驱动后，**只在 box 之外**保留 `!pinned` 过滤：
>
> ```tsx
>             .filter((row) =>
>               row.kind === "thread"
>                 ? !row.thread.pinned && row.thread.thread_id !== leadId && !row.thread.driver_ref
>                 : true,
>             )
> ```
>
> 这正是 spec 里「置顶的组内会话仍留在框里」的实现点；组外的置顶会话照旧只出现在顶部。

加组框渲染函数（放在 `renderGroupNode` 之前）：

```tsx
  /**
   * 一个分组框：标题常显、描述只在展开时显示、成员缩进一档。
   *
   * 框是`纯视图`:成员仍随服务端的 `updated_at` 排序，框本身没有自己的排序状态
   * （spec 明确不做手动排序），这样「记得住」靠的是位置稳定，而不是靠用户维护顺序。
   */
  const renderGroupBox = (
    workspace: string,
    group: SessionGroup,
    members: ThreadSummary[],
    indentLevel: 0 | 2,
  ) => {
    const expanded = expandedGroups.has(group.id);
    const editing =
      editingGroup?.groupId === group.id && editingGroup.workspace === workspace
        ? editingGroup.field
        : null;
    const menuOpen = menu?.kind === "groupBox" && menu.groupId === group.id;
    const commitField = async (field: "name" | "description") => {
      const patch = { [field]: groupDraft } as { name?: string; description?: string };
      const ok = await onUpdateGroup(workspace, group.id, patch);
      if (ok) {
        setEditingGroup(null);
        setGroupDraft("");
      }
    };
    return (
      <div
        key={group.id}
        data-group-box={group.id}
        className="mx-2 my-1 rounded-md border border-line-strong"
      >
        <div className="flex items-center gap-1 px-2 py-1">
          <button
            type="button"
            aria-label={`${expanded ? "折叠" : "展开"}分组 ${group.name}`}
            title={expanded ? "Collapse" : "Expand"}
            onClick={() =>
              setExpandedGroups((prev) => {
                const next = new Set(prev);
                if (next.has(group.id)) next.delete(group.id);
                else next.add(group.id);
                return next;
              })
            }
            className="shrink-0 px-0.5 text-fg-subtle hover:text-fg-muted"
          >
            {expanded ? "▾" : "▸"}
          </button>
          {editing === "name" ? (
            <input
              data-group-name-input=""
              autoFocus
              value={groupDraft}
              maxLength={GROUP_NAME_MAX}
              onChange={(e) => setGroupDraft(e.target.value)}
              onCompositionStart={ime.onCompositionStart}
              onCompositionEnd={ime.onCompositionEnd}
              onBlur={() => {
                ime.resetComposition();
                void commitField("name");
              }}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !isImeEnter(e, ime.composing.current)) void commitField("name");
                if (e.key === "Escape") setEditingGroup(null);
              }}
              className="min-w-0 flex-1 rounded bg-surface px-1 py-0.5 text-sm text-fg outline-none"
            />
          ) : (
            <div
              className="min-w-0 flex-1 truncate text-sm font-medium text-fg-muted"
              title={group.name}
              onClick={() => {
                setEditingGroup({ workspace, groupId: group.id, field: "name" });
                setGroupDraft(group.name);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                setNewMenuOpen(false);
                setMenu({ kind: "groupBox", ws: workspace, groupId: group.id });
              }}
            >
              {group.name}
            </div>
          )}
          <span className="shrink-0 text-xs text-fg-faint">{members.length}</span>
        </div>
        {expanded && (
          <div className="border-t border-line-strong/60 px-2 py-1">
            {editing === "description" ? (
              <textarea
                data-group-description-input=""
                autoFocus
                value={groupDraft}
                maxLength={GROUP_DESCRIPTION_MAX}
                rows={2}
                onChange={(e) => setGroupDraft(e.target.value)}
                onBlur={() => void commitField("description")}
                onKeyDown={(e) => {
                  if (e.key === "Escape") setEditingGroup(null);
                }}
                className="w-full rounded bg-surface px-1 py-0.5 text-xs text-fg-muted outline-none"
              />
            ) : (
              <div
                data-group-description=""
                className="line-clamp-2 cursor-text text-xs text-fg-subtle"
                onClick={() => {
                  setEditingGroup({ workspace, groupId: group.id, field: "description" });
                  setGroupDraft(group.description);
                }}
              >
                {group.description || "（点击添加描述）"}
              </div>
            )}
          </div>
        )}
        {expanded &&
          members.map((t) => renderThread(t, { rowAttr: "group", indentLevel: indentLevel === 2 ? 2 : 1 }))}
        <Popover open={menuOpen} onClose={closeMenus}>
          <div
            role="menu"
            className="absolute left-4 z-20 rounded-md border border-line-strong bg-raised py-1 shadow-xl"
          >
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                closeMenus();
                void onDeleteGroup(workspace, group.id);
              }}
              className="block w-full px-3 py-1.5 text-left text-xs whitespace-nowrap text-fg hover:bg-raised"
            >
              删除组
            </button>
          </div>
        </Popover>
      </div>
    );
  };
```

顶部常量：

```tsx
/** 与后端 `group_store::{NAME_MAX_CHARS, DESCRIPTION_MAX_CHARS}` 一致。 */
const GROUP_NAME_MAX = 24;
const GROUP_DESCRIPTION_MAX = 120;
```

- [x] **Step 6: 多选操作条与新建组输入**

在侧栏底部（列表之后、宽度拖柄之前）加：

```tsx
      {selected.size > 0 && (
        <div data-selection-bar="" className="sticky bottom-0 border-t border-line-strong bg-surface px-2 py-2">
          {newGroup ? (
            <div className="flex flex-col gap-1">
              <input
                data-new-group-name=""
                autoFocus
                placeholder="组名"
                maxLength={GROUP_NAME_MAX}
                value={newGroup.name}
                onChange={(e) => setNewGroup({ ...newGroup, name: e.target.value })}
                className="rounded bg-raised px-1 py-0.5 text-sm text-fg outline-none"
              />
              <input
                data-new-group-description=""
                placeholder="描述（可选）"
                maxLength={GROUP_DESCRIPTION_MAX}
                value={newGroup.description}
                onChange={(e) => setNewGroup({ ...newGroup, description: e.target.value })}
                className="rounded bg-raised px-1 py-0.5 text-xs text-fg-muted outline-none"
              />
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={async () => {
                    const workspace = newGroup.workspace;
                    const ok = await onCreateGroup(
                      workspace,
                      newGroup.name,
                      newGroup.description,
                      [...selected],
                    );
                    if (ok) {
                      setNewGroup(null);
                      setSelected(new Set());
                      setSelectionAnchor(null);
                    }
                  }}
                  className="rounded bg-raised px-2 py-0.5 text-xs text-fg"
                >
                  创建
                </button>
                <button
                  type="button"
                  onClick={() => setNewGroup(null)}
                  className="rounded px-2 py-0.5 text-xs text-fg-subtle"
                >
                  取消
                </button>
              </div>
            </div>
          ) : (
            <button
              type="button"
              onClick={() => {
                // 选择集必须同属一个 workspace——组不跨目录（spec 硬约束）。
                const owner = workspaceOfThread([...selected][0]);
                if (!owner) return;
                if ([...selected].some((id) => workspaceOfThread(id) !== owner)) {
                  onError("所选会话不在同一个 workspace，无法归于一组");
                  return;
                }
                setNewGroup({ workspace: owner, field: "name", name: "", description: "" });
              }}
              className="w-full rounded bg-raised px-2 py-1 text-xs text-fg"
            >
              新建组（{selected.size}）
            </button>
          )}
        </div>
      )}
```

`workspaceOfThread` 只读 `groups`。跨目录选择时用新增的 `onError` prop 报一句：
`ThreadSidebar` 目前没有错误通道，故在 props 里加

```tsx
  /** 侧栏自身的动作错误（跨目录建组等），落进会话错误区。 */
  onError: (message: string) => void;
```

`App` 渲染处传 `onError={setCurrentError}`。

```tsx
  /** thread_id → 它所在 workspace 的路径（用于建组时的同目录校验）。 */
  const workspaceOfThread = (id: string): string | null => {
    for (const g of groups) {
      if (g.threads.some((t) => t.thread_id === id)) return g.workspace;
    }
    return null;
  };
```

- [x] **Step 7: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx`
Expected: PASS（含原有测试与新增 5 条）。

- [x] **Step 8: 全量前端测试与类型检查**

Run: `cd desktop && npx vitest run && npx tsc --noEmit`
Expected: 全绿。

- [x] **Step 9: 提交**

```bash
cd desktop && npx prettier --write src/components/ThreadSidebar.tsx src/lib/sessionGroups.ts 2>/dev/null; cd .. \
  && git add desktop/src/components/ThreadSidebar.tsx \
             desktop/src/components/ThreadSidebar.test.tsx \
             desktop/src/App.tsx \
  && git commit -m "feat(desktop): 侧栏分组框、多选建组与就地编辑"
```

---

### Task 7: 搜索过滤、文档与全量校验

**Files:**
- Modify: `desktop/src/lib/threadSearch.ts`（组框内的会话参与搜索）
- Modify: `desktop/src/lib/threadSearch.test.ts`（若存在；否则在 `sessionGroups.test.ts` 内补）
- Modify: `docs/project-management/README.md` 与对应模块文件（项目规范要求）
- Modify: `docs/superpowers/specs/2026-10-21-session-groups-design.md`（若有实现期偏差）

**Interfaces:**
- Consumes: 全部前面的产物
- Produces: 搜索时「框内只剩命中成员、全不命中则整框隐藏」

- [x] **Step 1: 写失败测试**

在 `threadSearch` 的测试文件里加：

```ts
it("搜索时组框按命中成员收窄，未命中的组整框消失", () => {
  const result = filterThreadTree(
    [
      {
        workspace: "/p",
        exists: true,
        threads: [thread("a", "登录页面"), thread("b", "扫码")],
        groups: [{ id: "g", name: "登录模块", description: "", threadIds: ["a", "b"] }],
      },
    ],
    [],
    "扫码",
  );
  const box = (result.groups[0].groups ?? [])[0];
  expect(box.threadIds).toEqual(["b"]);
});

it("组名命中时整组保留（名字本身也是可搜内容）", () => {
  const result = filterThreadTree(
    [
      {
        workspace: "/p",
        exists: true,
        threads: [thread("a", "登录页面"), thread("b", "扫码")],
        groups: [{ id: "g", name: "登录模块", description: "", threadIds: ["a", "b"] }],
      },
    ],
    [],
    "登录模块",
  );
  const box = (result.groups[0].groups ?? [])[0];
  expect(box.threadIds).toEqual(["a", "b"]);
});
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/threadSearch.test.ts`
Expected: FAIL（组表未收窄）。

- [x] **Step 3: 实现搜索收窄**

在 `filterThreadTree` 的 `threads` 过滤之后加：

```ts
    // 分组随搜索收窄：只保留仍命中的成员；组名或描述命中时整组保留（名字本身
    // 也是用户记得住的东西，不能搜不到）。收窄后为空的组整组丢弃——否则会出现
    // 一个空框占着位置。
    const matched = new Set(threads.map((t) => t.thread_id));
    const narrowedGroups = (g.groups ?? [])
      .map((group) => {
        const selfHit = tokens.every((tok) => group.name.toLowerCase().includes(tok))
          || tokens.every((tok) => group.description.toLowerCase().includes(tok));
        const ids = selfHit ? group.threadIds : group.threadIds.filter((id) => matched.has(id));
        return { ...group, threadIds: ids };
      })
      .filter((group) => group.threadIds.length > 0);
```

并把出组条件与产物改成：

```ts
    if (threads.some((t) => !t.pinned) || narrowedGroups.length > 0) {
      groups2.push({ ...g, threads, groups: narrowedGroups });
    }
```

- [x] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/threadSearch.test.ts`
Expected: PASS。

- [x] **Step 5: 更新项目进度文档**

在 `docs/project-management/` 下找到侧栏/会话模块文件（`rg -l "ThreadSidebar" docs/project-management/`），加一条：

```markdown
- [x] 会话分组：同 workspace 内把多条会话收进带名字/描述的框，纯视图不改归属
      （`yi-agent-rs/crates/yi-agent-app-server/src/group_store.rs`、
      `desktop/src/lib/sessionGroups.ts`、`desktop/src/components/ThreadSidebar.tsx`）
```

并同步 `docs/project-management/README.md` 模块索引表的「完成 / 总计」计数。

- [x] **Step 6: 全量校验**

```bash
cd yi-agent-rs && cargo fmt --all && cargo test -p yi-agent-app-server --lib \
  && cargo test --all-features -p yi-agent-app-server export_bindings \
  && cd .. && git diff --exit-code -- desktop/src/lib/generated \
  && cd desktop && npx tsc --noEmit && npx vitest run
```

Expected: 全绿，且 bindings 无未提交改动。

- [x] **Step 7: 提交**

```bash
git add desktop/src/lib/threadSearch.ts desktop/src/lib/threadSearch.test.ts docs/project-management \
  && git commit -m "feat: 分组参与侧栏搜索；同步项目进度文档"
```

---

## 自查

**Spec 覆盖**

| Spec 要求 | 实现任务 |
| --- | --- |
| `groups.json` 存储与原子写 | Task 1 |
| 同 workspace / 成员存在 / 空组解散 | Task 1（`prune`/`remove_thread`）+ Task 2（读路径过滤）+ Task 3（写路径删空） |
| 置顶与组框两处显示 | Task 6 Step 5 的 `!pinned` 只作用于框外 |
| `thread/listAll` 带 `groups` | Task 2 |
| 四个 RPC 与错误码 | Task 3 |
| `thread/groupsChanged` | Task 2 + Task 5 |
| 就地编辑 / 描述仅展开显示 | Task 6 |
| 多选建组 | Task 6 |
| 搜索收窄 | Task 7 |
| 组内不变更排序 | Task 4（`layoutWorkspace` 保持 `threads` 原序） |
| 不进模型上下文 | 全程仅侧栏 + 存储，无 prompt 路径改动 |

**占位符扫描**：无 TBD/TODO；每个代码步骤都给出可直接粘贴的内容。测试里对 harness 名字的若存在偏差，已在步骤内注明「按该文件既有约定改写，断言不变」。

**类型一致性**：`Group.thread_ids`（Rust，snake_case 落盘）↔ wire 的 `threadIds` ↔ TS `SessionGroup.threadIds`，三处名字在 Task 1/2/4 中逐一对齐；`layoutWorkspace` 的 `SidebarRow` 在 Task 4 定义、Task 6 消费，字段名一致。
