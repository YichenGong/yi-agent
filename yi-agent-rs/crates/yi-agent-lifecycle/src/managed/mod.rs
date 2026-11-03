//! Private, owner-bound managed-process channels.
//!
//! Two halves: the descriptor-passing primitive, and the caller's client. The
//! guardian's server half lives in `yi-agent-supervisors`, with the process
//! registry it must share.

pub mod client;
pub mod fdpass;

pub use client::{ManagedExitArg, ManagedSpawnClient, ManagedSpawnSpecArg, StdioFds};
