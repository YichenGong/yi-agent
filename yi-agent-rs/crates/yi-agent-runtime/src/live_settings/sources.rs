//! 固定来源解析：显式 override > 真实启动环境变量 > 当前 settings > 默认值。
//!
//! 本模块只做**数据**：`capture` 在初始装配时把真实存在的 override / env 取值
//! 冻结成一张 FIELDS 标量表，`resolve` 再把它与**当前** settings 快照合并成
//! 一次可用的有效值。运行期设置变化只需重新 `resolve`，不需要重建本表。
//!
//! 三条硬约束（见 spec 与 Task 3 brief）：
//! 1. 只保留真实存在的来源。初次合并的结果、父会话默认值都不会被固化成 override。
//! 2. 模型 / 凭据键永不进入本表：本表只描述 FIELDS 的运行标量。
//! 3. 两个 Frozen 类型不提供 serde 接口，手写 `Debug` 只显示键名与来源，绝不
//!    显示取值——值里可能有提示词。

use std::collections::BTreeMap;

use serde_json::Value;

use crate::config::{self, ConfigOverrides};
use crate::settings::{FIELDS, FieldType, Settings};

/// 一个值的固定来源。
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    /// CLI 覆盖项（或 GUI 请求显式传入的覆盖）。
    Override,
    /// 真实进程环境变量。
    Environment,
    /// 当前 settings 快照（只在 [`EffectiveRuntimeSettings`] 里出现）。
    Settings,
    /// 字段默认值（只在 [`EffectiveRuntimeSettings`] 里出现）。
    Default,
}

/// 一个被冻结的值及其来源。
#[derive(Clone, PartialEq, Eq)]
pub struct FrozenRuntimeValue {
    pub value: Value,
    pub source: ValueSource,
}

impl std::fmt::Debug for FrozenRuntimeValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrozenRuntimeValue")
            .field("value", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

/// 初始装配时冻结的 override / env 取值表。
///
/// 空表是合法默认：显式无环境测试与宿主可用 `Default` 表示「没有任何固定来源」。
#[derive(Clone, Default)]
pub struct FrozenRuntimeSources {
    values: BTreeMap<String, FrozenRuntimeValue>,
}

impl std::fmt::Debug for FrozenRuntimeSources {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只暴露键名与来源：取值可能包含提示词等用户内容。
        let keys: BTreeMap<&str, ValueSource> = self
            .values
            .iter()
            .map(|(key, frozen)| (key.as_str(), frozen.source))
            .collect();
        formatter
            .debug_struct("FrozenRuntimeSources")
            .field("values", &keys)
            .finish()
    }
}

/// 一次合并的结果：16 个运行字段的有效值与各自来源。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveRuntimeSettings {
    pub values: BTreeMap<String, Value>,
    pub sources: BTreeMap<String, ValueSource>,
}

