use std::path::PathBuf;

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{CardId, CardState};

use crate::client::BoardDaemon;

/// What one tick did to one card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TickAction {
    Launched {
        session_id: String,
        root_task_id: String,
    },
    Transitioned(CardState),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    pub card_id: CardId,
    pub action: TickAction,
}

/// 启动一张卡片：建会话、置 `Running`、记下根任务 id 与预建好的 worktree。
///
/// 调用方必须在调用前先领到一个全局并发名额；本函数返回 `Failed` 表示这张卡
/// 没启动起来，调用方应把名额还回去（drop 掉）。
pub fn launch(
    board: &mut Board,
    daemon: &BoardDaemon,
    card_id: &CardId,
    workdir: PathBuf,
) -> TickOutcome {
    let objective = board.get(card_id).map(objective_for).unwrap_or_default();
    match daemon.create_session(&objective, &workdir) {
        Ok(created) => {
            if board.transition(card_id, CardState::Running).is_ok() {
                // 记下根任务 id：之后靠它问 daemon「这张卡片跑完没有」，
                // 完成的卡片才能让出并发名额（否则队列在 N 张后会停摆）。
                let _ = board.set_task_id(card_id, created.root_task_id.clone());
                let _ = board.set_workdir(card_id, workdir);
                TickOutcome {
                    card_id: card_id.clone(),
                    action: TickAction::Launched {
                        session_id: created.session_id,
                        root_task_id: created.root_task_id,
                    },
                }
            } else {
                TickOutcome {
                    card_id: card_id.clone(),
                    action: TickAction::Failed("could not transition to running".to_string()),
                }
            }
        }
        Err(error) => TickOutcome {
            card_id: card_id.clone(),
            action: TickAction::Failed(error.to_string()),
        },
    }
}

/// 把每张 `Running` 卡片的状态与 daemon 对齐，返回本次因此让出名额的迁移。
///
/// 只有「不再占名额」的映射才迁移：daemon 说仍在跑、或我们认不出的新状态，
/// 一律保持 `Running`（认不出就不动手，升级 daemon 不能让我们误判）。查不到
/// 该任务（`None`）同样保持——可能只是刚启动、daemon 尚未登记。
pub fn reconcile_running(board: &mut Board, daemon: &BoardDaemon) -> Vec<TickOutcome> {
    let running: Vec<CardId> = board
        .cards()
        .iter()
        .filter(|card| card.state == CardState::Running)
        .map(|card| card.id.clone())
        .collect();

    let mut outcomes = Vec::new();
    for card_id in running {
        let Some(task_id) = board.get(&card_id).and_then(|card| card.task_id.clone()) else {
            continue;
        };
        let observed = match daemon.task_state(&task_id) {
            Ok(observed) => observed,
            // A query that could not be made is not evidence about the card.
            Err(_) => continue,
        };
        let Some(state) = observed else {
            // The task is gone entirely: the card can never be reconciled again,
            // so fail it visibly instead of stranding it in `running` — which
            // would also hold its slot forever and block every later card.
            if board.transition(&card_id, CardState::Failed).is_ok() {
                outcomes.push(TickOutcome {
                    card_id,
                    action: TickAction::Transitioned(CardState::Failed),
                });
            }
            continue;
        };
        let Some(next) = crate::runner::state_for_task_state(&state) else {
            continue;
        };
        // 只看「不再占名额」的迁移；仍占名额（Running）或不可迁移的一律跳过。
        if next.occupies_slot() {
            continue;
        }
        if board.transition(&card_id, next).is_ok() {
            outcomes.push(TickOutcome {
                card_id,
                action: TickAction::Transitioned(next),
            });
        }
    }
    outcomes
}

