# Superpowers 看板 — 闭环接线 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 打通看板闭环——daemon 按开关托管插件进程，两端能「加入看板」把卡片投递进队列，插件消费投递并入队推进，两端实时显示卡片。

**Architecture:** 新增宿主侧通用 crate `yi-agent-supervisors`（按清单 + 开关键托管子进程，不含看板逻辑）；`daemon serve` 内跑监督循环。卡片投递走**文件目录**（宿主写 `inbox/*.json`，插件消费后删），零新增 IPC。两端各自读插件写出的 `board.json`：TUI 直接调用，Desktop 经 app-server 新增的 4 条 board RPC。

**Tech Stack:** Rust 2024（`serde`/`serde_json`）、Tauri 2 + React/TS（desktop）、既有 daemon IPC。

## Global Constraints

- 插件进程保持**零 `yi-agent-*` 依赖**（它自带 cargo workspace）。
- 宿主与插件**不共享 `board.json` 的写权**：宿主只写 `inbox/*.json`，插件只写 `board.json`。
- 所有既有文件写入沿用 **temp + rename 原子替换**；所有读取遇到缺失/损坏**回退默认，绝不 panic**。
- **开关键名逐字 `superpowers_board`**（布尔，写在 `preferences.json` 顶层）；两层解析：**项目覆盖全局，两层都缺省→关闭**。
- `yi-agent-supervisors` 是**通用**能力：只认清单里的通用字段（命令/参数/开关键/state-dir），**不得**出现 "kanban"/"board" 字样。
- 关闭开关只**停止推进与监督**，**绝不**取消已在 daemon 中运行的会话。
- 提交信息用 conventional commits，**不写** `Co-Authored-By`；每次提交前 `cargo fmt --all`。
- Rust 命令：`cd yi-agent-rs && cargo test -p <crate>`；插件：`cd plugins/superpowers-board && cargo test`；desktop：`cd desktop && npx tsc --noEmit && npm test`。
- **desktop 组件测试约定**：文件首行 `/** @vitest-environment jsdom */`，且每个测试文件 `afterEach(() => cleanup())`。
- 新增 crate 必须加入 `yi-agent-rs/Cargo.toml` 的 `members`。

**目录约定（本次固定）：**
- `workdir` = 项目根。
- `state_dir` = `<workdir>/.yi-agent/board`（放 `board.json`、`inbox/`）。
- `runtime_dir` = `<workdir>/.yi-agent/runtime`（复用 `yi_agent_subagent::attach::project_runtime_directory`）。
- 清单目录 = `<workdir>/.yi-agent/supervisors/`。

---

## 文件结构

| 文件 | 职责 |
|------|------|
| `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml` | 新宿主侧 crate |
| `.../src/lib.rs` | 导出 |
| `.../src/manifest.rs` | 清单解析 + `{workdir}`/`{state_dir}`/`{runtime_dir}` 占位展开 |
| `.../src/switch.rs` | 通用「按 JSON 布尔键」两层开关解析（**通用**，与看板无关） |
| `.../src/supervisor.rs` | 监督循环：对齐期望/实际、拉起、停止、退避重启、退出回收 |
| `.../tests/supervisor.rs` | 用假子进程的集成测试 |
| `yi-agent-rs/crates/yi-agent/src/main.rs` | `daemon serve` 内起监督循环 |
| `yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs` | 宿主侧：写投递文件（原子）、路径助手 |
| `plugins/superpowers-board/crates/board-runner/src/inbox.rs` | 插件侧：消费投递、校验成对、入队、拒绝归档 |
| `plugins/superpowers-board/crates/board-runner/src/main.rs` | 每 tick 先消费 inbox |
| `plugins/superpowers-board/kanban.toml` | 时段并发示例配置 |
| `plugins/superpowers-board/supervisors/superpowers-board.json` | 随插件提供的清单样例 |
| `yi-agent-rs/crates/yi-agent/src/tui/board.rs` | `/kanban` 读实时卡片 + `add <spec> <plan>` |
| `yi-agent-rs/crates/yi-agent/src/tui/slash.rs` | `Kanban` 参数用法补 `add` |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | 4 条 board RPC |
| `desktop/src/App.tsx` | 挂载 `BoardView` + `SettingsPanel` |

---

### Task 1: 监督器 crate — 清单解析与占位展开

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs`
- Modify: `yi-agent-rs/Cargo.toml`（`members` 追加一行）

**Interfaces:**
- Produces:
  - `yi_agent_supervisors::manifest::SupervisorManifest { name: String, command: String, args: Vec<String>, switch_key: String, restart_backoff_ms: u64, restart_backoff_max_ms: u64 }`
  - `SupervisorManifest::parse(json: &str) -> Result<SupervisorManifest, ManifestError>`
  - `SupervisorManifest::expand_args(&self, workdir: &Path, state_dir: &Path, runtime_dir: &Path) -> Vec<String>`
  - `yi_agent_supervisors::manifest::load_manifests(dir: &Path) -> Vec<SupervisorManifest>`（缺失目录→空；单个坏清单跳过）

- [x] **Step 1: 写失败的测试**

`crates/yi-agent-supervisors/src/manifest.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SAMPLE: &str = r#"{
        "name": "demo",
        "command": "demo-bin",
        "args": ["--state", "{state_dir}", "--root", "{workdir}", "--rt", "{runtime_dir}"],
        "switch_key": "demo_on",
        "restart_backoff_ms": 500,
        "restart_backoff_max_ms": 4000
    }"#;

    #[test]
    fn a_manifest_parses_with_all_fields() {
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.command, "demo-bin");
        assert_eq!(manifest.switch_key, "demo_on");
        assert_eq!(manifest.restart_backoff_ms, 500);
        assert_eq!(manifest.restart_backoff_max_ms, 4000);
    }

    #[test]
    fn placeholders_expand_to_the_given_directories() {
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        let args = manifest.expand_args(
            Path::new("/proj"),
            Path::new("/proj/.yi-agent/board"),
            Path::new("/proj/.yi-agent/runtime"),
        );
        assert_eq!(
            args,
            vec![
                "--state",
                "/proj/.yi-agent/board",
                "--root",
                "/proj",
                "--rt",
                "/proj/.yi-agent/runtime",
            ]
        );
    }

    #[test]
    fn defaults_apply_when_optional_fields_are_absent() {
        let json = r#"{"name":"m","command":"c","args":[],"switch_key":"k"}"#;
        let manifest = SupervisorManifest::parse(json).unwrap();
        assert_eq!(manifest.restart_backoff_ms, 1000);
        assert_eq!(manifest.restart_backoff_max_ms, 30000);
    }

    #[test]
    fn a_manifest_missing_a_required_field_is_rejected() {
        let json = r#"{"name":"m","args":[],"switch_key":"k"}"#;
        assert!(SupervisorManifest::parse(json).is_err());
    }

    #[test]
    fn load_manifests_skips_broken_files_and_sorts_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.json"), SAMPLE.replace("demo", "b")).unwrap();
        std::fs::write(dir.path().join("a.json"), SAMPLE.replace("demo", "a")).unwrap();
        std::fs::write(dir.path().join("broken.json"), "{ not json").unwrap();
        let names: Vec<String> = load_manifests(dir.path())
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_missing_directory_yields_no_manifests() {
        assert!(load_manifests(Path::new("/definitely/not/here")).is_empty());
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --offline`
Expected: 编译失败（crate 不存在 / 类型未定义）。

- [x] **Step 3: 写最小实现**

`crates/yi-agent-supervisors/Cargo.toml`：

```toml
[package]
name = "yi-agent-supervisors"
description = "Generic switch-driven child-process supervision for the yi-agent daemon"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
tempfile = "3"
```

`crates/yi-agent-supervisors/src/lib.rs`：

```rust
//! 通用的「按开关托管子进程」能力。
//!
//! 本 crate 不认识任何具体插件：它只读清单里的通用字段（命令、参数、开关键、
//! state-dir 占位）并负责拉起/守护/停止。任何自主场景都可复用它。

pub mod manifest;
pub mod supervisor;
pub mod switch;
```

`crates/yi-agent-supervisors/src/manifest.rs`：

```rust
use std::path::Path;

use serde::Deserialize;

/// 清单解析失败的原因。
#[derive(Debug)]
pub enum ManifestError {
    Json(serde_json::Error),
    EmptyName,
    EmptyCommand,
    EmptySwitchKey,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Json(error) => write!(f, "invalid manifest json: {error}"),
            ManifestError::EmptyName => write!(f, "manifest needs a non-empty name"),
            ManifestError::EmptyCommand => write!(f, "manifest needs a non-empty command"),
            ManifestError::EmptySwitchKey => write!(f, "manifest needs a non-empty switch_key"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// 一个被托管进程的声明。字段皆通用，不含任何插件语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorManifest {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub switch_key: String,
    pub restart_backoff_ms: u64,
    pub restart_backoff_max_ms: u64,
}

const DEFAULT_BACKOFF_MS: u64 = 1000;
const DEFAULT_BACKOFF_MAX_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    switch_key: String,
    restart_backoff_ms: Option<u64>,
    restart_backoff_max_ms: Option<u64>,
}

impl SupervisorManifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest =
            serde_json::from_str(json).map_err(ManifestError::Json)?;
        if raw.name.is_empty() {
            return Err(ManifestError::EmptyName);
        }
        if raw.command.is_empty() {
            return Err(ManifestError::EmptyCommand);
        }
        if raw.switch_key.is_empty() {
            return Err(ManifestError::EmptySwitchKey);
        }
        Ok(Self {
            name: raw.name,
            command: raw.command,
            args: raw.args,
            switch_key: raw.switch_key,
            restart_backoff_ms: raw.restart_backoff_ms.unwrap_or(DEFAULT_BACKOFF_MS),
            restart_backoff_max_ms: raw.restart_backoff_max_ms.unwrap_or(DEFAULT_BACKOFF_MAX_MS),
        })
    }

    /// 展开 `{workdir}` / `{state_dir}` / `{runtime_dir}` 占位。
    pub fn expand_args(&self, workdir: &Path, state_dir: &Path, runtime_dir: &Path) -> Vec<String> {
        let expand = |arg: &str| {
            arg.replace("{workdir}", &workdir.to_string_lossy())
                .replace("{state_dir}", &state_dir.to_string_lossy())
                .replace("{runtime_dir}", &runtime_dir.to_string_lossy())
        };
        self.args.iter().map(|arg| expand(arg)).collect()
    }
}

