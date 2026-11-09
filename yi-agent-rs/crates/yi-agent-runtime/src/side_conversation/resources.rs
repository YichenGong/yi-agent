//! Per-side resource slots and their cleanup.
//!
//! A side no longer gets a private directory or attachment copies: it answers from
//! the conversation snapshot, and anything it needs is already embedded in that
//! snapshot. What remains here is the accounting that keeps repeated open/close
//! honest. Three invariants hold:
//!
//! 1. **A slot is real.** A side holds one of eight slots from creation until its
//!    (possibly absent) tree is actually gone, so cycling open/close cannot escape
//!    the quota.
//! 2. **Cleanup never blocks its initiator.** The last owner's `Drop` submits a
//!    bounded job to the host's blocking pool; it never deletes inline.
//! 3. **A failed delete keeps the slot.** Removing the slot on failure would report
//!    a free slot over a directory that still exists; only a confirmed delete (or
//!    a confirmed "nothing to delete") frees it, and a receipt says which happened.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use yi_agent_core::{ContentBlock, Session, measure_message_size};
use yi_agent_tools::SideBlockingPool;

use super::cleanup::{self, SideCleanupReceipt, SideCleanupState};
use super::source::EffectiveSideSource;
use super::{SideError, SideErrorReason, SideResult};

/// How one slot's tree is deleted.
///
/// Always [`remove_tree`] in production; a test injects a failure so the
/// "a failed delete keeps its slot" rule is provable without a real IO error.
type RemoveTree = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

/// Resource slots one host may hand out.
///
/// Creating, open, closing and not-yet-drained cleanup resources all count, so a
/// client cannot escape the quota by cycling open/close.
pub const SIDE_RESOURCE_SLOTS: usize = 8;

/// The per-side binary ceiling: decoded inline images in the inherited snapshot.
pub const SIDE_BINARY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Directory name under the instance home.
pub const SIDE_CONVERSATIONS_DIR: &str = "side-conversations";

/// A resource limit failure: the side asked for more than its allowance.
fn resource_limit(detail: impl std::fmt::Display) -> SideError {
    SideError::new(SideErrorReason::ResourceLimit, detail.to_string())
}

/// Everything the resource layer produced for one side.
#[derive(Debug)]
pub struct PreparedSideResources {
    pub lease: Arc<SideResourceLease>,
    /// Binary bytes committed to the side's budget: the decoded inline media
    /// already carried by the snapshot. No attachment file is copied any more.
    pub binary_bytes: usize,
}

/// A held exclusive lock; freed when the file closes.
///
/// Deliberately never deleted: removing a locked file lets a third process lock
/// the new inode while the first still believes it holds the directory.
struct HostLock {
    _file: std::fs::File,
}

/// One host's side conversations: id, directory lock and slot accounting.
pub struct SideHostResources {
    instance_home: PathBuf,
    host_instance_id: String,
    host_dir: PathBuf,
    _lock: HostLock,
    pool: Arc<SideBlockingPool>,
    /// How a slot's tree is deleted. Production always uses [`remove_tree`]; the
    /// indirection exists so a test can prove a failed delete keeps its slot
    /// instead of waiting for a real filesystem error that cannot be provoked.
    remove: RemoveTree,
    /// One entry per held slot. The map holds only a `Weak` reference: owning the
    /// lease here would keep its refcount above zero forever, so cleanup could
    /// never run and the slot would leak.
    slots: Mutex<HashMap<String, SlotEntry>>,
    /// Cleanup jobs queued but not finished.
    live_cleanups: Arc<AtomicUsize>,
    /// Cleanup jobs that finished.
    completed_cleanups: Arc<AtomicUsize>,
    /// Cleanup jobs that failed and were left for startup reclamation.
    failed_cleanups: Arc<AtomicUsize>,
    closed: AtomicBool,
}

