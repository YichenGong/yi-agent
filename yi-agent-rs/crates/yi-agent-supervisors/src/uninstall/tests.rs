use super::*;
use yi_agent_plugins::{Binding, Effect, ReleaseId};

fn setup() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn plugin() -> PluginId {
    PluginId::parse("demo").expect("plugin id")
}

fn release(version: &str) -> ReleaseId {
    ReleaseId {
        plugin: plugin(),
        version: version.to_string(),
        target: "aarch64-apple-darwin".to_string(),
        sha256: "a".repeat(64),
    }
}

/// 造一个"已安装 + 已授权 + 已激活"的实例，并把版本目录真的建出来。
fn installed_home(project: &Path, versions: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let home = setup();
    let registry = Registry::open(home.path()).expect("registry");
    for version in versions {
        let relative = format!("plugins/installed/demo/{version}/aarch64-apple-darwin");
        let directory = home.path().join(&relative);
        std::fs::create_dir_all(&directory).expect("version dir");
        std::fs::write(directory.join("plugin.json"), b"{}").expect("manifest");
        registry
            .record_install(&plugin(), version, &"a".repeat(64), &relative, 1)
            .expect("record install");
    }
    let project_id = ProjectId::parse(&project.display().to_string()).expect("project id");
    grants::update(home.path(), |table| {
        table
            .grant(
                plugin().as_str(),
                project_id,
                &release(versions[versions.len() - 1]),
                "host-test",
                &[Effect::Read, Effect::Write],
            )
            .map(|_| ())
    })
    .expect("grant");
    let path = home.path().to_path_buf();
    (home, path)
}

/// 卸载默认不删用户数据，并把保留了什么讲清楚。
#[test]
fn uninstall_keeps_user_data_by_default_and_lists_it() {
    let project = setup();
    let data = project.path().join(".yi-agent/plugins/demo");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("cards.json"), b"[]").expect("cards");

    let (home, _) = installed_home(project.path(), &["1.0.0"]);
    let outcome = uninstall(home.path(), &plugin(), false).expect("uninstall");

    assert_eq!(outcome.versions_removed, 1);
    assert_eq!(outcome.grants_revoked, 1);
    assert_eq!(
        outcome.retained.len(),
        1,
        "the project's data must be listed"
    );
    assert_eq!(
        outcome.retained[0].path.as_deref(),
        Some(data.display().to_string().as_str())
    );
    assert!(
        data.join("cards.json").exists(),
        "uninstalling a program must not delete the user's data"
    );

    // 注册记录必须没了，否则下一次安装会撞上遗留状态。
    assert!(
        Registry::open(home.path())
            .expect("registry")
            .list()
            .expect("list")
            .is_empty()
    );
    // 授权记录有意保留（代次已前进、能力已清空）：删掉记录会让"旧代次的
    // 绑定"无法被识别为「已撤销」，只能被判成「未知项目」。所以这里断言
    // 的是"它不再授权任何东西"，而不是"记录不存在"。
    let table = grants::load(home.path()).expect("grants");
    for grant in table.all() {
        assert_eq!(grant.plugin, "demo");
        assert!(
            grant.granted.is_empty() && grant.pending.is_empty(),
            "a revoked grant must authorize nothing, got {:?}",
            grant.granted
        );
    }
}

/// 显式要求才删数据。
#[test]
fn uninstall_removes_data_only_when_asked() {
    let project = setup();
    let data = project.path().join(".yi-agent/plugins/demo");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("cards.json"), b"[]").expect("cards");

    let (home, _) = installed_home(project.path(), &["1.0.0"]);
    let outcome = uninstall(home.path(), &plugin(), true).expect("uninstall");

    assert!(
        outcome.retained.is_empty(),
        "nothing is retained when asked to clear"
    );
    assert!(
        !data.exists(),
        "explicit clear must actually delete the data"
    );
}

/// 卸载一个没装过的插件报 NotFound，不能静默成功。
#[test]
fn uninstall_of_an_unknown_plugin_is_not_found() {
    let home = setup();
    let error = uninstall(home.path(), &plugin(), false).expect_err("unknown plugin");
    assert!(
        matches!(error, PluginError::NotFound(_)),
        "want NotFound, got {error:?}"
    );
}

/// gc 留下激活版本，删掉没人引用的旧版本。
#[test]
fn gc_removes_only_versions_nothing_references() {
    let project = setup();
    let (home, path) = installed_home(project.path(), &["1.0.0", "2.0.0"]);

    // 授权指向 2.0.0；激活记录是空的（本测试不跑完整事务），所以 1.0.0
    // 既没激活也没授权——它才是唯一可以删的那个。
    let outcome = gc(&path).expect("gc");

    assert_eq!(outcome.removed.len(), 1);
    assert!(
        outcome.removed[0].contains("1.0.0"),
        "the unreferenced version must be the one removed, got {:?}",
        outcome.removed
    );
    assert!(
        !home.path().join(&outcome.removed[0]).exists(),
        "the directory really has to be gone"
    );
    assert!(
        outcome.kept.iter().any(|kept| kept.path.contains("2.0.0")),
        "the granted version must be kept"
    );
}

/// gc 从不删仍被授权的版本，即使它没有激活。
#[test]
fn gc_keeps_a_granted_version_even_without_activation() {
    let project = setup();
    let (home, path) = installed_home(project.path(), &["1.0.0"]);

    let outcome = gc(&path).expect("gc");

    assert!(
        outcome.removed.is_empty(),
        "a granted version must never be collected"
    );
    assert!(
        home.path()
            .join("plugins/installed/demo/1.0.0/aarch64-apple-darwin")
            .exists()
    );
}

/// 数据目录只在项目路径是绝对路径时才算得出来。
#[test]
fn plugin_data_dir_requires_an_absolute_project_path() {
    assert!(plugin_data_dir(&plugin(), "relative/dir").is_none());

    let project = setup();
    let path = plugin_data_dir(&plugin(), &project.path().display().to_string())
        .expect("absolute projects have a data dir");
    assert!(path.starts_with(project.path()));
    assert!(path.ends_with("demo"));
}

// A binding type import keeps the test module honest about what `grants`
// stores; if `Binding` ever needs to be consulted here, this fails to compile
// instead of silently drifting.
#[allow(dead_code)]
fn _binding_shape_is_available(binding: Binding) -> ReleaseId {
    binding.release
}