/// 读清单目录：目录缺失→空；单个文件坏→跳过并记 warning（绝不 panic）。
/// 结果按 `name` 排序，保证调用方看到稳定的顺序。
pub fn load_manifests(dir: &Path) -> Vec<SupervisorManifest> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match SupervisorManifest::parse(&text) {
                Ok(manifest) => manifests.push(manifest),
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "skipping malformed supervisor manifest"
                ),
            },
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not read supervisor manifest"
            ),
        }
    }
    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    manifests
}
```

在 `yi-agent-rs/Cargo.toml` 的 `members` 数组里，`"crates/yi-agent-board-ui",` 之后追加：

```toml
    "crates/yi-agent-supervisors",
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --offline`
Expected: PASS（manifest 6 个测试）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/Cargo.toml yi-agent-rs/Cargo.lock yi-agent-rs/crates/yi-agent-supervisors
git commit -m "feat(supervisors): parse supervisor manifests with placeholder expansion"
```

---

### Task 2: 监督器 crate — 通用两层开关解析

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/switch.rs`

**Interfaces:**
- Consumes: 无
- Produces:
  - `yi_agent_supervisors::switch::read_bool_key(path: &Path, key: &str) -> Option<bool>`
  - `yi_agent_supervisors::switch::resolve_bool(global: Option<bool>, project: Option<bool>) -> bool`

> 这是**通用**的「按 JSON 布尔键读两层开关」，与看板无关——不依赖 `yi-agent-board-ui`。

- [x] **Step 1: 写失败的测试**

`crates/yi-agent-supervisors/src/switch.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_wins_over_global() {
        assert!(!resolve_bool(Some(true), Some(false)));
        assert!(resolve_bool(Some(false), Some(true)));
    }

    #[test]
    fn an_unset_project_layer_inherits_global() {
        assert!(resolve_bool(Some(true), None));
        assert!(!resolve_bool(Some(false), None));
    }

    #[test]
    fn both_unset_defaults_to_off() {
        assert!(!resolve_bool(None, None));
    }

    #[test]
    fn read_bool_key_returns_the_typed_value_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"demo_on":true,"other":"x"}"#).unwrap();
        assert_eq!(read_bool_key(&path, "demo_on"), Some(true));
        assert_eq!(read_bool_key(&path, "other"), None);
        assert_eq!(read_bool_key(&path, "missing"), None);
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_unset() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_bool_key(&dir.path().join("nope.json"), "k"), None);
        let broken = dir.path().join("preferences.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert_eq!(read_bool_key(&broken, "k"), None);
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors switch --offline`
Expected: 编译失败（`read_bool_key` / `resolve_bool` 未定义）。

- [x] **Step 3: 写最小实现**

`crates/yi-agent-supervisors/src/switch.rs`：

```rust
use std::path::Path;

use serde_json::Value;

/// 从一个 JSON 文件里读顶层布尔键。缺失、不可读、损坏或类型不符一律 `None`。
pub fn read_bool_key(path: &Path, key: &str) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value.get(key).and_then(Value::as_bool)
}