impl SideHostResources {
    /// Create a host behind an `Arc`, take its directory lock and reclaim any
    /// provably dead older host.
    pub fn new(instance_home: PathBuf, pool: Arc<SideBlockingPool>) -> SideResult<Arc<Self>> {
        Self::assemble(instance_home, pool, Arc::new(|dir: &Path| remove_tree(dir)))
    }

    #[cfg(test)]
    pub fn new_with_cleanup(
        instance_home: PathBuf,
        pool: Arc<SideBlockingPool>,
        remove: RemoveTree,
    ) -> SideResult<Arc<Self>> {
        Self::assemble(instance_home, pool, remove)
    }

    fn assemble(
        instance_home: PathBuf,
        pool: Arc<SideBlockingPool>,
        remove: RemoveTree,
    ) -> SideResult<Arc<Self>> {
        let host_instance_id = uuid::Uuid::new_v4().to_string();
        let root = instance_home.join(SIDE_CONVERSATIONS_DIR);
        std::fs::create_dir_all(&root).map_err(|error| {
            resource_limit(format!("无法创建旁支目录 {}：{error}", root.display()))
        })?;

        reclaim_dead_hosts(&root, &host_instance_id);

        let host_dir = root.join(&host_instance_id);
        std::fs::create_dir_all(&host_dir).map_err(|error| {
            resource_limit(format!("无法创建宿主目录 {}：{error}", host_dir.display()))
        })?;
        restrict_to_owner(&host_dir)?;
        let lock = lock_directory(&host_dir)?;

        Ok(Arc::new(Self {
            instance_home,
            host_instance_id,
            host_dir,
            _lock: lock,
            pool,
            remove,
            slots: Mutex::new(HashMap::new()),
            live_cleanups: Arc::new(AtomicUsize::new(0)),
            completed_cleanups: Arc::new(AtomicUsize::new(0)),
            failed_cleanups: Arc::new(AtomicUsize::new(0)),
            closed: AtomicBool::new(false),
        }))
    }

    pub fn host_instance_id(&self) -> &str {
        &self.host_instance_id
    }

    pub fn instance_home(&self) -> &Path {
        &self.instance_home
    }

    /// Where a side's private directory lives.
    pub fn side_dir(&self, side_id: &str) -> PathBuf {
        self.host_dir.join(side_id)
    }

    /// How many slots are held, including slots whose cleanup is still running.
    pub fn slots_in_use(&self) -> usize {
        self.slots.lock().unwrap().len()
    }

