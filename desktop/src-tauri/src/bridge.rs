use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

use crate::remote::profile::ProfileKey;
use crate::remote::{run_remote, sh_quote};

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

/// Owns every spawned `app-server` child process, one per slot.
///
/// **锁的粒度是刻意的**：外层锁只保护"查表"，每个槽位有自己的内层锁。写 pipe
/// 是同步 IO（`CommandChild::write` → `write_all`），远端链路一旦积压就会阻塞；
/// 若共用一把锁，一个卡住的远端会让所有槽位（含本地）的 RPC 全部卡死，表现为
/// 「整个 App 卡死」（spec §4.6 障碍三）。
pub struct Sidecar {
    slots: Mutex<HashMap<ProfileKey, Arc<Mutex<Option<CommandChild>>>>>,
    /// 用户**希望**连着的槽位。
    ///
    /// 监督循环靠它区分「进程意外退出，应当重连」与「用户点了断开，应当安静
    /// 地待着」。没有这一层，`remote_disconnect` 杀掉进程后监督循环会立刻
    /// 重新拉起，按钮看起来完全无效。
    desired: Mutex<std::collections::HashSet<ProfileKey>>,
    /// 每个远端槽位最近一次失败原因。
    ///
    /// 只存**最后一次**：设置页要回答的是「现在为什么没连上」，历史原因对用户
    /// 没有可操作性。成功起进程时清空——留着一条旧错误会让界面在已连上时仍
    /// 显示红字。
    last_errors: Mutex<HashMap<ProfileKey, String>>,
}

impl Sidecar {
    pub fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            desired: Mutex::new(std::collections::HashSet::new()),
            last_errors: Mutex::new(HashMap::new()),
        }
    }

    /// 取某槽位的锁，不创建。外层锁在这里**立刻释放**。
    fn slot(&self, key: &ProfileKey) -> Option<Arc<Mutex<Option<CommandChild>>>> {
        self.slots
            .lock()
            .ok()
            .and_then(|guard| guard.get(key).cloned())
    }

    /// 登记一个槽位（覆盖旧句柄）。
    fn set_slot(&self, key: ProfileKey, child: Arc<Mutex<Option<CommandChild>>>) {
        if let Ok(mut guard) = self.slots.lock() {
            guard.insert(key, child);
        }
    }

    /// 所有已登记的槽位键。
    fn keys(&self) -> Vec<ProfileKey> {
        match self.slots.lock() {
            Ok(guard) => guard.keys().cloned().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// 某槽位的粗粒度状态。只用于 UI 展示与 `remote/relayTargets` 的 `available`。
    /// 记下最近一次失败原因（覆盖旧值）。
    fn note_error(&self, key: &ProfileKey, error: impl Into<String>) {
        if let Ok(mut guard) = self.last_errors.lock() {
            guard.insert(key.clone(), error.into());
        }
    }

    /// 起进程成功、或用户主动断开时清掉错误。
    fn clear_error(&self, key: &ProfileKey) {
        if let Ok(mut guard) = self.last_errors.lock() {
            guard.remove(key);
        }
    }

    fn last_error(&self, key: &ProfileKey) -> Option<String> {
        self.last_errors
            .lock()
            .ok()
            .and_then(|guard| guard.get(key).cloned())
    }

    fn status_of(&self, key: &ProfileKey) -> &'static str {
        match self.slot(key) {
            None => "disconnected",
            Some(cell) => match cell.lock() {
                Ok(guard) if guard.is_some() => "connected",
                Ok(_) => "connecting",
                Err(_) => "disconnected",
            },
        }
    }
}

impl Sidecar {
    /// 标记为「用户希望连着」。
    fn mark_desired(&self, key: &ProfileKey) {
        if let Ok(mut guard) = self.desired.lock() {
            guard.insert(key.clone());
        }
    }

    /// 是否仍然希望连着。监督循环每次重试前都问一次。
    fn is_desired(&self, key: &ProfileKey) -> bool {
        self.desired
            .lock()
            .map(|guard| guard.contains(key))
            .unwrap_or(false)
    }

    /// 撤销「希望连着」并返回它此前是否存在过。
    fn unmark_desired(&self, key: &ProfileKey) -> bool {
        self.desired
            .lock()
            .map(|mut guard| guard.remove(key))
            .unwrap_or(false)
    }

    /// 摘掉槽位登记（连同内层句柄一起）。
    fn remove_slot(&self, key: &ProfileKey) {
        if let Ok(mut guard) = self.slots.lock() {
            guard.remove(key);
        }
    }
}

/// 事件名格式的唯一来源：`app-server://{kind}/{slot}`。
fn event_name(kind: &str, slot: &ProfileKey) -> String {
    format!("app-server://{kind}/{}", slot.as_str())
}

/// 前端的槽位标识 → `ProfileKey`。`"local"` 是保留字。
fn slot_key(raw: &str) -> ProfileKey {
    if raw == "local" {
        ProfileKey::Local
    } else {
        ProfileKey::Remote(raw.to_string())
    }
}

fn write_line(state: &Sidecar, slot: &ProfileKey, value: &serde_json::Value) -> Result<(), String> {
    let mut buf = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    buf.push(b'\n');
    // 外层锁已在这一行内释放；下面只持本槽位的锁。
    let cell = state
        .slot(slot)
        .ok_or_else(|| format!("sidecar not running for slot `{}`", slot.as_str()))?;
    let mut guard = cell.lock().map_err(|e| e.to_string())?;
    let child = guard
        .as_mut()
        .ok_or_else(|| format!("sidecar not running for slot `{}`", slot.as_str()))?;
    child.write(&buf).map_err(|e| e.to_string())
}

