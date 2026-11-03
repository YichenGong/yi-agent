//! T17 Step 1：资源服务的边界测试。
//!
//! 这些测试都**直接读磁盘**，不经过 WebView。它们证明的是宿主的判定逻辑；
//! "WebView 里真的拿不到"由 `plugin-probe` 的 isolation suite 证明（Step 4）。
//! 两者不能互相代替：逻辑对但原生接线漏了，页面照样能读；反之亦然。

use std::path::{Path, PathBuf};

use yi_agent_plugins::{CommittedActivation, InstalledRelease, Manifest, ReleaseId};

use crate::plugin_host::assets::AssetCache;
use crate::plugin_host::cache::{OpenRequest, Rejection, ViewRegistry};
use crate::plugin_host::container::ViewRejection;
use crate::plugin_host::remote_assets::HostIdentity;

const MANIFEST: &str = r#"{
  "schemaVersion": 1,
  "packageFormat": 1,
  "plugin": "example-counter",
  "version": "1.4.0",
  "target": "darwin-aarch64",
  "views": [
    { "id": "main", "kind": "main", "entry": "ui/index.html",
      "digest": "3b1f2c9a7d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8" }
  ],
  "skills": [],
  "methods": [
    { "name": "counters/read", "effect": "Read" }
  ],
  "cli": [],
  "capabilities": { "required": [], "optional": [] },
  "dependencies": { "workflows": [] },
  "data": { "format": 1, "migrations": [] },
  "compatibility": { "hostApi": "v1", "uiApi": "v1" }
}"#;

fn manifest() -> Manifest {
    Manifest::parse(MANIFEST.as_bytes()).expect("fixture manifest must parse")
}

fn release_id(version: &str, sha: &str) -> ReleaseId {
    ReleaseId {
        plugin: yi_agent_plugins::PluginId::parse("example-counter").unwrap(),
        version: version.to_string(),
        target: "darwin-aarch64".to_string(),
        sha256: sha.to_string(),
    }
}

/// 在 `dir` 下铺一个最小安装：入口 + 一个样式 + 一个脚本。
fn install_at(dir: &Path) -> InstalledRelease {
    std::fs::create_dir_all(dir.join("ui")).unwrap();
    std::fs::write(dir.join("ui/index.html"), "<!doctype html><p>hi</p>").unwrap();
    std::fs::write(dir.join("ui/style.css"), "p{color:red}").unwrap();
    std::fs::write(dir.join("ui/app.js"), "console.log(1)").unwrap();
    InstalledRelease {
        destination: dir.to_path_buf(),
        files_written: 3,
        total_bytes: 0,
        program_entry: None,
    }
}

fn cache_for(dir: &Path) -> AssetCache {
    let installed = install_at(dir);
    AssetCache::load(&installed, &manifest()).expect("cache must load")
}

fn committed(activation: u64, version: &str, sha: &str) -> CommittedActivation {
    CommittedActivation {
        release: release_id(version, sha),
        activation,
    }
}

fn open(registry: &mut ViewRegistry, dir: &Path, instance: &str, activation: u64) -> ReleaseId {
    let installed = install_at(dir);
    let manifest = manifest();
    let release = release_id("1.4.0", "aaa");
    let host = HostIdentity::local("test-instance");
    registry
        .open(OpenRequest {
            instance_id: instance.to_string(),
            release: &release,
            installation: &installed,
            manifest: &manifest,
            activation,
            view: "main",
            host: &host,
        })
        .expect("open must succeed");
    release
}

/// 打开视图所用的本机宿主身份。与 `open` 里的同一份。
fn local_host() -> HostIdentity {
    HostIdentity::local("test-instance")
}

#[test]
fn resolves_only_files_that_were_present_when_opened() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_for(tmp.path());

    let index = cache
        .resolve("ui/index.html")
        .expect("declared entry resolves");
    assert_eq!(
        std::fs::read_to_string(index).unwrap(),
        "<!doctype html><p>hi</p>"
    );

    // 打开之后才出现的文件不属于这次发布。
    std::fs::write(tmp.path().join("ui/late.js"), "late").unwrap();
    assert!(
        cache.resolve("ui/late.js").is_none(),
        "a file created after the release was verified must not be readable"
    );
}

