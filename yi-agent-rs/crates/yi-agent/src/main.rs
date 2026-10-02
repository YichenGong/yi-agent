//! yi-agent CLI 入口。

mod config;
mod control_commands;
mod llm_prefix;
mod schedule_intent;
mod tracing_init;
mod tui;

#[cfg(test)]
mod control_commands_tests;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use yi_agent_core::Provider;
// Headless 模式的工具 + system prompt 构建结果。类型来自共享 crate
// (`ToolSetup` 的字段都是 `pub`),`build_headless_root_tools` 直接用
// `HeadlessSetup { tools, catalog, system_prompt, mcp, process_manager }`
// 字面量构造仍然成立。
use yi_agent_runtime::bootstrap::ToolSetup as HeadlessSetup;

use crate::config::{AgentAction, Cli, Command, DaemonAction, PairAction, ScheduleAction};

fn format_ipc_error(code: yi_agent_store::ipc::IpcErrorCode, message: Option<String>) -> String {
    match message {
        Some(message) => format!("{code}: {message}"),
        None => code.to_string(),
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Completion scripts are pure stdout. Skip tracing for them so no log or
    // warning line can ever pollute the script a user redirects into their
    // shell config.
    let _trace_guard = if matches!(cli.command, Some(Command::Completions { .. })) {
        None
    } else {
        Some(tracing_init::init(cli.debug))
    };

    match cli.command {
        Some(Command::Web { ref host, ref port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env_path = config::resolve_env_path(&cli);
                let global_env_path = if config::is_workdir_explicit(&cli) {
                    None
                } else {
                    config::resolve_global_env_path()
                };
                yi_agent_web::serve(host, *port, env_path, global_env_path).await
            })
        }
        Some(Command::Run {
            ref prompt,
            json,
            stdin,
            naked,
            subagents,
        }) => {
            let prompt = prompt.clone();
            run_headless(cli, prompt, json, stdin, naked, subagents)
        }
        Some(Command::Daemon { action }) => control_daemon(&cli, action),
        Some(Command::Agents { ref project, all }) => control_agents(&cli, project.clone(), all),
        Some(Command::Agent { ref action }) => control_agent(&cli, action.clone()),
        Some(Command::Schedule { ref action }) => control_schedule(&cli, action),
        Some(Command::AppServer {
            ref listen,
            ref relay,
        }) => {
            let listen = listen.clone();
            let relay = relay.clone();
            run_app_server(cli, &listen, relay.as_deref())
        }
        Some(Command::Completions { shell }) => print_completion(shell),
        Some(Command::Pair { ref action }) => run_pair(action),
        None => run_agent(cli),
    }
}

/// 解析 `--listen`。支持 stdio、ws,以及 `relay://` 长写法的中继出站模式。
#[derive(Debug)]
enum Listen {
    Stdio,
    Ws(std::net::SocketAddr),
    /// 中继出站模式:值是中继的电脑侧端点(裸 `wss://…/connect?session=…`)。
    Relay(String),
}

fn parse_listen(listen: &str) -> Result<Listen> {
    if listen == "stdio://" {
        return Ok(Listen::Stdio);
    }
    if let Some(rest) = listen.strip_prefix("ws://") {
        let addr: std::net::SocketAddr = rest
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid ws address `{rest}`: {e}"))?;
        return Ok(Listen::Ws(addr));
    }
    if let Some(rest) = listen.strip_prefix("relay://") {
        if rest.is_empty() {
            anyhow::bail!("`relay://` needs a ws/wss endpoint after it");
        }
        // 中继端点必须是 ws/wss;提前拒绝 http 之类,避免把错误推迟到连接时。
        if !rest.starts_with("ws://") && !rest.starts_with("wss://") {
            anyhow::bail!("relay endpoint must start with ws:// or wss://, got `{rest}`");
        }
        return Ok(Listen::Relay(rest.to_string()));
    }
    anyhow::bail!(
        "unsupported app-server transport `{listen}`: expected `stdio://`, \
         `ws://host:port`, or `relay://wss://host/connect?session=<id>`"
    )
}

/// 把中继端点拆成「去掉 `session` 的 URL」+「session id」。
///
/// `run_client` 会自己补 `?session=<id>`,故这里必须把它摘掉,否则 URL 上会出现
/// 两个 `session` 参数。不依赖 `url` crate(与 app-server 的依赖面保持一致),
/// 只做本场景够用的查询串切分。
fn relay_parts(relay_url: &str) -> Result<(String, String)> {
    let (base, query) = match relay_url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (relay_url, None),
    };
    let session = query
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "session")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "relay endpoint `{relay_url}` is missing `?session=<id>`: \
                 the phone and the computer must agree on the same session id"
            )
        })?;

    let kept: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("session="))
        .collect();
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    Ok((url, session))
}

/// Emit a clap-generated shell completion script for `shell` on stdout.
///
/// The registered name is the binary name the script completes, not the clap
/// `name` attribute, so `yi-agent <TAB>` is what a shell expands.
fn print_completion(shell: clap_complete::Shell) -> Result<()> {
    let mut command = <Cli as clap::CommandFactory>::command();
    clap_complete::generate(shell, &mut command, "yi-agent", &mut std::io::stdout());
    Ok(())
}

/// Pairing helpers, sharing the same `pairing.json` + `devices.json` as the
/// app-server. A code minted here is redeemable by a `--relay`/`ws://`
/// app-server on the same machine (both read the same files).
fn run_pair(action: &PairAction) -> Result<()> {
    use yi_agent_app_server::device_store::{DeviceStore, default_path as devices_path};
    use yi_agent_app_server::pairing::PairingState;

    let pairing = PairingState::new(DeviceStore::new(devices_path()));
    match action {
        PairAction::Code => {
            let code = pairing.create_code();
            println!("{}", format_pair_code(&code.code, code.expires_in));
        }
        PairAction::List => {
            let devices = pairing.store().list();
            print!("{}", format_device_list(&devices));
        }
        PairAction::Revoke { device_id } => {
            let removed = pairing
                .revoke(device_id)
                .map_err(|e| anyhow::anyhow!("failed to revoke device: {e:?}"))?;
            println!("{}", format_revoke_result(device_id, removed));
        }
    }
    Ok(())
}

/// `ABCD-EFGH` + `(valid 300s)` — pure so it can be unit-tested.
fn format_pair_code(code: &str, expires_in: u64) -> String {
    format!("{code} (valid {expires_in}s)")
}

/// One device per line: `id  name  scope  created`. Pure for testing.
fn format_device_list(devices: &[yi_agent_app_server::device_store::Device]) -> String {
    if devices.is_empty() {
        return "no paired devices\n".to_string();
    }
    let mut out = String::new();
    for d in devices {
        out.push_str(&format!(
            "{}  {}  {:?}  {}\n",
            d.id, d.name, d.scope, d.created_at
        ));
    }
    out
}

fn format_revoke_result(device_id: &str, removed: bool) -> String {
    if removed {
        format!("revoked {device_id}")
    } else {
        format!("no such device: {device_id}")
    }
}

/// Run the JSON-RPC 2.0 app-server over stdio for the desktop GUI sidecar, or
/// over `ws://` for network clients.
///
/// For stdio, stdout is the protocol channel and must stay free of log lines;
/// tracing writes to a file (and to stderr only when `YI_LOG` is set).
fn run_app_server(cli: Cli, listen: &str, relay: Option<&str>) -> Result<()> {
    // `--relay <url>` 是 `--listen relay://<url>` 的等价写法;两者都给时以 `--relay` 为准。
    let requested = match relay {
        Some(url) => format!("relay://{url}"),
        None => listen.to_string(),
    };
    let listen = parse_listen(&requested)?;
    let config = config::load(&cli)?;
    let rt = tokio::runtime::Runtime::new()?;
    match listen {
        Listen::Stdio => rt.block_on(yi_agent_app_server::run(
            tokio::io::stdin(),
            tokio::io::stdout(),
            config,
        )),
        Listen::Ws(addr) => rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind(addr).await?;
            let bound = listener.local_addr()?;
            if !bound.ip().is_loopback() {
                eprintln!(
                    "warning: ws app-server is bound to {bound}; every client must now \
                     authenticate with a paired device token (rejected with ws close 4401)"
                );
            }
            let workspaces =
                std::sync::Arc::new(yi_agent_app_server::workspace_index::WorkspaceIndex::new(
                    yi_agent_app_server::workspace_index::default_path(),
                ));
            // 与桌面 stdio 同一张设备表:手机在桌面 `pair/create` 铸出的码兑现
            // 出的 token,必须能被这台 ws server 认证。用 `default_path()` 让二者
            // 进程内/跨进程都读同一份 `~/.yi-agent/devices.json`。
            let pairing = std::sync::Arc::new(yi_agent_app_server::pairing::PairingState::new(
                yi_agent_app_server::device_store::DeviceStore::new(
                    yi_agent_app_server::device_store::default_path(),
                ),
            ));
            yi_agent_app_server::ws::serve_ws(listener, config, workspaces, pairing).await
        }),
        Listen::Relay(url) => rt.block_on(run_relay_mode(config, url)),
    }
}

/// 中继出站模式:本地起一个只监听环回的 ws app-server,再把它与中继桥起来。
///
/// 电脑侧**不开放入站端口**:本地 listener 绑在 `127.0.0.1:0`(仅本机可达),
/// 中继客户端作为 ws 客户端出站连它,因此网络路径上没有新增暴露面。
///
/// 本地 app-server 仍是「无 token 即 4401」:这里用与它**共享**的 `PairingState`
/// 现铸一枚本机设备 token(等价于本机走一次正常配对,`seed_local_device`),交给
/// 中继客户端连接本地 ws。**不**把本地 ws 改成免认证——那会削弱「准入即认证」
/// 的不变量,而这枚 token 的成本只是一个函数调用。
async fn run_relay_mode(
    config: yi_agent_runtime::config::RuntimeConfig,
    url: String,
) -> Result<()> {
    let (relay_url, session) = relay_parts(&url)?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let workspaces =
        std::sync::Arc::new(yi_agent_app_server::workspace_index::WorkspaceIndex::new(
            yi_agent_app_server::workspace_index::default_path(),
        ));
    let pairing = std::sync::Arc::new(yi_agent_app_server::pairing::PairingState::new(
        yi_agent_app_server::device_store::DeviceStore::new(
            yi_agent_app_server::device_store::default_path(),
        ),
    ));
    // 本机中继桥的凭据:直接铸一台 Control 设备(与 spec §5.4「新配对设备默认
    // control」一致),i.e. 桌面为「中继桥」这个本机客户端发的一张设备卡。
    let local_token = pairing.seed_local_device("relay-bridge");

    let serve_pairing = std::sync::Arc::clone(&pairing);
    tokio::spawn(async move {
        if let Err(e) =
            yi_agent_app_server::ws::serve_ws(listener, config, workspaces, serve_pairing).await
        {
            eprintln!("local app-server for relay stopped: {e}");
        }
    });

    let relay_url = url::Url::parse(&relay_url)
        .map_err(|e| anyhow::anyhow!("invalid relay url `{relay_url}`: {e}"))?;
    let app_server_ws = url::Url::parse(&format!("ws://{addr}/ws")).expect("loopback ws url");
    eprintln!("relaying via {relay_url} (session {session}); local app-server on {addr}");

    yi_agent_relay::run_client(relay_url, app_server_ws, session, local_token).await
}

fn control_agents(cli: &Cli, project: Option<std::path::PathBuf>, all: bool) -> Result<()> {
    if project.is_some() {
        anyhow::bail!("project filtering is not available from this daemon version")
    }
    let workdir = config::resolve_workdir(cli)?;
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&workdir))?;
    let mut subscription = yi_agent_store::ipc::subscribe(&socket, 0).map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })?;
    let yi_agent_store::ipc::IpcResponse::Subscription(snapshot) = subscription.next_response()?
    else {
        anyhow::bail!("runtime daemon returned an invalid task snapshot")
    };
    for task in snapshot
        .tasks
        .into_iter()
        .filter(|task| all || !matches!(task.state.as_str(), "completed" | "cancelled" | "failed"))
    {
        println!("{} {}", task.task_id, task.state);
    }
    Ok(())
}

