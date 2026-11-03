//! The executor's side of the provider-turn gate.
//!
//! The real permit lives in the daemon, which owns the one machine-wide admission
//! limit. An executor that had its own counter would multiply that limit by the
//! number of clients -- and a wedged executor could hand itself turns forever. So
//! this side only *asks*: it sends `AcquireTurn`, waits for the daemon's answer, and
//! hands the worker a lease whose drop sends `ReleaseTurn`.
//!
//! Two failure paths matter and are deliberately not silent:
//!
//! - A refused acquire arrives as `Denied` with a reason, so the worker fails
//!   visibly instead of hanging on a permit that will never come.
//! - If the channel dies, every waiting acquire fails, and `ProxyLease::drop`
//!   cannot reach the daemon -- which is fine, because the daemon releases the
//!   real permit when *its* proxy for this executor is dropped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use tokio::sync::oneshot;
use yi_agent_core::agent::{ProviderTurnGate, ProviderTurnLease};

use super::protocol::GateMessage;

/// Where a grant or refusal is delivered.
///
/// Shared between the reader thread (which routes the daemon's answer) and the
/// gate the workers call, because the answer arrives on the read loop while the
/// waiter lives on a worker thread.
#[derive(Default)]
pub struct GateRouter {
    pending: Mutex<HashMap<String, oneshot::Sender<Result<String, String>>>>,
}

impl GateRouter {
    /// Delivers one answer. Returns `false` when nobody is waiting any more.
    ///
    /// A late answer is normal: the attempt may have been cancelled between the
    /// question and the answer. The daemon keeps the real permit and releases it
    /// with its own record, so dropping the answer here does not leak a slot.
    pub fn deliver(&self, request_id: &str, outcome: Result<String, String>) -> bool {
        let waiter = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
        match waiter {
            Some(sender) => sender.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Fails every waiter, for a channel that has gone away.
    pub fn fail_all(&self, reason: &str) {
        let waiters: Vec<_> = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain()
            .map(|(_, sender)| sender)
            .collect();
        for waiter in waiters {
            let _ = waiter.send(Err(reason.to_string()));
        }
    }

    fn register(&self, request_id: String, sender: oneshot::Sender<Result<String, String>>) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(request_id, sender);
    }

    fn withdraw(&self, request_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(request_id);
    }

    pub fn waiting(&self) -> usize {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// Sends gate messages to the daemon.
pub trait GateWire: Send + Sync {
    fn send_gate(&self, message: GateMessage) -> std::io::Result<()>;
}

/// The executor's gate: asks the daemon, never grants itself.
pub struct ProxyTurnGate {
    wire: Arc<dyn GateWire>,
    router: Arc<GateRouter>,
    task: String,
    attempt: String,
    profile: String,
    next_request: AtomicU64,
}

impl ProxyTurnGate {
    pub fn new(
        wire: Arc<dyn GateWire>,
        router: Arc<GateRouter>,
        task: impl Into<String>,
        attempt: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            wire,
            router,
            task: task.into(),
            attempt: attempt.into(),
            profile: profile.into(),
            next_request: AtomicU64::new(0),
        }
    }

    pub fn router(&self) -> Arc<GateRouter> {
        Arc::clone(&self.router)
    }
}

impl ProviderTurnGate for ProxyTurnGate {
    fn acquire(&self) -> BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
        let sequence = self.next_request.fetch_add(1, Ordering::Relaxed);
        // Request ids are per attempt, so a late answer for a stopped attempt can
        // never be matched to the next one's question.
        let request_id = format!("{}/{}/{sequence}", self.task, self.attempt);
        let (sender, receiver) = oneshot::channel();
        self.router.register(request_id.clone(), sender);
        let requested = self.wire.send_gate(GateMessage::AcquireTurn {
            request_id: request_id.clone(),
            task: self.task.clone(),
            attempt: self.attempt.clone(),
            profile: self.profile.clone(),
        });
        if let Err(error) = requested {
            self.router.withdraw(&request_id);
            return Box::pin(async move {
                Err(format!(
                    "the daemon is unreachable for a provider turn: {error}"
                ))
            });
        }
        let wire = Arc::clone(&self.wire);
        let router = Arc::clone(&self.router);
        Box::pin(async move {
            match receiver.await {
                Ok(Ok(permit_id)) => Ok(Box::new(ProxyLease {
                    permit_id,
                    wire,
                    router,
                }) as Box<dyn ProviderTurnLease>),
                Ok(Err(reason)) => Err(reason),
                Err(_) => Err("the provider gate answer was lost".to_string()),
            }
        })
    }
}

/// The executor's stand-in for a permit.
///
/// Holding it proves nothing locally; it only records which daemon permit must be
/// released. That is the point: no local object can mint a turn.
pub struct ProxyLease {
    permit_id: String,
    wire: Arc<dyn GateWire>,
    router: Arc<GateRouter>,
}

impl ProxyLease {
    pub fn permit_id(&self) -> &str {
        &self.permit_id
    }
}

impl Drop for ProxyLease {
    fn drop(&mut self) {
        // Best effort: if the daemon is gone there is nobody to release to, and its
        // own proxy drop already freed the real permit. Leaking here is impossible.
        let _ = self.wire.send_gate(GateMessage::ReleaseTurn {
            permit_id: self.permit_id.clone(),
        });
        // No pending entry to remove: the answer was consumed by the acquire.
        let _ = &self.router;
    }
}
