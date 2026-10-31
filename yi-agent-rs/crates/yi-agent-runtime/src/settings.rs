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
    fn comments_and_blank_lines_produce_nothing() {
        assert_eq!(parse_env_line("# just a comment"), None);
        assert_eq!(parse_env_line(""), None);
        assert_eq!(parse_env_line("   "), None);
    }

    #[test]
    fn a_line_without_equals_produces_nothing() {
        assert_eq!(parse_env_line("NO_EQUALS_HERE"), None);
    }

    #[test]
    fn a_line_with_an_empty_key_produces_nothing() {
        assert_eq!(parse_env_line("=value"), None);
        assert_eq!(parse_env_line(" =value"), None);
    }

    #[test]
    fn empty_value_is_allowed() {
        assert_eq!(
            parse_env_line("KEY="),
            Some(("KEY".to_string(), String::new()))
        );
    }

    #[test]
    fn text_parsing_collects_lines_and_later_wins() {
        let text = "\
# comment
A=1
export B=2
A=3
malformed line
";
        let parsed = parse_env_text(text);
        assert_eq!(parsed.get("A").map(String::as_str), Some("3"));
        assert_eq!(parsed.get("B").map(String::as_str), Some("2"));
        assert_eq!(parsed.len(), 2);
    }

    /// 迁移读的是真实 `.env`：非 ASCII 值与注释混排必须正确。
    #[test]
    fn non_ascii_values_are_handled() {
        assert_eq!(
            parse_env_line("A=中文值 # 注释"),
            Some(("A".to_string(), "中文值".to_string()))
        );
    }
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
                    assert!(
                        !field.options.is_empty(),
                        "{} is Select but has no options",
                        field.key
                    );
                }
                _ => {
                    assert!(
                        field.options.is_empty(),
                        "{} is not Select but has options",
                        field.key
                    );
                }
            }
        }
    }

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

    /// `workdir` 与 `yolo` 刻意不在表里。workdir 只由启动方式决定，yolo 必须每次
    /// 启动显式给出——持久化它会让 `sandbox_promotable` 恒为 false。
    #[test]
    fn workdir_and_yolo_are_not_persistable() {
        for forbidden in ["workdir", "agent.workdir", "yolo", "startup.yolo"] {
            assert!(
                find_spec(forbidden).is_none(),
                "{forbidden} must not be persistable"
            );
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
}
