//! 把常驻值守装成 macOS LaunchAgent：登录自启 + 崩溃自动拉起。
//!
//! 装的是**通用**子命令 `yi-agent resident watch`，它只读通用常驻登记、
//! 不认识任何插件。launchctl 调用经 `_with` 变体注入，测试不需要真的动 launchd。

use std::path::{Path, PathBuf};

/// 值守开关的偏好键。**这是磁盘契约**（写在 `preferences.json` 里），
/// 通用化改名连同旧键兼容读由 Task 6 落地，本任务只动 plist。
pub const PREFERENCE_KEY: &str = "board_watchman_enabled";
pub const LABEL: &str = "ai.yi-agent.resident-daemon-watchman";

/// 通用化之前的标签。*过渡期*：安装时顺手清掉它，否则一台已经装过旧 plist 的
/// 机器上会同时跑两个值守（旧 plist 走的 `boards watch` 仍被 CLI 当别名接受）。
/// Task 6 起旧 plist 的兼容窗口关闭后，这个常量可删。
pub const LEGACY_LABEL: &str = "ai.yi-agent.board-watchman";

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// 通用化之前的 plist 路径。只在安装/卸载时用于清理。
fn legacy_plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LEGACY_LABEL}.plist"))
}

/// 生成 plist。日志落到 `~/.yi-agent/logs/resident-daemon-watchman.log`，便于排障。
pub fn plist_contents(exe: &Path, home: &Path) -> String {
    let log = home
        .join(".yi-agent")
        .join("logs")
        .join("resident-daemon-watchman.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>resident</string>
    <string>watch</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = exe.display(),
        log = log.display(),
    )
}

fn write_plist(home: &Path, contents: &str) -> Result<(), String> {
    let path = plist_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(&path, contents).map_err(|error| error.to_string())
}

/// 是否已装且指向**当前**可执行文件。路径变了要重装（升级后 exe 可能换位置）。
pub fn is_installed(home: &Path) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    is_installed_for(home, &exe)
}

pub fn is_installed_for(home: &Path, exe: &Path) -> bool {
    std::fs::read_to_string(plist_path(home))
        .map(|text| text.contains(&exe.display().to_string()))
        .unwrap_or(false)
}

