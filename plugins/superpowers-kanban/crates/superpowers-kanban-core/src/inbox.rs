//! 把「加入看板」写成一个投递文件。
//!
//! 插件只写这里，从不直接碰 `board.json`——推进循环（runner）是 `board.json`
//! 的唯一写者，投递与消费由此解耦。文件形状与宿主的
//! `yi-agent-board-ui::inbox`、以及本插件 `board-runner` 的消费端一致：
//! `{ "id", "spec_path", "plan_path" }`。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::board::Board;

/// 投递目录：`<state_dir>/inbox`。
pub fn inbox_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("inbox")
}

/// 一张卡片的投递文件：`<state_dir>/inbox/<id>.json`。
pub fn enqueue_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.json"))
}

/// 写一个投递文件。同 id 覆盖（幂等）；temp + rename 原子替换，
/// 读者永远看不到半个文件。
pub fn deliver_card(state_dir: &Path, id: &str, spec: &str, plan: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({
        "id": id,
        "spec_path": spec,
        "plan_path": plan,
    });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// 投递一张合并卡：`{"id","kind":"merge","source","base"[,"origin_card"]}`。
pub fn deliver_merge_card(
    state_dir: &Path,
    id: &str,
    source: &str,
    base: &str,
    origin: Option<&str>,
) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let mut body = serde_json::json!({ "id": id, "kind": "merge", "source": source, "base": base });
    if let Some(origin) = origin {
        body["origin_card"] = serde_json::Value::String(origin.to_string());
    }
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// 提升投递的文件路径：`<state_dir>/inbox/<id>.promote.json`。
///
/// 与建卡投递分文件：同一张卡可能在同一 tick 内先建后提，两件事必须能并存。
pub fn promote_path(state_dir: &Path, id: &str) -> PathBuf {
    inbox_dir(state_dir).join(format!("{id}.promote.json"))
}

/// 投递一张预排队卡：`{"kind":"prequeued","id","title"}`。
pub fn deliver_prequeued_card(state_dir: &Path, id: &str, title: &str) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = enqueue_path(state_dir, id);
    let body = serde_json::json!({ "kind": "prequeued", "id": id, "title": title });
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// 投递一次提升：`{"kind":"promote","id","spec_path","plan_path"[,"title"]}`。
pub fn deliver_promote_card(
    state_dir: &Path,
    id: &str,
    spec: &str,
    plan: &str,
    title: Option<&str>,
) -> std::io::Result<()> {
    let dir = inbox_dir(state_dir);
    std::fs::create_dir_all(&dir)?;
    let path = promote_path(state_dir, id);
    let mut body = serde_json::json!({
        "kind": "promote",
        "id": id,
        "spec_path": spec,
        "plan_path": plan,
    });
    if let Some(title) = title {
        body["title"] = serde_json::Value::String(title.to_string());
    }
    let text = serde_json::to_string_pretty(&body).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// 尚未被占用的预排队卡 id：既避开看板上已有的卡，也避开 `<state_dir>/inbox`
/// 里**尚未被 runner 消费**的投递。
///
/// 只看看板是不够的：投递要等下一个 tick 才入队，在此之前同一窗口内的第二次
/// `enqueue_prequeued` 会算出同一个 id，而 `deliver_prequeued_card` 写的是同一个
/// `<id>.json`（temp+rename 覆盖），后一张卡会静默吃掉前一张。
///
/// IPC 与 CLI 都必须走这里，不能各自 `Board::next_free_prequeued_id`——那正是
/// 上面这个洞的来源。
pub fn next_free_prequeued_id(
    board: &Board,
    state_dir: &Path,
    title: &str,
    now: DateTime<Local>,
) -> crate::card::CardId {
    let base = crate::board::prequeued_id_base(title, now);
    let taken =
        |candidate: &str| board.contains(candidate) || enqueue_path(state_dir, candidate).exists();
    if !taken(&base) {
        return crate::card::CardId::new(base);
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if !taken(&candidate) {
            return crate::card::CardId::new(candidate);
        }
    }
    unreachable!("u32 suffix space exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone;

    #[test]
    fn delivers_one_json_file_per_card() {
        let dir = tempfile::tempdir().unwrap();
        deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
        let path = dir.path().join("inbox/card-1.json");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["id"], "card-1");
        assert_eq!(value["spec_path"], "a.spec.md");
        assert_eq!(value["plan_path"], "a.plan.md");
    }

    #[test]
    fn delivering_is_idempotent_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        deliver_card(dir.path(), "card-1", "a.spec.md", "a.plan.md").unwrap();
        deliver_card(dir.path(), "card-1", "b.spec.md", "b.plan.md").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(dir.path().join("inbox"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["card-1.json".to_string()]);
        assert!(!dir.path().join("inbox/card-1.json.tmp").exists());
        // 后一次覆盖前一次。
        let text = std::fs::read_to_string(dir.path().join("inbox/card-1.json")).unwrap();
        assert!(text.contains("b.spec.md"));
    }

    #[test]
    fn creates_the_inbox_directory_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!dir.path().join("inbox").exists());
        deliver_card(dir.path(), "card-1", "a", "b").unwrap();
        assert!(dir.path().join("inbox").is_dir());
    }

    #[test]
    fn delivers_a_merge_card_with_its_kind_and_refs() {
        let dir = tempfile::tempdir().unwrap();
        deliver_merge_card(dir.path(), "merge-a-into-main", "kanban/a", "main", None).unwrap();
        let text =
            std::fs::read_to_string(dir.path().join("inbox/merge-a-into-main.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "merge");
        assert_eq!(value["source"], "kanban/a");
        assert_eq!(value["base"], "main");
    }

    #[test]
    fn a_merge_card_carries_its_origin_when_given() {
        let dir = tempfile::tempdir().unwrap();
        deliver_merge_card(dir.path(), "merge-a", "kanban/a", "main", Some("card-1")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/merge-a.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["origin_card"], "card-1");
    }

    #[test]
    fn delivers_a_prequeued_card_with_its_kind_and_title() {
        let dir = tempfile::tempdir().unwrap();
        deliver_prequeued_card(dir.path(), "idea", "一个想法").unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "prequeued");
        assert_eq!(value["id"], "idea");
        assert_eq!(value["title"], "一个想法");
    }

    #[test]
    fn delivers_a_promote_with_its_own_file_name() {
        let dir = tempfile::tempdir().unwrap();
        deliver_promote_card(dir.path(), "idea", "a.spec.md", "a.plan.md", Some("新标题")).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "promote");
        assert_eq!(value["id"], "idea");
        assert_eq!(value["spec_path"], "a.spec.md");
        assert_eq!(value["plan_path"], "a.plan.md");
        assert_eq!(value["title"], "新标题");
    }

    #[test]
    fn a_promote_without_a_title_omits_the_key() {
        let dir = tempfile::tempdir().unwrap();
        deliver_promote_card(dir.path(), "idea", "s", "p", None).unwrap();
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("title").is_none());
    }

    #[test]
    fn a_pending_delivery_blocks_its_id_so_two_quick_captures_differ() {
        let dir = tempfile::tempdir().unwrap();
        let board = Board::new();
        let now = Local.with_ymd_and_hms(2026, 10, 18, 17, 7, 29).unwrap();
        // 两个不同的中文标题在同一秒：基名相同，第二个必须换 id。
        let first = next_free_prequeued_id(&board, dir.path(), "一个想法", now);
        deliver_prequeued_card(dir.path(), &first.0, "一个想法").unwrap();
        let second = next_free_prequeued_id(&board, dir.path(), "另一个想法", now);
        assert_ne!(first, second, "同一窗口的第二次捕获必须换 id");
        assert!(dir.path().join(format!("inbox/{}.json", first.0)).exists());
    }

    #[test]
    fn a_pending_delivery_with_the_same_title_gets_a_suffix_too() {
        let dir = tempfile::tempdir().unwrap();
        let board = Board::new();
        let now = Local.with_ymd_and_hms(2026, 10, 18, 17, 7, 29).unwrap();
        let first = next_free_prequeued_id(&board, dir.path(), "idea", now);
        assert_eq!(first.0, "idea");
        deliver_prequeued_card(dir.path(), &first.0, "idea").unwrap();
        let second = next_free_prequeued_id(&board, dir.path(), "idea", now);
        assert_eq!(second.0, "idea-2");
    }

    #[test]
    fn a_promote_delivery_does_not_block_a_new_card_id() {
        // 提升投递另用 `<id>.promote.json`：它不该把 `<id>.json` 这个建卡位置也占掉。
        let dir = tempfile::tempdir().unwrap();
        let board = Board::new();
        let now = Local.with_ymd_and_hms(2026, 10, 18, 17, 7, 29).unwrap();
        deliver_promote_card(dir.path(), "idea", "a.spec.md", "a.plan.md", None).unwrap();
        assert_eq!(
            next_free_prequeued_id(&board, dir.path(), "idea", now).0,
            "idea"
        );
    }

    #[test]
    fn a_card_already_on_the_board_still_blocks_its_id() {
        // 看板那一半判据不能被 inbox 判据顶掉。
        let dir = tempfile::tempdir().unwrap();
        let mut board = Board::new();
        let now = Local.with_ymd_and_hms(2026, 10, 18, 17, 7, 29).unwrap();
        board.enqueue_prequeued(crate::card::CardId::new("idea"), "idea".into(), now);
        assert_eq!(
            next_free_prequeued_id(&board, dir.path(), "idea", now).0,
            "idea-2"
        );
    }
}