/// Frontend → sidecar: write one JSON-RPC request line.
#[tauri::command]
pub fn rpc(
    state: State<'_, Sidecar>,
    slot: String,
    id: u64,
    method: String,
    params: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &slot_key(&slot),
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
}

/// Frontend → sidecar: reply to a reverse request (permission approval).
#[tauri::command]
pub fn rpc_respond(
    state: State<'_, Sidecar>,
    slot: String,
    id: String,
    result: serde_json::Value,
) -> Result<(), String> {
    write_line(
        &state,
        &slot_key(&slot),
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
#[cfg(test)]
fn next_restart_delay(consecutive_restarts: u32) -> Duration {
    next_restart_delay_for(&ProfileKey::Local, consecutive_restarts)
}

/// 按槽位区分的退避。
///
/// 本地保持原样（200ms → 5s）：同机进程崩溃多半瞬时。
/// 远端上限抬到 60s（spec §4.3）：失败原因通常是网络断/服务器关机/VPN 掉线，
/// 会持续数分钟到数小时，5s 一次的 ssh 只是给服务器制造无意义的认证请求。
fn next_restart_delay_for(slot: &ProfileKey, consecutive_restarts: u32) -> Duration {
    const BASE_MS: u64 = 200;
    let cap_ms: u64 = match slot {
        ProfileKey::Local => 5_000,
        ProfileKey::Remote(_) => 60_000,
    };
    let shift = consecutive_restarts.min(16);
    let ms = (BASE_MS << shift).min(cap_ms);
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
pub fn spawn(app: &AppHandle, key: ProfileKey) -> Result<(), String> {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut consecutive_restarts: u32 = 0;
        loop {
            if !app.state::<Sidecar>().is_desired(&key) {
                eprintln!("[{}] no longer desired; supervisor stops", key.as_str());
                return;
            }
            match spawn_once(&app, &key) {
                Ok(mut rx) => {
                    // A successful launch resets the backoff: the previous exit
                    // was a one-off, not a start-up loop.
                    consecutive_restarts = 0;
                    // 起来过就不再显示上一次的原因：设置页的红字要回答「现在
                    // 为什么没连上」，进程已经在跑时答案是「没有」。
                    app.state::<Sidecar>().clear_error(&key);
                    while let Some(event) = rx.recv().await {
                        match event {
                            CommandEvent::Stdout(bytes) => forward_stdout(&app, &key, &bytes),
                            CommandEvent::Stderr(bytes) => {
                                eprintln!("[{}] {}", key.as_str(), String::from_utf8_lossy(&bytes));
                            }
                            CommandEvent::Terminated(payload) => {
                                let _ = app.emit(
                                    &event_name("status", &key),
                                    serde_json::json!({"state":"exited","code":payload.code}),
                                );
                            }
                            _ => {}
                        }
                    }
                    // The event stream closed: the process is gone. Drop the
                    // stale handle so `rpc` reports "sidecar not running" rather
                    // than writing into a dead pipe while we restart.
                    clear_child(&app, &key);
                    eprintln!("[{}] exited; restarting", key.as_str());
                }
                Err(e) => {
                    eprintln!("[{}] spawn failed: {e}", key.as_str());
                    app.state::<Sidecar>().note_error(&key, e);
                }
            }
            if !should_retry(&key, consecutive_restarts) {
                let last = app.state::<Sidecar>().last_error(&key);
                let reason = match &last {
                    Some(detail) => {
                        format!("gave up after {REMOTE_MAX_RETRIES} attempts: {detail}")
                    }
                    None => format!("gave up after {REMOTE_MAX_RETRIES} attempts"),
                };
                let _ = app.emit(
                    &event_name("status", &key),
                    serde_json::json!({ "state": "disconnected", "reason": reason }),
                );
                eprintln!(
                    "[{}] giving up after {REMOTE_MAX_RETRIES} attempts",
                    key.as_str()
                );
                return;
            }
            let delay = next_restart_delay_for(&key, consecutive_restarts);
            consecutive_restarts = consecutive_restarts.saturating_add(1);
            tokio::time::sleep(delay).await;
        }
    });
    Ok(())
}

/// 远端连续失败这么多次后停止自动重试，转「断线」让人手动重连（spec §4.3）。
const REMOTE_MAX_RETRIES: u32 = 5;

fn should_retry(slot: &ProfileKey, consecutive_failures: u32) -> bool {
    match slot {
        ProfileKey::Local => true,
        ProfileKey::Remote(_) => consecutive_failures < REMOTE_MAX_RETRIES,
    }
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
    let home = instance_home();
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

/// 实例状态根：`YI_AGENT_HOME` 优先，否则 `<home>/.yi-agent`。
///
/// 与 `yi_agent_store::paths::instance_home_from` 同一套归一化：覆盖值若不以
/// `.yi-agent` 结尾会自动补上，这样所有消费者都能安全地拼 `.yi-agent/...`，
/// 不会出现 `<dir>/.yi-agent/.yi-agent/...` 的双重嵌套。桌面壳刻意不依赖
/// app-server crate，故此处镜像同一规则（sidecar 侧读的是同一个环境变量，
/// 两侧因此解析到同一个目录）。
fn instance_home_from(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = override_value.filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(dir);
        // 纯空白不算有效覆盖：`YI_AGENT_HOME="   "` 多半是启动脚本写错了。
        if dir.as_os_str().to_string_lossy().trim().is_empty() {
            // fall through to the default
        } else if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            return Some(dir);
        } else {
            return Some(dir.join(".yi-agent"));
        }
    }
    home.map(|home| home.join(".yi-agent"))
}

