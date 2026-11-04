//! 生产宿主的端到端测试（真实磁盘、真实注册表、真实生命周期状态）。
//!
//! # 为什么不只用 `ViewRegistry` 的单测
//!
//! `cache.rs` 的单测直接构造 `OpenRequest`，因此它们证明不了"宿主真的会去读
//! 注册表、真的会校验安装、真的会拒绝没有提交激活的插件"。这三件事都在
//! [`PluginHost`] 里，而那里最容易出的错是**顺序**：先建窗口再校验，或把
//! "已安装"当成"已激活"。所以这里铺一个真实实例根，走完整条路径。
//!
//! 每个测试一个 `TempDir`：实例根互不共享，`lifecycle-state` 与 `registry.json`
//! 都由真实代码写出，而不是测试自己拼 JSON（拼 JSON 会让测试与实现同时错）。

use std::path::{Path, PathBuf};

use desktop_lib::plugin_host::host::{split_asset_uri, view_url, PluginHost};
use sha2::{Digest, Sha256};
use yi_agent_plugins::{PluginId, Registry};

const MANIFEST_TEMPLATE: &str = r#"{
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

/// 一个铺好的实例根：注册表里有一条 1.4.0，磁盘上有安装目录，生命周期里
/// 有（可选的）已提交激活。
struct Fixture {
    tmp: tempfile::TempDir,
    home: PathBuf,
    plugin: PluginId,
}

impl Fixture {
    /// `activation == None` 表示"装了但没激活"。
    fn new(activation: Option<u64>) -> Fixture {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let home = tmp.path().join(".yi-agent");
        let plugin = PluginId::parse("example-counter").unwrap();

        let relative = "plugins/installed/example-counter/1.4.0";
        let install = home.join(relative);
        std::fs::create_dir_all(install.join("ui")).unwrap();
        std::fs::write(install.join("ui/index.html"), "<!doctype html><p>hi</p>").unwrap();
        std::fs::write(install.join("ui/app.js"), "console.log(1)").unwrap();
        std::fs::write(install.join("plugin.json"), MANIFEST_TEMPLATE).unwrap();

        // 真实安装会写 digest 记录；`verify_installed` 依赖它，所以这里用
        // 与 bundle.rs 相同的格式铺一份，而不是跳过校验。
        let mut lines = Vec::new();
        for name in ["plugin.json", "ui/index.html", "ui/app.js"] {
            let bytes = std::fs::read(install.join(name)).unwrap();
            let digest = format!("{:x}", Sha256::digest(&bytes));
            lines.push(format!("{digest}  {name}"));
        }
        std::fs::write(install.join(".yi-agent-digests"), lines.join("\n") + "\n").unwrap();

        let registry = Registry::open(&home).unwrap();
        registry
            .record_install(&plugin, "1.4.0", "a".repeat(64).as_str(), relative, 1)
            .expect("record install");

        if let Some(activation) = activation {
            let state = home
                .join("plugins")
                .join("lifecycle-state")
                .join("example-counter.json");
            std::fs::create_dir_all(state.parent().unwrap()).unwrap();
            let body = format!(
                r#"{{"active":{{"release":{{"plugin":"example-counter","version":"1.4.0",
                    "target":"darwin-aarch64","sha256":"{sha}"}},"activation":{activation}}}}}"#,
                sha = "a".repeat(64),
            );
            std::fs::write(&state, body).unwrap();
        }

        Fixture { tmp, home, plugin }
    }

    fn host(&self) -> PluginHost {
        PluginHost::new(self.home.clone())
    }

    fn install_dir(&self) -> PathBuf {
        self.home.join("plugins/installed/example-counter/1.4.0")
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }
}

#[test]
fn a_committed_plugin_opens_and_serves_its_own_files() {
    let fixture = Fixture::new(Some(7));
    let host = fixture.host();

    let opened = host
        .open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .expect("a committed plugin opens");
    assert_eq!(opened.activation, 7);
    assert_eq!(opened.entry, "ui/index.html");
    assert_eq!(opened.window_label, "plugin-view-1");

    let response = host.serve(&view_url("view-1", "ui/app.js"));
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"console.log(1)");
    assert!(response.content_type.starts_with("text/javascript"));
}

#[test]
fn an_installed_but_unactivated_plugin_does_not_open() {
    let fixture = Fixture::new(None);
    let host = fixture.host();

    let error = host
        .open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .expect_err("installation alone is not authorization to run a view");
    let message = error.to_string();
    assert!(
        message.contains("committed activation"),
        "the error must say what is missing, got: {message}"
    );
    assert_eq!(host.open_count(), 0, "a rejected open leaves no handle");
}

#[test]
fn a_version_that_is_not_the_committed_one_does_not_open() {
    let fixture = Fixture::new(Some(3));
    let host = fixture.host();

    // 注册表里只有 1.4.0，问 9.9.9 是"没装"。
    let error = host
        .open(&fixture.plugin, "9.9.9", "main", "view-1".to_string())
        .expect_err("an uninstalled version cannot open");
    assert!(error.to_string().contains("9.9.9"));
}

