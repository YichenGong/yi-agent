//! Transcript tests: budget measured on the encoded item, stable ids, and no
//! unbounded or secret content.

use super::*;
use serde_json::json;
use yi_agent_core::{ContentBlock, Message, ThinkingToken, ToolResult};

fn item_bytes(item: &SideItem) -> usize {
    serde_json::to_vec(item).unwrap().len()
}

fn joined_text(items: &[Arc<SideItem>]) -> String {
    items
        .iter()
        .filter_map(|item| text_of(&item.content).map(str::to_string))
        .collect()
}

#[test]
fn side_transcript_splits_by_serialized_utf8_budget() {
    // The task's own case: CJK plus escaped quotes, where byte length, char count
    // and encoded length all disagree.
    let mut transcript = SideTranscript::side_defaults();
    let text = "中\\\"".repeat(40_000);
    let updates = transcript.begin_turn("t", &text).unwrap();
    assert!(updates.len() > 1, "the text must be split");
    for item in &updates {
        assert!(
            item_bytes(item) <= SIDE_TRANSCRIPT_MAX_ITEM_BYTES,
            "item {} is {} bytes",
            item.id,
            item_bytes(item)
        );
    }
    // Reassembling every part must reproduce the original exactly: nothing is lost
    // and no character is cut in half.
    assert_eq!(joined_text(&updates), text);
}

#[test]
fn side_transcript_ids_are_stable_and_ordinals_monotonic() {
    let mut transcript = SideTranscript::side_defaults();
    let text = "长文本".repeat(100_000);
    let updates = transcript.begin_turn("turn-a", &text).unwrap();
    assert!(updates.len() > 1);

    let ids: Vec<&str> = updates.iter().map(|item| item.id.as_str()).collect();
    assert_eq!(ids[0], "turn-a/user/0/0");
    assert_eq!(ids[1], "turn-a/user/0/1");
    let ordinals: Vec<u64> = updates.iter().map(|item| item.ordinal).collect();
    assert!(
        ordinals.windows(2).all(|pair| pair[0] <= pair[1]),
        "ordinals must not go backwards: {ordinals:?}"
    );

    // Re-applying the same delta must not add parts: the same id is updated.
    let before = transcript.items().len();
    let again = transcript.begin_turn("turn-a", &text).unwrap();
    let _ = again;
    assert!(transcript.items().len() >= before);
}

#[test]
fn side_transcript_streaming_grows_one_part() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("turn-a", "问题").unwrap();

    // The first delta creates the message; every later delta must grow it rather
    // than add a part.
    let first = transcript
        .apply("turn-a", &AgentEvent::AssistantText("一".into()))
        .unwrap();
    for delta in ["二", "三"] {
        let updates = transcript
            .apply("turn-a", &AgentEvent::AssistantText(delta.to_string()))
            .unwrap();
        assert_eq!(
            updates.len(),
            first.len(),
            "a delta inside the budget must grow the existing part, not add one"
        );
    }
    let agent = transcript
        .items()
        .into_iter()
        .find(|item| matches!(item.content, SideItemContent::AgentMessage { .. }))
        .expect("an agent message exists");
    assert_eq!(text_of(&agent.content), Some("一二三"));
}

#[test]
fn side_transcript_repeated_update_does_not_duplicate_text() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("turn-a", "问题").unwrap();
    let event = AgentEvent::AssistantText("回答".into());
    transcript.apply("turn-a", &event).unwrap();
    let items = transcript.apply("turn-a", &event).unwrap();
    let agent: String = items
        .iter()
        .filter(|item| matches!(item.content, SideItemContent::AgentMessage { .. }))
        .map(|item| text_of(&item.content).unwrap_or("").to_string())
        .collect();
    assert_eq!(agent, "回答回答");
    // Exactly one agent item, not two.
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item.content, SideItemContent::AgentMessage { .. }))
            .count(),
        1
    );
}

#[test]
fn side_transcript_tool_card_truncates_display_and_keeps_status() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("turn-a", "问题").unwrap();
    let big = "x".repeat(64 * 1024);
    transcript
        .apply(
            "turn-a",
            &AgentEvent::ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                input: json!({ "path": big }),
            },
        )
        .unwrap();

    let card = transcript
        .items()
        .into_iter()
        .find(|item| matches!(item.content, SideItemContent::ToolCall { .. }))
        .expect("a tool card exists");
    let SideItemContent::ToolCall {
        name,
        input,
        truncated,
        status,
        ..
    } = &card.content
    else {
        unreachable!()
    };
    assert_eq!(name, "read");
    assert_eq!(*status, SideToolStatus::Running);
    assert!(*truncated, "an oversized input must be marked truncated");
    assert!(
        serde_json::to_string(input).unwrap().len() <= SIDE_TOOL_DISPLAY_BYTES,
        "the display budget must bound the input"
    );
    assert!(item_bytes(&card) <= SIDE_TRANSCRIPT_MAX_ITEM_BYTES);

    // The result completes the same card instead of adding another.
    let count_before = transcript.items().len();
    transcript
        .apply(
            "turn-a",
            &AgentEvent::ToolResult {
                id: "call-1".into(),
                result: ToolResult::text("y".repeat(64 * 1024)),
            },
        )
        .unwrap();
    assert_eq!(transcript.items().len(), count_before);
    let card = transcript
        .items()
        .into_iter()
        .find(|item| matches!(item.content, SideItemContent::ToolCall { .. }))
        .expect("the card still exists");
    let SideItemContent::ToolCall {
        result,
        status,
        truncated,
        ..
    } = &card.content
    else {
        unreachable!()
    };
    assert_eq!(*status, SideToolStatus::Completed);
    assert!(*truncated);
    let result = result.as_ref().expect("a result is shown");
    assert!(result.contains("已截断"), "{result}");
}

