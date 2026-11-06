//! `settings.json`：agent 可调参数的唯一持久化位置。
//!
//! 取代过去的 `~/.yi-agent/.env`。本模块同时承担三件事：类型化存储、
//! 字段元数据（供 web 与桌面端渲染表单）、以及一次性迁移所需的 `.env` 行解析。

use std::collections::BTreeMap;

pub mod document;

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
///
/// **退役计划 `2027-01-01`**：本函数只服务于存量 `.env` 的一次性迁移
/// （[`crate::migrate::migrate_env_settings`] 是它唯一的生产消费者）。配置读取
/// 路径早已不碰 `.env`，故迁移层到期整体删除时，本函数与 [`parse_env_line`]
/// 一并移除。完整删除清单见 [`crate::migrate`] 的模块文档。
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
    pub tui: TuiSettings,
    pub power: PowerSettings,
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

/// 终端 TUI 的行为设置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiSettings {
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// **为什么是字符串而不是 `Option<bool>`**：`FIELDS` 的三种控件
    /// （`Number`/`Text`/`Select`）里，`Text` 与 `Select` 都只接受 JSON
    /// 字符串（`validate_value`）。用 `Option<bool>` 会导致设置页写进
    /// `"true"` 后反序列化读不回来。存字符串与 `startup.sandbox` 完全同构，
    /// 也让非法值能按字段 warn 回退。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notifications: Option<String>,
    /// `auto` / `osc9` / `bel`；`None` = `auto`。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notification_method: Option<String>,
    /// `unfocused` / `always`；`None` = `unfocused`。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub notification_condition: Option<String>,
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 与同组三个通知键同构：必须是字符串而不是 `Option<bool>`，因为
    /// `FIELDS` 的 `Select` / `Text` 只接受 JSON 字符串，用 `Option<bool>`
    /// 会导致设置页写进 `"true"` 后反序列化读不回来。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub terminal_title: Option<String>,
}

/// 电源与休眠设置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PowerSettings {
    /// `true` / `false`；`None` = 默认 `true`。
    ///
    /// 存字符串而不是 `Option<bool>`：`FIELDS` 的 `Select` 只接受 JSON
    /// 字符串（`validate_value`），用 `Option<bool>` 会让设置页写进
    /// `"true"` 后反序列化读不回来。与 `tui.notifications` 完全同构。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub prevent_idle_sleep: Option<String>,
    /// 条件消失后再保活多久（分钟）。范围 1–120。
    #[serde(default, deserialize_with = "de_optional_u64")]
    pub idle_grace_minutes: Option<u64>,
    /// 最近一次远端交互在多长时间内仍算「有活」（分钟）。范围 1–60。
    #[serde(default, deserialize_with = "de_optional_u64")]
    pub remote_touch_minutes: Option<u64>,
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

/// 「条件消失后再保活多久」的默认值，单位分钟。
///
/// 与字段语义同单位（分钟），秒数换算只在取用窗口时做一次。
/// 单位真实值 15 分钟 = `yi_agent_power::policy::DEFAULT_GRACE`，
/// 但本 crate 不依赖 `yi-agent-power`（二者互不依赖），故此处独立定义，
/// 由 `power_windows_minutes` 的测试与 `Policy` 的测试各自钉住。
pub const POWER_IDLE_GRACE_MINUTES_DEFAULT: u64 = 15;

/// 「远端交互有效窗口」的默认值，单位分钟。对应 300 秒。
pub const POWER_REMOTE_TOUCH_MINUTES_DEFAULT: u64 = 5;

/// 开关的点路径键。多处引用同一常量，避免字符串抄错。
///
/// **它从此只表示新键。** 迁移函数里的旧键必须写成字面量
/// `"agent.prevent_idle_sleep"`，不能引用本常量——否则将来再改常量，
/// 迁移就会跟着漂移、静默失效。
pub const PREVENT_IDLE_SLEEP_KEY: &str = "power.prevent_idle_sleep";

/// 开关是否打开。缺文件 / 空串 / 未知值一律按默认（开）处理，与
/// `tui.notifications` 的兜底一致。
pub fn prevent_idle_sleep_enabled(settings: &Settings) -> bool {
    match settings.power.prevent_idle_sleep.as_deref() {
        Some(value) => !value.eq_ignore_ascii_case("false"),
        None => true,
    }
}

