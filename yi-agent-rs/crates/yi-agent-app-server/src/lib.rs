//! yi-agent GUI app-server:JSON-RPC 2.0 over stdio。

pub mod protocol;
pub mod server;
pub mod session;
pub mod thread_store;
pub mod translate;
pub mod transport;

pub use server::run;
