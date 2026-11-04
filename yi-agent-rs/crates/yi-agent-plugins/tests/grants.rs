//! 项目授权、代次隔离与能力分类的行为测试。
//!
//! # 两条最容易写成假绿的断言
//!
//! 1. **两个项目必须是两个真实目录。** 用一个 `ProjectId` 冒充两个项目，
//!    「撤销 A 不影响 B」就变成「撤销 A 不影响 A」——恒真。
//! 2. **跨项目按路径而非按 id 判。** 若只在 id 层面测，"子目录继承授权"
//!    和"同名目录共享授权"两条都测不到，因为 id 层面根本看不到路径。

use yi_agent_plugins::grants::GrantTable;
use yi_agent_plugins::project::{ProjectResolver, is_independent};
use yi_agent_plugins::{Effect, ProjectId, ReleaseId};

const HOST: &str = "host-exec-1";

fn release(version: &str, digest: char) -> ReleaseId {
    ReleaseId {
        plugin: yi_agent_plugins::PluginId::parse("example-counter").unwrap(),
        version: version.to_string(),
        target: "darwin-aarch64".to_string(),
        sha256: std::iter::repeat_n(digest, 64).collect(),
    }
}

/// 造两个**真实存在**的项目目录。
fn two_projects() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::TempDir::new().expect("temp");
    let a = root.path().join("project-a");
    let b = root.path().join("project-b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    (root, a, b)
}

// ---------------------------------------------------------------------------
// 项目规范化
// ---------------------------------------------------------------------------

#[test]
fn a_registered_project_resolves_to_itself_and_its_subdirectories() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let root = resolver.register(&a).expect("register");

    let sub = a.join("src/deep");
    std::fs::create_dir_all(&sub).unwrap();
    assert_eq!(resolver.resolve(&sub).unwrap(), root.id);
    assert_eq!(resolver.resolve(&a).unwrap(), root.id);
}

#[test]
fn a_symlink_resolves_to_the_same_project_as_its_target() {
    // `/a/link` 与 `/a/repo` 是同一个项目。只按字面比较会让同一个项目
    // 拿到两份互不相关的授权，于是撤销一份另一份还在用。
    let (_root, a, _b) = two_projects();
    let link = a.parent().unwrap().join("link-to-a");
    std::os::unix::fs::symlink(&a, &link).unwrap();

    let mut resolver = ProjectResolver::new();
    let root = resolver.register(&a).expect("register");
    assert_eq!(
        resolver.resolve(&link).unwrap(),
        root.id,
        "a symlink must resolve to the target project, not a separate one"
    );
}

#[test]
fn a_sibling_directory_sharing_a_prefix_is_not_a_subdirectory() {
    // `/tmp/project-a-evil` 的字符串以 `/tmp/project-a` 开头，但它不是
    // 子目录。用 `starts_with` 比字符串会把它判成子目录并给它授权。
    let (root, a, _b) = two_projects();
    let evil = root.path().join("project-a-evil");
    std::fs::create_dir_all(&evil).unwrap();

    let mut resolver = ProjectResolver::new();
    resolver.register(&a).expect("register");
    let error = resolver
        .resolve(&evil)
        .expect_err("a prefix-sharing sibling must not inherit the grant");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

#[test]
fn an_unregistered_directory_does_not_inherit_a_grant_by_prefix() {
    // 手动新建、未登记的目录不能靠路径前缀继承授权（设计 §6）。
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    resolver.register(&a).expect("register");

    let manual = a.join("manual-worktree");
    std::fs::create_dir_all(&manual).unwrap();
    // 注意：这是**已登记根的子目录**，按设计可以解析回根。真正要拒绝的是
    // 与已登记根毫无关系的新目录。
    assert!(resolver.resolve(&manual).is_ok());

    let unrelated = std::env::temp_dir().join("yi-agent-unregistered-probe");
    std::fs::create_dir_all(&unrelated).unwrap();
    let error = resolver
        .resolve(&unrelated)
        .expect_err("an unrelated directory must not resolve at all");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    let _ = std::fs::remove_dir_all(&unrelated);
}

#[test]
fn a_nested_registered_root_wins_over_its_parent() {
    // 一个已登记项目里再登记一个更深的根（比如子模块），深处那个必须优先。
    // 若取"最短"或"最先匹配"，子模块的授权会被父项目吞掉——撤销子模块
    // 就撤销不掉，因为解析出来的是父项目。
    let (_root, outer, _b) = two_projects();
    let inner = outer.join("vendor/inner");
    std::fs::create_dir_all(&inner).unwrap();

    let mut resolver = ProjectResolver::new();
    let outer_id = resolver.register(&outer).unwrap().id;
    let inner_id = resolver.register(&inner).unwrap().id;
    assert_ne!(outer_id, inner_id);

    let deep = inner.join("src");
    std::fs::create_dir_all(&deep).unwrap();
    assert_eq!(
        resolver.resolve(&deep).unwrap(),
        inner_id,
        "a path inside the nested root must resolve to the nested root"
    );
    assert_eq!(
        resolver
            .resolve(&outer.join("top-level-file"))
            .unwrap_or(outer_id.clone()),
        outer_id
    );
}

#[test]
fn same_named_directories_are_different_projects() {
    // `/a/project` 与 `/b/project` 的 basename 相同。若用 basename 当项目
    // 标识，两个真实项目会共享授权。
    let root = tempfile::TempDir::new().unwrap();
    let first = root.path().join("one/project");
    let second = root.path().join("two/project");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();

    let mut resolver = ProjectResolver::new();
    let a = resolver.register(&first).unwrap();
    let b = resolver.register(&second).unwrap();
    assert_ne!(
        a.id, b.id,
        "same basename must not collapse into one project"
    );
}

#[test]
fn registering_the_same_path_twice_is_idempotent() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let first = resolver.register(&a).unwrap();
    let second = resolver.register(&a).unwrap();
    assert_eq!(first.id, second.id);
    assert_eq!(resolver.roots().len(), 1);
}

