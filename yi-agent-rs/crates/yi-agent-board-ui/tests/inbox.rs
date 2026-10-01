use std::path::Path;

use yi_agent_board_ui::inbox::{
    board_state_dir, board_state_dir_for_read, deliver_card, enqueue_path, inbox_dir,
};

#[test]
fn the_state_dir_sits_beside_the_other_plugin_state() {
    assert_eq!(
        board_state_dir(Path::new("/proj")),
        Path::new("/proj/.yi-agent/superpowers-kanban")
    );
    assert_eq!(
        inbox_dir(Path::new("/proj/.yi-agent/superpowers-kanban")),
        Path::new("/proj/.yi-agent/superpowers-kanban/inbox")
    );
    assert_eq!(
        enqueue_path(Path::new("/proj/.yi-agent/superpowers-kanban"), "card-1"),
        Path::new("/proj/.yi-agent/superpowers-kanban/inbox/card-1.json")
    );
}

#[test]
fn delivering_a_card_writes_one_json_file() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    let text = std::fs::read_to_string(enqueue_path(dir.path(), "card-1")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["id"], "card-1");
    assert_eq!(value["spec_path"], "a.spec.md");
    assert_eq!(value["plan_path"], "a.plan.md");
}

#[test]
fn delivering_is_idempotent_and_leaves_no_temp_file() {
    let dir = tempfile::tempdir().unwrap();
    deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
    deliver_card(dir.path(), "card-1", "b.spec.md", "b.plan.md").unwrap();
    let entries: Vec<_> = std::fs::read_dir(inbox_dir(dir.path()))
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(entries, vec!["card-1.json".to_string()]);
    assert!(!dir.path().join("inbox/card-1.json.tmp").exists());
}

#[test]
fn reads_fall_back_to_the_legacy_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/board")).unwrap();
    assert_eq!(
        board_state_dir_for_read(dir.path()),
        dir.path().join(".yi-agent/board")
    );
}

#[test]
fn reads_prefer_the_new_directory_when_both_exist() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/board")).unwrap();
    std::fs::create_dir_all(dir.path().join(".yi-agent/superpowers-kanban")).unwrap();
    assert_eq!(
        board_state_dir_for_read(dir.path()),
        dir.path().join(".yi-agent/superpowers-kanban")
    );
}