#[test]
fn rejects_paths_outside_the_package() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_for(tmp.path());

    for request in [
        "../outside.txt",
        "ui/../../outside.txt",
        "/etc/passwd",
        "",
        "./",
        "ui//../../x",
    ] {
        assert!(
            cache.resolve(request).is_none(),
            "`{request}` must not resolve"
        );
    }
    assert!(cache.resolve("ui/index.html").is_some());
}

#[test]
fn skips_symlinks_so_a_swapped_file_stays_invisible() {
    let tmp = tempfile::TempDir::new().unwrap();
    let cache = cache_for(tmp.path());

    // 包外有一个看起来合法的目标。
    let secret = tmp.path().parent().unwrap().join("secret.txt");
    std::fs::write(&secret, "top secret").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&secret, tmp.path().join("ui/evil.js")).unwrap();

    assert!(
        cache.resolve("ui/evil.js").is_none(),
        "a symlink pointing outside the install must not become a resource"
    );

    let _ = std::fs::remove_file(&secret);
}

#[test]
fn a_missing_declared_entry_fails_at_load_time() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(tmp.path().join("ui")).unwrap();
    // 清单声明了 ui/index.html，但不写它。
    let installed = InstalledRelease {
        destination: tmp.path().to_path_buf(),
        files_written: 0,
        total_bytes: 0,
        program_entry: None,
    };
    let error = AssetCache::load(&installed, &manifest())
        .expect_err("a declared entry that is not installed must fail");
    let message = error.to_string();
    assert!(
        message.contains("ui/index.html"),
        "the error must name the missing file, got: {message}"
    );
}

#[test]
fn serves_a_view_through_a_host_issued_handle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, tmp.path(), "view-1", 7);
    let active = committed(7, "1.4.0", &release.sha256);

    let body = registry
        .resolve("view-1", "ui/app.js", Some(&active), Some(&local_host()))
        .expect("a handle opened for this activation can read its own files");
    assert_eq!(body, b"console.log(1)");

    // 句柄本身不暴露安装根。
    let instance = registry.get("view-1").unwrap();
    assert_eq!(instance.instance_id, "view-1");
    assert_eq!(instance.activation, 7);
    assert_eq!(instance.view, "main");
}

#[test]
fn rejects_unknown_handles_instead_of_falling_back() {
    let registry = ViewRegistry::new();
    match registry.resolve("never-opened", "ui/index.html", None, Some(&local_host())) {
        Err(Rejection::View(ViewRejection::UnknownInstance)) => {}
        other => panic!("expected UnknownInstance, got {other:?}"),
    }
}

#[test]
fn closed_handles_stop_reading_immediately() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, tmp.path(), "view-1", 1);
    let active = committed(1, "1.4.0", &release.sha256);

    assert!(registry.close("view-1"));
    assert!(!registry.close("view-1"));
    match registry.resolve(
        "view-1",
        "ui/index.html",
        Some(&active),
        Some(&local_host()),
    ) {
        Err(Rejection::View(ViewRejection::UnknownInstance)) => {}
        other => panic!("a closed handle must not read, got {other:?}"),
    }
}

#[test]
fn a_handle_from_a_superseded_activation_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, tmp.path(), "view-1", 3);

    // 升级已提交：同一个发布，代次推进到 4。
    let next = committed(4, "1.4.0", &release.sha256);
    match registry.resolve("view-1", "ui/index.html", Some(&next), Some(&local_host())) {
        Err(Rejection::View(ViewRejection::StaleGeneration {
            bound: 3,
            current: Some(4),
        })) => {}
        other => panic!("expected StaleGeneration, got {other:?}"),
    }
}

#[test]
fn a_handle_from_a_different_release_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    // 打开的是版本 1.4.0。
    open(&mut registry, tmp.path(), "view-1", 1);

    // 已提交的是版本 1.5.0（不同摘要）：即使代次同为 1，也不是同一个发布。
    let other = committed(1, "1.5.0", "bbb");
    match registry.resolve("view-1", "ui/index.html", Some(&other), Some(&local_host())) {
        Err(Rejection::View(ViewRejection::StaleGeneration { bound: 1, .. })) => {}
        other => panic!("a cross-version token must be refused, got {other:?}"),
    }
}

