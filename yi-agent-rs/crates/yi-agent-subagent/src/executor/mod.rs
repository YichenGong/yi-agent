//! Per-client worker execution in its own process.
//!
//! A worker that hangs must be stoppable by force. Threads cannot be killed
//! safely, and a cooperative cancel is not a stop: a wedged worker can ignore the
//! token forever. So each client's workers run in a process the daemon can
//! terminate, and that process is a *sibling* of the daemon rather than a child of
//! it -- otherwise the daemon's death would take the workers with it, and the
//! daemon's own restart would be the only thing that ever stopped them.

pub mod factory;
pub mod gate;
pub mod protocol;
pub mod proxy;
pub mod serve;

pub use factory::{ExecutorConnector, ProcessWorkerFactory};
pub use gate::{GateRouter, GateWire, ProxyLease, ProxyTurnGate};
pub use protocol::{
    ExecutorEvent, ExecutorRequest, GateMessage, MAX_EXECUTOR_FRAME, MessageInbox,
    RootExecutionConfig, WorkerEventWire, WorkerStartWire, WorkerWatchdogEventWire,
};
