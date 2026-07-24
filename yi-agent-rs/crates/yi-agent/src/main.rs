//! yi-agent CLI 入口。

mod app;
mod compact;
mod config;
mod file_ref;
mod input;
mod render;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use render::InlineRenderer;
use yi_agent_core::Provider;

use crate::app::App;
use crate::config::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Web { ref host, ref port }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env_path = config::resolve_env_path(&cli);
                yi_agent_web::serve(host, *port, env_path).await
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

    Ok(())
}
