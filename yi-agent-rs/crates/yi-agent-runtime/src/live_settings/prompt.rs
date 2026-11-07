//! Live system-prompt composition for runtime settings.
//!
//! One composer owns the whole prompt identity for a session: the stable
//! context (built-in default instructions, project `AGENTS.md`, the session
//! date), the role-specific suffix, and the skills catalog. The live user
//! segment and the catalog budget are supplied per render, so a settings save
//! changes exactly those two derived pieces and nothing else.
//!
//! Three rules make that safe:
//!
//! 1. The stable context is captured once, at assembly. Re-deriving the project
//!    file or the date on every turn would turn a settings refresh into an
//!    unrelated filesystem read.
//! 2. `IncludeAll` binds the *effective* budget and its source at the moment the
//!    user chose it, not at the first render. A later change of either one
//!    revokes it permanently: going back to the old number must not silently
//!    re-include an oversized catalog.
//! 3. The rendered catalog is cached by catalog version, so an unchanged
//!    directory produces byte-identical output and never triggers a second
//!    install of the bundled skills.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex};

use yi_agent_core::live_settings::LiveSettingsError;
use yi_agent_skills::{SkillScope, SkillsService};

use crate::live_settings::sources::ValueSource;

/// Which prompt shape a session renders.
///
/// A side conversation inherits its parent's normal prompt and appends the fixed
/// read-only policy at the very end; a naked session renders nothing at all
/// (it also registers no tools, which is the host's business).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptRole {
    Normal,
    Side,
    Naked,
}

/// How the initial catalog budget decision was made.
///
/// `IncludeAll` carries the effective budget **and** the source it was selected
/// under. Passing a bare `bool` would let a save that lands between the
/// selection and the first request silently change which budget the decision
/// applied to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialCatalogPolicy {
    Bounded,
    IncludeAll { budget: usize, source: ValueSource },
}

/// The catalog truncation actually used for one render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogMode {
    Bounded(usize),
    Unlimited,
}

/// Lifetime state of an `IncludeAll` selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncludeAll {
    /// No `IncludeAll` decision was ever made for this session.
    Off,
    /// Selected under this exact budget and source.
    Active { budget: usize, source: ValueSource },
    /// The budget or its source moved; this never recovers for the session.
    Revoked,
}

/// Composes the system prompt from a stable context plus live inputs.
pub struct LivePromptComposer {
    role: PromptRole,
    /// Built-in default instructions.
    head: String,
    /// Project instructions and the date, in the historical order.
    tail: String,
    skills: Option<Arc<SkillsService>>,
    include_all: Mutex<IncludeAll>,
    catalog_cache: Mutex<Option<(CatalogMode, u64, String)>>,
}

impl std::fmt::Debug for LivePromptComposer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LivePromptComposer")
            .field("role", &self.role)
            .field("has_skills", &self.skills.is_some())
            .finish_non_exhaustive()
    }
}

impl LivePromptComposer {
    /// Build a composer for one session.
    ///
    /// The stable context is derived here, once. `initial_policy` only carries
    /// meaning for [`PromptRole::Normal`]; side and naked sessions start
    /// bounded so they can never inherit another role's oversized catalog.
    pub fn new(
        cfg: &crate::config::RuntimeConfig,
        role: PromptRole,
        skills: Option<Arc<SkillsService>>,
        initial_policy: InitialCatalogPolicy,
    ) -> Result<Self, LiveSettingsError> {
        let include_all = match (role, initial_policy) {
            (PromptRole::Normal, InitialCatalogPolicy::IncludeAll { budget, source }) => {
                IncludeAll::Active { budget, source }
            }
            _ => IncludeAll::Off,
        };
        Ok(Self {
            role,
            head: yi_agent_core::AgentConfig::default_system_prompt(),
            tail: stable_tail(&cfg.workdir),
            skills,
            include_all: Mutex::new(include_all),
            catalog_cache: Mutex::new(None),
        })
    }

    /// Render the system prompt for the given live inputs.
    ///
    /// `user` is the effective user segment (`None` or blank = no segment).
    /// `budget`/`source` are the effective catalog budget and where it came
    /// from. A naked session returns `None` and ignores every input.
    pub fn render(
        &self,
        user: Option<&str>,
        budget: usize,
        source: ValueSource,
    ) -> Result<Option<String>, LiveSettingsError> {
        self.render_tracked(user, budget, source)
            .map(|(prompt, _)| prompt)
    }

