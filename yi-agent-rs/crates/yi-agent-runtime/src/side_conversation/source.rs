//! The effective source a side conversation is assembled from, published
//! atomically.
//!
//! A side must inherit its parent's *effective* settings — the provider the
//! parent actually runs on, its model, its effort, its prompt, its roots — and it
//! must never observe a half-applied change. Two rules enforce that:
//!
//! 1. The published value is a single struct behind one lock, so a reader cannot
//!    combine an old provider with a new config.
//! 2. A reader that copies state records the revision it read *inside* the same
//!    lock and verifies it again before returning. A writer that changes the
//!    source does so through `begin_update` → [`SourceUpdate::commit`], and during
//!    that window [`SideSourcePublisher::snapshot`] reports
//!    [`SideErrorReason::SourceChanged`] instead of a half state.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_core::{AgentConfig, ManagedRunLimits, Provider, Session, forkable_messages};

use super::{SideError, SideErrorReason, SideResult, map_budget_error};

/// The generated attachment path prefix a manifest line carries.
///
/// A catalog entry is only honoured when its marker — this prefix plus the
/// generated file name — still appears in the safe history. The model reads
/// attachments through this path, so it is the one reliable, non-guessed anchor.
pub const MANIFEST_PATH_PREFIX: &str = ".yi-agent/attachments/";

/// One attachment the side may keep reading, with the manifest marker that
/// identified it when the request was accepted.
///
/// `manifest_marker` comes from the generated attachment manifest, never from
/// guessing a path out of natural language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideAttachment {
    pub original: PathBuf,
    pub manifest_marker: String,
}

/// The attachments a side inherited, in the order the parent listed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideAttachmentCatalog(pub Vec<SideAttachment>);

impl SideAttachmentCatalog {
    /// Resolve the catalog against a session's *safe* history.
    ///
    /// Only entries whose generated manifest marker is still present in the
    /// forkable prefix are returned: if the parent's manifest line was trimmed
    /// away, neither the model nor the side can refer to the file any more. The
    /// session text is never scanned for arbitrary paths — only the host-supplied
    /// markers are compared.
    pub fn resolve(&self, session: &Arc<Mutex<Session>>) -> Result<Vec<PathBuf>, String> {
        let guard = session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained: String = forkable_messages(&guard)
            .iter()
            .flat_map(|message| message.content.iter())
            .filter_map(|block| match block {
                yi_agent_core::ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(self
            .0
            .iter()
            .filter(|attachment| retained.contains(&attachment.manifest_marker))
            .map(|attachment| attachment.original.clone())
            .collect())
    }
}

/// Everything a side conversation runs on, captured from its parent.
///
/// `Clone` shares the provider, the history handle and the authorisation source,
/// and copies the config value: cloning never duplicates a provider or a session.
#[derive(Clone)]
pub struct EffectiveSideSource {
    /// The side's own history. Never the parent's `Session`: a side writes to its
    /// own `Arc` so nothing it does can reach the parent.
    pub session: Arc<Mutex<Session>>,
    /// The provider instance the parent runs on. Reused so the credential is read
    /// once and the client stays a single object.
    pub provider: Arc<dyn Provider>,
    /// The parent's effective model/effort/prompt/compaction settings.
    pub config: AgentConfig,
    /// The parent's primary root: relative paths resolve against it.
    pub primary: PathBuf,
    /// Every root the parent was authorised for.
    pub roots: Vec<PathBuf>,
    /// Live view of the parent's *current* authorisation, so a revoked root stops
    /// granting access to future reads.
    pub current_roots: Arc<dyn yi_agent_tools::SideRoots>,
    pub attachments: SideAttachmentCatalog,
    /// The parent's most recently rendered configuration (prompt included).
    ///
    /// `config` is the value assembled for the parent, but a live-settings save
    /// can change the prompt the provider actually receives. Reading this cache
    /// gives the side the real value **without** asking the live source to rescan
    /// project rules or skills. It is `None` once the snapshot is taken.
    pub effective_config: Option<Arc<Mutex<AgentConfig>>>,
    /// Monotonic revision of this source. A reader uses it to detect that the
    /// parent changed underneath it.
    pub revision: u64,
    /// The parent's lifecycle generation; a bump invalidates the side.
    pub generation: u64,
    /// The parent's provider admission gate, if the host has one. A side acquires
    /// its own lease from it rather than reusing the parent's.
    pub provider_gate: Option<Arc<dyn yi_agent_core::ProviderTurnGate>>,
}

impl std::fmt::Debug for EffectiveSideSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the session contents and the provider identity.
        f.debug_struct("EffectiveSideSource")
            .field("model", &self.config.model)
            .field("primary", &self.primary)
            .field("roots", &self.roots)
            .field("attachments", &self.attachments.0.len())
            .field("revision", &self.revision)
            .field("generation", &self.generation)
            .field("has_provider_gate", &self.provider_gate.is_some())
            .finish()
    }
}

