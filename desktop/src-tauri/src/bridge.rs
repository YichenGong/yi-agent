use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// `preferences.json` key the sidecar reads its relay URL from. Mirrors
/// `yi_agent_app_server::settings_store::RELAY_URL_KEY`.
const RELAY_URL_KEY: &str = "relay_url";

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

/// The relay URL the sidecar should advertise.
///
/// Two sources, in priority order: the `YI_AGENT_RELAY` environment variable
/// (explicit override for dev/launch scripts) and the `relay_url` key in
/// `<home>/.yi-agent/preferences.json` (what the desktop Settings page writes).
///
/// The pure part is [`resolve_relay`], so the priority rules are unit-tested
/// without touching the process env or the filesystem. Unset or blank in both
/// places → `None`, which reproduces the original stdio-only spawn exactly.
fn configured_relay() -> Option<String> {
    let home = home_dir();
    configured_relay_in(home.as_deref())
}

/// [`configured_relay`] with an injectable home directory, so tests never read
/// (or, in `set_relay_url`, never write) the developer's real `~`.
fn configured_relay_in(home: Option<&Path>) -> Option<String> {
    let stored = home.and_then(load_relay_url);
    resolve_relay(
        std::env::var("YI_AGENT_RELAY").ok().as_deref(),
        stored.as_deref(),
    )
}

/// Pure resolution of the two relay sources. Each is trimmed; a non-blank env
/// value wins (explicit override), otherwise a non-blank stored value is used.
fn resolve_relay(env: Option<&str>, stored: Option<&str>) -> Option<String> {
    let clean = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|url| !url.is_empty())
            .map(str::to_string)
    };
    clean(env).or_else(|| clean(stored))
}

/// The home directory, or `None` when the platform cannot report one.
fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `<home>/.yi-agent/preferences.json` — the file the sidecar reads/writes when
/// its workdir is the user home (`spawn_once` anchors the sidecar there).
///
/// Mirror of `yi_agent_app_server::settings_store::preferences_path`; the
/// desktop shell intentionally does not depend on the app-server crate.
fn preferences_path(home: &Path) -> PathBuf {
    home.join(".yi-agent").join("preferences.json")
}

/// Read `relay_url` from the home preferences file. Missing/broken file, a
/// missing key, a non-string or a blank value all read as `None` — a bad
/// preference must never keep the sidecar from starting.
fn load_relay_url(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(preferences_path(home)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get(RELAY_URL_KEY)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_string)
}