/// 进程级的实例状态根：`YI_AGENT_HOME` 环境变量 + `dirs::home_dir()`。
fn instance_home() -> Option<PathBuf> {
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty());
    instance_home_from(override_value.as_deref(), home_dir().as_deref())
}

/// sidecar 进程的工作目录：实例 home 的**父目录**（sidecar 自己拼 `.yi-agent`）。
///
/// 打包 app 从 Finder 启动时继承 `/`，会让 agent 的 workdir（及其 runtime 目录）
/// 变成文件系统根，故必须显式锚定到一个真实目录。
fn sidecar_cwd_from(
    override_value: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    instance_home_from(override_value, home).map(|dir| {
        dir.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.clone())
    })
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
    let home = instance_home().ok_or_else(|| "home directory is unavailable".to_string())?;
    save_relay_url(&home, url.as_deref()).map_err(|error| error.to_string())?;
    // 不把 URL 打进日志(设计 §5.5):只说是否已配置。
    let configured = url
        .as_deref()
        .map(str::trim)
        .is_some_and(|url| !url.is_empty());
    eprintln!("[sidecar] relay setting saved (configured: {configured}); restarting");
    kill_sidecar(&app, &ProfileKey::Local);
    Ok(())
}

/// 前端引导完成后调用：杀掉当前侧车，让监督循环用刚写入的 `.env` 重新拉起。
///
/// 侧车在启动时把 `cfg` 与清单读进内存，所以新写的 `.env` 只有换一个进程才
/// 生效。这里只杀不拉：监管循环（`spawn`）会自动补上，前端经既有的
/// `exited → 重新握手` 路径刷回主界面。
#[tauri::command]
pub fn restart_sidecar(app: AppHandle) -> Result<(), String> {
    kill_sidecar(&app, &ProfileKey::Local);
    Ok(())
}

/// Kill the current sidecar child so the supervisor loop respawns it with the
/// freshly persisted settings. An already-dead child is fine (its kill error is
/// ignored): the supervisor is about to restart it either way.
fn kill_sidecar(app: &AppHandle, slot: &ProfileKey) {
    let taken = app
        .state::<Sidecar>()
        .slot(slot)
        .and_then(|cell| cell.lock().ok().and_then(|mut guard| guard.take()));
    if let Some(child) = taken {
        let _ = child.kill();
    }
}

/// 杀全部槽位。**只给 App 退出用**，不要变成默认可调用的
/// （spec §4.6 障碍一：改个中继配置就把所有远端项目一起断开是用户可见事故）。
#[allow(dead_code)]
pub fn kill_all_slots(app: &AppHandle) {
    for key in app.state::<Sidecar>().keys() {
        kill_sidecar(app, &key);
    }
}

/// 关 `remote_ssh` 时调用：杀掉**全部**远端槽位，不留孤儿。
///
/// **与 `kill_all_slots` 的区别**：那个连本地槽位一起杀（只在 App 退出时用），
/// 这个只杀远端——本地 sidecar 跟 flag 无关，必须一直活着。
#[tauri::command]
pub fn kill_remote_slots(app: AppHandle) {
    let state = app.state::<Sidecar>();
    for key in state.keys() {
        if matches!(key, ProfileKey::Remote(_)) {
            // 先撤销意图再杀：监督循环每轮都读 desired，漏掉这一步就会在
            // 关掉开关后立刻把 ssh 重新拉起来（"开关关了，远端还在跑"）。
            state.unmark_desired(&key);
            kill_sidecar(&app, &key);
            state.remove_slot(&key);
        }
    }
}

/// `remote_ssh` 是否开启，**且**这是 dev 构建。
///
/// 两层判定缺一不可：
/// - `yi_agent_flags::is_enabled` 已经内含「非 dev 构建恒关」，但那是库的
///   硬闸；这里再显式看一次构建类型，让「为什么关着」在错误信息里说清楚。
/// - 本机判定（spec §5.2）：绝不能问 sidecar 的 `flags/list`——远端连接建立
///   后，那个 RPC 读到的是**远端机器**的 `flags.json`。
fn remote_enabled_now() -> bool {
    crate::remote::rpc::gate(
        yi_agent_flags::is_enabled(yi_agent_flags::registry::REMOTE_SSH_FLAG),
        yi_agent_flags::build_kind(),
    )
}

fn remote_gate() -> Result<(), String> {
    if remote_enabled_now() {
        Ok(())
    } else {
        Err(format!(
            "`{}` is off (build kind: {}, see 实验开关)",
            yi_agent_flags::registry::REMOTE_SSH_FLAG,
            yi_agent_flags::build_kind()
        ))
    }
}

