use super::*;

/// How long a plugin gets to answer before the daemon gives up.
///
/// `respond` runs on the daemon's request thread, so an unresponsive plugin
/// would otherwise wedge the request path for every client.
pub(crate) const PLUGIN_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Sockets this process may forward plugin queries to.
///
/// Populated by whoever supervises the plugins (the binary that runs the daemon
/// also runs the supervisor loop), because the store deliberately knows nothing
/// about manifests or plugin names. An entry exists only while its process is
/// actually running and its manifest declared a socket, so a request can never
/// name an arbitrary path on the machine.
///
/// The table is keyed by **project** (its runtime directory), not just by plugin
/// name. One process can host several daemons: the desktop app-server attaches a
/// runtime per project it touches, each with its own supervisor thread. Those
/// threads publish independently, and a project with no manifests publishes an
/// empty list. With a single flat table that empty publish erased every other
/// project's routes, so a plugin query flipped between "served" and "plugin is
/// not running" on the reconcile interval. Keying by scope makes each project's
/// publication affect only that project.
/// One project's forwarding routes: `(plugin name, socket path)` pairs.
pub(crate) type PluginRoutes = Vec<(String, PathBuf)>;

/// The whole table: one entry per project scope.
pub(crate) type PluginSocketTable = Vec<(PathBuf, PluginRoutes)>;

pub(crate) static PLUGIN_SOCKETS: Mutex<PluginSocketTable> = Mutex::new(Vec::new());

/// The identity of one project's daemon: its runtime directory.
///
/// Canonicalized when the directory exists so a supervisor and the daemon it
/// feeds agree on the key even if one side spells the path differently
/// (symlinks, a relative `YI_AGENT_RUNTIME_DIR`). A path that does not exist yet
/// is kept verbatim: the daemon creates it before serving, and the supervisor
/// derives the same value.
pub(crate) fn scope_key(scope: &Path) -> PathBuf {
    std::fs::canonicalize(scope).unwrap_or_else(|_| scope.to_path_buf())
}

/// Replace this project's forwarding table. Called on every supervisor
/// reconcile, so a stopped or undeclared plugin disappears from the table by
/// omission — while another project's routes are untouched.
pub fn register_plugin_sockets(scope: &Path, entries: PluginRoutes) {
    let scope = scope_key(scope);
    let mut table = PLUGIN_SOCKETS.lock().expect("plugin socket table poisoned");
    match table.iter_mut().find(|(key, _)| *key == scope) {
        Some((_, routes)) => *routes = entries,
        None => table.push((scope, entries)),
    }
}

/// Drop every route belonging to this project. Called when its daemon starts so
/// a previous process in the same address space cannot leave a stale route
/// behind. Other projects' routes are left alone.
pub fn clear_plugin_sockets(scope: &Path) {
    let scope = scope_key(scope);
    let mut table = PLUGIN_SOCKETS.lock().expect("plugin socket table poisoned");
    table.retain(|(key, _)| *key != scope);
}

/// The socket currently registered for `plugin` in `scope`, if any.
///
/// The read side of the forwarding table. It exists so a caller can assert that
/// a route is present (or gone) without issuing a request — the supervision
/// contract says a stopped supervisor leaves no routes behind, and that has to
/// be checkable from outside this module.
pub fn plugin_socket_for(scope: &Path, plugin: &str) -> Option<PathBuf> {
    plugin_socket(scope, plugin)
}

pub(crate) fn plugin_socket(scope: &Path, plugin: &str) -> Option<PathBuf> {
    let scope = scope_key(scope);
    PLUGIN_SOCKETS
        .lock()
        .expect("plugin socket table poisoned")
        .iter()
        .find(|(key, _)| *key == scope)
        .and_then(|(_, routes)| routes.iter().find(|(name, _)| name == plugin))
        .map(|(_, path)| path.clone())
}

