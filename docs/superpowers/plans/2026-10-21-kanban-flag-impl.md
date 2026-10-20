# 看板 `superpowers_kanban` 开关实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让设置页的一个全局 flag 控制整个 Superpowers 看板：关掉时停掉插件 daemon、隐藏 UI 与看板会话、拒绝转发；打开后各项目恢复原有开关。

**Architecture:** 全局 flag 来自 `flags.json`（唯一真源），项目层开关继续来自各项目的 `preferences.json`，两者按 `global && project` 复合。supervisor 每 500ms reconcile 一次，读到全局 flag 为关就停掉该进程。前端握手时读一次 `flags/list`，据此决定是否渲染看板入口、是否轮询。

**Tech Stack:** Rust（`yi-agent-flags` / `yi-agent-supervisors` / `yi-agent-app-server`）、TypeScript + React（`desktop`）。

## Global Constraints

- **禁止在 `main` 上直接改代码或提交。** 先 `git worktree add .worktrees/kanban-flag -b feat/kanban-flag`，改完合并回 `main`。
- **flag 注册表规则不可破：** `default` 必须是 `false`（`registry.rs` 的 `every_flag_defaults_to_off`）；`expires_on` 必填。
- **stable 构建必须恒为关：** `store.rs:75` 的 `if build_kind != "dev"` 硬闸不得改动。user 已确认接受 stable 上看板不可用。
- **禁止 `cargo test --workspace`**（OOM / exit 137）。按 crate 跑。
- `flags.json` 的键名发布后不改：`superpowers_kanban`。
- 不做热加载：flag 在装配期读取，改完需重启。
- 不删用户数据：看板会话、worktree、`preferences.json` 旧键一律不动。
- 本仓库**没有 `rg`**，用 `grep`。
- commit message 用 conventional commits，不写 `Co-Authored-By`。提交前跑 `cargo fmt --all`。

---

