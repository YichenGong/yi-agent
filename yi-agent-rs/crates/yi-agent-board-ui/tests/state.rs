use std::path::Path;

use yi_agent_board_ui::state::{board_state_path, load_cards};

fn write_board(dir: &Path, body: &str) {
    std::fs::write(board_state_path(dir), body).unwrap();
}

#[test]
fn the_state_file_sits_beside_the_other_plugin_state() {
    assert_eq!(
        board_state_path(Path::new("/proj/.yi-agent")),
        Path::new("/proj/.yi-agent/board.json")
    );
}

#[test]
fn cards_are_mapped_with_state_progress_and_detail() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[{"id":"card-1","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"running","enqueued_at":"2026-10-01T09:00:00+08:00","order":0,"workdir":"/w/card-1"}],"next_order":1}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].id, "card-1");
    assert_eq!(
        cards[0].state, "running",
        "state is shown in lowercase for the UI"
    );
    assert_eq!(
        cards[0].detail, "/w/card-1",
        "detail shows where the card runs"
    );
}

#[test]
fn a_card_without_a_workdir_still_maps() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[{"id":"card-2","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":0}],"next_order":1}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].state, "queued");
    assert_eq!(cards[0].detail, "a.plan.md", "falls back to the plan path");
}

#[test]
fn a_missing_or_corrupt_file_yields_no_cards_instead_of_panicking() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_cards(dir.path()).is_empty(), "missing file");
    write_board(dir.path(), "{ not json");
    assert!(load_cards(dir.path()).is_empty(), "corrupt file");
}

#[test]
fn cards_are_ordered_by_their_queue_order() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[
            {"id":"b","spec_path":"b.spec.md","plan_path":"b.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":5},
            {"id":"a","spec_path":"a.spec.md","plan_path":"a.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":1}
        ],"next_order":6}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(cards[0].id, "a", "order 1 comes first");
    assert_eq!(cards[1].id, "b");
}

#[test]
fn a_root_that_is_not_an_object_yields_no_cards() {
    let dir = tempfile::tempdir().unwrap();
    // 这两种输入 TS 端也必须返回空数组而不是抛异常。
    write_board(dir.path(), "null");
    assert!(load_cards(dir.path()).is_empty(), "null root");
    write_board(dir.path(), r#""just a string""#);
    assert!(load_cards(dir.path()).is_empty(), "string root");
    write_board(dir.path(), r#"{"cards":{}}"#);
    assert!(load_cards(dir.path()).is_empty(), "cards is not an array");
}

#[test]
fn a_bad_card_is_skipped_without_dropping_the_good_ones() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[
            {"id":"good","spec_path":"g.spec.md","plan_path":"g.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":0},
            {"id":"missing-plan","spec_path":"b.spec.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":1},
            {"id":"","spec_path":"e.spec.md","plan_path":"e.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":2}
        ],"next_order":3}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(cards.len(), 1, "only the well-formed card survives");
    assert_eq!(cards[0].id, "good", "a bad card does not sink the board");
}

#[test]
fn an_empty_workdir_falls_back_to_the_plan_path() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[{"id":"c","spec_path":"c.spec.md","plan_path":"c.plan.md","state":"queued","enqueued_at":"2026-10-01T09:00:00+08:00","order":0,"workdir":""}],"next_order":1}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(
        cards[0].detail, "c.plan.md",
        "an empty workdir is not a location"
    );
}

#[test]
fn a_mixed_case_state_is_lowercased_for_the_ui() {
    let dir = tempfile::tempdir().unwrap();
    write_board(
        dir.path(),
        r#"{"cards":[{"id":"c","spec_path":"c.spec.md","plan_path":"c.plan.md","state":"Awaiting_Merge","enqueued_at":"2026-10-01T09:00:00+08:00","order":0}],"next_order":1}"#,
    );
    let cards = load_cards(dir.path());
    assert_eq!(
        cards[0].state, "awaiting_merge",
        "the UI shows the state in lowercase, matching the TS reader"
    );
}