    /// [`Self::render`], plus whether a catalog scan was performed.
    ///
    /// A scan can take long enough for a save to land. Callers that hold an
    /// operation snapshot must therefore re-read the source afterwards and
    /// rebuild an unstarted request; a render without skills is pure string
    /// assembly and needs no such re-check.
    pub fn render_tracked(
        &self,
        user: Option<&str>,
        budget: usize,
        source: ValueSource,
    ) -> Result<(Option<String>, bool), LiveSettingsError> {
        if self.role == PromptRole::Naked {
            return Ok((None, false));
        }

        let user = user.map(str::trim).filter(|value| !value.is_empty());
        let mode = self.catalog_mode(budget, source);
        let (catalog, scanned) = match (&self.skills, mode) {
            (Some(service), Some(mode)) => {
                // Test-only barrier: proves that a slow directory scan cannot
                // let an early snapshot outlive a save. It is keyed by this
                // composer so concurrent renders elsewhere never consume it.
                test_hooks::fire(self as *const Self as usize);
                (self.rendered_catalog(service, mode), true)
            }
            _ => (String::new(), false),
        };

        let mut prompt = String::with_capacity(self.head.len() + self.tail.len() + 256);
        prompt.push_str(&self.head);
        if let Some(user) = user {
            prompt.push_str("\n\nUser-provided instructions:\n");
            prompt.push_str(user);
        }
        prompt.push_str(&self.tail);
        if !catalog.is_empty() {
            prompt.push_str("\n\n");
            prompt.push_str(&catalog);
        }

        let rendered = match self.role {
            // The side policy must stay last: nothing a refresh adds may
            // displace the read-only contract.
            PromptRole::Side => Some(yi_agent_core::side_system_prompt(Some(&prompt))),
            PromptRole::Normal => Some(prompt),
            PromptRole::Naked => None,
        };
        Ok((rendered, scanned))
    }

    /// Resolve the catalog truncation, revoking `IncludeAll` on any drift.
    fn catalog_mode(&self, budget: usize, source: ValueSource) -> Option<CatalogMode> {
        self.skills.as_ref()?;
        let mut state = self
            .include_all
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match *state {
            IncludeAll::Active {
                budget: selected_budget,
                source: selected_source,
            } if selected_budget == budget && selected_source == source => {
                Some(CatalogMode::Unlimited)
            }
            IncludeAll::Active { .. } => {
                // Permanent: a later save that returns to the selected value
                // must not resurrect an oversized catalog mid-session.
                *state = IncludeAll::Revoked;
                Some(CatalogMode::Bounded(budget))
            }
            IncludeAll::Revoked | IncludeAll::Off => Some(CatalogMode::Bounded(budget)),
        }
    }

    fn rendered_catalog(&self, service: &Arc<SkillsService>, mode: CatalogMode) -> String {
        // Re-scan so a newly added skill shows up on the next turn, exactly as
        // the previous catalog handle did. A discovery failure stays non-fatal:
        // the session simply renders no catalog.
        if let Err(error) = service.refresh() {
            tracing::warn!("skills refresh failed, keeping the previous catalog: {error}");
        }
        let version = catalog_version(service);
        {
            let cache = self
                .catalog_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some((cached_mode, cached_version, cached)) = cache.as_ref()
                && *cached_mode == mode
                && *cached_version == version
            {
                return cached.clone();
            }
        }
        let rendered = match mode {
            CatalogMode::Bounded(budget) => service.render_catalog(budget),
            CatalogMode::Unlimited => service.render_catalog(usize::MAX),
        };
        *self
            .catalog_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((mode, version, rendered.clone()));
        rendered
    }
}

/// The historical prompt order, minus the user segment.
///
/// Byte-identical to the previous `resolve_system_prompt`, which now delegates
/// here so there is only one implementation.
pub(crate) fn base_prompt(user: Option<&str>, workdir: &Path) -> String {
    let mut prompt = yi_agent_core::AgentConfig::default_system_prompt();
    if let Some(user) = user.map(str::trim).filter(|value| !value.is_empty()) {
        prompt.push_str("\n\nUser-provided instructions:\n");
        prompt.push_str(user);
    }
    prompt.push_str(&stable_tail(workdir));
    prompt
}