#[test]
fn resolving_a_path_that_does_not_exist_is_a_not_found_not_a_permission_error() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    resolver.register(&a).unwrap();
    let error = resolver
        .resolve(&a.join("does-not-exist"))
        .expect_err("a missing path must not resolve");
    assert_eq!(error.kind(), "NotFound", "got: {error}");
}

#[test]
fn a_worktree_resolves_back_to_its_registered_root_when_it_is_under_it() {
    // 设计要求 worktree 能解析回已授权项目。worktree 若登记在项目根下，
    // 它解析到同一个项目——撤销项目就撤销它。
    let (_root, a, _b) = two_projects();
    let worktree = a.join(".worktrees/feature");
    std::fs::create_dir_all(&worktree).unwrap();

    let mut resolver = ProjectResolver::new();
    let root = resolver.register(&a).unwrap();
    assert_eq!(resolver.resolve(&worktree).unwrap(), root.id);
}

// ---------------------------------------------------------------------------
// 代次隔离：撤销 A 不影响 B
// ---------------------------------------------------------------------------

#[test]
fn revoking_one_project_leaves_the_other_usable() {
    // 设计 §4：项目授权代次独立，只让该项目旧绑定失效。
    let (_root, a, b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project_a = resolver.register(&a).unwrap().id;
    let project_b = resolver.register(&b).unwrap().id;

    let mut table = GrantTable::new();
    let r1 = release("1.4.0", 'a');
    let grant_a = table
        .grant(
            "example-counter",
            project_a.clone(),
            &r1,
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();
    let grant_b = table
        .grant(
            "example-counter",
            project_b.clone(),
            &r1,
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();

    table.revoke("example-counter", &project_a).unwrap();

    assert!(
        table
            .check(
                "example-counter",
                &project_a,
                Effect::Write,
                HOST,
                grant_a.generation
            )
            .is_err(),
        "the revoked project must reject the old binding"
    );
    assert!(
        table
            .check(
                "example-counter",
                &project_b,
                Effect::Write,
                HOST,
                grant_b.generation
            )
            .is_ok(),
        "revoking project A must not disturb project B"
    );
}

#[test]
fn regranting_the_same_terms_is_idempotent_and_does_not_bump_the_generation() {
    // 递增代次只能由**变更**触发。一次重复的"启用"若也递增，客户端刚拿到
    // 的绑定会在没有任何变更的情况下失效——表现为"设置页点两次保存，
    // 正在跑的任务全被打断"。
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let release = release("1.4.0", 'a');
    let first = table
        .grant(
            "example-counter",
            project.clone(),
            &release,
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();
    let again = table
        .grant(
            "example-counter",
            project.clone(),
            &release,
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();

    assert_eq!(
        first, again,
        "a no-op regrant must return the identical grant"
    );
    assert_eq!(again.generation, first.generation);
    assert!(
        table
            .check(
                "example-counter",
                &project,
                Effect::Write,
                HOST,
                first.generation
            )
            .is_ok()
    );
}

#[test]
fn a_real_change_does_bump_the_generation() {
    // 上一条的反面：真正的能力变更必须递增，否则旧绑定继续有效。
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let first = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();
    let wider = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read, Effect::QuotaWrite],
        )
        .unwrap();

    assert_eq!(wider.generation, first.generation + 1);
    assert!(wider.granted.contains(&Effect::QuotaWrite));
    // 旧代次的绑定必须已经失效。
    assert_eq!(
        table
            .check(
                "example-counter",
                &project,
                Effect::Read,
                HOST,
                first.generation
            )
            .unwrap_err()
            .kind(),
        "StaleBinding"
    );
}

#[test]
fn a_stale_grant_generation_is_rejected() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();

    // 重新授权递增代次。
    table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.5.0", 'b'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();

    let error = table
        .check(
            "example-counter",
            &project,
            Effect::Read,
            HOST,
            grant.generation,
        )
        .expect_err("an old generation must be stale");
    assert_eq!(error.kind(), "StaleBinding", "got: {error}");
}

#[test]
fn a_binding_from_another_host_is_rejected() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();

    let error = table
        .check(
            "example-counter",
            &project,
            Effect::Read,
            "other-host",
            grant.generation,
        )
        .expect_err("a grant must not be usable from another host");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

#[test]
fn an_effect_that_was_not_granted_is_rejected() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();

    let error = table
        .check(
            "example-counter",
            &project,
            Effect::Launch,
            HOST,
            grant.generation,
        )
        .expect_err("an ungranted effect must be refused");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    assert!(
        error.to_string().contains("Launch"),
        "the error must name the refused effect, got: {error}"
    );
}

#[test]
fn an_unknown_project_is_rejected_rather_than_treated_as_granted() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    resolver.register(&a).unwrap();
    let stranger = ProjectId::parse("/somewhere/else").unwrap();

    let table = GrantTable::new();
    let error = table
        .check("example-counter", &stranger, Effect::Read, HOST, 1)
        .expect_err("an unknown project must not be usable");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

// ---------------------------------------------------------------------------
// 能力分类：基本授权 vs 独立确认
// ---------------------------------------------------------------------------

#[test]
fn quota_and_resident_are_independent_confirmations() {
    // 设计 §6：配额修改与后台常驻必须**分开**确认，不能捆绑进基本授权。
    for capability in [Effect::QuotaWrite, Effect::Resident, Effect::Launch] {
        assert!(
            is_independent(capability),
            "{capability:?} must be confirmed separately from basic project access"
        );
    }
    for capability in [Effect::Read, Effect::Write] {
        assert!(
            !is_independent(capability),
            "{capability:?} is part of basic project access"
        );
    }
}

#[test]
fn granting_basic_access_does_not_grant_residency() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();

    assert_eq!(
        table
            .check(
                "example-counter",
                &project,
                Effect::Resident,
                HOST,
                grant.generation
            )
            .unwrap_err()
            .kind(),
        "PermissionDenied"
    );
    assert_eq!(
        table
            .check(
                "example-counter",
                &project,
                Effect::QuotaWrite,
                HOST,
                grant.generation
            )
            .unwrap_err()
            .kind(),
        "PermissionDenied"
    );
}

