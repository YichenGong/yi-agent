//! Superpowers 看板插件进程入口。
//!
//! 用法：`board-runner --runtime-dir <dir> --state-dir <dir> [--project-root <dir>] [--interval-secs 60]`
//! 它只做一件事：周期性推进队列。安装 = 放这个二进制；卸载 = 删掉它。

use std::path::PathBuf;
use std::time::Duration;

use board_core::calendar::ConcurrencyCalendar;
use board_core::switch::{BoardSwitch, parse_switch_json, resolve};
use board_runner::client::BoardDaemon;

#[derive(Debug)]
struct Args {
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    /// 预建 worktree 的落点。缺省为进程当前目录。
    project_root: PathBuf,
    interval: Duration,
}

fn parse_args() -> Result<Args, String> {
    let mut runtime_dir = None;
    let mut state_dir = None;
    let mut project_root = None;
    let mut interval_secs = 60_u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runtime-dir" => runtime_dir = args.next().map(PathBuf::from),
            "--state-dir" => state_dir = args.next().map(PathBuf::from),
            "--project-root" => project_root = args.next().map(PathBuf::from),
            "--interval-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--interval-secs needs a value".to_string())?;
                interval_secs = value
                    .parse()
                    .map_err(|error| format!("invalid --interval-secs: {error}"))?;
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        runtime_dir: runtime_dir.ok_or_else(|| "--runtime-dir is required".to_string())?,
        state_dir: state_dir.ok_or_else(|| "--state-dir is required".to_string())?,
        project_root: match project_root {
            Some(root) => root,
            None => std::env::current_dir()
                .map_err(|error| format!("could not read the current directory: {error}"))?,
        },
        interval: Duration::from_secs(interval_secs),
    })
}

/// Reads the two preference layers and resolves the switch.
/// Missing or broken files fall back to "disabled" (the conservative default).
fn board_switch(args: &Args) -> BoardSwitch {
    let read = |path: &std::path::Path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_switch_json(&text))
    };
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let global = home
        .as_ref()
        .map(|home| home.join(".yi-agent").join("preferences.json"))
        .and_then(|path| read(&path));
    let project = read(&args.state_dir.join("preferences.json"));
    resolve(global, project)
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("board-runner: {message}");
            std::process::exit(2);
        }
    };
    let calendar = ConcurrencyCalendar::load_preferring_new(&args.state_dir);
    let socket = board_ipc::client::socket_path(&args.runtime_dir);
    let daemon = BoardDaemon::new(socket);
    let board_path = args.state_dir.join("board.json");

    loop {
        if !board_switch(&args).is_enabled() {
            // 关掉开关只停止推进，绝不取消已在 daemon 中运行的会话。
            std::thread::sleep(args.interval);
            continue;
        }

        let mut board = board_runner::persist::load_board(&board_path);
        for outcome in
            board_runner::inbox::consume(&args.state_dir, &mut board, chrono::Local::now())
        {
            match outcome.result {
                Ok(()) => eprintln!("board-runner: enqueued {}", outcome.id),
                Err(reason) => eprintln!("board-runner: rejected {} ({reason})", outcome.id),
            }
        }
        let limit = calendar.limit_at(chrono::Local::now());

        // 启动前为每张待启动卡片预建 worktree（纯本地 git，不消耗模型调用）。
        let project_root = args.project_root.clone();
        let mut launch = |card_id: &board_core::card::CardId| {
            let branch = format!("kanban/{}", board_runner::worktree::slugify(card_id));
            match board_runner::worktree::ensure_worktree(&project_root, card_id, &branch) {
                Ok(path) => Some(path),
                Err(error) => {
                    eprintln!("board-runner: worktree for {} failed: {error}", card_id.0);
                    None
                }
            }
        };

        let outcomes = board_runner::tick::run_once(&mut board, &daemon, limit, &mut launch);
        for outcome in &outcomes {
            // 把预建好的 worktree 记进卡片，控制面据此显示「跑在哪里」。
            if let board_runner::tick::TickAction::Launched { .. } = &outcome.action {
                let branch = format!(
                    "kanban/{}",
                    board_runner::worktree::slugify(&outcome.card_id)
                );
                if let Ok(path) = board_runner::worktree::ensure_worktree(
                    &project_root,
                    &outcome.card_id,
                    &branch,
                ) {
                    let _ = board.set_workdir(&outcome.card_id, path);
                }
            }
            eprintln!(
                "board-runner: {} -> {:?}",
                outcome.card_id.0, outcome.action
            );
        }

        if let Err(error) = board_runner::persist::save_board(&board_path, &board) {
            // 落盘失败只记 warning：控制面会看到旧数据，但推进循环不停。
            eprintln!(
                "board-runner: could not persist {}: {error}",
                board_path.display()
            );
        }

        std::thread::sleep(args.interval);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_sample_calendar_expresses_the_three_and_ten_windows() {
        use board_core::calendar::ConcurrencyCalendar;
        let text = include_str!("../../../kanban.toml");
        let calendar = ConcurrencyCalendar::from_toml(text).unwrap();
        use chrono::{Datelike, Local, TimeZone, Weekday};
        let at = |y, m, d, h| Local.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap();
        // 2026-10-01 是周四；2026-10-03 是周六；2026-10-04 是周日。
        assert_eq!(at(2026, 10, 1, 10).weekday(), Weekday::Thu);
        assert_eq!(calendar.limit_at(at(2026, 10, 1, 10)), 3, "周四上午 = 3");
        assert_eq!(calendar.limit_at(at(2026, 10, 1, 3)), 10, "周四凌晨 = 10");
        assert_eq!(calendar.limit_at(at(2026, 10, 3, 12)), 10, "周六全天 = 10");
        assert_eq!(calendar.limit_at(at(2026, 10, 4, 12)), 10, "周日全天 = 10");
    }
}
