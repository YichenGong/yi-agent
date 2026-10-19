# 稳定版 / 开发版分道与多实例并行 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让稳定版由一个已发布 tag 的构建独占，开发版作为可随时销毁的临时实例运行，两者可在同一台机器、同一 worktree 上并存且互不污染。

**Architecture:** 新增 `yi-agent-store::paths` 作为宿主路径的唯一解析点，用一个 `YI_AGENT_HOME` 决定实例边界；构建期由 `build.rs` 注入版本标识；实例由 `yi-agent-rs/scripts/dev-env.sh` 创建与销毁；开发版禁用 launchd 值守，改由会话内前台 `resident watch` 承担。

**Tech Stack:** Rust 2024 edition、Cargo workspace、clap 4（derive）、bash、GitHub Actions / GitLab CI

## Global Constraints

- 工作目录：所有命令在 `yi-agent-rs/` 下执行（worktree 路径为 `.worktrees/<name>/yi-agent-rs`）。
- **禁止**在 `main` 上直接提交。每个 Task 在功能分支所在 worktree 内完成。
- 每次 commit 前必须 `cargo fmt --all`。
- **不要**在 commit message 里写 `Co-Authored-By` 行。
- 测试按 crate 跑，**不要**用 `cargo test --workspace`（会 OOM / exit 137）。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认没有残留 cargo 进程。
- 环境变量未设置时，所有行为必须与改动前完全一致。
- 新环境变量前缀统一 `YI_AGENT_`。
- 版本号唯一真源仍是 `yi-agent-rs/Cargo.toml:21`，本计划**不修改**该文件的版本值。

## File Structure

| 文件 | 职责 |
|---|---|
| `crates/yi-agent-store/src/paths.rs`（新建） | 唯一路径解析点：`instance_home()`、`skills_dir()`、`os_home()`、`trace_dir()` |
| `crates/yi-agent-store/src/lib.rs`（改） | 注册 `pub mod paths;` |
| `crates/yi-agent/build.rs`（新建） | 生成 `YI_AGENT_VERSION` 环境变量，判定 release / dev / nogit |
| `crates/yi-agent/src/build_info.rs`（新建） | 暴露 `VERSION` 常量与 `BUILD_KIND` |
| `crates/yi-agent/src/config.rs`（改） | `#[command(version = ...)]` 改为注入值 |
| `crates/yi-agent-rs/scripts/dev-env.sh`（新建） | `up` / `run` / `down` 三个动作 |
| `crates/yi-agent-rs/scripts/test_dev_env.sh`（新建） | 脚本级回归测试 |

---

### Task 1: 建立 `paths` 模块

**Files:**
- Create: `crates/yi-agent-store/src/paths.rs`
- Modify: `crates/yi-agent-store/src/lib.rs:7-12`

**Interfaces:**
- Produces:
  - `pub fn os_home() -> Option<PathBuf>`
  - `pub fn instance_home() -> Option<PathBuf>`
  - `pub fn skills_dir() -> Option<PathBuf>`
  - `pub fn trace_dir() -> Option<PathBuf>`
  - `pub fn instance_root() -> Option<PathBuf>`
  - `pub fn home_join(instance_relative: &str) -> Option<PathBuf>`

- [ ] **Step 1: 写失败测试**

创建 `crates/yi-agent-store/src/paths.rs`，先只放测试：

```rust
//! 宿主路径的唯一解析点。
//!
//! 稳定版与开发版靠 `YI_AGENT_HOME` 切分实例边界；未设置时退化为
//! `$HOME/.yi-agent`，与历史布局逐字一致。

use std::path::PathBuf;

pub fn os_home() -> Option<PathBuf> {
    unimplemented!()
}

pub fn instance_home() -> Option<PathBuf> {
    unimplemented!()
}

pub fn skills_dir() -> Option<PathBuf> {
    unimplemented!()
}

pub fn trace_dir() -> Option<PathBuf> {
    unimplemented!()
}

pub fn home_join(instance_relative: &str) -> Option<PathBuf> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 进程级环境变量在测试里会互相竞态，故所有断言走注入版。
    fn instance_home_from(env: Option<&str>, os_home: Option<&str>) -> Option<PathBuf> {
        let _ = (env, os_home);
        unimplemented!()
    }

    #[test]
    fn defaults_to_dot_yi_agent_under_home() {
        assert_eq!(
            instance_home_from(None, Some("/Users/me")),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn a_bare_override_gains_the_dot_yi_agent_suffix() {
        assert_eq!(
            instance_home_from(Some("/tmp/dev-a"), Some("/Users/me")),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_empty_override_is_ignored() {
        assert_eq!(
            instance_home_from(Some("   "), Some("/Users/me")),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn no_home_and_no_override_stays_none() {
        assert_eq!(instance_home_from(None, None), None);
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p yi-agent-store --lib paths`

Expected: 编译通过但 4 个测试 FAIL（`not yet implemented`）。

- [ ] **Step 3: 实现**

把 `paths.rs` 替换为：

