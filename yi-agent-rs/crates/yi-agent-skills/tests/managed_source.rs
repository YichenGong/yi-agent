//! 插件**受管** skill 来源：与 Project/User/System 三种普通来源的边界。
//!
//! 三条要求来自设计（§7.2）：
//! 1. 签发包里的 skill 不复制到用户全局目录，直接从版本目录发现；
//! 2. 它**单独绑定来源**，不能被当成 User/System（否则用户看到一条"我的 skill"，
//!    而它其实随插件发布、插件一升就变）；
//! 3. 同名冲突必须**报出路径与来源**且不覆盖任何文件；冲突未解决前要能拦住
//!    自动任务，同时普通聊天与只读查看照旧。

use std::fs;
use std::path::Path;

use yi_agent_skills::{ManagedRoot, SkillScope, SkillSource, SkillVisibility, SkillsService};

fn make_skill(dir: &Path, name: &str, desc: &str) -> std::path::PathBuf {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    let skill_md = skill_dir.join("SKILL.md");
    fs::write(
        &skill_md,
        format!("---\nname: {name}\ndescription: {desc}\n---\nbody of {name}"),
    )
    .unwrap();
    skill_md
}

#[test]
fn a_managed_skill_is_reported_with_its_own_source_not_as_user_or_system() {
    let managed = tempfile::TempDir::new().unwrap();
    let user = tempfile::TempDir::new().unwrap();
    make_skill(managed.path(), "kanban", "Drive the board.");

    let service = SkillsService::with_managed(
        vec![(user.path().to_path_buf(), SkillScope::User)],
        vec![ManagedRoot {
            path: managed.path().to_path_buf(),
            plugin: "example-plugin".into(),
            version: "1.4.0".into(),
        }],
    );

    let skills = service.snapshot().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "kanban");
    assert_eq!(
        skills[0].scope,
        SkillScope::Managed,
        "a plugin-owned skill must not claim to be a user or system skill"
    );
    match &skills[0].source {
        SkillSource::Managed { plugin, version } => {
            assert_eq!(plugin, "example-plugin");
            assert_eq!(version, "1.4.0");
        }
        other => panic!("expected a managed source, got {other:?}"),
    }
    // 路径必须仍指向版本目录本身——不复制到全局目录。
    assert!(
        skills[0].path.starts_with(managed.path()),
        "the skill must be read from the release directory, not a copy: {}",
        skills[0].path.display()
    );
}

#[test]
fn an_ordinary_skill_keeps_its_plain_source() {
    let user = tempfile::TempDir::new().unwrap();
    make_skill(user.path(), "plain", "An ordinary skill.");

    let service = SkillsService::new(vec![(user.path().to_path_buf(), SkillScope::User)]);
    let skills = service.snapshot().unwrap();
    assert_eq!(skills[0].scope, SkillScope::User);
    assert!(!skills[0].source.is_managed());
}

/// 同名冲突：一边是用户自己写的，一边是发布包里的。
///
/// 关键不是"谁赢"——是**不许静默选一个**。用户必须看到两条路径和各自来源，
/// 去决定留下哪个；宿主绝不覆盖或删除用户文件。
#[test]
fn a_name_clash_between_managed_and_user_is_reported_with_both_paths() {
    let managed = tempfile::TempDir::new().unwrap();
    let user = tempfile::TempDir::new().unwrap();
    let managed_path = make_skill(managed.path(), "kanban", "Release-provided.");
    let user_path = make_skill(user.path(), "kanban", "Mine, hand-written.");

    let service = SkillsService::with_managed(
        vec![(user.path().to_path_buf(), SkillScope::User)],
        vec![ManagedRoot {
            path: managed.path().to_path_buf(),
            plugin: "example-plugin".into(),
            version: "1.4.0".into(),
        }],
    );

    let conflicts = service.conflicts().unwrap();
    assert_eq!(conflicts.len(), 1, "one clashing name: {conflicts:?}");
    let conflict = &conflicts[0];
    assert_eq!(conflict.name, "kanban");
    let paths: Vec<_> = conflict.entries.iter().map(|e| e.path.clone()).collect();
    assert!(paths.contains(&managed_path), "{paths:?}");
    assert!(paths.contains(&user_path), "{paths:?}");
    // 两条都在，且来源可分辨——这就是"显示路径与来源"。
    assert!(
        conflict.has_managed(),
        "the clash must involve the managed copy"
    );
    assert!(
        conflict.entries.iter().any(|e| !e.source.is_managed()),
        "the clash must also name the non-managed copy"
    );
    // 两条都还在磁盘上：发现过程不得删改任何文件。
    assert!(managed_path.is_file());
    assert!(user_path.is_file());
}

