use super::*;
use std::sync::Mutex;

/// 假安装闭包记下的调用：`(exe, home)`，按发生顺序。
type InstallCalls = Arc<Mutex<Vec<(PathBuf, PathBuf)>>>;

/// 记录调用的假安装闭包；不碰真实 launchd。
fn recorder() -> (WatchmanInstall, InstallCalls) {
    let calls: InstallCalls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let install: WatchmanInstall = Arc::new(move |exe: &Path, home: &Path| {
        seen.lock()
            .unwrap()
            .push((exe.to_path_buf(), home.to_path_buf()));
        Ok(())
    });
    (install, calls)
}

fn write_switch(workdir: &Path, enabled: bool) {
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        crate::settings_store::preferences_path(workdir),
        format!(r#"{{"{}":{enabled}}}"#, crate::settings_store::WATCHMAN_KEY),
    )
    .unwrap();
}

/// 开关关着就什么都不做：用户明确关掉的登录自启不得被安装路径悄悄打开。
#[test]
fn does_nothing_while_the_switch_is_off() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    write_switch(dir.path(), false);
    let (install, calls) = recorder();

    ensure_watchman_installed(dir.path(), home.path(), &install);

    assert!(calls.lock().unwrap().is_empty());
}

/// 开关开着、plist 缺失 → 装。这是 `board/create` 时代那段行为的通用落点：
/// 全新安装后用户应当得到登录自启。
#[test]
fn installs_when_the_switch_is_on_and_no_plist_points_at_this_exe() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    write_switch(dir.path(), true);
    let (install, calls) = recorder();

    ensure_watchman_installed(dir.path(), home.path(), &install);

    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// 已经指向当前 exe 就不再装：重复 bootstrap 会打断正在跑的值守。
#[test]
fn does_not_reinstall_when_the_plist_already_points_at_this_exe() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    write_switch(dir.path(), true);
    let exe = std::env::current_exe().unwrap();
    let plist = yi_agent_supervisors::watchman::plist_path(home.path());
    std::fs::create_dir_all(plist.parent().unwrap()).unwrap();
    std::fs::write(
        &plist,
        yi_agent_supervisors::watchman::plist_contents(&exe, home.path()),
    )
    .unwrap();
    let (install, calls) = recorder();

    ensure_watchman_installed(dir.path(), home.path(), &install);

    assert!(calls.lock().unwrap().is_empty());
}

/// 装失败不 panic、不外抛：best-effort 的语义就是「记一笔，继续」。
#[test]
fn a_failing_install_is_swallowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    write_switch(dir.path(), true);
    let install: WatchmanInstall =
        Arc::new(|_exe: &Path, _home: &Path| Err("launchctl bootstrap failed".to_string()));

    ensure_watchman_installed(dir.path(), home.path(), &install);
}
