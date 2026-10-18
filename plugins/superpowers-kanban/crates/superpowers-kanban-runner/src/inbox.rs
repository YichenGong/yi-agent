use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::CardId;
use superpowers_kanban_core::promotion::validate_promotion;

/// 一张投递文件的处理结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxOutcome {
    pub id: String,
    /// `Ok(())` = 已入队；`Err(原因)` = 已归档到 `inbox/rejected/`。
    pub result: Result<(), String>,
}

#[derive(serde::Deserialize)]
struct RawDelivery {
    id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    spec_path: Option<String>,
    #[serde(default)]
    plan_path: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    origin_card: Option<String>,
    /// 预排队卡的标题 / 提升时的新标题。
    #[serde(default)]
    title: Option<String>,
}

/// 消费 `<state_dir>/inbox` 下的全部投递。
///
/// **两趟**：先处理建卡类（implementation / merge / prequeued），再处理 promote。
/// 同一张卡可能在同一 tick 内先 `add-prequeued` 后 `promote`，文件到达顺序不可
/// 依赖，所以必须显式保证「先建后提」。
///
/// 校验成对 → 入队 → 删除；校验失败或损坏 → 移到 `inbox/rejected/` 并记录原因
/// （绝不静默丢弃、绝不 panic）。
pub fn consume(state_dir: &Path, board: &mut Board, now: DateTime<Local>) -> Vec<InboxOutcome> {
    let inbox = state_dir.join("inbox");
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    paths.sort();

    // 先把每个文件读成「路径 + id + 解析结果」，避免两趟各读一次盘。
    let mut parsed: Vec<(PathBuf, String, Result<RawDelivery, String>)> = Vec::new();
    for path in paths {
        let id_from_file = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let parsed_delivery = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<RawDelivery>(&text)
                .map_err(|error| format!("invalid delivery json: {error}")),
            Err(error) => Err(format!("could not read delivery: {error}")),
        };
        parsed.push((path, id_from_file, parsed_delivery));
    }

    let is_promote = |delivery: &Result<RawDelivery, String>| matches!(delivery, Ok(delivery) if delivery.kind.as_deref() == Some("promote"));

    let mut outcomes = Vec::new();
    // 第一趟：非 promote（含解析失败——那些只能按退回收档处理）。
    for (path, id_from_file, delivery) in &parsed {
        if is_promote(delivery) {
            continue;
        }
        outcomes.push(apply_delivery(
            &inbox,
            path,
            id_from_file,
            delivery,
            board,
            now,
        ));
    }
    // 第二趟：promote——此时同 tick 的建卡已经生效。
    for (path, id_from_file, delivery) in &parsed {
        if !is_promote(delivery) {
            continue;
        }
        outcomes.push(apply_delivery(
            &inbox,
            path,
            id_from_file,
            delivery,
            board,
            now,
        ));
    }
    outcomes
}

/// 归档一个被拒的投递：文件名沿用**盘上的原名去掉 `.json` 后缀**
/// （`ghost.promote.json` 归成 `rejected/ghost.promote.json`，保留 `.promote`
/// 标记，不与同名建卡投递撞车）。`reject` 的落盘行为因此完全不变。
fn reject_path(inbox: &Path, path: &Path) {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    reject(inbox, path, &stem);
}

/// 处理一条已读入的投递，返回结果并落盘（消费或拒绝）。
fn apply_delivery(
    inbox: &Path,
    path: &Path,
    id_from_file: &str,
    delivery: &Result<RawDelivery, String>,
    board: &mut Board,
    now: DateTime<Local>,
) -> InboxOutcome {
    let delivery = match delivery {
        Ok(delivery) => delivery,
        Err(reason) => {
            let id = id_from_file.trim_end_matches(".promote").to_string();
            reject_path(inbox, path);
            return InboxOutcome {
                id,
                result: Err(reason.clone()),
            };
        }
    };
    let id = CardId::new(delivery.id.clone());
    if board.get(&id).is_some() && delivery.kind.as_deref() != Some("promote") {
        // 幂等：同 id 已在队列里，直接消费投递即可（promote 例外，见下）。
        let _ = std::fs::remove_file(path);
        return InboxOutcome {
            id: delivery.id.clone(),
            result: Ok(()),
        };
    }
    if delivery.kind.as_deref() == Some("promote") {
        return apply_promote(inbox, path, delivery, board);
    }
    if delivery.kind.as_deref() == Some("prequeued") {
        let title = delivery.title.clone().unwrap_or_default();
        if title.trim().is_empty() {
            reject_path(inbox, path);
            return InboxOutcome {
                id: delivery.id.clone(),
                result: Err("prequeued delivery needs a non-empty title".to_string()),
            };
        }
        board.enqueue_prequeued(id, title, now);
        let _ = std::fs::remove_file(path);
        return InboxOutcome {
            id: delivery.id.clone(),
            result: Ok(()),
        };
    }
    if delivery.kind.as_deref() == Some("merge") {
        let (source, base) = (
            delivery.source.clone().unwrap_or_default(),
            delivery.base.clone().unwrap_or_default(),
        );
        if let Err(error) = superpowers_kanban_core::promotion::validate_merge_refs(&source, &base)
        {
            reject_path(inbox, path);
            return InboxOutcome {
                id: delivery.id.clone(),
                result: Err(error.to_string()),
            };
        }
        let origin = delivery.origin_card.as_deref().map(CardId::new);
        board.enqueue_merge(id, source, base, origin, now);
        let _ = std::fs::remove_file(path);
        return InboxOutcome {
            id: delivery.id.clone(),
            result: Ok(()),
        };
    }
    let spec = delivery.spec_path.clone().unwrap_or_default();
    let plan = delivery.plan_path.clone().unwrap_or_default();
    if let Err(error) = validate_promotion(Path::new(&spec), Path::new(&plan)) {
        reject_path(inbox, path);
        return InboxOutcome {
            id: delivery.id.clone(),
            result: Err(error.to_string()),
        };
    }
    board.enqueue(id, spec.into(), plan.into(), now);
    let _ = std::fs::remove_file(path);
    InboxOutcome {
        id: delivery.id.clone(),
        result: Ok(()),
    }
}