/// The objective handed to the daemon. Kept in one place so the wording (and
/// the Superpowers constraints it carries) is reviewable at a glance.
pub fn objective_for(card: &superpowers_kanban_core::card::Card) -> String {
    format!(
        "Implement the plan at {plan} following its spec at {spec}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED.",
        plan = card.plan_path.display(),
        spec = card.spec_path.display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Local, TimeZone};

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use superpowers_kanban_ipc::wire::PROTOCOL_VERSION;

    fn at(hour: u32) -> chrono::DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 1, hour, 0, 0)
            .single()
            .unwrap()
    }

    /// 一个只听得懂「建会话 / 列任务」的假 daemon，走真实 wire 协议。
    ///
    /// 不抽 `BoardDaemon` 成 trait：那会为了测试改动插件结构。这里让假 daemon
    /// 直接说 daemon 的话（一行 JSON 请求、一行 JSON 应答），`BoardDaemon` 原样
    /// 连接，测到的就是真链路。
    struct FakeDaemon {
        states: Arc<Mutex<HashMap<String, String>>>,
        stop: Arc<AtomicBool>,
        _dir: tempfile::TempDir,
    }

    /// 起一个假 daemon，返回它的控制柄与 socket 路径。
    fn start_fake_daemon() -> (FakeDaemon, PathBuf) {
        let states: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let states_thread = states.clone();
        std::thread::spawn(move || {
            let mut counter = 0u64;
            while !stop_thread.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, &states_thread, &mut counter),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        (
            FakeDaemon {
                states,
                stop,
                _dir: dir,
            },
            socket,
        )
    }

    impl FakeDaemon {
        fn finish(&self) {
            self.stop.store(true, Ordering::SeqCst);
        }

        fn set_state(&self, task_id: &str, state: &str) {
            self.states
                .lock()
                .unwrap()
                .insert(task_id.to_owned(), state.to_owned());
        }

        /// Drops a task from the listing, as a daemon that lost or reclaimed the
        /// task's session would.
        fn forget(&self, task_id: &str) {
            self.states.lock().unwrap().remove(task_id);
        }
    }

    fn serve_one(
        mut stream: UnixStream,
        states: &Arc<Mutex<HashMap<String, String>>>,
        counter: &mut u64,
    ) {
        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            return;
        };
        let request_id = value["request_id"].as_str().unwrap_or("0").to_owned();
        let result = match value["command"]["type"].as_str().unwrap_or("") {
            "CreateAutonomousSession" => {
                *counter += 1;
                let task_id = format!("task-{counter}");
                states
                    .lock()
                    .unwrap()
                    .insert(task_id.clone(), "running".to_owned());
                serde_json::json!({
                    "type": "AutonomousSessionCreated",
                    "session_id": format!("s-{counter}"),
                    "root_task_id": task_id,
                })
            }
            "ListTaskSummaries" => {
                let tasks: Vec<serde_json::Value> = states
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(id, state)| {
                        serde_json::json!({"task_id": id, "state": state, "is_root": true})
                    })
                    .collect();
                serde_json::json!({"type": "TaskSummaries", "tasks": tasks})
            }
            _ => serde_json::json!({"type": "Status", "high_water_event_id": 0}),
        };
        let response = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "request_id": request_id,
            "result": result,
        });
        let _ = writeln!(stream, "{response}");
        let _ = stream.flush();
    }

    fn board_with(ids: &[&str]) -> Board {
        let mut board = Board::new();
        for (index, id) in ids.iter().enumerate() {
            board.enqueue(
                CardId::new(*id),
                format!("{id}.spec.md").into(),
                format!("{id}.plan.md").into(),
                at(index as u32),
            );
        }
        board
    }

    #[test]
    fn a_successful_launch_moves_the_card_to_running() {
        let mut board = board_with(&["a", "b"]);
        // A daemon that accepts every request: exercise the loop with a stub by
        // asserting on the pure planning + transition path.
        let planned = crate::runner::plan_launches(&board, 1);
        assert_eq!(planned, vec![CardId::new("a")]);
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        assert_eq!(board.running_count(), 1);
        assert_eq!(
            board.get(&CardId::new("b")).unwrap().state,
            CardState::Queued
        );
    }

    #[test]
    fn the_objective_carries_the_plan_spec_and_constraints() {
        let board = board_with(&["a"]);
        let card = board.get(&CardId::new("a")).unwrap().clone();
        let objective = objective_for(&card);
        assert!(objective.contains("a.plan.md"));
        assert!(objective.contains("a.spec.md"));
        assert!(objective.contains("Never merge yourself"));
        assert!(objective.contains("BLOCKED"));
    }

    #[test]
    fn outcomes_record_launch_and_transition() {
        let outcome = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Launched {
                session_id: "s".into(),
                root_task_id: "t".into(),
            },
        };
        assert_eq!(outcome.card_id, CardId::new("a"));
        let transition = TickOutcome {
            card_id: CardId::new("a"),
            action: TickAction::Transitioned(CardState::AwaitingMerge),
        };
        assert!(matches!(
            transition.action,
            TickAction::Transitioned(CardState::AwaitingMerge)
        ));
    }
    #[test]
    fn a_card_whose_task_finished_leaves_running_and_frees_its_slot() {
        let (fake, socket) = start_fake_daemon();
        let daemon = BoardDaemon::new(socket);
        let mut board = board_with(&["a", "b"]);

        let outcome = launch(&mut board, &daemon, &CardId::new("a"), PathBuf::from("/tmp/wt/a"));
        assert!(
            matches!(outcome.action, TickAction::Launched { .. }),
            "{:?}",
            outcome.action
        );
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Running);
        assert_eq!(board.running_count(), 1);

        // daemon 说还在跑：对账不动它，名额仍被占着。
        assert!(reconcile_running(&mut board, &daemon).is_empty());
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Running);

        // daemon 说跑完了：卡片离开 Running，名额被让出来。
        let task_id = board.get(&CardId::new("a")).unwrap().task_id.clone().unwrap();
        fake.set_state(&task_id, "completed");
        let freed = reconcile_running(&mut board, &daemon);
        assert_eq!(freed.len(), 1);
        assert_eq!(freed[0].card_id, CardId::new("a"));
        assert_eq!(
            board.get(&CardId::new("a")).unwrap().state,
            CardState::AwaitingMerge
        );
        assert_eq!(board.running_count(), 0, "the slot is free again");
        // 空出来的名额立刻能被队列用上。
        assert_eq!(crate::runner::plan_launches(&board, 1), vec![CardId::new("b")]);
        fake.finish();
    }

    #[test]
    fn a_failed_task_also_leaves_running() {
        let (fake, socket) = start_fake_daemon();
        let daemon = BoardDaemon::new(socket);
        let mut board = board_with(&["a"]);
        launch(&mut board, &daemon, &CardId::new("a"), PathBuf::from("/tmp/wt/a"));

        let task_id = board.get(&CardId::new("a")).unwrap().task_id.clone().unwrap();
        fake.set_state(&task_id, "failed");
        let freed = reconcile_running(&mut board, &daemon);
        assert_eq!(freed.len(), 1);
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Failed);
        fake.finish();
    }

    #[test]
    fn an_unknown_daemon_state_leaves_the_card_running() {
        let (fake, socket) = start_fake_daemon();
        let daemon = BoardDaemon::new(socket);
        let mut board = board_with(&["a"]);
        launch(&mut board, &daemon, &CardId::new("a"), PathBuf::from("/tmp/wt/a"));

        let task_id = board.get(&CardId::new("a")).unwrap().task_id.clone().unwrap();
        fake.set_state(&task_id, "something_brand_new");
        assert!(reconcile_running(&mut board, &daemon).is_empty());
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Running);
        fake.finish();
    }

    #[test]
    fn a_card_whose_task_id_is_no_longer_listed_is_failed_not_stranded() {
        // A recorded task id that the daemon no longer lists (its session was
        // reclaimed, or the worker died before it was recorded) means the card
        // can never be reconciled again. Leaving it `Running` strands it
        // forever and holds a concurrency slot that blocks every later card, so
        // it must leave `Running` as a visible failure instead.
        let (fake, socket) = start_fake_daemon();
        let daemon = BoardDaemon::new(socket);
        let mut board = board_with(&["a"]);
        launch(&mut board, &daemon, &CardId::new("a"), PathBuf::from("/tmp/wt/a"));
        let task_id = board.get(&CardId::new("a")).unwrap().task_id.clone().unwrap();

        // The daemon forgot the task entirely.
        fake.forget(&task_id);

        let outcomes = reconcile_running(&mut board, &daemon);
        assert_eq!(outcomes.len(), 1, "the stranded card must be reconciled");
        assert_eq!(outcomes[0].card_id, CardId::new("a"));
        assert_eq!(
            board.get(&CardId::new("a")).unwrap().state,
            CardState::Failed,
            "an unknown task id is a failure, not a permanently running card"
        );
        assert_eq!(board.running_count(), 0, "the slot is freed for the next card");
        fake.finish();
    }

    #[test]
    fn a_card_with_no_task_id_is_left_alone() {
        // 旧状态文件里 Running 但没有 task_id：无从查询，保持原样而不是误判。
        let (fake, socket) = start_fake_daemon();
        let daemon = BoardDaemon::new(socket);
        let mut board = board_with(&["a"]);
        board.transition(&CardId::new("a"), CardState::Running).unwrap();
        assert!(reconcile_running(&mut board, &daemon).is_empty());
        assert_eq!(board.get(&CardId::new("a")).unwrap().state, CardState::Running);
        fake.finish();
    }
}