### Task 1: 注册 `superpowers_kanban` flag

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs:19-32`
- Test: 同文件 `mod tests`

**Interfaces:**
- Produces: `pub const SUPERPOWERS_KANBAN_FLAG: &str = "superpowers_kanban";`，注册进 `REGISTRY`，`default: false`。

- [ ] **Step 1: 写失败测试**

在 `registry.rs` 的 `mod tests` 末尾追加：

```rust
    #[test]
    fn the_superpowers_kanban_flag_is_registered() {
        let def = find("superpowers_kanban").expect("superpowers_kanban must be in the registry");
        assert!(!def.default, "the kanban must default to off");
        assert!(!def.summary.is_empty(), "a summary is what the settings page shows");
        assert_eq!(def.expires_on, "2027-03-31");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags the_superpowers_kanban_flag_is_registered
```
Expected: FAIL，`superpowers_kanban must be in the registry`。

- [ ] **Step 3: 加注册项**

在 `LEAD_AGENT_FLAG` 常量下面加：

```rust
/// Superpowers 看板的总开关。与 `superpowers_kanban` 的 `preferences.json` 键同名
/// 但**不是同一个东西**：那个按项目存、管"这个项目要不要跑看板"；这个是全局的
/// 闸门，关掉时插件进程直接停掉。
pub const SUPERPOWERS_KANBAN_FLAG: &str = "superpowers_kanban";
```

`REGISTRY` 改为两项：

```rust
pub const REGISTRY: &[FlagDef] = &[
    FlagDef {
        name: LEAD_AGENT_FLAG,
        summary: "组长：项目级常驻会话，负责分派与回收子任务",
        owner: "yichen",
        expires_on: "2027-01-31",
        default: false,
    },
    FlagDef {
        name: SUPERPOWERS_KANBAN_FLAG,
        summary: "Superpowers 看板：项目级看板、卡片会话与后台值守",
        owner: "yichen",
        expires_on: "2027-03-31",
        default: false,
    },
];
```

- [ ] **Step 4: 跑全部 flags 测试**

```bash
cargo test -p yi-agent-flags
```
Expected: 19 passed（原 18 + 新增 1）。特别注意 `every_flag_defaults_to_off` 仍通过。

- [ ] **Step 5: Commit**

```bash
cd .. && cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all
git add yi-agent-rs/crates/yi-agent-flags/src/registry.rs
git commit -m "feat(flags): 注册 superpowers_kanban 开关"
```

---

### Task 2: supervisor 只让**全局 flag** 决定起停

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs:37-45,58-85,159-180`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs:105`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs`

**Interfaces:**
- Consumes: `yi_agent_flags::store::status_at(&Path, &str, &str, Option<&str>) -> (bool, Source)`。
- Produces: `Supervisor::new(layout: Layout, build_kind: String)`。

**这条为什么不能简单"叠加项目层"**（先读，再动手）：

项目开关 `preferences.json` 的 `superpowers_kanban` 键是**插件 daemon 自己写**的
（`switch.write` → `superpowers-kanban-core/src/switch.rs`）。supervisor 如果因为
"项目层是关"而停掉 daemon，用户就再也没法用 `switch.write` 把它打开——单向门。
所以分工必须是：

| 层 | 存哪 | 管什么 | 谁写 |
|---|---|---|---|
| 全局 flag | `~/.yi-agent/flags.json` | **进程起不起** | 设置页 `flags/set` → app-server（不经插件） |
| 项目开关 | `<项目>/.yi-agent/preferences.json` | **这个项目干不干活** | 看板 UI → 插件 `switch.write` |

因此 `desired_on` **只读 global**。项目层仍由插件自己解析（已有的
`switch.read` 就是两层复合），supervisor 不再碰它。

- [ ] **Step 1: 写失败测试**

`tests/supervisor.rs` 追加：

```rust
/// 全局 flag 关 → 声明了 switch_key 的进程必须被停掉。
///
/// 这是本次的核心：过去全局层读 `$HOME/.yi-agent/preferences.json`，现在读
/// `flags.json`。用户在设置页关掉开关后 500ms 内进程就该没了。
#[test]
fn a_manifest_whose_global_flag_is_off_is_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("started");
    let script = workdir.join("touch.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\nsleep 30\n", marker.display())).unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&script).unwrap().permissions(),
        0o755,
    );
    write_manifest(workdir, "demo", &script, "demo_on");

    // flags.json 不存在 = flag 关。
    let mut supervisor = Supervisor::new(layout_for(workdir), "dev".to_string());
    supervisor.reconcile();
    assert_eq!(supervisor.running_count(), 0);
    assert!(!marker.exists(), "the process must never have been started");
}

/// 全局 flag 开 → 起进程。**项目层不再参与判定**，这是有意为之：
/// 项目开关由插件自己解析，supervisor 停掉它就断了恢复路径。
#[test]
fn a_manifest_whose_global_flag_is_on_is_started_even_if_the_project_layer_is_absent() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("started");
    let script = workdir.join("touch.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\nsleep 30\n", marker.display())).unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&script).unwrap().permissions(),
        0o755,
    );
    write_manifest(workdir, "demo", &script, "demo_on");

    let mut supervisor = Supervisor::new(layout_for(workdir), "dev".to_string());
    supervisor.reconcile_with_flags(Some(std::collections::BTreeMap::from([(
        "demo_on".to_string(),
        true,
    )])));
    assert_eq!(supervisor.running_count(), 1);
    supervisor.stop_all();
}

/// stable 构建里 flag 恒假，哪怕 flags.json 写着 true。
#[test]
fn a_release_build_never_starts_a_flagged_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("started");
    let script = workdir.join("touch.sh");
    std::fs::write(&script, format!("#!/bin/sh\ntouch {}\nsleep 30\n", marker.display())).unwrap();
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut std::fs::metadata(&script).unwrap().permissions(),
        0o755,
    );
    write_manifest(workdir, "demo", &script, "demo_on");

    let mut supervisor = Supervisor::new(layout_for(workdir), "release".to_string());
    supervisor.reconcile_with_flags(Some(std::collections::BTreeMap::from([(
        "demo_on".to_string(),
        true,
    )])));
    assert_eq!(supervisor.running_count(), 0);
}
```

> `reconcile_with_flags` 是测试缝：真实 `reconcile()` 读 `flags.json`，测试里
> 用注入值避免依赖 `$HOME`。若嫌多一个公开方法，可把 flag 读取抽成私有
> `fn global_flags(&self) -> Option<BTreeMap<String, bool>>` 并给 `#[cfg(test)]`
> 的可变覆盖字段；执行时按哪种更少噪音选哪种。

`layout_for` 若文件里已有等价 helper，直接用现有的。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-supervisors a_manifest_whose_global_flag
```
Expected: 编译失败（`new` 只收 1 个参数、没有 `reconcile_with_flags`）。

- [ ] **Step 3: 加依赖**

`yi-agent-supervisors/Cargo.toml` 的 `[dependencies]`：

```toml
# 全局开关来自 flags.json。supervisor 每 500ms 对账一次，必须自己读得到
# 当前值：由上层传一次性结论会漏掉"进程跑着时用户改了开关"。
yi-agent-flags = { workspace = true }
```

- [ ] **Step 4: 改 `Supervisor`**

```rust
pub struct Supervisor {
    layout: Layout,
    /// 进程级构建类型，供 flag 的 stable 硬闸使用。
    ///
    /// **不能在这里调 `yi_agent_flags::build_kind()`**：那是进程级 `OnceLock`，
    /// 测试里从未注入、恒为 `"release"`，flag 永远读成关，测试就没法验"开"的
    /// 那一半。与 `lead_gate` 是同一个坑。
    build_kind: String,
    running: BTreeMap<String, RunningChild>,
    spawned: Option<Child>,
    spawned_log: Option<ChildLog>,
    /// 测试缝：非 `None` 时用它代替 `flags.json`，免得测试去碰 `$HOME`。
    #[cfg(test)]
    flags_override: Option<BTreeMap<String, bool>>,
}

impl Supervisor {
    pub fn new(layout: Layout, build_kind: String) -> Self {
        Self {
            layout,
            build_kind,
            running: BTreeMap::new(),
            spawned: None,
            spawned_log: None,
            #[cfg(test)]
            flags_override: None,
        }
    }

    /// 生效开关：**只看全局 flag**。
    ///
    /// 项目层（`preferences.json`）故意不读：那个键由插件 daemon 自己写，
    /// 停掉 daemon 就没人能把它打开（`switch.write` 走的就是这条通道）。
    /// 项目级的"这个项目干不干活"由插件自己解析，见
    /// `superpowers-kanban-core/src/switch.rs`。
    pub fn desired_on(&self, manifest: &SupervisorManifest) -> bool {
        self.flag_on(&manifest.switch_key)
    }

    fn flag_on(&self, name: &str) -> bool {
        #[cfg(test)]
        if let Some(map) = &self.flags_override {
            // 测试里仍要遵守 stable 硬闸，才能验 `a_release_build_...`。
            return self.build_kind == "dev" && map.get(name).copied().unwrap_or(false);
        }
        let Some(path) = yi_agent_flags::store::flags_path() else {
            return false;
        };
        yi_agent_flags::store::status_at(&path, name, &self.build_kind, None).0
    }

    #[cfg(test)]
    pub fn reconcile_with_flags(&mut self, flags: Option<BTreeMap<String, bool>>) {
        self.flags_override = flags;
        self.reconcile();
    }
}
```

删掉不再使用的 `global_preferences_path`（`supervisor.rs:59`）和
`switch::resolve_bool` 的调用；`project_preferences_path` 若别处不用也一并删。

- [ ] **Step 5: 改 reconcile 的注释（逻辑不动）**

```rust
        for manifest in manifests {
            if !self.desired_on(&manifest) {
                // 关着 = 停掉。声明了 query_socket 的进程在这里**本该**由
                // `stop_when_disabled:false` 豁免，但看板场景下 daemon 必须退：
                // 它在跑就会轮询看板，用户的"关掉"就不是真关掉。
                //
                // 恢复路径是安全的：重新打开走设置页的 `flags/set`，那条路是
                // app-server 直接写 flags.json，不经过插件 daemon。这跟
                // `supervisor.rs` 原来那句"停掉就成了单向门"针对的是**项目层
                // preferences**——那时的开关只能靠插件自己写。现在全局层换了
                // 来源，单向门不成立了，但注释必须改掉，否则下一个人会照着
                // 旧结论把 daemon 又留下。
                if !manifest.stop_when_disabled {
                    self.ensure_running(&manifest);
                    continue;
                }
                self.stop(&manifest.name, "switch off");
                continue;
            }
            self.ensure_running(&manifest);
        }
```

- [ ] **Step 6: 修调用点**

`crates/yi-agent-runtime/Cargo.toml` 加依赖（**当前没有**，漏了会编译失败）：

```toml
# `supervise::serve` 起 supervisor 时要告诉它构建类型。
yi-agent-flags = { workspace = true }
```

`crates/yi-agent-runtime/src/supervise.rs:105`：

```rust
let mut supervisor = Supervisor::new(layout, yi_agent_flags::build_kind().to_string());
```

`tests/supervisor.rs` 里所有 `Supervisor::new(layout)` 补 `"dev".to_string()`。
同时**删掉**既有的"全局 preferences 决定起停"类测试（`enable(workdir, ...)`
写的是 `$HOME/.yi-agent/preferences.json`），改成写 `flags.json` 或直接用
`reconcile_with_flags`。

- [ ] **Step 7: 跑 supervisor 测试**

```bash
cargo test -p yi-agent-supervisors
```
Expected: 全绿。

- [ ] **Step 8: Commit**

```bash
cd .. && cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all
git add yi-agent-rs/crates/yi-agent-supervisors yi-agent-rs/crates/yi-agent-runtime
git commit -m "feat(supervisors): 全局 flag 决定进程起停"
```

---

### Task 3: 清单让 daemon 随 flag 退出

**Files:**
- Modify: `plugins/superpowers-kanban/supervisors/superpowers-kanban.json`
- Modify: `yi-agent-rs/crates/yi-agent-boards/src/scaffold.rs:42`

**Interfaces:**
- Consumes: Task 2 的 `desired_on` 只读 global。

- [ ] **Step 1: 改两份清单**

`plugins/superpowers-kanban/supervisors/superpowers-kanban.json` 与
`scaffold.rs` 里的模板字符串，都只改一个字段：

```json
  "stop_when_disabled": true,
```

理由写在清单旁边（`scaffold.rs` 的模板注释里）：

```rust
//  `stop_when_disabled: true`：全局 flag 关掉时 daemon 必须退出。
//  留一个"绕过开关继续跑"的进程，用户看到的"关掉"就是假的。
//  恢复路径由设置页的 `flags/set` 保证（app-server 直写 flags.json，
//  不经过这个 daemon），所以停掉不是单向门。
```

`switch_key` 字面量保持 `"superpowers_kanban"` 不动。

- [ ] **Step 2: 验证**

```bash
cd yi-agent-rs && cargo test -p yi-agent-boards && cargo test -p yi-agent-supervisors
```
Expected: 全绿。

- [ ] **Step 3: Commit**

```bash
cd .. && git add plugins/superpowers-kanban/supervisors/superpowers-kanban.json yi-agent-rs/crates/yi-agent-boards
git commit -m "feat(kanban): 清单改为随全局 flag 退出"
```

---

### Task 4: `plugin/query` 说清"被关掉"而不是"坏了"

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/plugin_gate.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`AppState`、`PluginQueryError`、`plugin_query`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/plugin.rs`
- Test: `rpc_tests.rs`

**为什么必须走"注入"而不是就地读**：`lead_gate.rs` 开头那段注释就是教训——
`yi_agent_flags::status` 读进程级 `YI_AGENT_HOME` 与进程级 `build_kind`，而
**测试里 `set_build_info` 从未被调用**（只有 `yi-agent/src/main.rs:44` 调），
所以 `build_kind()` 在测试中恒为 `"release"`。若 `plugin_query` 就地读，
所有既有的 `plugin/query` 测试都会突然返回"已禁用"。

- [ ] **Step 1: 写失败测试**

```rust
/// 清单声明了 flag 且 flag 关 → 必须回 `plugin_disabled`。
///
/// 与 `plugin_unavailable` 分开：那个让人去查安装，这个让人去设置页打开。
#[tokio::test]
async fn plugin_query_says_disabled_when_the_manifests_flag_is_off() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    // 用文件内既有 helper 装一个声明了 flag 的清单；flags.json 里没有它 = 关。
    write_manifest(&h, "demo", "demo_on");

    let response = rpc(
        &mut h,
        2,
        "plugin/query",
        json!({ "plugin": "demo", "project": h.workdir(), "method": "switch.read" }),
    )
    .await;
    assert_eq!(
        response["error"]["data"]["code"],
        json!("plugin_disabled"),
        "a disabled plugin must not look like a broken one: {response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("demo_on"),
        "the message must name the flag the user can flip: {response}"
    );
    h.shutdown().await;
}

/// 反证：没声明 flag 的清单不受影响，仍走原来的路径。
/// 没有这条，一个"永远报禁用"的实现也能让上一条通过。
#[tokio::test]
async fn plugin_query_leaves_an_unflagged_manifest_alone() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    write_manifest(&h, "plain", "");
    let response = rpc(
        &mut h,
        2,
        "plugin/query",
        json!({ "plugin": "plain", "project": h.workdir(), "method": "switch.read" }),
    )
    .await;
    assert_ne!(
        response["error"]["data"]["code"],
        json!("plugin_disabled"),
        "an unflagged manifest has no switch to be disabled by: {response}"
    );
    h.shutdown().await;
}
```

> `Harness` 的字段名、`write_manifest` 是否存在，以 `rpc_tests.rs` 现有写法为准。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_query_says_disabled
```
Expected: FAIL，`data.code` 是 `plugin_unavailable` 或缺失。

- [ ] **Step 3: 建 gate 模块**

`src/plugin_gate.rs`：

```rust
//! 插件"被自己的 flag 关掉"的判定。
//!
//! 与 `lead_gate` 同一手法：**只在构造点算一次，随实例下传**。不在这里调
//! `yi_agent_flags::status`——它读进程级 `YI_AGENT_HOME` 与 `build_kind`，
//! 而测试里 `build_kind` 恒为 `release`，所有 `plugin/query` 用例会被判成禁用。
//!
//! 宿主不认插件名：判定只依据清单里的 `switch_key`。

use std::path::Path;

/// 这个清单的 flag 现在是不是开的。
///
/// `switch_key` 在当前清单格式里必填且非空（`ManifestError::EmptySwitchKey`），
/// 所以不需要 `Option` 分支。
pub fn flag_is_on(flags_path: &Path, switch_key: &str, build_kind: &str) -> bool {
    yi_agent_flags::store::status_at(flags_path, switch_key, build_kind, None).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_build_honors_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"demo_on":true}}"#).unwrap();
        assert!(flag_is_on(&path, "demo_on", "dev"));
    }

    #[test]
    fn a_stable_build_ignores_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"demo_on":true}}"#).unwrap();
        assert!(
            !flag_is_on(&path, "demo_on", "release"),
            "stable must never let a dev flag through"
        );
    }

    #[test]
    fn a_missing_file_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(!flag_is_on(&dir.path().join("flags.json"), "demo_on", "dev"));
    }

    #[test]
    fn an_unregistered_name_is_off() {
        // `read_at` 对未注册的名字一律返回 `None`：只有注册表里的才能开。
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"not_registered":true}}"#).unwrap();
        assert!(!flag_is_on(&path, "not_registered", "dev"));
    }
}
```

`lib.rs` 加 `pub(crate) mod plugin_gate;`（若 `lead_gate` 是 `pub mod`，跟着它写）。

- [ ] **Step 4: 接进 `AppState`**

`server.rs:250` 附近（`lead_agent_enabled` 旁边）加：

```rust
    /// 插件是否被自己的 flag 关掉。`None` = 装配方没给，按"没被关"处理：
    /// 这条判定只管**措辞**，不能因为漏传而把能用的插件拒掉。
    pub(crate) plugin_flags: Option<Arc<PluginFlagTable>>,