#[test]
fn a_handle_for_a_different_plugin_cannot_read_another_package() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    open(&mut registry, tmp.path(), "view-1", 1);

    // 另一个包的已提交状态指向别的插件。
    let foreign = CommittedActivation {
        release: ReleaseId {
            plugin: yi_agent_plugins::PluginId::parse("other-plugin").unwrap(),
            version: "1.4.0".to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: "aaa".to_string(),
        },
        activation: 1,
    };
    assert!(
        matches!(
            registry.resolve(
                "view-1",
                "ui/index.html",
                Some(&foreign),
                Some(&local_host())
            ),
            Err(Rejection::View(ViewRejection::StaleGeneration { .. }))
        ),
        "a handle must not be validated against another plugin's activation"
    );
}

#[test]
fn rejects_paths_that_are_not_in_the_release_through_a_handle() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, tmp.path(), "view-1", 1);
    let active = committed(1, "1.4.0", &release.sha256);

    for request in [
        "../secret",
        "/etc/passwd",
        "ui/missing.js",
        "ui/../ui/index.html",
    ] {
        match registry.resolve("view-1", request, Some(&active), Some(&local_host())) {
            Err(Rejection::Path(_)) => {}
            other => panic!("`{request}` must be a path rejection, got {other:?}"),
        }
    }
}

#[test]
fn works_under_a_unicode_install_root() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root: PathBuf = tmp.path().join("插件根/示例-计数器");
    std::fs::create_dir_all(&root).unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, &root, "view-1", 2);
    let active = committed(2, "1.4.0", &release.sha256);

    let body = registry
        .resolve(
            "view-1",
            "ui/index.html",
            Some(&active),
            Some(&local_host()),
        )
        .expect("non-ASCII install roots must work");
    assert_eq!(body, b"<!doctype html><p>hi</p>");
}

#[test]
fn refuses_to_open_the_same_instance_twice() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    open(&mut registry, tmp.path(), "view-1", 1);

    let installed = install_at(tmp.path());
    let manifest = manifest();
    let release = release_id("1.4.0", "aaa");
    let error = registry
        .open(OpenRequest {
            instance_id: "view-1".to_string(),
            release: &release,
            installation: &installed,
            manifest: &manifest,
            activation: 1,
            view: "main",
            host: &local_host(),
        })
        .expect_err("reusing an instance id must not silently replace the old handle");
    assert!(error.to_string().contains("already open"));
}

#[test]
fn a_view_not_declared_by_the_manifest_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let installed = install_at(tmp.path());
    let manifest = manifest();
    let release = release_id("1.4.0", "aaa");
    let error = registry
        .open(OpenRequest {
            instance_id: "view-1".to_string(),
            release: &release,
            installation: &installed,
            manifest: &manifest,
            activation: 1,
            view: "settings",
            host: &local_host(),
        })
        .expect_err("settings is not in this manifest");
    assert!(error.to_string().contains("settings"));
    assert!(
        registry.is_empty(),
        "a failed open must not leave a half-registered handle"
    );
}

// ---------------------------------------------------------------------------
// T20：跨宿主隔离
// ---------------------------------------------------------------------------

/// 打开视图并绑定到指定宿主。`open` 用本机身份，这里要指定别的宿主。
fn open_on(
    registry: &mut ViewRegistry,
    dir: &Path,
    instance: &str,
    activation: u64,
    host: &HostIdentity,
) -> ReleaseId {
    let installed = install_at(dir);
    let manifest = manifest();
    let release = release_id("1.4.0", "aaa");
    registry
        .open(OpenRequest {
            instance_id: instance.to_string(),
            release: &release,
            installation: &installed,
            manifest: &manifest,
            activation,
            view: "main",
            host,
        })
        .expect("open must succeed");
    release
}

/// 宿主 A 的句柄在切到宿主 B 之后读不到任何字节。
///
/// 这是 T20 的核心断言：两个宿主上同一个插件、同一个版本、同一份字节，
/// 身份仍然不同。只按"插件 + 视图"记账的实现会在这里放行。
#[test]
fn a_handle_bound_to_host_a_stops_serving_once_host_b_is_active() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let host_a = HostIdentity::remote("profile-a", "inst-1");
    let host_b = HostIdentity::remote("profile-b", "inst-1");
    let release = open_on(&mut registry, tmp.path(), "view-a", 1, &host_a);
    let active = committed(1, "1.4.0", &release.sha256);

    // A 仍活跃时能读。
    assert!(registry
        .resolve("view-a", "ui/index.html", Some(&active), Some(&host_a))
        .is_ok());

    // 切到 B：同一句柄、同一代次、同一份已提交激活，仍然拒绝。
    match registry.resolve("view-a", "ui/index.html", Some(&active), Some(&host_b)) {
        Err(Rejection::View(ViewRejection::ForeignHost { .. })) => {}
        other => panic!("host B must not read host A's view, got {other:?}"),
    }
}

