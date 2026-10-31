# 桌面壳按实例隔离 home 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面壳认 `YI_AGENT_HOME`，使第二个 app 拥有独立的 relay session、runtime 与 threads。

**Architecture:** 在 `desktop/src-tauri/src/bridge.rs` 增加一个纯函数 `instance_home_from(env, home)`，与 `yi_agent_store::paths::instance_home_from` 语义一致（值不以 `.yi-agent` 结尾则补上）。三处消费者（`configured_relay`、`set_relay_url`、`spawn_once` 的 cwd）改为走它。sidecar 继承 `YI_AGENT_HOME`，两侧解析结果天然一致。

**Tech Stack:** Rust / Tauri 2 / `dirs` crate

## Global Constraints

- 未设 `YI_AGENT_HOME` 时，行为必须与今天**逐字一致**（`dirs::home_dir()/.yi-agent`）。
- 归一化规则与 `yi-agent-store/src/paths.rs:instance_home_from` 保持一致：值若 `file_name() != ".yi-agent"` 则 `join(".yi-agent")`。
- 空白环境变量视同未设置。
- 所有新测试必须是纯函数测试或使用临时目录，**绝不**读写真实 `~`。
- 不新增依赖。
- 提交前在 worktree 根目录跑 `cd yi-agent-rs && cargo fmt --all`（若改动涉及该 workspace），或对应 crate 的 fmt。

---

### Task 1: 新增 `instance_home_from` 纯函数

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs:207-218`（`home_dir()` 与 `preferences_path()` 之间）
- Test: `desktop/src-tauri/src/bridge.rs`（`mod tests` 内）

**Interfaces:**
- Produces:
  - `fn instance_home_from(override_value: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Option<PathBuf>`
  - `fn instance_home() -> Option<PathBuf>`

- [ ] **Step 1: 写失败测试**

在 `mod tests` 内追加：

```rust
    /// `YI_AGENT_HOME` 非空时赢，且不以 `.yi-agent` 结尾会自动补上。
    #[test]
    fn instance_home_prefers_a_non_blank_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().as_os_str();
        let home = Path::new("/Users/example");
        assert_eq!(
            instance_home_from(Some(value), Some(home)),
            Some(dir.path().join(".yi-agent"))
        );
    }

    /// 覆盖值已经叫 `.yi-agent` 时不再嵌套。
    #[test]
    fn instance_home_does_not_double_nest() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().join(".yi-agent");
        assert_eq!(
            instance_home_from(Some(value.as_os_str()), None),
            Some(value)
        );
    }

    /// 空白覆盖视同未设置，退化为 `<home>/.yi-agent`。
    #[test]
    fn instance_home_ignores_a_blank_override() {
        let home = Path::new("/Users/example");
        assert_eq!(
            instance_home_from(Some(std::ffi::OsStr::new("")), Some(home)),
            Some(home.join(".yi-agent"))
        );
        assert_eq!(
            instance_home_from(Some(std::ffi::OsStr::new("   ")), Some(home)),
            Some(home.join(".yi-agent"))
        );
    }

    /// 两侧都缺 → None。
    #[test]
    fn instance_home_is_none_without_any_home() {
        assert_eq!(instance_home_from(None, None), None);
    }
```

`mod tests` 顶部已有 `use super::*;`（bridge.rs:379），文件顶部已有 `use std::path::{Path, PathBuf};`，无需额外 import。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test instance_home -- --nocapture
```

Expected: 编译失败，`cannot find function instance_home_from`.

- [ ] **Step 3: 写最小实现**

在 `bridge.rs` 的 `home_dir()` 之后插入：

```rust
/// 实例状态根。`YI_AGENT_HOME` 优先，否则 `<home>/.yi-agent`。
///
/// 与 `yi_agent_store::paths::instance_home_from` 同一套归一化：覆盖值若不以
/// `.yi-agent` 结尾会自动补上，这样所有消费者都能安全地拼 `.yi-agent/...`。
/// 桌面壳刻意不依赖 app-server crate，故此处镜像同一规则。
fn instance_home_from(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_value.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        // 纯空白不算有效覆盖（`"   "` 的用户多半是 shell 里写错了）。
        if dir
            .as_os_str()
            .to_string_lossy()
            .trim()
            .is_empty()
        {
            // fall through to the default
        } else if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            return Some(dir);
        } else {
            return Some(dir.join(".yi-agent"));
        }
    }
    home.map(|home| home.join(".yi-agent"))
}

/// 进程级的实例状态根：`YI_AGENT_HOME` 环境变量 + `dirs::home_dir()`。
fn instance_home() -> Option<PathBuf> {
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty());
    instance_home_from(override_value.as_deref(), home_dir().as_deref())
}
```

