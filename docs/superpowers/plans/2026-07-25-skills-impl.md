# Skills Feature Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a skills system in yi-agent where SKILL.md files define instructional prompts that the LLM loads on-demand via a `Skill` tool, with progressive disclosure from a catalog in the system prompt to full SKILL.md content to referenced files.

**Architecture:** New `yi-agent-skills` crate handles discovery, loading, catalog rendering, and bundled-skill installation. A `SkillTool` in `yi-agent-tools` wraps a `SkillsService` to let the LLM load full skill instructions by path. Main crate wires up discovery on startup, appends the catalog to the system prompt, and registers the `SkillTool`.

**Tech Stack:** Rust, serde_yaml for frontmatter, walkdir for discovery, include_dir for bundled skills, sha2 for content hashing, tracing for warnings.

---

## Task 1: Create yi-agent-skills crate skeleton

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-skills/Cargo.toml`
- Create: `yi-agent-rs/crates/yi-agent-skills/src/lib.rs`
- Modify: `yi-agent-rs/Cargo.toml` (add to members and workspace.dependencies)

**Step 1: Create Cargo.toml**

Create `yi-agent-rs/crates/yi-agent-skills/Cargo.toml`:

```toml
[package]
name = "yi-agent-skills"
description = "Skills system: discovery, loading, catalog rendering"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true

[dependencies]
yi-agent-core = { workspace = true }

serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
walkdir = "2"
dirs = "5"
include_dir = "0.7"
sha2 = "0.10"
tracing = "0.1"
thiserror = "2"

[dev-dependencies]
tempfile = "3"
```

**Step 2: Create lib.rs skeleton**

Create `yi-agent-rs/crates/yi-agent-skills/src/lib.rs`:

```rust
mod model;
mod loader;
mod discovery;
mod service;
mod system;

pub use model::{SkillMetadata, SkillScope, SkillError};
pub use service::SkillsService;
pub use system::install_system_skills;
```

Create empty placeholder files for each module (they will be filled in later tasks):
- `yi-agent-rs/crates/yi-agent-skills/src/model.rs`
- `yi-agent-rs/crates/yi-agent-skills/src/loader.rs`
- `yi-agent-rs/crates/yi-agent-skills/src/discovery.rs`
- `yi-agent-rs/crates/yi-agent-skills/src/service.rs`
- `yi-agent-rs/crates/yi-agent-skills/src/system.rs`

Each file should just have a `// placeholder` comment for now.

**Step 3: Register in workspace**

Modify `yi-agent-rs/Cargo.toml`:
- In `[members]` array, add `"crates/yi-agent-skills",` (after `"crates/yi-agent-web",`)
- In `[workspace.dependencies]` section, add `yi-agent-skills = { path = "crates/yi-agent-skills" }` (after the `yi-agent-web` line)

**Step 4: Verify it compiles**

Run from `yi-agent-rs/`:
```bash
cargo check -p yi-agent-skills
```
Expected: Compiles with no errors (warnings about unused modules are fine).

**Step 5: Commit**

```bash
git add yi-agent-rs/Cargo.toml yi-agent-rs/crates/yi-agent-skills/
git commit -m "feat(skills): scaffold yi-agent-skills crate"
```

---

