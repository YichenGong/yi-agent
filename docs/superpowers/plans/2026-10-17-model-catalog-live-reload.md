# 模型清单运行期热重载 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让已运行的 daemon 与 app-server 在 `models.json` 变更后按指纹自动采用新配置，无需重启进程。

**Architecture:** 在 `yi-agent-runtime` 新增严格读取 `try_load_catalog_from` 与指纹自更新句柄 `CatalogWatcher`；daemon 侧给 `DaemonAgentWorkerFactory` 加一个可选的「按指纹重建派生件」刷新器（`new()` 签名不变，零回归），app-server 侧把 `production_factory` 的清单快照换成 watcher。坏文件保留 last-known-good 且不推进指纹，故修好即生效。

**Tech Stack:** Rust（tokio / std `Mutex` / `SystemTime`）、cargo workspace；测试用 `tempfile`，wiremock 风格的既有 harness。

## Global Constraints

- **禁止在 `main` 上直接修改或提交**；本计划全部在 worktree `.worktrees/model-catalog-live-reload`（分支 `fix/model-catalog-live-reload`）内执行。
- Commit message 用 conventional commits；**不得**写 `Co-Authored-By` 行；首行 <= 72 字符。
- 提交前必须 `cd yi-agent-rs && cargo fmt --all`（hook 不会自动跑）。
- 测试按 crate 跑，**不要** `cargo test --workspace`（易 OOM/死锁）。跑前先 `ps aux | grep cargo` 确认无残留。
- **零回归硬约束**：`DaemonAgentWorkerFactory::new(...)` 签名与语义逐字节不变；未装刷新器的 factory 行为不变（`lib.rs` 21 处 + `tests/subagent_fork_e2e.rs:148` 共 22 处测试构造点必须全绿）。
- **敏感字段**：`api_key` 不得进入任何 `Debug` 输出、日志或落盘内容。`EffectiveKey` 刻意不派生 `Debug`。
- **不改** openai base_url 语义、不加隐式 `/v1`（spec §1.3）；**不覆盖** `.env`（spec §6.1）；**不改** `provider_profile_id()`（spec §6.2）。
- 既有宽松读取 `load_catalog_from` 的「绝不报错」语义保持不变，**只增不改**。

---

## File Structure

| 文件 | 职责 | 改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-runtime/src/models.rs` | 清单读取与本进程新鲜度 | 新增 `try_load_catalog_from`、`Fingerprint`、`CatalogWatcher` + 单测 |
| `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` | worker 工厂 | 新增 `WorkerRefresh`/`WorkerParts`/`EffectiveKey`、`with_catalog_watcher`；`start_with_provider_turn_gate` 取当前清单 |
| `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` | 工厂装配 | `build_worker_factory` 装 watcher；`models_path` 下传（已存在） |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | app-server 工厂 | `production_factory` 收 `Arc<CatalogWatcher>`；2 生产 + 10 测试调用点 |
| `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs` | ws 传输工厂 | `load_catalog()` → watcher |
| `docs/bug-list.md` | 缺陷登记 | 新增一条 `[x]` |
| `docs/project-management/yi-agent-runtime.md` 等 | 模块进度 | 同步 |

---

### Task 1: 严格读取 `try_load_catalog_from`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（在 `load_catalog_from` 之后，约 L168 处新增函数）
- Test: 同文件 `mod tests`（L291 起）

**Interfaces:**
- Consumes: 既有 `ModelCatalog`、`load_catalog_from` 的解析逻辑。
- Produces: `pub fn try_load_catalog_from(path: &Path) -> Result<ModelCatalog, String>`
  —— `Ok` = 可用清单（文件不存在也是 `Ok(空清单)`）；`Err(reason)` = 不可读/不可解析。

- [ ] **Step 1: 写失败测试**

在 `models.rs` 的 `mod tests` 内（`legacy_openai_default_url_is_migrated_to_v1` 之后）追加：

```rust
    /// 严格读取必须区分「文件不存在」（可判定的明确信号 → Ok 空清单）与
    /// 「不可读 / 不可解析」（→ Err），否则调用方无法实现「坏文件保留旧值」。
    #[test]
    fn try_load_distinguishes_absent_from_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");

        // 不存在：Ok 空清单（与 cfg 回退语义一致，不是错误）。
        let absent = try_load_catalog_from(&path).expect("absent file must be Ok(empty)");
        assert!(absent.models.is_empty());
        assert!(absent.default_model.is_none());

        // 合法内容：Ok，条目就位。
        std::fs::write(
            &path,
            r#"{"models":[{"name":"A","provider":"openai","api_url":"https://a/v1","model":"m","api_key":"k"}],"default_model":"A"}"#,
        )
        .unwrap();
        let good = try_load_catalog_from(&path).expect("valid file must be Ok");
        assert_eq!(good.default_model.as_deref(), Some("A"));

        // 坏 JSON：Err（这是「损坏」，绝不能被当成空清单）。
        std::fs::write(&path, "{ not json").unwrap();
        let corrupt = try_load_catalog_from(&path);
        assert!(
            corrupt.is_err(),
            "corrupt file must be Err, got {:?}",
            corrupt.map(|c| c.models.len())
        );

        // 合法但为空清单：Ok（合法空不等于损坏）。
        std::fs::write(&path, r#"{"models":[]}"#).unwrap();
        assert!(try_load_catalog_from(&path).is_ok());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib try_load_distinguishes_absent_from_corrupt`
Expected: 编译失败，报 `cannot find function 'try_load_catalog_from'`（RED）。

- [ ] **Step 3: 实现**

在 `models.rs` 中 `load_catalog_from` 函数定义之后新增（**复用**其解析体，避免两份解析逻辑）：

```rust
/// 严格读取清单：区分「文件不存在」与「不可读 / 不可解析」。
///
/// `Ok(空清单)` 表示「确定没有配置」（文件缺失是明确信号，调用方据此回退 `cfg`）；
/// `Err(reason)` 表示「有文件但读不出来 / 解析不了」——调用方据此保留上一次可用值，
/// 而不是把一次坏编辑当成「用户清空了配置」。
///
/// 既有的 [`load_catalog_from`] 保持「绝不报错」的容错语义（坏文件不阻断启动），
/// 两者是**互补**而非替代关系。
pub fn try_load_catalog_from(path: &Path) -> Result<ModelCatalog, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelCatalog::default())
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse_catalog(&text, path).map_err(|error| format!("invalid {}: {error}", path.display()))
}

