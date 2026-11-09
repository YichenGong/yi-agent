//! Side (temporary, read-only) conversation support shared by every host.
//!
//! A side conversation is a short-lived, read-only branch of one parent thread.
//! Three things live here rather than in a host:
//!
//! - the **effective source** a side is assembled from, published atomically so
//!   a reader never mixes an old provider with a new config
//!   ([`source`]),
//! - the fixed capability boundary ([`policy`]), which is a whitelist of
//!   non-confirming read-only builtin tools, and
//! - the request ledger and idle deadline ([`ledger`]), which make a
//!   retried request idempotent and expire an idle side consistently across
//!   hosts.
//!
//! Nothing here depends on `yi-agent-app-server`: both hosts consume these
//! primitives directly, so the dependency never has to be inverted later.

pub mod ledger;
pub mod policy;
pub mod resources;
pub mod runner;
pub mod source;
pub mod transcript;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use ledger::{AcceptedRequest, SideIdleDeadline, SideRequestLedger};
pub use policy::{SideToolPolicy, SideToolSetupFactory, build_side_tool_setup};
pub use resources::{
    PreparedSideResources, SIDE_BINARY_BUDGET_BYTES, SIDE_RESOURCE_SLOTS, SideHostResources,
    SideResourceLease,
};
pub use runner::{
    SIDE_STOP_GRACE, SIDE_TURN_TIMEOUT, SideConversationRunner, SideRunnerEvent, SideStatus,
    SideTokenTotals, SideUsage,
};
pub use source::{
    EffectiveSideSource, SideAttachment, SideAttachmentCatalog, SideSourcePublisher, SourceUpdate,
};
pub use transcript::{
    SIDE_TOOL_DISPLAY_BYTES, SIDE_TRANSCRIPT_BYTES, SIDE_TRANSCRIPT_CONTROL_RESERVE,
    SIDE_TRANSCRIPT_MAX_ITEM_BYTES, SideItem, SideItemContent, SideToolStatus, SideTranscript,
};

use std::fmt;

use serde::{Deserialize, Serialize};

/// Why a side conversation stopped being usable.
///
/// Shared verbatim by every host so a client can react to one closed
/// notification without host-specific cases. `resource_lost` in particular means
/// the parent's worktree was reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// The user explicitly closed it.
    UserClosed,
    /// It was idle past its deadline.
    Expired,
    /// The parent thread was cleared.
    ParentCleared,
    /// The parent thread was deleted.
    ParentDeleted,
    /// The host is shutting down.
    HostShutdown,
    /// The resources behind it disappeared (for example a reclaimed worktree).
    ResourceLost,
}

impl CloseReason {
    /// The stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserClosed => "user_closed",
            Self::Expired => "expired",
            Self::ParentCleared => "parent_cleared",
            Self::ParentDeleted => "parent_deleted",
            Self::HostShutdown => "host_shutdown",
            Self::ResourceLost => "resource_lost",
        }
    }
}

/// The stable machine-readable reason carried in an error's `data.reason`.
///
/// These strings are a protocol contract: `side_not_found`, `parent_not_started`,
/// `side_limit`, `snapshot_too_large`, `source_changed`, `side_closing`,
/// `close_conflict`, `snapshot_expired`, `side_creating`, `request_limit` and
/// `resource_limit` come from the design; the remaining four reuse the existing
/// generic reasons rather than inventing new ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideErrorReason {
    // --- side-specific reasons (§8.3) ---
    SideNotFound,
    ParentNotStarted,
    SideLimit,
    SnapshotTooLarge,
    SourceChanged,
    SideClosing,
    CloseConflict,
    SnapshotExpired,
    SideCreating,
    RequestLimit,
    ResourceLimit,
    TurnFailed,
    // --- reused generic reasons ---
    InvalidParams,
    TurnInProgress,
    InsufficientScope,
    UnknownThread,
    Cancelled,
}

impl SideErrorReason {
    /// The stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SideNotFound => "side_not_found",
            Self::ParentNotStarted => "parent_not_started",
            Self::SideLimit => "side_limit",
            Self::SnapshotTooLarge => "snapshot_too_large",
            Self::SourceChanged => "source_changed",
            Self::SideClosing => "side_closing",
            Self::CloseConflict => "close_conflict",
            Self::SnapshotExpired => "snapshot_expired",
            Self::SideCreating => "side_creating",
            Self::RequestLimit => "request_limit",
            Self::ResourceLimit => "resource_limit",
            Self::TurnFailed => "turn_failed",
            Self::InvalidParams => "invalid_params",
            Self::TurnInProgress => "turn_in_progress",
            Self::InsufficientScope => "insufficient_scope",
            Self::UnknownThread => "unknown_thread",
            Self::Cancelled => "cancelled",
        }
    }
}

/// A side-conversation failure with a stable reason and a human message.
///
/// The message is shown to the user and logged, so it must never contain a
/// credential or a full prompt body: callers map an underlying error to a short,
/// non-secret explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideError {
    pub reason: SideErrorReason,
    pub message: String,
}

impl SideError {
    pub fn new(reason: SideErrorReason, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }

    /// The stable wire string for this failure.
    pub fn reason_str(&self) -> &'static str {
        self.reason.as_str()
    }
}

impl fmt::Display for SideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason.as_str(), self.message)
    }
}

impl std::error::Error for SideError {}

/// Result alias for every public side API.
pub type SideResult<T> = Result<T, SideError>;

/// Map an exceeded side budget to a stable, non-secret error.
///
/// A budget overflow is not an internal failure: it means the parent's history
/// (or a later turn's output) does not fit the side's allowance, so the caller
/// must be told to compact the parent or drop attachments.
pub fn map_budget_error(error: yi_agent_core::BudgetExceeded) -> SideError {
    SideError::new(
        SideErrorReason::SnapshotTooLarge,
        format!("按侧对话预算无法容纳该内容：{error}"),
    )
}
