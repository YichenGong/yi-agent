//! Inter-process protocol between the runtime daemon and its clients.
//!
//! Split by concern: `protocol` holds the wire DTOs, `error` the failure
//! taxonomy, `daemon` the server side, `client` the call helpers, and
//! `subscription` / `trace` / `plugin_socket` the three streaming or routing
//! concerns. Everything public is re-exported here so callers keep writing
//! `yi_agent_store::ipc::X`.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(crate) use chrono::{DateTime, Local, Timelike, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;
pub(crate) use yi_agent_core::subagent::task::{
    PermissionDecision, PermissionRequestId, RootSessionId, TaskId,
};
pub(crate) use yi_agent_core::subagent::worker::{
    AgentWorkerFactory, WorkerError, WorkerHandle, WorkerStart, WorkerWorkspace,
};

pub(crate) use crate::repository::{DEFAULT_ORPHAN_GRACE_SECS, RuntimeRepository};
pub(crate) use crate::runtime::{RuntimeCoordinator, RuntimeCoordinatorError, RuntimeStopOptions};
pub(crate) use crate::schedule::ScheduleDefinition;

mod client;
mod daemon;
mod error;
mod managed;
mod plugin_socket;
mod protocol;
mod subscription;
mod trace;

// Internal re-exports so sibling submodules can reach each other's helpers
// through their own `use super::*`. Public consumers only see the curated
// list below; everything here is crate-visible at most.
pub(crate) use client::*;
pub(crate) use daemon::*;
pub(crate) use error::*;
pub use managed::{
    Compatibility, MAINTENANCE_PROTOCOL_VERSION, MaintenanceBarrier, MaintenanceRequest,
    MaintenanceState, MaintenanceStatus,
};
pub(crate) use plugin_socket::*;
pub(crate) use protocol::*;
pub(crate) use subscription::*;
pub(crate) use trace::*;

pub use client::{
    MAX_SOCKET_PATH_BYTES, send_request, send_request_with_version,
    send_request_with_version_timeout, socket_path_for, subscribe, subscribe_with_filters,
};
pub use daemon::Daemon;
pub use error::IpcError;
pub use plugin_socket::{clear_plugin_sockets, plugin_socket_for, register_plugin_sockets};
pub use protocol::{
    IpcCancelDelivery, IpcCancelLease, IpcCompletedChildReport, IpcErrorCode, IpcEvent,
    IpcEventPayload, IpcMailboxMessage, IpcPermissionDecision, IpcRequest, IpcResponse,
    IpcSchedule, IpcTask, IpcTaskDetail, IpcTaskSummary, IpcTaskUsage, IpcTraceRow,
    PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope, SubscriptionFilters, SubscriptionSnapshot,
    TraceSnapshot,
};
pub use subscription::Subscription;
pub use trace::{TraceSubscription, read_task_trace, subscribe_trace};
