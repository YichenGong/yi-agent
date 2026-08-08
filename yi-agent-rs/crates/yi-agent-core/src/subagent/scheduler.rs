use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use super::task::{RootSessionId, TaskId};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceScope {
    Global,
    ProviderKey,
    Project,
    Workspace,
    Branch,
    Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeaseMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub scope: ResourceScope,
    pub key: String,
    pub mode: LeaseMode,
    pub units: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LeaseId(Uuid);

impl LeaseId {
    fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedLease {
    pub lease_id: LeaseId,
    pub task_id: TaskId,
    pub root_id: RootSessionId,
    pub request: ResourceRequest,
}

#[derive(Debug, Clone)]
struct QueuedRequest {
    root_id: RootSessionId,
    task_id: TaskId,
    request: ResourceRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReleaseError {
    #[error("lease does not exist or was already released")]
    UnknownLease,
}

#[derive(Debug)]
pub struct ResourceCoordinator {
    capacities: HashMap<String, u16>,
    in_use: HashMap<String, u16>,
    queues: HashMap<String, VecDeque<QueuedRequest>>,
    active: HashMap<LeaseId, GrantedLease>,
    last_grant_root: HashMap<String, RootSessionId>,
}

impl Default for ResourceCoordinator {
    fn default() -> Self {
        let mut capacities = HashMap::new();
        capacities.insert("resident:global".into(), 16);
        capacities.insert("coding:global".into(), 6);
        capacities.insert("build:host".into(), 2);
        Self {
            capacities,
            in_use: HashMap::new(),
            queues: HashMap::new(),
            active: HashMap::new(),
            last_grant_root: HashMap::new(),
        }
    }
}

impl ResourceCoordinator {
    pub const DEFAULT_LLM_PER_PROVIDER_KEY: u16 = 8;
    pub const RESERVED_COORDINATION_LLM_PERMITS: u16 = 1;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_capacity(&mut self, key: impl Into<String>, units: u16) {
        self.capacities.insert(key.into(), units);
    }

    /// Configure the regular and coordination-reserved LLM pools for a
    /// provider/API-key profile. A caller may borrow the reserve only when no
    /// coordination request is eligible.
    pub fn configure_provider_llm_capacity(&mut self, provider_key: &str) {
        self.set_capacity(
            format!("llm:{provider_key}"),
            Self::DEFAULT_LLM_PER_PROVIDER_KEY - Self::RESERVED_COORDINATION_LLM_PERMITS,
        );
        self.set_capacity(
            format!("llm-coordination:{provider_key}"),
            Self::RESERVED_COORDINATION_LLM_PERMITS,
        );
    }

    pub fn capacity(&self, key: &str) -> Option<u16> {
        self.capacities.get(key).copied()
    }

    pub fn enqueue(&mut self, root_id: RootSessionId, task_id: TaskId, request: ResourceRequest) {
        self.queues
            .entry(request.key.clone())
            .or_default()
            .push_back(QueuedRequest {
                root_id,
                task_id,
                request,
            });
    }

    pub fn grant_next(&mut self, key: &str) -> Option<GrantedLease> {
        let capacity = *self.capacities.get(key).unwrap_or(&0);
        let used = *self.in_use.get(key).unwrap_or(&0);
        let queue = self.queues.get_mut(key)?;
        if used >= capacity {
            return None;
        }
        let last_root = self.last_grant_root.get(key).cloned();
        let selected_index = select_fair_request(queue, last_root.as_ref(), capacity - used)?;
        let queued = queue
            .remove(selected_index)
            .expect("selected queue entry exists");
        let incompatible_lease = self.active.values().any(|lease| {
            lease.request.key == queued.request.key
                && (lease.request.mode == LeaseMode::Exclusive
                    || queued.request.mode == LeaseMode::Exclusive)
        });
        if incompatible_lease {
            queue.insert(selected_index, queued);
            return None;
        }
        let lease = GrantedLease {
            lease_id: LeaseId::new(),
            task_id: queued.task_id,
            root_id: queued.root_id,
            request: queued.request,
        };
        *self.in_use.entry(key.to_owned()).or_default() += lease.request.units;
        self.last_grant_root
            .insert(key.to_owned(), lease.root_id.clone());
        self.active.insert(lease.lease_id.clone(), lease.clone());
        Some(lease)
    }

    pub fn release(&mut self, lease_id: LeaseId) -> Result<(), ReleaseError> {
        let lease = self
            .active
            .remove(&lease_id)
            .ok_or(ReleaseError::UnknownLease)?;
        let used = self.in_use.entry(lease.request.key).or_default();
        *used = used.saturating_sub(lease.request.units);
        Ok(())
    }
}

fn select_fair_request(
    queue: &VecDeque<QueuedRequest>,
    last_root: Option<&RootSessionId>,
    available: u16,
) -> Option<usize> {
    let roots =
        queue
            .iter()
            .map(|entry| entry.root_id.clone())
            .fold(Vec::new(), |mut roots, root| {
                if !roots.contains(&root) {
                    roots.push(root);
                }
                roots
            });
    let start = last_root
        .and_then(|last| {
            roots
                .iter()
                .position(|root| root == last)
                .map(|index| (index + 1) % roots.len())
        })
        .unwrap_or(0);
    for offset in 0..roots.len() {
        let root = &roots[(start + offset) % roots.len()];
        if let Some((index, _)) = queue
            .iter()
            .enumerate()
            .find(|(_, entry)| &entry.root_id == root && entry.request.units <= available)
        {
            return Some(index);
        }
    }
    None
}