/// How many times a snapshot will retry while the parent keeps changing.
pub const SOURCE_SNAPSHOT_ATTEMPTS: usize = 3;

/// Publishes an [`EffectiveSideSource`] and hands out consistent snapshots.
pub struct SideSourcePublisher {
    state: Mutex<PublishedSource>,
}

struct PublishedSource {
    source: EffectiveSideSource,
    /// `true` while a writer is replacing the source; readers must not read
    /// across it.
    updating: bool,
    /// Set when the source became unusable (parent cleared/deleted/reclaimed).
    unavailable: Option<SideErrorReason>,
}

impl SideSourcePublisher {
    pub fn new(source: EffectiveSideSource) -> Self {
        Self {
            state: Mutex::new(PublishedSource {
                source,
                updating: false,
                unavailable: None,
            }),
        }
    }

    /// Copy the source and the revision it carries, under one lock.
    ///
    /// The revision is returned alongside the copy rather than read separately:
    /// two separate reads could observe a newer revision than the source they
    /// describe, which is exactly the false consistency this type exists to
    /// prevent.
    pub fn capture_source(&self) -> (EffectiveSideSource, Option<u64>) {
        let state = self.lock();
        if state.updating || state.unavailable.is_some() {
            return (state.source.clone(), None);
        }
        let source = state.source.clone();
        let revision = source.revision;
        (source, Some(revision))
    }

    /// The current revision, or `None` while a writer holds the source.
    pub fn revision(&self) -> Option<u64> {
        let state = self.lock();
        if state.updating || state.unavailable.is_some() {
            return None;
        }
        Some(state.source.revision)
    }

    /// Take the current source, or return why it cannot be used.
    pub fn current(&self) -> SideResult<EffectiveSideSource> {
        let state = self.lock();
        if let Some(reason) = state.unavailable {
            return Err(SideError::new(reason, "父会话已不可用"));
        }
        if state.updating {
            return Err(SideError::new(
                SideErrorReason::SourceChanged,
                "父会话配置正在更新",
            ));
        }
        Ok(state.source.clone())
    }

    /// Take a *consistent* snapshot of the parent's inherited history.
    ///
    /// The returned source has a fresh `Arc<Mutex<Session>>` holding only the
    /// forkable prefix, so later writes by the side cannot touch the parent and
    /// the parent's later turns cannot appear inside the side.
    ///
    /// The hidden boundary message is **not** added here: the runner adds it once
    /// when it is created, so a retry cannot append it twice.
    pub async fn snapshot(&self) -> SideResult<EffectiveSideSource> {
        self.snapshot_with(SOURCE_SNAPSHOT_ATTEMPTS).await
    }

