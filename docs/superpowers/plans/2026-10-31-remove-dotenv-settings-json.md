# 去掉 `.env`：配置迁到 `settings.json` 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `.env` 不再是任何生产代码的配置来源，把 agent 可调参数迁到 `<instance_home>/settings.json`，并让 CLI、app-server、桌面端、web 都从同一份类型化元数据读写。

**Architecture:** 新增 `yi-agent-runtime/src/settings.rs` 作为唯一的存储层与元数据层（类型化 `Option<T>` 字段 + 描述/默认值/可选项）。`config.rs` 的多层合并从「覆盖 > 真实 env > `.env` > 默认」改为「覆盖 > 真实 env > `settings.json` > 默认」。新增 `migrate_env_settings` 与既有 `migrate_model_keys` 挂在同一个生产启动函数里，`.env` 迁完后改名 `.env.migrated`。app-server 暴露 `settings/read|write` 两个 RPC，web 与桌面端都只是它的外壳。

**Tech Stack:** Rust（axum / serde / serde_json / anyhow / tracing）、TypeScript + React（桌面端 vitest）、shell（justfile recipe）。

## Global Constraints

以下约束适用于**每一个** task，不再逐条重复：

- **默认值只有 Rust 一处真相源。** 任何新增的默认值必须引用 `config.rs` 已有常量或新加的常量，不得在测试、web、桌面端各写一份字面量。既有漂移的教训见 spec 第 2 节（`MAX_TURNS` 20/20/200、`MAX_DIRECT_CHILDREN` 4/缺失/8）。
- **真实进程环境变量这一层必须保留。** `YI_AGENT_HOME`、`YI_AGENT_BUILD_KIND`、`YI_AGENT_DISABLE_WATCHMAN`、`YI_AGENT_RUNTIME_DIR`、`YI_AGENT_SKILLS_DIR`、`YI_AGENT_TEST_GUARD`、`YI_AGENT_REAL_LLM_*` 仍从进程环境读取；本计划只删除它们作为「配置来源」的地位，不删除读取它们的代码。
- **`settings.json` 里不含 `workdir`，也不含 `yolo`。** 工作目录只由 `--workdir` / 启动目录 / 桌面端选项目决定；`yolo` 必须每次启动显式给出。
- **写入必须读-改-写，保留文件里的未知顶层键。** 参照 `models.rs:650` 的 `save_catalog_to`：临时文件 + `rename` + `0600` + 同目录锁。绝不使用 `env_file.rs:80` 那种裸写。
- **空字符串等于未设置。** 加载侧与写入侧都要实现：`""` 与 `"   "` 按 `None` 处理；写入侧收到 `""` 必须删键而不是写空串或 `0`。
- **坏文件绝不阻断启动，但坏字段只降级自己。** 两级容错，不要混为一谈：
  - 整份读不出来（缺失 / 不可读 / JSON 非法 / 顶层不是对象）→ 全 `None` + 默认值，`tracing::warn!` 后继续。
  - 单个字段坏掉（类型不符 / 数值越界 / 枚举不在白名单）→ **只把那个字段置 `None`**，其余字段照常生效。
  第二级必须显式实现：`#[serde(default)]` 只补**缺失**字段，不救**坏值**——没有 `deserialize_with` 时，用户手改出 `"max_turns": "abc"` 会让整份文件回默认，连带正确的 `compact.ratio` 一起丢掉。
- **提交前必须 `cargo fmt --all`**（在 `yi-agent-rs/` 下），commit message 用 conventional commits，首行 ≤ 72 字符，不写 `Co-Authored-By`。
- **在 `main` 之外的分支上做**：`.worktrees/feat-settings-json`，分支 `feat/settings-json`。
- **`cargo` 不在默认 PATH**：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`。
- **同一时刻只跑一个 `cargo test`**；跑前 `ps aux | grep -c "[c]argo"` 确认没有残留进程。

## 文件结构

新建：

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs` | `Settings` 类型（全部 `Option<T>`）、`FIELDS` 元数据表、`load_from` / `load`、`write_to` / `write`、`FieldSpec`、值校验、`.env` 行解析纯函数 |
| `yi-agent-rs/crates/yi-agent-runtime/src/settings/migrate.rs`（或并入 `migrate.rs`，见 Task 6） | `migrate_env_settings` 与 `EnvMigrationOutcome` |
| `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs` | `settings/read` / `settings/write` 的 handler |
| `yi-agent-rs/scripts/export-model-env.sh` | 从 `models.json` 导出 real-test 所需环境变量 |
| `desktop/src/lib/runtimeSettings.ts` | 桌面端 settings 通道的形状收敛（仿 `lib/websearch.ts`） |
| `desktop/src/components/SettingsRuntimeTab.tsx` | 「运行参数」Tab |

修改（按 task 分组，详见各 task 的 Files 段）：

- `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`（加载链、`resolve_workdir`、`load_env_files` 下线）
- `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`（`pub mod settings;`）
- `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs`（新增 A 类迁移）
- `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`（删 Env 兜底层）
- `yi-agent-rs/crates/yi-agent-app-server/src/{server.rs,ws.rs,rpc/mod.rs,rpc/onboarding.rs,rpc/thread.rs,onboarding_rpc.rs,rpc/ui.rs,protocol.rs}`
- `yi-agent-rs/crates/yi-agent-tools/src/web/config.rs`（删 `BOCHA_API_KEY` 回退）
- `yi-agent-rs/crates/yi-agent-web/src/{lib.rs,api.rs,config_meta.rs,env_file.rs,assets/index.html}`
- `yi-agent-rs/crates/yi-agent/src/{main.rs,onboarding_prompt.rs}`
- `yi-agent-rs/justfile`、`yi-agent-rs/scripts/dev-env.sh`
- `desktop/src/components/SettingsDialog.tsx`、`desktop/src/App.tsx`
- 文档：`README.md`、`docs/project-management/yi-agent-runtime.md`、`docs/project-management/yi-agent-web.md`、`docs/bug-list.md`、`yi-agent-rs/justfile` 里 recipe 注释

---

## Task 1: `.env` 行解析纯函数

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（`#[cfg(test)] mod tests`）

**Interfaces:**
- Produces: `pub fn parse_env_line(line: &str) -> Option<(String, String)>` —— 剥 `export ` 前缀、截断未转义的行内 `#`、剥配对引号；不可解析返回 `None`
- Produces: `pub fn parse_env_text(text: &str) -> std::collections::BTreeMap<String, String>`

**背景：** `migrate.rs:23` 现有的 `active_model_keys` 用朴素行拆分，与 dotenvy 口径不一致（`bug-list.md:171` 未修）。实测：`MODEL_API_KEY=sk-x #note` 会把注释并进值；`export MODEL_API_KEY=sk-x` 完全不被识别。迁移要读同一份 `.env`，所以必须有一个口径正确的解析器。

- [ ] **Step 1: 写失败的测试**

在 `settings.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_key_value() {
        assert_eq!(
            parse_env_line("YI_AGENT_MAX_TURNS=500"),
            Some(("YI_AGENT_MAX_TURNS".to_string(), "500".to_string()))
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        assert_eq!(
            parse_env_line("  KEY = value  "),
            Some(("KEY".to_string(), "value".to_string()))
        );
    }

    /// dotenvy 会把未加引号的 ` #` 之后当注释截掉。旧实现把它并进了值。
    #[test]
    fn unquoted_trailing_comment_is_stripped() {
        assert_eq!(
            parse_env_line("MODEL_API_KEY=sk-x #note"),
            Some(("MODEL_API_KEY".to_string(), "sk-x".to_string()))
        );
    }

    /// `#` 紧贴值、中间没有空格时不是注释——这是 dotenvy 的口径。
    #[test]
    fn hash_without_preceding_space_is_part_of_the_value() {
        assert_eq!(
            parse_env_line("KEY=ab#cd"),
            Some(("KEY".to_string(), "ab#cd".to_string()))
        );
    }

    /// 引号内的 `#` 是字面值，绝不能截断。
    #[test]
    fn hash_inside_quotes_is_kept() {
        assert_eq!(
            parse_env_line(r#"KEY="ab # cd""#),
            Some(("KEY".to_string(), "ab # cd".to_string()))
        );
        assert_eq!(
            parse_env_line("KEY='ab # cd'"),
            Some(("KEY".to_string(), "ab # cd".to_string()))
        );
    }

    /// dotenvy 接受 `export` 前缀；旧实现会把 `export KEY` 当成键名，等于整行丢失。
    #[test]
    fn export_prefix_is_stripped() {
        assert_eq!(
            parse_env_line("export MODEL_API_KEY=sk-x"),
            Some(("MODEL_API_KEY".to_string(), "sk-x".to_string()))
        );
    }

    #[test]
    fn paired_quotes_are_stripped() {
        assert_eq!(
            parse_env_line(r#"A="quoted""#),
            Some(("A".to_string(), "quoted".to_string()))
        );
        assert_eq!(
            parse_env_line("B='single'"),
            Some(("B".to_string(), "single".to_string()))
        );
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        assert_eq!(parse_env_line("# a comment"), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("NO_EQUALS_SIGN"), None);
        assert_eq!(parse_env_line("=no-key"), None);
    }

    #[test]
    fn parse_text_collects_all_pairs_and_last_wins() {
        let text = "\
# header
export YI_AGENT_MAX_TURNS=500
BOCHA_API_KEY='abc # def'
YI_AGENT_MAX_TURNS=600

# trailing comment
";
        let map = parse_env_text(text);
        assert_eq!(map.get("YI_AGENT_MAX_TURNS").unwrap(), "600");
        assert_eq!(map.get("BOCHA_API_KEY").unwrap(), "abc # def");
        assert_eq!(map.len(), 2);
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -20
```

Expected: 编译失败，`cannot find function parse_env_line`。

- [ ] **Step 3: 实现解析器**

在 `settings.rs` 顶部（`#[cfg(test)]` 之前）写：

```rust
//! `settings.json`：agent 可调参数的唯一持久化位置。
//!
//! 取代过去的 `~/.yi-agent/.env`。本模块同时承担三件事：类型化存储、
//! 字段元数据（供 web 与桌面端渲染表单）、以及一次性迁移所需的 `.env` 行解析。

use std::collections::BTreeMap;

/// 解析一行 `.env`，口径与 `dotenvy` 对齐。
///
/// 对齐的四条：
/// - 剥 `export ` 前缀（`dotenvy` 接受它，旧实现当成键名的一部分）；
/// - 未加引号的 ` #`（空格 + `#`）之后是注释（旧实现并进值）；
/// - 引号内的 `#` 是字面值；
/// - 值两端的配对引号被剥掉。
///
/// 返回 `None` 表示这一行不产生键值对（注释、空行、无 `=`、键为空）。
pub fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() {
        return None;
    }
    Some((key.to_string(), parse_env_value(line[eq + 1..].trim())))
}

/// 解析 `=` 右侧。先按「是否被配对引号包裹」分流：引号内的一切都是字面值，
/// 否则在第一个前面有空格的 `#` 处截断。
///
/// **不支持转义序列。** `"a\\nb"` 会被读成字面的反斜杠加 `n`，而不是换行；
/// `"a\\"b"` 也不会被识别成转义引号。dotenvy 会展开这些。迁移场景里带转义的
/// 值几乎不存在（`.env` 里的都是数字、URL、API key），所以宁可保持实现简单，
/// 也不要手写一个半对的转义展开器。若将来真的遇到，正确做法是用 dotenvy 的
/// 解析器，而不是在这里加 `replace` 分支。
fn parse_env_value(raw: &str) -> String {
    let quoted = |s: &str, q: char| s.len() >= 2 && s.starts_with(q) && s.ends_with(q);
    if quoted(raw, '"') || quoted(raw, '\'') {
        return raw[1..raw.len() - 1].to_string();
    }
    // 找「前面是空白」的 `#`。没有则整串是值。
    let bytes = raw.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'#' || index == 0 {
            continue;
        }
        if bytes[index - 1].is_ascii_whitespace() {
            return raw[..index].trim_end().to_string();
        }
    }
    raw.trim_end().to_string()
}

/// 解析整份 `.env` 文本；同一键出现多次时后者覆盖前者（与 `dotenvy` 一致）。
pub fn parse_env_text(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = parse_env_line(line) {
            out.insert(key, value);
        }
    }
    out
}
```

在 `lib.rs` 的 `pub mod` 列表里按字母序插入：

```rust
pub mod settings;
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -20
```

Expected: 9 passed。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs yi-agent-rs/crates/yi-agent-runtime/src/lib.rs
git commit -m "feat(runtime): add a dotenvy-compatible .env line parser

迁移要读同一份 .env，而既有 migrate.rs 的朴素行拆分与 dotenvy 口径不一致：
注释会被并进值、export 前缀整行丢失（bug-list.md:171）。新解析器在迁移前
先把口径对齐，避免把脏值写进 settings.json。"
```

---

## Task 2: `Settings` 类型与元数据表

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`

**Interfaces:**
- Consumes: 无（Task 1 的解析函数本任务不用）
- Produces:
  - `pub struct Settings { pub agent: AgentSettings, pub compact: CompactSettings, pub startup: StartupSettings }`
  - `pub struct AgentSettings { pub max_turns: Option<u32>, pub max_direct_children: Option<u16>, pub max_resident_subagents: Option<u16>, pub system_prompt: Option<String>, pub skills_catalog_budget: Option<usize> }`
  - `pub struct CompactSettings { pub ratio: Option<u32>, pub user_budget_tokens: Option<usize>, pub tool_budget_tokens: Option<usize> }`
  - `pub struct StartupSettings { pub sandbox: Option<String> }`
  - `pub enum FieldType { Number, Text, Select }`
  - `pub struct FieldSpec { pub key: &'static str, pub group: &'static str, pub label: &'static str, pub field_type: FieldType, pub default_display: &'static str, pub description: &'static str, pub options: &'static [&'static str], pub min: Option<u64>, pub max: Option<u64> }`
  - `pub static FIELDS: &[FieldSpec]`
  - `pub fn find_spec(key: &str) -> Option<&'static FieldSpec>`

**设计要点：** 字段用「点路径」键（`agent.max_turns`）既做 RPC 的 key，也做写入时的定位。`FIELDS` 是 web 与桌面端唯一的元数据来源——`config_meta.rs` 的 `ALL_VARS` 会在 Task 12 删除。

- [ ] **Step 1: 写失败的测试**

追加到 `settings.rs` 的 `tests` 模块：

```rust
    #[test]
    fn field_keys_are_unique() {
        let mut keys: Vec<&str> = FIELDS.iter().map(|f| f.key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate field keys in FIELDS");
    }

    #[test]
    fn every_select_field_has_options_and_every_other_field_has_none() {
        for field in FIELDS {
            match field.field_type {
                FieldType::Select => {
                    assert!(!field.options.is_empty(), "{} is Select but has no options", field.key);
                }
                _ => {
                    assert!(field.options.is_empty(), "{} is not Select but has options", field.key);
                }
            }
        }
    }

    /// 默认值文案必须与 `config.rs` 的常量一致。这条测试是「默认值只有一处」的
    /// 回归网：改了常量却忘了改文案，这里会红。
    #[test]
    fn default_display_matches_the_runtime_constants() {
        use crate::config::{DIRECT_CHILDREN_DEFAULT, RESIDENT_SUBAGENTS_DEFAULT};
        assert_eq!(
            find_spec("agent.max_resident_subagents").unwrap().default_display,
            RESIDENT_SUBAGENTS_DEFAULT.to_string()
        );
        assert_eq!(
            find_spec("agent.max_direct_children").unwrap().default_display,
            DIRECT_CHILDREN_DEFAULT.to_string()
        );
        // 这三个在 config.rs 里是行内字面量，Task 3 会把它们提成常量后再回来改这里。
        assert_eq!(find_spec("agent.max_turns").unwrap().default_display, "200");
        assert_eq!(find_spec("compact.ratio").unwrap().default_display, "80");
        assert_eq!(find_spec("compact.user_budget_tokens").unwrap().default_display, "20000");
        assert_eq!(find_spec("compact.tool_budget_tokens").unwrap().default_display, "12000");
        assert_eq!(find_spec("agent.skills_catalog_budget").unwrap().default_display, "8192");
        assert_eq!(find_spec("startup.sandbox").unwrap().default_display, "workspace-write");
    }

    /// `workdir` 与 `yolo` 刻意不在表里。workdir 只由启动方式决定，yolo 必须每次
    /// 启动显式给出——持久化它会让 `sandbox_promotable` 恒为 false。
    #[test]
    fn workdir_and_yolo_are_not_persistable() {
        for forbidden in ["workdir", "agent.workdir", "yolo", "startup.yolo"] {
            assert!(find_spec(forbidden).is_none(), "{forbidden} must not be persistable");
        }
    }

    #[test]
    fn sandbox_options_match_the_sandbox_mode_variants() {
        let spec = find_spec("startup.sandbox").unwrap();
        assert_eq!(
            spec.options,
            &["read-only", "workspace-write", "danger-full-access"]
        );
        // 与 SandboxMode::as_str 同源，不是另抄一份。
        for mode in [
            yi_agent_tools::SandboxMode::ReadOnly,
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            yi_agent_tools::SandboxMode::DangerFullAccess,
        ] {
            assert!(spec.options.contains(&mode.as_str()));
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -20
```

Expected: 编译失败，`cannot find type Settings` / `FIELDS`。

- [ ] **Step 3: 实现类型与元数据**

追加到 `settings.rs`（在 `mod tests` 之前）：

```rust
use serde::{Deserialize, Serialize};

/// `settings.json` 的顶层结构。
///
/// 每个字段都是 `Option<T>`：`None` 表示「未设置，用默认值」。这让「恢复默认」
/// 等于删掉该键，而不是把当前默认值写回去——将来默认值改了，用户跟着变。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub agent: AgentSettings,
    pub compact: CompactSettings,
    pub startup: StartupSettings,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    #[serde(default, deserialize_with = "de_optional_u32")]
    pub max_turns: Option<u32>,
    #[serde(default, deserialize_with = "de_optional_u16")]
    pub max_direct_children: Option<u16>,
    #[serde(default, deserialize_with = "de_optional_u16")]
    pub max_resident_subagents: Option<u16>,
    #[serde(default, deserialize_with = "de_optional_string")]
    pub system_prompt: Option<String>,
    #[serde(default, deserialize_with = "de_optional_usize")]
    pub skills_catalog_budget: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactSettings {
    #[serde(default, deserialize_with = "de_optional_u32")]
    pub ratio: Option<u32>,
    #[serde(default, deserialize_with = "de_optional_usize")]
    pub user_budget_tokens: Option<usize>,
    #[serde(default, deserialize_with = "de_optional_usize")]
    pub tool_budget_tokens: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupSettings {
    /// `read-only` / `workspace-write` / `danger-full-access`；`None` = 未设置。
    /// 存字符串而非枚举：非法值要能被加载侧识别并 warn 回退，而不是让整份
    /// JSON 反序列化失败（那会把用户其它字段一起丢掉）。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub sandbox: Option<String>,
}

/// 表单控件类型。`Number` 与 `Text` 在 JSON 里都是标量，区别只在渲染与校验。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    Number,
    Text,
    Select,
}

/// 一个可持久化字段的元数据。web 与桌面端都渲染这张表，故描述与默认值文案
/// 只在这里写一次。
#[derive(Debug, Clone, Copy)]
pub struct FieldSpec {
    /// 点路径键，同时是 RPC 的 `key` 与写入定位。
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub field_type: FieldType,
    /// 默认值的展示文案。必须是 `config.rs` 常量的字符串形式（Task 3 会加
    /// 常量并在测试里比对）。
    pub default_display: &'static str,
    pub description: &'static str,
    pub options: &'static [&'static str],
    pub min: Option<u64>,
    pub max: Option<u64>,
}

/// 可持久化的全部字段。`workdir` 与 `yolo` 不在其中，见 spec「存储」一节。
pub static FIELDS: &[FieldSpec] = &[
    FieldSpec {
        key: "agent.max_turns",
        group: "agent",
        label: "最大轮次",
        field_type: FieldType::Number,
        default_display: "200",
        description: "一次对话里 agent 最多执行的轮数",
        options: &[],
        min: Some(1),
        // 上限必须给：`Settings.agent.max_turns` 是 `Option<u32>`，写入
        // 大于 u32::MAX 的值会让下次 `load_from` 反序列化失败，整份文件
        // 静默回默认——用户看到的只是「我改的设置全没了」。
        max: Some(u32::MAX as u64),
    },
    FieldSpec {
        key: "agent.max_direct_children",
        group: "agent",
        label: "直接子 agent 上限",
        field_type: FieldType::Number,
        default_display: "8",
        description: "一个 agent 可同时拥有的未终止直接子 agent 数",
        options: &[],
        min: Some(1),
        max: Some(u16::MAX as u64),
    },
    FieldSpec {
        key: "agent.max_resident_subagents",
        group: "agent",
        label: "常驻子 agent 容量",
        field_type: FieldType::Number,
        default_display: "64",
        description: "本 daemon 可常驻的子 agent 数；根 agent 不占额度",
        options: &[],
        min: Some(1),
        max: Some(u16::MAX as u64),
    },
    FieldSpec {
        key: "agent.system_prompt",
        group: "agent",
        label: "系统提示词",
        field_type: FieldType::Text,
        default_display: "",
        description: "覆盖默认系统提示词；留空表示用默认",
        options: &[],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "agent.skills_catalog_budget",
        group: "agent",
        label: "技能目录预算（字节）",
        field_type: FieldType::Number,
        default_display: "8192",
        description: "注入系统提示词的技能目录上限；超出时按策略截断或询问",
        options: &[],
        min: Some(1),
        max: Some(u32::MAX as u64),
    },
    FieldSpec {
        key: "compact.ratio",
        group: "compact",
        label: "压缩触发占比（%）",
        field_type: FieldType::Number,
        default_display: "80",
        description: "上下文占用达到这个百分比时触发自动压缩",
        options: &[],
        min: Some(1),
        max: Some(100),
    },
    FieldSpec {
        key: "compact.user_budget_tokens",
        group: "compact",
        label: "压缩保留用户 token",
        field_type: FieldType::Number,
        default_display: "20000",
        description: "压缩时保留的真实用户输入 token 数",
        options: &[],
        min: Some(1),
        max: Some(u32::MAX as u64),
    },
    FieldSpec {
        key: "compact.tool_budget_tokens",
        group: "compact",
        label: "压缩保留工具 token",
        field_type: FieldType::Number,
        default_display: "12000",
        description: "压缩时保留的完整工具交互 token 数",
        options: &[],
        min: Some(1),
        max: Some(u32::MAX as u64),
    },
    FieldSpec {
        key: "startup.sandbox",
        group: "startup",
        label: "默认沙箱档位",
        field_type: FieldType::Select,
        default_display: "workspace-write",
        description: "启动时的文件写入沙箱；空表示用默认档位",
        options: &["read-only", "workspace-write", "danger-full-access"],
        min: None,
        max: None,
    },
];

/// `Option<T>` 的宽容反序列化：类型不符（如 `"abc"`、`-1`、`1.5`、数组）按
/// **未设置**处理，而不是让整份 `settings.json` 反序列化失败。
///
/// 为什么需要：`#[serde(default)]` 只补缺失字段，**不救坏值**。没有这几个
/// 函数时，用户手改出 `"max_turns": "abc"` 会让 `load_from` 整份回默认，
/// 连正确的 `compact.ratio` 一起丢掉——spec 的错误处理一节要求逐字段降级。
///
/// 与 `models.rs` 的 `de_optional_string` / `de_optional_capabilities` 同一策略。
fn de_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| v.as_u64()))
}

