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

/// 保证异常退出时回收整个进程组；正常结束时由调用方 `disarm()`。
///
/// 只在未被 disarm 时于 `Drop` 中对整组发 `SIGKILL`。
struct ProcessGroupGuard {
    pgid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pgid: Option<u32>) -> Self {
        Self { pgid, armed: true }
    }

    /// 标记为正常结束：不再回收整组（保留后台进程）。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            if let Some(pgid) = self.pgid {
                crate::process_group::signal_process_group(pgid, crate::process_group::SIGKILL);
            }
        }
    }
}

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
        "Execute a shell command via sh -c. Subject to blocklist + timeout. cwd persists across calls. On timeout or cancellation the command's entire process group is killed, including background processes it started; for long-lived services use the managed process tools instead. Prefer combining dependent steps with && into a single call (e.g. `mkdir -p foo && touch foo/bar.txt && ls foo`) rather than splitting across turns."
    }

    fn sandbox_mode(&self) -> Option<&'static str> {
        Some(self.sandbox.mode().as_str())
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
            .envs(self.ctx.session_env().iter().cloned())
            .process_group(0) // 自成进程组（pgid == 子进程 pid），与 yi-agent 隔离
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

        // 必须在任何 wait() 之前抓取：Child::id() 一旦子进程被 poll 完成
        // 即返回 None（tokio 1.53.0 process/mod.rs:1216-1227）。
        // 隔离后该值同时是子进程的进程组 id。
        let child_pgid = child.id();
        let mut pgid_guard = ProcessGroupGuard::new(child_pgid);

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
                    // Idle watchdog: no output for idle_limit. Reap the whole
                    // group, not just the direct child: background grandchildren
                    // (`sh -c 'while :; do :; done' &`) would otherwise be
                    // orphaned and keep burning CPU forever.
                    if let Some(pgid) = child_pgid {
                        crate::process_group::signal_process_group(pgid, crate::process_group::SIGKILL);
                    }
                    let _ = child.wait().await;
                    let _ = tx.send(ToolEvent::Timeout).await;
                    timed_out = true;
                    pgid_guard.disarm();
                    break;
                }

                _ = tokio::time::sleep_until(hard_deadline) => {
                    // Hard timeout: same whole-group reclamation as the idle
                    // watchdog above.
                    if let Some(pgid) = child_pgid {
                        crate::process_group::signal_process_group(pgid, crate::process_group::SIGKILL);
                    }
                    let _ = child.wait().await;
                    let _ = tx.send(ToolEvent::Timeout).await;
                    timed_out = true;
                    pgid_guard.disarm();
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

        // Normal exit (the timeout branches already disarmed above and reaped
        // in place): keep background processes alive so `nohup ... &` services
        // can outlive this call. Without this the guard would kill them on drop.
        pgid_guard.disarm();

        // Reap the child if it was killed (wait already done if we hit the child.wait branch).
        // If we broke out via timeout, the group kill above already reaped it.
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
        let pid_file = tmp.path().join("child.pid");
        let marker = tmp.path().join("should-not-exist");
        let tool = Arc::new(make_tool(&tmp));
        // 先同步写出自身 pid，再长睡，最后才 touch：给出确定的观察点，
        // 且「是否被杀」可由 pid 存活直接判定。
        let command = format!(
            "echo $$ > {}; sleep 30; touch {}",
            pid_file.display(),
            marker.display()
        );

        let task = tokio::spawn({
            let tool = tool.clone();
            async move { tool.call(serde_json::json!({"command": command})).await }
        });

        // 等子进程真的起来（写出 pid）再取消。原先固定睡 50ms 是**竞态**：满载下
        // 这个 timer 会迟到，命令可能已经跑完（marker 已写出）才 abort，于是一次
        // 本该过的用例误报。先确认「已启动」，abort 才确实是在测「取消会杀掉它」。
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
            .expect("child must have started and written its pid before we cancel it");

        task.abort();
        let _ = task.await;

        // 取消必须杀掉子进程：有界等待它消失（满载下 SIGKILL 的回收也可能稍晚）。
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while pid_alive(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !pid_alive(pid),
            "cancelled tool call must kill the child (pid {pid} still alive)"
        );
        // 子进程既亡，其后半段（touch）不可能执行。
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_child_runs_in_its_own_process_group() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // 子进程打印自身 pgid 与 pid；隔离后应相等（自任组长）。
        let result = tool
            .call(serde_json::json!({
                "command": "echo \"pgid=$(ps -o pgid= -p $$ | tr -d ' ') pid=$$\""
            }))
            .await;
        assert!(!result.is_error);

        let text = match &result.content[0] {
            yi_agent_core::ContentBlock::Text(s) => s.clone(),
            _ => panic!("expected text block"),
        };
        let pgid: i32 = text
            .split("pgid=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let pid: i32 = text
            .split("pid=")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(pgid, pid, "child should lead its own process group");

        // 且不与测试进程（即模拟的 yi-agent）同组。
        let own = unsafe { libc::getpgid(0) };
        assert_ne!(pgid, own, "child must not share the caller's group");
    }

    #[cfg(unix)]
    /// 进程组 `pgid` 是否仍存在（至少还有一个成员）。
    ///
    /// 用 `kill(pgid, 0)` 探测，避免 `ps` 全文扫描的自匹配与竞态。
    fn group_still_alive(pgid: i32) -> bool {
        unsafe { libc::kill(-pgid, 0) == 0 }
    }

    #[cfg(unix)]
    /// 进程 `pid` 是否仍存在。`kill(pid, 0)` 探测：`EPERM` 也算存在（无权限≠不存在），
    /// 只有 `ESRCH` 才是真的没了。
    fn pid_alive(pid: i32) -> bool {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_timeout_kills_the_whole_process_group() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // 命令把自身 pgid 写入文件，便于测试精确探测该组是否仍存活。
        let pgid_file = tmp.path().join("pgid.txt");
        let cmd = format!(
            "ps -o pgid= -p $$ | tr -d ' ' > {}; sh -c 'while :; do :; done' & sleep 30",
            pgid_file.display()
        );
        let result = tool
            .call(serde_json::json!({
                "command": cmd,
                "timeout": 1,
                "expected_timeout_sec": 1
            }))
            .await;
        assert!(result.is_error, "should report timeout");

        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .expect("command wrote its pgid")
            .trim()
            .parse()
            .expect("pgid is numeric");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            !group_still_alive(pgid),
            "timeout must reap the whole group (pgid {pgid} still alive)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_normal_exit_keeps_background_process_alive() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // 命令：前台先同步写下后台进程的 pid，再退出；后台进程长睡着。
        //
        // 前台同步写 pid 是关键——这样 pid 一定可读，断言才能精确区分「后台进程被
        // 误杀」与「后台进程压根没起来」这两种截然不同的故障。若让后台进程自己晚点
        // 写，两者症状相同（文件不出现），就无法归因。
        //
        // 只用 `sh`（它在 PATH 上，否则本用例的 `sh -c` 根本起不来）与 /bin 的
        // `sleep`：不依赖 /usr/bin 的 `nohup`/`touch`，避免受限 PATH 下解析失败又被
        // `>/dev/null 2>&1` 吞掉、外层仍 `exit 0` 的静默误报。
        let pid_file = tmp.path().join("bg.pid");
        let cmd = format!(
            "sh -c 'sleep 30' >/dev/null 2>&1 & echo $! > {}; exit 0",
            pid_file.display()
        );
        let result = tool
            // 显式给出 expected_timeout_sec：正常结束时后台进程仍攥着管道，
            // reader_wait 需等到 idle_limit 才放弃；不设则默认 180s，测试会久等。
            .call(serde_json::json!({
                "command": cmd,
                "timeout": 5,
                "expected_timeout_sec": 3
            }))
            .await;
        assert!(
            !result.is_error,
            "normal exit expected: {:?}",
            result.content
        );

        // 等前台 shell 写下后台 pid（只是等「它已写出」这一正面事件，上限给足）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !pid_file.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or_else(|e| {
                panic!(
                    "front-end shell never wrote the background pid ({e}); cmd={cmd:?} result={:?}",
                    result.content
                )
            });

        // 核心断言：调用已返回，后台进程必须仍活着。这是被测属性本身，
        // 确定且即时——不再依赖「等它稍后写一个文件」这种间接代理。
        assert!(
            pid_alive(pid),
            "background process (pid {pid}) must survive normal exit; cmd={cmd:?}"
        );

        // 收尾：不留 `sleep 30` 给后续测试添乱。
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_cancel_reaps_the_whole_process_group() {
        let tmp = TempDir::new().unwrap();
        let tool = Arc::new(make_tool(&tmp));

        let pgid_file = tmp.path().join("pgid.txt");
        let cmd = format!(
            "ps -o pgid= -p $$ | tr -d ' ' > {}; sh -c 'while :; do :; done' & sleep 30",
            pgid_file.display()
        );
        let task = tokio::spawn({
            let tool = tool.clone();
            let cmd = cmd.clone();
            async move {
                tool.call(serde_json::json!({ "command": cmd, "timeout": 30 }))
                    .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let pgid: i32 = std::fs::read_to_string(&pgid_file)
            .expect("command wrote its pgid")
            .trim()
            .parse()
            .expect("pgid is numeric");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(
            !group_still_alive(pgid),
            "cancel must reap the whole group (pgid {pgid} still alive)"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bash_self_kill_by_process_group_does_not_kill_the_caller() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // 复刻事故：命令内杀自身进程组。隔离后不应伤到调用方（本测试进程）。
        // 外层用 `sleep` 制造组内成员，再由组内 sh 执行 kill -TERM -$PGID。
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            tool.call(serde_json::json!({
                "command": "PGID=$(ps -o pgid= -p $$ | tr -d ' '); kill -TERM -$PGID; echo survived",
                "timeout": 5,
                "expected_timeout_sec": 2
            })),
        )
        .await;
        // 只要能拿到结果，就说明调用方（本测试进程）没被信号带走。
        assert!(result.is_ok(), "caller must survive a self group-kill");
    }

    #[tokio::test]
    async fn bash_injects_the_session_env_into_the_child() {
        let tmp = TempDir::new().unwrap();
        let ctx = Arc::new(ToolsContext::with_roots_and_env(
            vec![tmp.path().to_path_buf()],
            vec![("YI_AGENT_THREAD_ID".to_string(), "thread-abc".to_string())],
        ));
        let tool = BashTool::new(ctx);
        let result = tool
            .call(serde_json::json!({"command": "echo $YI_AGENT_THREAD_ID"}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(
                s.contains("thread-abc"),
                "child must see the injected env: {s}"
            );
        } else {
            panic!("expected text block");
        }
    }

    #[tokio::test]
    async fn bash_without_a_session_env_leaves_the_variable_unset() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let result = tool
            .call(serde_json::json!({"command": "echo \"[$YI_AGENT_THREAD_ID]\""}))
            .await;
        assert!(!result.is_error);
        if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
            assert!(
                s.contains("[]"),
                "no session env means an empty expansion: {s}"
            );
        } else {
            panic!("expected text block");
        }
    }
}
