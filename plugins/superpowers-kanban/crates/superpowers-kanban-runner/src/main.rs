//! Superpowers 看板插件进程入口。
//!
//! 用法：
//! - `superpowers-kanban run --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs 60]`
//! - `superpowers-kanban add <spec> <plan> [--state-dir <d>]`——把一对 spec/plan 投进 `inbox`
//! - `superpowers-kanban list [--state-dir <d>]`——打印队列与待消费的投递
//! - `superpowers-kanban on|off [--state-dir <d>]`——写项目层开关
//! - `superpowers-kanban workdir [--state-dir <d>]`——打印该状态目录对应的项目根（skill 用）
//!
//! `run` 只做一件事：周期性推进队列。安装 = 放这个二进制；卸载 = 删掉它。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use superpowers_kanban_core::calendar::ConcurrencyCalendar;
use superpowers_kanban_core::card_id::card_id_for;
use superpowers_kanban_core::promotion::validate_promotion;
use superpowers_kanban_core::layout::{global_preferences_path, project_preferences_path, project_root};
use superpowers_kanban_core::switch::{BoardSwitch, SwitchValue, read_layer, resolve, write_layer};
use superpowers_kanban_runner::client::BoardDaemon;

#[derive(Debug)]
struct Args {
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    /// 预建 worktree 的落点。缺省为进程当前目录。
    project_root: PathBuf,
    interval: Duration,
}

/// 默认状态目录：`<cwd>/.yi-agent/superpowers-kanban`，与 supervisor 清单里
/// `{state_dir}` 的取值一致；`--state-dir` 可覆盖（测试与多项目场景用）。
fn default_state_dir() -> Result<PathBuf, String> {
    std::env::current_dir()
        .map(|cwd| cwd.join(".yi-agent").join("superpowers-kanban"))
        .map_err(|error| format!("could not read the current directory: {error}"))
}

fn parse_args() -> Result<Subcommand, String> {
    parse_subcommand(std::env::args().skip(1))
}

#[derive(Debug)]
enum Subcommand {
    Run(Args),
    Add {
        state_dir: PathBuf,
        spec: String,
        plan: String,
    },
    List {
        state_dir: PathBuf,
    },
    Workdir {
        state_dir: PathBuf,
    },
    Switch {
        state_dir: PathBuf,
        value: SwitchValue,
    },
}

const USAGE: &str = "usage: superpowers-kanban <run|add|list|on|off|workdir> [...]
  run     --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs N]
  add     <spec> <plan> [--state-dir <d>]
  list    [--state-dir <d>]
  on|off  [--state-dir <d>]
  workdir [--state-dir <d>]";

/// Parsed from an explicit iterator so tests can drive it without a process.
///
/// The verb is mandatory: `run` used to be the implicit default, which left no
/// room for the queue-management verbs next to it.
fn parse_subcommand<I>(tokens: I) -> Result<Subcommand, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = tokens.into_iter();
    match args.next().as_deref() {
        Some("run") => Ok(Subcommand::Run(parse_run_args(args)?)),
        Some("add") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let spec = rest.next().ok_or_else(|| format!("add needs a spec path\n{USAGE}"))?;
            let plan = rest.next().ok_or_else(|| format!("add needs a plan path\n{USAGE}"))?;
            if let Some(extra) = rest.next() {
                return Err(format!("add takes exactly two paths, got an extra: {extra}"));
            }
            Ok(Subcommand::Add {
                state_dir,
                spec,
                plan,
            })
        }
        Some("list") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            ensure_no_leftovers(&rest)?;
            Ok(Subcommand::List { state_dir })
        }
        Some("workdir") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            ensure_no_leftovers(&rest)?;
            Ok(Subcommand::Workdir { state_dir })
        }
        Some(verb @ ("on" | "off")) => {
            let (state_dir, rest) = parse_state_dir(args)?;
            ensure_no_leftovers(&rest)?;
            Ok(Subcommand::Switch {
                state_dir,
                value: if verb == "on" {
                    SwitchValue::Enabled
                } else {
                    SwitchValue::Disabled
                },
            })
        }
        Some(other) => Err(format!("unknown subcommand: {other}\n{USAGE}")),
        None => Err(USAGE.to_string()),
    }
}

