//! 探测远端能不能装、怎么解压。
//!
//! **一次 ssh 往返**问完所有事：每次往返几百毫秒到几秒，串行问三次会让「首次
//! 连接」多等好几秒。所以 [`probe_command`] 刻意拼成一条命令。
//!
//! 输出用 `YI_` 前缀标记，解析时**按标记找、不按行号**：远端 shell 的
//! motd / 升级提示会混进 stdout，按行号解析在别人机器上必定错位。

use crate::remote::arch::normalize_target;

#[derive(Debug, PartialEq, Eq)]
pub struct RemoteCaps {
    pub target: &'static str,
    /// `tar -xzf` 能不能用，即「有 tar 且 tar 能找到 gzip」。
    pub tar_supports_gzip: bool,
    pub has_tar: bool,
}

/// 一次 ssh 跑完：先 uname，再试探 tar 的 gzip 能力。
///
/// 输出约定（每行一个标记）：
/// - `YI_UNAME=<uname -sm>`
/// - `YI_TAR=gzip` / `YI_TAR=plain` / `YI_TAR=none`
///
/// 「gzip 能力」只能试不能查：GNU tar 的 `-z` 要 fork 外部 `gzip`，极简镜像里
/// `tar --version` 看着正常但 `-xzf` 会失败（spec §2.4.2）。
pub fn probe_command() -> &'static str {
    concat!(
        "echo YI_UNAME=$(uname -sm); ",
        "if ! command -v tar >/dev/null 2>&1; then echo YI_TAR=none; ",
        "elif tar --help 2>&1 | grep -qi gzip; then echo YI_TAR=gzip; ",
        "else echo YI_TAR=plain; fi"
    )
}

/// 远端 home 的绝对路径。
///
/// `~` 在非交互 ssh 里通常可用但不保证；`$HOME` 更可靠，两者都试一遍。
#[allow(dead_code)]
pub fn expand_home_probe_command() -> &'static str {
    r#"printf '%s\n' "${HOME:-$(cd ~ && pwd)}""#
}

fn tagged<'a>(output: &'a str, tag: &str) -> Option<&'a str> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix(tag))
        .map(str::trim)
}

/// 解析探测输出。
///
/// 不支持的架构在**这里**就失败（spec §2.4.5）：等下载 404 才报错，用户看到
/// 的是「网络失败」，而真正的原因是这台机器根本没有预编译产物。
pub fn parse_caps(uname_out: &str, tar_probe_out: &str) -> Result<RemoteCaps, String> {
    let uname = tagged(uname_out, "YI_UNAME=")
        .ok_or_else(|| format!("the remote probe printed no YI_UNAME line:\n{uname_out}"))?;
    let target = normalize_target(uname).map_err(|error| error.to_string())?;

    let tar = tagged(tar_probe_out, "YI_TAR=")
        .ok_or_else(|| format!("the remote probe printed no YI_TAR line:\n{tar_probe_out}"))?;
    let (has_tar, tar_supports_gzip) = match tar {
        "gzip" => (true, true),
        "plain" => (true, false),
        "none" => (false, false),
        other => return Err(format!("unexpected YI_TAR value `{other}`")),
    };
    Ok(RemoteCaps {
        target,
        tar_supports_gzip,
        has_tar,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_capable_tar_is_detected() {
        let caps = parse_caps("YI_UNAME=Linux x86_64\n", "YI_TAR=gzip\n").unwrap();
        assert_eq!(caps.target, "x86_64-unknown-linux-gnu");
        assert!(caps.tar_supports_gzip);
        assert!(caps.has_tar);
    }

    #[test]
    fn plain_tar_is_detected() {
        let caps = parse_caps("YI_UNAME=Linux x86_64\n", "YI_TAR=plain\n").unwrap();
        assert!(!caps.tar_supports_gzip);
        assert!(caps.has_tar, "plain tar still exists");
    }

    #[test]
    fn missing_tar_is_detected() {
        let caps = parse_caps("YI_UNAME=Linux x86_64\n", "YI_TAR=none\n").unwrap();
        assert!(!caps.has_tar);
        assert!(!caps.tar_supports_gzip);
    }

    /// 远端 motd / 警告会混进输出：解析必须按标记找。
    #[test]
    fn parsing_ignores_noise_lines() {
        let caps = parse_caps(
            "warning: something happened\nYI_UNAME=Linux x86_64\n",
            "noise\nYI_TAR=plain\n",
        )
        .unwrap();
        assert_eq!(caps.target, "x86_64-unknown-linux-gnu");
        assert!(!caps.tar_supports_gzip);
    }

    #[test]
    fn unsupported_arch_fails_at_probe_time() {
        // spec §2.4.5：连接前就失败，不能先 scp 再发现不对。
        let err = parse_caps("YI_UNAME=Linux aarch64\n", "YI_TAR=gzip\n").unwrap_err();
        assert!(err.contains("aarch64"), "{err}");
    }

    #[test]
    fn a_missing_marker_is_an_error_not_a_default() {
        let err = parse_caps("Linux x86_64 without a tag\n", "YI_TAR=gzip\n").unwrap_err();
        assert!(err.contains("YI_UNAME"), "{err}");
        let err = parse_caps("YI_UNAME=Linux x86_64\n", "gzip without a tag\n").unwrap_err();
        assert!(err.contains("YI_TAR"), "{err}");
    }

    #[test]
    fn an_unknown_tar_marker_is_refused() {
        let err = parse_caps("YI_UNAME=Linux x86_64\n", "YI_TAR=maybe\n").unwrap_err();
        assert!(err.contains("maybe"), "{err}");
    }

    #[test]
    fn the_probe_command_asks_uname_and_tar_capability() {
        let command = probe_command();
        assert!(command.contains("uname -sm"), "{command}");
        assert!(command.contains("tar"), "{command}");
        assert!(command.contains("gzip"), "{command}");
        // 一次 ssh 往返：命令本身不能再嵌套 ssh。
        assert!(!command.contains("ssh "), "{command}");
        // 换行会让 ssh 后面的参数被远端 shell 当成多条命令，行为不可控。
        assert!(!command.contains('\n'), "{command}");
    }

    #[test]
    fn the_home_probe_does_not_assume_home_is_set() {
        let command = expand_home_probe_command();
        assert!(command.contains("HOME"), "{command}");
        assert!(command.contains('~'), "{command}");
        assert!(!command.contains('\n'), "{command}");
    }
}
