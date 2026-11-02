//! 传输层：stdio / relay / scoped 的 serve 入口与后台 pump。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 8）。`serve()` 仍在本文件。

pub(crate) mod notifications;
pub(crate) mod scoped;
pub(crate) mod stdio;

#[allow(unused_imports)]
pub(crate) use notifications::{pump_git_diff_notifications, pump_theme_notifications};
// `run` 与 `serve_stdio_with_relay` 由 `lib.rs` 以 `pub use` 再导出给 CLI，故这里
// 必须同样是 `pub`；其余仅在 crate 内使用。
pub(crate) use scoped::{parse_relay_url, serve_scoped, serve_scoped_core, serve_stdio};
pub use scoped::{run, serve_stdio_with_relay};
#[allow(unused_imports)]
pub(crate) use stdio::{pump_stdout, read_lines};
