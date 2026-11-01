use super::*;

#[test]
fn plugin_channel_shared_state_is_owned_and_send() {
    fn assert_spawnable<T: Send + Sync + 'static>() {}
    assert_spawnable::<crate::rpc::PluginChannelState>();
}

/// 值守开关（[`crate::settings_store::WATCHMAN_KEY`]，兼容读旧键）的写路径:
/// `false` 卸载、`true` 安装,均经注入的
/// 记录器;偏好落盘且读回一致。
#[tokio::test(flavor = "multi_thread")]
async fn writing_the_watchman_setting_installs_or_uninstalls_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;
    let calls = Arc::clone(&h.watchman_calls);
    let home = h._watchman_home.path().to_path_buf();

    // 这里故意用**旧键**发写请求：一台机器上的桌面与 app-server 未必同批升级，
    // 老客户端仍发旧键名，宿主必须照样认（兼容读）。
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":false}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        v["result"]["warning"],
        json!(null),
        "注入的卸载不会失败:{v}"
    );
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "恰好一次卸载:{recorded:?}");
    assert!(!recorded[0].0, "false triggers uninstall");
    assert_eq!(recorded[0].1, home, "必须针对注入的 home");
    assert!(!crate::settings_store::load_watchman_enabled(dir.path()));

    h.send(
        r#"{"jsonrpc":"2.0","id":3,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
    )
    .await;
    let v = read_response(&mut h, 3).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "第二次是安装:{recorded:?}");
    assert!(recorded[1].0, "true triggers install");

    // 开关与 theme 同处一个文件,互不覆盖。
    assert!(crate::settings_store::load_watchman_enabled(dir.path()));
    assert_eq!(
        crate::settings_store::load(dir.path()),
        crate::settings_store::Theme::Dark
    );

    h.send(r#"{"jsonrpc":"2.0","id":4,"method":"ui/settings/read","params":{}}"#)
        .await;
    let v = read_response(&mut h, 4).await;
    assert_eq!(
        v["result"][crate::settings_store::WATCHMAN_KEY],
        true,
        "{v}"
    );
    h.shutdown().await;
}

