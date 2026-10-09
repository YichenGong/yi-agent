//! `/superpowers-kanban create|remove|status`——经生命周期管理**本项目**的看板。
//!
//! 看板的创建/移除/查询是 host 侧的动作（写登记表、起停 daemon、删队列），
//! 与插件通道无关，所以走 here 的 `yi_agent_boards::lifecycle` 而不是 daemon 的
//! `plugin/query`。`global` 由调用方注入，生产传 `yi_agent_boards::global_dir()`，
//! 测试传临时目录——否则测试会去动真实 `$HOME` 下的登记表。

use std::path::Path;

use yi_agent_boards::lifecycle::{self, BoardStatus};

/// What a board-lifecycle invocation produced: the lines to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardOutcome {
    pub lines: Vec<String>,
}

/// 经生命周期管理本项目看板。`global` 注入以便测试（生产传 `global_dir()`）。
///
/// `message` 只认 `create` / `remove` / `status`；其余（含空串）给 usage。
pub fn handle_board(project: &Path, global: &Path, message: &str) -> BoardOutcome {
    let resident = yi_agent_store::resident::default_dir().unwrap_or_default();
    let mut launcher = |project: &Path| lifecycle::launch_if_absent(project);
    let mut stopper = |project: &Path| yi_agent_boards::board_daemon::stop(project);
    handle_board_with(
        project,
        global,
        &resident,
        message,
        &mut launcher,
        &mut stopper,
    )
}

/// 与 [`handle_board`] 同一套语义，但把启动/停止 daemon 注入进来。
///
/// 生产实现真的会起进程，测试里 daemon 起不来也无妨：`create_in` 的报告因此
/// 只能是「登记与清单就位、daemon 未就绪」；要钉住「daemon 就绪时也必须说清
/// 楚」，只能注入一个会留下应答者的启动器。`remove` 的 stopper 同理——进程没
/// 起来时真实 stop 本就是 no-op。
pub fn handle_board_with(
    project: &Path,
    global: &Path,
    resident: &Path,
    message: &str,
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
    stopper: &mut dyn FnMut(&Path) -> Result<(), String>,
) -> BoardOutcome {
    match message.trim() {
        "create" => match lifecycle::create_with_project(project, global, resident, launcher) {
            Ok(status) => BoardOutcome {
                lines: vec![format!(
                    "看板已创建（{}）",
                    daemon_phrase(status.daemon_running)
                )],
            },
            Err(error) => lines(vec![format!("看板创建失败: {error}")]),
        },
        "remove" => match lifecycle::remove_with(project, global, resident, stopper) {
            Ok(()) => lines(vec!["看板已移除（队列已删，清单与开关保留）".to_string()]),
            Err(error) => lines(vec![format!("看板移除失败: {error}")]),
        },
        "status" => BoardOutcome {
            lines: render_status(&lifecycle::status_with(project, global)),
        },
        _ => lines(vec![
            "用法: /superpowers-kanban [create|remove|status]".to_string(),
        ]),
    }
}

/// 一行「看板是否在跑」。daemon 没就绪就直接说清，含恢复指引，不留悬案。
fn daemon_phrase(daemon_running: bool) -> String {
    if daemon_running {
        "daemon 已就绪".to_string()
    } else {
        "daemon 未就绪，下次打开本目录会自动拉起".to_string()
    }
}

/// `status` 的正文：未登记时给创建指引，已登记时报 daemon 与计数。
fn render_status(status: &BoardStatus) -> Vec<String> {
    if !status.registered {
        return vec!["本目录尚未创建看板；用 /superpowers-kanban create 创建".to_string()];
    }
    if status.daemon_running {
        vec![format!(
            "本目录的看板已登记，daemon 在跑（排队 {}，运行中 {}）",
            status.queued, status.running
        )]
    } else {
        vec!["本目录的看板已登记，但 daemon 未在跑".to_string()]
    }
}