/// 两层解析：项目层显式设置则用项目层，否则用全局层，两层都缺省则关闭。
pub fn resolve_bool(global: Option<bool>, project: Option<bool>) -> bool {
    project.or(global).unwrap_or(false)
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors switch --offline`
Expected: PASS（5 个测试）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-supervisors/src/switch.rs
git commit -m "feat(supervisors): resolve a generic two-layer boolean switch"
```

---

### Task 3: 监督器 crate — 监督循环（拉起/停止/退避重启/回收）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs`（导出 `supervisor`）
- Create: `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs`

**Interfaces:**
- Consumes: Task 1 的 `load_manifests` / `expand_args`；Task 2 的 `read_bool_key` / `resolve_bool`
- Produces:
  - `yi_agent_supervisors::supervisor::Layout { workdir: PathBuf, state_dir: PathBuf, runtime_dir: PathBuf }`
  - `Layout::for_workdir(workdir: &Path) -> Layout`
  - `Layout::manifests_dir(&self) -> PathBuf`
  - `Supervisor::new(layout: Layout) -> Supervisor`
  - `Supervisor::desired_on(&self, manifest: &SupervisorManifest) -> bool`
  - `Supervisor::reconcile(&mut self)`（一次对齐）
  - `Supervisor::stop_all(&mut self)`

- [x] **Step 1: 写失败的测试**

`crates/yi-agent-supervisors/tests/supervisor.rs`：

```rust
use std::path::Path;
use std::time::{Duration, Instant};

use yi_agent_supervisors::supervisor::{Layout, Supervisor};

/// 写一个假子进程：它在 `marker` 处写自己的 pid，然后睡到被杀。
fn write_fake_child(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let script = dir.join("child.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", marker.display()),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

fn write_manifest(dir: &Path, name: &str, command: &Path, switch_key: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = format!(
        r#"{{"name":"{name}","command":"{}","args":[],"switch_key":"{switch_key}","restart_backoff_ms":50,"restart_backoff_max_ms":200}}"#,
        command.display()
    );
    std::fs::write(dir.join(format!("{name}.json")), manifest).unwrap();
}

fn layout_for(workdir: &Path) -> Layout {
    Layout::for_workdir(workdir)
}

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn a_switch_on_spawns_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    // 打开项目层开关
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for(&marker, Duration::from_secs(2)), "child should start");
    supervisor.stop_all();
}

#[test]
fn a_switch_off_keeps_the_child_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    std::thread::sleep(Duration::from_millis(150));
    assert!(!marker.exists(), "child must not start while the switch is off");
    supervisor.stop_all();
}

#[test]
fn turning_the_switch_off_stops_a_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    let prefs = workdir.join(".yi-agent/preferences.json");
    std::fs::write(&prefs, r#"{"demo_on":true}"#).unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for(&marker, Duration::from_secs(2)), "child should start");

    std::fs::write(&prefs, r#"{"demo_on":false}"#).unwrap();
    supervisor.reconcile();
    assert!(
        supervisor.running_count() == 0,
        "turning the switch off must stop the child"
    );
}

#[test]
fn a_crashed_child_is_restarted_after_the_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let counter = workdir.join("starts");
    // 每次启动把计数 +1，然后立即退出（模拟崩溃）。
    let child = workdir.join("crash.sh");
    std::fs::write(
        &child,
        format!(
            "#!/bin/sh\nprintf x >> {}\nexit 1\n",
            counter.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
    }
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(20));
        if std::fs::read_to_string(&counter).map(|s| s.len()).unwrap_or(0) >= 3 {
            break;
        }
    }
    supervisor.stop_all();
    let starts = std::fs::read_to_string(&counter).unwrap_or_default().len();
    assert!(starts >= 3, "a crashing child must be restarted, got {starts}");
}

#[test]
fn stop_all_reaps_every_child() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for(&marker, Duration::from_secs(2)));
    supervisor.stop_all();
    assert_eq!(supervisor.running_count(), 0);
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --test supervisor --offline`
Expected: 编译失败（`supervisor` 模块 / `Supervisor` 未定义）。

- [x] **Step 3: 写最小实现**

`crates/yi-agent-supervisors/src/lib.rs` 追加 `pub mod supervisor;`（使其为 `manifest` / `supervisor` / `switch`）。

`crates/yi-agent-supervisors/src/supervisor.rs`：

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::manifest::{SupervisorManifest, load_manifests};
use crate::switch::{read_bool_key, resolve_bool};

/// 监督运行所需的目录布局。目录名对外固定，方便清单的占位展开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub workdir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Layout {
    pub fn for_workdir(workdir: &Path) -> Self {
        let state_dir = workdir.join(".yi-agent").join("board");
        let runtime_dir = workdir.join(".yi-agent").join("runtime");
        Self {
            workdir: workdir.to_path_buf(),
            state_dir,
            runtime_dir,
        }
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.workdir.join(".yi-agent").join("supervisors")
    }

    fn global_preferences_path(&self) -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| PathBuf::from(home).join(".yi-agent").join("preferences.json"))
    }

    fn project_preferences_path(&self) -> PathBuf {
        self.workdir.join(".yi-agent").join("preferences.json")
    }
}

struct RunningChild {
    child: Child,
    last_started: Instant,
}

/// 按清单 + 开关键托管子进程。生命周期与 daemon 一致。
pub struct Supervisor {
    layout: Layout,
    running: BTreeMap<String, RunningChild>,
    /// 一次 `spawn` 的暂存槽：先在无借用冲突处起进程，再搬进 `running`。
    spawned: Option<Child>,
}