/// 安装失败不静默:落盘仍成功,回复带 `warning`,并由 `ok: true` 表明偏好已写入。
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_watchman_install_reports_a_warning_without_failing_the_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_watchman(
        cfg,
        Arc::new(|_exe: &Path, _home: &Path| Err("launchctl is unavailable".to_string())),
        Arc::new(|_home: &Path| Ok(())),
    )
    .await;
    initialize(&mut h).await;

    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"ui/settings/write","params":{"board_watchman_enabled":true}}"#,
    )
    .await;
    let v = read_response(&mut h, 2).await;
    assert_eq!(v["result"]["ok"], true, "{v}");
    assert_eq!(
        v["result"]["warning"], "launchctl is unavailable",
        "失败原因必须回传给前端:{v}"
    );
    // 偏好仍按用户的意图落盘:下次启动会再试。
    assert!(crate::settings_store::load_watchman_enabled(dir.path()));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_resolves_the_command_and_writes_what_the_plugin_describes() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    let manifest = serde_json::json!({
        "name": "demo-plugin",
        "command": "/bin/true",
        "args": ["run"],
        "switch_key": "demo_plugin_on",
    });
    fake_plugin_printing(&h, "demo-plugin", &manifest.to_string());

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(
        response.get("error").is_none(),
        "installing a well-formed plugin must succeed: {response}"
    );
    assert_eq!(response["result"]["installed"], true);
    // 这份清单没声明 `query_socket`，故没有通道要等：安装返回时就是「可查询」的。
    assert_eq!(
        response["result"]["queryable"], true,
        "a manifest without a query channel is queryable at once"
    );

    // `project_arg` 会 canonicalize（macOS 上 `/var` → `/private/var`），故期望
    // 路径也必须先规范化，否则比的是同一个目录的两种写法。
    let written = std::fs::canonicalize(project.path())
        .unwrap()
        .join(".yi-agent")
        .join("supervisors")
        .join("demo-plugin.json");
    assert_eq!(
        response["result"]["manifest"].as_str(),
        Some(written.to_string_lossy().as_ref()),
        "the response must name the file it wrote"
    );
    let text = std::fs::read_to_string(&written)
        .expect("the manifest must be written under the project's supervisors dir");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        manifest,
        "the manifest must be installed verbatim, not reinterpreted by the host"
    );

    // 3b（spec §4.4.1）：清单只让 supervisor 拉起进程，开关才决定它要不要干活。
    // 只写清单会得到一个「起来了但队列永不推进」的插件，故这一步必须钉死。
    let preferences =
        std::fs::read_to_string(project.path().join(".yi-agent").join("preferences.json"))
            .expect("installing must also create the project's preferences file");
    let preferences: serde_json::Value = serde_json::from_str(&preferences).unwrap();
    assert_eq!(
        preferences["demo_plugin_on"], true,
        "the manifest's switch_key must be on: a registered plugin that never advances is not installed"
    );

    // 最后一步：装完要把项目 daemon 拉起来（否则没有进程去读这份清单）。
    let daemons = h
        .daemon_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    assert_eq!(
        daemons,
        vec![std::fs::canonicalize(project.path()).unwrap()],
        "plugin/install must ask for the project's daemon once the manifest is durable"
    );

    // 登录自启：`board/create` 时代由建板路径 best-effort 安装，那条路径随域
    // 删除后必须有通用替代落点，否则全新安装不再有登录自启、旧 plist 也无人清理。
    let watchman = h
        .watchman_calls
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    assert_eq!(
        watchman,
        vec![(true, h._watchman_home.path().to_path_buf())],
        "a successful plugin install must ensure the resident watchman is installed"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_reports_a_missing_plugin_without_writing_anything() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    send_install(&mut h, 9, project.path(), "not-installed-anywhere").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["code"], -32023,
        "a name that resolves to nothing is `plugin_not_installed`: {response}"
    );
    assert_eq!(response["error"]["data"]["code"], "plugin_not_installed");

    // 失败必须**什么都没写**：留下半个安装（有清单、无开关）比干净失败更难收拾。
    assert!(
        !project.path().join(".yi-agent").exists(),
        "a failed install must not create the project's .yi-agent directory"
    );
    assert!(
        h.daemon_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty(),
        "a failed install must not start a daemon"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_rejects_a_manifest_that_describes_another_plugin() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    // 名字对不上：落盘会写出 `demo-plugin.json`，内容却说自己是别人。
    // 宿主不解释插件的语义，但这种自述它无法安全使用。
    fake_plugin_printing(
        &h,
        "demo-plugin",
        &serde_json::json!({
            "name": "somebody-else",
            "command": "/bin/true",
            "switch_key": "demo_plugin_on",
        })
        .to_string(),
    );

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert_eq!(response["error"]["code"], -32024, "{response}");
    assert_eq!(response["error"]["data"]["code"], "plugin_rejected");
    assert!(
        !project.path().join(".yi-agent").exists(),
        "a rejected manifest must leave nothing behind"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_rejects_a_manifest_without_a_switch_key() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    // 没有 switch_key 就无法打开开关；装进去会得到一个永不推进的插件，
    // 故必须在落盘前拒绝。
    fake_plugin_printing(
        &h,
        "demo-plugin",
        &serde_json::json!({ "name": "demo-plugin", "command": "/bin/true" }).to_string(),
    );

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["data"]["code"], "plugin_rejected",
        "{response}"
    );
    assert!(!project.path().join(".yi-agent").exists());

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_rejects_a_manifest_the_supervisor_would_silently_skip() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    // 缺 `command`：宿主若只看「是 JSON 对象 + 有 name + 有 switch_key」就会
    // 安装成功，而 supervisor 的 `SupervisorManifest::parse` 会因缺 command 失败、
    // 只记一条 warn 后**跳过**这份清单。用户会拿到 `installed: true`，插件却
    // 永远不会启动——症状与「没装」一模一样，且没有任何界面错误可查。
    fake_plugin_printing(
        &h,
        "demo-plugin",
        &serde_json::json!({ "name": "demo-plugin", "switch_key": "demo_plugin_on" }).to_string(),
    );

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["data"]["code"], "plugin_rejected",
        "a manifest the supervisor cannot load must be rejected, not installed: {response}"
    );
    assert!(
        !project.path().join(".yi-agent").exists(),
        "a rejected manifest must leave nothing behind"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_rolls_the_manifest_back_when_the_switch_cannot_be_written() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    fake_plugin_printing(
        &h,
        "demo-plugin",
        &serde_json::json!({
            "name": "demo-plugin",
            "command": "/bin/true",
            "switch_key": "demo_plugin_on",
        })
        .to_string(),
    );

    // 让 `preferences.json` 的落点无法写入：把一个**目录**放到该文件名上。
    // 清单已经能写、开关却写不了——这正是「半安装」的触发条件。
    let yi_agent = project.path().join(".yi-agent");
    std::fs::create_dir_all(yi_agent.join("preferences.json")).unwrap();

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(
        response.get("error").is_some(),
        "a switch that cannot be written must fail the install: {response}"
    );
    assert!(
        !yi_agent
            .join("supervisors")
            .join("demo-plugin.json")
            .exists(),
        "the manifest must be rolled back: a manifest with a closed switch is a plugin that is \
         launched and never advances"
    );
    assert!(
        h.daemon_calls
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_empty(),
        "no daemon may be started for an install that did not complete"
    );
    // 第 3 条：清单被撤回后，不能留下一个「已开启」的开关。开关只由
    // `enable_switch` 写，而它正是失败的那一步——所以 `preferences.json` 必须
    // 仍是那个挡路的目录，没有被任何一次开关写入替换成文件。
    assert!(
        yi_agent.join("preferences.json").is_dir(),
        "a failed install must not leave the plugin's switch written"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_reinstall_does_not_destroy_a_working_installation() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    let original = serde_json::json!({
        "name": "demo-plugin",
        "command": "/bin/true",
        "switch_key": "demo_plugin_on",
    })
    .to_string();
    fake_plugin_printing(&h, "demo-plugin", &original);

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(response.get("error").is_none(), "{response}");

    // 现在让开关写不进去，再装一次。失败必须把**原来那份清单**写回去——
    // 删掉它等于把一次失败的升级变成一次卸载。
    let preferences = project.path().join(".yi-agent").join("preferences.json");
    std::fs::remove_file(&preferences).unwrap();
    std::fs::create_dir_all(&preferences).unwrap();

    send_install(&mut h, 10, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(response.get("error").is_some(), "{response}");

    let restored = std::fs::read_to_string(
        project
            .path()
            .join(".yi-agent")
            .join("supervisors")
            .join("demo-plugin.json"),
    )
    .expect("a failed reinstall must leave the previous manifest in place");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&restored).unwrap(),
        serde_json::from_str::<serde_json::Value>(&original).unwrap()
    );

    h.shutdown().await;
}

