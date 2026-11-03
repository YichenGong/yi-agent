//! Starts MCP stdio servers through the same owner-bound channel tools use.
//!
//! The MCP crate must not know how a managed process is started, and the tools
//! crate must not know about MCP. This module is the join: it implements the MCP
//! crate's [`StdioLauncher`] seam on top of the tools crate's [`ManagedSpawner`].
//!
//! Why a launcher at all: an MCP server that is spawned directly is a child of
//! whoever called `connect`, unregistered with the guardian. Quitting the frontend
//! then leaves it running -- and a stdio MCP server is a long-lived process by
//! design, so that leak is not the rare case, it is the normal one.

use std::collections::BTreeMap;
use std::sync::Arc;

use yi_agent_mcp::{LaunchedStdio, StdioLauncher};
use yi_agent_tools::{ManagedChild, ManagedSpawner};

/// Launches MCP stdio servers as guardian-owned groups.
pub struct ManagedStdioLauncher {
    spawner: ManagedSpawner,
}

impl ManagedStdioLauncher {
    pub fn new(spawner: ManagedSpawner) -> Arc<Self> {
        Arc::new(Self { spawner })
    }
}

impl StdioLauncher for ManagedStdioLauncher {
    fn launch<'a>(
        &'a self,
        command: &'a str,
        args: &'a [String],
        env: &'a BTreeMap<String, String>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = std::io::Result<LaunchedStdio>> + Send + 'a>,
    > {
        Box::pin(async move {
            // The guardian refuses a bare program name: it would resolve `npx`
            // against the guardian's own PATH, not this session's.
            let executable = yi_agent_tools::resolve_program(command)?;
            let env: Vec<(String, String)> =
                env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            // MCP servers are started in the session's cwd, as a direct spawn did.
            let cwd = std::env::current_dir()?;
            let mut child: ManagedChild = self.spawner.spawn(&executable, args, &env, &cwd).await?;

            // Pipes are taken out and boxed; the child itself rides along as the
            // guard so its group stays owned for as long as the transport lives.
            let stdin = child.stdin.take();
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            Ok(LaunchedStdio {
                stdin: Box::new(stdin.ok_or_else(missing("stdin"))?),
                stdout: Box::new(stdout.ok_or_else(missing("stdout"))?),
                stderr: Box::new(stderr.ok_or_else(missing("stderr"))?),
                guard: Some(Box::new(child)),
            })
        })
    }
}

fn missing(stream: &'static str) -> impl FnOnce() -> std::io::Error {
    move || std::io::Error::other(format!("a managed MCP server came back without {stream}"))
}