/// 把清单文本解析为 [`ModelCatalog`]。`load_catalog_from`（宽松）与
/// `try_load_catalog_from`（严格）共用它，保证两份读取口径逐字节一致
/// （含 warning 日志字段，故**必须**接收 `path`）。
fn parse_catalog(text: &str, path: &Path) -> Result<ModelCatalog, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    let mut catalog = ModelCatalog::default();
    if let Some(models) = value.get("models").and_then(|v| v.as_array()) {
        for raw in models {
            match serde_json::from_value::<ModelEntry>(raw.clone()) {
                Ok(mut entry)
                    if !entry.name.trim().is_empty()
                        && !entry.api_url.trim().is_empty()
                        && !entry.model.trim().is_empty() =>
                {
                    migrate_openai_api_url(&mut entry);
                    catalog.models.push(entry);
                }
                Ok(_) => tracing::warn!(path = %path.display(), "skipping incomplete model entry"),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping malformed model entry")
                }
            }
        }
    }
    catalog.default_model = value
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog.subagent_model = value
        .get("subagent_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(catalog)
}
```

然后把**既有** `load_catalog_from` 的函数体改为复用 `parse_catalog`（保持它的宽松语义与签名不变）：

```rust
pub fn load_catalog_from(path: &Path) -> ModelCatalog {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelCatalog::default();
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not read models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    match parse_catalog(&text, path) {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "invalid models.json; treating as empty");
            ModelCatalog::default()
        }
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib try_load_distinguishes_absent_from_corrupt`
Expected: PASS（1 passed）。

再跑该 crate 全量，确认既有清单测试未被重构破坏：
Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib models`
Expected: 全绿（含 `legacy_openai_default_url_is_migrated_to_v1`）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/models.rs
git commit -m "feat(runtime): 新增严格清单读取 try_load_catalog_from"
```

---

### Task 2: `CatalogWatcher` 指纹自更新句柄

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/models.rs`（新增 `Fingerprint` + `CatalogWatcher`；文件顶部补 `use`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `try_load_catalog_from`。
- Produces:
  - `pub struct CatalogWatcher`
  - `pub fn CatalogWatcher::new(path: PathBuf) -> Arc<CatalogWatcher>`
  - `pub fn CatalogWatcher::from_catalog(path: PathBuf, catalog: ModelCatalog) -> Arc<CatalogWatcher>`
  - `pub fn CatalogWatcher::current(&self) -> Arc<ModelCatalog>`

- [ ] **Step 1: 写失败测试**

在 `models.rs` 的 `mod tests` 内追加（`std::sync::Arc` 已在测试内可用，若未 import 则写全路径 `std::sync::Arc`）：

```rust
    /// 指纹未变时必须复用同一个 `Arc`——用 `ptr_eq` 证明「零重读」，而不是
    /// 只看值相等（值相等无法区分「重读后恰好相同」与「根本没读」）。
    #[test]
    fn watcher_reuses_the_cached_catalog_when_the_fingerprint_is_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"A"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        let first = watcher.current();
        let second = watcher.current();
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "unchanged fingerprint must return the cached Arc, not a re-read"
        );
    }

    /// 指纹变化后必须读到新值（这是「配置改了进程要跟上」的最小断言）。
    #[test]
    fn watcher_picks_up_a_changed_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"OLD"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        assert_eq!(watcher.current().default_model.as_deref(), Some("OLD"));

        // 改 mtime + size 都变，确保指纹判定必然不同。
        std::fs::write(&path, r#"{"models":[],"default_model":"NEWER-VALUE"}"#).unwrap();
        assert_eq!(
            watcher.current().default_model.as_deref(),
            Some("NEWER-VALUE")
        );
    }

    /// 坏文件必须保留上一次可用值，**且不推进指纹**——这样文件修好后下一次
    /// `current()` 立即生效（本修复的核心行为）。
    #[test]
    fn watcher_keeps_last_known_good_across_a_corrupt_file_and_heals() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"GOOD"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        let good = watcher.current();
        assert_eq!(good.default_model.as_deref(), Some("GOOD"));

        // 坏文件：保留旧值（同一个 Arc），不 panic、不变空。
        std::fs::write(&path, "{ broken").unwrap();
        let during = watcher.current();
        assert!(
            std::sync::Arc::ptr_eq(&good, &during),
            "a corrupt file must keep the last known good catalog"
        );
        assert_eq!(during.default_model.as_deref(), Some("GOOD"));

        // 修好：下一次 current() 立即生效（不推进指纹的意义所在）。
        std::fs::write(&path, r#"{"models":[],"default_model":"HEALED"}"#).unwrap();
        assert_eq!(
            watcher.current().default_model.as_deref(),
            Some("HEALED"),
            "a fixed file must take effect on the very next call"
        );
    }

    /// 文件被删除是明确信号（不是损坏）：采纳空清单、回退 cfg。
    #[test]
    fn watcher_adopts_an_absent_file_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"X"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        assert_eq!(watcher.current().default_model.as_deref(), Some("X"));

        std::fs::remove_file(&path).unwrap();
        assert!(
            watcher.current().default_model.is_none(),
            "a deleted file means 'no configured catalog', not 'keep the old one'"
        );
    }

    /// `from_catalog` 是固定快照：即便磁盘变了也不重读（测试与「本就持有清单」的调用方用它）。
    #[test]
    fn from_catalog_is_a_fixed_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"DISK"}"#).unwrap();

        let fixed = ModelCatalog {
            models: Vec::new(),
            default_model: Some("PINNED".into()),
            subagent_model: None,
        };
        let watcher = CatalogWatcher::from_catalog(path, fixed);
        let a = watcher.current();
        let b = watcher.current();
        assert_eq!(a.default_model.as_deref(), Some("PINNED"));
        assert!(std::sync::Arc::ptr_eq(&a, &b));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib watcher_`
Expected: 编译失败，报 `cannot find struct 'CatalogWatcher'`（RED）。

- [ ] **Step 3: 实现**

在 `models.rs` 顶部的 `use` 区补：

```rust
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
```

（若 `use std::path::{Path, PathBuf};` 已在，保持不变。）

在 `try_load_catalog_from` 之后新增：

