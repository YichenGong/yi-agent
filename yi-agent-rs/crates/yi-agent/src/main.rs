//! yi-agent CLI 入口。

mod app;
mod compact;
mod config;
mod file_ref;
mod input;
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
    let _trace_guard = tracing_init::init();
    let cli = Cli::parse();

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
    let tools = Arc::new(registry);

    let agent_config = yi_agent_core::AgentConfig {
        model: config.model.clone(),
        system_prompt: config.system_prompt.clone(),
        max_turns: Some(config.max_turns),
        compact_threshold: Some(config.compact_threshold),
        compact_keep_turns: Some(config.compact_keep_turns),
        ..Default::default()
    };

    // Branch on --tui flag
    match select_tui_mode(&cli) {
        TuiMode::Ratatui => {
            return run_tui_agent(provider, tools, agent_config, config.workdir.clone());
        }
        TuiMode::Inline => {
            // Default: InlineRenderer + reedline path
            let agent = yi_agent_core::Agent::new(
                Arc::clone(&provider),
                Arc::clone(&tools),
                agent_config.clone(),
            );

            let printer = reedline::ExternalPrinter::default();
            let renderer = Box::new(InlineRenderer::with_printer(printer.sender()));

            let app = App::new(
                agent,
                provider,
                tools,
                agent_config,
                config.workdir.clone(),
                renderer,
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
        let is_running = Arc::new(AtomicBool::new(false));

        // Spawn agent driver task (stays on the async runtime)
        let provider_clone = Arc::clone(&provider);
        let tools_clone = Arc::clone(&tools);
        let config_clone = agent_config.clone();
        let is_running_clone = Arc::clone(&is_running);
        let driver = tokio::spawn(async move {
            let mut agent = yi_agent_core::Agent::new(provider_clone, tools_clone, config_clone);
            let _ = workdir; // workdir already passed to tools registration

            loop {
                // Wait for user input
                let Some(text) = input_rx.recv().await else {
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
            crate::tui::app::run_tui(agent_rx, input_tx, interrupt_tx, is_running)
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
        };
        assert!(matches!(select_tui_mode(&cli), TuiMode::Ratatui));
    }
}
