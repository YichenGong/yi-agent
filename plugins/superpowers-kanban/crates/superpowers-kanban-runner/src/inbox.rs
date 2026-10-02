use std::path::Path;

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
    spec_path: String,
    plan_path: String,
}

/// 消费 `<state_dir>/inbox` 下的全部投递：校验成对 → 入队 → 删除；
/// 校验失败或损坏 → 移到 `inbox/rejected/` 并记录原因（绝不静默丢弃、绝不 panic）。
pub fn consume(state_dir: &Path, board: &mut Board, now: DateTime<Local>) -> Vec<InboxOutcome> {
    let inbox = state_dir.join("inbox");
    let Ok(entries) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    paths.sort();
    for path in paths {
        let id_from_file = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                outcomes.push(InboxOutcome {
                    id: id_from_file,
                    result: Err(format!("could not read delivery: {error}")),
                });
                continue;
            }
        };
        let delivery = match serde_json::from_str::<RawDelivery>(&text) {
            Ok(delivery) => delivery,
            Err(error) => {
                reject(&inbox, &path, &id_from_file);
                outcomes.push(InboxOutcome {
                    id: id_from_file,
                    result: Err(format!("invalid delivery json: {error}")),
                });
                continue;
            }
        };
        let id = CardId::new(delivery.id.clone());
        if board.get(&id).is_some() {
            // 幂等：同 id 已在队列里，直接消费投递即可。
            let _ = std::fs::remove_file(&path);
            outcomes.push(InboxOutcome {
                id: delivery.id,
                result: Ok(()),
            });
            continue;
        }
        if let Err(error) = validate_promotion(
            Path::new(&delivery.spec_path),
            Path::new(&delivery.plan_path),
        ) {
            reject(&inbox, &path, &delivery.id);
            outcomes.push(InboxOutcome {
                id: delivery.id,
                result: Err(error.to_string()),
            });
            continue;
        }
        board.enqueue(
            id,
            delivery.spec_path.into(),
            delivery.plan_path.into(),
            now,
        );
        let _ = std::fs::remove_file(&path);
        outcomes.push(InboxOutcome {
            id: delivery.id,
            result: Ok(()),
        });
    }
    outcomes
}

fn reject(inbox: &Path, path: &Path, id: &str) {
    let rejected = inbox.join("rejected");
    if std::fs::create_dir_all(&rejected).is_err() {
        return;
    }
    let target = rejected.join(format!("{id}.json"));
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
}