    async fn snapshot_with(&self, attempts: usize) -> SideResult<EffectiveSideSource> {
        for _ in 0..attempts {
            let (source, before) = self.capture_source();
            let Some(before) = before else {
                // A writer is active: check again rather than read across it.
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            };

            // The session lock is a `std::sync::Mutex` shared with core. Take it
            // only long enough to measure and clone; never across an await.
            let messages = {
                let guard = source
                    .session
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                yi_agent_core::measure_side_history(&guard, &ManagedRunLimits::side_defaults())
                    .map_err(map_budget_error)?;
                let messages = forkable_messages(&guard);
                // A side with nothing to inherit cannot answer "based on the
                // context above", so it must fail before any turn is accepted
                // rather than start from an empty history.
                if messages.is_empty() {
                    return Err(SideError::new(
                        SideErrorReason::ParentNotStarted,
                        "父会话还没有可继承的安全历史",
                    ));
                }
                messages
            };

            // Re-read the revision *after* copying. If the parent changed while
            // the copy was in flight, the copy may be torn: retry.
            if self.revision() != Some(before) {
                tokio::task::yield_now().await;
                continue;
            }

            let mut captured = source;
            // Freeze the configuration the parent actually runs with. Reading the
            // published cache costs one lock and never rescans project prompts.
            //
            // The cache holds the *parent's* config, whose prompt says "you are
            // driving this task". Handing that over verbatim would let the side
            // resume the parent's work, so the fixed side policy is appended here
            // — the one place every snapshot path goes through, so a stored cache
            // can never smuggle in a prompt without it.
            if let Some(cache) = &captured.effective_config {
                let effective = cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                captured.config = yi_agent_core::side_agent_config(&effective);
            }
            captured.effective_config = None;
            // A fresh handle, so the side's history is independent even though the
            // content came from the parent's. The parent's own session is never
            // rewritten: trimming an unsafe tail happens on this copy only.
            captured.session = Arc::new(Mutex::new(Session::from_messages(messages)));
            return Ok(captured);
        }
        Err(SideError::new(
            SideErrorReason::SourceChanged,
            "父会话在取快照期间持续变化",
        ))
    }

    /// Mark the source as being replaced. The returned guard must be committed or
    /// dropped; dropping rolls the mark back.
    pub fn begin_update(&self) -> SideResult<SourceUpdate<'_>> {
        let mut state = self.lock();
        if state.unavailable.is_some() {
            return Err(SideError::new(
                SideErrorReason::SideClosing,
                "父会话已不可用",
            ));
        }
        if state.updating {
            return Err(SideError::new(
                SideErrorReason::SourceChanged,
                "另一个更新正在进行",
            ));
        }
        state.updating = true;
        Ok(SourceUpdate {
            publisher: self,
            committed: false,
        })
    }

    /// Publish a source that is no longer usable, so every later read fails with
    /// `reason` instead of serving stale state.
    pub fn mark_unavailable(&self, reason: SideErrorReason) {
        let mut state = self.lock();
        state.unavailable = Some(reason);
        state.updating = false;
    }

    /// Bump the revision and replace the source while holding the update mark.
    fn publish(&self, mut source: EffectiveSideSource) {
        let mut state = self.lock();
        let next = state.source.revision.wrapping_add(1);
        source.revision = next;
        state.source = source;
        state.updating = false;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PublishedSource> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// An in-flight source replacement.
///
/// Committing publishes the new source atomically (config, revision and
/// authorisation together). Dropping without committing clears the update mark
/// and leaves the previous source in place, so a failed update never exposes a
/// half state.
pub struct SourceUpdate<'a> {
    publisher: &'a SideSourcePublisher,
    committed: bool,
}

impl SourceUpdate<'_> {
    pub fn commit(mut self, source: EffectiveSideSource) {
        self.committed = true;
        self.publisher.publish(source);
    }
}