fn lines(lines: Vec<String>) -> BoardOutcome {
    BoardOutcome { lines }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个「真的把 daemon 拉起来」的启动器：在项目的 runtime socket 上绑定并
    /// 派一个线程应答 `Status`，于是 `is_running` 为真——和 `lifecycle` 自己的
    /// 测试夹具同一个套路。用来钉住 create 的 daemon-ready 说法。
    fn launcher_that_leaves_a_daemon() -> impl FnMut(&Path) -> Result<bool, String> {
        move |project: &Path| {
            let socket =
                yi_agent_store::ipc::socket_path_for(&project.join(".yi-agent").join("runtime"))
                    .map_err(|error| error.to_string())?;
            std::fs::create_dir_all(socket.parent().unwrap()).map_err(|error| error.to_string())?;
            let listener = std::os::unix::net::UnixListener::bind(&socket)
                .map_err(|error| error.to_string())?;
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { break };
                    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
                        continue;
                    }
                    let request: serde_json::Value =
                        serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
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
            Ok(true)
        }
    }

    fn noop_stopper() -> impl FnMut(&Path) -> Result<(), String> {
        |_project: &Path| Ok(())
    }

    #[test]
    fn create_reports_a_created_board_for_this_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        // `handle_board`（生产包装）会把常驻登记写进真实的 `$HOME`；测试改走
        // `_with` 变体，把 resident 也注入临时目录，免得碰开发者本机的登记。
        let resident = dir.path().join("resident");
        let mut launcher = |_p: &Path| Ok(false);
        let mut stopper = noop_stopper();
        let out = handle_board_with(
            &project,
            &global,
            &resident,
            "create",
            &mut launcher,
            &mut stopper,
        );
        assert!(
            out.lines.iter().any(|l| l.contains("看板已创建")),
            "{:?}",
            out.lines
        );
        assert!(yi_agent_boards::registry::contains(&global, &project).unwrap());
    }

    #[test]
    fn status_says_when_this_directory_has_no_board() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let out = handle_board(&project, &global, "status");
        assert!(
            out.lines.iter().any(|l| l.contains("未创建")),
            "{:?}",
            out.lines
        );
    }

    #[test]
    fn remove_unregisters_and_keeps_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let resident = dir.path().join("resident");
        let mut launcher = |_p: &Path| Ok(false);
        let mut stopper = noop_stopper();
        handle_board_with(
            &project,
            &global,
            &resident,
            "create",
            &mut launcher,
            &mut stopper,
        );
        let out = handle_board_with(
            &project,
            &global,
            &resident,
            "remove",
            &mut launcher,
            &mut stopper,
        );
        assert!(
            out.lines.iter().any(|l| l.contains("已移除")),
            "{:?}",
            out.lines
        );
        assert!(!yi_agent_boards::registry::contains(&global, &project).unwrap());
        assert!(
            project
                .join(".yi-agent/supervisors/superpowers-kanban.json")
                .exists()
        );
    }

    #[test]
    fn create_says_when_the_daemon_is_not_up_yet() {
        // 生产里 daemon 可能没能按期就绪：此时仍要如实说「未就绪」，不能报成
        // 一切正常（看板已登记但没人推进队列，用户得知道）。
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let resident = dir.path().join("resident");
        let mut launcher = |_p: &Path| Ok(false);
        let mut stopper = noop_stopper();

        let out = handle_board_with(
            &project,
            &global,
            &resident,
            "create",
            &mut launcher,
            &mut stopper,
        );
        assert!(
            out.lines
                .iter()
                .any(|l| l.contains("看板已创建") && l.contains("未就绪")),
            "{:?}",
            out.lines
        );
        assert!(yi_agent_boards::registry::contains(&global, &project).unwrap());
    }

    #[test]
    fn create_says_when_the_daemon_came_up() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let resident = dir.path().join("resident");
        let mut launcher = launcher_that_leaves_a_daemon();
        let mut stopper = noop_stopper();

        let out = handle_board_with(
            &project,
            &global,
            &resident,
            "create",
            &mut launcher,
            &mut stopper,
        );
        assert!(
            out.lines
                .iter()
                .any(|l| l.contains("看板已创建") && l.contains("就绪")),
            "{:?}",
            out.lines
        );
    }

    #[test]
    fn status_reports_a_registered_board_and_its_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        yi_agent_boards::registry::register(&global, &project).unwrap();

        let out = handle_board(&project, &global, "status");
        assert!(
            out.lines.iter().any(|l| l.contains("已登记")),
            "已登记的项目不能被说成未创建：{:?}",
            out.lines
        );
        assert!(
            out.lines.iter().any(|l| l.contains("未在跑")),
            "{:?}",
            out.lines
        );
    }

    #[test]
    fn an_unknown_argument_is_reported_instead_of_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let out = handle_board(dir.path(), &dir.path().join("global"), "sideways");
        assert!(
            out.lines.iter().any(|l| l.contains("用法")),
            "{:?}",
            out.lines
        );
    }

    #[test]
    fn remove_reports_a_failure_instead_of_claiming_success() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let global = dir.path().join("global");
        let resident = dir.path().join("resident");
        let mut launcher = |_p: &Path| Ok(false);
        let mut stopper = |_p: &Path| Err("daemon 拒绝停止".to_string());
        handle_board_with(
            &project,
            &global,
            &resident,
            "create",
            &mut launcher,
            &mut stopper,
        );

        let out = handle_board_with(
            &project,
            &global,
            &resident,
            "remove",
            &mut launcher,
            &mut stopper,
        );
        assert!(
            out.lines.iter().any(|l| l.contains("移除失败")),
            "{:?}",
            out.lines
        );
        // 失败时登记不能被悄悄抹掉。
        assert!(yi_agent_boards::registry::contains(&global, &project).unwrap());
    }
}