/// 提升：卡必须存在且正处 `PreQueued`，spec/plan 必须成对存在。
fn apply_promote(
    inbox: &Path,
    path: &Path,
    delivery: &RawDelivery,
    board: &mut Board,
) -> InboxOutcome {
    let id = CardId::new(delivery.id.clone());
    let reject_with = |path: &Path, reason: String| {
        reject_path(inbox, path);
        InboxOutcome {
            id: delivery.id.clone(),
            result: Err(reason),
        }
    };
    let Some(card) = board.get(&id) else {
        return reject_with(path, format!("unknown card: {}", delivery.id));
    };
    if card.state != superpowers_kanban_core::card::CardState::PreQueued {
        let state = card.state;
        return reject_with(
            path,
            format!("card {} is not pre-queued (state: {state:?})", delivery.id),
        );
    }
    let spec = delivery.spec_path.clone().unwrap_or_default();
    let plan = delivery.plan_path.clone().unwrap_or_default();
    if let Err(error) = validate_promotion(Path::new(&spec), Path::new(&plan)) {
        return reject_with(path, error.to_string());
    }
    if let Err(error) =
        board.promote_prequeued(&id, spec.into(), plan.into(), delivery.title.clone())
    {
        return reject_with(path, error.to_string());
    }
    let _ = std::fs::remove_file(path);
    InboxOutcome {
        id: delivery.id.clone(),
        result: Ok(()),
    }
}

