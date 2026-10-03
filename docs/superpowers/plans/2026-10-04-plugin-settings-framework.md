# Plugin Settings Framework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a Settings "Plugins" tab whose per-plugin panels only exist when the plugin is installed, starting with a superpowers-kanban concurrency + timing panel.

**Architecture:** The host gains a generic plugin channel (`plugins/list`, `plugin/settings/read|write`) scoped to the app-server's own workdir; it only enumerates `<workdir>/.yi-agent/supervisors/*.json` and forwards opaque settings to the plugin's query socket. The kanban plugin owns its settings payload in `superpowers-kanban.toml` (concurrency windows + `interval_secs`), validates and writes it, and the runner re-reads the interval every tick. Desktop renders panels via a name registry; TUI lists plugins and edits scalars.

**Tech Stack:** Rust (app-server JSON-RPC, plugin runner, kanban core), React + TypeScript (desktop), ratatui TUI, vitest + cargo test.

## Global Constraints

- Never commit to `main`; all work happens in the worktree `.worktrees/feat/plugin-settings-framework` on branch `feat/plugin-settings-framework`.
- Run `cargo fmt --all` (from `yi-agent-rs/`) before every commit.
- Commit messages: conventional commits, first line ≤72 chars, no `Co-Authored-By`.
- Host code must contain **zero kanban semantics**: it enumerates plugins and forwards JSON verbatim.
- No optimistic updates in the desktop settings panel: a failed write shows an inline error and keeps the user's input.
- Windows in the settings payload use the same `days`/`start`/`end`/`max_tasks`/`all_day` shape as `superpowers-kanban.toml`.
- `interval_secs` range is `[1, 3600]`, default `10`; max_tasks values are `>= 1`.
- Writes go only to the new filename `superpowers-kanban.toml`; the legacy `kanban.toml` stays read-only.
- Do not modify `plugin/query` / `board_query` behavior; the new channel coexists with it.

---

### Task 1: Kanban core — interval field, serialization, and settings validation

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs`
- Test: same file (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: existing `ConcurrencyCalendar`, `ConcurrencyWindow`, `parse_days`, `parse_time`, `load_preferring_new`.
- Produces:
  - `pub const DEFAULT_INTERVAL_SECS: u64 = 10;`
  - `pub struct SettingsError` (implements `Display` + `Error`)
  - `ConcurrencyCalendar::interval_secs: u64`
  - `ConcurrencyCalendar::from_settings_json(value: &serde_json::Value) -> Result<Self, SettingsError>`
  - `ConcurrencyCalendar::to_toml(&self) -> String`
  - `ConcurrencyCalendar::save(&self, state_dir: &std::path::Path) -> std::io::Result<()>`
  - `ConcurrencyCalendar::settings_json(&self) -> serde_json::Value`

- [ ] **Step 1: Write the failing tests**

Add to the existing `tests` module in `calendar.rs`:

```rust
#[test]
fn a_calendar_without_interval_secs_defaults_to_ten() {
    let calendar = ConcurrencyCalendar::from_toml("default_max_tasks = 3").unwrap();
    assert_eq!(calendar.interval_secs, DEFAULT_INTERVAL_SECS);
}

#[test]
fn interval_secs_round_trips_through_toml() {
    let calendar = ConcurrencyCalendar::from_toml("interval_secs = 30").unwrap();
    assert_eq!(calendar.interval_secs, 30);
    let text = calendar.to_toml();
    let reparsed = ConcurrencyCalendar::from_toml(&text).unwrap();
    assert_eq!(reparsed.interval_secs, 30);
}

#[test]
fn to_toml_round_trips_windows_semantically() {
    let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
    let reparsed = ConcurrencyCalendar::from_toml(&calendar.to_toml()).unwrap();
    assert_eq!(reparsed.default_max_tasks, calendar.default_max_tasks);
    assert_eq!(reparsed.interval_secs, calendar.interval_secs);
    assert_eq!(reparsed.windows.len(), calendar.windows.len());
    for (a, b) in reparsed.windows.iter().zip(calendar.windows.iter()) {
        assert_eq!(a.days, b.days);
        assert_eq!(a.start, b.start);
        assert_eq!(a.end, b.end);
        assert_eq!(a.all_day, b.all_day);
        assert_eq!(a.max_tasks, b.max_tasks);
    }
}

#[test]
fn settings_json_round_trips_through_the_calendar() {
    let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
    let json = calendar.settings_json();
    let rebuilt = ConcurrencyCalendar::from_settings_json(&json).unwrap();
    assert_eq!(rebuilt, calendar);
}

#[test]
fn from_settings_json_rejects_an_out_of_range_interval() {
    let json = serde_json::json!({
        "default_max_tasks": 3,
        "interval_secs": 0,
        "windows": []
    });
    let error = ConcurrencyCalendar::from_settings_json(&json).unwrap_err();
    assert!(error.to_string().contains("interval_secs"), "{error}");
}

#[test]
fn from_settings_json_rejects_a_zero_max_tasks() {
    let json = serde_json::json!({
        "default_max_tasks": 0,
        "interval_secs": 10,
        "windows": []
    });
    assert!(ConcurrencyCalendar::from_settings_json(&json).is_err());
}

#[test]
fn from_settings_json_rejects_a_reversed_window() {
    let json = serde_json::json!({
        "default_max_tasks": 3,
        "interval_secs": 10,
        "windows": [
            { "days": "Mon", "start": "12:00", "end": "09:00", "max_tasks": 3 }
        ]
    });
    assert!(ConcurrencyCalendar::from_settings_json(&json).is_err());
}

#[test]
fn save_then_load_prefers_the_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let calendar = ConcurrencyCalendar::from_toml("interval_secs = 45\ndefault_max_tasks = 7").unwrap();
    calendar.save(dir.path()).unwrap();
    assert!(dir.path().join("superpowers-kanban.toml").is_file());
    let loaded = ConcurrencyCalendar::load_preferring_new(dir.path());
    assert_eq!(loaded.interval_secs, 45);
    assert_eq!(loaded.default_max_tasks, 7);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core calendar::`
Expected: compile errors — `interval_secs`, `DEFAULT_INTERVAL_SECS`, `to_toml`, `from_settings_json`, `settings_json`, `save` do not exist.

- [ ] **Step 3: Add the field, constant, and error type**

In `calendar.rs`, near the other top-level items:

```rust
/// 推进循环的默认睡眠周期（秒）。与 supervisor 清单里历史硬编码的 10 保持一致。
pub const DEFAULT_INTERVAL_SECS: u64 = 10;

/// interval_secs 的合法上界：一小时。
const MAX_INTERVAL_SECS: u64 = 3600;

/// 设置写路径的校验失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(String);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid kanban settings: {}", self.0)
    }
}

impl std::error::Error for SettingsError {}
```

Add `interval_secs` to `ConcurrencyCalendar` and its `Default`:

```rust
pub struct ConcurrencyCalendar {
    pub default_max_tasks: u16,
    pub interval_secs: u64,
    pub windows: Vec<ConcurrencyWindow>,
}