impl FrozenRuntimeSources {
    /// 从覆盖项与真实进程环境变量抽取适用的固定来源。
    ///
    /// 只读取 FIELDS 里存在 override / env 入口的键；每字段的解析规则与
    /// 旧 `config.rs` 完全一致（数字按目标宽度解析，非法值忽略回落；
    /// `YI_AGENT_SANDBOX` 非法仍然报错；空 `system_prompt` 记为 `null`，
    /// 表示「显式清空」，因此能压过 settings）。
    ///
    /// 返回前调用 [`Self::validate`]。
    pub fn capture(overrides: &ConfigOverrides) -> anyhow::Result<Self> {
        let mut values = BTreeMap::new();

        // ---- 覆盖项层 ----
        if let Some(value) = overrides.max_turns {
            insert_override(&mut values, "agent.max_turns", Value::from(value));
        }
        if let Some(value) = overrides.system_prompt.clone() {
            insert_override(
                &mut values,
                "agent.system_prompt",
                if value.is_empty() {
                    Value::Null
                } else {
                    Value::String(value)
                },
            );
        }
        if let Some(value) = overrides.skills_catalog_budget {
            insert_override(
                &mut values,
                "agent.skills_catalog_budget",
                Value::from(value as u64),
            );
        }
        if let Some(value) = overrides.compact_ratio {
            insert_override(&mut values, "compact.ratio", Value::from(value));
        }
        if let Some(value) = overrides.compact_user_budget_tokens {
            insert_override(
                &mut values,
                "compact.user_budget_tokens",
                Value::from(value as u64),
            );
        }
        if let Some(value) = overrides.compact_tool_budget_tokens {
            insert_override(
                &mut values,
                "compact.tool_budget_tokens",
                Value::from(value as u64),
            );
        }
        if let Some(mode) = overrides.sandbox {
            insert_override(
                &mut values,
                "startup.sandbox",
                Value::String(mode.as_str().to_string()),
            );
        }

        // ---- 真实环境变量层（同一键已有 override 时不再读）----
        if !values.contains_key("agent.max_turns")
            && let Some(value) = env_parse::<u32>("YI_AGENT_MAX_TURNS")
        {
            insert_environment(&mut values, "agent.max_turns", Value::from(value));
        }
        if !values.contains_key("agent.max_direct_children")
            && let Some(value) = env_parse::<u16>("YI_AGENT_MAX_DIRECT_CHILDREN")
        {
            insert_environment(&mut values, "agent.max_direct_children", Value::from(value));
        }
        if !values.contains_key("agent.max_resident_subagents")
            && let Some(value) = env_parse::<u16>("YI_AGENT_MAX_RESIDENT_SUBAGENTS")
        {
            insert_environment(
                &mut values,
                "agent.max_resident_subagents",
                Value::from(value),
            );
        }
        if !values.contains_key("agent.system_prompt")
            && let Ok(value) = std::env::var("YI_AGENT_SYSTEM_PROMPT")
        {
            insert_environment(
                &mut values,
                "agent.system_prompt",
                if value.is_empty() {
                    Value::Null
                } else {
                    Value::String(value)
                },
            );
        }
        if !values.contains_key("agent.skills_catalog_budget")
            && let Some(value) = env_parse::<usize>("YI_AGENT_SKILLS_CATALOG_BUDGET")
        {
            insert_environment(
                &mut values,
                "agent.skills_catalog_budget",
                Value::from(value as u64),
            );
        }
        if !values.contains_key("compact.ratio")
            && let Some(value) = env_parse::<u32>("YI_AGENT_COMPACT_RATIO")
        {
            insert_environment(&mut values, "compact.ratio", Value::from(value));
        }
        if !values.contains_key("compact.user_budget_tokens")
            && let Some(value) = env_parse::<usize>("YI_AGENT_COMPACT_USER_BUDGET_TOKENS")
        {
            insert_environment(
                &mut values,
                "compact.user_budget_tokens",
                Value::from(value as u64),
            );
        }
        if !values.contains_key("compact.tool_budget_tokens")
            && let Some(value) = env_parse::<usize>("YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS")
        {
            insert_environment(
                &mut values,
                "compact.tool_budget_tokens",
                Value::from(value as u64),
            );
        }
        if !values.contains_key("startup.sandbox")
            && let Ok(value) = std::env::var("YI_AGENT_SANDBOX")
        {
            // 沙箱与数字字段不同：非法值在旧实现里是**启动错误**，不是忽略回落。
            let mode = config::parse_sandbox_mode(&value).ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid YI_AGENT_SANDBOX: expected read-only, workspace-write, or danger-full-access"
                )
            })?;
            insert_environment(
                &mut values,
                "startup.sandbox",
                Value::String(mode.as_str().to_string()),
            );
        }

        let sources = Self { values };
        sources.validate()?;
        Ok(sources)
    }

    /// 校验固定来源表：只接受适用的 override / environment 来源、已知键，
    /// 以及能被既有字段解析器接受的取值形状。
    ///
    /// 这里**不**施加 settings 写入侧的 min/max：真实 CLI / env 历史上接受
    /// `YI_AGENT_MAX_TURNS=0` 这类零值，套上写入范围会把既有启动语义变成错误。
    /// 文件候选仍由 `settings::document` 严格校验。
    pub fn validate(&self) -> anyhow::Result<()> {
        for (key, frozen) in &self.values {
            let spec = crate::settings::find_spec(key).ok_or_else(|| {
                anyhow::anyhow!("frozen runtime source names unknown key `{key}`")
            })?;
            match frozen.source {
                ValueSource::Override | ValueSource::Environment => {}
                ValueSource::Settings | ValueSource::Default => {
                    anyhow::bail!(
                        "frozen runtime source `{key}` claims a non-fixed source {:?}",
                        frozen.source
                    );
                }
            }
            if is_model_or_credential_key(key) {
                anyhow::bail!("model or credential key `{key}` must never be frozen");
            }
            match spec.field_type {
                FieldType::Number => {
                    let number = frozen
                        .value
                        .as_u64()
                        .ok_or_else(|| anyhow::anyhow!("frozen `{key}` must be a JSON number"))?;
                    if !number_fits_key(key, number) {
                        anyhow::bail!("frozen `{key}` value is out of range for its type");
                    }
                }
                FieldType::Text => {
                    if !(frozen.value.is_null() || frozen.value.is_string()) {
                        anyhow::bail!("frozen `{key}` must be a string or null");
                    }
                }
                FieldType::Select => {
                    let text = frozen
                        .value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("frozen `{key}` must be a JSON string"))?;
                    if !spec.options.contains(&text) {
                        anyhow::bail!("frozen `{key}` is not one of the documented options");
                    }
                }
            }
        }
        Ok(())
    }

    /// 与当前 settings 快照合并成一次有效值。
    ///
    /// 每个 FIELDS 键都一定出现在结果里：固定来源优先，其次 settings，
    /// 最后字段默认值。`sources` 记录实际生效的那一层。
    ///
    /// 只接受经 [`Self::capture`] / [`Self::validate`] 构造的实例；`Default` 是
    /// 空表，等价于「没有固定来源」。
    pub fn resolve(&self, settings: &Settings) -> EffectiveRuntimeSettings {
        let mut values = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for field in FIELDS {
            let (value, source) = match self.values.get(field.key) {
                Some(frozen) => (frozen.value.clone(), frozen.source),
                None => match settings_value(settings, field.key) {
                    Some(value) => (value, ValueSource::Settings),
                    None => (default_value(field.key), ValueSource::Default),
                },
            };
            values.insert(field.key.to_string(), value);
            sources.insert(field.key.to_string(), source);
        }
        EffectiveRuntimeSettings { values, sources }
    }

    /// 测试专用：查一个冻结项（生产代码只经 `resolve` 使用本表）。
    #[cfg(test)]
    pub(crate) fn frozen(&self, key: &str) -> Option<&FrozenRuntimeValue> {
        self.values.get(key)
    }

    /// Build a frozen table directly.
    ///
    /// Production code obtains this from [`FrozenRuntimeSources::capture`],
    /// which validates. This constructor exists so callers in *other* crates
    /// (the app-server settings RPC tests) can inject a known override without
    /// mutating the real process environment, which parallel tests share.
    ///
    /// It does **not** validate: the runtime's own tests build deliberately
    /// malformed tables here to exercise [`FrozenRuntimeSources::validate`], and
    /// asserting would make those tests unfalsifiable. Callers assembling a
    /// table that will actually be used must call `validate()` themselves.
    pub fn from_pairs(pairs: &[(&str, Value, ValueSource)]) -> Self {
        let values = pairs
            .iter()
            .map(|(key, value, source)| {
                (
                    (*key).to_string(),
                    FrozenRuntimeValue {
                        value: value.clone(),
                        source: *source,
                    },
                )
            })
            .collect();
        Self { values }
    }
}