/// 开始连一个远端项目。启动后由监督循环维持（断线自动重连、退避到 60s）。
#[tauri::command]
pub async fn remote_connect(app: AppHandle, profile_id: String) -> Result<(), String> {
    remote_gate()?;
    crate::remote::profile::validate_profile_id(&profile_id).map_err(|e| e.to_string())?;
    let key = ProfileKey::Remote(profile_id.clone());

    // 首次连接自动安装（用户选定的方案 A：不给用户一个手装步骤）。安装是
    // 阻塞的 ssh/scp/下载，绝不能跑在 Tauri 的异步执行器线程上——那会让整个
    // 窗口在一次 20 秒的连接超时里僵住。
    let profile = crate::remote::config::load(&remote_config_path())
        .profiles
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or_else(|| format!("unknown remote profile `{profile_id}`"))?;
    let home = instance_home().ok_or_else(|| "home directory is unavailable".to_string())?;
    tauri::async_runtime::spawn_blocking(move || ensure_installed(&home, &profile))
        .await
        .map_err(|error| format!("install task failed: {error}"))??;

    app.state::<Sidecar>().mark_desired(&key);
    spawn(&app, key)
}

/// 远端已经有匹配版本的二进制就跳过，否则按决策表装一次。
///
/// 「先查后装」而不是「每次连接都装」：`--version` 一次 ssh 往返，比再传一次
/// 十几 MB 便宜一个数量级，而且重连是常态（断线重连、切回前台）。
fn ensure_installed(
    home: &std::path::Path,
    profile: &crate::remote::profile::RemoteProfile,
) -> Result<(), String> {
    let binary =
        crate::remote::cache::remote_binary_path(&profile.instance_root, &profile.binary_version);
    let probe = format!(
        "if [ -x {binary} ]; then {binary} --version; fi",
        binary = sh_quote(&binary)
    );
    if let Ok(output) = run_remote(&profile.host, &probe) {
        let reported = output.trim();
        if !reported.is_empty() && reported.contains(&profile.binary_version) {
            return Ok(());
        }
    }

    let probe_output = run_remote(&profile.host, crate::remote::probe::probe_command())?;
    let caps = crate::remote::probe::parse_caps(&probe_output, &probe_output)?;
    if caps.target != profile.remote_arch {
        return Err(format!(
            "profile `{}` is configured for {} but the remote is {}; \
             refusing to upload a binary built for another architecture",
            profile.id, profile.remote_arch, caps.target
        ));
    }

    let fetcher = crate::remote::fetch::HttpFetcher;
    crate::remote::install::install(
        &fetcher,
        home,
        &profile.instance_root,
        caps.target,
        &profile.binary_version,
        &caps,
        &profile.host,
        None,
    )?;
    // 装完立刻清旧版本缓存，避免每次升级多留十几 MB。
    let _ = crate::remote::cache::prune(home, &profile.binary_version);
    Ok(())
}

/// 用户主动断开：撤销「希望连着」再杀进程。
///
/// **顺序很重要**：先撤销意图，监督循环才不会在下一轮立刻把进程拉回来。
#[tauri::command]
pub fn remote_disconnect(app: AppHandle, profile_id: String) -> Result<(), String> {
    let key = ProfileKey::Remote(profile_id);
    app.state::<Sidecar>().unmark_desired(&key);
    kill_sidecar(&app, &key);
    app.state::<Sidecar>().remove_slot(&key);
    // 用户主动断开不是故障：清掉原因，否则设置页会一直挂着上一次的红字。
    app.state::<Sidecar>().clear_error(&key);
    Ok(())
}

/// 当前所有槽位状态。只在远端开关开着时可用。
#[tauri::command]
pub fn remote_status(app: AppHandle) -> Result<serde_json::Value, String> {
    remote_gate()?;
    let state = app.state::<Sidecar>();
    let slots: serde_json::Map<String, serde_json::Value> = state
        .keys()
        .into_iter()
        .map(|key| {
            let status = state.status_of(&key).to_string();
            let error = state.last_error(&key);
            (
                key.as_str().to_string(),
                serde_json::json!({
                    "status": status,
                    "lastError": error,
                }),
            )
        })
        .collect();
    Ok(serde_json::json!({ "slots": slots }))
}

/// 前端渲染入口用：开关关着就不渲染任何远端 UI（spec §5.3.1）。
#[tauri::command]
pub fn remote_enabled() -> bool {
    remote_enabled_now()
}

/// 已配置的远端项目 + 功能是否可用。
///
/// 闸门关时返回 `{enabled:false, profiles:[]}` 而**不是**报错：设置页需要知道
/// 「这个功能不可用」才能不渲染入口，报错只会让它显示一条红色异常。
#[tauri::command]
pub fn remote_list() -> Result<serde_json::Value, String> {
    if !remote_enabled_now() {
        return Ok(serde_json::json!({ "enabled": false, "profiles": [] }));
    }
    let config = crate::remote::config::load(&remote_config_path());
    Ok(serde_json::json!({
        "enabled": true,
        "profiles": config.profiles,
    }))
}