impl Default for ConcurrencyCalendar {
    fn default() -> Self {
        Self {
            default_max_tasks: DEFAULT_MAX_TASKS,
            interval_secs: DEFAULT_INTERVAL_SECS,
            windows: Vec::new(),
        }
    }
}
```

- [ ] **Step 4: Parse `interval_secs` in `from_toml`**

Extend `CalendarFile` and `from_toml`:

```rust
struct CalendarFile {
    default_max_tasks: Option<u16>,
    interval_secs: Option<u64>,
    #[serde(default)]
    window: Vec<RawWindow>,
}
```

```rust
Ok(Self {
    default_max_tasks: file.default_max_tasks.unwrap_or(DEFAULT_MAX_TASKS),
    interval_secs: file.interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS),
    windows,
})
```

- [ ] **Step 5: Add `to_toml`, `settings_json`, `from_settings_json`, `save`**

In `impl ConcurrencyCalendar`:

```rust
/// 按规范形式渲染成 TOML。窗口的 `days` 压缩成 `Mon-Fri` 这类简写；
/// 语义等价的往返才是目标，不追求与原文件逐字节相同。
pub fn to_toml(&self) -> String {
    let mut out = format!(
        "default_max_tasks = {}\ninterval_secs = {}\n",
        self.default_max_tasks, self.interval_secs
    );
    for window in &self.windows {
        out.push_str("\n[[window]]\n");
        out.push_str(&format!("days = \"{}\"\n", render_days(&window.days)));
        if window.all_day {
            out.push_str("all_day = true\n");
        } else {
            out.push_str(&format!("start = \"{}\"\n", render_time(window.start)));
            out.push_str(&format!("end = \"{}\"\n", render_time(window.end)));
        }
        out.push_str(&format!("max_tasks = {}\n", window.max_tasks));
    }
    out
}

/// 面向 UI / RPC 的设置载荷。
pub fn settings_json(&self) -> serde_json::Value {
    let windows: Vec<serde_json::Value> = self
        .windows
        .iter()
        .map(|window| {
            serde_json::json!({
                "days": render_days(&window.days),
                "start": render_time(window.start),
                "end": render_time(window.end),
                "all_day": window.all_day,
                "max_tasks": window.max_tasks,
            })
        })
        .collect();
    serde_json::json!({
        "default_max_tasks": self.default_max_tasks,
        "interval_secs": self.interval_secs,
        "windows": windows,
    })
}

/// 从设置载荷构建并校验。任何非法字段都在此拒绝——调用方据此保证零落盘。
pub fn from_settings_json(value: &serde_json::Value) -> Result<Self, SettingsError> {
    let default_max_tasks = value
        .get("default_max_tasks")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SettingsError("default_max_tasks must be a positive integer".into()))?;
    if default_max_tasks < 1 {
        return Err(SettingsError("default_max_tasks must be >= 1".into()));
    }
    let interval_secs = value
        .get("interval_secs")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| SettingsError("interval_secs must be an integer".into()))?;
    if !(1..=MAX_INTERVAL_SECS).contains(&interval_secs) {
        return Err(SettingsError(format!(
            "interval_secs must be in [1, {MAX_INTERVAL_SECS}], got {interval_secs}"
        )));
    }
    let raw_windows = value
        .get("windows")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| SettingsError("windows must be an array".into()))?;
    let mut windows = Vec::with_capacity(raw_windows.len());
    for (index, raw) in raw_windows.iter().enumerate() {
        windows.push(decode_window(raw).map_err(|error| {
            SettingsError(format!("window #{index}: {error}"))
        })?);
    }
    Ok(Self {
        default_max_tasks: default_max_tasks as u16,
        interval_secs,
        windows,
    })
}

/// 原子写：临时文件 + rename。只写新名 `superpowers-kanban.toml`。
pub fn save(&self, state_dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let path = state_dir.join("superpowers-kanban.toml");
    let tmp = state_dir.join(format!(
        "superpowers-kanban.toml.{}.tmp",
        std::process::id()
    ));
    std::fs::write(&tmp, self.to_toml())?;
    std::fs::rename(&tmp, &path)
}
```

Add the free helpers:

```rust
/// `Vec<Weekday>` 压成 `Mon-Fri` 形式的简写。
fn render_days(days: &[Weekday]) -> String {
    use Weekday::*;
    let order = [Mon, Tue, Wed, Thu, Fri, Sat, Sun];
    let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let present: Vec<bool> = order.iter().map(|day| days.contains(day)).collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < 7 {
        if !present[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i + 1 < 7 && present[i + 1] {
            i += 1;
        }
        if i == start {
            parts.push(names[start].to_string());
        } else {
            parts.push(format!("{}-{}", names[start], names[i]));
        }
        i += 1;
    }
    parts.join(",")
}

/// 反渲染 `HH:MM`；当日末尾（`NaiveTime::MIN`）写回 `24:00`。
fn render_time(time: NaiveTime) -> String {
    if time == NaiveTime::MIN {
        return "24:00".to_string();
    }
    time.format("%H:%M").to_string()
}

/// 解一个 `windows` 数组元素；非法即报错。
fn decode_window(value: &serde_json::Value) -> Result<ConcurrencyWindow, String> {
    let all_day = value
        .get("all_day")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let raw = RawWindow {
        days: value
            .get("days")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        start: value
            .get("start")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        end: value
            .get("end")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        all_day,
        max_tasks: value
            .get("max_tasks")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "max_tasks must be a positive integer".to_string())?
            as u16,
    };
    if raw.max_tasks < 1 {
        return Err("max_tasks must be >= 1".into());
    }
    let window = raw.into_window().map_err(|error| error.to_string())?;
    if !window.all_day && window.end <= window.start {
        return Err("start must be before end".into());
    }
    Ok(window)
}
```

Note: `render_time` maps `NaiveTime::MIN` to `24:00`, and `parse_time("24:00")` maps back to `NaiveTime::MIN`, so the round trip is stable. A non-all-day window whose real end is `00:00` will serialize to `24:00` and reparse to `MIN`; the `settings_json`/`to_toml` and `from_toml`/`from_settings_json` paths agree, so the semantic round-trip tests hold.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-core calendar::`
Expected: PASS (existing calendar tests plus the new ones).

