use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::loader::parse_skill_md;
use crate::model::{SkillError, SkillMetadata, SkillScope};

const MAX_DEPTH: usize = 4;
const MAX_DIRS: usize = 2000;
const MAX_ENTRIES: usize = 20000;

/// A skill root directory paired with its scope.
pub struct SkillRoot {
    pub path: PathBuf,
    pub scope: SkillScope,
}

/// Discover all skills under the given roots.
///
/// Returns a Vec of successfully parsed skills. Errors for individual
/// SKILL.md files are logged via tracing::warn! and skipped.
pub fn discover_skills(roots: &[SkillRoot]) -> Vec<SkillMetadata> {
    let mut skills = Vec::new();
    for root in roots {
        if !root.path.is_dir() {
            continue; // root doesn't exist, skip silently
        }
        discover_one(root, &mut skills);
    }
    skills
}

fn discover_one(root: &SkillRoot, out: &mut Vec<SkillMetadata>) {
    let mut dir_count = 0usize;
    let mut entry_count = 0usize;

    let root_path = root.path.clone();
    for entry in WalkDir::new(&root.path)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Don't filter the root entry itself, even if its name starts with '.'
            e.path() == root_path || !is_hidden(e.file_name())
        })
    {
        let Ok(entry) = entry else { continue };

        entry_count += 1;
        if entry_count > MAX_ENTRIES {
            tracing::warn!(
                "skill discovery: root {} exceeded max entries ({}), stopping",
                root.path.display(),
                MAX_ENTRIES
            );
            break;
        }

        if entry.file_type().is_dir() {
            dir_count += 1;
            if dir_count > MAX_DIRS {
                tracing::warn!(
                    "skill discovery: root {} exceeded max dirs ({}), stopping",
                    root.path.display(),
                    MAX_DIRS
                );
                break;
            }
            continue;
        }

        if entry.file_name() != "SKILL.md" {
            continue;
        }

        match load_skill_file(entry.path(), root.scope) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!("skipping skill at {}: {}", entry.path().display(), e),
        }
    }
}

fn load_skill_file(path: &Path, scope: SkillScope) -> Result<SkillMetadata, SkillError> {
    let content = std::fs::read_to_string(path)?;
    parse_skill_md(&content, path, scope)
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .map(|s| s.starts_with('.') && s != ".")
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_skill(dir: &Path, name: &str, desc: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        fs::write(
            &skill_md,
            format!("---\nname: {name}\ndescription: {desc}\n---\nbody"),
        )
        .unwrap();
        skill_md
    }

    #[test]
    fn single_root_single_skill() {
        let tmp = TempDir::new().unwrap();
        make_skill(tmp.path(), "foo", "A foo skill.");
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "foo");
    }

    #[test]
    fn multiple_skills_in_root() {
        let tmp = TempDir::new().unwrap();
        make_skill(tmp.path(), "foo", "Foo.");
        make_skill(tmp.path(), "bar", "Bar.");
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 2);
    }

    #[test]
    fn nested_subdir_skill_found() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("a/b");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("SKILL.md"),
            "---\nname: nested\ndescription: nested.\n---\nbody",
        )
        .unwrap();
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::Project,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "nested");
    }

    #[test]
    fn hidden_dir_skipped() {
        let tmp = TempDir::new().unwrap();
        let hidden = tmp.path().join(".hidden");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(
            hidden.join("SKILL.md"),
            "---\nname: hidden\ndescription: hidden.\n---\nbody",
        )
        .unwrap();
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn nonexistent_root_skipped() {
        let roots = vec![SkillRoot {
            path: PathBuf::from("/nonexistent/definitely/not/here"),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn invalid_skill_skipped_valid_returned() {
        let tmp = TempDir::new().unwrap();
        // invalid: bad name
        let bad_dir = tmp.path().join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(
            bad_dir.join("SKILL.md"),
            "---\nname: Bad_Name\ndescription: x\n---\nbody",
        )
        .unwrap();
        // valid
        make_skill(tmp.path(), "good", "Good.");
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn empty_root_no_skills() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![SkillRoot {
            path: tmp.path().to_path_buf(),
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn file_root_skipped() {
        // a root that is a file, not a dir, should be skipped silently
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("notadir");
        fs::write(&file_path, "hello").unwrap();
        let roots = vec![SkillRoot {
            path: file_path,
            scope: SkillScope::User,
        }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }
}
