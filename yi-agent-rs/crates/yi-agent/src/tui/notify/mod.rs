//! 终端通知：BEL / OSC 9。
//!
//! 分三层：`text` 做摘要与消毒（纯函数）、`bel`/`osc9` 做字节编码、
//! 本模块持有策略（后端选择、焦点条件、失败自禁）。

pub mod bel;
pub mod osc9;
pub mod text;

use std::collections::BTreeMap;
use std::io::IsTerminal;

use bel::BelBackend;
use osc9::Osc9Backend;

/// 通知协议。`Auto` 在构造时解析成具体后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Auto,
    Osc9,
    Bel,
}

impl Method {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "osc9" => Some(Self::Osc9),
            "bel" => Some(Self::Bel),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Osc9 => "osc9",
            Self::Bel => "bel",
        }
    }
}

/// 何时发送。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// 只在窗口失焦时发。
    Unfocused,
    /// 总是发。
    Always,
}

impl Condition {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "unfocused" => Some(Self::Unfocused),
            "always" => Some(Self::Always),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Unfocused => "unfocused",
            Self::Always => "always",
        }
    }
}

/// 注入式环境，测试传 `HashMap` 即可，不碰进程环境。
///
/// 用 `std::env::set_var` 写测试会污染并行测试，且 Rust 2024 里它已是
/// `unsafe`。注入式环境让全部识别逻辑可以在无终端环境下断言。
#[derive(Debug, Clone, Default)]
pub struct TerminalEnv(BTreeMap<String, String>);

