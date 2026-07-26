//! yi-agent CLI 入口。

mod compact;
mod config;
mod llm_prefix;
mod tracing_init;
mod tui;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use yi_agent_core::Provider;

use crate::config::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let _trace_guard = tracing_init::init(cli.debug);

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
        }) => {
            let prompt = prompt.clone();
            run_headless(cli, prompt, json, stdin, naked)
        }
        None => run_agent(cli),
    }
}

fn run_agent(cli: Cli) -> Result<()> {
    let config = config::load(&cli)?;

    // Load permissions and construct checker
    let workdir = config.workdir.clone();
    let yolo = config.yolo;
    let permissions = {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(yi_agent_core::permission::PermissionChecker::load(&workdir))
            .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?
    };
    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        permissions,
        yolo,
        workdir,
        blocklist_fn,
    ));
    let (decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);

    let provider: Arc<dyn Provider> = match config.provider.as_str() {
        "anthropic" => Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        "openai" => Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    };

    let mut registry = yi_agent_core::ToolRegistry::new();
    yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());

    // --- Skills system setup ---
    let skills_service = setup_skills(&config)?;

    let system_prompt = resolve_system_prompt_with_skills(
        config.system_prompt.clone(),
        &skills_service,
        config.skills_catalog_budget,
        config.skills_catalog_budget_explicit,
    );

    // Register Skill tool
    if let Some(svc) = &skills_service {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    let tools = Arc::new(registry);

    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt,
        max_turns: Some(config.max_turns),
        compact_threshold: Some(config.compact_threshold),
        compact_keep_turns: Some(config.compact_keep_turns),
        ..Default::default()
    };

    run_tui_agent(
        provider,
        tools,
        agent_config,
        config.workdir.clone(),
        checker,
        decision_tx,
        decision_rx,
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
async fn drain_stream_human<W: std::io::Write, E: std::io::Write>(
    stream: futures::stream::BoxStream<'static, yi_agent_core::AgentEvent>,
    out: &mut W,
    err: &mut E,
) -> i32 {
    use futures::StreamExt;

    let mut stream = Box::pin(stream);
    let mut exit_code = 0;
    // True when the last bytes written to `out` did NOT end with '\n'.
    // Used to ensure we terminate assistant text before returning so the
    // shell prompt starts on a fresh line.
    let mut mid_line = false;

    while let Some(event) = stream.next().await {
        match &event {
            yi_agent_core::AgentEvent::AssistantText(t) => {
                let _ = out.write_all(t.as_bytes());
                mid_line = !t.ends_with('\n');
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
            yi_agent_core::AgentEvent::Done { reason } => match reason {
                // Normal completion is already signaled by exit code 0; the
                // [done:EndTurn] line is noise on stderr and is suppressed
                // to match the TUI, which renders EndTurn as a silent
                // separator. Only abnormal non-error terminations emit a
                // diagnostic line.
                yi_agent_core::DoneReason::EndTurn => {}
                yi_agent_core::DoneReason::MaxTurns => {
                    let _ = writeln!(err, "[done:{reason:?}]");
                }
            },
            yi_agent_core::AgentEvent::Cancelled => {
                let _ = writeln!(err, "[cancelled]");
                exit_code = 130;
            }
            yi_agent_core::AgentEvent::Error(e) => {
                let _ = writeln!(err, "[error:{e}]");
                exit_code = 1;
            }
            _ => {}
        }
        if matches!(
            event,
            yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Cancelled
                | yi_agent_core::AgentEvent::Error(_)
        ) {
            break;
        }
    }

    if mid_line {
        let _ = out.write_all(b"\n");
    }

    exit_code
}

/// Drain an `AgentEvent` stream to the provided writer as JSONL (one JSON
/// object per line). Returns the process exit code.
async fn drain_stream_json<W: std::io::Write>(
    stream: futures::stream::BoxStream<'static, yi_agent_core::AgentEvent>,
    out: &mut W,
) -> i32 {
    use futures::StreamExt;

    let mut stream = Box::pin(stream);
    let exit_code = 0;
    while let Some(event) = stream.next().await {
        let line = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
        let _ = writeln!(out, "{line}");
        if matches!(
            event,
            yi_agent_core::AgentEvent::Done { .. }
                | yi_agent_core::AgentEvent::Cancelled
                | yi_agent_core::AgentEvent::Error(_)
        ) {
            break;
        }
    }
    exit_code
}

/// Headless 模式的工具 + system prompt 构建结果。
struct HeadlessSetup {
    tools: Arc<yi_agent_core::ToolRegistry>,
    system_prompt: Option<String>,
}

/// 根据 `naked` flag 构建 headless 模式用的工具集和 system prompt。
///
/// `naked = true`:不注册任何工具,不加载 skills,`system_prompt = None`(裸模型)。
/// `naked = false`:与 TUI `run_agent` 对齐 — 注册内置工具、加载 skills、
/// 注册 SkillTool、用 `resolve_system_prompt_with_skills` 拼接默认 prompt +
/// 当前日期 + skills catalog。
fn build_headless_setup(config: &config::Config, naked: bool) -> Result<HeadlessSetup> {
    let mut registry = yi_agent_core::ToolRegistry::new();

    if naked {
        return Ok(HeadlessSetup {
            tools: Arc::new(registry),
            system_prompt: None,
        });
    }

    yi_agent_tools::register_builtin_tools(&mut registry, config.workdir.clone());
    let skills_service = setup_skills(config)?;
    let system_prompt = resolve_system_prompt_with_skills(
        config.system_prompt.clone(),
        &skills_service,
        config.skills_catalog_budget,
        config.skills_catalog_budget_explicit,
    );
    if let Some(svc) = &skills_service {
        registry.register(Arc::new(yi_agent_tools::SkillTool::new(svc.clone())));
    }

    Ok(HeadlessSetup {
        tools: Arc::new(registry),
        system_prompt,
    })
}

/// Run agent non-interactively: drain AgentEvent stream to stdout/stderr.
/// Used for headless CLI usage and end-to-end real-LLM testing.
fn run_headless(
    cli: Cli,
    prompt: Option<String>,
    json: bool,
    from_stdin: bool,
    naked: bool,
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

    let workdir = config.workdir.clone();
    // Headless mode: auto-allow non-blacklisted tools (yolo behavior)
    let yolo = true;
    let permissions = {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(yi_agent_core::permission::PermissionChecker::load(&workdir))
            .map_err(|e| anyhow::anyhow!("failed to load permissions: {e}"))?
    };
    let blocklist_fn: yi_agent_core::permission::BlocklistFn =
        Arc::new(|cmd: &str| yi_agent_tools::blocklist::is_blocked(cmd).map(|s| s.to_string()));
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        permissions,
        yolo,
        workdir.clone(),
        blocklist_fn,
    ));
    let (_decision_tx, decision_rx) =
        tokio::sync::mpsc::channel::<(u64, yi_agent_core::permission::Decision)>(16);

    let provider: Arc<dyn Provider> = match config.provider.as_str() {
        "anthropic" => Arc::new(yi_agent_llm::AnthropicProvider::new(
            yi_agent_llm::AnthropicProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        "openai" => Arc::new(yi_agent_llm::OpenaiProvider::new(
            yi_agent_llm::OpenaiProviderOpts {
                base_url: Some(config.api_url.clone()),
                api_key: Some(config.api_key.clone()),
                ..Default::default()
            },
        )?),
        other => anyhow::bail!(
            "unknown provider '{}': expected 'anthropic' or 'openai'",
            other
        ),
    };

    let setup = build_headless_setup(&config, naked)?;
    let tools = setup.tools;

    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt: setup.system_prompt,
        max_turns: Some(config.max_turns),
        ..Default::default()
    };

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
        if json {
            drain_stream_json(stream, &mut out).await
        } else {
            drain_stream_human(stream, &mut out, &mut err).await
        }
    });

    std::process::exit(exit_code);
}