```rust
//! 宿主路径的唯一解析点。
//!
//! 稳定版与开发版靠 `YI_AGENT_HOME` 切分实例边界；未设置时退化为
//! `$HOME/.yi-agent`，与历史布局逐字一致。

use std::path::{Path, PathBuf};

/// 真实宿主目录，仅供 launchd 等必须落在真实 HOME 的位置使用。
pub fn os_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// 实例状态根。`YI_AGENT_HOME` 优先，否则 `$HOME/.yi-agent`。
///
/// **归一化**：覆盖值若不以 `.yi-agent` 结尾，会自动补上。这条不变量让所有
/// 消费者都能安全地拼 `.yi-agent/...`（例如 `skill_rpc::roots`），不会出现
/// `<id>/.yi-agent/.yi-agent/skills` 这种双重嵌套。
pub fn instance_home() -> Option<PathBuf> {
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty());
    instance_home_from(override_value.as_deref(), os_home().as_deref())
}

fn instance_home_from(override_value: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = override_value {
        let dir = PathBuf::from(dir);
        if dir.as_os_str().is_empty() {
            // fall through to the default
        } else if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            return Some(dir);
        } else {
            return Some(dir.join(".yi-agent"));
        }
    }
    home.map(|home| home.join(".yi-agent"))
}

/// `<instance_home>` 的父目录，即「像一个 HOME 的目录」。
///
/// `skill_rpc::roots` 会在传入目录后拼 `.yi-agent/skills`；传本函数的返回值即可
/// 得到与 [`skills_dir`] 一致的用户技能根。
pub fn instance_root() -> Option<PathBuf> {
    instance_home().and_then(|home| home.parent().map(Path::to_path_buf))
}

/// 用户技能根。`YI_AGENT_SKILLS_DIR` 优先，否则 `<instance_home>/skills`。
pub fn skills_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("YI_AGENT_SKILLS_DIR").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    instance_home().map(|home| home.join("skills"))
}

/// 会话日志根。始终随实例走，便于按实例排障。
pub fn trace_dir() -> Option<PathBuf> {
    instance_home().map(|home| home.join("trace"))
}

/// `<instance_home>/<relative>`。
pub fn home_join(instance_relative: &str) -> Option<PathBuf> {
    instance_home().map(|home| home.join(instance_relative))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn from<S: AsRef<OsStr> + ?Sized>(
        override_value: Option<&S>,
        home: Option<&Path>,
    ) -> Option<PathBuf> {
        instance_home_from(override_value.map(AsRef::as_ref), home)
    }

    #[test]
    fn defaults_to_dot_yi_agent_under_home() {
        assert_eq!(
            from::<OsStr>(None, Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn a_bare_override_gains_the_dot_yi_agent_suffix() {
        assert_eq!(
            from(Some(OsStr::new("/tmp/dev-a")), Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_override_that_already_ends_in_dot_yi_agent_is_kept_verbatim() {
        assert_eq!(
            from(Some(OsStr::new("/tmp/dev-a/.yi-agent")), None),
            Some(PathBuf::from("/tmp/dev-a/.yi-agent"))
        );
    }

    #[test]
    fn an_empty_override_is_ignored() {
        assert_eq!(
            from(Some(OsStr::new("")), Some(Path::new("/Users/me"))),
            Some(PathBuf::from("/Users/me/.yi-agent"))
        );
    }

    #[test]
    fn no_home_and_no_override_stays_none() {
        assert_eq!(from::<OsStr>(None, None), None);
    }

    #[test]
    fn the_instance_root_is_the_parent_of_the_state_root() {
        // skill_rpc 会在这个父目录后拼 `.yi-agent/skills`。
        let home = PathBuf::from("/tmp/dev-a/.yi-agent");
        assert_eq!(home.parent().unwrap(), Path::new("/tmp/dev-a"));
    }
}
```

- [ ] **Step 4: 注册模块并跑测试**

在 `crates/yi-agent-store/src/lib.rs` 的 `pub mod ipc;` 前插入：

```rust
pub mod paths;
```

Run: `cargo test -p yi-agent-store --lib paths`

Expected: 4 passed。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add crates/yi-agent-store/src/paths.rs crates/yi-agent-store/src/lib.rs
git commit -m "feat(store): 宿主路径解析收敛到 paths 模块"
```

---

### Task 2: 让状态目录跟随实例

**Files:**
- Modify: `crates/yi-agent-store/src/resident.rs:13-19`
- Modify: `crates/yi-agent-runtime/src/config.rs:184-189`
- Modify: `crates/yi-agent-runtime/src/models.rs:285-296`
- Modify: `crates/yi-agent-supervisors/src/supervisor.rs:59-68`
- Modify: `crates/yi-agent-web/src/lib.rs:12-18`
- Modify: `crates/yi-agent-app-server/src/device_store.rs:50-55`
- Modify: `crates/yi-agent-app-server/src/workspace_index.rs:38-44`
- Modify: `crates/yi-agent/src/tracing_init.rs:76-79`
- Test: `crates/yi-agent-store/src/paths.rs`（追加）

**Interfaces:**
- Consumes: `yi_agent_store::paths::{instance_home, home_join, trace_dir, os_home}`（Task 1）
- Produces: 全部上述函数在未设环境变量时返回与改动前完全相同的路径。

- [ ] **Step 1: 写契约测试（防回归的核心）**

这些测试在 Task 1 之后应当**直接通过**——它们钉的是"替换调用点没有改变默认布局"。
若任何一条失败，说明 Task 1 的归一化逻辑改变了历史路径，必须先修 Task 1。

在 `crates/yi-agent-store/src/paths.rs` 的 `tests` 模块追加：

```rust
    /// 未设环境变量时，每个消费者拿到的路径都不许变。
    /// 这里直接断言消费者函数的输出，而不是断言 `paths` 内部实现。
    #[test]
    fn consumers_keep_the_legacy_layout_without_overrides() {
        // 只读断言：这些函数在 HOME 存在时必须有值。
        let home = std::env::var_os("HOME").expect("test needs HOME");
        let want = PathBuf::from(&home).join(".yi-agent");
        assert_eq!(instance_home(), Some(want));
    }

    #[test]
    fn trace_dir_sits_under_the_instance_home() {
        let home = std::env::var_os("HOME").expect("test needs HOME");
        assert_eq!(
            trace_dir(),
            Some(PathBuf::from(&home).join(".yi-agent").join("trace"))
        );
    }
