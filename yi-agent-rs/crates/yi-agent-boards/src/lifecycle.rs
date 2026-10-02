//! The board lifecycle: create, remove, and report on a project's board.
//!
//! `create`/`remove` are the two mutations the UI can trigger, `status` the
//! read it polls. Every filesystem and process dependency is reachable through
//! a `_with_project`/`_with` variant that takes the global registry directory
//! and the launcher/stopper, so the whole lifecycle is exercised without a real
//! daemon; the public [`create`]/[`remove`] wrappers supply the production
//! implementations.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::board_daemon;
use crate::registry;
use crate::scaffold;

/// How long [`create`] waits for a freshly spawned daemon to answer before
/// giving up on it. Generous enough for a cold plugin load, short enough that a
/// wedged start does not look like a hung UI.
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The plugin method whose answer carries the card list, and the params it
/// takes. The host does not interpret the board, so both are plain data.
const CARDS_METHOD: &str = "list";

/// What the sidebar and the main area need to know about a project's board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardStatus {
    /// Whether the project is in the global registry.
    pub registered: bool,
    /// Whether the project's daemon answers right now.
    pub daemon_running: bool,
    /// Cards waiting for a slot.
    pub queued: usize,
    /// Cards occupying a slot.
    pub running: usize,
}

/// Where a project keeps its board: the queue *and* the manifest the switch
/// reads. Removal deletes this, but only after the manifest has been written
/// separately, which is why the two are distinct paths on disk.
fn state_dir(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("superpowers-kanban")
}

/// The daemon socket serving `project`.
fn runtime_socket(project: &Path) -> Option<PathBuf> {
    yi_agent_store::ipc::socket_path_for(&project.join(".yi-agent").join("runtime")).ok()
}

/// Create `project`'s board in the default global registry. See [`create_in`].
pub fn create(project: &Path) -> Result<BoardStatus, String> {
    let global = crate::global_dir().map_err(|error| error.to_string())?;
    create_in(project, &global)
}

/// Create `project`'s board in `global`, using the real daemon lifecycle.
///
/// Separate from [`create_with_project`] so a caller that already resolved the
/// global directory (the app-server, which keeps it alongside its other
/// injectable paths) registers in the *same* place it lists from.
pub fn create_in(project: &Path, global: &Path) -> Result<BoardStatus, String> {
    let mut launcher = |project: &Path| launch_if_absent(project);
    create_with_project(project, global, &mut launcher)
}

/// Remove `project`'s board from the default global registry. See [`remove_in`].
///
/// Stops the daemon, deletes the queue, and unregisters — but keeps the
/// manifest and the preference switch, which are the project's *intent* to run
/// a board rather than board data (decision 5: removal is not reversible for
/// the queue, so the config is what makes a re-create cheap).
pub fn remove(project: &Path) -> Result<(), String> {
    let global = crate::global_dir().map_err(|error| error.to_string())?;
    remove_in(project, &global)
}

/// Remove `project`'s board from `global`, using the real daemon lifecycle.
///
/// The stopper can block for up to the daemon's shutdown timeout; that is
/// acceptable here because removal is a user-triggered RPC, not a poll.
pub fn remove_in(project: &Path, global: &Path) -> Result<(), String> {
    let mut stopper = |project: &Path| board_daemon::stop(project);
    remove_with(project, global, &mut stopper)
}

/// `project`'s board as it is right now. Never fails: a status query that blew
/// up would take the sidebar down with it, and "not registered, nothing to
/// count" is a perfectly good answer.
pub fn status(project: &Path) -> BoardStatus {
    let global = match crate::global_dir() {
        Ok(dir) => dir,
        Err(_) => PathBuf::new(),
    };
    status_with(project, &global)
}