impl Supervisor {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            running: BTreeMap::new(),
            spawned: None,
        }
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// 生效开关：项目层覆盖全局层，两层都缺省→关闭。
    pub fn desired_on(&self, manifest: &SupervisorManifest) -> bool {
        let global = self
            .layout
            .global_preferences_path()
            .and_then(|path| read_bool_key(&path, &manifest.switch_key));
        let project = read_bool_key(
            &self.layout.project_preferences_path(),
            &manifest.switch_key,
        );
        resolve_bool(global, project)
    }

    /// 一次对齐：重新扫描清单，起应起、停应停。
    ///
    /// 崩溃的子进程按清单的退避参数重启；除「开关关闭」外不在本次 reconcile 里主动杀进程
    /// （`stop_all` 负责 daemon 退出时的回收）。
    pub fn reconcile(&mut self) {
        let manifests = load_manifests(&self.layout.manifests_dir());
        let known: std::collections::BTreeSet<String> =
            manifests.iter().map(|m| m.name.clone()).collect();

        // 清单被删除 → 停止并移除。
        let stale: Vec<String> = self
            .running
            .keys()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            self.stop(&name);
        }

        for manifest in manifests {
            let on = self.desired_on(&manifest);
            if !on {
                self.stop(&manifest.name);
                continue;
            }
            self.ensure_running(&manifest);
        }
    }

    fn ensure_running(&mut self, manifest: &SupervisorManifest) {
        if let Some(running) = self.running.get_mut(&manifest.name) {
            match running.child.try_wait() {
                Ok(None) => return, // 仍在运行
                Ok(Some(_)) | Err(_) => {
                    // 已退出：按退避重启。
                    let backoff = Duration::from_millis(manifest.restart_backoff_ms);
                    if running.last_started.elapsed() < backoff {
                        return;
                    }
                }
            }
        }
        if self.spawn(manifest).is_ok() {
            if let Some(child) = self.spawned.take() {
                self.running.insert(
                    manifest.name.clone(),
                    RunningChild {
                        child,
                        last_started: Instant::now(),
                    },
                );
            }
        }
    }

    fn spawn(&mut self, manifest: &SupervisorManifest) -> std::io::Result<()> {
        let args = manifest.expand_args(
            &self.layout.workdir,
            &self.layout.state_dir,
            &self.layout.runtime_dir,
        );
        let child = Command::new(&manifest.command)
            .args(args)
            .current_dir(&self.layout.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.spawned = Some(child);
        Ok(())
    }

    fn stop(&mut self, name: &str) {
        if let Some(mut running) = self.running.remove(name) {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// daemon 退出时回收全部子进程。
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.running.keys().cloned().collect();
        for name in names {
            self.stop(&name);
        }
    }
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-supervisors --offline`
Expected: PASS（manifest 6 + switch 5 + supervisor 5）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-supervisors
git commit -m "feat(supervisors): supervise switch-gated child processes"
```

---

### Task 4: daemon 集成 — `daemon serve` 内跑监督循环

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`（加依赖）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`daemon serve` 起监督线程）
- Test: `yi-agent-rs/crates/yi-agent/src/main.rs`（同文件 `#[cfg(test)]`）

**Interfaces:**
- Consumes: Task 3 的 `Layout` / `Supervisor`
- Produces: `fn serve_supervisor(workdir: &std::path::Path)` —— 在后台线程里按固定间隔 `reconcile`，返回一个停止句柄（`Arc<AtomicBool>`），供测试与退出回收使用。

- [x] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent/src/main.rs` 的测试模块里追加（`use` 放测试模块内）：

```rust
    #[test]
    fn the_supervisor_loop_starts_a_manifest_when_its_switch_is_on() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path();
        // 假子进程：写标记后睡。
        let marker = workdir.join("child.pid");
        let child = workdir.join("child.sh");
        std::fs::write(
            &child,
            format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&child).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&child, perms).unwrap();
        }
        let manifests = workdir.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("demo.json"),
            format!(
                r#"{{"name":"demo","command":"{}","args":[],"switch_key":"demo_on","restart_backoff_ms":50,"restart_backoff_max_ms":200}}"#,
                child.display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
        std::fs::write(
            workdir.join(".yi-agent/preferences.json"),
            r#"{"demo_on":true}"#,
        )
        .unwrap();

        let handle = serve_supervisor(workdir);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut started = false;
        while Instant::now() < deadline {
            if marker.exists() {
                started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(started, "the supervisor loop should have started the child");
    }
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent the_supervisor_loop_starts --offline`
Expected: 编译失败（`serve_supervisor` 未定义）。

- [x] **Step 3: 写最小实现**

`yi-agent-rs/crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里追加：

```toml
yi-agent-supervisors = { path = "../yi-agent-supervisors" }
```

在 `main.rs` 里（`control_daemon` 附近）新增：

```rust
/// 在后台线程里按固定间隔对齐托管子进程。返回停止句柄：置位后循环退出并
/// `stop_all`，用于测试与 daemon 退出时的回收。
pub(crate) struct SupervisorHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl SupervisorHandle {
    pub(crate) fn stop(mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub(crate) fn serve_supervisor(workdir: &std::path::Path) -> SupervisorHandle {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let layout = yi_agent_supervisors::supervisor::Layout::for_workdir(workdir);
    let join = std::thread::spawn(move || {
        let mut supervisor = yi_agent_supervisors::supervisor::Supervisor::new(layout);
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            supervisor.reconcile();
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        supervisor.stop_all();
    });
    SupervisorHandle {
        stop,
        join: Some(join),
    }
}
```

在 `DaemonAction::Serve` 分支里，`Daemon::start_with_factory(...)` 成功之后、`daemon.wait()` 之前，起监督循环并在退出时回收：

```rust
            let supervisor = serve_supervisor(&workdir);
            report_reclaimed_orphans_to_stderr(daemon.reclaimed_orphans());
            let result = daemon
                .wait()
                .map_err(|error| anyhow::anyhow!("runtime daemon failed: {error}"));
            supervisor.stop();
            result
```

（即把原来那句 `daemon.wait().map_err(...)` 替换为上面的四行。）

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent the_supervisor_loop_starts --offline`
Expected: PASS（1 个测试）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/Cargo.lock yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(daemon): supervise plugin processes while the daemon serves"
```

---

### Task 5: 宿主侧投递 — `yi-agent-board-ui::inbox`

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs`
- Modify: `yi-agent-rs/crates/yi-agent-board-ui/src/lib.rs`（`pub mod inbox;`）
- Create: `yi-agent-rs/crates/yi-agent-board-ui/tests/inbox.rs`

**Interfaces:**
- Consumes: 既有 `state::board_state_path` 所在的 `state_dir` 约定
- Produces:
  - `yi_agent_board_ui::inbox::board_state_dir(workdir: &Path) -> PathBuf`（= `<workdir>/.yi-agent/board`）
  - `yi_agent_board_ui::inbox::inbox_dir(state_dir: &Path) -> PathBuf`（= `<state_dir>/inbox`）
  - `yi_agent_board_ui::inbox::enqueue_path(state_dir: &Path, id: &str) -> PathBuf`
  - `yi_agent_board_ui::inbox::deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()>`

- [x] **Step 1: 写失败的测试**

`crates/yi-agent-board-ui/tests/inbox.rs`：

```rust
use std::path::Path;

use yi_agent_board_ui::inbox::{board_state_dir, deliver_card, enqueue_path, inbox_dir};

#[test]
fn the_state_dir_sits_beside_the_other_plugin_state() {
    assert_eq!(
        board_state_dir(Path::new("/proj")),
        Path::new("/proj/.yi-agent/board")
    );
    assert_eq!(
        inbox_dir(Path::new("/proj/.yi-agent/board")),
        Path::new("/proj/.yi-agent/board/inbox")
    );
    assert_eq!(
        enqueue_path(Path::new("/proj/.yi-agent/board"), "card-1"),
        Path::new("/proj/.yi-agent/board/inbox/card-1.json")
    );
}

#[test]
fn delivering_a_card_writes_one_json_file() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    let text = std::fs::read_to_string(enqueue_path(dir.path(), "card-1")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["id"], "card-1");
    assert_eq!(value["spec_path"], "a.spec.md");
    assert_eq!(value["plan_path"], "a.plan.md");
}

#[test]
fn delivering_is_idempotent_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    deliver_card(dir.path(), "card-1", "b.spec.md", "b.plan.md").unwrap();
    let entries: Vec<_> = std::fs::read_dir(inbox_dir(dir.path()))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["card-1.json".to_string()]);
    assert!(!dir.path().join("inbox/card-1.json.tmp").exists());
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui --test inbox --offline`
Expected: 编译失败（`inbox` 模块未定义）。

- [x] **Step 3: 写最小实现**

`crates/yi-agent-board-ui/src/lib.rs` 追加 `pub mod inbox;`（保持字母序：`inbox` / `state` / `switch` / `view`）。

`crates/yi-agent-board-ui/src/inbox.rs`：

```rust
use std::path::{Path, PathBuf};

/// 插件状态目录：`<workdir>/.yi-agent/board`。`board.json` 与 `inbox/` 都在这里。
pub fn board_state_dir(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("board")
}

/// 投递目录：`<state_dir>/inbox`。
pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

/// 一张卡片的投递文件：`<state_dir>/inbox/<id>.json`。
pub fn enqueue_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.json"))
}

/// 把「加入看板」写成一个投递文件。宿主只写这里，从不碰 `board.json`。
///
/// 同 id 覆盖（幂等）；temp + rename 原子替换，读者永远看不到半个文件。
pub fn deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({
        "id": id,
        "spec_path": spec,
        "plan_path": plan,
    });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-board-ui --test inbox --offline`
Expected: PASS（3 个测试）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent-board-ui/src/lib.rs yi-agent-rs/crates/yi-agent-board-ui/src/inbox.rs yi-agent-rs/crates/yi-agent-board-ui/tests/inbox.rs
git commit -m "feat(board-ui): deliver cards into the plugin's inbox"
```

---

### Task 6: 插件侧 — `board-runner` 消费投递并入队

**Files:**
- Create: `plugins/superpowers-board/crates/board-runner/src/inbox.rs`
- Modify: `plugins/superpowers-board/crates/board-runner/src/lib.rs`（`pub mod inbox;`）
- Modify: `plugins/superpowers-board/crates/board-runner/src/main.rs`（每 tick 先消费）
- Test: `plugins/superpowers-board/crates/board-runner/src/inbox.rs`（同文件 `#[cfg(test)]`）

