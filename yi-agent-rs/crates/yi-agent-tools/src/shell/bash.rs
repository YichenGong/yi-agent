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
use crate::sandbox::{SandboxMode, SandboxPolicy};
use crate::shell::blocklist::is_blocked;

const DEFAULT_TIMEOUT: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 100 * 1024; // 100KB

pub struct BashTool {
    ctx: Arc<ToolsContext>,
    sandbox: SandboxPolicy,
}

impl BashTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        let sandbox = SandboxPolicy::new(SandboxMode::DangerFullAccess, ctx.root(), Vec::new());
        Self { ctx, sandbox }
    }

    pub fn with_sandbox(ctx: Arc<ToolsContext>, sandbox: SandboxPolicy) -> Self {
        Self { ctx, sandbox }
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
        // `call_stream` sends ToolEvent::OutputDelta/Exit/Timeout/Truncated
        // through `tx`. For the non-streaming `call` API we don't need those
        // events (the ToolResult already carries stdout/stderr/exit), but we
        // must drain the channel or `call_stream`'s `tx.send(...).await` will
        // block once the buffer fills, deadlocking the call. We spawn a task
        // that drains the channel until `call_stream` drops its sender on
        // return, which ends the drainer cleanly.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(8);
        let drainer = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let result = self.call_stream(args, tx).await;
        // `tx` was moved into `call_stream` and is dropped when it returns,
        // so the drainer's `rx.recv().await` returns None and the task exits.
        let _ = drainer.await;
        result
    }

    async fn call_stream(
        &self,
        args: Value,
        tx: tokio::sync::mpsc::Sender<ToolEvent>,
    ) -> ToolResult {
        let args: BashArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                // Never started: emit Exit so the TUI can finalize the task
                // (the agent already sent AgentEvent::ToolCall before
                // invoking us). Without this the timer would tick until
                // the turn-end cleanup aborts the task.
                let _ = tx.send(ToolEvent::Exit { code: Some(-1) }).await;
                return ToolsError::ArgsParse(e).into();
            }
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
            let _ = tx.send(ToolEvent::Exit { code: Some(-1) }).await;
            return ToolsError::CommandBlocked(reason.to_string()).into();
        }

        let hard_timeout = Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT));
        let expected = args.expected_timeout_sec.unwrap_or(DEFAULT_TIMEOUT as u32);
        let idle_limit = Duration::from_secs((expected as u64) * 3 / 2); // expected * 1.5

        let cwd = self.ctx.cwd();

        let (program, command_args) = match self.sandbox.command(&args.command, &cwd) {
            Ok(command) => command,
            Err(error) => {
                let _ = tx.send(ToolEvent::Exit { code: Some(-1) }).await;
                return error.into();
            }
        };

        let mut child = match Command::new(program)
            .args(command_args)
            .current_dir(&cwd)
            // Dropping the agent's tool future must not leave the shell running.
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(ToolEvent::Exit { code: Some(-1) }).await;
                return ToolsError::Io(e).into();
            }
        };

        // Take stdout/stderr pipes.
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");

        // Spawn reader tasks that send chunks through channels.
        let (stdout_tx, stdout_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (stderr_tx, stderr_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let stdout_reader = tokio::spawn(async move {
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

        let stderr_reader = tokio::spawn(async move {
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
                            let was_truncated = stdout_truncated;
                            append_with_truncation(
                                &mut stdout_buf,
                                &mut stdout_truncated,
                                &mut stdout_skipped,
                                &data,
                            );
                            if !was_truncated && stdout_truncated {
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

                            let was_truncated = stderr_truncated;
                            append_with_truncation(
                                &mut stderr_buf,
                                &mut stderr_truncated,
                                &mut stderr_skipped,
                                &data,
                            );
                            if !was_truncated && stderr_truncated {
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

        // Wait for the reader tasks to finish so we have all stdout/stderr
        // chunks. The process has exited (or was killed) so the pipes will
        // return EOF. Without this we could lose trailing chunks that were
        // still in flight when the select loop broke on child.wait().
        //
        // However, if an orphaned subprocess inherited the pipe FDs (common
        // with `sleep 30 &`, `nohup`, daemons), the reader tasks will never
        // see EOF even though the main child has exited. The raw `.await`
        // would block forever, hanging call_stream → join_all → Done event
        // → TUI shows frozen decode + ticking bash timer forever.
        //
        // Fix: bound the wait by idle_limit. If the readers don't finish by
        // then, an orphan is holding the pipe — give up and drain what we
        // have via try_recv below.
        let reader_wait = tokio::time::timeout(idle_limit, async {
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
        })
        .await;
        if reader_wait.is_err() {
            tracing::warn!(
                idle_limit_secs = idle_limit.as_secs(),
                "bash: reader tasks did not finish within idle_limit after child exited — \
                 orphaned subprocess is holding the pipe; draining available output"
            );
        }

        // Drain any chunks that arrived after the last select iteration.
        if let Some(rx) = stdout_rx.as_mut() {
            while let Ok(data) = rx.try_recv() {
                let text = String::from_utf8_lossy(&data).into_owned();
                let _ = tx
                    .send(ToolEvent::OutputDelta {
                        stream: OutputStream::Stdout,
                        text,
                    })
                    .await;
                let was_truncated = stdout_truncated;
                append_with_truncation(
                    &mut stdout_buf,
                    &mut stdout_truncated,
                    &mut stdout_skipped,
                    &data,
                );
                if !was_truncated && stdout_truncated {
                    let _ = tx
                        .send(ToolEvent::Truncated {
                            stream: OutputStream::Stdout,
                            skipped_bytes: stdout_skipped,
                        })
                        .await;
                }
            }
        }
        if let Some(rx) = stderr_rx.as_mut() {
            while let Ok(data) = rx.try_recv() {
                let text = String::from_utf8_lossy(&data).into_owned();
                let _ = tx
                    .send(ToolEvent::OutputDelta {
                        stream: OutputStream::Stderr,
                        text,
                    })
                    .await;
                let was_truncated = stderr_truncated;
                append_with_truncation(
                    &mut stderr_buf,
                    &mut stderr_truncated,
                    &mut stderr_skipped,
                    &data,
                );
                if !was_truncated && stderr_truncated {
                    let _ = tx
                        .send(ToolEvent::Truncated {
                            stream: OutputStream::Stderr,
                            skipped_bytes: stderr_skipped,
                        })
                        .await;
                }
            }
        }

        // Emit Exit event.
        let _ = tx.send(ToolEvent::Exit { code: exit_code }).await;

        // Build the result text. The buffer was already capped at
        // MAX_OUTPUT_BYTES by `append_with_truncation`; the `*_truncated`
        // flag + `*_skipped` count carry the information needed to build
        // the prefix. `truncate_output` alone can't tell us whether
        // truncation happened (the buffer is exactly at the cap, not
        // above it).
        let stdout_text = format_truncated(&stdout_buf, stdout_truncated, stdout_skipped);
        let stderr_text = format_truncated(&stderr_buf, stderr_truncated, stderr_skipped);

        if timed_out {
            ToolResult::error(format!(
                "command timeout after {}s\nstdout:\n{}\nstderr:\n{}",
                args.timeout.unwrap_or(DEFAULT_TIMEOUT),
                stdout_text,
                stderr_text,
            ))
        } else {
            let exit = exit_code.unwrap_or(-1);
            // Only a successful standalone cd can change our persistent cwd.
            // A cd embedded in a shell expression may not run at all.
            if exit == 0 {
                if let Some(new_cwd) = parse_standalone_cd_target(&args.command, &cwd) {
                    self.ctx.set_cwd(new_cwd);
                }
            }
            ToolResult::text(format!(
                "exit: {}\nstdout:\n{}\nstderr:\n{}",
                exit, stdout_text, stderr_text,
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

/// Format the captured output for the ToolResult. If `truncated` is true,
/// `append_with_truncation` already capped `bytes` at MAX_OUTPUT_BYTES and
/// `skipped` carries the total bytes dropped from the front. We prepend a
/// human-readable prefix; otherwise we return the bytes verbatim.
fn format_truncated(bytes: &[u8], truncated: bool, skipped: usize) -> String {
    if truncated {
        format!(
            "[truncated: showed last 100KB of {}B]\n{}",
            skipped + bytes.len(),
            String::from_utf8_lossy(bytes)
        )
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Legacy helper kept for compatibility: truncates a raw buffer to the last
/// MAX_OUTPUT_BYTES with a prefix. Kept for any callers that haven't been
/// migrated to `format_truncated`.
#[allow(dead_code)]
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

/// Parse the last `cd <dir>` target from a command string.
/// Returns None if there's no cd command.
fn parse_standalone_cd_target(
    cmd: &str,
    current_cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let re = regex::Regex::new(r"^\s*cd\s+(\S+)\s*$").unwrap();
    re.captures(cmd).map(|cap| {
        let target = cap[1].trim_matches(|c| c == '"' || c == '\'');
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

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn workspace_sandbox_allows_workspace_writes_and_denies_other_writes() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let ctx = Arc::new(ToolsContext::new(workspace.path().to_path_buf()));
        let sandbox = SandboxPolicy::new(SandboxMode::WorkspaceWrite, workspace.path(), vec![]);
        let tool = BashTool::with_sandbox(ctx, sandbox);

        let inside = tool
            .call(serde_json::json!({"command": "touch allowed.txt"}))
            .await;
        assert!(!inside.is_error);
        assert!(workspace.path().join("allowed.txt").exists());

        let denied_path = outside.path().join("denied.txt");
        let denied = tool
            .call(serde_json::json!({"command": format!("touch {}", denied_path.display())}))
            .await;
        assert!(!denied.is_error);
        assert!(
            !denied_path.exists(),
            "sandbox must deny writes outside workspace"
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn read_only_sandbox_denies_workspace_writes() {
        let workspace = TempDir::new().unwrap();
        let ctx = Arc::new(ToolsContext::new(workspace.path().to_path_buf()));
        let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, workspace.path(), vec![]);
        let tool = BashTool::with_sandbox(ctx, sandbox);

        let result = tool
            .call(serde_json::json!({"command": "touch denied.txt"}))
            .await;
        assert!(!result.is_error);
        assert!(!workspace.path().join("denied.txt").exists());
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
    async fn bash_failed_conditional_cd_does_not_persist_cwd() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("subdir")).unwrap();
        let tool = make_tool(&tmp);

        let result = tool
            .call(serde_json::json!({"command": "false && cd subdir"}))
            .await;
        assert!(!result.is_error);

        let result = tool.call(serde_json::json!({"command": "pwd"})).await;
        let yi_agent_core::ContentBlock::Text(output) = &result.content[0] else {
            panic!("expected text output");
        };
        assert!(output.contains(tmp.path().to_string_lossy().as_ref()));
        assert!(!output.contains("subdir"));
    }

    #[tokio::test]
    async fn dropping_bash_call_stops_the_child_process() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("should-not-exist");
        let tool = Arc::new(make_tool(&tmp));
        let command = format!("sleep 0.2; touch {}", marker.display());

        let task = tokio::spawn({
            let tool = tool.clone();
            async move { tool.call(serde_json::json!({"command": command})).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        task.abort();
        let _ = task.await;

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !marker.exists(),
            "child process outlived cancelled tool call"
        );
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

    /// Regression: when the bash tool early-returns on a blocked command
    /// (or arg-parse / spawn error), it must still emit `ToolEvent::Exit`
    /// so downstream consumers (the TUI task registry) can finalize the
    /// task. Without this the status-bar timer ticks forever waiting for a
    /// ToolExit that never arrives.
    #[tokio::test]
    async fn bash_error_paths_emit_exit() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // Blocked command path.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(8);
        let _ = tool
            .call_stream(serde_json::json!({"command": "rm -rf /"}), tx)
            .await;
        let mut saw_exit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ToolEvent::Exit { .. }) {
                saw_exit = true;
            }
        }
        assert!(
            saw_exit,
            "blocked command should still emit ToolEvent::Exit"
        );

        // Arg-parse error path (missing `command` field).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(8);
        let _ = tool
            .call_stream(serde_json::json!({"timeout": 1}), tx)
            .await;
        let mut saw_exit = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, ToolEvent::Exit { .. }) {
                saw_exit = true;
            }
        }
        assert!(
            saw_exit,
            "arg-parse error should still emit ToolEvent::Exit"
        );
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
    fn parse_standalone_cd_target_simple() {
        let cwd = std::path::Path::new("/root");
        let target = parse_standalone_cd_target("cd foo", cwd).unwrap();
        assert_eq!(target, std::path::PathBuf::from("/root/foo"));
    }

    #[test]
    fn parse_standalone_cd_target_absolute() {
        let cwd = std::path::Path::new("/root");
        let target = parse_standalone_cd_target("cd /abs/path", cwd).unwrap();
        assert_eq!(target, std::path::PathBuf::from("/abs/path"));
    }

    #[test]
    fn parse_standalone_cd_target_rejects_compound_command() {
        let cwd = std::path::Path::new("/root");
        assert!(parse_standalone_cd_target("cd foo && cd bar", cwd).is_none());
    }

    #[test]
    fn parse_standalone_cd_target_none() {
        let cwd = std::path::Path::new("/root");
        assert!(parse_standalone_cd_target("ls -la", cwd).is_none());
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

    /// Regression: when the main bash process exits but an orphaned
    /// subprocess inherits the stdout/stderr pipes (common with
    /// backgrounded processes, daemons, or `nohup`), the reader tasks
    /// block forever waiting for EOF. The `call_stream` must not hang
    /// indefinitely — it should return within a bounded time after the
    /// main child exits.
    ///
    /// Before the fix, lines 323-324 (`let _ = stdout_reader.await`)
    /// blocked forever because the orphan keeps the pipe open, so the
    /// reader never sees EOF. This caused the agent's ACT phase to hang
    /// (join_all never completes), the Done event to never fire, and
    /// the TUI to show a frozen decode counter + ticking bash timer
    /// until the user manually cancelled.
    #[tokio::test]
    async fn bash_orphan_subprocess_does_not_hang_call_stream() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // Spawn a command that backgrounds a long-running child which
        // inherits stdout/stderr, then exits immediately. The orphan
        // keeps the pipes open so the reader tasks never see EOF.
        //
        // `sleep 30 &` backgrounds a child; `exit 0` exits the main
        // shell. The orphan holds the pipe FDs for 30s.
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tool.call_stream(
                serde_json::json!({
                    "command": "sleep 30 & exit 0",
                    "timeout": 5,
                    "expected_timeout_sec": 3
                }),
                tokio::sync::mpsc::channel::<ToolEvent>(8).0,
            ),
        )
        .await;

        // If call_stream hangs forever (the bug), this timeout fires
        // and the test fails with "elapsed".
        let result = result.expect(
            "call_stream hung forever — reader await blocked on orphan-held pipe \
             (no timeout on stdout_reader/stderr_reader after child exits)",
        );
        // Should have completed successfully (exit 0).
        assert!(
            !result.is_error,
            "expected success (exit 0), got error: {:?}",
            result.content
        );
    }
}
