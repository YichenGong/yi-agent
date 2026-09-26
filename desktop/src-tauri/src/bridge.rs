use serde_json::Value;
use std::sync::Mutex;
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

/// Spawn the sidecar and forward its stdout frames as Tauri events.
pub fn spawn(app: &AppHandle) -> Result<(), String> {
    let (mut rx, child) = app
        .shell()
        .sidecar("yi-agent")
        .map_err(|e| format!("sidecar not found: {e}"))?
        .args(["app-server", "--listen", "stdio://"])
        .spawn()
        .map_err(|e| format!("sidecar spawn failed: {e}"))?;

    app.state::<Sidecar>()
        .child
        .lock()
        .map_err(|e| e.to_string())?
        .replace(child);

    let app = app.clone();
    tauri::async_runtime::spawn(async move {
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
    });
    Ok(())
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
}
