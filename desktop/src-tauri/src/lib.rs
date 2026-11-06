#[cfg(desktop)]
mod bridge;
#[cfg(desktop)]
mod foreground_root;
// 正式容器与探针共用这一个入口：容器本身始终编译，`probe`/`run` 子模块
// 由 `plugin-probe` feature 单独门控（见 `plugin_host/mod.rs`）。
#[cfg(desktop)]
pub mod plugin_host;
#[cfg(desktop)]
mod remote;
/// 桌面壳持有的 runtime 租约、退出屏障。
///
/// 必须由壳持有而不是 sidecar：App 的身份不能随 app-server 重启改变，残留的
/// sidecar 也不能替一个已经退出的 App 续租。
#[cfg(desktop)]
pub mod runtime_lifecycle;
pub mod runtime_management;
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
    // 全进程只展开一次 `generate_context!()`：探针与正式启动共用同一份
    // context，否则两份 `_EMBED_INFO_PLIST` 静态符号会在链接期撞车。
    let context = tauri::generate_context!();

    // 探针构建在创建任何窗口之前接管整个进程：它需要清空窗口配置、走自己的
    // 资源协议，且绝不能顺手拉起正式 sidecar（那会污染开发者的真实实例）。
    #[cfg(all(desktop, feature = "plugin-probe"))]
    if let Some(parsed) = plugin_host::run::ProbeOptions::from_args(std::env::args()) {
        let code = match parsed {
            Ok(options) => plugin_host::run::run_probe(context, options),
            Err(error) => {
                eprintln!("[plugin-probe] {error}");
                2
            }
        };
        std::process::exit(code);
    }

    // 必须在任何开关判定之前。OnceLock：重复调用无副作用。
    #[cfg(desktop)]
    yi_agent_flags::set_build_info(BUILD_KIND, WRAPPER_VERSION);

    let builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init());

    // 桌面端自带 app-server 侧车（经 stdio 驱动）；iOS 是纯 WebSocket 客户端，
    // **没有侧车**（见 `desktop/src/transportFactory.ts`）。故侧车管理只在桌面注册，
    // 否则 iOS 构建会去找一个它用不上的 externalBin（并试图 spawn 它）。
    // 宿主状态先拿到手：`manage` 之后从 Builder 上取不回来，而注册协议需要
    // 同一个实例。
    #[cfg(desktop)]
    let plugin_host_state = plugin_host::host_state();

    #[cfg(desktop)]
    let builder = builder
        .plugin(tauri_plugin_shell::init())
        .manage(bridge::Sidecar::new())
        // 宿主状态与资源协议都要在 Builder 阶段就位：协议注册是 Builder
        // 方法，且窗口创建后禁止再注册。
        .manage(std::sync::Arc::clone(&plugin_host_state))
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
            runtime_lifecycle::runtime_quit_outcome,
            runtime_lifecycle::runtime_status,
            runtime_lifecycle::runtime_preview,
            runtime_lifecycle::runtime_confirm,
            runtime_lifecycle::runtime_operation,
            runtime_lifecycle::runtime_active_operation,
            runtime_lifecycle::runtime_known_scopes,
            runtime_lifecycle::runtime_scope_for_project,
            runtime_lifecycle::runtime_plugin_demand_acquire,
            runtime_lifecycle::runtime_plugin_demand_release,
            runtime_lifecycle::runtime_migration_preview,
            runtime_lifecycle::runtime_migration_confirm,
            runtime_lifecycle::runtime_migration_operation,
            runtime_lifecycle::runtime_active_migration,
            foreground_root::foreground_root_recovery_inspect,
            foreground_root::foreground_root_recovery_prepare,
            foreground_root::foreground_root_recovery_confirm,
            foreground_root::foreground_root_recovery_operation,
            plugin_host::host::open_plugin_view,
            plugin_host::host::close_plugin_view,
            plugin_host::host::plugin_committed_activation,
            bridge::plugin_host_identity,
            bridge::plugin_bind_host
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

    #[cfg(desktop)]
    let builder = plugin_host::host::register_protocol(builder, plugin_host_state);

    let app = builder
        .build(context)
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
