//! Per-side directories, attachment copies and resource slots.
//!
//! A side keeps its own copy of every attachment it still needs under
//! `side-conversations/<hostInstanceId>/<sideId>/`, plus a bounded read
//! authorisation for exactly those files. Three invariants hold here:
//!
//! 1. **A slot is real.** A side holds one of eight slots from creation until its
//!    directory is actually deleted, so repeatedly opening and closing cannot
//!    escape the quota.
//! 2. **Cleanup never blocks its initiator.** The last owner's `Drop` submits a
//!    bounded job to the host's blocking pool; it never calls `remove_dir_all`
//!    inline, and the job keeps the slot and the host lock until the delete really
//!    finished.
//! 3. **A dead host can be reclaimed, a live one cannot.** Startup only removes old
//!    host directories whose exclusive lock it can actually take — never by PID or
//!    timestamp.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use yi_agent_core::{ContentBlock, ManagedRunLimits, RunBudget, Session, measure_message_size};
use yi_agent_tools::{SideBlockingPool, SideFileAlias, SideReadAccess, SideRoots};

use super::source::EffectiveSideSource;
use super::{SideError, SideErrorReason, SideResult};

/// Resource slots one host may hand out.
///
/// Creating, open, closing and not-yet-drained cleanup resources all count, so a
/// client cannot escape the quota by cycling open/close.
pub const SIDE_RESOURCE_SLOTS: usize = 8;

/// The per-side binary ceiling: copied file bytes plus decoded inline images.
pub const SIDE_BINARY_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Directory name under the instance home.
pub const SIDE_CONVERSATIONS_DIR: &str = "side-conversations";

/// A resource limit failure: the side asked for more than its allowance.
fn resource_limit(detail: impl std::fmt::Display) -> SideError {
    SideError::new(SideErrorReason::ResourceLimit, detail.to_string())
}

/// Everything the resource layer produced for one side.
pub struct PreparedSideResources {
    pub access: Arc<SideReadAccess>,
    pub lease: Arc<SideResourceLease>,
    /// Binary bytes committed to the side's budget.
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
        slots.insert(
            side_id.to_string(),
            SlotEntry {
                dir,
                lease: Arc::downgrade(&lease),
            },
        );
        Ok(lease)
    }

    /// Prepare a side's directory and read authorisation from its source.
    pub async fn prepare(
        self: &Arc<Self>,
        lease: Arc<SideResourceLease>,
        source: &EffectiveSideSource,
        cancel: CancellationToken,
    ) -> SideResult<PreparedSideResources> {
        let plan = plan_copies(source)?;
        let dir = lease.dir.clone();
        let side_id = lease.side_id.clone();
        let pool = Arc::clone(&self.pool);
        // The lease is the work's owner: a cancelled side cannot leave an
        // untracked reader behind, and the slot survives until the copy ends.
        let owner = Arc::clone(&lease) as Arc<dyn Send + Sync>;
        let copy_source = CopySource {
            provider_primary: source.primary.clone(),
            created_roots: source.roots.clone(),
            current_roots: Arc::clone(&source.current_roots),
            inline_bytes: inline_binary_bytes(source),
        };

        let job = {
            let plan = plan.clone();
            let dir = dir.clone();
            move || copy_attachments(&copy_source, &plan, &dir)
        };
        let (aliases, binary_bytes) = pool
            .run(&side_id, cancel, owner, job)
            .await
            .map_err(|error| resource_limit(format!("准备旁支资源失败：{error}")))?;

        // The side inherits the parent's roots. The private copy directory is not
        // added: copies are reachable only through their registered alias, which
        // stays bound to the original's authorisation.
        let mut access = SideReadAccess::new(
            source.primary.clone(),
            source.roots.clone(),
            Arc::clone(&source.current_roots),
        )
        .map_err(|error| resource_limit(format!("建立旁支读取授权失败：{error}")))?;
        access
            .set_attachment_aliases(aliases)
            .map_err(|error| resource_limit(format!("登记旁支附件映射失败：{error}")))?;

        Ok(PreparedSideResources {
            access: Arc::new(access),
            lease,
            binary_bytes,
        })
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

/// A held slot: its directory and a non-owning handle to its lease.
struct SlotEntry {
    dir: PathBuf,
    lease: Weak<SideResourceLease>,
}

/// What a copy job needs from the source, without keeping the session alive.
struct CopySource {
    provider_primary: PathBuf,
    /// The roots the parent authorised when the side was created. Copies are read
    /// through a handle authorised by exactly these, never by re-opening a path.
    created_roots: Vec<PathBuf>,
    current_roots: Arc<dyn SideRoots>,
    /// Decoded bytes already consumed by the history's inline images.
    inline_bytes: usize,
}

/// One attachment to copy.
#[derive(Clone, Debug)]
struct CopyPlan {
    original: PathBuf,
    copy_name: String,
}

/// Resolve the catalog against the safe history and name each copy.
///
/// An attachment is copied only when its host-generated manifest marker is still
/// present in the forkable prefix: if the parent's manifest line was trimmed away,
/// the model cannot see the reference and copying the file would only add an
/// unreachable duplicate. Natural-language text is never scanned for paths — the
/// marker is a generated attachment path the host supplies.
fn plan_copies(source: &EffectiveSideSource) -> SideResult<Vec<CopyPlan>> {
    let session = source
        .session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let retained = yi_agent_core::forkable_messages(&session);
    let retained_text: String = retained
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut plans = Vec::with_capacity(source.attachments.0.len());
    for (index, attachment) in source.attachments.0.iter().enumerate() {
        if !retained_text.contains(&attachment.manifest_marker) {
            continue;
        }
        let file_name = attachment
            .original
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| format!("attachment-{index}"));
        plans.push(CopyPlan {
            original: attachment.original.clone(),
            // Keep the index so two attachments sharing a basename cannot collide.
            copy_name: format!("{index}-{file_name}"),
        });
    }
    Ok(plans)
}