```

`PluginFlagTable` 就是 `BTreeMap<String, String>`（插件名 → `switch_key`），
在构造点从该项目清单算出来：

```rust
    /// 该项目下每个插件名对应的 `switch_key`。`plugin_query` 用它判断
    /// "是不是被 flag 关掉了"——只认清单，不认插件名。
    fn plugin_flag_table(project: &Path) -> Arc<BTreeMap<String, String>> {
        let layout = yi_agent_supervisors::Layout::for_workdir(project);
        Arc::new(
            yi_agent_supervisors::manifest::load_manifests(&layout.manifests_dir())
                .into_iter()
                .map(|manifest| (manifest.name, manifest.switch_key))
                .collect(),
        )
    }
```

> `Layout` / `load_manifests` 的确切路径看 `server.rs:list_installed_plugins`
> 怎么取的，照抄。若 `plugin_query` 的 `project` 是运行时参数（它不是构造期
> 已知的），那就**不要**放 `AppState`，而是按 `project` 就地取表——见 Step 5
> 的说明，选一种，别两边都做。

- [ ] **Step 5: 在 `plugin_query` 里判定**

`plugin_query(project, method, plugin, params)` 的 `project` 是运行时参数，
所以表按 `project` 现取（清单目录很小，500ms 一次的对账都在这么干，
每次查询读一次不是瓶颈）：

```rust
    if plugin.is_empty() {
        return Err(PluginQueryError::new(
            "invalid_params",
            "plugin/query needs a `plugin` name",
        ));
    }

    // 先看这个插件是不是被自己的 flag 关掉了。daemon 停着时下面必然拿到
    // "连接失败"，那句话会把用户引向"插件坏了"；这里给的是"去设置页打开"。
    //
    // 通用：只认清单的 `switch_key`。没有清单（未安装）时不判定，交给后面
    // 的 `plugin_unavailable`——"没装"和"关着"是两件事。
    if let Some(switch_key) = plugin_switch_key(project, plugin) {
        if let Some(flags_path) = yi_agent_flags::store::flags_path() {
            if !crate::plugin_gate::flag_is_on(
                &flags_path,
                &switch_key,
                yi_agent_flags::build_kind(),
            ) {
                return Err(PluginQueryError::new(
                    "plugin_disabled",
                    format!(
                        "this feature is disabled; turn on `{switch_key}` in Settings > Experimental"
                    ),
                ));
            }
        }
    }