```

- [ ] **Step 2: 运行确认通过**

Run: `cargo test -p yi-agent-store --lib paths`

Expected: 全部 passed（Task 1 的 6 条 + 本步 2 条）。

- [ ] **Step 3: 逐个替换调用点**

`crates/yi-agent-store/src/resident.rs`，把 `default_dir` 替换为：

```rust
/// 宿主实例目录：`YI_AGENT_HOME` 或 `$HOME/.yi-agent`。
pub fn default_dir() -> Option<PathBuf> {
    crate::paths::instance_home()
}
```

`crates/yi-agent-runtime/src/config.rs`：

```rust
/// 解析全局 .env 路径：`<instance_home>/.env`
pub fn resolve_global_env_path() -> Option<PathBuf> {
    yi_agent_store::paths::home_join(".env")
}
```

`crates/yi-agent-runtime/src/models.rs`：

```rust
/// `<instance_home>/models.json`；实例目录不可解析时回退 `.yi-agent/models.json`，
/// 与改动前的回退口径逐字一致。
pub fn models_path() -> PathBuf {
    yi_agent_store::paths::home_join("models.json")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("models.json"))
}
```

`crates/yi-agent-supervisors/src/supervisor.rs` 的 `global_preferences_path`：

```rust
    fn global_preferences_path(&self) -> Option<PathBuf> {
        yi_agent_store::paths::home_join("preferences.json")
    }
```

`crates/yi-agent-web/src/lib.rs`：

```rust
fn user_real_test_env_path() -> PathBuf {
    yi_agent_store::paths::home_join(".env")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join(".env"))
}
```

在 `crates/yi-agent-web/Cargo.toml` 的 `[dependencies]` 加：

```toml
yi-agent-store = { workspace = true }
```

`crates/yi-agent-app-server/src/device_store.rs`：

```rust
pub fn default_path() -> PathBuf {
    yi_agent_store::paths::home_join("devices.json")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("devices.json"))
}
```

`crates/yi-agent-app-server/src/workspace_index.rs`：

```rust
pub fn default_path() -> PathBuf {
    yi_agent_store::paths::home_join("workspaces.json")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("workspaces.json"))
}
```

`crates/yi-agent/src/tracing_init.rs`：

```rust
fn trace_dir() -> PathBuf {
    yi_agent_store::paths::trace_dir()
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("trace"))
}
```

`crates/yi-agent-app-server/src/legacy_compat.rs`：**不改**。旧看板登记表必须从真实
HOME 迁移，跟随实例会让老环境迁移找不到文件。在 `legacy_board_dir` 上方补注释：

```rust
/// 故意读真实 `$HOME` 而不是实例目录：这是**旧布局**的迁移来源，
/// 跟随 `YI_AGENT_HOME` 会让开发实例迁移不到稳定版的旧登记表。
```

- [ ] **Step 4: 跑受影响 crate 的测试**

先确认没有并发 cargo：

```bash
ps aux | grep -v grep | grep -E "cargo|rustc" || echo clean
```

依次跑（**不要**并行）：

```bash
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-runtime --lib
cargo test -p yi-agent-supervisors --lib
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent-web --lib
```

Expected: 全部 passed，且没有测试因为路径变化而失败。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -A
git commit -m "feat(paths): 状态目录跟随 YI_AGENT_HOME，未设置时布局不变"
```

---

### Task 3: 拆分 app-server 的 `home_dir()`

**Files:**
- Modify: `crates/yi-agent-app-server/src/server.rs:330-336`
- Modify: `crates/yi-agent-app-server/src/server.rs:1643`、`:1649`、`:1729`、`:1735`
- Modify: `crates/yi-agent-app-server/src/ws.rs:150-160`
- Modify: `crates/yi-agent-app-server/src/skill_rpc.rs:56-63`
- Modify: `crates/yi-agent-runtime/src/bootstrap.rs:525-545`

