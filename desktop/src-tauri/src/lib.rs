#[cfg(desktop)]
mod bridge;
#[cfg(desktop)]
pub mod window;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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
            bridge::restart_sidecar
        ])
        .setup(|app| {
            bridge::spawn(app.handle())?;
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
