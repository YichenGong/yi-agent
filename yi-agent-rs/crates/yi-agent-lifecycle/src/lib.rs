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
/// 共享的非业务管理 DTO：项目准备、逐 root 授权索引与授权快照。
///
/// 只有线上类型，没有 I/O：桌面壳、app-server 与 supervisor 必须按同一套字段
/// 讲话，而壳只允许依赖本 crate。
pub mod production;
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

/// How long one *root authorization* round trip may take.
///
/// Five seconds, and deliberately not the managed-channel budget: the shell has already
/// prepared the runtime by the time a conversation asks for a root, so what remains is a
/// single guardian round trip. Waiting longer would keep a conversation parked behind a
/// shell that is itself waiting on a stuck daemon, which is the failure this bounds.
///
/// Defined here because both sides of the round trip need it: the app-server bounds its
/// wait with it, and the subagent (which cannot see the app-server) builds the deadline
/// from it. Two private copies would drift apart silently.
pub const AUTHORIZE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one managed spawn channel may idle before it is closed.
///
/// Not a session limit: a channel serves exactly one owner for as long as that
/// owner lives, and the timeout is per *read*, so a long `Wait` re-arms it. The
/// value is generous because a tool's channel is idle while the tool's command
/// runs; a short idle window would close a channel mid-build.
pub const MANAGED_CHANNEL_BUDGET: std::time::Duration = std::time::Duration::from_secs(60 * 60);