```rust
/// `models.json` 的指纹：`(mtime, size)`；`None` 的 mtime 表示文件不存在。
///
/// 用 stat 而非内容哈希：每次调用只是一次微秒级 syscall，而重建清单解析只在
/// 指纹真的变化后发生（见 `2026-10-17-model-catalog-live-reload-design.md` §6.3）。
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    mtime: Option<SystemTime>,
    size: u64,
}

fn fingerprint_of(path: &Path) -> Fingerprint {
    match std::fs::metadata(path) {
        Ok(meta) => Fingerprint {
            mtime: meta.modified().ok(),
            size: meta.len(),
        },
        Err(_) => Fingerprint {
            mtime: None,
            size: 0,
        },
    }
}

struct WatcherState {
    fingerprint: Fingerprint,
    catalog: Arc<ModelCatalog>,
}

/// 进程内「按指纹自更新的模型清单」句柄。
///
/// 谁使用清单、谁就负责它的新鲜度：daemon 的 worker 工厂与 app-server 的
/// 会话工厂都通过它取清单，于是运行期改 `models.json`（无论手工编辑、CLI/TUI
/// 写入还是设置页 RPC）都会在下一次用到时生效，无需重启进程。
///
/// 语义（与设计文档 §4.2 一致）：
/// - 指纹未变 → 直接返回缓存的 `Arc`（零解析）；
/// - 指纹变化且解析成功 → 换缓存 + 推进指纹；
/// - 指纹变化但解析失败（损坏 / 不可读）→ **保留旧值且不推进指纹**，于是文件
///   修好后下一次调用立即生效；
/// - 文件不存在 → 采纳空清单（「没有配置」是明确信号，不是错误）。
pub struct CatalogWatcher {
    path: PathBuf,
    /// `from_catalog` 造的固定快照：永不重读（见 `current`）。
    pinned: bool,
    state: Mutex<WatcherState>,
}

impl CatalogWatcher {
    /// 构造即读一次并记指纹。
    ///
    /// 首次读取用宽松版：启动期的坏文件不该让 daemon 起不来（与既有
    /// 「坏文件绝不阻断启动」一致）。
    pub fn new(path: PathBuf) -> Arc<Self> {
        let catalog = Arc::new(load_catalog_from(&path));
        Arc::new(Self {
            state: Mutex::new(WatcherState {
                fingerprint: fingerprint_of(&path),
                catalog,
            }),
            path,
            pinned: false,
        })
    }

    /// 固定快照：永不重读。测试与「本就持有清单」的调用方用它。
    pub fn from_catalog(path: PathBuf, catalog: ModelCatalog) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WatcherState {
                fingerprint: fingerprint_of(&path),
                catalog: Arc::new(catalog),
            }),
            path,
            pinned: true,
        })
    }

    /// 返回当前清单：指纹变了才重读，且只在解析成功时推进指纹。
    pub fn current(&self) -> Arc<ModelCatalog> {
        // 固定快照（`from_catalog`）：无论磁盘怎么变都返回构造函数给的那份。
        if self.pinned {
            return Arc::clone(&self.state.lock().unwrap_or_else(|p| p.into_inner()).catalog);
        }
        let observed = fingerprint_of(&self.path);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if observed == state.fingerprint {
            return Arc::clone(&state.catalog);
        }
        match try_load_catalog_from(&self.path) {
            Ok(catalog) => {
                state.fingerprint = observed;
                state.catalog = Arc::new(catalog);
                Arc::clone(&state.catalog)
            }
            Err(reason) => {
                // 保留 last-known-good，且**不推进指纹**：文件改好后下一次调用即生效。
                tracing::warn!(
                    path = %self.path.display(),
                    %reason,
                    "model catalog is unreadable; keeping the last known good catalog"
                );
                Arc::clone(&state.catalog)
            }
        }
    }
}
```

> `from_catalog` 用独立的 `pinned: bool` 表达「固定快照」：`current()` 见到它
> 就直接返回构造时给的 `Arc`，与磁盘指纹无关。**不要**用「构造一个永不匹配的
> 指纹」来表达固定——`current()` 的规则是「指纹不同就重读」，永不匹配恰恰会
> 导致每次都重读（计划初稿的注释与代码在此自相矛盾，测试为准）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib watcher_`（4 passed）
再 Run: `cd yi-agent-rs && cargo test -p yi-agent-runtime --lib from_catalog_is_a_fixed_snapshot`（1 passed）
Expected: PASS（合计 5 passed）。cargo 只接受单个 TESTNAME 过滤词，两条命令分别跑。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/models.rs
git commit -m "feat(runtime): 新增 CatalogWatcher 按指纹自更新清单句柄"
```

---

### Task 3: daemon 侧按指纹重建派生件

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`（结构体字段 ~L50-70、`new` ~L73、`worker_config_model` 附近新增 builder、`start_with_provider_turn_gate` ~L559-590）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs` 的 `mod tests`（L2067 起，模块内已有 `DaemonAgentWorkerFactory::new` 用法可参照）

**Interfaces:**
- Consumes: Task 2 的 `yi_agent_runtime::models::CatalogWatcher`（`Arc<CatalogWatcher>::current() -> Arc<ModelCatalog>`）；既有 `resolve_effective`、`build_provider`、`build_prompt_setup`、`build_agent_config`。
- Produces:
  - `pub fn DaemonAgentWorkerFactory::with_catalog_watcher(self, cfg: RuntimeConfig, watcher: Arc<CatalogWatcher>) -> Self`
  - 内部 `struct WorkerRefresh` / `struct WorkerParts` / `struct EffectiveKey` + `fn WorkerParts::rebuild(cfg, catalog) -> anyhow::Result<WorkerParts>` + `fn EffectiveKey::of(cfg: &RuntimeConfig) -> EffectiveKey`

- [ ] **Step 1: 写失败测试**

在 `lib.rs` 的 `mod tests` 内追加（构造风格照同模块既有的 `DaemonAgentWorkerFactory::new`（L2426 起）与 `AgentConfig::default()`；`RuntimeConfig` **没有** `Default`，须逐字段写出——照 `attach.rs:438` 的 `base_config`）：

```rust
    /// 测试用 `RuntimeConfig`：`RuntimeConfig` 无 `Default`，逐字段写出。
    /// 字段集与 `attach.rs` 的 `base_config` 同构。
    fn watcher_test_config(workdir: &std::path::Path) -> yi_agent_runtime::config::RuntimeConfig {
        yi_agent_runtime::config::RuntimeConfig {
            provider: "openai".into(),
            api_url: "http://127.0.0.1:9/v1".into(),
            api_key: "k".into(),
            model: "base-model".into(),
            api_style: yi_agent_core::ApiStyle::Auto,
            reasoning_effort: None,
            max_turns: 4,
            max_resident_subagents: 8,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: workdir.to_path_buf(),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: true,
        }
    }

    /// 装了 watcher 的工厂必须在下一次调用用上新清单：改 `models.json` 的
    /// `subagent_model` 指向另一条条目后，解析出的模型必须跟着变。
    /// 这是「daemon 不再冻结启动快照」的最小可观测断言。
    #[test]
    fn a_watcher_backed_factory_follows_a_changed_subagent_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let catalog = |subagent: &str| {
            format!(
                r#"{{"models":[
                    {{"name":"S1","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-one","api_key":"k"}},
                    {{"name":"S2","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-two","api_key":"k"}}
                ],"subagent_model":"{subagent}"}}"#
            )
        };
        std::fs::write(&path, catalog("S1")).unwrap();

        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
            dir.path().join("runtime.sock"),
        )
        .with_catalog_watcher(
            watcher_test_config(dir.path()),
            yi_agent_runtime::models::CatalogWatcher::new(path.clone()),
        );

        assert_eq!(factory.resolved_model_for_test(), "model-one");

        std::fs::write(&path, catalog("S2")).unwrap();

        assert_eq!(
            factory.resolved_model_for_test(),
            "model-two",
            "the next call must use the new catalog, not the startup snapshot"
        );
    }

    /// 未装 watcher 的工厂行为必须逐字节不变（零回归门禁）。
    #[test]
    fn a_factory_without_a_watcher_keeps_its_static_config() {
        let dir = tempfile::TempDir::new().unwrap();
        let factory = DaemonAgentWorkerFactory::new(
            Arc::new(RecordingProvider::default()),
            Arc::new(ToolRegistry::new()),
            AgentConfig {
                model: "static-model".into(),
                ..AgentConfig::default()
            },
            dir.path().join("runtime.sock"),
        );
        assert_eq!(factory.worker_config_model("ignored"), "static-model");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_watcher_backed_factory_follows_a_changed_subagent_model`
Expected: 编译失败，报 `no method named 'with_catalog_watcher'`（RED）。

- [ ] **Step 3: 实现**

3a. 在 `lib.rs` 顶部 `use` 区补（按既有 import 风格）：

```rust
use yi_agent_runtime::models::CatalogWatcher;
```