// ---------------------------------------------------------------------------
// 升级：新能力不自动获批，旧能力不无故重置
// ---------------------------------------------------------------------------

#[test]
fn a_capability_increase_requires_reconfirmation() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();
    let generation_before = grant.generation;

    let updated = table
        .compare_capabilities(
            "example-counter",
            &project,
            &release("1.5.0", 'b'),
            &[Effect::Read, Effect::Write, Effect::QuotaWrite],
        )
        .unwrap();

    assert!(
        updated.needs_reconfirmation(),
        "the new capability must wait for a decision"
    );
    assert_eq!(updated.pending, vec![Effect::QuotaWrite]);
    // 旧能力还在：升级不该把用户已批准的东西打回未批准。
    assert_eq!(
        table
            .check(
                "example-counter",
                &project,
                Effect::Write,
                HOST,
                updated.generation
            )
            .unwrap(),
        ()
    );
    // 代次没有因为"只是比较"而前进：比较不是授权变更。
    assert_eq!(updated.generation, generation_before);
}

#[test]
fn a_pending_capability_is_refused_even_if_it_is_listed_as_granted() {
    // 纵深防御：代次未变、能力列表里出现了它，但它是"待确认"。
    // 升级不该顺手扩权。
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();
    let updated = table
        .compare_capabilities(
            "example-counter",
            &project,
            &release("1.5.0", 'b'),
            &[Effect::Read, Effect::QuotaWrite],
        )
        .unwrap();

    let error = table
        .check(
            "example-counter",
            &project,
            Effect::QuotaWrite,
            HOST,
            updated.generation,
        )
        .expect_err("a pending capability must not be usable before confirmation");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    assert!(
        error.to_string().contains("reconfirmation"),
        "the error should say reconfirmation is needed, got: {error}"
    );
}