## Task 2: Implement model.rs

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/model.rs`

**Step 1: Write the failing test**

Add to `yi-agent-rs/crates/yi-agent-skills/src/model.rs`:

```rust
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
    while let Some(c) = chars.next() {
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
```

**Step 2: Run tests to verify they pass**

Run:
```bash
cargo test -p yi-agent-skills model
```
Expected: PASS (all model tests pass)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/model.rs
git commit -m "feat(skills): add SkillMetadata, SkillScope, SkillError types"
```

---

## Task 3: Implement loader.rs (frontmatter parsing)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/loader.rs`

**Step 1: Write the failing tests**

Put this in `yi-agent-rs/crates/yi-agent-skills/src/loader.rs`:

```rust
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
        // truncate on char boundary
        let mut end = MAX_DESCRIPTION_LEN;
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
```

**Step 2: Run tests to verify they pass**

```bash
cargo test -p yi-agent-skills loader
```
Expected: PASS (all loader tests pass)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/loader.rs
git commit -m "feat(skills): add SKILL.md frontmatter parser"
```

---

## Task 4: Implement discovery.rs

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/discovery.rs`

**Step 1: Write the implementation with tests**

Put this in `yi-agent-rs/crates/yi-agent-skills/src/discovery.rs`:

```rust
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
    let mut stop = false;

    for entry in WalkDir::new(&root.path)
        .max_depth(MAX_DEPTH)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.file_name()))
    {
        if stop {
            break;
        }
        let Ok(entry) = entry else { continue };

        entry_count += 1;
        if entry_count > MAX_ENTRIES {
            tracing::warn!(
                "skill discovery: root {} exceeded max entries ({}), stopping",
                root.path.display(), MAX_ENTRIES
            );
            break;
        }

        if entry.file_type().is_dir() {
            dir_count += 1;
            if dir_count > MAX_DIRS {
                tracing::warn!(
                    "skill discovery: root {} exceeded max dirs ({}), stopping",
                    root.path.display(), MAX_DIRS
                );
                stop = true;
                continue;
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
    name.to_str().map(|s| s.starts_with('.') && s != ".").unwrap_or(false)
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
        ).unwrap();
        skill_md
    }

    #[test]
    fn single_root_single_skill() {
        let tmp = TempDir::new().unwrap();
        make_skill(tmp.path(), "foo", "A foo skill.");
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::User }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "foo");
    }

    #[test]
    fn multiple_skills_in_root() {
        let tmp = TempDir::new().unwrap();
        make_skill(tmp.path(), "foo", "Foo.");
        make_skill(tmp.path(), "bar", "Bar.");
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::User }];
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
        ).unwrap();
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::Project }];
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
        ).unwrap();
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::User }];
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
        fs::write(bad_dir.join("SKILL.md"), "---\nname: Bad_Name\ndescription: x\n---\nbody").unwrap();
        // valid
        make_skill(tmp.path(), "good", "Good.");
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::User }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "good");
    }

    #[test]
    fn empty_root_no_skills() {
        let tmp = TempDir::new().unwrap();
        let roots = vec![SkillRoot { path: tmp.path().to_path_buf(), scope: SkillScope::User }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }

    #[test]
    fn file_root_skipped() {
        // a root that is a file, not a dir, should be skipped silently
        let tmp = TempDir::new().unwrap();
        let file_path = tmp.path().join("notadir");
        fs::write(&file_path, "hello").unwrap();
        let roots = vec![SkillRoot { path: file_path, scope: SkillScope::User }];
        let skills = discover_skills(&roots);
        assert_eq!(skills.len(), 0);
    }
}
```

**Step 2: Run tests to verify they pass**

```bash
cargo test -p yi-agent-skills discovery
```
Expected: PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/discovery.rs
git commit -m "feat(skills): add skill discovery via filesystem walk"
```

---

## Task 5: Implement service.rs (SkillsService)

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/service.rs`

**Step 1: Write the implementation with tests**

Put this in `yi-agent-rs/crates/yi-agent-skills/src/service.rs`:

```rust
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::discovery::{discover_skills, SkillRoot};
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
            let cache = self.cache.read().unwrap();
            if let Some(ref skills) = *cache {
                return Ok(skills.clone());
            }
        }
        let skills = discover_skills(&self.roots);
        let mut cache = self.cache.write().unwrap();
        *cache = Some(skills.clone());
        Ok(skills)
    }

    /// Force re-scan, discarding cache.
    pub fn refresh(&self) -> Result<Vec<SkillMetadata>, SkillError> {
        let skills = discover_skills(&self.roots);
        let mut cache = self.cache.write().unwrap();
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
    /// Skills are ordered Project → User → System (most relevant first).
    pub fn render_catalog(&self, budget_bytes: usize) -> String {
        let skills = match self.snapshot() {
            Ok(s) => s,
            Err(_) => return String::new(),
        };
        render_catalog_with_budget(&skills, budget_bytes)
    }

    /// Load the body of a SKILL.md at the given path.
    /// Reads from filesystem each call (no cache).
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

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix("---\n")?;
    let end = content.find("\n---\n")?;
    let frontmatter = &content[..end];
    let body = &content[end + "\n---\n".len()..];
    Some((frontmatter, body))
}

fn scope_order(scope: SkillScope) -> u8 {
    match scope {
        SkillScope::Project => 0,
        SkillScope::User => 1,
        SkillScope::System => 2,
    }
}

fn catalog_entry(s: &SkillMetadata) -> String {
    format!("- {}: {} (path: {})", s.name, s.description, s.path.display())
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
        let skills: Vec<_> = (0..20).map(|i| make_metadata(&format!("s{i:02}"), SkillScope::User, 80)).collect();
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
            make_metadata("foo", SkillScope::User, 10),
            make_metadata("foo", SkillScope::Project, 10),
        ];
        let s = SkillsService::new(vec![]);
        {
            let mut cache = s.cache.write().unwrap();
            *cache = Some(skills);
        }
        let catalog = s.render_catalog(8192);
        // Both should be in catalog, distinguished by path
        assert!(catalog.contains("/skills/foo/SKILL.md"));
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
        ).unwrap();
        let s = SkillsService::new(vec![(tmp.path().to_path_buf(), SkillScope::User)]);
        let first = s.snapshot().unwrap();
        assert_eq!(first.len(), 1);
        // Second call should return cache (same content)
        let second = s.snapshot().unwrap();
        assert_eq!(second.len(), 1);
    }
}
```

**Step 2: Run tests to verify they pass**

```bash
cargo test -p yi-agent-skills service
```
Expected: PASS

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/service.rs
git commit -m "feat(skills): add SkillsService with caching and catalog rendering"
```

---

## Task 6: Implement system.rs (bundled skill installer)

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-creator/SKILL.md` (placeholder)
- Create: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-installer/SKILL.md` (placeholder)
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/system.rs`

**Step 1: Create placeholder bundled SKILL.md files**

Create `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-creator/SKILL.md`:

```markdown
---
name: skill-creator
description: Guide for creating effective skills. Use when users want to create a new skill or update an existing one. Covers naming, directory structure, SKILL.md format, and the references/scripts/assets subdirectories.
---

# Skill Creator

Placeholder. Full content will be written in Task 10.
```

Create `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-installer/SKILL.md`:

```markdown
---
name: skill-installer
description: Guide for installing skills from external sources like GitHub repositories. Use when users want to install, download, or share skills from outside the local filesystem.
---

# Skill Installer

Placeholder. Full content will be written in Task 10.
```

**Step 2: Write system.rs implementation with tests**

Put this in `yi-agent-rs/crates/yi-agent-skills/src/system.rs`:

```rust
use std::path::Path;

use include_dir::{include_dir, Dir};
use sha2::{Digest, Sha256};

static ASSETS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/assets");

/// Install bundled system skills into `cache_root`.
///
/// For each skill under `src/assets/`, writes its SKILL.md (and any other files)
/// to `<cache_root>/<skill-name>/`. If the target file already exists and has
/// the same content (by SHA-256), it is skipped. Otherwise it is overwritten.
///
/// Errors are returned; the caller should log and continue on failure.
pub fn install_system_skills(cache_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_root)?;

    for entry in ASSETS_DIR.dirs() {
        let name = entry.path().file_name().unwrap().to_string_lossy().to_string();
        let target_dir = cache_root.join(&name);
        std::fs::create_dir_all(&target_dir)?;
        write_dir_recursive(entry, &target_dir)?;
    }
    Ok(())
}

fn write_dir_recursive(dir: &Dir, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for file in dir.files() {
        let rel = file.path();
        let dest = target.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = file.contents();
        if let Some(existing) = std::fs::read(&dest).ok() {
            if hash(&existing) == hash(content) {
                continue; // same content, skip
            }
        }
        std::fs::write(&dest, content)?;
    }
    for sub in dir.dirs() {
        let sub_name = sub.path().file_name().unwrap().to_string_lossy().to_string();
        write_dir_recursive(sub, &target.join(sub_name))?;
    }
    Ok(())
}

fn hash(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn installs_to_empty_dir() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        assert!(tmp.path().join("skill-creator/SKILL.md").is_file());
        assert!(tmp.path().join("skill-installer/SKILL.md").is_file());
    }

    #[test]
    fn skips_unchanged_content() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let path = tmp.path().join("skill-creator/SKILL.md");
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Run again; should skip writing
        install_system_skills(tmp.path()).unwrap();
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2);
    }

    #[test]
    fn overwrites_changed_content() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let path = tmp.path().join("skill-creator/SKILL.md");
        // Modify it
        std::fs::write(&path, "modified content").unwrap();
        // Run again; should overwrite
        install_system_skills(tmp.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Placeholder")); // back to bundled content
    }

    #[test]
    fn creates_target_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested/deep");
        install_system_skills(&target).unwrap();
        assert!(target.join("skill-creator/SKILL.md").is_file());
    }
}
```

**Step 3: Run tests to verify they pass**

```bash
cargo test -p yi-agent-skills system
```
Expected: PASS

**Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/assets/ yi-agent-rs/crates/yi-agent-skills/src/system.rs
git commit -m "feat(skills): add bundled skill installer with content hashing"
```

---

## Task 7: Verify yi-agent-skills crate compiles and tests pass

**Step 1: Run all crate tests**

```bash
cargo test -p yi-agent-skills
```
Expected: All tests pass.

**Step 2: Run clippy**

```bash
cargo clippy -p yi-agent-skills --all-targets -- -D warnings
```
Expected: No warnings. Fix any that appear.

**Step 3: Commit if any fixes were needed**

Only commit if fixes were made.

---

## Task 8: Implement SkillTool in yi-agent-tools

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml` (add yi-agent-skills dep)
- Create: `yi-agent-rs/crates/yi-agent-tools/src/skill_tool.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs` (re-export SkillTool)

**Step 1: Add dependency**

In `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`, under `[dependencies]`, add:

```toml
yi-agent-skills = { workspace = true }
```

**Step 2: Write SkillTool with tests**

Create `yi-agent-rs/crates/yi-agent-tools/src/skill_tool.rs`:

```rust
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};
use yi_agent_skills::SkillsService;

pub struct SkillTool {
    service: Arc<SkillsService>,
}

#[derive(Debug, Deserialize)]
struct SkillArgs {
    path: String,
}

impl SkillTool {
    pub fn new(service: Arc<SkillsService>) -> Self {
        Self { service }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Load full instructions for a skill. Call this when you need detailed guidance from a skill listed in the available-skills section of the system prompt. Pass the exact path shown in the catalog."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to the skill's SKILL.md file, as shown in the available-skills section."
                }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let parsed: SkillArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolResult::error(format!("invalid arguments: {e}")),
        };
        match self.service.load_skill_body(&parsed.path) {
            Ok(body) => ToolResult::text(format!(
                "<skill path=\"{}\">\n{}\n</skill>",
                parsed.path, body
            )),
            Err(e) => ToolResult::error(format!("skill load failed: {e}")),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Plugin { name: "skills".to_string() },
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_service_with_file(content: &str) -> (tempfile::TempDir, Arc<SkillsService>) {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("SKILL.md");
        std::fs::write(&path, content).unwrap();
        let svc = Arc::new(SkillsService::new(vec![]));
        (tmp, svc)
    }

    #[tokio::test]
    async fn call_loads_skill_body() {
        let (tmp, svc) = make_service_with_file("---\nname: foo\ndescription: x\n---\nbody text");
        let path = tmp.path().join("SKILL.md");
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({ "path": path.to_str().unwrap() });
        let result = tool.call(args).await;
        assert!(!result.is_error);
        assert!(result.content[0].as_text().unwrap().contains("body text"));
        assert!(result.content[0].as_text().unwrap().contains("<skill path="));
    }

    #[tokio::test]
    async fn call_nonexistent_path_errors() {
        let svc = Arc::new(SkillsService::new(vec![]));
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({ "path": "/nonexistent/SKILL.md" });
        let result = tool.call(args).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn call_missing_path_arg_errors() {
        let svc = Arc::new(SkillsService::new(vec![]));
        let tool = SkillTool::new(svc);
        let args = serde_json::json!({});
        let result = tool.call(args).await;
        assert!(result.is_error);
    }
}
```

Note: the test uses `ContentBlock::as_text()` - check if that method exists; if not, match against `ContentBlock::Text(s)` directly.

**Step 3: Re-export SkillTool**

In `yi-agent-rs/crates/yi-agent-tools/src/lib.rs`, add at the top with the other `pub use` statements:

```rust
pub use skill_tool::SkillTool;
```

And add `mod skill_tool;` with the other `mod` declarations:

```rust
mod skill_tool;
```

**Step 4: Verify it compiles and tests pass**

```bash
cargo test -p yi-agent-tools skill_tool
cargo clippy -p yi-agent-tools --all-targets -- -D warnings
```
Expected: Tests pass, no clippy warnings.

**Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-tools/Cargo.toml yi-agent-rs/crates/yi-agent-tools/src/skill_tool.rs yi-agent-rs/crates/yi-agent-tools/src/lib.rs
git commit -m "feat(tools): add SkillTool wrapping SkillsService"
```

---

## Task 9: Add config fields for skills_catalog_budget

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/config.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs` (if AgentConfig needs the field; decision: keep AgentConfig unchanged, pass budget directly in main.rs)

**Decision**: Keep `AgentConfig` unchanged. The budget is a startup-time decision, not a per-agent config. Pass it directly in `main.rs`.

**Step 1: Add CLI flag and Config field**

In `yi-agent-rs/crates/yi-agent/src/config.rs`:

Add to the `Cli` struct:
```rust
#[arg(long)]
pub skills_catalog_budget: Option<usize>,
```

Add to the `Config` struct:
```rust
pub skills_catalog_budget: usize,
pub skills_catalog_budget_explicit: bool,
```

In the `load()` function, follow the existing CLI > env > default pattern:

```rust
let skills_catalog_budget_explicit = cli.skills_catalog_budget.is_some()
    || std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET").is_ok();
let skills_catalog_budget = cli.skills_catalog_budget
    .or_else(|| std::env::var("YI_AGENT_SKILLS_CATALOG_BUDGET").ok().and_then(|s| s.parse().ok()))
    .unwrap_or(8192);
```

Add these two fields to the final `Ok(Config { ... })` return.

**Step 2: Verify it compiles**

```bash
cargo check -p yi-agent
```
Expected: Compiles. (Will warn about unused fields until wired up in main.rs.)

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/config.rs
git commit -m "feat(config): add skills_catalog_budget CLI/env flag"
```

---

## Task 10: Wire up skills in main.rs

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/Cargo.toml` (add yi-agent-skills dep)
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Step 1: Add dependency**

In `yi-agent-rs/crates/yi-agent/Cargo.toml`, under `[dependencies]`, add:

```toml
yi-agent-skills = { workspace = true }
```

Also add to `[dev-dependencies]` if needed for tests (likely not needed since tests are in the skills crate itself).

**Step 2: Modify main.rs run_agent function**

In `yi-agent-rs/crates/yi-agent/src/main.rs`, modify `run_agent()`:

Find the section after:
```rust
let mut registry = yi_agent_core::ToolRegistry::new();
yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());
```

Insert this block (before `let tools = Arc::new(registry);`):

```rust
// --- Skills system setup ---
let skills_service = setup_skills(&config)?;

let system_prompt = resolve_system_prompt_with_skills(
    config.system_prompt.clone(),
    &skills_service,
    config.skills_catalog_budget,
    config.skills_catalog_budget_explicit,
);

// Register Skill tool
if let Some(svc) = &skills_service {
    registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
}
```

Then replace the `system_prompt: resolve_system_prompt(config.system_prompt.clone()),` line in the `AgentConfig` construction with:

```rust
system_prompt,
```

**Step 3: Add setup_skills and resolve_system_prompt_with_skills helpers**

Add these functions to `main.rs` (near `resolve_system_prompt`):

```rust
/// Set up the skills service: install bundled system skills, build roots, snapshot.
/// Returns None on hard failure (and logs a warning); the agent runs without skills.
fn setup_skills(config: &config::Config) -> Result<Option<Arc<yi_agent_skills::SkillsService>>> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let system_root = home.join(".yi-agent/skills/.system");

    // Install bundled skills; failure is non-fatal
    if let Err(e) = yi_agent_skills::install_system_skills(&system_root) {
        tracing::warn!("failed to install bundled skills: {e}");
    }

    let roots = vec![
        (config.workdir.join(".yi-agent/skills"), yi_agent_skills::SkillScope::Project),
        (home.join(".yi-agent/skills"), yi_agent_skills::SkillScope::User),
        (home.join(".yi-agent/skills/.system"), yi_agent_skills::SkillScope::System),
    ];

    let service = Arc::new(yi_agent_skills::SkillsService::new(roots));
    match service.snapshot() {
        Ok(skills) => {
            tracing::info!("skills: {} discovered", skills.len());
            Ok(Some(service))
        }
        Err(e) => {
            tracing::warn!("skills discovery failed: {e}");
            Ok(None)
        }
    }
}

/// Resolve the effective system prompt, appending the skills catalog if available.
fn resolve_system_prompt_with_skills(
    user: Option<String>,
    service: &Option<Arc<yi_agent_skills::SkillsService>>,
    budget: usize,
    budget_explicit: bool,
) -> Option<String> {
    let base = user.or_else(|| Some(yi_agent_core::AgentConfig::default_system_prompt()));
    let Some(svc) = service else { return base; };

    let total = svc.full_catalog_size();
    let effective_budget = resolve_effective_budget(total, budget, budget_explicit);
    let catalog = svc.render_catalog(effective_budget);

    if catalog.is_empty() {
        return base;
    }

    match base {
        Some(p) => Some(format!("{p}\n\n{catalog}")),
        None => Some(catalog),
    }
}

fn resolve_effective_budget(total: usize, default: usize, explicit: bool) -> usize {
    if explicit || total <= default || !is_interactive() {
        return default;
    }
    prompt_catalog_budget(total, default).unwrap_or(default)
}

fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn prompt_catalog_budget(total: usize, default: usize) -> Option<usize> {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {total_kb} KB, exceeds default {default_kb} KB budget.\n\
         Include all skills? [Y/n]"
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    match input.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => Some(total),
        _ => Some(default),
    }
}
```

**Step 4: Add deps to Cargo.toml**

The `dirs` crate is needed in the main crate. Check if it's already a dependency; if not, add to `yi-agent-rs/crates/yi-agent/Cargo.toml` `[dependencies]`:

```toml
dirs = "5"
```

Also ensure `anyhow` is available (it is, as workspace dep) and that `tracing` is (it is).

**Step 5: Update existing tests in main.rs**

The existing tests `resolve_system_prompt_none_uses_default` and `resolve_system_prompt_custom_overrides_default` test the old `resolve_system_prompt` function. Either:
- Keep `resolve_system_prompt` function (rename the new one to `resolve_system_prompt_with_skills` and keep the old one for tests)
- OR update the tests to use the new function

Decision: Keep the old `resolve_system_prompt` function as-is (still used by the new function internally when service is None). The new function `resolve_system_prompt_with_skills` replaces the call site. Existing tests still pass.

**Step 6: Verify it compiles and tests pass**

```bash
cargo check -p yi-agent
cargo test -p yi-agent
cargo clippy -p yi-agent --all-targets -- -D warnings
```
Expected: All pass. Fix any issues.

**Step 7: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/Cargo.toml yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "feat(agent): wire up skills system on startup"
```

---

## Task 11: Write the real bundled skill-creator SKILL.md

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-creator/SKILL.md`

**Step 1: Replace placeholder with real content**

Write the full content. Refer to codex's `skill-creator/SKILL.md` at `/Users/gongyichen/Documents/TechnicalStuff/projects/OpenSource/codex/codex-rs/skills/src/assets/samples/skill-creator/SKILL.md` for inspiration, but adapt to yi-agent specifics (paths `~/.yi-agent/skills/`, no plugin/orchestrator concepts, restart to reload).

Key content to cover:
- What skills are and the three-level progressive disclosure model
- Naming rules: `^[a-z0-9]+(-[a-z0-9]+)*$`, <= 64 chars
- Directory structure: `<name>/SKILL.md` + optional `references/`, `scripts/`, `assets/`
- SKILL.md format: YAML frontmatter (`name` + `description`) + markdown body
- How to write a good `description` (must state when to use it; it's the LLM's only trigger signal)
- Placement: `~/.yi-agent/skills/` (user) or `<project>/.yi-agent/skills/` (project)
- Restart yi-agent to load new/changed skills (no hot reload)
- Start minimal: write SKILL.md first, add references/scripts only when needed

**Step 2: Verify it parses correctly**

```bash
cargo test -p yi-agent-skills system
```
Expected: Pass (system tests install and read these files).

**Step 3: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/assets/skill-creator/SKILL.md
git commit -m "feat(skills): write skill-creator bundled skill content"
```

---

## Task 12: Write the real bundled skill-installer SKILL.md

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-skills/src/assets/skill-installer/SKILL.md`

**Step 1: Replace placeholder with real content**

Write content covering:
- A skill is just a directory containing SKILL.md; installation = copying the directory into a skill root
- From GitHub: `git clone <repo> ~/.yi-agent/skills/<name>` (clone into the user skill root)
- Verify the cloned dir has a SKILL.md with valid frontmatter (`name` and `description`)
- Place project-specific skills in `<project>/.yi-agent/skills/<name>/`
- No registration needed; restart yi-agent to load

**Step 2: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-skills/src/assets/skill-installer/SKILL.md
git commit -m "feat(skills): write skill-installer bundled skill content"
```

---

## Task 13: End-to-end verification

**Step 1: Full workspace test**

```bash
cd yi-agent-rs && cargo test --workspace
```
Expected: All tests pass across all crates.

**Step 2: Full workspace clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings
```
Expected: No warnings.

**Step 3: Manual smoke test**

From the worktree, run the binary to verify the skills system initializes:

```bash
cargo run -p yi-agent -- --help 2>&1 | grep skills-catalog-budget
```
Expected: Shows `--skills-catalog-budget` flag in help output.

**Step 4: Verify bundled skills install**

```bash
# Clear the system skills cache and run once to verify they get installed
rm -rf ~/.yi-agent/skills/.system
cargo run -p yi-agent -- --help >/dev/null 2>&1
ls ~/.yi-agent/skills/.system/
```
Expected: `skill-creator` and `skill-installer` directories exist with SKILL.md files.

Note: `cargo run -- --help` may exit before `setup_skills` runs if `--help` is handled by clap first. If so, use a different subcommand or check the test more carefully. A better check: write a small test or use a real run with `--yolo` and immediately exit.

Alternative Step 4: Just verify the system tests cover the install logic (they do in Task 6).

**Step 5: Commit any final fixes**

If any fixes were made during verification, commit them.

---

## Summary

After all tasks:
- New crate `yi-agent-skills` with model, loader, discovery, service, system modules
- `SkillTool` in `yi-agent-tools` registered in main
- Config fields `skills_catalog_budget` and `skills_catalog_budget_explicit`
- Two bundled skills: `skill-creator` and `skill-installer`
- Catalog appended to system prompt on startup
- Over-budget interactive prompt in interactive mode
- All tests passing, clippy clean

## Notes for executor

- Use `superpowers:test-driven-development` principles: tests first, then implementation
- Commit after each task (frequent commits)
- Run `cargo test -p <crate>` for specific crate tests, `cargo clippy -p <crate> --all-targets -- -D warnings` for lint
- If a test fails, use `superpowers:systematic-debugging` before proceeding
- The `ContentBlock::as_text()` method may not exist; check the actual `ContentBlock` enum in `yi-agent-core/src/message.rs` and adapt the SkillTool test to match against the variant directly
