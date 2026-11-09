//! The set of parents this process is actively driving.
//!
//! A side must inherit what the running parent *has*, not what it last wrote to
//! disk: a turn in flight has committed messages whose file copy does not exist
//! yet, and a mid-conversation model switch has already replaced the provider.
//! So the process keeps one published source per live parent, and the resolver
//! prefers that over decoding the transcript again.
//!
//! Entries are keyed by thread id and replaced wholesale (never mutated), so a
//! reader that grabbed one gets a consistent source. Removal is compare-and-swap
//! by pointer: an exiting driver cleans up *its* publisher and cannot delete a
//! newer one that a rebuilt agent already published.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_runtime::side_conversation::{SideAttachmentCatalog, SideSourcePublisher};

use super::source::source_from_live_agent;

/// Publishers for the parents this process currently drives.
#[derive(Default)]
pub struct LiveSideParents {
    inner: Mutex<HashMap<String, Arc<SideSourcePublisher>>>,
}

impl LiveSideParents {
    pub fn new() -> Self {
        Self::default()
    }

    /// The publisher for `parent`, if it is being driven here.
    pub fn get(&self, parent: &str) -> Option<Arc<SideSourcePublisher>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(parent)
            .cloned()
    }

    /// Record `publisher` as the current source for `parent`.
    pub fn publish(&self, parent: String, publisher: Arc<SideSourcePublisher>) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(parent, publisher);
    }

    /// Remove `parent`'s entry only if it is still `publisher`.
    ///
    /// The pointer check is the whole point: a driver that is exiting must not
    /// unmount a publisher that a rebuild already installed, or a live parent
    /// would silently fall back to the cold path.
    pub fn remove_if(&self, parent: &str, publisher: &Arc<SideSourcePublisher>) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matches = inner
            .get(parent)
            .map(|current| Arc::ptr_eq(current, publisher))
            .unwrap_or(false);
        if matches {
            inner.remove(parent);
        }
    }

    /// Drop the entry regardless of who published it.
    ///
    /// Only for a parent that is really gone (cleared or deleted): a *rebuilt*
    /// agent keeps the thread id and publishes a replacement instead.
    pub fn remove(&self, parent: &str) -> Option<Arc<SideSourcePublisher>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(parent)
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }
}

impl std::fmt::Debug for LiveSideParents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print a publisher: it owns a session handle and a provider.
        let count = self
            .inner
            .lock()
            .map(|inner| inner.len())
            .unwrap_or_default();
        f.debug_struct("LiveSideParents")
            .field("live", &count)
            .finish()
    }
}

/// The roots a side may be told about: the primary root first, then the extra
/// roots the conversation was authorised for.
///
/// Deduplicated and primary-first because the source's `roots` is also the list
/// a later read resolves relative paths against; a duplicate would only make the
/// authorisation receipt report the same root twice.
pub fn live_roots(primary: &str, extra_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(primary)];
    for root in extra_roots {
        if !roots.contains(root) {
            roots.push(root.clone());
        }
    }
    roots
}

/// One live parent's published source, owned by its driver.
///
/// # Why this is a separate type and not just an `Arc<SideSourcePublisher>`
///
/// Three things must happen together and in one place, or they drift apart:
///
/// 1. **Publish** — the map entry must exist before the driver can serve a turn,
///    so a `/side` typed the instant the thread appears reads real state.
/// 2. **Refresh** — every rebuild of the parent's agent (model switch, MCP
///    refresh, skills prompt update, clear/compact) must republish a *whole*
///    source, because provider and config must not be observed mismatched.
/// 3. **Unpublish on exit** — compare-and-swap removal, so an exiting driver
///    cannot delete a publisher that a rebuild installed.
///
/// Folding them into a value with a `Drop` makes (3) impossible to forget: the
/// driver holds it for its whole life and every `return` path cleans up.
pub struct LiveParentSource {
    parent: String,
    parents: Arc<LiveSideParents>,
    publisher: Arc<SideSourcePublisher>,
    primary: PathBuf,
    roots: Vec<PathBuf>,
    attachments: SideAttachmentCatalog,
}

