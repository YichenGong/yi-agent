use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use yi_agent_core::{OutputStream, Tool, ToolEvent, ToolMetadata, ToolResult, ToolSource};

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
    /// LLM-declared expected runtime. Default 120s. If no output for
    /// expected_timeout_sec * 1.5, process is killed as stuck.
    #[serde(default)]
    expected_timeout_sec: Option<u32>,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls. Prefer combining dependent steps with && into a single call (e.g. `mkdir -p foo && touch foo/bar.txt && ls foo`) rather than splitting across turns."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The bash command to execute" },
                "timeout": { "type": "integer", "description": "Optional timeout in seconds (legacy hard timeout)", "default": 120 },
                "expected_timeout_sec": {
                    "type": "integer",
                    "description": "Expected runtime in seconds. If no stdout/stderr output for expected_timeout_sec * 1.5, the process is killed as stuck. Default 120.",
                    "default": 120
                }
            },
            "required": ["command"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let (tx, _rx) = tokio::sync::mpsc::channel::<ToolEvent>(1);
        self.call_stream(args, tx).await
    }

    async fn call_stream(
        &self,
        args: Value,
        tx: tokio::sync::mpsc::Sender<ToolEvent>,
    ) -> ToolResult {
        let args: BashArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };

        tracing::info!(
            tool = "bash",
            command = %args.command,
            timeout = args.timeout,
            expected_timeout_sec = args.expected_timeout_sec,
            "executing"
        );

        if let Some(reason) = is_blocked(&args.command) {
            tracing::warn!(tool = "bash", reason = %reason, "command blocked");
            return ToolsError::CommandBlocked(reason.to_string()).into();
        }

        let hard_timeout = Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT));
        let expected = args.expected_timeout_sec.unwrap_or(DEFAULT_TIMEOUT as u32);
        let idle_limit = Duration::from_secs((expected as u64) * 3 / 2); // expected * 1.5

        let cwd = self.ctx.cwd();

        let mut child = match Command::new("sh")
            .arg("-c")
            .arg(&args.command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolsError::Io(e).into(),
        };

        // Update cwd based on cd commands in the command string.
        if let Some(new_cwd) = parse_cd_target(&args.command, &cwd) {
            self.ctx.set_cwd(new_cwd);
        }

        // Take stdout/stderr pipes.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        // Spawn reader tasks that send chunks through channels.
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stdout_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if stderr_tx.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // State for the main select loop.
        let mut stdout_buf: Vec<u8> = Vec::new();
        let mut stderr_buf: Vec<u8> = Vec::new();
        let mut stdout_truncated = false;
        let mut stderr_truncated = false;
        let mut stdout_skipped: usize = 0;
        let mut stderr_skipped: usize = 0;
        let mut exit_code: Option<i32> = None;
        let mut timed_out = false;

        let now = tokio::time::Instant::now();
        let hard_deadline = now + hard_timeout;
        let mut next_idle_deadline = now + idle_limit;

        // Track whether each reader channel is still open.
        let mut stdout_rx = Some(stdout_rx);
        let mut stderr_rx = Some(stderr_rx);

        loop {
            // Build the select branches conditionally using Option<Receiver>.
            let stdout_recv = async {
                if let Some(rx) = stdout_rx.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending::<Option<Vec<u8>>>().await
                }
            };
            let stderr_recv = async {
                if let Some(rx) = stderr_rx.as_mut() {
                    rx.recv().await
                } else {
                    std::future::pending::<Option<Vec<u8>>>().await
                }
            };

            tokio::select! {
                biased;

                chunk = stdout_recv => {
                    match chunk {
                        Some(data) => {
                            // Send OutputDelta event.
                            let text = String::from_utf8_lossy(&data).into_owned();
                            let _ = tx.send(ToolEvent::OutputDelta {
                                stream: OutputStream::Stdout,
                                text,
                            }).await;

                            // Accumulate with truncation tracking.
                            append_with_truncation(
                                &mut stdout_buf,
                                &mut stdout_truncated,
                                &mut stdout_skipped,
                                &data,
                            );
                            if stdout_truncated && stdout_skipped == data.len() {
                                // Just crossed the threshold — send Truncated event.
                                let _ = tx.send(ToolEvent::Truncated {
                                    stream: OutputStream::Stdout,
                                    skipped_bytes: stdout_skipped,
                                }).await;
                            }

                            // Reset idle deadline.
                            next_idle_deadline = tokio::time::Instant::now() + idle_limit;
                        }
                        None => {
                            // stdout reader finished; stop polling.
                            stdout_rx = None;
                        }
                    }
                }

                chunk = stderr_recv => {
                    match chunk {
                        Some(data) => {
                            let text = String::from_utf8_lossy(&data).into_owned();
                            let _ = tx.send(ToolEvent::OutputDelta {
                                stream: OutputStream::Stderr,
                                text,
                            }).await;

                            append_with_truncation(
                                &mut stderr_buf,
                                &mut stderr_truncated,
                                &mut stderr_skipped,
                                &data,
                            );
                            if stderr_truncated && stderr_skipped == data.len() {
                                let _ = tx.send(ToolEvent::Truncated {
                                    stream: OutputStream::Stderr,
                                    skipped_bytes: stderr_skipped,
                                }).await;
                            }

                            next_idle_deadline = tokio::time::Instant::now() + idle_limit;
                        }
                        None => {
                            // stderr reader finished; stop polling.
                            stderr_rx = None;
                        }
                    }
                }

                _ = tokio::time::sleep_until(next_idle_deadline) => {
                    // Idle watchdog: no output for idle_limit.
                    let _ = child.kill().await;
                    let _ = tx.send(ToolEvent::Timeout).await;
                    timed_out = true;
                    break;
                }

                _ = tokio::time::sleep_until(hard_deadline) => {
                    // Hard timeout.
                    let _ = child.kill().await;
                    let _ = tx.send(ToolEvent::Timeout).await;
                    timed_out = true;
                    break;
                }

                status = child.wait() => {
                    match status {
                        Ok(s) => {
                            exit_code = s.code();
                        }
                        Err(_) => {
                            exit_code = None;
                        }
                    }
                    break;
                }
            }
        }

        // Reap the child if it was killed (wait already done if we hit the child.wait branch).
        // If we broke out via timeout, child.kill() was already called, but we need to reap.
        if timed_out {
            let _ = child.wait().await;
        }

        // Emit Exit event.
        let _ = tx.send(ToolEvent::Exit { code: exit_code }).await;

        // Build the result text.
        let stdout_trunc = truncate_output(&stdout_buf);
        let stderr_trunc = truncate_output(&stderr_buf);

        if timed_out {
            ToolResult::error(format!(
                "command timeout after {}s\nstdout:\n{}\nstderr:\n{}",
                args.timeout.unwrap_or(DEFAULT_TIMEOUT),
                String::from_utf8_lossy(&stdout_trunc),
                String::from_utf8_lossy(&stderr_trunc),
            ))
        } else {
            let exit = exit_code.unwrap_or(-1);
            ToolResult::text(format!(
                "exit: {}\nstdout:\n{}\nstderr:\n{}",
                exit,
                String::from_utf8_lossy(&stdout_trunc),
                String::from_utf8_lossy(&stderr_trunc),
            ))
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

/// Append data to the buffer with truncation at MAX_OUTPUT_BYTES.
/// Once the buffer exceeds the limit, we keep only the last MAX_OUTPUT_BYTES
/// and track total skipped bytes for the Truncated event.
fn append_with_truncation(
    buf: &mut Vec<u8>,
    truncated: &mut bool,
    skipped: &mut usize,
    data: &[u8],
) {
    if *truncated {
        // Already truncated: append data, then trim to keep only last MAX_OUTPUT_BYTES.
        buf.extend_from_slice(data);
        if buf.len() > MAX_OUTPUT_BYTES {
            let excess = buf.len() - MAX_OUTPUT_BYTES;
            *skipped += excess;
            buf.drain(..excess);
        }
    } else if buf.len() + data.len() > MAX_OUTPUT_BYTES {
        // Crossing the threshold for the first time.
        let total = buf.len() + data.len();
        *skipped = total - MAX_OUTPUT_BYTES;
        *truncated = true;
        buf.extend_from_slice(data);
        let start = buf.len() - MAX_OUTPUT_BYTES;
        buf.drain(..start);
    } else {
        buf.extend_from_slice(data);
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

    #[test]
    fn bash_description_encourages_combining_steps() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let desc = tool.description();
        assert!(
            desc.contains("&&"),
            "description should guide combining dependent steps with &&, got: {desc}"
        );
    }
}
