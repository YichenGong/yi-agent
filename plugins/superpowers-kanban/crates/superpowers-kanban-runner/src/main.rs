//! Superpowers 看板插件进程入口。
//!
//! 用法：
//! - `superpowers-kanban run --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs N]`
//! - `superpowers-kanban add <spec> <plan> [--state-dir <d>]`——把一对 spec/plan 投进 `inbox`
//! - `superpowers-kanban add-merge <source> [--base <ref>] [--state-dir <d>]`——投递一张合并卡
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
use superpowers_kanban_core::layout::{
    global_preferences_path, project_preferences_path, project_root,
};
use superpowers_kanban_core::promotion::validate_promotion;
use superpowers_kanban_core::switch::{BoardSwitch, SwitchValue, read_layer, resolve, write_layer};
use superpowers_kanban_runner::client::BoardDaemon;
use superpowers_kanban_runner::service::BoardService;

#[derive(Debug)]
struct Args {
    /// supervisor 清单仍传 `--runtime-dir`，所以 CLI 必须继续接受它；但插件已不再
    /// 直连 daemon 建会话（改由宿主调度器驱动 `board.*`），此字段暂无用途。
    #[allow(dead_code)]
    runtime_dir: PathBuf,
    state_dir: PathBuf,
    /// 预建 worktree 的落点。缺省为进程当前目录。
    project_root: PathBuf,
    /// 显式 `--interval-secs`。`None` 表示未指定，此时每个 tick 从插件设置读取
    /// `interval_secs`；只有显式给了才用这里的值覆盖配置。
    interval: Option<Duration>,
}

/// 当前 tick 的睡眠周期：显式 CLI > 插件配置 > 默认 10 秒。
///
/// 每 tick 重新调用，因此设置界面改了 `interval_secs` 后下一个 tick 即生效，
/// 无需重启插件进程。
fn effective_interval(cli: Option<Duration>, state_dir: &std::path::Path) -> Duration {
    if let Some(cli) = cli {
        return cli;
    }
    let secs = ConcurrencyCalendar::load_preferring_new(state_dir).interval_secs;
    Duration::from_secs(secs)
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
        all: bool,
    },
    AddMerge {
        state_dir: PathBuf,
        source: String,
        base: Option<String>,
    },
    Workdir {
        state_dir: PathBuf,
    },
    Switch {
        state_dir: PathBuf,
        value: SwitchValue,
    },
    Done {
        state_dir: PathBuf,
        card_id: String,
    },
    Archive {
        state_dir: PathBuf,
        card_id: Option<String>,
        all_terminal: bool,
    },
    Purge {
        state_dir: PathBuf,
        card_id: Option<String>,
        all_archived: bool,
    },
}

