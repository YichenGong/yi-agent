use super::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;

/// A fake daemon bound to `dir`'s runtime socket that records the request it
/// received and replies with one frame carrying `result`, so the test can
/// prove the host forwards the plugin name, method and params verbatim
/// without interpreting any of them.
pub(super) fn fake_daemon(
    dir: &Path,
    result: serde_json::Value,
) -> (
    PathBuf,
    Arc<StdMutex<serde_json::Value>>,
    std::thread::JoinHandle<()>,
) {
    // The exact path `plugin_query` dials, via the same helper the
    // implementation uses: a fake bound anywhere else would let a
    // wrong-path implementation pass.
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(dir);
    std::fs::create_dir_all(&runtime_dir).unwrap();
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
    let seen = Arc::new(StdMutex::new(serde_json::Value::Null));
    let captured = Arc::clone(&seen);
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let request: serde_json::Value =
                serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
            *captured.lock().unwrap() = request.clone();
            // Echo the request id: the client rejects a response for another
            // request, so a fake daemon that hardcodes one would look broken.
            let reply = json!({
                "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                "request_id": request["request_id"],
                "result": result,
            });
            let mut stream = stream;
            let _ = stream.write_all(reply.to_string().as_bytes());
            let _ = stream.write_all(b"\n");
        }
    });
    (socket, seen, handle)
}

/// 一个存在的项目目录。
///
/// 曾经这里还会按需写一份 `boards.json`（`project_and_registry`），用来
/// 表达「这个项目登记没登记」——门控在场时那决定了 `plugin_query` 答不答。
/// 门控摘掉后登记表不再参与通用转发，故这个 helper 只剩「建一个目录」，
/// 名字也随之改回它真正做的事。
fn project_dir(dir: &Path) -> PathBuf {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    project
}

/// 通用转发不再按「这个项目有没有看板」拒答。
///
/// 反向断言的是**具体错误码**，不是「有没有报错」：摘掉门控后，一个
/// 未登记的项目会走到「拨 socket」，于是失败码是 `daemon_unavailable`
/// （socket 不在），**不再**是 `board_not_created`。
#[test]
fn a_query_is_not_gated_on_the_registry() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    // 不放 boards.json，也不要 nonce：这个项目从未登记过。
    let error = plugin_query(&project, "list", "superpowers-kanban", json!({}), None).unwrap_err();
    assert_eq!(error.code, "daemon_unavailable", "{error}");
    assert_ne!(error.code, "board_not_created", "{error}");
}

/// 而且它**真的**拨到了那个项目的 socket：门控给的是「登记表说了算」，
/// 摘掉之后「谁来答」只由 socket 是否应答决定。
#[test]
fn an_unregistered_project_still_reaches_its_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    // 没有 boards.json：登记表只会在**旧**实现里参与判断。
    let (_socket, seen, handle) = fake_daemon(
        &project,
        json!({ "type": "PluginResult", "value": { "cards": [] } }),
    );

    let value = plugin_query(&project, "list", "superpowers-kanban", json!({}), None).unwrap();

    assert_eq!(value["cards"], json!([]));
    let request = seen.lock().unwrap().clone();
    assert_eq!(request["command"]["method"], "list", "{request}");
    handle.join().unwrap();
}

#[test]
fn a_query_reaches_the_daemon_with_the_plugin_and_method_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    let (_socket, seen, handle) = fake_daemon(
        &project,
        json!({ "type": "PluginResult", "value": { "cards": [] } }),
    );

    plugin_query(
        &project,
        "list",
        "superpowers-kanban",
        json!({ "verbose": true }),
        None,
    )
    .unwrap();
    handle.join().unwrap();

    let request = seen.lock().unwrap().clone();
    assert_eq!(request["command"]["type"], "PluginQuery");
    assert_eq!(request["command"]["plugin"], "superpowers-kanban");
    assert_eq!(request["command"]["method"], "list");
    assert_eq!(request["command"]["params"]["verbose"], true);
}