```

`plugin_switch_key`：

```rust
/// 这个插件在清单里声明的 flag 名。没有清单 → `None`。
fn plugin_switch_key(project: &Path, plugin: &str) -> Option<String> {
    let layout = yi_agent_supervisors::Layout::for_workdir(project);
    yi_agent_supervisors::manifest::load_manifests(&layout.manifests_dir())
        .into_iter()
        .find(|manifest| manifest.name == plugin)
        .map(|manifest| manifest.switch_key)
}
```

`PluginQueryError::new("plugin_disabled", ...)` 走 `rpc/plugin.rs:67` 的
`RpcError::plugin_query_error(error.code, error.message)`，所以还要在
`protocol.rs:165` 的数字表里加一行：

```rust
            "plugin_disabled" => -32025,
```

并在 `protocol.rs:156` 那句文档的括号里补上 `plugin_disabled`。

**测试为什么要能过**：`plugin_query` 里的 `build_kind()` 在测试中恒为
`release` → 声明了 flag 的清单**永远**判成禁用。所以 Step 1 的第一条测试能过、
第二条（无 flag 清单）也能过——但既有那些用声明了 flag 的清单做的
`plugin/query` 测试会全部失败。**这是最可能出现的情况，处理办法**：

把 `build_kind` 也做成可注入的：`AppState` 加 `plugin_flags_build_kind: Option<&'static str>`，
生产传 `Some(yi_agent_flags::build_kind())`，测试传 `None` 表示**跳过判定**
（不是"当作 release"）。判定处改成：