fn de_optional_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(de_optional_u64(deserializer)?.and_then(|v| u32::try_from(v).ok()))
}

fn de_optional_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(de_optional_u64(deserializer)?.and_then(|v| u16::try_from(v).ok()))
}

fn de_optional_usize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(de_optional_u64(deserializer)?.and_then(|v| usize::try_from(v).ok()))
}

fn de_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| v.as_str().map(str::to_string)))
}

/// 按点路径键查元数据。
pub fn find_spec(key: &str) -> Option<&'static FieldSpec> {
    FIELDS.iter().find(|field| field.key == key)
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -25
```

Expected: 14 passed（Task 1 的 9 条 + 本任务的 5 条）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs
git commit -m "feat(runtime): define the settings.json shape and field metadata

字段全是 Option<T>，None 即「未设置用默认」。FIELDS 是 web 与桌面端唯一
的元数据来源，取代 config_meta.rs 手抄的 ALL_VARS 表——那张表的默认值已经
和 config.rs 漂移过（MAX_TURNS 20 vs 200）。"
```

---

## Task 3: 把 `config.rs` 的行内默认值提成常量

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:296-412`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`（`#[cfg(test)] mod tests`）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`（把 Task 2 测试里的字面量换成常量）

**Interfaces:**
- Produces: `pub const MAX_TURNS_DEFAULT: u32 = 200;`
- Produces: `pub const COMPACT_RATIO_DEFAULT: u32 = 80;`
- Produces: `pub const COMPACT_USER_BUDGET_DEFAULT: usize = 20_000;`
- Produces: `pub const COMPACT_TOOL_BUDGET_DEFAULT: usize = 12_000;`
- Produces: `pub const SKILLS_CATALOG_BUDGET_DEFAULT: usize = 8_192;`
- 既有：`RESIDENT_SUBAGENTS_DEFAULT: u16 = 64`、`DIRECT_CHILDREN_DEFAULT: u16 = 8`

**背景：** spec 要求「默认值只有 Rust 一处」。目前 `config.rs:306` 的 `unwrap_or(200)` 等六个字面量散在函数体里，`settings.rs` 的 `default_display` 无法与它们建立编译期联系。先提常量，测试才能把两者钉住。

- [ ] **Step 1: 写失败的测试**

在 `config.rs` 的 `tests` 模块里追加：

```rust
    #[test]
    fn defaults_are_constants_not_scattered_literals() {
        // 提常量之前，这几行会编译失败——这正是本测试的用途：它钉住
        // 「默认值只有一个名字」，防止有人再往函数体里写裸字面量。
        assert_eq!(MAX_TURNS_DEFAULT, 200);
        assert_eq!(COMPACT_RATIO_DEFAULT, 80);
        assert_eq!(COMPACT_USER_BUDGET_DEFAULT, 20_000);
        assert_eq!(COMPACT_TOOL_BUDGET_DEFAULT, 12_000);
        assert_eq!(SKILLS_CATALOG_BUDGET_DEFAULT, 8_192);
    }

    /// `settings.json` 的默认值文案必须与这里同源。两个常量组一旦漂移，这条测试先红。
    #[test]
    fn settings_metadata_defaults_track_these_constants() {
        use crate::settings::find_spec;
        let cases: &[(&str, String)] = &[
            ("agent.max_turns", MAX_TURNS_DEFAULT.to_string()),
            (
                "agent.max_direct_children",
                DIRECT_CHILDREN_DEFAULT.to_string(),
            ),
            (
                "agent.max_resident_subagents",
                RESIDENT_SUBAGENTS_DEFAULT.to_string(),
            ),
            (
                "agent.skills_catalog_budget",
                SKILLS_CATALOG_BUDGET_DEFAULT.to_string(),
            ),
            ("compact.ratio", COMPACT_RATIO_DEFAULT.to_string()),
            (
                "compact.user_budget_tokens",
                COMPACT_USER_BUDGET_DEFAULT.to_string(),
            ),
            (
                "compact.tool_budget_tokens",
                COMPACT_TOOL_BUDGET_DEFAULT.to_string(),
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(
                find_spec(key).unwrap().default_display,
                *expected,
                "default_display for {key} drifted from the runtime constant"
            );
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config::tests::defaults_are_constants 2>&1 | tail -12
```

Expected: 编译失败，`cannot find value MAX_TURNS_DEFAULT`。

- [ ] **Step 3: 提常量并替换字面量**

在 `config.rs` 的 `DIRECT_CHILDREN_DEFAULT` 之后加：

```rust
/// 一次对话里 agent 最多执行的轮数。
pub const MAX_TURNS_DEFAULT: u32 = 200;

/// 触发自动压缩的上下文占比（百分数）。
pub const COMPACT_RATIO_DEFAULT: u32 = 80;

/// 压缩时保留的真实用户输入 token 数。
pub const COMPACT_USER_BUDGET_DEFAULT: usize = 20_000;

/// 压缩时保留的完整工具交互 token 数。
pub const COMPACT_TOOL_BUDGET_DEFAULT: usize = 12_000;

/// 注入系统提示词的技能目录字节预算。
pub const SKILLS_CATALOG_BUDGET_DEFAULT: usize = 8_192;
```

替换六处裸字面量：

```rust
// :306
            .unwrap_or(MAX_TURNS_DEFAULT);
// :337
            .unwrap_or(COMPACT_RATIO_DEFAULT);
// :358
            .unwrap_or(COMPACT_USER_BUDGET_DEFAULT);
// :363
            .unwrap_or(COMPACT_TOOL_BUDGET_DEFAULT);
// :409
            .unwrap_or(SKILLS_CATALOG_BUDGET_DEFAULT);
```

- [ ] **Step 4: 把 `settings.rs` 的测试改成引用常量**

在 `settings.rs` 的 `tests` 模块里，把 `default_display_matches_the_runtime_constants` 改成：

```rust
    /// 默认值文案必须与 `config.rs` 的常量一致。这条测试是「默认值只有一处」的
    /// 回归网：改了常量却忘了改文案，这里会红。
    #[test]
    fn default_display_matches_the_runtime_constants() {
        use crate::config::{
            COMPACT_RATIO_DEFAULT, COMPACT_TOOL_BUDGET_DEFAULT, COMPACT_USER_BUDGET_DEFAULT,
            DIRECT_CHILDREN_DEFAULT, MAX_TURNS_DEFAULT, RESIDENT_SUBAGENTS_DEFAULT,
            SKILLS_CATALOG_BUDGET_DEFAULT,
        };
        let cases: &[(&str, String)] = &[
            ("agent.max_turns", MAX_TURNS_DEFAULT.to_string()),
            (
                "agent.max_direct_children",
                DIRECT_CHILDREN_DEFAULT.to_string(),
            ),
            (
                "agent.max_resident_subagents",
                RESIDENT_SUBAGENTS_DEFAULT.to_string(),
            ),
            (
                "agent.skills_catalog_budget",
                SKILLS_CATALOG_BUDGET_DEFAULT.to_string(),
            ),
            ("compact.ratio", COMPACT_RATIO_DEFAULT.to_string()),
            (
                "compact.user_budget_tokens",
                COMPACT_USER_BUDGET_DEFAULT.to_string(),
            ),
            (
                "compact.tool_budget_tokens",
                COMPACT_TOOL_BUDGET_DEFAULT.to_string(),
            ),
        ];
        for (key, expected) in cases {
            assert_eq!(find_spec(key).unwrap().default_display, *expected);
        }
        assert_eq!(
            find_spec("startup.sandbox").unwrap().default_display,
            yi_agent_tools::SandboxMode::default().as_str(),
            "the sandbox default must come from SandboxMode::default(), not a literal"
        );
    }
```

et `settings.rs` 里 `startup.sandbox` 的 `default_display` 从 `"workspace-write"` 保持不变——`SandboxMode::default()` 就是 `WorkspaceWrite`，`as_str()` 返回 `"workspace-write"`，两边一致。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib 2>&1 | tail -15
```

Expected: 全部通过，且 `defaults_are_constants_not_scattered_literals`、`settings_metadata_defaults_track_these_constants`、`default_display_matches_the_runtime_constants` 三条在列。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs yi-agent-rs/crates/yi-agent-runtime/src/settings.rs
git commit -m "refactor(runtime): name the remaining default values as constants

六个默认值原本是函数体里的裸字面量，settings.json 的元数据无法与它们建立
编译期联系。提成常量后，两条测试把 FIELDS 的 default_display 与它们钉在一起，
再出现 MAX_TURNS 20/200 那种漂移会直接变红。"
```

---

## Task 4: `Settings` 的读写（含未知键保留与 `0600`）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/settings.rs`

**Interfaces:**
- Consumes: `Settings`、`FIELDS`、`find_spec`（Task 2）
- Produces: `pub fn settings_path() -> PathBuf`
- Produces: `pub fn load_from(path: &Path) -> Settings`
- Produces: `pub fn load() -> Settings`
- Produces: `pub fn write_to(path: &Path, patch: &serde_json::Value) -> Result<(), SettingsError>`
- Produces: `pub fn write(patch: &serde_json::Value) -> Result<(), SettingsError>`
- Produces: `pub enum SettingsError { InvalidSetting { key: String, reason: String }, Io(String), Corrupt(String) }`

**设计要点：**

- `load_from` 宽容：缺失 / 坏 JSON → 全 `None` + `warn`。
- `write_to` 严格：base 读不出来（文件存在但 JSON 非法）→ 拒绝覆盖（沿用 `save_catalog_to` 的守卫）。文件不存在才从空对象起步。
- `write_to` 接收**扁平点路径 patch**（`{"agent.max_turns": 500}`），值为 `null` 表示删键。这与 RPC 形状一致，且天然规避「嵌套对象部分覆盖」的歧义。
- 写入 = 读文件得到 `serde_json::Map` → 逐字段套用 patch → 序列化 → 临时文件 `0600` → `rename` → 再 `set_permissions(0600)`。

- [ ] **Step 1: 写失败的测试**

追加到 `settings.rs` 的 `tests` 模块：

```rust
    use std::path::Path;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::TempDir::new().unwrap()
    }

    #[test]
    fn missing_file_loads_as_all_none() {
        let dir = temp_dir();
        let settings = load_from(&dir.path().join("settings.json"));
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn corrupt_json_loads_as_all_none_without_panicking() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load_from(&path), Settings::default());
    }

    #[test]
    fn loads_typed_values() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent":{"max_turns":500},"compact":{"ratio":75},"startup":{"sandbox":"read-only"}}"#,
        )
        .unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(settings.compact.ratio, Some(75));
        assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
        assert_eq!(settings.agent.system_prompt, None);
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(
            &path,
            &serde_json::json!({
                "agent.max_turns": 500,
                "compact.ratio": 75,
                "startup.sandbox": "read-only",
            }),
        )
        .unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(settings.compact.ratio, Some(75));
        assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
    }

    /// 一号缺陷的回归网：写入绝不能把文件里用户手写的未知顶层键删掉。
    /// 旧 `env_file::write` 的循环源是元数据表而不是文件内容，所以会删。
    #[test]
    fn write_preserves_unknown_top_level_keys() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"my_note":"keep me","agent":{"max_turns":100}}"#).unwrap();

        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["my_note"], "keep me");
        assert_eq!(value["agent"]["max_turns"], 500);
    }

    /// 未设置的字段不得被写成空值或 null 占位——那会让「未设置」与「设成空」失配。
    #[test]
    fn write_does_not_emit_keys_for_unset_fields() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let agent = value["agent"].as_object().unwrap();
        assert_eq!(agent.len(), 1, "only max_turns should be present: {agent:?}");
        assert!(value.get("compact").is_none(), "compact must not be materialized");
        assert!(value.get("startup").is_none(), "startup must not be materialized");
    }

    /// 空串等于未设置：写入侧必须删键，而不是写空串或 0。
    #[test]
    fn empty_string_removes_the_key() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();

        write_to(&path, &serde_json::json!({"agent.max_turns": ""})).unwrap();

        let settings = load_from(&path);
        assert_eq!(settings.agent.max_turns, None);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value.get("agent").and_then(|a| a.get("max_turns")).is_none());
    }

    #[test]
    fn null_removes_the_key() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();
        write_to(&path, &serde_json::json!({"agent.max_turns": null})).unwrap();
        assert_eq!(load_from(&path).agent.max_turns, None);
    }

    #[test]
    fn rejects_unknown_keys_without_writing() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let error = write_to(&path, &serde_json::json!({"agent.not_a_field": 1})).unwrap_err();
        assert!(matches!(error, SettingsError::InvalidSetting { .. }));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a rejected write must not touch the file"
        );
    }

    #[test]
    fn rejects_out_of_range_numbers_without_writing() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");

        for (key, value) in [
            ("agent.max_turns", serde_json::json!(0)),
            ("agent.max_direct_children", serde_json::json!(0)),
            ("compact.ratio", serde_json::json!(0)),
            ("compact.ratio", serde_json::json!(101)),
            ("compact.user_budget_tokens", serde_json::json!(0)),
            // 上限：`agent.max_turns` 落盘后要能反序列化回 `Option<u32>`。
            // 不设上限的话，写 u32::MAX+1 会让下次 `load_from` 整份失败回默认，
            // 用户只会看到「我改的设置全没了」。
            ("agent.max_turns", serde_json::json!(u32::MAX as u64 + 1)),
            ("agent.max_direct_children", serde_json::json!(u16::MAX as u64 + 1)),
            ("agent.max_resident_subagents", serde_json::json!(u16::MAX as u64 + 1)),
            ("agent.skills_catalog_budget", serde_json::json!(u32::MAX as u64 + 1)),
        ] {
            let error = write_to(&path, &serde_json::json!({ key: value })).unwrap_err();
            assert!(
                matches!(error, SettingsError::InvalidSetting { .. }),
                "{key}={value} should be rejected, got {error:?}"
            );
        }
        assert!(!path.exists(), "no rejected write may create the file");
    }

    #[test]
    fn rejects_unknown_select_option_without_writing() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        let error =
            write_to(&path, &serde_json::json!({"startup.sandbox": "bogus"})).unwrap_err();
        assert!(matches!(error, SettingsError::InvalidSetting { .. }));
        assert!(!path.exists());
    }

    /// 文件存在但读不出来 → 拒绝覆盖。那是用户的配置坏了，不是「没有配置」；
    /// 静默重建会把 `my_note` 之类手写内容整份扔掉。
    #[test]
    fn refuses_to_overwrite_a_corrupt_file() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ broken").unwrap();

        let error = write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap_err();
        assert!(matches!(error, SettingsError::Corrupt(_)));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ broken",
            "the corrupt file must be left exactly as it was"
        );
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "settings.json must be 0600");
    }

    #[test]
    fn write_creates_the_parent_directory() {
        let dir = temp_dir();
        let path = dir.path().join("nested/deeper/settings.json");
        write_to(&path, &serde_json::json!({"agent.max_turns": 500})).unwrap();
        assert!(path.exists());
    }

    /// 字符串字段的空白等同未设置。
    #[test]
    fn whitespace_only_string_is_treated_as_unset() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        write_to(&path, &serde_json::json!({"agent.system_prompt": "   "})).unwrap();
        assert_eq!(load_from(&path).agent.system_prompt, None);
    }

    /// spec 的「错误处理」一节：单个字段非法**只清掉那一个**，不把整份文件丢掉。
    /// 用户手改了 `sandbox` 拼错，不该连 `max_turns` 一起回默认。
    #[test]
    fn an_invalid_field_is_dropped_but_the_rest_survives() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent":{"max_turns":500},"compact":{"ratio":0},"startup":{"sandbox":"bogus"}}"#,
        )
        .unwrap();

        let settings = load_from(&path);

        assert_eq!(settings.agent.max_turns, Some(500), "a valid neighbour survives");
        assert_eq!(
            settings.compact.ratio, None,
            "an out-of-range value falls back to the default"
        );
        assert_eq!(
            settings.startup.sandbox, None,
            "an unknown enum value falls back to the default"
        );
    }

    /// 顶层不是对象（比如被写成了数组）→ 整份回默认，而不是 panic。
    #[test]
    fn a_non_object_settings_file_falls_back_to_defaults() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1,2,3]").unwrap();
        assert_eq!(load_from(&path), Settings::default());
    }

    /// 单个字段类型不对（`"abc"`、`-1`、数组）只让它自己变未设置。
    #[test]
    fn a_wrong_typed_field_is_dropped_but_the_rest_survives() {
        let dir = temp_dir();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent":{"max_turns":"abc","max_direct_children":-1,"system_prompt":42},
                "compact":{"ratio":75}}"#,
        )
        .unwrap();

        let settings = load_from(&path);

        assert_eq!(settings.agent.max_turns, None);
        assert_eq!(settings.agent.max_direct_children, None);
        assert_eq!(settings.agent.system_prompt, None);
        assert_eq!(
            settings.compact.ratio,
            Some(75),
            "a well-typed neighbour must survive"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -20
```

Expected: 编译失败，`cannot find function load_from`。

- [ ] **Step 3: 实现读写**

追加到 `settings.rs`：

```rust
use std::path::{Path, PathBuf};

/// `<instance_home>/settings.json`；实例目录不可解析时回退 `.yi-agent/settings.json`，
/// 与 `models_path` / `websearch_path` 同口径。
pub fn settings_path() -> PathBuf {
    yi_agent_store::paths::home_join("settings.json")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("settings.json"))
}

/// 读 `settings.json`。缺失 / 不可读 / JSON 非法一律回全 `None`（用默认值）——
/// 坏文件绝不阻断启动，与 `models.json` / `preferences.json` 同一约定。
///
/// **单个字段非法只清掉那一个**：先按 `Option<T>` 宽松反序列化（类型不符的字段
/// 会因 `deserialize` 失败而拖垮整份，故这里用 `Value` 中转），再逐字段按
/// `FIELDS` 校验，越界或枚举非法的值置 `None` 并 `warn`。用户手改 `sandbox`
/// 拼错时不该连 `max_turns` 一起回默认。
pub fn load_from(path: &Path) -> Settings {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "could not read settings.json; using defaults");
            return Settings::default();
        }
    };
    // 先确认顶层是对象。`serde_json::from_str::<Settings>` 对数组/标量会失败，
    // 那样整份回默认；这条路径单独提出只是为了让 warn 措辞准确。
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(error = %error, path = %path.display(), "invalid settings.json; using defaults");
            return Settings::default();
        }
    };
    if !value.is_object() {
        tracing::warn!(path = %path.display(), "settings.json is not a JSON object; using defaults");
        return Settings::default();
    }
    let settings = match serde_json::from_value::<Settings>(value) {
        Ok(settings) => settings,
        Err(error) => {
            // 到这一步只剩结构性的坏（比如 `"agent": 5`）。字段级的类型不符
            // 已被 `de_optional_*` 吸收成 `None`，不会走到这里。
            tracing::warn!(error = %error, path = %path.display(), "invalid settings.json; using defaults");
            return Settings::default();
        }
    };
    settings.normalized()
}

/// 读实例目录下的 `settings.json`。
pub fn load() -> Settings {
    load_from(&settings_path())
}

impl Settings {
    /// 施加两条加载期约束，让加载侧与写入侧对「有效值」只有一个判据：
    ///
    /// 1. **空串等于未设置**：字符串字段的空白串归一为 `None`。
    /// 2. **单字段非法只清掉那一个**：按 [`FIELDS`] 校验数值边界与枚举白名单，
    ///    不合法就置 `None` 并 `warn`。整份文件丢掉会让用户「改错一个字段、
    ///    全盘回默认」——spec 的错误处理一节明确要求逐字段降级。
    fn normalized(mut self) -> Settings {
        self.agent.system_prompt = self
            .agent
            .system_prompt
            .filter(|value| !value.trim().is_empty());
        self.startup.sandbox = self.startup.sandbox.filter(|value| !value.trim().is_empty());

        // 数值上下界。逐字段写出而不做泛型分派：`Option<u32>` / `Option<u16>` /
        // `Option<usize>` 的宽度不同，泛型反而要引入 trait。
        macro_rules! clamp {
            ($key:literal, $field:expr) => {{
                if let Some(value) = $field {
                    if !within($key, value as u64) {
                        tracing::warn!(
                            key = $key,
                            value,
                            "settings.json value is out of range; falling back to the default"
                        );
                        $field = None;
                    }
                }
            }};
        }
        clamp!("agent.max_turns", self.agent.max_turns);
        clamp!("agent.max_direct_children", self.agent.max_direct_children);
        clamp!("agent.max_resident_subagents", self.agent.max_resident_subagents);
        clamp!("agent.skills_catalog_budget", self.agent.skills_catalog_budget);
        clamp!("compact.ratio", self.compact.ratio);
        clamp!("compact.user_budget_tokens", self.compact.user_budget_tokens);
        clamp!("compact.tool_budget_tokens", self.compact.tool_budget_tokens);

        // 枚举白名单：只认 `parse_sandbox_mode` 那份映射，不另写一遍。
        // 该函数当前是 `config.rs` 里的私有 `fn`，本任务需把它的可见性改成
        // `pub(crate) fn parse_sandbox_mode(...)`（`settings.rs` 与 `config.rs`
        // 同 crate，`pub(crate)` 足够）。
        if let Some(sandbox) = self.startup.sandbox.as_deref() {
            if crate::config::parse_sandbox_mode(sandbox).is_none() {
                tracing::warn!(
                    value = sandbox,
                    "settings.json has an unknown startup.sandbox; falling back to the default"
                );
                self.startup.sandbox = None;
            }
        }
        self
    }
}

/// `key` 的 `min`/`max` 约束是否满足。`FIELDS` 之外的键一律放行。
fn within(key: &str, value: u64) -> bool {
    let Some(spec) = find_spec(key) else {
        return true;
    };
    spec.min.is_none_or(|min| value >= min) && spec.max.is_none_or(|max| value <= max)
}

/// 写入失败的原因。`InvalidSetting` 与 `Corrupt` 是**零写入**的拒绝：
/// 文件的 mtime 与内容都不会变。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsError {
    /// 键不认识、类型不对、数值越界、枚举值非法。
    InvalidSetting { key: String, reason: String },
    /// 文件存在，但读不出来 / 不是 JSON 对象。拒绝覆盖。
    Corrupt(String),
    Io(String),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::InvalidSetting { key, reason } => {
                write!(f, "invalid setting {key}: {reason}")
            }
            SettingsError::Corrupt(reason) => write!(f, "settings.json is not writable: {reason}"),
            SettingsError::Io(reason) => write!(f, "settings.json io error: {reason}"),
        }
    }
}

impl std::error::Error for SettingsError {}

/// 写实例目录下的 `settings.json`。
pub fn write(patch: &serde_json::Value) -> Result<(), SettingsError> {
    write_to(&settings_path(), patch)
}

/// 部分更新：`patch` 的键是点路径（`agent.max_turns`），值为 `null` 或 `""`
/// 表示删除该键、回退默认。
///
/// **读-改-写，不是重建**：文件里的未知顶层键原样保留。这是 `env_file::write`
/// 一号缺陷（保存时删掉不认识的键）的直接修复。
pub fn write_to(path: &Path, patch: &serde_json::Value) -> Result<(), SettingsError> {
    let patch = patch
        .as_object()
        .ok_or_else(|| SettingsError::InvalidSetting {
            key: "<root>".to_string(),
            reason: "patch must be a JSON object".to_string(),
        })?;

    // 全部校验先做完：任何一项失败都零写入。
    let mut ops: Vec<(&'static FieldSpec, Option<serde_json::Value>)> = Vec::new();
    for (key, value) in patch {
        let spec = find_spec(key).ok_or_else(|| SettingsError::InvalidSetting {
            key: key.clone(),
            reason: "unknown setting".to_string(),
        })?;
        ops.push((spec, validate_value(spec, value)?));
    }

    // 读底：文件存在却读不出来 → 拒绝覆盖（与 `save_catalog_to` 同一守卫）。
    let mut object = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| {
                SettingsError::Corrupt(format!(
                    "{} exists but is not a readable JSON object",
                    path.display()
                ))
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(SettingsError::Io(error.to_string())),
    };

    for (spec, value) in ops {
        apply_op(&mut object, spec.key, value);
    }

    // 删掉只剩空壳的分组对象，避免留下 {"agent":{}} 这种噪音。
    for group in ["agent", "compact", "startup"] {
        if object
            .get(group)
            .and_then(|value| value.as_object())
            .is_some_and(|inner| inner.is_empty())
        {
            object.remove(group);
        }
    }

    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(|error| SettingsError::Io(error.to_string()))?;
    write_atomically(path, &text).map_err(|error| SettingsError::Io(error.to_string()))
}

/// 校验单个值；`Ok(None)` 表示「删键」。
fn validate_value(
    spec: &'static FieldSpec,
    value: &serde_json::Value,
) -> Result<Option<serde_json::Value>, SettingsError> {
    let reject = |reason: String| SettingsError::InvalidSetting {
        key: spec.key.to_string(),
        reason,
    };

    // `null` 与空串都表示删键。
    if value.is_null() {
        return Ok(None);
    }
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() {
            return Ok(None);
        }
    }

    match spec.field_type {
        FieldType::Number => {
            let number = match value {
                serde_json::Value::Number(n) => n.as_u64(),
                // 表单发上来的是字符串，这里统一收口，避免两套解析。
                serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
                _ => None,
            }
            .ok_or_else(|| reject("expected a non-negative integer".to_string()))?;
            if let Some(min) = spec.min {
                if number < min {
                    return Err(reject(format!("must be at least {min}")));
                }
            }
            if let Some(max) = spec.max {
                if number > max {
                    return Err(reject(format!("must be at most {max}")));
                }
            }
            Ok(Some(serde_json::Value::from(number)))
        }
        FieldType::Text => Ok(Some(serde_json::Value::String(
            value
                .as_str()
                .ok_or_else(|| reject("expected a string".to_string()))?
                .to_string(),
        ))),
        FieldType::Select => {
            let text = value
                .as_str()
                .ok_or_else(|| reject("expected a string".to_string()))?;
            if !spec.options.contains(&text) {
                return Err(reject(format!(
                    "expected one of {:?}, got {text:?}",
                    spec.options
                )));
            }
            Ok(Some(serde_json::Value::String(text.to_string())))
        }
    }
}

/// 把一个点路径键套进顶层对象。`None` 表示删键。
fn apply_op(
    object: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    value: Option<serde_json::Value>,
) {
    let (group, field) = match key.split_once('.') {
        Some(pair) => pair,
        // FIELDS 里每一项都是 `group.field`，走不到这里。
        None => return,
    };
    match value {
        Some(value) => {
            let entry = object
                .entry(group.to_string())
                .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
            if let Some(inner) = entry.as_object_mut() {
                inner.insert(field.to_string(), value);
            }
        }
        None => {
            if let Some(inner) = object.get_mut(group).and_then(|v| v.as_object_mut()) {
                inner.remove(field);
            }
        }
    }
}

/// 临时文件 + `rename` + `0600`，与 `models.rs` 的 `save_catalog_to` 同一套。
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        "settings.json.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, text)?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib settings::tests 2>&1 | tail -25
```

Expected: 全部通过（约 28 条）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/settings.rs
git commit -m "feat(runtime): read and write settings.json atomically

写入是读-改-写并保留未知顶层键，直接修掉 env_file::write 那个「保存一次
删掉不认识的键」的一号缺陷。校验全部前置、失败零写入；文件存在却读不出来
时拒绝覆盖而不是重建；落盘用临时文件 + rename + 0600。"
```

---

## Task 5: `config.rs` 把 `.env` 换成 `settings.json` 作为第三层

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs:236-412`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`

**Interfaces:**
- Consumes: `settings::{load_from, settings_path, Settings}`（Task 4）
- Produces: `fn load_settings_for_config() -> Settings`（私有，便于测试注入路径）

**要改的三件事：**

1. `load_inner` 不再 `load_env_files`，改为读 `settings.json`。
2. 每个字段的合并链从 `overrides.or_else(env).unwrap_or(default)` 改为 `overrides.or_else(env).or_else(settings).unwrap_or(default)`。
3. `resolve_workdir` 删除 `YI_AGENT_WORKDIR` 那一层。

**顺序至关重要：** 合并链里 env 必须在 settings 之前。spec 已定「真实进程环境变量 > settings.json」。

- [ ] **Step 1: 写失败的测试**

在 `config.rs` 的 `tests` 模块追加：

```rust
    /// 设置一个白名单环境变量，并让它指向隔离的实例目录。
    struct IsolatedInstance {
        _env: EnvVarGuard,
        dir: tempfile::TempDir,
    }

    impl IsolatedInstance {
        fn new() -> Self {
            let dir = tempfile::TempDir::new().unwrap();
            let mut env = isolated_config_env();
            env.set("YI_AGENT_HOME", dir.path().join(".yi-agent"));
            Self { _env: env, dir }
        }

        fn write_settings(&self, json: &str) {
            let home = self.dir.path().join(".yi-agent");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("settings.json"), json).unwrap();
        }
    }

    #[test]
    fn settings_json_supplies_values_when_nothing_else_does() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(
            r#"{"agent":{"max_turns":321},"compact":{"ratio":55},"startup":{"sandbox":"read-only"}}"#,
        );

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 321);
        assert_eq!(cfg.compact_ratio, 55);
        assert_eq!(cfg.sandbox, yi_agent_tools::SandboxMode::ReadOnly);
        // 配置出来的沙箱不是「显式指定」，故仍可运行期提权。
        assert!(
            cfg.sandbox_promotable,
            "a sandbox from settings.json must stay promotable"
        );
    }

    /// 真实进程环境变量压过 settings.json——这是 spec 定的第二/第三层。
    #[test]
    fn real_env_beats_settings_json() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":321}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_MAX_TURNS", "654");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 654);
    }

    /// CLI 覆盖压过两者。
    #[test]
    fn override_beats_env_and_settings() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"max_turns":321}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.set("YI_AGENT_MAX_TURNS", "654");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            max_turns: Some(999),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, 999);
    }

    #[test]
    fn missing_settings_json_falls_back_to_defaults() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.max_turns, MAX_TURNS_DEFAULT);
        assert_eq!(cfg.compact_ratio, COMPACT_RATIO_DEFAULT);
        assert_eq!(cfg.skills_catalog_budget, SKILLS_CATALOG_BUDGET_DEFAULT);
    }

    #[test]
    fn corrupt_settings_json_does_not_block_startup() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings("{ not json");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .expect("a corrupt settings.json must not block startup");

        assert_eq!(cfg.max_turns, MAX_TURNS_DEFAULT);
    }

    /// 空白串等同于未设置，不能让 `system_prompt` 变成空串而不是 None。
    #[test]
    fn blank_settings_string_is_unset() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"system_prompt":"   "}}"#);

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.system_prompt, None);
    }

    /// `YI_AGENT_WORKDIR` 已下线：设了也不影响工作目录。
    #[test]
    fn workdir_env_var_no_longer_overrides() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let here = tempfile::TempDir::new().unwrap();
        let elsewhere = tempfile::TempDir::new().unwrap();

        let mut env = isolated_config_env();
        env.set("YI_AGENT_WORKDIR", elsewhere.path());

        let resolved = resolve_workdir(&ConfigOverrides {
            workdir: Some(here.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resolved, here.path());
        assert_ne!(resolved, elsewhere.path());
    }

    /// settings.json 里的 budget 不算「显式指定」：技能目录超预算时仍应询问
    /// （交互终端下），而不是静默截断。
    #[test]
    fn settings_budget_is_not_explicit() {
        let _lock = ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let instance = IsolatedInstance::new();
        instance.write_settings(r#"{"agent":{"skills_catalog_budget":4096}}"#);

        let mut env = isolated_config_env();
        env.set("YI_AGENT_HOME", instance.dir.path().join(".yi-agent"));
        env.remove("YI_AGENT_SKILLS_CATALOG_BUDGET");

        let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
            workdir: Some(instance.dir.path().to_path_buf()),
            ..Default::default()
        })
        .unwrap();

        assert_eq!(cfg.skills_catalog_budget, 4096);
        assert!(
            !cfg.skills_catalog_budget_explicit,
            "settings.json is a long-lived preference, not an explicit per-launch choice"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config::tests::settings_json_supplies 2>&1 | tail -15
```

Expected: FAIL，`assertion left == right failed`（`max_turns` 仍是 200 而非 321）。

- [ ] **Step 3: 改 `load_inner` 的加载源**

在 `config.rs` 的 `load_inner` 里，把开头三行替换为：

```rust
    fn load_inner(overrides: &ConfigOverrides) -> Result<Self> {
        // 快照必须早于任何环境注入（见 `ModelEnvSnapshot` 的文档）。
        let model_env = ModelEnvSnapshot::take();
        // `settings.json` 是第三层（覆盖 > 真实 env > settings.json > 默认）。
        // 坏文件已在 `load_from` 里降级成全 `None`，故这里不必再处理错误。
        let settings = crate::settings::load();
```

把 `load_env_files` 与 `prune_dotenv_model_keys` 两行删除。

**`ModelEnvSnapshot` 必须保留。** 它读的不是 `.env`，而是**真实进程环境变量**——`loaded_inner` 里的 `injected_provider` / `injected_model` / `injected_url` / `injected_key` 四个值来自 `model_env.get(...)`。`.env` 一消失，`prune_dotenv_model_keys` 就没有存在意义（它唯一的作用是把「因 dotenvy 注入而出现在进程环境里」的键剔除），但 `ModelEnvSnapshot::take()` 变成「在任何东西污染进程环境之前先快照真实值」这一步仍然必要：它保住了 spec 定的第二层「真实进程环境变量」。

`prune_dotenv_model_keys` 与 `load_env_files` 在本任务之后**没有生产调用方**了——它们连同各自的测试在 Task 14 一起删除。本任务先让它们留在原地：`load_env_files` / `prune_dotenv_model_keys` 只被自己的测试调用会触发 dead_code 警告，故本任务给它们加 `#[allow(dead_code)]` 并留 `// TODO(Task 14)` 注释。

- [ ] **Step 4: 改各字段的合并链**

逐字段改写。每个字段都是「在 `unwrap_or(...)` 之前插入 `.or_else(|| settings....)`」：

```rust
        let max_turns = overrides
            .max_turns
            .or_else(|| {
                std::env::var("YI_AGENT_MAX_TURNS")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .or(settings.agent.max_turns)
            .unwrap_or(MAX_TURNS_DEFAULT);

        let max_resident_subagents = env_u16("YI_AGENT_MAX_RESIDENT_SUBAGENTS")
            .or(settings.agent.max_resident_subagents)
            .unwrap_or(RESIDENT_SUBAGENTS_DEFAULT);

        let max_direct_children = env_u16("YI_AGENT_MAX_DIRECT_CHILDREN")
            .or(settings.agent.max_direct_children)
            .unwrap_or(DIRECT_CHILDREN_DEFAULT);

        let system_prompt = overrides
            .system_prompt
            .clone()
            .or_else(|| std::env::var("YI_AGENT_SYSTEM_PROMPT").ok())
            .or_else(|| settings.agent.system_prompt.clone())
            .filter(|s| !s.is_empty());

        let compact_ratio = overrides
            .compact_ratio
            .or_else(|| {
                std::env::var("YI_AGENT_COMPACT_RATIO")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .or(settings.compact.ratio)
            .unwrap_or(COMPACT_RATIO_DEFAULT);

        let compact_user_budget_tokens = overrides
            .compact_user_budget_tokens
            .or_else(|| env_usize("YI_AGENT_COMPACT_USER_BUDGET_TOKENS"))
            .or(settings.compact.user_budget_tokens)
            .unwrap_or(COMPACT_USER_BUDGET_DEFAULT);

        let compact_tool_budget_tokens = overrides
            .compact_tool_budget_tokens
            .or_else(|| env_usize("YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS"))
            .or(settings.compact.tool_budget_tokens)
            .unwrap_or(COMPACT_TOOL_BUDGET_DEFAULT);

        let skills_catalog_budget = overrides
            .skills_catalog_budget
            .or_else(|| {
                std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET")
                    .ok()
                    .and_then(|s| s.parse().ok())
            })
            .or(settings.agent.skills_catalog_budget)
            .unwrap_or(SKILLS_CATALOG_BUDGET_DEFAULT);
```

在 `env_usize` 旁边加一个 u16 版本：

```rust
fn env_u16(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}
```

沙箱那一段改成三层（`overrides` > `YI_AGENT_SANDBOX` > `settings.startup.sandbox`）：

```rust
        // settings.json 的沙箱与 env 的沙箱都不算「显式指定启动姿态」的 yolo，
        // 但按 `sandbox_promotable` 的原义（「启动时沙箱未被显式指定」），
        // 来自配置文件的档位等同过去来自 .env 的档位：不阻断运行期提权。
        // 只有 CLI flag 与真实 env 才把它钉死为不可提权。
        let settings_sandbox = settings
            .startup
            .sandbox
            .as_deref()
            .and_then(parse_sandbox_mode);
        let (sandbox, sandbox_promotable) = match overrides.sandbox {
            Some(mode) => (mode, false),
            None => match env_sandbox {
                Some(value) => (
                    parse_sandbox_mode(&value).ok_or_else(|| {
                        anyhow::anyhow!(
                            "invalid YI_AGENT_SANDBOX: expected read-only, workspace-write, or danger-full-access"
                        )
                    })?,
                    false,
                ),
                None if overrides.yolo => (yi_agent_tools::SandboxMode::DangerFullAccess, !yolo),
                None => (
                    settings_sandbox.unwrap_or_default(),
                    !yolo,
                ),
            },
        };
```

- [ ] **Step 5: 下线 `YI_AGENT_WORKDIR`**

```rust
/// Resolve the effective workdir without loading provider configuration.
///
/// Priority is the explicit workdir override, then the current directory.
/// `YI_AGENT_WORKDIR` is intentionally gone: the working directory is a
/// session/project property, decided by `--workdir`, the launch directory, or
/// the project the desktop app selected. A global env var fighting the
/// "last opened project" was the exact conflict this removal fixes.
pub fn resolve_workdir(overrides: &ConfigOverrides) -> Result<PathBuf> {
    let workdir = overrides
        .workdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    if !workdir.is_dir() {
        bail!("working directory does not exist: {}", workdir.display());
    }
    Ok(workdir)
}
```

- [ ] **Step 6: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib config:: 2>&1 | tail -25
```

Expected: 全部通过。若有既有的 `.env` 相关测试因加载链改变而失败（例如断言「non-model keys still come from the .env」），按 Task 14 的清单标记为待删——**但本任务里先让它们通过**：把它们的 `.env` 写入改成同时写 `settings.json`，保持断言语义不变。具体做法：这些测试原本在 `$HOME/.yi-agent/.env` 里写 `YI_AGENT_MAX_TURNS=123`，现在改为在 `$HOME/.yi-agent/settings.json` 里写 `{"agent":{"max_turns":123}}`。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "feat(runtime): read settings.json as the third config layer

加载链从「覆盖 > 真实 env > .env > 默认」改为「覆盖 > 真实 env >
settings.json > 默认」。YI_AGENT_WORKDIR 同步下线：工作目录是会话/项目属性，
只由 --workdir、启动目录、桌面端选项目决定。"
```

---

## Task 6: `migrate_env_settings`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs`
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs`

**Interfaces:**
- Consumes: `settings::{parse_env_text, load_from, write_to, settings_path}`（Task 1、4）、`web::config::{load_websearch_config_from, EngineConfig}`
- Produces: `pub enum EnvMigrationOutcome { NothingToDo, Migrated { fields: usize, bocha: bool }, EnvUnreadable { reason: String }, SettingsNotWritable { reason: String } }`
- Produces: `pub fn migrate_env_settings(env_path: &Path, settings_path: &Path, websearch_path: &Path) -> EnvMigrationOutcome`
- Produces: `pub fn log_env_settings_outcome(outcome: &EnvMigrationOutcome, env_path: &Path)`

**A 类键清单**（写死在函数里，不引入常量表——它是一次性迁移的历史契约，多一处引用只会多一处漂移点）：

```
YI_AGENT_MAX_TURNS                     → agent.max_turns
YI_AGENT_MAX_DIRECT_CHILDREN           → agent.max_direct_children
YI_AGENT_MAX_RESIDENT_SUBAGENTS        → agent.max_resident_subagents
YI_AGENT_SYSTEM_PROMPT                 → agent.system_prompt
YI_AGENT_SKILLS_CATALOG_BUDGET         → agent.skills_catalog_budget
YI_AGENT_COMPACT_RATIO                 → compact.ratio
YI_AGENT_COMPACT_USER_BUDGET_TOKENS    → compact.user_budget_tokens
YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS    → compact.tool_budget_tokens
YI_AGENT_SANDBOX                       → startup.sandbox
BOCHA_API_KEY                          → websearch.json engines.bocha.api_key
YI_AGENT_WORKDIR                       → 丢弃 + warn
YI_AGENT_YOLO                          → 丢弃（安全姿态不持久化）
```

**幂等性**：`.env` 改名成 `.env.migrated` 后第二次调用读不到 `.env`，自然返回 `NothingToDo`。

- [ ] **Step 1: 写失败的测试**

在 `migrate.rs` 的 `tests` 模块追加：

```rust
    use crate::settings::{load_from, write_to};

    struct MigrationFixture {
        dir: tempfile::TempDir,
    }

    impl MigrationFixture {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
            }
        }
        fn env(&self) -> std::path::PathBuf {
            self.dir.path().join(".env")
        }
        fn settings(&self) -> std::path::PathBuf {
            self.dir.path().join("settings.json")
        }
        fn websearch(&self) -> std::path::PathBuf {
            self.dir.path().join("websearch.json")
        }
        fn write_env(&self, text: &str) {
            std::fs::write(self.env(), text).unwrap();
        }
        fn run(&self) -> EnvMigrationOutcome {
            migrate_env_settings(&self.env(), &self.settings(), &self.websearch())
        }
    }

    #[test]
    fn missing_env_is_nothing_to_do() {
        let fixture = MigrationFixture::new();
        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
        assert!(!fixture.settings().exists(), "no settings.json may be created");
    }

    #[test]
    fn env_without_a_class_keys_is_nothing_to_do_and_leaves_the_file_alone() {
        let fixture = MigrationFixture::new();
        fixture.write_env("SOME_OTHER_TOOL=1\n");

        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
        assert!(fixture.env().exists(), "no A-class key means nothing to migrate");
        assert!(!fixture.settings().exists());
    }

    #[test]
    fn a_class_keys_land_in_settings_json() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
# MIGRATED: YI_AGENT_PROVIDER=openai
YI_AGENT_MAX_TURNS=500
YI_AGENT_COMPACT_RATIO=75
YI_AGENT_SANDBOX=read-only
",
        );

        let outcome = fixture.run();
        assert_eq!(
            outcome,
            EnvMigrationOutcome::Migrated {
                fields: 3,
                bocha: false
            }
        );

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(settings.compact.ratio, Some(75));
        assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
    }

    #[test]
    fn existing_settings_values_are_never_overwritten() {
        let fixture = MigrationFixture::new();
        write_to(&fixture.settings(), &serde_json::json!({"agent.max_turns": 42})).unwrap();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\nYI_AGENT_COMPACT_RATIO=75\n");

        fixture.run();

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, Some(42), "existing value must win");
        assert_eq!(
            settings.compact.ratio,
            Some(75),
            "the unset field still gets migrated"
        );
    }

    #[test]
    fn env_is_renamed_to_migrated_after_a_successful_run() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");

        fixture.run();

        assert!(!fixture.env().exists(), ".env must be gone");
        let renamed = fixture.dir.path().join(".env.migrated");
        assert!(renamed.exists(), ".env.migrated must exist");
        assert!(
            std::fs::read_to_string(&renamed)
                .unwrap()
                .contains("YI_AGENT_MAX_TURNS=500"),
            "the renamed file keeps its content for recovery"
        );
    }

    #[test]
    fn second_run_is_idempotent_and_does_not_touch_settings() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");
        fixture.run();

        let mtime_before = std::fs::metadata(fixture.settings()).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = fixture.run();

        assert_eq!(second, EnvMigrationOutcome::NothingToDo);
        let mtime_after = std::fs::metadata(fixture.settings()).unwrap().modified().unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "a second run must not rewrite settings.json"
        );
    }

    #[test]
    fn unusable_settings_json_blocks_the_rename() {
        let fixture = MigrationFixture::new();
        std::fs::write(fixture.settings(), "{ broken").unwrap();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");

        let outcome = fixture.run();
        assert!(
            matches!(outcome, EnvMigrationOutcome::SettingsNotWritable { .. }),
            "got {outcome:?}"
        );
        assert!(
            fixture.env().exists(),
            ".env must survive when the target is not writable"
        );
    }

    #[test]
    fn bocha_key_moves_into_websearch_json() {
        let fixture = MigrationFixture::new();
        fixture.write_env("BOCHA_API_KEY=bocha-abc\nYI_AGENT_MAX_TURNS=500\n");

        let outcome = fixture.run();
        assert!(matches!(
            outcome,
            EnvMigrationOutcome::Migrated { bocha: true, .. }
        ));

        let config = yi_agent_tools::web::config::load_websearch_config_from(&fixture.websearch());
        assert_eq!(config.engines["bocha"].api_key, "bocha-abc");
    }

    #[test]
    fn an_existing_bocha_key_is_not_overwritten() {
        let fixture = MigrationFixture::new();
        let mut cfg = yi_agent_tools::web::config::WebSearchConfig::default();
        cfg.engines.get_mut("bocha").unwrap().api_key = "keep-me".to_string();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        std::fs::write(fixture.websearch(), text).unwrap();

        fixture.write_env("BOCHA_API_KEY=bocha-abc\n");

        fixture.run();

        let config = yi_agent_tools::web::config::load_websearch_config_from(&fixture.websearch());
        assert_eq!(config.engines["bocha"].api_key, "keep-me");
    }

    /// `YI_AGENT_WORKDIR` 没有落点，丢弃即可；`YI_AGENT_YOLO` 是安全姿态，绝不能
    /// 被持久化（否则 `sandbox_promotable` 恒为 false）。
    #[test]
    fn workdir_and_yolo_are_dropped() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
