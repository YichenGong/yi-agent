//! Git Diff 守望者的回归测试。
//!
//! 与上面的主题守望者同理:直接驱动生产里的 [`pump_git_diff_notifications`]。
//! 新增的写帧表尤其必须这样测——工具在模型侧算好的帧能不能到 `thread/view/read`
//! 手里,全取决于这段循环里那次 `insert`。

use super::*;

fn frame(command: &str) -> crate::git_diff::ViewFrame {
    crate::git_diff::ViewFrame {
        command: Some(command.to_string()),
        as_kind: "log".to_string(),
        exit_code: Some(0),
        stdout: "cafe123 subject\n".to_string(),
        stderr: String::new(),
        truncated: false,
        timed_out: false,
        note: None,
        base: None,
        base_kind: String::new(),
    }
}

/// D3 的另一半:工具报的「我给你看了什么」必须与面板真读到的帧一致。
/// 工具算帧、watcher 落表,谁漏了这一步,面板都会显示旧帧(或空帧),
/// 而回执照样声称展示了命令输出——正是要杜绝的误导。
#[tokio::test]
async fn the_watcher_files_the_frame_under_its_thread() {
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let frames: ViewFrames = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let git_diff = crate::git_diff_tool::GitDiffHandle::new();
    let pump = tokio::spawn(pump_git_diff_notifications(
        git_diff.subscribe(),
        Arc::clone(&hub),
        Arc::clone(&frames),
    ));

    git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
        thread_id: Some("thread-1".into()),
        command: Some("git log --oneline -1".into()),
        note: Some("here".into()),
        frame: Some(Box::new(frame("git log --oneline -1"))),
    });

    let mut filed = None;
    for _ in 0..200 {
        if let Some(f) = frames.lock().await.get("thread-1").cloned() {
            filed = Some(f);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    pump.abort();
    let filed = filed.expect("the watcher must file the frame the tool computed");
    assert_eq!(filed.command.as_deref(), Some("git log --oneline -1"));
    assert_eq!(filed.as_kind, "log");
}

/// 无 thread 的焦点(前端按当前会话处理)不进表:键都没有,写进去只会有个
/// 取不出来的孤儿条目。
#[tokio::test]
async fn a_frameless_or_threadless_focus_files_nothing() {
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let frames: ViewFrames = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
    let git_diff = crate::git_diff_tool::GitDiffHandle::new();
    let pump = tokio::spawn(pump_git_diff_notifications(
        git_diff.subscribe(),
        Arc::clone(&hub),
        Arc::clone(&frames),
    ));

    git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
        thread_id: None,
        command: None,
        note: None,
        frame: Some(Box::new(frame("git log"))),
    });
    git_diff.broadcast(crate::git_diff_tool::GitDiffFocus {
        thread_id: Some("thread-2".into()),
        command: None,
        note: None,
        frame: None,
    });
    // 让守望者把两条都处理完,再断言表是空的。
    let mut drained = false;
    for _ in 0..200 {
        if git_diff.subscribe().try_recv().is_err() && frames.lock().await.is_empty() {
            drained = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    pump.abort();
    assert!(drained, "the frame table must stay empty");
    assert!(frames.lock().await.is_empty());
}