fn insert_override(values: &mut BTreeMap<String, FrozenRuntimeValue>, key: &str, value: Value) {
    values.insert(
        key.to_string(),
        FrozenRuntimeValue {
            value,
            source: ValueSource::Override,
        },
    );
}

fn insert_environment(values: &mut BTreeMap<String, FrozenRuntimeValue>, key: &str, value: Value) {
    values.insert(
        key.to_string(),
        FrozenRuntimeValue {
            value,
            source: ValueSource::Environment,
        },
    );
}

/// 与旧加载器同一口径：读不到或解析失败都返回 `None`，由下一层接手。
fn env_parse<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
}

/// 取值能否装进该字段的 Rust 宽度。宽度由旧实现的反序列化类型决定。
fn number_fits_key(key: &str, value: u64) -> bool {
    match key {
        "agent.max_turns" | "compact.ratio" => u32::try_from(value).is_ok(),
        "agent.max_direct_children" | "agent.max_resident_subagents" => {
            u16::try_from(value).is_ok()
        }
        "agent.skills_catalog_budget"
        | "compact.user_budget_tokens"
        | "compact.tool_budget_tokens" => usize::try_from(value).is_ok(),
        // u64 -> u64 always fits;列出这两个键是为了让宽度来源完整可读。
        "power.idle_grace_minutes" | "power.remote_touch_minutes" => true,
        _ => false,
    }
}

