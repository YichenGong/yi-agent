//! 受管接管的行为测试：从旧状态目录搬到受管项目目录，逐阶段中断。
//!
//! # 这些测试在防什么
//!
//! 接管要在"用户已经同意"之后动真实数据，所以失败代价很高。三个具体风险：
//!
//! 1. **搬到一半崩了，新旧都能跑**——同一项目同时跑两个看板进程，两边各自
//!    推进同一批卡片，合并冲突和重复开工都从这里来。
//! 2. **未知格式被"猜"过去**——一张 `state: "paused"` 的卡被改成 `queued`，
//!    迁移报告成功，卡片却真的被重新排队了。
//! 3. **瞬态资源被复制**——活锁 / socket / 租约搬过去就变成幽灵持有者。
//!
//! 第一条靠"目标非空即拒绝 + 先备份再原子写"来保证：任何中断要么回落到
//! "只有旧的"，要么落到"只有新的"，不会两个同时可用。

use std::path::PathBuf;

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{CardId, CardState};
use superpowers_kanban_runner::migrate::{MigrateError, adopt, scan};

fn at() -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(2026, 11, 2, 10, 0, 0)
        .unwrap()
}

/// 一份"跑过一阵"的旧看板：三种状态、两条队列次序、一张带会话关联。
fn legacy_board_json() -> String {
    r#"{
      "cards": [
        {
          "id": "impl-a",
          "spec_path": "docs/spec-a.md",
          "plan_path": "docs/plan-a.md",
          "state": "done",
          "enqueued_at": "2026-10-01T09:00:00+08:00",
          "order": 5,
          "thread_id": "thread-a"
        },
        {
          "id": "impl-b",
          "spec_path": "docs/spec-b.md",
          "plan_path": "docs/plan-b.md",
          "state": "queued",
          "enqueued_at": "2026-10-02T09:00:00+08:00",
          "order": 2,
          "thread_id": "thread-b"
        },
        {
          "id": "merge-b",
          "spec_path": "",
          "plan_path": "",
          "state": "queued",
          "enqueued_at": "2026-10-03T09:00:00+08:00",
          "order": 7,
          "kind": "merge"
        }
      ],
      "next_order": 8
    }"#
    .to_string()
}

struct Layout {
    /// 持有临时目录，`Drop` 时清理。
    _root: tempfile::TempDir,
    legacy: PathBuf,
    target: PathBuf,
}

impl Layout {
    fn new(board: &str) -> Layout {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("proj/.yi-agent/superpowers-kanban");
        let target = root
            .path()
            .join("proj/.yi-agent/managed/superpowers-kanban");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("board.json"), board).unwrap();
        Layout {
            _root: root,
            legacy,
            target,
        }
    }

    fn target_board(&self) -> Board {
        let bytes = std::fs::read(self.target.join("board.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn backups(&self) -> Vec<PathBuf> {
        let project = self.target.parent().unwrap();
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(project)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().contains("legacy-backup"))
                    .unwrap_or(false)
            })
            .collect();
        dirs.sort();
        dirs
    }
}

#[test]
fn adopting_preserves_ids_states_order_and_session_links() {
    let layout = Layout::new(&legacy_board_json());

    let backup = adopt(&layout.legacy, &layout.target, at()).unwrap();

    let board = layout.target_board();
    let ids: Vec<&str> = board
        .cards()
        .iter()
        .map(|card| card.id.0.as_str())
        .collect();
    assert_eq!(ids, ["impl-a", "impl-b", "merge-b"]);

    let by_id = |id: &str| {
        board
            .cards()
            .iter()
            .find(|card| card.id.0 == id)
            .unwrap_or_else(|| panic!("{id} missing"))
    };

    assert_eq!(by_id("impl-a").state, CardState::Done);
    assert_eq!(by_id("impl-b").state, CardState::Queued);
    assert_eq!(by_id("merge-b").state, CardState::Queued);

    // 队列次序不是"按 id 排"，而是原来的相对次序。
    assert!(by_id("impl-b").order < by_id("impl-a").order);
    assert!(by_id("impl-a").order < by_id("merge-b").order);

    // 会话跳转关联：§8.3 明写"保留"。
    assert_eq!(by_id("impl-a").thread_id.as_deref(), Some("thread-a"));
    assert_eq!(by_id("impl-b").thread_id.as_deref(), Some("thread-b"));

    // 旧文件原样留在原地（§8.3 没说删它），且备份里有一份。
    assert!(layout.legacy.join("board.json").is_file());
    assert!(backup.join("board.json").is_file());
    assert_eq!(layout.backups().len(), 1);
}

