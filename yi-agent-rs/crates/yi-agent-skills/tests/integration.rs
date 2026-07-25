//! Integration test: verify the bundled SKILL.md files parse correctly
//! through the full install -> discover -> render pipeline.

use std::fs;

use tempfile::TempDir;
use yi_agent_skills::{install_system_skills, SkillScope, SkillsService};

#[test]
fn bundled_skills_install_and_discover_correctly() {
    let tmp = TempDir::new().unwrap();
    let system_root = tmp.path().join(".system");

    // Install bundled skills
    install_system_skills(&system_root).unwrap();

    // Verify files exist
    assert!(system_root.join("skill-creator/SKILL.md").is_file());
    assert!(system_root.join("skill-installer/SKILL.md").is_file());

    // Discover skills from the installed root
    let service = SkillsService::new(vec![
        (system_root.clone(), SkillScope::System),
    ]);

    let skills = service.snapshot().unwrap();
    assert_eq!(skills.len(), 2, "expected exactly 2 bundled skills");

    let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"skill-creator"), "missing skill-creator");
    assert!(names.contains(&"skill-installer"), "missing skill-installer");

    // Verify descriptions are non-empty
    for skill in &skills {
        assert!(!skill.description.is_empty(), "skill {} has empty description", skill.name);
        assert!(skill.description.len() <= 1024, "skill {} description too long", skill.name);
        assert!(!skill.body.is_empty(), "skill {} has empty body", skill.name);
    }

    // Verify catalog rendering includes both
    let catalog = service.render_catalog(8192);
    assert!(catalog.contains("skill-creator"), "catalog missing skill-creator");
    assert!(catalog.contains("skill-installer"), "catalog missing skill-installer");
}

#[test]
fn render_catalog_truncates_excessively_large_catalog() {
    // Build a temp root with many skills that exceed a small budget
    let tmp = TempDir::new().unwrap();
    for i in 0..30 {
        let dir = tmp.path().join(format!("skill-{:02}", i));
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            format!(
                "---\nname: skill-{:02}\ndescription: {}\n---\nbody",
                i,
                "x".repeat(80)
            ),
        ).unwrap();
    }

    let service = SkillsService::new(vec![
        (tmp.path().to_path_buf(), SkillScope::User),
    ]);

    let full_size = service.full_catalog_size();
    assert!(full_size > 500, "full catalog should be > 500 bytes, got {}", full_size);

    let small_budget = 200;
    let catalog = service.render_catalog(small_budget);
    assert!(catalog.len() <= small_budget,
        "catalog len {} exceeds budget {}", catalog.len(), small_budget);

    // The last skill should NOT be in the truncated catalog
    assert!(!catalog.contains("skill-29"),
        "skill-29 should be truncated but was found");
}