/// 抽出可选的 `--state-dir`，把其余 token 原样交回。
fn parse_state_dir<I>(tokens: I) -> Result<(PathBuf, Vec<String>), String>
where
    I: IntoIterator<Item = String>,
{
    let mut state_dir = None;
    let mut rest = Vec::new();
    let mut args = tokens.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--state-dir" {
            let value = args
                .next()
                .ok_or_else(|| "--state-dir needs a value".to_string())?;
            state_dir = Some(PathBuf::from(value));
        } else {
            rest.push(arg);
        }
    }
    match state_dir {
        Some(dir) => Ok((dir, rest)),
        None => Ok((default_state_dir()?, rest)),
    }
}

fn ensure_no_leftovers(rest: &[String]) -> Result<(), String> {
    match rest.first() {
        Some(extra) => Err(format!("unexpected argument: {extra}")),
        None => Ok(()),
    }
}

fn parse_run_args<I>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = String>,
{
    let mut runtime_dir = None;
    let mut state_dir = None;
    let mut project_root = None;
    let mut interval_secs = 60_u64;
    let mut args = args.into_iter();
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
///
/// The project layer sits beside the state directory, not inside it:
/// `<state_dir>/../preferences.json`, i.e. `<workdir>/.yi-agent/preferences.json`.
fn board_switch(state_dir: &std::path::Path) -> BoardSwitch {
    let global = global_preferences_path().as_deref().and_then(read_layer);
    let project = read_layer(&project_preferences_path(state_dir));
    resolve(global, project)
}

/// `add`：先校验成对（存在且互不相同），再投递。校验在这一步做，是为了让调用方
/// 立刻拿到错误，而不是等 runner tick 之后卡片出现在 `inbox/rejected/` 里。
fn command_add(state_dir: &std::path::Path, spec: &str, plan: &str) -> Result<String, String> {
    validate_promotion(std::path::Path::new(spec), std::path::Path::new(plan))
        .map_err(|error| error.to_string())?;
    let id = card_id_for(spec, plan);
    superpowers_kanban_core::inbox::deliver_card(state_dir, &id, spec, plan)
        .map_err(|error| format!("could not deliver the card: {error}"))?;
    Ok(format!("delivered {id} to {}\n{spec}\n{plan}", superpowers_kanban_core::inbox::inbox_dir(state_dir).display()))
}

/// `list`：先落盘的队列，再列出尚未被 runner 消费的投递。
/// 刚 `add` 完立刻 `list` 必须能看见东西，否则调用方会以为入队失败。
fn command_list(state_dir: &std::path::Path) -> Result<String, String> {
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let mut lines = Vec::new();
    let queued = board.queued_in_order();
    if queued.is_empty() && board.is_empty() {
        lines.push("no cards on the board".to_string());
    }
    for (position, id) in queued.iter().enumerate() {
        lines.push(format!("{}. {} queued", position + 1, id.0));
    }
    // 非 queued 的卡片（running / done / failed）也列出来，按 id 排序保证稳定。
    let mut others: Vec<_> = board
        .cards()
        .iter()
        .filter(|card| card.state != superpowers_kanban_core::card::CardState::Queued)
        .collect();
    others.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    for card in others {
        let where_ = card
            .workdir
            .as_ref()
            .map(|path| format!(" @ {}", path.display()))
            .unwrap_or_default();
        lines.push(format!("{} {:?}{}", card.id.0, card.state, where_));
    }
    // 待消费投递。
    let inbox = superpowers_kanban_core::inbox::inbox_dir(state_dir);
    if let Ok(entries) = std::fs::read_dir(&inbox) {
        let mut pending: Vec<_> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .collect();
        pending.sort();
        for path in pending {
            let id = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_default();
            lines.push(format!("{id} pending (waiting for the next tick)"));
        }
    }
    Ok(lines.join("\n"))
}

/// `on`/`off`：只写项目层。全局层留给人显式设置，避免 CLI 悄悄改全局偏好。
fn command_set_switch(state_dir: &std::path::Path, value: SwitchValue) -> Result<String, String> {
    let path = project_preferences_path(state_dir);
    write_layer(&path, value).map_err(|error| format!("could not write {}: {error}", path.display()))?;
    let now = board_switch(state_dir);
    Ok(format!(
        "superpowers_kanban = {}\nwrote {}\nnow {}",
        matches!(value, SwitchValue::Enabled),
        path.display(),
        if now.is_enabled() { "enabled" } else { "disabled" }
    ))
}

fn main() {
    let subcommand = match parse_args() {
        Ok(subcommand) => subcommand,
        Err(message) => {
            eprintln!("superpowers-kanban: {message}");
            std::process::exit(2);
        }
    };
    // 除 run 外的子命令都是「一次性查询/写入」，打印结果后用退出码表达成败。
    let outcome = match subcommand {
        Subcommand::Run(args) => {
            run_daemon(args);
            Ok(String::new())
        }
        Subcommand::Add {
            state_dir,
            spec,
            plan,
        } => command_add(&state_dir, &spec, &plan),
        Subcommand::List { state_dir } => command_list(&state_dir),
        Subcommand::Workdir { state_dir } => Ok(project_root(&state_dir).display().to_string()),
        Subcommand::Switch { state_dir, value } => command_set_switch(&state_dir, value),
    };
    match outcome {
        Ok(message) => {
            if !message.is_empty() {
                println!("{message}");
            }
        }
        Err(message) => {
            eprintln!("superpowers-kanban: {message}");
            std::process::exit(1);
        }
    }
}

/// 把插件的查询入口接到 `dispatch` 上。
struct QueryDispatch {
    state_dir: PathBuf,
}

impl superpowers_kanban_ipc::server::Dispatch for QueryDispatch {
    fn dispatch(&self, method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
        superpowers_kanban_runner::dispatch::dispatch(&self.state_dir, method, params)
    }
}

/// 起查询服务线程。跑在自己的线程里，慢客户端不能拖住推进循环；
/// 进程退出即随之消失，不需要显式 join。
fn start_query_server(state_dir: &std::path::Path) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    let socket = match superpowers_kanban_ipc::server::socket_path(state_dir) {
        Ok(socket) => socket,
        Err(error) => {
            // 深到回退也放不下：静默下去只会让看板 UI 显示「插件不存在」。
            eprintln!("superpowers-kanban: query server disabled: {error}");
            return stop;
        }
    };
    eprintln!("superpowers-kanban: query socket at {}", socket.display());
    let dispatch = Arc::new(QueryDispatch {
        state_dir: state_dir.to_path_buf(),
    });
    let keep_going = Arc::clone(&stop);
    std::thread::spawn(move || {
        if let Err(error) = superpowers_kanban_ipc::server::serve_with(
            &socket,
            dispatch,
            || !keep_going.load(Ordering::SeqCst),
        ) {
            eprintln!(
                "superpowers-kanban: query server stopped: {error} (queries will be refused)"
            );
        }
    });
    stop
}