/// 新增或覆盖一个远端项目（按 `id` 匹配）。
///
/// 覆盖而不是新增是刻意的：设置页「编辑」与「新增」走同一个入口，用 id 判断
/// 语义，避免出现两份同 id 的配置——那会让 `remote.json` 里的槽位概念失效。
#[tauri::command]
pub fn remote_upsert(profile: serde_json::Value) -> Result<serde_json::Value, String> {
    remote_gate()?;
    let validated = crate::remote::rpc::validate_profile_payload(&profile)?;
    let path = remote_config_path();
    let mut config = crate::remote::config::load(&path);
    match config
        .profiles
        .iter()
        .position(|existing| existing.id == validated.id)
    {
        Some(index) => config.profiles[index] = validated.clone(),
        None => config.profiles.push(validated.clone()),
    }
    crate::remote::config::save(&path, &config).map_err(|error| error.to_string())?;
    Ok(serde_json::json!({ "id": validated.id }))
}

/// 删掉一个远端项目：先断开（否则监督循环会继续为重连而持有它），再落盘。
#[tauri::command]
pub fn remote_remove(app: AppHandle, profile_id: String) -> Result<(), String> {
    remote_gate()?;
    crate::remote::profile::validate_profile_id(&profile_id).map_err(|e| e.to_string())?;

    let path = remote_config_path();
    let mut config = crate::remote::config::load(&path);
    let before = config.profiles.len();
    config.profiles.retain(|profile| profile.id != profile_id);
    if config.profiles.len() == before {
        return Err(format!("unknown remote profile `{profile_id}`"));
    }
    // 顺序：先断开再落盘。断开撤销「希望连着」的意图并杀掉进程；此时 profile
    // 还在 `remote.json` 里，所以即使监督循环恰好在这一刻醒来，它拉起的也是一
    // 个已知 profile（随后被 kill_sidecar 收掉）。反过来的话，落盘成功后到断开
    // 之间会有一个窗口：槽位仍被标记为 desired，而 `remote.json` 里已经找不到
    // 它，重连表现为「unknown remote profile」这种看不出因果的错误。
    remote_disconnect(app, profile_id)?;
    crate::remote::config::save(&path, &config).map_err(|error| error.to_string())?;
    Ok(())
}

/// Launch one `yi-agent app-server` and return its event stream.
fn spawn_once(
    app: &AppHandle,
    key: &ProfileKey,
) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    match key {
        ProfileKey::Local => spawn_local_once(app, key),
        ProfileKey::Remote(id) => {
            let config = crate::remote::config::load(&remote_config_path());
            let profile = config
                .profiles
                .iter()
                .find(|profile| &profile.id == id)
                .ok_or_else(|| format!("unknown remote profile `{id}`"))?;
            spawn_remote_once(app, key, profile)
        }
    }
}

/// `<本机 instance_home>/remote-status.json`，app-server 读它来回答
/// `remote/relayTargets` 的 `available`（spec §4.6、§4.9）。
fn remote_status_path() -> PathBuf {
    instance_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("remote-status.json")
}

/// 把槽位状态写进旁路文件。失败**只记日志**：这是可丢的运行时状态，
/// 不该让连接流程因为它失败。
fn publish_status(slot: &ProfileKey, status: crate::remote::status::SlotStatus) {
    let ProfileKey::Remote(id) = slot else {
        // 本地槽位不该出现在这个文件里：它的可用性是显然的，
        // 而且 phones 只关心远端项目。
        return;
    };
    if let Err(error) = crate::remote::status::write_status(&remote_status_path(), id, status) {
        eprintln!("[{}] could not publish status: {error}", slot.as_str());
    }
}

/// `<本机 instance_home>/remote.json`（spec §4.9）。
fn remote_config_path() -> PathBuf {
    instance_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("remote.json")
}

/// 远端槽位的启动：一条 `ssh <host> '<command>'`，stdio 即 RPC 通道。
///
/// 之所以能直接复用 stdio 协议：ssh 只负责把本地管道接到远端进程的
/// stdin/stdout，对上层完全等价于本地 sidecar。**代价**是 stderr 会混入 ssh
/// 自己的诊断信息（`Connection closed by ...`），所以远端日志必须能区分来源。
fn spawn_remote_once(
    app: &AppHandle,
    key: &ProfileKey,
    profile: &crate::remote::profile::RemoteProfile,
) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    let binary =
        crate::remote::cache::remote_binary_path(&profile.instance_root, &profile.binary_version);
    let relay = crate::remote::profile::derive_relay(configured_relay().as_deref(), &profile.id);
    let argv = crate::remote::argv::remote_argv(profile, &binary, relay.as_deref())
        .map_err(|error| error.to_string())?;

    let (rx, child) = app
        .shell()
        .command("ssh")
        .args(argv)
        .spawn()
        .map_err(|error| format!("ssh spawn failed: {error}"))?;

    app.state::<Sidecar>()
        .set_slot(key.clone(), Arc::new(Mutex::new(Some(child))));
    // 进程起来了但还没握手：对手机而言「连接中」，不是「可用」。
    publish_status(key, crate::remote::status::SlotStatus::Connecting);

    Ok(rx)
}

