//! 卡片调度器的纯决策逻辑：把「看板卡片 + 本进程跟踪的 thread 状态」映射成动作。
//! 与 I/O 解耦，便于单测；真正的 thread/start、plugin_query 在 server.rs 里执行。
//!
//! `run_once` 是调度器的一轮核心：启动（`board.next_launch`）与对账
//! （`board_cards` + `plan`）。副作用（起会话、读 thread 状态）经依赖注入，
//! 故无需 serve / spawn 即可单测。

use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::server::{BoardCard, board_cards, board_query};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The session's turn finished: the card is ready for review.
    AwaitingMerge,
    NeedsYou,
    Failed,
    /// The card is (again) running: a follow-up turn started after the card had
    /// already been reported `awaiting_merge`/`needs_you`.
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CardAction {
    Reconcile {
        card_id: String,
        thread_id: String,
        outcome: Outcome,
    },
    /// 卡处于 `merging` 且其会话已空闲：宿主代调 `merge_finish` 收尾。
    /// 复核以 git 为准，故这个动作幂等。
    RequestMerge { card_id: String, thread_id: String },
}

/// What the host last told the plugin about a tracked card. A card we reported
/// `awaiting_merge`/`needs_you` is still tracked, so a later turn can revive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackState {
    Running,
    AwaitingMerge,
}

#[derive(Debug, Clone)]
pub(crate) struct TrackedThread {
    pub thread_id: String,
    pub state: TrackState,
    /// The thread has no in-flight turn right now.
    pub idle: bool,
    pub failed: bool,
    pub needs_you: bool,
}

/// Decide what to tell the plugin about each tracked card.
///
/// A lone idle snapshot is **not** proof that a card is done: a follow-up turn
/// (or automatic continuation) can start seconds later. So a tracked card is
/// only reported `awaiting_merge` when its still-`Running` entry goes idle, and
/// an entry we already reported as `awaiting_merge` is flipped back to
/// `Running` as soon as its thread is busy again. Cards the host does not track
/// (foreign cards, or `running` cards whose session this process cannot see)
/// are left untouched.
pub(crate) fn plan(
    cards: &[BoardCard],
    tracked: &HashMap<String, TrackedThread>,
) -> Vec<CardAction> {
    cards
        .iter()
        .filter_map(|card| {
            // Terminal cards can never come back; leave them alone.
            if matches!(card.state.as_str(), "done" | "failed" | "cancelled") {
                return None;
            }
            let t = tracked.get(&card.id)?;
            let thread_id = card
                .thread_id
                .clone()
                .unwrap_or_else(|| t.thread_id.clone());
            // 合并轮：卡在 merging 时，会话空闲即收纳尾（会话正在跑则等它）。
            if card.state == "merging" {
                return if t.idle {
                    Some(CardAction::RequestMerge {
                        card_id: card.id.clone(),
                        thread_id,
                    })
                } else {
                    None
                };
            }
            match t.state {
                TrackState::Running => {
                    if !t.idle {
                        return None;
                    }
                    let outcome = if t.failed {
                        Outcome::Failed
                    } else if t.needs_you {
                        Outcome::NeedsYou
                    } else {
                        Outcome::AwaitingMerge
                    };
                    Some(CardAction::Reconcile {
                        card_id: card.id.clone(),
                        thread_id,
                        outcome,
                    })
                }
                TrackState::AwaitingMerge => {
                    if t.idle {
                        return None;
                    }
                    Some(CardAction::Reconcile {
                        card_id: card.id.clone(),
                        thread_id,
                        outcome: Outcome::Running,
                    })
                }
            }
        })
        .collect()
}

/// 宿主侧读到的 thread 标志位。由注入的「状态快照」函数产出;调度器自身不接触
/// `ThreadSession`(那需要 `&mut threads` 与 serve 的并发结构,留给 Task 6c)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThreadFlags {
    pub idle: bool,
    pub failed: bool,
    pub needs_you: bool,
}

/// 一次启动请求:插件交出一张卡,宿主据此起一个可见会话。
#[derive(Debug, Clone)]
pub(crate) struct LaunchRequest {
    pub card_id: String,
    /// 发起这张卡的项目根（canonical 绝对路径）。落进卡片会话的 meta。
    pub board_project: String,
    pub workdir: String,
    pub title: String,
    pub objective: String,
}