3b. 在 `DaemonAgentWorkerFactory` 结构体（L50 起）末尾加字段：

```rust
    /// 装了它才按指纹重建派生件；未装则完全沿用 `new()` 传入的静态字段
    /// （全部测试构造点因此行为不变）。
    refresh: Option<WorkerRefresh>,
```

并在 `new()` 的初始化列表里加 `refresh: None,`。

3c. 在 `impl DaemonAgentWorkerFactory` 内新增：

```rust
/// 由 `effective` 派生、供 worker 启动使用的一组对象。
///
/// 与 `DaemonAgentWorkerFactory` 的静态字段一一对应：装刷新器后它们由
/// 当前清单重建，未装时仍用 `new()` 传入的那份。
struct WorkerParts {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    config: AgentConfig,
    sandbox: yi_agent_tools::SandboxMode,
    sandbox_writable_roots: Vec<PathBuf>,
}

impl WorkerParts {
    /// 从清单解析出 `effective` 并重建全部派生件。
    fn rebuild(
        cfg: &yi_agent_runtime::config::RuntimeConfig,
        catalog: &yi_agent_runtime::models::ModelCatalog,
    ) -> anyhow::Result<Self> {
        let effective = yi_agent_runtime::models::resolve_effective(
            cfg,
            catalog,
            catalog.subagent_model.as_deref(),
        );
        let provider = yi_agent_runtime::bootstrap::build_provider(&effective)?;
        let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(&effective)?;
        let mut registry = ToolRegistry::new();
        if let Some(skills) = &prompt.skills {
            registry.register(Arc::new(yi_agent_tools::SkillTool::new(skills.clone())));
        }
        let config = yi_agent_runtime::bootstrap::build_agent_config(&effective, prompt.system_prompt);
        Ok(Self {
            provider,
            tools: Arc::new(registry),
            config,
            sandbox: effective.sandbox,
            sandbox_writable_roots: effective.sandbox_writable_roots.clone(),
        })
    }
}

/// 决定派生件是否需要重建的键：`resolve_effective` 覆盖的 6 个字段全等即命中。
///
/// 刻意**不派生 `Debug`**：`api_key` 是敏感字段，不得进入任何可打印位置。
#[derive(PartialEq, Eq)]
struct EffectiveKey {
    provider: String,
    api_url: String,
    api_key: String,
    model: String,
    api_style: yi_agent_core::ApiStyle,
    reasoning_effort: Option<String>,
}

impl EffectiveKey {
    fn of(cfg: &yi_agent_runtime::config::RuntimeConfig) -> Self {
        Self {
            provider: cfg.provider.clone(),
            api_url: cfg.api_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            api_style: cfg.api_style.clone(),
            reasoning_effort: cfg.reasoning_effort.clone(),
        }
    }
}

/// 按指纹重建派生件所需的一切。
struct WorkerRefresh {
    cfg: yi_agent_runtime::config::RuntimeConfig,
    watcher: Arc<CatalogWatcher>,
    cache: std::sync::Mutex<Option<(EffectiveKey, Arc<WorkerParts>)>>,
}

impl WorkerRefresh {
    /// 取当前清单解析出的派生件；键命中则复用缓存。
    ///
    /// 重建失败（例如新条目 provider 名非法）时保留上一次可用派生件：
    /// 一次坏编辑不该当场打断所有委派（与设计文档 §5 的降级策略一致）。
    fn parts(&self) -> Option<Arc<WorkerParts>> {
        let catalog = self.watcher.current();
        let effective = yi_agent_runtime::models::resolve_effective(
            &self.cfg,
            &catalog,
            catalog.subagent_model.as_deref(),
        );
        let key = EffectiveKey::of(&effective);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((cached_key, parts)) = cache.as_ref() {
            if *cached_key == key {
                return Some(Arc::clone(parts));
            }
        }
        match WorkerParts::rebuild(&self.cfg, &catalog) {
            Ok(parts) => {
                let parts = Arc::new(parts);
                *cache = Some((key, Arc::clone(&parts)));
                Some(parts)
            }
            Err(error) => {
                tracing::warn!(%error, "could not rebuild worker parts from the current catalog; keeping the previous ones");
                cache.as_ref().map(|(_, parts)| Arc::clone(parts))
            }
        }
    }
}
```

3d. 在 builder 区（`with_sandbox` 附近）新增：

```rust
    /// 让工厂按指纹跟随 `models.json`：`cfg` 是 `resolve_effective` 的基准，
    /// `watcher` 提供当前清单。未调用它时工厂完全静态（零回归）。
    pub fn with_catalog_watcher(
        mut self,
        cfg: yi_agent_runtime::config::RuntimeConfig,
        watcher: Arc<CatalogWatcher>,
    ) -> Self {
        self.refresh = Some(WorkerRefresh {
            cfg,
            watcher,
            cache: std::sync::Mutex::new(None),
        });
        self
    }

    /// 测试接缝：当前清单解析出的模型名。未装刷新器时回落静态配置。
    #[cfg(test)]
    fn resolved_model_for_test(&self) -> String {
        match &self.refresh {
            Some(refresh) => refresh
                .parts()
                .map(|parts| parts.config.model.clone())
                .unwrap_or_else(|| self.config.model.clone()),
            None => self.config.model.clone(),
        }
    }
```

3e. 改 `start_with_provider_turn_gate`（L559 起）的取件方式。把开头：

```rust
        let provider = Arc::clone(&self.provider);
```

改为（保持 `provide` 的后续用法不变）：

```rust
        // 装了刷新器就用「当前清单」重建出的派生件；否则用 `new()` 传入的静态字段。
        let refreshed = self.refresh.as_ref().and_then(|refresh| refresh.parts());
        let provider = refreshed
            .as_ref()
            .map(|parts| Arc::clone(&parts.provider))
            .unwrap_or_else(|| Arc::clone(&self.provider));
```

同函数内 `effective_sandbox` 的计算，把 `self.sandbox` / `self.config` 的使用改为优先取 `refreshed`：

```rust
        let effective_sandbox = match workspace_mode {
            ChildWriteMode::Coding => resolve_effective_sandbox(
                refreshed.as_ref().map_or(self.sandbox, |parts| parts.sandbox),
                request.inherited_sandbox,
            ),
            ChildWriteMode::ReadOnly => yi_agent_tools::SandboxMode::ReadOnly,
        };
        let worker_tools = Arc::new(match refreshed.as_ref() {
            Some(parts) => {
                // 用刷新后的工具集重构 workspace 工具：复用既有的注册路径。
                self.worker_tool_registry_with_base(&workspace, workspace_mode, request.inherited_sandbox, Arc::clone(&parts.tools))
            }
            None => self.worker_tool_registry(&workspace, workspace_mode, request.inherited_sandbox),
        });
        let mut config = refreshed
            .as_ref()
            .map(|parts| parts.config.clone())
            .unwrap_or_else(|| self.config.clone());
        // 未装刷新器时才走静态取值：有刷新器时 `parts.config.model` 已是当前
        // 清单解析出的模型，不能被静态配置覆盖回去。
        if refreshed.is_none() {
            config.model = self.worker_config_model(&request.model);
        }
```

