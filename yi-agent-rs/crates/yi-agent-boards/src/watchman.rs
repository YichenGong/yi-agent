//! 把常驻值守装成 macOS LaunchAgent：登录自启 + 崩溃自动拉起。
//!
//! 装的是通用子命令 `yi-agent boards watch`，它只读通用常驻登记、不认识看板。
//! launchctl 调用经 `_with` 变体注入，测试不需要真的动 launchd。

use std::path::{Path, PathBuf};

pub const PREFERENCE_KEY: &str = "board_watchman_enabled";
pub const LABEL: &str = "ai.yi-agent.board-watchman";

pub fn plist_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

/// 生成 plist。日志落到 `~/.yi-agent/logs/board-watchman.log`，便于排障。
pub fn plist_contents(exe: &Path, home: &Path) -> String {
    let log = home.join(".yi-agent").join("logs").join("board-watchman.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>boards</string>
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

pub fn install(exe: &Path, home: &Path) -> Result<(), String> {
    install_with(exe, home, &mut default_launchctl)
}

pub fn install_with(
    exe: &Path,
    home: &Path,
    launchctl: &mut dyn FnMut(&[String]) -> Result<(), String>,
) -> Result<(), String> {
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
    fn the_plist_keeps_it_alive_and_runs_boards_watch() {
        let exe = PathBuf::from("/usr/local/bin/yi-agent");
        let home = PathBuf::from("/Users/tester");
        let plist = plist_contents(&exe, &home);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains(LABEL));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("/usr/local/bin/yi-agent"));
        assert!(plist.contains("boards"));
        assert!(plist.contains("watch"));
    }

    #[test]
    fn install_writes_the_plist_and_bootstraps_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let mut calls: Vec<Vec<String>> = Vec::new();
        install_with(
            &PathBuf::from("/usr/local/bin/yi-agent"),
            home,
            &mut |args| { calls.push(args.to_vec()); Ok(()) },
        )
        .unwrap();
        assert!(plist_path(home).exists(), "the plist must be on disk");
        assert_eq!(calls.len(), 1, "bootstrap is invoked once");
        assert_eq!(calls[0][0], "bootstrap");
        assert_eq!(calls[0][1], format!("gui/{}", unsafe { libc::getuid() }));
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
        uninstall_with(home, &mut |args| { calls.push(args.to_vec()); Ok(()) }).unwrap();
        assert_eq!(calls[0][0], "bootout");
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
