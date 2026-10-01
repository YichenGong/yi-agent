# Superpowers Kanban 插件命令面 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给插件二进制 `superpowers-kanban` 加上 `add|list|on|off` 命令，并交付 `superpowers-kanban` skill 与 INSTALL.md，使"在对话框里说一句就加到看板"可行。

**Architecture:** 插件内核（`superpowers-kanban-core`）目前只有纯逻辑（卡片、队列、日历、开关解析、提升校验），没有任何 I/O 写入。本计划先把"写入 inbox""写开关""卡片 id 规则"三件事补进内核（纯函数 + 原子写，可独立测试），再在二进制里接成四个子命令，最后写 skill 与安装文档。

**Tech Stack:** Rust（插件独立 workspace，零 `yi-agent-*` 依赖）、Markdown（skill / INSTALL）。

## Global Constraints

- 插件工作区根：`plugins/superpowers-kanban/`；本计划**只改插件**，不碰主程序。
- Rust 工具链：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`；cargo 一律 `--offline`。
- 插件**零** `yi-agent-*` 依赖（这是插件的独立性契约，不得破坏）。
- 状态目录布局（Spec 1 已定）：写入 `<state_dir>`；`board.json` 与 `inbox/` 在 `<state_dir>` 下。`state_dir` 由插件自身参数决定，不由主程序告知。
- 开关键：`superpowers_kanban`（旧键 `superpowers_board` 仍读）。
- 写入一律 **temp + rename 原子替换**，同 id 幂等。
- skill 职责**只入队**，不生成 spec/plan。
- 不写安装脚本（用户选定手工安装 + INSTALL.md）。

---

### Task 1: 卡片 id 规则下沉到插件内核

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/card_id.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/lib.rs`

**Interfaces:**
- Produces: `pub fn card_id_for(spec: &str, plan: &str) -> String`——与 TUI/app-server 现有规则**逐字一致**（取两文件 stem，各自 slug 化，`"{spec}-{plan}"`，空则 `"card"`）。
- Consumes: 无。

- [ ] **Step 1: 写失败测试**

```rust
// card_id.rs 内联测试
#[cfg(test)]
mod tests {
    use super::card_id_for;

    #[test]
    fn derives_the_id_from_both_file_stems() {
        assert_eq!(
            card_id_for("docs/a.spec.md", "docs/a.plan.md"),
            "a-spec-a-plan"
        );
    }

    #[test]
    fn slugs_punctuation_and_case() {
        assert_eq!(
            card_id_for("My Spec.md", "My Plan.md"),
            "my-spec-my-plan"
        );
    }

    #[test]
    fn falls_back_to_card_when_nothing_usable_remains() {
        assert_eq!(card_id_for("", ""), "card");
    }
}
```

- [ ] **Step 2: 运行确认失败**

Run: `cd plugins/superpowers-kanban && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline card_id`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**（与 TUI `card_id_for` 同规则）

```rust
/// 由一对路径派生卡片 id：两个文件 stem 各自 slug 化后拼接。
pub fn card_id_for(spec: &str, plan: &str) -> String {
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

`lib.rs` 加 `pub mod card_id;`。

- [ ] **Step 4: 运行确认通过**

Run: `cd plugins/superpowers-kanban && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline card_id`
Expected: PASS

- [ ] **Step 5: 一致性测试（与主程序逐一比对）**

新增一个测试，用与 `yi-agent-app-server` 的 `derive_card_id_matches_the_tui_rule` 相同的样例，确认插件规则产出相同 id（样例值抄自该测试，若该测试不在本机可读，用上一步的样例）。

- [ ] **Step 6: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core
git commit -m "feat(superpowers-kanban): derive card ids in the plugin core"
```

---

### Task 2: 入队写入（`inbox`）下沉到插件内核

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/inbox.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/lib.rs`

**Interfaces:**
- Produces: `pub fn deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()>`——写 `<state_dir>/inbox/<id>.json`，temp + rename，幂等。
- Consumes: Task 1 无依赖（本任务不派生 id，id 由调用方给）。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn delivers_one_json_file_per_card() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    let path = dir.path().join("inbox/card-1.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["id"], "card-1");
    assert_eq!(value["spec_path"], "a.spec.md");
    assert_eq!(value["plan_path"], "a.plan.md");
}

#[test]
fn delivering_is_idempotent_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    deliver_card(dir.path(), "card-1", "b.spec.md", "b.plan.md").unwrap();
    let names: Vec<_> = std::fs::read_dir(dir.path().join("inbox"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["card-1.json".to_string()]);
    assert!(!dir.path().join("inbox/card-1.json.tmp").exists());
}
```

- [ ] **Step 2: 运行确认失败** → FAIL

- [ ] **Step 3: 实现**（复制自 `yi-agent-board-ui::inbox`，去掉宿主注释）

```rust
use std::path::{Path, PathBuf};

pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

pub fn enqueue_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.json"))
}

/// 写一个投递文件。同 id 覆盖；temp + rename 原子替换。
pub fn deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({ "id": id, "spec_path": spec, "plan_path": plan });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}
```

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git add plugins/superpowers-kanban/crates/superpowers-kanban-core
git commit -m "feat(superpowers-kanban): deliver cards into the inbox from the plugin core"
```

---

### Task 3: 开关写入下沉到插件内核

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-core/src/switch.rs`

**Interfaces:**
- Produces: `pub fn write_layer(path: &Path, value: SwitchValue) -> std::io::Result<()>`——读-改-写整个 JSON 对象（保留其他键），只写 `superpowers_kanban`，temp + rename。
- Produces: `pub fn project_preferences_path(state_dir: &Path) -> PathBuf` / `pub fn global_preferences_path() -> Option<PathBuf>`。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn writing_sets_the_new_key_and_preserves_others() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.json");
    std::fs::write(&path, r#"{"subagent_runtime":"always"}"#).unwrap();
    write_layer(&path, SwitchValue::Enabled).unwrap();
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["superpowers_kanban"], true);
    assert_eq!(v["subagent_runtime"], "always");
}