/// Project instructions and the date, in that order.
fn stable_tail(workdir: &Path) -> String {
    let mut tail = String::new();
    if let Some(instructions) = crate::bootstrap::load_project_instructions(workdir) {
        tail.push_str("\n\nProject instructions (AGENTS.md):\n");
        tail.push_str(&instructions);
    }
    let today = chrono::Local::now().format("%Y-%m-%d");
    tail.push_str(&format!("\n\nCurrent date: {today}"));
    tail
}

/// A stable identifier for the discovered catalog.
///
/// Only identity matters: any add/remove/rename/move produces a new version,
/// while an unchanged directory keeps its cached rendering.
fn catalog_version(service: &SkillsService) -> u64 {
    let skills = service.snapshot().unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    for skill in &skills {
        skill.name.hash(&mut hasher);
        skill.path.hash(&mut hasher);
        skill.description.len().hash(&mut hasher);
        match skill.scope {
            SkillScope::System => 0u8,
            SkillScope::User => 1,
            SkillScope::Project => 2,
        }
        .hash(&mut hasher);
    }
    skills.len().hash(&mut hasher);
    hasher.finish()
}

/// Barrier hook inside the catalog scan.
///
/// Test-only injection point; in production it compiles to a no-op call, so the
/// source trait stays synchronous and no production RPC is introduced.
#[cfg(not(test))]
pub(crate) mod test_hooks {
    pub(super) fn fire(_composer: usize) {}
}

#[cfg(test)]
pub(crate) mod test_hooks {
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    /// A hook plus the composer it belongs to. Keying the barrier by instance
    /// keeps unrelated tests that render prompts in parallel from consuming it.
    static HOOK: OnceLock<Mutex<Option<(usize, Hook)>>> = OnceLock::new();
    static SERIAL: Mutex<()> = Mutex::new(());

