//! Tests for side resource ownership, attachment copies and cleanup.
//!
//! The properties are the ones a user can observe going wrong: a slot that is
//! released before its directory is gone, a copy that reads outside the parent's
//! authorisation, a budget that is only checked after unbounded work, and cleanup
//! that blocks the caller.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::*;
use crate::side_conversation::source::{
    EffectiveSideSource, MANIFEST_PATH_PREFIX, SideAttachment, SideAttachmentCatalog,
};
use crate::side_conversation::test_support::{CapturingProvider, StaticRoots, source_with_history};

fn host(dir: &tempfile::TempDir) -> Arc<SideHostResources> {
    SideHostResources::new(
        dir.path().to_path_buf(),
        Arc::new(SideBlockingPool::new(8, 2, 8, 2)),
    )
    .unwrap()
}

fn side_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// A feasible fixture: a private source root and a file the catalog names.
struct Fixture {
    /// Kept alive so the instance home outlives the host that uses it.
    _instance: tempfile::TempDir,
    workspace: tempfile::TempDir,
    host: Arc<SideHostResources>,
}

impl Fixture {
    fn new() -> Self {
        let instance = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let host = host(&instance);
        Self {
            _instance: instance,
            workspace,
            host,
        }
    }

    /// Build a source whose catalog points at `relative`, with the manifest line
    /// present in the history so the entry survives resolution.
    fn source_with_attachment(&self, relative: &str, contents: &[u8]) -> EffectiveSideSource {
        let path = self.workspace.path().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        let marker = format!(
            "{}0-{}",
            MANIFEST_PATH_PREFIX,
            path.file_name().unwrap().to_string_lossy()
        );
        let mut source = source_with_history(
            &format!("总结一下\n\nAttached files (read them with read_document):\n- {marker}\n"),
            "父回答",
        );
        source.primary = self.workspace.path().to_path_buf();
        source.roots = vec![self.workspace.path().to_path_buf()];
        source.current_roots = Arc::new(StaticRoots(vec![self.workspace.path().to_path_buf()]));
        source.attachments = SideAttachmentCatalog(vec![SideAttachment {
            original: path,
            manifest_marker: marker,
        }]);
        source.provider = Arc::new(CapturingProvider::new("旁支回答"));
        source
    }
}

#[test]
fn side_resource_slot_is_held_until_last_owner() {
    let root = tempfile::tempdir().unwrap();
    let pool = Arc::new(SideBlockingPool::new(8, 2, 8, 2));
    let host = SideHostResources::new(root.path().into(), pool).unwrap();
    let lease = host
        .reserve_slot("00000000-0000-4000-8000-000000000001")
        .unwrap();
    let worker_owner = Arc::clone(&lease);
    drop(lease);
    assert_eq!(host.slots_in_use(), 1);
    drop(worker_owner);
    host.wait_for_cleanup_blocking(Duration::from_secs(2))
        .unwrap();
    assert_eq!(host.slots_in_use(), 0);
}

#[test]
fn side_resource_slot_is_idempotent_and_capped_at_eight() {
    let fixture = Fixture::new();
    let first = side_id();
    let lease = fixture.host.reserve_slot(&first).unwrap();
    let same = fixture.host.reserve_slot(&first).unwrap();
    assert!(
        Arc::ptr_eq(&lease, &same),
        "re-reserving a side must not take a second slot"
    );
    assert_eq!(fixture.host.slots_in_use(), 1);

    // Hold every lease: a dropped lease releases its slot asynchronously, so a
    // test that dropped them would be measuring cleanup, not the cap.
    let mut held = vec![lease];
    for _ in 1..SIDE_RESOURCE_SLOTS {
        held.push(fixture.host.reserve_slot(&side_id()).unwrap());
    }
    assert_eq!(fixture.host.slots_in_use(), SIDE_RESOURCE_SLOTS);

    let error = fixture.host.reserve_slot(&side_id()).unwrap_err();
    assert_eq!(error.reason, SideErrorReason::SideLimit);
    drop(held);
}