**Interfaces:**
- Consumes: `board_core::board::{Board, CardId}`、`board_core::promotion::validate_promotion`
- Produces:
  - `board_runner::inbox::consume(state_dir: &Path, board: &mut Board, now: DateTime<Local>) -> Vec<InboxOutcome>`
  - `board_runner::inbox::InboxOutcome { id: String, result: Result<(), String> }`

- [x] **Step 1: 写失败的测试**

`plugins/superpowers-board/crates/board-runner/src/inbox.rs` 末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at() -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap()
    }

    fn deliver(state_dir: &std::path::Path, id: &str, spec: &str, plan: &str) {
        let dir = state_dir.join("inbox");
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({"id": id, "spec_path": spec, "plan_path": plan});
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_valid_delivery_is_enqueued_and_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        deliver(dir.path(), "card-1", spec.to_str().unwrap(), plan.to_str().unwrap());

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok());
        assert_eq!(board.len(), 1, "the card is queued");
        assert!(
            !dir.path().join("inbox/card-1.json").exists(),
            "the delivery file is consumed"
        );
    }

    #[test]
    fn an_incomplete_pair_is_rejected_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        std::fs::write(&spec, "# spec").unwrap();
        // plan 缺失
        deliver(dir.path(), "card-2", spec.to_str().unwrap(), "/nope/a.plan.md");

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert_eq!(board.len(), 0, "nothing is queued");
        assert!(
            dir.path().join("inbox/rejected/card-2.json").exists(),
            "the rejected delivery is kept for inspection"
        );
        assert!(!dir.path().join("inbox/card-2.json").exists());
    }

    #[test]
    fn a_redelivery_of_a_known_card_does_not_enqueue_twice() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        let mut board = Board::new();

        deliver(dir.path(), "card-1", spec.to_str().unwrap(), plan.to_str().unwrap());
        consume(dir.path(), &mut board, at());
        deliver(dir.path(), "card-1", spec.to_str().unwrap(), plan.to_str().unwrap());
        consume(dir.path(), &mut board, at());

        assert_eq!(board.len(), 1, "the same card id is not queued twice");
    }

    #[test]
    fn a_corrupt_delivery_is_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("bad.json"), "{ not json").unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(board.len(), 0);
        assert!(
            dir.path().join("inbox/rejected/bad.json").exists(),
            "a corrupt delivery is archived, not lost"
        );
        assert!(!outcomes.is_empty());
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-runner inbox --offline`
Expected: 编译失败（`consume` / `InboxOutcome` 未定义）。

- [x] **Step 3: 写最小实现**

`crates/board-runner/src/lib.rs` 追加 `pub mod inbox;`。

`crates/board-runner/src/inbox.rs`：

```rust
use std::path::Path;

use board_core::board::{Board, CardId};
use board_core::promotion::validate_promotion;
use chrono::{DateTime, Local};

/// 一张投递文件的处理结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxOutcome {
    pub id: String,
    /// `Ok(())` = 已入队；`Err(原因)` = 已归档到 `inbox/rejected/`。
    pub result: Result<(), String>,
}

#[derive(serde::Deserialize)]
struct RawDelivery {
    id: String,
    spec_path: String,
    plan_path: String,
}

/// 消费 `<state_dir>/inbox` 下的全部投递：校验成对 → 入队 → 删除；
/// 校验失败或损坏 → 移到 `inbox/rejected/` 并记录原因（绝不静默丢弃、绝不 panic）。
pub fn consume(state_dir: &Path, board: &mut Board, now: DateTime<Local>) -> Vec<InboxOutcome> {
    let inbox = state_dir.join("inbox");
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    paths.sort();
    for path in paths {
        let id_from_file = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                outcomes.push(InboxOutcome {
                    id: id_from_file,
                    result: Err(format!("could not read delivery: {error}")),
                });
                continue;
            }
        };
        let delivery = match serde_json::from_str::<RawDelivery>(&text) {
            Ok(delivery) => delivery,
            Err(error) => {
                reject(&inbox, &path, &id_from_file);
                outcomes.push(InboxOutcome {
                    id: id_from_file,
                    result: Err(format!("invalid delivery json: {error}")),
                });
                continue;
            }
        };
        let id = CardId::new(delivery.id.clone());
        if board.get(&id).is_some() {
            // 幂等：同 id 已在队列里，直接消费投递即可。
            let _ = std::fs::remove_file(&path);
            outcomes.push(InboxOutcome {
                id: delivery.id,
                result: Ok(()),
            });
            continue;
        }
        if let Err(error) = validate_promotion(
            Path::new(&delivery.spec_path),
            Path::new(&delivery.plan_path),
        ) {
            reject(&inbox, &path, &delivery.id);
            outcomes.push(InboxOutcome {
                id: delivery.id,
                result: Err(error.to_string()),
            });
            continue;
        }
        board.enqueue(
            id,
            delivery.spec_path.into(),
            delivery.plan_path.into(),
            now,
        );
        let _ = std::fs::remove_file(&path);
        outcomes.push(InboxOutcome {
            id: delivery.id,
            result: Ok(()),
        });
    }
    outcomes
}

fn reject(inbox: &Path, path: &Path, id: &str) {
    let rejected = inbox.join("rejected");
    if std::fs::create_dir_all(&rejected).is_err() {
        return;
    }
    let target = rejected.join(format!("{id}.json"));
    let _ = std::fs::rename(path, target);
}
```

在 `crates/board-runner/src/main.rs` 的 tick 循环里，`let mut board = ...load_board(...)` 之后、`let limit = ...` 之前插入：

```rust
        for outcome in board_runner::inbox::consume(&args.state_dir, &mut board, chrono::Local::now()) {
            match outcome.result {
                Ok(()) => eprintln!("board-runner: enqueued {}", outcome.id),
                Err(reason) => {
                    eprintln!("board-runner: rejected {} ({reason})", outcome.id)
                }
            }
        }
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test --offline`
Expected: PASS（board-core 42 + board-ipc 14 + board-runner 15 + inbox 4）。

- [x] **Step 5: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board/crates/board-runner
git commit -m "feat(board-runner): consume the inbox and enqueue validated cards"
```

---

### Task 7: TUI — `/kanban` 显示实时卡片 + `add` 投递

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/board.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`（`Kanban` 的参数用法补 `add`）
- Test: `yi-agent-rs/crates/yi-agent/src/tui/board.rs`（同文件 `#[cfg(test)]`）

**Interfaces:**
- Consumes: Task 5 的 `yi_agent_board_ui::inbox::{board_state_dir, deliver_card}`；既有 `yi_agent_board_ui::state::load_cards`
- Produces: `handle_kanban(workdir, args)` 支持 `""`（显示实时卡片）、`on`、`off`、`add <spec> <plan>`

- [x] **Step 1: 写失败的测试**

在 `crates/yi-agent/src/tui/board.rs` 的测试模块追加：

