use tempfile::TempDir;
use yi_agent_core::subagent::task::{RootSessionId, TaskId};
use yi_agent_store::repository::{PersistedAdmissionCursor, RuntimeRepository};

#[test]
fn admission_cursor_round_trips_across_repository_reopen() {
    let directory = TempDir::new().unwrap();
    let database = directory.path().join("runtime.sqlite");
    let root = RootSessionId::new();
    let parent = TaskId::new();
    let mut repository = RuntimeRepository::open(&database).unwrap();

    repository
        .save_admission_cursor("resident:global", Some(&root), Some(&parent), 42)
        .unwrap();
    drop(repository);

    let repository = RuntimeRepository::open(&database).unwrap();
    assert_eq!(
        repository.admission_cursor("resident:global").unwrap(),
        Some(PersistedAdmissionCursor {
            root_id: Some(root),
            parent_id: Some(parent),
            sequence: 42,
        })
    );
}