#[test]
fn a_written_layer_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("preferences.json");
    write_layer(&path, SwitchValue::Disabled).unwrap();
    assert_eq!(parse_switch_json(&std::fs::read_to_string(&path).unwrap()), Some(SwitchValue::Disabled));
}
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

```rust
use std::path::{Path, PathBuf};

/// 项目层偏好路径：`<workdir>/.yi-agent/preferences.json`。
/// `state_dir` 是 `<workdir>/.yi-agent/superpowers-kanban`，故取其父目录。
pub fn project_preferences_path(state_dir: &Path) -> PathBuf {
    state_dir
        .parent()
        .unwrap_or(state_dir)
        .join("preferences.json")
}

pub fn global_preferences_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".yi-agent").join("preferences.json"))
}

/// 读一层：缺失/损坏/缺键一律 `None`（视作未设置）。新键优先、旧键回退。
pub fn read_layer(path: &Path) -> Option<SwitchValue> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_switch_json(&text)
}

/// 写一层：读-改-写整个 JSON 对象（保留其他键），只写新键，temp + rename。
pub fn write_layer(path: &Path, value: SwitchValue) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    object.insert(
        "superpowers_kanban".to_string(),
        serde_json::Value::Bool(matches!(value, SwitchValue::Enabled)),
    );
    let body = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}
```
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(superpowers-kanban): write the switch layer from the plugin core"
```

---

### Task 4: CLI 子命令 `add` / `list` / `on` / `off`

**Files:**
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
- Test: 同文件

**Interfaces:**
- Produces: `superpowers-kanban add <spec> <plan>`、`list`、`on`、`off`；`run` 不变。
- Consumes: Task 1 `card_id_for`、Task 2 `deliver_card`、Task 3 `write_layer`。

- [ ] **Step 1: 写失败测试（参数分发）**

```rust
#[test]
fn add_requires_two_paths() {
    assert!(parse_subcommand(["add", "a.spec.md"]).is_err());
    assert!(parse_subcommand(["add", "a.spec.md", "a.plan.md"]).is_ok());
}

#[test]
fn on_and_off_take_no_arguments() {
    assert!(parse_subcommand(["on"]).is_ok());
    assert!(parse_subcommand(["off"]).is_ok());
    assert!(parse_subcommand(["on", "extra"]).is_err());
}
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**

```rust
enum Subcommand {
    Run(Args),
    Add { spec: String, plan: String },
    List,
    On,
    Off,
}
```

`add` 用 `card_id_for(spec, plan)` 派生 id，`deliver_card(&state_dir, &id, spec, plan)` 写入，打印 `delivered <id> to inbox`。
`list` 读 `<state_dir>/board.json`（`superpowers_kanban_runner::persist::load_board`）并打印每卡 `id / state / workdir`。
`on`/`off` 写**项目层**偏好，路径由 `state_dir.parent()/preferences.json` 推导
（`state_dir` = `<workdir>/.yi-agent/superpowers-kanban`，故父目录即 `<workdir>/.yi-agent`）。
必须加一个测试固定这条推导：

```rust
#[test]
fn project_preferences_live_beside_the_state_directory() {
    assert_eq!(
        superpowers_kanban_core::switch::project_preferences_path(
            std::path::Path::new("/proj/.yi-agent/superpowers-kanban")
        ),
        std::path::PathBuf::from("/proj/.yi-agent/preferences.json")
    );
}
```

- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

```bash
git commit -m "feat(superpowers-kanban): add the add/list/on/off commands"
```

---

### Task 5: `superpowers-kanban` skill

**Files:**
- Create: `plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`

**Interfaces:**
- Produces: 一份指导模型"识别意图 → 确认两文件存在 → 调 CLI 入队"的 skill 文档。

- [ ] **Step 1: 写 SKILL.md**（含 name/description 前置元数据，正文含：何时触发、前置校验、确切命令、失败处理、绝不生成 spec/plan）
- [ ] **Step 2: 校验命令与真实 CLI 一致**

Run: `grep -n "superpowers-kanban add" plugins/superpowers-kanban/skills/superpowers-kanban/SKILL.md`
Expected: 与 Task 4 的用法逐字一致

- [ ] **Step 3: Commit**

```bash
git commit -m "docs(superpowers-kanban): add the enqueue skill"
```

---

### Task 6: INSTALL.md

**Files:**
- Create: `plugins/superpowers-kanban/INSTALL.md`

- [ ] **Step 1: 写安装步骤**：构建 → 放 PATH → 复制 skill 到 `~/.yi-agent/skills/superpowers-kanban/` → 复制清单 → 开开关 → **验证**（`list` 有输出、daemon 在跑、清单被扫到）→ **卸载**。
- [ ] **Step 2: 逐条验证命令可执行**（在临时目录跑构建与 `list`）。
- [ ] **Step 3: Commit**

```bash
git commit -m "docs(superpowers-kanban): write the agent-followable install guide"
```

---

### Task 7: 端到端回归

- [ ] **Step 1:** `cd plugins/superpowers-kanban && TMPDIR=/Users/gongyichen/.yi-agent-tmp cargo test --offline --no-fail-fast` → 全 ok
- [ ] **Step 2:** 手工冒烟：临时 state_dir 跑 `add` → `inbox/<id>.json` 出现 → 跑一个 tick 后 `list` 显示该卡（或由 `list` 直接读 board.json 验证投递已消费）
- [ ] **Step 3:** `git commit`（若冒烟暴露问题则先修）
