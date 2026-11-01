//! OSC 9 通知后端（含 tmux / zellij DCS 透传）。

use std::io::{self, stdout};

use crossterm::Command;

/// 把通知文案编码为 OSC 9 序列。
///
/// `dcs_passthrough` 为真时（tmux / zellij 内）整段包进
/// `ESC P tmux; ... ESC \`，并把正文里的 ESC 翻倍——多路复用器会剥掉一层，
/// 不翻倍就会被 tmux 当成自己的控制序列吃掉。
pub fn encode(message: &str, dcs_passthrough: bool) -> String {
    if dcs_passthrough {
        let escaped = message.replace('\u{1b}', "\u{1b}\u{1b}");
        format!("\u{1b}Ptmux;\u{1b}\u{1b}]9;{escaped}\u{7}\u{1b}\\")
    } else {
        format!("\u{1b}]9;{message}\u{7}")
    }
}

/// 通过 crossterm 的 `Command` 写 stdout，测试可断言字节。
#[derive(Debug, Clone)]
pub struct PostNotification {
    pub message: String,
    pub dcs_passthrough: bool,
}

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(&encode(&self.message, self.dcs_passthrough))
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

/// OSC 9 后端。`dcs_passthrough` 在构造时定死，会话内不变。
#[derive(Debug, Clone)]
pub struct Osc9Backend {
    dcs_passthrough: bool,
}

impl Osc9Backend {
    pub fn new(dcs_passthrough: bool) -> Self {
        Self { dcs_passthrough }
    }

    pub fn notify(&mut self, message: &str) -> io::Result<()> {
        crossterm::execute!(
            stdout(),
            PostNotification {
                message: message.to_string(),
                dcs_passthrough: self.dcs_passthrough,
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_encodes_osc9() {
        assert_eq!(encode("hello", false), "\u{1b}]9;hello\u{7}");
    }

    #[test]
    fn tmux_wraps_in_dcs_passthrough() {
        assert_eq!(
            encode("done", true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;done\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn tmux_doubles_esc_inside_payload() {
        assert_eq!(
            encode("danger\u{1b}[31m", true),
            "\u{1b}Ptmux;\u{1b}\u{1b}]9;danger\u{1b}\u{1b}[31m\u{7}\u{1b}\\"
        );
    }

    #[test]
    fn plain_path_never_emits_raw_esc_from_message() {
        // Codex 的 bug：非 tmux 路径不翻倍 ESC。我们靠调用方先 sanitize，
        // 这里断言 encode 本身对已消毒输入不再引入 ESC。
        let sanitized = crate::tui::notify::text::sanitize("a\u{1b}]0;x\u{7}");
        let out = encode(&sanitized, false);
        assert_eq!(out.matches('\u{1b}').count(), 1, "out: {out:?}");
    }
}
