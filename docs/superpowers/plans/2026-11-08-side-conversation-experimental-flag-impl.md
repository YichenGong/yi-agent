# `/side` 纳入「实验功能」开关 — 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把旁问（`/side`、`/btw`、`/back`、`/side-close`）改为 `yi-agent-flags` 注册表里的一项实验开关，关掉时 TUI、app-server、桌面/手机三层同时不可用，且立即关闭在跑的旁支。

**Architecture:** 注册表加一项 `side_conversation`（默认关、dev-only 硬闸）。app-server 用一个可注入的 `SideGate = Arc<dyn Fn() -> bool>` 每请求重读判定：`initialize` 能力位随之下发，`side/*` 六个方法关着时回 `-32601`；`flags/set` 关掉后后台 spawn `SideRegistry::close_all()`（新增，与不可逆的 `shutdown()` 并列）。TUI 在装配点直接走既有的 `SideRuntime::disabled()` 路径，帮助文本改用可注入的 `help_text_with`。桌面端只加一条收尾：本地槽位的旁支绑定被丢弃、视图先返回主会话。

**Tech Stack:** Rust（`yi-agent-flags`、`yi-agent-app-server`、`yi-agent`）、React + TypeScript（`desktop/`）、ts-rs、nextest。

**Spec:** `docs/superpowers/specs/2026-11-08-side-conversation-experimental-flag-design.md`

## Global Constraints

- flag 名固定为 `side_conversation`，**发布后不改名**（它就是 `flags.json` 的键）。
- `FlagDef.default` 必须是 `false` —— `every_flag_defaults_to_off` 会强制。
- `expires_on` 必须是 `YYYY-MM-DD` 且晚于今天，本功能用 `2027-06-30`。
- 非 dev 构建一律全关、无逃生口；不得为 release 开后门（`store.rs` 的
  `status_at` 已实现硬闸，不要绕它）。
- 闸门判定**绝不**在 `yi-agent` crate 里调 `yi_agent_app_server::side_gate`
  （会反向依赖）；TUI 侧直接用 `yi_agent_flags::is_enabled`。
- 所有 Rust 改动提交前跑 `cargo fmt --all`；合并前 `just ci` 必须全绿。
- **不要在 `main` 上直接提交**；本计划在 `.worktrees/feat-side-experimental-flag` 里执行。
- 提交信息用 conventional commits，首行 <= 72 字符，**不写** `Co-Authored-By`。

---

## 文件结构

| 文件 | 责任 |
|---|---|
| `crates/yi-agent-flags/src/registry.rs` | flag 的唯一定义处（名字/说明/owner/到期/默认） |
| `crates/yi-agent-app-server/src/side_gate.rs` | 新建。旁支闸门的取值链（纯函数，可注入 path/build_kind） |
| `crates/yi-agent-app-server/src/rpc/side.rs` | `side/*` 六方法的前置闸门判定 |
| `crates/yi-agent-app-server/src/rpc/flags.rs` | `flags/set` 关掉后触发 `close_all` |
| `crates/yi-agent-app-server/src/side_conversation/registry.rs` | 新增 `close_all()` + epoch 护栏 |
| `crates/yi-agent-app-server/src/server.rs` | `RuntimeAttachments.side_gate` 字段与类型别名 |
| `crates/yi-agent-app-server/src/server/transport/mod.rs` | `initialize` 能力位 + `AppState.side_gate` 透传 |
| `crates/yi-agent-app-server/src/rpc/mod.rs` | `AppState.side_gate` 字段 |
| `crates/yi-agent-app-server/src/server/tests/mod.rs` | Harness 注入面（默认开 + 可关构造器） |
| `crates/yi-agent/src/main.rs` | 装配点按 flag 选择 `SideRuntime` |
| `crates/yi-agent/src/tui/slash.rs` | `help_text_with` + 薄包装 |
| `desktop/src/App.tsx` | `flagsCall` 关掉后清本地旁支绑定 |
| `docs/project-management/*.md` | 五个模块文件同步 |

---

### Task 1: 注册表登记 `side_conversation`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Produces: `pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";`
  —— 后续所有任务（Task 2、4、9）都用这个常量，不写字面量。

- [ ] **Step 1: 写失败的测试**

在 `registry.rs` 的 `mod tests` 里，`the_remote_ssh_flag_is_registered` 之后加：

```rust
#[test]
fn the_side_conversation_flag_is_registered() {
    let def = find(SIDE_CONVERSATION_FLAG).expect("side_conversation must be registered");
    assert!(
        !def.default,
        "side conversations must default to off: it is an experimental feature"
    );
    assert!(!def.summary.is_empty(), "the settings page shows this line");
    assert!(!def.owner.is_empty());
    assert_eq!(
        SIDE_CONVERSATION_FLAG, "side_conversation",
        "the name is the flags.json key; renaming it after release orphans user settings"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-flags --lib the_side_conversation_flag_is_registered`
Expected: 编译失败，`cannot find value SIDE_CONVERSATION_FLAG in this scope`

- [ ] **Step 3: 加常量与注册项**

在 `REMOTE_SSH_FLAG` 常量之后加：

```rust
/// 旁问（`/side`、`/btw`）的总开关。dev-only：stable 构建下恒关。
pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";
```

在 `REGISTRY` 数组末尾（`REMOTE_SSH_FLAG` 项之后）加：

```rust
    FlagDef {
        name: SIDE_CONVERSATION_FLAG,
        summary: "旁问：在主会话之外开一条只读临时分支（/side、/btw）",
        owner: "yichen",
        expires_on: "2027-06-30",
        default: false,
    },
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-flags --lib`
Expected: 全部通过（含既有的 `every_flag_defaults_to_off`、`no_flag_is_past_its_expiry_date`）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-flags/src/registry.rs
git commit -m "feat(flags): register the side_conversation experiment"
```

---

### Task 2: 新建 `side_gate` 判定模块

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/side_gate.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: `yi_agent_flags::registry::SIDE_CONVERSATION_FLAG`（Task 1）
- Produces:
  - `pub const SIDE_CONVERSATION_FLAG: &str`
  - `pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool`
  - `pub fn from_process_environment() -> bool`
  - `pub fn is_enabled(flag_value: Option<bool>) -> bool`
  - `pub type SideGate = Arc<dyn Fn() -> bool + Send + Sync>;`

- [ ] **Step 1: 定下模块的测试（写在模块内 `mod tests`）**

创建文件时直接包含测试。先只写模块骨架 + 测试：

```rust
//! 旁问功能的开关判定。
//!
//! 形状与 [`crate::lead_gate`] 一致：取值链复用 `yi-agent-flags`，但**不**读进程
//! 级全局量。区别在于旁问必须**运行中可关**，所以生产侧提供的是每次调用都重读
//! 的 [`from_process_environment`]，而不是装配时算一次的 `bool`。
//!
//! 不叫 `is_enabled` 的那个入口（[`resolve_from_store`]）是为了让测试能把
//! `flags.json` 指到 tempdir：`yi_agent_flags::is_enabled` 读的是进程级
//! `YI_AGENT_HOME`，并行测试互相污染。

