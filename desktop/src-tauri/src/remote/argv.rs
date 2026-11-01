//! 把 profile 变成一条 ssh 命令行。
//!
//! **这是全项目唯一拼 shell 的地方**（spec §8.6）。因此本文件必须保持纯函数：
//! 不读环境、不读文件、不起进程，输入的每个字段都已校验过，输出可以直接断言。
//!
//! 两件容易混淆的事，在下面的 `format!` 里是分开的两段：
//! - `cd '<remote_path>'` 决定**远端 app-server 的 cwd**（项目目录）
//! - `env YI_AGENT_HOME='<instance_root>'` 决定**状态目录**
//!
//! 二者不可互相替代：`instance_home()`（`store/src/paths.rs:20-23`）只看
//! `YI_AGENT_HOME` 与 `$HOME`，**完全不看 cwd**。只 `cd` 不设 env 会让所有远端
//! 项目共用一个状态目录，静默互相覆盖（spec §8.5）。

use super::profile::{ProfileError, RemoteProfile};

/// shell 单引号转义：`it's` → `it'\''s`，再由调用方整体包一层单引号。
fn sh_quote(raw: &str) -> String {
    format!("'{}'", raw.replace('\'', r"'\''"))
}

/// 远端路径的字符白名单。单引号已由 `sh_quote` 处理，这里拒绝的是
/// "即便转义了也不该出现"的东西——分号、反引号、`$()`、管道、换行。
fn path_is_safe(raw: &str) -> bool {
    if !raw.starts_with('/') {
        return false;
    }
    !raw.chars().any(|c| {
        matches!(
            c,
            ';' | '|' | '&' | '$' | '`' | '(' | ')' | '<' | '>' | '\n' | '\r' | '\\'
        )
    })
}

/// `user@host` 或 `host`，字符集与 spec §4.9 一致。
fn host_is_safe(raw: &str) -> bool {
    if raw.is_empty() || raw.starts_with('-') {
        // 前导 `-` 会被 ssh 当成选项（`-oProxyCommand=...`）
        return false;
    }
    raw.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
}

