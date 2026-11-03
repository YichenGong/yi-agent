//! A database written by a newer build must be refused **without being touched**.
//!
//! "Without being touched" is the hard part and the reason this is an integration
//! test rather than a unit test: opening a SQLite database the ordinary way creates
//! `-wal`/`-shm`, and a write PRAGMA changes bytes and mtime. The assertions below
//! compare the whole file byte-for-byte and require that no sidecar appeared.

use yi_agent_store::schema_compat::{
    SUPPORTED_SCHEMA_MAX, SchemaRange, inspect_schema, require_compatible,
};

fn database_with_version(dir: &tempfile::TempDir, version: u32) -> std::path::PathBuf {
    let db = dir.path().join("runtime.sqlite");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
             INSERT INTO schema_migrations(version) VALUES({version});"
        ))
        .unwrap();
    drop(connection);
    db
}

fn snapshot(db: &std::path::Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(db).unwrap(),
        std::fs::metadata(db).unwrap().modified().unwrap(),
    )
}

#[test]
fn future_schema_is_rejected_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = database_with_version(&dir, 999);
    let (before, before_mtime) = snapshot(&db);

    let error = require_compatible(&db, SchemaRange::supported()).unwrap_err();
    assert!(
        matches!(
            error,
            yi_agent_store::repository::RepositoryError::SchemaIncompatible { .. }
        ),
        "expected a schema-incompatible error, got {error:?}"
    );

    let (after, after_mtime) = snapshot(&db);
    assert_eq!(before, after, "the probe must not modify a single byte");
    assert_eq!(before_mtime, after_mtime, "the probe must not touch mtime");
    assert!(
        !db.with_file_name("runtime.sqlite-wal").exists(),
        "a read-only probe must not create a WAL file"
    );
    assert!(
        !db.with_file_name("runtime.sqlite-journal").exists(),
        "a read-only probe must not create a rollback journal"
    );
    assert!(
        !db.with_file_name("runtime.sqlite-shm").exists(),
        "a read-only probe must not create a shared-memory file"
    );
}

#[test]
fn a_database_at_the_supported_ceiling_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let db = database_with_version(&dir, SUPPORTED_SCHEMA_MAX);
    require_compatible(&db, SchemaRange::supported()).unwrap();
    assert_eq!(inspect_schema(&db).unwrap(), Some(SUPPORTED_SCHEMA_MAX));
}

#[test]
fn one_version_past_the_ceiling_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = database_with_version(&dir, SUPPORTED_SCHEMA_MAX + 1);
    assert!(require_compatible(&db, SchemaRange::supported()).is_err());
}

#[test]
fn an_absent_database_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runtime.sqlite");
    assert_eq!(inspect_schema(&db).unwrap(), None);
    require_compatible(&db, SchemaRange::supported()).unwrap();
    assert!(
        !db.exists(),
        "probing an absent database must not create one"
    );
}

#[test]
fn a_pre_migration_database_reports_version_zero() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runtime.sqlite");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY);")
        .unwrap();
    drop(connection);
    assert_eq!(inspect_schema(&db).unwrap(), Some(0));
    require_compatible(&db, SchemaRange::supported()).unwrap();
}

#[test]
fn a_vanished_migration_table_is_not_mistaken_for_a_future_schema() {
    // A database whose `schema_migrations` rows were never written (an aborted
    // first migration) reads as version 0, which is migratable, not as "newer".
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runtime.sqlite");
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY);")
        .unwrap();
    drop(connection);
    assert_eq!(inspect_schema(&db).unwrap(), Some(0));
}

#[test]
fn a_non_database_file_is_an_error_not_an_empty_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("runtime.sqlite");
    std::fs::write(&db, b"this is not sqlite").unwrap();
    assert!(
        require_compatible(&db, SchemaRange::supported()).is_err(),
        "a corrupt file must be reported, not treated as a fresh database"
    );
}

#[test]
fn the_supported_range_matches_the_migration_ceiling() {
    // The range is derived from the migration constant, so a migration bump can
    // never leave the probe behind. Asserting the derivation, not a literal: a
    // literal here would fail on every later migration for the wrong reason.
    assert_eq!(
        SUPPORTED_SCHEMA_MAX as i64,
        yi_agent_store::repository::LATEST_SCHEMA_VERSION
    );
    assert_eq!(yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MIN, 0);
    assert!(SchemaRange::supported().contains(SUPPORTED_SCHEMA_MAX));
    assert!(!SchemaRange::supported().contains(SUPPORTED_SCHEMA_MAX + 1));
}

/// `migrate` itself must refuse a future schema, not just the pre-flight probe.
///
/// A probe that nobody calls is decoration. This pins the guard at the place where
/// a daemon would otherwise write: opening an existing database whose
/// `schema_migrations` ceiling is past this build must fail, and must leave the
/// file byte-identical.
#[test]
fn opening_a_future_database_through_the_repository_is_refused() {
    use yi_agent_store::repository::{LATEST_SCHEMA_VERSION, RepositoryError, RuntimeRepository};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime.sqlite");

    // Build a database this build understands, then push its recorded ceiling one
    // past the newest migration — exactly what a newer build would leave behind.
    {
        let repository = RuntimeRepository::open(&path).unwrap();
        drop(repository);
    }
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO schema_migrations (version) VALUES (?1)",
                [LATEST_SCHEMA_VERSION + 1],
            )
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();

    let error = match RuntimeRepository::open(&path) {
        Ok(_) => panic!("a future schema must not open"),
        Err(error) => error,
    };
    assert!(
        matches!(error, RepositoryError::SchemaIncompatible { .. }),
        "{error:?}"
    );

    // Refusal happens before any write: the file is untouched, and no journal or
    // WAL was created next to it.
    assert_eq!(std::fs::read(&path).unwrap(), before);
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(
            !dir.path().join(format!("runtime.sqlite{suffix}")).exists(),
            "opening a future schema created runtime.sqlite{suffix}"
        );
    }
}