```rust
    if let Some(build_kind) = plugin_flags_build_kind {
        ...判定...
    }
```

> 这是刻意的：测试不该假装自己是 stable 或 dev，它只验"判定接上了没有"。
> 真实构建类型的语义由 `plugin_gate` 的单元测试覆盖，那里 `build_kind` 是
> 显式传参的。

- [ ] **Step 6: 跑测试**

```bash
cargo test -p yi-agent-app-server --lib plugin_query
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent-app-server plugin_gate
```
Expected: 全绿，既有 `plugin/query` 用例一个都不许挂。

- [ ] **Step 7: Commit**

```bash
cd .. && cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): flag 关掉时 plugin/query 直说禁用"
```

---

### Task 5: 前端隐藏入口与看板会话

**Files:**
- Modify: `desktop/src/App.tsx`（握手读 `flags/list`、`useBoardState` 调用 `:362`、看板视图渲染）
- Modify: `desktop/src/hooks/useBoardState.ts`
- Modify: `desktop/src/components/ThreadSidebar.tsx`
- Test: `desktop/src/App.test.tsx`、`desktop/src/components/ThreadSidebar.test.tsx`

**Interfaces:**
- Consumes: `flags/list` → `{ flags: [{ name, enabled, ... }] }`。
- Produces: `useBoardState({ clientRef, kanbanEnabled })`；`ThreadSidebar` 新增
  `kanbanEnabled?: boolean`（缺省 `true`）。