/// 清单声明了查询通道，但插件一直没 bind：到期也要**安装成功**，只是如实回
/// `queryable: false`。把超时变成失败会把一次已经落盘的安装回滚成「没装」。
#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_succeeds_but_reports_not_queryable_when_the_socket_never_appears() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    let manifest = serde_json::json!({
        "name": "demo-plugin",
        "command": "/bin/true",
        "switch_key": "demo_plugin_on",
        "query_socket": "{state_dir}/demo.sock",
    });
    fake_plugin_printing(&h, "demo-plugin", &manifest.to_string());

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(
        response.get("error").is_none(),
        "a socket that never appears must not fail the install: {response}"
    );
    assert_eq!(response["result"]["installed"], true);
    assert_eq!(response["result"]["queryable"], false);

    // 清单与开关仍然在盘上：超时只是「这次还不能查」，不是回滚。
    let written = std::fs::canonicalize(project.path())
        .unwrap()
        .join(".yi-agent")
        .join("supervisors")
        .join("demo-plugin.json");
    assert!(written.is_file(), "the manifest must stay installed");

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_reports_a_plugin_that_fails_to_describe_itself() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    write_fake_plugin(
        &h,
        "demo-plugin",
        "#!/bin/sh\necho 'no manifest for you' >&2\nexit 3\n",
    );

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["data"]["code"], "plugin_rejected",
        "{response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no manifest for you"),
        "the error must carry the plugin's own first stderr line: {response}"
    );
    assert!(!project.path().join(".yi-agent").exists());

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_install_rejects_non_json_and_requires_a_project() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    write_fake_plugin(&h, "demo-plugin", "#!/bin/sh\necho 'not json at all'\n");

    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["data"]["code"], "plugin_rejected",
        "{response}"
    );
    assert!(!project.path().join(".yi-agent").exists());

    // 缺 `project` / 缺 `plugin` 是调用方参数错误，不是插件问题。
    for (id, params) in [
        (10_u64, serde_json::json!({ "plugin": "demo-plugin" })),
        (
            11,
            serde_json::json!({ "project": project.path().to_string_lossy() }),
        ),
        (
            12,
            serde_json::json!({ "project": project.path().to_string_lossy(), "plugin": "" }),
        ),
        (
            13,
            serde_json::json!({ "project": project.path().to_string_lossy(), "plugin": "../escape" }),
        ),
    ] {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"plugin/install","params":{params}}}"#
        ))
        .await;
        let response = h.read_value().await;
        assert_eq!(response["id"], id);
        assert_eq!(
            response["error"]["code"], -32602,
            "invalid params: {response}"
        );
    }

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_uninstall_removes_the_manifest_the_switch_and_the_state() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    install_and_seed_plugin(&h, project.path(), "demo-plugin", "demo_plugin_on");
    send_install(&mut h, 9, project.path(), "demo-plugin").await;
    let installed = h.read_value().await;
    assert!(installed.get("error").is_none(), "{installed}");
    let canonical = std::fs::canonicalize(project.path()).unwrap();

    send_uninstall(&mut h, 10, project.path(), "demo-plugin").await;
    let response = h.read_value().await;
    assert!(
        response.get("error").is_none(),
        "uninstalling an installed plugin must succeed: {response}"
    );
    assert_eq!(response["result"]["uninstalled"], true);
    assert_eq!(
        response["result"]["warnings"],
        serde_json::json!([]),
        "nothing should have failed here: {response}"
    );

    // 清单没了 = supervisor 下一拍会停掉这个子进程，且不会再拉起。
    assert!(
        !canonical
            .join(".yi-agent/supervisors/demo-plugin.json")
            .exists(),
        "the manifest must be gone, otherwise the supervisor keeps the plugin alive"
    );
    // 状态目录没了 = 插件即使还活着写一拍，也该由它自己重建（它已经没清单了）。
    assert!(
        !canonical.join(".yi-agent/demo-plugin").exists(),
        "the plugin state dir must be gone"
    );
    // 日志没了。
    assert!(!canonical.join(".yi-agent/logs/demo-plugin.log").exists());
    // 开关键删掉，别人的键原样保留。
    let preferences: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(canonical.join(".yi-agent/preferences.json")).unwrap(),
    )
    .unwrap();
    assert!(
        preferences.get("demo_plugin_on").is_none(),
        "the switch key must be removed, not left behind: {preferences}"
    );
    assert_eq!(
        preferences["subagent_runtime"], "always",
        "uninstalling one plugin must not touch other keys: {preferences}"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_uninstall_is_safe_to_repeat_and_on_a_never_installed_plugin() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    // 从未装过:不该报错，也不该凭空造出 `.yi-agent`。
    send_uninstall(&mut h, 9, project.path(), "never-installed").await;
    let response = h.read_value().await;
    assert!(
        response.get("error").is_none(),
        "uninstalling something absent is already the goal: {response}"
    );
    assert_eq!(response["result"]["uninstalled"], true);

    let project2 = tempfile::TempDir::new().unwrap();
    install_and_seed_plugin(&h, project2.path(), "demo-plugin", "demo_plugin_on");
    send_install(&mut h, 10, project2.path(), "demo-plugin").await;
    let _ = h.read_value().await;
    send_uninstall(&mut h, 11, project2.path(), "demo-plugin").await;
    let first = h.read_value().await;
    assert!(first.get("error").is_none(), "{first}");
    // 第二次：幂等。
    send_uninstall(&mut h, 12, project2.path(), "demo-plugin").await;
    let second = h.read_value().await;
    assert!(
        second.get("error").is_none(),
        "a second uninstall must not fail: {second}"
    );

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn plugin_uninstall_rejects_an_unusable_plugin_name() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let project = tempfile::TempDir::new().unwrap();
    send_uninstall(&mut h, 9, project.path(), "../escape").await;
    let response = h.read_value().await;
    assert_eq!(
        response["error"]["code"], -32602,
        "a traversing name must be refused: {response}"
    );

    h.shutdown().await;
}
