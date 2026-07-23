//! 用户输入解析：将 reedline 读到的一行文本解析为 UserCommand。

/// 用户输入的命令。
#[derive(Debug, Clone)]
pub enum UserCommand {
    /// 普通 prompt，发送给 agent
    Prompt(String),
    /// 中断当前 agent 运行
    Interrupt,
    /// 退出程序
    Quit,
    /// 清空对话上下文
    Clear,
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
  /help, /h    显示此帮助
  <其他文本>    发送给 agent 作为 prompt

Ctrl+C 或 ESC 可中断当前 agent 运行。"
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
}
