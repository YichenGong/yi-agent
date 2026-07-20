use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::shell::blocklist::is_blocked;

const DEFAULT_TIMEOUT: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

pub struct BashTool {
    ctx: Arc<ToolsContext>,
}

impl BashTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self { ctx }
    }
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in seconds, default 120" }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: BashArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        if let Some(reason) = is_blocked(&args.command) {
            return ToolsError::CommandBlocked(reason.to_string()).into();
        }

        let timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let cwd = self.ctx.cwd();

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolsError::Io(e).into(),
        };

        // Update cwd based on cd commands in the command string.
        if let Some(new_cwd) = parse_cd_target(&args.command, &cwd) {
            self.ctx.set_cwd(new_cwd);
        }

        // Take stdout/stderr pipes so we can read them concurrently with wait.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        // Read stdout/stderr concurrently with waiting to avoid pipe-buffer deadlock.
        let stdout_fut = read_to_end(&mut stdout);
        let stderr_fut = read_to_end(&mut stderr);

        let combined = async {
            let (status, stdout_buf, stderr_buf) =
                tokio::join!(child.wait(), stdout_fut, stderr_fut);
            (status, stdout_buf, stderr_buf)
        };

        match tokio::time::timeout(Duration::from_secs(timeout), combined).await {
            Ok((Ok(status), stdout_buf, stderr_buf)) => {
                let stdout_trunc = truncate_output(&stdout_buf);
                let stderr_trunc = truncate_output(&stderr_buf);
                let exit = status.code().unwrap_or(-1);
                ToolResult::text(format!(
                    "exit: {}\nstdout:\n{}\nstderr:\n{}",
                    exit,
                    String::from_utf8_lossy(&stdout_trunc),
                    String::from_utf8_lossy(&stderr_trunc),
                ))
            }
            Ok((Err(e), _, _)) => ToolsError::Io(e).into(),
            Err(_) => {
                let _ = child.kill().await;
                ToolsError::Timeout(timeout).into()
            }
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: true,
            read_only: false,
            version: None,
        }
    }
}

fn truncate_output(bytes: &[u8]) -> Vec<u8> {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        bytes.to_vec()
    } else {
        let start = bytes.len() - MAX_OUTPUT_BYTES;
        let mut truncated =
            format!("[truncated: showed last 100KB of {}B]\n", bytes.len()).into_bytes();
        truncated.extend_from_slice(&bytes[start..]);
        truncated
    }
}

/// Read all bytes from an async reader into a Vec.
async fn read_to_end<R: AsyncRead + Unpin>(reader: &mut R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf).await;
    buf
}

/// Parse the last `cd <dir>` target from a command string.
/// Returns None if there's no cd command.
fn parse_cd_target(cmd: &str, current_cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let re = regex::Regex::new(r"(?:^|;|\|\||&&|\n)\s*cd\s+(\S+)").unwrap();
    let mut last_target: Option<String> = None;
    for cap in re.captures_iter(cmd) {
        last_target = Some(cap[1].trim_matches(|c| c == '"' || c == '\'').to_string());
    }

    last_target.map(|target| {
        let target_path = std::path::PathBuf::from(&target);
        if target_path.is_absolute() {
            target_path
        } else {
            current_cwd.join(target_path)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_tool(tmp: &TempDir) -> BashTool {
        BashTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[tokio::test]
    async fn bash_echo() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"command": "echo hello"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("exit: 0"));
            assert!(s.contains("hello"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"command": "exit 1"})).await;
        assert!(!result.is_error); // errors are data, not ToolResult::is_error
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("exit: 1"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_stderr_captured() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"command": "echo err >&2"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("err"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_cwd_persists() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = make_tool(&tmp);
        // First call: cd into subdir
        tool.call(serde_json::json!({"command": "cd subdir"})).await;
        // Second call: pwd should show subdir
        let result = tool.call(serde_json::json!({"command": "pwd"})).await;
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("subdir"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_timeout_kills() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({
                "command": "sleep 10",
                "timeout": 1
            }))
            .await;
        assert!(result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("timeout"));
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_blocklist_rm_rf() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool.call(serde_json::json!({"command": "rm -rf /"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn bash_blocklist_fork_bomb() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"command": ":(){ :|:& };:"}))
            .await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn bash_output_truncated() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        // Generate ~200KB output
        let result = tool
            .call(serde_json::json!({
                "command": "yes hello | head -c 200000"
            }))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(s.contains("[truncated:"));
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn parse_cd_target_simple() {
        let cwd = std::path::Path::new("/root");
        let target = parse_cd_target("cd foo", cwd).unwrap();
        assert_eq!(target, std::path::PathBuf::from("/root/foo"));
    }

    #[test]
    fn parse_cd_target_absolute() {
        let cwd = std::path::Path::new("/root");
        let target = parse_cd_target("cd /abs/path", cwd).unwrap();
        assert_eq!(target, std::path::PathBuf::from("/abs/path"));
    }

    #[test]
    fn parse_cd_target_last_wins() {
        let cwd = std::path::Path::new("/root");
        let target = parse_cd_target("cd foo && cd bar", cwd).unwrap();
        assert_eq!(target, std::path::PathBuf::from("/root/bar"));
    }

    #[test]
    fn parse_cd_target_none() {
        let cwd = std::path::Path::new("/root");
        assert!(parse_cd_target("ls -la", cwd).is_none());
    }
}
