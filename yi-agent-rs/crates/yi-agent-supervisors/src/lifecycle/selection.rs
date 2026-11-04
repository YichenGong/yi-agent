//! The per-runtime build selection record.
//!
//! This record is a *policy input*, not a source of truth about what is running.
//! It answers "which build did this runtime last commit to", so a still-running
//! older client cannot silently replace the daemon with its own older binary after
//! the daemon exits. It deliberately holds no credentials and no liveness claims:
//! those come from the lock and from probing the runtime identity.
//!
//! Written atomically (temp file → fsync → rename → directory fsync) so a crash
//! mid-write leaves either the old record or the new one, never a truncated one.
//! The temp file is created mode 0600 in the same directory: `rename` is atomic
//! only within one filesystem.

use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use yi_agent_lifecycle::types::{ArtifactIdentity, OperationStatus, RuntimeScope};

use super::CoordinationGuard;

/// What this runtime last chose to run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeSelection {
    /// The artifact the runtime is expected to run.
    pub target: ArtifactIdentity,
    /// The last switch/preview outcome, kept so the UI can explain a failure it
    /// did not itself initiate.
    #[serde(default)]
    pub last_operation: Option<OperationStatus>,
}

/// File name inside the runtime directory.
pub const SELECTION_FILE: &str = "selection.json";

fn selection_path(scope: &RuntimeScope) -> PathBuf {
    scope.runtime_dir.join(SELECTION_FILE)
}

/// Read the record, or `None` when this runtime has never committed to a build.
///
/// A corrupt record is an **error**, not `None`: reporting "no selection" would
/// make a fresh start treat a damaged record as a first start and happily downgrade
/// to whatever binary happens to be available.
pub fn read_selection(scope: &RuntimeScope) -> io::Result<Option<RuntimeSelection>> {
    let bytes = match std::fs::read(selection_path(scope)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Write the record atomically. The guard is required so two coordinators cannot
/// interleave a read-modify-write of the selection.
pub fn write_selection(guard: &CoordinationGuard, selection: &RuntimeSelection) -> io::Result<()> {
    let dir = &guard.scope().runtime_dir;
    std::fs::create_dir_all(dir)?;
    let final_path = dir.join(SELECTION_FILE);
    let temp_path = dir.join(format!(".{SELECTION_FILE}.{}.tmp", std::process::id()));

    let body = serde_json::to_vec_pretty(selection)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp_path)?;
        file.write_all(&body)?;
        // Without this the rename can be durable while the contents are not.
        file.sync_all()?;
    }
    // `rename` replaces atomically inside one directory; a reader sees the old
    // record or the new one.
    if let Err(error) = std::fs::rename(&temp_path, &final_path) {
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    // The rename itself must be durable, which needs the directory's own fsync.
    let dir_file = std::fs::File::open(dir)?;
    dir_file.sync_all()?;
    // Belt and braces: `mode(0o600)` applies at creation, but an existing file
    // keeps whatever mode it had.
    std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
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

    fn test_selection(build_id: &str) -> RuntimeSelection {
        RuntimeSelection {
            target: ArtifactIdentity {
                build_id: build_id.into(),
                display_version: "0.1.13".into(),
                executable: "/tmp/yi-agent".into(),
                sha256: String::new(),
                schema_min: 0,
                schema_max: 15,
                handover_version: 1,
                business_protocol_min: 2,
                business_protocol_max: 2,
                capabilities: vec![],
            },
            last_operation: None,
        }
    }

    #[test]
    fn the_path_lives_in_the_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        assert_eq!(
            selection_path(&scope),
            scope.runtime_dir.join("selection.json")
        );
    }

    #[test]
    fn an_absent_file_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        assert!(read_selection(&scope).unwrap().is_none());
    }

    #[test]
    fn a_write_is_visible_to_a_reader() {
        let dir = tempfile::tempdir().unwrap();
        let scope = test_scope(dir.path());
        let guard =
            CoordinationGuard::acquire(&scope, Deadline::after(Duration::from_secs(1))).unwrap();
        let selection = test_selection("abc");
        write_selection(&guard, &selection).unwrap();
        assert_eq!(read_selection(&scope).unwrap(), Some(selection));
    }
}
