use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// Classification of one line received on the sidecar's stdout.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// Server→client notification (has `method`, no `id`).
    Notification,
    /// Server→client reverse request (has both `method` and `id`).
    ReverseRequest,
    /// Response to a client request (has `id`, no `method`).
    Response,
    /// Not a JSON-RPC frame — drop and log, never crash.
    Garbage,
}

pub fn classify(value: &Value) -> Frame {
    let has_method = value.get("method").is_some();
    let has_id = value.get("id").is_some();
    match (has_method, has_id) {
        (true, true) => Frame::ReverseRequest,
        (true, false) => Frame::Notification,
        (false, true) => Frame::Response,
        (false, false) => Frame::Garbage,
    }
}

/// Owns the spawned `yi-agent app-server` child process.
pub struct Sidecar {
    child: Mutex<Option<CommandChild>>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self {
            child: Mutex::new(None),
        }
    }
}

fn write_line(state: &Sidecar, value: &serde_json::Value) -> Result<(), String> {
    let mut buf = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    buf.push(b'\n');
    let mut guard = state.child.lock().map_err(|e| e.to_string())?;
    let child = guard
        .as_mut()
        .ok_or_else(|| "sidecar not running".to_string())?;
    child.write(&buf).map_err(|e| e.to_string())
}

/// Frontend → sidecar: write one JSON-RPC request line.
#[tauri::command]
pub fn rpc(
    state: State<'_, Sidecar>,
    id: u64,
    method: String,
    params: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
}

/// Frontend → sidecar: reply to a reverse request (permission approval).
#[tauri::command]
pub fn rpc_respond(
    state: State<'_, Sidecar>,
    id: String,
    result: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}),
    )
}

/// How long to wait before respawning a sidecar that exited, given how many
/// times in a row it has already exited.
///
/// The first failure restarts promptly — one editor `save` can take the daemon
/// down and the user should not notice. Repeated exits back off exponentially
/// (capped) so a sidecar that cannot even start does not spin the CPU; the app
/// keeps retrying forever because a transient cause (a still-closing socket, a
/// temporary disk issue) resolves on its own and a permanent one should surface
/// as a visible, retried error rather than a dead app.
fn next_restart_delay(consecutive_restarts: u32) -> Duration {
    const BASE_MS: u64 = 200;
    const CAP_MS: u64 = 5_000;
    let shift = consecutive_restarts.min(5); // 200ms * 2^5 = 6.4s, then cap
    let ms = (BASE_MS << shift).min(CAP_MS);
    Duration::from_millis(ms)
}

/// Spawn the sidecar, forward its stdout frames as Tauri events, and respawn it
/// whenever it exits.
///
/// The desktop app is only useful while its sidecar is alive: without a live
/// `yi-agent app-server` every RPC fails (the frontend surfaces `broken pipe`
/// when writing to the dead process's stdin). So the sidecar is supervised here
/// rather than spawned once — an unexpected exit is restarted with backoff and
/// announced on `app-server://status` so the UI can reconnect.
pub fn spawn(app: &AppHandle) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut consecutive_restarts: u32 = 0;
        loop {
            match spawn_once(&app) {
                Ok(mut rx) => {
                    // A successful launch resets the backoff: the previous exit
                    // was a one-off, not a start-up loop.
                    consecutive_restarts = 0;
                    while let Some(event) = rx.recv().await {
                        match event {
                            CommandEvent::Stdout(bytes) => forward_stdout(&app, &bytes),
                            CommandEvent::Stderr(bytes) => {
                                eprintln!("[sidecar] {}", String::from_utf8_lossy(&bytes));
                            }
                            CommandEvent::Terminated(payload) => {
                                let _ = app.emit(
                                    "app-server://status",
                                    serde_json::json!({"state":"exited","code":payload.code}),
                                );
                            }
                            _ => {}
                        }
                    }
                    // The event stream closed: the process is gone. Drop the
                    // stale handle so `rpc` reports "sidecar not running" rather
                    // than writing into a dead pipe while we restart.
                    clear_child(&app);
                    eprintln!("[sidecar] exited; restarting");
                }
                Err(e) => {
                    eprintln!("[sidecar] spawn failed: {e}");
                }
            }
            let delay = next_restart_delay(consecutive_restarts);
            consecutive_restarts = consecutive_restarts.saturating_add(1);
            tokio::time::sleep(delay).await;
        }
    });
    Ok(())
}

/// The sidecar argv. Pure: no I/O, so it is directly unit-testable.
///
/// The base is a plain stdio app-server (today's behavior). When a relay URL is
/// configured we append `--relay <url>`, which makes the CLI serve stdio *and*
/// the relay in one `serve()` — so the desktop GUI and the phone share a single
/// app-server/session.
fn sidecar_args(relay: Option<&str>) -> Vec<String> {
    let mut args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
    if let Some(url) = relay {
        args.push("--relay".into());
        args.push(url.into());
    }
    args
}