/// 起一个会话的副作用。真实实现(`start_thread_core` + `prepare_turn_core`)在
/// Task 6c 接线;本任务用测试替身驱动核心逻辑,从而不必起 serve / spawn。
pub(crate) trait CardLauncher {
    async fn launch(&mut self, request: &LaunchRequest) -> anyhow::Result<String>;
}

/// 看板会话首轮的 objective 文案。
///
/// 与插件 `tick.rs::objective_for` 逐字等价(宿主不依赖插件 crate,故复刻一份,
/// 并把缺口路径退化成空串):它承载 Superpowers 的执行约束,措辞本身是契约。
fn objective_for(plan_path: &str, spec_path: &str) -> String {
    format!(
        "Implement the plan at {plan_path} following its spec at {spec_path}. \
         Work only in this worktree. Use superpowers:subagent-driven-development \
         (or superpowers:executing-plans) to execute it, then \
         superpowers:finishing-a-development-branch to present the integration \
         options to the user. Never merge yourself. If you are blocked, report BLOCKED."
    )
}

/// 终态名,与插件 `board.mark_terminal` 接受的 `outcome` 取值一致。
fn outcome_name(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::AwaitingMerge => "awaiting_merge",
        Outcome::NeedsYou => "needs_you",
        Outcome::Failed => "failed",
        Outcome::Running => "running",
    }
}

