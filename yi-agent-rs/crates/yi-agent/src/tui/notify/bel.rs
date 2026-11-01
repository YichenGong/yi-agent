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
}