/// Start the production launcher: reuse a daemon that is already serving the
/// project, otherwise spawn one detached and wait for it to answer.
///
/// Public so the app-server can inject it into its dispatch loop unchanged; the
/// `board/create` RPC and the TUI command therefore start daemons the same way.
pub fn launch_if_absent(project: &Path) -> Result<bool, String> {
    // The spec's rule: a project that already has a live daemon (perhaps
    // because a thread was opened there first) keeps it. Two daemons on one
    // project would fight over the runtime lock.
    if board_daemon::is_running(project) {
        return Ok(true);
    }
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate this executable to start the board daemon: {error}"))?;
    board_daemon::spawn_detached(&exe, project)
        .map_err(|error| format!("could not start the board daemon: {error}"))?;
    Ok(board_daemon::wait_ready(project, READY_TIMEOUT))
}

/// Create with an injected launcher and global directory.
///
/// Idempotence is keyed on the daemon, not on the registry: the launcher runs
/// whenever no daemon answers for the project. A project that is already
/// registered but whose daemon has died (the app was closed, the machine
/// rebooted) is therefore relaunched rather than reported dead forever — the
/// registry is a record of intent, not a reason to skip recovery. "Two daemons
/// for one project" still cannot happen: the real launcher is itself idempotent
/// (it returns early when `is_running`), and an injected launcher used in tests
/// is expected to leave a daemon answering, exactly as the real one does.
pub fn create_with_project(
    project: &Path,
    global: &Path,
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
) -> Result<BoardStatus, String> {
    // Only a real directory can be a project. Accepting a path that does not
    // exist — a typo, a project since deleted — would scaffold a manifest
    // inside it, register a board that can never answer, and draw a sidebar
    // entry that opens onto nothing.
    if !project.is_dir() {
        return Err(format!(
            "cannot create a board for {}: not a directory",
            project.display()
        ));
    }
    scaffold::install_manifest(project).map_err(|error| error.to_string())?;
    scaffold::enable_switch(project).map_err(|error| error.to_string())?;

    let daemon_running = if board_daemon::is_running(project) {
        true
    } else {
        launcher(project)?
    };

    registry::register(global, project).map_err(|error| error.to_string())?;

    Ok(BoardStatus {
        registered: true,
        daemon_running,
        queued: 0,
        running: 0,
    })
}

/// Remove with an injected stopper and global directory.
pub fn remove_with(
    project: &Path,
    global: &Path,
    stopper: &mut dyn FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    stopper(project)?;
    // The daemon is already gone, so nothing can be writing the queue while it
    // is deleted. A project with no queue is already removed.
    match std::fs::remove_dir_all(state_dir(project)) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "could not delete the board queue at {}: {error}",
                state_dir(project).display()
            ))
        }
    }
    registry::unregister(global, project).map_err(|error| error.to_string())
}

/// Report on a project's board using an explicit global directory.
///
/// Counts come from the project's own daemon, because the queue lives in the
/// project and the daemon is what runs the plugin that owns it. A daemon or
/// plugin that cannot answer leaves the counts at zero: the numbers are a
/// detail, the registered/running flags are the answer.
pub fn status_with(project: &Path, global: &Path) -> BoardStatus {
    let registered = registry::contains(global, project).unwrap_or(false);
    let daemon_running = board_daemon::is_running(project);
    let (queued, running) = if daemon_running {
        card_counts(project).unwrap_or((0, 0))
    } else {
        (0, 0)
    };
    BoardStatus {
        registered,
        daemon_running,
        queued,
        running,
    }
}

