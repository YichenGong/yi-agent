//! One record that says "this install no longer restarts itself".
//!
//! Migrating to the managed runtime removes several *independent* self-healing
//! paths: a launchd job, the per-installation resident registration, and the
//! plugin-install hook that re-installs the job. They live in different crates and
//! run at different times, so removing each one in place would leave the set
//! inconsistent — the classic "uninstalled at startup, reinstalled on the next
//! plugin install" bug.
//!
//! This module gives them one answer to consult instead of three places to patch.
//! The marker is a *record*, not a lock: it says a migration completed, and every
//! self-healing path is required to check it before acting.
//!
//! It deliberately lives beside the resident registration rather than inside it.
//! The registration stays valid configuration data — "this project wants a daemon
//! when a frontend is around" — while the marker says "nobody should start one
//! because no frontend asked". Overloading one file with both meanings would make
//! "delete the registration to stop restarting" look correct, and that would throw
//! away a user's configuration.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Marker file inside the instance directory (beside `resident-daemons.json`).
pub const RETIREMENT_FILE: &str = "self-heal-retired.json";

/// What the record contains. Only explanatory fields: nothing here is a credential
/// or a liveness claim, so a stale file cannot break a later start on its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfHealRetirement {
    /// Seconds since the epoch, for the UI to explain *when* the migration ran.
    /// Never used for a decision: decisions compare wall-clock-free state.
    pub retired_at_unix: u64,
    /// Why the migration happened, in one line. Prose for a human; no logic reads it.
    pub reason: String,
}

pub fn path(instance_home: &Path) -> PathBuf {
    instance_home.join(RETIREMENT_FILE)
}

/// Whether every self-healing path must stand down.
///
/// **Existence is the signal, not the contents.** A half-written or damaged record
/// is treated as retired on purpose: the conservative direction is "do not start
/// a background process nobody asked for". The opposite default would let a corrupt
/// file resurrect the old supervisor, which is the exact failure this record exists
/// to prevent.
pub fn is_retired(instance_home: &Path) -> bool {
    path(instance_home).exists()
}

/// Records the retirement. Idempotent: retiring twice keeps the first reason.
///
/// Written atomically (temp + rename) so a process killed mid-write cannot leave a
/// truncated file that some future reader might decide to interpret.
pub fn retire(instance_home: &Path, reason: &str) -> std::io::Result<SelfHealRetirement> {
    if let Some(existing) = read(instance_home) {
        return Ok(existing);
    }
    let record = SelfHealRetirement {
        retired_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or(0),
        reason: reason.to_string(),
    };
    std::fs::create_dir_all(instance_home)?;
    let final_path = path(instance_home);
    let temp_path = instance_home.join(format!(".{RETIREMENT_FILE}.{}.tmp", std::process::id()));
    let body = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
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
    std::fs::rename(&temp_path, &final_path)?;
    Ok(record)
}

/// Reads the record, or `None` when it is absent or unreadable.
///
/// `None` here means "cannot explain", *not* "not retired" — callers must use
/// [`is_retired`] for the decision. Keeping the two separate stops an unreadable
/// file from being mistaken for permission to restart.
pub fn read(instance_home: &Path) -> Option<SelfHealRetirement> {
    let bytes = std::fs::read(path(instance_home)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Existence decides, so a damaged record must still stop the old paths.
    ///
    /// The tempting alternative — "unreadable, therefore not retired" — would let a
    /// truncated file bring back the supervisor this record was written to stop.
    #[test]
    fn a_damaged_record_still_counts_as_retired() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_retired(dir.path()));
        std::fs::write(path(dir.path()), "{ not json").unwrap();
        assert!(is_retired(dir.path()));
        assert_eq!(read(dir.path()), None, "no explanation is available");
    }

    /// Retiring twice keeps the first reason: the record describes when the
    /// migration *happened*, and a later call is not a second migration.
    #[test]
    fn retiring_twice_keeps_the_first_record() {
        let dir = tempfile::tempdir().unwrap();
        let first = retire(dir.path(), "managed runtime took over").unwrap();
        let second = retire(dir.path(), "something else").unwrap();
        assert_eq!(first, second);
        assert_eq!(
            read(dir.path()).unwrap().reason,
            "managed runtime took over"
        );
    }

    /// The record carries no secret and no executable path, so it is safe to keep
    /// in a directory that a diagnostic command may print.
    #[test]
    fn the_record_holds_no_paths_or_credentials() {
        let dir = tempfile::tempdir().unwrap();
        retire(dir.path(), "managed runtime took over").unwrap();
        let text = std::fs::read_to_string(path(dir.path())).unwrap();
        assert!(!text.contains(dir.path().to_string_lossy().as_ref()));
        assert!(text.contains("managed runtime took over"));
    }
}