/// Forward a `PluginQuery` to the plugin's declared socket.
///
/// The socket path comes from the supervisor's manifest, never from the caller:
/// accepting a caller-supplied path would turn this into a proxy for any local
/// socket. Unknown, undeclared, or not-running plugins are a structured error.
pub(crate) fn forward_plugin_query(
    scope: &Path,
    plugin: &str,
    method: &str,
    params: serde_json::Value,
) -> Result<IpcResponse, IpcError> {
    let Some(socket) = plugin_socket(scope, plugin) else {
        return Err(IpcError::PluginUnavailable {
            plugin: plugin.to_string(),
        });
    };
    let request = serde_json::json!({
        "type": "plugin.query",
        "method": method,
        "params": params,
    });
    let mut frame = serde_json::to_vec(&request)?;
    frame.push(b'\n');

    let mut stream = UnixStream::connect(&socket).map_err(|_| IpcError::PluginUnavailable {
        plugin: plugin.to_string(),
    })?;
    stream.set_read_timeout(Some(PLUGIN_QUERY_TIMEOUT))?;
    stream.set_write_timeout(Some(PLUGIN_QUERY_TIMEOUT))?;
    stream.write_all(&frame)?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = match reader.read_line(&mut line) {
        Ok(read) => read,
        Err(error)
            if error.kind() == std::io::ErrorKind::WouldBlock
                || error.kind() == std::io::ErrorKind::TimedOut =>
        {
            return Err(IpcError::PluginTimeout {
                plugin: plugin.to_string(),
                seconds: PLUGIN_QUERY_TIMEOUT.as_secs(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    if read == 0 {
        // The plugin closed without answering.
        return Err(IpcError::PluginRefused {
            plugin: plugin.to_string(),
        });
    }
    let reply: serde_json::Value = serde_json::from_str(&line)?;
    match reply.get("type").and_then(|value| value.as_str()) {
        Some("plugin.result") => Ok(IpcResponse::PluginResult {
            value: reply
                .get("value")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        // The plugin refused the query (bad method, invalid paths, ...). Surface
        // it through the standard error path rather than inventing a second
        // error shape for plugin-owned failures.
        _ => Err(IpcError::PluginRefused {
            plugin: plugin.to_string(),
        }),
    }
}

#[cfg(test)]
mod plugin_query_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    /// The forwarding table is one process-wide static, so the tests that
    /// publish to it must not run in parallel: a concurrently registered
    /// "demo" socket would answer `forward_plugin_query` in place of this
    /// test's fake plugin. Hold this for the whole test.
    static TABLE: Mutex<()> = Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        TABLE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A one-shot fake plugin: answers the first line with `reply`.
    fn serve_once(socket: PathBuf, reply: &'static str) -> thread::JoinHandle<()> {
        let listener = UnixListener::bind(&socket).unwrap();
        thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut stream = stream;
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.write_all(b"\n");
            }
        })
    }

    #[test]
    fn an_undeclared_plugin_is_a_structured_error_not_a_panic() {
        let _guard = exclusive();
        let scope = tempfile::tempdir().unwrap();
        clear_plugin_sockets(scope.path());
        let error = forward_plugin_query(scope.path(), "nobody", "list", serde_json::json!({}))
            .expect_err("an unknown plugin must not be forwarded to");
        assert!(matches!(error, IpcError::PluginUnavailable { .. }));
        assert_eq!(ipc_error_code(&error), IpcErrorCode::NotFound);
    }

    #[test]
    fn a_result_is_passed_through_untouched() {
        let _guard = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        let handle = serve_once(
            socket.clone(),
            r#"{"type":"plugin.result","value":{"cards":[{"id":"a"}]}}"#,
        );
        register_plugin_sockets(dir.path(), vec![("demo".to_string(), socket)]);

        // The daemon adds no interpretation: whatever the plugin put in `value`
        // comes back verbatim, including nested objects it knows nothing about.
        let response =
            forward_plugin_query(dir.path(), "demo", "list", serde_json::json!({"x": 1})).unwrap();
        match response {
            IpcResponse::PluginResult { value } => {
                assert_eq!(value["cards"][0]["id"], "a");
            }
            other => panic!("expected a plugin result, got {other:?}"),
        }
        handle.join().unwrap();
        clear_plugin_sockets(dir.path());
    }

    #[test]
    fn the_request_forwards_method_and_params_verbatim() {
        let _guard = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        let seen = Arc::new(Mutex::new(String::new()));
        let listener = UnixListener::bind(&socket).unwrap();
        let captured = Arc::clone(&seen);
        let handle = thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                *captured.lock().unwrap() = line;
                let mut stream = stream;
                let _ = stream.write_all(b"{\"type\":\"plugin.result\",\"value\":1}\n");
            }
        });
        register_plugin_sockets(dir.path(), vec![("demo".to_string(), socket)]);

        forward_plugin_query(
            dir.path(),
            "demo",
            "switch.write",
            serde_json::json!({"on": true}),
        )
        .unwrap();
        handle.join().unwrap();

        let sent: serde_json::Value = serde_json::from_str(seen.lock().unwrap().trim()).unwrap();
        assert_eq!(sent["method"], "switch.write");
        assert_eq!(sent["params"]["on"], true);
        clear_plugin_sockets(dir.path());
    }

    #[test]
    fn a_plugin_that_closes_without_answering_is_refused() {
        let _guard = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let handle = thread::spawn(move || {
            // Accept, read, then drop without replying.
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
            }
        });
        register_plugin_sockets(dir.path(), vec![("demo".to_string(), socket)]);

        let error = forward_plugin_query(dir.path(), "demo", "list", serde_json::json!({}))
            .expect_err("a silent plugin must not look like success");
        assert!(matches!(error, IpcError::PluginRefused { .. }));
        handle.join().unwrap();
        clear_plugin_sockets(dir.path());
    }

    #[test]
    fn a_plugin_that_refuses_maps_to_a_validation_error() {
        let _guard = exclusive();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        let handle = serve_once(socket.clone(), r#"{"type":"plugin.error"}"#);
        register_plugin_sockets(dir.path(), vec![("demo".to_string(), socket)]);

        let error = forward_plugin_query(dir.path(), "demo", "enqueue", serde_json::json!({}))
            .expect_err("a plugin-side refusal is not a result");
        assert_eq!(ipc_error_code(&error), IpcErrorCode::Validation);
        handle.join().unwrap();
        clear_plugin_sockets(dir.path());
    }

    #[test]
    fn a_stopped_plugin_disappears_from_the_table() {
        let _guard = exclusive();
        // Republishing with an empty list is how a stop is expressed.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("plugin.sock");
        register_plugin_sockets(dir.path(), vec![("demo".to_string(), socket)]);
        assert!(plugin_socket(dir.path(), "demo").is_some());
        register_plugin_sockets(dir.path(), vec![]);
        assert!(plugin_socket(dir.path(), "demo").is_none());
    }

    /// 回归测试：桌面 app-server 在**一个进程**里跑多个项目的 daemon，每个项目
    /// 各有自己的监督线程。只有装了插件的那个项目才有清单，其余项目的
    /// reconcile 发布的是空表。空表不得抹掉别的项目的路由，否则看板查询会以
    /// 约 0.4s 的周期在"能读到"和"插件未运行"之间抖动。
    #[test]
    fn an_empty_publish_from_one_project_does_not_erase_another_projects_route() {
        let _guard = exclusive();
        let kanban = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let socket = PathBuf::from("/tmp/kanban.sock");

        register_plugin_sockets(kanban.path(), vec![("demo".to_string(), socket.clone())]);
        // The other project has no manifests, so its supervisor publishes nothing.
        register_plugin_sockets(other.path(), Vec::new());

        assert_eq!(
            plugin_socket(kanban.path(), "demo"),
            Some(socket.clone()),
            "another project's empty reconcile must not erase this route"
        );
        assert_eq!(
            plugin_socket(other.path(), "demo"),
            None,
            "routes must not leak across projects"
        );
        clear_plugin_sockets(kanban.path());
    }

    /// 一个项目的 daemon 退出只清自己的路由；别的项目的 daemon 仍在服务。
    #[test]
    fn clearing_one_project_leaves_another_projects_routes_alone() {
        let _guard = exclusive();
        let kanban = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        register_plugin_sockets(
            kanban.path(),
            vec![("demo".to_string(), PathBuf::from("/tmp/a.sock"))],
        );
        register_plugin_sockets(
            other.path(),
            vec![("demo".to_string(), PathBuf::from("/tmp/b.sock"))],
        );

        clear_plugin_sockets(kanban.path());

        assert_eq!(plugin_socket(kanban.path(), "demo"), None);
        assert_eq!(
            plugin_socket(other.path(), "demo"),
            Some(PathBuf::from("/tmp/b.sock"))
        );
        clear_plugin_sockets(other.path());
    }

    #[test]
    fn plugin_query_positions_a_verified_socket_are_not_reused_across_plugins() {
        let _guard = exclusive();
        let dir = tempfile::tempdir().unwrap();
        register_plugin_sockets(
            dir.path(),
            vec![
                ("a".to_string(), PathBuf::from("/tmp/a.sock")),
                ("b".to_string(), PathBuf::from("/tmp/b.sock")),
            ],
        );
        assert_eq!(
            plugin_socket(dir.path(), "a"),
            Some(PathBuf::from("/tmp/a.sock"))
        );
        assert_eq!(
            plugin_socket(dir.path(), "b"),
            Some(PathBuf::from("/tmp/b.sock"))
        );
        assert_eq!(plugin_socket(dir.path(), "c"), None);
        clear_plugin_sockets(dir.path());
    }
}