```rust
    #[test]
    fn the_board_shows_cards_read_from_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            yi_agent_board_ui::state::board_state_path(&state_dir),
            r#"{"cards":[{"id":"card-1","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"running","enqueued_at":"2026-10-01T09:00:00+08:00","order":0}],"next_order":1}"#,
        )
        .unwrap();

        let outcome = handle_kanban(dir.path(), "");
        assert!(
            outcome.lines.iter().any(|line| line.contains("card-1")),
            "the live card should be rendered, got {:?}",
            outcome.lines
        );
        assert!(
            outcome.lines.iter().any(|line| line.contains("running")),
            "the card state should be visible, got {:?}",
            outcome.lines
        );
    }

    #[test]
    fn add_delivers_a_card_to_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(
            dir.path(),
            "add a.spec.md a.plan.md",
        );
        assert_eq!(outcome.toggled_to, None);
        assert!(
            outcome.lines[0].contains("inbox") || outcome.lines[0].contains("投递"),
            "expected a delivery acknowledgement, got {:?}",
            outcome.lines
        );
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        let entries: Vec<_> = std::fs::read_dir(yi_agent_board_ui::inbox::inbox_dir(&state_dir))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(entries.len(), 1, "exactly one delivery file is written");
    }

    #[test]
    fn add_with_missing_paths_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outcome = handle_kanban(dir.path(), "add only-one-arg");
        assert!(
            outcome.lines[0].contains("usage"),
            "expected a usage line, got {:?}",
            outcome.lines
        );
    }
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::board --offline`
Expected: FAIL（`add` 分支与实时卡片未实现）。

- [x] **Step 3: 写最小实现**

在 `crates/yi-agent/src/tui/board.rs` 里：

把 `""` 分支的 `cards: Vec::new()` 换成从状态文件读取：

```rust
        "" => {
            let state_dir = yi_agent_board_ui::inbox::board_state_dir(workdir);
            let cards = yi_agent_board_ui::state::load_cards(&state_dir);
            let view = yi_agent_board_ui::view::BoardView {
                switch_on: resolved.value.is_enabled(),
                switch_source: source,
                cards,
            };
            let mut lines = vec![view.header()];
            lines.extend(view.render_lines());
            KanbanOutcome {
                lines,
                toggled_to: None,
            }
        }
```

在 `match argument` 里新增 `add` 分支（放在 `"on" | "off"` 之后）：

```rust
        _ if argument.starts_with("add ") || argument == "add" => {
            let mut parts = argument.split_whitespace();
            let _verb = parts.next();
            match (parts.next(), parts.next(), parts.next()) {
                (Some(spec), Some(plan), None) => {
                    let state_dir = yi_agent_board_ui::inbox::board_state_dir(workdir);
                    let id = card_id_for(spec, plan);
                    match yi_agent_board_ui::inbox::deliver_card(&state_dir, &id, spec, plan) {
                        Ok(()) => KanbanOutcome {
                            lines: vec![format!(
                                "Superpowers 看板: delivered {id} to inbox"
                            )],
                            toggled_to: None,
                        },
                        Err(error) => KanbanOutcome {
                            lines: vec![format!("Superpowers 看板: could not deliver: {error}")],
                            toggled_to: None,
                        },
                    }
                }
                _ => KanbanOutcome {
                    lines: vec!["usage: /kanban add <spec> <plan>".to_string()],
                    toggled_to: None,
                },
            }
        }
```

并在文件底部加一个稳定的 id 生成函数（纯函数，便于测试）：

```rust
/// 由一对路径派生卡片 id：取两文件名主干，非字母数字折叠为 `-`。
fn card_id_for(spec: &str, plan: &str) -> String {
    let stem = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let slug = |text: &str| {
        let mut out = String::new();
        let mut last_dash = false;
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    };
    let id = format!("{}-{}", slug(&stem(spec)), slug(&stem(plan)));
    if id == "-" || id.is_empty() {
        "card".to_string()
    } else {
        id
    }
}
```

在 `slash.rs` 的 `Kanban` 参数用法里把 `"[on|off|run]"` 改成 `"[on|off|add <spec> <plan>]"`。

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent tui::board --offline && cargo test -p yi-agent --bin yi-agent slash --offline`
Expected: PASS（board 9 + slash 47）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add yi-agent-rs/crates/yi-agent/src/tui/board.rs yi-agent-rs/crates/yi-agent/src/tui/slash.rs
git commit -m "feat(tui): show live board cards and deliver cards from /kanban add"
```

---

### Task 8: Desktop — 4 条 board RPC + 挂载看板与设置

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（4 条 RPC）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（同文件 `#[cfg(test)]`）
- Modify: `desktop/src/lib/boardSwitch.ts`（增 RPC 包装）
- Modify: `desktop/src/App.tsx`（挂载 `BoardView` + `SettingsPanel`）

**Interfaces:**
- Consumes: Task 5 的 `inbox::{board_state_dir, deliver_card}`；既有 `state::load_cards`、`switch::{resolve, read_layer, project_path, write_layer}`
- Produces（app-server RPC）:
  - `board/list` → `{"cards":[{"id","state","progress","detail"}]}`
  - `board/enqueue`（params `{id?, spec_path, plan_path}`）→ `{"id"}`
  - `board/switch/read` → `{"on":bool,"source":"project"|"global"|"default"}`
  - `board/switch/write`（params `{on:bool}`）→ `{"on":bool}`

- [x] **Step 1: 写失败的测试**

在 `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的测试模块追加：

```rust
    #[test]
    fn board_list_reads_cards_from_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            yi_agent_board_ui::state::board_state_path(&state_dir),
            r#"{"cards":[{"id":"card-1","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"awaiting_merge","enqueued_at":"2026-10-01T09:00:00+08:00","order":0}],"next_order":1}"#,
        )
        .unwrap();

        let value = board_list(dir.path());
        let cards = value["cards"].as_array().unwrap();
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["id"], "card-1");
        assert_eq!(cards[0]["state"], "awaiting_merge");
    }

    #[test]
    fn board_enqueue_writes_a_delivery_file() {
        let dir = tempfile::tempdir().unwrap();
        let value = board_enqueue(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
        assert_eq!(value["id"], "card-1");
        let state_dir = yi_agent_board_ui::inbox::board_state_dir(dir.path());
        assert!(yi_agent_board_ui::inbox::enqueue_path(&state_dir, "card-1").exists());
    }

    #[test]
    fn board_switch_write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        board_switch_write(dir.path(), true).unwrap();
        let value = board_switch_read(dir.path());
        assert_eq!(value["on"], true);
        assert_eq!(value["source"], "project");
    }
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server board --offline`
Expected: 编译失败（`board_list` 等未定义）。

- [x] **Step 3: 写最小实现**

在 `server.rs` 里新增三个自由函数（供 RPC 与测试共用）：

```rust
/// `board/list`：读插件写出的 `board.json`，映射成可渲染的卡片数组。
fn board_list(workdir: &std::path::Path) -> serde_json::Value {
    let state_dir = yi_agent_board_ui::inbox::board_state_dir(workdir);
    let cards: Vec<serde_json::Value> = yi_agent_board_ui::state::load_cards(&state_dir)
        .into_iter()
        .map(|card| {
            serde_json::json!({
                "id": card.id,
                "state": card.state,
                "progress": card.progress,
                "detail": card.detail,
            })
        })
        .collect();
    serde_json::json!({ "cards": cards })
}

/// `board/enqueue`：把一张卡投递进插件的 inbox。
fn board_enqueue(
    workdir: &std::path::Path,
    id: &str,
    spec: &str,
    plan: &str,
) -> Result<serde_json::Value, String> {
    let state_dir = yi_agent_board_ui::inbox::board_state_dir(workdir);
    yi_agent_board_ui::inbox::deliver_card(&state_dir, id, spec, plan)
        .map_err(|error| error.to_string())?;
    Ok(serde_json::json!({ "id": id }))
}

