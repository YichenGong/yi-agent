use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::task::{RootSessionId, TaskId};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveBudget {
    pub max_turns: Option<u32>,
    pub max_wall_time_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathScope {
    pub read_paths: BTreeSet<String>,
    pub write_paths: BTreeSet<String>,
}

impl PathScope {
    pub fn new(write_paths: BTreeSet<String>) -> Self {
        Self {
            read_paths: write_paths.clone(),
            write_paths,
        }
    }

    fn is_subset_of(&self, parent: &Self) -> bool {
        self.read_paths.iter().all(|path| {
            parent
                .read_paths
                .iter()
                .any(|allowed| path_is_within(path, allowed))
        }) && self.write_paths.iter().all(|path| {
            parent
                .write_paths
                .iter()
                .any(|allowed| path_is_within(path, allowed))
        })
    }
}

fn path_is_within(candidate: &str, allowed: &str) -> bool {
    let allowed_prefix = allowed.strip_suffix("/**").unwrap_or(allowed);
    candidate == allowed || candidate.starts_with(&format!("{allowed_prefix}/"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegatedAuthority {
    pub issuer: TaskId,
    pub holder: TaskId,
    pub root_session_id: RootSessionId,
    pub tools: BTreeSet<String>,
    pub paths: PathScope,
    pub budget: EffectiveBudget,
    pub deadline: Option<DateTime<Utc>>,
}

impl DelegatedAuthority {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        issuer: TaskId,
        holder: TaskId,
        root_session_id: RootSessionId,
        tools: BTreeSet<String>,
        paths: PathScope,
        budget: EffectiveBudget,
        deadline: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            issuer,
            holder,
            root_session_id,
            tools,
            paths,
            budget,
            deadline,
        }
    }

    pub fn derive_child(&self, proposed: &Self) -> Result<Self, AuthorityDerivationError> {
        if !proposed.tools.is_subset(&self.tools) {
            let tool = proposed
                .tools
                .difference(&self.tools)
                .next()
                .expect("non-subset has a differing tool")
                .clone();
            return Err(AuthorityDerivationError::ToolNotDelegable { tool });
        }
        if !proposed.paths.is_subset_of(&self.paths) {
            return Err(AuthorityDerivationError::PathOutsideLease);
        }
        if exceeds(proposed.budget.max_turns, self.budget.max_turns)
            || exceeds(
                proposed.budget.max_wall_time_secs,
                self.budget.max_wall_time_secs,
            )
        {
            return Err(AuthorityDerivationError::BudgetOverallocated);
        }
        if let (Some(child), Some(parent)) = (proposed.deadline, self.deadline) {
            if child > parent {
                return Err(AuthorityDerivationError::DeadlineAfterParent);
            }
        }
        Ok(proposed.clone())
    }
}

fn exceeds<T: Ord>(child: Option<T>, parent: Option<T>) -> bool {
    matches!((child, parent), (Some(child), Some(parent)) if child > parent)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorityDerivationError {
    #[error("tool {tool} is not delegable")]
    ToolNotDelegable { tool: String },
    #[error("path is outside the parent lease")]
    PathOutsideLease,
    #[error("child budget exceeds the parent budget")]
    BudgetOverallocated,
    #[error("child deadline is after the parent deadline")]
    DeadlineAfterParent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryPolicy {
    Immediate,
    Batch,
    OnWait,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationContract {
    pub task_id: TaskId,
    pub parent_id: Option<TaskId>,
    pub root_session_id: RootSessionId,
    pub version: u32,
    pub title: String,
    pub objective: String,
    pub non_goals: Vec<String>,
    pub paths: PathScope,
    pub authority: DelegatedAuthority,
    pub budget: EffectiveBudget,
    pub delivery_policy: DeliveryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContractAmendment {
    AddContext {
        facts: Vec<String>,
        reason: String,
    },
    NarrowScope {
        paths: PathScope,
        reason: String,
    },
    ExtendScope {
        paths: PathScope,
        reason: String,
    },
    ChangeAcceptance {
        checks: Vec<String>,
        reason: String,
    },
    ChangeBudget {
        budget: EffectiveBudget,
        reason: String,
    },
    ChangeDeliveryPolicy {
        delivery_policy: DeliveryPolicy,
        reason: String,
    },
}