/// Copy every planned attachment from an authorised handle, never by path.
///
/// The read is bounded by `remaining + 1`, so an overflowing file is detected from
/// the bytes actually read rather than from a metadata length that may have
/// changed since.
fn copy_attachments(
    source: &CopySource,
    plan: &[CopyPlan],
    dir: &Path,
) -> std::io::Result<(Vec<SideFileAlias>, usize)> {
    std::fs::create_dir_all(dir)?;
    // Two handles: one reads the originals under the parent's authorisation, one
    // registers the copies that were just written.
    let reader = SideReadAccess::new(
        source.provider_primary.clone(),
        source.created_roots.clone(),
        Arc::clone(&source.current_roots),
    )?;

    if source.inline_bytes > SIDE_BINARY_BUDGET_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "inherited inline images already exceed the {SIDE_BINARY_BUDGET_BYTES}-byte budget"
            ),
        ));
    }
    let mut total = 0usize;
    let mut aliases = Vec::with_capacity(plan.len());
    for item in plan {
        let remaining = SIDE_BINARY_BUDGET_BYTES.saturating_sub(source.inline_bytes + total);
        if remaining == 0 {
            // Refuse before creating anything: a zero-byte placeholder in the
            // side's directory would look like a copy that succeeded.
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "the side's 64 MiB budget is already spent; refusing {}",
                    item.original.display()
                ),
            ));
        }
        let mut input = reader.open_regular(&item.original)?;
        let copy = dir.join(&item.copy_name);
        let mut output = std::fs::File::create(&copy)?;
        let copied = std::io::copy(&mut input.by_ref().take(remaining as u64 + 1), &mut output)?;
        if copied as usize > remaining {
            drop(output);
            // Never leave a truncated copy behind: the alias would otherwise
            // serve a file that looks complete.
            let _ = std::fs::remove_file(&copy);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "attachment exceeds the side's 64 MiB budget: {}",
                    item.original.display()
                ),
            ));
        }
        drop(output);
        total += copied as usize;
        aliases.push(SideFileAlias {
            original: item.original.clone(),
            copy,
        });
    }

    // Validate the finished set against the shared accounting rules as well, so a
    // future change to one path cannot drift from the other.
    let budget = RunBudget::new(ManagedRunLimits {
        binary_bytes: SIDE_BINARY_BUDGET_BYTES,
        ..ManagedRunLimits::side_defaults()
    });
    let held = budget
        .reserve_binary(source.inline_bytes + total)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    drop(held);
    Ok((aliases, total))
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
        host.live_cleanups.fetch_add(1, Ordering::SeqCst);

        let dir = self.dir.clone();
        let side_id = self.side_id.clone();
        let live = Arc::clone(&host.live_cleanups);
        let completed = Arc::clone(&host.completed_cleanups);
        let failed = Arc::clone(&host.failed_cleanups);
        let failed_outside = Arc::clone(&failed);
        let live_outside = Arc::clone(&live);
        let weak = Arc::downgrade(&host);

        // The job holds a strong handle, so the host (and its directory lock) stays
        // alive until the directory is really gone, and the `Weak` upgrade below
        // always succeeds.
        let keep_alive: Arc<dyn Send + Sync> = Arc::clone(&host) as Arc<dyn Send + Sync>;
        let submitted = host.pool.submit_detached(keep_alive, move || {
            match remove_tree(&dir) {
                Ok(()) => {
                    completed.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) => {
                    // Diagnostics never include file contents.
                    tracing::warn!(
                        side_id = %side_id,
                        dir = %dir.display(),
                        "side cleanup failed, leaving it for startup reclamation: {error}"
                    );
                    failed.fetch_add(1, Ordering::SeqCst);
                }
            }
            // Free the slot last: `slots_in_use` must never report a slot as free
            // while its directory still exists.
            if let Some(host) = weak.upgrade() {
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
            // The pool is closed: keep the slot and let startup reclaim the tree,
            // rather than reporting a release that did not happen.
            failed_outside.fetch_add(1, Ordering::SeqCst);
            live_outside.fetch_sub(1, Ordering::SeqCst);
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
