//! Narrow, runtime-agnostic interface for live settings.
//!
//! The runtime owns file reading, validation, last-known-good retention and the
//! refresh gate. Core only asks for one immutable operation snapshot at a safe
//! point (after the first provider admission of an operation, before the
//! request is constructed). Keep this module free of `yi-agent-runtime` types
//! and of any filesystem access: importing either would invert the layering.

use std::sync::Arc;

/// One complete, effective settings snapshot for a single semantic operation.
///
/// When a source is attached, every field here is authoritative for the next
/// operation: the runtime already resolved `explicit override > process env >
/// settings > default`, so `None` means "no limit / no user prompt segment",
/// not "keep the static value". Fields that this snapshot does not carry
/// (model, generation params, retry policy) stay on `AgentConfig`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveAgentSnapshot {
    /// Content identity of the settings this snapshot was produced from. Not
    /// orderable; A -> B -> A is legal.
    pub revision: String,
    /// Maximum semantic iterations, `None` = unlimited.
    pub max_turns: Option<u32>,
    /// Token threshold that triggers automatic compaction, `None` = disabled.
    pub compact_threshold: Option<u32>,
    /// Real user input token budget retained by compaction.
    pub compact_user_budget_tokens: usize,
    /// Complete raw tool interaction token budget retained by compaction.
    pub compact_tool_budget_tokens: usize,
    /// Effective system prompt for the next normal THINK request.
    pub system_prompt: Option<String>,
}

/// A live settings operation failed in a way the current operation cannot use.
///
/// Only a stable code crosses the boundary: never a prompt, a key or raw
/// settings text. The runtime's health/LKG machinery absorbs file corruption;
/// this error covers "a connected source could not form a snapshot now".
#[derive(Clone, Debug, thiserror::Error)]
#[error("runtime settings refresh failed: {code}")]
pub struct LiveSettingsError {
    pub code: &'static str,
}

impl LiveSettingsError {
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }
}

/// Read-only narrow source consumed at provider operation boundaries.
///
/// Implementations must return an immutable snapshot; they must not expose the
/// provider, the session or the settings file, and they must not rebuild the
/// agent. `snapshot` is expected to be short and non-blocking for long: it is
/// called after admission, before the request is built, never per token.
pub trait AgentSettingsSource: Send + Sync {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError>;
}

/// Convenience alias used by [`crate::Agent`] bindings.
pub type SharedSettingsSource = Arc<dyn AgentSettingsSource>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_carries_only_the_code() {
        let error = LiveSettingsError::new("snapshot_unavailable");
        assert_eq!(
            error.to_string(),
            "runtime settings refresh failed: snapshot_unavailable"
        );
    }
}
