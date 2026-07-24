//! 用户输入解析：将 reedline 读到的一行文本解析为 UserCommand。

/// 用户输入的命令。
#[derive(Debug, Clone)]
pub enum UserCommand {
    /// 普通 prompt，发送给 agent
    Prompt(String),
    /// 退出程序
    Quit,
    /// 中断当前 agent 运行；若 agent 空闲则退出程序
    Interrupt,
    /// 清空对话上下文
    Clear,
    /// 切换模型
    Model(String),
    /// 显示 token 用量
    Cost,
    /// 手动压缩对话
    Compact,
    /// 显示当前配置
    Config,
    /// 显示帮助
    Help,
}

/// 将一行输入解析为 UserCommand。
///
/// `/` 开头的是 slash 命令，其余是普通 prompt。
/// 空行返回 None（忽略）。
pub fn parse_user_input(line: &str) -> Option<UserCommand> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.starts_with('/') {
        return Some(UserCommand::Prompt(trimmed.to_string()));
    }
    // slash 命令解析
    let cmd = trimmed.trim_start_matches('/');
    let parts: Vec<&str> = cmd.splitn(2, ' ').collect();
    let command = parts[0];
    match command {
        "quit" | "q" | "exit" => Some(UserCommand::Quit),
        "clear" => Some(UserCommand::Clear),
        "model" => {
            if let Some(name) = parts.get(1) {
                Some(UserCommand::Model(name.to_string()))
            } else {
                Some(UserCommand::Prompt(trimmed.to_string()))
            }
        }
        "cost" => Some(UserCommand::Cost),
        "compact" => Some(UserCommand::Compact),
        "config" => Some(UserCommand::Config),
        "help" | "h" | "?" => Some(UserCommand::Help),
        _ => Some(UserCommand::Prompt(trimmed.to_string())), // 未知 / 命令当普通输入
    }
}

/// 帮助文本
pub fn help_text() -> &'static str {
    "\
可用命令：
  /quit, /q    退出
  /clear       清空对话上下文
  /model <name>  切换模型
  /cost        显示 token 用量
  /compact     手动压缩对话
  /config      显示当前配置
  /help, /h    显示此帮助
  <其他文本>    发送给 agent 作为 prompt

Ctrl+C 或 ESC 可中断当前 agent 运行。"
}

/// 将 reedline 的 `Signal` 映射为 `UserCommand`。
///
/// - `Success(line)` → 解析为 slash 命令或普通 prompt（沿用 `parse_user_input`）
/// - `CtrlC` → `Interrupt`（中断 agent 运行，或空闲时退出）
/// - `CtrlD` → `Quit`（EOF，直接退出）
///
/// 返回 `None` 表示该信号不应产生命令（例如空行）。
pub fn map_reedline_signal(sig: Result<reedline::Signal, std::io::Error>) -> Option<UserCommand> {
    match sig {
        Ok(reedline::Signal::Success(line)) => parse_user_input(&line),
        Ok(reedline::Signal::CtrlC) => Some(UserCommand::Interrupt),
        Ok(reedline::Signal::CtrlD) => Some(UserCommand::Quit),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_returns_none() {
        assert!(parse_user_input("").is_none());
        assert!(parse_user_input("   ").is_none());
    }

    #[test]
    fn parse_plain_text_is_prompt() {
        let cmd = parse_user_input("hello world").unwrap();
        match cmd {
            UserCommand::Prompt(text) => assert_eq!(text, "hello world"),
            _ => panic!("expected Prompt"),
        }
    }

    #[test]
    fn parse_quit_variants() {
        assert!(matches!(
            parse_user_input("/quit").unwrap(),
            UserCommand::Quit
        ));
        assert!(matches!(parse_user_input("/q").unwrap(), UserCommand::Quit));
        assert!(matches!(
            parse_user_input("/exit").unwrap(),
            UserCommand::Quit
        ));
    }

    #[test]
    fn parse_clear() {
        assert!(matches!(
            parse_user_input("/clear").unwrap(),
            UserCommand::Clear
        ));
    }

    #[test]
    fn parse_help_variants() {
        assert!(matches!(
            parse_user_input("/help").unwrap(),
            UserCommand::Help
        ));
        assert!(matches!(parse_user_input("/h").unwrap(), UserCommand::Help));
        assert!(matches!(parse_user_input("/?").unwrap(), UserCommand::Help));
    }

    #[test]
    fn parse_unknown_slash_is_prompt() {
        let cmd = parse_user_input("/unknown args").unwrap();
        match cmd {
            UserCommand::Prompt(text) => assert_eq!(text, "/unknown args"),
            _ => panic!("expected Prompt for unknown slash command"),
        }
    }

    #[test]
    fn parse_trims_whitespace() {
        let cmd = parse_user_input("  /quit  ").unwrap();
        assert!(matches!(cmd, UserCommand::Quit));
    }

    #[test]
    fn parse_model_command() {
        let cmd = parse_user_input("/model claude-sonnet-4-5").unwrap();
        match cmd {
            UserCommand::Model(name) => assert_eq!(name, "claude-sonnet-4-5"),
            _ => panic!("expected Model"),
        }
    }

    #[test]
    fn parse_model_command_no_arg_returns_prompt() {
        let cmd = parse_user_input("/model").unwrap();
        match cmd {
            UserCommand::Prompt(_) => {}
            _ => panic!("expected Prompt for /model without arg"),
        }
    }

    #[test]
    fn parse_cost_command() {
        assert!(matches!(
            parse_user_input("/cost").unwrap(),
            UserCommand::Cost
        ));
    }

    #[test]
    fn parse_compact_command() {
        assert!(matches!(
            parse_user_input("/compact").unwrap(),
            UserCommand::Compact
        ));
    }

    #[test]
    fn parse_config_command() {
        assert!(matches!(
            parse_user_input("/config").unwrap(),
            UserCommand::Config
        ));
    }

    #[test]
    fn map_signal_ctrl_c_returns_interrupt() {
        let cmd = map_reedline_signal(Ok(reedline::Signal::CtrlC));
        assert!(matches!(cmd, Some(UserCommand::Interrupt)));
    }

    #[test]
    fn map_signal_ctrl_d_returns_quit() {
        let cmd = map_reedline_signal(Ok(reedline::Signal::CtrlD));
        assert!(matches!(cmd, Some(UserCommand::Quit)));
    }

    #[test]
    fn map_signal_success_with_text_returns_prompt() {
        let cmd = map_reedline_signal(Ok(reedline::Signal::Success("hello".to_string())));
        match cmd {
            Some(UserCommand::Prompt(text)) => assert_eq!(text, "hello"),
            other => panic!("expected Prompt, got {other:?}"),
        }
    }

    #[test]
    fn map_signal_success_with_slash_quit_returns_quit() {
        let cmd = map_reedline_signal(Ok(reedline::Signal::Success("/quit".to_string())));
        assert!(matches!(cmd, Some(UserCommand::Quit)));
    }

    #[test]
    fn map_signal_success_with_empty_line_returns_none() {
        let cmd = map_reedline_signal(Ok(reedline::Signal::Success("   ".to_string())));
        assert!(cmd.is_none());
    }

    #[test]
    fn map_signal_error_returns_none() {
        let cmd = map_reedline_signal(Err(std::io::Error::other("test")));
        assert!(cmd.is_none());
    }
}
