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
use crate::config::Cli;

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = config::load(&cli)?;

    let provider: Arc<dyn Provider> = Arc::new(yi_agent_llm::AnthropicProvider::new(
        yi_agent_llm::AnthropicProviderOpts {
            base_url: Some(config.api_url.clone()),
            api_key: Some(config.api_key.clone()),
            ..Default::default()
        },
    )?);

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

    let renderer = Box::new(InlineRenderer::new());

    let app = App::new(agent, provider, tools, agent_config, config.workdir.clone(), renderer);

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(app.run())?;

    Ok(())
}