/// Write (or, for `None`/blank, remove) `relay_url` in the home preferences
/// file: read-modify-write so `theme` / `board_watchman_enabled` /
/// `subagent_runtime` survive, temp-file + rename so the replace is atomic.
fn save_relay_url(home: &Path, url: Option<&str>) -> std::io::Result<()> {
    let dir = home.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("preferences.json");
    let mut object = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    match url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => {
            object.insert(
                RELAY_URL_KEY.to_string(),
                serde_json::Value::String(url.to_string()),
            );
        }
        None => {
            object.remove(RELAY_URL_KEY);
        }
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    // 唯一临时名 + rename:与 app-server 的 settings_store 同一约定,避免并发
    // 写者(runtime_prefs/kanban)互相截断。
    let tmp = dir.join(format!(
        "preferences.json.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::write(&tmp, &text)?;
    std::fs::rename(&tmp, &path)
}

/// Per-write suffix so concurrent writers never share a temp name.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Settings page → host: persist the sidecar's relay URL and restart the
/// sidecar so the new setting takes effect without restarting the app.
///
/// The write happens **before** the kill: the supervisor's respawn reads the
/// resolved relay, so killing first would restart the sidecar on the old value
/// and the setting would not take effect until the next exit.
#[tauri::command]
pub fn set_relay_url(app: AppHandle, url: Option<String>) -> Result<(), String> {
    let home = home_dir().ok_or_else(|| "home directory is unavailable".to_string())?;
    save_relay_url(&home, url.as_deref()).map_err(|error| error.to_string())?;
    // 不把 URL 打进日志(设计 §5.5):只说是否已配置。
    let configured = url
        .as_deref()
        .map(str::trim)
        .is_some_and(|url| !url.is_empty());
    eprintln!("[sidecar] relay setting saved (configured: {configured}); restarting");
    kill_sidecar(&app);
    Ok(())
}

/// 前端引导完成后调用：杀掉当前侧车，让监督循环用刚写入的 `.env` 重新拉起。
///
/// 侧车在启动时把 `cfg` 与清单读进内存，所以新写的 `.env` 只有换一个进程才
/// 生效。这里只杀不拉：监管循环（`spawn`）会自动补上，前端经既有的
/// `exited → 重新握手` 路径刷回主界面。
#[tauri::command]
pub fn restart_sidecar(app: AppHandle) -> Result<(), String> {
    kill_sidecar(&app);
    Ok(())
}

/// Kill the current sidecar child so the supervisor loop respawns it with the
/// freshly persisted settings. An already-dead child is fine (its kill error is
/// ignored): the supervisor is about to restart it either way.
fn kill_sidecar(app: &AppHandle) {
    let taken = app
        .state::<Sidecar>()
        .child
        .lock()
        .ok()
        .and_then(|mut guard| guard.take());
    if let Some(child) = taken {
        let _ = child.kill();
    }
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

    /// env 是显式覆盖:非空即赢,落盘值被忽略。
    #[test]
    fn resolve_relay_prefers_a_non_blank_env_value() {
        assert_eq!(
            resolve_relay(
                Some("wss://env/connect?session=e"),
                Some("wss://stored/connect?session=s")
            )
            .as_deref(),
            Some("wss://env/connect?session=e")
        );
        // 两侧都 trim:env 里的空白被剥掉后仍是非空,照样优先。
        assert_eq!(
            resolve_relay(Some("  wss://env  "), Some("wss://stored")).as_deref(),
            Some("wss://env")
        );
        // env 为空白等于未设置,让位给落盘值。
        assert_eq!(
            resolve_relay(Some("   "), Some("wss://stored  ")).as_deref(),
            Some("wss://stored")
        );
    }

    /// 无 env 时用落盘值;空白落盘值等于未设置。
    #[test]
    fn resolve_relay_uses_the_stored_value_when_env_is_unset() {
        assert_eq!(
            resolve_relay(None, Some("  wss://stored/connect?session=s  ")).as_deref(),
            Some("wss://stored/connect?session=s")
        );
        assert_eq!(resolve_relay(Some(""), Some("")), None);
        assert_eq!(resolve_relay(None, Some("  ")), None);
    }

    /// 两个来源都空 → None,即今日的纯 stdio 行为。
    #[test]
    fn resolve_relay_is_none_when_both_sources_are_empty() {
        assert_eq!(resolve_relay(None, None), None);
    }

    /// `resolve_relay` 只读入参,不碰进程 env——三分支因此可并行测。
    #[test]
    fn resolve_relay_reads_only_its_arguments() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();
        std::env::set_var("YI_AGENT_RELAY", "wss://env/connect?session=e");
        assert_eq!(
            resolve_relay(None, Some("wss://stored")).as_deref(),
            Some("wss://stored"),
            "the env var must not leak into the pure resolver"
        );
        restore_relay_env(previous);
    }

    /// 落盘往返:save 后 load 读回同一个值,写入的是 `<home>/.yi-agent/preferences.json`。
    #[test]
    fn saving_the_relay_url_round_trips_through_the_preferences_file() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();
        assert!(dir
            .path()
            .join(".yi-agent")
            .join("preferences.json")
            .is_file());
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://r/connect?session=x")
        );
    }

    /// `None` / 空白 → 删除键,而不是留空串。清除即回到纯 stdio。
    #[test]
    fn saving_an_empty_relay_url_removes_the_key() {
        for clear in [None, Some(""), Some("   ")] {
            let dir = tempfile::TempDir::new().unwrap();
            save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();
            save_relay_url(dir.path(), clear).unwrap();
            assert_eq!(load_relay_url(dir.path()), None, "clear: {clear:?}");
            let text =
                std::fs::read_to_string(dir.path().join(".yi-agent").join("preferences.json"))
                    .unwrap();
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            assert!(
                value.get("relay_url").is_none(),
                "a cleared key must be removed: {text}"
            );
        }
    }

    /// 读-改-写:侧车偏好文件里的 theme/值守/subagent_runtime 必须原样保留。
    #[test]
    fn saving_the_relay_url_preserves_unrelated_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent").join("preferences.json"),
            r#"{"theme":"light","board_watchman_enabled":false,"subagent_runtime":"never"}"#,
        )
        .unwrap();
        save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();

        let text =
            std::fs::read_to_string(dir.path().join(".yi-agent").join("preferences.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["relay_url"], "wss://r/connect?session=x");
        assert_eq!(value["theme"], "light");
        assert_eq!(value["board_watchman_enabled"], false);
        assert_eq!(value["subagent_runtime"], "never");
    }

    /// 坏文件回退「未设置」,且覆盖写会把它修好,绝不阻断侧车启动。
    #[test]
    fn a_broken_preferences_file_reads_as_no_relay() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
        std::fs::write(
            dir.path().join(".yi-agent").join("preferences.json"),
            "not json",
        )
        .unwrap();
        assert_eq!(load_relay_url(dir.path()), None);

        save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();
        assert_eq!(
            load_relay_url(dir.path()).as_deref(),
            Some("wss://r/connect?session=x")
        );
    }

    /// 落盘不留临时文件(临时文件 + rename 原子替换的既有约定)。
    #[test]
    fn saving_the_relay_url_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        save_relay_url(dir.path(), Some("wss://r/connect?session=x")).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "preferences.json")
            .collect();
        assert!(
            stray.is_empty(),
            "no temp file may survive a save: {stray:?}"
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

    /// `configured_relay` 读 env 与 home 的偏好文件。测试传入一个空的临时 home,
    /// 免得读到开发者机器上真实的 `~/.yi-agent/preferences.json`。
    #[test]
    fn configured_relay_is_none_when_unset_or_blank() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();
        let home = tempfile::TempDir::new().unwrap();

        std::env::remove_var("YI_AGENT_RELAY");
        assert_eq!(
            configured_relay_in(Some(home.path())),
            None,
            "unset and nothing stored must mean no relay"
        );

        std::env::set_var("YI_AGENT_RELAY", "   ");
        assert_eq!(
            configured_relay_in(Some(home.path())),
            None,
            "blank must mean no relay"
        );

        restore_relay_env(previous);
    }

    #[test]
    fn configured_relay_trims_a_set_url() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();
        let home = tempfile::TempDir::new().unwrap();

        std::env::set_var("YI_AGENT_RELAY", "  wss://r/connect?session=x  ");
        assert_eq!(
            configured_relay_in(Some(home.path())).as_deref(),
            Some("wss://r/connect?session=x")
        );

        restore_relay_env(previous);
    }

    /// 无 env 时 `configured_relay` 用偏好文件里的值 —— 这正是本次修复的落点。
    #[test]
    fn configured_relay_falls_back_to_the_home_preferences_file() {
        let _guard = RELAY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var("YI_AGENT_RELAY").ok();
        let home = tempfile::TempDir::new().unwrap();

        std::env::remove_var("YI_AGENT_RELAY");
        save_relay_url(home.path(), Some("wss://stored/connect?session=s")).unwrap();
        assert_eq!(
            configured_relay_in(Some(home.path())).as_deref(),
            Some("wss://stored/connect?session=s")
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