fn control_agent(cli: &Cli, action: AgentAction) -> Result<()> {
    let workdir = config::resolve_workdir(cli)?;
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&workdir))?;
    let request = match action {
        AgentAction::Show { task_id } => yi_agent_store::ipc::IpcRequest::InspectTask { task_id },
        AgentAction::Events { task_id, follow } => {
            if follow {
                return follow_task_events(&socket, task_id);
            }
            yi_agent_store::ipc::IpcRequest::ReadTaskEvents {
                task_id,
                after_event_id: None,
            }
        }
        AgentAction::Mailbox { task_id } => {
            yi_agent_store::ipc::IpcRequest::ReadTaskMailbox { task_id }
        }
        AgentAction::Diff { task_id } => yi_agent_store::ipc::IpcRequest::ReadTaskDiff { task_id },
        AgentAction::Message { task_id, text, .. } => {
            yi_agent_store::ipc::IpcRequest::SendUserMessage {
                task_id,
                message: text,
            }
        }
        AgentAction::Cancel {
            task_id,
            recursive,
            yes,
            confirmation,
        } => {
            if !yes {
                return show_cancel_preview(&socket, task_id, recursive);
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "cancel confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmCancel {
                task_id,
                recursive,
                confirmation_token,
            }
        }
        AgentAction::Pause { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::PauseTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Resume { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::ResumeTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Retry { task_id } => {
            let session_id = inspect_session(&socket, &task_id)?;
            yi_agent_store::ipc::IpcRequest::RetryTask {
                session_id,
                task_id,
            }
        }
        AgentAction::Accept {
            task_id,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Accept {};
            if !yes {
                return show_review_preview(&socket, task_id, decision, "accept");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        AgentAction::Rework {
            task_id,
            feedback,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Rework { feedback };
            if !yes {
                return show_review_preview(&socket, task_id, decision, "rework");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        AgentAction::Reject {
            task_id,
            reason,
            yes,
            confirmation,
        } => {
            let decision = yi_agent_store::ipc::IpcReviewDecision::Reject { reason };
            if !yes {
                return show_review_preview(&socket, task_id, decision, "reject");
            }
            let confirmation_token = confirmation.ok_or_else(|| {
                anyhow::anyhow!(
                    "review confirmation token is required; rerun without --yes to create a preview"
                )
            })?;
            yi_agent_store::ipc::IpcRequest::ConfirmReview {
                task_id,
                decision,
                confirmation_token,
            }
        }
        other => {
            anyhow::bail!("agent control `{other:?}` is not yet supported by this daemon version")
        }
    };
    match yi_agent_store::ipc::send_request(&socket, request).map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => {
            println!("{} {}", detail.task_id, detail.state);
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskEvents { events } => {
            for event in events {
                println!("{}", task_event_line(&event));
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskMailbox { messages } => {
            for message in messages {
                println!(
                    "{} {} {} {}",
                    message.message_id, message.kind, message.priority, message.payload_json
                );
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskDiff {
            task_id,
            delivery_json,
            diff,
        } => {
            println!("{task_id} {delivery_json}");
            if let Some(diff) = diff {
                println!("{diff}");
            }
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::TaskCancelled
        | yi_agent_store::ipc::IpcResponse::TaskPaused
        | yi_agent_store::ipc::IpcResponse::TaskResumed
        | yi_agent_store::ipc::IpcResponse::TaskRetried
        | yi_agent_store::ipc::IpcResponse::MessageQueued
        | yi_agent_store::ipc::IpcResponse::ReviewApproved
        | yi_agent_store::ipc::IpcResponse::ReviewReworkRequested
        | yi_agent_store::ipc::IpcResponse::ReviewRejected => Ok(()),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected request: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
}

fn show_review_preview(
    socket: &std::path::Path,
    task_id: String,
    decision: yi_agent_store::ipc::IpcReviewDecision,
    action: &str,
) -> Result<()> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewReview { task_id, decision },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::ReviewPreview {
            task_id,
            delivery_id,
            confirmation_token,
            expires_in_secs,
            ..
        } => {
            println!("task: {task_id}");
            println!("delivery: {delivery_id}");
            println!(
                "rerun agent {action} with --yes --confirmation {confirmation_token} within {expires_in_secs}s"
            );
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected review preview: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime review preview response: {other:?}"),
    }
}

fn show_cancel_preview(socket: &std::path::Path, task_id: String, recursive: bool) -> Result<()> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::PreviewCancel { task_id, recursive },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })? {
        yi_agent_store::ipc::IpcResponse::CancelPreview {
            confirmation_token,
            task_ids,
            expires_in_secs,
            ..
        } => {
            println!("affected tasks: {}", task_ids.join(", "));
            println!(
                "rerun with --yes --confirmation {confirmation_token} within {expires_in_secs}s"
            );
            Ok(())
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => anyhow::bail!(
            "runtime daemon rejected cancel preview: {}",
            format_ipc_error(code, message)
        ),
        other => anyhow::bail!("unexpected runtime cancel preview response: {other:?}"),
    }
}

fn follow_task_events(socket: &std::path::Path, task_id: String) -> Result<()> {
    let mut subscription = yi_agent_store::ipc::subscribe_with_filters(
        socket,
        0,
        yi_agent_store::ipc::SubscriptionFilters {
            task_ids: vec![task_id],
            kinds: Vec::new(),
        },
    )
    .map_err(|error| {
        anyhow::anyhow!("runtime daemon is unavailable; run `yi-agent daemon start`: {error}")
    })?;

    loop {
        match subscription.next_response()? {
            yi_agent_store::ipc::IpcResponse::Subscription(snapshot) => {
                for event in snapshot.events {
                    println!("{}", task_event_line(&event));
                }
            }
            yi_agent_store::ipc::IpcResponse::Event(event) => {
                println!("{}", task_event_line(&event));
            }
            yi_agent_store::ipc::IpcResponse::ResyncRequired => {
                anyhow::bail!(
                    "daemon event stream requires resync; rerun `yi-agent agent events --follow`"
                )
            }
            yi_agent_store::ipc::IpcResponse::Error { code, message } => {
                anyhow::bail!(
                    "runtime daemon rejected event subscription: {}",
                    format_ipc_error(code, message)
                )
            }
            other => anyhow::bail!("unexpected runtime event subscription response: {other:?}"),
        }
    }
}

fn task_event_line(event: &yi_agent_store::ipc::IpcEvent) -> String {
    format!("{} {} {}", event.event_id, event.kind, event.payload_json)
}

fn inspect_session(socket: &std::path::Path, task_id: &str) -> Result<String> {
    match yi_agent_store::ipc::send_request(
        socket,
        yi_agent_store::ipc::IpcRequest::InspectTask {
            task_id: task_id.into(),
        },
    )? {
        yi_agent_store::ipc::IpcResponse::TaskDetail(detail) => Ok(detail.session_id),
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected request: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
}

fn control_schedule(cli: &Cli, action: &ScheduleAction) -> Result<()> {
    let ScheduleAction::Add { request, confirm } = action;
    let config = config::load(cli)?;
    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let preview = runtime.block_on(
        schedule_intent::ScheduleIntentParser::new(provider, config.model).preview(request),
    )?;
    if !*confirm {
        println!(
            "Schedule preview (not saved): {} -> {}\nRun again with --confirm to create it.",
            preview.definition.cron, preview.definition.objective
        );
        return Ok(());
    }
    let socket = yi_agent_store::ipc::socket_path_for(&runtime_directory_for(&config.workdir))?;
    match yi_agent_store::ipc::send_request(
        &socket,
        yi_agent_store::ipc::IpcRequest::CreateSchedule {
            cron: preview.definition.cron,
            objective: preview.definition.objective,
        },
    )? {
        yi_agent_store::ipc::IpcResponse::ScheduleCreated { schedule_id } => {
            println!("schedule created: {schedule_id}")
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected schedule: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
    Ok(())
}

/// 停止句柄：置位后循环退出并 `stop_all`，用于测试与 daemon 退出时的回收。
pub(crate) fn control_daemon(cli: &Cli, action: DaemonAction) -> Result<()> {
    let workdir = config::resolve_workdir(cli)?;
    let runtime_dir = runtime_directory_for(&workdir);
    let runtime = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    let database = runtime_database_path(&runtime_dir);
    match action {
        DaemonAction::Start => {
            if yi_agent_store::ipc::send_request(&runtime, yi_agent_store::ipc::IpcRequest::Status)
                .is_ok()
            {
                anyhow::bail!("runtime daemon is already running")
            }
            // A manually started daemon creates the same project-local state
            // root the embedded runtimes do, so it must self-ignore it too.
            ignore_project_local_runtime_state(&workdir);
            std::process::Command::new(std::env::current_exe()?)
                .args(["daemon", "serve"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .current_dir(&workdir)
                .spawn()?;
            for _ in 0..50 {
                if yi_agent_store::ipc::send_request(
                    &runtime,
                    yi_agent_store::ipc::IpcRequest::Status,
                )
                .is_ok()
                {
                    println!("daemon started");
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            anyhow::bail!("daemon process did not become ready")
        }
        DaemonAction::Serve => {
            // `daemon start` spawns this subcommand detached; a directly
            // invoked `serve` must record the same self-ignore so the project
            // state it creates never dirties the checkout.
            ignore_project_local_runtime_state(&workdir);
            let daemon = yi_agent_store::ipc::Daemon::start_with_factory(
                &runtime_dir,
                &database,
                build_daemon_worker_factory(
                    cli,
                    yi_agent_store::ipc::socket_path_for(&runtime_dir)?,
                )?,
            )
            .map_err(|error| anyhow::anyhow!("could not start runtime daemon: {error}"))?;
            // `daemon start` detaches this subcommand with its stderr on
            // `/dev/null`, but a directly invoked `serve` owns the terminal and
            // so keeps the historical line.
            let supervisor = yi_agent_runtime::supervise::serve(&workdir);
            report_reclaimed_orphans_to_stderr(daemon.reclaimed_orphans());
            let result = daemon
                .wait()
                .map_err(|error| anyhow::anyhow!("runtime daemon failed: {error}"));
            supervisor.stop();
            result
        }
        DaemonAction::Status | DaemonAction::Stop => control_daemon_client(action, &runtime),
    }
}

fn build_daemon_worker_factory(
    cli: &Cli,
    runtime_socket: std::path::PathBuf,
) -> Result<Arc<dyn yi_agent_core::subagent::worker::AgentWorkerFactory>> {
    let config = config::load(cli)?;
    yi_agent_subagent::attach::worker_factory(&config, runtime_socket)
}

/// The runtime socket for a project, resolved identically everywhere.
///
/// The TUI's slash commands and the CLI must reach the same daemon, so both
/// resolve through here rather than each inventing their own location.
pub(crate) fn runtime_socket_for(
    workdir: &std::path::Path,
) -> Result<std::path::PathBuf, yi_agent_store::ipc::IpcError> {
    yi_agent_store::ipc::socket_path_for(&runtime_directory_for(workdir))
}

pub(crate) fn runtime_directory_for(workdir: &std::path::Path) -> std::path::PathBuf {
    yi_agent_subagent::attach::project_runtime_directory(workdir)
}

/// Single source of truth for the runtime database filename. The daemon and the
/// embedded TUI/headless runtimes must share one store, so they all resolve the
/// path here instead of hardcoding a name that can drift apart.
fn runtime_database_path(runtime_dir: &std::path::Path) -> std::path::PathBuf {
    runtime_dir.join("runtime.sqlite")
}

/// Keeps the daemon's own project-local state out of the checkout's git status.
///
/// The shared implementation is the single source of truth; this wrapper keeps
/// the existing call sites (and their regression tests) reading the same name.
fn ignore_project_local_runtime_state(workdir: &std::path::Path) {
    yi_agent_subagent::attach::ignore_project_local_runtime_state(workdir);
}

fn control_daemon_client(action: DaemonAction, runtime: &std::path::Path) -> Result<()> {
    let request = match action {
        DaemonAction::Status => yi_agent_store::ipc::IpcRequest::Status,
        DaemonAction::Stop => yi_agent_store::ipc::IpcRequest::Stop,
        DaemonAction::Start | DaemonAction::Serve => {
            unreachable!("handled by its own arm")
        }
    };
    let response = yi_agent_store::ipc::send_request(runtime, request)
        .map_err(|error| anyhow::anyhow!("runtime daemon is unavailable: {error}"))?;
    match response {
        yi_agent_store::ipc::IpcResponse::Status {
            high_water_event_id,
        } => println!("daemon running (event high-water: {high_water_event_id})"),
        yi_agent_store::ipc::IpcResponse::Stopping => println!("daemon stopping"),
        yi_agent_store::ipc::IpcResponse::UnsupportedProtocol { .. } => {
            anyhow::bail!("runtime daemon protocol is incompatible")
        }
        yi_agent_store::ipc::IpcResponse::Error { code, message } => {
            anyhow::bail!(
                "runtime daemon rejected request: {}",
                format_ipc_error(code, message)
            )
        }
        other => anyhow::bail!("unexpected runtime daemon response: {other:?}"),
    }
    Ok(())
}

/// Lists reclaimable worktrees, then reclaims the automatic scope.
///
fn build_headless_root_tools(
    config: &config::Config,
    runtime_socket: std::path::PathBuf,
    attached_root: &crate::tui::subagents::AttachedRoot,
) -> Result<HeadlessSetup> {
    let setup =
        build_headless_setup_for_workspace(config, false, attached_root.workspace.path.clone())?;
    let mut registry = (*setup.tools).clone();
    let binding = crate::tui::subagents::root_binding(runtime_socket, attached_root);
    let delegation_controller = yi_agent_tools::SandboxController::new(
        yi_agent_core::autonomy::YoloSwitch::new(config.yolo),
        config.sandbox,
        config.sandbox_promotable,
    );
    crate::tui::subagents::register_attached_root_tools(
        &mut registry,
        binding,
        delegation_controller,
    );
    Ok(HeadlessSetup {
        tools: Arc::new(registry),
        catalog: setup.catalog,
        system_prompt: setup.system_prompt,
        mcp: setup.mcp,
        process_manager: setup.process_manager,
    })
}

struct HeadlessRuntimeSession {
    socket_path: std::path::PathBuf,
    attached_root: crate::tui::subagents::AttachedRoot,
    embedded_daemon: Option<yi_agent_store::ipc::Daemon>,
}

fn attach_application_root_request(
    idempotency_key: String,
    workspace: std::path::PathBuf,
) -> yi_agent_store::ipc::IpcRequest {
    yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
        idempotency_key,
        workspace,
    }
}

fn attach_headless_runtime(cli: &Cli, config: &config::Config) -> Result<HeadlessRuntimeSession> {
    let runtime_dir = runtime_directory_for(&config.workdir);
    let database = runtime_database_path(&runtime_dir);
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    // Record the project-local state before the daemon creates it, otherwise
    // root worktree provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&config.workdir);
    // A daemon wedged before this launch must not be adopted: its socket lives
    // in the project directory, so it would outlive any restart. Retire it so
    // the `start_with_factory` below replaces it with a working one.
    replace_wedged_daemon(&socket_path);
    let embedded_daemon = match yi_agent_store::ipc::Daemon::start_with_factory(
        &runtime_dir,
        &database,
        build_daemon_worker_factory(cli, socket_path.clone())?,
    ) {
        Ok(daemon) => Some(daemon),
        Err(yi_agent_store::ipc::IpcError::AlreadyRunning { .. }) => None,
        Err(error) => anyhow::bail!("could not start subagent runtime: {error}"),
    };
    let response = yi_agent_store::ipc::send_request(
        &socket_path,
        attach_application_root_request(
            format!(
                "headless:{}:{}:{}",
                std::process::id(),
                config.workdir.display(),
                uuid::Uuid::new_v4()
            ),
            config.workdir.clone(),
        ),
    )
    .map_err(|error| anyhow::anyhow!("could not attach headless subagent runtime: {error}"))?;
    let yi_agent_store::ipc::IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        anyhow::bail!("{}", runtime_attached_root_rejection(&response));
    };
    Ok(HeadlessRuntimeSession {
        socket_path,
        attached_root: crate::tui::subagents::AttachedRoot {
            session_id,
            task_id: root_task_id,
            capability: message_capability,
            workspace,
        },
        embedded_daemon,
    })
}

fn activate_headless_runtime_root(runtime: &HeadlessRuntimeSession, objective: &str) -> Result<()> {
    activate_tui_runtime_root(&runtime.socket_path, &runtime.attached_root, objective)
        .map_err(|error| anyhow::anyhow!("could not activate headless subagent runtime: {error}"))
}

fn detach_headless_runtime_root(runtime: &HeadlessRuntimeSession) {
    detach_tui_runtime_root(&runtime.socket_path, &runtime.attached_root);
}

fn build_tui_root_tools(
    base_registry: &yi_agent_core::ToolRegistry,
    config: &config::Config,
    runtime_socket: std::path::PathBuf,
    attached_root: &crate::tui::subagents::AttachedRoot,
) -> yi_agent_core::ToolRegistry {
    let mut registry = base_registry.clone();
    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        attached_root.workspace.path.clone(),
        config.sandbox,
        config.sandbox_writable_roots.clone(),
    );
    let binding = crate::tui::subagents::root_binding(runtime_socket, attached_root);
    let delegation_controller = yi_agent_tools::SandboxController::new(
        yi_agent_core::autonomy::YoloSwitch::new(config.yolo),
        config.sandbox,
        config.sandbox_promotable,
    );
    crate::tui::subagents::register_attached_root_tools(
        &mut registry,
        binding,
        delegation_controller,
    );
    registry
}

/// The outcome of trying to attach the TUI to a runtime.
///
/// `Unavailable` is not an error: delegation is optional, so a runtime that
/// cannot start only disables delegation. It still carries the reason, so the
/// degradation is visible instead of appearing as a mysteriously absent tool.
enum TuiRuntimeSession {
    Attached(Box<AttachedTuiRuntime>),
    Unavailable { reason: String },
}

struct AttachedTuiRuntime {
    socket_path: std::path::PathBuf,
    attached_root: crate::tui::subagents::AttachedRoot,
    embedded_daemon: Option<yi_agent_store::ipc::Daemon>,
    /// 插件监督句柄。本 TUI 起了 daemon 才真的监督；借用别人的 daemon 时是空转。
    /// 生命周期交给 Drop：会话结束即停止监督，不必依赖调用点的纪律。
    supervisor: yi_agent_runtime::supervise::SuperviseHandle,
}

impl TuiRuntimeSession {
    fn unavailable(reason: String) -> Self {
        Self::Unavailable { reason }
    }
}

/// Explains, in one actionable line, why subagent delegation is unavailable.
///
/// Delegation is optional: a runtime that cannot start degrades the session to
/// no-delegation rather than aborting it. That degradation must still be
/// visible, because the symptom the user sees is merely a missing tool, which
/// is impossible to diagnose from the outside.
fn runtime_unavailable_reason(error: &yi_agent_store::ipc::IpcError) -> String {
    format!("subagent delegation unavailable: {error}")
}

/// Turns a rejected attach response into an actionable, human-readable reason.
///
/// A raw `{response:?}` dump buries the real cause (for example a dirty
/// checkout) behind `Error { code: InvalidState, message: Some(...) }`, which
/// reads like an internal admission fault rather than a local git state
/// problem the user can fix.
fn runtime_attached_root_rejection(response: &yi_agent_store::ipc::IpcResponse) -> String {
    match response {
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: Some(message),
        } => format!(
            "daemon rejected the runtime attachment: {code}: {message}; \
             subagent delegation is disabled for this session"
        ),
        yi_agent_store::ipc::IpcResponse::Error {
            code,
            message: None,
        } => format!(
            "daemon rejected the runtime attachment: {code}; \
             subagent delegation is disabled for this session"
        ),
        other => format!("daemon rejected the runtime attachment: {other:?}"),
    }
}

/// Retires a *wedged* local daemon so the caller can replace it.
///
/// A daemon can be alive and listening while answering every ordinary request
/// with a bare `internal` (its per-request repository connection fails before it
/// can read anything, while `Stop` still succeeds because `graceful_stop` works
/// from in-memory state). Because the socket, database, and instance lock live
/// in the project directory rather than in the process, "restart yi-agent" then
/// reconnects to the same dud forever.
///
/// The probe is `Status`: it is read-only (no session or attachment is created),
/// it is the cheapest request a daemon can serve, and the wedged daemon failed
/// it exactly as it failed everything else. `internal` is the signal -- it means
/// the daemon could not read its own store, which is never a legitimate answer
/// to `Status` (a healthy daemon always reports its high-water mark), whereas
/// `InvalidState`/`Validation` etc. are legitimate rejections that must still be
/// reported, not "fixed" by a restart.
///
/// Returns `true` when a wedged daemon was retired (its files are gone, so a
/// fresh `Daemon::start` can take over), `false` for every other outcome --
/// including a `Stop` that could not be delivered, because that daemon is not
/// ours to replace.
fn replace_wedged_daemon(socket_path: &std::path::Path) -> bool {
    // One rule, shared with the desktop's self-heal: retire only a daemon that
    // answers `internal`, and only when it accepts the `Stop`.
    yi_agent_subagent::attach::retire_if_wedged(socket_path)
}

/// Builds the notice emitted when subagent-runtime bring-up fails after the
/// user asked for it (`y`, or a remembered `always`).
///
/// Returns the event rather than sending it so the classification is testable
/// without a live driver: the original bug was that this path produced
/// `AgentEvent::Error(ProviderTurnAdmission)`, rendering the raw cause as
/// `Error: provider turn admission failed: ...` with no hint that a restart
/// restores delegation. `stage` names the failing bring-up step and `cause`
/// carries the diagnostic; both go to the trace, never to the user's line.
fn runtime_unavailable_event(stage: &str, cause: impl Into<String>) -> yi_agent_core::AgentEvent {
    yi_agent_core::AgentEvent::SubagentRuntimeUnavailable {
        stage: stage.to_owned(),
        cause: cause.into(),
    }
}

/// Maps the persisted runtime preference to what the TUI should do on launch.
///
/// The preference file is the single source of truth: a missing or malformed
/// file reads as `Ask`, so existing users keep today's behaviour.
fn startup_intent_for(
    workdir: &std::path::Path,
) -> Option<crate::tui::subagents::RuntimeStartupIntent> {
    use crate::tui::runtime_prefs::{self, RuntimePreference};
    use crate::tui::subagents::RuntimeStartupIntent;

    Some(match runtime_prefs::load(workdir) {
        RuntimePreference::Ask => RuntimeStartupIntent::Prompt,
        RuntimePreference::Always => RuntimeStartupIntent::AutoStart,
        RuntimePreference::Never => RuntimeStartupIntent::DisabledNotice {
            reason: format!(
                "已禁用子 Agent 委派（{}: never）；用 /runtime 开启",
                runtime_prefs::preferences_path(workdir).display()
            ),
        },
    })
}

fn attach_tui_runtime(cli: &Cli, config: &config::Config) -> Result<Option<TuiRuntimeSession>> {
    let runtime_dir = runtime_directory_for(&config.workdir);
    let database = runtime_database_path(&runtime_dir);
    let socket_path = yi_agent_store::ipc::socket_path_for(&runtime_dir)?;
    // Record the project-local state before the daemon creates it, otherwise
    // root worktree provisioning sees a checkout dirtied by our own store.
    ignore_project_local_runtime_state(&config.workdir);
    // A daemon wedged before this launch must not be adopted: its socket lives
    // in the project directory, so it would outlive any restart. Retire it so
    // the `start_with_factory` below replaces it with a working one.
    replace_wedged_daemon(&socket_path);
    let embedded_daemon = match yi_agent_store::ipc::Daemon::start_with_factory(
        &runtime_dir,
        &database,
        build_daemon_worker_factory(cli, socket_path.clone())?,
    ) {
        Ok(daemon) => Some(daemon),
        Err(yi_agent_store::ipc::IpcError::AlreadyRunning { .. }) => None,
        Err(error) => {
            let reason = runtime_unavailable_reason(&error);
            tracing::warn!(error = %error, "{reason}");
            return Ok(Some(TuiRuntimeSession::unavailable(reason)));
        }
    };
    // 只有"我起的 daemon"才该由我监督：借用了别人的 daemon 还去监督，
    // 两边会互相拉/停同一批插件进程。
    let owns_daemon = embedded_daemon.is_some();
    let idempotency_key = format!(
        "tui:{}:{}:{}",
        std::process::id(),
        config.workdir.display(),
        uuid::Uuid::new_v4()
    );
    let response = match yi_agent_store::ipc::send_request(
        &socket_path,
        attach_application_root_request(idempotency_key, config.workdir.clone()),
    ) {
        Ok(response) => response,
        Err(error) => {
            let reason = format!("could not reach the runtime socket: {error}");
            tracing::warn!(error = %error, "{reason}");
            return Ok(Some(TuiRuntimeSession::Unavailable { reason }));
        }
    };
    let yi_agent_store::ipc::IpcResponse::ApplicationRootAttached {
        session_id,
        root_task_id,
        message_capability,
        workspace,
    } = response
    else {
        let reason = runtime_attached_root_rejection(&response);
        tracing::warn!(response = ?response, "{reason}");
        return Ok(Some(TuiRuntimeSession::Unavailable { reason }));
    };
    Ok(Some(TuiRuntimeSession::Attached(Box::new(
        AttachedTuiRuntime {
            socket_path,
            attached_root: crate::tui::subagents::AttachedRoot {
                session_id,
                task_id: root_task_id,
                capability: message_capability,
                workspace,
            },
            embedded_daemon,
            supervisor: yi_agent_runtime::supervise::for_daemon_ownership(
                &config.workdir,
                owns_daemon,
            ),
        },
    ))))
}

fn activate_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
    objective: &str,
) -> Result<()> {
    yi_agent_subagent::attach::activate_root(socket_path, root, objective)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

fn detach_tui_runtime_root(
    socket_path: &std::path::Path,
    root: &crate::tui::subagents::AttachedRoot,
) {
    yi_agent_subagent::attach::detach_root(socket_path, root);
}

fn load_permission_checker_for_workdir(
    workdir: std::path::PathBuf,
    config: &config::Config,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    yi_agent_runtime::bootstrap::load_permission_checker(&workdir, config.yolo)
}

async fn load_permission_checker_for_workdir_async(
    workdir: std::path::PathBuf,
    config: &config::Config,
) -> Result<Arc<yi_agent_core::permission::PermissionChecker>> {
    let yolo = config.yolo;
    tokio::task::spawn_blocking(move || {
        yi_agent_runtime::bootstrap::load_permission_checker(&workdir, yolo)
    })
    .await
    .map_err(|e| anyhow::anyhow!("permission loader task failed: {e}"))?
}

fn run_agent(cli: Cli) -> Result<()> {
    let config = config::load(&cli)?;

    let agent_workdir = config.workdir.clone();

    // Load permissions and construct checker for the initial project workspace.
    let checker = load_permission_checker_for_workdir(agent_workdir.clone(), &config)?;
    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);

    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;

    let mut registry = yi_agent_core::ToolRegistry::new();

    // --- Skills system setup ---
    let prompt = yi_agent_runtime::bootstrap::build_prompt_setup(&config)?;

    // Register Skill tool
    if let Some(svc) = &prompt.skills {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    // `base_registry` MUST stay skills-only: `build_tui_root_tools` clones it
    // and adds workspace-rooted builtin tools + subagent tools on top.
    let base_registry = registry.clone();
    yi_agent_tools::register_builtin_tools_with_sandbox(
        &mut registry,
        config.workdir.clone(),
        config.sandbox,
        config.sandbox_writable_roots.clone(),
    );
    let process_manager = yi_agent_tools::ProcessManager::with_sandbox(
        config.workdir.clone(),
        yi_agent_tools::SandboxPolicy::new(
            config.sandbox,
            &config.workdir,
            config.sandbox_writable_roots.clone(),
        ),
    );
    yi_agent_tools::register_process_tools(&mut registry, process_manager.clone());

    // Register MCP tools and keep the manager so the TUI can toggle servers and
    // the driver can refresh the registry at runtime. A bad MCP config must never
    // block the agent: warn and continue with an empty manager.
    let mcp = match yi_agent_mcp::register_mcp_tools(&mut registry, &config.workdir) {
        Ok(Some(manager)) => manager,
        Ok(None) => yi_agent_mcp::McpManager::empty(),
        Err(err) => {
            tracing::warn!("MCP disabled: {err:#}");
            yi_agent_mcp::McpManager::empty()
        }
    };

    let tools = Arc::new(registry);

    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, prompt.system_prompt);

    run_tui_agent(
        provider,
        tools,
        agent_config,
        agent_workdir,
        checker,
        decision_tx,
        decision_rx,
        cli,
        config,
        base_registry,
        process_manager,
        prompt.catalog,
        mcp,
    )
}

/// Drain an `AgentEvent` stream to the provided writers in human-readable
/// (non-JSON) form. Returns the process exit code.
///
/// `AssistantText` deltas are written inline without forcing a newline
/// after every chunk, so streaming text renders as one continuous line
/// (the LLM's own newlines are preserved). A trailing newline is emitted
/// at the end of the stream if the last assistant text did not end with
/// one, so the shell prompt (or any following output) starts on a fresh
/// line.
///
/// ToolCall / ToolResult / Done / Cancelled / Error are routed to `err`.
/// Drain the stream while staying interruptible from the terminal.
///
/// Without this, a `SIGINT` (Ctrl+C) during `yi-agent run` hit the default
/// disposition: the process died on the spot, before the in-flight bash tool's
/// future was dropped. The bash tool reaps its process group only from
/// `ProcessGroupGuard::drop` / tokio's `kill_on_drop`, so nothing ran and the
/// command's whole group was **reparented to init and kept running forever** —
/// invisible to the user who had just pressed Ctrl+C. `SIGTERM` (`kill`, a
/// supervisor, a CI timeout) behaved the same way.
///
/// Here a signal takes the same path the TUI and the app-server already use:
/// cancel the agent, keep draining until the run loop emits `Cancelled` (which
/// drops the tool future and so reaps the group), then exit 130 like a shell
/// job killed by Ctrl+C. A second signal means the user is insisting, so it
/// falls through to the default disposition.
async fn drain_with_signals<W: std::io::Write, E: std::io::Write>(
    stream: futures::stream::BoxStream<'static, yi_agent_core::AgentEvent>,
    out: &mut W,
    err: &mut E,
    json: bool,
    agent: &yi_agent_core::Agent,
) -> i32 {
    use futures::StreamExt;

    let mut stream = std::pin::pin!(stream);
    let mut signal = std::pin::pin!(shutdown_signal());
    let mut interrupted = false;
    let mut exit_code = 0;
    // Mirrors `drain_stream_human`: text deltas are written inline, and a single
    // trailing newline is emitted at the end only when the last one lacked it.
    let mut mid_line = false;

    loop {
        tokio::select! {
            biased;
            name = &mut signal, if !interrupted => {
                if let Some(name) = name {
                    eprintln!("[interrupted:received {name}, stopping the current run]");
                    interrupted = true;
                    agent.cancel();
                }
            }
            event = stream.next() => {
                let Some(event) = event else { break };
                if json {
                    let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                    let _ = writeln!(out, "{line}");
                } else {
                    record_human(&event, out, err, &mut mid_line);
                }
                // Both output modes report the same outcome: a cancelled run is
                // 130 and a failed one is 1, in `--json` as much as in the human
                // rendering. A silent 0 would make automation read a cancelled
                // run as a success.
                exit_code = exit_code.max(event_exit_code(&event));
                if is_terminal(&event) {
                    break;
                }
            }
        }
    }

    if !json && mid_line {
        let _ = out.write_all(b"\n");
    }
    exit_code
}

/// The exit code an event implies once the stream reaches it.
fn event_exit_code(event: &yi_agent_core::AgentEvent) -> i32 {
    match event {
        yi_agent_core::AgentEvent::Cancelled => 130,
        yi_agent_core::AgentEvent::Error(_) => 1,
        yi_agent_core::AgentEvent::Done {
            reason: yi_agent_core::DoneReason::Interrupted { .. },
        } => 1,
        _ => 0,
    }
}

/// Render one event in the human format. Returns the exit code it implies and
/// leaves the trailing-newline bookkeeping to the caller.
fn record_human<W: std::io::Write, E: std::io::Write>(
    event: &yi_agent_core::AgentEvent,
    out: &mut W,
    err: &mut E,
    mid_line: &mut bool,
) {
    match event {
        yi_agent_core::AgentEvent::AssistantText(t) => {
            let _ = out.write_all(t.as_bytes());
            *mid_line = !t.ends_with('\n');
        }
        yi_agent_core::AgentEvent::ToolCall { name, input, .. } => {
            let _ = writeln!(err, "[tool:{name}] {input}");
        }
        yi_agent_core::AgentEvent::ToolResult { id, result } => {
            let _ = writeln!(
                err,
                "[result:{id}] error={} content={:?}",
                result.is_error, result.content
            );
        }
        yi_agent_core::AgentEvent::ToolRetry { id } => {
            let _ = writeln!(err, "[tool-retry:{id}]");
        }
        yi_agent_core::AgentEvent::ProviderRetry {
            attempt,
            max,
            cause,
            ..
        } => match cause {
            // The stall line keeps its historical shape (no suffix) so existing
            // log scrapers stay valid; a timeout adds one.
            yi_agent_core::RetryCause::IdleStall => {
                let _ = writeln!(err, "[provider-retry:{attempt}/{max}]");
            }
            yi_agent_core::RetryCause::RequestTimeout => {
                let _ = writeln!(err, "[provider-retry:{attempt}/{max} timeout]");
            }
        },
        yi_agent_core::AgentEvent::Done { reason } => match reason {
            // Normal completion is already signaled by exit code 0; the
            // [done:EndTurn] line is noise on stderr and is suppressed to match
            // the TUI, which renders EndTurn as a silent separator.
            yi_agent_core::DoneReason::EndTurn => {}
            yi_agent_core::DoneReason::MaxTurns => {
                let _ = writeln!(err, "[done:{reason:?}]");
            }
            yi_agent_core::DoneReason::Interrupted { reason } => {
                let _ = writeln!(err, "[interrupted:{reason}]");
            }
        },
        yi_agent_core::AgentEvent::Cancelled => {
            let _ = writeln!(err, "[cancelled]");
        }
        yi_agent_core::AgentEvent::Error(e) => {
            let _ = writeln!(err, "[error:{e}]");
        }
        _ => {}
    }
}

/// Announces a startup reclaim sweep on stderr.
///
/// The store used to print this itself as it reclaimed. That write is unsafe
/// whenever the daemon is embedded: it shares the process -- and the terminal --
/// with the front end that started it, so a bare `eprintln!` landed in the
/// middle of a live TUI frame and smeared the input box's styling. The store now
/// only reports the count; whoever owns the terminal decides what to do with it.
/// A TUI renders it as a transcript notice, everyone else keeps this line.
fn report_reclaimed_orphans_to_stderr(reclaimed: usize) {
    report_reclaimed_orphans(&mut std::io::stderr(), reclaimed);
}

/// The writer-injectable core of [`report_reclaimed_orphans_to_stderr`].
fn report_reclaimed_orphans<W: std::io::Write>(out: &mut W, reclaimed: usize) {
    if reclaimed > 0 {
        let _ = writeln!(
            out,
            "yi-agent runtime: reclaimed {reclaimed} orphaned task(s)"
        );
    }
}

fn is_terminal(event: &yi_agent_core::AgentEvent) -> bool {
    matches!(
        event,
        yi_agent_core::AgentEvent::Done { .. }
            | yi_agent_core::AgentEvent::Cancelled
            | yi_agent_core::AgentEvent::Error(_)
    )
}

/// Watcher for the signals that mean "stop this run": `SIGINT` (Ctrl+C) and
/// `SIGTERM` (`kill`, supervisor shutdown, CI timeout). Yields the signal name
/// once; the caller stops listening afterwards, leaving a second signal to the
/// default disposition so `Ctrl+C Ctrl+C` still terminates immediately.
async fn shutdown_signal() -> Option<&'static str> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = int.recv() => Some("SIGINT"),
            _ = term.recv() => Some("SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.ok()?;
        Some("Ctrl+C")
    }
}

/// 根据 `naked` flag 构建 headless 模式用的工具集和 system prompt。
///
/// `naked = true`:不注册任何工具,不加载 skills,`system_prompt = None`(裸模型)。
/// `naked = false`:与 TUI `run_agent` 对齐 — 注册内置工具、加载 skills、
/// 注册 SkillTool、解析默认 prompt + 当前日期 + skills catalog。
fn build_headless_setup(config: &config::Config, naked: bool) -> Result<HeadlessSetup> {
    build_headless_setup_for_workspace(config, naked, config.workdir.clone())
}

fn build_headless_setup_for_workspace(
    config: &config::Config,
    naked: bool,
    workspace: std::path::PathBuf,
) -> Result<HeadlessSetup> {
    yi_agent_runtime::bootstrap::build_tool_setup_in(config, naked, &workspace)
}

/// Run agent non-interactively: drain AgentEvent stream to stdout/stderr.
/// Used for headless CLI usage and end-to-end real-LLM testing.
fn run_headless(
    cli: Cli,
    prompt: Option<String>,
    json: bool,
    from_stdin: bool,
    naked: bool,
    subagents: bool,
) -> Result<()> {
    let config = config::load(&cli)?;

    // Resolve prompt: explicit stdin flag > no prompt arg > prompt arg
    let prompt_text = match (from_stdin, prompt) {
        (true, _) | (false, None) => {
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            buf.trim_end_matches('\n').to_string()
        }
        (false, Some(p)) => p,
    };
    if prompt_text.is_empty() {
        anyhow::bail!("empty prompt");
    }

    if subagents && naked {
        anyhow::bail!("--subagents cannot be combined with --naked");
    }

    let headless_runtime = if subagents {
        let runtime = attach_headless_runtime(&cli, &config)?;
        // `yi-agent run` owns the terminal and streams its own diagnostics, so
        // the reclaim line keeps its historical home here.
        if let Some(reclaimed) = runtime
            .embedded_daemon
            .as_ref()
            .map(|daemon| daemon.reclaimed_orphans())
        {
            report_reclaimed_orphans_to_stderr(reclaimed);
        }
        activate_headless_runtime_root(&runtime, &prompt_text)?;
        Some(runtime)
    } else {
        None
    };
    let workdir = headless_runtime
        .as_ref()
        .map(|runtime| runtime.attached_root.workspace.path.clone())
        .unwrap_or_else(|| config.workdir.clone());
    // Headless mode: auto-allow non-blacklisted tools (yolo behavior)
    let checker = yi_agent_runtime::bootstrap::load_permission_checker(&workdir, true)?;
    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);
    // Headless mode has no confirmation UI. Closing the sender makes a
    // blacklisted command resolve as Deny instead of waiting forever.
    drop(decision_tx);

    let provider = yi_agent_runtime::bootstrap::build_provider(&config)?;

    let setup = match &headless_runtime {
        Some(runtime) => {
            build_headless_root_tools(&config, runtime.socket_path.clone(), &runtime.attached_root)?
        }
        None => build_headless_setup(&config, naked)?,
    };
    let tools = setup.tools;

    let agent_config =
        yi_agent_runtime::bootstrap::build_agent_config(&config, setup.system_prompt);
    let mcp = setup.mcp;

    let rt = tokio::runtime::Runtime::new()?;
    let exit_code = rt.block_on(async move {
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        let mut agent = yi_agent_core::Agent::new(provider, tools, agent_config)
            .with_permission(checker, decision_rx);

        let stream = match agent.run(prompt_text).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: {e}");
                return 1;
            }
        };

        let stdout = std::io::stdout();
        let stderr = std::io::stderr();
        let mut out = stdout.lock();
        let mut err = stderr.lock();
        drain_with_signals(stream, &mut out, &mut err, json, &agent).await
    });

    // Close MCP server connections on a running reactor so the child processes
    // are reaped; `std::process::exit` below would skip destructors. Done after
    // `block_on` so it also covers the early-error exit path.
    if let Some(manager) = &mcp {
        rt.block_on(manager.shutdown());
    }

    if let Some(runtime) = &headless_runtime {
        detach_headless_runtime_root(runtime);
        let _embedded_daemon = runtime.embedded_daemon.as_ref();
    }
    drop(headless_runtime);
    std::process::exit(exit_code);
}

/// Run the ratatui TUI. Sets up channels, spawns agent driver task, calls run_tui.
#[allow(clippy::too_many_arguments)]
fn run_tui_agent(
    provider: Arc<dyn Provider>,
    tools: Arc<yi_agent_core::ToolRegistry>,
    agent_config: yi_agent_core::AgentConfig,
    workdir: std::path::PathBuf,
    checker: Arc<yi_agent_core::permission::PermissionChecker>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    decision_rx: tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
    cli: Cli,
    config: config::Config,
    base_registry: yi_agent_core::ToolRegistry,
    process_manager: Arc<yi_agent_tools::ProcessManager>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
) -> Result<()> {
    use futures::StreamExt;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::mpsc;

    let rt = tokio::runtime::Runtime::new()?;
    let tui_result = rt.block_on(async move {
        // Channels between agent driver and TUI
        let (agent_tx, agent_rx) = mpsc::channel::<yi_agent_core::AgentEvent>(256);
        let (input_tx, mut input_rx) = mpsc::channel::<String>(16);
        let (interrupt_tx, mut interrupt_rx) = mpsc::channel::<()>(1);
        let (control_tx, mut control_rx) = mpsc::channel::<ControlCommand>(8);
        // Mid-turn kill requests (TUI bash panel) need their own channel: the
        // control channel is consumed at prompt boundaries, and a kill has to
        // land while the tool is actually running.
        let (kill_tx, mut kill_rx) = mpsc::channel::<String>(8);
        let (runtime_choice_tx, mut runtime_choice_rx) =
            mpsc::channel::<crate::tui::subagents::RuntimeStartupChoice>(1);
        let runtime_detach = Arc::new(std::sync::Mutex::new(None::<(
            std::path::PathBuf,
            crate::tui::subagents::AttachedRoot,
        )>));
        let is_running = Arc::new(AtomicBool::new(false));

        // `always` reuses the existing attach path by pre-seeding the choice the
        // driver would otherwise wait for. Capacity is 1 and nothing has been
        // sent yet, so `try_send` cannot fail here.
        let runtime_intent = startup_intent_for(&workdir);
        if matches!(
            runtime_intent,
            Some(crate::tui::subagents::RuntimeStartupIntent::AutoStart)
        ) {
            let _ = runtime_choice_tx
                .try_send(crate::tui::subagents::RuntimeStartupChoice::Start);
        }

        enum DriverInput {
            Prompt(Option<String>),
            Control(Option<ControlCommand>),
            RuntimeChoice(Option<crate::tui::subagents::RuntimeStartupChoice>),
        }

        // Spawn agent driver task (stays on the async runtime)
        let provider_clone = Arc::clone(&provider);
        let mut current_tools = Arc::clone(&tools);
        let mut current_checker = Arc::clone(&checker);
        let config_clone = agent_config.clone();
        let is_running_clone = Arc::clone(&is_running);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        let rebuild_provider = Arc::clone(&provider);
        let rebuild_config = agent_config.clone();
        let rebuild_decision_rx = Arc::clone(&decision_rx);
        let runtime_detach_for_driver = Arc::clone(&runtime_detach);
        let mcp_for_driver = Arc::clone(&mcp);
        let mcp_for_teardown = Arc::clone(&mcp);
        let driver = tokio::spawn(async move {
            let mut root_activated = false;
            let mut current_runtime: Option<TuiRuntimeSession> = None;
            let catalog = catalog;
            let mut agent = yi_agent_core::Agent::new(
                Arc::clone(&provider_clone),
                Arc::clone(&current_tools),
                config_clone,
            )
            .with_permission(Arc::clone(&current_checker), Arc::clone(&decision_rx));
            let _ = workdir; // workdir already passed to tools registration

            loop {
                // Wait for user input, runtime startup choice, or a control command.
                let input = tokio::select! {
                    biased;
                    cmd = control_rx.recv() => DriverInput::Control(cmd),
                    choice = runtime_choice_rx.recv() => DriverInput::RuntimeChoice(choice),
                    text = input_rx.recv() => DriverInput::Prompt(text),
                };

                // Handle control commands first (rebuild agent, no prompt run).
                if let DriverInput::Control(Some(cmd)) = input {
                    match cmd {
                        ControlCommand::Clear => {
                            // Rebuild agent with empty session.
                            agent = yi_agent_core::Agent::new(
                                Arc::clone(&rebuild_provider),
                                Arc::clone(&current_tools),
                                rebuild_config.clone(),
                            )
                            .with_session(yi_agent_core::Session::new())
                            .with_permission(
                                Arc::clone(&current_checker),
                                Arc::clone(&rebuild_decision_rx),
                            );
                            tracing::info!("agent session cleared via /clear");
                        }
                        ControlCommand::Compact => {
                            let session = agent.session();
                            match yi_agent_core::compact_session(
                                &rebuild_provider,
                                &rebuild_config,
                                &session,
                            )
                            .await
                            {
                                Ok(Some(new_session)) => {
                                    agent = yi_agent_core::Agent::new(
                                        Arc::clone(&rebuild_provider),
                                        Arc::clone(&current_tools),
                                        rebuild_config.clone(),
                                    )
                                    .with_session(new_session)
                                    .with_permission(
                                        Arc::clone(&current_checker),
                                        Arc::clone(&rebuild_decision_rx),
                                    );
                                    tracing::info!("agent session compacted via /compact");
                                }
                                Ok(None) => {
                                    tracing::info!("no compactable session history");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "compact failed");
                                    let _ =
                                        agent_tx.send(yi_agent_core::AgentEvent::Error(e)).await;
                                }
                            }
                        }
                        ControlCommand::McpRefresh => {
                            let mut refreshed = (*current_tools).clone();
                            mcp_for_driver.refresh_registry(&mut refreshed);
                            current_tools = Arc::new(refreshed);
                            agent = yi_agent_core::Agent::new(
                                Arc::clone(&rebuild_provider),
                                Arc::clone(&current_tools),
                                rebuild_config.clone(),
                            )
                            .with_session(agent.session())
                            .with_permission(
                                Arc::clone(&current_checker),
                                Arc::clone(&rebuild_decision_rx),
                            );
                            tracing::info!("MCP tool registry refreshed");
                        }
                    }
                    continue;
                }

                if let DriverInput::RuntimeChoice(choice) = input {
                    match choice {
                        Some(crate::tui::subagents::RuntimeStartupChoice::Start) => {
                            match attach_tui_runtime(&cli, &config) {
                                Ok(Some(TuiRuntimeSession::Unavailable { reason })) => {
                                    tracing::warn!(
                                        cause = %reason,
                                        "subagent runtime unavailable; continuing without delegation"
                                    );
                                    let _ = agent_tx
                                        .send(runtime_unavailable_event("runtime attach", reason))
                                        .await;
                                }
                                Ok(Some(TuiRuntimeSession::Attached(attached))) => {
                                    let AttachedTuiRuntime {
                                        socket_path,
                                        attached_root,
                                        embedded_daemon,
                                        supervisor,
                                    } = *attached;
                                    if let Some(daemon) = &embedded_daemon {
                                        tracing::info!("embedded subagent runtime started for TUI");
                                        // A started daemon may have swept orphaned
                                        // tasks. The count goes through the event
                                        // stream so the TUI draws it in the
                                        // transcript; printing it here would write
                                        // into a live frame and smear the input box.
                                        let reclaimed = daemon.reclaimed_orphans();
                                        if reclaimed > 0 {
                                            tracing::info!(
                                                reclaimed,
                                                "reclaimed orphaned subagent tasks"
                                            );
                                            let _ = agent_tx
                                                .send(
                                                    yi_agent_core::AgentEvent::OrphanedTasksReclaimed {
                                                        count: reclaimed,
                                                    },
                                                )
                                                .await;
                                        }
                                    }
                                    *runtime_detach_for_driver
                                        .lock()
                                        .expect("runtime detach mutex poisoned") = Some((
                                        socket_path.clone(),
                                        attached_root.clone(),
                                    ));
                                    crate::tui::subagents::set_current_attached_root(
                                        attached_root.clone(),
                                    );
                                    let runtime_workdir = attached_root.workspace.path.clone();
                                    let mut next_registry = build_tui_root_tools(
                                        &base_registry,
                                        &config,
                                        socket_path.clone(),
                                        &attached_root,
                                    );
                                    mcp_for_driver.refresh_registry(&mut next_registry);
                                    let next_tools = Arc::new(next_registry);
                                    match load_permission_checker_for_workdir_async(runtime_workdir, &config).await {
                                        Ok(next_checker) => {
                                            let session = agent.session();
                                            current_tools = next_tools;
                                            current_checker = next_checker;
                                            agent = yi_agent_core::Agent::new(
                                                Arc::clone(&rebuild_provider),
                                                Arc::clone(&current_tools),
                                                rebuild_config.clone(),
                                            )
                                            .with_session(session)
                                            .with_permission(
                                                Arc::clone(&current_checker),
                                                Arc::clone(&rebuild_decision_rx),
                                            );
                                            current_runtime =
                                                Some(TuiRuntimeSession::Attached(Box::new(
                                                    AttachedTuiRuntime {
                                                        socket_path,
                                                        attached_root,
                                                        embedded_daemon,
                                                        supervisor,
                                                    },
                                                )));
                                            root_activated = false;
                                        }
                                        Err(error) => {
                                            if let Some((socket, root)) = runtime_detach_for_driver
                                                .lock()
                                                .expect("runtime detach mutex poisoned")
                                                .take()
                                            {
                                                detach_tui_runtime_root(&socket, &root);
                                            }
                                            let cause = error.to_string();
                                            tracing::warn!(
                                                %cause,
                                                "subagent runtime attach failed; continuing without delegation"
                                            );
                                            let _ = agent_tx
                                                .send(runtime_unavailable_event(
                                                    "runtime attach",
                                                    cause,
                                                ))
                                                .await;
                                        }
                                    }
                                }
                                Ok(None) => {
                                    tracing::warn!("subagent runtime unavailable; continuing without delegation");
                                }
                                Err(error) => {
                                    let _ = agent_tx
                                        .send(yi_agent_core::AgentEvent::Error(
                                            yi_agent_core::AgentError::ProviderTurnAdmission(
                                                error.to_string(),
                                            ),
                                        ))
                                        .await;
                                }
                            }
                        }
                        Some(crate::tui::subagents::RuntimeStartupChoice::ContinueWithoutDelegation) => {
                            tracing::info!("TUI subagent runtime disabled by user choice");
                        }
                        None => break,
                    }
                    continue;
                }

                // Otherwise, a prompt arrived (or all channels closed).
                let DriverInput::Prompt(Some(text)) = input else {
                    break;
                };

                // Clear any stale interrupt signal
                let _ = interrupt_rx.try_recv();

                if !root_activated {
                    if let Some(TuiRuntimeSession::Attached(attached)) =
                        current_runtime.as_ref()
                    {
                        if let Err(error) = activate_tui_runtime_root(
                            &attached.socket_path,
                            &attached.attached_root,
                            &text,
                        ) {
                            let cause = error.to_string();
                            tracing::warn!(
                                %cause,
                                "could not activate TUI runtime root; continuing without delegation"
                            );
                            let _ = agent_tx
                                .send(runtime_unavailable_event("runtime activation", cause))
                                .await;
                            continue;
                        }
                    }
                    root_activated = true;
                }

                // Run agent
                is_running_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(handle) = &catalog {
                    if let Some(prompt) = handle.current_system_prompt() {
                        agent.set_system_prompt(Some(prompt));
                    }
                }
                match agent.run(text).await {
                    Ok(stream) => {
                        let mut stream = Box::pin(stream);
                        // 每次 run() 之后取一次：句柄只在本次 stream 的生命期内有效，
                        // 下一轮重新取（run() 内部会重建 inbox）。
                        let inbox = agent.inbox_handle();
                        loop {
                            // Concurrently forward events and listen for interrupt
                            tokio::select! {
                                event = stream.next() => {
                                    match event {
                                        Some(ev) => {
                                            if agent_tx.send(ev).await.is_err() {
                                                break;
                                            }
                                        }
                                        None => break, // stream ended
                                    }
                                }
                                Some(tool_call_id) = kill_rx.recv() => {
                                    // The TUI's bash panel asked to stop one call.
                                    // The run keeps going; the model sees an error
                                    // result for the killed call.
                                    let killed = agent.cancel_tool_call(&tool_call_id);
                                    tracing::info!(
                                        tool_call = %tool_call_id,
                                        killed,
                                        "TUI requested a tool-call kill"
                                    );
                                }
                                _ = interrupt_rx.recv() => {
                                    // User pressed Ctrl+C/Esc: cancel agent
                                    agent.cancel();
                                    // Drain remaining events until Cancelled/Done
                                    while let Some(ev) = stream.next().await {
                                        if agent_tx.send(ev).await.is_err() { break; }
                                    }
                                    break;
                                }
                                Some(text) = input_rx.recv() => {
                                    // 本轮在途时的提交：折进当前轮次，而不是让 driver
                                    // 把它当成下一个 prompt（那要等整轮结束才生效）。
                                    // inbox 为 None 的窗口（attach/重建 agent 与首次
                                    // run() 之间）走回推分支，TUI 收到后把文本还给输入框。
                                    let outcome = match &inbox {
                                        Some(handle) => handle.interject(text.clone(), None),
                                        None => Err(yi_agent_core::InterjectError::NotRunning),
                                    };
                                    if let Err(error) = outcome {
                                        tracing::warn!(
                                            %error,
                                            "mid-turn interjection was not accepted"
                                        );
                                        if agent_tx
                                            .send(yi_agent_core::AgentEvent::InterjectionsReturned {
                                                items: vec![yi_agent_core::Interjection {
                                                    seq: 0,
                                                    text,
                                                    tag: None,
                                                }],
                                            })
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = agent_tx.send(yi_agent_core::AgentEvent::Error(e)).await;
                    }
                }
                is_running_clone.store(false, std::sync::atomic::Ordering::SeqCst);
            }
            if current_runtime.is_some() {
                if let Some((socket, root)) = runtime_detach_for_driver
                    .lock()
                    .expect("runtime detach mutex poisoned")
                    .take()
                {
                    detach_tui_runtime_root(&socket, &root);
                }
            }
        });

        // Run TUI on a dedicated blocking thread (it uses sync crossterm polling)
        let tui_handle = tokio::task::spawn_blocking(move || {
            crate::tui::app::run_tui(
                agent_rx,
                input_tx,
                interrupt_tx,
                kill_tx,
                control_tx,
                decision_tx,
                is_running,
                agent_config.model.clone(),
                runtime_intent,
                // The driver's `RuntimeChoice(None) => break` means this sender
                // must outlive the session: dropping it would end the run.
                Some(runtime_choice_tx),
                process_manager,
                workdir.clone(),
                mcp,
            )
        });

        let result = match tui_handle.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(e) => Err(anyhow::Error::from(e)),
        };

        if let Some((socket, root)) = runtime_detach
            .lock()
            .expect("runtime detach mutex poisoned")
            .take()
        {
            detach_tui_runtime_root(&socket, &root);
        }

        // TUI exited; abort the driver task to clean up
        // (driver may still be blocked on input_rx.recv() if agent was idle)
        driver.abort();
        // Await the aborted handle so the driver future — and any `Arc<Client>`
        // clone it held mid-call — is dropped before shutdown. Otherwise
        // `shutdown`'s `Arc::try_unwrap` fails and the child is not reaped
        // deterministically. `await` on an aborted handle resolves promptly.
        let _ = driver.await;

        // Close MCP server connections while the reactor is still running so the
        // child processes are reaped before the runtime is dropped.
        mcp_for_teardown.shutdown().await;

        result
    });

    tui_result?;

    Ok(())
}

/// Control commands sent from the TUI to the agent driver task.
/// Allows the TUI to trigger agent session rebuilds (e.g. /clear, /compact)
/// without reconstructing the whole agent inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlCommand {
    /// Clear the agent session (rebuild with empty session).
    Clear,
    /// Compact the agent session (summarize old messages, keep recent turns).
    Compact,
    /// Rebuild the agent so its tool registry matches the MCP switches the TUI
    /// already applied directly to the shared `McpManager`.
    McpRefresh,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_listen_accepts_stdio() {
        assert!(matches!(parse_listen("stdio://").unwrap(), Listen::Stdio));
    }

    #[test]
    fn the_supervisor_loop_starts_a_manifest_when_its_switch_is_on() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path();
        // 假子进程：写标记后睡。
        let marker = workdir.join("child.pid");
        let child = workdir.join("child.sh");
        std::fs::write(
            &child,
            format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", marker.display()),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&child).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&child, perms).unwrap();
        }
        let manifests = workdir.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("demo.json"),
            format!(
                r#"{{"name":"demo","command":"{}","args":[],"switch_key":"demo_on","restart_backoff_ms":50,"restart_backoff_max_ms":200}}"#,
                child.display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
        std::fs::write(
            workdir.join(".yi-agent/preferences.json"),
            r#"{"demo_on":true}"#,
        )
        .unwrap();

        let handle = yi_agent_runtime::supervise::serve(workdir);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut started = false;
        while Instant::now() < deadline {
            if marker.exists() {
                started = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        handle.stop();
        assert!(started, "the supervisor loop should have started the child");
    }

    #[test]
    fn parse_listen_accepts_ws_with_an_address() {
        match parse_listen("ws://127.0.0.1:8790").unwrap() {
            Listen::Ws(addr) => assert_eq!(addr.port(), 8790),
            _ => panic!("ws:// must parse to a Ws listener"),
        }
    }

    #[test]
    fn parse_listen_rejects_other_transports() {
        assert!(parse_listen("tcp://127.0.0.1:9000").is_err());
        assert!(parse_listen("ws://not-an-address").is_err());
    }

    /// `--listen relay://<ws-url>` 是 `--relay` 的等价长写法:剥掉 `relay://`
    /// 前缀后应得到裸的 `wss://` 电脑侧端点。
    #[test]
    fn parse_listen_accepts_a_relay_url() {
        match parse_listen("relay://wss://relay.example/connect?session=abc").unwrap() {
            Listen::Relay(url) => assert_eq!(url, "wss://relay.example/connect?session=abc"),
            other => panic!("relay:// must parse to a Relay listener, got {other:?}"),
        }
    }

    /// `relay://` 后面必须是 `ws://`/`wss://`,且不能为空。
    #[test]
    fn parse_listen_rejects_a_malformed_relay_url() {
        assert!(parse_listen("relay://").is_err());
        assert!(parse_listen("relay://http://relay.example").is_err());
    }

    /// 从中继端点里拆出 `session` 参数:返回「去掉 session 的 URL」+「session id」,
    /// 供 `run_client` 分别使用(它会自己补 `?session=`;若不摘掉就会重复)。
    #[test]
    fn relay_parts_extracts_the_session_and_strips_it() {
        let (url, session) = relay_parts("wss://relay.example/connect?session=abc&pin=1").unwrap();
        assert_eq!(session, "abc");
        assert_eq!(url.as_str(), "wss://relay.example/connect?pin=1");
    }

    #[test]
    fn relay_parts_requires_a_session() {
        assert!(relay_parts("wss://relay.example/connect").is_err());
    }

    #[test]
    fn format_pair_code_includes_code_and_validity() {
        assert_eq!(format_pair_code("ABCD-EFGH", 300), "ABCD-EFGH (valid 300s)");
    }

    #[test]
    fn format_device_list_renders_rows_and_an_empty_notice() {
        use yi_agent_app_server::device_store::Device;
        use yi_agent_app_server::protocol::Scope;

        assert_eq!(format_device_list(&[]), "no paired devices\n");

        let devices = vec![Device {
            id: "dev-1".into(),
            name: "iPhone".into(),
            scope: Scope::Control,
            token_hash: "h".into(),
            created_at: 42,
            last_seen_at: 42,
        }];
        let rendered = format_device_list(&devices);
        assert!(rendered.contains("dev-1"));
        assert!(rendered.contains("iPhone"));
        assert!(rendered.contains("Control"));
    }

    #[test]
    fn format_revoke_result_distinguishes_hit_and_miss() {
        assert_eq!(format_revoke_result("dev-1", true), "revoked dev-1");
        assert_eq!(
            format_revoke_result("dev-9", false),
            "no such device: dev-9"
        );
    }

    /// `pair <sub>` 子命令可被 clap 解析。
    #[test]
    fn pair_subcommand_parses_code_list_and_revoke() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["yi-agent", "pair", "code"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pair {
                action: PairAction::Code
            })
        ));
        let cli = Cli::try_parse_from(["yi-agent", "pair", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Pair {
                action: PairAction::List
            })
        ));
        let cli = Cli::try_parse_from(["yi-agent", "pair", "revoke", "dev-1"]).unwrap();
        match cli.command {
            Some(Command::Pair {
                action: PairAction::Revoke { device_id },
            }) => assert_eq!(device_id, "dev-1"),
            other => panic!("expected pair revoke, got {other:?}"),
        }
    }

    #[test]
    fn default_system_prompt_requires_a_workdir_for_isolation() {
        let prompt = yi_agent_core::AgentConfig::default_system_prompt();
        assert!(
            prompt.contains("git worktree add"),
            "default prompt must tell a parent to prepare an isolated workdir"
        );
        assert!(
            prompt.contains("workdir"),
            "default prompt must name the spawn_agent workdir argument"
        );
        assert!(
            prompt.contains("git merge --no-ff"),
            "default prompt must still tell parents to integrate the delivered commit"
        );
    }

    // --- drain_stream_human tests ---

    use futures::stream::{self, BoxStream, StreamExt};
    use yi_agent_core::{AgentEvent, DoneReason};

    fn scripted_stream(events: Vec<AgentEvent>) -> BoxStream<'static, AgentEvent> {
        stream::iter(events).boxed()
    }

    // --- build_headless_setup tests ---

    use crate::config::Config;
    use std::path::PathBuf;

    fn test_config() -> Config {
        Config {
            provider: "anthropic".into(),
            api_url: "https://api.anthropic.com".into(),
            api_key: "test-key".into(),
            model: "claude-sonnet-4-5".into(),
            max_turns: 50,
            max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
            workdir: PathBuf::from("/tmp"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 8192,
            compact_tool_budget_tokens: 4096,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    fn attached_root_for_main_tests() -> crate::tui::subagents::AttachedRoot {
        crate::tui::subagents::AttachedRoot {
            session_id: "session-1".into(),
            task_id: "task-1".into(),
            capability: "capability-1".into(),
            workspace: yi_agent_core::subagent::worker::WorkerWorkspace {
                lease_id: yi_agent_core::subagent::task::WorkspaceLeaseId::new(),
                repository_root: "/tmp/repo".into(),
                path: "/tmp/repo/.worktrees/root".into(),
                branch: "feat/root".into(),
                parent_branch: "main".into(),
                base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            },
        }
    }

    #[test]
    fn headless_root_tools_include_delegation_only_when_attached() {
        let config = test_config();
        let ordinary = build_headless_setup(&config, false)
            .expect("ordinary setup")
            .tools;
        assert!(ordinary.get("spawn_agent").is_none());

        let root = attached_root_for_main_tests();
        let registry = build_headless_root_tools(&config, "/tmp/runtime.sock".into(), &root)
            .expect("attached headless setup");
        let names = registry
            .tools
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"send_message".to_string()));
        assert!(names.contains(&"wait_agent".to_string()));
        assert!(!names.contains(&"accept_review".to_string()));
    }

    /// A bring-up failure must come back as a reportable session, never as an
    /// `Err` the driver renders as `Error: ...`.
    ///
    /// The reported symptom was an English internal string where a Chinese
    /// notice belonged. `attach_tui_runtime` signals every ordinary bring-up
    /// failure (including a runtime directory the daemon cannot create) by
    /// returning `Unavailable`, so the driver has something to show the user.
    /// Only the `?` sites inside still raise, and those are configuration
    /// failures that happen before the daemon is involved.
    #[test]
    fn a_failed_runtime_bring_up_is_reported_rather_than_raised() {
        // `attach_tui_runtime` only reads the non-subcommand fields, so the
        // default command line is enough to exercise bring-up.
        let cli = <Cli as clap::Parser>::parse_from(["yi-agent"]);
        let mut config = test_config();
        // A path no process can create the runtime directory under, so bring-up
        // fails regardless of the machine's `$TMPDIR` or permissions.
        config.workdir = std::path::PathBuf::from(format!("/{}", "a".repeat(200)));

        match attach_tui_runtime(&cli, &config) {
            Ok(Some(TuiRuntimeSession::Unavailable { reason })) => {
                assert!(
                    !reason.trim().is_empty(),
                    "an unavailable runtime must explain itself"
                );
            }
            Ok(Some(TuiRuntimeSession::Attached(_))) => {
                panic!("bring-up was expected to fail for an uncreatable workdir")
            }
            Ok(None) => panic!("the TUI must always get a session, attached or not"),
            Err(error) => {
                panic!("a failed bring-up escaped as `Error: ...` instead of a notice: {error}")
            }
        }
    }

    /// A runtime that cannot start must say so. Silently continuing left the
    /// user with no visible reason why `spawn_agent` was missing from the tool
    /// list, which is exactly how the long-socket-path failure hid for days.
    #[test]
    fn a_failed_runtime_attach_reports_an_actionable_reason() {
        let reason =
            runtime_unavailable_reason(&yi_agent_store::ipc::IpcError::SocketPathTooLong {
                path: "/very/long/path/runtime.sock".into(),
                limit: yi_agent_store::ipc::MAX_SOCKET_PATH_BYTES,
            });

        assert!(
            reason.contains("socket"),
            "the reason must name the failing mechanism, got: {reason}"
        );
        assert!(
            reason.contains("YI_AGENT_RUNTIME_DIR"),
            "the reason must offer an actionable remedy, got: {reason}"
        );
    }

    #[test]
    fn build_tui_root_tools_registers_subagent_tools_for_attached_runtime() {
        let config = test_config();
        let root = attached_root_for_main_tests();
        let registry = build_tui_root_tools(
            &yi_agent_core::ToolRegistry::new(),
            &config,
            "/tmp/runtime.sock".into(),
            &root,
        );
        let names = registry
            .schemas()
            .into_iter()
            .map(|schema| schema.name)
            .collect::<Vec<_>>();

        assert!(names.contains(&"spawn_agent".to_string()));
        assert!(names.contains(&"send_message".to_string()));
        assert!(names.contains(&"wait_agent".to_string()));
        assert!(names.contains(&"bash".to_string()));
    }

    #[test]
    fn build_headless_setup_naked_has_no_tools_and_no_system_prompt() {
        let config = test_config();
        let setup = build_headless_setup(&config, true).expect("setup should succeed");
        assert!(
            setup.tools.schemas().is_empty(),
            "naked mode should register zero tools, got: {:?}",
            setup
                .tools
                .schemas()
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            setup.system_prompt.is_none(),
            "naked mode should pass None as system_prompt, got: {:?}",
            setup.system_prompt
        );
    }

    #[test]
    fn build_headless_setup_default_registers_builtin_tools() {
        let config = test_config();
        let setup = build_headless_setup(&config, false).expect("setup should succeed");
        assert!(
            !setup.tools.schemas().is_empty(),
            "default mode should register builtin tools, got empty set"
        );
        let names: Vec<String> = setup
            .tools
            .schemas()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        // 至少应该有 read/write/bash 这几个核心工具
        assert!(
            names.iter().any(|n| n == "read"),
            "default mode should register 'read' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "write"),
            "default mode should register 'write' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "bash"),
            "default mode should register 'bash' tool, got: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "process_start"),
            "non-naked headless setup must register process tools, got: {names:?}"
        );
    }

    #[test]
    fn build_headless_setup_default_includes_current_date_in_system_prompt() {
        let config = test_config();
        let setup = build_headless_setup(&config, false).expect("setup should succeed");
        let sp = setup
            .system_prompt
            .as_ref()
            .expect("default mode should produce a system prompt");
        assert!(
            sp.contains("Current date:"),
            "default system_prompt should contain current date marker, got: {sp}"
        );
    }

    // Sync wrapper around `drain_stream_human` so tests can drive the async
    // stream without spinning up a multi-thread runtime.
    /// Drive the production drain path with no agent attached: the signal
    /// branch is inert (no real signal is delivered in a unit test), so these
    /// assertions pin the event rendering that the human/JSON formats guarantee.
    #[test]
    fn drain_stream_json_emits_one_line_per_event_and_reports_cancelled() {
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hi".into()),
            AgentEvent::Cancelled,
        ]);
        let mut out = Vec::new();
        let code = drain_stream_json_sync(stream, &mut out);
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 2, "one JSONL line per event, got {lines:?}");
        assert!(lines[0].contains("hi"), "first line: {}", lines[0]);
        assert_eq!(code, 130, "a cancelled --json run still reports 130");
    }

    /// The reclaim line belongs to whoever owns the terminal. `daemon serve`
    /// and a headless run have a real stderr, so the sweep must still announce
    /// itself there.
    #[test]
    fn reclaim_report_names_the_count_on_a_plain_stream() {
        let mut out = Vec::new();
        report_reclaimed_orphans(&mut out, 3);
        assert_eq!(
            String::from_utf8(out).unwrap().trim(),
            "yi-agent runtime: reclaimed 3 orphaned task(s)"
        );
    }

    /// A clean start must stay silent: a `0` line is pure noise.
    #[test]
    fn reclaim_report_stays_silent_when_nothing_was_reclaimed() {
        let mut out = Vec::new();
        report_reclaimed_orphans(&mut out, 0);
        assert!(out.is_empty(), "a zero count must not print: {out:?}");
    }

    fn drain_stream_human_sync<W: std::io::Write, E: std::io::Write>(
        stream: BoxStream<'static, AgentEvent>,
        out: &mut W,
        err: &mut E,
    ) -> i32 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let agent = test_agent();
        rt.block_on(drain_with_signals(stream, out, err, false, &agent))
    }

    fn drain_stream_json_sync<W: std::io::Write>(
        stream: BoxStream<'static, AgentEvent>,
        out: &mut W,
    ) -> i32 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        let agent = test_agent();
        let mut err = Vec::new();
        rt.block_on(drain_with_signals(stream, out, &mut err, true, &agent))
    }

    /// A real `Agent` over a provider that is never called. `drain_with_signals`
    /// only ever uses it to cancel, and no prompt is run.
    fn test_agent() -> yi_agent_core::Agent {
        struct NeverCalled;
        #[async_trait::async_trait]
        impl yi_agent_core::Provider for NeverCalled {
            async fn call_stream(
                &self,
                _req: yi_agent_core::ProviderRequest,
            ) -> Result<
                BoxStream<'static, yi_agent_core::ProviderEvent>,
                yi_agent_core::ProviderError,
            > {
                Ok(futures::stream::iter(Vec::new()).boxed())
            }
        }
        yi_agent_core::Agent::new(
            Arc::new(NeverCalled),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            yi_agent_core::AgentConfig::default(),
        )
    }

    #[test]
    fn drain_stream_human_concatenates_text_deltas_without_extra_newlines() {
        // Bug regression: each AssistantText delta should NOT be on its own
        // line. Three chunks "chunk1" "chunk2" "chunk3" must produce
        // "chunk1chunk2chunk3\n" (one trailing newline), not
        // "chunk1\nchunk2\nchunk3\n".
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("chunk1".into()),
            AgentEvent::AssistantText("chunk2".into()),
            AgentEvent::AssistantText("chunk3".into()),
            AgentEvent::Done {
                reason: DoneReason::MaxTurns,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0, "exit code should be 0 for Done::MaxTurns");
        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "chunk1chunk2chunk3\n",
            "AssistantText deltas should concatenate without per-chunk newlines"
        );
        assert!(
            String::from_utf8(err).unwrap().contains("[done:MaxTurns]"),
            "MaxTurns is an abnormal non-error termination and should be reported on stderr"
        );
    }

    #[test]
    fn drain_stream_human_suppresses_done_endturn_on_stderr() {
        // Bug regression: normal completion (EndTurn) is already signaled by
        // exit code 0. The [done:EndTurn] line is noise on stderr and should
        // be suppressed, matching the TUI which renders EndTurn as a silent
        // separator. Only abnormal terminations (MaxTurns/Cancelled/Error)
        // emit diagnostic lines to stderr.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0, "exit code should be 0 for Done::EndTurn");
        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(stdout, "hello\n");
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            !stderr.contains("[done:"),
            "EndTurn must not emit [done:EndTurn] on stderr; got: {stderr:?}"
        );
    }

    #[test]
    fn drain_stream_human_reports_provider_retry_on_stderr() {
        let stream = futures::stream::iter(vec![
            AgentEvent::AssistantText("partial".into()),
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 60,
                cause: yi_agent_core::RetryCause::IdleStall,
            },
            AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        ])
        .boxed();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0);
        let err_text = String::from_utf8(err).unwrap();
        assert!(
            err_text.contains("[provider-retry:1/3]"),
            "the retry must be announced on stderr, got: {err_text}"
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "partial\n",
            "stdout must carry only assistant text"
        );
        // A stall line keeps its historical shape (no suffix) for scrapers.
        assert!(
            !err_text.contains("timeout]"),
            "an idle stall must not be labelled a timeout, got: {err_text}"
        );
    }

    #[test]
    fn drain_stream_human_marks_a_timeout_retry_on_stderr() {
        let stream = futures::stream::iter(vec![
            AgentEvent::AssistantText("partial".into()),
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 0,
                cause: yi_agent_core::RetryCause::RequestTimeout,
            },
            AgentEvent::Done {
                reason: yi_agent_core::DoneReason::EndTurn,
            },
        ])
        .boxed();
        let mut out: Vec<u8> = Vec::new();
        let mut err: Vec<u8> = Vec::new();

        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 0);
        let err_text = String::from_utf8(err).unwrap();
        // The suffix distinguishes a deadline from a stall without changing the
        // stall line's existing format.
        assert!(
            err_text.contains("[provider-retry:1/3 timeout]"),
            "a timeout retry must be labelled on stderr, got: {err_text}"
        );
    }

    #[test]
    fn drain_stream_human_preserves_embedded_newlines_in_text() {
        // The LLM's own newlines inside AssistantText must be preserved.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("line one\n".into()),
            AgentEvent::AssistantText("line two\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "line one\nline two\n",
            "embedded newlines preserved, no extra trailing newline added"
        );
    }

    #[test]
    fn drain_stream_human_adds_trailing_newline_only_when_missing() {
        // If the final AssistantText does NOT end with '\n', drain_stream
        // should add exactly one so the shell prompt starts on a fresh line.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(stdout, "hello\n", "exactly one trailing newline added");
    }

    #[test]
    fn drain_stream_human_no_trailing_newline_when_text_already_ends_with_newline() {
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("hello\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        assert_eq!(
            stdout, "hello\n",
            "no duplicate trailing newline when text already ends with \\n"
        );
    }

    #[test]
    fn drain_stream_human_routes_tool_events_to_err_not_out() {
        // ToolCall and ToolResult must go to stderr, not stdout, so they
        // don't pollute the assistant text stream.
        let stream = scripted_stream(vec![
            AgentEvent::AssistantText("let me run ".into()),
            AgentEvent::AssistantText("a command\n".into()),
            AgentEvent::ToolCall {
                id: "tool_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "echo hi"}),
            },
            AgentEvent::ToolResult {
                id: "tool_1".into(),
                result: yi_agent_core::ToolResult::text("hi\n"),
            },
            AgentEvent::AssistantText("done\n".into()),
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
        ]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let _ = drain_stream_human_sync(stream, &mut out, &mut err);

        let stdout = String::from_utf8(out).unwrap();
        let stderr = String::from_utf8(err).unwrap();
        assert_eq!(
            stdout, "let me run a command\ndone\n",
            "stdout should contain only assistant text, concatenated"
        );
        assert!(
            stderr.contains("[tool:bash]"),
            "stderr should contain tool call: {stderr}"
        );
        assert!(
            stderr.contains("[result:tool_1]"),
            "stderr should contain tool result: {stderr}"
        );
    }

    #[test]
    fn drain_stream_human_cancelled_returns_exit_130() {
        let stream = scripted_stream(vec![AgentEvent::Cancelled]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 130, "Cancelled should produce exit code 130");
    }

    #[test]
    fn drain_stream_human_error_returns_exit_1() {
        let stream = scripted_stream(vec![AgentEvent::Error(
            yi_agent_core::AgentError::Provider(yi_agent_core::ProviderError::Network(
                "boom".into(),
            )),
        )]);

        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = drain_stream_human_sync(stream, &mut out, &mut err);

        assert_eq!(code, 1, "Error should produce exit code 1");
        let stderr = String::from_utf8(err).unwrap();
        assert!(
            stderr.contains("[error:"),
            "stderr should contain error: {stderr}"
        );
    }

    #[test]
    fn attach_application_root_request_uses_the_configured_workdir() {
        let yi_agent_store::ipc::IpcRequest::AttachApplicationRoot {
            idempotency_key,
            workspace,
        } = attach_application_root_request(
            "test-key".into(),
            std::path::PathBuf::from("/projects/b"),
        )
        else {
            panic!("expected an application-root attach request");
        };
        assert_eq!(idempotency_key, "test-key");
        assert_eq!(workspace, std::path::PathBuf::from("/projects/b"));
    }

    #[test]
    fn task_event_line_uses_the_stable_event_wire_fields() {
        let event = yi_agent_store::ipc::IpcEvent {
            event_id: 42,
            task_id: "task-1".into(),
            kind: "task_progress".into(),
            payload_json: r#"{"done":true}"#.into(),
        };

        assert_eq!(task_event_line(&event), "42 task_progress {\"done\":true}");
    }

    #[test]
    fn attach_rejection_reason_surfaces_the_daemon_message_not_a_debug_dump() {
        let response = yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::InvalidState,
            message: Some("worker startup failed: Git workspace error: parent worktree is dirty: /tmp/p — a coding root needs a clean checkout; commit or stash the changes first".into()),
        };

        let reason = runtime_attached_root_rejection(&response);

        assert!(
            reason.contains("parent worktree is dirty"),
            "the user must see the real cause, got: {reason}"
        );
        assert!(
            reason.contains("commit or stash"),
            "the reason must offer an actionable remedy, got: {reason}"
        );
        assert!(
            !reason.contains("Some("),
            "the reason must not leak a Debug dump, got: {reason}"
        );
    }

    #[test]
    fn attach_rejection_reason_degrades_gracefully_without_a_message() {
        let response = yi_agent_store::ipc::IpcResponse::Error {
            code: yi_agent_store::ipc::IpcErrorCode::Internal,
            message: None,
        };

        let reason = runtime_attached_root_rejection(&response);

        assert!(reason.contains("internal"), "got: {reason}");
        assert!(reason.contains("delegation is disabled"), "got: {reason}");
    }

    #[test]
    fn ignoring_project_local_runtime_state_keeps_a_clean_checkout_usable() {
        use std::process::Command;

        let directory = tempfile::TempDir::new().unwrap();
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(directory.path())
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?} failed"
            );
        };
        run(&["init", "-b", "main"]);
        run(&["config", "user.email", "tests@example.com"]);
        run(&["config", "user.name", "Tests"]);
        std::fs::write(directory.path().join("README.md"), "base\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "base"]);
        let porcelain = || {
            let output = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(directory.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(porcelain(), "");

        // The daemon hook runs before the state dir exists.
        ignore_project_local_runtime_state(directory.path());

        assert_eq!(
            porcelain(),
            "",
            "the hook alone must not dirty the checkout"
        );

        // Once the daemon creates its store, the checkout must stay clean.
        std::fs::create_dir_all(directory.path().join(".yi-agent/runtime")).unwrap();
        std::fs::write(
            directory.path().join(".yi-agent/runtime/runtime.sqlite"),
            "state",
        )
        .unwrap();
        std::fs::create_dir_all(directory.path().join(".yi-agent/threads")).unwrap();
        std::fs::write(
            directory.path().join(".yi-agent/threads/thread-1.jsonl"),
            "log",
        )
        .unwrap();

        assert_eq!(
            porcelain(),
            "",
            "the daemon's own state must never dirty the checkout"
        );
    }

    #[test]
    fn ignoring_project_local_runtime_state_is_a_no_op_outside_git() {
        let directory = tempfile::TempDir::new().unwrap();

        // Must not panic or create stray files in a non-git workdir.
        ignore_project_local_runtime_state(directory.path());

        assert!(!directory.path().join(".git").exists());
    }

    /// The user pressed `y` and the runtime did not come up. That outcome must
    /// reach the session as a notice, never as `Error`: an error reads as a
    /// failed turn, and the code this replaced sent `ProviderTurnAdmission`,
    /// which rendered the raw `IpcError` text as `Error: provider turn
    /// admission failed: ...` and never mentioned the restart that fixes it.
    ///
    /// This pins the classification. Reverting the emit site to the error path
    /// used to leave every test green, so the regression could only be caught
    /// by hand-driving a TUI.
    #[test]
    fn a_runtime_bring_up_failure_becomes_a_notice_not_an_error() {
        let event = runtime_unavailable_event("runtime attach", "file is not a database");
        match event {
            yi_agent_core::AgentEvent::SubagentRuntimeUnavailable { stage, cause } => {
                assert_eq!(stage, "runtime attach");
                assert_eq!(cause, "file is not a database");
            }
            other => panic!("bring-up failure must be a notice, not a turn error: {other:?}"),
        }
    }

    /// The activation step reports the same remedy, so it takes the same
    /// notice path with its own stage label.
    #[test]
    fn a_runtime_activation_failure_takes_the_same_notice_path() {
        let event = runtime_unavailable_event("runtime activation", "daemon rejected: internal");
        match event {
            yi_agent_core::AgentEvent::SubagentRuntimeUnavailable { stage, .. } => {
                assert_eq!(stage, "runtime activation");
            }
            other => panic!("activation failure must be a notice: {other:?}"),
        }
    }

    #[test]
    fn preference_maps_to_startup_intent() {
        use crate::tui::runtime_prefs::{RuntimePreference, save};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();

        save(dir.path(), RuntimePreference::Ask).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));

        save(dir.path(), RuntimePreference::Always).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::AutoStart)
        ));

        save(dir.path(), RuntimePreference::Never).unwrap();
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::DisabledNotice { .. })
        ));
    }

    #[test]
    fn never_notice_is_chinese_and_points_at_the_command() {
        use crate::tui::runtime_prefs::{RuntimePreference, save};
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), RuntimePreference::Never).unwrap();
        let Some(RuntimeStartupIntent::DisabledNotice { reason }) = startup_intent_for(dir.path())
        else {
            panic!("never must produce a disabled notice");
        };
        assert!(reason.contains("/runtime"), "reason: {reason}");
        assert!(reason.contains("已禁用"), "reason: {reason}");
    }

    #[test]
    fn reading_the_intent_does_not_create_the_preference_directory() {
        use crate::tui::runtime_prefs::preferences_path;
        use crate::tui::subagents::RuntimeStartupIntent;

        let dir = tempfile::TempDir::new().unwrap();
        // A project that never opted in has no `.yi-agent/`. Deriving the
        // startup intent must stay read-only: creating the directory would
        // dirty every project the user merely launched the TUI in.
        assert!(matches!(
            startup_intent_for(dir.path()),
            Some(RuntimeStartupIntent::Prompt)
        ));
        assert!(
            !preferences_path(dir.path()).exists(),
            "deriving the startup intent must not create .yi-agent/"
        );
    }
    // --- a wedged daemon must not survive a restart ---

    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    /// A stand-in for the daemon that wedged in the field: a live listener that
    /// answers ordinary requests with a bare `internal`, but whose `Stop` path
    /// still works (the real one's did -- `graceful_stop` uses in-memory state,
    /// while every other request opens a fresh repository connection first).
    ///
    /// It never touches a database, so the only thing under test is the client's
    /// decision to retire a wedged daemon and start a fresh one.
    struct InternalErrorDaemon {
        socket: PathBuf,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        non_status_requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        listener: Option<std::thread::JoinHandle<()>>,
    }

    impl InternalErrorDaemon {
        fn start(socket: &std::path::Path) -> Self {
            let listener = UnixListener::bind(socket).expect("probe socket binds");
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let thread_stop = std::sync::Arc::clone(&stop);
            let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let thread_requests = std::sync::Arc::clone(&requests);
            let non_status_requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let thread_non_status = std::sync::Arc::clone(&non_status_requests);
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { break };
                    let _ = answer_like_a_wedged_daemon(
                        stream,
                        &thread_stop,
                        &thread_requests,
                        &thread_non_status,
                    );
                }
            });
            Self {
                socket: socket.to_path_buf(),
                stop,
                requests,
                non_status_requests,
                listener: Some(handle),
            }
        }

        fn socket_path(&self) -> &std::path::Path {
            &self.socket
        }

        /// How many requests reached the probe, and how many asked for anything
        /// other than a read-only `Status`. The wedge fix must never write.
        fn request_counts(&self) -> (usize, usize) {
            (
                self.requests.load(std::sync::atomic::Ordering::Acquire),
                self.non_status_requests
                    .load(std::sync::atomic::Ordering::Acquire),
            )
        }
    }

    impl Drop for InternalErrorDaemon {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            // Wake the accept loop so shutdown does not wait for its sleep.
            let _ = UnixStream::connect(&self.socket);
            if let Some(handle) = self.listener.take() {
                let _ = handle.join();
            }
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    /// A bare `internal` for everything except `Stop`, which stops the probe --
    /// exactly how the field daemon behaved.
    fn answer_like_a_wedged_daemon(
        mut stream: UnixStream,
        stop: &std::sync::atomic::AtomicBool,
        requests: &std::sync::atomic::AtomicUsize,
        non_status_requests: &std::sync::atomic::AtomicUsize,
    ) -> std::io::Result<()> {
        stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
        let request_id = request
            .get("request_id")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        let kind = request
            .get("command")
            .and_then(|command| command.get("type"))
            .and_then(|kind| kind.as_str())
            .unwrap_or_default();
        requests.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if kind != "Status" {
            non_status_requests.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        let result = if kind == "Stop" {
            stop.store(true, std::sync::atomic::Ordering::Release);
            serde_json::json!({"type": "Stopping"})
        } else {
            serde_json::json!({"type": "Error", "code": "internal"})
        };
        let body = serde_json::json!({
            // Speak the version the real client accepts: a stale literal would
            // make the response a protocol mismatch instead of an `internal`
            // answer, and these tests are about the latter.
            "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
            "request_id": request_id,
            "result": result,
        });
        writeln!(stream, "{body}")?;
        stream.flush()
    }

    /// A daemon that answers `internal` is *unhealthy*, but it still looks
    /// "already running". Adopting it is exactly the field bug: the user
    /// restarts yi-agent, and the restart reconnects to the same wedged daemon
    /// and reports the same failure forever.
    #[test]
    fn an_existing_daemon_that_answers_internal_is_retired_not_adopted() {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = dir.path().join(".yi-agent/runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let probe = InternalErrorDaemon::start(&runtime_dir.join("runtime.sock"));

        assert!(
            replace_wedged_daemon(probe.socket_path()),
            "an `internal` Status answer must retire the daemon, not adopt it"
        );
        let (requests, non_status) = probe.request_counts();
        assert!(
            requests >= 2,
            "retirement must probe and then stop, saw {requests} request(s)"
        );
        assert_eq!(
            non_status, 1,
            "the only non-Status request may be the `Stop`; a health probe must not write"
        );
    }

    /// The healthy case must stay cheap: one probe round trip and no stop.
    #[test]
    fn a_daemon_that_answers_status_is_not_retired() {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = dir.path().join(".yi-agent/runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let database = runtime_dir.join("runtime.sqlite");
        let daemon =
            yi_agent_store::ipc::Daemon::start(&runtime_dir, &database).expect("daemon starts");
        let socket = daemon.socket_path().to_path_buf();

        assert!(
            !replace_wedged_daemon(&socket),
            "a healthy daemon must never be retired"
        );
        assert!(
            yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Status)
                .is_ok(),
            "a healthy daemon must remain reachable"
        );
    }

    /// The recovery the notice promises -- "restart yi-agent" -- only works if a
    /// wedged daemon is actually stopped and replaced. This drives the real
    /// retirement path, then proves the next launch serves the request that used
    /// to fail.
    #[test]
    fn retiring_a_wedged_daemon_lets_the_next_launch_start_a_working_runtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let runtime_dir = dir.path().join(".yi-agent/runtime");
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let database = runtime_dir.join("runtime.sqlite");
        let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();

        let probe = InternalErrorDaemon::start(&socket);
        assert!(
            replace_wedged_daemon(&socket),
            "the run must retire the wedged daemon"
        );
        // The retirement itself sends `Stop`; the probe only exits its accept
        // loop once it sees it, so a bug that skips the stop shows up here.
        assert!(
            probe.stop.load(std::sync::atomic::Ordering::Acquire),
            "the wedged daemon must actually receive `Stop`"
        );
        drop(probe);

        let daemon =
            yi_agent_store::ipc::Daemon::start(&runtime_dir, &database).expect("fresh daemon");
        let response =
            yi_agent_store::ipc::send_request(&socket, yi_agent_store::ipc::IpcRequest::Status)
                .expect("the replacement daemon answers");
        assert!(
            matches!(response, yi_agent_store::ipc::IpcResponse::Status { .. }),
            "a restarted runtime must delegate again, got {response:?}"
        );
        drop(daemon);
    }
}