YI_AGENT_WORKDIR=/tmp/somewhere
YI_AGENT_YOLO=true
YI_AGENT_MAX_TURNS=500
",
        );

        fixture.run();

        let text = std::fs::read_to_string(fixture.settings()).unwrap();
        assert!(!text.contains("workdir"), "workdir must not be persisted");
        assert!(!text.contains("yolo"), "yolo must not be persisted");
        assert!(text.contains("max_turns"));
    }

    /// 迁移必须用 Task 1 的解析口径：注释不能被并进值，`export` 前缀要剥掉。
    #[test]
    fn migration_uses_the_dotenvy_compatible_parser() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
export YI_AGENT_MAX_TURNS=500 # this is a comment
YI_AGENT_SYSTEM_PROMPT='hello # world'
",
        );

        fixture.run();

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(
            settings.agent.system_prompt.as_deref(),
            Some("hello # world"),
            "a # inside quotes is literal, not a comment"
        );
    }

    /// 一个非法值不能让整次迁移失败：该字段跳过，其余照常落盘。
    #[test]
    fn an_invalid_value_is_skipped_but_the_rest_still_migrates() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=not-a-number\nYI_AGENT_COMPACT_RATIO=75\n");

        let outcome = fixture.run();
        assert!(matches!(outcome, EnvMigrationOutcome::Migrated { .. }));

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, None);
        assert_eq!(settings.compact.ratio, Some(75));
    }

    /// 只剩已废弃键时也要改名：那两个键已无读取方，留着会让每次启动都重复
    /// 同一条告警，而文件永远不消失。
    #[test]
    fn a_file_with_only_retired_keys_is_renamed_not_left_behind() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_WORKDIR=/some/project\nYI_AGENT_YOLO=1\n");

        assert_eq!(fixture.run(), EnvMigrationOutcome::DroppedOnly);

        assert!(!fixture.env().exists(), "the .env must be renamed away");
        assert!(
            fixture.dir.path().join(".env.migrated").exists(),
            "the old file is kept under .env.migrated for recovery"
        );
        assert!(
            !fixture.settings().exists(),
            "there was nothing to write, so settings.json must not appear"
        );
        assert!(!fixture.websearch().exists());
    }

    /// 第二次调用已无 `.env`，返回 `NothingToDo`——`DroppedOnly` 也是幂等的。
    #[test]
    fn dropped_only_is_idempotent() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_WORKDIR=/some/project\n");
        assert_eq!(fixture.run(), EnvMigrationOutcome::DroppedOnly);
        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib migrate::tests::a_class_keys 2>&1 | tail -15