/// 模型 / 凭据键永不允许被冻结。只匹配明确的模式，避免误伤
/// `compact.*_tokens` 之类含 "token" 的运行字段。
fn is_model_or_credential_key(key: &str) -> bool {
    key.contains("api_key")
        || key.contains("secret")
        || key.contains("credential")
        || key == "provider"
        || key == "model"
        || key.starts_with("model.")
        || key.ends_with(".model")
}

/// settings 快照里的规范值；`None` = 该键未设置。
fn settings_value(settings: &Settings, key: &str) -> Option<Value> {
    match key {
        "agent.max_turns" => settings.agent.max_turns.map(Value::from),
        "agent.max_direct_children" => settings.agent.max_direct_children.map(Value::from),
        "agent.max_resident_subagents" => settings.agent.max_resident_subagents.map(Value::from),
        "agent.system_prompt" => settings.agent.system_prompt.clone().map(Value::String),
        "agent.skills_catalog_budget" => settings
            .agent
            .skills_catalog_budget
            .map(|v| Value::from(v as u64)),
        "compact.ratio" => settings.compact.ratio.map(Value::from),
        "compact.user_budget_tokens" => settings
            .compact
            .user_budget_tokens
            .map(|v| Value::from(v as u64)),
        "compact.tool_budget_tokens" => settings
            .compact
            .tool_budget_tokens
            .map(|v| Value::from(v as u64)),
        "startup.sandbox" => settings.startup.sandbox.clone().map(Value::String),
        "tui.notifications" => settings.tui.notifications.clone().map(Value::String),
        "tui.terminal_title" => settings.tui.terminal_title.clone().map(Value::String),
        "tui.notification_method" => settings.tui.notification_method.clone().map(Value::String),
        "tui.notification_condition" => settings
            .tui
            .notification_condition
            .clone()
            .map(Value::String),
        "power.prevent_idle_sleep" => settings.power.prevent_idle_sleep.clone().map(Value::String),
        "power.idle_grace_minutes" => settings.power.idle_grace_minutes.map(Value::from),
        "power.remote_touch_minutes" => settings.power.remote_touch_minutes.map(Value::from),
        _ => None,
    }
}

