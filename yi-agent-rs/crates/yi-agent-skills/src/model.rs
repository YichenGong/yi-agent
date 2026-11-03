use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub body: String,
    /// 这条 skill 从哪来。`scope` 只说「排在谁前面」，`source` 才说得清
    /// 「它属于谁」——插件发布的 skill 与用户自己写的是两件事，不能混。
    pub source: SkillSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    System,
    User,
    Project,
    /// 由已安装、已激活的插件发布。
    ///
    /// 单列一个 scope 而不是塞进 `User`：插件的 skill 会随插件升级而变，用户
    /// 自己写的不会。让它在 catalog 里冒充用户的 skill，用户就没法理解「我没
    /// 改过它，它怎么变了」。
    Managed,
}

/// 一条 skill 的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// 普通来源：项目目录、用户目录、随主程序分发的系统 skill。
    Local,
    /// 插件发布包里的 skill，绑定到具体插件与具体版本。
    ///
    /// 带 `version` 是必须的：同一插件的 1.3.0 与 1.4.0 目录可能同时在磁盘上，
    /// 「属于哪个插件」还不足以说清这条 skill 是哪个发布里的那一份。
    Managed { plugin: String, version: String },
}

impl SkillSource {
    pub fn is_managed(&self) -> bool {
        matches!(self, SkillSource::Managed { .. })
    }

    /// 给人看的来源短名，用于冲突提示。
    pub fn label(&self) -> String {
        match self {
            SkillSource::Local => "local".to_string(),
            SkillSource::Managed { plugin, version } => format!("{plugin}@{version}"),
        }
    }
}

/// 一个插件发布目录，作为 skill 的**受管来源**参与发现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRoot {
    /// 该发布的安装目录（绝对路径）。skill 直接从这里读，不复制。
    pub path: PathBuf,
    pub plugin: String,
    pub version: String,
}

/// 同名 skill 在多个来源里同时出现。
///
/// 不是"选一个赢"：宿主不替用户裁决，只把两条（或更多）连路径带来源摆出来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillConflict {
    pub name: String,
    pub entries: Vec<SkillConflictEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillConflictEntry {
    pub path: PathBuf,
    pub source: SkillSource,
    /// 这条来源的目录根。用户要判断"留哪个"时，看的是根而不是文件。
    pub root: PathBuf,
}

impl SkillConflict {
    pub fn has_managed(&self) -> bool {
        self.entries.iter().any(|entry| entry.source.is_managed())
    }
}

/// 插件自有 skill 的可用状态。决定「能不能拿它驱动自动任务」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillVisibility {
    /// 没有歧义，可以驱动自动任务。
    Available,
    /// 有未解决的冲突或依赖问题：**不启动**相关自动任务，但普通聊天与只读
    /// 查看照旧。
    Blocked { reason: String },
}

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill not found: {0}")]
    NotFound(PathBuf),
    #[error("failed to parse skill at {0}: {1}")]
    ParseError(PathBuf, String),
    #[error("invalid skill name '{0}': must match ^[a-z0-9]+(-[a-z0-9]+)*$ and be <= 64 chars")]
    InvalidName(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Validate a skill name: lowercase, hyphens, digits only, <= 64 chars.
pub fn is_valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut prev_was_hyphen = true; // disallow leading hyphen
    for c in name.chars() {
        if c == '-' {
            if prev_was_hyphen {
                return false; // no leading or consecutive hyphens
            }
            prev_was_hyphen = true;
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            prev_was_hyphen = false;
        } else {
            return false;
        }
    }
    !prev_was_hyphen // no trailing hyphen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(is_valid_skill_name("skill-creator"));
        assert!(is_valid_skill_name("foo"));
        assert!(is_valid_skill_name("a"));
        assert!(is_valid_skill_name("skill-creator-2"));
        assert!(is_valid_skill_name("abc123"));
    }

    #[test]
    fn invalid_names() {
        assert!(!is_valid_skill_name(""));
        assert!(!is_valid_skill_name("Skill-Creator")); // uppercase
        assert!(!is_valid_skill_name("skill_creator")); // underscore
        assert!(!is_valid_skill_name("skill creator")); // space
        assert!(!is_valid_skill_name("-leading"));
        assert!(!is_valid_skill_name("trailing-"));
        assert!(!is_valid_skill_name("double--hyphen"));
        assert!(!is_valid_skill_name(&"a".repeat(65))); // too long
    }
}
