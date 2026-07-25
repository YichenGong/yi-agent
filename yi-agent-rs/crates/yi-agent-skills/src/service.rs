use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::discovery::{SkillRoot, discover_skills};
use crate::loader::split_frontmatter;
use crate::model::{SkillError, SkillMetadata, SkillScope};

const CATALOG_HEADER: &str = "## Skills\n\n\
A skill is a set of instructions. Each skill is listed below with its name, a brief description, \
and the path to its SKILL.md file. To load the full instructions for a skill, call the Skill tool \
with that path.\n\n\
### Available skills\n";

pub struct SkillsService {
    roots: Vec<SkillRoot>,
    cache: RwLock<Option<Vec<SkillMetadata>>>,
}

impl SkillsService {
    pub fn new(roots: Vec<(PathBuf, SkillScope)>) -> Self {
        let roots = roots
            .into_iter()
            .map(|(path, scope)| SkillRoot { path, scope })
            .collect();
        Self {
            roots,
            cache: RwLock::new(None),
        }
    }

    /// Trigger discovery if not yet cached. Returns slice of cached skills.
    pub fn snapshot(&self) -> Result<Vec<SkillMetadata>, SkillError> {
        {
            let cache = self.cache.read().unwrap_or_else(|e| e.into_inner());
            if let Some(ref skills) = *cache {
                return Ok(skills.clone());
            }
        }
        let skills = discover_skills(&self.roots);
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        *cache = Some(skills.clone());
        Ok(skills)
    }

    /// Force re-scan, discarding cache.
    pub fn refresh(&self) -> Result<Vec<SkillMetadata>, SkillError> {
        let skills = discover_skills(&self.roots);
        let mut cache = self.cache.write().unwrap_or_else(|e| e.into_inner());
        *cache = Some(skills.clone());
        Ok(skills)
    }

    /// Return the total byte size of the full catalog (no truncation).
    pub fn full_catalog_size(&self) -> usize {
        let skills = match self.snapshot() {
            Ok(s) => s,
            Err(_) => return 0,
        };
        full_catalog_string(&skills).len()
    }

    /// Render the catalog markdown, truncated to budget_bytes.
    /// Skills are ordered Project -> User -> System (most relevant first).
    pub fn render_catalog(&self, budget_bytes: usize) -> String {
        let skills = match self.snapshot() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        render_catalog_with_budget(&skills, budget_bytes)
    }

    /// Load the body of a SKILL.md at the given path.
    /// Reads from filesystem each call (no cache).
    ///
    /// # Safety rationale
    ///
    /// The path is NOT validated against the discovered skill list. This is intentional:
    /// an LLM may slightly misspell a path but the file still exists, and strict validation
    /// would block legitimate calls. The path-traversal risk is equivalent to the LLM using
    /// ReadTool to read any file — this does not increase the attack surface.
    pub fn load_skill_body(&self, path: &str) -> Result<String, SkillError> {
        let p = Path::new(path);
        if !p.is_file() {
            return Err(SkillError::NotFound(p.to_path_buf()));
        }
        let content = std::fs::read_to_string(p)?;
        // Strip frontmatter, return body
        if let Some((_fm, body)) = split_frontmatter(&content) {
            Ok(body.to_string())
        } else {
            // No frontmatter, return whole content
            Ok(content)
        }
    }
}

fn scope_order(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Project => 0,
        SkillScope::User => 1,
        SkillScope::System => 2,
    }
}

fn catalog_entry(s: &SkillMetadata) -> String {
    format!(
        "- {}: {} (path: {})",
        s.name,
        s.description,
        s.path.display()
    )
}

fn full_catalog_string(skills: &[SkillMetadata]) -> String {
    let mut sorted: Vec<&SkillMetadata> = skills.iter().collect();
    sorted.sort_by_key(|s| (scope_order(s.scope), s.name.clone()));
    let mut out = String::from(CATALOG_HEADER);
    for s in &sorted {
        out.push_str(&catalog_entry(s));
        out.push('\n');
    }
    out
}