其中新增一个以「基准工具集」为准的注册辅助（把既有 `worker_tool_registry` 改为它的薄封装，**保持既有方法签名与行为不变**）：

```rust
    fn worker_tool_registry(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: ChildWriteMode,
        inherited: Option<yi_agent_core::InheritedSandbox>,
    ) -> ToolRegistry {
        self.worker_tool_registry_with_base(workspace, workspace_mode, inherited, Arc::clone(&self.tools))
    }

    /// 与 [`Self::worker_tool_registry`] 同逻辑，但基准工具集由调用方给出
    /// （刷新路径传入当前清单重建出的那份）。
    fn worker_tool_registry_with_base(
        &self,
        workspace: &WorkerWorkspace,
        workspace_mode: ChildWriteMode,
        inherited: Option<yi_agent_core::InheritedSandbox>,
        base: Arc<ToolRegistry>,
    ) -> ToolRegistry {
        let mut tools = (*base).clone();
        // …以下与既有 `worker_tool_registry` 主体逐字相同…
    }
```

3f. 若 `resolve_effective_sandbox` 的 `workspace` 分支还用到 `self.sandbox_writable_roots`（既有代码在别处使用），一并按 `refreshed` 优先取值。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib a_watcher_backed_factory_follows_a_changed_subagent_model a_factory_without_a_watcher_keeps_its_static_config`
Expected: PASS（2 passed）。

再跑该 crate 全量，确认 22 处既有构造点未受影响：
Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-subagent/src/lib.rs
git commit -m "feat(subagent): worker 工厂按指纹跟随模型清单"
```

---

### Task 4: 装配 daemon 的刷新器

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`（`worker_factory` ~L93 / `build_worker_factory` ~L110）
- Test: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs` 的 `mod tests`（L438 起）

**Interfaces:**
- Consumes: Task 3 的 `with_catalog_watcher(cfg, watcher)`；Task 2 的 `CatalogWatcher::new(path)`。
- Produces: 装配后的 daemon 工厂（`worker_factory` 签名不变，行为变为「按指纹跟随清单」）。

- [ ] **Step 1: 写失败测试**

在 `attach.rs` 的 `mod tests` 内追加：

```rust
    /// 生产装配路径必须装上刷新器：改 `models.json` 的 `subagent_model` 后，
    /// 工厂解析出的模型必须跟着变（这是 daemon 侧对本次事故的直接回归门禁）。
    #[test]
    fn the_attached_worker_factory_follows_a_changed_catalog() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = catalog_file(
            directory.path(),
            r#"{"models":[
                {"name":"S1","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-one","api_key":"k"},
                {"name":"S2","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-two","api_key":"k"}
            ],"subagent_model":"S1"}"#,
        );
        let cfg = base_config(directory.path());
        let socket = directory.path().join("runtime.sock");

        // 走生产装配路径 `build_worker_factory`（返回具体类型，便于断言）。
        let factory = super::build_worker_factory(
            &cfg,
            socket,
            &yi_agent_runtime::models::load_catalog_from(&path),
            &path,
        )
        .expect("factory");
        assert_eq!(
            factory.resolved_model_for_test(),
            "model-one",
            "the factory must start from the on-disk catalog"
        );

        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"S1","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-one","api_key":"k"},
                {"name":"S2","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-two","api_key":"k"}
            ],"subagent_model":"S2"}"#,
        )
        .unwrap();

        assert_eq!(
            factory.resolved_model_for_test(),
            "model-two",
            "changing models.json must take effect without restarting the daemon"
        );
    }

    /// 坏清单不得让工厂崩掉或清空配置：保留上一次可用值。
    #[test]
    fn the_attached_worker_factory_survives_a_corrupt_catalog() {
        let directory = tempfile::TempDir::new().unwrap();
        let path = catalog_file(
            directory.path(),
            r#"{"models":[{"name":"S1","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-one","api_key":"k"}],"subagent_model":"S1"}"#,
        );
        let cfg = base_config(directory.path());
        let factory = super::build_worker_factory(
            &cfg,
            directory.path().join("runtime.sock"),
            &yi_agent_runtime::models::load_catalog_from(&path),
            &path,
        )
        .expect("factory");
        assert_eq!(factory.resolved_model_for_test(), "model-one");

        std::fs::write(&path, "{ broken").unwrap();
        assert_eq!(
            factory.resolved_model_for_test(),
            "model-one",
            "a corrupt catalog must keep the last known good model"
        );

        std::fs::write(
            &path,
            r#"{"models":[{"name":"S1","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"healed","api_key":"k"}],"subagent_model":"S1"}"#,
        )
        .unwrap();
        assert_eq!(
            factory.resolved_model_for_test(),
            "healed",
            "a fixed catalog must take effect on the next call"
        );
    }
```

> `worker_factory` 返回 `Arc<dyn AgentWorkerFactory>`，测试拿不到具体类型。**做法**：测试直接
> 调同 crate 的私有 `super::build_worker_factory(...)`（返回具体 `DaemonAgentWorkerFactory`），
> 再调其 `#[cfg(test)] resolved_model_for_test()`——**不**给 core 的生产 trait 加测试方法。
> `build_worker_factory` 需在 Task 3 完成后带上 `models_path` 形参（见本任务 Step 3）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent --lib the_attached_worker_factory_follows_a_changed_catalog`
Expected: RED（刷新器尚未装配，第二次断言拿到 `model-one`）。

- [ ] **Step 3: 实现**

`attach.rs` 的 `build_worker_factory` 增加 `models_path` 参数并装刷新器：

```rust
fn build_worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
    catalog: &yi_agent_runtime::models::ModelCatalog,
    models_path: &Path,
) -> anyhow::Result<crate::DaemonAgentWorkerFactory> {
    // …既有解析与构造逐字不变，直到最后的 builder 链…
    Ok(crate::DaemonAgentWorkerFactory::new(
        provider,
        Arc::new(registry),
        agent_config,
        runtime_socket,
    )
    .with_catalog(catalog)
    .with_sandbox(effective.sandbox, effective.sandbox_writable_roots.clone())
    .with_workspace(effective.workdir.clone())
    .with_max_resident_subagents(effective.max_resident_subagents)
    .with_max_direct_children(usize::from(effective.max_direct_children))
    // 本次新增：装刷新器，使 daemon 跟随 models.json 的运行期变更。
    .with_catalog_watcher(
        cfg.clone(),
        yi_agent_runtime::models::CatalogWatcher::new(models_path.to_path_buf()),
    ))
}
```

`worker_factory` 透传 `models_path`：

```rust
pub fn worker_factory(
    cfg: &RuntimeConfig,
    runtime_socket: PathBuf,
    models_path: &Path,
) -> anyhow::Result<std::sync::Arc<dyn AgentWorkerFactory>> {
    Ok(std::sync::Arc::new(build_worker_factory(
        cfg,
        runtime_socket,
        &yi_agent_runtime::models::load_catalog_from(models_path),
        models_path,
    )?))
}
```

`worker_factory_with_catalog`（`#[cfg(test)]`，L158）同样补 `models_path` 形参并透传。
`attach_project_runtime`（L192）的调用已是 `worker_factory(cfg, socket, models_path)`，**无需改**。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-subagent`
Expected: 全绿（含既有 `the_worker_uses_the_subagent_model_entry` 等）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-subagent/src/attach.rs
git commit -m "feat(subagent): daemon 装配清单刷新器"
```