- [ ] **Step 7: Format and commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
cd ../.. && git add plugins/superpowers-kanban/crates/superpowers-kanban-core/src/calendar.rs
git commit -m "feat(kanban-core): settings round-trip and interval_secs"
```

---

### Task 2: Plugin dispatch — `settings.read` / `settings.write`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs`
- Test: same file (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `ConcurrencyCalendar::{settings_json, from_settings_json, save, load_preferring_new}` (Task 1).
- Produces (dispatch methods): `"settings.read"` → `{"settings": {...}}`; `"settings.write"` with `params.settings` → `{"ok": true}` or `Err(String)`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `dispatch.rs`:

```rust
#[test]
fn settings_read_returns_the_calendar_settings() {
    let workdir = tempfile::tempdir().unwrap();
    let state = state_dir(workdir.path());
    let value = dispatch_with_global(&state, None, "settings.read", &json!({})).unwrap();
    assert_eq!(value["settings"]["default_max_tasks"], 3);
    assert_eq!(value["settings"]["interval_secs"], 10);
    assert!(value["settings"]["windows"].as_array().unwrap().is_empty());
}

#[test]
fn settings_write_then_read_round_trips() {
    let workdir = tempfile::tempdir().unwrap();
    let state = state_dir(workdir.path());
    let payload = json!({
        "default_max_tasks": 5,
        "interval_secs": 25,
        "windows": [
            { "days": "Mon-Fri", "start": "09:00", "end": "24:00", "max_tasks": 3 }
        ]
    });
    dispatch_with_global(&state, None, "settings.write", &json!({ "settings": payload }))
        .unwrap();
    // 文件确实落在新名下。
    assert!(state.join("superpowers-kanban.toml").is_file());
    let value = dispatch_with_global(&state, None, "settings.read", &json!({})).unwrap();
    assert_eq!(value["settings"]["default_max_tasks"], 5);
    assert_eq!(value["settings"]["interval_secs"], 25);
    assert_eq!(value["settings"]["windows"].as_array().unwrap().len(), 1);
}

#[test]
fn an_invalid_settings_write_leaves_no_file() {
    let workdir = tempfile::tempdir().unwrap();
    let state = state_dir(workdir.path());
    let bad = json!({
        "default_max_tasks": 3,
        "interval_secs": 0,
        "windows": []
    });
    let error = dispatch_with_global(&state, None, "settings.write", &json!({ "settings": bad }))
        .unwrap_err();
    assert!(error.contains("interval_secs"), "{error}");
    assert!(
        !state.join("superpowers-kanban.toml").exists(),
        "a rejected write must not leave the settings file behind"
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner dispatch::`
Expected: FAIL — `unknown method: settings.read`.

- [ ] **Step 3: Implement the two methods**

In `dispatch_with_service`, add arms before `other =>`:

```rust
"settings.read" => {
    let calendar =
        superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(state_dir);
    Ok(json!({ "settings": calendar.settings_json() }))
}
"settings.write" => {
    let settings = params
        .get("settings")
        .ok_or_else(|| "settings.write needs a `settings` object".to_string())?;
    // 先校验后落盘：非法输入必须一个字节都不写。
    let calendar = superpowers_kanban_core::calendar::ConcurrencyCalendar::from_settings_json(
        settings,
    )
    .map_err(|error| error.to_string())?;
    calendar
        .save(state_dir)
        .map_err(|error| format!("could not save settings: {error}"))?;
    Ok(json!({ "ok": true }))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner dispatch::`
Expected: PASS.

- [ ] **Step 5: Format and commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
cd ../.. && git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/dispatch.rs
git commit -m "feat(kanban-plugin): settings.read/write dispatch"
```

---

### Task 3: Runner re-reads the interval each tick

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Modify: `plugins/superpowers-kanban/supervisors/superpowers-kanban.json`
- Test: `main.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `ConcurrencyCalendar::load_preferring_new` + `interval_secs` (Task 1).
- Produces: `fn effective_interval(cli: Option<Duration>, state_dir: &Path) -> Duration` — CLI wins, else config, else default.

- [ ] **Step 1: Write the failing test**

Add to `main.rs` tests module (create it at the file end if absent):

```rust
#[test]
fn the_configured_interval_wins_over_the_default() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("superpowers-kanban.toml"), "interval_secs = 42\n").unwrap();
    assert_eq!(
        effective_interval(None, dir.path()),
        Duration::from_secs(42),
        "the settings file drives the tick interval"
    );
}

#[test]
fn an_explicit_cli_interval_wins_over_the_config() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("superpowers-kanban.toml"), "interval_secs = 42\n").unwrap();
    assert_eq!(
        effective_interval(Some(Duration::from_secs(5)), dir.path()),
        Duration::from_secs(5),
        "an explicit CLI flag is an override"
    );
}

#[test]
fn a_missing_config_falls_back_to_ten() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(effective_interval(None, dir.path()), Duration::from_secs(10));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner effective_interval`
Expected: compile error — `effective_interval` not found.

- [ ] **Step 3: Add the helper and use it in the loop**

Change `Args.interval` to `interval: Option<Duration>` so "explicit CLI" is distinguishable from the default. Add near the run loop:

```rust
/// 当前 tick 的睡眠周期：显式 CLI > 插件配置 > 默认 10 秒。
///
/// 每 tick 重新调用，因此设置界面改了 `interval_secs` 后下一个 tick 即生效，
/// 无需重启插件进程。
fn effective_interval(cli: Option<Duration>, state_dir: &std::path::Path) -> Duration {
    if let Some(cli) = cli {
        return cli;
    }
    let secs = ConcurrencyCalendar::load_preferring_new(state_dir).interval_secs;
    Duration::from_secs(secs)
}
```

In parsing, set `interval: None` by default and `Some(Duration::from_secs(value))` when `--interval-secs` is present (replacing `let mut interval_secs = 10_u64;` with `let mut interval: Option<Duration> = None;`).

In `run_daemon`, replace both `std::thread::sleep(args.interval);` calls with:

```rust
std::thread::sleep(effective_interval(args.interval, &args.state_dir));
```

Also change the startup log line in the runner loop that mentions `3 × 10s` to compute from the current interval, or simplify it to a fixed comment; keep behavior identical. Specifically replace the `// 阈值 3 × 10s ≈ 30s 让位窗口。` comment with `// 阈值 3 × interval ≈ 让位窗口。` (comment only, no code change).

- [ ] **Step 4: Drop `--interval-secs` from the shipped manifest**

In `plugins/superpowers-kanban/supervisors/superpowers-kanban.json`, remove the final two array elements so `args` ends at `"{workdir}"`:

```json
"args": [
  "run",
  "--runtime-dir",
  "{runtime_dir}",
  "--state-dir",
  "{state_dir}",
  "--project-root",
  "{workdir}"
],
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`
Expected: PASS (existing arg-parsing tests still pass because `--interval-secs` is still accepted).

- [ ] **Step 6: Format and commit**

```bash
cd plugins/superpowers-kanban && cargo fmt --all
cd ../.. && git add plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs plugins/superpowers-kanban/supervisors/superpowers-kanban.json
git commit -m "feat(kanban-plugin): re-read tick interval from plugin settings"
```

---

### Task 4: App-server `plugins/list` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml` (add dependency)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `server.rs` (`mod plugin_query_tests` or a new `mod plugins_tests`)

**Interfaces:**
- Consumes: `yi_agent_supervisors::manifest::load_manifests(dir)`; app-server `workdir` (the JSON-RPC handler reads it from `theme_handle.workdir()`).
- Produces:
  - `fn list_installed_plugins(workdir: &Path) -> Vec<PluginSummary>` with `struct PluginSummary { name: String, queryable: bool, switch_key: String }`
  - RPC `plugins/list` → `{"plugins": [{"name":..,"queryable":..,"switch_key":..}]}`

- [ ] **Step 1: Add the dependency**

In `yi-agent-rs/crates/yi-agent-app-server/Cargo.toml`, under `[dependencies]` next to `yi-agent-boards`:

```toml
yi-agent-supervisors = { path = "../yi-agent-supervisors" }
```

- [ ] **Step 2: Write the failing test**

Add a new `mod plugins_tests` in `server.rs`:

```rust
mod plugins_tests {
    use super::list_installed_plugins;

    #[test]
    fn an_absent_supervisors_directory_lists_no_plugins() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_installed_plugins(dir.path()).is_empty());
    }

    #[test]
    fn a_manifest_with_a_query_socket_is_queryable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        std::fs::write(
            sup.join("plain.json"),
            r#"{"name":"plain","command":"y","switch_key":"plain_on"}"#,
        )
        .unwrap();
        let mut plugins = list_installed_plugins(dir.path());
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(plugins.len(), 2);
        assert_eq!(plugins[0].name, "plain");
        assert!(!plugins[0].queryable);
        assert_eq!(plugins[1].name, "superpowers-kanban");
        assert!(plugins[1].queryable);
        assert_eq!(plugins[1].switch_key, "superpowers_kanban");
    }
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server plugins_tests`
Expected: compile error — `list_installed_plugins` not found.

- [ ] **Step 4: Implement the helper and the RPC arm**

Add near `plugin_query` in `server.rs`:

```rust
/// 已装插件的一行。宿主不解释任何插件语义：`name`/`switch_key` 原样来自清单，
/// `queryable` 只是「该清单声明了 query_socket」。
pub(crate) struct PluginSummary {
    pub name: String,
    pub queryable: bool,
    pub switch_key: String,
}

/// 列出 `<workdir>/.yi-agent/supervisors/` 下的插件清单。
///
/// 纯文件读取：daemon 没起也能列。目录缺失或清单损坏都只是「少一个插件」，
/// 不是错误——一个坏清单不该让整个设置页打不开。
pub(crate) fn list_installed_plugins(workdir: &Path) -> Vec<PluginSummary> {
    let dir = workdir.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| PluginSummary {
            name: manifest.name,
            queryable: manifest.query_socket.is_some(),
            switch_key: manifest.switch_key,
        })
        .collect()
}
```

In the main JSON-RPC `match`, add an arm (alongside `"plugin/query"`):

```rust
"plugins/list" => {
    let workdir = theme_handle.workdir();
    let plugins: Vec<serde_json::Value> = list_installed_plugins(workdir)
        .into_iter()
        .map(|plugin| {
            json!({
                "name": plugin.name,
                "queryable": plugin.queryable,
                "switch_key": plugin.switch_key,
            })
        })
        .collect();
    write_response(&hub, &client, ok_response(id, json!({ "plugins": plugins }))).await?;
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server plugins_tests`
Expected: PASS.

- [ ] **Step 6: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/Cargo.toml yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/Cargo.lock
git commit -m "feat(app-server): add plugins/list RPC"
```

---

### Task 5: App-server `plugin/settings/read|write` RPC

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs` (add `plugin_not_installed` code)
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `server.rs`

**Interfaces:**
- Consumes: `list_installed_plugins` (Task 4); existing `yi_agent_store::ipc::{send_request, IpcRequest::PluginQuery}` and `yi_agent_subagent::attach::project_runtime_directory`.
- Produces:
  - `fn plugin_query_self(workdir: &Path, plugin: &str, method: &str, params: Value) -> Result<Value, BoardQueryError>`
  - RPC `plugin/settings/read` `{plugin}` → plugin's `settings.read` result verbatim
  - RPC `plugin/settings/write` `{plugin, settings}` → plugin's `settings.write` result verbatim

- [ ] **Step 1: Add the `plugin_not_installed` code**

In `protocol.rs`, extend the `board_query` numeric map:

```rust
let numeric = match code {
    "board_not_created" => -32020,
    "daemon_unavailable" => -32021,
    "plugin_unavailable" => -32022,
    "plugin_not_installed" => -32023,
    _ => -32603,
};
```

- [ ] **Step 2: Write the failing tests**

Add to `mod plugins_tests` in `server.rs`:

```rust
    #[test]
    fn a_plugin_not_in_the_manifests_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let error = super::plugin_query_self(dir.path(), "ghost", "settings.read", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, "plugin_not_installed", "{error}");
    }

    #[test]
    fn an_installed_plugin_without_a_query_socket_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("quiet.json"),
            r#"{"name":"quiet","command":"y","switch_key":"quiet_on"}"#,
        )
        .unwrap();
        let error = super::plugin_query_self(dir.path(), "quiet", "settings.read", serde_json::json!({}))
            .unwrap_err();
        assert_eq!(error.code, "plugin_unavailable", "{error}");
    }
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server plugins_tests`
Expected: compile error — `plugin_query_self` not found.

- [ ] **Step 4: Implement `plugin_query_self`**

Add near `plugin_query` in `server.rs`:

```rust
/// 向本宿主 workdir 的插件问话。
///
/// 与 `plugin_query` 的区别：作用域是 app-server 自己的 workdir，不带 `project`，
/// 也不受看板登记表门控——这是一条通用插件通道，任何插件都能走。宿主只转发
/// `method`/`params`，不解释应答。
fn plugin_query_self(
    workdir: &Path,
    plugin: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, BoardQueryError> {
    if plugin.is_empty() {
        return Err(BoardQueryError::new(
            "invalid_params",
            "plugin/settings needs a `plugin` name",
        ));
    }
    let summary = list_installed_plugins(workdir)
        .into_iter()
        .find(|summary| summary.name == plugin);
    let Some(summary) = summary else {
        return Err(BoardQueryError::new(
            "plugin_not_installed",
            format!("plugin {plugin} is not installed"),
        ));
    };
    if !summary.queryable {
        return Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("plugin {plugin} does not declare a query socket"),
        ));
    }
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(workdir);
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).map_err(|error| {
        BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| {
        BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon is unavailable: {error}"),
        )
    })?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!(
                "the plugin rejected the query: {code:?} {}",
                message.unwrap_or_default()
            ),
        )),
        other => Err(BoardQueryError::new(
            "plugin_unavailable",
            format!("daemon returned an unexpected response: {other:?}"),
        )),
    }
}
```

- [ ] **Step 5: Add the two RPC arms**

Alongside `"plugins/list"`:

```rust
"plugin/settings/read" => {
    let workdir = theme_handle.workdir().to_path_buf();
    let plugin = req
        .params
        .get("plugin")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match plugin_query_self(&workdir, &plugin, "settings.read", json!({})) {
        Ok(value) => write_response(&hub, &client, ok_response(id, value)).await?,
        Err(error) => {
            write_response(
                &hub,
                &client,
                err_response(id, RpcError::board_query(error.code, error.message)),
            )
            .await?
        }
    }
}
"plugin/settings/write" => {
    let workdir = theme_handle.workdir().to_path_buf();
    let plugin = req
        .params
        .get("plugin")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let settings = req.params.get("settings").cloned().unwrap_or(json!({}));
    match plugin_query_self(
        &workdir,
        &plugin,
        "settings.write",
        json!({ "settings": settings }),
    ) {
        Ok(value) => write_response(&hub, &client, ok_response(id, value)).await?,
        Err(error) => {
            write_response(
                &hub,
                &client,
                err_response(id, RpcError::board_query(error.code, error.message)),
            )
            .await?
        }
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server plugins_tests`
Expected: PASS.

- [ ] **Step 7: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): add plugin/settings read-write RPCs"
```

---

### Task 6: Desktop plugin-settings client

**Files:**
- Create: `desktop/src/lib/pluginSettings.ts`
- Test: `desktop/src/lib/pluginSettings.test.ts`

**Interfaces:**
- Consumes: an RPC seam shaped like `(method, params) => Promise<unknown>` (same as `BoardRpc`).
- Produces:
  - `export interface PluginSummary { name: string; queryable: boolean; switchKey: string }`
  - `export interface KanbanSettings { default_max_tasks: number; interval_secs: number; windows: KanbanWindow[] }`
  - `export interface KanbanWindow { days: string; start: string; end: string; all_day: boolean; max_tasks: number }`
  - `listPlugins(rpc)`, `readKanbanSettings(rpc)`, `writeKanbanSettings(rpc, settings)`
  - `export type PluginErrorKind = "not_installed" | "not_running" | "other"`
  - `pluginErrorKind(error: unknown): PluginErrorKind`

- [ ] **Step 1: Write the failing test**

Create `desktop/src/lib/pluginSettings.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import {
  listPlugins,
  readKanbanSettings,
  writeKanbanSettings,
  pluginErrorKind,
} from "./pluginSettings";