impl LiveParentSource {
    /// Build and publish the first source for `parent`.
    ///
    /// A live parent is authoritative from the moment it exists, even with an
    /// empty history: the alternative (publish nothing) would send a `/side`
    /// typed before the first message to the cold path, which reads the file
    /// copy — possibly older than what the running conversation holds, and for a
    /// brand-new thread that file does not exist yet. The empty case is reported
    /// by `snapshot()` as `parent_not_started`, so the error is the same; what
    /// this avoids is a stale *success*.
    ///
    /// `None` is therefore only for a source that cannot be built at all (an
    /// oversized history), and the next refresh retries.
    pub fn install(
        parents: Arc<LiveSideParents>,
        parent: impl Into<String>,
        agent: &yi_agent_core::Agent,
        primary: impl Into<PathBuf>,
        roots: Vec<PathBuf>,
        attachments: SideAttachmentCatalog,
    ) -> Option<Arc<Self>> {
        let parent = parent.into();
        let primary = primary.into();
        let source =
            source_from_live_agent(agent, primary.clone(), roots.clone(), attachments.clone())
                .ok()?;
        let publisher = Arc::new(SideSourcePublisher::new(source));
        parents.publish(parent.clone(), Arc::clone(&publisher));
        Some(Arc::new(Self {
            parent,
            parents,
            publisher,
            primary,
            roots,
            attachments,
        }))
    }

    pub fn publisher(&self) -> &Arc<SideSourcePublisher> {
        &self.publisher
    }

    /// Republish the parent's current provider/config/history.
    ///
    /// A failed rebuild (the agent has no forkable history, e.g. right after a
    /// clear, or its history exceeds the side budget) marks the published source
    /// unavailable with the same reason a reader would have got, so later opens
    /// fail with a real cause instead of silently serving the previous state.
    pub fn refresh(&self, agent: &yi_agent_core::Agent) {
        let next = match source_from_live_agent(
            agent,
            self.primary.clone(),
            self.roots.clone(),
            self.attachments.clone(),
        ) {
            Ok(source) => source,
            Err(error) => {
                self.publisher.mark_unavailable(error.reason);
                return;
            }
        };
        let Ok(update) = self.publisher.begin_update() else {
            // Another update is in flight; the writer that holds the mark will
            // publish a source derived from the same agent. Forcing a commit here
            // would let a reader observe two sources racing.
            return;
        };
        update.commit(next);
    }
}

impl Drop for LiveParentSource {
    fn drop(&mut self) {
        self.parents.remove_if(&self.parent, &self.publisher);
    }
}