    fn slot() -> &'static Mutex<Option<(usize, Hook)>> {
        HOOK.get_or_init(|| Mutex::new(None))
    }

    /// The barrier is process-global, so hook tests must not overlap.
    pub(crate) fn serial() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Install a one-shot barrier for one composer: it fires at most once and
    /// then removes itself, so the rebuild after the re-check can complete.
    pub(crate) fn install(composer: usize, hook: Hook) {
        *slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((composer, hook));
    }

    pub(crate) fn clear() {
        *slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(super) fn fire(composer: usize) {
        let hook = {
            let mut slot = slot()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match slot.as_ref() {
                Some((installed, _)) if *installed == composer => slot.take().map(|(_, hook)| hook),
                _ => None,
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    use crate::config::sample_config;

    fn skill_dir(dir: &Path, name: &str, description: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }

    fn composer_in(project: &Path, policy: InitialCatalogPolicy) -> LivePromptComposer {
        let mut cfg = sample_config();
        cfg.workdir = project.to_path_buf();
        cfg.system_prompt = None;
        LivePromptComposer::new(&cfg, PromptRole::Normal, None, policy).unwrap()
    }

    fn composer_with_skills(
        project: &Path,
        skills_root: &Path,
        policy: InitialCatalogPolicy,
    ) -> LivePromptComposer {
        let mut cfg = sample_config();
        cfg.workdir = project.to_path_buf();
        cfg.system_prompt = None;
        let service = Arc::new(SkillsService::new(vec![(
            skills_root.to_path_buf(),
            SkillScope::User,
        )]));
        LivePromptComposer::new(&cfg, PromptRole::Normal, Some(service), policy).unwrap()
    }

    /// default → user → AGENTS.md → date → catalog, in that exact order.
    #[test]
    fn normal_role_orders_every_segment() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "PROJECT-INSTRUCTIONS").unwrap();
        skill_dir(skills.path(), "alpha-skill", "first catalog entry");

        let composer =
            composer_with_skills(project.path(), skills.path(), InitialCatalogPolicy::Bounded);
        let prompt = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();

        let default = yi_agent_core::AgentConfig::default_system_prompt();
        let user_at = prompt.find("USER-A").expect("user segment");
        let project_at = prompt.find("PROJECT-INSTRUCTIONS").expect("project block");
        let date_at = prompt.find("Current date:").expect("date");
        let catalog_at = prompt.find("alpha-skill").expect("catalog");

        assert!(
            prompt.starts_with(&default),
            "default instructions come first"
        );
        assert!(user_at < project_at, "user before project: {prompt}");
        assert!(project_at < date_at, "project before date: {prompt}");
        assert!(date_at < catalog_at, "catalog after date: {prompt}");
    }

    /// Clearing the user segment removes that block and nothing else.
    #[test]
    fn clearing_the_user_segment_keeps_every_other_block() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "PROJECT-INSTRUCTIONS").unwrap();
        let composer = composer_in(project.path(), InitialCatalogPolicy::Bounded);

        let with_user = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        for blank in [None, Some(""), Some("   ")] {
            let cleared = composer
                .render(blank, 8192, ValueSource::Settings)
                .unwrap()
                .unwrap();
            assert!(!cleared.contains("USER-A"), "the segment must be gone");
            assert!(!cleared.contains("User-provided instructions:"));
            assert!(cleared.contains("PROJECT-INSTRUCTIONS"));
            assert!(cleared.contains("Current date:"));
            assert_eq!(
                cleared.len(),
                with_user.len() - "\n\nUser-provided instructions:\nUSER-A".len(),
                "only the user block may change"
            );
        }
    }

    /// Naked sessions render no prompt at all.
    #[test]
    fn naked_role_renders_nothing() {
        let project = tempfile::TempDir::new().unwrap();
        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();
        let composer = LivePromptComposer::new(
            &cfg,
            PromptRole::Naked,
            None,
            InitialCatalogPolicy::IncludeAll {
                budget: 8192,
                source: ValueSource::Default,
            },
        )
        .unwrap();
        let (prompt, scanned) = composer
            .render_tracked(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap();
        assert!(prompt.is_none());
        assert!(!scanned, "a naked session must not scan the catalog");
    }

    /// The side policy stays the very last block.
    #[test]
    fn side_role_keeps_the_policy_last() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        skill_dir(skills.path(), "alpha-skill", "first catalog entry");
        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();
        let service = Arc::new(SkillsService::new(vec![(
            skills.path().to_path_buf(),
            SkillScope::User,
        )]));
        let composer = LivePromptComposer::new(
            &cfg,
            PromptRole::Side,
            Some(service),
            InitialCatalogPolicy::IncludeAll {
                budget: 8192,
                source: ValueSource::Default,
            },
        )
        .unwrap();

        let prompt = composer
            .render(Some("USER-A"), 8192, ValueSource::Default)
            .unwrap()
            .unwrap();
        let catalog_at = prompt.find("alpha-skill").expect("catalog");
        let policy_at = prompt
            .find(yi_agent_core::SIDE_SYSTEM_POLICY)
            .expect("the fixed side policy must be present");
        assert!(catalog_at < policy_at, "catalog before the fixed policy");
        assert!(
            prompt
                .trim_end()
                .ends_with(yi_agent_core::SIDE_SYSTEM_POLICY.trim_end()),
            "the policy must be the tail: {prompt}"
        );
        assert!(
            prompt.contains("USER-A"),
            "a side inherits its parent's prompt"
        );
    }

    /// A side never inherits its parent's IncludeAll decision.
    #[test]
    fn side_does_not_inherit_an_unlimited_catalog() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        for index in 0..20 {
            skill_dir(skills.path(), &format!("s{index:02}"), &"x".repeat(600));
        }
        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();
        let service = Arc::new(SkillsService::new(vec![(
            skills.path().to_path_buf(),
            SkillScope::User,
        )]));
        let composer = LivePromptComposer::new(
            &cfg,
            PromptRole::Side,
            Some(service),
            InitialCatalogPolicy::IncludeAll {
                budget: 400,
                source: ValueSource::Default,
            },
        )
        .unwrap();
        let prompt = composer
            .render(None, 400, ValueSource::Default)
            .unwrap()
            .unwrap();
        assert!(
            prompt.len() < 8192,
            "a side must stay bounded even if its parent includes all"
        );
    }

    /// Same budget and source keep IncludeAll; a change of either revokes it for
    /// the rest of the session.
    #[test]
    fn include_all_survives_unrelated_changes_and_revokes_on_drift() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        for index in 0..20 {
            skill_dir(skills.path(), &format!("s{index:02}"), &"x".repeat(600));
        }
        let composer = composer_with_skills(
            project.path(),
            skills.path(),
            InitialCatalogPolicy::IncludeAll {
                budget: 8192,
                source: ValueSource::Default,
            },
        );

        let first = composer
            .render(Some("USER-A"), 8192, ValueSource::Default)
            .unwrap()
            .unwrap();
        assert!(first.contains("s19"), "IncludeAll must include every skill");

        // Unrelated field changed: the budget and its source did not move.
        let second = composer
            .render(Some("USER-B"), 8192, ValueSource::Default)
            .unwrap()
            .unwrap();
        assert!(second.contains("USER-B"));
        assert!(!second.contains("USER-A"));
        assert!(
            second.contains("s19"),
            "an unrelated save must not revoke IncludeAll"
        );
        assert_eq!(
            second.matches("<available_skills>").count(),
            first.matches("<available_skills>").count()
        );

        // Same number, different source: this is a different decision input.
        let third = composer
            .render(Some("USER-C"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert!(
            !third.contains("s19"),
            "a source change must revoke IncludeAll"
        );

        // And it never comes back, even at the originally selected values.
        let fourth = composer
            .render(Some("USER-D"), 8192, ValueSource::Default)
            .unwrap()
            .unwrap();
        assert!(
            !fourth.contains("s19"),
            "revocation is permanent for the session"
        );

        let smaller = composer
            .render(Some("USER-E"), 4096, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert!(!smaller.contains("s19"));
    }

    /// The decision binds at selection, not at the first request.
    #[test]
    fn include_all_binds_at_selection_not_first_request() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        for index in 0..20 {
            skill_dir(skills.path(), &format!("s{index:02}"), &"x".repeat(600));
        }
        // Selected under settings-origin 8192, then the file was saved before
        // the first render with the *same* number but a settings source.
        let composer = composer_with_skills(
            project.path(),
            skills.path(),
            InitialCatalogPolicy::IncludeAll {
                budget: 8192,
                source: ValueSource::Default,
            },
        );
        let prompt = composer
            .render(None, 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert!(
            !prompt.contains("s19"),
            "the selection-time source decides, not the first request"
        );

        let restored = composer
            .render(None, 8192, ValueSource::Default)
            .unwrap()
            .unwrap();
        assert!(
            !restored.contains("s19"),
            "restoring the old source must not resurrect the selection"
        );
    }

    /// An unchanged directory renders byte-identical output and reuses the
    /// service (no reinstall, no tool work).
    #[test]
    fn catalog_output_is_byte_stable_for_an_unchanged_directory() {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        skill_dir(skills.path(), "alpha-skill", "first catalog entry");
        let composer =
            composer_with_skills(project.path(), skills.path(), InitialCatalogPolicy::Bounded);

        let first = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        let second = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert_eq!(first, second, "an unchanged catalog must not jitter");

        // A new skill invalidates the cache and shows up on the next render.
        skill_dir(skills.path(), "beta-skill", "second catalog entry");
        let third = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert!(third.contains("beta-skill"));
        assert!(third.starts_with(&first[..first.len().min(64)]));
    }

    /// `base_prompt` is the single implementation the old bootstrap helper uses.
    #[test]
    fn base_prompt_matches_the_historical_order() {
        let project = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "PROJECT-INSTRUCTIONS").unwrap();
        let prompt = base_prompt(Some("USER-A"), project.path());
        let default = yi_agent_core::AgentConfig::default_system_prompt();
        assert!(prompt.starts_with(&default));
        let user_at = prompt.find("USER-A").unwrap();
        let project_at = prompt.find("PROJECT-INSTRUCTIONS").unwrap();
        let date_at = prompt.find("Current date:").unwrap();
        assert!(user_at < project_at && project_at < date_at);

        let without = base_prompt(None, project.path());
        assert!(!without.contains("User-provided instructions:"));
        assert!(without.starts_with(&default));
    }

    /// The barrier fires at most once, so a rebuild after the re-check can run.
    #[test]
    fn catalog_barrier_is_one_shot() {
        let _serial = test_hooks::serial();
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        skill_dir(skills.path(), "alpha-skill", "first catalog entry");
        let composer = Arc::new(composer_with_skills(
            project.path(),
            skills.path(),
            InitialCatalogPolicy::Bounded,
        ));
        let target = Arc::as_ptr(&composer) as usize;

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        {
            let release_rx = Arc::clone(&release_rx);
            test_hooks::install(
                target,
                Arc::new(move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }),
            );
        }

        let renderer = Arc::clone(&composer);
        let handle = std::thread::spawn(move || {
            renderer
                .render(Some("USER-A"), 8192, ValueSource::Settings)
                .unwrap()
                .unwrap()
        });
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the scan must reach the barrier");
        release_tx.send(()).unwrap();
        let first = handle.join().unwrap();
        test_hooks::clear();

        // The same composer must not park again.
        let second = composer
            .render(Some("USER-A"), 8192, ValueSource::Settings)
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
    }
}