/// Run the ratatui TUI. Sets up channels, spawns agent driver task, calls run_tui.
fn run_tui_agent(
    provider: Arc<dyn Provider>,
    tools: Arc<yi_agent_core::ToolRegistry>,
    agent_config: yi_agent_core::AgentConfig,
    workdir: std::path::PathBuf,
    checker: Arc<yi_agent_core::permission::PermissionChecker>,
    decision_tx: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
    decision_rx: tokio::sync::mpsc::Receiver<(u64, yi_agent_core::permission::Decision)>,
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
        let is_running = Arc::new(AtomicBool::new(false));

        // Spawn agent driver task (stays on the async runtime)
        let provider_clone = Arc::clone(&provider);
        let tools_clone = Arc::clone(&tools);
        let config_clone = agent_config.clone();
        let is_running_clone = Arc::clone(&is_running);
        let checker_clone = Arc::clone(&checker);
        let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
        // Keep extra clones for agent rebuild on /clear and /compact.
        let rebuild_provider = Arc::clone(&provider);
        let rebuild_tools = Arc::clone(&tools);
        let rebuild_config = agent_config.clone();
        let rebuild_checker = Arc::clone(&checker);
        let rebuild_decision_rx = Arc::clone(&decision_rx);
        let driver = tokio::spawn(async move {
            let mut agent = yi_agent_core::Agent::new(provider_clone, tools_clone, config_clone)
                .with_permission(checker_clone, decision_rx);
            let _ = workdir; // workdir already passed to tools registration

            loop {
                // Wait for user input or a control command. Control commands
                // take priority (biased) so /clear and /compact are handled
                // even if a prompt is also pending.
                let (prompt_text, control_cmd) = tokio::select! {
                    biased;
                    cmd = control_rx.recv() => (None, cmd),
                    text = input_rx.recv() => (text, None),
                };

                // Handle control commands first (rebuild agent, no prompt run).
                if let Some(cmd) = control_cmd {
                    match cmd {
                        ControlCommand::Clear => {
                            // Rebuild agent with empty session.
                            agent = yi_agent_core::Agent::new(
                                Arc::clone(&rebuild_provider),
                                Arc::clone(&rebuild_tools),
                                rebuild_config.clone(),
                            )
                            .with_session(yi_agent_core::Session::new())
                            .with_permission(
                                Arc::clone(&rebuild_checker),
                                Arc::clone(&rebuild_decision_rx),
                            );
                            tracing::info!("agent session cleared via /clear");
                        }
                        ControlCommand::Compact => {
                            let session = agent.session();
                            let keep_turns = rebuild_config.compact_keep_turns.unwrap_or(4);
                            match crate::compact::compact_session(
                                &rebuild_provider,
                                &rebuild_config,
                                &session,
                                keep_turns,
                            )
                            .await
                            {
                                Ok(new_session) => {
                                    agent = yi_agent_core::Agent::new(
                                        Arc::clone(&rebuild_provider),
                                        Arc::clone(&rebuild_tools),
                                        rebuild_config.clone(),
                                    )
                                    .with_session(new_session)
                                    .with_permission(
                                        Arc::clone(&rebuild_checker),
                                        Arc::clone(&rebuild_decision_rx),
                                    );
                                    tracing::info!("agent session compacted via /compact");
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "compact failed");
                                    let _ =
                                        agent_tx.send(yi_agent_core::AgentEvent::Error(e)).await;
                                }
                            }
                        }
                    }
                    continue;
                }

                // Otherwise, a prompt arrived (or both channels closed).
                let Some(text) = prompt_text else {
                    break;
                };

                // Clear any stale interrupt signal
                let _ = interrupt_rx.try_recv();

                // Run agent
                is_running_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                match agent.run(text).await {
                    Ok(stream) => {
                        let mut stream = Box::pin(stream);
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
                                _ = interrupt_rx.recv() => {
                                    // User pressed Ctrl+C/Esc: cancel agent
                                    agent.cancel();
                                    // Drain remaining events until Cancelled/Done
                                    while let Some(ev) = stream.next().await {
                                        if agent_tx.send(ev).await.is_err() { break; }
                                    }
                                    break;
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
        });

        // Run TUI on a dedicated blocking thread (it uses sync crossterm polling)
        let tui_handle = tokio::task::spawn_blocking(move || {
            crate::tui::app::run_tui(
                agent_rx,
                input_tx,
                interrupt_tx,
                control_tx,
                decision_tx,
                is_running,
                agent_config.model.clone(),
            )
        });

        let result = match tui_handle.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::Error::from(e)),
            Err(e) => Err(anyhow::Error::from(e)),
        };

        // TUI exited; abort the driver task to clean up
        // (driver may still be blocked on input_rx.recv() if agent was idle)
        driver.abort();

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
}

/// Resolve the effective system prompt: fall back to the built-in default
/// when the user did not provide one. The current local date is appended to
/// the end so the model knows today's date; placed at the tail to avoid
/// disrupting the cached prefix of the prompt.
fn resolve_system_prompt(user: Option<String>) -> Option<String> {
    let base = user.unwrap_or_else(yi_agent_core::AgentConfig::default_system_prompt);
    let today = chrono::Local::now().format("%Y-%m-%d");
    Some(format!("{base}\n\nCurrent date: {today}"))
}

/// Set up the skills service: install bundled system skills, build roots, snapshot.
/// Returns None on hard failure (and logs a warning); the agent runs without skills.
fn setup_skills(config: &config::Config) -> Result<Option<Arc<yi_agent_skills::SkillsService>>> {
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("skills: could not determine home directory, skipping");
        return Ok(None);
    };
    let system_root = home.join(".yi-agent/skills/.system");

    // Install bundled skills; failure is non-fatal
    if let Err(e) = yi_agent_skills::install_system_skills(&system_root) {
        tracing::warn!("failed to install bundled skills: {e}");
    }

    let roots = vec![
        (
            config.workdir.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::Project,
        ),
        (
            home.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::User,
        ),
        (
            home.join(".yi-agent/skills/.system"),
            yi_agent_skills::SkillScope::System,
        ),
    ];

    let service = Arc::new(yi_agent_skills::SkillsService::new(roots));
    match service.snapshot() {
        Ok(skills) => {
            tracing::info!("skills: {} discovered", skills.len());
            Ok(Some(service))
        }
        Err(e) => {
            tracing::warn!("skills discovery failed: {e}");
            Ok(None)
        }
    }
}

