#[cfg(desktop)]
mod bridge;
#[cfg(desktop)]
mod remote;
#[cfg(desktop)]
pub mod window;

/// 本壳的构建标识（由 `build.rs` 注入）。
///
/// 缺失时退回 `"release"`：宁可开关全部失效，也不让未验证的功能漏给用户
/// （与 `yi-agent-flags::build_kind` 的兜底一致）。
#[cfg(desktop)]
const BUILD_KIND: &str = match option_env!("YI_AGENT_BUILD_KIND") {
    Some(kind) => kind,
    None => "release",
};

#[cfg(desktop)]
const WRAPPER_VERSION: &str = match option_env!("YI_AGENT_WRAPPER_VERSION") {
    Some(version) => version,
    None => "0.0.0",
};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 必须在任何开关判定之前。OnceLock：重复调用无副作用。
    #[cfg(desktop)]
    yi_agent_flags::set_build_info(BUILD_KIND, WRAPPER_VERSION);

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init());

    // 桌面端自带 app-server 侧车（经 stdio 驱动）；iOS 是纯 WebSocket 客户端，
    // **没有侧车**（见 `desktop/src/transportFactory.ts`）。故侧车管理只在桌面注册，
    // 否则 iOS 构建会去找一个它用不上的 externalBin（并试图 spawn 它）。
    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_shell::init())
        .manage(bridge::Sidecar::new())
        .invoke_handler(tauri::generate_handler![
            bridge::rpc,
            bridge::rpc_respond,
            bridge::set_relay_url,
            bridge::restart_sidecar,
            bridge::kill_remote_slots,
            bridge::remote_connect,
            bridge::remote_disconnect,
            bridge::remote_status,
            bridge::remote_enabled
        ])
        .setup(|app| {
            bridge::spawn(app.handle(), crate::remote::profile::ProfileKey::Local)?;
            // Sizing is cosmetic: log and continue rather than fail launch.
            if let Err(err) = window::fit_main_window(app.handle()) {
                eprintln!("[window] fit failed: {err}");
            }
            Ok(())
        });

    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Closing the main window hides it instead of quitting, so the app keeps
    // running in the Dock like every other Mac app. `Reopen` (the Dock click)
    // only exists on macOS; elsewhere the framework default is correct.
    #[cfg(target_os = "macos")]
    app.run(|app, event| window::handle_run_event(app, &event));
    #[cfg(not(target_os = "macos"))]
    app.run(|_app, _event| {});
}