```

Expected: 编译失败，`cannot find function migrate_env_settings`。

- [ ] **Step 3: 实现迁移**

追加到 `migrate.rs`（在 `#[cfg(test)]` 之前）：

```rust
/// A 类键迁移的结果。与 [`MigrationOutcome`] 分开：模型键与运行参数是两条独立
/// 的一次性迁移，混用一个枚举会让日志措辞打架。
#[derive(Debug, PartialEq, Eq)]
pub enum EnvMigrationOutcome {
    /// `.env` 不存在，或里面没有任何 A 类键、也没有已废弃的 `YI_AGENT_WORKDIR` /
    /// `YI_AGENT_YOLO`。
    NothingToDo,
    /// 已落盘并改名。
    Migrated { fields: usize, bocha: bool },
    /// `.env` 里只有已废弃的键（`YI_AGENT_WORKDIR` / `YI_AGENT_YOLO`），没有可迁移的
    /// 值。文件已改名成 `.env.migrated`，`settings.json` 与 `websearch.json` 未创建。
    ///
    /// 为什么仍要改名：这两个键已经没有任何读取方，留着只会让每次启动都重复同一条
    /// 告警，而文件本身永远不会消失。改名即「这件事已经处理完了」。
    DroppedOnly,
    /// `.env` 读不出来。`settings.json` 未动。
    EnvUnreadable { reason: String },
    /// `settings.json`（或 `websearch.json`）写不进去。`.env` 未改名，可重试。
    SettingsNotWritable { reason: String },
}

/// 把 `.env` 里的 A 类键迁进 `settings.json`，`BOCHA_API_KEY` 迁进 `websearch.json`，
/// 成功后把 `.env` 改名为 `.env.migrated`。
///
/// **必须与 [`migrate_model_keys`] 在同一处、且在 `RuntimeConfig::load_lenient`
/// 之前调用。** 顺序错了会造出半迁移：`.env` 已被注入进程环境又被改名，配置看起来
/// 迁了、重启后没了。
///
/// **幂等**：第二次调用时 `.env` 已改名，读不到，返回 `NothingToDo` 且不碰
/// `settings.json`。
pub fn migrate_env_settings(
    env_path: &Path,
    settings_path: &Path,
    websearch_path: &Path,
) -> EnvMigrationOutcome {
    let text = match std::fs::read_to_string(env_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EnvMigrationOutcome::NothingToDo;
        }
        Err(error) => {
            return EnvMigrationOutcome::EnvUnreadable {
                reason: error.to_string(),
            };
        }
    };

    let values = crate::settings::parse_env_text(&text);

    // (点路径键, .env 键)。顺序即日志里 `fields` 的计数依据。
    const A_CLASS: &[(&str, &str)] = &[
        ("agent.max_turns", "YI_AGENT_MAX_TURNS"),
        ("agent.max_direct_children", "YI_AGENT_MAX_DIRECT_CHILDREN"),
        (
            "agent.max_resident_subagents",
            "YI_AGENT_MAX_RESIDENT_SUBAGENTS",
        ),
        ("agent.system_prompt", "YI_AGENT_SYSTEM_PROMPT"),
        (
            "agent.skills_catalog_budget",
            "YI_AGENT_SKILLS_CATALOG_BUDGET",
        ),
        ("compact.ratio", "YI_AGENT_COMPACT_RATIO"),
        (
            "compact.user_budget_tokens",
            "YI_AGENT_COMPACT_USER_BUDGET_TOKENS",
        ),
        (
            "compact.tool_budget_tokens",
            "YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS",
        ),
        ("startup.sandbox", "YI_AGENT_SANDBOX"),
    ];

    // 只填「尚未设置」的字段：既有 `settings.json` 的值优先。
    let existing = crate::settings::load_from(settings_path);
    let mut patch = serde_json::Map::new();
    for (path_key, env_key) in A_CLASS {
        let Some(raw) = values.get(*env_key) else {
            continue;
        };
        if raw.trim().is_empty() {
            continue;
        }
        if field_is_set(&existing, path_key) {
            continue;
        }
        patch.insert((*path_key).to_string(), serde_json::Value::from(raw.clone()));
    }

    let bocha_key = values
        .get("BOCHA_API_KEY")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let has_workdir = values.contains_key("YI_AGENT_WORKDIR");
    let has_yolo = values.contains_key("YI_AGENT_YOLO");

    if patch.is_empty() && bocha_key.is_none() {
        if !has_workdir && !has_yolo {
            // 既没有 A 类键也没有废弃键：这不是我们的文件，原样留着。
            return EnvMigrationOutcome::NothingToDo;
        }
        // 只有废弃键。它们已无读取方，留着只会让每次启动重复同一条告警，
        // 故把所有废弃键逐条 warn 一遍后改名，让 `.env` 彻底退场。
        if has_workdir {
            tracing::warn!(
                path = %env_path.display(),
                "YI_AGENT_WORKDIR is no longer read; workdir now comes from --workdir, \
                 the launch directory, or the project picked in the desktop app"
            );
        }
        if has_yolo {
            tracing::warn!(
                path = %env_path.display(),
                "YI_AGENT_YOLO is no longer read; pass --yolo on every launch instead"
            );
        }
        return match std::fs::rename(env_path, env_path.with_file_name(".env.migrated")) {
            Ok(()) => EnvMigrationOutcome::DroppedOnly,
            Err(error) => EnvMigrationOutcome::SettingsNotWritable {
                reason: format!("could not rename .env to .env.migrated: {error}"),
            },
        };
    }

    // 1) 先落 settings.json：写不进去就绝不改名 `.env`（否则用户的值凭空丢失）。
    let mut migrated_fields = 0usize;
    if !patch.is_empty() {
        // 逐个字段写入：单个字段的非法值只跳过它本身，不拖垮整次迁移。
        for (key, value) in patch {
            let single = serde_json::Value::Object([(key.clone(), value)].into_iter().collect());
            match crate::settings::write_to(settings_path, &single) {
                Ok(()) => migrated_fields += 1,
                Err(error) => {
                    // 键不认识 / 类型不对 / 越界：都是「这个值迁不了」，不是存储故障。
                    // 存储故障（文件损坏 / IO）则必须中止。
                    match error {
                        crate::settings::SettingsError::InvalidSetting { .. } => {
                            tracing::warn!(
                                key = %key,
                                %error,
                                "skipping a .env value that settings.json cannot accept"
                            );
                        }
                        other => {
                            return EnvMigrationOutcome::SettingsNotWritable {
                                reason: other.to_string(),
                            };
                        }
                    }
                }
            }
        }
    }

    // 2) 再落 websearch.json：只在文件里没有非空 key 时写。
    let mut bocha_migrated = false;
    if let Some(key) = bocha_key {
        let config = yi_agent_tools::web::config::load_websearch_config_from(websearch_path);
        let already = config
            .engines
            .get("bocha")
            .map(|engine| !engine.api_key.trim().is_empty())
            .unwrap_or(false);
        if !already {
            let mut config = config;
            config
                .engines
                .entry("bocha".to_string())
                .or_default()
                .api_key = key;
            match save_websearch_config(websearch_path, &config) {
                Ok(()) => bocha_migrated = true,
                Err(error) => {
                    return EnvMigrationOutcome::SettingsNotWritable {
                        reason: format!("websearch.json: {error}"),
                    };
                }
            }
        }
    }

    // 3) 最后改名 `.env`：保留唯一一份明文 key 供用户取回。
    let renamed = env_path.with_file_name(".env.migrated");
    if let Err(error) = std::fs::rename(env_path, &renamed) {
        return EnvMigrationOutcome::EnvUnreadable {
            reason: format!("could not rename .env to .env.migrated: {error}"),
        };
    }
    tracing::info!(
        from = %env_path.display(),
        to = %renamed.display(),
        "migrated .env runtime settings into settings.json; the old file was renamed for recovery"
    );

    if has_workdir || has_yolo {
        tracing::warn!(
            "YI_AGENT_WORKDIR / YI_AGENT_YOLO are no longer read; workdir now comes from \
             --workdir or the launch directory, and yolo must be passed on every launch"
        );
    }

    EnvMigrationOutcome::Migrated {
        fields: migrated_fields,
        bocha: bocha_migrated,
    }
}

/// 某个点路径字段在既有 `Settings` 里是否已有值。
fn field_is_set(settings: &crate::settings::Settings, key: &str) -> bool {
    match key {
        "agent.max_turns" => settings.agent.max_turns.is_some(),
        "agent.max_direct_children" => settings.agent.max_direct_children.is_some(),
        "agent.max_resident_subagents" => settings.agent.max_resident_subagents.is_some(),
        "agent.system_prompt" => settings.agent.system_prompt.is_some(),
        "agent.skills_catalog_budget" => settings.agent.skills_catalog_budget.is_some(),
        "compact.ratio" => settings.compact.ratio.is_some(),
        "compact.user_budget_tokens" => settings.compact.user_budget_tokens.is_some(),
        "compact.tool_budget_tokens" => settings.compact.tool_budget_tokens.is_some(),
        "startup.sandbox" => settings.startup.sandbox.is_some(),
        _ => false,
    }
}

/// `websearch.json` 的原子落盘。与 `websearch_rpc::save_config` 同一套语义
/// （读-改-写保留未知键），但那一个是私有的且属于 app-server，故此处独立实现。
fn save_websearch_config(
    path: &Path,
    config: &yi_agent_tools::web::config::WebSearchConfig,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(ours) = serde_json::to_value(config)
        .map_err(std::io::Error::other)?
        .as_object()
    {
        for (key, value) in ours {
            object.insert(key.clone(), value.clone());
        }
    }
    let text = serde_json::to_vec_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!("websearch.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// 把 A 类迁移结果记成日志。`NothingToDo` 是常态，静默。
pub fn log_env_settings_outcome(outcome: &EnvMigrationOutcome, env_path: &Path) {
    match outcome {
        EnvMigrationOutcome::NothingToDo => {}
        EnvMigrationOutcome::DroppedOnly => tracing::warn!(
            path = %env_path.display(),
            "the .env file held only retired keys (YI_AGENT_WORKDIR / YI_AGENT_YOLO); \
             it was renamed to .env.migrated"
        ),
        EnvMigrationOutcome::Migrated { fields, bocha } => tracing::warn!(
            path = %env_path.display(),
            fields = *fields,
            bocha = *bocha,
            "migrated runtime settings from .env into settings.json; .env was renamed to .env.migrated"
        ),
        EnvMigrationOutcome::EnvUnreadable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate .env runtime settings; nothing was changed"
        ),
        EnvMigrationOutcome::SettingsNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate .env runtime settings; .env was left untouched, will retry next start"
        ),
    }
}
```

`yi-agent-runtime` 已经依赖 `yi-agent-tools`（Cargo.toml 有该依赖），无需新增。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib migrate:: 2>&1 | tail -25
```

Expected: 全部通过。若 `an_invalid_value_is_skipped_but_the_rest_still_migrates` 失败，检查 `write_to` 是否在 `InvalidSetting` 时保持了零写入——它必须如此（Task 4 已有测试钉住）。

- [ ] **Step 5: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-runtime/src/migrate.rs
git commit -m "feat(runtime): migrate .env runtime settings into settings.json

只填充尚未设置的字段，绝不覆盖已有值。BOCHA_API_KEY 优先进 websearch.json
（那里语义更合适且已热读）。全部成功后把 .env 改名 .env.migrated 而不是删除：
那里面是用户唯一一份明文 key。YI_AGENT_WORKDIR / YI_AGENT_YOLO 丢弃并告警。"
```

---

## Task 7: 把两个迁移挂到同一个启动函数

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2348-2356`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:55-70`（`web` 分支）、`main.rs:1294-1310`（`run_agent`）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`#[cfg(test)]`）、`yi-agent-rs/crates/yi-agent/src/main.rs`

**Interfaces:**
- Consumes: `migrate::{migrate_env_settings, log_env_settings_outcome}`（Task 6）
- Produces: `migrate_legacy_model_keys_on_startup()` 的语义扩展——它现在跑两条迁移。**函数名保持不变**，避免四个调用点全部改动。

**为什么集中在 `migrate_legacy_model_keys_on_startup`：** 它有四个调用点（`server.rs:1959`、`server.rs:2047`、`ws.rs:64`，加 CLI 的两个独立位置）。挂进去就自动覆盖全部入口，包括 `app-server --listen ws://`（桌面 relay 与手机配对走这条路）。漏掉它会在手机上表现为「配置丢失」。

- [ ] **Step 1: 写失败的测试**

在 `server.rs` 的测试模块追加：

```rust
    /// 两条迁移都必须挂在同一个生产启动函数里。逐条断言，漏一条就会让某个
    /// 入口（典型是 ws）静默不迁移。
    #[test]
    fn the_startup_migration_runs_both_model_and_settings_migrations() {
        let source = include_str!("server.rs");
        let start = source
            .find("pub(crate) fn migrate_legacy_model_keys_on_startup()")
            .expect("the startup migration function must exist");
        let body = &source[start..];
        let body = body
            .split("\n}\n")
            .next()
            .expect("the function body must be terminated");
        for call in [
            "yi_agent_runtime::migrate::migrate_model_keys",
            "yi_agent_runtime::migrate::migrate_env_settings",
        ] {
            assert!(
                body.contains(call),
                "`migrate_legacy_model_keys_on_startup` must call `{call}`"
            );
        }
    }

    /// ws 入口必须走同一个启动函数——手机配对的配置迁移全靠这一条。
    #[test]
    fn the_ws_entry_runs_the_startup_migration() {
        let ws = include_str!("ws.rs");
        let production = ws.split("#[cfg(test)]").next().expect("production part");
        assert!(
            production.contains("migrate_legacy_model_keys_on_startup"),
            "ws.rs must run the shared startup migration"
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_startup_migration_runs_both 2>&1 | tail -12
```

Expected: FAIL，`must call yi_agent_runtime::migrate::migrate_env_settings`。

- [ ] **Step 3: 扩展启动函数**

替换 `server.rs:2348` 起的那段：

```rust
/// 生产启动期的一次性迁移，供所有生产入口共用。
///
/// **两条迁移，固定顺序，一次跑完**：
/// 1. 模型键 `.env → models.json`（既有）；
/// 2. 运行参数 `.env → settings.json`（本 spec 新增）。
///
/// 顺序不能反：两条之间没有数据依赖，但把它们放在同一个函数里，是为了让
/// 「所有入口都迁移」成为结构性保证——四个调用点（`run`、`serve_stdio`、ws、
/// CLI 两处）漏掉任何一个，都会表现为「某个客户端配置丢失」。
///
/// **函数名保留 `model_keys`**：它现在是历史名字，改名要动四个调用点且不增加
/// 正确性。名字与实际职责的偏差写在这里，比散落四处更可控。
///
/// 只在生产入口调用：测试用 `Harness` 注入隔离路径，绝不能在这里产生副作用。
/// `HOME` 缺失时无从解析实例目录，直接跳过（避免在凭空拼出的路径上写文件）。
pub(crate) fn migrate_legacy_model_keys_on_startup() {
    let Some(env_path) = yi_agent_runtime::config::resolve_global_env_path() else {
        return;
    };
    let models_path = yi_agent_runtime::models::models_path();
    let settings_path = yi_agent_runtime::settings::settings_path();
    let websearch_path = yi_agent_tools::websearch_path();

    let outcome = yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);
    yi_agent_runtime::migrate::log_outcome(&outcome, &env_path);

    let outcome = yi_agent_runtime::migrate::migrate_env_settings(
        &env_path,
        &settings_path,
        &websearch_path,
    );
    yi_agent_runtime::migrate::log_env_settings_outcome(&outcome, &env_path);
}
```

CLI 的两处（`main.rs:55-70` 与 `main.rs:1294-1310`）也各加一次 `migrate_env_settings` 调用。它们没有经过 app-server 的函数，必须各自补：

```rust
                let outcome = yi_agent_runtime::migrate::migrate_env_settings(
                    &env_path,
                    &yi_agent_runtime::settings::settings_path(),
                    &yi_agent_tools::websearch_path(),
                );
                yi_agent_runtime::migrate::log_env_settings_outcome(&outcome, &env_path);
```

- [ ] **Step 4: 写一条启动序测试**

在 `yi-agent-rs/crates/yi-agent/src/main.rs` 的测试模块追加：

```rust
    /// `migrate_env_settings` 的调用点必须在 `config::load_lenient` 之前。
    /// 反过来会让 `.env` 先被注入进程环境、随后又被改名——配置看起来迁了，
    /// 重启后消失。与 `only_the_lenient_loader_exists` 同一手法：静态扫源码。
    #[test]
    fn the_env_migration_runs_before_config_loading() {
        let source = include_str!("main.rs");
        for (migration, load) in [
            ("migrate::migrate_env_settings(", "run_agent("),
            ("migrate_model_keys(", "run_agent("),
        ] {
            let migration_at = source
                .find(migration)
                .unwrap_or_else(|| panic!("{migration} must be called from main.rs"));
            let _ = (migration_at, load);
        }
        // run_agent 内部：迁移在前，加载在后。
        let run_agent_at = source.find("fn run_agent(").expect("run_agent must exist");
        let body = &source[run_agent_at..];
        let body = body.split("\nfn ").next().expect("body terminates");
        let migration_at = body
            .find("migrate::migrate_env_settings(")
            .expect("run_agent must migrate before loading config");
        let load_at = body
            .find("config::load_lenient(")
            .expect("run_agent must load config");
        assert!(
            migration_at < load_at,
            "the .env migration must run before the config is loaded, otherwise values get \
             injected into the process env and then lost on the next start"
        );
    }
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib the_startup_migration the_ws_entry 2>&1 | tail -12
cd yi-agent-rs && cargo test -p yi-agent --lib the_env_migration_runs_before 2>&1 | tail -12
```

Expected: 全部 passed。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat: run both startup migrations from one shared function

两个迁移挂在同一个函数里，四个调用点自动覆盖——尤其是 ws 入口，它走的是
桌面 relay 与手机配对，漏掉会表现为「桌面正常、手机配置丢失」。CLI 的两个
独立入口各自补一次，并加静态扫描测试钉住「迁移早于配置加载」这个顺序。"
```

---

## Task 8: 删掉 onboarding 的 `.env` 兜底层

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/onboarding_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/onboarding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:55`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs:355-365`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（字段定义 + 6 处装配）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs:151`
- Modify: `yi-agent-rs/crates/yi-agent/src/onboarding_prompt.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:1296-1310`
- Test: 上述各文件的 `#[cfg(test)]`、`yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Produces: `pub fn assess(catalog: Option<&ModelCatalog>) -> Assessment`
- 删除：`EnvModelFields`、`read_env_fields`、`EnvModelFields` 所有字段
- 删除：`ReadySource::Env`（`ReadySource` 若只剩 `Catalog` 一个变体，改成 `pub enum Assessment { Ready, Needed { reasons: Vec<Missing> } }`）
- Produces: `pub fn should_offer(preferences_path: &Path) -> bool`
- 删除：`AppState.onboarding_env_path`、`RuntimeAttachments.onboarding_env_path`
- Produces: `handle_onboarding_request_at(preferences_path, models_path, method, params, fallback)`

**背景：** `.env` 改名后 `read_env_fields` 永远返回全空，于是 `onboarding/status` 恒报 `needed`、`should_offer` 一直弹引导、`missing_model_error` 恒返回四项全缺。`onboarding/apply` 本就只写 `models.json`（形参名就是 `_env_path`，有测试钉着），所以这一层删掉不丢数据。

- [ ] **Step 1: 改写 `onboarding.rs` 的测试**

先改测试再改实现。在 `onboarding.rs` 的 `tests` 里，把以 `EnvModelFields` 为输入的测试全部改写。以下是**完整替换**后的测试集（原 20 条里保留判定语义、去掉 env 输入）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ModelCatalog, ModelEntry, ModelProvider};

    fn catalog_with(api_key: &str, model: &str) -> ModelCatalog {
        let mut catalog = ModelCatalog::default();
        catalog.models.push(ModelEntry {
            name: "entry".to_string(),
            provider: ModelProvider::Openai,
            api_url: "https://api.openai.com/v1".to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        });
        catalog.default_model = Some("entry".to_string());
        catalog
    }

    #[test]
    fn a_usable_default_entry_is_ready() {
        let catalog = catalog_with("sk-x", "gpt-4o");
        assert_eq!(assess(Some(&catalog)), Assessment::Ready);
    }

    #[test]
    fn a_default_entry_without_a_key_is_needed_with_only_the_key_missing() {
        let catalog = catalog_with("", "gpt-4o");
        assert_eq!(
            assess(Some(&catalog)),
            Assessment::Needed {
                reasons: vec![Missing::ApiKey]
            },
            "a catalog-only machine must report exactly what is missing, not all four"
        );
    }

    #[test]
    fn a_default_entry_without_a_model_is_needed_with_only_the_model_missing() {
        let catalog = catalog_with("sk-x", "");
        assert_eq!(
            assess(Some(&catalog)),
            Assessment::Needed {
                reasons: vec![Missing::Model]
            }
        );
    }

    #[test]
    fn an_empty_catalog_is_needed() {
        assert_eq!(
            assess(Some(&ModelCatalog::default())),
            Assessment::Needed {
                reasons: vec![Missing::Provider, Missing::Model, Missing::ApiKey]
            }
        );
    }

    #[test]
    fn no_catalog_is_needed() {
        // CLI 传 `None` 表示「不读清单」，此时无任何可用配置。
        assert_eq!(
            assess(None),
            Assessment::Needed {
                reasons: vec![Missing::Provider, Missing::Model, Missing::ApiKey]
            }
        );
    }

    #[test]
    fn is_valid_api_url_accepts_http_and_https() {
        assert!(is_valid_api_url("https://api.example.com"));
        assert!(is_valid_api_url("http://localhost:8080/v1"));
        assert!(!is_valid_api_url("ftp://example.com"));
        assert!(!is_valid_api_url("api.example.com"));
    }

    #[test]
    fn dismissed_flag_round_trips_and_preserves_other_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, r#"{"theme":"light"}"#).unwrap();

        assert!(!load_dismissed(&path));
        save_dismissed(&path, true).unwrap();
        assert!(load_dismissed(&path));

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["theme"], "light", "other keys must survive");
    }

    #[test]
    fn a_corrupt_preferences_file_is_not_dismissed() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("preferences.json");
        std::fs::write(&path, "{ broken").unwrap();
        assert!(!load_dismissed(&path));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding:: 2>&1 | tail -20
```