    /// Take one of the eight slots for `side_id`.
    ///
    /// Idempotent: a side that already holds a slot gets the same lease, so it
    /// cannot consume a second one.
    pub fn reserve_slot(self: &Arc<Self>, side_id: &str) -> SideResult<Arc<SideResourceLease>> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(resource_limit("宿主正在关闭，不再接受新的旁支"));
        }
        let mut slots = self.slots.lock().unwrap();
        if let Some(existing) = slots.get(side_id).and_then(|entry| entry.lease.upgrade()) {
            return Ok(existing);
        }
        // A dead entry cannot happen while its lease is alive, but never let a
        // stale one keep a real side from its slot.
        slots.remove(side_id);
        if slots.len() >= SIDE_RESOURCE_SLOTS {
            return Err(SideError::new(
                SideErrorReason::SideLimit,
                format!("每个宿主最多 {SIDE_RESOURCE_SLOTS} 个旁支资源槽"),
            ));
        }
        let dir = self.host_dir.join(side_id);
        let lease = Arc::new(SideResourceLease {
            side_id: side_id.to_string(),
            dir: dir.clone(),
            host: Arc::downgrade(self),
            released: AtomicBool::new(false),
        });
        let (receipt, keepalive) = cleanup::channel();
        slots.insert(
            side_id.to_string(),
            SlotEntry {
                dir,
                lease: Arc::downgrade(&lease),
                receipt,
                _keepalive: keepalive,
            },
        );
        Ok(lease)
    }

    /// Account for one side's binary footprint.
    ///
    /// Deliberately does no IO: the snapshot already holds every embedded image it
    /// will ever use, and an attachment that only has a path is *not* re-read. The
    /// old implementation created a private directory and copied catalog files,
    /// which both created a second copy of user data and gave the side a file path
    /// it could re-open. `cancel` is still honoured so a side cancelled during
    /// preparation fails instead of starting.
    pub async fn prepare(
        self: &Arc<Self>,
        lease: Arc<SideResourceLease>,
        source: &EffectiveSideSource,
        cancel: CancellationToken,
    ) -> SideResult<PreparedSideResources> {
        if cancel.is_cancelled() {
            return Err(resource_limit("旁支在资源准备期间被取消"));
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(resource_limit("宿主正在关闭，不再接受新的旁支"));
        }
        let binary_bytes = inline_binary_bytes(source);
        if binary_bytes > SIDE_BINARY_BUDGET_BYTES {
            return Err(resource_limit(format!(
                "继承的内嵌媒体已超过 {SIDE_BINARY_BUDGET_BYTES} 字节预算"
            )));
        }
        Ok(PreparedSideResources {
            lease,
            binary_bytes,
        })
    }

    /// The cleanup receipt for `side_id`, if the host still tracks that slot.
    pub fn cleanup_receipt(&self, side_id: &str) -> SideResult<SideCleanupReceipt> {
        let slots = self.slots.lock().unwrap();
        let entry = slots.get(side_id).ok_or_else(|| {
            SideError::new(
                SideErrorReason::SourceChanged,
                format!("旁支 {side_id} 没有可跟踪的资源槽"),
            )
        })?;
        Ok(SideCleanupReceipt::new(entry.receipt.subscribe()))
    }

    /// Retry a cleanup that failed, provided no owner still holds the lease.
    ///
    /// Only legal once the last lease is gone: retrying while an owner is alive
    /// would delete a directory under a live side. Returns the (possibly still
    /// pending) receipt so the caller waits on the real outcome instead of
    /// assuming the retry succeeded.
    pub fn retry_cleanup(self: &Arc<Self>, side_id: &str) -> SideResult<SideCleanupReceipt> {
        let (dir, sender, has_owner) = {
            let slots = self.slots.lock().unwrap();
            let entry = slots.get(side_id).ok_or_else(|| {
                SideError::new(
                    SideErrorReason::SourceChanged,
                    format!("旁支 {side_id} 没有可重试的资源槽"),
                )
            })?;
            (
                entry.dir.clone(),
                entry.receipt.clone(),
                entry.lease.strong_count() > 0,
            )
        };
        if has_owner {
            return Err(resource_limit("旁支仍有持有者在运行，不能重试清理"));
        }
        if sender.borrow().is_complete() {
            return Ok(SideCleanupReceipt::new(sender.subscribe()));
        }
        // `Ok` cannot fail here: the slot keeps a receiver alive on purpose.
        let _ = sender.send(SideCleanupState::Pending);
        self.live_cleanups.fetch_add(1, Ordering::SeqCst);
        let receiver = sender.subscribe();
        self.submit_cleanup(side_id.to_string(), dir, sender);
        Ok(SideCleanupReceipt::new(receiver))
    }

    /// Refuse new slots and wait (bounded) for live cleanup.
    ///
    /// Returns `false` when cleanup is still running after `grace`; the caller
    /// then leaves reclamation to the next start instead of claiming the
    /// resources were released.
    pub fn close(&self, grace: Duration) -> bool {
        self.closed.store(true, Ordering::SeqCst);
        wait_for_cleanups(&self.live_cleanups, grace)
    }

    pub fn live_cleanups(&self) -> usize {
        self.live_cleanups.load(Ordering::SeqCst)
    }

    pub fn completed_cleanups(&self) -> usize {
        self.completed_cleanups.load(Ordering::SeqCst)
    }

    pub fn failed_cleanups(&self) -> usize {
        self.failed_cleanups.load(Ordering::SeqCst)
    }

    /// Test-only: wait until every queued cleanup finished, or time out.
    ///
    /// Production never waits here: cleanup is asynchronous by design.
    #[cfg(test)]
    pub fn wait_for_cleanup_blocking(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = std::time::Instant::now() + timeout;
        while self.live_cleanups() > 0 {
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("{} cleanup job(s) still running", self.live_cleanups()),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

/// A held slot: its (possibly never created) directory, a non-owning handle to its
/// lease, and the sender that publishes the cleanup outcome.
///
/// The receiver lives in the receipt handed to callers, so the slot does **not**
/// keep a receiver alive: a caller that dropped its receipt must not be able to
/// prevent the sender from being dropped with the slot.
struct SlotEntry {
    dir: PathBuf,
    lease: Weak<SideResourceLease>,
    receipt: watch::Sender<SideCleanupState>,
    /// Keeps one receiver alive so publishing a terminal state never fails for
    /// lack of listeners. It holds no lease, so it cannot delay cleanup.
    _keepalive: watch::Receiver<SideCleanupState>,
}

/// Decoded size of every inline base64 image in the inherited history.
fn inline_binary_bytes(source: &EffectiveSideSource) -> usize {
    let guard = source
        .session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .messages()
        .iter()
        .map(measure_message_size)
        .map(|size| size.binary_bytes)
        .fold(0usize, |total, bytes| total.saturating_add(bytes))
}

/// One held resource slot.
///
/// Dropping the **last** owner submits cleanup; the slot is freed only once the
/// directory is really gone.
pub struct SideResourceLease {
    side_id: String,
    dir: PathBuf,
    host: Weak<SideHostResources>,
    released: AtomicBool,
}

impl SideResourceLease {
    pub fn side_id(&self) -> &str {
        &self.side_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl std::fmt::Debug for SideResourceLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SideResourceLease")
            .field("side_id", &self.side_id)
            .field("dir", &self.dir)
            .finish()
    }
}

impl Drop for SideResourceLease {
    fn drop(&mut self) {
        if self.released.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(host) = self.host.upgrade() else {
            // The host is gone; its whole tree is reclaimed at the next start.
            return;
        };
        // Pick up this slot's own sender so the outcome reaches its receipt. A
        // missing entry means the slot was already freed; nothing to publish.
        let Some(sender) = host
            .slots
            .lock()
            .unwrap()
            .get(&self.side_id)
            .map(|entry| entry.receipt.clone())
        else {
            return;
        };
        host.live_cleanups.fetch_add(1, Ordering::SeqCst);
        host.submit_cleanup(self.side_id.clone(), self.dir.clone(), sender);
    }
}

impl SideHostResources {
    /// Queue one bounded delete and publish its outcome.
    ///
    /// The job holds a strong host handle, so the host (and its directory lock)
    /// stays alive until the delete finishes. The slot is removed only after a
    /// confirmed delete: on failure it stays, so the quota keeps counting a
    /// directory that still exists.
    fn submit_cleanup(
        self: &Arc<Self>,
        side_id: String,
        dir: PathBuf,
        sender: watch::Sender<SideCleanupState>,
    ) {
        let live = Arc::clone(&self.live_cleanups);
        let completed = Arc::clone(&self.completed_cleanups);
        let failed = Arc::clone(&self.failed_cleanups);
        let failed_outside = Arc::clone(&failed);
        let live_outside = Arc::clone(&live);
        // A clone for the closed-pool path: the job closure consumes its own.
        let sender_outside = sender.clone();
        let weak = Arc::downgrade(self);
        let keep_alive: Arc<dyn Send + Sync> = Arc::clone(self) as Arc<dyn Send + Sync>;
        let remove = Arc::clone(&self.remove);

        let submitted = self.pool.submit_detached(keep_alive, move || {
            let outcome = match remove(&dir) {
                Ok(()) => {
                    completed.fetch_add(1, Ordering::SeqCst);
                    SideCleanupState::Complete
                }
                Err(error) => {
                    // Diagnostics never include file contents.
                    tracing::warn!(
                        side_id = %side_id,
                        dir = %dir.display(),
                        "side cleanup failed, keeping its slot for a retry: {error}"
                    );
                    failed.fetch_add(1, Ordering::SeqCst);
                    SideCleanupState::Failed {
                        code: "cleanup_failed".into(),
                    }
                }
            };
            let complete = outcome.is_complete();
            // Publish before freeing the slot, so a caller that sees a free slot
            // can never observe a still-pending receipt.
            let _ = sender.send(outcome);
            if complete && let Some(host) = weak.upgrade() {
                let mut slots = host.slots.lock().unwrap();
                let matching = slots
                    .get(&side_id)
                    .map(|held| held.dir == dir)
                    .unwrap_or(false);
                if matching {
                    slots.remove(&side_id);
                }
            }
            live.fetch_sub(1, Ordering::SeqCst);
        });
        if !submitted {
            // The pool is closed: keep the slot and report the failure on the
            // receipt rather than claiming a release that did not happen.
            failed_outside.fetch_add(1, Ordering::SeqCst);
            live_outside.fetch_sub(1, Ordering::SeqCst);
            let _ = sender_outside.send(SideCleanupState::Failed {
                code: "cleanup_pool_closed".into(),
            });
        }
    }
}

/// Delete a side's tree, treating "already gone" as success.
fn remove_tree(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Remove host directories whose exclusive lock can be taken.
fn reclaim_dead_hosts(root: &Path, current: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == current || uuid::Uuid::parse_str(&name).is_err() {
            continue;
        }
        let dir = entry.path();
        // Taking the lock proves no live host owns the directory.
        let Ok(_lock) = try_lock_directory(&dir) else {
            continue;
        };
        if let Err(error) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(
                dir = %dir.display(),
                "could not reclaim an old side host directory: {error}"
            );
        }
    }
}

/// Take an exclusive lock on `dir/lock`, or fail.
fn try_lock_directory(dir: &Path) -> std::io::Result<HostLock> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(HostLock { _file: file })
}

