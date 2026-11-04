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

/// A private instance directory per test: the retirement marker lives there, and
/// touching the developer's real one would retire their install.
fn instance() -> tempfile::TempDir {
    tempfile::TempDir::new().unwrap()
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
    let instance = instance();
    write_switch(dir.path(), false);
    let (install, calls) = recorder();

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());

    assert!(calls.lock().unwrap().is_empty());
}

/// 开关开着、plist 缺失 → 装。这是 `board/create` 时代那段行为的通用落点：
/// 全新安装后用户应当得到登录自启。
#[test]
fn installs_when_the_switch_is_on_and_no_plist_points_at_this_exe() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let instance = instance();
    write_switch(dir.path(), true);
    let (install, calls) = recorder();

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());

    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// 已经指向当前 exe 就不再装：重复 bootstrap 会打断正在跑的值守。
#[test]
fn does_not_reinstall_when_the_plist_already_points_at_this_exe() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let instance = instance();
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

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());

    assert!(calls.lock().unwrap().is_empty());
}

/// 装失败不 panic、不外抛：best-effort 的语义就是「记一笔，继续」。
#[test]
fn a_failing_install_is_swallowed() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let instance = instance();
    write_switch(dir.path(), true);
    let install: WatchmanInstall =
        Arc::new(|_exe: &Path, _home: &Path| Err("launchctl bootstrap failed".to_string()));

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());
}

/// 迁移到 managed 之后，装插件**不得**把旧值守装回来。
///
/// 这是「启动时卸载、装插件时又装回」的那一半：只改启动路径不改这里，用户的
/// 机器永远收敛不到 managed，而且每次装插件都会多一个无人值守的常驻进程。
#[test]
fn a_retired_instance_never_reinstalls_the_watchman() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let instance = instance();
    write_switch(dir.path(), true);
    yi_agent_supervisors::lifecycle::retirement::retire(
        instance.path(),
        "managed runtime took over",
    )
    .unwrap();
    let (install, calls) = recorder();

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());

    assert!(
        calls.lock().unwrap().is_empty(),
        "a retired instance must not get its self-healing login item back"
    );
}

/// 开关仍是「开」但实例已退休：退休优先，且**不改写**用户的偏好。
///
/// 偏好键是用户对后台值守的表态，迁移是既成事实；把偏好改成 false 会让用户
/// 在迁移回滚后失去自己原本的选择。
#[test]
fn retirement_does_not_rewrite_the_users_preference() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let instance = instance();
    write_switch(dir.path(), true);
    yi_agent_supervisors::lifecycle::retirement::retire(
        instance.path(),
        "managed runtime took over",
    )
    .unwrap();
    let (install, _calls) = recorder();

    ensure_watchman_installed_in(dir.path(), home.path(), &install, instance.path());

    assert!(
        crate::settings_store::load_watchman_enabled(dir.path()),
        "the user's preference is configuration, not a migration side effect"
    );
}
