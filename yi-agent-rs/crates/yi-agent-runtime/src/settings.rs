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
    InvalidSetting {
        key: String,
        reason: String,
    },
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
}
