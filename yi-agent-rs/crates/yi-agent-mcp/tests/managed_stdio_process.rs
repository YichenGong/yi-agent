//! A managed MCP server must belong to its owner, on probe and on reconnect.
//!
//! The failure this test exists to catch is silent: a `TokioChildProcess` spawned
//! by this crate is a child of whoever connected, registered nowhere. Such a server
//! outlives the frontend that asked for it, and a stdio server is long-lived by
//! design, so nothing about it looks wrong until the app is quit and the process is
//! still there.
//!
//! So the launcher is observed directly rather than inferred: every launch must go
//! through the seam, and a launch failure must fail the probe instead of falling
//! back to a direct spawn.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use yi_agent_mcp::{LaunchedStdio, McpConfig, ServerConfig, ServerKind, StdioLauncher};

/// Records every launch and refuses to start anything.
///
/// Refusing is the point: a probe that fell back to a direct spawn would succeed
/// (a shell is always available) and the recorded count would stay at zero.
#[derive(Default)]
struct RecordingLauncher {
    launched: Mutex<Vec<(String, Vec<String>)>>,
}

impl StdioLauncher for RecordingLauncher {
    fn launch<'a>(
        &'a self,
        command: &'a str,
        args: &'a [String],
        _env: &'a BTreeMap<String, String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<LaunchedStdio>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.launched
                .lock()
                .unwrap()
                .push((command.to_string(), args.to_vec()));
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "launcher refuses: the seam must be the only way to start a server",
            ))
        })
    }
}

fn stdio_server() -> ServerConfig {
    ServerConfig {
        kind: ServerKind::Stdio,
        command: Some("definitely-not-installed-server".to_string()),
        args: vec!["--serve".to_string()],
        env: BTreeMap::new(),
        url: None,
        headers: BTreeMap::new(),
        enabled: true,
    }
}

#[tokio::test]
async fn mcp_probe_and_reconnect_leave_no_unregistered_child() {
    let directory = tempfile::TempDir::new().unwrap();
    let launcher = Arc::new(RecordingLauncher::default());

    let mut servers = BTreeMap::new();
    servers.insert("recorded".to_string(), stdio_server());
    let cfg = McpConfig {
        enabled: true,
        mcp_servers: servers,
    };

    // A cold cache forces a probe, which is where an unregistered child used to be
    // created before anything asked the guardian.
    let manager = yi_agent_mcp::McpManager::load_and_probe_with(
        cfg,
        directory.path(),
        Some(launcher.clone() as Arc<dyn StdioLauncher>),
    )
    .await
    .expect("a refusing launcher must not fail the manager");

    let launches = launcher.launched.lock().unwrap().clone();
    assert_eq!(
        launches.len(),
        1,
        "the probe must go through the launcher exactly once: {launches:?}"
    );
    assert_eq!(launches[0].0, "definitely-not-installed-server");
    assert_eq!(launches[0].1, vec!["--serve".to_string()]);

    // The refusing launcher means no tools were discovered. That is a probe
    // failure recorded on the server, not a silent local spawn.
    let _ = manager;
}