#[test]
fn side_blocking_cleanup_keeps_lease_and_resource_slot() {
    // The slot must survive until the directory is actually gone: a worker holding
    // the lease keeps it, and only the last drop queues the delete.
    let fixture = Fixture::new();
    let id = side_id();
    let lease = fixture.host.reserve_slot(&id).unwrap();
    let dir = fixture.host.side_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("copy.txt"), b"data").unwrap();

    let worker = Arc::clone(&lease);
    drop(lease);
    assert_eq!(
        fixture.host.slots_in_use(),
        1,
        "the worker's handle must keep the slot"
    );
    assert!(dir.exists());
    drop(worker);
    fixture
        .host
        .wait_for_cleanup_blocking(Duration::from_secs(2))
        .unwrap();
    assert_eq!(fixture.host.slots_in_use(), 0);
    assert!(!dir.exists(), "the directory must really be deleted");
}

#[test]
fn side_host_lock_is_per_host_directory() {
    // Two hosts under one instance home must both start: each locks its own
    // directory, so neither path can be reclaimed by the other.
    let dir = tempfile::tempdir().unwrap();
    let first = host(&dir);
    let second = host(&dir);
    assert_ne!(first.host_instance_id(), second.host_instance_id());
    assert!(first.side_dir("x").starts_with(dir.path()));
    // Reclaiming on the second start must not have removed the first's directory.
    let first_dir = first.side_dir("keep").parent().unwrap().to_path_buf();
    assert!(
        first_dir.exists(),
        "a locked host directory must never be reclaimed"
    );
}

#[test]
fn side_startup_reclaims_only_unlocked_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(SIDE_CONVERSATIONS_DIR);
    std::fs::create_dir_all(&root).unwrap();

    // A dead host: a well-formed directory nobody locks.
    let dead = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(dead.join("orphan")).unwrap();
    std::fs::write(dead.join("orphan/copy.txt"), b"left behind").unwrap();

    // A live host: locked by another process-like holder.
    let live = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&live).unwrap();
    let holder = try_lock_directory(&live).unwrap();

    // A non-UUID directory must be left alone entirely.
    let foreign = root.join("not-a-host-id");
    std::fs::create_dir_all(&foreign).unwrap();

    let host = SideHostResources::new(
        dir.path().to_path_buf(),
        Arc::new(SideBlockingPool::new(4, 1, 4, 1)),
    )
    .unwrap();

    assert!(!dead.exists(), "an unlocked old host must be reclaimed");
    assert!(
        live.exists(),
        "a live host holds its lock and must never be reclaimed"
    );
    assert!(foreign.exists(), "only host-id directories are considered");
    drop(holder);
    drop(host);
}

#[tokio::test]
async fn side_prepare_creates_no_directory_and_copies_no_attachment() {
    let fixture = Fixture::new();
    let source = fixture.source_with_attachment("报告.txt", b"report body");
    // An inline image is the only binary the side inherits.
    source
        .session
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::assistant(vec![
            yi_agent_core::ContentBlock::Image {
                source: yi_agent_core::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "iVBORw0KGgo=".into(),
                },
                detail: yi_agent_core::ImageDetail::High,
                path: None,
            },
        ]));

    let lease = fixture.host.reserve_slot(&side_id()).unwrap();
    let dir = lease.dir().to_path_buf();
    let prepared = fixture
        .host
        .prepare(Arc::clone(&lease), &source, CancellationToken::new())
        .await
        .unwrap();

    assert!(
        prepared.binary_bytes > 0 && prepared.binary_bytes < SIDE_BINARY_BUDGET_BYTES,
        "only the inherited inline media counts: {}",
        prepared.binary_bytes
    );
    assert!(
        !dir.exists(),
        "preparing a side must not create a private directory"
    );
    assert!(
        !source.roots.iter().any(|root| root == &dir),
        "no private directory may become a root"
    );
}

