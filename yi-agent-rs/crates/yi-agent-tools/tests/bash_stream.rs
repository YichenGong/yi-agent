use std::sync::Arc;
use std::time::Duration;
use yi_agent_core::{OutputStream, Tool, ToolEvent};
use yi_agent_tools::{BashTool, ToolsContext};

fn make_tool() -> BashTool {
    let cwd = std::env::temp_dir();
    BashTool::new(Arc::new(ToolsContext::new(cwd)))
}

#[tokio::test]
async fn test_bash_stream_emits_stdout_delta() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let result = tool
        .call_stream(serde_json::json!({"command": "echo hello"}), tx)
        .await;
    assert!(!result.is_error);
    let mut got_hello = false;
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ToolEvent::OutputDelta {
                stream: OutputStream::Stdout,
                text,
            } if text.contains("hello") => {
                got_hello = true;
            }
            _ => {}
        }
    }
    assert!(got_hello, "expected stdout delta containing 'hello'");
}

#[tokio::test]
async fn test_bash_stream_emits_exit_code() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let _ = tool
        .call_stream(serde_json::json!({"command": "true"}), tx)
        .await;
    let mut exit_code = None;
    while let Ok(ev) = rx.try_recv() {
        if let ToolEvent::Exit { code } = ev {
            exit_code = code;
        }
    }
    assert_eq!(exit_code, Some(0));
}

#[tokio::test]
async fn test_bash_stream_expected_timeout_kills_on_no_output() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let start = std::time::Instant::now();
    let result = tool
        .call_stream(
            serde_json::json!({"command": "sleep 10", "expected_timeout_sec": 1}),
            tx,
        )
        .await;
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "watchdog should kill within ~1.5s, took {elapsed:?}"
    );
    assert!(result.is_error);
    let mut got_timeout = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, ToolEvent::Timeout) {
            got_timeout = true;
        }
    }
    assert!(got_timeout, "expected Timeout event");
}

#[tokio::test]
async fn test_bash_stream_expected_timeout_allows_long_output() {
    let tool = make_tool();
    let (tx, _rx) = tokio::sync::mpsc::channel::<ToolEvent>(64);
    let cmd = "for i in $(seq 1 15); do echo $i; sleep 0.2; done";
    let result = tool
        .call_stream(
            serde_json::json!({"command": cmd, "expected_timeout_sec": 1}),
            tx,
        )
        .await;
    assert!(
        !result.is_error,
        "should complete despite exceeding expected_timeout, because output resets watchdog"
    );
}

#[tokio::test]
async fn test_bash_stream_emits_truncated_event() {
    let tool = make_tool();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<ToolEvent>(256);
    // ~200KB stdout, MAX_OUTPUT_BYTES is 100KB
    let result = tool
        .call_stream(
            serde_json::json!({"command": "yes hello | head -c 200000"}),
            tx,
        )
        .await;
    assert!(!result.is_error);
    let mut got_truncated = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(
            ev,
            ToolEvent::Truncated {
                stream: OutputStream::Stdout,
                ..
            }
        ) {
            got_truncated = true;
        }
    }
    assert!(got_truncated, "expected Truncated event for stdout");
}

/// Regression: a timed-out bash call must not leave busy-looping
/// grandchildren behind. The original incident used `xargs -P4` to fan out
/// CPU-spinning shells; killing only the direct child orphaned them.
#[cfg(unix)]
#[tokio::test]
async fn bash_tool_does_not_leak_busy_loop_after_timeout() {
    use std::sync::Arc;
    use tempfile::TempDir;
    use yi_agent_core::Tool;
    use yi_agent_tools::{BashTool, ToolsContext};

    let tmp = TempDir::new().unwrap();
    let tool = BashTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())));

    // 复刻事故命令形态：xargs 并发拉起空转进程；命令写出自身 pgid 供探测。
    let pgid_file = tmp.path().join("pgid.txt");
    let cmd = format!(
        "ps -o pgid= -p $$ | tr -d ' ' > {}; seq 1 4 | xargs -P4 -I{{}} sh -c 'while :; do :; done'; sleep 30",
        pgid_file.display()
    );
    let result = tool
        .call(serde_json::json!({
            "command": cmd,
            "timeout": 1,
            "expected_timeout_sec": 1
        }))
        .await;
    assert!(result.is_error, "expected timeout");

    let pgid: i32 = std::fs::read_to_string(&pgid_file)
        .expect("command wrote its pgid")
        .trim()
        .parse()
        .expect("pgid is numeric");
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let alive = unsafe { libc::kill(-pgid, 0) == 0 };
    assert!(!alive, "no busy loop may survive the timeout (pgid {pgid})");
}
