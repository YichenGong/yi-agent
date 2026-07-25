//! yi-agent CLI 入口。

mod app;
mod compact;
mod config;
mod file_ref;
mod input;
mod llm_prefix;
mod render;
mod tracing_init;
mod tui;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use render::InlineRenderer;
use yi_agent_core::Provider;

use crate::app::App;
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

    // Branch on --tui flag
    match select_tui_mode(&cli) {
        TuiMode::Ratatui => {
            return run_tui_agent(
                provider,
                tools,
                agent_config,
                config.workdir.clone(),
                checker,
                decision_tx,
                decision_rx,
            );
        }
        TuiMode::Inline => {
            // Inline mode: wire permission checker into agent
            let decision_rx = Arc::new(tokio::sync::Mutex::new(decision_rx));
            let agent = yi_agent_core::Agent::new(
                Arc::clone(&provider),
                Arc::clone(&tools),
                agent_config.clone(),
            )
            .with_permission(Arc::clone(&checker), Arc::clone(&decision_rx));

            let printer = reedline::ExternalPrinter::default();
            let renderer = Box::new(InlineRenderer::with_printer(printer.sender()));

            let app = App::new(
                agent,
                provider,
                tools,
                agent_config,
                config.workdir.clone(),
                renderer,
                decision_tx,
                checker,
                decision_rx,
            );

            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(app.run(printer))?;
        }
    }

    Ok(())
}

/// Which TUI mode to use, based on the `--tui` CLI flag.
/// Default (no flag) is Ratatui. `--tui inline` selects the old InlineRenderer.
fn select_tui_mode(cli: &Cli) -> TuiMode {
    match cli.tui.as_deref() {
        Some("inline") => TuiMode::Inline,
        _ => TuiMode::Ratatui,
    }
}

enum TuiMode {
    Ratatui,
    Inline,
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
                            let keep_turns = rebuild_config
                                .compact_keep_turns
                                .unwrap_or(4);
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
                                    let _ = agent_tx
                                        .send(yi_agent_core::AgentEvent::Error(e))
                                        .await;
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
    use crate::config::Cli;

    #[test]
    fn default_tui_mode_is_ratatui() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            tui: None,
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        assert!(matches!(select_tui_mode(&cli), TuiMode::Ratatui));
    }

    #[test]
    fn tui_inline_selects_inline() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            tui: Some("inline".into()),
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        assert!(matches!(select_tui_mode(&cli), TuiMode::Inline));
    }

    #[test]
    fn tui_ratatui_selects_ratatui() {
        let cli = Cli {
            command: None,
            provider: None,
            api_url: None,
            api_key: None,
            model: None,
            max_turns: None,
            workdir: None,
            system_prompt: None,
            model_context_length: None,
            compact_ratio: None,
            compact_keep_turns: None,
            tui: Some("ratatui".into()),
            yolo: false,
            skip_permissions: false,
            skills_catalog_budget: None,
            debug: false,
        };
        assert!(matches!(select_tui_mode(&cli), TuiMode::Ratatui));
    }

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
}