`tempfile = "3"` 已在 `desktop/src-tauri/Cargo.toml` 的 `[dev-dependencies]`，
无需新增依赖。既有测试用 `tempfile::TempDir::new()`，本计划的测试沿用同一写法。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test instance_home -- --nocapture
```

Expected: 4 passed.

- [ ] **Step 5: 提交**

```bash
git add desktop/src-tauri/src/bridge.rs
git commit -m "feat(desktop): 新增 instance_home_from，按 YI_AGENT_HOME 切分实例"
```

---

### Task 2: 三处消费者改用实例 home

**Files:**
- Modify: `desktop/src-tauri/src/bridge.rs:174-186`（`configured_relay`）
- Modify: `desktop/src-tauri/src/bridge.rs:275-292`（`set_relay_url`）
- Modify: `desktop/src-tauri/src/bridge.rs:325-336`（`spawn_once` 的 cwd）
- Test: `desktop/src-tauri/src/bridge.rs`（`mod tests` 内）

**Interfaces:**
- Consumes: Task 1 的 `instance_home_from`、`instance_home`
- Produces: 无新公开接口；行为变化仅在于偏好文件与 cwd 的落点

- [ ] **Step 1: 写失败测试**

```rust
    /// relay 偏好从实例 home 读，而不是真实 `~`。
    #[test]
    fn configured_relay_reads_the_instance_home_preferences() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        save_relay_url(home, Some("wss://instance/connect?session=i")).unwrap();
        let stored = load_relay_url(home);
        assert_eq!(
            stored.as_deref(),
            Some("wss://instance/connect?session=i")
        );
        // 落点确认：写在 `<instance home>/.yi-agent/preferences.json`。
        assert!(home.join(".yi-agent").join("preferences.json").is_file());
    }

    /// sidecar 的 cwd 解析：有实例 home 时用它，否则用 `home_dir()` 的结果。
    #[test]
    fn sidecar_cwd_prefers_the_instance_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().to_path_buf();
        let resolved = sidecar_cwd_from(Some(value.as_os_str()), None);
        assert_eq!(resolved, Some(dir.path().join(".yi-agent")));
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test sidecar_cwd_prefers -- --nocapture
```

Expected: 编译失败，`cannot find function sidecar_cwd_from`.

- [ ] **Step 3: 写最小实现**

3a. 新增 cwd 纯函数（放在 `instance_home()` 之后）：

```rust
/// sidecar 进程的工作目录：实例 home 的父目录（sidecar 会自己拼 `.yi-agent`）。
///
/// 打包 app 从 Finder 启动时继承 `/`，会让 agent 的 workdir 变成文件系统根，
/// 所以必须显式锚定到一个真实目录。
fn sidecar_cwd_from(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    instance_home_from(override_value, home).map(|dir| {
        dir.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.clone())
    })
}
```

3b. `configured_relay()` 改用它：

```rust
fn configured_relay() -> Option<String> {
    let home = instance_home();
    configured_relay_in(home.as_deref())
}
```

3c. `set_relay_url()` 改用它：

```rust
pub fn set_relay_url(app: AppHandle, url: Option<String>) -> Result<(), String> {
    let home = instance_home().ok_or_else(|| "home directory is unavailable".to_string())?;
    // ... 其余不变
}
```

3d. `spawn_once()` 的 cwd 段：

```rust
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty());
    match sidecar_cwd_from(override_value.as_deref(), home_dir().as_deref()) {
        Some(dir) => cmd = cmd.current_dir(dir),
        None => eprintln!("[sidecar] home_dir unavailable, using inherited cwd"),
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test -- --nocapture
```

Expected: 全部通过（含既有 bridge 测试；它们注入 temp home，不应受本轮改动影响）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src-tauri/src/bridge.rs
git commit -m "feat(desktop): relay 偏好与 sidecar cwd 走实例 home"
```

---

### Task 3: 文档与验收

**Files:**
- Modify: `docs/superpowers/specs/2026-10-25-desktop-instance-home-design.md`（若实现与设计有出入，回写偏差）

- [ ] **Step 1: 全量测试**

```bash
cd desktop/src-tauri && cargo test
```

Expected: 全绿。

- [ ] **Step 2: 手工验收（两个 app）**

```bash
# 给 nongit app 注入独立实例 home（LSEnvironment 不展开 ~，用绝对路径）
/usr/libexec/PlistBuddy -c "Add :LSEnvironment dict" /Applications/yi-agent-nongit.app/Contents/Info.plist
/usr/libexec/PlistBuddy -c "Add :LSEnvironment:YI_AGENT_HOME string $HOME/.yi-agent-nongit" /Applications/yi-agent-nongit.app/Contents/Info.plist
codesign --force --sign - /Applications/yi-agent-nongit.app
```

启动两个 app 后验证：

```bash
# 两个 sidecar 的 session 必须不同
ps -axo command | grep "app-server --listen" | grep -o "session=[a-f0-9]*"
```

Expected: 两个不同的 session id。

```bash
# runtime 目录必须不同
ls -d "$HOME/.yi-agent/runtime" "$HOME/.yi-agent-nongit/.yi-agent/runtime"
```

Expected: 两个目录都存在。

- [ ] **Step 3: 提交**

```bash
git add -A
git commit -m "docs(specs): 回写实例 home 实现的验收结果"
```

---

## Self-Review

**Spec coverage:**
- 设计 §1「新增 instance home 解析」→ Task 1
- 设计 §2「三处消费者改用它」→ Task 2（`configured_relay` / `set_relay_url` / `spawn_once` 全部覆盖）
- 设计 §3「nongit app 声明 LSEnvironment」→ Task 3 Step 2
- 设计「兼容性」三条 → Task 1 Step 1 的 3 个测试（override / 补 `.yi-agent` / 空白）
- 设计「测试」6 项 → `instance_home_*` 4 项 + `configured_relay_reads_the_instance_home_preferences` + `sidecar_cwd_prefers_the_instance_home`
- 设计「不做」→ 计划未引入新依赖、未改 sidecar 侧 semantics

**Placeholder scan:** 无 TBD/TODO；每个 step 都带可执行命令与完整代码。

**Type consistency:** `instance_home_from` 签名在 Task 1 定义，Task 2 的 `sidecar_cwd_from` 与三处消费者均按 `Option<&OsStr>` + `Option<&Path>` 调用，一致。