/// 写一份只含指定插件的清单，返回它的 `switch_key`。
fn write_manifest(project: &Path, name: &str, switch_key: &str) {
    let dir = project.join(".yi-agent/supervisors");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.json")),
        json!({
            "name": name,
            "command": "/bin/true",
            "switch_key": switch_key,
            "stop_when_disabled": true,
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn a_switched_off_plugin_says_disabled_instead_of_broken() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    write_manifest(&project, "superpowers-kanban", "superpowers_kanban");

    let flags = dir.path().join("flags.json");
    std::fs::write(&flags, r#"{"version":1,"flags":{}}"#).unwrap();
    let gate = crate::plugin_gate::Gate {
        build_kind: "dev",
        flags_path: flags,
    };

    // 故意不起 daemon：若实现走到连 socket，返回的会是 `daemon_unavailable`。
    let error = plugin_query(
        &project,
        "list",
        "superpowers-kanban",
        json!({}),
        Some(&gate),
    )
    .unwrap_err();

    assert_eq!(error.code, "plugin_disabled", "{error}");
    assert!(
        error.message.contains("superpowers_kanban"),
        "the message must name the switch to flip: {error}"
    );
}

#[test]
fn a_switched_on_plugin_still_reaches_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    write_manifest(&project, "superpowers-kanban", "superpowers_kanban");

    let flags = dir.path().join("flags.json");
    std::fs::write(
        &flags,
        r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
    )
    .unwrap();
    let gate = crate::plugin_gate::Gate {
        build_kind: "dev",
        flags_path: flags,
    };

    let (_socket, _seen, handle) = fake_daemon(
        &project,
        json!({ "type": "PluginResult", "value": { "cards": [] } }),
    );
    let value = plugin_query(
        &project,
        "list",
        "superpowers-kanban",
        json!({}),
        Some(&gate),
    )
    .unwrap();
    assert_eq!(value["cards"], json!([]));
    handle.join().unwrap();
}

#[test]
fn a_stable_build_never_lets_the_flag_through() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    write_manifest(&project, "superpowers-kanban", "superpowers_kanban");

    let flags = dir.path().join("flags.json");
    std::fs::write(
        &flags,
        r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
    )
    .unwrap();
    let gate = crate::plugin_gate::Gate {
        build_kind: "release",
        flags_path: flags,
    };

    let error = plugin_query(
        &project,
        "list",
        "superpowers-kanban",
        json!({}),
        Some(&gate),
    )
    .unwrap_err();
    assert_eq!(error.code, "plugin_disabled", "{error}");
}

#[test]
fn a_missing_plugin_name_is_refused_before_touching_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    let error = plugin_query(&project, "list", "", json!({}), None).unwrap_err();
    assert!(error.message.contains("plugin"), "{error}");
    assert_eq!(
        error.code, "invalid_params",
        "空 plugin 名与「没有看板」是两回事:{error}"
    );
}

#[test]
fn an_unreachable_daemon_reports_that_the_plugin_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    let error = plugin_query(&project, "list", "superpowers-kanban", json!({}), None).unwrap_err();
    assert!(error.message.contains("daemon is unavailable"), "{error}");
    assert_eq!(error.code, "daemon_unavailable", "{error}");
}

#[test]
fn a_daemon_that_does_not_supervise_the_plugin_reports_plugin_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let project = project_dir(dir.path());
    let (_socket, _seen, handle) = fake_daemon(
        &project,
        json!({
            "type": "Error",
            "code": "not_found",
            "message": "plugin superpowers-kanban is not available",
        }),
    );

    let error = plugin_query(&project, "list", "superpowers-kanban", json!({}), None).unwrap_err();
    handle.join().unwrap();

    assert_eq!(error.code, "plugin_unavailable", "{error}");
    assert!(error.message.contains("is not available"), "{error}");
    assert!(
        error.message.contains("plugin"),
        "既有措辞形状必须保留:{error}"
    );
}

/// A daemon that reached the plugin and relayed the plugin's OWN rejection
/// (an invalid settings payload, say) is the plugin talking, not the plugin
/// being gone. Reporting `plugin_unavailable` here makes the desktop say
/// "插件未运行" and discards the plugin's reason; the code must be
/// `plugin_rejected`. This is the app-server's own-workdir channel
/// (`plugin_query_self`), which is what `plugin/settings/*` dispatch uses.
#[test]
fn a_plugin_side_rejection_is_not_reported_as_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("bad.json"),
        r#"{"name":"bad","command":"x","switch_key":"bad_on","query_socket":"{state_dir}/bad.sock"}"#,
    )
    .unwrap();
    let (_socket, _seen, handle) = fake_daemon(
        dir.path(),
        json!({
            "type": "Error",
            "code": "validation",
            "message": "interval_secs must be in [1, 3600], got 0",
        }),
    );

    let error = plugin_query_self(dir.path(), "bad", "settings.write", json!({})).unwrap_err();
    handle.join().unwrap();

    assert_eq!(error.code, "plugin_rejected", "{error}");
    assert_ne!(
        error.code, "plugin_unavailable",
        "别把拒绝说成没运行:{error}"
    );
    assert!(
        error.message.contains("interval_secs"),
        "插件的具体理由要带出来:{error}"
    );
}

/// The dispatch mapping the UI actually reads. The numeric code is the
/// coarse fallback; `data.code` is the stable string, so both have to reach
/// the wire together.
///
/// 摘掉登记表门控后，未登记的项目会走到「拨 socket」而不是被拒，故这条
/// 断言的是 `daemon_unavailable` + `-32021`。宿主不再有「这个项目没登记」
/// 这个回答，故那个码位已退休（见 `protocol.rs` 的 `plugin_query_error`）。
#[tokio::test(flavor = "multi_thread")]
async fn the_rpc_reports_a_missing_daemon_in_data_for_an_unregistered_project() {
    let mut h = tests::Harness::new();
    tests::initialize(&mut h).await;
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = project_dir.path().join("unregistered");
    std::fs::create_dir_all(&project).unwrap();

    h.send(
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "plugin/query",
            "params": {
                "project": project.to_string_lossy(),
                "plugin": "superpowers-kanban",
                "method": "list",
                "params": {},
            },
        })
        .to_string(),
    )
    .await;

    let value = loop {
        let value = h.read_value().await;
        if value.get("id") == Some(&json!(2)) {
            break value;
        }
    };
    assert_eq!(
        value["error"]["data"]["code"], "daemon_unavailable",
        "{value}"
    );
    assert_eq!(value["error"]["code"], -32021, "{value}");
    h.shutdown().await;
}
