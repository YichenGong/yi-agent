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

/// The compaction-owned subset of a [`LiveAgentSnapshot`].
///
/// A compaction operation is not a normal THINK: it must not render the user
/// prompt, must not inspect the old turn's consumed budget, and must use its own
/// admission lease. Giving it a narrower snapshot makes that structural instead
/// of a convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveCompactSnapshot {
    /// Content identity of the settings this snapshot was produced from.
    pub revision: String,
    /// Token threshold that triggers automatic compaction, `None` = disabled.
    pub compact_threshold: Option<u32>,
    /// Real user input token budget retained by this compaction.
    pub user_budget_tokens: usize,
    /// Complete raw tool interaction token budget retained by this compaction.
    pub tool_budget_tokens: usize,
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

    /// The compaction-owned subset of the current snapshot.
    ///
    /// The default derives it from [`Self::snapshot`], so a source that cannot
    /// distinguish the two stays correct. The runtime overrides it when the
    /// compaction prompt segment must be composed differently from the normal
    /// system prompt.
    fn compact_snapshot(&self) -> Result<LiveCompactSnapshot, LiveSettingsError> {
        let value = self.snapshot()?;
        Ok(LiveCompactSnapshot {
            revision: value.revision,
            compact_threshold: value.compact_threshold,
            user_budget_tokens: value.compact_user_budget_tokens,
            tool_budget_tokens: value.compact_tool_budget_tokens,
        })
    }
}

/// Convenience alias used by [`crate::Agent`] bindings.
pub type SharedSettingsSource = Arc<dyn AgentSettingsSource>;

/// The two subagent admission capacities a settings save can change live.
///
/// Both values are effective capacities, already resolved by the runtime; core
/// only compares and applies them. Shrinking never cancels a worker that has
/// already been admitted and never rewrites an already-consumed slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdmissionLimits {
    /// Non-terminal direct children one task may own.
    pub direct_children: u16,
    /// Resident subagents this runtime admits.
    pub resident_subagents: u16,
}

/// Read-only live source for [`AdmissionLimits`], owned by the worker factory.
///
/// `snapshot` is read inside the existing admission critical section, so a save
/// that lands before an actual grant is honoured and one that lands after it is
/// not. `subscribe` is only a wake-up: the revision is monotonic for change
/// detection and is **not** a settings `revision`, so callers must re-read
/// `snapshot` instead of deriving limits from it.
pub trait AdmissionLimitsSource: Send + Sync {
    fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError>;
    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64>;
}

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

    #[test]
    fn admission_limits_carry_exactly_the_two_capacities() {
        let limits = AdmissionLimits {
            direct_children: 3,
            resident_subagents: 7,
        };
        let copied = limits;
        assert_eq!(copied, limits, "the limits are a plain copyable value");
        assert_eq!(copied.direct_children, 3);
        assert_eq!(copied.resident_subagents, 7);
        assert_ne!(
            limits,
            AdmissionLimits {
                direct_children: 4,
                resident_subagents: 7,
            }
        );
    }

    #[tokio::test]
    async fn admission_limits_source_reports_snapshot_and_wakes_subscribers() {
        struct Limits {
            value: std::sync::Mutex<AdmissionLimits>,
            changed: tokio::sync::watch::Sender<u64>,
        }
        impl AdmissionLimitsSource for Limits {
            fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError> {
                Ok(*self.value.lock().unwrap())
            }
            fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
                self.changed.subscribe()
            }
        }

        let (changed, _keepalive) = tokio::sync::watch::channel(0);
        let source = Limits {
            value: std::sync::Mutex::new(AdmissionLimits {
                direct_children: 2,
                resident_subagents: 5,
            }),
            changed,
        };

        assert_eq!(
            source.snapshot().unwrap(),
            AdmissionLimits {
                direct_children: 2,
                resident_subagents: 5,
            }
        );

        let mut receiver = source.subscribe();
        assert_eq!(*receiver.borrow_and_update(), 0);
        *source.value.lock().unwrap() = AdmissionLimits {
            direct_children: 1,
            resident_subagents: 5,
        };
        source.changed.send(1).unwrap();
        receiver.changed().await.unwrap();
        assert_eq!(*receiver.borrow_and_update(), 1);
        assert_eq!(source.snapshot().unwrap().direct_children, 1);
    }
}
