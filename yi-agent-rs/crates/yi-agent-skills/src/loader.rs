use std::path::Path;

use serde::Deserialize;

use crate::model::{is_valid_skill_name, SkillError, SkillMetadata, SkillScope};

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

const MAX_DESCRIPTION_LEN: usize = 1024;

/// Parse a SKILL.md file's content into a SkillMetadata.
///
/// Format: YAML frontmatter delimited by `---\n...\n---\n`, followed by markdown body.
/// The `scope` and `path` are passed in by the caller (discovery).
pub fn parse_skill_md(
    content: &str,
    path: &Path,
    scope: SkillScope,
) -> Result<SkillMetadata, SkillError> {
    let (frontmatter_text, body) = split_frontmatter(content)
        .ok_or_else(|| SkillError::ParseError(
            path.to_path_buf(),
            "missing YAML frontmatter".to_string(),
        ))?;

    let fm: Frontmatter = serde_yaml::from_str(&frontmatter_text)
        .map_err(|e| SkillError::ParseError(path.to_path_buf(), e.to_string()))?;

    if !is_valid_skill_name(&fm.name) {
        return Err(SkillError::InvalidName(fm.name));
    }

    let description = truncate_description(&fm.description);

    Ok(SkillMetadata {
        name: fm.name,
        description,
        path: path.to_path_buf(),
        scope,
        body: body.to_string(),
    })
}

/// Split content into (frontmatter_text, body_text).
/// Returns None if no valid frontmatter delimiters found.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix("---\n")?;
    let end = content.find("\n---\n")?;
    let frontmatter = &content[..end];
    let body = &content[end + "\n---\n".len()..];
    Some((frontmatter, body))
}

fn truncate_description(desc: &str) -> String {
    if desc.len() <= MAX_DESCRIPTION_LEN {
        desc.to_string()
    } else {
        // truncate on char boundary, reserving 3 bytes for the "..." suffix
        let mut end = MAX_DESCRIPTION_LEN - 3;
        while end > 0 && !desc.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &desc[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn parse(content: &str, scope: SkillScope) -> Result<SkillMetadata, SkillError> {
        parse_skill_md(content, &PathBuf::from("/test/SKILL.md"), scope)
    }

    #[test]
    fn parses_valid_skill() {
        let content = "---\nname: skill-creator\ndescription: A skill.\n---\n# Body\n\nHello";
        let m = parse(content, SkillScope::User).unwrap();
        assert_eq!(m.name, "skill-creator");
        assert_eq!(m.description, "A skill.");
        assert_eq!(m.body, "# Body\n\nHello");
        assert_eq!(m.scope, SkillScope::User);
    }

    #[test]
    fn missing_frontmatter_errors() {
        let content = "# No frontmatter here";
        assert!(matches!(parse(content, SkillScope::User), Err(SkillError::ParseError(_, _))));
    }

    #[test]
    fn missing_name_field_errors() {
        let content = "---\ndescription: A skill.\n---\nbody";
        let err = parse(content, SkillScope::User).unwrap_err();
        assert!(matches!(err, SkillError::ParseError(_, _)));
    }

    #[test]
    fn missing_description_field_errors() {
        let content = "---\nname: foo\n---\nbody";
        let err = parse(content, SkillScope::User).unwrap_err();
        assert!(matches!(err, SkillError::ParseError(_, _)));
    }

    #[test]
    fn invalid_name_errors() {
        let content = "---\nname: Foo_Bar\ndescription: x\n---\nbody";
        assert!(matches!(parse(content, SkillScope::User), Err(SkillError::InvalidName(_))));
    }

    #[test]
    fn long_description_truncated() {
        let long = "a".repeat(2000);
        let content = format!("---\nname: foo\ndescription: {long}\n---\nbody");
        let m = parse(&content, SkillScope::User).unwrap();
        assert!(m.description.len() <= MAX_DESCRIPTION_LEN);
        assert!(m.description.ends_with("..."));
    }

    #[test]
    fn empty_body_ok() {
        let content = "---\nname: foo\ndescription: x\n---\n";
        let m = parse(content, SkillScope::User).unwrap();
        assert_eq!(m.body, "");
    }

    #[test]
    fn yaml_syntax_error() {
        let content = "---\nname: foo\ndescription: [unclosed\n---\nbody";
        assert!(matches!(parse(content, SkillScope::User), Err(SkillError::ParseError(_, _))));
    }
}
