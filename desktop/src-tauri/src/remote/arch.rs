//! 远端架构 → 受支持的预编译 target。
//!
//! 发布矩阵只有三个 target（spec §2.4.5）。**不支持时必须显式拒绝**，且文案要
//! 指向真实原因——「该架构没有预编译产物」，而不是含糊的「下载失败」。用户看到
//! 「下载失败」会去查网络，查一天也查不出来。

use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum ArchError {
    /// 能解析，但不在发布矩阵里。
    Unsupported { os: String, machine: String },
    /// `uname -sm` 的输出看不出来是什么。
    Unparsable(String),
}

impl fmt::Display for ArchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchError::Unsupported { os, machine } => write!(
                f,
                "remote architecture `{os} {machine}` has no prebuilt yi-agent binary \
                 (only aarch64-apple-darwin, x86_64-apple-darwin and \
                 x86_64-unknown-linux-gnu are published). Run the project on a supported \
                 host, or install a binary you built yourself with `--from <path>`."
            ),
            ArchError::Unparsable(raw) => {
                write!(f, "could not parse `uname -sm` output: `{raw}`")
            }
        }
    }
}

/// `uname -sm` 的两段（OS、machine）→ Rust target triple。
///
/// macOS 的机器名是 `arm64` 而 Rust 叫 `aarch64`，这处不一致是最常见的错。
pub fn normalize_target(uname_sm: &str) -> Result<&'static str, ArchError> {
    let mut parts = uname_sm.split_whitespace();
    let (Some(os), Some(machine)) = (parts.next(), parts.next()) else {
        return Err(ArchError::Unparsable(uname_sm.trim().to_string()));
    };
    if parts.next().is_some() {
        // `uname -sm` 只应有两段；多出来说明调用错了（比如传了 `uname -a`）。
        return Err(ArchError::Unparsable(uname_sm.trim().to_string()));
    }
    let os = os.to_ascii_lowercase();
    let machine = machine.to_ascii_lowercase();
    match (os.as_str(), machine.as_str()) {
        ("linux", "x86_64") | ("linux", "amd64") => Ok("x86_64-unknown-linux-gnu"),
        ("darwin", "arm64") | ("darwin", "aarch64") => Ok("aarch64-apple-darwin"),
        ("darwin", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => Err(ArchError::Unsupported {
            os: os.to_string(),
            machine: machine.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_x86_64_maps_to_the_gnu_target() {
        assert_eq!(
            normalize_target("Linux x86_64").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
    }

    /// macOS 的 `uname -m` 是 `arm64`，不是 Rust target 里的 `aarch64`。
    #[test]
    fn macos_arm64_maps_to_aarch64_darwin() {
        assert_eq!(
            normalize_target("Darwin arm64").unwrap(),
            "aarch64-apple-darwin"
        );
    }

    #[test]
    fn macos_x86_64_maps_to_x86_64_darwin() {
        assert_eq!(
            normalize_target("Darwin x86_64").unwrap(),
            "x86_64-apple-darwin"
        );
    }

    /// 不支持的架构必须给出**可执行**的出路，且不能伪装成网络问题。
    #[test]
    fn arm_linux_is_rejected_with_an_actionable_message() {
        let err = normalize_target("Linux aarch64").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("aarch64"), "must name the arch: {msg}");
        assert!(
            msg.contains("--from") || msg.contains("x86_64"),
            "must offer a way out: {msg}"
        );
        assert!(
            !msg.contains("network") && !msg.contains("网络"),
            "must NOT look like a network problem: {msg}"
        );
    }

    #[test]
    fn unsupported_platforms_are_rejected() {
        assert!(normalize_target("Linux x86_64").is_ok());
        assert!(normalize_target("Windows x86_64").is_err());
        assert!(normalize_target("FreeBSD amd64").is_err());
    }

    #[test]
    fn garbage_is_unparsable_not_unsupported() {
        assert!(matches!(
            normalize_target(""),
            Err(ArchError::Unparsable(_))
        ));
        assert!(matches!(
            normalize_target("Linux"),
            Err(ArchError::Unparsable(_))
        ));
        assert!(
            matches!(
                normalize_target("Linux x86_64 extra"),
                Err(ArchError::Unparsable(_))
            ),
            "extra fields mean the caller ran uname -a, not uname -sm"
        );
    }

    #[test]
    fn case_and_whitespace_do_not_matter() {
        assert_eq!(
            normalize_target("  LINUX   AMD64  ").unwrap(),
            "x86_64-unknown-linux-gnu"
        );
    }
}
