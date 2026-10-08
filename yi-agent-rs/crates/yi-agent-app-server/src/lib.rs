//! yi-agent GUI app-server:JSON-RPC 2.0 over stdio。

pub mod attachments;
pub mod broadcast;
pub(crate) mod card_scheduler;
pub mod device_store;
pub(crate) mod git_diff;
pub mod git_diff_tool;
pub mod model_rpc;
pub mod pair_uri;
pub mod pairing;
pub mod protocol;
pub mod server;
pub mod session;
pub mod settings_store;
pub mod theme_tool;
pub mod thread_store;
pub mod translate;
pub mod transport;
pub mod workspace_index;
pub(crate) mod worktree_reclaim;
pub mod ws;

pub use server::run;
// Merged entry for `app-server --listen stdio:// --relay <url>`: the CLI crate
// calls it, so it must be publicly reachable (the underlying core is private).
pub use server::serve_stdio_with_relay;