/// 冲突未解决时，自动任务必须被拦住；但这不是"插件坏了"，所以是一个可以
/// 单独查询的结论，而不是让整个 catalog 失败。
#[test]
fn a_clash_blocks_automation_but_still_publishes_a_catalog() {
    let managed = tempfile::TempDir::new().unwrap();
    let user = tempfile::TempDir::new().unwrap();
    make_skill(managed.path(), "kanban", "Release-provided.");
    make_skill(user.path(), "kanban", "Mine.");

    let service = SkillsService::with_managed(
        vec![(user.path().to_path_buf(), SkillScope::User)],
        vec![ManagedRoot {
            path: managed.path().to_path_buf(),
            plugin: "example-plugin".into(),
            version: "1.4.0".into(),
        }],
    );

    assert_eq!(
        service.managed_visibility(),
        SkillVisibility::Blocked {
            reason: "conflict".to_string()
        },
        "an unresolved name clash must block plugin-owned automation"
    );
    // 普通聊天依赖 catalog；冲突不该让聊天没有 skill 可看。
    let catalog = service.render_catalog(4096);
    assert!(
        catalog.contains("kanban"),
        "the catalog must still be published"
    );
}

#[test]
fn no_clash_means_automation_is_allowed() {
    let managed = tempfile::TempDir::new().unwrap();
    make_skill(managed.path(), "kanban", "Drive the board.");

    let service = SkillsService::with_managed(
        Vec::new(),
        vec![ManagedRoot {
            path: managed.path().to_path_buf(),
            plugin: "example-plugin".into(),
            version: "1.4.0".into(),
        }],
    );
    assert_eq!(
        service.managed_visibility(),
        SkillVisibility::Available,
        "an unambiguous managed skill may drive automation"
    );
    assert!(service.conflicts().unwrap().is_empty());
}

/// 两个已安装版本都在磁盘上时，只认**被显式指定的那个**版本目录。
///
/// 去重靠的不是"目录里只有一份"，而是调用方按已提交激活给出确切的版本目录。
/// 这条钉住"不按最新目录猜"。
#[test]
fn only_the_given_release_directory_is_scanned() {
    let releases = tempfile::TempDir::new().unwrap();
    let old = releases.path().join("1.3.0");
    let new = releases.path().join("1.4.0");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    make_skill(&old, "kanban", "Old release.");
    make_skill(&new, "kanban", "New release.");

    let service = SkillsService::with_managed(
        Vec::new(),
        vec![ManagedRoot {
            path: old.clone(),
            plugin: "example-plugin".into(),
            version: "1.3.0".into(),
        }],
    );

    let skills = service.snapshot().unwrap();
    assert_eq!(skills.len(), 1, "only one release directory is in scope");
    assert!(
        skills[0].path.starts_with(&old),
        "the pinned release must be the one read: {}",
        skills[0].path.display()
    );
}

/// 受管来源缺失（插件被卸载/版本目录不可达）不是错误：只是没有那个 skill。
#[test]
fn a_missing_managed_root_yields_no_skill_and_no_error() {
    let service = SkillsService::with_managed(
        Vec::new(),
        vec![ManagedRoot {
            path: Path::new("/nonexistent/release/dir").to_path_buf(),
            plugin: "example-plugin".into(),
            version: "1.4.0".into(),
        }],
    );
    assert!(service.snapshot().unwrap().is_empty());
    assert_eq!(service.managed_visibility(), SkillVisibility::Available);
}