#[tokio::test]
async fn side_prepare_refuses_a_snapshot_whose_inline_media_exceeds_the_budget() {
    let fixture = Fixture::new();
    let source = source_with_history("q", "a");
    // Pretend the history already carries more than the allowance: no IO runs, so
    // the refusal is about the accounting, not about reading a huge file.
    let block = yi_agent_core::ContentBlock::Image {
        source: yi_agent_core::ImageSource::Base64 {
            media_type: "image/png".into(),
            // Base64 decodes to ~3/4 of its length, so 90 MB of padding is
            // ~67.5 MiB decoded, just past the 64 MiB allowance.
            data: "A".repeat(90_000_000),
        },
        detail: yi_agent_core::ImageDetail::High,
        path: None,
    };
    source
        .session
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::assistant(vec![block]));
    let lease = fixture.host.reserve_slot(&side_id()).unwrap();
    let error = fixture
        .host
        .prepare(lease, &source, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.reason, SideErrorReason::ResourceLimit);
}

#[tokio::test]
async fn side_prepare_refuses_after_cancel_before_touching_anything() {
    let fixture = Fixture::new();
    let source = fixture.source_with_attachment("report.txt", b"body");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let lease = fixture.host.reserve_slot(&side_id()).unwrap();
    let dir = lease.dir().to_path_buf();
    let error = fixture
        .host
        .prepare(lease, &source, cancel)
        .await
        .unwrap_err();
    assert_eq!(error.reason, SideErrorReason::ResourceLimit);
    assert!(!dir.exists());
}

/// The failure this guards: a delete that fails must not free the slot, or the
/// quota reports capacity over a directory that still exists.
#[tokio::test]
async fn side_cleanup_failure_keeps_the_slot_and_a_retry_can_finish_it() {
    let instance = tempfile::tempdir().unwrap();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let remove = {
        let fail = Arc::clone(&fail);
        Arc::new(move |_: &std::path::Path| {
            if fail.load(std::sync::atomic::Ordering::SeqCst) {
                Err(std::io::Error::other("injected"))
            } else {
                Ok(())
            }
        })
    };
    let host = SideHostResources::new_with_cleanup(
        instance.path().to_path_buf(),
        Arc::new(SideBlockingPool::new(8, 2, 8, 2)),
        remove,
    )
    .unwrap();

    let id = side_id();
    let lease = host.reserve_slot(&id).unwrap();
    let mut receipt = host.cleanup_receipt(&id).unwrap();
    assert_eq!(receipt.state(), SideCleanupState::Pending);

    drop(lease);
    let error = receipt.wait(Duration::from_secs(5)).await.unwrap_err();
    assert_eq!(error.message, "cleanup_failed");
    assert_eq!(host.slots_in_use(), 1, "a failed delete must keep its slot");
    assert_eq!(host.failed_cleanups(), 1);

    // The retry only becomes legal once no owner is left, which it now is.
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    let mut retried = host.retry_cleanup(&id).unwrap();
    retried.wait(Duration::from_secs(5)).await.unwrap();
    assert_eq!(host.slots_in_use(), 0);
    assert_eq!(host.completed_cleanups(), 1);
}

#[tokio::test]
async fn side_cleanup_receipt_stays_pending_while_another_owner_holds_the_lease() {
    let fixture = Fixture::new();
    let id = side_id();
    let lease = fixture.host.reserve_slot(&id).unwrap();
    let worker = Arc::clone(&lease);
    let mut receipt = fixture.host.cleanup_receipt(&id).unwrap();
    drop(lease);
    assert_eq!(receipt.state(), SideCleanupState::Pending);
    let error = receipt.wait(Duration::from_millis(10)).await.unwrap_err();
    assert_eq!(error.message, "cleanup_timeout");
    drop(worker);
    receipt.wait(Duration::from_secs(5)).await.unwrap();
    assert_eq!(fixture.host.slots_in_use(), 0);
}