describe("pluginSettings", () => {
  it("maps plugins/list rows", async () => {
    const rpc = async () => ({
      plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "superpowers_kanban" }],
    });
    expect(await listPlugins(rpc)).toEqual([
      { name: "superpowers-kanban", queryable: true, switchKey: "superpowers_kanban" },
    ]);
  });

  it("reads kanban settings through the plugin channel", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    const settings = await readKanbanSettings(rpc);
    expect(settings.interval_secs).toBe(10);
    expect(calls[0]).toEqual([
      "plugin/settings/read",
      { plugin: "superpowers-kanban" },
    ]);
  });

  it("writes kanban settings", async () => {
    const calls: unknown[] = [];
    const rpc = async (method: string, params: unknown) => {
      calls.push([method, params]);
      return { ok: true };
    };
    const settings = { default_max_tasks: 5, interval_secs: 20, windows: [] };
    await writeKanbanSettings(rpc, settings);
    expect(calls[0]).toEqual([
      "plugin/settings/write",
      { plugin: "superpowers-kanban", settings },
    ]);
  });

  it("distinguishes not_installed from not_running", () => {
    expect(pluginErrorKind({ data: { code: "plugin_not_installed" } })).toBe("not_installed");
    expect(pluginErrorKind({ data: { code: "plugin_unavailable" } })).toBe("not_running");
    expect(pluginErrorKind(new Error("boom"))).toBe("other");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd desktop && npx vitest run src/lib/pluginSettings.test.ts`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement the module**

Create `desktop/src/lib/pluginSettings.ts`:

```ts
/** 桌面与宿主通用插件通道之间的接缝。形状与 `BoardRpc` 一致。 */
export type PluginRpc = <T = unknown>(method: string, params: unknown) => Promise<T>;

/** 看板插件名。宿主按名字转发，壳子不含看板语义。 */
export const KANBAN_PLUGIN = "superpowers-kanban";

export interface PluginSummary {
  name: string;
  queryable: boolean;
  switchKey: string;
}

export interface KanbanWindow {
  days: string;
  start: string;
  end: string;
  all_day: boolean;
  max_tasks: number;
}

export interface KanbanSettings {
  default_max_tasks: number;
  interval_secs: number;
  windows: KanbanWindow[];
}

/** 列出 app-server workdir 下已装的插件。 */
export async function listPlugins(rpc: PluginRpc): Promise<PluginSummary[]> {
  const result = await rpc<{ plugins?: Array<Record<string, unknown>> }>("plugins/list", {});
  const rows = Array.isArray(result?.plugins) ? result.plugins : [];
  return rows
    .filter((row) => typeof row.name === "string")
    .map((row) => ({
      name: row.name as string,
      queryable: row.queryable === true,
      switchKey: typeof row.switch_key === "string" ? (row.switch_key as string) : "",
    }));
}

/** 经插件通道读看板设置。 */
export async function readKanbanSettings(rpc: PluginRpc): Promise<KanbanSettings> {
  const result = await rpc<{ settings?: KanbanSettings }>("plugin/settings/read", {
    plugin: KANBAN_PLUGIN,
  });
  if (!result?.settings) throw new Error("the plugin returned no settings");
  return result.settings;
}

/** 经插件通道写看板设置（全量替换）。 */
export async function writeKanbanSettings(
  rpc: PluginRpc,
  settings: KanbanSettings,
): Promise<void> {
  await rpc("plugin/settings/write", { plugin: KANBAN_PLUGIN, settings });
}

/** 插件设置的三种失败，UI 各给一句话。 */
export type PluginErrorKind = "not_installed" | "not_running" | "other";

/**
 * 读 `data.code`（结构化优先），退路是 message 文本——宿主在补上码之前
 * 只有人话。
 */
export function pluginErrorKind(error: unknown): PluginErrorKind {
  const record = error as { message?: unknown; data?: { code?: unknown } } | null;
  const code = typeof record?.data?.code === "string" ? record.data.code : "";
  if (code === "plugin_not_installed") return "not_installed";
  if (code === "plugin_unavailable") return "not_running";
  const text =
    typeof record?.message === "string"
      ? record.message
      : error instanceof Error
        ? error.message
        : String(error ?? "");
  if (text.includes("plugin_not_installed") || text.includes("is not installed")) {
    return "not_installed";
  }
  if (text.includes("plugin_unavailable") || text.includes("is not available")) {
    return "not_running";
  }
  return "other";
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd desktop && npx vitest run src/lib/pluginSettings.test.ts`
Expected: PASS.

- [ ] **Step 5: Typecheck and commit**

```bash
cd desktop && npx tsc --noEmit
cd .. && git add desktop/src/lib/pluginSettings.ts desktop/src/lib/pluginSettings.test.ts
git commit -m "feat(desktop): plugin settings RPC client"
```

---

### Task 7: Desktop Plugins tab, registry, and kanban panel

**Files:**
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Create: `desktop/src/components/SettingsPluginsTab.tsx`
- Create: `desktop/src/components/SuperpowersKanbanPluginSettings.tsx`
- Test: `desktop/src/components/SettingsDialog.test.tsx`, `desktop/src/components/SettingsPluginsTab.test.tsx`
- Modify: `desktop/src/App.tsx` (pass a plugin RPC seam into the dialog)

**Interfaces:**
- Consumes: `pluginSettings.ts` (Task 6).
- Produces:
  - `SettingsDialog` new prop `pluginCall?: (method: string, params: unknown) => Promise<unknown>`.
  - `SettingsPluginsTab({ call })` renders plugin list; registry maps `"superpowers-kanban"` → `SuperpowersKanbanPluginSettings`.
  - `SuperpowersKanbanPluginSettings({ rpc })`.

- [ ] **Step 1: Write the failing tests**

Add to `desktop/src/components/SettingsDialog.test.tsx`:

```tsx
  it("shows a 插件 tab that reports the empty state", async () => {
    const pluginCall = async () => ({ plugins: [] });
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        pluginCall={pluginCall}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "插件" }));
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });
```

Create `desktop/src/components/SettingsPluginsTab.test.tsx`:

```tsx
/** @vitest-environment jsdom */
import { afterEach, describe, expect, it } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import { SettingsPluginsTab } from "./SettingsPluginsTab";