/// 周期性推进队列，直到进程被杀。
fn run_daemon(args: Args) {
    let calendar = ConcurrencyCalendar::load_preferring_new(&args.state_dir);
    let socket = match superpowers_kanban_ipc::client::socket_path(&args.runtime_dir) {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("superpowers-kanban: cannot locate the daemon socket: {error}");
            return;
        }
    };
    eprintln!("superpowers-kanban: daemon socket at {}", socket.display());
    let daemon = BoardDaemon::new(socket);
    let board_path = args.state_dir.join("board.json");

    // 查询通道与推进循环互不阻塞：宿主问状态时队列照常在动。
    let _query_server = start_query_server(&args.state_dir);

    // 每张正在运行的卡片各持一个全局名额，跨 tick 一直拿着，直到对账发现它跑完。
    // 名额必须活到卡片结束：若只在本轮 tick 内持有，别的项目会在卡片还在跑时抢走
    // 名额，多个看板加起来就超了服务端限流。
    let mut leases: std::collections::HashMap<
        superpowers_kanban_core::card::CardId,
        superpowers_kanban_runner::lease::Lease,
    > = std::collections::HashMap::new();

    // daemon 重启后，board 里仍标着 `Running` 的卡片需要补领名额（本进程刚起，
    // 还没有任何租约）。进程退出时 flock 会自动释放，所以正常重启后这些名额是空的；
    // 补领让「重启」不会既留着 Running 的卡片、又为新卡片发新名额而超发。若此刻
    // 名额已被别的项目占满则领不到，只做尽力而为：第一轮对账会按卡片真实状态收尾。
    {
        let board = superpowers_kanban_runner::persist::load_board(&board_path);
        if let Some(dir) = superpowers_kanban_runner::lease::global_leases_dir() {
            let limit = calendar.limit_at(chrono::Local::now());
            for card in board.cards() {
                if card.state != superpowers_kanban_core::card::CardState::Running
                    || card.task_id.is_none()
                {
                    continue;
                }
                if let Some(lease) =
                    superpowers_kanban_runner::lease::acquire_in(&dir, limit as usize)
                {
                    leases.insert(card.id.clone(), lease);
                } else {
                    eprintln!(
                        "superpowers-kanban: {} is running but no slot could be reclaimed on restart",
                        card.id.0
                    );
                }
            }
        }
    }

    loop {
        if !board_switch(&args.state_dir).is_enabled() {
            // 关掉开关只停止推进，绝不取消已在 daemon 中运行的会话。
            std::thread::sleep(args.interval);
            continue;
        }

        let mut board = superpowers_kanban_runner::persist::load_board(&board_path);
        for outcome in
            superpowers_kanban_runner::inbox::consume(&args.state_dir, &mut board, chrono::Local::now())
        {
            match outcome.result {
                Ok(()) => eprintln!("superpowers-kanban: enqueued {}", outcome.id),
                Err(reason) => eprintln!("superpowers-kanban: rejected {} ({reason})", outcome.id),
            }
        }
        let limit = calendar.limit_at(chrono::Local::now());

        // 对账：跑完的卡片让出名额，下一轮队列才能继续放行。
        for outcome in superpowers_kanban_runner::tick::reconcile_running(&mut board, &daemon) {
            leases.remove(&outcome.card_id);
            eprintln!(
                "superpowers-kanban: {} -> {:?}",
                outcome.card_id.0, outcome.action
            );
        }

        // 启动：按 FIFO 顺序逐张尝试，每张启动前先领一个全局名额。领不到就停在
        // 这里（后续卡片留在 queued），不做「本轮 plan 已定」那套——名额由本
        // 进程跨 tick 持有，领不到就是真没空位，等下一轮即可。
        let project_root = args.project_root.clone();
        let leases_dir = superpowers_kanban_runner::lease::global_leases_dir();
        for card_id in superpowers_kanban_runner::runner::plan_launches(&board, limit) {
            if leases.contains_key(&card_id) {
                continue;
            }
            let Some(dir) = leases_dir.as_deref() else {
                // 取不到全局目录：退化为「不设全局上限」（仍受本项目日历上限约束），
                // 而不是凭空造目录——两个不同的猜测会各自发出一池名额，反而更超发。
                break;
            };
            let Some(lease) = superpowers_kanban_runner::lease::acquire_in(dir, limit as usize)
            else {
                break;
            };
            // 先领名额再建 worktree：建 worktree 是纯本地 git，不消耗模型调用，
            // 但也不能白建一个注定启动不了的目录。
            let branch = format!(
                "kanban/{}",
                superpowers_kanban_runner::worktree::slugify(&card_id)
            );
            let workdir = match superpowers_kanban_runner::worktree::ensure_worktree(
                &project_root,
                &card_id,
                &branch,
            ) {
                Ok(path) => path,
                Err(error) => {
                    eprintln!(
                        "superpowers-kanban: worktree for {} failed: {error}",
                        card_id.0
                    );
                    continue;
                }
            };
            let outcome = superpowers_kanban_runner::tick::launch(
                &mut board, &daemon, &card_id, workdir,
            );
            eprintln!(
                "superpowers-kanban: {} -> {:?}",
                outcome.card_id.0, outcome.action
            );
            match outcome.action {
                superpowers_kanban_runner::tick::TickAction::Launched { .. } => {
                    leases.insert(card_id, lease);
                }
                // 启动失败：立刻把名额还回去，别占着不放。
                _ => drop(lease),
            }
        }

        if let Err(error) = superpowers_kanban_runner::persist::save_board(&board_path, &board) {
            // 落盘失败只记 warning：控制面会看到旧数据，但推进循环不停。
            eprintln!(
                "superpowers-kanban: could not persist {}: {error}",
                board_path.display()
            );
        }

        std::thread::sleep(args.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::{Subcommand, command_add, command_list, command_set_switch, parse_subcommand};
    use superpowers_kanban_core::switch::SwitchValue;

    fn parse(tokens: &[&str]) -> Result<Subcommand, String> {
        parse_subcommand(tokens.iter().map(|token| token.to_string()))
    }

    /// `run` keeps its own verb: it used to be the implicit default, which left
    /// no room for the queue verbs now sitting next to it.
    #[test]
    fn run_still_requires_its_own_verb_and_its_flags() {
        let ok = parse(&["run", "--runtime-dir", "/r", "--state-dir", "/s"]);
        assert!(ok.is_ok(), "{ok:?}");

        assert!(parse(&["--runtime-dir", "/r", "--state-dir", "/s"]).is_err());
        assert!(parse(&["serve", "--runtime-dir", "/r", "--state-dir", "/s"]).is_err());
    }

    #[test]
    fn run_rejects_an_unknown_flag_instead_of_ignoring_it() {
        assert!(parse(&["run", "--runtime-dir", "/r", "--state-dir", "/s", "--oops"]).is_err());
    }

    #[test]
    fn add_requires_exactly_two_paths() {
        assert!(parse(&["add", "a.spec.md"]).is_err(), "one path is not enough");
        assert!(parse(&["add"]).is_err(), "no path is not enough");
        assert!(
            parse(&["add", "a.spec.md", "a.plan.md", "extra"]).is_err(),
            "a third path must be refused rather than silently dropped"
        );
        match parse(&["add", "a.spec.md", "a.plan.md", "--state-dir", "/s"]) {
            Ok(Subcommand::Add { state_dir, spec, plan }) => {
                assert_eq!(state_dir.to_string_lossy(), "/s");
                assert_eq!(spec, "a.spec.md");
                assert_eq!(plan, "a.plan.md");
            }
            other => panic!("expected an add subcommand, got {other:?}"),
        }
    }

    #[test]
    fn list_on_and_off_take_no_positional_arguments() {
        assert!(matches!(parse(&["list"]), Ok(Subcommand::List { .. })));
        assert!(matches!(parse(&["workdir"]), Ok(Subcommand::Workdir { .. })));
        assert!(matches!(
            parse(&["on"]),
            Ok(Subcommand::Switch { value: SwitchValue::Enabled, .. })
        ));
        assert!(matches!(
            parse(&["off"]),
            Ok(Subcommand::Switch { value: SwitchValue::Disabled, .. })
        ));
        assert!(parse(&["on", "extra"]).is_err());
        assert!(parse(&["list", "extra"]).is_err());
    }

    #[test]
    fn add_refuses_a_missing_spec_before_touching_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        let spec = dir.path().join("a.spec.md");

        let error = command_add(dir.path(), &spec.to_string_lossy(), &plan.to_string_lossy())
            .expect_err("a missing spec must be refused here, not by the runner later");
        assert!(error.contains("a.spec.md"), "the error must name the file: {error}");
        assert!(
            !dir.path().join("inbox").exists(),
            "nothing may be delivered when validation fails"
        );
    }

    #[test]
    fn add_delivers_into_the_inbox_and_list_shows_it_as_pending() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("2026-10-01-a-feature.spec.md");
        let plan = dir.path().join("2026-10-01-a-feature.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();

        let message = command_add(dir.path(), &spec.to_string_lossy(), &plan.to_string_lossy())
            .expect("a valid pair must be delivered");
        assert!(message.contains("delivered"), "{message}");

        // 刚投递、尚未 tick：list 必须报告 pending，否则调用方会以为入队失败。
        let listing = command_list(dir.path()).unwrap();
        assert!(
            listing.contains("pending"),
            "an unconsumed delivery must be visible immediately: {listing}"
        );
    }

    #[test]
    fn on_and_off_write_the_project_layer_beside_the_state_directory() {
        // state_dir = <workdir>/.yi-agent/superpowers-kanban
        let workdir = tempfile::tempdir().unwrap();
        let state_dir = workdir.path().join(".yi-agent").join("superpowers-kanban");

        command_set_switch(&state_dir, SwitchValue::Enabled).unwrap();
        let path = workdir.path().join(".yi-agent").join("preferences.json");
        assert!(path.is_file(), "the project layer must land at {path:?}");
        assert_eq!(
            superpowers_kanban_core::switch::read_layer(&path),
            Some(SwitchValue::Enabled)
        );

        command_set_switch(&state_dir, SwitchValue::Disabled).unwrap();
        assert_eq!(
            superpowers_kanban_core::switch::read_layer(&path),
            Some(SwitchValue::Disabled)
        );
    }

    #[test]
    fn the_sample_calendar_expresses_the_three_and_ten_windows() {
        use superpowers_kanban_core::calendar::ConcurrencyCalendar;
        let text = include_str!("../../../superpowers-kanban.toml");
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