#[test]
fn side_manifest_markers_only_come_from_generated_paths() {
    let message = yi_agent_core::Message::user(
        "look at src/main.rs and /etc/passwd, and .yi-agent/attachments/0-x.pdf",
    );
    let markers = manifest_markers(&message);
    assert_eq!(markers, vec![".yi-agent/attachments/0-x.pdf".to_string()]);
}

#[tokio::test]
async fn side_cleanup_close_reports_live_work_honestly() {
    // Deterministic: one worker thread is pinned by a blocker, so a queued cleanup
    // provably cannot run. `close` must then report that work is still live instead
    // of claiming the resources were released.
    let dir = tempfile::tempdir().unwrap();
    let pool = Arc::new(SideBlockingPool::new(1, 1, 4, 4));
    let host = SideHostResources::new(dir.path().to_path_buf(), Arc::clone(&pool)).unwrap();

    let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let blocker = {
        let gate = Arc::clone(&gate);
        let pool = Arc::clone(&pool);
        let owner: Arc<dyn Send + Sync> = Arc::new(());
        tokio::spawn(async move {
            pool.run("blocker", CancellationToken::new(), owner, move || {
                let (lock, condvar) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open {
                    open = condvar.wait(open).unwrap();
                }
                Ok(())
            })
            .await
        })
    };
    while pool.stats().running == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let id = side_id();
    let lease = host.reserve_slot(&id).unwrap();
    let side_dir = host.side_dir(&id);
    std::fs::create_dir_all(&side_dir).unwrap();
    drop(lease);

    // The cleanup is admitted but stuck behind the blocker.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while host.live_cleanups() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the cleanup job was never admitted"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !host.close(Duration::from_millis(10)),
        "close must report live work, not a release"
    );
    assert_eq!(
        host.slots_in_use(),
        1,
        "the slot must stay held while its cleanup has not run"
    );
    assert!(side_dir.exists());

    // Release the blocker: the cleanup now runs and really deletes the tree.
    {
        let (lock, condvar) = &*gate;
        *lock.lock().unwrap() = true;
        condvar.notify_all();
    }
    host.wait_for_cleanup_blocking(Duration::from_secs(2))
        .unwrap();
    blocker.await.unwrap().unwrap();
    assert_eq!(host.slots_in_use(), 0);
    assert!(!side_dir.exists());
    assert_eq!(host.completed_cleanups(), 1);
    assert_eq!(host.failed_cleanups(), 0);
}

#[test]
fn side_instance_home_is_created_under_the_given_root() {
    let dir = tempfile::tempdir().unwrap();
    let host = host(&dir);
    assert_eq!(host.instance_home(), dir.path());
    assert!(
        dir.path().join(SIDE_CONVERSATIONS_DIR).is_dir(),
        "the host must create its private root under the instance home"
    );
    // The host directory is owner-only.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(host.side_dir("x").parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700, "the host directory must be private");
}

#[test]
fn side_attachment_catalog_resolves_only_retained_markers() {
    let mut source = source_with_history("- .yi-agent/attachments/0-kept.pdf", "a");
    source.attachments = SideAttachmentCatalog(vec![
        SideAttachment {
            original: PathBuf::from("/tmp/kept.pdf"),
            manifest_marker: ".yi-agent/attachments/0-kept.pdf".into(),
        },
        SideAttachment {
            original: PathBuf::from("/tmp/trimmed.pdf"),
            manifest_marker: ".yi-agent/attachments/1-trimmed.pdf".into(),
        },
    ]);
    let resolved = source.attachments.resolve(&source.session).unwrap();
    assert_eq!(resolved, vec![PathBuf::from("/tmp/kept.pdf")]);
}
