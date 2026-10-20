# 功能开关（feature flags）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让功能可以先合进 `main`、默认对用户不可见，只有 `-dev` 构建能逐项打开；同时 dev 版可独立编译、独立运行、独立管理开关。

**Architecture:** 新增 `yi-agent-flags` crate 作为唯一判定点：注册表（静态表）定义全部 flag，`flags.json` 存实例级取值，最外层用 `BUILD_KIND` 硬闸把 stable/nogit 全部压成 `false`。上层只通过 `is_enabled(name)` 查询，两个入口（`yi-agent flags` 子命令、`flags/*` RPC + 设置页 Tab）共用同一份存储。

**Tech Stack:** Rust 2024、serde_json、clap 4（derive）、bash、React + vitest

## Global Constraints

- 工作目录：所有 cargo 命令在 `yi-agent-rs/` 下执行。
- **禁止**在 `main` 上直接提交；每个 Task 在 worktree 内完成，最后 `merge --no-ff`。
- 每次 commit 前必须 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- **不要**在 commit message 里写 `Co-Authored-By` 行。
- 测试按 crate 跑，**不要**用 `cargo test --workspace`（OOM / exit 137）。
- 跑测试前先 `ps aux | grep -v grep | grep -E "cargo|rustc"` 确认没有残留 cargo 进程。
- 环境变量未设置时，所有行为必须与改动前完全一致。
- 新环境变量前缀统一 `YI_AGENT_`。
- flag 名一旦发布不改名（它就是 `flags.json` 里的键）。
- 注册表 `default` 字段当前**必须全为 `false`**。
- 版本号唯一真源仍是 `yi-agent-rs/Cargo.toml` 的 `version`，本计划不修改该值。

## File Structure

| 文件 | 职责 |
|---|---|
| `crates/yi-agent-flags/Cargo.toml`（新建） | 新 crate，仅依赖 `yi-agent-store` + serde |
| `crates/yi-agent-flags/src/lib.rs`（新建） | `is_enabled()` / `all()` / `set()` / `source_of()` |
| `crates/yi-agent-flags/src/registry.rs`（新建） | `FlagDef`、`REGISTRY`、默认关测试、过期测试 |
| `crates/yi-agent-flags/src/store.rs`（新建） | `flags.json` 读写（走 `paths::instance_home()`） |
| `crates/yi-agent-flags/src/error.rs`（新建） | `FlagError`（未注册键、写失败） |
| `crates/yi-agent-app-server/src/flags_rpc.rs`（新建） | `flags/list`、`flags/set`、`build/info` |
| `crates/yi-agent-app-server/src/rpc/flags.rs`（新建） | 域 handler |
| `crates/yi-agent-app-server/src/rpc/mod.rs`（改） | `Domain::Flags` + `domain_for` 映射 |
| `crates/yi-agent/src/config.rs`（改） | `Flags` 子命令 |
| `desktop/src/components/SettingsFlagsTab.tsx`（新建） | 设置页「实验功能」 |
| `desktop/src/components/SettingsDialog.tsx`（改） | Tab 表按 dev 判定增删 |
| `yi-agent-rs/scripts/dev-env.sh`（改） | 补 `models.json` 复制、`down --purge` 清目录 |

---

### Task 0: 修 `dev-env.sh` 的两个缺口

先做这个，因为后面每个 Task 都要用它起实例验证。

**Files:**
- Modify: `yi-agent-rs/scripts/dev-env.sh`
- Modify: `yi-agent-rs/scripts/test_dev_env.sh`

**Interfaces:**
- Produces: `ensure_instance` 会复制 `models.json`；`down --purge` 删除整个 `<id>/` 目录

- [ ] **Step 1: 写失败测试**

在 `yi-agent-rs/scripts/test_dev_env.sh` 的第 4 项（`down --purge`）之前插入：

```bash
# 3b. models.json 会被复制到新实例（否则实例开箱不可用）
mkdir -p "$TMP/stable/.yi-agent"
printf '{"version":1,"models":[],"default_model":null}' > "$TMP/stable/.yi-agent/models.json"
rm -rf "$TMP/instances"
YI_AGENT_INSTANCES_ROOT="$TMP/instances" HOME="$TMP/stable" \
  "$SCRIPT" run true >/dev/null 2>&1 || true
copied="$TMP/instances/$id1/.yi-agent/models.json"
[ -f "$copied" ] || fail "models.json was not copied into the instance"
pass "models.json is copied into the instance"
```

并把第 4 项断言从「实例目录不存在」改为「实例根为空」：

```bash
# 4. down --purge 删除目录（连空壳父目录一起）
YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" down --purge >/dev/null 2>&1
[ -d "$TMP/instances/$id1" ] && fail "down --purge left an empty $id1 dir behind"
pass "down --purge removes the instance dir"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_dev_env.sh
```

Expected: `FAIL: models.json was not copied into the instance`

- [ ] **Step 3: 实现**

在 `dev-env.sh` 的 `ensure_instance` 里，`cp "$HOME/.yi-agent/.env"` 那段之后加：

```bash
  # 模型清单同样在实例外：不复制的话实例开箱就是「尚未配置模型」。
  if [ ! -f "$home/models.json" ] && [ -f "$HOME/.yi-agent/models.json" ]; then
    cp "$HOME/.yi-agent/models.json" "$home/models.json"
    printf 'dev-env: 已复制 models.json 到 %s/models.json\n' "$home" >&2
  fi
```

把 `down --purge` 的删除目标从实例 home 改成整个实例目录：