fn default_launchctl(args: &[String]) -> Result<(), String> {
    let status = std::process::Command::new("launchctl")
        .args(args)
        .status()
        .map_err(|error| format!("could not run launchctl: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("launchctl {} failed: {status}", args.join(" ")))
    }
}

fn uid_domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

/// 清掉通用化之前装的那个 plist（best effort）。
///
/// 关键在**不留下两个值守**：旧 plist 跑的是 `boards watch`，而 CLI 在过渡期
/// 仍把它当 `resident watch` 的别名接受，于是两台「常驻值守」会各跑各的循环。
/// bootout 失败（本来就没加载）不算错；目标是「最终没有它」。
fn remove_legacy_plist(home: &Path, launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>) {
    let _ = launchctl(&[
        "bootout".into(),
        format!("{}/{}", uid_domain(), LEGACY_LABEL),
    ]);
    let _ = std::fs::remove_file(legacy_plist_path(home));
}

pub fn install(exe: &Path, home: &Path) -> Result<(), String> {
    install_with(exe, home, &mut default_launchctl)
}

pub fn install_with(
    exe: &Path,
    home: &Path,
    launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    // 先清旧、再装新：中间态只能是「没有值守」，不能是「两个值守」。
    remove_legacy_plist(home, launchctl);
    write_plist(home, &plist_contents(exe, home))?;
    // bootstrap 是唯一一次调用；若服务已加载（重复安装），launchctl 会报
    // already bootstrapped，此时目标已达成，按成功处理而不是先 bootout
    // （否则每次安装都会多一次调用，重装也可能中断正在跑的值守）。
    match launchctl(&[
        "bootstrap".into(),
        uid_domain(),
        plist_path(home).display().to_string(),
    ]) {
        Ok(()) => Ok(()),
        Err(error) if is_already_loaded(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_already_loaded(error: &str) -> bool {
    let lowered = error.to_ascii_lowercase();
    lowered.contains("already bootstrapped") || lowered.contains("service already loaded")
}

pub fn uninstall(home: &Path) -> Result<(), String> {
    uninstall_with(home, &mut default_launchctl)
}

pub fn uninstall_with(
    home: &Path,
    launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
    // bootout 失败（没加载过）不算错；目标是「最终没有它」。
    let _ = launchctl(&["bootout".into(), format!("{}/{}", uid_domain(), LABEL)]);
    // 旧标签也一并清掉：卸载只删新 plist 会把过渡期的旧值守留在机器上。
    remove_legacy_plist(home, launchctl);
    match std::fs::remove_file(plist_path(home)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_watchman_plist_is_generic_not_board_specific() {
        let contents = plist_contents(Path::new("/bin/yi-agent"), Path::new("/home/u"));
        assert!(contents.contains("resident-daemon-watchman"));
        assert!(
            !contents.to_lowercase().contains("board"),
            "the plist must name no plugin: {contents}"
        );
        // 跑的必须是**通用**子命令：plist 指向一个不存在的命令等于值守静默失效。
        assert!(contents.contains("<string>resident</string>"), "{contents}");
        assert!(contents.contains("<string>watch</string>"), "{contents}");
    }

    #[test]
    fn the_plist_keeps_it_alive_and_runs_the_generic_watch() {
        let exe = PathBuf::from("/usr/local/bin/yi-agent");
        let home = PathBuf::from("/Users/tester");
        let plist = plist_contents(&exe, &home);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(LABEL));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("/usr/local/bin/yi-agent"));
        assert!(plist.contains("<string>resident</string>"));
        assert!(plist.contains("<string>watch</string>"));
    }

    /// The log path is what a human reads when the watchman misbehaves; a
    /// plugin-named file would put a plugin's name in the generic crate.
    #[test]
    fn the_plist_log_path_is_generic() {
        let plist = plist_contents(Path::new("/bin/yi-agent"), Path::new("/home/u"));
        assert!(
            plist.contains("/home/u/.yi-agent/logs/resident-daemon-watchman.log"),
            "{plist}"
        );
    }

    /// Installing over a machine that still has the pre-generic plist must not
    /// leave a SECOND watchman running: the legacy plist invokes the legacy
    /// subcommand, which the CLI still accepts as an alias, so without removing
    /// it two watchmen would both drive the same loop forever.
    #[test]
    fn installing_removes_the_legacy_plist_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let legacy = home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{LEGACY_LABEL}.plist"));
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "the pre-generic plist").unwrap();

        let mut calls: Vec<Vec<String>> = Vec::new();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |args| {
                calls.push(args.to_vec());
                Ok(())
            },
        )
        .unwrap();

        assert!(!legacy.exists(), "the legacy plist must be gone");
        assert!(
            calls
                .iter()
                .any(|args| args[0] == "bootout" && args[1].ends_with(LEGACY_LABEL)),
            "the legacy service must be booted out: {calls:?}"
        );
        assert!(plist_path(home).exists(), "the new plist must be installed");
    }

    /// A machine that never had the legacy plist must still install cleanly.
    #[test]
    fn installing_without_a_legacy_plist_is_still_a_success() {
        let dir = tempfile::tempdir().unwrap();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            dir.path(),
            &mut |_args| Ok(()),
        )
        .unwrap();
        assert!(plist_path(dir.path()).exists());
    }

    #[test]
    fn install_writes_the_plist_and_bootstraps_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut calls: Vec<Vec<String>> = Vec::new();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |args| {
                calls.push(args.to_vec());
                Ok(())
            },
        )
        .unwrap();
        assert!(plist_path(home).exists(), "the plist must be on disk");
        // 一次清旧（bootout）+ 一次启动（bootstrap）。顺序要紧：先清后启，
        // 中间态只允许「没有值守」。
        assert_eq!(calls.len(), 2, "legacy cleanup, then bootstrap: {calls:?}");
        assert_eq!(calls[0][0], "bootout");
        assert_eq!(calls[1][0], "bootstrap");
        assert_eq!(calls[1][1], format!("gui/{}", unsafe { libc::getuid() }));
    }

    #[test]
    fn uninstall_boots_out_and_removes_the_plist() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |_args| Ok(()),
        )
        .unwrap();
        let mut calls: Vec<Vec<String>> = Vec::new();
        uninstall_with(home, &mut |args| {
            calls.push(args.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(calls[0][0], "bootout");
        assert!(
            calls[0][1].ends_with(LABEL),
            "the generic label is booted out: {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|args| args[0] == "bootout" && args[1].ends_with(LEGACY_LABEL)),
            "the legacy service is booted out too: {calls:?}"
        );
        assert!(!plist_path(home).exists(), "the plist must be gone");
    }

    #[test]
    fn a_plist_pointing_at_a_different_exe_is_not_considered_installed() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        write_plist(home, &plist_contents(&PathBuf::from("/old/yi-agent"), home)).unwrap();
        assert!(!is_installed_for(home, &PathBuf::from("/new/yi-agent")));
        assert!(is_installed_for(home, &PathBuf::from("/old/yi-agent")));
    }
}