/// 调度器的一轮核心逻辑(依赖注入,可单测)。
///
/// 只做两件事,顺序不可换:
/// 1. **启动**:反复 `board.next_launch` 直到 null;每张卡起会话成功后
///    `board.mark_running` 并登记 `tracked`,失败则 `board.release`。
/// 2. **对账**:先用注入的 `flags` 刷新 `tracked.idle`,再 `board_cards` +
///    `plan`,把每个 Reconcile 回写插件。对账**保留 tracked**(不摘除):
///    - 仍在跑的卡空闲 → `board.mark_terminal(awaiting_merge|needs_you|failed)`,
///      记为 `AwaitingMerge`并继续跟踪(会话可能追问续跑);
///    - 已判 `awaiting_merge` 的卡所在会话又忙起来 → `board.mark_running`,
///      让卡片状态跟着会话走;
///    - `failed` 摘除跟踪。
///
/// 所有插件访问都走 `board_query` / `board_cards`(绝不直接读 board.json),
/// 且**不向任何客户端发帧**——起会话的帧副作用被隔离在 `CardLauncher` 之后。
pub(crate) async fn run_once<L: CardLauncher>(
    project: &Path,
    board_dir: &Path,
    tracked: &mut HashMap<String, TrackedThread>,
    launcher: &mut L,
    flags: &(dyn Fn(&str) -> Option<ThreadFlags> + Send + Sync),
) {
    // 1) 启动:把插件交出的每张卡都起成会话,直到它说「没有了」。
    // 插件/daemon 不可用时本轮无法启动(错误即退出循环),留给下一轮重试。
    while let Ok(value) = board_query(project, board_dir, "board.next_launch", json!({})) {
        let Some(card_id) = value
            .get("card_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
        else {
            break;
        };
        let title = value
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("看板卡片")
            .to_string();
        // objective 要 plan/spec 路径,而 next_launch 只交出 card_id;从 list 里补。
        let card = board_cards(project, board_dir)
            .unwrap_or_default()
            .into_iter()
            .find(|card| card.id == card_id);
        // next_launch 的 workdir 是插件刚建的 worktree(权威);卡片记录作为兜底,
        // 以防某天 claim 不再随带 workdir。
        let workdir = value
            .get("workdir")
            .and_then(serde_json::Value::as_str)
            .filter(|workdir| !workdir.is_empty())
            .map(str::to_string)
            .or_else(|| {
                card.as_ref()
                    .and_then(|card| card.workdir.as_ref())
                    .map(|workdir| workdir.to_string_lossy().into_owned())
            })
            .unwrap_or_default();
        let objective = match &card {
            Some(card) => objective_for(&card.plan_path, &card.spec_path),
            None => objective_for("", ""),
        };
        let request = LaunchRequest {
            card_id: card_id.clone(),
            board_project: std::fs::canonicalize(project)
                .unwrap_or_else(|_| project.to_path_buf())
                .to_string_lossy()
                .into_owned(),
            workdir,
            title,
            objective,
        };

        match launcher.launch(&request).await {
            Ok(thread_id) => {
                let _ = board_query(
                    project,
                    board_dir,
                    "board.mark_running",
                    json!({ "card_id": card_id, "thread_id": thread_id }),
                );
                tracked.insert(
                    card_id,
                    TrackedThread {
                        thread_id,
                        state: TrackState::Running,
                        idle: false,
                        failed: false,
                        needs_you: false,
                    },
                );
            }
            Err(error) => {
                let _ = board_query(
                    project,
                    board_dir,
                    "board.release",
                    json!({ "card_id": card_id, "detail": error.to_string() }),
                );
            }
        }
    }

    // 2) 对账:先按当前 thread 状态刷新 idle,再让 plan 决定谁该收尾。
    for entry in tracked.values_mut() {
        if let Some(flags) = flags(&entry.thread_id) {
            entry.idle = flags.idle;
            entry.failed = flags.failed;
            entry.needs_you = flags.needs_you;
        }
    }
    let cards = match board_cards(project, board_dir) {
        Ok(cards) => cards,
        Err(_) => return,
    };
    for action in plan(&cards, tracked) {
        let (card_id, thread_id, outcome) = match action {
            CardAction::Reconcile {
                card_id,
                thread_id,
                outcome,
            } => (card_id, thread_id, Some(outcome)),
            CardAction::RequestMerge { card_id, .. } => {
                // 合并轮的 git 动作发生在会话内；宿主只在会话空闲时触发复核收尾
                // （幂等，以 git 复核为准）。
                let _ = board_query(
                    project,
                    board_dir,
                    "merge_finish",
                    json!({ "card_id": card_id }),
                );
                // 复核后卡会落 done/needs_you：不再由本进程跟踪（终态不会再回来）。
                tracked.remove(&card_id);
                continue;
            }
        };
        let outcome = outcome.expect("Reconcile carries an outcome");
        let (method, params) = match &outcome {
            Outcome::Running => (
                "board.mark_running",
                json!({ "card_id": card_id, "thread_id": thread_id }),
            ),
            _ => (
                "board.mark_terminal",
                json!({ "card_id": card_id, "outcome": outcome_name(&outcome) }),
            ),
        };
        let _ = board_query(project, board_dir, method, params);
        match &outcome {
            // Terminal: stop tracking; the card will not come back.
            Outcome::Failed => {
                tracked.remove(&card_id);
            }
            // A follow-up turn revived the card: track it as running again.
            Outcome::Running => {
                if let Some(entry) = tracked.get_mut(&card_id) {
                    entry.state = TrackState::Running;
                }
            }
            // Ready for review, but keep tracking so a later turn can revive it.
            Outcome::AwaitingMerge | Outcome::NeedsYou => {
                if let Some(entry) = tracked.get_mut(&card_id) {
                    entry.state = TrackState::AwaitingMerge;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::BoardCard;
    use std::collections::HashMap;

    // A running card always carries a thread_id in production: `board.mark_running`
    // sets it before `board_cards` is read. The helper must supply one for the
    // `running` cases, otherwise `plan` (which needs `card.thread_id`) sees nothing.
    fn card(id: &str, state: &str) -> BoardCard {
        BoardCard {
            id: id.into(),
            state: state.into(),
            kind: "implementation".into(),
            source: None,
            base: None,
            thread_id: Some(format!("t-{id}")),
            workdir: None,
            spec_path: format!("{id}.spec.md"),
            plan_path: format!("{id}.plan.md"),
        }
    }

    pub(crate) fn tt(
        thread_id: &str,
        state: TrackState,
        idle: bool,
        needs_you: bool,
    ) -> TrackedThread {
        TrackedThread {
            thread_id: thread_id.into(),
            state,
            idle,
            failed: false,
            needs_you,
        }
    }

    #[test]
    fn a_queued_card_is_not_launched_by_plan_because_the_plugin_owns_slots() {
        // plan() 只负责「对账已 tracked 的卡片」；启动由 next_launch 驱动，不在 plan 里。
        let actions = plan(&[card("a", "queued")], &Default::default());
        assert!(actions.is_empty());
    }

    #[test]
    fn a_tracked_card_whose_thread_finished_with_changes_awaits_merge() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), tt("t1", TrackState::Running, true, false));
        let actions = plan(&[card("a", "running")], &tracked);
        assert!(matches!(actions.as_slice(),
            [CardAction::Reconcile { card_id, outcome: Outcome::AwaitingMerge, .. }] if card_id == "a"));
    }

    #[test]
    fn a_busy_tracked_card_is_left_alone() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), tt("t1", TrackState::Running, false, false));
        assert!(plan(&[card("a", "running")], &tracked).is_empty());
    }

    #[test]
    fn an_idle_thread_without_changes_needs_you() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), tt("t1", TrackState::Running, true, true));
        let actions = plan(&[card("a", "running")], &tracked);
        assert!(matches!(
            &actions[0],
            CardAction::Reconcile {
                outcome: Outcome::NeedsYou,
                ..
            }
        ));
    }

    /// 一条空闲快照把卡判成 `awaiting_merge` 后，其会话再忙起来必须把卡翻回
    /// `running`——否则卡片会停在错误的终态（本 bug 的核心）。
    #[test]
    fn a_card_awaiting_review_is_revived_when_its_thread_runs_again() {
        let mut tracked = HashMap::new();
        tracked.insert(
            "a".to_string(),
            tt("t1", TrackState::AwaitingMerge, false, false),
        );
        let actions = plan(&[card("a", "awaiting_merge")], &tracked);
        assert!(matches!(actions.as_slice(),
            [CardAction::Reconcile { card_id, outcome: Outcome::Running, .. }] if card_id == "a"));
    }

    #[test]
    fn an_idle_awaiting_review_card_stays_awaiting_review() {
        let mut tracked = HashMap::new();
        tracked.insert(
            "a".to_string(),
            tt("t1", TrackState::AwaitingMerge, true, false),
        );
        assert!(plan(&[card("a", "awaiting_merge")], &tracked).is_empty());
    }

    #[test]
    fn a_card_the_host_does_not_track_is_left_alone() {
        // 别的进程起的会话，或本进程看不到的 running 卡，都不该被误判。
        assert!(plan(&[card("a", "running")], &Default::default()).is_empty());
    }

    #[test]
    fn a_terminal_card_is_never_revived() {
        let mut tracked = HashMap::new();
        tracked.insert(
            "a".to_string(),
            tt("t1", TrackState::AwaitingMerge, false, false),
        );
        for state in ["done", "failed", "cancelled"] {
            assert!(plan(&[card("a", state)], &tracked).is_empty(), "{state}");
        }
    }

    #[test]
    fn an_idle_merging_card_is_finished_by_the_host() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), tt("t1", TrackState::Running, true, false));
        let actions = plan(&[card("a", "merging")], &tracked);
        assert!(matches!(
            actions.as_slice(),
            [CardAction::RequestMerge { card_id, .. }] if card_id == "a"
        ));
    }

    #[test]
    fn a_merging_card_still_running_is_left_alone() {
        let mut tracked = HashMap::new();
        tracked.insert("a".to_string(), tt("t1", TrackState::Running, false, false));
        assert!(plan(&[card("a", "merging")], &tracked).is_empty());
    }

    #[test]
    fn an_untracked_merging_card_is_left_alone() {
        // 别的进程起的合并会话：本进程无法判断它跑完没有，不动。
        assert!(plan(&[card("a", "merging")], &Default::default()).is_empty());
    }
}