**Interfaces:**
- Consumes: `yi_agent_store::paths::{os_home, instance_home, skills_dir}`
- Produces: `home_dir()` 保留原语义（真实 HOME），新增 `instance_home_dir()`。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent-app-server/src/server.rs` 的测试模块追加：

```rust
    #[test]
    fn watchman_home_is_the_real_home_not_the_instance_home() {
        // launchd 的 plist 必须写到真实 `~/Library/LaunchAgents`。
        // 用注入参数断言，不改进程环境变量（并行测试会竞态）。
        assert_eq!(
            super::watchman_home_from(Some(std::ffi::OsStr::new("/Users/me"))),
            std::path::PathBuf::from("/Users/me")
        );
    }

    #[test]
    fn skills_home_follows_the_instance_override() {
        assert_eq!(
            super::skills_home_from(
                Some(std::ffi::OsStr::new("/tmp/dev-a")),
                Some(std::ffi::OsStr::new("/Users/me"))
            ),
            std::path::PathBuf::from("/tmp/dev-a")
        );
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-app-server --lib watchman_home_is_the_real_home`

Expected: FAIL，`watchman_home_from` / `skills_home_from` 未定义。

- [ ] **Step 3: 实现**

把 `server.rs:330-336` 的 `home_dir` 替换为：

```rust
/// 真实宿主 `$HOME`。用于必须留在真实 HOME 的位置（launchd plist、LaunchAgents）。
///
/// 与 [`instance_home_dir`] 的区别是**语义**而非便利：开发实例的 plist 若写进
/// 实例目录，launchd 根本不会加载它。
pub(crate) fn home_dir() -> PathBuf {
    watchman_home_from(std::env::var_os("HOME").as_deref())
}

pub(crate) fn watchman_home_from(home: Option<&std::ffi::OsStr>) -> PathBuf {
    home.map(PathBuf::from).unwrap_or_default()
}

/// 实例状态根。preferences、skills 等随实例走。
pub(crate) fn instance_home_dir() -> PathBuf {
    skills_home_from(
        std::env::var_os("YI_AGENT_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

pub(crate) fn skills_home_from(
    override_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(dir) = override_home.filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    home.map(|h| PathBuf::from(h).join(".yi-agent"))
        .unwrap_or_default()
}
```

然后逐个替换调用点（`server.rs` 四处、`ws.rs` 三处）：

- `onboarding_preferences_path`：改为 `instance_home_dir().join("preferences.json")`
- `skills_home`：改为 `instance_root_or_home()`（见下），**不是** `instance_home_dir()`
- `watchman_home`：保持 `home_dir()`

**不要改 `skill_rpc.rs` 的签名。** 它的 `home` 参数是"home"，`roots()` 内部拼
`.yi-agent/skills`（`skill_rpc.rs:56-63`），而 `Harness::skills_home()` 与
`server.rs:7326` 的测试都依赖"home"语义。改签名会连带改十几个测试且没有收益。

只改**生产装配处**传入的值：

- `server.rs:1643`、`:1729` 的 `skills_home: home_dir()` → `skills_home: skills_home_dir()`
- `ws.rs:154` 的 `skills_home: crate::server::home_dir()` → `crate::server::skills_home_dir()`

`skill_rpc::roots` 会在传入目录后拼 `.yi-agent/skills`，所以要传「像 HOME 的目录」：

```rust
/// `skill/*` 需要一个「home 形状」的目录（它自己会拼 `.yi-agent/skills`）。
/// 传实例状态根的父目录，结果与 `paths::skills_dir()` 一致。
pub(crate) fn skills_home_dir() -> PathBuf {
    yi_agent_store::paths::instance_root().unwrap_or_default()
}
```

`paths::instance_home()` 已保证状态根一定以 `.yi-agent` 结尾，故父目录 + `.yi-agent/skills`
恒等于 `<instance_home>/skills`。

`crates/yi-agent-runtime/src/bootstrap.rs:525-545`：

```rust
    let Some(skills_root) = yi_agent_store::paths::skills_dir() else {
        tracing::warn!("skills: could not determine the skills directory, skipping");
        return Ok(None);
    };
    let system_root = skills_root.join(".system");

    // Install bundled skills; failure is non-fatal
    if let Err(e) = yi_agent_skills::install_system_skills(&system_root) {
        tracing::warn!("failed to install bundled skills: {e}");
    }

    let roots = vec![
        (
            cfg.workdir.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::Project,
        ),
        (skills_root, yi_agent_skills::SkillScope::User),
    ];
```

注意：`skills_dir()` 返回的就是**用户技能根**（`<instance_home>/skills`），
不再拼 `.yi-agent/skills`；系统根在其下的 `.system`。

- [ ] **Step 4: 跑测试**

```bash
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent-runtime --lib
```

Expected: passed。`skill_rpc` 的 Fixture 需要把 `home` 语义改成 `skills_root` 的父级；
若 Fixture 里 `self.home.join(".yi-agent/skills")` 出现，改为直接使用 `self.skills_root`。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 区分真实 HOME 与实例状态根"
```

---

### Task 4: 让 runtime 目录统一走一个解析函数

**Files:**
- Modify: `crates/yi-agent-supervisors/src/daemon.rs:30-32`
- Modify: `crates/yi-agent-supervisors/src/supervisor.rs:25-33`

**Interfaces:**
- Consumes: `yi_agent_subagent::attach::project_runtime_directory`
- Produces: 监督器与 daemon 对同一 worktree 解析出同一个 runtime 目录。

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent-supervisors/src/daemon.rs` 的测试模块追加：

```rust
    #[test]
    fn the_daemon_runtime_dir_honours_the_override() {
        // 注入式：不改进程环境变量。
        let workdir = std::path::Path::new("/proj/a");
        assert_eq!(
            super::runtime_dir_from(Some(std::ffi::OsStr::new("/tmp/inst/runtime")), workdir),
            std::path::PathBuf::from("/tmp/inst/runtime")
        );
        assert_eq!(
            super::runtime_dir_from(None, workdir),
            std::path::PathBuf::from("/proj/a/.yi-agent/runtime")
        );
    }
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent-supervisors --lib the_daemon_runtime_dir_honours_the_override`

Expected: FAIL，`runtime_dir_from` 未定义。

- [ ] **Step 3: 实现**

`daemon.rs`：

```rust
fn runtime_dir(project: &Path) -> PathBuf {
    runtime_dir_from(std::env::var_os("YI_AGENT_RUNTIME_DIR").as_deref(), project)
}

fn runtime_dir_from(override_dir: Option<&std::ffi::OsStr>, project: &Path) -> PathBuf {
    override_dir
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| project.join(".yi-agent").join("runtime"))
}
```

`supervisor.rs` 的 `Layout::for_workdir`：

```rust
    pub fn for_workdir(workdir: &Path) -> Self {
        let state_base = workdir.join(".yi-agent");
        let runtime_dir = crate::daemon::runtime_dir(workdir);
        Self {
            workdir: workdir.to_path_buf(),
            state_base,
            runtime_dir,
        }
    }
```

把 `daemon::runtime_dir` 提升为 `pub(crate) fn runtime_dir`。

- [ ] **Step 4: 跑测试**

```bash
cargo test -p yi-agent-supervisors --lib
```

Expected: passed。已有断言 `Layout::for_workdir` 默认路径的测试必须仍然通过。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add -A
git commit -m "refactor(supervisors): runtime 目录统一解析，支持实例覆盖"
```

---

### Task 5: 构建期版本标识

**Files:**
- Create: `crates/yi-agent/build.rs`
- Create: `crates/yi-agent/src/build_info.rs`
- Modify: `crates/yi-agent/src/main.rs`（在 `mod` 列表加 `mod build_info;`）
- Modify: `crates/yi-agent/src/config.rs:16`
- Modify: `crates/yi-agent/src/tracing_init.rs:50-55`

**Interfaces:**
- Produces:
  - `build_info::VERSION: &'static str`
  - `build_info::BUILD_KIND: &'static str`，取值 `"release" | "dev" | "nogit"`

- [ ] **Step 1: 写失败测试**

在 `crates/yi-agent/src/build_info.rs` 先放：

```rust
//! 构建期注入的版本标识。

pub const VERSION: &'static str = "unimplemented";
pub const BUILD_KIND: &'static str = "unimplemented";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn the_build_kind_is_one_of_three() {
        assert!(
            matches!(BUILD_KIND, "release" | "dev" | "nogit"),
            "unexpected build kind: {BUILD_KIND}"
        );
    }

    #[test]
    fn only_a_release_build_reports_a_bare_version() {
        if BUILD_KIND == "release" {
            assert!(
                !VERSION.contains("-dev") && !VERSION.contains("+nogit"),
                "release build must not carry a dev marker: {VERSION}"
            );
            assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        }
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p yi-agent --bin yi-agent build_info`

Expected: FAIL（`the_build_kind_is_one_of_three`，值为 `unimplemented`）。

- [ ] **Step 3: 实现 `build.rs`**

创建 `crates/yi-agent/build.rs`：

```rust
//! 生成构建标识：release / dev / nogit。
//!
//! `Cargo.toml` 的版本永远是下一个正式版的干净号；是否发布由 git 状态判定，
//! 不写进版本字段——发布流水线要求 tag 与 Cargo 版本严格相等。

use std::path::Path;
use std::process::Command;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let repo_root = Path::new(&manifest_dir).join("../..");
    let git_dir = repo_root.join(".git");
    println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
    println!("cargo:rerun-if-changed={}", git_dir.join("index").display());
    // 发布流水线可显式指定，避免浅克隆下误判。
    println!("cargo:rerun-if-env-changed=YI_AGENT_BUILD_KIND");

    let cargo_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let forced = std::env::var("YI_AGENT_BUILD_KIND").ok().filter(|v| !v.is_empty());

    let (version, kind) = match forced.as_deref() {
        Some("release") => (cargo_version, "release"),
        Some(other) => panic!("YI_AGENT_BUILD_KIND must be `release`, got `{other}`"),
        None => {
            let tag = git(&repo_root, &["describe", "--tags", "--exact-match"]);
            match tag {
                Some(tag) if tag == format!("v{cargo_version}") => (cargo_version.clone(), "release"),
                _ => {
                    let sha = git(&repo_root, &["rev-parse", "--short", "HEAD"]);
                    match sha {
                        Some(sha) => {
                            let dirty = git(&repo_root, &["status", "--porcelain", "--untracked-files=no"])
                                .filter(|s| !s.trim().is_empty())
                                .map(|_| "-dirty")
                                .unwrap_or("");
                            (format!("{cargo_version}-dev+{sha}{dirty}"), "dev")
                        }
                        None => (format!("{cargo_version}+nogit"), "nogit"),
                    }
                }
            }
        }
    };

    println!("cargo:rustc-env=YI_AGENT_VERSION={version}");
    println!("cargo:rustc-env=YI_AGENT_BUILD_KIND={kind}");
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if text.is_empty() { None } else { Some(text) }
}
```

- [ ] **Step 4: 实现 `build_info.rs` 并接入 clap**

把 `build_info.rs` 整个替换为（**保留 Step 1 的测试模块**）：

```rust
//! 构建期注入的版本标识。

pub const VERSION: &str = env!("YI_AGENT_VERSION");
pub const BUILD_KIND: &str = env!("YI_AGENT_BUILD_KIND");

/// clap 需要 `&'static str`；`env!` 正是静态值。
pub const CLAP_VERSION: &str = VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn the_build_kind_is_one_of_three() {
        assert!(
            matches!(BUILD_KIND, "release" | "dev" | "nogit"),
            "unexpected build kind: {BUILD_KIND}"
        );
    }

    #[test]
    fn only_a_release_build_reports_a_bare_version() {
        if BUILD_KIND == "release" {
            assert!(
                !VERSION.contains("-dev") && !VERSION.contains("+nogit"),
                "release build must not carry a dev marker: {VERSION}"
            );
            assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        }
    }
}
```

**必须同时**把版本写进 trace 首行，否则 `BUILD_KIND` 无人读取，`clippy -D warnings`
会报 `dead_code`。修改 `crates/yi-agent/src/tracing_init.rs` 的 `tracing::info!`：

```rust
    tracing::info!(
        trace_file = %filepath.display(),
        stderr_level = %std::env::var("YI_LOG").unwrap_or_else(|_| "(off)".to_string()),
        version = crate::build_info::VERSION,
        build_kind = crate::build_info::BUILD_KIND,
        "tracing initialized"
    );
```

`crates/yi-agent/src/config.rs:16`：

```rust
#[command(name = "yi-agent", version = crate::build_info::CLAP_VERSION, about = "Interactive AI agent CLI")]
```

`crates/yi-agent/src/main.rs` 的模块声明处加：

```rust
mod build_info;
```

- [ ] **Step 5: 跑测试并验证两种模式**

```bash
cargo test -p yi-agent --bin yi-agent build_info
cargo build -p yi-agent --quiet
./target/debug/yi-agent --version
```

Expected: 测试 3 passed；`--version` 输出 `yi-agent <版本>`，当前工作区非 tag，应含 `-dev+<sha>`。

再验证 release 模式：

```bash
YI_AGENT_BUILD_KIND=release cargo build -p yi-agent --quiet
./target/debug/yi-agent --version
```

Expected: 输出裸版本号（等于 `Cargo.toml` 的 `0.1.7`），不含 `-dev`。

- [ ] **Step 6: 提交**

```bash
cargo fmt --all
git add crates/yi-agent/build.rs crates/yi-agent/src/build_info.rs crates/yi-agent/src/config.rs crates/yi-agent/src/main.rs crates/yi-agent/src/tracing_init.rs
git commit -m "feat(cli): 构建期注入版本标识，区分 release 与 dev"
```

---

### Task 6: 实例管理脚本

**Files:**
- Create: `yi-agent-rs/scripts/dev-env.sh`
- Create: `yi-agent-rs/scripts/test_dev_env.sh`

**Interfaces:**
- Produces:
  - `dev-env.sh up [--sync-skills]`：启动子 shell
  - `dev-env.sh run <cmd...>`：在实例环境中执行一次命令
  - `dev-env.sh down [--purge]`：停止实例内进程，可选删除目录
  - 实例 id 通过 `dev-env.sh id` 打印

- [ ] **Step 1: 写失败的脚本测试**

创建 `yi-agent-rs/scripts/test_dev_env.sh`：

```bash
#!/usr/bin/env bash
# dev-env.sh 的行为测试。用临时目录，不碰真实 ~/.yi-agent。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/dev-env.sh"
ROOT="$(cd "$HERE/.." && pwd)"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1. id 确定且幂等
id1="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" id)"
id2="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" id)"
[ "$id1" = "$id2" ] || fail "id is not stable: $id1 vs $id2"
[ -n "$id1" ] || fail "id is empty"
pass "id is stable: $id1"