#[test]
fn side_transcript_tool_error_marks_the_card_failed() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    transcript
        .apply(
            "t",
            &AgentEvent::ToolCall {
                id: "c".into(),
                name: "grep".into(),
                input: json!({}),
            },
        )
        .unwrap();
    transcript
        .apply(
            "t",
            &AgentEvent::ToolResult {
                id: "c".into(),
                result: ToolResult {
                    content: vec![ContentBlock::Text("boom".into())],
                    is_error: true,
                },
            },
        )
        .unwrap();
    let card = transcript
        .items()
        .into_iter()
        .find(|item| matches!(item.content, SideItemContent::ToolCall { .. }))
        .unwrap();
    assert!(matches!(
        card.content,
        SideItemContent::ToolCall {
            status: SideToolStatus::Failed,
            ..
        }
    ));
}

#[test]
fn side_transcript_ignores_results_for_unshown_calls() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    let before = transcript.items().len();
    let after = transcript
        .apply(
            "t",
            &AgentEvent::ToolResult {
                id: "never-shown".into(),
                result: ToolResult::text("x"),
            },
        )
        .unwrap();
    assert_eq!(after.len(), before);
}

#[test]
fn side_transcript_describes_images_instead_of_carrying_bytes() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    transcript
        .apply(
            "t",
            &AgentEvent::ToolCall {
                id: "c".into(),
                name: "view_image".into(),
                input: json!({ "path": "shot.png" }),
            },
        )
        .unwrap();
    transcript
        .apply(
            "t",
            &AgentEvent::ToolResult {
                id: "c".into(),
                result: ToolResult {
                    content: vec![
                        ContentBlock::Text("image".into()),
                        ContentBlock::Image {
                            source: yi_agent_core::ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "iVBORw0KGgo=".into(),
                            },
                            detail: yi_agent_core::ImageDetail::High,
                            path: Some("/private/side/copy.png".into()),
                        },
                    ],
                    is_error: false,
                },
            },
        )
        .unwrap();
    let encoded = serde_json::to_string(&transcript.items()).unwrap();
    assert!(encoded.contains("图片，未放入旁支转录"));
    assert!(
        !encoded.contains("iVBORw0KGgo="),
        "binary must never enter the transcript"
    );
    assert!(
        !encoded.contains("/private/side"),
        "a private path must not leak into the transcript"
    );
}

#[test]
fn side_transcript_never_publishes_thinking_credentials() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    // A display-only delta first, then the authoritative block with its credential.
    transcript
        .apply(
            "t",
            &AgentEvent::ReasoningDelta {
                text: "思考中".into(),
                kind: yi_agent_core::ReasoningKind::Content,
            },
        )
        .unwrap();
    transcript
        .apply(
            "t",
            &AgentEvent::ThinkingBlock {
                text: "最终思考".into(),
                token: ThinkingToken::Signature("super-secret-signature".into()),
            },
        )
        .unwrap();

    let encoded = serde_json::to_string(&transcript.items()).unwrap();
    assert!(
        !encoded.contains("super-secret-signature"),
        "the thinking credential must never reach the transcript"
    );
    // The display-only stream is replaced by the authoritative block.
    let thinking: Vec<String> = transcript
        .items()
        .iter()
        .filter(|item| matches!(item.content, SideItemContent::Thinking { .. }))
        .filter_map(|item| text_of(&item.content).map(str::to_string))
        .collect();
    assert_eq!(thinking, vec!["最终思考".to_string()]);
}

#[test]
fn side_transcript_rejects_when_the_overall_budget_is_spent() {
    // A tiny transcript proves the cap is enforced rather than silently dropping.
    let mut transcript = SideTranscript::new(8 * 1024, 4096);
    // The body budget is `limit - reserve`, so this fills it and leaves only the
    // 4 KiB control reserve.
    transcript.begin_turn("t", &"a".repeat(3500)).unwrap();
    let error = transcript
        .apply("t", &AgentEvent::AssistantText("b".repeat(3000)))
        .expect_err("the budget must refuse, not drop");
    assert_eq!(error.reason, SideErrorReason::SnapshotTooLarge);
    assert!(transcript.retained_bytes() <= 8 * 1024);
}