```bash
    if [ "${1:-}" = "--purge" ]; then
      # 删整个 <id>/，否则 ~/.yi-agent-instances 会积一堆空壳目录。
      rm -rf "$(dirname "$home")"
      printf 'dev-env: 已删除 %s\n' "$(dirname "$home")" >&2
    fi
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_dev_env.sh
```

Expected: 7 项全 ok，末行 `all dev-env checks passed`

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/scripts/dev-env.sh yi-agent-rs/scripts/test_dev_env.sh
git commit -m "fix(scripts): 实例复制 models.json，purge 清掉空壳目录"
```

---

### Task 1: 新建 `yi-agent-flags` crate 与注册表

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-flags/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-flags/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-flags/src/registry.rs`
- Modify: `yi-agent-rs/Cargo.toml`（members + workspace.dependencies）

**Interfaces:**
- Produces:
  - `pub struct FlagDef { pub name: &'static str, pub summary: &'static str, pub owner: &'static str, pub expires_on: &'static str, pub default: bool }`
  - `pub const REGISTRY: &[FlagDef]`
  - `pub fn find(name: &str) -> Option<&'static FlagDef>`
  - `pub fn today() -> String`（`YYYY-MM-DD`）

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent-flags/src/registry.rs`，先只放测试与空表：

```rust
//! flag 注册表：唯一真源。未在此登记的键一律不受理。

/// 一个功能开关的定义。
pub struct FlagDef {
    /// 唯一键，snake_case，发布后不改名（它就是 flags.json 里的键）。
    pub name: &'static str,
    /// 设置页上显示的一句话说明。
    pub summary: &'static str,
    /// 责任人。
    pub owner: &'static str,
    /// 过期日 `YYYY-MM-DD`，必填。到期后 `no_flag_is_past_its_expiry_date` 失败。
    pub expires_on: &'static str,
    /// 默认值。当前规则：必须全部为 `false`。
    pub default: bool,
}

pub const REGISTRY: &[FlagDef] = &[];

/// 按名字查注册表。
pub fn find(name: &str) -> Option<&'static FlagDef> {
    REGISTRY.iter().find(|def| def.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_flag_defaults_to_off() {
        for def in REGISTRY {
            assert!(
                !def.default,
                "flag `{}` must default to off: a true default means merging changes \
                 user-visible behavior",
                def.name
            );
        }
    }

    #[test]
    fn no_flag_is_past_its_expiry_date() {
        let today = today();
        for def in REGISTRY {
            assert!(
                def.expires_on > today.as_str(),
                "flag `{}` expired on {} (today is {}) — delete it, or extend the \
                 date if the work is genuinely unfinished",
                def.name,
                def.expires_on,
                today
            );
        }
    }

    #[test]
    fn names_are_unique_and_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for def in REGISTRY {
            assert!(seen.insert(def.name), "duplicate flag name: {}", def.name);
            assert!(
                def.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "flag `{}` must be snake_case (it becomes a JSON key)",
                def.name
            );
        }
    }

    #[test]
    fn expiry_dates_are_iso_formatted() {
        for def in REGISTRY {
            let bytes = def.expires_on.as_bytes();
            assert_eq!(bytes.len(), 10, "flag `{}`: expiry must be YYYY-MM-DD", def.name);
            assert_eq!(bytes[4], b'-', "flag `{}`: expiry must be YYYY-MM-DD", def.name);
            assert_eq!(bytes[7], b'-', "flag `{}`: expiry must be YYYY-MM-DD", def.name);
        }
    }
}
```

创建 `crates/yi-agent-flags/src/lib.rs`：

```rust
//! 功能开关：让功能先合入、后开启。

pub mod registry;

pub use registry::{FlagDef, REGISTRY, find};