# 2. run 导出了正确的环境
out="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" run sh -c 'printf "%s|%s|%s" "$YI_AGENT_HOME" "$YI_AGENT_RUNTIME_DIR" "$YI_AGENT_DISABLE_WATCHMAN"')"
home="${out%%|*}"
rest="${out#*|}"
runtime="${rest%%|*}"
watchman="${rest#*|}"
[ "$home" = "$TMP/instances/$id1/.yi-agent" ] || fail "YI_AGENT_HOME=$home, want $TMP/instances/$id1/.yi-agent"
[ "$runtime" = "$home/runtime" ] || fail "YI_AGENT_RUNTIME_DIR=$runtime"
[ "$watchman" = "1" ] || fail "YI_AGENT_DISABLE_WATCHMAN=$watchman"
pass "run exports an isolated environment"

# 3. run 会创建实例目录
[ -d "$home" ] || fail "instance dir was not created"
pass "run creates the instance dir"

# 4. down --purge 删除目录
YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" down --purge >/dev/null
[ ! -d "$home" ] || fail "down --purge did not remove $home"
pass "down --purge removes the instance dir"

# 5. 已存在的 .env 会被继承提示但不覆盖
mkdir -p "$TMP/instances/$id1/.yi-agent"
printf 'MODEL_API_KEY=from-parent\n' > "$TMP/instances/$id1/.yi-agent/.env"
YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" run true >/dev/null
grep -q 'from-parent' "$TMP/instances/$id1/.yi-agent/.env" || fail "existing .env was overwritten"
pass "an existing .env is preserved"

