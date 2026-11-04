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

/// Roots the test can empty while a handle is already alive.
#[derive(Default)]
struct RevocableRoots(std::sync::Mutex<Vec<PathBuf>>);

impl RevocableRoots {
    fn new(paths: Vec<PathBuf>) -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(paths)))
    }

    fn revoke(&self) {
        self.0.lock().unwrap().clear();
    }
}

impl yi_agent_tools::SideRoots for RevocableRoots {
    fn current_roots(&self) -> Vec<PathBuf> {
        self.0.lock().unwrap().clone()
    }
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

#[test]
fn side_resource_limits_are_enforced_before_unbounded_work() {
    let fixture = Fixture::new();
    let source = fixture.source_with_attachment("huge.bin", &vec![0u8; 1024]);
    // Shrink the allowance by pretending the history already holds most of it.
    let dir = tempfile::tempdir().unwrap();
    let plan = plan_copies(&source).unwrap();
    assert_eq!(
        plan.len(),
        1,
        "the manifest marker is present, so it copies"
    );

    let copy_source = CopySource {
        provider_primary: source.primary.clone(),
        created_roots: source.roots.clone(),
        current_roots: Arc::clone(&source.current_roots),
        inline_bytes: SIDE_BINARY_BUDGET_BYTES,
    };
    let error = copy_attachments(&copy_source, &plan, dir.path()).unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidData,
        "a spent budget must be refused up front: {error}"
    );
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "nothing may be written once the budget is exhausted"
    );

    // An empty attachment must be refused too: a zero-byte copy would otherwise
    // look like a successful copy under a spent budget.
    let empty = fixture.source_with_attachment("empty.txt", b"");
    let empty_plan = plan_copies(&empty).unwrap();
    assert_eq!(empty_plan.len(), 1);
    let spent = CopySource {
        provider_primary: empty.primary.clone(),
        created_roots: empty.roots.clone(),
        current_roots: Arc::clone(&empty.current_roots),
        inline_bytes: SIDE_BINARY_BUDGET_BYTES,
    };
    let error = copy_attachments(&spent, &empty_plan, dir.path()).unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidData,
        "a spent budget must refuse even an empty attachment: {error}"
    );
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "no copy may exist for a refused attachment"
    );
}

#[tokio::test]
async fn side_inherits_inline_images_and_copies_catalog_files() {
    let fixture = Fixture::new();
    let source = fixture.source_with_attachment("报告.txt", b"report body");
    // An inline image whose original path is gone still counts as binary.
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
    let prepared = fixture
        .host
        .prepare(Arc::clone(&lease), &source, CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(
        prepared.binary_bytes,
        "report body".len(),
        "only the copied file counts as copied bytes"
    );
    // The copy exists in the side's private directory.
    let copied: Vec<_> = std::fs::read_dir(prepared.lease.dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(copied.len(), 1, "{copied:?}");
    assert!(copied[0].ends_with("报告.txt"), "{copied:?}");

    // The alias resolves to the copy, and reads work through the side handle.
    let file = prepared
        .access
        .open_regular(&source.attachments.0[0].original)
        .unwrap();
    use std::io::Read;
    let mut bytes = Vec::new();
    let mut file = file;
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"report body");

    // The private directory must never become a general root.
    assert!(
        !source.roots.iter().any(|root| root == prepared.lease.dir()),
        "the side's private directory must not be added to the parent's roots"
    );
}

#[tokio::test]
async fn side_attachment_copy_is_revoked_with_its_original() {
    let fixture = Fixture::new();
    let roots = RevocableRoots::new(vec![fixture.workspace.path().to_path_buf()]);
    let mut source = fixture.source_with_attachment("doc.txt", b"secret body");
    source.current_roots = roots.clone();

    let lease = fixture.host.reserve_slot(&side_id()).unwrap();
    let prepared = fixture
        .host
        .prepare(Arc::clone(&lease), &source, CancellationToken::new())
        .await
        .unwrap();
    let original = source.attachments.0[0].original.clone();

    // While the original is authorised, the alias serves its copy.
    assert!(prepared.access.open_regular(&original).is_ok());
    assert!(prepared.access.open_regular(prepared.lease.dir()).is_err());

    // Prove the read really went through the copy, not the parent's file: after
    // the original is replaced, the alias must still return the copied bytes.
    let copied_file = std::fs::read_dir(prepared.lease.dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .next()
        .expect("a private copy exists");
    assert_eq!(std::fs::read(&copied_file).unwrap(), b"secret body");

    // Revoke the parent's root. The *same* handle must stop serving the alias:
    // otherwise the copy would be a way around revocation.
    roots.revoke();
    let error = prepared.access.open_regular(&original).unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "{error}"
    );

    // Restoring the root restores the alias, proving the decision is live rather
    // than a one-way latch.
    *roots.0.lock().unwrap() = vec![fixture.workspace.path().to_path_buf()];
    assert!(prepared.access.open_regular(&original).is_ok());
}

#[tokio::test]
async fn side_attachment_is_skipped_when_its_marker_left_the_safe_history() {
    let fixture = Fixture::new();
    let source = fixture.source_with_attachment("gone.txt", b"x");
    // Drop the manifest line: the model can no longer see the reference.
    source
        .session
        .lock()
        .unwrap()
        .replace_messages(vec![yi_agent_core::Message::user("no attachments here")]);
    let plan = plan_copies(&source).unwrap();
    assert!(
        plan.is_empty(),
        "an attachment whose manifest line was trimmed must not be copied"
    );

    let lease = fixture.host.reserve_slot(&side_id()).unwrap();
    let prepared = fixture
        .host
        .prepare(lease, &source, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(prepared.binary_bytes, 0);
}

#[tokio::test]
async fn side_attachment_cannot_escape_the_parent_root() {
    let fixture = Fixture::new();
    // The catalog names a file outside every authorised root.
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, b"outside").unwrap();
    let marker = format!("{}0-secret.txt", MANIFEST_PATH_PREFIX);
    let mut source = source_with_history(&format!("- {marker}\n"), "a");
    source.primary = fixture.workspace.path().to_path_buf();
    source.roots = vec![fixture.workspace.path().to_path_buf()];
    source.current_roots = Arc::new(StaticRoots(vec![fixture.workspace.path().to_path_buf()]));
    source.attachments = SideAttachmentCatalog(vec![SideAttachment {
        original: secret.clone(),
        manifest_marker: marker,
    }]);

    let dir = tempfile::tempdir().unwrap();
    let plan = plan_copies(&source).unwrap();
    assert_eq!(plan.len(), 1);
    let copy_source = CopySource {
        provider_primary: source.primary.clone(),
        created_roots: source.roots.clone(),
        current_roots: Arc::clone(&source.current_roots),
        inline_bytes: 0,
    };
    let error = copy_attachments(&copy_source, &plan, dir.path()).unwrap_err();
    assert!(
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        ),
        "an out-of-root attachment must be refused by the access handle: {error}"
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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
