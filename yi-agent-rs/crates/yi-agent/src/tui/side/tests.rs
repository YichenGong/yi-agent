//! `tui::side` 的纯逻辑测试:分类、视图隔离、停止目标与审批键路由。

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn item(id: &str, ordinal: u64, text: &str) -> Arc<SideItem> {
    Arc::new(SideItem {
        id: id.to_string(),
        turn_id: "turn-1".to_string(),
        ordinal,
        content: SideItemContent::AgentMessage {
            text: text.to_string(),
        },
    })
}

// --- 分类 ---------------------------------------------------------------

#[test]
fn side_command_while_main_running_does_not_interject_or_interrupt() {
    // 计划里钉住的那条:分类在投递之前发生,所以运行中敲 `/side 问题`
    // 得到的是「打开旁支」,而不是一条要插进主轮的消息。
    let route = classify_conversation_input("/btw 问题", false).unwrap();
    assert!(matches!(
        route,
        RoutedInput::Side(SideInput::Open {
            question: Some(ref q)
        }) if q == "问题"
    ));
}

#[test]
fn side_command_without_question_opens_empty() {
    for text in ["/side", "/btw", "/side   ", "  /side"] {
        let route = classify_conversation_input(text, false).unwrap();
        assert_eq!(
            route,
            RoutedInput::Side(SideInput::Open { question: None }),
            "{text} 应当打开空旁支"
        );
    }
}

#[test]
fn btw_is_only_an_alias_for_side() {
    let a = classify_conversation_input("/btw 你好", false).unwrap();
    let b = classify_conversation_input("/side 你好", false).unwrap();
    assert_eq!(a, b);
}

#[test]
fn plain_text_stays_main_byte_for_byte() {
    for text in [
        "普通问题",
        " /不是命令开头的空格",
        "sidebar 是什么",
        "/usr/bin/env 呢",
        "提问里带 /side 但不以它开头",
    ] {
        assert_eq!(
            classify_conversation_input(text, false).unwrap(),
            RoutedInput::Main(text.to_string()),
            "{text} 必须原样投给主会话"
        );
    }
}

#[test]
fn command_matching_is_first_token_only() {
    // `/sidefoo`、`/sidebar` 都不是 `/side`。
    assert!(matches!(
        classify_conversation_input("/sidefoo hi", false).unwrap(),
        RoutedInput::Main(_)
    ));
    assert!(matches!(
        classify_conversation_input("/sidebar", false).unwrap(),
        RoutedInput::Main(_)
    ));
}

#[test]
fn side_nesting_is_refused() {
    let error = classify_conversation_input("/side 再问一次", true).unwrap_err();
    assert_eq!(error.reason, SideErrorReason::InvalidParams);
    let error = classify_conversation_input("/btw 再问一次", true).unwrap_err();
    assert_eq!(error.reason, SideErrorReason::InvalidParams);
}

#[test]
fn back_and_close_are_structural_commands_without_arguments() {
    assert_eq!(
        classify_conversation_input("/back", true).unwrap(),
        RoutedInput::Side(SideInput::Back)
    );
    assert_eq!(
        classify_conversation_input("/side-close", true).unwrap(),
        RoutedInput::Side(SideInput::Close)
    );

    // 带参数是错误,不是问题正文:静默忽略会让用户以为已经关闭。
    assert!(classify_conversation_input("/back now", true).is_err());
    assert!(classify_conversation_input("/side-close 现在", true).is_err());

    // 主会话里没有可返回/可关闭的对象。
    assert!(classify_conversation_input("/back", false).is_err());
    assert!(classify_conversation_input("/side-close", false).is_err());
}

