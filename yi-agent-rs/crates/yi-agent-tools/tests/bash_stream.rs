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
///
/// **Self-reaping watchdog.** The command carries a same-group watchdog
/// subshell that reaps the whole group on its own after
/// [`WATCHDOG_BUDGET`]. This is the only layer that survives the test
/// process being SIGKILLed: `ProcessGroupGuard::drop`, `atexit` and
/// `TempDir`'s cleanup all need a living process, so a hard-killed test
/// (cargo timeout, CI cancel, editor "stop") used to orphan the busy loops
/// under launchd forever — that actually happened on 2026-11-02 and spun
/// four cores for ~22 hours.
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
    //
    // 开头的后台子 shell 是兜底看门狗：它与忙等进程同组（bash 工具让子进程
    // 自成组，pgid == 组长 pid == 这里的 `$$`），`kill -9 -$$` 一次带走全组
    // **包括它自己**。它只依赖自己那 WATCHDOG_BUDGET 秒，不依赖本测试进程
    // 存活，故测试被 SIGKILL 也能回收。
    //
    // **不能加「组长还活着才动手」之类的守卫**：实测该守卫是反的——组长被
    // 单独杀掉、孙进程仍存活时，`kill -0 $$` 失败，看门狗会以为「组已空」而
    // 放弃，泄漏照旧发生（2026-11-02 消融实验实测踩到）。无条件 `kill -9 -$$`
    // 是安全的：看门狗自己就在这个组里，只要它还活着，该 pgid 就不可能被内核
    // 回收复用给别的进程组，因此不存在误杀无关进程的可能。
    let pgid_file = tmp.path().join("pgid.txt");
    let cmd = format!(
        "( sleep {budget}; kill -9 -$$ ) >/dev/null 2>&1 & \
         ps -o pgid= -p $$ | tr -d ' ' > {pgid}; \
         seq 1 4 | xargs -P4 -I{{}} sh -c 'while :; do :; done'; \
         sleep 30",
        budget = WATCHDOG_BUDGET,
        pgid = pgid_file.display()
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

    // 轮询而不是固定 sleep：慢机器上 SIGKILL 投递与收割可能超过 400ms，
    // 固定窗口会抖；而硬上限远小于看门狗预算，实现回退时必须在看门狗
    // 出手之前就判失败，否则测的是看门狗而不是被测实现。
    let start = std::time::Instant::now();
    while unsafe { libc::kill(-pgid, 0) == 0 } {
        let elapsed = start.elapsed();
        if elapsed >= REAP_DEADLINE {
            panic!(
                "busy loops still alive {}ms after the timeout \
                 (pgid {pgid}); the group was not reaped",
                elapsed.as_millis()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// How long the in-command watchdog waits before reaping the whole group.
///
/// Must be comfortably longer than [`REAP_DEADLINE`] so the watchdog stays
/// out of the way on the normal path (the tool's own 1s timeout does the
/// reaping), and only acts as a backstop when that fails.
#[cfg(unix)]
const WATCHDOG_BUDGET: u32 = 5;

/// Upper bound on how long we wait for the group to disappear before
/// failing. Deliberately far below [`WATCHDOG_BUDGET`]: a regression must be
/// caught by this assertion, not masked by the watchdog.
#[cfg(unix)]
const REAP_DEADLINE: Duration = Duration::from_secs(2);

#[cfg(unix)]
const POLL_INTERVAL: Duration = Duration::from_millis(50);