const USAGE: &str =
    "usage: superpowers-kanban <run|add|add-merge|done|archive|purge|list|on|off|workdir> [...]
  run       --runtime-dir <d> --state-dir <d> [--project-root <d>] [--interval-secs N]
  add       <spec> <plan> [--state-dir <d>]
  add-merge <source> [--base <ref>] [--state-dir <d>]
  done      <card-id> [--state-dir <d>]
  archive   <card-id>|--all-terminal [--state-dir <d>]
  purge     <card-id>|--all-archived [--state-dir <d>]
  list      [--all] [--state-dir <d>]
  on|off    [--state-dir <d>]
  workdir   [--state-dir <d>]";

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
            let spec = rest
                .next()
                .ok_or_else(|| format!("add needs a spec path\n{USAGE}"))?;
            let plan = rest
                .next()
                .ok_or_else(|| format!("add needs a plan path\n{USAGE}"))?;
            if let Some(extra) = rest.next() {
                return Err(format!(
                    "add takes exactly two paths, got an extra: {extra}"
                ));
            }
            Ok(Subcommand::Add {
                state_dir,
                spec,
                plan,
            })
        }
        Some("add-merge") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let source = rest
                .next()
                .ok_or_else(|| format!("add-merge needs a source branch\n{USAGE}"))?;
            let mut base = None;
            while let Some(token) = rest.next() {
                if token == "--base" {
                    base = Some(
                        rest.next()
                            .ok_or_else(|| "--base needs a value".to_string())?,
                    );
                } else {
                    return Err(format!("unexpected argument: {token}"));
                }
            }
            Ok(Subcommand::AddMerge {
                state_dir,
                source,
                base,
            })
        }
        Some("list") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let all = match rest.as_slice() {
                [] => false,
                [flag] if flag == "--all" => true,
                [extra] => return Err(format!("unexpected argument: {extra}")),
                _ => return Err(format!("list takes at most one --all\n{USAGE}")),
            };
            Ok(Subcommand::List { state_dir, all })
        }
        Some("done") => {
            let (state_dir, rest) = parse_state_dir(args)?;
            let mut rest = rest.into_iter();
            let card_id = rest
                .next()
                .ok_or_else(|| format!("done needs a card id\n{USAGE}"))?;
            if let Some(extra) = rest.next() {
                return Err(format!("done takes one card id, got an extra: {extra}"));
            }
            Ok(Subcommand::Done { state_dir, card_id })
        }
        Some("archive") => {
            let (state_dir, card_id, all_terminal) = parse_single_or_all(args, "--all-terminal")?;
            Ok(Subcommand::Archive {
                state_dir,
                card_id,
                all_terminal,
            })
        }
        Some("purge") => {
            let (state_dir, card_id, all_archived) = parse_single_or_all(args, "--all-archived")?;
            Ok(Subcommand::Purge {
                state_dir,
                card_id,
                all_archived,
            })
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

/// `archive`/`purge` 共用：要么一个 card id，要么那个 `--all-*` 开关，互斥。
fn parse_single_or_all<I>(
    tokens: I,
    all_flag: &str,
) -> Result<(PathBuf, Option<String>, bool), String>
where
    I: IntoIterator<Item = String>,
{
    let (state_dir, rest) = parse_state_dir(tokens)?;
    let mut card_id = None;
    let mut all = false;
    for token in rest {
        if token == all_flag {
            all = true;
        } else if token.starts_with("--") {
            return Err(format!("unexpected argument: {token}"));
        } else if card_id.is_some() {
            return Err(format!("unexpected argument: {token}"));
        } else {
            card_id = Some(token);
        }
    }
    if card_id.is_some() && all {
        return Err(format!("take either a card id or {all_flag}, not both"));
    }
    if card_id.is_none() && !all {
        return Err(format!("needs a card id or {all_flag}\n{USAGE}"));
    }
    Ok((state_dir, card_id, all))
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
    let mut interval: Option<Duration> = None;
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
                let secs = value
                    .parse()
                    .map_err(|error| format!("invalid --interval-secs: {error}"))?;
                interval = Some(Duration::from_secs(secs));
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
        interval,
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
    Ok(format!(
        "delivered {id} to {}\n{spec}\n{plan}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}

/// `add-merge`：先校验 refs 与 source 分支存在，再投递一张合并卡。
fn command_add_merge(
    state_dir: &std::path::Path,
    source: &str,
    base: Option<&str>,
    project_root: &std::path::Path,
) -> Result<String, String> {
    let base = match base {
        Some(base) => base.to_string(),
        None => superpowers_kanban_runner::merge::default_branch(project_root),
    };
    superpowers_kanban_core::promotion::validate_merge_refs(source, &base)
        .map_err(|error| error.to_string())?;
    if !superpowers_kanban_runner::merge::source_branch_exists(project_root, source) {
        return Err(format!("source branch does not exist: {source}"));
    }
    // 与已有卡撞名时用 next_free_merge_id 派生唯一 id。
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let id = board.next_free_merge_id(source, &base).0;
    superpowers_kanban_core::inbox::deliver_merge_card(state_dir, &id, source, &base, None)
        .map_err(|error| format!("could not deliver the card: {error}"))?;
    Ok(format!(
        "delivered {id} to {}\n{source} -> {base}",
        superpowers_kanban_core::inbox::inbox_dir(state_dir).display()
    ))
}

/// `list`：先落盘的队列，再列出尚未被 runner 消费的投递。
/// 刚 `add` 完立刻 `list` 必须能看见东西，否则调用方会以为入队失败。
fn command_list(state_dir: &std::path::Path, all: bool) -> Result<String, String> {
    let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
    let mut lines = Vec::new();
    // 默认只列未归档卡；`--all` 连归档卡一起列（行尾标 `[archived]`）。
    let visible: Vec<_> = if all {
        board.cards().iter().collect()
    } else {
        board.visible().collect()
    };
    let queued: Vec<_> = visible
        .iter()
        .filter(|card| card.state == superpowers_kanban_core::card::CardState::Queued)
        .map(|card| card.id.clone())
        .collect();
    if visible.is_empty() {
        lines.push("no cards on the board".to_string());
    }
    for (position, id) in queued.iter().enumerate() {
        lines.push(format!("{}. {} queued", position + 1, id.0));
    }
    // 非 queued 的卡片（running / done / failed）也列出来，按 id 排序保证稳定。
    let mut others: Vec<_> = visible
        .iter()
        .filter(|card| card.state != superpowers_kanban_core::card::CardState::Queued)
        .copied()
        .collect();
    others.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    for card in others {
        let where_ = card
            .workdir
            .as_ref()
            .map(|path| format!(" @ {}", path.display()))
            .unwrap_or_default();
        let archived = if card.archived { " [archived]" } else { "" };
        lines.push(format!(
            "{} {:?}{}{}",
            card.id.0, card.state, where_, archived
        ));
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

/// `done`：把一张 `awaiting_merge` 的卡结算为 `done`。分支核对结果如实打印，
/// 但**不**据此阻断——合并权归人，这里只登记「已合并」这一事实。
fn command_done(
    state_dir: &std::path::Path,
    card_id: &str,
    project_root: &std::path::Path,
) -> Result<String, String> {
    use superpowers_kanban_core::card::{CardId, CardState};
    let path = state_dir.join("board.json");
    let mut board = superpowers_kanban_runner::persist::load_board(&path);
    let id = CardId::new(card_id);
    match board.get(&id) {
        None => return Err(format!("unknown card: {card_id}")),
        Some(card) if card.state != CardState::AwaitingMerge => {
            return Err(format!(
                "card {card_id} is {:?}, not awaiting_merge",
                card.state
            ));
        }
        Some(_) => {}
    }
    // 核对：推导分支并判是否已并入 base（分支可能已被删除——那时只能采信人工确认）。
    let source = format!(
        "kanban/{}",
        superpowers_kanban_runner::worktree::slugify(&id)
    );
    let base = superpowers_kanban_runner::merge::default_branch(project_root);
    let verdict = if !superpowers_kanban_runner::merge::source_branch_exists(project_root, &source)
    {
        "branch-missing"
    } else if superpowers_kanban_runner::merge::branch_merged_into(project_root, &source, &base) {
        "verified"
    } else {
        "not-merged"
    };
    board
        .transition(&id, CardState::Done)
        .map_err(|e| e.to_string())?;
    superpowers_kanban_runner::persist::save_board(&path, &board).map_err(|e| e.to_string())?;
    Ok(format!(
        "card {card_id} -> done ({verdict}); auto-archives in 24h — \
         `superpowers-kanban archive {card_id}` to hide it now"
    ))
}

/// `archive`：按 id 或 `--all-terminal` 归档（隐藏但保留）。
fn command_archive(
    state_dir: &std::path::Path,
    card_id: Option<&str>,
    all_terminal: bool,
) -> Result<String, String> {
    use superpowers_kanban_core::card::CardId;
    let path = state_dir.join("board.json");
    let mut board = superpowers_kanban_runner::persist::load_board(&path);
    let archived = if all_terminal {
        let due: Vec<CardId> = board
            .cards()
            .iter()
            .filter(|card| card.state.is_terminal() && !card.archived)
            .map(|card| card.id.clone())
            .collect();
        let count = due.len();
        for id in due {
            board.archive(&id).map_err(|e| e.to_string())?;
        }
        count
    } else {
        let id = CardId::new(card_id.expect("parse guarantees an id or --all-terminal"));
        board.archive(&id).map_err(|e| e.to_string())?;
        1
    };
    superpowers_kanban_runner::persist::save_board(&path, &board).map_err(|e| e.to_string())?;
    Ok(format!("archived {archived} card(s)"))
}

/// `purge`：按 id 或 `--all-archived` 真删（仅限已归档，不可逆）。
fn command_purge(
    state_dir: &std::path::Path,
    card_id: Option<&str>,
    all_archived: bool,
) -> Result<String, String> {
    use superpowers_kanban_core::card::CardId;
    let path = state_dir.join("board.json");
    let mut board = superpowers_kanban_runner::persist::load_board(&path);
    let removed = if all_archived {
        board.purge_archived()
    } else {
        let id = CardId::new(card_id.expect("parse guarantees an id or --all-archived"));
        board.purge(&id).map_err(|e| e.to_string())?;
        1
    };
    superpowers_kanban_runner::persist::save_board(&path, &board).map_err(|e| e.to_string())?;
    Ok(format!("purged {removed} card(s)"))
}

/// `on`/`off`：只写项目层。全局层留给人显式设置，避免 CLI 悄悄改全局偏好。
fn command_set_switch(state_dir: &std::path::Path, value: SwitchValue) -> Result<String, String> {
    let path = project_preferences_path(state_dir);
    write_layer(&path, value)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;
    let now = board_switch(state_dir);
    Ok(format!(
        "superpowers_kanban = {}\nwrote {}\nnow {}",
        matches!(value, SwitchValue::Enabled),
        path.display(),
        if now.is_enabled() {
            "enabled"
        } else {
            "disabled"
        }
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
        Subcommand::AddMerge {
            state_dir,
            source,
            base,
        } => {
            let project_root = superpowers_kanban_core::layout::project_root(&state_dir);
            command_add_merge(&state_dir, &source, base.as_deref(), &project_root)
        }
        Subcommand::List { state_dir, all } => command_list(&state_dir, all),
        Subcommand::Workdir { state_dir } => Ok(project_root(&state_dir).display().to_string()),
        Subcommand::Switch { state_dir, value } => command_set_switch(&state_dir, value),
        Subcommand::Done { state_dir, card_id } => {
            let project_root = superpowers_kanban_core::layout::project_root(&state_dir);
            command_done(&state_dir, &card_id, &project_root)
        }
        Subcommand::Archive {
            state_dir,
            card_id,
            all_terminal,
        } => command_archive(&state_dir, card_id.as_deref(), all_terminal),
        Subcommand::Purge {
            state_dir,
            card_id,
            all_archived,
        } => command_purge(&state_dir, card_id.as_deref(), all_archived),
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
///
/// `service` 与推进循环是**同一个** `Arc<BoardService>`：查询里 `board.next_launch`
/// 占的槽位、`board.mark_running` 记的 thread，推进循环立刻看得见；反过来推进
/// 循环 `inbox::consume` 的入队也不会让查询读到过期快照。
struct QueryDispatch {
    service: Arc<BoardService>,
    state_dir: PathBuf,
}

impl superpowers_kanban_ipc::server::Dispatch for QueryDispatch {
    fn dispatch(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let global = global_preferences_path().as_deref().and_then(read_layer);
        superpowers_kanban_runner::dispatch::dispatch_with_service(
            self.service.as_ref(),
            &self.state_dir,
            global,
            method,
            params,
        )
    }
}

/// 起查询服务线程。跑在自己的线程里，慢客户端不能拖住推进循环；
/// 进程退出即随之消失，不需要显式 join。
fn start_query_server(service: Arc<BoardService>, state_dir: &std::path::Path) -> Arc<AtomicBool> {
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
        service,
        state_dir: state_dir.to_path_buf(),
    });
    let keep_going = Arc::clone(&stop);
    std::thread::spawn(move || {
        if let Err(error) = superpowers_kanban_ipc::server::serve_with(&socket, dispatch, || {
            !keep_going.load(Ordering::SeqCst)
        }) {
            eprintln!(
                "superpowers-kanban: query server stopped: {error} (queries will be refused)"
            );
        }
    });
    stop
}

/// 周期性推进队列，直到进程被杀。
///
/// 这里**不再创建会话**：认领（`board.next_launch`）、起会话、回写终态都由宿主
/// 调度器负责（plan Task 5/6）。插件只负责消费 inbox 把卡排进队列，并落盘。
fn run_daemon(args: Args) {
    // 单实例：同一项目同一时刻只有一个插件推进队列。拿不到就等旧实例让位——
    // 旧实例要么正常退出，要么靠 daemon 失联探测自行退出。用轮询避免忙等。
    // 锁必须在函数存活期间一直持有：`_lock` 绑在这里，函数结束才 drop。
    let _lock = loop {
        if let Some(lock) = superpowers_kanban_runner::single_instance::acquire(&args.state_dir) {
            break lock;
        }
        eprintln!(
            "superpowers-kanban: another instance holds the lock at {}; waiting",
            args.state_dir.join("plugin.lock").display()
        );
        std::thread::sleep(Duration::from_millis(500));
    };

    // daemon 存活探测的地址。取不到（runtime 目录不可用）就没有可探的目标，此后
    // 的探测一律当作「已失联」——孤儿必须能有界退出。
    let daemon = superpowers_kanban_ipc::client::socket_path(&args.runtime_dir)
        .ok()
        .map(BoardDaemon::new)
        .map(Arc::new);

    let calendar = ConcurrencyCalendar::load_preferring_new(&args.state_dir);
    // 推进循环与查询服务共享同一实例，槽位租约与看板状态因此只有一份真相。
    let service = Arc::new(BoardService::new(
        args.state_dir.clone(),
        args.project_root.clone(),
        None,
    ));

    // 启动迁移：回收重启后已死的卡片（`Running` 无 thread id → `NeedsYou`；
    // 崩溃留下的 `Launching` 僵尸 → `Failed`），再为仍在 `Running` 的卡片补领
    // 全局名额——重启后内存 lease 为空，不补领会让全局池少算这些在跑卡片。
    service.migrate_legacy_running();
    service.adopt_running_leases(calendar.limit_at(chrono::Local::now()));

    // 查询通道与推进循环互不阻塞：宿主问状态时队列照常在动。
    let _query_server = start_query_server(Arc::clone(&service), &args.state_dir);

    let mut liveness = superpowers_kanban_runner::single_instance::Liveness::default();
    loop {
        // 先探 daemon，且必须在开关判断之前：开关关闭的孤儿走 `continue` 分支，
        // 若把探测放在其后，它永远发现不了 daemon 已死，会一直占着单实例锁。
        // 阈值 3 × interval ≈ 让位窗口。
        let alive = daemon.as_ref().is_some_and(|daemon| daemon.is_alive());
        if liveness.observe(alive, 3) {
            eprintln!(
                "superpowers-kanban: daemon is gone; exiting so a fresh instance can take over"
            );
            std::process::exit(0);
        }
        if !board_switch(&args.state_dir).is_enabled() {
            // 关掉开关只停止推进，绝不取消已在跑的会话。
            std::thread::sleep(effective_interval(args.interval, &args.state_dir));
            continue;
        }

        // 插件在这里只做一件事：把 inbox 里的投递排进共享看板并落盘。认领与
        // 会话推进全部交给宿主调度器经 `board.*` 驱动。
        for outcome in service.consume_inbox(chrono::Local::now()) {
            match outcome.result {
                Ok(()) => eprintln!("superpowers-kanban: enqueued {}", outcome.id),
                Err(reason) => eprintln!("superpowers-kanban: rejected {} ({reason})", outcome.id),
            }
        }

        // 合并与实现共用一个 tick：消费投递后，若本项目没有合并在跑，就做一张。
        match service.merge_next() {
            Ok(Some(claim)) => eprintln!(
                "superpowers-kanban: merged {} ({:?})",
                claim.card_id, claim.outcome
            ),
            Ok(None) => {}
            Err(error) => eprintln!("superpowers-kanban: merge failed: {error}"),
        }

        // 归档：真终止态过宽限期自动隐藏；awaiting_merge/needs_you 永不自动。
        let swept = service.archive_due(chrono::Local::now());
        if !swept.is_empty() {
            eprintln!(
                "superpowers-kanban: archived {} finished card(s)",
                swept.len()
            );
        }

        std::thread::sleep(effective_interval(args.interval, &args.state_dir));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Duration, Subcommand, command_add, command_add_merge, command_archive, command_done,
        command_list, command_purge, command_set_switch, effective_interval, parse_subcommand,
    };
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

    /// `--runtime-dir` 仍被接受（supervisor 清单照传），且解析后无误——插件不再用它
    /// 建会话，但丢掉这个 flag 会让清单启动直接失败。
    #[test]
    fn run_accepts_the_runtime_dir_the_manifest_still_passes() {
        match parse(&["run", "--runtime-dir", "/r", "--state-dir", "/s"]) {
            Ok(Subcommand::Run(args)) => {
                assert_eq!(args.runtime_dir.to_string_lossy(), "/r");
                assert_eq!(args.state_dir.to_string_lossy(), "/s");
            }
            other => panic!("expected a run subcommand, got {other:?}"),
        }
    }

    #[test]
    fn add_requires_exactly_two_paths() {
        assert!(
            parse(&["add", "a.spec.md"]).is_err(),
            "one path is not enough"
        );
        assert!(parse(&["add"]).is_err(), "no path is not enough");
        assert!(
            parse(&["add", "a.spec.md", "a.plan.md", "extra"]).is_err(),
            "a third path must be refused rather than silently dropped"
        );
        match parse(&["add", "a.spec.md", "a.plan.md", "--state-dir", "/s"]) {
            Ok(Subcommand::Add {
                state_dir,
                spec,
                plan,
            }) => {
                assert_eq!(state_dir.to_string_lossy(), "/s");
                assert_eq!(spec, "a.spec.md");
                assert_eq!(plan, "a.plan.md");
            }
            other => panic!("expected an add subcommand, got {other:?}"),
        }
    }

    #[test]
    fn list_on_and_off_take_no_positional_arguments() {
        assert!(matches!(
            parse(&["list"]),
            Ok(Subcommand::List { all: false, .. })
        ));
        assert!(matches!(
            parse(&["list", "--all"]),
            Ok(Subcommand::List { all: true, .. })
        ));

        // done/archive/purge：id 与 --all-* 互斥，缺参报错。
        assert!(matches!(
            parse(&["done", "card-1", "--state-dir", "/s"]),
            Ok(Subcommand::Done { .. })
        ));
        assert!(matches!(
            parse(&["archive", "card-1"]),
            Ok(Subcommand::Archive {
                all_terminal: false,
                ..
            })
        ));
        assert!(matches!(
            parse(&["archive", "--all-terminal"]),
            Ok(Subcommand::Archive {
                all_terminal: true,
                ..
            })
        ));
        assert!(matches!(
            parse(&["purge", "card-1"]),
            Ok(Subcommand::Purge { .. })
        ));
        assert!(matches!(
            parse(&["purge", "--all-archived"]),
            Ok(Subcommand::Purge {
                all_archived: true,
                ..
            })
        ));
        assert!(parse(&["done"]).is_err(), "done 缺 id");
        assert!(parse(&["archive"]).is_err(), "archive 缺 id/开关");
        assert!(parse(&["archive", "a", "b"]).is_err(), "archive 多参数");
        assert!(
            parse(&["purge", "--all-archived", "card-1"]).is_err(),
            "id 与 --all-archived 互斥"
        );
        assert!(matches!(
            parse(&["workdir"]),
            Ok(Subcommand::Workdir { .. })
        ));
        assert!(matches!(
            parse(&["on"]),
            Ok(Subcommand::Switch {
                value: SwitchValue::Enabled,
                ..
            })
        ));
        assert!(matches!(
            parse(&["off"]),
            Ok(Subcommand::Switch {
                value: SwitchValue::Disabled,
                ..
            })
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
        assert!(
            error.contains("a.spec.md"),
            "the error must name the file: {error}"
        );
        assert!(
            !dir.path().join("inbox").exists(),
            "nothing may be delivered when validation fails"
        );
    }

    /// 真 git 仓库 + 一张处于 `AwaitingMerge` 的实现卡；用于 `done` 的三种核对。
    fn repo_with_awaiting_card(
        card_id: &str,
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        use superpowers_kanban_core::board::Board;
        use superpowers_kanban_core::card::{CardId, CardState};
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(project.join("f.txt"), "hi").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);
        run(&["branch", "-M", "main"]);

        // 状态目录放在仓库之外：测试里的 `git add -A` 不该把 board.json 提交进去。
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = Board::new();
        let id = CardId::new(card_id);
        board.enqueue(
            id.clone(),
            "c.spec.md".into(),
            "c.plan.md".into(),
            chrono::Local::now(),
        );
        board.transition(&id, CardState::Running).unwrap();
        board.transition(&id, CardState::AwaitingMerge).unwrap();
        superpowers_kanban_runner::persist::save_board(&state_dir.join("board.json"), &board)
            .unwrap();
        (dir, project, state_dir)
    }

    fn card_state(state_dir: &std::path::Path, card_id: &str) -> String {
        let board = superpowers_kanban_runner::persist::load_board(&state_dir.join("board.json"));
        let id = superpowers_kanban_core::card::CardId::new(card_id);
        let card = board.get(&id).unwrap();
        format!("{:?}", card.state).to_lowercase()
    }

    #[test]
    fn done_settles_an_awaiting_card_and_reports_the_branch_verdict() {
        let (dir, project, state_dir) = repo_with_awaiting_card("card-1");
        // 没有推导出的 kanban/<slug> 分支 → branch-missing，但仍放行为 done。
        let out = command_done(&state_dir, "card-1", &project).unwrap();
        assert!(out.contains("branch-missing"), "{out}");
        assert_eq!(card_state(&state_dir, "card-1"), "done");
        drop(dir);
    }

    #[test]
    fn done_verifies_a_branch_that_is_already_merged() {
        let (dir, project, state_dir) = repo_with_awaiting_card("card-1");
        // 分支名按 slug 推导；把它建出来并合进 main → verified。
        let slug = superpowers_kanban_runner::worktree::slugify(
            &superpowers_kanban_core::card::CardId::new("card-1"),
        );
        let branch = format!("kanban/{slug}");
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        run(&["branch", &branch]);
        let out = command_done(&state_dir, "card-1", &project).unwrap();
        assert!(out.contains("verified"), "{out}");
        drop(dir);
    }

    #[test]
    fn done_flags_a_branch_that_exists_but_is_not_merged() {
        let (dir, project, state_dir) = repo_with_awaiting_card("card-1");
        let slug = superpowers_kanban_runner::worktree::slugify(
            &superpowers_kanban_core::card::CardId::new("card-1"),
        );
        let branch = format!("kanban/{slug}");
        let run = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        // 分支上多一个未进 main 的提交 → not-merged（仍放行）。
        run(&["checkout", "-q", "-b", &branch]);
        std::fs::write(project.join("g.txt"), "work").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "work"]);
        run(&["checkout", "-q", "main"]);
        let out = command_done(&state_dir, "card-1", &project).unwrap();
        assert!(out.contains("not-merged"), "{out}");
        drop(dir);
    }

    #[test]
    fn done_refuses_a_card_that_is_not_awaiting_merge() {
        let (dir, project, state_dir) = repo_with_awaiting_card("card-1");
        command_done(&state_dir, "card-1", &project).unwrap();
        // 已 done → 再 done 报错，且状态不变。
        let err = command_done(&state_dir, "card-1", &project).unwrap_err();
        assert!(err.contains("not awaiting_merge"), "{err}");
        assert_eq!(card_state(&state_dir, "card-1"), "done");
        drop(dir);
    }

    #[test]
    fn archive_hides_a_terminal_card_and_purge_requires_archiving_first() {
        let (dir, project, state_dir) = repo_with_awaiting_card("card-1");
        command_done(&state_dir, "card-1", &project).unwrap();
        // 未归档不得 purge。
        assert!(command_purge(&state_dir, Some("card-1"), false).is_err());
        command_archive(&state_dir, Some("card-1"), false).unwrap();
        let listing = command_list(&state_dir, false).unwrap();
        assert!(
            !listing.contains("card-1"),
            "归档卡不出现在 list: {listing}"
        );
        assert!(
            command_list(&state_dir, true)
                .unwrap()
                .contains("[archived]"),
            "--all 显示归档标记"
        );
        command_purge(&state_dir, Some("card-1"), false).unwrap();
        assert!(!command_list(&state_dir, true).unwrap().contains("card-1"));
        drop(dir);
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
        let listing = command_list(dir.path(), false).unwrap();
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
    fn add_merge_requires_a_source() {
        assert!(parse(&["add-merge"]).is_err());
    }

    #[test]
    fn add_merge_parses_the_base_flag() {
        match parse(&[
            "add-merge",
            "kanban/a",
            "--base",
            "develop",
            "--state-dir",
            "/s",
        ]) {
            Ok(Subcommand::AddMerge {
                state_dir,
                source,
                base,
            }) => {
                assert_eq!(source, "kanban/a");
                assert_eq!(base.as_deref(), Some("develop"));
                assert_eq!(state_dir.to_string_lossy(), "/s");
            }
            other => panic!("expected add-merge, got {other:?}"),
        }
    }

    #[test]
    fn add_merge_refuses_an_unknown_source_branch() {
        let dir = tempfile::tempdir().unwrap();
        // 初始化一个真实仓库，且没有 feat/x 分支。
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["init", "-q", "-b", "main"])
                .status()
                .unwrap()
                .success()
        );
        let error = command_add_merge(dir.path(), "feat/x", Some("main"), dir.path())
            .expect_err("a missing source branch must be refused");
        assert!(error.contains("feat/x"), "{error}");
        assert!(
            !dir.path().join("inbox").exists(),
            "nothing delivered on failure"
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

    #[test]
    fn the_configured_interval_wins_over_the_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("superpowers-kanban.toml"),
            "interval_secs = 42\n",
        )
        .unwrap();
        assert_eq!(
            effective_interval(None, dir.path()),
            Duration::from_secs(42),
            "the settings file drives the tick interval"
        );
    }

    #[test]
    fn an_explicit_cli_interval_wins_over_the_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("superpowers-kanban.toml"),
            "interval_secs = 42\n",
        )
        .unwrap();
        assert_eq!(
            effective_interval(Some(Duration::from_secs(5)), dir.path()),
            Duration::from_secs(5),
            "an explicit CLI flag is an override"
        );
    }

    #[test]
    fn a_missing_config_falls_back_to_ten() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            effective_interval(None, dir.path()),
            Duration::from_secs(10)
        );
    }
}