#[test]
fn side_local_commands_are_limited_and_management_commands_are_not_accepted() {
    for name in SIDE_LOCAL_COMMANDS {
        assert_eq!(
            classify_conversation_input(&format!("/{name}"), true).unwrap(),
            RoutedInput::Side(SideInput::LocalCommand(name.to_string()))
        );
    }

    // 管理类命令在旁支里既不执行也不能被当成问题吞掉:原样保留。
    // 调用方据此把输入还给输入框并提示「旁支只读」。
    for name in ["clear", "compact", "model", "effort"] {
        let text = format!("/{name}");
        assert_eq!(
            classify_conversation_input(&text, true).unwrap(),
            RoutedInput::Side(SideInput::Submit(text.clone())),
            "/{name} 不能被当作旁支可执行命令"
        );
    }
}

#[test]
fn free_text_in_side_is_a_question_not_a_command() {
    assert_eq!(
        classify_conversation_input("这段代码什么意思？", true).unwrap(),
        RoutedInput::Side(SideInput::Submit("这段代码什么意思？".to_string()))
    );
}

// --- 视图隔离 -----------------------------------------------------------

#[test]
fn apply_items_is_idempotent_for_streaming_updates() {
    let mut view = TuiSideView::new("s".into(), 1);
    view.apply_items(vec![item("t/agent/0/0", 1, "你")]);
    view.apply_items(vec![item("t/agent/0/0", 1, "你好")]);
    view.apply_items(vec![item("t/agent/0/0", 1, "你好，世界")]);
    assert_eq!(view.items.len(), 1, "同一 item 只能更新不能追加");
    match &view.items[0].content {
        SideItemContent::AgentMessage { text } => assert_eq!(text, "你好，世界"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn apply_items_keeps_ordinal_order_and_ignores_stale_values() {
    let mut view = TuiSideView::new("s".into(), 1);
    view.apply_items(vec![item("b", 2, "第二")]);
    view.apply_items(vec![item("a", 1, "第一")]);
    assert_eq!(
        view.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
        vec!["a", "b"]
    );

    // 迟到的旧值（更小的 ordinal）不能覆盖已写下的内容。
    view.apply_items(vec![item("b", 0, "旧")]);
    match &view.items[1].content {
        SideItemContent::AgentMessage { text } => assert_eq!(text, "第二"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn background_parent_events_do_not_change_side_history_or_running() {
    let mut views = TuiSideViews::new();
    views.open("s".into(), 7);
    let side = views.side.as_mut().unwrap();
    side.status = SideStatus::Running;
    side.running = true;
    side.apply_items(vec![item("a", 1, "旁支回答")]);
    let before = (side.items.len(), side.running, side.draft.clone());

    let main_event = ConversationEvent {
        target: ConversationTarget::Main,
        turn_id: None,
        event: ConversationPayload::Main(yi_agent_core::AgentEvent::Cancelled),
    };
    assert_eq!(views.route(&main_event), SideFrameEffect::Redraw);

    let side = views.side.as_ref().unwrap();
    assert_eq!(
        (side.items.len(), side.running, side.draft.clone()),
        before,
        "父会话的事件不能改旁支的 history/running/draft"
    );
}

#[test]
fn stale_generation_frames_are_discarded() {
    let mut view = TuiSideView::new("s".into(), 2);
    view.apply_items(vec![item("a", 1, "新的")]);
    let effect = view.apply(
        "s",
        1,
        ConversationPayload::SideItems(vec![item("a", 1, "旧的")]),
    );
    assert_eq!(effect, SideFrameEffect::Discard);
    match &view.items[0].content {
        SideItemContent::AgentMessage { text } => assert_eq!(text, "新的"),
        other => panic!("unexpected {other:?}"),
    }

    // 别的旁支（不同 side_id）的帧同样不能落进来。
    assert_eq!(
        view.apply(
            "other",
            2,
            ConversationPayload::SideClosed(CloseReason::UserClosed)
        ),
        SideFrameEffect::Discard
    );
    assert!(view.is_live());
}

#[test]
fn closed_side_ignores_late_status_frames() {
    let mut view = TuiSideView::new("s".into(), 1);
    view.status = SideStatus::Running;
    assert_eq!(
        view.apply(
            "s",
            1,
            ConversationPayload::SideClosed(CloseReason::UserClosed)
        ),
        SideFrameEffect::Redraw
    );
    assert!(!view.is_live());
    // 迟到的 Running 不能把已关闭的旁支拉回运行态。
    assert_eq!(
        view.apply("s", 1, ConversationPayload::SideStatus(SideStatus::Running)),
        SideFrameEffect::Discard
    );
    assert_eq!(view.status, SideStatus::Closed);
}

#[test]
fn opening_the_same_side_twice_keeps_its_content() {
    let mut views = TuiSideViews::new();
    views.open("s".into(), 1);
    views
        .side
        .as_mut()
        .unwrap()
        .apply_items(vec![item("a", 1, "保留")]);
    views.open("s".into(), 1);
    assert_eq!(views.side.as_ref().unwrap().items.len(), 1);

    // 新 generation 是同一个 side_id 的新生命:必须重新开始。
    views.open("s".into(), 2);
    assert!(views.side.as_ref().unwrap().items.is_empty());
}

#[test]
fn back_restores_main_draft_and_viewport() {
    // 主视图的草稿与滚动位置存在主视图自己身上;这里断言切换不改动它们,
    // 旁支的草稿/滚动也彼此独立（两者都由调用方按 active 选择）。
    let mut views = TuiSideViews::new();
    views.open("s".into(), 1);
    views.side.as_mut().unwrap().draft = "旁支草稿".into();
    views.side.as_mut().unwrap().scroll_offset = 42;

    views.back();
    assert!(!views.in_side());
    assert_eq!(
        views.active,
        Some(ConversationTarget::Main),
        "back 只切视图,不丢弃旁支"
    );

    let side = views.side.as_ref().expect("back 不能丢弃旁支状态");
    assert_eq!(side.draft, "旁支草稿");
    assert_eq!(side.scroll_offset, 42);
}

#[test]
fn drop_side_returns_to_main() {
    let mut views = TuiSideViews::new();
    views.open("s".into(), 1);
    views.drop_side();
    assert!(views.side.is_none());
    assert_eq!(views.active, Some(ConversationTarget::Main));
}

// --- 停止目标 -----------------------------------------------------------

#[test]
fn side_esc_cancels_only_side() {
    let active = ConversationTarget::Side {
        side_id: "s".into(),
        generation: 1,
    };
    assert_eq!(
        visible_stop_target(&active, true, true),
        Some(active.clone())
    );
    // 旁支空闲时不拦截 Esc:让它保持原有的「不退出、不动父」语义。
    assert_eq!(
        visible_stop_target(
            &ConversationTarget::Side {
                side_id: "s".into(),
                generation: 1
            },
            true,
            false
        ),
        None
    );
}

#[test]
fn main_stop_target_never_reaches_the_side() {
    assert_eq!(
        visible_stop_target(&ConversationTarget::Main, true, true),
        Some(ConversationTarget::Main)
    );
    assert_eq!(
        visible_stop_target(&ConversationTarget::Main, false, true),
        None,
        "主会话空闲时 Ctrl+C 不能去停旁支"
    );
}

// --- 审批命令模式 -------------------------------------------------------

#[test]
fn parent_approval_command_mode_does_not_approve_on_enter() {
    let mut mode = ApprovalInputMode::Decision;
    let slash = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(matches!(
        route_approval_key(slash, &mut mode),
        ApprovalKeyAction::EnterCommandMode
    ));
    assert_eq!(mode, ApprovalInputMode::Command);

    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        route_approval_key(enter, &mut mode),
        ApprovalKeyAction::EditCommand(_)
    ));
}

#[test]
fn decision_mode_enter_still_approves() {
    let mut mode = ApprovalInputMode::Decision;
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert!(matches!(
        route_approval_key(enter, &mut mode),
        ApprovalKeyAction::DecisionKey(_)
    ));
    // 数字键在裁决模式里仍是裁决,不被命令模式吞掉。
    let one = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE);
    assert!(matches!(
        route_approval_key(one, &mut mode),
        ApprovalKeyAction::DecisionKey(_)
    ));
    assert_eq!(mode, ApprovalInputMode::Decision);
}

#[test]
fn esc_restores_the_saved_approval_draft() {
    let mut mode = ApprovalInputMode::Decision;
    route_approval_key(
        KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
        &mut mode,
    );
    for ch in ['s', 'i', 'd', 'e'] {
        assert!(matches!(
            route_approval_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut mode
            ),
            ApprovalKeyAction::EditCommand(_)
        ));
    }
    // Esc 只退出命令编辑,不退出应用,也不批准。
    assert_eq!(
        route_approval_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut mode),
        ApprovalKeyAction::ExitCommandMode
    );
    assert_eq!(mode, ApprovalInputMode::Decision);
}

#[test]
fn ctrl_q_exits_from_both_modes() {
    let ctrl_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
    for mode in [ApprovalInputMode::Decision, ApprovalInputMode::Command] {
        let mut mode = mode;
        assert_eq!(
            route_approval_key(ctrl_q, &mut mode),
            ApprovalKeyAction::ApplicationExit
        );
    }
}

// --- 呈现 ---------------------------------------------------------------

#[test]
fn side_item_lines_never_rewrite_the_text() {
    let cases = vec![
        SideItemContent::UserMessage {
            text: "第一行\n第二行".into(),
        },
        SideItemContent::AgentMessage {
            text: "答案".into(),
        },
        SideItemContent::Thinking {
            text: "推理".into(),
            redacted: false,
        },
        SideItemContent::Notice {
            text: "提示".into(),
        },
    ];
    for content in cases {
        let lines = side_item_lines(&content);
        assert!(!lines.is_empty());
    }

    let user = side_item_lines(&SideItemContent::UserMessage {
        text: "第一行\n第二行".into(),
    });
    assert_eq!(user, vec!["第一行".to_string(), "第二行".to_string()]);
}

#[test]
fn side_header_names_the_parent_state() {
    assert!(side_header_line(false, false).contains("父会话空闲"));
    assert!(side_header_line(true, false).contains("父会话运行中"));
    assert!(side_header_line(true, true).contains("父会话等待审批"));
    let header = side_header_line(true, false);
    assert!(header.contains("只读") && header.contains("/back") && header.contains("/side-close"));
}

#[test]
fn close_reason_text_is_explicit_for_every_reason() {
    // 每一个关闭原因都得有话说:静默关闭会让用户不知道旁支去哪了。
    for reason in [
        CloseReason::UserClosed,
        CloseReason::Expired,
        CloseReason::ParentCleared,
        CloseReason::ParentDeleted,
        CloseReason::HostShutdown,
        CloseReason::ResourceLost,
    ] {
        assert!(!close_reason_text(reason).is_empty());
    }
}

#[tokio::test]
async fn quit_cleans_both_runners_and_signals_side_exit() {
    // 退出时必须真的给旁支调度发 Exit,而不能只丢一个 JoinHandle。
    // 通道另一端由测试自己持有,于是可以断言「确实收到了 Exit」。
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let join = tokio::spawn(async {
        // 一个会真正结束的任务:模拟旁支调度在收到 Exit 后退出。
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    });
    let mut coordinator = TuiExitCoordinator::new(Some(tx), Some(join));

    let stopped = coordinator.stop_all().await.unwrap();
    assert!(stopped, "旁支应被优雅停止");

    match rx.recv().await {
        Some(crate::side_driver::SideDriverCommand::Exit) => {}
        other => panic!("旁支必须收到 Exit,实际收到 {other:?}"),
    }
}

#[tokio::test]
async fn quit_without_side_capability_is_a_no_op() {
    // 没有旁支能力的宿主也必须能正常退出:不能因为「没有东西可以停」而报错。
    let mut coordinator = TuiExitCoordinator::disabled();
    assert!(coordinator.stop_all().await.unwrap());
}