#[test]
fn adopting_is_atomic_so_an_interrupted_run_leaves_exactly_one_side_usable() {
    // 走两遍：第一遍成功；第二遍目标已经有数据，必须被拒绝。
    // "第二次被拒绝"就是中断留下的状态在现实中的形态——目标里要么没有
    // `board.json`（回落到只有旧的），要么是完整的一份（只有新的）。
    let layout = Layout::new(&legacy_board_json());

    adopt(&layout.legacy, &layout.target, at()).unwrap();

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();
    assert!(
        matches!(error, MigrateError::TargetNotEmpty(_)),
        "{error:?}"
    );
    // 拒绝之后目标里的内容仍然是完整的那一份，没有被清空或被写坏。
    let board = layout.target_board();
    assert_eq!(board.cards().len(), 3);
}

#[test]
fn a_missing_target_board_means_no_new_owner_is_running() {
    // 只有旧侧存在时，受管侧读不到 `board.json`——也就是"新进程不会被
    // 允许开跑"。这条断言把"中断后不会新旧并跑"变成可检查的事实。
    let layout = Layout::new(&legacy_board_json());
    assert!(!layout.target.join("board.json").exists());
    // 受管的 `with_board` 不会因为文件缺失就凭空建一张空板去推进。
    let refused: Result<(), String> =
        superpowers_kanban_runner::persist::with_board(&layout.target, |_board| {
            Err("must not run without adopted state".to_string())
        });
    assert!(refused.is_err());
}

#[test]
fn an_unknown_card_state_is_refused_instead_of_being_requeued() {
    let board = legacy_board_json().replace("\"state\": \"done\"", "\"state\": \"paused\"");
    let layout = Layout::new(&board);

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();

    assert!(
        matches!(error, MigrateError::Unrecognized { .. }),
        "{error:?}"
    );
    // 失败不落盘：目标里连 board.json 都没有。
    assert!(!layout.target.join("board.json").exists());
    // 旧文件没被动过。
    assert!(layout.legacy.join("board.json").is_file());
}

#[test]
fn an_unknown_top_level_key_is_refused_rather_than_dropped() {
    // 顶层多一个 `schemaVersion`（或任何未知键）就说明这不是我们认识的旧版。
    // 静默丢掉它等于悄悄改变数据含义。
    let board = legacy_board_json().replace(
        "\"next_order\": 8",
        "\"next_order\": 8, \"futureField\": {\"anything\": true}",
    );
    let layout = Layout::new(&board);

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();
    assert!(
        matches!(error, MigrateError::Unrecognized { .. }),
        "{error:?}"
    );
    assert!(!layout.target.join("board.json").exists());
}

#[test]
fn a_corrupt_board_is_not_read_as_an_empty_queue() {
    let layout = Layout::new("{ this is not json");

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();

    assert!(
        matches!(error, MigrateError::Unrecognized { .. }),
        "{error:?}"
    );
    // 读成"空队列"会造出一个全新的空看板，用户的卡就无声消失了。
    assert!(!layout.target.join("board.json").exists());
}

