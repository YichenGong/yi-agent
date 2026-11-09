//! Host-side assembly of side-conversation sources.
//!
//! The runtime crate owns the *rules* (what a side may inherit, what it may
//! read). This module owns the host's half: where a parent's state actually
//! lives in this process (a live driver, or a strict cold read of its files) and
//! how to turn either into an [`EffectiveSideSource`].
//!
//! Kept separate from the runtime so the shared crate never has to know about
//! `ThreadStore`, `Item`, or the app-server's persisted wire types.

pub mod lifecycle;
pub mod live_parents;
pub mod registry;
pub mod replay;
pub mod source;
pub mod tickets;

pub use live_parents::{LiveParentSource, LiveSideParents, live_roots};
pub use registry::{
    ClosedSide, ParentChangeGuard, SIDE_TOMBSTONE_TTL, SIDE_TOMBSTONES, SideEffectiveConfig,
    SideRegistry, SideSnapshot, SideSummary,
};
pub use replay::{
    Page, ReplayBaseline, ReplayStore, SIDE_ENVELOPE_BUDGET_BYTES, SIDE_ITEM_FLUSH_INTERVAL,
    SIDE_PAGE_BUDGET_BYTES, SIDE_PAGE_CONTROL_RESERVE, SIDE_SNAPSHOT_BYTES,
    SIDE_TOKEN_ABSOLUTE_TTL, SIDE_TOKEN_IDLE_TTL, SIDE_TOKENS_PER_CONNECTION, SIDE_TOKENS_PER_HOST,
    SharedReplayStore, SidePublishedUpdate, encode_side_envelope, log_page, next_seq,
    shared_replay_store, wire_item,
};
pub use source::{
    ColdParent, SIDE_COLD_ATTEMPTS, SideParentResolver, SideResolverFactory, ThreadStoreResolver,
    WorkspaceResolver, assemble_cold_source, read_cold_parent, source_from_live_agent,
};
pub use tickets::{
    CLOSED_TICKET_TTL, ENTRY_TICKET_TTL, ENTRY_TICKETS_PER_HOST, ENTRY_TICKETS_PER_PARENT,
    EntryTarget, EntryTickets, ExitReceipt, ExitState, SideGeneration,
};