Expected: 编译失败（`assess` 仍要两个参数、`Assessment::Ready` 仍带 `source`）。

- [ ] **Step 3: 改 `assess` 与类型**

替换 `onboarding.rs` 的这几个定义：

```rust
//! 首次安装引导的共享逻辑：就绪判定、连接测试、结束标记。
//!
//! CLI（TUI 首启问答）与 app-server（桌面向导）共用本模块，避免两处各写一份
//! 判定与写入。**模型配置的唯一真相源是 `models.json`**——过去还有一层「读
//! `.env` 兜底」，那条路径已随后者的下线一并删除：留着它只会让 `.env` 改名后
//! 判定永远为「未就绪」。

use crate::models::{ModelCatalog, effective_entry};
```

```rust
/// 导致「未就绪」的缺失项，用于向导首屏说清到底缺什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Provider,
    Model,
    ApiUrl,
    ApiKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Assessment {
    Ready,
    Needed { reasons: Vec<Missing> },
}

/// 判定当前配置是否足以跑起一次对话。
///
/// 唯一判据是清单的默认条目：能解析出条目、且 `api_key` 与 `model` 都非空 →
/// `Ready`；否则 `Needed` 并逐个列出该条目缺哪几项。
///
/// `catalog: None` 表示调用方不读清单（CLI 的某些路径），等价于「空清单」。
/// 原因列表与清单状态同源——过去 `.env` 兜底层会让「只缺 key」被报成「四项
/// 全缺」，因为兜底层一空就什么都不知道了。
pub fn assess(catalog: Option<&ModelCatalog>) -> Assessment {
    let entry = catalog.and_then(|catalog| effective_entry(catalog, None));
    let mut reasons = Vec::new();
    match entry {
        Some(entry) => {
            if crate::models::ModelProvider::parse(entry.provider.as_str()).is_none() {
                reasons.push(Missing::Provider);
            }
            if entry.model.trim().is_empty() {
                reasons.push(Missing::Model);
            }
            if entry.api_key.trim().is_empty() {
                reasons.push(Missing::ApiKey);
            }
            if !entry.api_url.trim().is_empty() && !is_valid_api_url(entry.api_url.trim()) {
                reasons.push(Missing::ApiUrl);
            }
        }
        None => {
            // 没有默认条目 = 什么都缺。
            reasons.extend([Missing::Provider, Missing::Model, Missing::ApiKey]);
        }
    }
    if reasons.is_empty() {
        Assessment::Ready
    } else {
        Assessment::Needed { reasons }
    }
}
```

删除 `EnvModelFields` 结构体、`ReadySource` 枚举、`read_env_fields` 函数、`strip_quotes` 函数（后两者在 `read_env_fields` 里用到）。`strip_quotes` 若只被 `read_env_fields` 用，一并删除；`parse_env_line` 的口径已在 `settings.rs`。

- [ ] **Step 4: 改 `should_offer`**

`yi-agent/src/onboarding_prompt.rs:15`：

```rust
/// 是否应自动弹引导：未就绪且用户尚未结束过引导。
///
/// 就绪判定只看**清单**（模型设置的唯一真相源）：清单里已有可用默认条目就不再弹。
pub(crate) fn should_offer(preferences_path: &Path) -> bool {
    use yi_agent_runtime::onboarding::{Assessment, assess, load_dismissed};
    if load_dismissed(preferences_path) {
        return false;
    }
    let catalog =
        yi_agent_runtime::models::load_catalog_from(&yi_agent_runtime::models::models_path());
    matches!(assess(Some(&catalog)), Assessment::Needed { .. })
}
```

- [ ] **Step 5: 改三个调用点**

`onboarding_rpc.rs:17-24`（`onboarding/status` 分支）。**整段**替换，注意 `read_env_fields` 那一句必须一起删——`env_path` 形参随后在 Step 6 被去掉，留着这一句会编译不过：

```rust
        "onboarding/status" => {
            let catalog = yi_agent_runtime::models::load_catalog_from(models_path);
            let assessment = yi_agent_runtime::onboarding::assess(Some(&catalog));
            let dismissed = yi_agent_runtime::onboarding::load_dismissed(preferences_path);
            let (needed, reasons) = match assessment {
                yi_agent_runtime::onboarding::Assessment::Ready => (false, Vec::new()),
                yi_agent_runtime::onboarding::Assessment::Needed { reasons } => (
                    true,
                    reasons
                        .into_iter()
                        .map(missing_name)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                ),
            };
            Ok(json!({ "needed": needed, "dismissed": dismissed, "reasons": reasons }))
        }
```

**响应体不变**：现在返回的就是 `{"needed", "dismissed", "reasons"}` 三个键，没有 `source`（已核实现状）。`Assessment::Ready { source }` 改成裸 `Ready` 后这里只是把 `Ready { .. }` 的 `{ .. }` 去掉，JSON 形状零变化，**桌面端与 web 端无需改动**。

`rpc/thread.rs:55`：

```rust
    // 原因列表留给 UI 展示「还差哪几项」，与 `onboarding/status` 同源。
    let reasons = match yi_agent_runtime::onboarding::assess(Some(&catalog)) {
        yi_agent_runtime::onboarding::Assessment::Needed { reasons } => reasons
            .into_iter()
            .map(|m| crate::onboarding_rpc::missing_name(m).to_string())
            .collect(),
        yi_agent_runtime::onboarding::Assessment::Ready => Vec::new(),
    };
```

`main.rs:1309`：

```rust
    if onboarding_prompt::should_offer(&prefs_path) {
```

- [ ] **Step 6: 删掉 `onboarding_env_path` 全链**

按顺序改：

1. `rpc/mod.rs`：删 `pub(crate) onboarding_env_path: &'a Path,` 及其 doc 注释。
2. `server.rs:417`：删 `RuntimeAttachments.onboarding_env_path` 字段定义（连同 doc）。
3. `server.rs:1979`、`server.rs:2068`、`ws.rs:151`：删装配行。
4. `server.rs:3457`：从 `app_state!` 宏里删 `onboarding_env_path: &onboarding_env_path,`。
5. `server.rs:3161` 附近：删解构该字段的那一行。
6. `onboarding_rpc.rs`：`handle_onboarding_request_at` 去掉第一个形参 `env_path`，`apply` 的 `_env_path` 形参一并删除。
7. `rpc/onboarding.rs`：三处 `state.onboarding_env_path` 实参删除。
8. `rpc_tests.rs`：删 **5 处** `onboarding_env_path`（实际行号 2384、4115、4200、4287、17767）。其中 2384 是 `Harness::new` 里的真实装配，值为 `index_dir.path().join(".env")`；其余四处是 `PathBuf::new()` 占位。删之前先 `grep -n "onboarding_env_path" crates/yi-agent-app-server/src/rpc_tests.rs` 确认全部命中，别漏 17767 那处（它在文件末尾，属另一个测试块，容易漏）。

**注意 `app_state!` 宏的注释**：它特意说明「`models_path` / `onboarding_env_path` / `onboarding_preferences_path` / `watchman_home` 四个相邻字段同为 `&Path`，位置写错会静默接错」。删掉其中一个后要把注释里的数字与列表改掉——现在就剩三个。

- [ ] **Step 7: 加一条「兜底层已删」的测试**

在 `onboarding.rs` 的测试里加：

```rust
    /// 钉住「`.env` 兜底层已删」：把一份写满四个模型键的 `.env` 放在实例目录里，
    /// 判定依然只认清单。将来有人把 Env 分支加回来，这条会红。
    #[test]
    fn an_env_file_with_a_full_model_config_does_not_make_it_ready() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".env"),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\nMODEL_API_URL=https://api.openai.com/v1\n",
        )
        .unwrap();
        std::fs::write(home.join(".env.migrated"), "YI_AGENT_MODEL=gpt-4o\n").unwrap();

        // 清单为空 → 未就绪，与 .env 里写了什么无关。
        let catalog = ModelCatalog::default();
        assert!(matches!(
            assess(Some(&catalog)),
            Assessment::Needed { .. }
        ));
    }
```

- [ ] **Step 8: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib onboarding:: 2>&1 | tail -20
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib 2>&1 | tail -20
cd yi-agent-rs && cargo build -p yi-agent 2>&1 | tail -10
```

Expected: 全部通过、无编译错误。

- [ ] **Step 9: 加一条 `should_offer` 的测试**

在 `yi-agent/src/onboarding_prompt.rs` 的测试模块（已有两条，改签名）加：

```rust
    /// `.env.migrated` 的存在不影响判定：判定依据是 catalog。
    #[test]
    fn a_migrated_env_file_does_not_suppress_the_prompt() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".env.migrated"), "MODEL_API_KEY=sk-x\n").unwrap();
        let prefs = dir.path().join("preferences.json");

        // 清单为空时应当弹；这里的 catalog 来自 `models_path()`，测试里用
        // `YI_AGENT_HOME` 指向隔离目录即可保证为空。
        std::env::set_var("YI_AGENT_HOME", &home);
        let offered = should_offer(&prefs);
        std::env::remove_var("YI_AGENT_HOME");
        assert!(offered, "an empty catalog must still offer onboarding");
    }
```

**注意**：这条测试改进程环境变量，必须与 `should_offer` 现有的测试串行（该文件若已有 mutex 就复用；没有则在测试模块加 `static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());` 并在三条测试开头 `let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());`）。

- [ ] **Step 10: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add -A yi-agent-rs/crates/yi-agent-runtime/src/onboarding.rs yi-agent-rs/crates/yi-agent-app-server/src yi-agent-rs/crates/yi-agent/src
git commit -m "refactor: drop the .env fallback layer from onboarding

.env 改名后 read_env_fields 永远返回全空，于是 onboarding/status 恒报
needed、should_offer 一直弹引导、missing_model_error 恒返回四项全缺。
判定收敛到 models.json 单一来源；onboarding/apply 本就只写 models.json
（形参名是 _env_path，有测试钉着），所以删掉这一层不丢用户数据。

顺带修好一个既有缺陷：清单有默认条目但 key 为空时，过去报「四项全缺」，
现在只报缺 api_key。"
```

---

## Task 9: 删除 `BOCHA_API_KEY` 的环境变量回退

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/web/config.rs:94-101`
- Test: `yi-agent-rs/crates/yi-agent-tools/src/web/config.rs`

**Interfaces:**
- 改变语义：`EngineConfig::resolved_api_key()` 从「文件优先、env 回退」改为「只看文件」

**背景：** 这层回退的唯一价值是「老用户不改配置也能继续用」，前提是那个 env 值来自 `.env`。`.env` 不再加载后，回退在下次启动时恒为 `None`，制造「当前进程能用、重启静默失效」的难查故障。

- [ ] **Step 1: 写失败的测试**

在 `web/config.rs` 的 `tests` 模块追加：

```rust
    /// `BOCHA_API_KEY` 不再是回退层：文件里没有 key 就是没配，哪怕进程环境里有。
    /// 这一条钉住「不再读 env」——`.env` 下线后那个 env 值在重启时必然消失，
    /// 留着回退会造出「当前能用、重启失效」的故障。
    #[test]
    fn resolved_api_key_does_not_fall_back_to_the_environment() {
        let mut engine = EngineConfig::default();
        engine.api_key = String::new();
        // env 里有值也不能被采用。
        std::env::set_var("BOCHA_API_KEY", "from-env");
        let resolved = engine.resolved_api_key();
        std::env::remove_var("BOCHA_API_KEY");
        assert_eq!(resolved, None, "the env fallback must be gone");
    }

    #[test]
    fn resolved_api_key_reads_the_file_value() {
        let mut engine = EngineConfig::default();
        engine.api_key = "from-file".to_string();
        assert_eq!(engine.resolved_api_key().as_deref(), Some("from-file"));
    }
```

**注意**：这条测试改进程环境变量。`yi-agent-tools` 里若有其他测试也读 `BOCHA_API_KEY`，需要用互斥锁串行。跑前先 `grep -n "BOCHA_API_KEY" crates/yi-agent-tools/src/web/config.rs` 确认；若有冲突，加 `static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());`。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --lib resolved_api_key_does_not_fall_back 2>&1 | tail -12
```

Expected: FAIL，`left: Some("from-env")`。

- [ ] **Step 3: 删掉回退**

```rust
    /// 生效的 key：只认文件里的值。
    ///
    /// 过去这里还有一层 `std::env::var("BOCHA_API_KEY")` 回退，目的是「老用户
    /// 不改配置也能继续用」。`.env` 下线后那个 env 值在下次启动时必然为 `None`，
    /// 回退只会造出「当前进程能用、重启后静默失效」的难查故障。迁移已把旧 key
    /// 写进 `websearch.json`，故文件是唯一来源。
    pub fn resolved_api_key(&self) -> Option<String> {
        if self.api_key.is_empty() {
            None
        } else {
            Some(self.api_key.clone())
        }
    }
```

- [ ] **Step 4: 更新调用方注释**

`yi-agent-app-server/src/websearch_rpc.rs:57` 的注释与 `yi-agent-tools/src/web/config.rs:4` 的模块注释都提到「环境变量兜底」，改成「只认文件」。`has_key` 的语义（`:58`）变成「文件里有没有 key」——这正是用户想要的答案，无需改代码。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-tools --lib 2>&1 | tail -15
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib websearch 2>&1 | tail -15
```

Expected: 全部通过。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-tools/src/web/config.rs yi-agent-rs/crates/yi-agent-app-server/src/websearch_rpc.rs
git commit -m "fix(tools): drop the BOCHA_API_KEY environment fallback

.env 不再加载后，这层回退在重启时恒为 None，会造出「当前进程搜索可用、
重启后静默失效」的故障。迁移已把旧 key 写进 websearch.json，文件是唯一来源。"
```

---

## Task 10: `settings/read` 与 `settings/write` RPC

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/settings_rpc.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/ui.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs`（`AppState` 加 `settings_path`、`domain_for` 加 `settings`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（装配 `settings_path`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/ws.rs`（同上）
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: `settings::{load_from, write_to, FIELDS, FieldType, SettingsError}`（Task 2、4）
- Produces: `settings/read` → `{"fields":[{"key","group","label","type","value","default","description","options"}],"path":"..."}`
- Produces: `settings/write` params `{"patch":{"agent.max_turns":500}}` → `{"ok":true}` 或 `invalid_setting` 错误

- [ ] **Step 1: 写失败的测试**

先写一条**经真实 server 走通**的集成测试（放在 `rpc_tests.rs`，用现有 harness），它同时验证 `domain_for`、`AppState.settings_path` 注入与 handler 接线：

```rust
    /// `settings/read` 经真实 dispatch 路径可达，且未设置的字段返回 null。
    #[test]
    fn settings_read_over_rpc_reports_unset_fields_as_null() {
        let dir = tempfile::TempDir::new().unwrap();
        let settings_path = dir.path().join("settings.json");
        std::fs::write(&settings_path, r#"{"agent":{"max_turns":500}}"#).unwrap();

        let h = Harness::new_with_settings_path(&settings_path); // 见 Step 3 的装配说明
        let reply = h.request(r#"{"jsonrpc":"2.0","id":1,"method":"settings/read","params":{}}"#);

        let fields = reply["result"]["fields"].as_array().unwrap();
        let max_turns = fields.iter().find(|f| f["key"] == "agent.max_turns").unwrap();
        assert_eq!(max_turns["value"], 500);
        assert_eq!(max_turns["default"], "200");

        let ratio = fields.iter().find(|f| f["key"] == "compact.ratio").unwrap();
        assert!(
            ratio["value"].is_null(),
            "an unset field must be null, not its default and not an empty string"
        );
        assert_eq!(ratio["default"], "80");
    }
```

> 若 harness 没有现成的 `request` 辅助，就用 `ui/settings/read` 现有测试的同款写法（`h.send(...)` + 读一行响应）。`Harness` 的路径注入见 Step 3。

纯函数级的测试（`read_value` / `write_value`）写在 `settings_rpc.rs` 自己的 `mod tests` 里（见 Step 4），不要放进 `rpc_tests.rs`——那里没有 `settings_rpc::` 的 import，写 `settings::read_value` 会指向 `yi_agent_runtime::settings`，而那里没有这两个函数。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib settings_read_reports_null 2>&1 | tail -12
```

Expected: 编译失败，`cannot find module settings`。

- [ ] **Step 3: 实现 handler**

`settings_rpc.rs`：

```rust
//! `settings/read` 与 `settings/write`：`settings.json` 的 RPC 外壳。
//!
//! 逻辑全在 `yi_agent_runtime::settings`。本模块只做两件事：把类型化的
//! `Settings` 摊成前端能渲染的 JSON 数组，以及把 `SettingsError` 翻成 RPC 错误。
//! **校验规则只在这里调用一次**（`write_to` 内部），不另写一份——多一份就是
//! 多一处迟早漂移的规则。

use std::path::Path;

use serde_json::{json, Value};
use yi_agent_runtime::settings::{self, FieldType, FieldSpec, SettingsError};

use crate::protocol::{RpcError, Scope};

/// `settings/read` 的响应体。
///
/// `value` 为 `null` 表示**未设置**——前端必须据此显示「默认 X」并禁用「恢复默认」
/// 按钮。不要把 `null` 归一成 `default`：那正是旧 `get_config` 的做法，它让
/// 「保存一次后所有字段都显示成已设置」。
pub fn read_value(path: &Path) -> Value {
    let current = settings::load_from(path);
    let fields: Vec<Value> = settings::FIELDS
        .iter()
        .map(|spec| {
            json!({
                "key": spec.key,
                "group": spec.group,
                "label": spec.label,
                "type": type_name(spec.field_type),
                "value": current_value(&current, spec),
                "default": spec.default_display,
                "description": spec.description,
                "options": spec.options,
                "min": spec.min,
                "max": spec.max,
            })
        })
        .collect();
    json!({
        "fields": fields,
        "path": path.display().to_string(),
    })
}

/// 从已加载的 `Settings` 里取某个字段的 JSON 值；`None` → `null`。
fn current_value(current: &settings::Settings, spec: &FieldSpec) -> Value {
    match spec.key {
        "agent.max_turns" => option_to_json(current.agent.max_turns),
        "agent.max_direct_children" => option_to_json(current.agent.max_direct_children),
        "agent.max_resident_subagents" => option_to_json(current.agent.max_resident_subagents),
        "agent.system_prompt" => option_to_json(current.agent.system_prompt.as_deref()),
        "agent.skills_catalog_budget" => option_to_json(current.agent.skills_catalog_budget),
        "compact.ratio" => option_to_json(current.compact.ratio),
        "compact.user_budget_tokens" => option_to_json(current.compact.user_budget_tokens),
        "compact.tool_budget_tokens" => option_to_json(current.compact.tool_budget_tokens),
        "startup.sandbox" => option_to_json(current.startup.sandbox.as_deref()),
        // `FIELDS` 与这里的 match 必须同步。漏一个就会静默返回 null，
        // 故有一条测试遍历 FIELDS 断言每个键都能取到值。
        _ => Value::Null,
    }
}

fn option_to_json<T: Into<Value>>(value: Option<T>) -> Value {
    value.map(Into::into).unwrap_or(Value::Null)
}

fn type_name(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::Number => "number",
        FieldType::Text => "text",
        FieldType::Select => "select",
    }
}

/// `settings/write` 的响应。校验失败时零写入。
pub fn write_value(path: &Path, patch: &Value) -> Result<Value, SettingsError> {
    settings::write_to(path, patch)?;
    Ok(json!({"ok": true}))
}

/// 把 `SettingsError` 翻成 RPC 错误。`invalid_setting` 是稳定词汇，UI 据此
/// 在对应输入框旁内联提示，而不是弹一个整页错误。
pub fn write_error(error: SettingsError) -> RpcError {
    match error {
        SettingsError::InvalidSetting { key, reason } => RpcError {
            code: -32602,
            message: format!("invalid setting {key}: {reason}"),
            data: Some(json!({ "code": "invalid_setting", "key": key, "reason": reason })),
        },
        SettingsError::Corrupt(reason) => RpcError {
            code: -32603,
            message: format!("settings.json is not writable: {reason}"),
            data: Some(json!({ "code": "settings_corrupt" })),
        },
        SettingsError::Io(reason) => RpcError::internal(reason),
    }
}

