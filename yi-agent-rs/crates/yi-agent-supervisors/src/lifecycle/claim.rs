//! Which project and configuration instance a runtime directory belongs to.
//!
//! A runtime directory can be forced with `YI_AGENT_RUNTIME_DIR`. That override is
//! how a user gives one project its own runtime, but it is also how two projects
//! could end up sharing one database while each believes it is independent. The
//! directory alone cannot tell them apart: its path is identical for both.
//!
//! So the first coordinator to start a runtime writes down who it is, and every
//! later coordinator compares. A mismatch is reported, never resolved by starting a
//! second daemon in the same directory or by quietly creating a new database
//! somewhere else.
//!
//! This is a *claim*, not a lease: it holds no credentials and says nothing about
//! liveness. Liveness is the lock's job; this answers only "which scope is this".

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use yi_agent_lifecycle::types::RuntimeScope;

use super::CoordinationGuard;

/// File name inside the runtime directory.
pub const CLAIM_FILE: &str = "scope.json";

/// The scope a runtime directory was first claimed by.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeClaim {
    /// Canonical project key of the claiming client.
    pub project_key: String,
    /// Canonical instance home (`YI_AGENT_HOME`) of the claiming client.
    pub instance_home: PathBuf,
}

fn claim_path(scope: &RuntimeScope) -> PathBuf {
    scope.runtime_dir.join(CLAIM_FILE)
}

/// Read the claim, or `None` when this runtime has never been claimed.
///
/// A corrupt file is an error rather than `None`: `None` would let the next client
/// treat an occupied runtime as a fresh one and overwrite the claim.
pub fn read_claim(scope: &RuntimeScope) -> io::Result<Option<ScopeClaim>> {
    let bytes = match std::fs::read(claim_path(scope)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write the claim atomically. The guard is required so two coordinators cannot
/// each believe they were the first to claim this directory.
pub fn write_claim(guard: &CoordinationGuard, claim: &ScopeClaim) -> io::Result<()> {
    let dir = &guard.scope().runtime_dir;
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(CLAIM_FILE);
    let temp_path = dir.join(format!(".{CLAIM_FILE}.{}.tmp", std::process::id()));

    let body = serde_json::to_vec_pretty(claim)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp_path)?;
        file.write_all(&body)?;
        file.sync_all()?;
    }
    if let Err(error) = std::fs::rename(&temp_path, &final_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    // The rename is only durable once the directory entry is.
    std::fs::File::open(dir)?.sync_all()?;
    std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

impl ScopeClaim {
    /// The claim implied by a scope.
    pub fn of(scope: &RuntimeScope) -> Self {
        Self {
            project_key: scope.project_key.clone(),
            instance_home: scope.instance_home.clone(),
        }
    }

    /// `Some(explanation)` when `scope` is not the scope that claimed this runtime.
    ///
    /// The message names both sides and the escape hatch, because the user's only
    /// safe move is to point one of them at a different directory.
    pub fn conflict_with(&self, scope: &RuntimeScope) -> Option<String> {
        if self.project_key != scope.project_key {
            return Some(format!(
                "runtime directory {} belongs to project {}, but this client is in {}; \
                 point YI_AGENT_RUNTIME_DIR at a separate directory for one of them",
                scope.runtime_dir.display(),
                self.project_key,
                scope.project_key
            ));
        }
        if self.instance_home != scope.instance_home {
            return Some(format!(
                "runtime directory {} belongs to instance home {}, but this client uses {}; \
                 they would share one database under two configurations, so set \
                 YI_AGENT_RUNTIME_DIR to a separate directory for one of them",
                scope.runtime_dir.display(),
                self.instance_home.display(),
                scope.instance_home.display()
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use yi_agent_lifecycle::deadline::Deadline;

    fn test_scope(root: &std::path::Path) -> RuntimeScope {
        use crate::lifecycle::scope::resolve;
        resolve(&root.join("runtime"), root, root).unwrap()
    }

    fn guard(scope: &RuntimeScope) -> CoordinationGuard {
        CoordinationGuard::acquire(scope, Deadline::after(Duration::from_secs(1))).unwrap()
    }

    #[test]
    fn an_unclaimed_runtime_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        assert!(read_claim(&scope).unwrap().is_none());
    }

    #[test]
    fn a_written_claim_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        let claim = ScopeClaim::of(&scope);
        write_claim(&guard(&scope), &claim).unwrap();
        assert_eq!(read_claim(&scope).unwrap(), Some(claim));
    }

    #[test]
    fn a_corrupt_claim_is_an_error_not_a_vacant_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        std::fs::create_dir_all(&scope.runtime_dir).unwrap();
        std::fs::write(claim_path(&scope), b"{not json").unwrap();
        assert_eq!(
            read_claim(&scope).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn another_project_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let a =
            crate::lifecycle::scope::resolve(&runtime, dir.path().join("a").as_path(), dir.path())
                .unwrap();
        let b =
            crate::lifecycle::scope::resolve(&runtime, dir.path().join("b").as_path(), dir.path())
                .unwrap();
        let conflict = ScopeClaim::of(&a)
            .conflict_with(&b)
            .expect("projects differ");
        assert!(conflict.contains("project"), "{conflict}");
    }

    #[test]
    fn another_instance_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = dir.path().join("runtime");
        let project = dir.path().join("project");
        let a = crate::lifecycle::scope::resolve(
            &runtime,
            &project,
            dir.path().join("home-a").as_path(),
        )
        .unwrap();
        let b = crate::lifecycle::scope::resolve(
            &runtime,
            &project,
            dir.path().join("home-b").as_path(),
        )
        .unwrap();
        let conflict = ScopeClaim::of(&a)
            .conflict_with(&b)
            .expect("instances differ");
        assert!(conflict.contains("instance home"), "{conflict}");
    }

    #[test]
    fn the_same_scope_does_not_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        assert!(ScopeClaim::of(&scope).conflict_with(&scope).is_none());
    }
}
