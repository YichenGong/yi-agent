//! 核心值类型。
//!
//! 标识符（`PluginId` / `ProjectId`）只能通过校验构造：它们会被用作目录名与
//! 数据键，允许任意字符串意味着每次使用都要重新转义，而漏一处就是路径穿越。

use serde::{Deserialize, Serialize};

use crate::error::PluginError;

/// 插件 ID：小写 ASCII、数字与连字符。
///
/// **反序列化必须走 `parse`**。`#[serde(transparent)]` 会直接构造内部字符串，
/// 绕过校验——那样一份非法 id 的清单能从 JSON 里反序列化成功，而校验只在
/// 显式调用 `parse` 时才发生。这类"能构造出非法值"的类型会让下游每一处
/// 使用都重新怀疑自己的输入。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct PluginId(String);

impl<'de> Deserialize<'de> for PluginId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        PluginId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl PluginId {
    pub fn parse(raw: &str) -> Result<Self, PluginError> {
        if raw.is_empty() || raw.len() > 64 {
            return Err(PluginError::InvalidManifest(format!(
                "plugin id must be 1..=64 characters, got {}",
                raw.len()
            )));
        }
        let mut chars = raw.chars();
        let first = chars.next().expect("non-empty checked above");
        if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
            return Err(PluginError::InvalidManifest(format!(
                "plugin id must start with a lowercase letter or a digit, got {raw:?}"
            )));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(PluginError::InvalidManifest(format!(
                "plugin id may only contain lowercase ASCII letters, digits and hyphens, got {raw:?}"
            )));
        }
        Ok(PluginId(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 项目标识。首版用调用方给出的规范路径字符串，由宿主注入，不由插件自造。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ProjectId(String);

impl<'de> Deserialize<'de> for ProjectId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        ProjectId::parse(&raw).map_err(serde::de::Error::custom)
    }
}

impl ProjectId {
    pub fn parse(raw: &str) -> Result<Self, PluginError> {
        if raw.trim().is_empty() {
            return Err(PluginError::InvalidManifest(
                "project id must not be empty".to_string(),
            ));
        }
        Ok(ProjectId(raw.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一个已签名发布的身份：插件 + 版本 + 平台 + 包摘要。
///
/// 四者共同构成身份，因为同一版本在两个平台是**两个发布**（各有签名与序号），
/// 而同一 `(plugin, version)` 配不同摘要是冲突而不是升级。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReleaseId {
    pub plugin: PluginId,
    pub version: String,
    pub target: String,
    pub sha256: String,
}

/// 宿主授予某个项目的一条绑定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    pub host_id: String,
    pub release: ReleaseId,
    pub activation: u64,
    pub project: ProjectId,
    pub grant: u64,
}

/// 能力种类。与 `manifest-v1.schema.json` 的 `effect` enum 逐字一致。
///
/// `Maintenance` 在 schema 的 enum 里**刻意缺席**：它只由生命周期事务签发。
/// 这里保留该变体是因为宿主内部需要表达它；`Manifest::parse` 在解析清单时
/// 显式拒绝它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Effect {
    Read,
    Write,
    Launch,
    QuotaWrite,
    Resident,
    Maintenance,
}

impl Effect {
    pub fn as_str(self) -> &'static str {
        match self {
            Effect::Read => "Read",
            Effect::Write => "Write",
            Effect::Launch => "Launch",
            Effect::QuotaWrite => "QuotaWrite",
            Effect::Resident => "Resident",
            Effect::Maintenance => "Maintenance",
        }
    }
}

/// 一次受管调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub request_id: String,
    pub binding: Binding,
    pub effect: Effect,
    pub parent_turn: Option<String>,
}