---

### Task 5: app-server 工厂改用 watcher

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`production_factory` ~L1628；生产点 `:1421`/`:1494`；测试点 10 处）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs`（`:59`）
- Test: `server.rs` 的 `mod tests`（新增一条；并沿用既有 `test_catalog()` 辅助）

**Interfaces:**
- Consumes: Task 2 的 `CatalogWatcher`（`new` / `from_catalog` / `current`）。
- Produces: `production_factory(cfg: RuntimeConfig, theme: ThemeHandle, catalog: Arc<CatalogWatcher>) -> impl Fn(...)`。

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 `mod tests` 内追加（沿用既有 `test_catalog()` / `Harness` 风格；`test_catalog` 是清单辅助，这里是 watcher 版本）：

```rust
    /// 运行期改 `models.json` 后，**新建** thread 必须用上新条目——这是
    /// app-server 侧「工厂不再持启动快照」的直接断言。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_thread_started_after_a_catalog_change_uses_the_new_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let models_path = dir.path().join("models.json");
        std::fs::write(
            &models_path,
            r#"{"models":[
                {"name":"A","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-a","api_key":"k"},
                {"name":"B","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-b","api_key":"k"}
            ],"default_model":"A"}"#,
        )
        .unwrap();

        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        cfg.provider = "openai".to_string();
        cfg.api_key = "k".to_string();

        let watcher = yi_agent_runtime::models::CatalogWatcher::new(models_path.clone());
        let factory = production_factory(cfg.clone(), test_theme(), Arc::clone(&watcher));
        let mut h = Harness::with_config(cfg, factory, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        // 第一个 thread：走默认条目 A。`Harness` 无 `read_until_id`，用既有
        // 「循环 read_value 直到 id 匹配」的写法（见 server.rs:20036 的既有测试）。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let first = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                break v;
            }
        };
        assert_eq!(
            first["result"]["model"].as_str(),
            Some("model-a"),
            "the first thread must run the entry present at startup"
        );

        // 运行期换默认条目（改 mtime + size），**不重启 app-server**。
        std::fs::write(
            &models_path,
            r#"{"models":[
                {"name":"A","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-a","api_key":"k"},
                {"name":"B","provider":"openai","api_url":"http://127.0.0.1:9/v1","model":"model-b","api_key":"k"}
            ],"default_model":"B"}"#,
        )
        .unwrap();

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let second = loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                break v;
            }
        };
        assert_eq!(
            second["result"]["model"].as_str(),
            Some("model-b"),
            "a thread started after the change must use the new default entry"
        );
        h.shutdown().await;
    }
```

> 若 `Harness` 没有 `read_until_id`（本仓库确实没有），用既有测试的「循环 `read_value` 直到
> `id` 匹配」写法（参见 `thread_start_and_resume_report_the_built_agents_model`，`server.rs:20036`）。
> `test_catalog()` 返回的是 `Arc<ModelCatalog>`（`server.rs:21026`），传参时按此取用。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_thread_started_after_a_catalog_change_uses_the_new_entry`
Expected: RED（第二条断言拿到 `model-a`，因为工厂持启动快照）。

- [ ] **Step 3: 实现**

3a. `production_factory` 签名与取清单方式：

```rust
pub(crate) fn production_factory(
    cfg: RuntimeConfig,
    theme: crate::theme_tool::ThemeHandle,
    catalog: Arc<yi_agent_runtime::models::CatalogWatcher>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let mut thread_cfg = cfg.clone();
        thread_cfg.workdir = cwd.to_path_buf();
        thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
        let model_ref = current_session_model_ref();
        // 取「当前」清单：运行期新增/修改的条目立刻可见（原先持启动快照）。
        let catalog = catalog.current();
        let mut thread_cfg = yi_agent_runtime::models::resolve_effective(
            &thread_cfg,
            &catalog,
            model_ref.as_deref(),
        );
        // …以下逐字不变…
    }
}
```

3b. 生产调用点 `server.rs:1421`：

```rust
    let catalog = yi_agent_runtime::models::CatalogWatcher::new(
        yi_agent_runtime::models::models_path(),
    );
```

`server.rs:1494` 同改。`ws.rs:59` 同改（并把 `:60` 传入的 `catalog` 保持不变）。

3c. **测试调用点**（10 处）改用固定快照，保持既有语义：

```rust
// 原: production_factory(cfg, test_theme(), test_catalog())
// 新:
production_factory(cfg, test_theme(), Arc::new(yi_agent_runtime::models::CatalogWatcher::from_catalog(
    std::path::PathBuf::new(),
    test_catalog(),
)))
```

为避免 10 处重复长表达式，在 `mod tests` 加一个辅助（`test_catalog()` 与
`Arc::new(catalog)` 两种实参形态统一收 `Arc<ModelCatalog>`）：

```rust
    /// 固定快照 watcher：测试要的是「注入一份清单」，不是「跟随磁盘」。
    fn fixed_watcher(
        catalog: Arc<yi_agent_runtime::models::ModelCatalog>,
    ) -> Arc<yi_agent_runtime::models::CatalogWatcher> {
        Arc::new(yi_agent_runtime::models::CatalogWatcher::from_catalog(
            std::path::PathBuf::new(),
            (*catalog).clone(),
        ))
    }
```

然后把 `test_catalog()` / `test_catalog_without_default()` / `Arc::new(catalog)` 三种实参形态
分别改为 `fixed_watcher(test_catalog())` / `fixed_watcher(test_catalog_without_default())` /
`fixed_watcher(Arc::new(catalog))`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib a_thread_started_after_a_catalog_change_uses_the_new_entry`
Expected: PASS。

再跑该 crate 全量（既有 10 处工厂测试必须全绿）：
Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/ws.rs
git commit -m "feat(app-server): 会话工厂按指纹跟随模型清单"
```

---

### Task 6: 真机端到端验证（决定性判据）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/model_catalog_live_reload_e2e.rs`
- 参照: `yi-agent-rs/crates/yi-agent/tests/subagent_real_e2e.rs`（隔离 daemon 起法，`YI_AGENT_RUNTIME_DIR`）

**Interfaces:**
- Consumes: Task 3/4 的 daemon 刷新行为；`yi_agent_store::ipc::{IpcRequest, send_request}`。
- Produces: 一条 `#[ignore]` 的真机测试（默认跳过，需 `--ignored`）。

> **实现时发现并修正的 4 个判据失效点**（计划初稿的代码片段按下述修正；测试文件是权威版本）：
> 1. root 侧凭据必须走 CLI 且带 `/v1`（root 的 provider 来自 env/CLI，与清单无关），
>    否则第 2 步会把 root 一起打死、跑不到 worker 失败；
> 2. `run` 必须带 `--subagents`，否则 headless root 不注册委派工具；
> 3. 子任务按 `depth == 1` 且**排除已有 task id** 选取：daemon 跨次保留历史任务，
>    否则第 2 步立刻读到第 1 步遗留的子任务终态；
> 4. 404 属不可重试错误 → `report_failure` → 任务落 `failed`（不是卡在 `running`），
>    故第 2 步可稳定观测到 `failed`。