/// `settings/write` 需要 `Control`：它改的是本机的长期运行参数。
pub fn write_scope() -> Scope {
    Scope::Control
}
```

在 `rpc/ui.rs` 里加两个分支（放在 `ui/settings/write` 之后）。注意两个分支要**写在 `Domain::Ui` 的 dispatch arm 里**，而 `settings/*` 的域由 `domain_for` 映射成 `Domain::Ui`（见本 Step 末），所以不需要新开域臂。

```rust
        "settings/read" => {
            let value = crate::settings_rpc::read_value(state.settings_path);
            write_response(state.hub, state.client, ok_response(id, value)).await?;
        }
        "settings/write" => {
            if state.client_scope < crate::settings_rpc::write_scope() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::insufficient_scope(crate::settings_rpc::write_scope()),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let patch = req.params.get("patch").cloned().unwrap_or(Value::Null);
            match crate::settings_rpc::write_value(state.settings_path, &patch) {
                Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
                Err(error) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, crate::settings_rpc::write_error(error)),
                    )
                    .await?
                }
            }
        }
```

在 `rpc/mod.rs` 加字段与路由：

```rust
    /// `settings.json` 路径。可注入，让测试不必改 `HOME`。
    pub(crate) settings_path: &'a Path,
```

```rust
        Some("ui") | Some("config") | Some("settings") => Domain::Ui,
```

在 `server.rs` 与 `ws.rs` 的 `app_state!` / `RuntimeAttachments` 里各加一处 `settings_path`（生产值 `yi_agent_runtime::settings::settings_path()`，注意它返回 `PathBuf`，需在装配处 clone 或提前声明变量——参照 `websearch_path` 的写法）。

**必须同时改的构造点（漏一处就编译不过）：**

`AppState` 的定义里只有宏内**一处**构造（`server.rs:3451` 的 `app_state!()`），但 `RuntimeAttachments`（`server.rs:396` 起，承载 `models_path` / `websearch_path` 这类可注入路径）在测试里有 **5 处**手动构造：

```bash
cd yi-agent-rs && grep -n "websearch_path:" crates/yi-agent-app-server/src/rpc_tests.rs
```

当前命中 `2378`、`4113`、`4198`、`4285`、`17765`。逐个补 `settings_path`：
- `2378` 是 `Harness::new` 里的装配，用 `models_dir.path().join("settings.json")`（与同行 `websearch_path` 同一个 tempdir），这样 `settings/read|write` 的集成测试才能断言落盘。
- 其余四处是 `PathBuf::new()` 占位（它们测的是与 settings 无关的路径），照抄成 `settings_path: PathBuf::new(),`。

**不要**给这些测试传 `Path::new("")` 又让它们触发写入——空路径 join 出的是相对路径，`write_to` 会在当前工作目录建出文件。只读用例用空路径没问题，写用例必须用 tempdir。

在 `server.rs` 的 `app_state!` 宏里加：

```rust
                            settings_path: &settings_path,
```

并在 `serve()` 的局部量里声明 `let settings_path = settings_path.clone();`（`settings_path` 来自 `RuntimeAttachments` 的字段，放在 `websearch_path` 的 destructure 旁边）。

`ws.rs` 同理：它有自己的装配路径（`ws.rs:64` 附近），照 `websearch_path` 的处理方式接上。

- [ ] **Step 4: 补 `FIELDS` 全覆盖测试**

在 `settings_rpc.rs` 里加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// `FIELDS` 与 `current_value` 的 match 必须同步：漏一个键会让它静默
    /// 返回 null，前端显示「未设置」而文件里其实有值。
    #[test]
    fn every_field_in_fields_can_be_read_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        // 给每个可持久化字段都写一个合法值。
        let mut patch = serde_json::Map::new();
        for spec in settings::FIELDS {
            let value = match spec.field_type {
                FieldType::Number => json!(spec.min.unwrap_or(1)),
                FieldType::Text => json!("set"),
                FieldType::Select => json!(spec.options[0]),
            };
            patch.insert(spec.key.to_string(), value);
        }
        settings::write_to(&path, &Value::Object(patch)).unwrap();

        let current = settings::load_from(&path);
        for spec in settings::FIELDS {
            assert_ne!(
                current_value(&current, spec),
                Value::Null,
                "{} is in FIELDS but current_value returned null for it",
                spec.key
            );
        }
    }

    #[test]
    fn read_reports_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let value = read_value(&path);
        assert_eq!(value["path"], path.display().to_string());
    }
}
```

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib settings 2>&1 | tail -20
```

Expected: 全部通过。

- [ ] **Step 6: 补 `domain_for` 回归表**

在 `rpc_tests.rs:3647` 附近那串 `("method", Domain::...)` 里加：

```rust
        ("settings/read", Domain::Ui),
        ("settings/write", Domain::Ui),
```

并**不加** `settings/test`——spec 已砍掉它。

- [ ] **Step 7: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "feat(app-server): add settings/read and settings/write RPCs

settings/read 每个字段都带 value 与 default：value 为 null 才是「未设置」，
不归一成默认值——那正是旧 get_config 让「保存一次后全部显示已设置」的原因。
settings/write 校验失败零写入，错误码 invalid_setting 带具体字段供 UI 内联提示。
不提供 settings/test：校验规则只存在于 write 一处。"
```

---

## Task 11: `yi-agent web` 换成 `settings.json` 后端

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-web/Cargo.toml`（`yi-agent-runtime` 从 dev 提到正式依赖）
- Modify: `yi-agent-rs/crates/yi-agent-web/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/api.rs`
- Delete: `yi-agent-rs/crates/yi-agent-web/src/config_meta.rs`
- Delete: `yi-agent-rs/crates/yi-agent-web/src/env_file.rs`
- Modify: `yi-agent-rs/crates/yi-agent-web/src/assets/index.html`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:55-70`（`web` 子命令传参）
- Test: `yi-agent-rs/crates/yi-agent-web/tests/api_test.rs`

**Interfaces:**
- Consumes: `settings::{FIELDS, FieldType, load_from, write_to, SettingsError}`
- `GET /api/config` → `{"groups":[{"name","vars":[{"key","value","default","type","group","description","options","masked","source"}]}],"path":...}`
- `PUT /api/config` → `{"updates":[{"key","value"}]}`

**关键形状约定**：`source` 保留，改为 `value === null ? "default" : "local"`；`masked` 字段删除（settings.json 不含密钥，`BOCHA_API_KEY` 已归 `websearch.json`）；`envPath` 改名为 `path`。

- [ ] **Step 1: 写失败的测试**

替换 `tests/api_test.rs` 里针对 `get_config` / `put_config` 的测试，新增：

```rust
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn app(settings_path: std::path::PathBuf) -> axum::Router {
    yi_agent_web::router(settings_path)
}

#[tokio::test]
async fn get_config_reports_null_for_unset_and_default_source() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    let all: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();
    let max_turns = all.iter().find(|v| v["key"] == "agent.max_turns").unwrap();
    assert_eq!(max_turns["value"], 500);
    assert_eq!(max_turns["source"], "local");

    let ratio = all.iter().find(|v| v["key"] == "compact.ratio").unwrap();
    assert!(ratio["value"].is_null());
    assert_eq!(ratio["source"], "default");
    assert_eq!(ratio["default"], "80");
}

/// 一号缺陷的回归网：保存一次不能删掉不认识的键。
#[tokio::test]
async fn put_config_preserves_unknown_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"my_note":"keep me"}"#).unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["my_note"], "keep me");
    assert_eq!(value["agent"]["max_turns"], 500);
}

/// 未设置的字段不得被写成空值行。
#[tokio::test]
async fn put_config_does_not_materialize_unset_fields() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let agent = value["agent"].as_object().unwrap();
    assert_eq!(agent.len(), 1);
    assert!(value.get("compact").is_none());
}

#[tokio::test]
async fn put_config_rejects_an_invalid_value_with_400() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"compact.ratio","value":"0"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!path.exists(), "a rejected write must not create the file");
}

/// 「真实 LLM 测试」面板已删除：路由必须是 404。
#[tokio::test]
async fn the_real_llm_test_routes_are_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    for (method, url) in [
        ("GET", "/api/real-llm-test-config"),
        ("PUT", "/api/real-llm-test-config"),
        ("POST", "/api/real-llm-test-config/validate"),
        ("DELETE", "/api/real-llm-test-config/api-key"),
    ] {
        let request = match method {
            "GET" => Request::get(url),
            "PUT" => Request::put(url),
            "POST" => Request::post(url),
            _ => Request::delete(url),
        }
        .body(Body::empty())
        .unwrap();
        let response = app(dir.path().join("settings.json"))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {url} must be gone"
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && cargo test -p yi-agent-web 2>&1 | tail -20
```

Expected: 编译失败，`cannot find function router`。

- [ ] **Step 3: 重写 `lib.rs`**

```rust
//! yi-agent WebUI：编辑 `<instance_home>/settings.json`。
//!
//! 这是一层 HTTP 外壳，**不含任何配置元数据与校验规则**：字段类型、默认值、
//! 取值范围、可选项都来自 `yi_agent_runtime::settings`，与桌面端同源。过去
//! 这里有一份手抄的 `config_meta::ALL_VARS` 和一套 `env_file` 读写，默认值与
//! `config.rs` 漂移过（`YI_AGENT_MAX_TURNS` 20 vs 200），写盘还会删掉不认识的键。

pub mod api;

use std::path::PathBuf;

use anyhow::Result;
use axum::routing::{get, put};

/// 构造路由。抽出来让测试不必绑定端口。
pub fn router(settings_path: PathBuf) -> axum::Router {
    let state = api::AppState { settings_path };
    axum::Router::new()
        .route("/", get(api::index_html))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .with_state(state)
}

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, settings_path: PathBuf) -> Result<()> {
    println!("yi-agent web 配置服务器已启动");
    println!("  地址:     http://{host}:{port}");
    println!("  配置文件: {}", settings_path.display());
    println!("按 Ctrl+C 停止");

    let app = router(settings_path);
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [ ] **Step 4: 重写 `api.rs` 的两个 handler**

```rust
use std::path::PathBuf;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use yi_agent_runtime::settings::{self, FieldType, SettingsError};

/// 共享状态：`settings.json` 路径（唯一）。
#[derive(Clone)]
pub struct AppState {
    pub settings_path: PathBuf,
}

/// GET / — 返回内嵌 HTML 页面
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

/// GET /api/config — 返回所有字段的元数据 + 当前值。
///
/// `value: null` 表示未设置；`source` 是它的派生（`"default"` / `"local"`）。
/// **不要**把 `null` 归一成 `default`：旧实现这么做过，结果是「保存一次后所有
/// 字段都显示成已设置」。
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let current = settings::load_from(&state.settings_path);

    let mut groups: Vec<Value> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for spec in settings::FIELDS {
        if !seen.contains(&spec.group) {
            seen.push(spec.group);
        }
    }
    for group in seen {
        let vars: Vec<Value> = settings::FIELDS
            .iter()
            .filter(|spec| spec.group == group)
            .map(|spec| {
                let value = field_value(&current, spec);
                json!({
                    "key": spec.key,
                    "value": value,
                    "default": spec.default_display,
                    "type": match spec.field_type {
                        FieldType::Number => "number",
                        FieldType::Text => "text",
                        FieldType::Select => "select",
                    },
                    "group": spec.group,
                    "description": spec.description,
                    "options": spec.options,
                    "min": spec.min,
                    "max": spec.max,
                    "source": if value.is_null() { "default" } else { "local" },
                })
            })
            .collect();
        groups.push(json!({ "name": group, "vars": vars }));
    }

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "groups": groups,
            "path": state.settings_path.display().to_string(),
        })),
    )
}

/// 从 `Settings` 取某个字段的 JSON 值（`None` → `null`）。
///
/// 与 `settings_rpc::current_value` 同一个 match 结构。两处都覆盖全部 `FIELDS`，
/// 各有一条测试遍历断言，漏一个键会红。
fn field_value(current: &settings::Settings, spec: &settings::FieldSpec) -> Value {
    let opt = |value: Option<Value>| value.unwrap_or(Value::Null);
    match spec.key {
        "agent.max_turns" => opt(current.agent.max_turns.map(Value::from)),
        "agent.max_direct_children" => opt(current.agent.max_direct_children.map(Value::from)),
        "agent.max_resident_subagents" => {
            opt(current.agent.max_resident_subagents.map(Value::from))
        }
        "agent.system_prompt" => opt(current.agent.system_prompt.clone().map(Value::from)),
        "agent.skills_catalog_budget" => opt(current.agent.skills_catalog_budget.map(Value::from)),
        "compact.ratio" => opt(current.compact.ratio.map(Value::from)),
        "compact.user_budget_tokens" => opt(current.compact.user_budget_tokens.map(Value::from)),
        "compact.tool_budget_tokens" => opt(current.compact.tool_budget_tokens.map(Value::from)),
        "startup.sandbox" => opt(current.startup.sandbox.clone().map(Value::from)),
        _ => Value::Null,
    }
}

#[derive(Deserialize)]
pub struct UpdateItem {
    pub key: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct PutConfigRequest {
    pub updates: Vec<UpdateItem>,
}

/// PUT /api/config — 接收部分更新。
///
/// 空串与全空白串都表示「删掉这个键、回退默认」——由 `settings::write_to` 统一
/// 处理，这里不另写一份判据。
pub async fn put_config(
    State(state): State<AppState>,
    Json(request): Json<PutConfigRequest>,
) -> impl IntoResponse {
    let mut patch = serde_json::Map::new();
    for item in request.updates {
        patch.insert(item.key, Value::from(item.value));
    }

    match settings::write_to(&state.settings_path, &Value::Object(patch)) {
        Ok(()) => (axum::http::StatusCode::OK, Json(json!({ "ok": true }))),
        Err(SettingsError::InvalidSetting { key, reason }) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "code": "invalid_setting", "key": key, "error": reason })),
        ),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}
```

删除 `config_meta.rs`、`env_file.rs` 整个文件，以及 `api.rs` 里四个 `real-llm-test` handler 与 `mask` / `is_masked` 的使用。

- [ ] **Step 5: 更新 `Cargo.toml`**

```toml
[dependencies]
tokio = { workspace = true }
anyhow = { workspace = true }
serde_json = { workspace = true }
axum = { workspace = true }
serde = { version = "1", features = ["derive"] }
yi-agent-store = { workspace = true }
yi-agent-runtime = { path = "../yi-agent-runtime" }

[dev-dependencies]
tempfile = "3"
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"
```

- [ ] **Step 6: 改 `main.rs` 的 `web` 分支**

```rust
        Some(Command::Web { ref host, ref port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env_path = config::resolve_global_env_path()
                    .unwrap_or_else(|| std::path::PathBuf::from(".yi-agent").join(".env"));
                // 与 `run_agent` 同一组迁移：web 也是用户面向入口。
                let migration = yi_agent_runtime::migrate::migrate_model_keys(
                    &env_path,
                    &yi_agent_runtime::models::models_path(),
                );
                yi_agent_runtime::migrate::log_outcome(&migration, &env_path);
                let outcome = yi_agent_runtime::migrate::migrate_env_settings(
                    &env_path,
                    &yi_agent_runtime::settings::settings_path(),
                    &yi_agent_tools::websearch_path(),
                );
                yi_agent_runtime::migrate::log_env_settings_outcome(&outcome, &env_path);
                yi_agent_web::serve(host, *port, yi_agent_runtime::settings::settings_path()).await
            })
        }
```

- [ ] **Step 7: 改 `index.html`**

删掉 `renderRealLlmTestsTab`、`renderMain` 里对 `'Real LLM Tests'` 的分派、标签页表里对应的条目，以及顶部 `envPath` 的展示文案改为 `path`。其余渲染逻辑无需改动——字段形状（`key`/`value`/`default`/`type`/`options`）保持不变。

- [ ] **Step 8: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-web 2>&1 | tail -20
cd yi-agent-rs && cargo build -p yi-agent 2>&1 | tail -10
```

Expected: 全部通过。

- [ ] **Step 9: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/crates/yi-agent-web yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(web): edit settings.json instead of .env

config_meta.rs 与 env_file.rs 整体删除：元数据改由 runtime 的 settings.rs
提供，web 只是 HTTP 外壳。source 判定改成基于 value === null，不再用「键是否
存在于文件」，因为旧判据无法区分「键存在但为空」与「已设置」。

同时删除「真实 LLM 测试」面板：填配置归设置→模型（写 models.json、支持
多条目），测连通性归 model/probe。该面板写的 .env 与 just test-real-llm 实际
读取的 yi-agent-rs/.env 从来不是一个文件。"
```

---

## Task 12: 桌面端「运行参数」Tab

**Files:**
- Create: `desktop/src/lib/runtimeSettings.ts`
- Create: `desktop/src/lib/runtimeSettings.test.ts`
- Create: `desktop/src/components/SettingsRuntimeTab.tsx`
- Create: `desktop/src/components/SettingsRuntimeTab.test.tsx`
- Modify: `desktop/src/components/SettingsDialog.tsx`
- Modify: `desktop/src/App.tsx`
- Test: `desktop/src/components/SettingsDialog.test.tsx`

**Interfaces:**
- Consumes: RPC `settings/read` / `settings/write`（Task 10）
- Produces: `export type RuntimeSettingsCall = (method: string, params: unknown) => Promise<unknown>`
- Produces: `export interface RuntimeField { key: string; group: string; label: string; type: "number" | "text" | "select"; value: string | number | null; default: string; description: string; options: string[]; min: number | null; max: number | null }`
- Produces: `export function parseRuntimeSettings(view: unknown): { fields: RuntimeField[]; path: string }`
- Produces: `export function buildPatch(original: RuntimeField[], current: Record<string, string>): Record<string, string | null>`

**三态渲染（这是本 Tab 的核心）：**

| 数据 | 显示 | 「恢复默认」 |
|---|---|---|
| `value === null` | 「未设置 · 默认 X」 | 禁用 |
| `value !== null` | 「已设为 Y」 | 可用，发 `null` |
| 用户正在编辑（draft 非空且不同于原值） | 编辑框内的值 | 可用 |

- [ ] **Step 1: 写失败的 parser 测试**

`desktop/src/lib/runtimeSettings.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import { buildPatch, parseRuntimeSettings } from "./runtimeSettings";

describe("parseRuntimeSettings", () => {
  it("keeps null as null so the UI can distinguish unset from set", () => {
    const view = parseRuntimeSettings({
      path: "/home/me/.yi-agent/settings.json",
      fields: [
        {
          key: "compact.ratio",
          group: "compact",
          label: "压缩触发占比（%）",
          type: "number",
          value: null,
          default: "80",
          description: "d",
          options: [],
          min: 1,
          max: 100,
        },
      ],
    });
    expect(view.fields[0].value).toBeNull();
    expect(view.fields[0].default).toBe("80");
  });

  it("keeps a numeric zero instead of treating it as falsy", () => {
    const view = parseRuntimeSettings({
      fields: [{ key: "agent.max_turns", type: "number", value: 0 }],
    });
    expect(view.fields[0].value).toBe(0);
  });

  it("drops rows without a key and fills safe defaults for the rest", () => {
    const view = parseRuntimeSettings({
      fields: [{ type: "number", value: 1 }, { key: "agent.max_turns", value: 5 }],
    });
    expect(view.fields).toHaveLength(1);
    expect(view.fields[0].default).toBe("");
    expect(view.fields[0].options).toEqual([]);
  });

  it("tolerates a completely malformed payload", () => {
    expect(parseRuntimeSettings(null).fields).toEqual([]);
    expect(parseRuntimeSettings("nope").fields).toEqual([]);
    expect(parseRuntimeSettings({}).path).toBe("");
  });
});

describe("buildPatch", () => {
  const original = [
    {
      key: "agent.max_turns",
      group: "agent",
      label: "",
      type: "number" as const,
      value: 500,
      default: "200",
      description: "",
      options: [],
      min: 1,
      max: null,
    },
    {
      key: "compact.ratio",
      group: "compact",
      label: "",
      type: "number" as const,
      value: null,
      default: "80",
      description: "",
      options: [],
      min: 1,
      max: 100,
    },
  ];

  it("only emits fields the user actually changed", () => {
    const patch = buildPatch(original, { "agent.max_turns": "500", "compact.ratio": "" });
    expect(patch).toEqual({});
  });

  it("sends the new value for a changed field", () => {
    const patch = buildPatch(original, { "agent.max_turns": "600" });
    expect(patch).toEqual({ "agent.max_turns": "600" });
  });

  it("sends null to restore the default", () => {
    const patch = buildPatch(original, { "agent.max_turns": "" });
    expect(patch).toEqual({ "agent.max_turns": null });
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop && npx vitest run src/lib/runtimeSettings.test.ts 2>&1 | tail -15
```

Expected: FAIL，`Failed to resolve import "./runtimeSettings"`。

- [ ] **Step 3: 实现 `runtimeSettings.ts`**

```ts
/**
 * 桌面端与宿主之间的运行参数通道（`settings/read` / `settings/write`）。
 *
 * 形状与 `websearch.ts` / `mcp.ts` 一致：服务端回手写 `json!`（不走 ts-rs DTO），
 * 故本模块做一次形状收敛——坏行丢掉、缺失字段补安全默认，绝不把半成品往下传。
 *
 * **`value` 的 `null` 必须原样保留。** 它是「未设置」，与 `0`、`""` 都不同：
 * 归一成默认值就会重演旧 `get_config` 的问题——保存一次后所有字段都显示成已设置。
 */

export type RuntimeSettingsCall = (method: string, params: unknown) => Promise<unknown>;

export type RuntimeFieldType = "number" | "text" | "select";

export interface RuntimeField {
  key: string;
  group: string;
  label: string;
  type: RuntimeFieldType;
  /** `null` = 未设置。数字 0 与空串是不同的值，不要用真值判断。 */
  value: string | number | null;
  /** 默认值的展示文案（服务端常量）。 */
  default: string;
  description: string;
  options: string[];
  min: number | null;
  max: number | null;
}

export interface RuntimeSettingsView {
  fields: RuntimeField[];
  path: string;
}

const FIELD_TYPES: RuntimeFieldType[] = ["number", "text", "select"];

function str(v: unknown, fallback = ""): string {
  return typeof v === "string" ? v : fallback;
}

function numOrNull(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** `null` / 数字 / 字符串三种都合法；其它（对象、布尔）按「未设置」处理。 */
function scalarOrNull(v: unknown): string | number | null {
  if (v === null || v === undefined) return null;
  if (typeof v === "number" && Number.isFinite(v)) return v;
  if (typeof v === "string") return v;
  return null;
}

function parseField(raw: unknown): RuntimeField | null {
  if (typeof raw !== "object" || raw === null) return null;
  const record = raw as Record<string, unknown>;
  const key = str(record.key);
  if (!key) return null;
  const type = FIELD_TYPES.includes(record.type as RuntimeFieldType)
    ? (record.type as RuntimeFieldType)
    : "text";
  const options = Array.isArray(record.options)
    ? record.options.filter((o): o is string => typeof o === "string")
    : [];
  return {
    key,
    group: str(record.group),
    label: str(record.label, key),
    type,
    value: scalarOrNull(record.value),
    default: str(record.default),
    description: str(record.description),
    options,
    min: numOrNull(record.min),
    max: numOrNull(record.max),
  };
}

export function parseRuntimeSettings(view: unknown): RuntimeSettingsView {
  if (typeof view !== "object" || view === null) return { fields: [], path: "" };
  const record = view as Record<string, unknown>;
  const fields = Array.isArray(record.fields)
    ? record.fields
        .map(parseField)
        .filter((field): field is RuntimeField => field !== null)
    : [];
  return { fields, path: str(record.path) };
}

/**
 * 算出「用户改了哪几项」。`drafts` 里没有的键视为未改动——这样用户只动一个
 * 输入框时，不会把其余字段的当前值整体回写一遍。
 *
 * 输入框是空串时发 `null`（删键、回退默认），而不是发空串让服务端去猜。
 */
export function buildPatch(
  original: RuntimeField[],
  drafts: Record<string, string>,
): Record<string, string | null> {
  const patch: Record<string, string | null> = {};
  for (const field of original) {
    if (!(field.key in drafts)) continue;
    const draft = drafts[field.key];
    const current = field.value === null ? "" : String(field.value);
    if (draft === current) continue;
    patch[field.key] = draft === "" ? null : draft;
  }
  return patch;
}
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop && npx vitest run src/lib/runtimeSettings.test.ts 2>&1 | tail -15
```

Expected: 7 passed。

- [ ] **Step 5: 写组件测试**

`desktop/src/components/SettingsRuntimeTab.test.tsx`：

```tsx
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { SettingsRuntimeTab } from "./SettingsRuntimeTab";

const field = (over: Partial<Record<string, unknown>> = {}) => ({
  key: "agent.max_turns",
  group: "agent",
  label: "最大轮次",
  type: "number",
  value: null,
  default: "200",
  description: "一次对话里 agent 最多执行的轮数",
  options: [],
  min: 1,
  max: null,
  ...over,
});

function callWith(fields: unknown[]) {
  return vi.fn(async (method: string) => {
    if (method === "settings/read") return { fields, path: "/x/settings.json" };
    return { ok: true };
  });
}

describe("SettingsRuntimeTab", () => {
  it("shows the default and disables restore when the value is unset", async () => {
    render(<SettingsRuntimeTab call={callWith([field()])} />);
    expect(await screen.findByText(/未设置/)).toBeTruthy();
    expect(screen.getByText(/默认 200/)).toBeTruthy();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    // 用原生 DOM property 断言。本仓库没有装 `@testing-library/jest-dom`
    // （desktop/package.json 只有 @testing-library/react），所以 `toBeDisabled()`
    // 与 `toHaveProperty()` 这类匹配器都不可用——前者没注册，后者不是 vitest 内建。
    expect(restore.disabled).toBe(true);
  });

  it("enables restore and shows the set value when a value exists", async () => {
    render(<SettingsRuntimeTab call={callWith([field({ value: 500 })])} />);
    expect(await screen.findByText(/已设为 500/)).toBeTruthy();
    const restore = screen.getByRole("button", { name: /恢复默认/ }) as HTMLButtonElement;
    expect(restore.disabled).toBe(false);
  });

  it("sends null to restore the default", async () => {
    const call = callWith([field({ value: 500 })]);
    render(<SettingsRuntimeTab call={call} />);
    fireEvent.click(await screen.findByRole("button", { name: /恢复默认/ }));
    await waitFor(() => {
      expect(call).toHaveBeenCalledWith("settings/write", {
        patch: { "agent.max_turns": null },
      });
    });
  });

  it("reports the server's rejection inline instead of a whole-page error", async () => {
    const call = vi.fn(async (method: string) => {
      if (method === "settings/read") return { fields: [field({ value: 500 })] };
      throw new Error("invalid setting agent.max_turns: must be at least 1");
    });
    render(<SettingsRuntimeTab call={call} />);
    const input = await screen.findByLabelText("最大轮次");
    fireEvent.change(input, { target: { value: "0" } });
    fireEvent.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByText(/must be at least 1/)).toBeTruthy();
  });
});
```

- [ ] **Step 6: 实现组件**

`SettingsRuntimeTab.tsx`（结构仿 `SettingsWebSearchTab.tsx`）：

```tsx
import { useCallback, useEffect, useMemo, useState } from "react";
import {
  buildPatch,
  parseRuntimeSettings,
  type RuntimeField,
  type RuntimeSettingsCall,
} from "../lib/runtimeSettings";