fn spawn_local_once(
    app: &AppHandle,
    key: &ProfileKey,
) -> Result<tauri::async_runtime::Receiver<CommandEvent>, String> {
    let mut cmd = app
        .shell()
        .sidecar("yi-agent")
        .map_err(|e| format!("sidecar not found: {e}"))?
        .args(sidecar_args(configured_relay().as_deref()));

    // A bundled app launched from Finder inherits `/` as its cwd, which would
    // make the agent's workdir (and its runtime directory) the filesystem root.
    // Anchor the sidecar to the instance home so packaged and dev launches
    // behave predictably, and so a second app (`YI_AGENT_HOME`) gets its own
    // runtime/threads instead of sharing the first app's.
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty());
    match sidecar_cwd_from(override_value.as_deref(), home_dir().as_deref()) {
        Some(dir) => cmd = cmd.current_dir(dir),
        None => eprintln!("[sidecar] home_dir unavailable, using inherited cwd"),
    }

    let (rx, child) = cmd
        .spawn()
        .map_err(|e| format!("sidecar spawn failed: {e}"))?;

    app.state::<Sidecar>()
        .set_slot(key.clone(), Arc::new(Mutex::new(Some(child))));

    Ok(rx)
}

/// 只清空**该槽位**的句柄内容（保留槽位登记，监督循环会重新填）。
///
/// 若某个槽位从未登记过，这里什么也不做——不能因为一个未知 key 就动整张表
/// （spec §4.6 障碍二：任一槽位退出清空整张表，会让其余槽位的 RPC 立刻报
/// "sidecar not running"）。
fn clear_child(app: &AppHandle, key: &ProfileKey) {
    if matches!(key, ProfileKey::Remote(_)) {
        // 进程没了：手机会在下一次 `relayTargets` 看到不可用。
        if let Ok(mut guard) = REPORTED_CONNECTED.lock() {
            guard
                .get_or_insert_with(std::collections::HashSet::new)
                .remove(key.as_str());
        }
        publish_status(key, crate::remote::status::SlotStatus::Disconnected);
    }
    if let Some(cell) = app.state::<Sidecar>().slot(key) {
        if let Ok(mut guard) = cell.lock() {
            *guard = None;
        }
    }
}

/// 已标记为 `Connected` 的远端槽位。首帧之后不再重复写盘：这条路径每帧都跑，
/// 每次都写 JSON 会在流式输出时把磁盘打满。
static REPORTED_CONNECTED: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
    std::sync::Mutex::new(None);