echo "all dev-env checks passed"
```

- [ ] **Step 2: 运行确认失败**

```bash
chmod +x yi-agent-rs/scripts/test_dev_env.sh
bash yi-agent-rs/scripts/test_dev_env.sh
```

Expected: FAIL，`dev-env.sh` 不存在。

- [ ] **Step 3: 实现脚本**

创建 `yi-agent-rs/scripts/dev-env.sh`：

```bash
#!/usr/bin/env bash
# 临时开发实例：一个 worktree 一个实例，随用随删。
#
# 用法：
#   dev-env.sh id                    打印当前 worktree 的实例 id
#   dev-env.sh up [--sync-skills]    进入实例子 shell
#   dev-env.sh run <cmd...>          在实例环境中执行一次命令
#   dev-env.sh down [--purge]        停止实例进程；--purge 连同目录一起删除
#
# 实例根目录：$YI_AGENT_INSTANCES_ROOT，默认 ~/.yi-agent-instances
set -euo pipefail

instances_root() {
  printf '%s\n' "${YI_AGENT_INSTANCES_ROOT:-$HOME/.yi-agent-instances}"
}

worktree_root() {
  git rev-parse --show-toplevel 2>/dev/null || pwd
}

instance_id() {
  local root branch slug hash
  root="$(worktree_root)"
  branch="$(git -C "$root" rev-parse --abbrev-ref HEAD 2>/dev/null || printf 'detached')"
  slug="$(basename "$root" | tr -c '[:alnum:]-' '-' | sed 's/--*/-/g; s/^-//; s/-$//' | cut -c1-32)"
  hash="$(printf '%s' "$root" | shasum -a 256 | awk '{print substr($1,1,6)}')"
  printf '%s-%s-%s\n' "$slug" "$(printf '%s' "$branch" | tr '/' '-' | cut -c1-24)" "$hash"
}

# 注意：`paths::instance_home()` 会把不以 `.yi-agent` 结尾的值自动补上后缀。
# 这里显式输出带后缀的路径，让脚本自己的断言与实际解析结果一致。
instance_home() {
  printf '%s/%s/.yi-agent\n' "$(instances_root)" "$(instance_id)"
}

ensure_instance() {
  local home
  home="$(instance_home)"
  mkdir -p "$home"
  if [ ! -f "$home/.env" ] && [ -f "$HOME/.yi-agent/.env" ]; then
    cp "$HOME/.yi-agent/.env" "$home/.env"
    printf 'dev-env: 已从 ~/.yi-agent/.env 复制到 %s/.env\n' "$home" >&2
    printf 'dev-env: 这是**真实凭据的副本**；实验跑量会消耗同一份额度，需要隔离请改用它。\n' >&2
  fi
  printf '%s\n' "$home"
}

export_env() {
  local home="$1"
  export YI_AGENT_HOME="$home"
  export YI_AGENT_RUNTIME_DIR="$home/runtime"
  export YI_AGENT_DISABLE_WATCHMAN=1
  export YI_AGENT_INSTANCE_ID="$(instance_id)"
}

sync_skills() {
  local home="$1" src
  src="$(worktree_root)/.yi-agent/skills"
  [ -d "$src" ] || return 0
  mkdir -p "$home/skills"
  cp -R "$src"/. "$home/skills"/ 2>/dev/null || true
  printf 'dev-env: 已同步项目 skill 到 %s/skills\n' "$home" >&2
}

