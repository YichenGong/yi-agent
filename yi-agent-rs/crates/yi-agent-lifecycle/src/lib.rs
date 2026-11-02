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
//! - [`codec`] — length-prefixed, version-checked framing bounded by
//!   [`MAX_CONTROL_FRAME`].
//! - [`deadline`] — one total budget per operation on an injectable monotonic
//!   clock.
//! - [`client`] — the blocking control-channel client.

pub mod client;
pub mod codec;
pub mod deadline;
pub mod types;

/// Control protocol version. Evolves independently of the business protocol.
pub const CONTROL_VERSION: u16 = 1;

/// Upper bound on one control frame, including its 4-byte length header.
pub const MAX_CONTROL_FRAME: usize = 16 * 1024;

/// Per-request ceiling for a single control request, further capped by whatever
/// remains of the operation's total budget.
pub const CONTROL_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