#[cfg(test)]
mod scheduler_tests {
    use super::*;
    use crate::card_scheduler::tests::tt;
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};

    /// 一台假看板插件 daemon:按脚本回答 `board.next_launch`(按序 pop,耗尽即
    /// null)与 `list`(固定 cards),其余 `board.*` 回 `{"ok":true}`;并把收到的
    /// 每次调用记下来,供测试断言宿主真的走的是 `board_query` 这条线。
    struct FakeBoard {
        project: PathBuf,
        board_dir: PathBuf,
        calls: Arc<StdMutex<Vec<serde_json::Value>>>,
        stop: Arc<AtomicBool>,
        socket: PathBuf,
        handle: Option<std::thread::JoinHandle<()>>,
        _dir: tempfile::TempDir,
    }

    impl FakeBoard {
        fn new(launches: Vec<serde_json::Value>, cards: serde_json::Value) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let project = dir.path().join("proj");
            std::fs::create_dir_all(&project).unwrap();
            let board_dir = dir.path().join("global");
            yi_agent_boards::registry::register(&board_dir, &project).unwrap();

            let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&project);
            std::fs::create_dir_all(&runtime_dir).unwrap();
            let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();

            let calls: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
            let recorded = Arc::clone(&calls);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let queue = Arc::new(StdMutex::new(VecDeque::from(launches)));
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        continue;
                    }
                    let request: serde_json::Value =
                        serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                    let method = request["command"]["method"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    recorded.lock().unwrap().push(serde_json::json!({
                        "method": method,
                        "params": request["command"]["params"].clone(),
                    }));
                    let value = match method.as_str() {
                        "board.next_launch" => queue
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or(serde_json::Value::Null),
                        "list" => cards.clone(),
                        _ => serde_json::json!({ "ok": true }),
                    };
                    let reply = serde_json::json!({
                        "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                        "request_id": request["request_id"],
                        "result": { "type": "PluginResult", "value": value },
                    });
                    let mut stream = stream;
                    let _ = stream.write_all(reply.to_string().as_bytes());
                    let _ = stream.write_all(b"\n");
                }
            });

            Self {
                project,
                board_dir,
                calls,
                stop,
                socket,
                handle: Some(handle),
                _dir: dir,
            }
        }

        fn calls_to(&self, method: &str) -> Vec<serde_json::Value> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call["method"] == method)
                .cloned()
                .collect()
        }
    }

    impl Drop for FakeBoard {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = UnixStream::connect(&self.socket);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[derive(Default)]
    struct FakeLauncher {
        seen: Vec<LaunchRequest>,
        thread_id: String,
        fail: Option<String>,
    }

    impl CardLauncher for FakeLauncher {
        async fn launch(&mut self, request: &LaunchRequest) -> anyhow::Result<String> {
            self.seen.push(request.clone());
            match &self.fail {
                Some(detail) => Err(anyhow::anyhow!(detail.clone())),
                None => Ok(self.thread_id.clone()),
            }
        }
    }

    fn thread_flags<'a>(
        idle: impl Fn(&str) -> bool + Send + Sync + 'a,
    ) -> impl Fn(&str) -> Option<ThreadFlags> + Send + Sync + 'a {
        move |thread_id: &str| {
            Some(ThreadFlags {
                idle: idle(thread_id),
                failed: false,
                needs_you: false,
            })
        }
    }

    /// 一张卡被 `board.next_launch` 交出来 → 宿主起会话并向插件报 running。
    #[tokio::test]
    async fn the_scheduler_launches_a_claimed_card_and_reports_it_running() {
        let board = FakeBoard::new(
            vec![
                serde_json::json!({"card_id": "c1", "workdir": "/w", "title": "T"}),
                serde_json::Value::Null,
            ],
            serde_json::json!({"cards": [
                {"id": "c1", "state": "queued", "spec_path": "c1.spec.md", "plan_path": "c1.plan.md"}
            ]}),
        );
        let mut tracked = HashMap::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-1".to_string(),
            ..Default::default()
        };

        run_once(
            &board.project,
            &board.board_dir,
            &mut tracked,
            &mut launcher,
            &|_| None,
        )
        .await;

        assert_eq!(launcher.seen.len(), 1, "exactly one claim is launched");
        let request = &launcher.seen[0];
        assert_eq!(request.card_id, "c1");
        assert_eq!(request.workdir, "/w");
        assert_eq!(request.title, "T");
        assert!(
            request.objective.contains("c1.plan.md") && request.objective.contains("c1.spec.md"),
            "objective must carry the card's plan and spec: {}",
            request.objective
        );
        let expected_project = std::fs::canonicalize(&board.project)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            request.board_project, expected_project,
            "the launch must carry the project root for grouping"
        );

        let running = board.calls_to("board.mark_running");
        assert_eq!(
            running.len(),
            1,
            "the plugin must learn the card is running"
        );
        assert_eq!(running[0]["params"]["card_id"], "c1");
        assert_eq!(running[0]["params"]["thread_id"], "thread-1");

        let tracked_thread = tracked.get("c1").expect("the card stays tracked");
        assert_eq!(tracked_thread.thread_id, "thread-1");
    }

    /// 一个已 tracked 且已 idle 的 running 卡,对账后被回写 `awaiting_merge`,但
    /// **仍被跟踪**——它的会话稍后可能追问续跑。
    #[tokio::test]
    async fn an_idle_tracked_thread_is_reconciled_to_awaiting_merge() {
        let board = FakeBoard::new(
            Vec::new(),
            serde_json::json!({"cards": [
                {"id": "c1", "state": "running", "thread_id": "thread-1",
                 "spec_path": "s", "plan_path": "p"}
            ]}),
        );
        let mut tracked = HashMap::new();
        tracked.insert(
            "c1".to_string(),
            tt("thread-1", TrackState::Running, false, false),
        );
        let mut launcher = FakeLauncher::default();

        run_once(
            &board.project,
            &board.board_dir,
            &mut tracked,
            &mut launcher,
            &thread_flags(|_| true),
        )
        .await;

        assert!(launcher.seen.is_empty(), "nothing to launch");
        assert!(
            board.calls_to("board.mark_running").is_empty(),
            "a first-turn idle snapshot must not be reported as running"
        );
        let terminal = board.calls_to("board.mark_terminal");
        assert_eq!(terminal.len(), 1, "{:?}", board.calls.lock().unwrap());
        assert_eq!(terminal[0]["params"]["card_id"], "c1");
        assert_eq!(terminal[0]["params"]["outcome"], "awaiting_merge");
        assert_eq!(
            tracked.get("c1").map(|t| t.state),
            Some(TrackState::AwaitingMerge),
            "the card stays tracked so a later turn can revive it"
        );
    }

    /// 已判 `awaiting_merge` 的卡，其会话又开始跑（追问/自动续跑）→ 卡片翻回
    /// `running`。这正是本 bug 的修复点：误判后的卡必须能被纠正回来。
    #[tokio::test]
    async fn a_card_awaiting_review_is_revived_when_its_thread_runs_again() {
        let board = FakeBoard::new(
            Vec::new(),
            serde_json::json!({"cards": [
                {"id": "c1", "state": "awaiting_merge", "thread_id": "thread-1",
                 "spec_path": "s", "plan_path": "p"}
            ]}),
        );
        let mut tracked = HashMap::new();
        tracked.insert(
            "c1".to_string(),
            tt("thread-1", TrackState::AwaitingMerge, true, false),
        );
        let mut launcher = FakeLauncher::default();

        run_once(
            &board.project,
            &board.board_dir,
            &mut tracked,
            &mut launcher,
            &thread_flags(|_| false),
        )
        .await;

        let running = board.calls_to("board.mark_running");
        assert_eq!(running.len(), 1, "{:?}", board.calls.lock().unwrap());
        assert_eq!(running[0]["params"]["card_id"], "c1");
        assert_eq!(running[0]["params"]["thread_id"], "thread-1");
        assert_eq!(
            tracked.get("c1").map(|t| t.state),
            Some(TrackState::Running)
        );
    }

    /// launch 失败时释放名额,绝不能谎报 running。
    #[tokio::test]
    async fn a_failed_launch_is_released_rather_than_marked_running() {
        let board = FakeBoard::new(
            vec![
                serde_json::json!({"card_id": "c1", "workdir": "/w", "title": "T"}),
                serde_json::Value::Null,
            ],
            serde_json::json!({"cards": []}),
        );
        let mut tracked = HashMap::new();
        let mut launcher = FakeLauncher {
            fail: Some("build failed".to_string()),
            ..Default::default()
        };

        run_once(
            &board.project,
            &board.board_dir,
            &mut tracked,
            &mut launcher,
            &|_| None,
        )
        .await;

        assert!(tracked.is_empty(), "a failed launch tracks nothing");
        assert!(board.calls_to("board.mark_running").is_empty());
        let released = board.calls_to("board.release");
        assert_eq!(released.len(), 1, "{:?}", board.calls.lock().unwrap());
        assert_eq!(released[0]["params"]["card_id"], "c1");
        assert!(
            released[0]["params"]["detail"]
                .as_str()
                .unwrap()
                .contains("build failed")
        );
    }
}