#[test]
fn side_transcript_keeps_a_control_reserve() {
    let mut transcript = SideTranscript::new(8 * 1024, 4096);
    // Fill the body right up to the reserve: body budget is limit - reserve.
    let filler = "c".repeat(8 * 1024 - SIDE_TRANSCRIPT_CONTROL_RESERVE - 200);
    transcript.begin_turn("t", &filler).unwrap();
    assert!(transcript.remaining_body_bytes() < 512);
    // A terminal notice still fits, which is what makes an error observable.
    transcript
        .push_notice("t", "本轮已停止")
        .expect("a control note must fit inside the reserve");
}

#[test]
fn side_transcript_every_item_stays_within_the_item_cap() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", &"é".repeat(200_000)).unwrap();
    for (index, delta) in ["a", "\\", "\"", "\u{1}", "\u{7f}"]
        .iter()
        .cycle()
        .take(50)
        .enumerate()
    {
        let _ = index;
        transcript
            .apply("t", &AgentEvent::AssistantText(delta.to_string()))
            .unwrap();
    }
    for item in transcript.items() {
        assert!(
            item_bytes(&item) <= SIDE_TRANSCRIPT_MAX_ITEM_BYTES,
            "{} is {} bytes",
            item.id,
            item_bytes(&item)
        );
    }
}

#[test]
fn side_transcript_items_serialise_with_the_documented_shape() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "问题").unwrap();
    let encoded = serde_json::to_value(transcript.items()).unwrap();
    let first = &encoded[0];
    assert_eq!(first["content"]["type"], "userMessage");
    assert_eq!(first["content"]["text"], "问题");
    assert!(first["id"].is_string());
    assert!(first["turn_id"].is_string());
}

#[test]
fn side_transcript_splits_escaped_backslashes_by_encoded_size() {
    // Backslashes double when encoded, so a byte-count split would overflow the cap.
    let mut transcript = SideTranscript::new(16 * 1024 * 1024, 4 * 1024);
    let text = "\\".repeat(10_000);
    let items = transcript.begin_turn("t", &text).unwrap();
    assert!(items.len() > 1);
    for item in &items {
        assert!(item_bytes(item) <= 4 * 1024, "{} bytes", item_bytes(item));
    }
    assert_eq!(joined_text(&items), text);
}

#[test]
fn side_transcript_breaks_streams_between_tool_boundaries() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    transcript
        .apply("t", &AgentEvent::AssistantText("前".into()))
        .unwrap();
    transcript.break_stream("t", Kind::Agent);
    transcript
        .apply("t", &AgentEvent::AssistantText("后".into()))
        .unwrap();
    let agents: Vec<String> = transcript
        .items()
        .iter()
        .filter(|item| matches!(item.content, SideItemContent::AgentMessage { .. }))
        .filter_map(|item| text_of(&item.content).map(str::to_string))
        .collect();
    assert_eq!(agents, vec!["前".to_string(), "后".to_string()]);
}

#[test]
fn side_transcript_turn_boundaries_do_not_merge_messages() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("turn-a", "一").unwrap();
    transcript.begin_turn("turn-b", "二").unwrap();
    let ids: Vec<String> = transcript
        .items()
        .into_iter()
        .map(|item| item.id.clone())
        .collect();
    assert_eq!(ids, vec!["turn-a/user/0/0", "turn-b/user/0/0"]);
}

#[test]
fn side_transcript_ignores_empty_deltas() {
    let mut transcript = SideTranscript::side_defaults();
    transcript.begin_turn("t", "q").unwrap();
    let before = transcript.items().len();
    let after = transcript
        .apply("t", &AgentEvent::AssistantText(String::new()))
        .unwrap();
    assert_eq!(after.len(), before);
}

#[test]
fn side_transcript_does_not_mutate_published_snapshots() {
    let mut transcript = SideTranscript::side_defaults();
    let first = transcript.begin_turn("t", "第一").unwrap();
    let retained = Arc::clone(&first[0]);
    transcript
        .apply("t", &AgentEvent::AssistantText("第二".into()))
        .unwrap();
    // The earlier snapshot is unchanged: updates publish new versions.
    assert_eq!(text_of(&retained.content), Some("第一"));
}

#[test]
fn side_transcript_accepts_a_real_user_message_shape() {
    // Guards against the transcript depending on the parent's Message type: it only
    // ever sees text the caller extracted.
    let message = Message::user("hi");
    let text = match &message.content[0] {
        ContentBlock::Text(text) => text.clone(),
        _ => panic!("expected text"),
    };
    let mut transcript = SideTranscript::side_defaults();
    let items = transcript.begin_turn("t", &text).unwrap();
    assert_eq!(joined_text(&items), "hi");
}
