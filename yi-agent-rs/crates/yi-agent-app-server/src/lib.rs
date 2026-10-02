//! yi-agent GUI app-server:JSON-RPC 2.0 over stdio。

pub mod protocol;
pub mod server;
pub mod session;
pub mod settings_store;
pub mod theme_tool;
pub mod thread_store;
pub mod translate;
pub mod transport;
pub mod workspace_index;

pub use server::run;
