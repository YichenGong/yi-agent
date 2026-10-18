//! End-to-end acceptance for per-project Superpowers kanban boards.
//!
//! This drives the **real** binaries: it spawns `yi-agent app-server` over
//! stdio, has it create a board (which starts a detached `daemon serve`), and
//! queries that board through the daemon's plugin channel. What a unit test
//! cannot reach is exactly what this pins: that the created daemon actually
//! outlives the app-server that started it, and that the real plugin answers
//! over the real sockets.
//!
//! The plugin binary (`superpowers-kanban`) lives outside this repository's
//! build, so the test is opt-in: set `YI_AGENT_BOARD_E2E=1` to run it. Without
//! that, or without the plugin installed, it reports why and returns green —
//! a missing *external* tool must not fail the suite. Run it explicitly with:
//!
//! ```text
//! YI_AGENT_BOARD_E2E=1 cargo test -p yi-agent --test board_e2e -- --nocapture
//! ```
//!
//! A single `#[test]` covers the whole sequence on purpose: the four acceptance
//! checks share one daemon's lifetime, and splitting them would spawn several
//! real daemons that race each other.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

mod common;

use common::AppServer;

/// The plugin executable a board's daemon supervises. Boards point at the
/// Homebrew install; the test skips when it is absent (see the module docs).
const PLUGIN_BIN: &str = "/opt/homebrew/bin/superpowers-kanban";

/// How long `board/create` may take to report a ready daemon. The real
/// `launch_if_absent` waits up to its own `READY_TIMEOUT`, so this is the outer
/// bound for the whole RPC, not just the spawn.
const CREATE_TIMEOUT: Duration = Duration::from_secs(30);

fn yi_agent_bin() -> PathBuf {
    std::env::var_os("CARGO_BIN_EXE_yi-agent")
        .map(Into::into)
        .expect("Cargo must provide the yi-agent binary")
}

/// Opt-in gate. Returns `Some(reason)` when the test should report and skip.
fn skip_reason() -> Option<String> {
    if std::env::var_os("YI_AGENT_BOARD_E2E").is_none() {
        return Some("set YI_AGENT_BOARD_E2E=1 to run the board end-to-end test".into());
    }
    if !Path::new(PLUGIN_BIN).is_file() {
        return Some(format!("{PLUGIN_BIN} is not installed"));
    }
    None
}

/// Stops the project's board daemon on drop, so a failing assertion cannot leak
/// a detached process into the developer's machine.
struct DaemonGuard(PathBuf);

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = yi_agent_boards::board_daemon::stop(&self.0);
    }
}

fn read_json(path: &Path) -> Value {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parsing {path:?}: {e}"))
}

#[test]
fn a_created_board_is_queryable_and_outlives_the_app_server() {
    if let Some(reason) = skip_reason() {
        eprintln!("skipping board end-to-end: {reason}");
        return;
    }

    let root = tempfile::TempDir::new().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    let project_str = project.to_string_lossy().to_string();
    // Registered paths are canonicalized by the server; compare like for like.
    let global = home.join(".yi-agent").join("superpowers-kanban");
    let bin = yi_agent_bin();

    let mut server = AppServer::start(&bin, &home);
    server.initialize();

    // (0) The routing contract: a query without a project is rejected, not
    // silently answered from the server's own cwd.
    let missing = server.rpc(
        "plugin/query",
        json!({"plugin": "superpowers-kanban", "method": "switch.read"}),
    );
    assert_eq!(
        missing["error"]["code"], -32602,
        "plugin/query without a project must be invalid_params: {missing}"
    );

    // (1) Create: registry + manifest + switch, and a daemon that answers.
    let created = server.rpc_within(
        "board/create",
        json!({"project": project_str}),
        CREATE_TIMEOUT,
    );
    assert!(
        created.get("error").is_none(),
        "board/create failed: {created}"
    );
    assert_eq!(created["result"]["registered"], true, "{created}");
    assert_eq!(
        created["result"]["daemon_running"], true,
        "the created board must have a running daemon: {created}"
    );
    let _guard = DaemonGuard(project.clone());

    let registry = read_json(&global.join("boards.json"));
    let boards = registry["boards"].as_array().unwrap();
    assert_eq!(
        boards.len(),
        1,
        "registry must hold exactly this board: {registry}"
    );
    assert_eq!(boards[0]["project"], project_str, "{registry}");

    assert!(
        project
            .join(".yi-agent/supervisors/superpowers-kanban.json")
            .is_file(),
        "the supervisor manifest must be installed"
    );
    let prefs = read_json(&project.join(".yi-agent/preferences.json"));
    assert_eq!(prefs["superpowers_kanban"], true, "{prefs}");

    // (2) The plugin answers through the project's daemon.
    let switch = server.rpc(
        "plugin/query",
        json!({"project": project_str, "plugin": "superpowers-kanban", "method": "switch.read", "params": {}}),
    );
    assert!(
        switch.get("error").is_none(),
        "switch.read failed: {switch}"
    );
    assert_eq!(switch["result"]["on"], true, "{switch}");
    assert_eq!(
        switch["result"]["source"], "project",
        "the project layer is what enabled it: {switch}"
    );

    let list = server.rpc(
        "plugin/query",
        json!({"project": project_str, "plugin": "superpowers-kanban", "method": "list", "params": {}}),
    );
    assert!(list.get("error").is_none(), "list failed: {list}");
    assert!(
        list["result"]["cards"].is_array(),
        "list must return a cards array: {list}"
    );

    let listed = server.rpc("board/list", json!({}));
    assert_eq!(
        listed["result"]["boards"][0]["project"], project_str,
        "{listed}"
    );

    // (3) The board outlives the app-server that created it.
    let status = server.shutdown();
    assert!(status.success(), "app-server exited abnormally: {status}");
    assert!(
        yi_agent_boards::board_daemon::is_running(&project),
        "the daemon must keep answering after the app-server exits"
    );

    // (4) Remove, driven by a fresh app-server: daemon stops, queue goes, the
    // manifest stays.
    let mut server = AppServer::start(&bin, &home);
    server.initialize();
    let removed = server.rpc("board/remove", json!({"project": project_str}));
    assert!(
        removed.get("error").is_none(),
        "board/remove failed: {removed}"
    );
    let _ = server.shutdown();

    assert!(
        !yi_agent_boards::board_daemon::is_running(&project),
        "the daemon must stop when the board is removed"
    );
    assert!(
        !global.join("boards.json").exists()
            || read_json(&global.join("boards.json"))["boards"]
                .as_array()
                .unwrap()
                .is_empty(),
        "the registry must forget the removed board"
    );
    assert!(
        !project.join(".yi-agent/superpowers-kanban").exists(),
        "the queue directory must be deleted"
    );
    assert!(
        project
            .join(".yi-agent/supervisors/superpowers-kanban.json")
            .is_file(),
        "the manifest must survive removal"
    );
}