/// 字段默认值的 JSON 形状：数字是 number，布尔选项仍是字符串，
/// 未设置的 `system_prompt` 是 null。
fn default_value(key: &str) -> Value {
    match key {
        "agent.max_turns" => Value::from(config::MAX_TURNS_DEFAULT),
        "agent.max_direct_children" => Value::from(config::DIRECT_CHILDREN_DEFAULT),
        "agent.max_resident_subagents" => Value::from(config::RESIDENT_SUBAGENTS_DEFAULT),
        "agent.system_prompt" => Value::Null,
        "agent.skills_catalog_budget" => Value::from(config::SKILLS_CATALOG_BUDGET_DEFAULT as u64),
        "compact.ratio" => Value::from(config::COMPACT_RATIO_DEFAULT),
        "compact.user_budget_tokens" => Value::from(config::COMPACT_USER_BUDGET_DEFAULT as u64),
        "compact.tool_budget_tokens" => Value::from(config::COMPACT_TOOL_BUDGET_DEFAULT as u64),
        "startup.sandbox" => {
            Value::String(yi_agent_tools::SandboxMode::default().as_str().to_string())
        }
        "tui.notifications" => Value::String("true".to_string()),
        "tui.terminal_title" => Value::String("true".to_string()),
        "tui.notification_method" => Value::String("auto".to_string()),
        "tui.notification_condition" => Value::String("unfocused".to_string()),
        "power.prevent_idle_sleep" => Value::String("true".to_string()),
        "power.idle_grace_minutes" => {
            Value::from(crate::settings::POWER_IDLE_GRACE_MINUTES_DEFAULT)
        }
        "power.remote_touch_minutes" => {
            Value::from(crate::settings::POWER_REMOTE_TOUCH_MINUTES_DEFAULT)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;

    /// 全 16 字段的表驱动覆盖：默认、settings、override/env、逐级撤掉。
    #[test]
    fn every_runtime_field_resolves_through_all_four_layers() {
        // (键, 默认 JSON, settings 能表达的值)
        // 默认值全部引用 config 常量，绝不硬编码第二份默认。
        let cases: Vec<(&str, Value, Option<Value>)> = vec![
            (
                "agent.max_turns",
                Value::from(config::MAX_TURNS_DEFAULT),
                Some(Value::from(37u32)),
            ),
            (
                "agent.max_direct_children",
                Value::from(config::DIRECT_CHILDREN_DEFAULT),
                Some(Value::from(5u16)),
            ),
            (
                "agent.max_resident_subagents",
                Value::from(config::RESIDENT_SUBAGENTS_DEFAULT),
                Some(Value::from(12u16)),
            ),
            (
                "agent.system_prompt",
                Value::Null,
                Some(Value::String("from settings".into())),
            ),
            (
                "agent.skills_catalog_budget",
                Value::from(config::SKILLS_CATALOG_BUDGET_DEFAULT as u64),
                Some(Value::from(4096u64)),
            ),
            (
                "compact.ratio",
                Value::from(config::COMPACT_RATIO_DEFAULT),
                Some(Value::from(55u32)),
            ),
            (
                "compact.user_budget_tokens",
                Value::from(config::COMPACT_USER_BUDGET_DEFAULT as u64),
                Some(Value::from(7000u64)),
            ),
            (
                "compact.tool_budget_tokens",
                Value::from(config::COMPACT_TOOL_BUDGET_DEFAULT as u64),
                Some(Value::from(8000u64)),
            ),
            (
                "startup.sandbox",
                Value::String("workspace-write".into()),
                Some(Value::String("read-only".into())),
            ),
            (
                "tui.notifications",
                Value::String("true".into()),
                Some(Value::String("false".into())),
            ),
            (
                "tui.terminal_title",
                Value::String("true".into()),
                Some(Value::String("false".into())),
            ),
            (
                "tui.notification_method",
                Value::String("auto".into()),
                Some(Value::String("bel".into())),
            ),
            (
                "tui.notification_condition",
                Value::String("unfocused".into()),
                Some(Value::String("always".into())),
            ),
            (
                "power.prevent_idle_sleep",
                Value::String("true".into()),
                Some(Value::String("false".into())),
            ),
            (
                "power.idle_grace_minutes",
                Value::from(crate::settings::POWER_IDLE_GRACE_MINUTES_DEFAULT),
                Some(Value::from(30u64)),
            ),
            (
                "power.remote_touch_minutes",
                Value::from(crate::settings::POWER_REMOTE_TOUCH_MINUTES_DEFAULT),
                Some(Value::from(10u64)),
            ),
        ];
        assert_eq!(
            cases.len(),
            FIELDS.len(),
            "the table must cover every field"
        );

        for (key, default, settings_value) in cases {
            // 1) 什么都没有 -> default
            let empty = FrozenRuntimeSources::default();
            let resolved = empty.resolve(&Settings::default());
            assert_eq!(resolved.values[key], default, "default for {key}");
            assert_eq!(
                resolved.sources[key],
                ValueSource::Default,
                "source for {key}"
            );

            // 2) settings 有值 -> settings
            let settings = settings_with(key, settings_value.clone().unwrap());
            let resolved = empty.resolve(&settings);
            assert_eq!(
                resolved.values[key],
                settings_value.clone().unwrap(),
                "settings wins for {key}"
            );
            assert_eq!(resolved.sources[key], ValueSource::Settings);

            // 3) 固定来源压过 settings
            let fixed_value = fixed_probe_value(key);
            let frozen = FrozenRuntimeSources::from_pairs(&[(
                key,
                fixed_value.clone(),
                ValueSource::Environment,
            )]);
            let resolved = frozen.resolve(&settings);
            assert_eq!(resolved.values[key], fixed_value, "fixed wins for {key}");
            assert_eq!(resolved.sources[key], ValueSource::Environment);

            // 4) 撤掉固定来源 -> 回到 settings，再撤掉 settings -> 默认
            let resolved = FrozenRuntimeSources::default().resolve(&Settings::default());
            assert_eq!(resolved.values[key], default, "delete falls back for {key}");
        }
    }

    /// 覆盖项优先于环境变量；构建固定表本身不读 settings。
    #[test]
    fn capture_keeps_override_above_environment_and_never_reads_settings() {
        let frozen = FrozenRuntimeSources::from_pairs(&[
            ("agent.max_turns", Value::from(11u32), ValueSource::Override),
            (
                "compact.ratio",
                Value::from(60u32),
                ValueSource::Environment,
            ),
        ]);

        let mut settings = Settings::default();
        settings.agent.max_turns = Some(99);
        settings.compact.ratio = Some(45);

        let resolved = frozen.resolve(&settings);
        assert_eq!(resolved.values["agent.max_turns"], Value::from(11u32));
        assert_eq!(resolved.sources["agent.max_turns"], ValueSource::Override);
        assert_eq!(resolved.values["compact.ratio"], Value::from(60u32));
        assert_eq!(resolved.sources["compact.ratio"], ValueSource::Environment);
    }

    /// 运行期换一份 settings 快照，固定来源不变、其余字段跟着走。
    ///
    /// 抓的破坏：把初次 merge 的 settings 结果固化成 override，于是热更新再也
    /// 看不到新值。
    #[test]
    fn resolve_is_recomputed_against_the_current_settings_snapshot() {
        let frozen = FrozenRuntimeSources::from_pairs(&[(
            "agent.max_turns",
            Value::from(11u32),
            ValueSource::Override,
        )]);

        let mut first = Settings::default();
        first.compact.ratio = Some(50);
        let resolved = frozen.resolve(&first);
        assert_eq!(resolved.values["compact.ratio"], Value::from(50u32));
        assert_eq!(resolved.sources["compact.ratio"], ValueSource::Settings);

        let mut second = Settings::default();
        second.compact.ratio = Some(70);
        let resolved = frozen.resolve(&second);
        assert_eq!(resolved.values["compact.ratio"], Value::from(70u32));
        assert_eq!(
            resolved.values["agent.max_turns"],
            Value::from(11u32),
            "the explicit override stays fixed"
        );
    }

    /// 两个 Frozen 类型的手写 Debug 不泄露任何取值。
    #[test]
    fn debug_redacts_values() {
        let frozen = FrozenRuntimeSources::from_pairs(&[
            (
                "agent.system_prompt",
                Value::String("SECRET-PROMPT-BODY".into()),
                ValueSource::Override,
            ),
            (
                "agent.max_turns",
                Value::from(4242u32),
                ValueSource::Environment,
            ),
        ]);
        let rendered = format!("{frozen:?}");
        assert!(rendered.contains("agent.system_prompt"));
        assert!(
            !rendered.contains("SECRET-PROMPT-BODY"),
            "Debug must not print values: {rendered}"
        );
        assert!(
            !rendered.contains("4242"),
            "Debug must not print numbers either: {rendered}"
        );

        let (_, frozen_value) = frozen
            .values
            .iter()
            .next()
            .expect("the frozen table is not empty");
        let rendered = format!("{frozen_value:?}");
        assert!(!rendered.contains("SECRET-PROMPT-BODY"));
        assert!(!rendered.contains("4242"));
    }

    /// 模型 / 凭据键不能伪装成运行标量进入固定表。
    #[test]
    fn validate_rejects_model_and_credential_keys() {
        for key in [
            "model.api_key",
            "provider",
            "agent.model",
            "credentials.secret",
            "unknown.key",
        ] {
            let frozen = FrozenRuntimeSources::from_pairs(&[(
                key,
                Value::String("sk-secret".into()),
                ValueSource::Override,
            )]);
            assert!(
                frozen.validate().is_err(),
                "`{key}` must be rejected as a frozen runtime source"
            );
        }
    }

    /// settings / default 不是「固定来源」，不能出现在冻结表里。
    #[test]
    fn validate_rejects_settings_and_default_sources() {
        for source in [ValueSource::Settings, ValueSource::Default] {
            let frozen =
                FrozenRuntimeSources::from_pairs(&[("agent.max_turns", Value::from(5u32), source)]);
            assert!(frozen.validate().is_err(), "{source:?} must be rejected");
        }
    }

    /// 零值与空串按旧 CLI/env 语义保留：这里不做 settings 写入侧范围校验。
    #[test]
    fn validate_keeps_legacy_zero_and_empty_semantics() {
        let frozen = FrozenRuntimeSources::from_pairs(&[
            (
                "agent.max_turns",
                Value::from(0u32),
                ValueSource::Environment,
            ),
            ("agent.system_prompt", Value::Null, ValueSource::Environment),
        ]);
        frozen
            .validate()
            .expect("zero turns and an explicit blank prompt are legal startup values");

        let resolved = frozen.resolve(&Settings::default());
        assert_eq!(resolved.values["agent.max_turns"], Value::from(0u32));
        assert_eq!(resolved.values["agent.system_prompt"], Value::Null);
    }

    /// 真实环境变量 / 覆盖项的优先级与解析回落，跑在真实进程环境上。
    ///
    /// `cargo test` 下环境变量进程内共享，故用互斥锁串行；实际断言仍走
    /// `capture`，不手搓固定表。
    #[test]
    fn capture_follows_real_cli_and_environment_semantics() {
        // 与 `config::tests` 共用同一把环境变量锁：两个模块都会写
        // `YI_AGENT_MAX_TURNS` 等键，各拿一把锁等于没锁。
        let _lock = crate::config::test_support::ENV_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let guard = EnvVarGuard::new([
            "YI_AGENT_MAX_TURNS",
            "YI_AGENT_MAX_DIRECT_CHILDREN",
            "YI_AGENT_SYSTEM_PROMPT",
            "YI_AGENT_SANDBOX",
            "YI_AGENT_COMPACT_RATIO",
        ]);

        // 非法数字忽略回落；合法数字生效。
        guard.set("YI_AGENT_MAX_TURNS", "not-a-number");
        guard.set("YI_AGENT_MAX_DIRECT_CHILDREN", "3");
        guard.set("YI_AGENT_COMPACT_RATIO", "0");
        guard.remove("YI_AGENT_SYSTEM_PROMPT");
        guard.remove("YI_AGENT_SANDBOX");
        let frozen = FrozenRuntimeSources::capture(&ConfigOverrides::default()).unwrap();
        let resolved = frozen.resolve(&Settings::default());
        assert_eq!(
            resolved.sources["agent.max_turns"],
            ValueSource::Default,
            "an unparsable env number must fall through"
        );
        assert_eq!(
            resolved.values["agent.max_direct_children"],
            Value::from(3u16)
        );
        assert_eq!(
            resolved.values["compact.ratio"],
            Value::from(0u32),
            "zero is kept: only the settings file gets range validation"
        );

        // 覆盖项压过环境变量。
        let overrides = ConfigOverrides {
            max_turns: Some(17),
            ..Default::default()
        };
        let frozen = FrozenRuntimeSources::capture(&overrides).unwrap();
        assert_eq!(
            frozen.resolve(&Settings::default()).values["agent.max_turns"],
            Value::from(17u32)
        );
        assert_eq!(
            frozen.resolve(&Settings::default()).sources["agent.max_turns"],
            ValueSource::Override
        );

        // 空 system_prompt 是「显式清空」，压过 settings。
        let guard = guard;
        guard.set("YI_AGENT_SYSTEM_PROMPT", "");
        let frozen = FrozenRuntimeSources::capture(&ConfigOverrides::default()).unwrap();
        let mut settings = Settings::default();
        settings.agent.system_prompt = Some("from settings".into());
        let resolved = frozen.resolve(&settings);
        assert_eq!(resolved.values["agent.system_prompt"], Value::Null);
        assert_eq!(
            resolved.sources["agent.system_prompt"],
            ValueSource::Environment
        );

        // 非法沙箱仍然是启动错误。
        guard.set("YI_AGENT_SANDBOX", "bogus");
        let error = FrozenRuntimeSources::capture(&ConfigOverrides::default()).unwrap_err();
        assert!(
            error.to_string().contains("invalid YI_AGENT_SANDBOX"),
            "unexpected error: {error}"
        );
    }

    /// 模型 / 凭据来源从不进入固定表，即便覆盖项里带着它们。
    #[test]
    fn capture_never_freezes_model_or_credentials() {
        let overrides = ConfigOverrides {
            provider: Some("openai".into()),
            api_key: Some("sk-should-not-be-frozen".into()),
            model: Some("gpt-x".into()),
            ..Default::default()
        };
        let frozen = FrozenRuntimeSources::capture(&overrides).unwrap();
        let rendered = format!("{frozen:?}");
        assert!(!rendered.contains("sk-should-not-be-frozen"));
        assert!(!rendered.contains("gpt-x"));
        assert!(!rendered.contains("openai"));
        assert_eq!(
            frozen.resolve(&Settings::default()).values.len(),
            FIELDS.len(),
            "the effective table only names FIELDS keys"
        );
    }

    fn settings_with(key: &str, value: Value) -> Settings {
        let mut settings = Settings::default();
        match key {
            "agent.max_turns" => settings.agent.max_turns = value.as_u64().map(|v| v as u32),
            "agent.max_direct_children" => {
                settings.agent.max_direct_children = value.as_u64().map(|v| v as u16)
            }
            "agent.max_resident_subagents" => {
                settings.agent.max_resident_subagents = value.as_u64().map(|v| v as u16)
            }
            "agent.system_prompt" => {
                settings.agent.system_prompt = value.as_str().map(String::from)
            }
            "agent.skills_catalog_budget" => {
                settings.agent.skills_catalog_budget = value.as_u64().map(|v| v as usize)
            }
            "compact.ratio" => settings.compact.ratio = value.as_u64().map(|v| v as u32),
            "compact.user_budget_tokens" => {
                settings.compact.user_budget_tokens = value.as_u64().map(|v| v as usize)
            }
            "compact.tool_budget_tokens" => {
                settings.compact.tool_budget_tokens = value.as_u64().map(|v| v as usize)
            }
            "startup.sandbox" => settings.startup.sandbox = value.as_str().map(String::from),
            "tui.notifications" => settings.tui.notifications = value.as_str().map(String::from),
            "tui.terminal_title" => settings.tui.terminal_title = value.as_str().map(String::from),
            "tui.notification_method" => {
                settings.tui.notification_method = value.as_str().map(String::from)
            }
            "tui.notification_condition" => {
                settings.tui.notification_condition = value.as_str().map(String::from)
            }
            "power.prevent_idle_sleep" => {
                settings.power.prevent_idle_sleep = value.as_str().map(String::from)
            }
            "power.idle_grace_minutes" => settings.power.idle_grace_minutes = value.as_u64(),
            "power.remote_touch_minutes" => settings.power.remote_touch_minutes = value.as_u64(),
            other => panic!("unhandled key {other}"),
        }
        settings
    }

    /// 与默认值、settings 值都不同的探针值，用来证明固定来源真的压过了它们。
    fn fixed_probe_value(key: &str) -> Value {
        match key {
            "agent.system_prompt" => Value::String("from override".into()),
            "startup.sandbox" => Value::String("danger-full-access".into()),
            "tui.notifications" | "tui.terminal_title" | "power.prevent_idle_sleep" => {
                Value::String("false".into())
            }
            "tui.notification_method" => Value::String("osc9".into()),
            "tui.notification_condition" => Value::String("always".into()),
            _ => Value::from(1u64),
        }
    }

    struct EnvVarGuard {
        original: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvVarGuard {
        fn new(names: impl IntoIterator<Item = &'static str>) -> Self {
            Self {
                original: names
                    .into_iter()
                    .map(|name| (name, std::env::var_os(name)))
                    .collect(),
            }
        }

        fn set(&self, name: &'static str, value: impl AsRef<std::ffi::OsStr>) {
            unsafe { std::env::set_var(name, value) };
        }

        fn remove(&self, name: &'static str) {
            unsafe { std::env::remove_var(name) };
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (name, value) in &self.original {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(name, value),
                        None => std::env::remove_var(name),
                    }
                }
            }
        }
    }
}