impl std::fmt::Debug for LiveParentSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveParentSource")
            .field("parent", &self.parent)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_runtime::side_conversation::{
        EffectiveSideSource, SideAttachmentCatalog, SideErrorReason,
    };

    fn publisher() -> Arc<SideSourcePublisher> {
        let session = Arc::new(std::sync::Mutex::new(yi_agent_core::Session::new()));
        session
            .lock()
            .unwrap()
            .push(yi_agent_core::Message::user("父问题"));
        Arc::new(SideSourcePublisher::new(EffectiveSideSource {
            session,
            provider: Arc::new(NoopProvider),
            config: yi_agent_core::AgentConfig {
                model: "m".into(),
                ..Default::default()
            },
            primary: std::path::PathBuf::from("/tmp"),
            roots: vec![std::path::PathBuf::from("/tmp")],
            current_roots: Arc::new(yi_agent_tools::StaticSideRoots::new(vec![
                std::path::PathBuf::from("/tmp"),
            ])),
            attachments: SideAttachmentCatalog::default(),
            effective_config: None,
            revision: 1,
            generation: 1,
            provider_gate: None,
        }))
    }

    struct NoopProvider;

    #[async_trait::async_trait]
    impl yi_agent_core::Provider for NoopProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::ProviderEvent>,
            yi_agent_core::ProviderError,
        > {
            Ok(Box::pin(futures::stream::empty()))
        }
    }

    #[test]
    fn a_stale_removal_never_unmounts_a_newer_publisher() {
        let map = LiveSideParents::new();
        let first = publisher();
        let second = publisher();
        map.publish("parent".into(), Arc::clone(&first));
        map.publish("parent".into(), Arc::clone(&second));

        // The old driver exits after a rebuild installed `second`.
        map.remove_if("parent", &first);
        assert!(
            Arc::ptr_eq(&map.get("parent").unwrap(), &second),
            "removing the superseded publisher must leave the current one"
        );
    }

    #[test]
    fn the_current_publisher_can_be_removed_by_its_owner() {
        let map = LiveSideParents::new();
        let only = publisher();
        map.publish("parent".into(), Arc::clone(&only));
        map.remove_if("parent", &only);
        assert!(map.get("parent").is_none());
        assert!(map.is_empty());
    }

    /// The agent a parent driver would own, with one committed message.
    fn parent_agent(history: bool) -> yi_agent_core::Agent {
        let agent = yi_agent_core::Agent::new(
            Arc::new(NoopProvider),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            yi_agent_core::AgentConfig {
                model: "live-model".into(),
                system_prompt: Some("parent prompt".into()),
                ..Default::default()
            },
        );
        if history {
            agent
                .session_handle()
                .lock()
                .unwrap()
                .push(yi_agent_core::Message::user("父已经提交的问题"));
        }
        agent
    }

    fn rendered(publisher: &SideSourcePublisher) -> String {
        let source = publisher.current().expect("a published live source");
        let session = source.session.lock().unwrap();
        serde_json::to_string(&session.messages().to_vec()).unwrap()
    }

    /// A refresh is how a running parent's *new* progress reaches a later `/side`.
    #[test]
    fn refresh_publishes_progress_the_parent_committed_after_install() {
        let parents = Arc::new(LiveSideParents::new());
        let agent = parent_agent(true);
        let installed = LiveParentSource::install(
            Arc::clone(&parents),
            "p1",
            &agent,
            std::path::Path::new("/tmp"),
            vec![std::path::PathBuf::from("/tmp")],
            SideAttachmentCatalog::default(),
        )
        .expect("a parent with history publishes at once");

        assert!(!rendered(installed.publisher()).contains("父后来的一步"));
        agent
            .session_handle()
            .lock()
            .unwrap()
            .push(yi_agent_core::Message::assistant(vec![
                yi_agent_core::ContentBlock::Text("父后来的一步".into()),
            ]));
        installed.refresh(&agent);

        let after = rendered(installed.publisher());
        assert!(
            after.contains("父后来的一步"),
            "a refresh must publish the parent's latest committed state: {after}"
        );
        assert_eq!(
            installed.publisher().current().unwrap().config.model,
            "live-model"
        );
    }

    /// Dropping the owner is the whole cleanup story: every driver exit path is
    /// covered because there is no separate call to forget.
    #[test]
    fn dropping_the_owner_unpublishes_the_parent() {
        let parents = Arc::new(LiveSideParents::new());
        let agent = parent_agent(true);
        let installed = LiveParentSource::install(
            Arc::clone(&parents),
            "p1",
            &agent,
            std::path::Path::new("/tmp"),
            vec![std::path::PathBuf::from("/tmp")],
            SideAttachmentCatalog::default(),
        )
        .expect("a parent with history publishes at once");
        assert!(parents.get("p1").is_some());

        drop(installed);
        assert!(
            parents.get("p1").is_none(),
            "a driver that exits must not leave a live entry behind"
        );
    }

    /// An empty live parent is still authoritative: it publishes (so a `/side`
    /// cannot fall through to a possibly-older file copy), and the *snapshot*
    /// reports `parent_not_started`. After a clear, a refresh must not keep
    /// serving the content from before it.
    #[tokio::test]
    async fn an_empty_parent_publishes_but_snapshots_as_not_started() {
        let parents = Arc::new(LiveSideParents::new());
        let agent = parent_agent(true);
        let installed = LiveParentSource::install(
            Arc::clone(&parents),
            "p1",
            &agent,
            std::path::Path::new("/tmp"),
            vec![std::path::PathBuf::from("/tmp")],
            SideAttachmentCatalog::default(),
        )
        .expect("history publishes");
        assert!(parents.get("p1").is_some());

        // Cleared: the new state is authoritative and empty.
        let cleared = parent_agent(false);
        installed.refresh(&cleared);
        let error = installed
            .publisher()
            .snapshot()
            .await
            .expect_err("a cleared parent has nothing to fork");
        assert_eq!(error.reason, SideErrorReason::ParentNotStarted);

        // A brand-new live parent publishes too, for the same reason.
        let fresh = parent_agent(false);
        let installed = LiveParentSource::install(
            Arc::clone(&parents),
            "p2",
            &fresh,
            std::path::Path::new("/tmp"),
            vec![std::path::PathBuf::from("/tmp")],
            SideAttachmentCatalog::default(),
        )
        .expect("an empty live parent still publishes");
        assert!(parents.get("p2").is_some());
        let error = installed.publisher().snapshot().await.expect_err(
            "an empty parent must still be reported as not started, not inherited as empty",
        );
        assert_eq!(error.reason, SideErrorReason::ParentNotStarted);
    }
}
