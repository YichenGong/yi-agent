#[cfg(desktop)]
mod bridge;
#[cfg(desktop)]
mod remote;
/// 桌面壳持有的 runtime 租约、退出屏障。
///
/// 必须由壳持有而不是 sidecar：App 的身份不能随 app-server 重启改变，残留的
/// sidecar 也不能替一个已经退出的 App 续租。
#[cfg(desktop)]
pub mod runtime_lifecycle;
mod runtime_manifest;
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
            bridge::remote_enabled,
            bridge::remote_list,
            bridge::remote_upsert,
            bridge::remote_remove,
            runtime_lifecycle::runtime_acquire_project,
            runtime_lifecycle::runtime_release_project,
            runtime_lifecycle::runtime_registered_projects,
            runtime_lifecycle::runtime_quit_outcome
        ])
        .setup(|app| {
            // 先装生命周期状态：`spawn` 起的监督循环与窗口事件都可能立刻用到它。
            runtime_lifecycle::install(app.handle());
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

    // Two policies run on every platform:
    //
    // * Closing the main window hides it instead of quitting, so the app keeps
    //   running in the Dock like every other Mac app. `Reopen` (the Dock click)
    //   only exists on macOS.
    // * A real quit (`Cmd+Q`, Dock → Quit, or the framework's own exit) first
    //   releases every project's runtime lease, in parallel, under one budget.
    //   The drain is platform-independent: other platforms exit for real.
    app.run(|app, event| {
        runtime_lifecycle::handle_run_event(app, &event);
        #[cfg(target_os = "macos")]
        window::handle_run_event(app, &event);
    });
}