/// The relay URL the sidecar should advertise, from the `YI_AGENT_RELAY`
/// environment variable.
///
/// There is deliberately no desktop settings field for this yet: the env var
/// keeps this task small (and it composes with dev/launch scripts). A
/// settings-page field plus a pairing hand-off is a future enhancement. Unset
/// or blank → `None`, which reproduces the original stdio-only spawn exactly.
fn configured_relay() -> Option<String> {
    std::env::var("YI_AGENT_RELAY")
        .ok()
        .map(|url| url.trim().to_string())
        .filter(|url| !url.is_empty())
}

/// Launch one `yi-agent app-server` and return its event stream.
fn spawn_once(app: &AppHandle) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    let mut cmd = app
        .shell()
        .sidecar("yi-agent")
        .map_err(|e| format!("sidecar not found: {e}"))?
        .args(sidecar_args(configured_relay().as_deref()));

    // A bundled app launched from Finder inherits `/` as its cwd, which would
    // make the agent's workdir (and its runtime directory) the filesystem root.
    // Anchor the sidecar to the user's home directory so packaged and dev
    // launches behave predictably.
    match app.path().home_dir() {
        Ok(home) => cmd = cmd.current_dir(home),
        Err(e) => eprintln!("[sidecar] home_dir unavailable, using inherited cwd: {e}"),
    }

    let (rx, child) = cmd
        .spawn()
        .map_err(|e| format!("sidecar spawn failed: {e}"))?;

    app.state::<Sidecar>()
        .child
        .lock()
        .map_err(|e| e.to_string())?
        .replace(child);

    Ok(rx)
}

/// Forget the current child handle (called once its event stream ended).
fn clear_child(app: &AppHandle) {
    if let Ok(mut guard) = app.state::<Sidecar>().child.lock() {
        *guard = None;
    }
}

fn forward_stdout(app: &AppHandle, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let line = text.trim();
    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        eprintln!("[sidecar] dropping non-JSON line: {line}");
        return;
    };
    match classify(&value) {
        Frame::ReverseRequest => {
            let _ = app.emit("app-server://request", &value);
        }
        Frame::Notification | Frame::Response => {
            let _ = app.emit("app-server://message", &value);
        }
        Frame::Garbage => eprintln!("[sidecar] dropping non-protocol frame: {value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn notification_has_method_without_id() {
        assert_eq!(
            classify(&json!({"method":"turn/started","params":{}})),
            Frame::Notification
        );
    }

    #[test]
    fn reverse_request_has_method_and_id() {
        assert_eq!(
            classify(&json!({"id":"perm-1","method":"item/toolCall/requestApproval","params":{}})),
            Frame::ReverseRequest
        );
    }

    #[test]
    fn response_has_id_without_method() {
        assert_eq!(classify(&json!({"id":1,"result":{}})), Frame::Response);
    }

    #[test]
    fn garbage_is_dropped() {
        assert_eq!(classify(&json!({"hello":"world"})), Frame::Garbage);
    }

    #[test]
    fn sidecar_args_add_relay_only_when_configured() {
        assert_eq!(sidecar_args(None), ["app-server", "--listen", "stdio://"]);
        assert_eq!(
            sidecar_args(Some("wss://r/connect?session=x")),
            [
                "app-server",
                "--listen",
                "stdio://",
                "--relay",
                "wss://r/connect?session=x"
            ]
        );
    }

    // `configured_relay` reads a process-global env var. Cargo runs tests in
    // parallel threads of one process, so any test that mutates `YI_AGENT_RELAY`
    // must hold this lock and restore the previous value; otherwise a sibling
    // test could read a value it never set.
    static RELAY_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn restore_relay_env(previous: Option<String>) {
        match previous {
            Some(value) => std::env::set_var("YI_AGENT_RELAY", value),
            None => std::env::remove_var("YI_AGENT_RELAY"),
        }
    }

    #[test]
    fn configured_relay_is_none_when_unset_or_blank() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();

        std::env::remove_var("YI_AGENT_RELAY");
        assert_eq!(configured_relay(), None, "unset must mean no relay");

        std::env::set_var("YI_AGENT_RELAY", "   ");
        assert_eq!(configured_relay(), None, "blank must mean no relay");

        restore_relay_env(previous);
    }

    #[test]
    fn configured_relay_trims_a_set_url() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();

        std::env::set_var("YI_AGENT_RELAY", "  wss://r/connect?session=x  ");
        assert_eq!(
            configured_relay().as_deref(),
            Some("wss://r/connect?session=x")
        );

        restore_relay_env(previous);
    }

    #[test]
    fn first_restart_is_prompt() {
        assert_eq!(next_restart_delay(0), Duration::from_millis(200));
    }

    #[test]
    fn repeated_restarts_back_off_exponentially() {
        assert_eq!(next_restart_delay(1), Duration::from_millis(400));
        assert_eq!(next_restart_delay(2), Duration::from_millis(800));
        assert_eq!(next_restart_delay(3), Duration::from_millis(1600));
    }

    #[test]
    fn restart_backoff_is_capped() {
        // The doubling must stop well before it overflows a u64 shift and must
        // never exceed the cap, so a sidecar stuck in a start-up loop cannot
        // wedge the app with an absurd sleep.
        assert_eq!(next_restart_delay(5), Duration::from_millis(5_000));
        assert_eq!(next_restart_delay(50), Duration::from_millis(5_000));
        assert_eq!(next_restart_delay(u32::MAX), Duration::from_millis(5_000));
    }
}
