//! Read-only schema compatibility probing.
//!
//! The daemon performs migrations, task recovery, and orphan reclamation the
//! moment it opens a database. If the database was written by a *newer* build,
//! those writes run against a schema this build does not understand. The existing
//! migration gate (`current_version >= LATEST_SCHEMA_VERSION` → success) makes it
//! worse by treating "newer" as "up to date".
//!
//! So the check that matters happens **before** any of that, and it must be
//! read-only: opening the database normally, or issuing a write PRAGMA, would
//! itself create files and mutate state (a read-only violation the tests pin by
//! byte-comparing the database and asserting no `-wal` appears).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::repository::{LATEST_SCHEMA_VERSION, RepositoryError};

/// Oldest schema this build can read and migrate.
///
/// Zero is deliberate: a database whose `schema_migrations` table exists but holds
/// no rows (an aborted first migration) is version 0, and `migrate` knows how to
/// build it up. Excluding 0 would refuse exactly the recoverable case.
pub const SUPPORTED_SCHEMA_MIN: u32 = 0;

/// The newest schema this build can migrate to and read.
pub const SUPPORTED_SCHEMA_MAX: u32 = LATEST_SCHEMA_VERSION as u32;

/// An inclusive schema version range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchemaRange {
    pub min: u32,
    pub max: u32,
}

impl SchemaRange {
    /// The range this build supports.
    pub fn supported() -> Self {
        Self {
            min: SUPPORTED_SCHEMA_MIN,
            max: SUPPORTED_SCHEMA_MAX,
        }
    }

    pub fn contains(&self, version: u32) -> bool {
        version >= self.min && version <= self.max
    }
}

impl Default for SchemaRange {
    fn default() -> Self {
        Self::supported()
    }
}

/// Read the schema version without writing anything.
///
/// Returns `Ok(None)` when the database file does not exist yet: a first start
/// has nothing to be incompatible with. An existing but unreadable or
/// non-database file is an error, not `None`, because treating it as "no database"
/// would let the daemon create over a corrupted one.
pub fn inspect_schema(path: &Path) -> Result<Option<u32>, RepositoryError> {
    if !path.exists() {
        return Ok(None);
    }
    // READ_ONLY plus a plain open: no journal/WAL creation, no PRAGMA writes, no
    // `schema_migrations` table creation (that is `migrate`'s job).
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // A database with no `schema_migrations` table yet is a pre-migration file;
    // report version 0 so a caller can distinguish it from "absent".
    let has_table: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(Some(0));
    }
    let version: i64 = connection.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )?;
    Ok(Some(version as u32))
}

/// Reject a database this build cannot safely handle, before any write.
///
/// A future schema is refused explicitly. A schema below `range.min` is also
/// refused: this build has no migration path back that far, and guessing is how
/// a downgrade destroys data.
pub fn require_compatible(path: &Path, range: SchemaRange) -> Result<(), RepositoryError> {
    let Some(version) = inspect_schema(path)? else {
        return Ok(());
    };
    if version > range.max {
        return Err(RepositoryError::SchemaIncompatible {
            reason: format!(
                "database schema v{version} is newer than the newest supported v{}; \
                 refusing to open it read-write",
                range.max
            ),
        });
    }
    if version < range.min {
        return Err(RepositoryError::SchemaIncompatible {
            reason: format!(
                "database schema v{version} is older than the oldest supported v{}",
                range.min
            ),
        });
    }
    Ok(())
}