/// 撤销宿主之后，句柄立即变成"未知实例"，而不是"存在但被拒"。
#[test]
fn revoking_a_host_drops_its_handles_instead_of_leaving_them_dangling() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let host = HostIdentity::remote("profile-a", "inst-1");
    let release = open_on(&mut registry, tmp.path(), "view-a", 1, &host);
    let active = committed(1, "1.4.0", &release.sha256);
    assert_eq!(registry.len(), 1);

    assert_eq!(registry.revoke_host(&host), 1);
    assert_eq!(registry.revoke_host(&host), 0, "revoking twice is a no-op");
    assert!(registry.is_empty());
    match registry.resolve("view-a", "ui/index.html", Some(&active), Some(&host)) {
        Err(Rejection::View(ViewRejection::UnknownInstance)) => {}
        other => panic!("a revoked handle must be unknown, got {other:?}"),
    }
}

/// 撤销只影响目标宿主：另一个宿主的同名视图必须留下。
#[test]
fn revoking_one_host_leaves_the_other_host_intact() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let host_a = HostIdentity::remote("a", "i");
    let host_b = HostIdentity::remote("b", "i");
    let release_a = open_on(&mut registry, tmp.path(), "view-a", 1, &host_a);
    let release_b = open_on(&mut registry, tmp.path(), "view-b", 1, &host_b);
    let active_a = committed(1, "1.4.0", &release_a.sha256);
    let active_b = committed(1, "1.4.0", &release_b.sha256);

    assert_eq!(registry.revoke_host(&host_a), 1);
    assert!(matches!(
        registry.resolve("view-a", "ui/index.html", Some(&active_a), Some(&host_a)),
        Err(Rejection::View(ViewRejection::UnknownInstance))
    ));
    assert!(registry
        .resolve("view-b", "ui/index.html", Some(&active_b), Some(&host_b))
        .is_ok());
}

/// 同一个宿主推进代次之后，旧句柄失效，新句柄可用。
#[test]
fn advancing_the_host_generation_invalidates_the_previous_handles() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let mut host = HostIdentity::remote("a", "i");
    let release = open_on(&mut registry, tmp.path(), "view-old", 1, &host);
    let active = committed(1, "1.4.0", &release.sha256);

    host.generation += 1;
    match registry.resolve("view-old", "ui/index.html", Some(&active), Some(&host)) {
        Err(Rejection::View(ViewRejection::ForeignHost { bound, current })) => {
            assert_ne!(bound, current, "the rejection must name both generations");
        }
        other => panic!("an older generation must not be current, got {other:?}"),
    }

    let release = open_on(&mut registry, tmp.path(), "view-new", 1, &host);
    let active = committed(1, "1.4.0", &release.sha256);
    assert!(registry
        .resolve("view-new", "ui/index.html", Some(&active), Some(&host))
        .is_ok());
}

/// 宿主还没握手完成（`None`）时，远端句柄一律拒绝。
///
/// "不确定"绝不能退化成"放行"：连上第二个连接的那一瞬间就是最大的混淆窗口。
#[test]
fn an_unhandshaked_host_never_serves_a_remote_view() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let host = HostIdentity::remote("a", "i");
    let release = open_on(&mut registry, tmp.path(), "view-a", 1, &host);
    let active = committed(1, "1.4.0", &release.sha256);

    match registry.resolve("view-a", "ui/index.html", Some(&active), None) {
        Err(Rejection::View(ViewRejection::ForeignHost { current, .. })) => {
            assert!(current.contains("no host handshake"), "got {current}");
        }
        other => panic!("an unknown host must not serve a remote view, got {other:?}"),
    }
}

/// 本机视图在 `None` 时照旧可用：本地没有"另一个连接"可混淆。
#[test]
fn a_local_view_still_serves_when_the_host_argument_is_absent() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ViewRegistry::new();
    let release = open(&mut registry, tmp.path(), "view-1", 1);
    let active = committed(1, "1.4.0", &release.sha256);

    assert_eq!(
        registry
            .resolve("view-1", "ui/index.html", Some(&active), None)
            .expect("local views do not need a remote handshake"),
        b"<!doctype html><p>hi</p>"
    );
}
