//! 旁支冷读的严格性与无副作用性。
//!
//! 每条测试都对应 spec §5.1 的一条硬要求：拒绝损坏、不修复写回、不改父文件。

use std::io::Write;
use std::path::Path;

use super::*;
use crate::protocol::Attachment;
use crate::thread_store::{ThreadMode, TurnUsage};
use tempfile::TempDir;

fn store() -> (TempDir, ThreadStore) {
    let dir = TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    (dir, store)
}

fn meta(id: &str) -> ThreadMeta {
    ThreadMeta {
        thread_id: id.into(),
        cwd: "/tmp".into(),
        model: "m".into(),
        created_at: 1,
        updated_at: 1,
        title: None,
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        extra_roots: Vec::new(),
        role: None,
        anchor_sha: None,
        content_epoch: 0,
        last_item_id: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
    }
}

/// 写一条完整的历史轮次：一条用户消息 + 一条助手回复。
fn seed_turn(store: &ThreadStore, id: &str, question: &str, answer: &str) {
    let items = vec![
        crate::protocol::Item::UserMessage {
            id: format!("user-{question}"),
            text: question.into(),
            attachments: Vec::new(),
            images: Vec::new(),
        },
        crate::protocol::Item::AgentMessage {
            id: format!("assistant-{question}"),
            text: answer.into(),
        },
    ];
    let messages = vec![
        yi_agent_core::Message::user(question),
        yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(answer.into())]),
    ];
    store
        .append_turn(
            id,
            &crate::thread_store::TurnLine::Turn {
                items,
                usage: Some(TurnUsage {
                    model: "m".into(),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                }),
                usage_total: None,
                messages,
            },
        )
        .unwrap();
}

#[test]
fn side_strict_log_rejects_corruption() {
    // 这一行不是 JSON。宽松入口会跳过它；严格入口必须报错，否则旁支会拿着
    // 「少了一轮」的历史冒充完整快照。
    let bytes = b"{not json}\n";
    assert!(decode_side_log_line_strict(bytes).is_err());
}

#[test]
fn side_strict_log_rejects_a_blank_line() {
    // 写入端只会写「序列化 + 换行」，故中间的空行说明有别的写者夹了内容进来，
    // 或文件被截断后拼接。
    assert!(decode_side_log_line_strict(b"\n").is_err());
    assert!(decode_side_log_line_strict(b"   \n").is_err());
}

#[test]
fn side_strict_log_accepts_a_real_turn_line() {
    let line = serde_json::to_vec(&crate::thread_store::TurnLine::Turn {
        items: Vec::new(),
        usage: None,
        usage_total: None,
        messages: vec![yi_agent_core::Message::user("hi")],
    })
    .unwrap();
    assert!(decode_side_log_line_strict(&line).is_ok());
}

#[test]
fn side_cold_snapshot_reads_a_healthy_parent() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");

    let snapshot = store
        .read_side_snapshot_strict("p1")
        .unwrap()
        .expect("a healthy parent has a snapshot");
    assert_eq!(snapshot.meta.thread_id, "p1");
    assert_eq!(snapshot.messages.len(), 2);
    assert_eq!(
        match &snapshot.messages[0].content[0] {
            yi_agent_core::ContentBlock::Text(text) => text.as_str(),
            _ => panic!("expected text"),
        },
        "q1"
    );
}

#[test]
fn side_cold_snapshot_rejects_a_corrupt_log_instead_of_skipping() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");
    // 追加一行坏数据。宽松的 load() 会跳过它并正常返回——正是这里不能接受的。
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(store.log_path("p1"))
        .unwrap();
    file.write_all(b"{oops\n").unwrap();
    drop(file);

    let loose = store.load("p1").unwrap().expect("loose load still works");
    assert_eq!(loose.messages.len(), 2, "the loose path skips the bad line");

    let error = store
        .read_side_snapshot_strict("p1")
        .err()
        .expect("the strict path must refuse");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn side_cold_snapshot_refuses_a_truncated_partial() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");
    // 写到一半的 partial：JSON 不完整。
    std::fs::write(store.partial_path("p1"), br#"{"turn_id":"t","items":[{"#).unwrap();

    assert!(
        store.read_side_snapshot_strict("p1").is_err(),
        "a half-written checkpoint must not be presented as complete history"
    );
}

#[test]
fn side_cold_snapshot_includes_the_partial_turn() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");
    store
        .write_partial(
            "p1",
            &crate::thread_store::PartialTurn {
                turn_id: "t2".into(),
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-q2".into(),
                    text: "q2".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                messages: vec![
                    yi_agent_core::Message::user("q1"),
                    yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                        "a1".into(),
                    )]),
                    yi_agent_core::Message::user("q2"),
                ],
                usage: None,
                usage_total: None,
            },
        )
        .unwrap();

    let snapshot = store.read_side_snapshot_strict("p1").unwrap().unwrap();
    assert_eq!(
        snapshot.messages.len(),
        3,
        "the in-flight turn is real content"
    );
}

#[test]
fn side_cold_snapshot_leaves_every_parent_file_untouched() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");
    store
        .write_partial(
            "p1",
            &crate::thread_store::PartialTurn {
                turn_id: "t2".into(),
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-q2".into(),
                    text: "q2".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                messages: vec![yi_agent_core::Message::user("q2")],
                usage: None,
                usage_total: None,
            },
        )
        .unwrap();

    let paths = [
        store.meta_path("p1"),
        store.log_path("p1"),
        store.partial_path("p1"),
    ];
    let before: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();

    store.read_side_snapshot_strict("p1").unwrap().unwrap();

    let after: Vec<Vec<u8>> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(before, after, "a cold read must never rewrite the parent");

    // The partial must still be there: a cold read does not promote it either.
    assert!(store.partial_path("p1").exists());
}