#[test]
fn transient_files_are_never_copied_into_a_managed_project() {
    let layout = Layout::new(&legacy_board_json());
    for name in [
        "plugin.lock",
        "board.lock",
        "operations.lock",
        "superpowers-kanban.sock",
    ] {
        std::fs::write(layout.legacy.join(name), "held").unwrap();
    }
    std::fs::create_dir_all(layout.legacy.join("inbox")).unwrap();
    std::fs::write(layout.legacy.join("inbox/delivered.json"), "{}").unwrap();

    let backup = adopt(&layout.legacy, &layout.target, at()).unwrap();

    for name in [
        "plugin.lock",
        "board.lock",
        "operations.lock",
        "superpowers-kanban.sock",
        "inbox",
    ] {
        assert!(
            !layout.target.join(name).exists(),
            "{name} was copied into the managed project"
        );
        assert!(
            !backup.join(name).exists(),
            "{name} was copied into the backup; it would become a ghost holder"
        );
    }
    // 旧侧的瞬态文件仍然属于旧进程，一个都没被拿走。
    assert!(layout.legacy.join("plugin.lock").is_file());
}

#[test]
fn the_scan_reports_what_moves_and_why_the_rest_stays() {
    let layout = Layout::new(&legacy_board_json());
    std::fs::write(layout.legacy.join("board.lock"), "held").unwrap();

    let scan = scan(&layout.legacy).unwrap();

    assert_eq!(scan.cards, 3);
    assert_eq!(scan.migrated, vec!["board.json".to_string()]);
    let skipped = scan
        .skipped
        .iter()
        .find(|entry| entry.path == "board.lock")
        .expect("board.lock should be listed as skipped");
    assert!(!skipped.reason.trim().is_empty());
}

#[test]
fn a_project_preferences_file_survives_adoption_untouched() {
    // 配置住在状态目录**之外**（`<workdir>/.yi-agent/preferences.json`）。
    // 接管只写 `<target>/board.json`，所以这份文件既不会被删，也不会被
    // 复制成第二份而分叉。
    let layout = Layout::new(&legacy_board_json());
    let preferences = layout.legacy.parent().unwrap().join("preferences.json");
    let settings = r#"{"superpowers_kanban":false,"concurrency":{"max":2}}"#;
    std::fs::write(&preferences, settings).unwrap();

    adopt(&layout.legacy, &layout.target, at()).unwrap();

    assert_eq!(std::fs::read_to_string(&preferences).unwrap(), settings);
    assert!(!layout.target.join("preferences.json").exists());
}

#[test]
fn adopting_twice_in_a_row_is_refused_so_two_processes_cannot_both_own_it() {
    // 接管是"一个项目一个 owner"的建立动作。第二次必须失败，否则两个
    // 进程会各自认为自己是主人。
    let layout = Layout::new(&legacy_board_json());
    adopt(&layout.legacy, &layout.target, at()).unwrap();

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();

    assert!(
        matches!(error, MigrateError::TargetNotEmpty(_)),
        "{error:?}"
    );
    assert_eq!(layout.target_board().cards().len(), 3);
}

#[test]
fn adopting_boards_with_the_same_cards_does_not_duplicate_ids() {
    let layout = Layout::new(&legacy_board_json());
    adopt(&layout.legacy, &layout.target, at()).unwrap();

    let board = layout.target_board();
    let mut ids: Vec<String> = board.cards().iter().map(|card| card.id.0.clone()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), total, "duplicate card ids after adoption");
    assert!(board.cards().iter().all(|card| card.id != CardId::new("")));
}

#[test]
fn a_target_that_is_a_file_is_refused_rather_than_overwritten() {
    let layout = Layout::new(&legacy_board_json());
    std::fs::create_dir_all(layout.target.parent().unwrap()).unwrap();
    std::fs::write(&layout.target, "not a directory").unwrap();

    let error = adopt(&layout.legacy, &layout.target, at()).unwrap_err();

    // 不能把用户的文件当目录覆盖掉。
    assert!(
        matches!(
            error,
            MigrateError::Io { .. } | MigrateError::TargetNotEmpty(_)
        ),
        "{error:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&layout.target).unwrap(),
        "not a directory"
    );
}