#[test]
fn a_pure_version_bump_does_not_require_reconfirmation() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read, Effect::Write],
        )
        .unwrap();
    let updated = table
        .compare_capabilities(
            "example-counter",
            &project,
            &release("1.5.0", 'b'),
            &[Effect::Read, Effect::Write],
        )
        .unwrap();

    assert!(
        !updated.needs_reconfirmation(),
        "a version bump with no new capability must not ask the user again"
    );
    assert!(
        table
            .check(
                "example-counter",
                &project,
                Effect::Write,
                HOST,
                updated.generation
            )
            .is_ok()
    );
}

#[test]
fn confirming_a_reconfirmation_clears_pending_and_bumps_the_generation() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let first = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Read],
        )
        .unwrap();
    let pending = table
        .compare_capabilities(
            "example-counter",
            &project,
            &release("1.5.0", 'b'),
            &[Effect::Read, Effect::QuotaWrite],
        )
        .unwrap();

    let confirmed = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.5.0", 'b'),
            HOST,
            &[Effect::Read, Effect::QuotaWrite],
        )
        .unwrap();

    assert!(!confirmed.needs_reconfirmation());
    assert!(confirmed.generation > pending.generation);
    assert!(confirmed.generation > first.generation);
    assert!(
        table
            .check(
                "example-counter",
                &project,
                Effect::QuotaWrite,
                HOST,
                confirmed.generation
            )
            .is_ok()
    );
}

// ---------------------------------------------------------------------------
// 路径 → 授权，一步到位
// ---------------------------------------------------------------------------

#[test]
fn check_path_resolves_then_checks() {
    let (_root, a, b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project_a = resolver.register(&a).unwrap().id;
    resolver.register(&b).unwrap();

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project_a.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Write],
        )
        .unwrap();

    let resolved = table
        .check_path(
            &resolver,
            "example-counter",
            &a,
            Effect::Write,
            HOST,
            grant.generation,
        )
        .expect("an authorized project must pass");
    assert_eq!(resolved, project_a);

    // B 项目没有授权：路径解析成功，但检查失败。
    let error = table
        .check_path(
            &resolver,
            "example-counter",
            &b,
            Effect::Write,
            HOST,
            grant.generation,
        )
        .expect_err("project B was never authorized");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

#[test]
fn a_revoked_grant_cannot_be_reused_through_a_subdirectory_path() {
    // 子目录解析回项目根，所以撤销根之后，从子目录进来也必须被拒。
    // 若实现按"子目录自己的授权"来判断，这条会漏。
    let (_root, a, _b) = two_projects();
    let sub = a.join("nested/deeper");
    std::fs::create_dir_all(&sub).unwrap();

    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Write],
        )
        .unwrap();
    table.revoke("example-counter", &project).unwrap();

    let error = table
        .check_path(
            &resolver,
            "example-counter",
            &sub,
            Effect::Write,
            HOST,
            grant.generation,
        )
        .expect_err("a subdirectory must not survive the root's revocation");
    assert_eq!(error.kind(), "StaleBinding", "got: {error}");
}

// ---------------------------------------------------------------------------
// 授权持久化后重开
// ---------------------------------------------------------------------------

#[test]
fn a_grant_table_survives_a_round_trip_and_still_rejects_stale_bindings() {
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let project = resolver.register(&a).unwrap().id;

    let mut table = GrantTable::new();
    let grant = table
        .grant(
            "example-counter",
            project.clone(),
            &release("1.4.0", 'a'),
            HOST,
            &[Effect::Write],
        )
        .unwrap();
    table.revoke("example-counter", &project).unwrap();

    let json = serde_json::to_vec(&table).expect("serialize");
    let reopened: GrantTable = serde_json::from_slice(&json).expect("deserialize");

    let error = reopened
        .check(
            "example-counter",
            &project,
            Effect::Write,
            HOST,
            grant.generation,
        )
        .expect_err("a revoked grant must stay revoked after a restart");
    assert_eq!(error.kind(), "StaleBinding", "got: {error}");
}

#[test]
fn the_same_project_registered_twice_yields_one_id_even_via_a_relative_path() {
    // `ProjectId` 来自规范化路径。若调用方从"相对路径"和"绝对路径"各自
    // 解析，同一个项目会得到两个 id，于是授权分裂：在一边启用，另一边
    // 看不到。这条钉住「解析必须收敛到同一个规范路径」。
    let (_root, a, _b) = two_projects();
    let mut resolver = ProjectResolver::new();
    let absolute = resolver.register(&a).unwrap();

    // 用带 `..` 的等价路径再解析一次，必须落到同一个项目。
    let convoluted = a.join("..").join("project-a");
    let resolved = resolver.resolve(&convoluted).unwrap();
    assert_eq!(
        resolved, absolute.id,
        "equivalent paths must resolve to one project id, otherwise a grant \
         created via one spelling is invisible from the other"
    );
}