fn render_catalog_with_budget(skills: &[SkillMetadata], budget_bytes: usize) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&SkillMetadata> = skills.iter().collect();
    sorted.sort_by_key(|s| (scope_order(s.scope), s.name.clone()));

    let header_len = CATALOG_HEADER.len();
    if header_len >= budget_bytes {
        return String::new();
    }
    let mut out = String::from(CATALOG_HEADER);
    let mut budget_left = budget_bytes - header_len;
    for s in &sorted {
        let entry = catalog_entry(s);
        let cost = entry.len() + 1; // +1 for newline
        if cost > budget_left {
            break;
        }
        out.push_str(&entry);
        out.push('\n');
        budget_left -= cost;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_metadata(name: &str, scope: SkillScope, desc_len: usize) -> SkillMetadata {
        SkillMetadata {
            name: name.to_string(),
            description: "x".repeat(desc_len),
            path: PathBuf::from(format!("/skills/{}/SKILL.md", name)),
            scope,
            body: String::new(),
        }
    }

    #[test]
    fn render_empty_returns_empty_string() {
        let s = SkillsService::new(vec![]);
        assert_eq!(s.render_catalog(8192), "");
    }

    #[test]
    fn render_under_budget_lists_all() {
        let skills = vec![
            make_metadata("a", SkillScope::User, 50),
            make_metadata("b", SkillScope::User, 50),
        ];
        // Bypass snapshot: write cache directly
        let s = SkillsService::new(vec![]);
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(skills);
        }
        let catalog = s.render_catalog(8192);
        assert!(catalog.contains("- a: "));
        assert!(catalog.contains("- b: "));
    }

    #[test]
    fn render_over_budget_truncates() {
        // Each entry is ~100 bytes; budget of 300 should only fit a couple
        let skills: Vec<_> = (0..20)
            .map(|i| make_metadata(&format!("s{i:02}"), SkillScope::User, 80))
            .collect();
        let s = SkillsService::new(vec![]);
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(skills);
        }
        let catalog = s.render_catalog(300);
        assert!(catalog.len() <= 300);
        // Should not contain all 20
        assert!(!catalog.contains("s19"));
    }

    #[test]
    fn project_scope_listed_before_user_and_system() {
        let skills = vec![
            make_metadata("sys", SkillScope::System, 10),
            make_metadata("usr", SkillScope::User, 10),
            make_metadata("proj", SkillScope::Project, 10),
        ];
        let s = SkillsService::new(vec![]);
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(skills);
        }
        let catalog = s.render_catalog(8192);
        let proj_pos = catalog.find("proj").unwrap();
        let usr_pos = catalog.find("usr").unwrap();
        let sys_pos = catalog.find("sys").unwrap();
        assert!(proj_pos < usr_pos);
        assert!(usr_pos < sys_pos);
    }

    #[test]
    fn same_name_different_scope_both_listed() {
        let skills = vec![
            SkillMetadata {
                name: "foo".to_string(),
                description: "x".to_string(),
                path: PathBuf::from("/user/skills/foo/SKILL.md"),
                scope: SkillScope::User,
                body: String::new(),
            },
            SkillMetadata {
                name: "foo".to_string(),
                description: "x".to_string(),
                path: PathBuf::from("/project/skills/foo/SKILL.md"),
                scope: SkillScope::Project,
                body: String::new(),
            },
        ];
        let s = SkillsService::new(vec![]);
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(skills);
        }
        let catalog = s.render_catalog(8192);
        // Both should be in catalog, distinguished by path
        let count = catalog.matches("/foo/SKILL.md").count();
        assert_eq!(count, 2, "both foo skills should appear in catalog");
    }

    #[test]
    fn load_skill_body_strips_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("SKILL.md");
        std::fs::write(&p, "---\nname: foo\ndescription: x\n---\nbody content").unwrap();
        let s = SkillsService::new(vec![]);
        let body = s.load_skill_body(p.to_str().unwrap()).unwrap();
        assert_eq!(body, "body content");
    }

    #[test]
    fn load_skill_body_not_found() {
        let s = SkillsService::new(vec![]);
        let err = s.load_skill_body("/nonexistent/SKILL.md").unwrap_err();
        assert!(matches!(err, SkillError::NotFound(_)));
    }

    #[test]
    fn snapshot_caches() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let s = SkillsService::new(vec![(tmp.path().to_path_buf(), SkillScope::User)]);
        let first = s.snapshot().unwrap();
        assert_eq!(first.len(), 1);

        // Add a new skill file after the first snapshot; if cache works,
        // the second snapshot should still return the old count (1), not 2.
        std::fs::create_dir_all(tmp.path().join("bar")).unwrap();
        std::fs::write(
            tmp.path().join("bar/SKILL.md"),
            "---\nname: bar\ndescription: y\n---\nbody",
        )
        .unwrap();

        let second = s.snapshot().unwrap();
        assert_eq!(
            second.len(),
            1,
            "cache should serve stale data, not re-scan filesystem"
        );
    }
}