impl Drop for SourceUpdate<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Roll back: the previous source stays published.
        let mut state = self.publisher.lock();
        state.updating = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_conversation::test_support::{mock_provider, source_with_history};
    use yi_agent_core::{ContentBlock, Message};

    #[test]
    fn side_source_capture_returns_the_matching_revision() {
        let source = source_with_history("父问题", "父回答");
        let publisher = SideSourcePublisher::new(source.clone());
        let (captured, revision) = publisher.capture_source();
        assert_eq!(revision, Some(source.revision));
        assert_eq!(captured.config.model, source.config.model);
    }

    #[tokio::test]
    async fn side_source_snapshot_clones_history_into_a_new_session() {
        let source = source_with_history("父问题", "父回答");
        let parent_session = Arc::clone(&source.session);
        let publisher = SideSourcePublisher::new(source);
        let captured = publisher.snapshot().await.unwrap();

        assert!(
            !Arc::ptr_eq(&captured.session, &parent_session),
            "the side must get its own session handle"
        );
        let parent = parent_session.lock().unwrap();
        let side = captured.session.lock().unwrap();
        assert_eq!(side.messages(), parent.messages());
        drop(parent);
        drop(side);

        // Writing to the side must not reach the parent.
        captured
            .session
            .lock()
            .unwrap()
            .push(Message::user("旁支问题"));
        assert_eq!(
            parent_session.lock().unwrap().messages().len(),
            2,
            "the parent history must stay untouched"
        );
    }

    #[tokio::test]
    async fn side_source_snapshot_drops_an_unpaired_trailing_tool_use() {
        let source = source_with_history("父问题", "父回答");
        source
            .session
            .lock()
            .unwrap()
            .push(Message::assistant(vec![ContentBlock::ToolUse {
                id: "call-1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            }]));
        let publisher = SideSourcePublisher::new(source);
        let captured = publisher.snapshot().await.unwrap();
        assert_eq!(
            captured.session.lock().unwrap().messages().len(),
            2,
            "an unpaired tool-use must never be inherited"
        );
    }

    #[tokio::test]
    async fn side_source_snapshot_reports_the_missing_history_as_parent_not_started() {
        let source = source_with_history("", "");
        source.session.lock().unwrap().replace_messages(Vec::new());
        let publisher = SideSourcePublisher::new(source);
        let error = publisher.snapshot().await.unwrap_err();
        assert_eq!(
            error.reason,
            SideErrorReason::ParentNotStarted,
            "an empty safe history must fail before a turn is accepted"
        );
    }

    #[tokio::test]
    async fn side_source_snapshot_refuses_while_an_update_is_in_flight() {
        let source = source_with_history("父问题", "父回答");
        let publisher = SideSourcePublisher::new(source.clone());
        let update = publisher.begin_update().unwrap();
        let error = publisher.snapshot().await.unwrap_err();
        assert_eq!(error.reason, SideErrorReason::SourceChanged);
        assert_eq!(publisher.revision(), None);
        drop(update);
        // The rollback restores readability.
        assert_eq!(publisher.revision(), Some(source.revision));
    }

    #[tokio::test]
    async fn side_source_commit_publishes_config_and_revision_together() {
        let source = source_with_history("父问题", "父回答");
        let publisher = SideSourcePublisher::new(source.clone());
        let mut next = source.clone();
        next.config.model = "claude-opus-new".into();
        next.provider = mock_provider("claude-opus-new");
        publisher.begin_update().unwrap().commit(next);

        let captured = publisher.snapshot().await.unwrap();
        assert_eq!(captured.config.model, "claude-opus-new");
        assert_eq!(captured.revision, source.revision + 1);
        assert_eq!(
            publisher.revision(),
            Some(source.revision + 1),
            "the published revision must match the published source"
        );
    }

    #[tokio::test]
    async fn side_source_mark_unavailable_blocks_every_read() {
        let source = source_with_history("父问题", "父回答");
        let publisher = SideSourcePublisher::new(source);
        publisher.mark_unavailable(SideErrorReason::SourceChanged);
        let error = publisher.current().unwrap_err();
        assert_eq!(error.reason, SideErrorReason::SourceChanged);
        assert!(publisher.capture_source().1.is_none());
        assert!(publisher.snapshot().await.is_err());
    }

    #[test]
    fn side_source_debug_never_prints_history_or_credentials() {
        let source = source_with_history("父秘密问题", "父秘密回答");
        let rendered = format!("{source:?}");
        assert!(!rendered.contains("父秘密"), "{rendered}");
        assert!(rendered.contains("has_provider_gate"), "{rendered}");
    }
}