- [ ] **Step 1: 写失败的侧栏测试**

```tsx
  it("hides the board section and its threads when kanban is off", () => {
    const card: ThreadSummary = { ...thread("card-1", "卡片会话", "/p"), driver_ref: "card-1" };
    const { container } = renderSidebar({
      groups: [{ workspace: "/p", exists: true, threads: [card] }],
      kanbanEnabled: false,
    });
    expect(screen.queryByText("看板会话", { exact: false })).toBeNull();
    // 关键：不能只藏小节不剔行。否则卡片会话会混进普通分组，点进去还没有看板语义。
    expect(container.querySelector('[data-thread-row="card-1"]')).toBeNull();
  });

  it("shows the board section by default", () => {
    const card: ThreadSummary = { ...thread("card-1", "卡片会话", "/p"), driver_ref: "card-1" };
    renderSidebar({ groups: [{ workspace: "/p", exists: true, threads: [card] }] });
    expect(screen.getByText("看板会话", { exact: false })).toBeTruthy();
  });
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx -t "kanban"
```

- [ ] **Step 3: 改侧栏**

props 加 `kanbanEnabled = true,`，类型加：

```tsx
  /**
   * 看板是否开启（`flags/list` 的 `superpowers_kanban`）。缺省 `true`：
   * 既有调用点与测试都不传；生产由 `App` 显式传真实值。
   */
  kanbanEnabled?: boolean;
```