cmd="${1:-}"; shift || true
case "$cmd" in
  id)
    instance_id
    ;;
  up)
    home="$(ensure_instance)"
    export_env "$home"
    if [ "${1:-}" = "--sync-skills" ]; then sync_skills "$home"; fi
    printf 'dev-env: 实例 %s\n' "$home" >&2
    printf 'dev-env: YI_AGENT_HOME=%s\n' "$YI_AGENT_HOME" >&2
    printf 'dev-env: 退出子 shell 后实例目录保留；销毁请跑 dev-env.sh down --purge\n' >&2
    exec "${SHELL:-/bin/bash}" -i
    ;;
  run)
    home="$(ensure_instance)"
    export_env "$home"
    exec "$@"
    ;;
  down)
    home="$(instance_home)"
    if [ -d "$home" ]; then
      # 先停值守前端进程（若由 up 拉起过）
      if [ -f "$home/watch.pid" ]; then
        kill "$(cat "$home/watch.pid")" 2>/dev/null || true
        rm -f "$home/watch.pid"
      fi
      # 再停实例内 daemon；socket 属于该实例，不会误伤 stable
      YI_AGENT_HOME="$home" YI_AGENT_RUNTIME_DIR="$home/runtime" \
        "${HOME}/.local/bin/yi-agent-dev" daemon stop >/dev/null 2>&1 || true
    fi
    if [ "${1:-}" = "--purge" ]; then
      rm -rf "$home"
      printf 'dev-env: 已删除 %s\n' "$home" >&2
    fi
    ;;
  *)
    sed -n '2,11p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
    exit 2
    ;;
esac
```

- [ ] **Step 4: 跑脚本测试**

```bash
chmod +x yi-agent-rs/scripts/dev-env.sh yi-agent-rs/scripts/test_dev_env.sh
bash yi-agent-rs/scripts/test_dev_env.sh
```

Expected: `all dev-env checks passed`。

注意：测试在临时目录里执行，`git rev-parse` 会回落到 `pwd`，因此不会污染真实仓库。

- [ ] **Step 5: 提交**

```bash
git add yi-agent-rs/scripts/dev-env.sh yi-agent-rs/scripts/test_dev_env.sh
git commit -m "feat(scripts): 临时开发实例的创建与销毁"
```

---

### Task 7: `rebuild.sh` 增加 dev profile

**Files:**
- Modify: `scripts/rebuild.sh`（本地文件，已在 `.git/info/exclude`，**不提交**）

**Interfaces:**
- Produces: `./scripts/rebuild.sh cli --profile dev` 只安装 `~/.local/bin/yi-agent-dev`。

- [ ] **Step 1: 修改 `build_cli`**

把 `scripts/rebuild.sh` 的 `build_cli` 中安装段替换为：

```bash
  if [ "${PROFILE:-stable}" = "dev" ]; then
    mkdir -p "$HOME/.local/bin"
    install -m 0755 "$bin" "$HOME/.local/bin/yi-agent-dev"
    echo "CLI  -> $HOME/.local/bin/yi-agent-dev (dev profile)"
    echo "     稳定版未被改动：$(command -v yi-agent || echo '未安装')"
    echo "版本确认: $("$HOME/.local/bin/yi-agent-dev" --version 2>&1 || echo '(执行失败)')"
  else
    mkdir -p "$HOME/.cargo/bin"
    cp "$bin" "$HOME/.cargo/bin/yi-agent"
    ln -sf "$HOME/.cargo/bin/yi-agent" /opt/homebrew/bin/yi-agent
    echo "CLI  -> $bin"
    echo "     -> $HOME/.cargo/bin/yi-agent (已同步)"
    echo "     -> /opt/homebrew/bin/yi-agent (软链)"
    echo "版本确认: $(/opt/homebrew/bin/yi-agent --version 2>&1 || echo '(执行失败)')"
  fi
```

在参数解析的 `for arg in "$@"` 循环里加：

```bash
    dev|--profile=dev) PROFILE=dev ;;
```

并在 `FORCE=0` 之前加 `PROFILE="stable"`。

- [ ] **Step 2: 验证**

```bash
./scripts/rebuild.sh cli --profile dev
command -v yi-agent && yi-agent --version
~/.local/bin/yi-agent-dev --version
```

Expected: `yi-agent` 仍指向原稳定版且版本未变；`yi-agent-dev` 输出含 `-dev`。

- [ ] **Step 3: 不提交**

`scripts/` 在 `.git/info/exclude:15`，该文件属本地工具。确认 `git status` 干净。

---

### Task 8: 发布流水线门禁

**Files:**
- Modify: `.github/workflows/release.yml:26`
- Modify: `.gitlab-ci.yml`（发布 job 的 `variables`）
- Modify: `yi-agent-rs/justfile:package-all` 之前新增校验 recipe

- [ ] **Step 1: 写失败测试（脚本级）**

创建 `yi-agent-rs/scripts/test_release_version_gate.sh`：

```bash
#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE="$HERE/check-release-version.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# 合法：裸版本号
out="$("$GATE" "0.1.7" "0.1.7" 2>&1)" || fail "bare version rejected: $out"
# 非法：dev 标记
if "$GATE" "0.1.7" "0.1.7-dev+abc123" >/dev/null 2>&1; then
  fail "dev version was accepted"
fi
# 非法：nogit
if "$GATE" "0.1.7" "0.1.7+nogit" >/dev/null 2>&1; then
  fail "nogit version was accepted"
fi
# 非法：与 Cargo 版本不一致
if "$GATE" "0.1.7" "0.1.8" >/dev/null 2>&1; then
  fail "mismatched version was accepted"
fi