/// Lock this host's directory, failing hard if another process owns it.
fn lock_directory(dir: &Path) -> SideResult<HostLock> {
    try_lock_directory(dir)
        .map_err(|error| resource_limit(format!("无法取得宿主目录锁 {}：{error}", dir.display())))
}

/// Restrict a directory to its owner.
fn restrict_to_owner(dir: &Path) -> SideResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| resource_limit(format!("无法限制宿主目录权限 {}：{error}", dir.display())))
}

/// Wait for live cleanups up to `grace`.
fn wait_for_cleanups(live: &AtomicUsize, grace: Duration) -> bool {
    let deadline = std::time::Instant::now() + grace;
    loop {
        if live.load(Ordering::SeqCst) == 0 {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The manifest markers an accepted user message carries.
///
/// Only generated manifests are considered; natural-language text is never
/// scanned for paths.
pub fn manifest_markers(message: &yi_agent_core::Message) -> Vec<String> {
    let mut markers = Vec::new();
    for block in &message.content {
        if let ContentBlock::Text(text) = block {
            for token in text.split_whitespace() {
                if token.starts_with(super::source::MANIFEST_PATH_PREFIX) {
                    markers.push(token.trim_end_matches([',', ')']).to_string());
                }
            }
        }
    }
    markers
}

/// Count the inline binaries already in a session, for tests and diagnostics.
pub fn session_inline_binary_bytes(session: &Session) -> usize {
    session
        .messages()
        .iter()
        .map(measure_message_size)
        .map(|size| size.binary_bytes)
        .fold(0usize, |total, bytes| total.saturating_add(bytes))
}

#[cfg(test)]
mod tests;