#[test]
fn side_cold_snapshot_refuses_an_oversized_log_before_reading_it() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    // A log just over the cap. We cannot assert on memory use, but we can assert
    // the error is the size guard rather than a parse failure.
    let path = store.log_path("p1");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(SIDE_LOG_MAX_BYTES + 1).unwrap();
    drop(file);

    let error = store
        .read_side_snapshot_strict("p1")
        .err()
        .expect("an over-cap log must be refused");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        error.to_string().contains("over the"),
        "the error must name the limit, got: {error}"
    );
}

#[test]
fn side_cold_snapshot_rejects_a_meta_that_names_another_thread() {
    let (_dir, store) = store();
    let mut wrong = meta("p1");
    wrong.thread_id = "p2".into();
    // Create the directory first (seed_turn also does this, but the meta has to be
    // written under p1's name before the log exists).
    std::fs::create_dir_all(store.root()).unwrap();
    // Write it *at p1's path*: the file name says p1 but the contents name p2, which
    // is what a botched fork or a hand-copied directory looks like.
    std::fs::write(
        store.meta_path("p1"),
        serde_json::to_vec_pretty(&wrong).unwrap(),
    )
    .unwrap();
    seed_turn(&store, "p1", "q1", "a1");

    assert!(
        store.read_side_snapshot_strict("p1").is_err(),
        "meta from another thread must not be adopted"
    );
}

#[test]
fn side_cold_snapshot_is_none_when_nothing_exists() {
    let (_dir, store) = store();
    assert!(store.read_side_snapshot_strict("nobody").unwrap().is_none());
}

#[test]
fn side_cold_snapshot_drops_a_dangling_tool_use() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    // 一条 assistant tool_use 后面没有配对结果：父可能正跑到一半。旁支不会执行
    // 这个工具，留着它只会让 provider 拒绝请求。
    let dangling = yi_agent_core::Message::assistant(vec![
        yi_agent_core::ContentBlock::Text("先读一下".into()),
        yi_agent_core::ContentBlock::ToolUse {
            id: "call-1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "a.rs"}),
        },
    ]);
    store
        .append_turn(
            "p1",
            &crate::thread_store::TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: vec![yi_agent_core::Message::user("q1"), dangling],
            },
        )
        .unwrap();

    let snapshot = store.read_side_snapshot_strict("p1").unwrap().unwrap();
    let has_tool_use = snapshot
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .any(|block| matches!(block, yi_agent_core::ContentBlock::ToolUse { .. }));
    assert!(
        !has_tool_use,
        "an unpaired tool_use must not enter the side's history"
    );
}

#[test]
fn side_cold_snapshot_collects_persisted_user_attachments() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    let attachment = Attachment {
        name: "报告.pdf".into(),
        path: ".yi-agent/attachments/p1/abc-报告.pdf".into(),
        mime: Some("application/pdf".into()),
        size: 42,
    };
    store
        .append_turn(
            "p1",
            &crate::thread_store::TurnLine::Turn {
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-q1".into(),
                    text: "看这个".into(),
                    attachments: vec![attachment.clone()],
                    images: Vec::new(),
                }],
                usage: None,
                usage_total: None,
                messages: vec![yi_agent_core::Message::user("看这个")],
            },
        )
        .unwrap();

    let snapshot = store.read_side_snapshot_strict("p1").unwrap().unwrap();
    assert_eq!(snapshot.attachments, vec![attachment]);
}

#[test]
fn side_cold_snapshot_does_not_guess_attachment_paths_from_text() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    // 自然语言里提到一个看起来像附件的路径：没有结构化记录就不能当成附件。
    store
        .append_turn(
            "p1",
            &crate::thread_store::TurnLine::Turn {
                items: vec![crate::protocol::Item::UserMessage {
                    id: "user-q1".into(),
                    text: "请读 .yi-agent/attachments/p1/abc-x.pdf".into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                usage: None,
                usage_total: None,
                messages: vec![yi_agent_core::Message::user("请读那个文件")],
            },
        )
        .unwrap();

    let snapshot = store.read_side_snapshot_strict("p1").unwrap().unwrap();
    assert!(
        snapshot.attachments.is_empty(),
        "a path mentioned in prose is not an attachment"
    );
}

#[test]
fn side_cold_stamp_changes_when_a_file_is_rewritten() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    seed_turn(&store, "p1", "q1", "a1");
    let before = store.side_cold_stamp("p1").unwrap();

    // Rewrite the meta atomically: a new inode, so the stamp must differ.
    store.set_title_if_unchanged("p1", "", "新标题").unwrap();
    let after = store.side_cold_stamp("p1").unwrap();
    assert_ne!(before, after, "a replaced file must change the stamp");
}

#[test]
fn side_cold_snapshot_rejects_a_thread_with_no_messages() {
    let (_dir, store) = store();
    store.create(&meta("p1")).unwrap();
    // meta-only: /clear leaves exactly this shape.
    assert!(
        store.read_side_snapshot_strict("p1").is_err(),
        "an empty history cannot answer questions about the context above"
    );
}

fn _assert_path_is_readable(path: &Path) {
    assert!(std::fs::read(path).is_ok());
}
