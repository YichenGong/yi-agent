use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::discovery::{SkillRoot, discover_skills};
use crate::loader::split_frontmatter;
use crate::model::{
    ManagedRoot, SkillConflict, SkillConflictEntry, SkillError, SkillMetadata, SkillScope,
    SkillSource, SkillVisibility,
};

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
            .map(|(path, scope)| SkillRoot {
                path,
                scope,
                source: SkillSource::Local,
            })
            .collect();
        Self {
            roots,
            cache: RwLock::new(None),
        }
    }

    /// 普通来源 + 插件受管来源。
    ///
    /// 受管来源各占一个 root，scope 固定 `Managed`、source 带上插件与版本：
    /// 这样 catalog 能排、冲突能说清是谁的。
    pub fn with_managed(roots: Vec<(PathBuf, SkillScope)>, managed: Vec<ManagedRoot>) -> Self {
        let mut all: Vec<SkillRoot> = roots
            .into_iter()
            .map(|(path, scope)| SkillRoot {
                path,
                scope,
                source: SkillSource::Local,
            })
            .collect();
        all.extend(managed.into_iter().map(|root| SkillRoot {
            path: root.path,
            scope: SkillScope::Managed,
            source: SkillSource::Managed {
                plugin: root.plugin,
                version: root.version,
            },
        }));
        Self {
            roots: all,
            cache: RwLock::new(None),
        }
    }

    /// 同名 skill 出现在多个来源里。
    ///
    /// 只在**涉及受管来源**时算冲突：两条普通 skill 同名是用户自己的排布，
    /// 已有的 Project→User→System 排序已经说明了谁优先，宿主不该替用户裁决
    /// 他自己写的两份东西。
    pub fn conflicts(&self) -> Result<Vec<SkillConflict>, SkillError> {
        let skills = self.snapshot()?;
        let mut by_name: std::collections::BTreeMap<&str, Vec<&SkillMetadata>> =
            std::collections::BTreeMap::new();
        for skill in &skills {
            by_name.entry(skill.name.as_str()).or_default().push(skill);
        }
        let mut out = Vec::new();
        for (name, entries) in by_name {
            if entries.len() < 2 {
                continue;
            }
            if !entries.iter().any(|s| s.source.is_managed()) {
                continue;
            }
            out.push(SkillConflict {
                name: name.to_string(),
                entries: entries
                    .into_iter()
                    .map(|skill| SkillConflictEntry {
                        path: skill.path.clone(),
                        source: skill.source.clone(),
                        root: root_of(&self.roots, &skill.path),
                    })
                    .collect(),
            });
        }
        Ok(out)
    }

    /// 插件自有 skill 现在能不能驱动自动任务。
    ///
    /// 只回答「能不能自动跑」——冲突时普通聊天和只读查看都该照旧，所以这是
    /// 一个可单独查询的结论，而不是让 snapshot/catalog 失败。
    pub fn managed_visibility(&self) -> SkillVisibility {
        match self.conflicts() {
            Ok(conflicts) if !conflicts.is_empty() => SkillVisibility::Blocked {
                reason: "conflict".to_string(),
            },
            Ok(_) => SkillVisibility::Available,
            Err(error) => SkillVisibility::Blocked {
                reason: error.to_string(),
            },
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
        // 受管 skill 排在最后：它们不属于用户，也不该盖过用户自己的同名条目。
        // 冲突要显式呈现（见 `conflicts`），而不是靠排序静默选一个赢家。
        SkillScope::Managed => 3,
    }
}

/// 一个 skill 文件属于哪个 root。冲突提示要给人看的"目录"是 root，不是文件。
fn root_of(roots: &[SkillRoot], path: &Path) -> PathBuf {
    roots
        .iter()
        .filter(|root| path.starts_with(&root.path))
        .max_by_key(|root| root.path.components().count())
        .map(|root| root.path.clone())
        .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_default())
}

fn catalog_entry(s: &SkillMetadata) -> String {
    format!(
        "- {}: {} (path: {})",
        s.name,
        s.description,
        s.path.display()
    )
}

/// Sort skills into the catalog's canonical order: scope, then name, then path.
///
/// The path is the final tiebreaker so that two skills with the same name in the
/// same scope render in a stable, readdir-independent order (byte-stable prompt).
fn sorted_skills(skills: &[SkillMetadata]) -> Vec<&SkillMetadata> {
    let mut sorted: Vec<&SkillMetadata> = skills.iter().collect();
    sorted.sort_by(|a, b| {
        (scope_order(a.scope), &a.name, &a.path).cmp(&(scope_order(b.scope), &b.name, &b.path))
    });
    sorted
}

fn full_catalog_string(skills: &[SkillMetadata]) -> String {
    let sorted = sorted_skills(skills);
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
    let sorted = sorted_skills(skills);

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
            source: SkillSource::Local,
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
                source: SkillSource::Local,
            },
            SkillMetadata {
                name: "foo".to_string(),
                description: "x".to_string(),
                path: PathBuf::from("/project/skills/foo/SKILL.md"),
                scope: SkillScope::Project,
                body: String::new(),
                source: SkillSource::Local,
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

    #[test]
    fn refresh_picks_up_new_skill() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("foo")).unwrap();
        std::fs::write(
            tmp.path().join("foo/SKILL.md"),
            "---\nname: foo\ndescription: x\n---\nbody",
        )
        .unwrap();
        let s = SkillsService::new(vec![(tmp.path().to_path_buf(), SkillScope::User)]);
        assert_eq!(s.snapshot().unwrap().len(), 1);

        std::fs::create_dir_all(tmp.path().join("bar")).unwrap();
        std::fs::write(
            tmp.path().join("bar/SKILL.md"),
            "---\nname: bar\ndescription: y\n---\nbody",
        )
        .unwrap();

        // refresh() re-scans and sees the new skill...
        assert_eq!(s.refresh().unwrap().len(), 2);
        // ...and the refreshed result replaces the cache for later snapshots.
        assert_eq!(s.snapshot().unwrap().len(), 2);
    }

    #[test]
    fn same_name_same_scope_ordered_by_path() {
        let mk = |p: &str| SkillMetadata {
            name: "dup".into(),
            description: "x".into(),
            path: PathBuf::from(p),
            scope: SkillScope::User,
            body: String::new(),
            source: SkillSource::Local,
        };
        let s = SkillsService::new(vec![]);
        // Seed zzz BEFORE aaa so readdir/insertion order cannot mask a
        // missing path tiebreak in the sort key.
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(vec![mk("/t/zzz/SKILL.md"), mk("/t/aaa/SKILL.md")]);
        }
        let catalog = s.render_catalog(8192);
        let aaa = catalog.find("/t/aaa").expect("aaa listed");
        let zzz = catalog.find("/t/zzz").expect("zzz listed");
        assert!(aaa < zzz, "same-name skills must be ordered by path");
    }
}