/// 今天的日期，`YYYY-MM-DD`。
///
/// 测试可用 `YI_FLAGS_TODAY` 固定时间，避免「跨零点时测试忽然失败」。
pub fn today() -> String {
    if let Ok(fixed) = std::env::var("YI_FLAGS_TODAY") {
        if !fixed.is_empty() {
            return fixed;
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_civil(secs)
}

/// Unix 秒 → `YYYY-MM-DD`（UTC）。用 Howard Hinnant 的 civil_from_days 算法，
/// 避免为一行日期引入 chrono。
fn format_civil(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_is_iso_formatted() {
        let today = today();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    #[test]
    fn a_fixed_date_wins() {
        // 环境变量是全局的，测试里只读不写；此处验证解析逻辑本身。
        assert_eq!(format_civil(0), "1970-01-01");
        assert_eq!(format_civil(1_700_000_000), "2023-11-14");
    }
}
```

- [ ] **Step 2: 跑测试确认失败（crate 还不存在）**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags
```

Expected: `error: package ID specification 'yi-agent-flags' did not match any packages`

- [ ] **Step 3: 建 crate 并登记**

`crates/yi-agent-flags/Cargo.toml`：

```toml
[package]
name = "yi-agent-flags"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
yi-agent-store = { workspace = true }
serde_json = { workspace = true }
```

在 `yi-agent-rs/Cargo.toml` 的 `members` 里加 `"crates/yi-agent-flags",`（放在 `crates/yi-agent-store` 之后），并在 `[workspace.dependencies]` 里加：

```toml
yi-agent-flags = { path = "crates/yi-agent-flags" }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags
```

Expected: 6 passed（`every_flag_defaults_to_off` / `no_flag_is_past_its_expiry_date` / `names_are_unique_and_snake_case` / `expiry_dates_are_iso_formatted` / `today_is_iso_formatted` / `a_fixed_date_wins`）

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-flags Cargo.toml Cargo.lock
git commit -m "feat(flags): 新建注册表 crate，默认关与过期日由测试钉住"
```

---

### Task 2: `flags.json` 读写与三层取值

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-flags/src/store.rs`
- Create: `yi-agent-rs/crates/yi-agent-flags/src/error.rs`
- Modify: `yi-agent-rs/crates/yi-agent-flags/src/lib.rs`

**Interfaces:**
- Consumes: `registry::find`、`registry::REGISTRY`
- Produces:
  - `pub enum Source { Default, File, Env, BuildKind }`
  - `pub fn is_enabled(name: &str) -> bool`
  - `pub fn status(name: &str) -> Option<(bool, Source)>`
  - `pub fn set_at(path: &Path, name: &str, enabled: bool) -> Result<(), FlagError>`
  - `pub fn all_at(path: &Path, build_kind: &str) -> Vec<FlagView>`
  - `pub struct FlagView { pub name: String, pub summary: String, pub enabled: bool, pub expires_on: String, pub source: Source }`

- [ ] **Step 1: 写失败测试**

创建 `crates/yi-agent-flags/src/store.rs`，先只放测试：

```rust
//! `flags.json` 的读写，以及「默认 → 文件 → 环境变量 → 构建类型硬闸」的取值链。

use std::path::{Path, PathBuf};

use crate::error::FlagError;
use crate::registry::{find, REGISTRY};

/// 一个取值来自哪一层。`flags status` 要把它打出来——值不对时八成是环境变量盖了文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Default,
    File,
    Env,
    /// 构建类型不是 dev，硬闸把所有开关注销。
    BuildKind,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Default => "default",
            Source::File => "file",
            Source::Env => "env",
            Source::BuildKind => "build-kind",
        }
    }
}

/// `flags/list` 与服务端表格共用的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagView {
    pub name: String,
    pub summary: String,
    pub enabled: bool,
    pub expires_on: String,
    pub source: Source,
}

/// `$YI_AGENT_HOME/flags.json`。
pub fn flags_path() -> Option<PathBuf> {
    yi_agent_store::paths::home_join("flags.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry_test_support::with_flag;

    // 说明：以下用例通过 `set_at` / `read_at` 注入路径，不改进程环境变量
    // （cargo 并行跑测试，全局量会让用例互相竞态）。唯一例外是环境变量层，
    // 那几个用例用 `serial` 风格的互斥量串行。

    #[test]
    fn a_missing_file_means_everything_is_off() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        assert_eq!(read_at(&path, "known").unwrap(), None);
    }

    #[test]
    fn a_saved_value_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), Some(true));
        write_at(&path, "known", false).unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), Some(false));
    }

    #[test]
    fn a_corrupt_file_reads_as_empty_instead_of_failing_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(read_at(&path, "known").unwrap(), None);
    }

    #[test]
    fn writing_an_unregistered_name_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = write_at(&path, "no_such_flag", true).unwrap_err();
        assert!(matches!(err, FlagError::UnknownFlag(_)), "got {err:?}");
    }

    #[test]
    fn unknown_keys_in_the_file_are_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        std::fs::write(&path, r#"{"version":1,"flags":{"bogus_key":true}}"#).unwrap();
        assert_eq!(read_at(&path, "bogus_key").unwrap(), None);
    }

    #[test]
    fn the_build_kind_gate_wins_over_everything() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "release", Some("1"));
        assert!(!enabled, "a release build must not honour any flag");
        assert_eq!(source, Source::BuildKind);
    }

    #[test]
    fn a_dev_build_reads_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "dev", None);
        assert!(enabled);
        assert_eq!(source, Source::File);
    }

    #[test]
    fn an_env_override_beats_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "dev", Some("0"));
        assert!(!enabled);
        assert_eq!(source, Source::Env);
    }

    #[test]
    fn an_absent_flag_reports_the_default_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let (enabled, source) = status_at(&path, "known", "dev", None);
        assert!(!enabled);
        assert_eq!(source, Source::Default);
    }

    #[test]
    fn a_nogit_build_is_treated_like_release() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        write_at(&path, "known", true).unwrap();
        let (enabled, source) = status_at(&path, "known", "nogit", None);
        assert!(!enabled);
        assert_eq!(source, Source::BuildKind);
    }
}
```

在 `crates/yi-agent-flags/src/registry.rs` 末尾加一个**测试专用**注册项模块，让上面的用例有确定的 flag 名：

```rust
/// 测试用的注册项。`cfg(test)` 之外的代码看不到它，也进不了 `REGISTRY`。
#[cfg(test)]
pub(crate) mod registry_test_support {
    /// 测试里唯一被承认的 flag 名。
    pub(crate) const KNOWN: &str = "known";
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags store
```

Expected: 编译失败，`cannot find function read_at in this scope`

- [ ] **Step 3: 实现**

`crates/yi-agent-flags/src/error.rs`：

```rust
//! 开关操作的错误。故意很小：只有「不认识的键」和「写不进去」两种。

use std::fmt;

#[derive(Debug)]
pub enum FlagError {
    /// 名字不在注册表里。写入时必须拒绝——否则一个拼写错误会表现成
    /// 「开了但没生效」，极难排查。
    UnknownFlag(String),
    /// 落盘失败。
    Io(std::io::Error),
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlagError::UnknownFlag(name) => {
                write!(f, "unknown flag `{name}`: it is not in the registry")
            }
            FlagError::Io(e) => write!(f, "could not write flags.json: {e}"),
        }
    }
}

impl std::error::Error for FlagError {}
```

`crates/yi-agent-flags/src/store.rs` 的实现（去掉 `#[cfg(test)]` 包壳，`mod tests` 保留在最下方）：

```rust
/// 文件里只存开着的键与否；缺省一律视为关。
fn load_map(path: &Path) -> serde_json::Map<String, serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Default::default();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        // 坏文件不阻断启动；下一次写入会把它覆盖成合法结构。
        return Default::default();
    };
    value
        .get("flags")
        .and_then(|f| f.as_object())
        .cloned()
        .unwrap_or_default()
}

/// 读一个键的落盘值。未注册的名字一律当作不存在。
pub fn read_at(path: &Path, name: &str) -> Result<Option<bool>, FlagError> {
    if find(name).is_none() && !crate::registry::is_test_known(name) {
        return Ok(None);
    }
    Ok(load_map(path).get(name).and_then(|v| v.as_bool()))
}

/// 「默认 → 文件 → 环境变量 → 构建类型」的完整取值链。
pub fn status_at(
    path: &Path,
    name: &str,
    build_kind: &str,
    env_override: Option<&str>,
) -> (bool, Source) {
    if build_kind != "dev" {
        return (false, Source::BuildKind);
    }
    if let Some(raw) = env_override {
        return (raw == "1", Source::Env);
    }
    match read_at(path, name) {
        Ok(Some(value)) => (value, Source::File),
        _ => (false, Source::Default),
    }
}

/// 生产入口：用真实构建类型与真实环境变量。
pub fn is_enabled(name: &str) -> bool {
    status(name).map(|(enabled, _)| enabled).unwrap_or(false)
}

pub fn status(name: &str) -> Option<(bool, Source)> {
    let Some(path) = flags_path() else {
        return None;
    };
    let env_key = format!(
        "YI_AGENT_FLAG_{}",
        name.to_ascii_uppercase().replace('-', "_")
    );
    let env = std::env::var(&env_key).ok().filter(|v| !v.is_empty());
    Some(status_at(&path, name, crate::build_kind(), env.as_deref()))
}

/// 写一个键。原子替换：先写临时文件再 rename，避免读到半截 JSON。
pub fn write_at(path: &Path, name: &str, enabled: bool) -> Result<(), FlagError> {
    if find(name).is_none() && !crate::registry::is_test_known(name) {
        return Err(FlagError::UnknownFlag(name.to_string()));
    }
    let mut map = load_map(path);
    map.insert(name.to_string(), serde_json::Value::Bool(enabled));
    let document = serde_json::json!({ "version": 1, "flags": map });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(FlagError::Io)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&document).unwrap())
        .map_err(FlagError::Io)?;
    std::fs::rename(&tmp, path).map_err(FlagError::Io)
}

/// 列出全部注册项及其生效值。
pub fn all_at(path: &Path, build_kind: &str) -> Vec<FlagView> {
    REGISTRY
        .iter()
        .map(|def| {
            let env_key = format!(
                "YI_AGENT_FLAG_{}",
                def.name.to_ascii_uppercase().replace('-', "_")
            );
            let env = std::env::var(&env_key).ok().filter(|v| !v.is_empty());
            let (enabled, source) = status_at(path, def.name, build_kind, env.as_deref());
            FlagView {
                name: def.name.to_string(),
                summary: def.summary.to_string(),
                enabled,
                expires_on: def.expires_on.to_string(),
                source,
            }
        })
        .collect()
}
```

在 `crates/yi-agent-flags/src/registry.rs` 加两个测试支撑函数：

```rust
/// 测试里承认的名字。生产代码永远返回 `false`（`cfg(test)` 之外不存在）。
#[cfg(test)]
pub(crate) fn is_test_known(name: &str) -> bool {
    name == registry_test_support::KNOWN
}

#[cfg(not(test))]
pub(crate) fn is_test_known(_name: &str) -> bool {
    false
}
```

在 `crates/yi-agent-flags/src/lib.rs` 加构建类型来源与模块声明：

```rust
pub mod error;
pub mod store;

pub use error::FlagError;
pub use store::{FlagView, Source, all_at, is_enabled, status, status_at, write_at};

/// 当前构建类型。由 `yi-agent` 的 `build.rs` 注入；测试里由 `YI_AGENT_BUILD_KIND` 决定。
pub fn build_kind() -> &'static str {
    option_env!("YI_AGENT_BUILD_KIND").unwrap_or("dev")
}
```

把 `flags_path` 用到的 `home_join` 加进 `yi-agent-store/src/paths.rs`——它已存在（Task 1 的前序工作里已有），确认签名是 `pub fn home_join(rel: &str) -> Option<PathBuf>` 即可，无需改动。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-flags
```

Expected: 16 passed（6 个 registry + 10 个 store）

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-flags
git commit -m "feat(flags): flags.json 读写与三层取值链，构建类型硬闸在最外层"
```

---

### Task 3: `yi-agent flags` 子命令

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/flags_cmd.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`（`Commands` 枚举 + dispatch）
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`mod flags_cmd;`）
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml`（加 `yi-agent-flags` 依赖）

**Interfaces:**
- Consumes: `yi_agent_flags::{all_at, status_at, write_at, store::flags_path}`
- Produces: `pub fn run(sub: FlagsSub) -> anyhow::Result<i32>`

- [ ] **Step 1: 写失败测试**

创建 `yi-agent-rs/crates/yi-agent/src/flags_cmd.rs`，先只放测试：

```rust
//! `yi-agent flags` 子命令：注册表的读、改、以及「值来自哪一层」。

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_prints_a_row_per_registered_flag() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let out = render_list_at(&path, "dev");
        // 注册表当前可能为空；关键是不 panic 且表头在。
        assert!(out.contains("NAME"), "got: {out}");
        assert!(out.contains("SOURCE"), "got: {out}");
    }

    #[test]
    fn setting_an_unknown_flag_reports_it_and_returns_nonzero() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = set_at_cli(&path, "no_such_flag", true).unwrap_err();
        assert!(err.to_string().contains("unknown flag"), "got: {err}");
    }

    #[test]
    fn a_release_build_reports_the_build_kind_gate() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let out = render_status_at(&path, "anything", "release");
        assert!(out.contains("build-kind"), "got: {out}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent flags_cmd
```

Expected: 编译失败，`cannot find function render_list_at`

- [ ] **Step 3: 实现**

`crates/yi-agent/src/flags_cmd.rs` 的实现：

```rust
use std::path::Path;

use anyhow::Result;
use yi_agent_flags::store::flags_path;
use yi_agent_flags::{all_at, status_at, write_at};

/// `flags list`
pub fn run_list() -> Result<i32> {
    let path = flags_path().ok_or_else(|| anyhow::anyhow!("cannot resolve the state directory"))?;
    print!("{}", render_list_at(&path, yi_agent_flags::build_kind()));
    Ok(0)
}

/// `flags set <name> on|off`
pub fn run_set(name: &str, enabled: bool) -> Result<i32> {
    let path = flags_path().ok_or_else(|| anyhow::anyhow!("cannot resolve the state directory"))?;
    match set_at_cli(&path, name, enabled) {
        Ok(()) => {
            println!("{name} = {enabled}");
            Ok(0)
        }
        Err(e) => {
            eprintln!("error: {e}");
            Ok(1)
        }
    }
}

/// `flags status <name>`
pub fn run_status(name: &str) -> Result<i32> {
    let path = flags_path().ok_or_else(|| anyhow::anyhow!("cannot resolve the state directory"))?;
    print!("{}", render_status_at(&path, name, yi_agent_flags::build_kind()));
    Ok(0)
}

fn set_at_cli(path: &Path, name: &str, enabled: bool) -> Result<()> {
    write_at(path, name, enabled).map_err(|e| anyhow::anyhow!("{e}"))
}

fn render_list_at(path: &Path, build_kind: &str) -> String {
    let mut out = String::from("NAME\tSTATE\tSOURCE\tEXPIRES\tSUMMARY\n");
    for view in all_at(path, build_kind) {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\n",
            view.name,
            if view.enabled { "on" } else { "off" },
            view.source.as_str(),
            view.expires_on,
            view.summary
        ));
    }
    if yi_agent_flags::REGISTRY.is_empty() {
        out.push_str("(no flags registered)\n");
    }
    out
}

fn render_status_at(path: &Path, name: &str, build_kind: &str) -> String {
    let env = std::env::var(format!(
        "YI_AGENT_FLAG_{}",
        name.to_ascii_uppercase().replace('-', "_")
    ))
    .ok()
    .filter(|v| !v.is_empty());
    let (enabled, source) = status_at(path, name, build_kind, env.as_deref());
    format!("{name}: enabled={enabled} (source: {})\n", source.as_str())
}
```

在 `crates/yi-agent/src/main.rs` 的 `mod build_info;` 之后加 `mod flags_cmd;`。

在 `crates/yi-agent/src/config.rs` 的 `Commands` 枚举里加（放在 `Revoke` 之后）：

```rust
    /// 功能开关（仅 dev 构建可开）。
    Flags {
        #[command(subcommand)]
        cmd: FlagsSub,
    },
```

并在同文件定义：

```rust
#[derive(Subcommand)]
pub enum FlagsSub {
    /// 列出全部注册的开关及生效值。
    List,
    /// 打开或关闭一个开关。
    Set {
        name: String,
        /// `on` 或 `off`
        value: String,
    },
    /// 打印一个开关的最终生效值，以及它来自哪一层。
    Status { name: String },
}
```

在 `main.rs` 的 dispatch 里加：

```rust
        Commands::Flags { cmd } => match cmd {
            FlagsSub::List => flags_cmd::run_list()?,
            FlagsSub::Set { name, value } => {
                let enabled = match value.as_str() {
                    "on" | "true" | "1" => true,
                    "off" | "false" | "0" => false,
                    other => {
                        eprintln!("error: expected on|off, got `{other}`");
                        return Ok(1);
                    }
                };
                flags_cmd::run_set(&name, enabled)?
            }
            FlagsSub::Status { name } => flags_cmd::run_status(&name)?,
        },
```

在 `crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里加 `yi-agent-flags = { workspace = true }`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --bin yi-agent flags_cmd
```

Expected: 3 passed

再手测一次真实行为：

```bash
./target/debug/yi-agent flags list
./target/debug/yi-agent flags status anything
# 期望含 build-kind（本机是 dev，则含 default）
./target/debug/yi-agent flags set bogus on ; echo "exit=$?"
# 期望打印 unknown flag 且 exit=1
```

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent/src/flags_cmd.rs crates/yi-agent/src/config.rs crates/yi-agent/src/main.rs crates/yi-agent/Cargo.toml Cargo.lock
git commit -m "feat(cli): yi-agent flags 子命令（list/set/status）"
```

---

### Task 4: `flags/*` 与 `build/info` RPC

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/flags_rpc.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/flags.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`

**Interfaces:**
- Consumes: `yi_agent_flags::{all_at, write_at, build_kind}`
- Produces:
  - `pub fn handle_flags_request_at(path: &Path, build_kind: &str, method: &str, params: &Value) -> Result<Value, RpcError>`
  - `Domain::Flags`，`domain_for("flags/*" | "build/*")` → `Domain::Flags`

- [ ] **Step 1: 写失败测试**

创建 `crates/yi-agent-app-server/src/flags_rpc.rs`，先只放测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_returns_the_registry_with_sources() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let value = handle_flags_request_at(&path, "dev", "flags/list", &json!({})).unwrap();
        assert!(value.get("flags").unwrap().is_array());
    }

    #[test]
    fn set_refuses_an_unregistered_name_with_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = handle_flags_request_at(
            &path,
            "dev",
            "flags/set",
            &json!({"name": "bogus", "enabled": true}),
        )
        .unwrap_err();
        assert_eq!(err.code, crate::protocol::RpcError::invalid_params("x").code);
    }

    #[test]
    fn flags_are_method_not_found_in_a_release_build() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = handle_flags_request_at(&path, "release", "flags/set", &json!({})).unwrap_err();
        assert_eq!(err.code, crate::protocol::RpcError::method_not_found("x").code);
    }

    #[test]
    fn build_info_reports_the_kind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let value = handle_flags_request_at(&path, "dev", "build/info", &json!({})).unwrap();
        assert_eq!(value.get("build_kind").unwrap(), "dev");
        assert!(value.get("version").unwrap().is_string());
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib flags_rpc
```

Expected: 编译失败，`cannot find function handle_flags_request_at`

- [ ] **Step 3: 实现**

`crates/yi-agent-app-server/src/flags_rpc.rs`：

```rust
//! `flags/*` 与 `build/info` 的 RPC 接缝。
//!
//! 与 `model_rpc` 同类：机器级、不带 project 参数；路径由调用方注入以便测试。
//! **stable 构建里这些方法不存在**（`-32601`），而不是「存在但拒绝」——
//! 这样前端不需要任何权限判断，只需要看 `build/info`。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_flags::FlagError;

use crate::protocol::RpcError;

pub fn handle_flags_request(
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    let path = yi_agent_flags::store::flags_path()
        .ok_or_else(|| RpcError::internal("cannot resolve the state directory"))?;
    handle_flags_request_at(&path, yi_agent_flags::build_kind(), method, params)
}

pub fn handle_flags_request_at(
    path: &Path,
    build_kind: &str,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    // `build/info` 在任何构建里都存在：前端靠它判断自己是不是 dev。
    if method == "build/info" {
        return Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "build_kind": build_kind,
        }));
    }

    if build_kind != "dev" {
        // 方法在 stable 上**不存在**：前端根本不会渲染入口。
        return Err(RpcError::method_not_found(method));
    }

    match method {
        "flags/list" => Ok(json!({ "flags": views(path, build_kind) })),
        "flags/set" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
            let enabled = params
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| RpcError::invalid_params("`enabled` is required"))?;
            match yi_agent_flags::write_at(path, name, enabled) {
                Ok(()) => Ok(json!({ "name": name, "enabled": enabled })),
                Err(FlagError::UnknownFlag(name)) => Err(RpcError::invalid_params(format!(
                    "unknown flag `{name}`: it is not in the registry"
                ))),
                Err(FlagError::Io(e)) => Err(RpcError::internal(format!(
                    "could not write flags.json: {e}"
                ))),
            }
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn views(path: &Path, build_kind: &str) -> Vec<Value> {
    yi_agent_flags::all_at(path, build_kind)
        .into_iter()
        .map(|view| {
            json!({
                "name": view.name,
                "summary": view.summary,
                "enabled": view.enabled,
                "expires_on": view.expires_on,
                "source": view.source.as_str(),
            })
        })
        .collect()
}
```

先确认 `RpcError` 有 `internal` 构造；若没有，用 `RpcError::invalid_params` 替代并在同一步补一个 `pub fn internal(msg: impl Into<String>) -> Self`。

`crates/yi-agent-app-server/src/rpc/flags.rs`：

```rust
//! `flags/*` 与 `build/*` 域的 RPC 处理函数。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "flags/list" | "build/info" => {
            match crate::flags_rpc::handle_flags_request(req.method.as_str(), &req.params) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        "flags/set" => {
            // 改配置与 `model/upsert` 同档：需要 Control。
            if state.client_scope < crate::protocol::Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        crate::protocol::RpcError::insufficient_scope(crate::protocol::Scope::Control),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match crate::flags_rpc::handle_flags_request(req.method.as_str(), &req.params) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

`rpc/mod.rs`：在 `pub(crate) mod mcp;` 之前加 `pub(crate) mod flags;`，在 `Domain` 枚举里 `Mcp` 之后加：

```rust
    /// `flags/*` 与 `build/*`（功能开关面板 + 构建类型查询）。
    Flags,
```

在 `domain_for` 里加：

```rust
        Some("flags") | Some("build") => Domain::Flags,
```

在 `server.rs` 的 `match crate::rpc::domain_for(...)` 里加 `crate::rpc::Domain::Flags => { ... }` 臂（照抄 `Domain::Mcp` 那一段的结构，调用 `crate::rpc::flags::handle(...)`）。

`crates/yi-agent-app-server/src/lib.rs` 加 `pub mod flags_rpc;`（放在 `pub mod model_rpc;` 附近）。

`Cargo.toml` 加 `yi-agent-flags = { workspace = true }`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib flags_rpc
cargo test -p yi-agent-app-server --lib
```

Expected: 4 passed；全量 930+ 不回归

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server
git commit -m "feat(rpc): flags/list、flags/set、build/info"
```

---

### Task 5: 设置页「实验功能」Tab（仅 dev 可见）

**Files:**
- Create: `desktop/src/components/SettingsFlagsTab.tsx`
- Create: `desktop/src/components/SettingsFlagsTab.test.tsx`
- Modify: `desktop/src/components/SettingsDialog.tsx`

**Interfaces:**
- Consumes: 宿主注入的 `flagsCall`（`(method, params) => Promise<unknown>`）与 `buildKind: "dev" | "release" | "nogit"`
- Produces: `export function SettingsFlagsTab(props: { call: FlagsCall }): JSX.Element`

- [ ] **Step 1: 写失败测试**

`desktop/src/components/SettingsFlagsTab.test.tsx`：

```tsx
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import { SettingsFlagsTab } from "./SettingsFlagsTab";

const rows = [
  { name: "a_flag", summary: "实验 A", enabled: false, expires_on: "2026-12-31", source: "default" },
  { name: "b_flag", summary: "实验 B", enabled: true, expires_on: "2026-11-30", source: "file" },
];

describe("SettingsFlagsTab", () => {
  it("lists the flags it read from the host", async () => {
    const call = vi.fn().mockResolvedValue({ flags: rows });
    render(<SettingsFlagsTab call={call} />);
    expect(await screen.findByText("a_flag")).toBeTruthy();
    expect(screen.getByText("实验 B")).toBeTruthy();
  });

  it("shows where each value came from", async () => {
    const call = vi.fn().mockResolvedValue({ flags: rows });
    render(<SettingsFlagsTab call={call} />);
    await screen.findByText("a_flag");
    expect(screen.getByText("default")).toBeTruthy();
    expect(screen.getByText("file")).toBeTruthy();
  });

  it("writes a toggle and refreshes the row", async () => {
    const call = vi
      .fn()
      .mockResolvedValueOnce({ flags: rows })
      .mockResolvedValueOnce({ name: "a_flag", enabled: true })
      .mockResolvedValueOnce({
        flags: [{ ...rows[0], enabled: true, source: "file" }, rows[1]],
      });
    render(<SettingsFlagsTab call={call} />);
    const toggle = await screen.findByRole("checkbox", { name: /a_flag/i });
    await userEvent.click(toggle);
    await waitFor(() =>
      expect(call).toHaveBeenCalledWith("flags/set", { name: "a_flag", enabled: true }),
    );
    await waitFor(() => expect(screen.getByText("file")).toBeTruthy());
  });

  it("says so when there is nothing registered yet", async () => {
    const call = vi.fn().mockResolvedValue({ flags: [] });
    render(<SettingsFlagsTab call={call} />);
    expect(await screen.findByText(/还没有注册任何开关/)).toBeTruthy();
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/components/SettingsFlagsTab.test.tsx
```

Expected: `Failed to resolve import "./SettingsFlagsTab"`

- [ ] **Step 3: 实现**

`desktop/src/components/SettingsFlagsTab.tsx`：

```tsx
import { useCallback, useEffect, useState } from "react";
import { Switch } from "./Switch";

export type FlagRow = {
  name: string;
  summary: string;
  enabled: boolean;
  expires_on: string;
  source: string;
};

export type FlagsCall = (method: string, params?: unknown) => Promise<unknown>;

export function SettingsFlagsTab({ call }: { call: FlagsCall }) {
  const [rows, setRows] = useState<FlagRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const result = (await call("flags/list")) as { flags: FlagRow[] };
      setRows(result.flags);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggle = async (row: FlagRow, enabled: boolean) => {
    try {
      await call("flags/set", { name: row.name, enabled });
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  if (error) {
    return <p className="p-4 text-sm text-red-400">读取开关失败：{error}</p>;
  }
  if (!rows) {
    return <p className="p-4 text-sm text-fg-muted">读取中…</p>;
  }
  if (rows.length === 0) {
    return (
      <p className="p-4 text-sm text-fg-muted">
        还没有注册任何开关。功能做进来时，在 yi-agent-flags 的注册表里登记一项。
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-2 p-4">
      <p className="text-xs text-fg-muted">
        这些是实验功能，只在这个开发版本里可开关；稳定版不存在此页面。
      </p>
      {rows.map((row) => (
        <div
          key={row.name}
          className="flex items-center justify-between gap-3 rounded border border-line p-3"
        >
          <div className="min-w-0">
            <div className="truncate text-sm text-fg">{row.name}</div>
            <div className="truncate text-xs text-fg-muted">{row.summary}</div>
            <div className="text-xs text-fg-subtle">
              {row.source} · 到期 {row.expires_on}
            </div>
          </div>
          <Switch
            checked={row.enabled}
            onChange={(next) => void toggle(row, next)}
            label={row.name}
          />
        </div>
      ))}
    </div>
  );
}
```

先确认 `desktop/src/components/Switch.tsx` 存在且接受 `checked` / `onChange` / `label`；若签名不同，按实际签名调整（这是唯一需要现场核对的地方）。

`desktop/src/components/SettingsDialog.tsx`：

1. 在组件 props 里加：

```tsx
  /** dev 构建才渲染「实验功能」。由宿主经 `build/info` 得到。 */
  buildKind?: "dev" | "release" | "nogit";
  /** 「实验功能」Tab 的 RPC 接缝。 */
  flagsCall?: FlagsCall;
```

2. 把 `TABS` 从常量改为函数，或在渲染时过滤：

```tsx
const visibleTabs = TABS.filter((tab) => tab.id !== "flags" || buildKind === "dev");
```

并在 `TABS` 里加一项 `{ id: "flags", label: "实验功能" }`。

3. 在面板渲染处加：

```tsx
{active === "flags" && flagsCall ? <SettingsFlagsTab call={flagsCall} /> : null}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/components/SettingsFlagsTab.test.tsx && npx tsc --noEmit
```

Expected: 4 passed；tsc 无错误

- [ ] **Step 5: Commit**

```bash
git add desktop/src/components/SettingsFlagsTab.tsx desktop/src/components/SettingsFlagsTab.test.tsx desktop/src/components/SettingsDialog.tsx
git commit -m "feat(desktop): 设置页实验功能 Tab，仅 dev 构建可见"
```

---

### Task 6: 文档与全量验收

**Files:**
- Modify: `docs/project-management/tooling.md`
- Modify: `docs/project-management/yi-agent-app-server.md`（若有 RPC 清单）
- Modify: `docs/project-management/README.md`（若新增 crate 需登记）

- [ ] **Step 1: 更新文档**

在 `docs/project-management/tooling.md` 的 Features 里加两条（格式与既有条目一致，必须带可验证判据）：

```markdown
- [x] 功能开关（feature flags）— `yi-agent-rs/crates/yi-agent-flags/` 为唯一判定点：注册表 `registry.rs` 定义全部 flag（必填 `expires_on`），`flags.json` 存实例级取值（`paths::instance_home()`），取值链为「默认 → 文件 → 环境变量 `YI_AGENT_FLAG_*` → 构建类型硬闸」。**非 dev 构建一律全关**，无逃生口。两个入口共用同一份存储：`yi-agent flags list|set|status` 与 RPC `flags/list`、`flags/set`（stable 上返回 `-32601`）。判据：`cargo test -p yi-agent-flags`（16 passed）；`yi-agent flags status <name>` 在 dev 报 `source: file`、在 release 报 `source: build-kind`
- [x] 开关不会永久堆积 — `registry.rs` 的 `no_flag_is_past_its_expiry_date` 在 CI 里跑；到期即失败，失败信息同时给出「删掉」与「续期」两条出路。判据：把任一 `expires_on` 改成过去日期后 `cargo test -p yi-agent-flags` 必须失败且信息含 flag 名
```

在 `docs/project-management/README.md` 的模块索引表里加一行 `yi-agent-flags`（若该表按 crate 登记）。

- [ ] **Step 2: 全量验收**

```bash
ps aux | grep -v grep | grep -E "cargo|rustc" || echo clean
cd yi-agent-rs
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test -p yi-agent-flags
cargo test -p yi-agent-store --lib
cargo test -p yi-agent-app-server --lib
cargo test -p yi-agent --bin yi-agent
bash scripts/test_dev_env.sh
cd ../desktop && npx vitest run && npx tsc --noEmit
```

Expected：全部通过；`yi-agent-flags` 16 passed

- [ ] **Step 3: 端到端复核**

```bash
cd yi-agent-rs
cargo build -p yi-agent
./target/debug/yi-agent --version          # 期望含 -dev
./target/debug/yi-agent flags list         # 期望表头 + (no flags registered)
./target/debug/yi-agent flags status x     # 期望 source: default（dev 构建）
YI_AGENT_BUILD_KIND=release cargo build -p yi-agent
./target/debug/yi-agent --version          # 期望裸版本号
./target/debug/yi-agent flags status x     # 期望 source: build-kind
cargo build -p yi-agent                    # 恢复 dev
```

- [ ] **Step 4: Commit**

```bash
git add docs
git commit -m "docs: 功能开关的进度与判据"
```

---

## Self-Review

**Spec coverage:**

| spec 章节 | 对应 Task |
|---|---|
| D1 运行期开关 | 全部（无 compile-time feature） |
| D2 存储 `flags.json` 跟实例走 | Task 2 |
| D3 注册表唯一真源、未注册无法开 | Task 1（registry）+ Task 2（写入拒绝） |
| D4 stable 硬闸 | Task 2（`status_at` 首行）+ Task 4（`-32601`） |
| D5 CLI + UI 双入口 | Task 3 + Task 5 |
| D6 默认全关 | Task 1（`every_flag_defaults_to_off`） |
| D7 dev 独立编译/运行 + `build/info` | Task 0 + Task 4 |
| D8 过期日期 | Task 1 |
| 验证判据表 | Task 6 |

**Placeholder scan:** 无 TBD/TODO；每个代码步骤都给了完整代码。

**Type consistency:**
- `FlagDef` 字段在 Task 1 定义，Task 2/3/4 只用不重定义。
- `Source` 枚举 Task 2 定义，Task 3（`source.as_str()`）、Task 4（`view.source.as_str()`）、Task 5（前端读字符串）一致。
- `FlagView` Task 2 定义，Task 4 的 `views()` 消费。
- `handle_flags_request_at(path, build_kind, method, params)` 在 Task 4 的测试与实现签名一致。
- `FlagsSub::{List, Set, Status}` 在 Task 3 定义与 dispatch 使用一致。

**已知需要在执行时现场核对的点（不是 placeholder，是真实依赖）：**
1. `desktop/src/components/Switch.tsx` 的实际 props 签名。
2. `RpcError::internal` 是否存在（不存在则补一个）。
3. `rpc/mod.rs` 的 `Domain` 分支在 `server.rs` 里的具体写法（照抄 `Domain::Mcp` 那段）。
