//! BEL 通知后端。

use std::io::{self, stdout};

use crossterm::Command;

/// BEL 是单字节，不带文案——终端的 BEL 通知无法承载正文。
pub const BEL_SEQUENCE: &str = "\u{7}";

#[derive(Debug, Clone)]
pub struct PostNotification;

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(BEL_SEQUENCE)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        Err(io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[derive(Debug, Default, Clone)]
pub struct BelBackend;

impl BelBackend {
    pub fn new() -> Self {
        Self
    }

    /// 忽略文案：BEL 协议本身不携带消息体。签名与 `Osc9Backend::notify`
    /// 保持一致，好让 `mod.rs` 用同一处代码分派。
    pub fn notify(&mut self, _message: &str) -> io::Result<()> {
        crossterm::execute!(stdout(), PostNotification)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bel_is_a_single_byte() {
        assert_eq!(BEL_SEQUENCE, "\u{7}");
    }

    /// 断言 `Command` 真正写出的字节，而不只是常量。`notify` 走的是
    /// `crossterm::execute!` → `write_ansi` 这条链，只测常量的话把
    /// `write_ansi` 改成空实现仍然全绿，用户却收不到任何通知。
    #[test]
    fn the_command_writes_exactly_one_bel() {
        use crossterm::Command;
        let mut out = String::new();
        PostNotification.write_ansi(&mut out).unwrap();
        assert_eq!(out, "\u{7}");
        assert_eq!(out.as_bytes(), b"\x07");
    }
}