- [ ] **Step 1: 写测试（隔离 daemon + 三步判据）**

```rust
//! 真机 e2e：改 `models.json` 后 daemon 必须不重启即跟上。
//!
//! 三步判据（对照 2026-10-17 的现场事故：旧 daemon 在配置修好后仍 0 成功 / 120 次 404）：
//!   1. 清单可用 → 委派成功；
//!   2. 改成不可用 → 委派失败（**证明确实重读了**，不是缓存侥幸）；
//!   3. 改回可用 → 委派再次成功（**证明修好即生效**，无需重启 daemon）。

mod common;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

struct Fixture {
    _root: tempfile::TempDir,
    runtime_dir: PathBuf,
    models_path: PathBuf,
    daemon: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Fixture {
    fn write_catalog(&self, api_url: &str) {
        std::fs::write(
            &self.models_path,
            format!(
                r#"{{"models":[{{"name":"S","provider":"openai","api_url":"{api_url}","model":"HORIZON-DeepSeek","api_key":"{}"}}],"subagent_model":"S"}}"#,
                std::env::var("MODEL_API_KEY").unwrap_or_default()
            ),
        )
        .unwrap();
    }
}

/// 起一个隔离 daemon；无 API key 时返回 `None`（测试据此安全 skip）。
///
/// 关键隔离点（照 `subagent_real_e2e.rs:31` `start_real_runtime_fixture` 的结构）：
/// - `YI_AGENT_RUNTIME_DIR` → 隔离 runtime 目录，绝不碰真实项目 runtime；
/// - `HOME` → 隔离目录，于是 `models_path()`（`$HOME/.yi-agent/models.json`）与
///   `load_env_files` 都落在这个临时 HOME 下，**不改用户真实清单**。
fn start_fixture() -> Option<Fixture> {
    let api_key = std::env::var("MODEL_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())?;

    let root = tempfile::TempDir::new().expect("fixture tempdir");
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".yi-agent")).unwrap();
    let repository = root.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    // 委派需要 git 工作区（照 e2e 基线，非 git 目录会退化）。
    let _ = Command::new("git").args(["init", "-q"]).current_dir(&repository).status();

    let runtime_dir = root.path().join("runtime");
    let models_path = home.join(".yi-agent/models.json");

    let mut command = Command::new(common::yi_agent_bin());
    command
        .arg("--workdir")
        .arg(&repository)
        .arg("--provider")
        .arg("openai")
        .arg("--api-url")
        .arg("https://llmapi.horizon.auto/v1")
        .arg("--api-key")
        .arg(&api_key)
        .arg("--model")
        .arg("HORIZON-DeepSeek")
        .arg("daemon")
        .arg("serve")
        .env("YI_AGENT_RUNTIME_DIR", &runtime_dir)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let daemon = command.spawn().expect("start isolated runtime daemon");

    // 等 socket 就绪（与 `subagent_real_e2e.rs` 同法）。
    let socket = runtime_dir.join("runtime.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(
            send_request(&socket, IpcRequest::Status),
            Ok(IpcResponse::Status { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "isolated runtime daemon did not become ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let fixture = Fixture {
        _root: root,
        runtime_dir,
        models_path,
        daemon,
    };
    // 起点必须是可用清单。
    fixture.write_catalog("https://llmapi.horizon.auto/v1");
    Some(fixture)
}

/// 委派一次并返回**子任务**终态字符串（`failed` / `completed*`）。
///
/// 读状态走 IPC（`ListTaskSummaries` + `InspectTask`），与
/// `subagent_real_e2e.rs:169` 的 `task_snapshot` 同法——**不需要** `rusqlite`
/// （本 crate 无该依赖，也不该为测试新增）。
fn delegate_once(fixture: &Fixture) -> String {
    let socket = fixture.runtime_dir.join("runtime.sock");
    let before: Vec<String> = task_ids(&socket);

    // 以同一个隔离 daemon 为目标发起一次真实委派。
    let out = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(fixture.repository())
        .arg("--yolo")
        .args(["run", "delegate a trivial subtask and report back"])
        .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir)
        .env("HOME", fixture.home())
        .output()
        .expect("run delegation");
    assert!(
        out.status.success() || !out.stdout.is_empty(),
        "delegation run must complete: {out:?}"
    );

    // 等新任务出现并落到终态。
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        for task_id in task_ids(&socket) {
            if before.contains(&task_id) {
                continue;
            }
            if let Ok(IpcResponse::TaskDetail(detail)) =
                send_request(&socket, IpcRequest::InspectTask { task_id })
            {
                if detail.state == "failed" || detail.state.starts_with("completed") {
                    return detail.state;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "the delegated task never reached a terminal state"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 当前 daemon 的全部 task id（非 root 的也在内）。
fn task_ids(socket: &std::path::Path) -> Vec<String> {
    match send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    ) {
        Ok(IpcResponse::TaskSummaries { tasks }) => tasks.into_iter().map(|t| t.task_id).collect(),
        _ => Vec::new(),
    }
}
```

`Fixture` 另需两个访问器（`repository()` / `home()` 返回隔离路径），照上面结构补：

```rust
impl Fixture {
    fn home(&self) -> PathBuf {
        self._root.path().join("home")
    }
    fn repository(&self) -> PathBuf {
        self._root.path().join("repository")
    }
}
```

- [ ] **Step 1c: 三步判据主体**

```rust
/// 真机三步判据。`#[ignore]`：默认跳过，CI 不跑（避免成本与密钥依赖）。
#[test]
#[ignore]
fn changing_models_json_takes_effect_without_restarting_the_daemon() {
    let Some(fixture) = start_fixture() else {
        eprintln!("skip: MODEL_API_KEY is not set");
        return;
    };

    // 1. 清单可用 → 委派成功。
    let first = delegate_once(&fixture);
    assert!(
        first.contains("completed"),
        "step 1: a usable catalog must let the delegation succeed, got {first}"
    );

    // 2. 改成不可用（同一 daemon 进程，绝不重启）→ 委派失败。
    fixture.write_catalog("https://llmapi.horizon.auto");
    let second = delegate_once(&fixture);
    assert!(
        second.contains("failed"),
        "step 2: a bad catalog must make the next delegation fail, got {second} \
         (this proves the daemon re-read the file rather than serving a stale cache)"
    );

    // 3. 改回可用 → 委派再次成功。本次事故正是卡在这里。
    fixture.write_catalog("https://llmapi.horizon.auto/v1");
    let third = delegate_once(&fixture);
    assert!(
        third.contains("completed"),
        "step 3: fixing the catalog must take effect on the next delegation, got {third}"
    );
}
```

- [ ] **Step 2: 无 key 时确认安全跳过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test model_catalog_live_reload_e2e`
Expected: PASS（0 个测试运行，`#[ignore]` 生效）。

- [ ] **Step 3: 有 key 时跑三步判据**