impl TerminalEnv {
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    pub fn from_pairs(pairs: impl IntoIterator<Item = (&'static str, &'static str)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    fn var(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    fn non_empty(&self, name: &str) -> Option<&str> {
        self.var(name).filter(|v| !v.trim().is_empty())
    }

    fn has_non_empty(&self, name: &str) -> bool {
        self.non_empty(name).is_some()
    }

    fn has(&self, name: &str) -> bool {
        self.var(name).is_some()
    }
}

/// 归一化 `TERM_PROGRAM`：trim 后剔除空格、`-`、`_`、`.`，转小写。
fn normalize_term_program(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 终端是否在白名单里（Ghostty / iTerm2 / Kitty / Warp / WezTerm）。
///
/// 顺序照 Codex `detect_terminal_identity_from_env`。不在白名单里的一律
/// 走 BEL，包括 Alacritty 与 Windows Terminal——它们是否弹系统通知未经
/// 实测，不猜。
pub fn detect_osc9_supported(env: &TerminalEnv) -> bool {
    if let Some(program) = env.non_empty("TERM_PROGRAM") {
        if !program.eq_ignore_ascii_case("tmux") {
            return matches!(
                normalize_term_program(program).as_str(),
                "iterm"
                    | "iterm2"
                    | "itermapp"
                    | "ghostty"
                    | "kitty"
                    | "warp"
                    | "warpterminal"
                    | "wezterm"
            );
        }
    }

    env.has_non_empty("GHOSTTY_RESOURCES_DIR")
        || env.has("WEZTERM_VERSION")
        || env.has("ITERM_SESSION_ID")
        || env.has("ITERM_PROFILE")
        || env.has("ITERM_PROFILE_NAME")
        || env.has("KITTY_WINDOW_ID")
        || env.var("TERM").is_some_and(|t| t.contains("kitty"))
}

/// 是否处在多路复用器里（tmux 或 zellij）。
///
/// 判「非空」而非「存在」：`TMUX=` 空串在部分 shell 里是残留，按存在判会
/// 误把普通终端当成 tmux 环境，导致 OSC 9 被套上一层无用的 DCS 包裹。
pub fn is_multiplexed(env: &TerminalEnv) -> bool {
    env.has_non_empty("TMUX")
        || env.has_non_empty("TMUX_PANE")
        || env.has_non_empty("ZELLIJ")
        || env.has_non_empty("ZELLIJ_SESSION_NAME")
        || env.has_non_empty("ZELLIJ_VERSION")
}

#[derive(Debug)]
enum Backend {
    Osc9(Osc9Backend),
    Bel(BelBackend),
}

/// 通知器。持有后端与条件，`notify` 失败后自禁。
pub struct Notifier {
    /// `None` 表示已失效（写失败一次后不再尝试）。
    backend: Option<Backend>,
    condition: Condition,
}

impl Notifier {
    pub fn new(method: Method, condition: Condition, env: &TerminalEnv) -> Self {
        let resolved = match method {
            Method::Auto => {
                if detect_osc9_supported(env) {
                    Method::Osc9
                } else {
                    Method::Bel
                }
            }
            other => other,
        };
        let backend = match resolved {
            Method::Osc9 | Method::Auto => Backend::Osc9(Osc9Backend::new(is_multiplexed(env))),
            Method::Bel => Backend::Bel(BelBackend::new()),
        };
        Self {
            backend: Some(backend),
            condition,
        }
    }

    pub fn condition(&self) -> Condition {
        self.condition
    }

    /// 当前生效的后端名，供 `/config` 展示。失效后返回 `"disabled"`。
    pub fn effective_backend(&self) -> &'static str {
        match &self.backend {
            Some(Backend::Osc9(_)) => "osc9",
            Some(Backend::Bel(_)) => "bel",
            None => "disabled",
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.backend.is_none()
    }

    /// 是否选了 OSC 9 后端。测试与 `/config` 之外的内部断言用。
    pub fn uses_osc9(&self) -> bool {
        matches!(self.backend, Some(Backend::Osc9(_)))
    }

    /// 发一条通知。
    ///
    /// 三个短路条件：后端已失效、stdout 不是 TTY、文案消毒后为空。写失败
    /// 时置 `backend = None`——终端已不可用，继续每回合重试只会刷日志。
    pub fn notify(&mut self, message: &str) -> std::io::Result<()> {
        let Some(backend) = self.backend.as_mut() else {
            return Ok(());
        };
        if !std::io::stdout().is_terminal() {
            return Ok(());
        }
        let payload = text::sanitize(message);
        let result = match backend {
            Backend::Osc9(b) => b.notify(&payload),
            Backend::Bel(b) => b.notify(&payload),
        };
        if let Err(error) = &result {
            tracing::warn!(error = %error, "terminal notification failed; disabling for this session");
            self.backend = None;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&'static str, &'static str)]) -> TerminalEnv {
        TerminalEnv::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn whitelist_terminals_support_osc9() {
        for program in [
            "iTerm.app",
            "iTerm2",
            "ghostty",
            "Ghostty",
            "kitty",
            "Warp",
            "WarpTerminal",
            "WezTerm",
        ] {
            assert!(
                detect_osc9_supported(&env(&[("TERM_PROGRAM", program)])),
                "{program} should support OSC 9"
            );
        }
    }

    #[test]
    fn non_whitelist_terminals_use_bel() {
        for program in [
            "Apple_Terminal",
            "alacritty",
            "vscode",
            "Hyper",
            "unknown-term",
        ] {
            assert!(
                !detect_osc9_supported(&env(&[("TERM_PROGRAM", program)])),
                "{program} should not support OSC 9"
            );
        }
    }

    #[test]
    fn warp_normalization_removes_separators() {
        assert!(detect_osc9_supported(&env(&[(
            "TERM_PROGRAM",
            "warp-terminal"
        )])));
        assert!(detect_osc9_supported(&env(&[(
            "TERM_PROGRAM",
            "warp_terminal"
        )])));
    }

    #[test]
    fn fallback_probes_work_without_term_program() {
        assert!(detect_osc9_supported(&env(&[(
            "GHOSTTY_RESOURCES_DIR",
            "/x"
        )])));
        assert!(detect_osc9_supported(&env(&[(
            "WEZTERM_VERSION",
            "20240101"
        )])));
        assert!(detect_osc9_supported(&env(&[(
            "ITERM_SESSION_ID",
            "w0t0p0"
        )])));
        assert!(detect_osc9_supported(&env(&[("ITERM_PROFILE", "Default")])));
        assert!(detect_osc9_supported(&env(&[("KITTY_WINDOW_ID", "1")])));
        assert!(detect_osc9_supported(&env(&[("TERM", "xterm-kitty")])));
    }

    #[test]
    fn term_program_tmux_is_skipped_in_favour_of_probes() {
        let inside_tmux_kitty = env(&[
            ("TERM_PROGRAM", "tmux"),
            ("KITTY_WINDOW_ID", "3"),
            ("TMUX", "/tmp/tmux-501/default,1,0"),
        ]);
        assert!(detect_osc9_supported(&inside_tmux_kitty));
        assert!(is_multiplexed(&inside_tmux_kitty));
    }

    #[test]
    fn alacritty_and_wt_are_recognized_but_not_whitelisted() {
        assert!(!detect_osc9_supported(&env(&[(
            "ALACRITTY_SOCKET",
            "/tmp/a"
        )])));
        assert!(!detect_osc9_supported(&env(&[("TERM", "alacritty")])));
        assert!(!detect_osc9_supported(&env(&[("WT_SESSION", "guid")])));
    }

    #[test]
    fn multiplexer_detection_requires_non_empty() {
        assert!(is_multiplexed(&env(&[("TMUX", "/tmp/x,1,0")])));
        assert!(is_multiplexed(&env(&[("TMUX_PANE", "%0")])));
        assert!(is_multiplexed(&env(&[("ZELLIJ", "0")])));
        assert!(is_multiplexed(&env(&[("ZELLIJ_SESSION_NAME", "s")])));
        assert!(!is_multiplexed(&env(&[("TMUX", "")])));
        assert!(!is_multiplexed(&env(&[("ZELLIJ", "")])));
    }

    #[test]
    fn method_parse_accepts_three_values() {
        assert_eq!(Method::parse("auto"), Some(Method::Auto));
        assert_eq!(Method::parse("osc9"), Some(Method::Osc9));
        assert_eq!(Method::parse("bel"), Some(Method::Bel));
        assert_eq!(Method::parse("OSC9"), Some(Method::Osc9));
        assert_eq!(Method::parse("nope"), None);
    }

    #[test]
    fn condition_parse_accepts_two_values() {
        assert_eq!(Condition::parse("unfocused"), Some(Condition::Unfocused));
        assert_eq!(Condition::parse("always"), Some(Condition::Always));
        assert_eq!(Condition::parse(""), None);
    }

    #[test]
    fn explicit_bel_overrides_whitelist() {
        let notifier = Notifier::new(
            Method::Bel,
            Condition::Always,
            &env(&[("TERM_PROGRAM", "WezTerm")]),
        );
        assert!(!notifier.is_disabled());
        assert!(!notifier.uses_osc9());
    }

    #[test]
    fn auto_picks_osc9_for_wezterm() {
        let notifier = Notifier::new(
            Method::Auto,
            Condition::Always,
            &env(&[("TERM_PROGRAM", "WezTerm")]),
        );
        assert!(notifier.uses_osc9());
        assert_eq!(notifier.effective_backend(), "osc9");
    }

    #[test]
    fn auto_falls_back_to_bel_for_unknown() {
        let notifier = Notifier::new(Method::Auto, Condition::Always, &env(&[]));
        assert!(!notifier.uses_osc9());
        assert_eq!(notifier.effective_backend(), "bel");
    }
}