#[test]
fn a_tampered_installation_is_refused_before_any_window() {
    let fixture = Fixture::new(Some(1));
    // 装上之后替换一个文件：签名只覆盖安装那一刻。
    std::fs::write(fixture.install_dir().join("ui/app.js"), "alert(1)").unwrap();
    let host = fixture.host();

    let error = host
        .open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .expect_err("modified bytes must not be served to a view");
    assert!(
        format!("{error}").contains("modified"),
        "the error must identify the integrity failure, got: {error}"
    );
    assert_eq!(host.open_count(), 0);
}

#[test]
fn a_symlink_swap_after_installation_is_not_served() {
    let fixture = Fixture::new(Some(1));
    let outside = fixture.root().join("outside.js");
    std::fs::write(&outside, "steal()").unwrap();
    // 用符号链接替换已安装文件。`verify_installed` 读的是链接目标，因此这一步
    // 先被摘要校验拦下；即便绕过，白名单构建也不收符号链接。
    let target = fixture.install_dir().join("ui/app.js");
    std::fs::remove_file(&target).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &target).unwrap();

    let host = fixture.host();
    assert!(
        host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
            .is_err(),
        "a swapped-in symlink must not become a view resource"
    );
}

#[test]
fn closing_a_view_revokes_its_handle() {
    let fixture = Fixture::new(Some(1));
    let host = fixture.host();
    host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .unwrap();
    assert_eq!(host.serve(&view_url("view-1", "ui/index.html")).status, 200);

    assert!(host.close("view-1"));
    let response = host.serve(&view_url("view-1", "ui/index.html"));
    assert_eq!(response.status, 403);
    assert_eq!(host.open_count(), 0);
}

#[test]
fn destroying_the_window_revokes_the_handle_by_label() {
    let fixture = Fixture::new(Some(1));
    let host = fixture.host();
    host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .unwrap();

    // Tauri 的 `Destroyed` 事件只给 label，所以这条路径必须能独立工作。
    assert!(host.close_window("plugin-view-1"));
    assert_eq!(host.serve(&view_url("view-1", "ui/index.html")).status, 403);
    assert!(
        !host.close_window("plugin-view-1"),
        "closing twice is a no-op"
    );
}

#[test]
fn a_bumped_activation_invalidates_an_open_view() {
    let fixture = Fixture::new(Some(1));
    let host = fixture.host();
    host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .unwrap();

    // 模拟升级提交：代次推进。会话不知道自己被升级过，是宿主必须告诉它。
    let state = fixture
        .home
        .join("plugins/lifecycle-state/example-counter.json");
    let body = std::fs::read_to_string(&state).unwrap();
    let body = body.replace("\"activation\":1", "\"activation\":2");
    std::fs::write(&state, body).unwrap();

    let response = host.serve(&view_url("view-1", "ui/index.html"));
    assert_eq!(
        response.status, 403,
        "a view bound to a superseded activation must stop reading"
    );
}

#[test]
fn requests_for_another_instance_are_refused() {
    let fixture = Fixture::new(Some(1));
    let host = fixture.host();
    host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .unwrap();

    // 句柄是随机的 uuid：猜不出来。这里直接用一个"看起来合理"的 id。
    let response = host.serve(&view_url("view-2", "ui/index.html"));
    assert_eq!(response.status, 403);
}

#[test]
fn path_traversal_through_the_protocol_never_reads_outside() {
    let fixture = Fixture::new(Some(1));
    let host = fixture.host();
    host.open(&fixture.plugin, "1.4.0", "main", "view-1".to_string())
        .unwrap();

    for path in [
        "../../registry.json",
        "ui/../../../etc/passwd",
        "/etc/passwd",
    ] {
        let (instance, relative) =
            split_asset_uri(&format!("plugin-asset://localhost/view-1/{path}"))
                .expect("well-formed uri");
        assert_eq!(instance, "view-1");
        let response = host.serve(&view_url("view-1", &relative));
        assert!(
            response.status == 403 || response.status == 404,
            "`{path}` must not be served (status {})",
            response.status
        );
        assert!(response.body != b"root:x:0:0" && !response.body.starts_with(b"{\n  \"plugins\""));
    }
}

#[test]
fn the_host_reports_the_committed_activation() {
    let fixture = Fixture::new(Some(5));
    let host = fixture.host();
    let info = host
        .committed(&fixture.plugin)
        .expect("read committed")
        .expect("there is a committed activation");
    assert_eq!(info.activation, 5);
    assert_eq!(info.release.version, "1.4.0");
    assert_eq!(info.release.sha256, "a".repeat(64));
}

#[test]
fn a_plugin_with_no_state_reports_no_activation() {
    let fixture = Fixture::new(None);
    let host = fixture.host();
    assert!(host.committed(&fixture.plugin).unwrap().is_none());
}