Run: `cd yi-agent-rs && cargo test -p yi-agent --test model_catalog_live_reload_e2e -- --ignored --nocapture`
Expected: 三步全绿：成功 → 失败 → 成功。**且第 3 步必须成功**（本次事故正是这里卡住）。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent/tests/model_catalog_live_reload_e2e.rs
git commit -m "test(e2e): 模型清单热重载真机三步判据"
```

---

### Task 7: 文档登记与全量回归

**Files:**
- Modify: `docs/bug-list.md`（新增 `[x]` 一条）
- Modify: `docs/project-management/README.md` 及对应模块文件（按 CLAUDE.md 同步）

**Interfaces:**
- Consumes: Task 1-6 的全部产出与实测数据。
- Produces: 可追溯的缺陷登记与进度同步。

- [ ] **Step 1: 全量回归**

```bash
ps aux | grep -v grep | grep -E "cargo|rustc|yi_agent" || echo "clean"
cd yi-agent-rs
cargo test -p yi-agent-runtime --lib
cargo test -p yi-agent-subagent
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
Expected: 全绿；fmt/clippy 干净。

- [ ] **Step 2: 登记 `bug-list.md`**

按该文件既有格式追加一条 `[x]`（一段式：症状 → 根因 → 修复 → 验证判据 → 设计文档相对链接）：

- 症状：改好 `~/.yi-agent/models.json` 后 subagent 仍 100% 失败（`failed: provider error: server error: unexpected status 404: `）。
- 根因：daemon 在 `attach.rs` 的 `build_worker_factory` 里 `build_provider(effective)` 一次成 `Arc<dyn Provider>` 并永久持有（`lib.rs` 的 `start` 只 `Arc::clone`）；app-server 的 `production_factory` 同样在启动时快照清单一次。配置变更永不到达已运行进程。
- 修复：新增 `try_load_catalog_from`（严格读取）与 `CatalogWatcher`（按 `(mtime, size)` 指纹自更新；坏文件保留 last-known-good 且不推进指纹，故修好即生效）；daemon 侧 `with_catalog_watcher` 按 `EffectiveKey` 重建派生件（`new()` 签名不变，零回归）；app-server 的 `production_factory` 改收 watcher。
- 验证：`cargo test -p yi-agent-runtime --lib watcher_`、`-p yi-agent-subagent`、`-p yi-agent-app-server --lib`；真机 e2e 三步判据（成功 → 改坏必失败 → 改好必成功，全程不重启 daemon）；对照现场：旧 daemon 200/404 = 0/120，修复后第 3 步直接成功。
- 链接：`../superpowers/specs/2026-10-17-model-catalog-live-reload-design.md`

- [ ] **Step 3: 同步模块进度**

按 CLAUDE.md「项目进度维护」：
- 在 `docs/project-management/yi-agent-runtime.md` 登记 `try_load_catalog_from` 与 `CatalogWatcher`（带 `file.rs:line` 判据）。
- 在 `docs/project-management/yi-agent-subagent.md` 登记 `with_catalog_watcher`。
- 同步 `README.md` 模块索引表的「完成 / 总计」计数（若有计数变化）。

- [ ] **Step 4: 提交**

```bash
cd .. && git add docs/bug-list.md docs/project-management/
git commit -m "docs: 登记模型清单热重载修复与实测判据"
```

- [ ] **Step 5: 合并回 main（按 CLAUDE.md 流程）**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff fix/model-catalog-live-reload
git worktree remove .worktrees/model-catalog-live-reload
git branch -d fix/model-catalog-live-reload
```

---

## Self-Review

**1. Spec coverage**

| Spec 章节 | 对应任务 |
|---|---|
| §4.1 `try_load_catalog_from` | Task 1 |
| §4.2 `CatalogWatcher`（含失败语义、线程安全） | Task 2 |
| §4.3 daemon 侧 `with_catalog_watcher` / 派生件重建 / `new()` 不变 | Task 3、Task 4 |
| §4.4 app-server `production_factory` 换 watcher + 调用点 | Task 5 |
| §4.5 数据流 | Task 2（watcher）→ Task 3/5（消费） |
| §5 错误处理与边界（坏文件/删除/非法 provider/并发） | Task 1（读）、Task 2（降级）、Task 3（重建降级） |
| §6.1 只覆盖 models.json | 全局约束 + Task 4（只装清单 watcher） |
| §6.2 `provider_profile_id` 不动 | 计划未触碰（符合） |
| §6.3 指纹 (mtime,size) | Task 2 |
| §7.1 runtime 单测 | Task 1、Task 2 |
| §7.2 subagent 单测 | Task 3、Task 4 |
| §7.3 app-server 测试 | Task 5 |
| §7.4 真机 e2e 三步 | Task 6 |
| §7.5 回归门禁 | Task 7 Step 1 |
| §8 影响面清单 | Task 1-7 覆盖全部文件 |
| §9 未决（指纹盲区等） | 未实现（符合非目标） |

无缺口。

**2. Placeholder scan**

无 `TBD` / `TODO` / 「类似 Task N」/ 空泛「加错误处理」。Task 6 的 fixture 与判据以
可直接落地的完整代码给出（隔离 `HOME` + `YI_AGENT_RUNTIME_DIR`，状态经
`ListTaskSummaries`/`InspectTask` IPC 读取，与 `subagent_real_e2e.rs:169` 同法，
不引入新依赖）。

**2b. 已核实的 API 事实（避免计划不可执行）**

| 计划中的用法 | 核实结果 |
|---|---|
| `AgentConfig::default()` | 存在（`agent.rs:126`） |
| `RuntimeConfig` 无 `Default` | 确认；Task 3 测试用逐字段构造 |
| `ApiStyle: PartialEq + Copy` | 确认（`provider.rs:75`），`EffectiveKey` 可 `PartialEq` |
| `AgentWorkerFactory` 是 core 的生产 trait | 确认；**不**在其上加测试方法，改用同 crate 具体类型 |
| `Harness::read_value` 存在、`read_until_id` 不存在 | 确认；Task 5 用循环匹配 `id` |
| `test_catalog()` 返回 `Arc<ModelCatalog>` | 确认（`server.rs:21026`）；`fixed_watcher` 按 `Arc` 取参 |
| `common::yi_agent_bin()` | 确认（`common/mod.rs:140`） |
| `rusqlite` 非依赖 | 确认；Task 6 改用 IPC 读状态 |
| `RecordingProvider` 在同模块测试内 | 确认（`lib.rs:2579`） |

**3. Type consistency**

- `CatalogWatcher::new(path: PathBuf) -> Arc<CatalogWatcher>` / `from_catalog(path, ModelCatalog) -> Arc<CatalogWatcher>` / `current() -> Arc<ModelCatalog>`：Task 2 定义，Task 3/4/5 使用一致。
- `with_catalog_watcher(self, cfg: RuntimeConfig, watcher: Arc<CatalogWatcher>) -> Self`：Task 3 定义，Task 4 使用一致。
- `EffectiveKey` 6 字段（provider/api_url/api_key/model/api_style/reasoning_effort）与 `resolve_effective` 覆盖集一致（已核对 `models.rs:246-268`）。
- `WorkerParts` 5 字段与 `start_with_provider_turn_gate` 内实际取用点（provider/tools/config/sandbox）一致。
- `production_factory(..., catalog: Arc<CatalogWatcher>)`：Task 5 定义并在生产/测试两类调用点统一使用。