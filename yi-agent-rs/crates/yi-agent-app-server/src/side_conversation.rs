//! Host-side assembly of side-conversation sources.
//!
//! The runtime crate owns the *rules* (what a side may inherit, what it may
//! read). This module owns the host's half: where a parent's state actually
//! lives in this process (a live driver, or a strict cold read of its files) and
//! how to turn either into an [`EffectiveSideSource`].
//!
//! Kept separate from the runtime so the shared crate never has to know about
//! `ThreadStore`, `Item`, or the app-server's persisted wire types.

pub mod source;

pub use source::{
    ColdParent, SIDE_COLD_ATTEMPTS, SideParentResolver, SideResolverFactory, ThreadStoreResolver,
    assemble_cold_source, read_cold_parent, source_from_live_agent,
};