afterEach(cleanup);

describe("SettingsPluginsTab", () => {
  it("shows the empty state when nothing is installed", async () => {
    const call = async () => ({ plugins: [] });
    render(<SettingsPluginsTab call={call} />);
    expect(await screen.findByText("未安装任何插件")).toBeTruthy();
  });

  it("renders a neutral row for a plugin without a registered panel", async () => {
    const call = async () => ({
      plugins: [{ name: "mystery", queryable: true, switch_key: "mystery_on" }],
    });
    render(<SettingsPluginsTab call={call} />);
    expect(await screen.findByText("mystery")).toBeTruthy();
    expect(screen.getByText("该插件暂无可配置项")).toBeTruthy();
  });

  it("renders the kanban panel for the kanban plugin", async () => {
    const call = async (method: string) => {
      if (method === "plugins/list") {
        return { plugins: [{ name: "superpowers-kanban", queryable: true, switch_key: "k" }] };
      }
      return { settings: { default_max_tasks: 3, interval_secs: 10, windows: [] } };
    };
    render(<SettingsPluginsTab call={call} />);
    expect(await screen.findByLabelText("默认并发上限")).toBeTruthy();
    expect(await screen.findByLabelText("推进间隔秒数")).toBeTruthy();
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd desktop && npx vitest run src/components/SettingsPluginsTab.test.tsx src/components/SettingsDialog.test.tsx`
Expected: FAIL — components/prop missing.

- [ ] **Step 3: Create `SuperpowersKanbanPluginSettings.tsx`**

The panel: load settings on mount, edit `default_max_tasks`, `interval_secs`, and a window table; save via `writeKanbanSettings`; on failure show an inline error and keep the user's input.

```tsx
import { useEffect, useState } from "react";
import {
  readKanbanSettings,
  writeKanbanSettings,
  pluginErrorKind,
  type KanbanSettings,
  type KanbanWindow,
  type PluginRpc,
} from "../lib/pluginSettings";

const EMPTY_WINDOW: KanbanWindow = {
  days: "Mon-Fri",
  start: "09:00",
  end: "24:00",
  all_day: false,
  max_tasks: 3,
};

/** superpowers-kanban 的插件设置面板：并发窗口 + 推进间隔。 */
export function SuperpowersKanbanPluginSettings({ rpc }: { rpc: PluginRpc }) {
  const [settings, setSettings] = useState<KanbanSettings | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await readKanbanSettings(rpc);
        if (!cancelled) setSettings(loaded);
      } catch (e) {
        if (!cancelled) {
          setError(
            pluginErrorKind(e) === "not_running"
              ? "插件未运行，无法读取设置"
              : "无法读取设置",
          );
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [rpc]);

  if (error && settings === null) {
    return <p role="alert" className="p-4 text-xs text-amber-500">{error}</p>;
  }
  if (settings === null) {
    return <p className="p-4 text-xs text-fg-subtle">正在读取设置…</p>;
  }

  const patch = (next: Partial<KanbanSettings>) => setSettings({ ...settings, ...next });
  const patchWindow = (index: number, next: Partial<KanbanWindow>) => {
    const windows = settings.windows.map((w, i) => (i === index ? { ...w, ...next } : w));
    patch({ windows });
  };

  const save = async () => {
    setBusy(true);
    setError(null);
    try {
      await writeKanbanSettings(rpc, settings);
    } catch (e) {
      setError(pluginErrorKind(e) === "other" ? "保存失败" : "插件未运行，保存失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="p-4">
      <label className="flex items-center gap-3 text-sm">
        <span>默认并发上限</span>
        <input
          aria-label="默认并发上限"
          type="number"
          min={1}
          value={settings.default_max_tasks}
          onChange={(e) => patch({ default_max_tasks: Number(e.target.value) })}
        />
      </label>
      <label className="mt-3 flex items-center gap-3 text-sm">
        <span>推进间隔秒数</span>
        <input
          aria-label="推进间隔秒数"
          type="number"
          min={1}
          max={3600}
          value={settings.interval_secs}
          onChange={(e) => patch({ interval_secs: Number(e.target.value) })}
        />
      </label>

      <h3 className="mt-4 text-sm font-medium text-fg">时段并发窗口</h3>
      <table className="mt-2 w-full text-xs">
        <thead>
          <tr>
            <th>星期</th>
            <th>开始</th>
            <th>结束</th>
            <th>上限</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {settings.windows.map((w, index) => (
            <tr key={index}>
              <td>
                <input
                  aria-label={`窗口 ${index} 星期`}
                  value={w.days}
                  onChange={(e) => patchWindow(index, { days: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 开始`}
                  value={w.start}
                  disabled={w.all_day}
                  onChange={(e) => patchWindow(index, { start: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 结束`}
                  value={w.end}
                  disabled={w.all_day}
                  onChange={(e) => patchWindow(index, { end: e.target.value })}
                />
              </td>
              <td>
                <input
                  aria-label={`窗口 ${index} 上限`}
                  type="number"
                  min={1}
                  value={w.max_tasks}
                  onChange={(e) => patchWindow(index, { max_tasks: Number(e.target.value) })}
                />
              </td>
              <td>
                <button
                  type="button"
                  aria-label={`删除窗口 ${index}`}
                  onClick={() => patch({ windows: settings.windows.filter((_, i) => i !== index) })}
                >
                  删除
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
      <button
        type="button"
        className="mt-2 text-xs"
        onClick={() => patch({ windows: [...settings.windows, { ...EMPTY_WINDOW }] })}
      >
        添加窗口
      </button>

      <div className="mt-4">
        <button
          type="button"
          disabled={busy}
          onClick={() => void save()}
          className="rounded border border-line px-3 py-1 text-sm"
        >
          保存
        </button>
      </div>
      {error && (
        <p role="alert" className="mt-2 text-xs text-amber-500">
          {error}
        </p>
      )}
    </section>
  );
}
```

- [ ] **Step 4: Create `SettingsPluginsTab.tsx`**

```tsx
import { useEffect, useState, type ComponentType } from "react";
import { listPlugins, type PluginRpc, type PluginSummary } from "../lib/pluginSettings";
import { SuperpowersKanbanPluginSettings } from "./SuperpowersKanbanPluginSettings";

/**
 * 插件名 → 设置面板。未注册的插件仍列出来，只是没有面板——不静默消失。
 * 加一个插件 = 往这里加一行。
 */
const REGISTRY: Record<string, ComponentType<{ rpc: PluginRpc }>> = {
  "superpowers-kanban": SuperpowersKanbanPluginSettings,
};

export function SettingsPluginsTab({ call }: { call?: PluginRpc }) {
  const [plugins, setPlugins] = useState<PluginSummary[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!call) {
      setPlugins([]);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const loaded = await listPlugins(call);
        if (!cancelled) setPlugins(loaded);
      } catch {
        if (!cancelled) setError("无法读取插件列表");
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [call]);

  if (error) {
    return <p role="alert" className="p-4 text-xs text-amber-500">{error}</p>;
  }
  if (plugins === null) {
    return <p className="p-4 text-xs text-fg-subtle">正在读取插件…</p>;
  }
  if (plugins.length === 0) {
    return <p className="p-4 text-sm text-fg-muted">未安装任何插件</p>;
  }

  return (
    <div className="divide-y divide-line">
      {plugins.map((plugin) => {
        const Panel = REGISTRY[plugin.name];
        return (
          <section key={plugin.name} className="p-4">
            <h2 className="text-sm font-medium text-fg">{plugin.name}</h2>
            {Panel ? (
              <Panel rpc={call as PluginRpc} />
            ) : (
              <p className="mt-2 text-xs text-fg-subtle">该插件暂无可配置项</p>
            )}
          </section>
        );
      })}
    </div>
  );
}
```

- [ ] **Step 5: Wire the tab into `SettingsDialog.tsx`**

Add to `TABS`:

```ts
const TABS = [
  { id: "general", label: "通用" },
  { id: "remote", label: "远程访问" },
  { id: "plugins", label: "插件" },
] as const;
```

Add the prop to the component signature's props type:

```ts
  /** 「插件」Tab 的宿主 RPC 接缝（plugins/list、plugin/settings/*）。 */
  pluginCall?: (method: string, params: unknown) => Promise<unknown>;
```

Destructure `pluginCall` and render it in the panel body (replace the two-way ternary with a switch-like chain):

```tsx
            {active === "general" ? (
              <SettingsGeneralTab theme={theme} onThemeChange={onThemeChange} />
            ) : active === "remote" ? (
              <SettingsRemoteTab
                call={remoteCall}
                initialRelayUrl={relayUrl}
                saveRelayUrl={saveRelayUrl}
              />
            ) : (
              <SettingsPluginsTab call={pluginCall} />
            )}
```

Add the import: `import { SettingsPluginsTab } from "./SettingsPluginsTab";`

Note: `pluginCall`'s type is a plain `(method, params) => Promise<unknown>`; `SettingsPluginsTab` expects `PluginRpc = <T>(method, params) => Promise<T>`. The plain function is assignable to the generic at the call site only if structurally compatible; if `tsc` complains, type the prop as `PluginRpc` by importing it: `import type { PluginRpc } from "../lib/pluginSettings";` and use `pluginCall?: PluginRpc;`.

- [ ] **Step 6: Wire `App.tsx`**

In `App.tsx`, pass the plugin seam to `SettingsDialog` (next to `remoteCall`):

```tsx
        pluginCall={(method, params) =>
          (clientRef.current as RpcClient).request(method, params)
        }
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cd desktop && npx vitest run src/components/SettingsPluginsTab.test.tsx src/components/SettingsDialog.test.tsx`
Expected: PASS.

- [ ] **Step 8: Typecheck and full desktop suite**

Run: `cd desktop && npx tsc --noEmit && npx vitest run`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add desktop/src/components/SettingsDialog.tsx desktop/src/components/SettingsPluginsTab.tsx desktop/src/components/SuperpowersKanbanPluginSettings.tsx desktop/src/components/SettingsDialog.test.tsx desktop/src/components/SettingsPluginsTab.test.tsx desktop/src/App.tsx
git commit -m "feat(desktop): Plugins settings tab with kanban panel"
```

---

### Task 8: TUI `/plugins` command

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/src/tui/plugins.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/slash.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/mod.rs` (register module, if modules are listed there)
- Test: `plugins.rs` and `slash.rs`

**Interfaces:**
- Consumes: `yi_agent_supervisors::manifest::load_manifests` (add dep to `yi-agent` crate if absent); the daemon plugin channel via `yi_agent_store::ipc` (same pattern as `superpowers_kanban::query_plugin`).
- Produces:
  - `pub fn handle_plugins(workdir: &Path, args: &str) -> Vec<String>`
  - `SlashCommand::Plugins` with name `"plugins"`.

- [ ] **Step 1: Write the failing tests**

Add to `slash.rs` tests:

```rust
#[test]
fn plugins_is_a_known_command() {
    assert_eq!(SlashCommand::from_name("plugins"), Some(SlashCommand::Plugins));
    assert!(SlashCommand::all().contains(&SlashCommand::Plugins));
    assert_eq!(SlashCommand::Plugins.name(), "plugins");
}
```

Add a `#[cfg(test)] mod tests` to `plugins.rs`:

```rust
#[test]
fn no_plugins_prints_a_neutral_line() {
    let dir = tempfile::tempdir().unwrap();
    let lines = handle_plugins(dir.path(), "");
    assert!(lines.iter().any(|line| line.contains("未安装任何插件")), "{lines:?}");
}

#[test]
fn an_installed_plugin_is_listed() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"k","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
    )
    .unwrap();
    let lines = handle_plugins(dir.path(), "");
    assert!(lines.iter().any(|line| line.contains("superpowers-kanban")), "{lines:?}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cd yi-agent-rs && cargo test -p yi-agent plugins`
Expected: compile error — `Plugins` variant and `handle_plugins` not found.

- [ ] **Step 3: Add the `Plugins` slash command**

In `slash.rs`:
- Add `Plugins,` to the `SlashCommand` enum.
- In `control_spec`, add `Self::Plugins` to the arm that returns `None` (with `Runtime`, `Kanban`).
- In `name()`, add `SlashCommand::Plugins => "plugins",`.
- In `description()`, add `SlashCommand::Plugins => "列出已安装插件 / 查看与设置插件配置",`.
- In the args hint, add `SlashCommand::Plugins => Some("[<name> [set <key> <value>]]"),`.
- In `all()`, add `SlashCommand::Plugins,`.
- In `from_name`, add `if name == "plugins" { return Some(Self::Plugins); }`.

- [ ] **Step 4: Create `tui/plugins.rs`**

```rust
//! `/plugins`——列出本机（当前工作目录）已装插件，并读/写插件设置。
//!
//! 与看板一样走 daemon 的通用 `plugin/query` 通道；列表则直接读
//! `<workdir>/.yi-agent/supervisors/`，因为「装了没装」是文件事实，不需要 daemon。

use std::path::Path;

use serde_json::{json, Value};

/// 经 daemon 问插件一个问题，返回原始应答。失败给一句人话。
fn query_plugin(workdir: &Path, plugin: &str, method: &str, params: Value) -> Result<Value, String> {
    let socket = crate::runtime_socket_for(workdir)
        .map_err(|error| format!("no daemon socket: {error}"))?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: plugin.to_string(),
            method: method.to_string(),
            params,
        },
    )
    .map_err(|error| format!("daemon is unavailable: {error}"))?;
    match response {
        yi_agent_store::ipc::IpcResponse::PluginResult { value } => Ok(value),
        yi_agent_store::ipc::IpcResponse::Error { message, .. } => {
            Err(message.unwrap_or_else(|| "the plugin rejected the query".to_string()))
        }
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// 已装插件名（按清单）。
fn installed(workdir: &Path) -> Vec<String> {
    let dir = workdir.join(".yi-agent").join("supervisors");
    yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .map(|manifest| manifest.name)
        .collect()
}

/// `/plugins [<name> [set <key> <value>]]`。
pub fn handle_plugins(workdir: &Path, args: &str) -> Vec<String> {
    let mut words = args.split_whitespace();
    let first = words.next();
    let rest: Vec<&str> = words.collect();

    match (first, rest.as_slice()) {
        (None, _) => {
            let names = installed(workdir);
            if names.is_empty() {
                return vec!["未安装任何插件".to_string()];
            }
            let mut lines = vec!["已安装插件：".to_string()];
            lines.extend(names.into_iter().map(|name| format!("- {name}")));
            lines
        }
        (Some(name), []) => match query_plugin(workdir, name, "settings.read", json!({})) {
            Ok(value) => render_settings(name, &value),
            Err(message) => vec![format!("无法读取 {name} 的设置: {message}")],
        },
        (Some(name), ["set", key, value]) => {
            let current = match query_plugin(workdir, name, "settings.read", json!({})) {
                Ok(value) => value,
                Err(message) => return vec![format!("无法读取 {name} 的设置: {message}")],
            };
            let mut settings = current
                .get("settings")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let parsed: serde_json::Value = match value.parse() {
                Ok(parsed) => parsed,
                Err(_) => json!(value),
            };
            if let Some(object) = settings.as_object_mut() {
                object.insert((*key).to_string(), parsed);
            } else {
                return vec![format!("{name} 的设置不是对象，无法设置 {key}")];
            }
            match query_plugin(
                workdir,
                name,
                "settings.write",
                json!({ "settings": settings }),
            ) {
                Ok(_) => vec![format!("{name}: 已设置 {key} = {value}")],
                Err(message) => vec![format!("{name}: 设置失败: {message}")],
            }
        }
        _ => vec![
            "usage: /plugins [<name> [set <key> <value>]]".to_string(),
        ],
    }
}

/// 把 `settings.read` 的载荷渲染成几行。窗口表只读展示，编辑请去桌面端。
fn render_settings(name: &str, value: &Value) -> Vec<String> {
    let settings = value.get("settings").unwrap_or(value);
    let mut lines = vec![format!("{name} 设置：")];
    if let Some(default_max_tasks) = settings.get("default_max_tasks") {
        lines.push(format!("  默认并发上限: {default_max_tasks}"));
    }
    if let Some(interval) = settings.get("interval_secs") {
        lines.push(format!("  推进间隔秒数: {interval}"));
    }
    if let Some(windows) = settings.get("windows").and_then(Value::as_array) {
        lines.push("  时段窗口（只读；编辑请用桌面端）：".to_string());
        for window in windows {
            let days = window.get("days").and_then(Value::as_str).unwrap_or("");
            let start = window.get("start").and_then(Value::as_str).unwrap_or("");
            let end = window.get("end").and_then(Value::as_str).unwrap_or("");
            let max = window
                .get("max_tasks")
                .map(|v| v.to_string())
                .unwrap_or_default();
            lines.push(format!("    {days} {start}-{end} → 并发 {max}"));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_plugins_prints_a_neutral_line() {
        let dir = tempfile::tempdir().unwrap();
        let lines = handle_plugins(dir.path(), "");
        assert!(lines.iter().any(|line| line.contains("未安装任何插件")), "{lines:?}");
    }

    #[test]
    fn an_installed_plugin_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let sup = dir.path().join(".yi-agent/supervisors");
        std::fs::create_dir_all(&sup).unwrap();
        std::fs::write(
            sup.join("superpowers-kanban.json"),
            r#"{"name":"superpowers-kanban","command":"x","switch_key":"k","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
        )
        .unwrap();
        let lines = handle_plugins(dir.path(), "");
        assert!(lines.iter().any(|line| line.contains("superpowers-kanban")), "{lines:?}");
    }

    #[test]
    fn a_bad_subcommand_prints_usage() {
        let dir = tempfile::tempdir().unwrap();
        let lines = handle_plugins(dir.path(), "a b c d");
        assert!(lines.iter().any(|line| line.contains("usage: /plugins")), "{lines:?}");
    }
}
```

- [ ] **Step 5: Register the module and dispatch the command**

In `yi-agent-rs/crates/yi-agent/src/tui/mod.rs`, add `pub mod plugins;` next to `pub mod superpowers_kanban;` (match the existing style).

In `app.rs`, add an arm in `execute_slash_command` next to `SlashCommand::Kanban`:

```rust
        SlashCommand::Plugins => {
            let lines = crate::tui::plugins::handle_plugins(workdir, args.as_deref().unwrap_or(""));
            for line in lines {
                history.push(HistoryCell::Separator { label: Some(line) }, width);
            }
            KeyOutcome::None
        }
```

If `yi-agent` lacks `yi-agent-supervisors`, add `yi-agent-supervisors = { path = "../yi-agent-supervisors" }` to `yi-agent-rs/crates/yi-agent/Cargo.toml`. (Verified: `yi-agent/Cargo.toml:25` already has this, so no change is needed.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cd yi-agent-rs && cargo test -p yi-agent plugins`
Expected: PASS.

- [ ] **Step 7: Format and commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent/src/tui/plugins.rs yi-agent-rs/crates/yi-agent/src/tui/slash.rs yi-agent-rs/crates/yi-agent/src/tui/app.rs yi-agent-rs/crates/yi-agent/src/tui/mod.rs yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/Cargo.lock
git commit -m "feat(tui): /plugins list and scalar settings"
```

---

### Task 9: Documentation sync

**Files:**
- Modify: `plugins/superpowers-kanban/INSTALL.md`
- Modify: `plugins/superpowers-kanban/README.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/yi-agent-app-server.md`

**Interfaces:** none (docs only).

- [ ] **Step 1: Fix the stale interval claim in INSTALL.md**

Replace the line `下一次 tick（默认 60 秒）后变成排队中的卡。` with:

```
下一次 tick（默认 10 秒；可在桌面「设置 → 插件 → superpowers-kanban」里改 `推进间隔秒数`）后变成排队中的卡。
```

- [ ] **Step 2: Document the settings location in README.md**

Under the `## 配置` section, add:

```
并发与时间设置也可在桌面「设置 → 插件 → superpowers-kanban」里改：默认并发上限、
时段窗口（星期/起止/上限）、推进间隔秒数。保存即写入同一个
`superpowers-kanban.toml`；推进间隔下一个 tick 生效，无需重启插件。
```

- [ ] **Step 3: Register the settings UI and RPCs in project-management docs**

In `docs/project-management/desktop.md`, add a feature line (verify the count in the file's header/table and bump it):

```
- [x] 设置「插件」Tab：枚举已装插件 + 每个插件自有设置面板（名字注册表）；superpowers-kanban 提供并发窗口与推进间隔编辑 — `desktop/src/components/SettingsDialog.tsx`、`desktop/src/components/SettingsPluginsTab.tsx`、`desktop/src/components/SuperpowersKanbanPluginSettings.tsx`、`desktop/src/lib/pluginSettings.ts`；验证 `cd desktop && npx vitest run src/components/SettingsPluginsTab.test.tsx`
```

In `docs/project-management/yi-agent-app-server.md`, add under Features (and bump the count):

```
- [x] 通用插件设置通道：`plugins/list`（枚举 `<workdir>/.yi-agent/supervisors/*.json`）、`plugin/settings/read|write`（转发到插件 query socket，宿主不解释语义）— `src/server.rs`（`list_installed_plugins` / `plugin_query_self`）、`src/protocol.rs`（`plugin_not_installed`）；验证 `cd yi-agent-rs && cargo test -p yi-agent-app-server plugins_tests`
```

Update the "完成 / 总计" counts in `README.md`'s module index if those files' counts changed.

- [ ] **Step 4: Commit**

```bash
git add plugins/superpowers-kanban/INSTALL.md plugins/superpowers-kanban/README.md docs/project-management/desktop.md docs/project-management/yi-agent-app-server.md README.md
git commit -m "docs: plugin settings tab and host channel"
```

---

## Self-Review

**1. Spec coverage**
- §4 host generic channel → Tasks 4, 5.
- §5 RPCs + error codes → Tasks 4, 5.
- §6 plugin settings + interval migration → Tasks 2, 3 (and Task 1 for serialization/validation).
- §6.3 calendar serialization → Task 1.
- §7 desktop tab/registry/panel → Tasks 6, 7.
- §8 TUI → Task 8.
- §9 error handling/gating → Tasks 5 (codes), 7 (empty state, disabled on not_running), 2 (zero-write on invalid).
- §10 testing → test steps in every task.
- §11 migration/docs → Task 9, plus Task 3 Step 4 (manifest) and Task 5 Step 1 (code).

**2. Placeholder scan** — no TBD/TODO; every code step carries real code.

**3. Type consistency**
- `PluginSummary` fields (`name`/`queryable`/`switch_key`) consistent between Tasks 4 and 6.
- Settings payload keys (`default_max_tasks`/`interval_secs`/`windows[].days|start|end|all_day|max_tasks`) consistent across Tasks 1, 2, 6, 7, 8.
- `pluginErrorKind` codes (`plugin_not_installed`/`plugin_unavailable`) match Task 5's codes.
- `effective_interval(cli, state_dir)` signature consistent between definition and call sites in Task 3.

**Known execution risks (verify during the task, adjust if wrong):**
- `Args.interval` becoming `Option<Duration>` may require updating existing arg-parsing tests in `main.rs`; fix them to expect `None`/`Some`.
- `SettingsDialog`'s `pluginCall` generic-vs-plain typing may need `import type { PluginRpc }` (noted in Task 7 Step 5).
- The app-server main `match` uses `theme_handle.workdir()` which returns `&Path`; confirm the borrow works where `.to_path_buf()` is used in Task 5.

**Verified environment facts (no action needed):**
- `BoardQueryError.code` is `&'static str` (`server.rs:950`), so `error.code == "plugin_not_installed"` works and `RpcError::board_query(error.code, error.message)` compiles.
- `tempfile` is already a dependency of `yi-agent-app-server` (`Cargo.toml:39`) and of `superpowers-kanban-core` / `superpowers-kanban-runner` (workspace `tempfile`).
- `yi-agent` already depends on `yi-agent-supervisors` (`yi-agent/Cargo.toml:25`).