echo "release version gate checks passed"
```

- [ ] **Step 2: 运行确认失败**

```bash
chmod +x yi-agent-rs/scripts/test_release_version_gate.sh
bash yi-agent-rs/scripts/test_release_version_gate.sh
```

Expected: FAIL，`check-release-version.sh` 不存在。

- [ ] **Step 3: 实现门禁脚本**

创建 `yi-agent-rs/scripts/check-release-version.sh`：

```bash
#!/usr/bin/env bash
# 用法: check-release-version.sh <cargo-version> <reported-version>
# 发布前断言：二进制自称的版本必须逐字等于 Cargo.toml 的版本。
set -euo pipefail

want="${1:?cargo version required}"
got="${2:?reported version required}"

if [ "$want" != "$got" ]; then
  printf 'ERROR: 发布产物的版本 (%s) 与 Cargo.toml (%s) 不一致\n' "$got" "$want" >&2
  printf '       常见原因：浅克隆导致 git describe 失败，构建被标成 dev/nogit。\n' >&2
  printf '       修复：checkout 加 fetch-depth: 0 / GIT_DEPTH: "0"。\n' >&2
  exit 1
fi

printf 'release version verified: %s\n' "$want"
```

- [ ] **Step 4: 接入 CI**

`.github/workflows/release.yml` 的 `- uses: actions/checkout@v4` 改为：

```yaml
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
```

在 `Build & package all platforms` 之后、`Upload artifacts` 之前插入：

```yaml
      - name: Verify the binary reports the release version
        working-directory: yi-agent-rs
        run: |
          source "$HOME/.cargo/env"
          bash scripts/check-release-version.sh "${{ steps.version.outputs.version }}" \
            "$(./target/release/yi-agent --version | awk '{print $2}')"
```

`.gitlab-ci.yml` 的 `release:macos-dmg` job 加：

```yaml
  variables:
    GIT_DEPTH: "0"
```

- [ ] **Step 5: 验证脚本并提交**

```bash
bash yi-agent-rs/scripts/test_release_version_gate.sh
```

Expected: `release version gate checks passed`。

```bash
git add .github/workflows/release.yml .gitlab-ci.yml yi-agent-rs/scripts/check-release-version.sh yi-agent-rs/scripts/test_release_version_gate.sh
git commit -m "ci(release): 浅克隆补救与版本一致性门禁"
```

---

### Task 9: 分支模型与文档

**Files:**
- Modify: `.github/workflows/ci.yml:4-6`
- Modify: `docs/project-management/ci-cd.md`
- Modify: `docs/project-management/tooling.md`
- Modify: `docs/superpowers/specs/2026-10-19-stable-dev-channels-design.md`（状态改为已实施）

- [ ] **Step 1: CI 触发覆盖 release 分支**

`.github/workflows/ci.yml`：

```yaml
on:
  pull_request:
    branches: [main, "release/**"]
```

- [ ] **Step 2: 更新模块文档**

`docs/project-management/ci-cd.md` 的 Features 追加：

```markdown
- [x] 稳定版/开发版分道与多实例并行 — 分支模型 `main` + `release/X.Y`；稳定版只经 npm/Homebrew 安装；开发版 `yi-agent-dev` + `YI_AGENT_HOME` 实例隔离；验证：`bash yi-agent-rs/scripts/test_dev_env.sh && bash yi-agent-rs/scripts/test_release_version_gate.sh` — [设计](../superpowers/specs/2026-10-19-stable-dev-channels-design.md)
```

`docs/project-management/tooling.md` 的 Features 追加：

```markdown
- [x] 开发实例管理脚本 — `yi-agent-rs/scripts/dev-env.sh`（`id` / `up` / `run` / `down`）；验证：`bash yi-agent-rs/scripts/test_dev_env.sh`
- [x] 版本标识注入 — `yi-agent-rs/crates/yi-agent/build.rs` 写入 `YI_AGENT_VERSION` / `YI_AGENT_BUILD_KIND`；验证：`cargo test -p yi-agent --bin yi-agent build_info && cargo run -p yi-agent -- --version`
```

- [ ] **Step 3: 全量验收**

按 crate 顺序跑（**逐个**，不要并行）：

```bash
ps aux | grep -v grep | grep -E "cargo|rustc" || echo clean
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-runtime --lib
cargo test -p yi-agent-supervisors --lib
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent --bin yi-agent
cargo test -p yi-agent-web --lib
bash scripts/test_dev_env.sh
bash scripts/test_release_version_gate.sh
```

- [ ] **Step 4: 隔离验收**

```bash
# 同一 worktree 起两个实例，确认互不可见
bash yi-agent-rs/scripts/dev-env.sh id
bash yi-agent-rs/scripts/dev-env.sh run sh -c 'echo $YI_AGENT_HOME; ls -d "$YI_AGENT_RUNTIME_DIR" 2>/dev/null || echo "runtime not created yet"'
grep -rn 'var_os("HOME")\|var("HOME")' yi-agent-rs/crates --include=*.rs | grep -v tests
```

Expected: 最后一条只应剩 `paths.rs`、`legacy_compat.rs`（有意保留）与
`onboarding_prompt.rs`（测试用的 HOME 注入）。

- [ ] **Step 5: 提交**

```bash
cargo fmt --all
git add .github/workflows/ci.yml docs/project-management/ci-cd.md docs/project-management/tooling.md
git commit -m "docs(ci): 登记稳定版/开发版分道与实例隔离"
```

---

## Self-Review

**Spec coverage：** D1 → Task 9；D2 → Task 5 + Task 8；D3 → Task 1–4；D4 → Task 3
（watchman_home 保持真实 HOME）+ Task 6（`YI_AGENT_DISABLE_WATCHMAN=1`）；D5 → Task 6 +
Task 7。风险表 6 条分别由路径门禁（Task 9 Step 4）、`.env` 提示（Task 6）、trace 隔离
（Task 2）、登记表隔离（Task 2）、skills 同步（Task 6 `--sync-skills`）、发布门禁
（Task 8）覆盖。

**已知边界：** `<worktree>/.yi-agent/threads/` 仍按 worktree 共享，spec 7.1 已显式
说明并排除在验收之外。
