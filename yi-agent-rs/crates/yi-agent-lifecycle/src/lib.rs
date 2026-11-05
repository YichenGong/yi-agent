//! Shared runtime lifecycle control protocol.
//!
//! This crate is the only artifact both the desktop shell and the runtime link,
//! so it must stay dependency-light: no Tokio, no SQLite, no model stack. Check
//! with `cargo tree -p yi-agent-lifecycle`. The guard exists because the Tauri
//! binary links this crate directly, and pulling `rusqlite` in through it would
//! give the desktop shell a second database stack it must never have.
//!
//! Contents:
//! - [`types`] — wire DTOs and identity types.
//! - [`authorization`] — who may attach a root, as decided by the guardian.
//! - [`codec`] — length-prefixed, version-checked framing bounded by
//!   [`MAX_CONTROL_FRAME`].
//! - [`deadline`] — one total budget per operation on an injectable monotonic
//!   clock.
//! - [`client`] — the blocking control-channel client.
//! - [`leases`] — the frontend lease state machine (no I/O, injectable time).
//! - [`managed`] — private managed-process channels, including descriptor passing.
//! - [`scope`] — canonical runtime scope resolution and the socket names derived
//!   from it. Here rather than in the supervisors because the desktop shell must
//!   resolve the same paths without linking them.

pub mod authorization;
pub mod client;
pub mod codec;
pub mod deadline;
/// 前台会话根生命周期的跨进程 DTO。
pub mod foreground;
pub mod leases;
pub mod managed;
/// 只读影响快照与一次性确认。
pub mod preview;
pub mod scope;
pub mod types;

/// Control protocol version. Evolves independently of the business protocol.
pub const CONTROL_VERSION: u16 = 1;

/// The guardian's control socket file inside a runtime directory.
///
/// One definition, here, because the desktop shell and the supervisors must agree
/// on the path or the shell would connect to a socket nobody is serving.
pub const GUARDIAN_SOCKET_FILE: &str = "guardian-control.sock";

/// Upper bound on one control frame, including its 4-byte length header.
pub const MAX_CONTROL_FRAME: usize = 16 * 1024;

/// Per-request ceiling for a single control request, further capped by whatever
/// remains of the operation's total budget.
pub const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How long one managed spawn channel may idle before it is closed.
///
/// Not a session limit: a channel serves exactly one owner for as long as that
/// owner lives, and the timeout is per *read*, so a long `Wait` re-arms it. The
/// value is generous because a tool's channel is idle while the tool's command
/// runs; a short idle window would close a channel mid-build.
pub const MANAGED_CHANNEL_BUDGET: std::time::Duration = std::time::Duration::from_secs(60 * 60);
