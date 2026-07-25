use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub scope: SkillScope,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    System,
    User,
    Project,
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
    let mut chars = name.chars().peekable();
    let mut prev_was_hyphen = true; // disallow leading hyphen
    for c in chars.by_ref() {
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