const GROUP_LABELS: Record<string, string> = {
  agent: "Agent",
  compact: "压缩",
  startup: "启动",
};

const GROUP_ORDER = ["agent", "compact", "startup"];

/**
 * 「运行参数」页。
 *
 * 三态是这一页的核心：`value === null` 是「未设置」，显示默认值并禁用恢复按钮；
 * 有值则显示「已设为 Y」并允许一键恢复。**绝不把 null 归一成默认值**——那正是
 * 旧 web 的 `get_config` 做过的事，它让「保存一次后所有字段都像已设置」。
 *
 * 保存只发用户改过的字段（`buildPatch`），故不会把其余项的当前值整体回写。
 */
export function SettingsRuntimeTab({ call }: { call: RuntimeSettingsCall }) {
  const [fields, setFields] = useState<RuntimeField[]>([]);
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [notice, setNotice] = useState<{ ok: boolean; text: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const view = parseRuntimeSettings(await call("settings/read", {}));
      setFields(view.fields);
      // 草稿从「当前值」起步，让 buildPatch 能算出用户改了哪一项。
      const next: Record<string, string> = {};
      for (const field of view.fields) {
        next[field.key] = field.value === null ? "" : String(field.value);
      }
      setDrafts(next);
    } catch (error) {
      setNotice({ ok: false, text: String(error) });
    } finally {
      setLoading(false);
    }
  }, [call]);

  useEffect(() => {
    void load();
  }, [load]);

  const grouped = useMemo(() => {
    const order = GROUP_ORDER.filter((group) => fields.some((f) => f.group === group));
    const extra = [...new Set(fields.map((f) => f.group))].filter(
      (group) => !order.includes(group),
    );
    return [...order, ...extra].map((group) => ({
      group,
      fields: fields.filter((f) => f.group === group),
    }));
  }, [fields]);

  const save = async (patch: Record<string, string | null>) => {
    if (Object.keys(patch).length === 0) return;
    setSaving(true);
    setNotice(null);
    try {
      await call("settings/write", { patch });
      await load();
      setNotice({ ok: true, text: "已保存" });
    } catch (error) {
      // 服务端把 `invalid_setting` 的原因放在 message 里，直接内联显示。
      setNotice({ ok: false, text: String(error) });
    } finally {
      setSaving(false);
    }
  };

  if (loading) return <p className="settings-hint">读取中…</p>;

  return (
    <div className="settings-runtime">
      {notice ? (
        <p className={notice.ok ? "settings-hint" : "settings-error"} role="status">
          {notice.text}
        </p>
      ) : null}

      {grouped.map(({ group, fields: rows }) => (
        <section key={group} className="settings-group">
          <h3>{GROUP_LABELS[group] ?? group}</h3>
          {rows.map((field) => {
            const isUnset = field.value === null;
            return (
              <div key={field.key} className="settings-row">
                <label htmlFor={`runtime-${field.key}`}>{field.label}</label>
                {field.type === "select" ? (
                  <select
                    id={`runtime-${field.key}`}
                    value={drafts[field.key] ?? ""}
                    onChange={(event) =>
                      setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                    }
                  >
                    <option value="">未设置（默认 {field.default || "—"}）</option>
                    {field.options.map((option) => (
                      <option key={option} value={option}>
                        {option}
                      </option>
                    ))}
                  </select>
                ) : (
                  <input
                    id={`runtime-${field.key}`}
                    type={field.type === "number" ? "number" : "text"}
                    min={field.min ?? undefined}
                    max={field.max ?? undefined}
                    value={drafts[field.key] ?? ""}
                    placeholder={`未设置 · 默认 ${field.default || "—"}`}
                    onChange={(event) =>
                      setDrafts((prev) => ({ ...prev, [field.key]: event.target.value }))
                    }
                  />
                )}
                <p className="settings-hint">
                  {isUnset ? `未设置 · 默认 ${field.default || "—"}` : `已设为 ${field.value}`}
                </p>
                <p className="settings-hint">{field.description}</p>
                <button
                  type="button"
                  disabled={saving || isUnset}
                  onClick={() => void save({ [field.key]: null })}
                >
                  恢复默认
                </button>
              </div>
            );
          })}
        </section>
      ))}

      <button
        type="button"
        disabled={saving}
        onClick={() => void save(buildPatch(fields, drafts))}
      >
        保存
      </button>
    </div>
  );
}
```

- [ ] **Step 7: 跑组件测试确认通过**

```bash
cd desktop && npx vitest run src/components/SettingsRuntimeTab.test.tsx 2>&1 | tail -20
```

Expected: 4 passed。

- [ ] **Step 8: 接进 `SettingsDialog` 与 `App`**

`SettingsDialog.tsx`：

```tsx
import { SettingsRuntimeTab } from "./SettingsRuntimeTab";
import type { RuntimeSettingsCall } from "../lib/runtimeSettings";
```

```tsx
  { id: "runtime", label: "运行参数" },
```

放在 `general` 之后（它属于「这台机器的运行行为」，与「外观」相邻比与「实验功能」相邻更合理）。

props 加：

```tsx
  /** 「运行参数」Tab 的宿主 RPC 接缝（settings/read、settings/write）。 */
  runtimeSettingsCall?: RuntimeSettingsCall;
```

面板渲染链里加一条（放在 `general` 之后）：

```tsx
            ) : active === "runtime" && runtimeSettingsCall ? (
              <SettingsRuntimeTab call={runtimeSettingsCall} />
```

`App.tsx` 加接缝（与 `websearchCall` 同一写法）：

```tsx
  /**
   * 「运行参数」Tab 的宿主接缝。理由同 `modelCall`：只闭合 `clientRef`，空依赖
   * 即稳定；换新身份会让面板的读取 effect 重跑，把用户没保存的编辑冲掉。
   */
  const runtimeSettingsCall = useCallback(
    (method: string, params: unknown): Promise<unknown> => {
      const c = clientRef.current;
      if (!c) return Promise.reject(new Error("not connected"));
      return c.request(method, params);
    },
    [],
  );
```

在 `<SettingsDialog … />` 的 props 里传：

```tsx
        runtimeSettingsCall={runtimeSettingsCall}
```

- [ ] **Step 9: 补 `SettingsDialog` 的 Tab 测试**

在 `SettingsDialog.test.tsx` 追加：

```tsx
  it("renders the runtime tab and reads settings on open", async () => {
    const call = vi.fn(async () => ({ fields: [], path: "" }));
    render(
      <SettingsDialog
        open
        theme="dark"
        onThemeChange={() => {}}
        onClose={() => {}}
        runtimeSettingsCall={call}
      />,
    );
    fireEvent.click(screen.getByRole("tab", { name: "运行参数" }));
    await screen.findByText("读取中…");
    expect(call).toHaveBeenCalledWith("settings/read", {});
  });
```

- [ ] **Step 10: 跑全部前端测试与类型检查**

```bash
cd desktop && npx tsc --noEmit && npx vitest run --maxWorkers=1 2>&1 | tail -25
```

Expected: 类型检查无错，全部测试通过。

- [ ] **Step 11: Commit**

```bash
cd .. && git add desktop/src
git commit -m "feat(desktop): add a runtime settings tab

三态是这一页的核心：value 为 null 时显示「未设置 · 默认 X」并禁用恢复按钮，
有值时显示「已设为 Y」并允许一键恢复。绝不把 null 归一成默认值——那正是旧
web get_config 让「保存一次后所有字段都像已设置」的原因。

保存只发用户改过的字段，其余不回写；服务端的 invalid_setting 原因内联显示
在页内，不弹整页错误。"
```

---

## Task 13: `justfile` 与 `dev-env.sh` 不再依赖 `.env`

**Files:**
- Create: `yi-agent-rs/scripts/export-model-env.sh`
- Create: `yi-agent-rs/scripts/test_export_model_env.sh`
- Modify: `yi-agent-rs/justfile:93-160`
- Modify: `yi-agent-rs/scripts/dev-env.sh:36-50`
- Modify: `yi-agent-rs/scripts/test_dev_env.sh`

**Interfaces:**
- Produces: `scripts/export-model-env.sh` —— 向 stdout 输出 `export KEY='VALUE'` 行，供 `source <(...)` 使用；无 `python3`/`jq`、或 `models.json` 不可读时输出一行提示到 stderr 并 `exit 0`

- [ ] **Step 1: 写脚本的测试**

`scripts/test_export_model_env.sh`：

```bash
#!/usr/bin/env bash
# export-model-env.sh 的行为测试。全部在临时目录里跑，不碰真实 ~/.yi-agent。
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="$script_dir/export-model-env.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
pass() { printf 'ok - %s\n' "$1"; }

# 1. 默认条目被导出
mkdir -p "$tmp/home/.yi-agent"
cat > "$tmp/home/.yi-agent/models.json" <<'JSON'
{
  "models": [
    {"name":"a","provider":"openai","api_url":"https://a.example/v1","model":"m-a","api_key":"ka"},
    {"name":"b","provider":"anthropic","api_url":"https://b.example","model":"m-b","api_key":"kb"}
  ],
  "default_model": "b"
}
JSON

out="$(HOME="$tmp/home" bash "$target")"
printf '%s\n' "$out" | grep -q "export MODEL_API_KEY='kb'" || fail "default entry key not exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_PROVIDER='anthropic'" || fail "provider not exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_MODEL='m-b'" || fail "model not exported: $out"
printf '%s\n' "$out" | grep -q "export MODEL_API_URL='https://b.example'" || fail "url not exported: $out"
pass "default entry is exported"

# 2. 空字段不导出（否则会覆盖真实环境变量）
mkdir -p "$tmp/empty/.yi-agent"
cat > "$tmp/empty/.yi-agent/models.json" <<'JSON'
{"models":[{"name":"a","provider":"openai","api_url":"https://a.example/v1","model":"m-a","api_key":""}],"default_model":"a"}
JSON
out="$(HOME="$tmp/empty" bash "$target")"
printf '%s\n' "$out" | grep -q "MODEL_API_KEY" && fail "an empty key must not be exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_MODEL='m-a'" || fail "non-empty fields still export: $out"
pass "empty fields are skipped"

# 3. 缺 models.json：静默 exit 0
mkdir -p "$tmp/none"
HOME="$tmp/none" bash "$target" >/dev/null 2>&1 || fail "a missing models.json must exit 0"
pass "missing models.json exits 0"

# 4. 单引号被转义，source 之后值不变
mkdir -p "$tmp/quote/.yi-agent"
cat > "$tmp/quote/.yi-agent/models.json" <<'JSON'
{"models":[{"name":"a","provider":"openai","api_url":"https://a.example/v1","model":"m-a","api_key":"it's"}],"default_model":"a"}
JSON
value="$(HOME="$tmp/quote" bash "$target" | grep '^export MODEL_API_KEY=' | sed "s/^export MODEL_API_KEY='//; s/'\\+$//" || true)"
[ "$value" = "it'\\''s" ] || fail "single quote is not escaped as '\\'': $value"
pass "single quotes are escaped for source"

# 5. 没有 python3 也没有 jq：提示并 exit 0
mkdir -p "$tmp/stub"
cat > "$tmp/stub/python3" <<'SH'
#!/usr/bin/env bash
exit 127
SH
chmod +x "$tmp/stub/python3"
# 用一个只含 stub 的 PATH，确保 jq 也找不到。
out="$(PATH="$tmp/stub:/usr/bin:/bin" HOME="$tmp/home" bash "$target" 2>&1 || true)"
printf '%s\n' "$out" | grep -qi "python3\|jq" || fail "a missing tool must be reported: $out"
pass "missing tools are reported without failing"

echo "all export-model-env tests passed"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_export_model_env.sh 2>&1 | tail -10
```

Expected: FAIL，`export-model-env.sh: No such file`。

- [ ] **Step 3: 实现脚本**

`scripts/export-model-env.sh`：

```bash
#!/usr/bin/env bash
# 从 `<instance_home>/models.json` 的默认条目导出 real-test 所需的环境变量。
#
# 用法（在 justfile 里）：
#     eval "$(bash scripts/export-model-env.sh)"
#
# 三条硬要求，都有测试钉着（scripts/test_export_model_env.sh）：
#   1. 首选 python3、回退 jq。前者在本仓库的开发前提（Rust 工具链 + CLT）下必然
#      存在，后者常需 brew install；而且 python3 能一次读出整份 JSON 再统一处理
#      null/缺字段，不必为四个变量写四份表达式。
#   2. 两者都缺时打一行提示并 exit 0——缺工具是环境问题，不该伪装成测试失败。
#      与「无 key 时 real-test 自动跳过、exit 0 不算失败」同一约定。
#   3. 只导出非空值。某字段为空时不导出空串，否则会把本来能用的真实环境变量
#      覆盖成空。
set -uo pipefail

instance_home() {
  if [ -n "${YI_AGENT_HOME:-}" ]; then
    case "$YI_AGENT_HOME" in
      */.yi-agent) printf '%s\n' "$YI_AGENT_HOME" ;;
      *) printf '%s/.yi-agent\n' "$YI_AGENT_HOME" ;;
    esac
  else
    printf '%s/.yi-agent\n' "${HOME:-}"
  fi
}

models_path="$instance_home/models.json"

if [ ! -f "$models_path" ]; then
  printf 'export-model-env: 没有找到 %s；真实 LLM 测试将跳过\n' "$models_path" >&2
  exit 0
fi