/// 拼出**传给 `ssh` 的参数**（不含 `ssh` 本身）。
///
/// 最后一个是完整的远端命令串：ssh 会把 host 之后的所有 argv 用空格拼起来交给
/// 远端 shell，所以这里刻意只放**一个**元素，避免参数被重复分词。
pub fn remote_argv(
    profile: &RemoteProfile,
    binary: &str,
    relay: Option<&str>,
) -> Result<Vec<String>, ProfileError> {
    if !host_is_safe(&profile.host) {
        return Err(ProfileError::BadHost(profile.host.clone()));
    }
    if !path_is_safe(&profile.remote_path) {
        return Err(ProfileError::BadRemotePath(profile.remote_path.clone()));
    }
    if !path_is_safe(&profile.instance_root) {
        return Err(ProfileError::BadRemotePath(profile.instance_root.clone()));
    }
    if !binary.starts_with('/') || !path_is_safe(binary) {
        return Err(ProfileError::NotAbsolute(binary.to_string()));
    }

    let mut command = format!(
        "cd {} && exec env YI_AGENT_HOME={} {} app-server --listen stdio://",
        sh_quote(&profile.remote_path),
        sh_quote(&profile.instance_root),
        sh_quote(binary),
    );
    if let Some(url) = relay {
        command.push_str(&format!(" --relay {}", sh_quote(url)));
    }

    Ok(vec![
        // 非交互：不要弹密码提示，失败就退出（否则 Tauri 侧会挂在一个永远等不到
        // 输入的 ssh 上，表现为"连接中"永不结束）。
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        // 连接超时，避免不可达主机把监督循环占住。
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        profile.host.clone(),
        command,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::profile::RemoteProfile;
    use std::path::PathBuf;

    fn sample() -> RemoteProfile {
        RemoteProfile {
            id: "proj-foo".into(),
            label: "foo".into(),
            host: "dev@10.0.0.5".into(),
            local_path: PathBuf::from("/Users/y/proj/foo"),
            remote_path: "/home/dev/proj/foo".into(),
            instance_root: "/home/dev/.yi-agent-projects/proj-foo/.yi-agent".into(),
            binary_version: "0.1.10".into(),
            remote_arch: "x86_64-unknown-linux-gnu".into(),
        }
    }

    #[test]
    fn argv_starts_with_host_then_one_remote_command() {
        let argv = remote_argv(&sample(), "/home/dev/.yi-agent/bin/0.1.10/yi-agent", None).unwrap();
        assert!(
            argv.len() >= 3,
            "ssh needs options, a host, and one remote command: {argv:?}"
        );
        let last = argv.last().unwrap();
        assert!(last.contains("app-server"), "remote command: {last}");
    }

    #[test]
    fn argv_sets_home_env_and_cd_separately() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        let cmd = argv.last().unwrap();
        assert!(cmd.contains("cd '/home/dev/proj/foo'"), "cwd via cd: {cmd}");
        assert!(
            cmd.contains("YI_AGENT_HOME='/home/dev/.yi-agent-projects/proj-foo/.yi-agent'"),
            "state dir via env, NOT via cwd: {cmd}"
        );
        assert!(
            cmd.contains("exec "),
            "exec so signals reach app-server: {cmd}"
        );
    }

    #[test]
    fn argv_sets_batch_mode_to_fail_fast() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes"),
            "a non-interactive ssh must not hang waiting for a password prompt: {argv:?}"
        );
    }

    /// 判据 34：远端进程只能以 `stdio://` 监听。
    ///
    /// 一旦有人把这里改成 `ws://`/`tcp://`，远端就自己开了一个端口——它既绕过
    /// 了 ssh 的传输加密，也没做任何鉴权。所以这里断言「只能是 stdio」，
    /// 而不只是「有 --listen」。
    #[test]
    fn listen_is_always_stdio() {
        for relay in [None, Some("wss://relay.example/ws?session=s1")] {
            let argv = remote_argv(&sample(), "/b/yi-agent", relay).unwrap();
            let command = argv.last().unwrap();
            assert!(command.contains("--listen stdio://"), "{command}");
            assert!(!command.contains("--listen ws://"), "{command}");
            assert!(!command.contains("--listen tcp://"), "{command}");
            assert!(!command.contains("--listen relay://"), "{command}");
        }
    }

    #[test]
    fn argv_appends_relay_after_listen() {
        let argv =
            remote_argv(&sample(), "/b/yi-agent", Some("wss://r/connect?session=x")).unwrap();
        let cmd = argv.last().unwrap();
        let listen = cmd.find("--listen stdio://").expect("listen present");
        let relay = cmd.find("--relay").expect("relay present");
        assert!(relay > listen, "relay must come after listen: {cmd}");
        assert!(
            cmd.contains("'wss://r/connect?session=x'"),
            "relay quoted: {cmd}"
        );
    }

    #[test]
    fn argv_omits_relay_when_none() {
        let argv = remote_argv(&sample(), "/b/yi-agent", None).unwrap();
        assert!(!argv.last().unwrap().contains("--relay"));
    }

    #[test]
    fn single_quotes_in_paths_are_escaped() {
        let mut p = sample();
        p.remote_path = "/home/dev/it's here".into();
        let argv = remote_argv(&p, "/b/yi-agent", None).unwrap();
        let cmd = argv.last().unwrap();
        assert!(cmd.contains(r"'/home/dev/it'\''s here'"), "escaped: {cmd}");
        assert!(
            !cmd.contains("it's here'"),
            "must not leak a bare quote: {cmd}"
        );
    }

    /// spec §9 判据 4 按这个名字找测试，别改名。
    #[test]
    fn a_host_with_a_semicolon_is_rejected() {
        let mut p = sample();
        p.host = "dev@10.0.0.5;id".into();
        assert!(
            remote_argv(&p, "/b/yi-agent", None).is_err(),
            "a semicolon would break out of the ssh argument"
        );
    }

    #[test]
    fn shell_metacharacters_in_remote_path_are_rejected() {
        for bad in [
            "/home/dev/a;id",
            "/home/dev/$(id)",
            "/home/dev/a`id`",
            "/home/dev/a|b",
        ] {
            let mut p = sample();
            p.remote_path = bad.into();
            assert!(
                remote_argv(&p, "/b/yi-agent", None).is_err(),
                "`{bad}` must be rejected"
            );
        }
    }

    #[test]
    fn relative_remote_path_is_rejected() {
        let mut p = sample();
        p.remote_path = "proj/foo".into();
        assert!(remote_argv(&p, "/b/yi-agent", None).is_err());
    }

    #[test]
    fn bad_host_is_rejected() {
        for bad in ["dev@10.0.0.5;id", "dev host", "-oProxyCommand=evil"] {
            let mut p = sample();
            p.host = bad.into();
            assert!(
                remote_argv(&p, "/b/yi-agent", None).is_err(),
                "`{bad}` must be rejected"
            );
        }
    }
}
