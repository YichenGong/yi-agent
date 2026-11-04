//! Superpowers 看板的推进循环。

pub mod client;
pub mod drain;
pub mod inbox;
pub mod lease;
pub mod managed;
pub mod merge;
pub mod merge_attempt;
pub mod merge_lock;
pub mod migrate;
pub mod observation;
pub mod operations;
pub mod tick;

pub mod dispatch;
pub mod driver_observation;
pub mod persist;
pub mod quota;
pub mod readiness;
pub mod resident;
pub mod service;
pub mod single_instance;
pub mod worktree;