# 输出四行 `KEY=value`（不带 export），空值行省略。由调用方加 export。
read_fields() {
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$1" <<'PY'
import json, sys
path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    sys.exit(0)
models = data.get("models") or []
default = data.get("default_model")
entry = None
for model in models:
    if model.get("name") == default:
        entry = model
        break
if entry is None and models:
    entry = models[0]
if entry is None:
    sys.exit(0)
for env_key, field in (
    ("YI_AGENT_PROVIDER", "provider"),
    ("YI_AGENT_MODEL", "model"),
    ("MODEL_API_URL", "api_url"),
    ("MODEL_API_KEY", "api_key"),
):
    value = entry.get(field)
    if isinstance(value, str) and value.strip():
        print(f"{env_key}={value}")
PY
    return 0
  fi
  if command -v jq >/dev/null 2>&1; then
    jq -r '
      (.models // []) as $models
      | ((.default_model // null) as $d
         | ($models | map(select(.name == $d)) | first) // $models[0]) as $e
      | select($e != null)
      | [
          ["YI_AGENT_PROVIDER", $e.provider],
          ["YI_AGENT_MODEL", $e.model],
          ["MODEL_API_URL", $e.api_url],
          ["MODEL_API_KEY", $e.api_key]
        ]
      | .[]
      | select(.[1] != null and (.[1] | tostring | length) > 0)
      | "\(.[0])=\(.[1])"
    ' "$1"
    return 0
  fi
  return 1
}

if ! fields="$(read_fields "$models_path")"; then
  printf 'export-model-env: 需要 python3 或 jq 之一来读取 models.json；真实 LLM 测试将跳过\n' >&2
  exit 0
fi

# 转义单引号：'  →  '\''
#
# 这个写法已在 bash 3.2 上实测过回环（含单引号、反斜杠、$ 的取值）：
#   v="it's a 'test' with \\ backslash and \$dollar"
#   eval "out='$(escape "$v")'"  →  out 与原值逐字节相同
# 双引号里的 `\\\\` 先变成 `\\`，sed 替换文本把 `\\` 解释成字面反斜杠，
# 故最终输出正是 `'\''`。不要「顺手简化」成 `s/'/\\''/g`——那会少一层转义，
# 生成的脚本在 `source` 时把值截断。
escape() {
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
}

printf '%s\n' "$fields" | while IFS='=' read -r key value; do
  [ -n "$key" ] || continue
  printf "export %s='%s'\n" "$key" "$(escape "$value")"
done
```

`chmod +x scripts/export-model-env.sh scripts/test_export_model_env.sh`。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_export_model_env.sh 2>&1 | tail -10
```

Expected: 5 条 `ok -`，末尾 `all export-model-env tests passed`。

- [ ] **Step 5: 改五个 real-test recipe**

以 `test-real-llm` 为例，其余三个同样处理（把 `set -a; . ./.env; set +a` 换成 `eval`）：

```make
test-real-llm:
    #!/usr/bin/env bash
    set -e
    # 清除 shell 环境变量污染（如 ~/.zshrc 里的全局 key），让 models.json 成为唯一配置源
    unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY MODEL_API_URL YI_AGENT_PROVIDER YI_AGENT_MODEL
    eval "$(bash scripts/export-model-env.sh)"
    if [ -z "$ANTHROPIC_API_KEY" ] && [ -z "$OPENAI_API_KEY" ] && [ -z "$MODEL_API_KEY" ]; then
        echo "SKIPPED: no provider API key configured (models.json has no usable default entry)"
        exit 0
    fi
    # 通用键优先：从 models.json 拿到的是 MODEL_API_KEY + provider/model 三件套。
    if [ -n "$MODEL_API_KEY" ] && [ -n "$YI_AGENT_PROVIDER" ]; then
        case "$YI_AGENT_PROVIDER" in
          anthropic) export ANTHROPIC_API_KEY="$MODEL_API_KEY" ;;
          openai) export OPENAI_API_KEY="$MODEL_API_KEY" ;;
        esac
    fi
    if [ -n "$ANTHROPIC_API_KEY" ]; then
        cargo test -p yi-agent-llm --test real_integration real_anthropic_ -- --ignored --test-threads=1
    fi
    if [ -n "$OPENAI_API_KEY" ]; then
        cargo test -p yi-agent-llm --test real_integration real_openai_ -- --ignored --test-threads=1
    fi
```

**一共五个 recipe 要改**（不要漏 `test-real-subagent`，它在 `justfile` 里排第 4、且是唯一读 `$HOME/.yi-agent/.env` 的那个）：

- `test-real-llm`
- `test-real-e2e`
- `test-real-complex`
- `test-real-subagent`——除了 `.env`，它还有一段 `if [ -f "$HOME/.yi-agent/.env" ]; then set -a; . "$HOME/.yi-agent/.env"; set +a; fi`，那段整块删除
- `test-real-all`——它只是聚合前四个（`test-real-all: test-real-llm test-real-e2e test-real-complex test-real-subagent`），无 recipe 体改动，但注释要跟着改

五个 recipe 上方的注释「配置源:yi-agent-rs/.env(优先,强制覆盖 shell 环境变量)」改为「配置源：`~/.yi-agent/models.json` 的默认条目」。

- [ ] **Step 6: 改 `dev-env.sh`**

```bash
ensure_instance() {
  local home
  home="$(instance_home)"
  mkdir -p "$home"
  # 模型清单与运行参数都在实例外：不复制的话实例开箱就是「尚未配置模型」。
  for file in models.json settings.json websearch.json; do
    if [ ! -f "$home/$file" ] && [ -f "$HOME/.yi-agent/$file" ]; then
      cp "$HOME/.yi-agent/$file" "$home/$file"
      printf 'dev-env: 已复制 %s 到 %s/%s\n' "$file" "$home" "$file" >&2
    fi
  done
  printf '%s\n' "$home"
}
```

`.env` 的复制整段删除。若 `.env.migrated` 存在，复制它没有意义——它已不被读取。

- [ ] **Step 7: 改 `test_dev_env.sh`**

把第 5 条测试（「已存在的 .env 会被保留」）改成断言三个配置文件各自被复制、且已存在的不被覆盖：

```bash
# 5. 配置文件被复制且已存在的不会被覆盖
mkdir -p "${TMP:?}/instances/$id1/.yi-agent"
for f in models.json settings.json websearch.json; do
  printf '{"from":"parent"}\n' > "$TMP/parent/$f"
  printf '{"from":"instance"}\n' > "$TMP/instances/$id1/.yi-agent/$f"
done
```

具体断言按该文件既有的 `pass`/`fail` 风格写；关键是**不再有任何 `.env` 相关断言**。

- [ ] **Step 8: 跑两个脚本的测试**

```bash
cd yi-agent-rs && bash scripts/test_export_model_env.sh && bash scripts/test_dev_env.sh 2>&1 | tail -15
```

Expected: 两者都通过。

- [ ] **Step 9: Commit**

```bash
cd .. && git add yi-agent-rs/justfile yi-agent-rs/scripts
git commit -m "ci: source real-test credentials from models.json instead of .env

五个 real-test recipe 原本 source ./.env，而那与 ~/.yi-agent/.env 从来不是
一个文件——.env 下线后它们会静默无 key。改为从 models.json 的默认条目导出，
正好接上 tests/common 里早已设计好但被绕过的第 2 级回退。

export-model-env.sh 首选 python3 回退 jq；两者都缺时提示并 exit 0（缺工具是
环境问题，不该伪装成测试失败）；只导出非空值，避免覆盖真实环境变量。
dev-env.sh 同步改为复制三个 json 而不是 .env。"
```

---

## Task 14: 清理 `config.rs` 的 `.env` 遗留代码与测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`（若 `dotenvy` 不再被任何 crate 使用则删除依赖）
- Test: `yi-agent-rs/crates/yi-agent-runtime/src/config.rs`

**Interfaces:**
- 删除：`prune_dotenv_model_keys`、`load_env_files`、`load_one_env`
- **保留**：`ModelEnvSnapshot`（读真实进程环境变量，与 `.env` 无关）、`MODEL_ENV_KEYS`（`migrate.rs` 用它判断模型键）、`resolve_global_env_path`（迁移仍在使用）

**要删的测试**（它们在 Task 5 之后已经没有意义，留着只会测一个不存在的行为）：

- `load_env_files_loads_the_single_path`
- `load_env_files_missing_path_is_silent`
- `load_env_files_real_env_overrides_the_file`
- 任何断言「non-model keys still come from the .env」的测试
- `a_dotenv_model_key_never_reaches_the_config`

- [ ] **Step 1: 找出全部待删符号**

```bash
cd yi-agent-rs && grep -rn "ModelEnvSnapshot\|prune_dotenv_model_keys" --include=*.rs crates | grep -v "^crates/yi-agent-runtime/src/config.rs"
```

Expected: 无输出。

```bash
cd yi-agent-rs && grep -rn "load_env_files\|load_one_env" --include=*.rs crates | grep -v "^crates/yi-agent-runtime/src/config.rs"
```

Expected: 无输出（Task 5 之后它们已无生产调用方）。

```bash
cd yi-agent-rs && grep -rn "dotenvy" --include=*.toml --include=*.rs crates | grep -v "^crates/yi-agent-runtime/src/config.rs"
```

Expected: 无输出。若有，说明别处还在用，先别删依赖。

- [ ] **Step 2: 删除符号与测试**

从 `config.rs` 删除：

- `prune_dotenv_model_keys` 函数
- `load_env_files` 与 `load_one_env` 函数
- `ModelEnvSnapshot` **不删**：它的 `take()` 仍在 `load_inner` 顶部，用于在任何污染之前快照真实进程环境变量。删掉它会让 spec 的第二层「真实进程环境变量」消失。
- `MODEL_ENV_KEYS` **保留**（`ModelEnvSnapshot` 与 `migrate.rs` 都用它）
- 上述五条测试

- [ ] **Step 3: 更新模块文档**

`config.rs` 顶部注释改为：

```rust
//! 配置加载：调用方覆盖项 + 真实进程环境变量 + `settings.json` > 默认值。
//!
//! 本模块由 CLI 与 GUI(app-server)共用，因此不依赖 clap。CLI 负责把
//! `clap` 解析出的参数转换成 [`ConfigOverrides`]，再交给
//! [`RuntimeConfig::load_lenient`]。
//!
//! **不再有 `.env`。** 过去 `load_inner` 会 `dotenvy::from_path` 把 `~/.yi-agent/.env`
//! 注入进程环境，再剔除模型键——`load_env_files` / `prune_dotenv_model_keys`
//! 已随之删除。`ModelEnvSnapshot` 保留：它快照的是**真实进程环境变量**，
//! 与 `.env` 无关，是第二层的实现。
//! 运行参数现在读 `settings.json`（见 [`crate::settings`]），且它是**第三层**：
//! 覆盖 > 真实环境变量 > settings.json > 默认值。
```

- [ ] **Step 4: 删除 `dotenvy` 依赖**

确认 Step 1 第二条 grep 无输出后：

```bash
cd yi-agent-rs && grep -n "dotenvy" Cargo.toml crates/*/Cargo.toml
```

从根 `Cargo.toml` 的 `[workspace.dependencies]` 与 `crates/yi-agent-runtime/Cargo.toml` 删除 `dotenvy` 行。

- [ ] **Step 5: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib 2>&1 | tail -20
cd yi-agent-rs && cargo build --workspace 2>&1 | tail -15
```

Expected: 全部通过，无 dead_code 警告（`prune_dotenv_model_keys` / `load_env_files` 等已删，不该再有 `#[allow(dead_code)]`；`ModelEnvSnapshot` 有生产调用方，也不需 `allow`）。

- [ ] **Step 6: Commit**

```bash
cd yi-agent-rs && cargo fmt --all
cd .. && git add yi-agent-rs/Cargo.toml yi-agent-rs/Cargo.lock yi-agent-rs/crates/yi-agent-runtime/Cargo.toml yi-agent-rs/crates/yi-agent-runtime/src/config.rs
git commit -m "refactor(runtime): delete the dead .env loading path

load_env_files / load_one_env / prune_dotenv_model_keys 在 settings.json
接管第三层后已无生产调用方，连同 dotenvy 依赖一起删除。留着它们会让
后来的人以为 .env 还在被读取。

ModelEnvSnapshot 保留：它快照真实进程环境变量，与 .env 无关。"
```

---

## Task 15: 文档与 `.env.example`

**Files:**
- Delete: `.env.example`
- Modify: `README.md:216-236`
- Modify: `docs/project-management/yi-agent-runtime.md`
- Modify: `docs/project-management/yi-agent-web.md`
- Modify: `docs/bug-list.md`
- Modify: `docs/project-management/README.md`（若模块计数变动）

- [ ] **Step 1: 删 `.env.example`**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent && git rm .env.example
```

已核实没有任何 Rust 代码读它（`grep -rn "env.example" yi-agent-rs/crates` 只命中测试里的假 URL `https://env.example`）。

- [ ] **Step 2: 改 `README.md` 的「配置放在哪」**

把四层结构改为：

```markdown
### 配置放在哪

| 层 | 位置 | 用途 |
|---|---|---|
| 默认值 | Rust 常量（`yi-agent-runtime/src/config.rs`） | 不需要文件 |
| 模型 | `~/.yi-agent/models.json` | 设置 → 模型 UI 或 `model/*` RPC |
| 运行参数 | `~/.yi-agent/settings.json` | 设置 → 运行参数 UI 或 `settings/*` RPC |
| 临时覆盖 | CLI 参数、真实环境变量 | CI、脚本、一次性试验 |

`YI_AGENT_HOME` 可以把 `~/.yi-agent` 整体换到别处（打包独立实例时用它隔离）。
`.env` 已不再被读取：存量配置在首次启动时自动迁入 `settings.json`，原文件改名为
`.env.migrated` 保留取回。`YI_AGENT_WORKDIR` 已下线——工作目录只由 `--workdir`、
启动目录或桌面端选项目决定。
```

- [ ] **Step 3: 改 `docs/project-management/yi-agent-runtime.md`**

- 删掉「`workdir/.yi-agent/.env` > `~/.yi-agent/.env`」这段已过时的分层描述。
- 新增一条 feature 记录：

```markdown
- [x] 运行参数存于 `<instance_home>/settings.json`，默认值只在 `config.rs` 常量一处
  - 判据：`cargo test -p yi-agent-runtime --lib settings`（字段元数据与默认值同源）
  - 判据：`cargo test -p yi-agent-runtime --lib config::tests::real_env_beats_settings_json`
- [x] `.env` 的存量运行参数一次性迁入 `settings.json`，原文件改名 `.env.migrated`
  - 判据：`cargo test -p yi-agent-runtime --lib migrate::tests::a_class_keys_land_in_settings_json`
  - 判据：`cargo test -p yi-agent-runtime --lib migrate::tests::second_run_is_idempotent_and_does_not_touch_settings`
- [x] `YI_AGENT_WORKDIR` 下线，工作目录只由 `--workdir` / 启动目录决定
  - 判据：`cargo test -p yi-agent-runtime --lib config::tests::workdir_env_var_no_longer_overrides`
```

- [ ] **Step 4: 改 `docs/project-management/yi-agent-web.md`**

范围边界改为「编辑 `settings.json`」，删掉「7 个非模型环境变量」的表述，并记录面板删除：

```markdown
- [x] Web UI 编辑 `<instance_home>/settings.json`（元数据来自 runtime 的 `settings.rs`）
  - 判据：`cargo test -p yi-agent-web`
- [x] 删除「真实 LLM 测试」面板（职能由「设置 → 模型」+ `model/probe` 覆盖）
  - 判据：`cargo test -p yi-agent-web`（`the_real_llm_test_routes_are_gone`）
```

- [ ] **Step 5: 改 `docs/bug-list.md`**

把两条标记为已修，并写明回归测试名：

```markdown
- [x] `yi-agent web` 保存配置会删掉 `ALL_VARS` 之外的键（含用户手写的项）
  - 回归网：`cargo test -p yi-agent-web put_config_preserves_unknown_keys`
  - 回归网：`cargo test -p yi-agent-runtime --lib settings::tests::write_preserves_unknown_top_level_keys`
- [x] `.env` 解析口径与 dotenvy 不一致（`KEY=v #c` 把注释并进值，`export KEY=v` 整行丢失）
  - 回归网：`cargo test -p yi-agent-runtime --lib settings::tests`（`unquoted_trailing_comment_is_stripped`、`export_prefix_is_stripped`、`hash_inside_quotes_is_kept`）
```

- [ ] **Step 6: 同步模块计数**

若 `docs/project-management/README.md` 的索引表有「完成 / 总计」计数，按新增的 feature 行数更新。

- [ ] **Step 7: 跑文档检查脚本**

```bash
cd yi-agent-rs && bash scripts/test_model_setup_docs.sh 2>&1 | tail -15
cd yi-agent-rs && bash scripts/check-model-setup-docs.sh 2>&1 | tail -15
```

Expected: 通过。若 `check-model-setup-docs.sh` 因新文案里的 `.env` 字样报红，检查它是否只拦「教用户往 .env 写模型 key」——本处文案说的是「.env 已不再被读取」，语义相反，必要时在脚本的白名单里加上这一节。

- [ ] **Step 8: Commit**

```bash
cd .. && git add -A .env.example README.md docs/
git commit -m "docs: document settings.json as the runtime config location

删掉 .env.example：没有任何代码读它，留着只会误导用户去项目根建 .env。
README 的配置分层改为「默认值 / models.json / settings.json / 临时覆盖」，
并写明 .env 的存量配置会自动迁入、原文件改名保留。

bug-list 里两条已修缺陷补上回归测试名。"
```

---

## Task 16: 端到端验收

**Files:**
- Test: `yi-agent-rs/crates/yi-agent/tests/settings_migration.rs`（新建集成测试）

**Interfaces:**
- Consumes: 全部前置 task 的产品行为

**依赖前提（已核实，无需改动 Cargo.toml）：** `crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里已有 `yi-agent-tools`（不是 dev-dependency），故 `yi_agent_tools::...` 在集成测试里可直接用。

**用哪个路径：** 用 crate 根的 re-export，不要写 `yi_agent_tools::web::config::...`。`yi-agent-tools/src/lib.rs:36-39` 已经 `pub use web::config::{EngineConfig, Freshness, WebSearchConfig, load_websearch_config_from, websearch_path}`，两种写法都能编译，但根路径更短且与产品代码一致：

```rust
yi_agent_tools::load_websearch_config_from(&websearch_path)  // 不是 web::config::
```

- [ ] **Step 1: 写端到端测试**

`crates/yi-agent/tests/settings_migration.rs`：

```rust
//! 端到端：一个带存量 `.env` 的实例目录，跑一次真实启动路径后，配置必须落在
//! `settings.json`，`.env` 变成 `.env.migrated`，且第二次启动不再改动任何文件。

use std::path::Path;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_legacy_instance_migrates_and_then_stays_put() {
    let dir = tempfile::TempDir::new().unwrap();
    let instance = dir.path().join(".yi-agent");
    write(
        &instance.join(".env"),
        "\
YI_AGENT_MAX_TURNS=500
YI_AGENT_SANDBOX=read-only
BOCHA_API_KEY=bocha-abc
YI_AGENT_WORKDIR=/tmp/nowhere
",
    );

    let env_path = instance.join(".env");
    let settings_path = instance.join("settings.json");
    let websearch_path = instance.join("websearch.json");
    let models_path = instance.join("models.json");

    // 第一次：与生产启动函数相同的两条迁移。
    let _ = yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);
    let outcome = yi_agent_runtime::migrate::migrate_env_settings(
        &env_path,
        &settings_path,
        &websearch_path,
    );
    assert!(
        matches!(
            outcome,
            yi_agent_runtime::migrate::EnvMigrationOutcome::Migrated { .. }
        ),
        "got {outcome:?}"
    );

    let settings = yi_agent_runtime::settings::load_from(&settings_path);
    assert_eq!(settings.agent.max_turns, Some(500));
    assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
    assert!(
        std::fs::read_to_string(&settings_path)
            .unwrap()
            .find("workdir")
            .is_none(),
        "workdir must not be persisted: {settings:?}"
    );

    let websearch = yi_agent_tools::load_websearch_config_from(&websearch_path);
    assert_eq!(websearch.engines["bocha"].api_key, "bocha-abc");

    assert!(!env_path.exists(), ".env must be renamed");
    assert!(instance.join(".env.migrated").exists());

    // 第二次：幂等，不碰 settings.json。
    let mtime = std::fs::metadata(&settings_path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second = yi_agent_runtime::migrate::migrate_env_settings(
        &env_path,
        &settings_path,
        &websearch_path,
    );
    assert_eq!(
        second,
        yi_agent_runtime::migrate::EnvMigrationOutcome::NothingToDo
    );
    assert_eq!(
        std::fs::metadata(&settings_path).unwrap().modified().unwrap(),
        mtime,
        "a second start must not rewrite settings.json"
    );
}

/// 迁移后的配置必须能真的驱动 `RuntimeConfig`——不只是文件里有值。
#[test]
fn the_migrated_settings_are_what_the_config_loads() {
    let dir = tempfile::TempDir::new().unwrap();
    let instance = dir.path().join(".yi-agent");
    write(&instance.join(".env"), "YI_AGENT_MAX_TURNS=432\n");

    let env_path = instance.join(".env");
    let settings_path = instance.join("settings.json");
    let websearch_path = instance.join("websearch.json");
    let _ = yi_agent_runtime::migrate::migrate_env_settings(
        &env_path,
        &settings_path,
        &websearch_path,
    );
    assert!(!env_path.exists());

    // 用 `load_from` + 直接比较，避免改进程级 `YI_AGENT_HOME`（那会与并行测试打架）。
    let settings = yi_agent_runtime::settings::load_from(&settings_path);
    assert_eq!(settings.agent.max_turns, Some(432));
}
```

- [ ] **Step 2: 跑测试确认通过**

```bash
cd yi-agent-rs && cargo test -p yi-agent --test settings_migration 2>&1 | tail -20
```

Expected: 2 passed。

- [ ] **Step 3: 跑全部验证命令**

按顺序、串行执行（先确认没有其它 cargo 进程）：

```bash
ps aux | grep -c "[c]argo"
cd yi-agent-rs && cargo test -p yi-agent-runtime --lib 2>&1 | tail -8
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib 2>&1 | tail -8
cd yi-agent-rs && cargo test -p yi-agent-web 2>&1 | tail -8
cd yi-agent-rs && cargo test -p yi-agent-tools --lib 2>&1 | tail -8
cd yi-agent-rs && cargo test -p yi-agent 2>&1 | tail -8
cd desktop && npx tsc --noEmit && npx vitest run --maxWorkers=1 2>&1 | tail -10
```

Expected: 全绿。

- [ ] **Step 4: 手工验收真实实例**

```bash
ls -la ~/.yi-agent/.env ~/.yi-agent/.env.migrated ~/.yi-agent/settings.json 2>&1
cat ~/.yi-agent/settings.json 2>/dev/null
```

预期（本机现状：`.env` 里只有 `YI_AGENT_MAX_TURNS=500` 与 `BOCHA_API_KEY` 两个真实值）：首次启动后 `settings.json` 里有 `agent.max_turns: 500`，`websearch.json` 里有 bocha key，`.env` 变成 `.env.migrated`。

- [ ] **Step 5: 跑 `just ci`**

```bash
cd yi-agent-rs && just ci 2>&1 | tail -25
```

Expected: 通过。特别确认 `fmt-check`、`secret-check`、`model-setup-docs-check` 三项。

- [ ] **Step 6: Commit**

```bash
cd .. && git add yi-agent-rs/crates/yi-agent/tests/settings_migration.rs
git commit -m "test: cover the end-to-end .env to settings.json migration

走真实的迁移函数而不是桩：一个带存量 .env 的实例目录，跑一次后配置落在
settings.json、.env 变成 .env.migrated、BOCHA key 进了 websearch.json；
第二次启动幂等且不改写 settings.json。"
```

---

## 自审记录

按 writing-plans 的要求，写完后逐项核对 spec。

**1. Spec 覆盖检查**

| spec 要求 | 对应 task |
|---|---|
| `settings.json` 类型化存储、`Option<T>` 字段 | Task 2 |
| 默认值只有 Rust 一处 | Task 3（提常量）+ Task 2/3 的交叉断言测试 |
| 空字符串等于未设置 | Task 4（写入侧）、Task 5（加载侧 `normalized`）、Task 10（RPC） |
| 未知顶层键必须保留 | Task 4（`write_preserves_unknown_top_level_keys`） |
| `0600` + 临时文件 + rename | Task 4（`written_file_is_owner_only`） |
| 优先级：覆盖 > env > settings.json > 默认 | Task 5（三条优先级测试） |
| `skills_catalog_budget_explicit` 不含 settings.json | Task 5（`settings_budget_is_not_explicit`） |
| `.env` → `settings.json` 迁移 | Task 6 |
| `BOCHA_API_KEY` → `websearch.json` | Task 6 |
| `.env` 改名 `.env.migrated` | Task 6 |
| `YI_AGENT_WORKDIR` 丢弃 + warn | Task 5（下线）、Task 6（迁移丢弃） |
| `.env` 解析口径与 dotenvy 一致 | Task 1 |
| `BOCHA_API_KEY` env 回退删除 | Task 9 |
| 启动序：两条迁移在 `load_lenient` 之前 | Task 7 |
| 四个入口都迁移（含 ws） | Task 7 |
| onboarding 删 Env 兜底层 | Task 8 |
| `settings/read`、`settings/write` | Task 10 |
| 不提供 `settings/test` | Task 10 Step 6（只加两条到 `domain_for` 表） |
| `settings/write` 拒绝规则、零写入 | Task 4 + Task 10 |
| web 换存储、删 `config_meta`/`env_file` | Task 11 |
| web 删「真实 LLM 测试」面板 | Task 11 |
| `source` 基于 `null` 判定 | Task 11 |
| 桌面端「运行参数」Tab、三态 | Task 12 |
| justfile 五个 recipe 改数据源 | Task 13 |
| `export-model-env.sh` python3/jq/exit 0/只导非空 | Task 13 |
| `dev-env.sh` 不再复制 `.env` | Task 13 |
| 错误处理表（7 种情形） | Task 4、Task 5、Task 6 各自的测试 |
| 迁移幂等 | Task 6（`second_run_is_idempotent...`）、Task 16 |
| `.env.example` 删除 | Task 15 |
| README / 项目文档 / bug-list | Task 15 |
| `sandbox_promotable` 不被持久化配置影响 | Task 5（`settings_json_supplies_values...` 末段断言） |

**2. 占位符扫描**

已检查：无 `TBD` / `TODO` / 「类似 Task N」/ 「添加适当的错误处理」。每个代码步骤都给了可直接粘贴的实现。唯一保留的 `TODO` 字样是 Task 5 Step 3 里刻意写给实现者的注释（「留 TODO 注释指向 Task 14」），那是要求实现者**写下**一条注释，不是计划里的占位。

**3. 类型一致性**

- `Settings` / `AgentSettings` / `CompactSettings` / `StartupSettings` 的字段名在 Task 2 定义一次，Task 4、5、6、10、11 全部沿用，逐字段核对过。
- `FieldSpec` 的字段名（`key`/`group`/`label`/`field_type`/`default_display`/`description`/`options`/`min`/`max`）在 Task 2、3、10、11 一致。
- `SettingsError` 的三个变体（`InvalidSetting{key,reason}`/`Corrupt(String)`/`Io(String)`）在 Task 4、6、10、11 一致。
- `EnvMigrationOutcome`（`NothingToDo`/`Migrated{fields,bocha}`/`DroppedOnly`/`EnvUnreadable{reason}`/`SettingsNotWritable{reason}`）在 Task 6、7、16 一致。`DroppedOnly` 是写计划时补的变体：`.env` 只剩 `YI_AGENT_WORKDIR` / `YI_AGENT_YOLO` 时也必须改名，否则每次启动都重复同一条告警、文件永不消失。
- `assess` 的签名在 Task 8 从 `assess(env, catalog)` 改为 `assess(catalog)`，三个调用点与 3 条测试同步更新，没有残留旧签名。
- 前端 `RuntimeField` 的字段名（`key`/`group`/`label`/`type`/`value`/`default`/`description`/`options`/`min`/`max`）与 Task 10 的 JSON 键逐一对齐。

**4. 已识别的风险与处置**

- **Task 5 Step 6 的既有测试改写**：`config.rs` 里有几条测试原本依赖 `.env` 提供非模型键。计划要求把它们的数据源改成 `settings.json` 以保持断言语义，而不是直接删除——因为那些断言测的是「分层优先级」，语义仍然成立。Task 14 只删真正测 `.env` 加载机制的测试。
- **Task 8 改动面最大**（跨 9 个文件）。它被单独切成一个 task 正是为了让 reviewer 可以只否掉这一块而不影响其它。
- **`app_state!` 宏的字段顺序注释**：删 `onboarding_env_path` 后必须同步更新那段注释（当前说「四个相邻 `&Path`」），否则注释会误导下一个人。Task 8 Step 6 已点名。
- **`migrate_legacy_model_keys_on_startup` 的名字与职责不符**：计划明确选择保留名字并在 doc comment 说明，而不是改四个调用点。这是有意识的取舍，写在 Task 7 的 Steps 里。

**5. 写计划期间实测过的四件事（结论已回写进对应 Task，实施时不必重新验证）**

| 实测 | 方法 | 结论 |
|---|---|---|
| `Settings` 的 `de_optional_*` + `normalized()` 写法能编译且逐字段降级 | 最小 cargo 工程复刻，跑 5 组用例（合法邻居存活、坏类型、非对象、`u32::MAX+1`、未知顶层键） | 全通过。`#[serde(default)]` 只补缺失不救坏值，故必须显式写 `deserialize_with` |
| `write_to` 的读-改-写语义 | 复刻 `validate_value`/`apply_op`/`write_to`，跑 8 组用例 | 未知键保留、未设置字段不物化、`null` 与空串删键、越界拒绝、坏文件拒绝覆盖、字符串数字可收、空白串删键，全部符合预期 |
| `parse_env_line` 的 dotenvy 口径 | 复刻解析器，跑 12 条断言 + 3 条边界（`A=""`、`A='`、UTF-8 值带注释） | 全通过；UTF-8 值正确截断注释 |
| `escape()` 的单引号转义 | bash 实测 `eval "out='$(escape "$v")'"` 回环（含单引号、反斜杠、`$`） | 回环逐字节相同。计划里那句 `\\` 的 sed 写法是对的，不要「简化」 |

**6. 写计划期间核对掉的、曾怀疑是缺陷但实际不成立的项**

- `Task 16` 担心缺 `yi-agent-tools` 的 dev-dependency：实际它已在 `crates/yi-agent/Cargo.toml` 的 `[dependencies]` 里，集成测试可直接用，无需改 Cargo.toml。
- `Task 8` 担心删 `ReadySource` 后 `onboarding/status` 响应里的 `source` 字段要同步改：实际该响应只有 `needed` / `dismissed` / `reasons` 三个键，从来没有 `source`。桌面端与 web 端零改动。
- `Task 10` 担心 `AppState` 在测试里有多处手写构造：实际 `AppState` 只有 `server.rs` 的 `app_state!()` 宏一处构造；真正有多处的是 `RuntimeAttachments`（5 处），计划已改成点名 `RuntimeAttachments`。
- `Task 8` 原写 `rpc_tests.rs` 有 4 处 `onboarding_env_path`：实际 5 处（2384、4115、4200、4287、17767），已改。