use std::path::Path;
use std::sync::Arc;

/// 旁问开关的名字。与注册表同源：`the_flag_name_matches_the_registry` 钉住一致。
pub const SIDE_CONVERSATION_FLAG: &str = "side_conversation";

/// 每请求判定旁问是否可用。
///
/// 是闭包而不是 `bool`：用户的开关值可能在运行中改变，装配时算一次的快照会让
/// 「关掉开关」在已运行的宿主上完全无效——那正是本功能要消除的状态。
pub type SideGate = Arc<dyn Fn() -> bool + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_name_matches_the_registry() {
        assert!(
            yi_agent_flags::find(SIDE_CONVERSATION_FLAG).is_some(),
            "the gate and the registry must agree on the name"
        );
    }

    #[test]
    fn an_injected_value_wins() {
        assert!(is_enabled(Some(true)));
        assert!(!is_enabled(Some(false)));
    }

    #[test]
    fn a_missing_value_means_off() {
        assert!(!is_enabled(None), "a missing value must fail closed");
    }

    #[test]
    fn a_release_build_never_enables_it() {
        // 硬闸的鉴别测试：文件里写着 true，release 也必须关。
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, true).unwrap();
        assert!(
            !resolve_from_store(&path, "release"),
            "release must hard-close every flag"
        );
    }

    #[test]
    fn a_dev_build_follows_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert!(!resolve_from_store(&path, "dev"), "absent = off");
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, true).unwrap();
        assert!(resolve_from_store(&path, "dev"));
        yi_agent_flags::write_at(&path, SIDE_CONVERSATION_FLAG, false).unwrap();
        assert!(!resolve_from_store(&path, "dev"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side_gate`
Expected: 编译失败（`is_enabled` / `resolve_from_store` 未定义，模块也没注册）

- [ ] **Step 3: 补实现并注册模块**

在同文件测试模块之前加：

```rust
/// 从 `flags.json` + 构建类型解析生效值。**不读环境变量**：那是进程级的，会让
/// 并行测试互相污染（与 `lead_gate::resolve_from_store` 同一理由）。
pub fn resolve_from_store(flags_path: &Path, build_kind: &str) -> bool {
    matches!(
        yi_agent_flags::store::status_at(flags_path, SIDE_CONVERSATION_FLAG, build_kind, None),
        (true, _)
    )
}

/// 生产入口：每次调用都从真实环境重读。
///
/// 这里**刻意**读环境变量（与 `resolve_from_store` 相反）：生产进程里那正是
/// `YI_AGENT_FLAG_SIDE_CONVERSATION` 这条临时覆盖的入口。
pub fn from_process_environment() -> bool {
    yi_agent_flags::is_enabled(SIDE_CONVERSATION_FLAG)
}

/// 判定入口。`None` = 装配方漏传，**失败关闭**。
pub fn is_enabled(flag_value: Option<bool>) -> bool {
    flag_value == Some(true)
}
```

在 `lib.rs` 里按字母序插入：

```rust
pub mod side_gate;
```

具体位置：`pub mod shell_authorization;`（`:41`）之后、`pub mod side_cold;`（`:42`）
之前——`side_gate` < `side_cold` 不成立，但两者同属 `side_` 前缀，放在
`side_cold` 之前、`shell_authorization` 之后读起来是连贯的一组。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side_gate`
Expected: 5 passed

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/side_gate.rs crates/yi-agent-app-server/src/lib.rs
git commit -m "feat(app-server): add the side conversation gate"
```

---

### Task 3: `SideRegistry::close_all()` 与 epoch 护栏

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/side_conversation/registry.rs`

**Interfaces:**
- Produces: `pub async fn close_all(&self)`；私有字段 `sweep_epoch: AtomicU64`
- Consumes: 既有 `close`/`release_side_snapshots`/`stage`/`deliver`

> **这是本计划风险最高的一步。** 三个坑必须同时处理，否则「开关关了但旁支还在跑」：
> ① 不能置 `closed`（那是进程退出终态）；② 不能清 `ParentSlot::Creating`；
> ③ 在途 `create_inner` 会在清扫之后把 side 重新插回 `sides`。

- [ ] **Step 1: 写失败的测试**

在 `registry/tests.rs` 末尾加。该文件已有 `registry_fixture()` →
`(TempDir, TempDir, SideRegistry)`，`open("p1")` 是既有的开旁支手法，直接复用：

```rust
/// 关掉开关后，所有旁支必须消失，且 registry 仍可用。
///
/// 与既有的 shutdown 系列并列存在：那个测的是不可逆的进程退出，这个测的是
/// 可逆的开关。
#[tokio::test]
async fn close_all_stops_every_side_and_leaves_the_registry_usable() {
    let (_project, _home, registry) = registry_fixture().await;
    let first = registry.open("p1").await.unwrap();

    registry.close_all().await;

    assert_eq!(registry.side_count().await, 0, "every side must be gone");
    assert!(registry.list("p1").await.is_empty());

    // 可逆性：关掉再打开必须还能用。这条断言是 `close_all` 与 `shutdown()` 的
    // 分界线——复用 `shutdown()` 会让它失败。
    let again = registry
        .open("p1")
        .await
        .expect("close_all must not be terminal");
    assert_ne!(again.side_id, first.side_id, "a new generation");
}

/// 清扫与在途创建竞争时，不能留下一条活旁支。
///
/// 两种在途形状都要覆盖：
/// 1. 创建者已经跑在 `create_inner` 里（文件操作之后才 `sides.insert`）。
/// 2. 第二条 `open` 在同一父的 `Creating` 占位上等，清扫后它醒来**不能**
///    摇身变成新的创建者。
///
/// 第 2 种是 epoch 必须同时护住 `open()` 的原因：只护 `create_inner` 的话，
/// 等待者会带着清扫之后的新代次重试，成功建出一条活旁支。
#[tokio::test]
async fn close_all_wins_against_a_concurrent_open() {
    for _ in 0..20 {
        let (_project, _home, registry) = registry_fixture().await;
        let registry = Arc::new(registry);
        let a = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move { registry.open("p1").await }
        });
        let b = tokio::spawn({
            let registry = Arc::clone(&registry);
            async move { registry.open("p1").await }
        });
        tokio::task::yield_now().await;
        registry.close_all().await;
        let _ = a.await;
        let _ = b.await;
        assert_eq!(
            registry.side_count().await,
            0,
            "a create or a woken waiter that outlived the sweep must not leave a live side"
        );
    }
}
```

> `registry_fixture()` 返回的是 `SideRegistry` 值（不是 `Arc`），所以这里
> `Arc::new(registry)` 是必要的（`tokio::spawn` 要 `'static`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib close_all_`
Expected: 编译失败（`close_all` 未定义）

- [ ] **Step 3: 加字段、`close_all` 与 epoch 复核**

**(a) 结构体与 `new()`**：在 `SideRegistry` 的 `closed` 字段之后加

```rust
    /// 清扫代次。每次 `close_all()` 自增。
    ///
    /// `create_inner` 与 `open()` 的等待者在把一条 side 对外可见之前都要复核它，
    /// 不一致就放弃。没有它，「关掉开关」与「正在创建」并发时会漏：`create_inner`
    /// 要读完父会话、复制附件之后才 `sides.insert`，那时清扫可能已经跑完了。
    ///
    /// **两处都要复核**（只护 `create_inner` 不够）：等待者在 `Creating` 占位上
    /// 醒来后会回到 `open()` 的 loop，若不复核就会以清扫之后的新代次重试，成功
    /// 建出一条活旁支。
    sweep_epoch: AtomicU64,
```

`new()` 里补 `sweep_epoch: AtomicU64::new(0),`（放在 `closed: ...` 之后）。
`AtomicU64` 与 `Ordering` 已在文件顶部导入，无需新增。

**(b) `close_all`**：加在 `shutdown()` **之前**，照 `close()`（`:720`）的既有顺序
抄——**先关 runner，再 stage 终态，最后投递**。顺序反过来（先 stage 再等 runner）
会让 `seq` 在终态帧上提前推进，客户端可能把一个「已宣布关闭」的 revision 当成
仍在演进的当前状态。

```rust
    /// 关闭所有旁支，但不把 registry 置为终态。
    ///
    /// 与 [`Self::shutdown`] 的语义差：
    ///
    /// - **不碰 `closed`**：开关是可逆的，关掉之后用户还能再打开。
    /// - **写 tombstone**：与 `close()` 一致，让「刚关掉的那条」还能回答
    ///   `side_not_found` 而不是「从未存在」（重连时需要这个区别）。
    /// - **发布 `side/closed`**：客户端靠这个终态帧丢弃本地内容与在途分页。
    ///
    /// 并发处理的三个要点：
    ///
    /// - 只移除 `ParentSlot::Ready`，保留 `Creating`：清掉占位会让在途创建的
    ///   `create()` 把 `Ready` 写回去。
    /// - 自增 `sweep_epoch`：`create_inner` 在 `sides.insert` 之前复核、`open()`
    ///   的等待者醒来后复核，不一致就放弃。
    /// - 有界多轮：第一轮期间可能有 `Creating` 落定成 `Ready`。
    ///
    /// 不能内联在请求路径上：等 runner 收敛可能要几百毫秒，主循环是单 task。
    pub async fn close_all(&self) {
        self.sweep_epoch.fetch_add(1, Ordering::SeqCst);
        // 三轮足够覆盖「第一轮期间创建落定」这一种形状。超过上限记警告而不是
        // 静默返回——留下一条活旁支是本功能最不该有的失败模式。
        const MAX_ROUNDS: usize = 3;
        for round in 0..MAX_ROUNDS {
            let entries: Vec<(String, Arc<Mutex<SideEntry>>)> = {
                let sides = self.sides.lock().await;
                sides
                    .iter()
                    .map(|(k, v)| (k.clone(), Arc::clone(v)))
                    .collect()
            };
            if entries.is_empty() {
                self.drop_ready_parents().await;
                return;
            }
            for (side_id, entry) in &entries {
                // 与 `close()` 同一顺序：先关 runner（它会 join 核心任务），
                // 再 stage 终态，最后在锁外投递。
                let (parent, generation, accepted, closed) = {
                    let mut guard = entry.lock().await;
                    guard.closing = true;
                    let accepted: Vec<(String, String)> =
                        guard.ledger.accepted_requests().into_iter().collect();
                    let parent = guard.parent.clone();
                    let generation = guard.generation;
                    let _ = guard.runner.close().await;
                    let staged = Self::stage(&mut guard, |e| {
                        super::SidePublishedUpdate::Closed {
                            side_id: e.side_id.clone(),
                            reason: CloseReason::UserClosed,
                        }
                    });
                    (parent, generation, accepted, staged)
                };
                if let Some(staged) = closed {
                    self.deliver(staged).await;
                }
                self.release_side_snapshots(side_id).await;
                self.sides.lock().await.remove(side_id);
                {
                    let mut parents = self.parents.lock().await;
                    if let Some(ParentSlot::Ready(current)) = parents.get(&parent)
                        && current == side_id
                    {
                        parents.remove(&parent);
                    }
                }
                let mut tombstones = self.tombstones.lock().await;
                tombstones.insert(
                    side_id.clone(),
                    Tombstone {
                        side_id: side_id.clone(),
                        parent,
                        generation,
                        accepted,
                        created_at: tokio::time::Instant::now(),
                    },
                );
                Self::prune_tombstones(&mut tombstones, tokio::time::Instant::now());
            }
            self.drop_ready_parents().await;
            if self.side_count().await == 0 {
                return;
            }
            if round + 1 == MAX_ROUNDS {
                tracing::warn!(
                    remaining = self.side_count().await,
                    "close_all hit its round limit; some sides are still live"
                );
            }
        }
    }

    /// 移除所有 `Ready` 的父占位，保留 `Creating`。
    async fn drop_ready_parents(&self) {
        let mut parents = self.parents.lock().await;
        parents.retain(|_, slot| matches!(slot, ParentSlot::Creating(_)));
    }
```

> `Tombstone` 是 `create()` 与 `close()` 已在用的结构（字段：`side_id`、`parent`、
> `generation`、`accepted`、`created_at`），照抄即可，不要改它的定义。
> `guard.ledger.accepted_requests()` 就是 `close()` 里那段。

**(c) `create_inner` 的 epoch 复核**：方法开头（`let source = ...` **之前**）记下

```rust
        let epoch = self.sweep_epoch.load(Ordering::SeqCst);
```

并在 `sides.insert(...)` 那一块（`registry.rs:564-567`）改成

```rust
        {
            let mut sides = self.sides.lock().await;
            if self.sweep_epoch.load(Ordering::SeqCst) != epoch {
                // 清扫发生在创建期间：这条 side 从未对外可见，直接丢弃。
                // 返回 `SideClosing` 让 `create()` 的 Err 分支移除父占位，
                // 而不是把 `Ready` 写回去。
                return Err(SideError::new(
                    SideErrorReason::SideClosing,
                    "清扫期间创建的旁支已丢弃",
                ));
            }
            sides.insert(side_id.clone(), Arc::clone(&entry));
        }
```

**(d) `open()` 的等待者复核**：这是最容易漏的一处。`open()` 的 loop 在
`Some(Ok(future)) => future.await` 之后会**重新进 loop**，此时若著 epoch 已变，
它就成了新的创建者。改 `open()` 开头：

```rust
    pub async fn open(&self, parent: &str) -> Result<SideSummary, SideError> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(SideError::new(SideErrorReason::SideClosing, "宿主正在关闭"));
        }
        // 记下进来时的代次：等待者在 `Creating` 上醒来后会走到 loop 顶部，
        // 若期间发生过清扫，这次 open 属于「清扫之前的意图」，不能再建。
        let epoch = self.sweep_epoch.load(Ordering::SeqCst);
        loop {
            if self.sweep_epoch.load(Ordering::SeqCst) != epoch {
                return Err(SideError::new(
                    SideErrorReason::SideClosing,
                    "清扫期间放弃创建旁支",
                ));
            }
            // ...既有的 loop 体保持原样...
```

> 第二次 `open()`（新建意图）会读到新的代次，正常工作；只有**跨越清扫**的那次
> 才会被拒。这与 spec §3.5「`close_all` 期间有请求进来 → 直接 `-32601`」一致：
> 关掉开关后的下一次请求会被闸门先挡掉，这里的拒只是兜住闸门之前已经进来的那次。

- [ ] **Step 4: 跑测试确认失败再通过（TDD 两段）**

先只加测试（不动实现）：

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib close_all_`
Expected: 编译失败（`close_all` 未定义）

实现后：

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib close_all_`
Expected: 2 passed


- [ ] **Step 5: 跑全套确认没打破既有语义**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side_conversation`
Expected: 全绿（既有的 `shutdown` 系列不变）

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/side_conversation/registry.rs crates/yi-agent-app-server/src/side_conversation/registry/tests.rs
git commit -m "feat(app-server): add a reversible close_all to the side registry"
```

---

### Task 4: 把 `side_gate` 接进 `RuntimeAttachments` 与 `AppState`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:350-430`（`RuntimeAttachments`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs:530-545`（`AppState`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs:165-200`、`:767-815`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/scoped.rs:69`、`:163`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs:144`

**Interfaces:**
- Consumes: `crate::side_gate::SideGate`（Task 2）
- Produces: `RuntimeAttachments.side_gate: crate::side_gate::SideGate`；
  `AppState.side_gate: &'a crate::side_gate::SideGate`

- [ ] **Step 1: 先改结构体，让编译器列出所有构造点**

在 `RuntimeAttachments` 里，`lead_agent_enabled: bool` 之后加：

```rust
    /// 旁问开关。**判定函数**而不是 `bool`：用户会在运行中拨开关，装配时算
    /// 一次的快照会让「关掉」在已运行的宿主上无效（spec §3.2）。
    ///
    /// 生产构造点统一调 [`crate::side_gate::from_process_environment`]；测试直接
    /// 给闭包，避免依赖进程级 `YI_AGENT_HOME` 与恒为 `release` 的 build kind。
    pub(crate) side_gate: crate::side_gate::SideGate,
```

在 `AppState` 里，`lead_agent_enabled: bool` 之后加：

```rust
    /// 旁问开关的判定函数。每请求调一次，见 `RuntimeAttachments::side_gate`。
    pub(crate) side_gate: &'a crate::side_gate::SideGate,
```

Run: `cd yi-agent-rs && cargo check -p yi-agent-app-server 2>&1 | grep "missing field" | head`

- [ ] **Step 2: 补所有构造点**

`transport/scoped.rs` 两处、`ws.rs` 一处：在 `lead_agent_enabled: ...` 之后各加

```rust
            side_gate: Arc::new(crate::side_gate::from_process_environment),
```

`transport/mod.rs` 的 `RuntimeAttachments` 解构处（`:165` 左右的 `let RuntimeAttachments {`）加一行 `side_gate,`，并在 `:767` 附近的 `AppState {` 里加

```rust
                            side_gate: &side_gate,
```

- [ ] **Step 3: 让测试 harness 也编译**

`Harness` 的私有构造链是三层，`side_gate` 必须一路透到最里层的
`RuntimeAttachments {`。**按下面的顺序改，`cargo check` 会把每个漏传点报出来。**

(a) `server/tests/mod.rs` 的 `with_config_and_scope_and_watchman_in`（`:957` 起）
签名里，`side_registry` 之后加一个参数：

```rust
        side_gate: crate::side_gate::SideGate,
```

并在该函数体内 `RuntimeAttachments {` 的 `side_registry,` **之前**加：

```rust
                // 旁支默认**开**：既有 side 测试全部假定能力可用（spec §6）。
                // 需要关的用例走 `with_scope_side_flag_and_registry`（下面 Step 4）。
                side_gate,
```

(b) `with_config_and_scope_and_watchman`（`:916` 起）签名同样加
`side_gate: crate::side_gate::SideGate`，并在它调 `_in(...)` 的参数表里
`side_registry` 之后传入 `side_gate`。

(c) 该函数**已有 4 个调用点**，每个都补 `Arc::new(|| true)`：
- `with_scope_and_side_registry`（`:661`）
- `with_config_and_scope`（`:821`）
- `with_config_lead_and_scope`（`:851`）
- `with_watchman`（`:884`）

(d) 直接调 `_in(...)` 的 3 个调用点各补 `Arc::new(|| true)`：
- `with_scope_and_instance_home`（`:698`）
- `with_management`（`:735`）
- `with_config_and_scope_and_watchman`（`:938`，即 (b) 里那一处）

(e) 手工装配 `RuntimeAttachments` 的 3 个测试文件里各补同一行：
- `server/tests/thread_index.rs:580`、`:675`
- `server/tests/transport.rs:108`

```rust
                side_gate: Arc::new(|| true),
```

Run: `cd yi-agent-rs && cargo check -p yi-agent-app-server --tests 2>&1 | grep -E "^error|missing field" | head -20`

Expected: 无输出

- [ ] **Step 4: 新增可指定的 harness 构造器**

在 `server/tests/mod.rs` 的 `with_lead_flag`（`:762`）之后加两个。都走
**新增独立构造器**：`with_config_and_scope_and_watchman` 已有 4 个调用点，给它
换语义会牵动 `with_watchman` 这类与旁支无关的用例。

```rust
    /// 旁问开关按值注入。scope 固定 `Admin`。
    ///
    /// 走注入而不是写 `flags.json`：后者要求 build kind 为 `dev` 且共享进程级
    /// `YI_AGENT_HOME`，并行测试必然互相污染（同 `with_lead_flag` 的理由）。
    ///
    /// 不传注册表（`None`）：`serve()` 会按实例家目录自建一份，与既有
    /// `with_scope` 同一形状。需要注入注册表的用例走下面那个构造器。
    pub(crate) async fn with_side_flag(enabled: bool) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            None,
            None,
            Arc::new(move || enabled),
        )
        .await
    }

    /// 三个维度都可指定：scope、注册表、旁问开关。
    ///
    /// `side/*` 的闸门测试要同时钉「权限档不构成回答」与「关掉时方法不存在」，
    /// 必须能同时控制前两者。
    pub(crate) async fn with_scope_side_flag_and_registry(
        scope: Scope,
        registry: Arc<crate::side_conversation::SideRegistry>,
        side_enabled: bool,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            Some(registry),
            None,
            None,
            Arc::new(move || side_enabled),
        )
        .await
    }
```

> 两个构造器都是 `async fn` 且 `_in` 返回 `Self`，所以末尾各自 `.await`
> （`with_scope_and_side_registry` 就是这么写的：`Self::with_config_and_scope_and_watchman(...).await`）。
> 参数顺序照抄 `with_scope_and_instance_home`：`calls, lead_agent_enabled,
> side_registry, instance_home_override, management, side_gate`。

- [ ] **Step 5: 编译并跑既有 side 测试，确认零回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side`
Expected: 全绿（此时能力位还没接线，所以行为与今天完全一致）

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/
git commit -m "refactor(app-server): plumb the side gate through attachments and state"
```

---

### Task 5: `side/*` 六方法的前置闸门

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/side.rs:60-75`

**Interfaces:**
- Consumes: `AppState.side_gate`（Task 4）

- [ ] **Step 1: 写失败的测试**

在 `server/tests/side.rs` 末尾加：

```rust
/// 关掉开关后，六个方法都必须报「不存在」，而不是静默成功或「权限不够」。
///
/// 逐条断言不抽样：漏掉一个方法就是一个仍然可用的入口。
#[tokio::test]
async fn every_side_method_is_method_not_found_when_the_flag_is_off() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_side_flag_and_registry(Scope::Admin, registry, false).await;
    initialize(&mut h).await;

    for (i, method) in [
        "side/open",
        "side/list",
        "side/readItems",
        "side/turnStart",
        "side/turnInterrupt",
        "side/close",
    ]
    .iter()
    .enumerate()
    {
        let answer = call(&mut h, 10 + i as u64, method, serde_json::json!({})).await;
        assert_eq!(
            answer["error"]["code"],
            serde_json::json!(-32601),
            "{method} must not exist while the flag is off: {answer}"
        );
    }

    h.shutdown().await;
}
```

> 需要一个「给定 scope + registry + flag」的 Harness 构造器。若 Task 4 只加了
> `with_side_flag(bool)`，就在这里再加一个接受 registry 的变体
> `with_scope_side_flag_and_registry(scope, registry, enabled)`；名字可以不同，
> **能力必须齐**：scope、registry、flag 三者都可指定。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib every_side_method_is_method_not_found_when_the_flag_is_off`
Expected: FAIL —— 得到 `-32011`（side_not_found）或 `-32602`，而不是 `-32601`

- [ ] **Step 3: 在 `rpc/side.rs` 里前置判定**

在 `if !owns(method) { return Ok(Flow::Unknown); }` **之后**、scope 检查**之前**加：

```rust
    // 功能不存在时，权限档不构成回答：客户端要能区分「本版本没有旁问」与
    // 「你没有权限」，否则会渲染一个必然失败的面板。与 `remote_ssh` 关掉时
    // `remote/relayTargets` 的处理同构。
    if !(state.side_gate)() {
        write_response(
            state.hub,
            state.client,
            err_response(id, RpcError::method_not_found(method)),
        )
        .await?;
        return Ok(Flow::Continue);
    }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib every_side_method_is_method_not_found_when_the_flag_is_off`
Expected: PASS

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side`
Expected: 全绿（harness 默认开，既有用例不受影响）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/rpc/side.rs crates/yi-agent-app-server/src/server/tests/
git commit -m "feat(app-server): hide side/* while the flag is off"
```

---

### Task 6: `initialize` 的能力位跟随开关

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs:1015-1022`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/side.rs:167-186`、`:270-290`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:1162-1167`（注释）

**Interfaces:**
- Consumes: `side_gate`（Task 4）

- [ ] **Step 1: 写失败的测试**

在 `server/tests/side.rs` 里新增（不改既有两条）：

```rust
/// 能力位跟着开关走，但宿主 id 与它无关：客户端用后者区分「重连」与「宿主
/// 换代」，翻能力位时把 id 一起改掉会让它丢掉全部本地基线。
#[tokio::test]
async fn the_capability_bit_follows_the_flag_but_the_host_id_does_not() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let expected = registry.host_instance_id().to_string();
    let mut h = Harness::with_scope_side_flag_and_registry(Scope::Admin, registry, false).await;

    let hello = call(&mut h, 1, "initialize", serde_json::json!({})).await;
    assert_eq!(
        hello["result"]["capabilities"]["side_conversation_v1"],
        serde_json::json!(false),
        "{hello}"
    );
    assert_eq!(
        hello["result"]["sideHostInstanceId"],
        serde_json::json!(expected),
        "the host id must not depend on the flag: {hello}"
    );

    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_capability_bit_follows_the_flag`
Expected: FAIL —— 能力位仍是 `true`

- [ ] **Step 3: 改下发值**

`server/transport/mod.rs` 里 `"side_conversation_v1": crate::protocol::SIDE_CONVERSATION_V1,` 改为：

```rust
                                // 能力位跟随本机开关；宿主 id 与它无关（下一行），
                                // 客户端靠 id 区分「重连」与「宿主换代」。
                                "side_conversation_v1": (side_gate)(),
```

同时把 `protocol.rs` 里 `SIDE_CONVERSATION_V1` 的 doc 注释从「当前值」改成：

```rust
/// 本协议版本**支持**旁支协议（不是「当前启用」）。
///
/// 实际下发值由 `side_gate` 决定：能力位是逐宿主、逐时刻的判定，常量只声明
/// 协议存在。`assert!(SIDE_CONVERSATION_V1)` 保留在测试里钉住协议未被移除。
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_capability_bit_follows_the_flag`
Expected: PASS

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib side`
Expected: 全绿。既有的两条（`side_initialize_advertises_capability_and_a_host_id`
`:175`、`side_handshake_and_list_work_together` `:280`）走
`with_scope_and_side_registry`，那个构造器在 Task 4 里被改成传
`Arc::new(|| true)`，所以它们断言的 `== SIDE_CONVERSATION_V1`（常量值为 `true`）
仍然成立，**不需要改**。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/
git commit -m "feat(app-server): advertise the side capability from the flag"
```

---

### Task 7: `flags/set` 关掉后关闭所有旁支

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/flags.rs:23-47`
- Test: 同文件 `#[cfg(test)] mod tests`（新建）

**Interfaces:**
- Consumes: `SideRegistry::close_all`（Task 3）、`side_gate::SIDE_CONVERSATION_FLAG`（Task 2）
- Produces: `pub(crate) fn turns_side_conversation_off(params: &Value) -> bool`

> **为什么这里不写端到端测试。** `flags/set` 的实现在 `flags_rpc::handle_flags_request`
> 里，第一行就是「非 dev 构建 → `-32601`」；而本 crate 的 lib 测试从不注入 build
> info，`build_kind()` **恒为 `release`**（`lead_gate.rs` 的注释里写明了这个坑）。
> 于是经 RPC 发 `flags/set` 在测试里永远返回 method-not-found，端到端断言它「写成功
> 并触发清扫」是**不可能通过**的。所以本任务的可测部分是「判定」与「清扫」两块，
> 分别由纯函数测试和 Task 3 的注册表级测试覆盖；粘合代码只有三行，靠 `cargo check`
> 与人工验收（Task 11）保证。

- [ ] **Step 1: 写失败的测试**

在 `rpc/flags.rs` 末尾新建测试模块：

```rust
#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn turning_the_side_flag_off_is_recognized() {
        assert!(turns_side_conversation_off(&json!({
            "name": crate::side_gate::SIDE_CONVERSATION_FLAG,
            "enabled": false,
        })));
    }

    #[test]
    fn other_names_and_directions_are_not_mistaken_for_it() {
        // 打开不是清扫时机：那正是要允许用旁支的时候。
        assert!(!turns_side_conversation_off(&json!({
            "name": crate::side_gate::SIDE_CONVERSATION_FLAG,
            "enabled": true,
        })));
        // 关掉别的开关不该连坐。
        assert!(!turns_side_conversation_off(
            &json!({"name": "remote_ssh", "enabled": false})
        ));
        // 形状不对一律不算：宁可漏一次清扫（下次拨开关还会触发），
        // 也不要在参数畸形时误清。
        assert!(!turns_side_conversation_off(&json!({})));
        assert!(!turns_side_conversation_off(&json!({"name": "side_conversation"})));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turns_side_conversation_off`
Expected: 编译失败（`turns_side_conversation_off` 未定义）

- [ ] **Step 3: 加判定函数并在 `flags/set` 成功后触发清扫**

在 `rpc/flags.rs` 的 `handle` 之前加：

```rust
/// 这次 `flags/set` 是不是「把旁问关掉」。
///
/// 独立成纯函数是为了可测：`flags/set` 的实际写盘在 `flags_rpc` 里受
/// build kind 硬闸约束，测试二进制永远走不到那条成功路径。
pub(crate) fn turns_side_conversation_off(params: &serde_json::Value) -> bool {
    params.get("name").and_then(|v| v.as_str()) == Some(crate::side_gate::SIDE_CONVERSATION_FLAG)
        && params.get("enabled").and_then(|v| v.as_bool()) == Some(false)
}
```

把 `flags/set` 分支的 `Ok(value)` 改成：

```rust
                Ok(value) => {
                    if turns_side_conversation_off(&req.params) {
                        // 放在请求路径之外：`close_all` 要等 runner 收敛，内联
                        // `await` 会堵住单 task 主循环（与 `side/open` 的纪律相同）。
                        //
                        // 放在宿主而不是前端：开关也可能经 CLI 拨（那时前端根本
                        // 不存在）。CLI 拨完正在跑的 TUI 不会热重载，但**这个
                        // 进程里的旁支会立刻停下**——那是「立即关闭」的含义。
                        let registry = Arc::clone(state.side_registry);
                        tokio::spawn(async move {
                            registry.close_all().await;
                        });
                    }
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
```

文件顶部补 `use std::sync::Arc;`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib turns_side_conversation_off`
Expected: 2 passed

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib close_all`
Expected: 全绿（Task 3 的两条，含并发竞争那条）

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib flags`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/src/rpc/flags.rs
git commit -m "feat(app-server): close live sides when the flag turns off"
```

---

### Task 7b: 人工验收「关开关即停在跑的旁支」

自动化测试到此为止（理由见 Task 7 开头）。这一条是**必须人工做一次**的验收，
写进 Task 11 的清单：

1. dev 构建启动 TUI，`/side` 提一个会跑较久的问题。
2. 另开一个终端 `yi-agent flags set side_conversation false`。
3. 观察 TUI 里那条旁支：新一轮 `side/*` 请求应当被拒（`-32601`），并且在跑的
   旁支由宿主 `close_all` 停掉（该进程的日志里出现对应的关闭记录）。
4. 重新 `flags set side_conversation true` 并重启 TUI，`/side` 恢复可用。

---

### Task 8: TUI 装配点按 flag 选择 `SideRuntime`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:3486-3520`

**Interfaces:**
- Consumes: `yi_agent_flags::is_enabled(yi_agent_flags::registry::SIDE_CONVERSATION_FLAG)`

- [ ] **Step 1: 加开关判定并在关时走 disabled**

`main.rs` 里 `// 旁支:一份 source 发布器 + 一个独立调度任务。` 那段之前加：

```rust
        // 旁问是实验功能：关掉时不建 resources、不 spawn driver，直接走既有的
        // `disabled()` 降级路径。这条路径本来就在 resources 初始化失败时使用，
        // 所以 TUI 各处（`side_capable` 的所有分支）都已按能力缺失处理过。
        let side_enabled = yi_agent_flags::is_enabled(yi_agent_flags::registry::SIDE_CONVERSATION_FLAG);
```

然后把 `let (side_publisher, side_runtime) = match side_resources {...}` 整体包一层：

```rust
        let (side_publisher, side_runtime) = if !side_enabled {
            (None, crate::tui::app::SideRuntime::disabled())
        } else {
            match side_resources {
                // ……既有的两个分支原样保留……
            }
        };
```

> 注意：`side_resources` 是 `Result`，关掉时**不要**再构造它。若它的构造写在
> 上面几行（`let side_resources = SideHostResources::new(...)`），把整段移进
> `else` 分支里再构造，避免关掉时仍去建目录、取 flock。

- [ ] **Step 2: 编译并跑 TUI 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent side 2>&1 | tail -20`
Expected: 全绿（TUI lib 测试里 build_kind 为 release，判定恒为关，所以走 disabled；
既有 side 测试用的是显式的 `SideRuntime::new`，不受影响）

- [ ] **Step 3: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/main.rs
git commit -m "feat(tui): only wire side conversations when the flag is on"
```

---

### Task 9: `help_text_with` 与帮助目录条件化

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs:241-290`、`mod tests`

**Interfaces:**
- Produces: `pub fn help_text_with(target: Option<&str>, side_enabled: bool) -> String`
- 保留 `pub fn help_text(target: Option<&str>) -> String` 作为薄包装

- [ ] **Step 1: 写失败的测试**

在 `slash.rs` 的 `mod tests` 里加：

```rust
#[test]
fn help_omits_the_side_section_when_the_flag_is_off() {
    let help = help_text_with(None, false);
    for absent in ["/side", "/btw", "/back", "/side-close"] {
        assert!(
            !help.contains(absent),
            "a disabled feature must not be listed as available: {absent} in {help}"
        );
    }
    assert!(help.contains("/config"), "the rest of the catalog stays");
}

#[test]
fn contextual_help_still_explains_a_disabled_side_command() {
    // 用户可能从旧文档或别人那里得知 /side。查它要给出「本版本没有」而不是
    // 「未知命令」——后者会把「功能不存在」误报成「你打错了」。
    let text = help_text_with(Some("side"), false);
    assert!(text.contains("/side"), "{text}");
    assert!(text.contains("本版本未启用"), "{text}");
}

#[test]
fn help_lists_side_commands_when_the_flag_is_on() {
    let help = help_text_with(None, true);
    assert!(help.contains("/side [问题]"), "{help}");
    assert!(help.contains("/back"), "{help}");
    assert!(help.contains("/side-close"), "{help}");
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent help_ 2>&1 | tail -20`
Expected: 编译失败（`help_text_with` 未定义）

- [ ] **Step 3: 改造 `help_text`**

把现有 `pub fn help_text(target: Option<&str>) -> String {` 改为：

```rust
/// 生产入口：按本进程的开关值渲染帮助。
pub fn help_text(target: Option<&str>) -> String {
    help_text_with(target, side_enabled())
}

/// 本进程是否启用旁问。
///
/// `main` 启动时已经注入了真实 build kind，所以 release 里恒假——硬闸也覆盖
/// `/help`，不会出现「帮助说能用、敲了没用」。
fn side_enabled() -> bool {
    yi_agent_flags::is_enabled(yi_agent_flags::registry::SIDE_CONVERSATION_FLAG)
}

/// 渲染命令目录。`side_enabled` 显式传入而不是内部读全局：`help_text` 是纯函数，
/// lib 测试里 build kind 恒为 `release`，内部读会让「开启」这一侧永远测不到。
pub fn help_text_with(target: Option<&str>, side_enabled: bool) -> String {
```

然后在函数体内做两处条件化：

**(a) 目录段**——`text.push_str("\n旁问:\n");` 那段用 `if side_enabled { ... }` 包住：

```rust
            if side_enabled {
                text.push_str("\n旁问:\n");
                for (usage, description) in super::side::SIDE_HELP_ENTRIES {
                    text.push_str(&format!("  /{usage} {description}\n"));
                }
            }
```

**(b) 上下文帮助**——`Some((usage, description)) => format!("/{usage}\n{description}")` 改为：

```rust
                Some((usage, description)) => {
                    if side_enabled {
                        format!("/{usage}\n{description}")
                    } else {
                        format!("/{usage}\n{description}\n（本版本未启用旁问）")
                    }
                }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent help_ 2>&1 | tail -20`
Expected: 全绿。既有 5 条帮助测试走 `help_text` 薄包装，而 lib 测试里 build kind
为 release → `side_enabled()` 为假 → **它们会红**。此时把既有测试改成显式传入：

```rust
    let help = help_text_with(None, true);
```

`full_help_lists_side_commands_once_without_the_alias`、
`contextual_help_resolves_side_commands_but_not_the_alias_entry`、
`full_help_lists_only_completable_commands`（后者不涉及 side，可不改）——
**只改前两条**，并在注释里写明「显式开：lib 测试的 build kind 是 release」。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): hide side commands from help when the flag is off"
```

---

### Task 10: 桌面端 `flagsCall` 收尾

**Files:**
- Modify: `desktop/src/App.tsx:916-960`（`flagsCall`）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `LOCAL_SLOT`（`desktop/src/lib/slots.ts`）、`sideStoreRef`

- [ ] **Step 1: 写失败的测试**

在 `App.test.tsx` 的 side 相关 describe 里加（沿用文件内既有的
`state.sideSupported`、`clients[0].requests` 手法）：

```tsx
it("drops the local side binding when the flag is turned off", async () => {
  state.sideSupported = true;
  await command("/btw why is it slow");
  await waitFor(() => expect(methods()).toContain("side/open"));

  // 用户在设置页把开关关掉。宿主会 close_all；前端必须把本地视图一起清掉，
  // 否则界面上留着一个永远 side_not_found 的壳。
  fireEvent.click(screen.getByRole("button", { name: /设置/ }));
  fireEvent.click(await screen.findByRole("tab", { name: "实验功能" }));
  fireEvent.click(
    await screen.findByRole("checkbox", { name: "side_conversation 开关" }),
  );

  await waitFor(() => expect(methods()).toContain("flags/set"));
  await waitFor(() =>
    expect(screen.queryByRole("button", { name: "返回主会话" })).toBeNull(),
  );
});

it("leaves remote side bindings alone when the local flag turns off", async () => {
  // 远端项目跑的是另一台机器的二进制，本机拨开关不该影响它（spec §3.6）。
  // 断言只检查本地槽位的 binding 被丢弃、远端仍登记。
});
```

> 第二条断言需要 touching 到 `registryRef` / `sideStoreRef` 的内部状态。若现有
> 测试没有暴露这类观察面，就把它降级为「只断言本地视图消失」，并在测试里写一行
> 注释说明远端的隔离由「flags 是机器级、远端走自己的 initialize」保证，不在此
> 用例覆盖。**不要**为了这条测试在生产代码里加观察接口。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "drops the local side binding"`
Expected: FAIL —— 返回按钮仍在

- [ ] **Step 3: 在 `flagsCall` 里加收尾**

在 `flagsCall` 的 `pending.then(async (result) => { ... })` 里，`remote_ssh` 那段
**之前**加：

```tsx
        // 关掉旁问开关时立刻清掉**本地**槽位的旁支视图。宿主那边的 `close_all`
        // 已经把它们都关了，界面上再挂着就是点不动的空壳。
        //
        // 只处理本地：远端项目跑的是另一台机器的二进制，它们的开关由那台机器的
        // `initialize` 决定（spec 2026-11-03 §5.2 的同一口径）。
        if (written.name === "side_conversation" && written.enabled === false) {
          // 先返回主会话再销毁：顺序反了会让用户盯着一个已消失的视图。
          // `back()` 无参、且能撤销一个尚未完成的入场，正是这里要的语义
          // （`selectMain` 需要 binding + parentId，而此刻视图可能已经没 binding 了）。
          sideConversationRef.current.back();
          const store = sideStoreRef.current;
          for (const binding of store.bindingsForSlot(LOCAL_SLOT)) {
            store.dropHost(binding.key);
          }
        }
```

三处名字都按文件内既有形态接线，**不要**新造 ref：
- `sideConversationRef.current.back()` —— `SideConversationController.back: () => void`
  （`useSideConversationController.ts:66`，实现见 `:585`）。`flagsCall` 原本只闭合
  `clientRef`、deps 为空，改走 `Ref.current` 同样不引入依赖。
- `sideStoreRef.current.bindingsForSlot(LOCAL_SLOT)` —— 同文件 `:1210`、`:3042` 已有用法。
- `store.dropHost(binding.key)` —— `dropHost` 是唯一销毁入口
  （`lib/sideConversation.ts:256` 注释、`:322` 实现）。

`sideConversationRef` 是 `clearError` 之外唯一需要的 ref，已在 App 里存在（`:1212` 用到）。
若 `back()` 在此之前并未被 `sideConversationRef.current` 调用过，直接照加即可——
controller 的返回对象里已导出 `back`（`useSideConversationController.ts:831` 附近）。

- [ ] **Step 4: 跑测试与类型检查**

Run: `cd desktop && npx vitest run src/App.test.tsx -t "drops the local side binding"`
Expected: PASS

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: 全绿

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): clear local side state when the flag turns off"
```

---

### Task 11: 文档同步与全量验证

**Files:**
- Modify: `docs/project-management/yi-agent-flags.md`
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/yi-agent-tui.md`
- Modify: `docs/project-management/desktop.md`

- [ ] **Step 1: 更新四个模块文件**

每个文件里找到对应条目，追加一段，**必须包含可验证判据**：

- `yi-agent-flags.md`：注册表新增 `side_conversation`（默认关、dev-only、
  到期 2027-06-30）。判据：`cargo test -p yi-agent-flags --lib`；
  `yi-agent flags status side_conversation` 在 dev 报 `source: default|file`、
  在 release 报 `source: build-kind`。
- `yi-agent-app-server.md`：`side/*` 六方法与 `initialize` 能力位受
  `side_gate` 逐请求判定；`flags/set` 关掉后后台 `close_all()`。判据：
  `cargo test -p yi-agent-app-server --lib side`、`cargo test -p yi-agent-app-server --lib close_all_`、
  `cargo test -p yi-agent-app-server --lib turning_the_flag_off_closes_every_live_side`。
- `yi-agent-tui.md`：`/side` 等四个命令在 flag 关时不入帮助目录、敲了报
  「本版本未启用旁问」。判据：`cargo test -p yi-agent --bin yi-agent help_`。
- `desktop.md`：设置页「实验功能」多出 `side_conversation`，关掉即清本地旁支
  视图。判据：`cd desktop && npx vitest run src/App.test.tsx`。

- [ ] **Step 2: 跑全量验证**

```bash
cd yi-agent-rs && cargo fmt --all -- --check && cargo clippy --all-targets --all-features -- -D warnings
cd yi-agent-rs && cargo test -p yi-agent-flags -p yi-agent-app-server
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent
cd desktop && npx tsc --noEmit && npx vitest run
```

Expected: 全绿

- [ ] **Step 3: 跑 `just ci`**

```bash
cd yi-agent-rs && just ci
```

Expected: exit 0（22 道门禁）

- [ ] **Step 4: 提交**

```bash
git add docs/project-management/
git commit -m "docs: record the side_conversation experiment"
```

---

## 人工验收（本计划完成后由用户执行）

这些是自动化测试覆盖不到的部分，**不要**在计划里写成假通过：

1. dev 构建启动桌面，设置 → 实验功能，确认多出 `side_conversation` 一行；
2. 打开一行旁支，回到设置页关掉开关：旁支视图应立刻消失并回到主会话，
   `ps` 里没有残留的 runner；
3. 再次打开开关，`/side` 仍可用（验证 `close_all` 没有把自己锁死）；
4. release 构建（`cargo build --release` 后跑 `yi-agent flags status side_conversation`）
   报 `source: build-kind`；TUI 里 `/side` 报「本版本未启用旁问」，`/help` 里
   没有旁问段。