`renderBoardThreads` 开头加 `if (!kanbanEnabled) return null;`。

**先跑测试**：卡片会话带 `driver_ref`，普通分组本来就按 `!t.driver_ref` 过滤
（`:1006` 附近），所以理论上会自动消失。测试通过就不要加多余逻辑；失败再查
是哪条路径把它们渲染出来的——不要提前加一层过滤猜着修。

- [ ] **Step 4: 改 hook**

```tsx
export function useBoardState({
  clientRef,
  kanbanEnabled,
}: {
  clientRef: RefObject<RpcClient | null>;
  /** 关着就不拉列表、不轮询——功能关着，没有可展示的东西。 */
  kanbanEnabled: boolean;
}) {
```

`refreshBoards` 与 2 秒计时器的 effect 开头各加 `if (!kanbanEnabled) return;`，
依赖数组加 `kanbanEnabled`。

- [ ] **Step 5: 改 App**

state：

```tsx
  /**
   * 看板开关（`superpowers_kanban`）。`null` = 还不知道；只有 `=== true` 才算开。
   * 与 `leadAgentEnabled` 同一时机、同一失败口径（读不到按关）。
   */
  const [kanbanEnabled, setKanbanEnabled] = useState<boolean | null>(null);
```

握手（读 `flags/list` 处）把两项一起取出：

```tsx
          const rows = flags.flags ?? [];
          leadOn = rows.find((f) => f.name === "lead_agent")?.enabled === true;
          kanbanOn = rows.find((f) => f.name === "superpowers_kanban")?.enabled === true;
```

调用点：

```tsx
  const board = useBoardState({ clientRef, kanbanEnabled: kanbanEnabled === true });
```

`ThreadSidebar` 传 `kanbanEnabled={kanbanEnabled === true}`；看板视图
（`SuperpowersKanbanView` / `SuperpowersKanbanEnqueue`）外包
`{kanbanEnabled === true && (...)}`。

- [ ] **Step 6: 改 App 测试 mock**

`state` 加 `kanbanFlagEnabled: true,` + `beforeEach` 重置；mock 的 `flags/list`
返回两项（`lead_agent`、`superpowers_kanban`）。新增两条：

```tsx
  it("does not render the board when the flag is off", async () => {
    state.kanbanFlagEnabled = false;
    render(<App />);
    await waitFor(() => expect(screen.getByText("one")).toBeTruthy());
    expect(clients[0].requests.map((r) => r.method)).not.toContain("board/list");
  });

  it("renders the board when the flag is on", async () => {
    state.kanbanFlagEnabled = true;
    render(<App />);
    await waitFor(() =>
      expect(clients[0].requests.map((r) => r.method)).toContain("board/list"),
    );
  });
```

> `board/list` 的确切方法名以既有 mock 为准。

- [ ] **Step 7: 跑前端测试**

```bash
cd desktop && npx vitest run && npx tsc --noEmit
```
Expected: 全绿；既有看板用例若因默认值变化失败，把它们的
`state.kanbanFlagEnabled` 显式设为 `true`。

- [ ] **Step 8: Commit**

```bash
cd .. && git add desktop/src
git commit -m "feat(desktop): 看板入口与看板会话跟随 superpowers_kanban 开关"
```

---

### Task 6: `plugin/list` 暴露开关状态

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`PluginSummary`、`list_installed_plugins`、`rpc/plugin.rs:84`）
- Test: `rpc_tests.rs`

**Interfaces:**
- Produces: `plugin/list` 每项多一个 `"enabled": bool`。

- [ ] **Step 1: 写失败测试**

```rust
/// `plugin/list` 必须说清"装但被关掉"。否则前端只能显示成"已安装未运行"，
/// 用户会以为是崩溃。
#[tokio::test]
async fn plugin_list_reports_whether_each_plugins_flag_is_on() {
    let mut h = Harness::new();
    initialize(&mut h).await;
    let response = rpc(&mut h, 2, "plugin/list", json!({})).await;
    for plugin in response["result"]["plugins"].as_array().expect("plugins") {
        assert!(plugin.get("enabled").is_some(), "every row needs `enabled`: {plugin}");
    }
    h.shutdown().await;
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cargo test -p yi-agent-app-server --lib plugin_list_reports_whether
```
Expected: FAIL，没有 `enabled` 字段。