/// Resolve the effective system prompt, appending the skills catalog if available.
fn resolve_system_prompt_with_skills(
    user: Option<String>,
    service: &Option<Arc<yi_agent_skills::SkillsService>>,
    budget: usize,
    budget_explicit: bool,
) -> Option<String> {
    let base = resolve_system_prompt(user);
    let Some(svc) = service else {
        return base;
    };

    let total = svc.full_catalog_size();
    let effective_budget = resolve_effective_budget(total, budget, budget_explicit);
    let catalog = svc.render_catalog(effective_budget);

    if catalog.is_empty() {
        return base;
    }

    match base {
        Some(p) => Some(format!("{p}\n\n{catalog}")),
        None => Some(catalog),
    }
}

fn resolve_effective_budget(total: usize, default: usize, explicit: bool) -> usize {
    if explicit || total <= default || !is_interactive() {
        return default;
    }
    prompt_catalog_budget(total, default).unwrap_or(default)
}

fn is_interactive() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

fn prompt_catalog_budget(total: usize, default: usize) -> Option<usize> {
    let total_kb = total / 1024;
    let default_kb = default / 1024;
    eprintln!(
        "Skills catalog is {total_kb} KB, exceeds default {default_kb} KB budget.\n\
         Include all skills? [Y/n]"
    );
    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        return None;
    }
    match input.trim().to_lowercase().as_str() {
        "" | "y" | "yes" => Some(total),
        _ => Some(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_system_prompt_none_uses_default() {
        let resolved = resolve_system_prompt(None);
        let default = yi_agent_core::AgentConfig::default_system_prompt();
        // The resolved prompt should start with the default prompt and have
        // the current date appended at the end.
        assert!(
            resolved.as_deref().is_some_and(|r| r.starts_with(&default)),
            "resolved should start with default prompt"
        );
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            resolved.as_deref().is_some_and(|r| r.ends_with(&today)),
            "resolved should end with today's date: {resolved:?}"
        );
    }

    #[test]
    fn resolve_system_prompt_custom_overrides_default() {
        let resolved = resolve_system_prompt(Some("custom".into()));
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert!(
            resolved
                .as_deref()
                .is_some_and(|r| r.starts_with("custom") && r.ends_with(&today)),
            "resolved should start with custom prompt and end with today's date: {resolved:?}"
        );
    }

    #[test]
    fn resolve_effective_budget_explicit_returns_default() {
        // When explicit=true, should return default regardless of total.
        assert_eq!(resolve_effective_budget(100_000, 8192, true), 8192);
        assert_eq!(resolve_effective_budget(0, 8192, true), 8192);
        assert_eq!(resolve_effective_budget(8192, 8192, true), 8192);
    }

    #[test]
    fn resolve_effective_budget_total_under_default_returns_default() {
        // When total <= default, should return default.
        assert_eq!(resolve_effective_budget(4096, 8192, false), 8192);
        assert_eq!(resolve_effective_budget(8192, 8192, false), 8192);
        assert_eq!(resolve_effective_budget(0, 8192, false), 8192);
    }

    #[test]
    fn resolve_effective_budget_non_interactive_returns_default() {
        // Tests run non-interactive (stdin is not a TTY), so even when
        // total > default and explicit=false, should return default without prompting.
        assert_eq!(resolve_effective_budget(100_000, 8192, false), 8192);
    }

    #[test]
    fn resolve_system_prompt_with_skills_no_service_returns_base() {
        // When service is None, should fall back to base via resolve_system_prompt
        // (which appends the current date).
        let resolved = resolve_system_prompt_with_skills(None, &None, 8192, false);
        let expected = resolve_system_prompt(None);
        assert_eq!(resolved, expected);
    }

    #[test]
    fn resolve_system_prompt_with_skills_empty_catalog_returns_base() {
        // When service is Some but catalog is empty (no skills discovered),
        // should return the base prompt unchanged (with current date appended).
        let svc = Arc::new(yi_agent_skills::SkillsService::new(vec![]));
        let expected = resolve_system_prompt(None);
        let resolved = resolve_system_prompt_with_skills(None, &Some(svc), 8192, false);
        assert_eq!(resolved, expected);
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
            workdir: PathBuf::from("/tmp"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_keep_turns: 4,
            yolo: false,
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
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
    fn drain_stream_human_sync<W: std::io::Write, E: std::io::Write>(
        stream: BoxStream<'static, AgentEvent>,
        out: &mut W,
        err: &mut E,
    ) -> i32 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(drain_stream_human(stream, out, err))
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
}