/// `board/switch/read`：返回两层解析后的开关与来源。
fn board_switch_read(workdir: &std::path::Path) -> serde_json::Value {
    use yi_agent_board_ui::switch::{BoardSwitch, SwitchSource, global_path, project_path, read_layer, resolve};
    let project = read_layer(&project_path(workdir));
    let global = global_path().and_then(|path| read_layer(&path));
    let resolved = resolve(global, project);
    let source = match resolved.source {
        SwitchSource::Project => "project",
        SwitchSource::Global => "global",
        SwitchSource::Default => "default",
    };
    serde_json::json!({ "on": resolved.value.is_enabled(), "source": source })
}

/// `board/switch/write`：写项目层开关。
fn board_switch_write(workdir: &std::path::Path, on: bool) -> Result<serde_json::Value, String> {
    use yi_agent_board_ui::switch::{BoardSwitch, project_path, write_layer};
    let value = if on { BoardSwitch::Enabled } else { BoardSwitch::Disabled };
    write_layer(&project_path(workdir), value).map_err(|error| error.to_string())?;
    Ok(serde_json::json!({ "on": on }))
}
```

> **注意**：`switch.rs` 需已导出 `global_path` / `project_path` / `read_layer` / `write_layer` / `resolve` / `SwitchSource` / `BoardSwitch`——Plan 3b 已实现；若 `global_path` 未公开，改为 `pub`。

在 RPC 分派处（紧邻 `"config/read"` 分支）加入 4 条：

```rust
                    "board/list" => {
                        write_response(&writer, ok_response(id, board_list(&cfg.workdir))).await?;
                    }
                    "board/enqueue" => {
                        let p = &req.params;
                        let requested = p.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let spec = p.get("spec_path").and_then(|v| v.as_str()).unwrap_or("");
                        let plan = p.get("plan_path").and_then(|v| v.as_str()).unwrap_or("");
                        // 未给 id 时用「两文件主干」派生，保证重复投递同 id 幂等。
                        let card_id = if requested.is_empty() {
                            derive_card_id(spec, plan)
                        } else {
                            requested.to_string()
                        };
                        match board_enqueue(&cfg.workdir, &card_id, spec, plan) {
                            Ok(value) => write_response(&writer, ok_response(id, value)).await?,
                            Err(message) => write_response(
                                &writer,
                                err_response(id, RpcError::internal(message)),
                            )
                            .await?,
                        }
                    }
                    "board/switch/read" => {
                        write_response(&writer, ok_response(id, board_switch_read(&cfg.workdir)))
                            .await?;
                    }
                    "board/switch/write" => {
                        let on = req.params.get("on").and_then(|v| v.as_bool()).unwrap_or(false);
                        match board_switch_write(&cfg.workdir, on) {
                            Ok(value) => write_response(&writer, ok_response(id, value)).await?,
                            Err(message) => write_response(
                                &writer,
                                err_response(id, RpcError::internal(message)),
                            )
                            .await?,
                        }
                    }
```

> `id` 即该分支已有的响应 id 变量（与相邻 `"config/read"` 分支一致）；**不要**引入 `id_req` 之类的新名字。上面的 `derive_card_id(spec, plan)` 用与 Task 7 的 TUI 相同的「两文件名主干派生」规则，保证两端对同一对文件得到同一个卡片 id——在 `server.rs` 里把它实现为自由函数（与 Task 7 的 `card_id_for` 同逻辑）：

```rust
/// 由一对路径派生卡片 id（与 TUI 的 `/kanban add` 同规则，保证两端一致）。
fn derive_card_id(spec: &str, plan: &str) -> String {
    let stem = |path: &str| {
        std::path::Path::new(path)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default()
    };
    let slug = |text: &str| {
        let mut out = String::new();
        let mut last_dash = false;
        for ch in text.chars() {
            if ch.is_ascii_alphanumeric() {
                out.push(ch.to_ascii_lowercase());
                last_dash = false;
            } else if !last_dash {
                out.push('-');
                last_dash = true;
            }
        }
        out.trim_matches('-').to_string()
    };
    let id = format!("{}-{}", slug(&stem(spec)), slug(&stem(plan)));
    if id == "-" || id.is_empty() {
        "card".to_string()
    } else {
        id
    }
}
```

在 `desktop/src/lib/boardSwitch.ts` 追加（薄包装，经既有 `RpcClient.request` 调用）：

```ts
export interface BoardCardDto {
  id: string;
  state: string;
  progress: string | null;
  detail: string;
}

type BoardRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 经 app-server 读看板卡片（宿主侧读 board.json）。 */
export async function fetchBoard(rpc: BoardRpc): Promise<BoardCardDto[]> {
  const result = await rpc<{ cards?: BoardCardDto[] }>("board/list", {});
  return result?.cards ?? [];
}

/** 经 app-server 读两层解析后的开关与来源。 */
export async function readBoardSwitch(
  rpc: BoardRpc,
): Promise<{ on: boolean; source: string }> {
  return rpc<{ on: boolean; source: string }>("board/switch/read", {});
}

/** 经 app-server 写项目层开关。 */
export async function setBoardSwitch(rpc: BoardRpc, on: boolean): Promise<void> {
  await rpc("board/switch/write", { on });
}