- [ ] **Step 3: 实现**

`PluginSummary` 加 `pub enabled: bool`（字段是 `String` 型 `switch_key`，**不改类型**）：

```rust
/// 这一行的开关**现在**是不是开的：`flags.json` 里 `switch_key` 的值，
/// 受构建类型硬闸约束。前端据此把"装但关着"与"装且该跑"分开显示。
pub enabled: bool,
```

`list_installed_plugins` 改签名收 `build_kind: &str`，逐项算：

```rust
            let enabled = yi_agent_flags::store::flags_path()
                .map(|path| {
                    yi_agent_flags::store::status_at(
                        &path,
                        &manifest.switch_key,
                        build_kind,
                        None,
                    )
                    .0
                })
                // 解析不出 flags.json 路径 = 没有持久化开关 = 按关处理，
                // 与 `status_at` 的缺省一致。
                .unwrap_or(false);
```

响应 JSON 加 `"enabled": plugin.enabled`（`server.rs` 两处 + `rpc/plugin.rs:84`）。

- [ ] **Step 4: 跑测试**

```bash
cargo test -p yi-agent-app-server --lib plugin_list
```
Expected: 全绿。

- [ ] **Step 5: Commit**

```bash
cd .. && cargo fmt --manifest-path yi-agent-rs/Cargo.toml --all
git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): plugin/list 区分被 flag 关掉与未安装"
```

---

### Task 7: 全量验收与文档

**Files:**
- Modify: `docs/project-management/yi-agent-flags.md`
- Modify: `docs/project-management/README.md`
- Modify: `docs/project-management/yi-agent-supervisors.md`

- [ ] **Step 1: 跑全量**

```bash
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd yi-agent-rs
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p yi-agent-flags
cargo test -p yi-agent-supervisors
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent --bin yi-agent
bash scripts/test_dev_env.sh
cd ../desktop && npx vitest run && npx tsc --noEmit
```

- [ ] **Step 2: 更新文档**

`yi-agent-flags.md` 的 Features 加：

```markdown
- [x] 第二个 flag：`superpowers_kanban` — `registry.rs` 的 `SUPERPOWERS_KANBAN_FLAG`，默认 `false`、`expires_on: "2027-03-31"`。判据：`cargo test -p yi-agent-flags` 的 `the_superpowers_kanban_flag_is_registered`（19 passed）；`cargo test -p yi-agent-supervisors` 证明 flag 关时进程被停
```

`README.md` 的 `yi-agent-flags` 计数 `8 / 8` → `9 / 9`。

`yi-agent-supervisors.md` 加一条记录 `switch_key` 语义变更与 `stop_when_disabled` 的新优先级。

- [ ] **Step 3: 端到端手测**

```bash
cd yi-agent-rs && bash scripts/dev-env.sh run sh -c '
  yi-agent-dev flags status superpowers_kanban
  yi-agent-dev flags set superpowers_kanban on
  yi-agent-dev flags status superpowers_kanban
'
```
Expected: 初始 `off (source: default)`；`set on` 后 `on (source: file)`。

然后打开 dev App，确认：设置页「实验功能」里能看到看板开关；关掉后看板入口消失、`ps` 里没有 `superpowers-kanban` 进程；重新打开并重启后看板回来、项目开关保持原状。

- [ ] **Step 4: Commit 并合并**

```bash
cd .. && git add docs/project-management
git commit -m "docs: 记录 superpowers_kanban 开关"
git checkout main && git merge --no-ff feat/kanban-flag
git worktree remove .worktrees/kanban-flag
git branch -d feat/kanban-flag
```

---

## 自审记录

**Spec 覆盖：**
- §4.1 注册表 → Task 1
- §4.2 supervisor 起停 → Task 2、Task 3
- §4.3 UI 隐藏（hook / App / 侧栏 / 视图） → Task 5
- §4.4 宿主转发拒绝 → Task 4
- §6 风险 3（`plugin/list` 显示） → Task 6
- §7 验收 → Task 7

**已知待确认**（执行时按实际情况微调，不要为凑测试写不可测代码）：
- Task 2 Step 1 第二个测试的 `new_with_flag` 若显得多余，可删掉，第一个测试已覆盖核心。
- Task 5 Step 3 的侧栏过滤可能无需改动，先跑测试验证。
- Task 5 Step 6 的 `board/list` 方法名以既有 mock 为准。