/// 两个窗口的实际生效值（分钟），返回 `(grace, remote)`。
///
/// **只兜 `None`，不兜越界**——越界值由 `normalized()` 的 `clamp!` 先行清空，
/// 调用方必须先取得经过 `normalized()` 的 `Settings`。`load_from` 的返回值
/// 已经过；`write_to` 里要自己先调 `.normalized()`。
///
/// 不做 slack 推导，也不做单位换算：slack 是策略层概念，放在
/// `power_guard::slack_for`；分钟到秒的换算由调用点乘 60。
pub fn power_windows_minutes(settings: &Settings) -> (u64, u64) {
    let grace = settings
        .power
        .idle_grace_minutes
        .unwrap_or(POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote = settings
        .power
        .remote_touch_minutes
        .unwrap_or(POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    (grace, remote)
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
        key: PREVENT_IDLE_SLEEP_KEY,
        group: "power",
        label: "干活期间防止休眠",
        field_type: FieldType::Select,
        default_display: "true",
        description: "有网且（插电 或 有活在跑）时阻止系统空闲休眠；条件消失后按「保活窗口」再等一段时间",
        options: &["true", "false"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "power.idle_grace_minutes",
        group: "power",
        label: "条件消失后再保活多久（分钟）",
        field_type: FieldType::Number,
        default_display: "15",
        description: "有网且（插电 或 有活在跑）的条件消失后，继续阻止休眠这么久；范围 1–120",
        options: &[],
        min: Some(1),
        max: Some(120),
    },
    FieldSpec {
        key: "power.remote_touch_minutes",
        group: "power",
        label: "远端交互有效窗口（分钟）",
        field_type: FieldType::Number,
        default_display: "5",
        description: "手机等远端客户端最近一次请求在这么久之内，仍算作有活在跑；不得大于上面的保活窗口。范围 1–60",
        options: &[],
        min: Some(1),
        max: Some(60),
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
    FieldSpec {
        key: "tui.notifications",
        group: "tui",
        label: "终端通知",
        field_type: FieldType::Select,
        default_display: "true",
        description: "回合完成或等待批准时发送终端通知；空表示用默认（开）",
        options: &["true", "false"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "tui.terminal_title",
        group: "tui",
        label: "终端标题",
        field_type: FieldType::Select,
        default_display: "true",
        description: "在终端标签页标题里显示项目名与运行状态；空表示用默认（开）",
        options: &["true", "false"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "tui.notification_method",
        group: "tui",
        label: "通知协议",
        field_type: FieldType::Select,
        default_display: "auto",
        description: "auto 按终端自动选择；osc9 带正文，bel 只响铃",
        options: &["auto", "osc9", "bel"],
        min: None,
        max: None,
    },
    FieldSpec {
        key: "tui.notification_condition",
        group: "tui",
        label: "通知触发条件",
        field_type: FieldType::Select,
        default_display: "unfocused",
        description: "unfocused 只在窗口失焦时发；always 总是发",
        options: &["unfocused", "always"],
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

use std::path::{Path, PathBuf};

/// `<instance_home>/settings.json`；实例目录不可解析时回退 `.yi-agent/settings.json`，
/// 与 `models_path` / `websearch_path` 同口径。
pub fn settings_path() -> PathBuf {
    yi_agent_store::paths::home_join("settings.json")
        .unwrap_or_else(|| PathBuf::from(".yi-agent").join("settings.json"))
}

/// 把旧键 `agent.prevent_idle_sleep` 搬到 `power.prevent_idle_sleep`。
///
/// **必须做在 `serde_json::Value` 上，不能等反序列化之后。**
/// `Settings` 上没有 `deny_unknown_fields`，`AgentSettings` 里删掉该字段后，
/// serde 会静默忽略文件里的旧键——反序列化之后旧值**已经不存在**，无处可搬。
///
/// 三条约束：
/// 1. 只在新键缺失时迁移，否则用户显式设的 `power` 值每次启动都被旧值覆盖；
/// 2. 不落盘（只在内存里搬），避免擅自改用户文件；
/// 3. 幂等——旧键被删掉，同一份 `Value` 上再跑是空操作。
///
/// 旧键字符串是**字面量**，不得引用 `PREVENT_IDLE_SLEEP_KEY`：那个常量
/// 已经指向新键，将来再改值会让迁移跟着漂移、静默失效。
fn migrate_legacy_keys(value: &mut serde_json::Value) {
    const LEGACY_GROUP: &str = "agent";
    const LEGACY_FIELD: &str = "prevent_idle_sleep";
    const NEW_GROUP: &str = "power";

    let existing = value
        .get(LEGACY_GROUP)
        .and_then(|group| group.get(LEGACY_FIELD))
        .cloned();
    let Some(legacy) = existing else {
        return;
    };

    // 新键已存在就不动：用户显式设的值优先。
    let already_set = value
        .get(NEW_GROUP)
        .and_then(|group| group.get(LEGACY_FIELD))
        .is_some();
    if already_set {
        return;
    }

    let Some(object) = value.as_object_mut() else {
        return;
    };
    object
        .entry(NEW_GROUP.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .expect("just inserted an object")
        .insert(LEGACY_FIELD.to_string(), legacy);

    // 删旧键；agent 变空壳则整组删掉，与 `write_to` 的空组清理同口径。
    let agent_now_empty = match object
        .get_mut(LEGACY_GROUP)
        .and_then(|group| group.as_object_mut())
    {
        Some(group) => {
            group.remove(LEGACY_FIELD);
            group.is_empty()
        }
        None => false,
    };
    if agent_now_empty {
        object.remove(LEGACY_GROUP);
    }
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
    let mut value: serde_json::Value = match serde_json::from_str(&text) {
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

    // 旧键在反序列化之后就不存在了，必须在这里搬。
    migrate_legacy_keys(&mut value);

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
        self.startup.sandbox = self
            .startup
            .sandbox
            .filter(|value| !value.trim().is_empty());

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
        clamp!(
            "agent.max_resident_subagents",
            self.agent.max_resident_subagents
        );
        clamp!(
            "agent.skills_catalog_budget",
            self.agent.skills_catalog_budget
        );
        clamp!("compact.ratio", self.compact.ratio);
        clamp!(
            "compact.user_budget_tokens",
            self.compact.user_budget_tokens
        );
        clamp!(
            "compact.tool_budget_tokens",
            self.compact.tool_budget_tokens
        );
        clamp!("power.idle_grace_minutes", self.power.idle_grace_minutes);
        clamp!(
            "power.remote_touch_minutes",
            self.power.remote_touch_minutes
        );

        // 枚举白名单：只认 `parse_sandbox_mode` 那份映射，不另写一遍。
        // 该函数当前是 `config.rs` 里的私有 `fn`，本任务需把它的可见性改成
        // `pub(crate) fn parse_sandbox_mode(...)`（`settings.rs` 与 `config.rs`
        // 同 crate，`pub(crate)` 足够）。
        if let Some(enabled) = self.tui.notifications.as_deref()
            && !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false")
        {
            tracing::warn!(
                value = enabled,
                "settings.json has an unknown tui.notifications; falling back to the default"
            );
            self.tui.notifications = None;
        }
        if let Some(enabled) = self.tui.terminal_title.as_deref()
            && !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false")
        {
            tracing::warn!(
                value = enabled,
                "settings.json has an unknown tui.terminal_title; falling back to the default"
            );
            self.tui.terminal_title = None;
        }
        if let Some(method) = self.tui.notification_method.as_deref()
            && !matches!(
                method.to_ascii_lowercase().as_str(),
                "auto" | "osc9" | "bel"
            )
        {
            tracing::warn!(
                value = method,
                "settings.json has an unknown tui.notification_method; falling back to the default"
            );
            self.tui.notification_method = None;
        }
        if let Some(condition) = self.tui.notification_condition.as_deref()
            && !matches!(
                condition.to_ascii_lowercase().as_str(),
                "unfocused" | "always"
            )
        {
            tracing::warn!(
                value = condition,
                "settings.json has an unknown tui.notification_condition; falling back to the default"
            );
            self.tui.notification_condition = None;
        }
        if let Some(enabled) = self.power.prevent_idle_sleep.as_deref()
            && !matches!(enabled.to_ascii_lowercase().as_str(), "true" | "false")
        {
            tracing::warn!(
                value = enabled,
                "settings.json has an unknown power.prevent_idle_sleep; falling back to the default"
            );
            self.power.prevent_idle_sleep = None;
        }
        if let Some(sandbox) = self.startup.sandbox.as_deref()
            && crate::config::parse_sandbox_mode(sandbox).is_none()
        {
            tracing::warn!(
                value = sandbox,
                "settings.json has an unknown startup.sandbox; falling back to the default"
            );
            self.startup.sandbox = None;
        }

        // 跨字段：远端窗口长于宽限会让「宽限」在电池场景下失去意义——
        // 用户以为「没活 15 分钟就放睡」，实际因为远端窗口 30 分钟一直在扛。
        // `clamp!` 管不到这个：两个值各自都在范围内，倒挂仍然成立。
        if let (Some(remote), Some(grace)) = (
            self.power.remote_touch_minutes,
            self.power.idle_grace_minutes,
        ) && remote > grace
        {
            tracing::warn!(
                remote,
                grace,
                "power.remote_touch_minutes exceeds power.idle_grace_minutes; falling back to the defaults"
            );
            self.power.remote_touch_minutes = None;
            self.power.idle_grace_minutes = None;
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
    InvalidSetting {
        key: String,
        reason: String,
    },
    /// 文件存在，但读不出来 / 不是 JSON 对象。拒绝覆盖。
    Corrupt(String),
    Io(String),
    /// 整份文档有多个非法已知字段。**只带键名**：错误会进日志、RPC 和前端，
    /// 不能把用户填的原文（可能含密钥）带出去。
    InvalidDocument {
        keys: Vec<String>,
    },
    /// 等实例写锁超过单调时钟上限（5 秒）。
    LockTimeout,
    /// 完整文件超过 1 MiB 上限。
    TooLarge,
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::InvalidSetting { key, reason } => {
                write!(f, "invalid setting {key}: {reason}")
            }
            SettingsError::Corrupt(reason) => write!(f, "settings.json is not writable: {reason}"),
            SettingsError::Io(reason) => write!(f, "settings.json io error: {reason}"),
            SettingsError::InvalidDocument { keys } => {
                write!(f, "invalid settings.json keys: {}", keys.join(", "))
            }
            SettingsError::LockTimeout => {
                write!(
                    f,
                    "settings.json is locked by another writer; try again later"
                )
            }
            SettingsError::TooLarge => write!(f, "settings.json exceeds the 1 MiB limit"),
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

    // 跨字段：远端窗口不得长于宽限窗口。`validate_value` 是逐字段无状态的，
    // 做不了这个判断，只能在这里做。
    //
    // 必须基于**合并后的完整对象**：用户可能只 patch 一个键（例如只把宽限改小），
    // 此时远端窗口的生效值来自文件里的旧值。
    //
    // 但**不能**在这里调 `normalized()`：它自带的倒挂回落会把倒挂值清成 None，
    // 于是下面的比较永远为假、任何倒挂都不会被拒绝。改为对两个字段各自做
    // 单字段钳制（`within` 按 FieldSpec.min/max 判界），越界即当默认。
    let merged_value = serde_json::Value::Object(object.clone());
    let merged = match serde_json::from_value::<Settings>(merged_value) {
        Ok(settings) => settings,
        Err(error) => {
            // `write_to` 只校验过「顶层是对象」，内层可能是结构性的坏
            // （比如 `"agent": 5`）。这与「文件存在却读不出来 → 拒绝覆盖」
            // 的守卫同向：宁可不写，也不把坏文件当默认值覆盖掉。
            return Err(SettingsError::Corrupt(format!(
                "the merged settings would not deserialize: {error}"
            )));
        }
    };

    // 只钳制、不做倒挂回落——否则本次要检查的倒挂会先被清掉。
    // 不复用 `power_windows_minutes`：它要一个完整的 `&Settings`，
    // 而这里手上只有两个 Option。直接算等价。
    let grace_minutes = merged
        .power
        .idle_grace_minutes
        .filter(|&value| within("power.idle_grace_minutes", value))
        .unwrap_or(POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote_minutes = merged
        .power
        .remote_touch_minutes
        .filter(|&value| within("power.remote_touch_minutes", value))
        .unwrap_or(POWER_REMOTE_TOUCH_MINUTES_DEFAULT);

    // 只在本次 patch 触碰了这两个键时才拒绝。否则文件里既有的倒挂会让用户
    // 连改 `agent.max_turns` 都失败——那是把人锁在文件外面。文件里的既有
    // 倒挂由加载侧的 `normalized()` 静默回落并 warn，不影响写入。
    let touches_power_windows = patch.contains_key("power.idle_grace_minutes")
        || patch.contains_key("power.remote_touch_minutes");
    if touches_power_windows && remote_minutes > grace_minutes {
        return Err(SettingsError::InvalidSetting {
            key: "power.remote_touch_minutes".to_string(),
            reason: format!(
                "must not exceed power.idle_grace_minutes \
                 (the remote window {remote_minutes} is longer than the grace \
                 {grace_minutes}, which would make the grace ineffective on battery)"
            ),
        });
    }

    // 删掉只剩空壳的分组对象，避免留下 {"agent":{}} 这种噪音。
    for group in ["agent", "compact", "startup", "tui", "power"] {
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
    if let Some(text) = value.as_str()
        && text.trim().is_empty()
    {
        return Ok(None);
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
            if let Some(min) = spec.min
                && number < min
            {
                return Err(reject(format!("must be at least {min}")));
            }
            if let Some(max) = spec.max
                && number > max
            {
                return Err(reject(format!("must be at most {max}")));
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
    let tmp = dir.join(format!("settings.json.{}.{}.tmp", std::process::id(), seq));
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
        use crate::settings::{
            POWER_IDLE_GRACE_MINUTES_DEFAULT, POWER_REMOTE_TOUCH_MINUTES_DEFAULT,
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
            (
                "power.idle_grace_minutes",
                POWER_IDLE_GRACE_MINUTES_DEFAULT.to_string(),
            ),
            (
                "power.remote_touch_minutes",
                POWER_REMOTE_TOUCH_MINUTES_DEFAULT.to_string(),
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
        assert_eq!(
            agent.len(),
            1,
            "only max_turns should be present: {agent:?}"
        );
        assert!(
            value.get("compact").is_none(),
            "compact must not be materialized"
        );
        assert!(
            value.get("startup").is_none(),
            "startup must not be materialized"
        );
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
        assert!(
            value
                .get("agent")
                .and_then(|a| a.get("max_turns"))
                .is_none()
        );
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
            (
                "agent.max_direct_children",
                serde_json::json!(u16::MAX as u64 + 1),
            ),
            (
                "agent.max_resident_subagents",
                serde_json::json!(u16::MAX as u64 + 1),
            ),
            (
                "agent.skills_catalog_budget",
                serde_json::json!(u32::MAX as u64 + 1),
            ),
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
        let error = write_to(&path, &serde_json::json!({"startup.sandbox": "bogus"})).unwrap_err();
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

        assert_eq!(
            settings.agent.max_turns,
            Some(500),
            "a valid neighbour survives"
        );
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

    #[test]
    fn tui_defaults_are_all_none() {
        let settings = Settings::default();
        assert!(settings.tui.notifications.is_none());
        assert!(settings.tui.notification_method.is_none());
        assert!(settings.tui.notification_condition.is_none());
        assert!(settings.tui.terminal_title.is_none());
    }

    #[test]
    fn tui_fields_are_registered() {
        for key in [
            "tui.notifications",
            "tui.notification_method",
            "tui.notification_condition",
            "tui.terminal_title",
        ] {
            assert!(
                find_spec(key).is_some(),
                "{key} must be in FIELDS or the settings UI will not show it"
            );
        }
    }

    #[test]
    fn invalid_notification_method_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"tui":{"notification_method":"carrier-pigeon","notifications":"false"},
                "agent":{"max_turns":42}}"#,
        )
        .unwrap();

        let settings = load_from(&path);
        // 非法值被清成 None（回退默认），而不是让整份文件失败。
        assert_eq!(settings.tui.notification_method, None);
        // 同一份文件里的其它字段必须还在。
        assert_eq!(settings.tui.notifications.as_deref(), Some("false"));
        assert_eq!(settings.agent.max_turns, Some(42));
    }

    #[test]
    fn invalid_notification_condition_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"notification_condition":"sometimes"}}"#).unwrap();
        assert_eq!(load_from(&path).tui.notification_condition, None);
    }

    #[test]
    fn invalid_notifications_flag_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"notifications":"maybe"}}"#).unwrap();
        assert_eq!(load_from(&path).tui.notifications, None);
    }

    #[test]
    fn valid_notification_values_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"tui":{"notifications":"true","notification_method":"osc9","notification_condition":"always"}}"#,
        )
        .unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.tui.notifications.as_deref(), Some("true"));
        assert_eq!(settings.tui.notification_method.as_deref(), Some("osc9"));
        assert_eq!(
            settings.tui.notification_condition.as_deref(),
            Some("always")
        );
    }

    #[test]
    fn invalid_terminal_title_flag_is_dropped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"terminal_title":"maybe"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(
            settings.tui.terminal_title, None,
            "an unknown value must fall back to the default, not abort"
        );
    }

    #[test]
    fn valid_terminal_title_values_survive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"tui":{"terminal_title":"false"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.tui.terminal_title.as_deref(), Some("false"));
    }

    #[test]
    fn deleting_all_tui_keys_removes_the_empty_group() {
        // 写三个键再全部删掉，文件里不该留下 {"tui":{}} 这种空壳。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_to(
            &path,
            &serde_json::json!({
                "tui.notifications": "true",
                "tui.notification_method": "bel",
            }),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"tui\""), "group must exist: {text}");

        write_to(
            &path,
            &serde_json::json!({
                "tui.notifications": null,
                "tui.notification_method": null,
            }),
        )
        .unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(
            value.get("tui").is_none(),
            "empty tui group must be pruned, got {text}"
        );
    }

    #[test]
    fn prevent_idle_sleep_defaults_to_enabled() {
        let settings = Settings::default();
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    #[test]
    fn prevent_idle_sleep_honours_false() {
        let mut settings = Settings::default();
        settings.power.prevent_idle_sleep = Some("false".to_string());
        assert!(!prevent_idle_sleep_enabled(&settings));
    }

    /// 未知值在 `normalized` 里被清成 `None`，于是读取端按默认（开）处理。
    #[test]
    fn an_unknown_prevent_idle_sleep_falls_back_to_enabled() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":"yes"}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep, None);
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// 空串等于未设置。
    #[test]
    fn an_empty_prevent_idle_sleep_is_unset() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"agent":{"prevent_idle_sleep":""}}"#).unwrap();
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep, None);
    }

    /// 整份 JSON 坏掉时 `load_from` 返回默认值，即开关回到「开」。
    ///
    /// 方向是刻意的：坏文件下**不能**让保活失效（那正是「夜里跑着跑着睡了」
    /// 的原始故障），所以兜底偏向「继续保活」。用户在设置页重新保存一次即可
    /// 修正文件。
    #[test]
    fn a_corrupt_settings_file_still_enables_prevention() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let settings = load_from(&path);
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// `"FALSE"` / `"False"` 也算关闭——大小写不敏感。
    /// 设置页写的是小写，但用户手改文件时不该因为大小写就悄悄打开。
    #[test]
    fn prevent_idle_sleep_is_case_insensitive() {
        for raw in ["false", "FALSE", "False"] {
            let mut settings = Settings::default();
            settings.power.prevent_idle_sleep = Some(raw.to_string());
            assert!(!prevent_idle_sleep_enabled(&settings), "{raw} must disable");
        }
    }

    #[test]
    fn power_windows_default_to_fifteen_and_five() {
        let (grace, remote) = power_windows_minutes(&Settings::default());
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
        assert_eq!((grace, remote), (15, 5));
    }

    /// 两个值各自合法但倒挂（宽限 5、远端 30）→ 两个都回默认。
    ///
    /// 只清远端会留下「宽限 5 单独生效」，用户看到的仍是自己设的 5 分钟，
    /// 而文件里那对矛盾值会一直在，每次启动 warn 一次。两个一起回默认，
    /// 行为单调且可解释。
    #[test]
    fn power_windows_inverted_pair_falls_back_to_both_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    }

    /// 相等是合法的：远端窗口在宽限到期的同一刻失效，语义自洽。
    #[test]
    fn power_windows_allow_equal_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":10,"remote_touch_minutes":10}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!((grace, remote), (10, 10));
    }

    #[test]
    fn power_windows_read_through_from_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"power":{"idle_grace_minutes":45,"remote_touch_minutes":20}}"#,
        )
        .unwrap();
        let (grace, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!((grace, remote), (45, 20));
    }

    #[test]
    fn migrate_moves_the_legacy_key_into_power() {
        let mut value = serde_json::json!({"agent":{"prevent_idle_sleep":"false"}});
        migrate_legacy_keys(&mut value);
        assert_eq!(value["power"]["prevent_idle_sleep"], "false");
        // 旧键必须被删掉，否则幂等性靠的是「新键已存在」这个间接条件。
        assert!(
            value["agent"].get("prevent_idle_sleep").is_none(),
            "the legacy key must be removed, got {value}"
        );
        // agent 变空壳则整组删掉，不留 {"agent":{}} 噪音。
        assert!(
            value.get("agent").is_none(),
            "an emptied agent group must be pruned, got {value}"
        );
    }

    #[test]
    fn migrate_keeps_an_explicit_power_key() {
        let mut value = serde_json::json!({
            "agent": {"prevent_idle_sleep": "false"},
            "power": {"prevent_idle_sleep": "true"}
        });
        migrate_legacy_keys(&mut value);
        // 新键优先：不能被旧的 false 覆盖。
        assert_eq!(value["power"]["prevent_idle_sleep"], "true");
    }

    #[test]
    fn migrate_twice_is_a_no_op() {
        let mut value = serde_json::json!({"agent":{"prevent_idle_sleep":"false"}});
        migrate_legacy_keys(&mut value);
        let once = value.clone();
        migrate_legacy_keys(&mut value);
        assert_eq!(value, once, "migration must be idempotent");
    }

    fn write_json(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("settings.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// 造真实旧格式文件，走完整的「读 → 解析 → 迁移 → 反序列化 → normalized」。
    #[test]
    fn migrate_the_old_agent_key_into_the_power_group() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep.as_deref(), Some("false"));
        assert!(!prevent_idle_sleep_enabled(&settings));
    }

    #[test]
    fn migrate_twice_on_disk_is_stable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let first = load_from(&path);
        let second = load_from(&path);
        assert_eq!(first, second);
        assert_eq!(first.power.prevent_idle_sleep.as_deref(), Some("false"));
    }

    /// 两个键都在且冲突时，`power` 的值生效。
    #[test]
    fn migrate_keeps_an_explicit_power_key_on_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"agent":{"prevent_idle_sleep":"false"},"power":{"prevent_idle_sleep":"true"}}"#,
        );
        let settings = load_from(&path);
        assert_eq!(settings.power.prevent_idle_sleep.as_deref(), Some("true"));
        assert!(prevent_idle_sleep_enabled(&settings));
    }

    /// 写入被拒时文件必须**逐字节不变**。
    /// 不断言 mtime：同一秒内两次写入的 mtime 可能一样，APFS 的粗粒度
    /// 时间戳会让这条断言假通过。字节比对才是「零写入」的真正判据。
    #[test]
    fn power_windows_reject_a_remote_window_longer_than_the_grace() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":5}}"#,
        );
        let before = std::fs::read(&path).unwrap();

        // 只 patch 宽限（5 → 60 会合法；这里相反：把宽限压到 2，远端 5 就倒挂了）。
        let patch = serde_json::json!({"power.idle_grace_minutes": 2});
        let error = write_to(&path, &patch).unwrap_err();
        match error {
            SettingsError::InvalidSetting { key, reason } => {
                assert_eq!(key, "power.remote_touch_minutes");
                assert!(
                    reason.contains("must not exceed"),
                    "reason should explain the cross-field rule, got {reason}"
                );
            }
            other => panic!("expected InvalidSetting, got {other:?}"),
        }

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after, before, "a rejected write must not touch the file");
    }

    /// 旧键不会因为「保存了别的字段」而消失——这是预期行为，不是 bug。
    /// `write_to` 只对 patch 里出现的键调 `apply_op`，其余原样保留。
    #[test]
    fn migrate_legacy_key_stays_on_disk_after_other_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        write_to(&path, &serde_json::json!({"power.idle_grace_minutes": 30})).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("agent"),
            "the legacy key must survive an unrelated write, got {text}"
        );
        assert!(text.contains("prevent_idle_sleep"), "got {text}");
        // 而新值确实写进去了。
        assert!(text.contains("idle_grace_minutes"), "got {text}");
    }

    /// 迁移结果可重放：把迁好的 Settings 整份写回，再读一次必须相等。
    /// 若迁移只在内存搬、而值本身落不进新键，第二次读会退回默认（开）。
    #[test]
    fn migrate_value_survives_a_fresh_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(&dir, r#"{"agent":{"prevent_idle_sleep":"false"}}"#);
        let first = load_from(&path);
        assert_eq!(first.power.prevent_idle_sleep.as_deref(), Some("false"));

        std::fs::write(&path, serde_json::to_string(&first).unwrap()).unwrap();
        let second = load_from(&path);
        assert_eq!(first, second, "a migrated value must survive a rewrite");
        assert_eq!(second.power.prevent_idle_sleep.as_deref(), Some("false"));
    }

    /// `clamp!` 是逐字段列举的，新增字段漏了它不会报错、越界值会一路进
    /// 策略层。这条测试是那两个 `clamp!` 的唯一问责令。
    ///
    /// 它必须放在本 Task 而不是 Task 3：`within` 靠 `find_spec` 取 min/max，
    /// `FIELDS` 里没有 `power.*` 时 `within` 返回 true、`clamp!` 清不掉任何值。
    #[test]
    fn power_windows_out_of_range_fall_back_to_defaults() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        // 下限：0 被清成 None。
        std::fs::write(&path, r#"{"power":{"idle_grace_minutes":0}}"#).unwrap();
        let (grace, _) = power_windows_minutes(&load_from(&path));
        assert_eq!(
            grace, POWER_IDLE_GRACE_MINUTES_DEFAULT,
            "0 must be clamped away, not passed through"
        );

        // 上限：121 > 120 被清成 None。
        std::fs::write(&path, r#"{"power":{"idle_grace_minutes":121}}"#).unwrap();
        let (grace, _) = power_windows_minutes(&load_from(&path));
        assert_eq!(grace, POWER_IDLE_GRACE_MINUTES_DEFAULT);

        // 远端窗口上限 60。
        std::fs::write(&path, r#"{"power":{"remote_touch_minutes":61}}"#).unwrap();
        let (_, remote) = power_windows_minutes(&load_from(&path));
        assert_eq!(remote, POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    }

    /// 文件里已有一对倒挂的手改值（宽限 5、远端 30）时，改**无关**字段
    /// 仍须成功。否则用户手改坏了一对值之后就再也存不进任何设置——
    /// 那是把人锁在文件外面。倒挂本身由加载侧 `normalized()` 回落。
    #[test]
    fn an_unrelated_write_succeeds_despite_an_inverted_pair_on_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        );
        write_to(&path, &serde_json::json!({"agent.max_turns": 123})).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("123"),
            "the unrelated write must land, got {text}"
        );
    }

    /// 但触碰这对窗口就被拒——哪怕只是其中一个键。
    #[test]
    fn touching_an_inverted_pair_is_still_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_json(
            &dir,
            r#"{"power":{"idle_grace_minutes":5,"remote_touch_minutes":30}}"#,
        );
        let error =
            write_to(&path, &serde_json::json!({"power.idle_grace_minutes": 2})).unwrap_err();
        assert!(
            matches!(error, SettingsError::InvalidSetting { .. }),
            "touching the inverted pair must be rejected, got {error:?}"
        );
    }
}