/// 把一个投递文件挪进 `inbox/rejected/<name>.json`（`name` 不含 `.json`）。
fn reject(inbox: &Path, path: &Path, name: &str) {
    let rejected = inbox.join("rejected");
    if std::fs::create_dir_all(&rejected).is_err() {
        return;
    }
    let target = rejected.join(format!("{name}.json"));
    let _ = std::fs::rename(path, target);
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    fn at() -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap()
    }

    fn deliver(state_dir: &std::path::Path, id: &str, spec: &str, plan: &str) {
        let dir = state_dir.join("inbox");
        std::fs::create_dir_all(&dir).unwrap();
        let body = serde_json::json!({"id": id, "spec_path": spec, "plan_path": plan});
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string(&body).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn a_valid_delivery_is_enqueued_and_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        deliver(
            dir.path(),
            "card-1",
            spec.to_str().unwrap(),
            plan.to_str().unwrap(),
        );

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_ok());
        assert_eq!(board.len(), 1, "the card is queued");
        assert!(
            !dir.path().join("inbox/card-1.json").exists(),
            "the delivery file is consumed"
        );
    }

    #[test]
    fn an_incomplete_pair_is_rejected_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        std::fs::write(&spec, "# spec").unwrap();
        // plan 缺失
        deliver(
            dir.path(),
            "card-2",
            spec.to_str().unwrap(),
            "/nope/a.plan.md",
        );

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert_eq!(board.len(), 0, "nothing is queued");
        assert!(
            dir.path().join("inbox/rejected/card-2.json").exists(),
            "the rejected delivery is kept for inspection"
        );
        assert!(!dir.path().join("inbox/card-2.json").exists());
    }

    #[test]
    fn a_redelivery_of_a_known_card_does_not_enqueue_twice() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        let mut board = Board::new();

        deliver(
            dir.path(),
            "card-1",
            spec.to_str().unwrap(),
            plan.to_str().unwrap(),
        );
        consume(dir.path(), &mut board, at());
        deliver(
            dir.path(),
            "card-1",
            spec.to_str().unwrap(),
            plan.to_str().unwrap(),
        );
        consume(dir.path(), &mut board, at());

        assert_eq!(board.len(), 1, "the same card id is not queued twice");
    }

    #[test]
    fn a_corrupt_delivery_is_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("bad.json"), "{ not json").unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(board.len(), 0);
        assert!(
            dir.path().join("inbox/rejected/bad.json").exists(),
            "a corrupt delivery is archived, not lost"
        );
        assert!(!outcomes.is_empty());
    }

    #[test]
    fn a_merge_delivery_is_queued_as_a_merge_card() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("m1.json"),
            r#"{"id":"m1","kind":"merge","source":"kanban/a","base":"main"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0]);
        let card = board.get(&CardId::new("m1")).unwrap();
        assert_eq!(card.kind, superpowers_kanban_core::card::CardKind::Merge);
        assert_eq!(card.source_ref.as_deref(), Some("kanban/a"));
    }

    #[test]
    fn a_merge_delivery_with_empty_refs_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("bad.json"),
            r#"{"id":"bad","kind":"merge","source":"","base":"main"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert!(dir.path().join("inbox/rejected/bad.json").exists());
    }

    #[test]
    fn a_prequeued_delivery_is_queued_without_a_spec_or_plan() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0]);
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(
            card.state,
            superpowers_kanban_core::card::CardState::PreQueued
        );
        assert_eq!(card.title.as_deref(), Some("my idea"));
    }

    #[test]
    fn a_promote_in_the_same_tick_as_its_prequeue_takes_effect() {
        // 两趟消费的意义：文件按名字排序处理，`idea.json` 在 `idea.promote.json`
        // 之前——但即便顺序反过来，建卡也必须先于提升生效。
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("idea.spec.md");
        let plan = dir.path().join("idea.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        std::fs::write(
            inbox.join("idea.promote.json"),
            serde_json::json!({
                "kind": "promote",
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            })
            .to_string(),
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, superpowers_kanban_core::card::CardState::Queued);
        assert_eq!(card.spec_path, spec);
        assert!(!inbox.join("idea.promote.json").exists(), "投递被消费");
    }

    #[test]
    fn a_promote_for_an_unknown_card_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("ghost.promote.json"),
            r#"{"kind":"promote","id":"ghost","spec_path":"s","plan_path":"p"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err());
        assert!(inbox.join("rejected/ghost.promote.json").exists());
    }

    #[test]
    fn a_promote_for_a_card_that_is_not_pre_queued_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        // 先排一张正式卡（state=Queued）。
        std::fs::write(
            inbox.join("a.json"),
            serde_json::json!({
                "id": "a",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            inbox.join("a.promote.json"),
            r#"{"kind":"promote","id":"a","spec_path":"s","plan_path":"p"}"#,
        )
        .unwrap();
        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert_eq!(
            board.get(&CardId::new("a")).unwrap().state,
            superpowers_kanban_core::card::CardState::Queued
        );
        assert!(
            outcomes.iter().any(|o| o.result.is_err()),
            "非 pre_queued 的提升必须被拒: {outcomes:?}"
        );
        assert!(inbox.join("rejected/a.promote.json").exists());
    }

    #[test]
    fn a_promote_can_overwrite_the_title() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("idea.spec.md");
        let plan = dir.path().join("idea.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        std::fs::write(
            inbox.join("idea.promote.json"),
            serde_json::json!({
                "kind": "promote",
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
                "title": "a sharper idea",
            })
            .to_string(),
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes.iter().all(|o| o.result.is_ok()), "{outcomes:?}");
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(card.state, superpowers_kanban_core::card::CardState::Queued);
        assert_eq!(card.title.as_deref(), Some("a sharper idea"));
    }

    #[test]
    fn a_promote_with_a_missing_plan_is_rejected_and_the_card_stays_pre_queued() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("idea.spec.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        std::fs::write(
            inbox.join("idea.promote.json"),
            serde_json::json!({
                "kind": "promote",
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": "/nope/idea.plan.md",
            })
            .to_string(),
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes.iter().any(|o| o.result.is_err()), "{outcomes:?}");
        let card = board.get(&CardId::new("idea")).unwrap();
        assert_eq!(
            card.state,
            superpowers_kanban_core::card::CardState::PreQueued
        );
        assert_eq!(card.spec_path, std::path::PathBuf::new(), "spec 未被写入");
        assert!(inbox.join("rejected/idea.promote.json").exists());
        assert!(!inbox.join("idea.promote.json").exists());
    }

    #[test]
    fn a_promote_whose_spec_and_plan_are_identical_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let spec = dir.path().join("idea.spec.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"my idea"}"#,
        )
        .unwrap();
        std::fs::write(
            inbox.join("idea.promote.json"),
            serde_json::json!({
                "kind": "promote",
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": spec.to_str().unwrap(),
            })
            .to_string(),
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes.iter().any(|o| o.result.is_err()), "{outcomes:?}");
        assert_eq!(
            board.get(&CardId::new("idea")).unwrap().state,
            superpowers_kanban_core::card::CardState::PreQueued
        );
        assert!(inbox.join("rejected/idea.promote.json").exists());
    }

    #[test]
    fn a_prequeued_delivery_with_a_blank_title_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(
            inbox.join("idea.json"),
            r#"{"kind":"prequeued","id":"idea","title":"   "}"#,
        )
        .unwrap();

        let mut board = Board::new();
        let outcomes = consume(dir.path(), &mut board, at());
        assert!(outcomes[0].result.is_err(), "{:?}", outcomes[0]);
        assert_eq!(board.len(), 0);
        assert!(inbox.join("rejected/idea.json").exists());
        assert!(!inbox.join("idea.json").exists());
    }
}
