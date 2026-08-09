use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AdmissionPriority {
    Background,
    Normal,
    High,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub scope: ResourceScope,
    pub key: String,
    pub mode: LeaseMode,
    pub units: u16,
    /// Resource-wait deadline. Expired requests never acquire a permit.
    pub deadline: Option<DateTime<Utc>>,
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
    priority: AdmissionPriority,
    enqueued_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ReleaseError {
    #[error("lease does not exist or was already released")]
    UnknownLease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AdmissionError {
    #[error("queued subagent capacity is exhausted")]
    QueueCapacityExceeded,
}

#[derive(Debug)]
pub struct ResourceCoordinator {
    capacities: HashMap<String, u16>,
    in_use: HashMap<String, u16>,
    queues: HashMap<String, VecDeque<QueuedRequest>>,
    active: HashMap<LeaseId, GrantedLease>,
    last_grant_root: HashMap<String, RootSessionId>,
    queue_capacity: usize,
}

impl Default for ResourceCoordinator {
    fn default() -> Self {
        let mut capacities = HashMap::new();
        capacities.insert(
            "resident:global".into(),
            ResourceCoordinator::DEFAULT_GLOBAL_RESIDENT_SUBAGENTS,
        );
        capacities.insert("coding:global".into(), 6);
        capacities.insert("build:host".into(), 2);
        Self {
            capacities,
            in_use: HashMap::new(),
            queues: HashMap::new(),
            active: HashMap::new(),
            last_grant_root: HashMap::new(),
            queue_capacity: 64,
        }
    }
}

impl ResourceCoordinator {
    pub const DEFAULT_GLOBAL_RESIDENT_SUBAGENTS: u16 = 16;
    pub const DEFAULT_LLM_PER_PROVIDER_KEY: u16 = 8;
    pub const RESERVED_COORDINATION_LLM_PERMITS: u16 = 1;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_capacity(&mut self, key: impl Into<String>, units: u16) {
        self.capacities.insert(key.into(), units);
    }

    pub fn set_queue_capacity(&mut self, capacity: usize) {
        self.queue_capacity = capacity;
    }

    pub fn queued_request_count(&self) -> usize {
        self.queues.values().map(VecDeque::len).sum()
    }

    pub fn try_enqueue(
        &mut self,
        root_id: RootSessionId,
        task_id: TaskId,
        request: ResourceRequest,
    ) -> Result<(), AdmissionError> {
        if self.queued_request_count() >= self.queue_capacity {
            return Err(AdmissionError::QueueCapacityExceeded);
        }
        self.enqueue(root_id, task_id, request);
        Ok(())
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
        self.enqueue_with_priority(root_id, task_id, request, AdmissionPriority::Normal);
    }

    pub fn enqueue_with_priority(
        &mut self,
        root_id: RootSessionId,
        task_id: TaskId,
        request: ResourceRequest,
        priority: AdmissionPriority,
    ) {
        self.enqueue_with_priority_at(root_id, task_id, request, priority, Utc::now());
    }

    pub fn enqueue_with_priority_at(
        &mut self,
        root_id: RootSessionId,
        task_id: TaskId,
        request: ResourceRequest,
        priority: AdmissionPriority,
        enqueued_at: DateTime<Utc>,
    ) {
        self.queues
            .entry(request.key.clone())
            .or_default()
            .push_back(QueuedRequest {
                root_id,
                task_id,
                request,
                priority,
                enqueued_at,
            });
    }

    pub fn grant_next(&mut self, key: &str) -> Option<GrantedLease> {
        self.grant_next_at(key, Utc::now())
    }

    pub fn grant_next_at(&mut self, key: &str, now: DateTime<Utc>) -> Option<GrantedLease> {
        let capacity = *self.capacities.get(key).unwrap_or(&0);
        let used = *self.in_use.get(key).unwrap_or(&0);
        let has_active_lease = self.active.values().any(|lease| lease.request.key == key);
        let has_exclusive_lease = self
            .active
            .values()
            .any(|lease| lease.request.key == key && lease.request.mode == LeaseMode::Exclusive);
        let queue = self.queues.get_mut(key)?;
        queue.retain(|entry| entry.request.deadline.is_none_or(|deadline| deadline > now));
        if used >= capacity {
            return None;
        }
        let last_root = self.last_grant_root.get(key).cloned();
        let selected_index = select_fair_request(
            queue,
            last_root.as_ref(),
            capacity - used,
            now,
            has_active_lease,
            has_exclusive_lease,
        )?;
        let queued = queue
            .remove(selected_index)
            .expect("selected queue entry exists");
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
        let Some(lease) = self.active.remove(&lease_id) else {
            // Terminal transitions can race with crash/stop cleanup. Releasing
            // an already released ID must not double-decrement capacity.
            return Ok(());
        };
        let used = self.in_use.entry(lease.request.key).or_default();
        *used = used.saturating_sub(lease.request.units);
        Ok(())
    }
}

fn select_fair_request(
    queue: &VecDeque<QueuedRequest>,
    last_root: Option<&RootSessionId>,
    available: u16,
    now: DateTime<Utc>,
    has_active_lease: bool,
    has_exclusive_lease: bool,
) -> Option<usize> {
    let highest_score = queue
        .iter()
        .filter(|entry| {
            entry.request.units <= available
                && request_is_compatible(entry, has_active_lease, has_exclusive_lease)
        })
        .map(|entry| admission_score(entry, now))
        .max()?;
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
        if let Some((index, _)) = queue.iter().enumerate().find(|(_, entry)| {
            &entry.root_id == root
                && admission_score(entry, now) == highest_score
                && entry.request.units <= available
                && request_is_compatible(entry, has_active_lease, has_exclusive_lease)
        }) {
            return Some(index);
        }
    }
    None
}

fn request_is_compatible(
    entry: &QueuedRequest,
    has_active_lease: bool,
    has_exclusive_lease: bool,
) -> bool {
    !has_exclusive_lease && !(has_active_lease && entry.request.mode == LeaseMode::Exclusive)
}

fn admission_score(entry: &QueuedRequest, now: DateTime<Utc>) -> i64 {
    let priority = match entry.priority {
        AdmissionPriority::Background => 0,
        AdmissionPriority::Normal => 10,
        AdmissionPriority::High => 20,
        AdmissionPriority::Critical => 40,
    };
    let wait_seconds = (now - entry.enqueued_at).num_seconds().max(0);
    let age_boost = (wait_seconds / 30).min(20);
    priority + age_boost
}
