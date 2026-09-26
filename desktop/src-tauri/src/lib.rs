mod bridge;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(bridge::Sidecar::new())
        .invoke_handler(tauri::generate_handler![bridge::rpc, bridge::rpc_respond])
        .setup(|app| {
            bridge::spawn(app.handle())?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