/// Ask the project's plugin for its cards and count them by state.
///
/// Goes through the daemon's `PluginQuery` rather than reading
/// `<project>/.yi-agent/superpowers-kanban/board.json` directly: the daemon is
/// the only thing that tracks which plugins are actually running, and it keeps
/// its answer and its socket consistent. The host still does not interpret the
/// board beyond "how many are queued, how many are running" — the counts the
/// sidebar badge needs.
fn card_counts(project: &Path) -> Option<(usize, usize)> {
    let socket = runtime_socket(project)?;
    let response = yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::PluginQuery {
            plugin: scaffold::plugin_name().to_string(),
            method: CARDS_METHOD.to_string(),
            params: serde_json::Value::Object(serde_json::Map::new()),
        },
    )
    .ok()?;
    let yi_agent_store::ipc::IpcResponse::PluginResult { value } = response else {
        return None;
    };
    let cards = value.get("cards")?.as_array()?;
    let mut queued = 0;
    let mut running = 0;
    for card in cards {
        match card.get("state").and_then(serde_json::Value::as_str) {
            Some("queued") => queued += 1,
            Some("running") => running += 1,
            // Every other state (done, failed, awaiting_merge, …) is neither
            // waiting for a slot nor holding one, which is exactly what the
            // badge summarises.
            _ => {}
        }
    }
    Some((queued, running))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::contains;
    use std::path::Path;

    #[test]
    fn create_scaffolds_registers_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");

        // 注入的「启动器」按真实启动器的语义工作：第一次调用时绑定项目的
        // runtime socket 并派一个线程在那里回答 Status（之后复用同一个
        // listener，绝不绑第二个）。于是第一次 create 真的把一个 daemon
        // 「拉起来了」，第二次 create 的 is_running 探测为真、从而短路。这样
        // 「重复创建不该再起一个 daemon」由 daemon 的存在担保，而不是由登记
        // 表——一个 daemon 已死但仍在登记表里的项目才能被 create 重新拉起。
        let socket = runtime_socket(&project).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let mut listener: Option<std::os::unix::net::UnixListener> = None;
        let mut daemon: Option<std::thread::JoinHandle<usize>> = None;

        let launched = std::cell::Cell::new(0);
        let mut launcher = |p: &Path| {
            launched.set(launched.get() + 1);
            if listener.is_none() {
                let bound = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                listener = Some(bound.try_clone().unwrap());
                daemon = Some(std::thread::spawn(move || {
                    // create #1 的探测发生在 socket 出现之前（那时 is_running
                    // 为假、不建连）；只有 create #2 会真正连上来探一次。
                    let mut served = 0usize;
                    for _ in 0..1 {
                        let Ok((stream, _)) = bound.accept() else {
                            return served;
                        };
                        let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                            return served;
                        }
                        let request: serde_json::Value =
                            serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                        assert_eq!(
                            request["command"]["type"].as_str(),
                            Some("Status"),
                            "create 只应探 Status：{request}"
                        );
                        let reply = serde_json::json!({
                            "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                            "request_id": request["request_id"],
                            "result": { "type": "Status", "high_water_event_id": 0 },
                        });
                        let mut stream = stream;
                        std::io::Write::write_all(&mut stream, reply.to_string().as_bytes()).unwrap();
                        std::io::Write::write_all(&mut stream, b"\n").unwrap();
                        served += 1;
                    }
                    served
                }));
            }
            let _ = p;
            Ok(true)
        };

        let first = create_with_project(&project, &global, &mut launcher).unwrap();
        assert!(first.registered);
        assert_eq!(launched.get(), 1);
        // 清单与开关就位
        assert!(
            project
                .join(".yi-agent/supervisors/superpowers-kanban.json")
                .exists()
        );
        let prefs: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(project.join(".yi-agent/preferences.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(prefs["superpowers_kanban"], true);

        // 再创建一次：不重复起进程
        let second = create_with_project(&project, &global, &mut launcher).unwrap();
        assert!(second.registered);
        assert_eq!(launched.get(), 1, "重复创建不该再起一个 daemon");

        drop(listener);
        assert_eq!(
            daemon.unwrap().join().unwrap(),
            1,
            "daemon 应被 create #2 探一次 Status（create #1 时 socket 尚不存在）"
        );
    }

    #[test]
    fn create_relaunches_a_registered_board_whose_daemon_died() {
        // 项目已在登记表里，但 daemon 没在跑（app 关过、机器重启过：socket 不在，
        // is_running 为假）。create 必须把它重新拉起来，而不是返回
        // daemon_running:false 就把这个项目永远钉死。登记表记录的是意图，不是
        // 「不许再启动」——否则看板会静默死掉，唯一的补救是手改 boards.json。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        crate::registry::register(&global, &project).unwrap();
        assert!(
            crate::registry::contains(&global, &project).unwrap(),
            "前置条件：项目已登记"
        );

        let launched = std::cell::Cell::new(0);
        let mut launcher = |_p: &Path| {
            launched.set(launched.get() + 1);
            Ok(true)
        };

        let created = create_with_project(&project, &global, &mut launcher).unwrap();
        assert_eq!(
            launched.get(),
            1,
            "已登记但 daemon 已死的项目必须被重新拉起"
        );
        assert!(created.registered);
        assert!(created.daemon_running, "{created:?}");
    }

    #[test]
    fn create_refuses_a_project_that_is_not_a_directory() {
        // board/create 的 project 由调用方给出。一个不存在的路径若被当成项目，
        // RPC 会回答「已创建」并登记一条永远连不上的看板：清单建在一个空目录
        // 里、daemon 起在别处、侧栏画出的条目点开什么都没有。宁可当场拒绝。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("missing"); // 故意不创建
        let global = dir.path().join("global");
        let mut launcher = |_p: &Path| Ok(true);

        let error = create_with_project(&project, &global, &mut launcher).unwrap_err();
        assert!(error.contains("not a directory"), "{error}");
        assert!(
            !global.join("boards.json").exists(),
            "被拒绝的创建不得留下登记"
        );
        assert!(
            !project
                .join(".yi-agent/supervisors/superpowers-kanban.json")
                .exists(),
            "被拒绝的创建不得留下清单"
        );
    }

    #[test]
    fn remove_stops_unregisters_and_deletes_the_queue_but_keeps_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(project.join(".yi-agent/superpowers-kanban")).unwrap();
        std::fs::write(project.join(".yi-agent/superpowers-kanban/board.json"), "{}").unwrap();
        let global = dir.path().join("global");
        let mut launcher = |_p: &Path| Ok(true);
        create_with_project(&project, &global, &mut launcher).unwrap();

        let mut stopped = 0;
        let mut stopper = |_p: &Path| {
            stopped += 1;
            Ok(())
        };
        remove_with(&project, &global, &mut stopper).unwrap();

        assert_eq!(stopped, 1);
        assert!(
            !project
                .join(".yi-agent/superpowers-kanban/board.json")
                .exists(),
            "队列状态必须被删"
        );
        assert!(
            project
                .join(".yi-agent/supervisors/superpowers-kanban.json")
                .exists(),
            "清单是配置不是数据，必须保留"
        );
        // 开关同理：它也是配置，删掉就等于「用户从没开过」，重建时会被重置。
        let prefs: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(project.join(".yi-agent/preferences.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            prefs["superpowers_kanban"], true,
            "开关是配置不是数据，必须保留：{prefs}"
        );
        assert!(!contains(&global, &project).unwrap());
    }

    #[test]
    fn create_registers_the_project_so_status_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let mut launcher = |_p: &Path| Ok(true);

        let created = create_with_project(&project, &global, &mut launcher).unwrap();
        assert!(created.registered);

        let reported = status_with(&project, &global);
        assert!(
            reported.registered,
            "刚创建的项目必须出现在状态里：{reported:?}"
        );
    }

    #[test]
    fn status_counts_nothing_when_no_daemon_answers() {
        // 没有 daemon 就没有队列可数。这里要的是 0，而不是一个能把侧栏
        // 打没的 Err：状态查询只许回答「现在是什么样」。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");

        let reported = status_with(&project, &global);
        assert_eq!(reported.queued, 0);
        assert_eq!(reported.running, 0);
        assert!(!reported.daemon_running);
    }

    #[test]
    fn a_registered_board_reports_itself_registered_even_with_no_daemon() {
        // 登记状态与 daemon 状态正交：app 刚重启时 daemon 可能还没起，
        // 但侧栏仍必须给已登记的项目画看板条目。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        crate::registry::register(&global, &project).unwrap();

        let reported = status_with(&project, &global);
        assert!(reported.registered, "{reported:?}");
        assert!(!reported.daemon_running, "{reported:?}");
        assert_eq!(reported.queued, 0);
        assert_eq!(reported.running, 0);
    }

    #[test]
    fn status_counts_queued_and_running_from_the_plugin() {
        // 计数不是「永远 0」：它经 daemon 问插件、按 state 数。用一个假的
        // 项目 daemon 回答 Status 与 PluginQuery，钉住这条链路。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        crate::registry::register(&global, &project).unwrap();

        let socket = runtime_socket(&project).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                    return;
                }
                let request: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                let result = match request["command"]["type"].as_str() {
                    Some("Status") => serde_json::json!({
                        "type": "Status",
                        "high_water_event_id": 0,
                    }),
                    Some("PluginQuery") => serde_json::json!({
                        "type": "PluginResult",
                        "value": {
                            "cards": [
                                { "id": "a", "state": "queued" },
                                { "id": "b", "state": "queued" },
                                { "id": "c", "state": "running" },
                                { "id": "d", "state": "done" },
                            ],
                        },
                    }),
                    other => panic!("unexpected request: {other:?}"),
                };
                let reply = serde_json::json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": result,
                });
                let mut stream = stream;
                std::io::Write::write_all(&mut stream, reply.to_string().as_bytes()).unwrap();
                std::io::Write::write_all(&mut stream, b"\n").unwrap();
            }
        });

        let reported = status_with(&project, &global);
        handle.join().unwrap();

        assert!(reported.daemon_running, "{reported:?}");
        assert_eq!(reported.queued, 2, "两张 queued：{reported:?}");
        assert_eq!(reported.running, 1, "一张 running：{reported:?}");
    }

    #[test]
    fn create_reuses_a_daemon_that_is_already_answering() {
        // 项目下已经有 daemon 时（例如用户先在这里开过线程）create 必须复用它，
        // 而不是再起一个：同项目两个 daemon 会争同一个 runtime 锁。用一个假的
        // 项目 daemon 回答 Status，钉住「is_running 为真就绝不调启动器」。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");

        let socket = runtime_socket(&project).unwrap();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // create 只探一次 Status；照既有假 daemon 的写法按次数收连接。
        let handle = std::thread::spawn(move || {
            for _ in 0..1 {
                let Ok((stream, _)) = listener.accept() else {
                    return;
                };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                    return;
                }
                let request: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                assert_eq!(
                    request["command"]["type"].as_str(),
                    Some("Status"),
                    "create 复用已有 daemon 前只应探一次 Status：{request}"
                );
                let reply = serde_json::json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": { "type": "Status", "high_water_event_id": 0 },
                });
                let mut stream = stream;
                std::io::Write::write_all(&mut stream, reply.to_string().as_bytes()).unwrap();
                std::io::Write::write_all(&mut stream, b"\n").unwrap();
            }
        });

        let mut launcher = |_p: &Path| panic!("已有 daemon 在跑时不得再起一个");
        let created = create_with_project(&project, &global, &mut launcher).unwrap();
        handle.join().unwrap();

        assert!(created.registered, "{created:?}");
        assert!(created.daemon_running, "必须报告复用的 daemon 在跑：{created:?}");
    }

    #[test]
    fn create_serialises_status_the_ui_can_render() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let mut launcher = |_p: &Path| Ok(true);

        let created = create_with_project(&project, &global, &mut launcher).unwrap();
        assert_eq!(
            serde_json::to_value(&created).unwrap(),
            serde_json::json!({
                "registered": true,
                "daemon_running": true,
                "queued": 0,
                "running": 0,
            })
        );
    }
}