/** 经 app-server 投递一张卡片。 */
export async function enqueueBoardCard(
  rpc: BoardRpc,
  specPath: string,
  planPath: string,
): Promise<void> {
  await rpc("board/enqueue", { spec_path: specPath, plan_path: planPath });
}
```

在 `desktop/src/App.tsx`：文件顶部的 import 区加：

```tsx
import { BoardView } from "./components/BoardView";
import { SettingsPanel } from "./components/SettingsPanel";
import {
  type BoardCardDto,
  fetchBoard,
  readBoardSwitch,
  setBoardSwitch,
} from "./lib/boardSwitch";
```

在 `export default function App() {` 的既有 state 声明之后（约在 `const [workspaces, ...]` 附近）加：

```tsx
  const [boardOn, setBoardOn] = useState(false);
  const [boardSource, setBoardSource] = useState<string>("default");
  const [boardCards, setBoardCards] = useState<BoardCardDto[]>([]);
```

在既有的 `useEffect` 初始化块里（`inited.current` 守卫的那个 effect 内，初始化 workspace 之后）加一次读取与轮询：

```tsx
      const refreshBoard = async () => {
        const client = clientRef.current;
        if (!client) return;
        try {
          const [sw, cards] = await Promise.all([
            readBoardSwitch(client.request.bind(client)),
            fetchBoard(client.request.bind(client)),
          ]);
          setBoardOn(sw.on);
          setBoardSource(sw.source);
          setBoardCards(cards);
        } catch {
          /* 看板读取失败绝不影响主流程 */
        }
      };
      void refreshBoard();
      const boardTimer = window.setInterval(refreshBoard, 2000);
```

并在同一 effect 的清理函数里加 `window.clearInterval(boardTimer);`。

在 `return (` 的布局里，把 `BoardView` 与 `SettingsPanel` 挂上（放在 `ThreadSidebar` 之后、聊天区之前的分栏内）：

```tsx
        <div className="flex w-72 flex-col border-r border-neutral-800">
          <SettingsPanel
            switchOn={boardOn}
            source={boardSource}
            onToggle={(next) => {
              const client = clientRef.current;
              if (!client) return;
              void setBoardSwitch(client.request.bind(client), next)
                .then(() => {
                  setBoardOn(next);
                  setBoardSource("project");
                })
                .catch(() => {
                  /* 写失败保持原状，下一轮轮询会纠正 */
                });
            }}
          />
          <BoardView switchOn={boardOn} source={boardSource} cards={boardCards} />
        </div>
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server board --offline`
Expected: PASS（3 个测试）。
Run: `cd desktop && npx tsc --noEmit && TMPDIR="$PWD/.tmpverify" npm test`
Expected: tsc 干净；测试全绿（含新增的 App 挂载断言）。

- [x] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
cd ../desktop && npx tsc --noEmit
cd .. && git add yi-agent-rs/crates/yi-agent-app-server desktop/src
git commit -m "feat(desktop): expose board RPCs and mount the board view"
```

---

### Task 9: 随插件提供示例配置与清单，并补安装说明

**Files:**
- Create: `plugins/superpowers-board/kanban.toml`
- Create: `plugins/superpowers-board/supervisors/superpowers-board.json`
- Create: `plugins/superpowers-board/README.md`
- Test: `plugins/superpowers-board/crates/board-runner/src/main.rs`（同文件测试：读示例 `kanban.toml` 得到预期上限）

**Interfaces:**
- Consumes: Task 1/3 的清单字段与 `board_core::calendar::ConcurrencyCalendar::from_toml`
- Produces: 示例 `kanban.toml`（工作日 09:00–24:00→3；工作日 00:00–09:00→10；周末全天→10）与清单样例

- [x] **Step 1: 写失败的测试**

在 `plugins/superpowers-board/crates/board-runner/src/main.rs` 末尾**新建**测试模块（该文件当前没有测试模块）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sample_calendar_expresses_the_three_and_ten_windows() {
        use board_core::calendar::ConcurrencyCalendar;
        let text = include_str!("../../../kanban.toml");
        let calendar = ConcurrencyCalendar::from_toml(text).unwrap();
        use chrono::{Datelike, Local, TimeZone, Weekday};
        let at = |y, m, d, h| Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        // 2026-10-01 是周四；2026-10-03 是周六；2026-10-04 是周日。
        assert_eq!(at(2026, 10, 1, 10).weekday(), Weekday::Thu);
        assert_eq!(calendar.limit_at(at(2026, 10, 1, 10)), 3, "周四上午 = 3");
        assert_eq!(calendar.limit_at(at(2026, 10, 1, 3)), 10, "周四凌晨 = 10");
        assert_eq!(calendar.limit_at(at(2026, 10, 3, 12)), 10, "周六全天 = 10");
        assert_eq!(calendar.limit_at(at(2026, 10, 4, 12)), 10, "周日全天 = 10");
    }
}
```

> `board-runner` 的 `Cargo.toml` 需已有 `chrono` 依赖（Plan 3a 已加）。若 `mod tests` 里用到 `super::*` 但 `main.rs` 顶层没有可复用的导入，改为在测试里写全路径（如 `board_core::calendar::ConcurrencyCalendar`），本测试已如此。

- [x] **Step 2: 运行测试确认失败**

Run: `cd plugins/superpowers-board && cargo test -p board-runner the_sample_calendar --offline`
Expected: FAIL（`kanban.toml` 不存在，`include_str!` 编译失败）。

- [x] **Step 3: 写最小实现**

`plugins/superpowers-board/kanban.toml`：

```toml
# Superpowers 看板 — 时段并发上限（任务级）。
# 命中第一个匹配的 window；无命中用 default_max_tasks。
default_max_tasks = 3

# 工作日白天：工作日 09:00–24:00 限流（≈800 req/h），最多 3 个任务并发。
[[window]]
days = "Mon-Fri"
start = "09:00"
end = "24:00"
max_tasks = 3

# 工作日凌晨：不限流，最多 10 个任务并发。
[[window]]
days = "Mon-Fri"
start = "00:00"
end = "09:00"
max_tasks = 10

# 周末全天：不限流，最多 10 个任务并发。
[[window]]
days = "Sat,Sun"
all_day = true
max_tasks = 10
```

`plugins/superpowers-board/supervisors/superpowers-board.json`：

```json
{
  "name": "superpowers-board",
  "command": "board-runner",
  "args": [
    "--runtime-dir", "{runtime_dir}",
    "--state-dir", "{state_dir}",
    "--project-root", "{workdir}",
    "--interval-secs", "60"
  ],
  "switch_key": "superpowers_board",
  "restart_backoff_ms": 1000,
  "restart_backoff_max_ms": 30000
}
```

`plugins/superpowers-board/README.md`：写清装/卸与启动前提：

```markdown
# Superpowers 看板插件

## 安装
1. 放入 `board-runner` 可执行文件（`cargo build -p board-runner --release`）。
2. 放入 `kanban` skill 目录。
3. 把 `supervisors/superpowers-board.json` 复制到
   `<项目>/.yi-agent/supervisors/superpowers-board.json`，并按需改 `command` 为绝对路径。
4. 在 `<项目>/.yi-agent/preferences.json` 写入 `{"superpowers_board": true}`（或在 TUI/桌面里打开开关）。

## 运行前提
daemon 常驻（`yi-agent daemon start`）。daemon 会按清单与开关拉起 `board-runner` 并守护它。

## 卸载
删除 `supervisors/superpowers-board.json`（daemon 随即停掉该进程），再删除本目录。
正在 daemon 中运行的会话会照常跑完，不影响主程序。

## 配置
`kanban.toml` 放在 `<项目>/.yi-agent/board/kanban.toml`（示例见本目录）。
```

- [x] **Step 4: 运行测试确认通过**

Run: `cd plugins/superpowers-board && cargo test --offline`
Expected: PASS（含新增的 `the_sample_calendar_expresses_the_three_and_ten_windows`）。

- [x] **Step 5: 提交**

```bash
cd plugins/superpowers-board && cargo fmt --all
git add plugins/superpowers-board/kanban.toml plugins/superpowers-board/supervisors plugins/superpowers-board/README.md plugins/superpowers-board/crates/board-runner/src/main.rs
git commit -m "feat(board-runner): ship the sample calendar, manifest and install notes"
```

---

## 完成判据

- `cd yi-agent-rs && cargo test -p yi-agent-supervisors` 全绿（manifest 6 + switch 5 + supervisor 5）。
- `cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent` 中 **`tui::board`（含 `add` 与实时卡片）全绿**；`slash` 47 全绿。
- `cd yi-agent-rs && cargo test -p yi-agent-board-ui` 全绿（含 `tests/inbox.rs` 3 个）。
- `cd yi-agent-rs && cargo test -p yi-agent-app-server` 全绿（含 3 个 board RPC 测试）。
- `cd desktop && npx tsc --noEmit && npm test` 全绿。
- `cd plugins/superpowers-board && cargo test` 全绿（board-runner 追加 inbox 4 + 示例日历 1）。
- 清单解析、占位展开、两层开关、监督循环（起/停/退避重启/回收）、inbox（投递/校验/幂等/拒绝归档）、
  TUI（实时渲染 + `add`）、4 条 board RPC、示例 `kanban.toml`（3/10 窗口）各有专门测试。
- **不得**引入 §9 的非目标能力（卡片操作、详情、时间线、Web、全局层编辑、推送、自注册、安装器）。

## 与本次非目标的关系（提醒）

本计划只做闭环接线。spec §9 记录的 14 条非目标**一律不做**，且**不得**为其预留半成品接口。
若实现中发现必须触碰某条非目标，停下来向用户报告，不要自行扩大范围。