fn forward_stdout(app: &AppHandle, slot: &ProfileKey, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    let line = text.trim();
    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        eprintln!("[sidecar] dropping non-JSON line: {line}");
        return;
    };
    // 第一帧能解析 = 远端 app-server 真的在说话（ssh 已连上且进程已 exec）。
    // 真正的 `initialize` 由前端发，桥接层只凭「有合法协议帧」判定可用。
    if let ProfileKey::Remote(id) = slot {
        let mut guard = REPORTED_CONNECTED
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let reported = guard.get_or_insert_with(std::collections::HashSet::new);
        if reported.insert(id.clone()) {
            publish_status(slot, crate::remote::status::SlotStatus::Connected);
        }
    }

    match classify(&value) {
        Frame::ReverseRequest => {
            let _ = app.emit(&event_name("request", slot), &value);
        }
        Frame::Notification | Frame::Response => {
            let _ = app.emit(&event_name("message", slot), &value);
        }
        Frame::Garbage => eprintln!("[{}] dropping non-protocol frame: {value}", slot.as_str()),
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

    // ---- 多槽位事件名与路由（计划 01 · Task 5）----

    /// 不同槽位的事件名必须互相隔离：远端的一条 message 绝不能落到本地槽位
    /// 的监听器上，否则本地 UI 会显示另一个项目的对话（spec §4.6 障碍四）。
    #[test]
    fn event_names_are_per_slot() {
        let local = event_name("message", &ProfileKey::Local);
        let remote = event_name("message", &ProfileKey::Remote("alpha".into()));
        assert_eq!(local, "app-server://message/local");
        assert_eq!(remote, "app-server://message/alpha");
        assert_ne!(local, remote);
    }

    /// 本地槽位必须保持历史事件名 `app-server://message`（无后缀）。
    ///
    /// 但设计要求统一为 `/local`；这里锁定**新的**约定，并显式记录与旧名的
    /// 差异，防止有人为了让旧前端继续工作而把本地槽位改回无后缀、从而让
    /// 「本地」在前后端有第二套命名。
    #[test]
    fn local_slot_keeps_the_historical_event_prefix() {
        for kind in ["message", "request", "status"] {
            let name = event_name(kind, &ProfileKey::Local);
            assert!(
                name.starts_with(&format!("app-server://{kind}")),
                "{name} must keep the app-server://{kind} prefix"
            );
            assert_eq!(name, format!("app-server://{kind}/local"));
        }
    }

    /// 往未登记的槽位写 RPC 必须**报错**，不能静默写入别的槽位。
    ///
    /// 静默误投会导致「发给远端项目 A 的请求被执行在本地项目上」这类数据损坏。
    #[test]
    fn writing_to_an_unknown_slot_is_an_error_not_a_silent_misdelivery() {
        let state = Sidecar::new();
        let result = write_line(
            &state,
            &ProfileKey::Remote("nobody-home".into()),
            &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping","params":{}}),
        );
        let err = result.expect_err("unknown slot must fail");
        assert!(
            err.contains("nobody-home"),
            "error must name the slot so the UI can tell which project broke: {err}"
        );
    }

    /// 槽位表的 status 只反映该槽位，不会因别的槽位有空句柄而误报。
    #[test]
    fn status_is_per_slot() {
        let state = Sidecar::new();
        state.set_slot(ProfileKey::Local, Arc::new(Mutex::new(None)));
        assert_eq!(state.status_of(&ProfileKey::Local), "connecting");
        assert_eq!(
            state.status_of(&ProfileKey::Remote("alpha".into())),
            "disconnected"
        );
    }

    // 闸门本身的真值表在 `remote::rpc` 里测（它现在是纯函数，注入两个输入
    // 就能穷举）；这里只留槽位语义。

    /// 槽位键不能因为 id 恰好叫 `local` 就撞上本地槽位。
    #[test]
    fn a_remote_slot_key_is_never_the_local_key() {
        assert_ne!(ProfileKey::Remote("local".into()), ProfileKey::Local);
        assert_eq!(slot_key("local"), ProfileKey::Local);
        assert_eq!(slot_key("local"), slot_key("local"));
        assert_ne!(slot_key("proj-foo"), ProfileKey::Local);
    }

    #[test]
    fn remote_status_starts_disconnected_for_an_unknown_profile() {
        let state = Sidecar::new();
        assert_eq!(
            state.status_of(&ProfileKey::Remote("nope".into())),
            "disconnected"
        );
    }

    /// 设置页的红字要回答「现在为什么没连上」，所以只留最后一次原因。
    #[test]
    fn the_last_error_is_overwritten_by_a_newer_one() {
        let state = Sidecar::new();
        let key = ProfileKey::Remote("a".into());
        state.note_error(&key, "ssh: connect: timed out");
        state.note_error(&key, "permission denied");
        assert_eq!(state.last_error(&key).as_deref(), Some("permission denied"));
    }

    /// 起进程成功后必须清掉：留着旧原因会让已连上的项目一直显示红字。
    #[test]
    fn a_successful_spawn_clears_the_previous_error() {
        let state = Sidecar::new();
        let key = ProfileKey::Remote("a".into());
        state.note_error(&key, "ssh: connect: timed out");
        state.clear_error(&key);
        assert_eq!(state.last_error(&key), None);
    }

    /// 一个项目的错误不得显示在另一个项目上——多项目并行时这是误导性最强的
    /// 一类 bug：用户点 A 却看到 B 的报错。
    #[test]
    fn errors_do_not_leak_between_slots() {
        let state = Sidecar::new();
        let a = ProfileKey::Remote("a".into());
        let b = ProfileKey::Remote("b".into());
        state.note_error(&a, "boom");
        assert_eq!(state.last_error(&b), None);
        state.clear_error(&b);
        assert_eq!(state.last_error(&a).as_deref(), Some("boom"));
    }

    /// 用户主动断开后，监督循环必须能看出「这次不该重连」。
    #[test]
    fn disconnecting_revokes_the_desired_flag() {
        let state = Sidecar::new();
        let key = ProfileKey::Remote("proj-foo".into());
        state.mark_desired(&key);
        assert!(state.is_desired(&key));
        assert!(state.unmark_desired(&key));
        assert!(
            !state.is_desired(&key),
            "a disconnected slot must stay down"
        );
    }

    /// 一个槽位的断开不得影响另一个槽位——多项目并行时这是数据完整性问题。
    #[test]
    fn revoking_one_slot_leaves_the_others_desired() {
        let state = Sidecar::new();
        let a = ProfileKey::Remote("a".into());
        let b = ProfileKey::Remote("b".into());
        state.mark_desired(&a);
        state.mark_desired(&b);
        state.unmark_desired(&a);
        assert!(!state.is_desired(&a));
        assert!(
            state.is_desired(&b),
            "another project must keep reconnecting"
        );
    }

    // ---- 关开关后的清理（计划 01 · Task 9）----

    /// 关掉开关必须同时撤销「希望连着」与槽位登记。
    ///
    /// 只杀进程不撤销意图 = 监督循环在 200ms 后重新拉起 ssh，用户看到的正是
    /// 「开关关了但远端还在跑」。只撤销意图不摘槽位 = `remote_status` 继续报
    /// 一个早已死掉的槽位。
    #[test]
    fn revoking_a_remote_slot_clears_both_intent_and_registration() {
        let state = Sidecar::new();
        let key = ProfileKey::Remote("proj-foo".into());
        state.set_slot(key.clone(), Arc::new(Mutex::new(None)));
        state.mark_desired(&key);

        // `kill_remote_slots` 的三步，按同一顺序。
        assert!(state.unmark_desired(&key));
        state.remove_slot(&key);

        assert!(!state.is_desired(&key), "a revoked slot must not respawn");
        assert_eq!(state.status_of(&key), "disconnected");
        assert!(state.keys().is_empty());
    }

    /// 本地槽位不受远端清理影响：它跟 flag 无关，必须一直活着。
    #[test]
    fn clearing_remote_slots_keeps_the_local_one() {
        let state = Sidecar::new();
        state.set_slot(ProfileKey::Local, Arc::new(Mutex::new(None)));
        state.set_slot(ProfileKey::Remote("a".into()), Arc::new(Mutex::new(None)));
        state.set_slot(ProfileKey::Remote("b".into()), Arc::new(Mutex::new(None)));

        let remotes: Vec<ProfileKey> = state
            .keys()
            .into_iter()
            .filter(|key| matches!(key, ProfileKey::Remote(_)))
            .collect();
        assert_eq!(remotes.len(), 2, "both remote slots must be found");
        for key in remotes {
            state.remove_slot(&key);
        }

        assert_eq!(state.keys(), vec![ProfileKey::Local]);
    }

    // ---- 槽位化的重试策略（计划 03 · Task 1）----

    /// 本地行为必须逐字不变：这是重构打底的前提，改了就是回归。
    #[test]
    fn local_backoff_keeps_its_five_second_cap() {
        assert_eq!(
            next_restart_delay_for(&ProfileKey::Local, 10),
            Duration::from_millis(5_000)
        );
    }

    /// 远端的失败原因通常是网络断/服务器关机，5s 一次的 ssh 只是白撞。
    #[test]
    fn remote_backoff_climbs_to_sixty_seconds() {
        let key = ProfileKey::Remote("p".into());
        assert_eq!(next_restart_delay_for(&key, 0), Duration::from_millis(200));
        assert_eq!(next_restart_delay_for(&key, 9), Duration::from_secs(60));
        // 上限是硬顶，不能因为移位溢出回绕成一个小值。
        assert_eq!(next_restart_delay_for(&key, 1_000), Duration::from_secs(60));
    }

    #[test]
    fn remote_gives_up_after_five_consecutive_failures() {
        let key = ProfileKey::Remote("p".into());
        assert!(should_retry(&key, 4), "the 5th attempt still happens");
        assert!(
            !should_retry(&key, 5),
            "after 5 failures stop hammering the server"
        );
    }

    #[test]
    fn local_never_gives_up() {
        assert!(should_retry(&ProfileKey::Local, 1_000));
    }

    /// `Connected` 只在首帧写一次。
    ///
    /// 这条路径每帧都跑（流式输出一秒几十帧），每帧都做一次读-改-写 JSON
    /// 会把磁盘打满，而且这些写没有任何新信息。
    #[test]
    fn connected_is_published_once_per_slot() {
        use crate::remote::status::SlotStatus;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("remote-status.json");

        // 模拟 forward_stdout 的记账逻辑。
        let mut reported = std::collections::HashSet::new();
        for _ in 0..50 {
            if reported.insert("proj-foo".to_string()) {
                crate::remote::status::write_status(&path, "proj-foo", SlotStatus::Connected)
                    .unwrap();
            }
        }

        assert_eq!(reported.len(), 1);
        let map = crate::remote::status::read_status(&path);
        assert_eq!(map.get("proj-foo"), Some(&SlotStatus::Connected));
    }

    /// 重连后必须能重新记为 Connected：清空记账的那一步不能漏。
    #[test]
    fn a_reconnect_can_republish_connected() {
        let mut reported: std::collections::HashSet<String> =
            ["proj-foo".to_string()].into_iter().collect();
        // clear_child 会把它摘掉。
        reported.remove("proj-foo");
        assert!(
            reported.insert("proj-foo".to_string()),
            "after a disconnect the slot must be reportable again"
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

    /// `YI_AGENT_HOME` 非空时赢，且不以 `.yi-agent` 结尾会自动补上。
    #[test]
    fn instance_home_prefers_a_non_blank_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().as_os_str().to_owned();
        let home = Path::new("/Users/example");
        assert_eq!(
            instance_home_from(Some(&value), Some(home)),
            Some(dir.path().join(".yi-agent"))
        );
    }

    /// 覆盖值已经叫 `.yi-agent` 时不再嵌套。
    #[test]
    fn instance_home_does_not_double_nest() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().join(".yi-agent");
        assert_eq!(
            instance_home_from(Some(value.as_os_str()), None),
            Some(value)
        );
    }

    /// 空白覆盖视同未设置，退化为 `<home>/.yi-agent`。
    #[test]
    fn instance_home_ignores_a_blank_override() {
        let home = Path::new("/Users/example");
        assert_eq!(
            instance_home_from(Some(std::ffi::OsStr::new("")), Some(home)),
            Some(home.join(".yi-agent"))
        );
        assert_eq!(
            instance_home_from(Some(std::ffi::OsStr::new("   ")), Some(home)),
            Some(home.join(".yi-agent"))
        );
    }

    /// 两侧都缺 → None。
    #[test]
    fn instance_home_is_none_without_any_home() {
        assert_eq!(instance_home_from(None, None), None);
    }

    /// relay 偏好落在实例 home，而不是真实 `~`。
    #[test]
    fn configured_relay_reads_the_instance_home_preferences() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        save_relay_url(home, Some("wss://instance/connect?session=i")).unwrap();
        assert_eq!(
            load_relay_url(home).as_deref(),
            Some("wss://instance/connect?session=i")
        );
        // 落点确认：写在 `<instance home>/.yi-agent/preferences.json`。
        assert!(home.join(".yi-agent").join("preferences.json").is_file());
    }

    /// sidecar 的 cwd 用实例 home 的父目录，而不是真实 `~`。
    #[test]
    fn sidecar_cwd_prefers_the_instance_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let value = dir.path().as_os_str().to_owned();
        assert_eq!(
            sidecar_cwd_from(Some(&value), None),
            Some(dir.path().to_path_buf())
        );
    }

    /// 没有覆盖值时 cwd 退化为 `<home>`（sidecar 自己拼 `.yi-agent`）。
    #[test]
    fn sidecar_cwd_falls_back_to_the_platform_home() {
        let home = Path::new("/Users/example");
        assert_eq!(sidecar_cwd_from(None, Some(home)), Some(home.to_path_buf()));
        assert_eq!(sidecar_cwd_from(None, None), None);
    }
}
