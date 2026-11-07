//! Per-conversation sandbox baselines.
//!
//! Startup settings are the default for *new* conversations. Nothing in this
//! module ever reads the current defaults while re-opening an existing
//! conversation: a saved baseline is the only authority, because silently
//! re-deriving it from today's configuration is exactly how a conversation the
//! user started in a narrow sandbox comes back wider after an unrelated settings
//! change.
//!
//! Three inputs can constrain a resume, in increasing trust:
//!
//! 1. `saved` — what the conversation was created with, persisted in its meta.
//! 2. `trusted_live` — a live controller for a session already running in this
//!    process. Its base `mode`/`promotable` are ground truth, so it wins over
//!    `saved` (a stale meta must not widen a running conversation).
//! 3. `explicit_host` — a hard limit from this invocation (CLI flag, environment,
//!    parent process). It can only ever narrow, never widen.
//!
//! The result is the *effective floor*: whatever all applicable inputs agree is
//! safe. When no input carries evidence (a meta written before baselines were
//! recorded, cold history), the answer is read-only and non-promotable, and the
//! caller is told so it can persist that conservative floor.

use serde::{Deserialize, Serialize};
use yi_agent_tools::SandboxMode;

/// The tool-execution baseline a conversation was created with.
///
/// `promotable` is stored rather than derived: it decides whether a live YOLO
/// switch may escalate *this* conversation, so recomputing it from current
/// configuration would retroactively change what the user authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SandboxBaseline {
    pub mode: SandboxMode,
    pub promotable: bool,
}

impl SandboxBaseline {
    /// Parse a baseline from its wire label, normalizing the read-only
    /// invariant.
    ///
    /// Read-only is never promotable; accepting `promotable: true` here would
    /// create a baseline that claims an escalation the controller will refuse,
    /// and the mismatch would only show up as a silent no-op much later. Errors
    /// name the offending *category*, never echo the input, so a malformed meta
    /// cannot smuggle arbitrary text into a log line.
    pub fn new(mode: &str, promotable: bool) -> Result<Self, String> {
        let mode = parse_mode(mode)?;
        Ok(Self {
            mode,
            promotable: normalize_promotable(mode, promotable),
        })
    }

    /// Construct from an already-validated mode, applying the same invariant.
    pub fn from_parts(mode: SandboxMode, promotable: bool) -> Self {
        Self {
            mode,
            promotable: normalize_promotable(mode, promotable),
        }
    }

    /// The narrowest of two baselines: the stricter mode wins, and promotion is
    /// allowed only if both permit it.
    ///
    /// Used to intersect with host restrictions, where "strictest" is the only
    /// safe merge — a host passing `read-only` must not be widened by a saved
    /// `workspace-write`.
    pub fn intersect(self, other: Self) -> Self {
        let mode = if strictness(self.mode) <= strictness(other.mode) {
            self.mode
        } else {
            other.mode
        };
        Self::from_parts(mode, self.promotable && other.promotable)
    }
}

/// Fixed shape `{mode, promotable}` with the kebab-case wire label.
///
/// Manual rather than derived so the field names and the wire label stay the
/// single documented shape, whatever the enum happens to look like later.
impl Serialize for SandboxBaseline {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut out = serializer.serialize_struct("SandboxBaseline", 2)?;
        out.serialize_field("mode", &self.mode)?;
        out.serialize_field("promotable", &self.promotable)?;
        out.end()
    }
}

/// Deserialization goes through [`SandboxBaseline::new`], not a `derive` impl.
///
/// A derived impl would accept `{"mode": "read-only", "promotable": true}` and
/// hand back a baseline the runtime controller silently refuses to honor. Routing
/// through the constructor keeps the invariant true for every path, including
/// hand-edited meta files.
impl<'de> Deserialize<'de> for SandboxBaseline {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = RawBaseline::deserialize(deserializer)?;
        Self::new(&raw.mode, raw.promotable).map_err(D::Error::custom)
    }
}

#[derive(Deserialize)]
struct RawBaseline {
    mode: String,
    promotable: bool,
}

/// Strictness ordering used for intersection: lower is narrower.
fn strictness(mode: SandboxMode) -> u8 {
    match mode {
        SandboxMode::ReadOnly => 0,
        SandboxMode::WorkspaceWrite => 1,
        SandboxMode::DangerFullAccess => 2,
    }
}

fn normalize_promotable(mode: SandboxMode, promotable: bool) -> bool {
    promotable && mode != SandboxMode::ReadOnly
}

/// Parse only the three documented wire labels.
///
/// Deliberately not `serde`-based on `SandboxMode` alone: this is also the
/// boundary for host-provided values, and the error must not echo the input.
fn parse_mode(mode: &str) -> Result<SandboxMode, String> {
    match mode {
        "read-only" => Ok(SandboxMode::ReadOnly),
        "workspace-write" => Ok(SandboxMode::WorkspaceWrite),
        "danger-full-access" => Ok(SandboxMode::DangerFullAccess),
        _ => Err("unknown sandbox mode".to_string()),
    }
}

/// Which baseline a resume actually runs under, plus why it was narrowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandboxBaseline {
    pub baseline: SandboxBaseline,
    pub warning: Option<SandboxBaselineWarning>,
}

/// Controlled categories only: no settings values, no paths, no user text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxBaselineWarning {
    /// No saved baseline and no live controller: cold history with no evidence,
    /// so the conversation falls back to read-only until it is re-established.
    LegacyReadOnlyFallback,
    /// A host restriction was stricter than the saved/trusted baseline.
    HostRestrictionApplied,
    /// The meta carried no baseline because it was never created through
    /// `thread/start`, and the conversation has no history either — so the
    /// current default was adopted and is being persisted. Distinct from
    /// `LegacyReadOnlyFallback`: that one exists because tools already ran under
    /// values nobody recorded.
    NewSessionAdoptedDefault,
}

impl ResolvedSandboxBaseline {
    /// Whether the caller should persist this result as the conversation's
    /// baseline. Legacy fallbacks are sticky: once recorded, the conversation no
    /// longer looks like "no evidence" and never re-widens later.
    pub fn should_persist(&self) -> bool {
        self.warning.is_some()
    }
}

/// Choose the baseline a resume runs under.
///
/// Trust order is `trusted_live` > `saved` > conservative fallback; the explicit
/// host limit is then intersected so it can only narrow. A warning is reported
/// when the fallback was used or when the host actually narrowed the result.
pub fn resolve_resume_baseline(
    saved: Option<SandboxBaseline>,
    trusted_live: Option<SandboxBaseline>,
    explicit_host: Option<SandboxBaseline>,
) -> ResolvedSandboxBaseline {
    let (base, mut warning) = match (trusted_live, saved) {
        (Some(live), _) => (live, None),
        (None, Some(saved)) => (saved, None),
        (None, None) => (
            SandboxBaseline::from_parts(SandboxMode::ReadOnly, false),
            Some(SandboxBaselineWarning::LegacyReadOnlyFallback),
        ),
    };
    let resolved = match explicit_host {
        Some(host) => {
            let narrowed = base.intersect(host);
            if narrowed != base {
                warning = Some(SandboxBaselineWarning::HostRestrictionApplied);
            }
            narrowed
        }
        None => base,
    };
    ResolvedSandboxBaseline {
        baseline: resolved,
        warning,
    }
}

/// Choose the baseline for an *activation* (resume, fork-source re-open, lazy
/// lead creation, driver rebuild) where the conversation may predate baselines.
///
/// [`resolve_resume_baseline`] answers "what did this conversation record?". This
/// wrapper adds the one case where the record legitimately cannot exist yet: a
/// meta created outside `thread/start` (the lazily seeded lead conversation is
/// the real example) has no baseline *and* no transcript. Such a conversation
/// has never run a turn, so there is no access to retroactively widen — adopting
/// today's configured default is not a widening, and the result is persisted so
/// the question never has to be asked again.
///
/// A conversation with any history and no record is different: tools ran under
/// values nobody wrote down, so the fallback is the conservative floor.
pub fn resolve_activation_baseline(
    saved: Option<SandboxBaseline>,
    trusted_live: Option<SandboxBaseline>,
    explicit_host: Option<SandboxBaseline>,
    configured: SandboxBaseline,
    has_history: bool,
) -> ResolvedSandboxBaseline {
    if saved.is_none() && trusted_live.is_none() && !has_history {
        // Never ran: seed from today's default (still narrowed by the host) and
        // mark it for persistence.
        return ResolvedSandboxBaseline {
            baseline: new_session_baseline(configured, explicit_host),
            warning: Some(SandboxBaselineWarning::NewSessionAdoptedDefault),
        };
    }
    resolve_resume_baseline(saved, trusted_live, explicit_host)
}

/// Build the baseline a brand-new conversation starts under.
///
/// Takes the configured defaults plus any host restriction, so a new
/// conversation is never wider than either. No warning: a new conversation is
/// not a compatibility case.
pub fn new_session_baseline(
    configured: SandboxBaseline,
    explicit_host: Option<SandboxBaseline>,
) -> SandboxBaseline {
    match explicit_host {
        Some(host) => configured.intersect(host),
        None => configured,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wider_host_does_not_expand_saved_read_only() {
        let saved = SandboxBaseline::new("read-only", false).unwrap();
        let host = SandboxBaseline::new("danger-full-access", false).unwrap();
        let result = resolve_resume_baseline(Some(saved), None, Some(host));
        assert_eq!(result.baseline, saved);
        assert_eq!(result.warning, None);
    }

    #[test]
    fn explicit_read_only_narrows_a_saved_workspace_write() {
        let saved = SandboxBaseline::new("workspace-write", true).unwrap();
        let host = SandboxBaseline::new("read-only", false).unwrap();
        let result = resolve_resume_baseline(Some(saved), None, Some(host));
        assert_eq!(result.baseline.mode, SandboxMode::ReadOnly);
        assert!(!result.baseline.promotable);
        assert_eq!(
            result.warning,
            Some(SandboxBaselineWarning::HostRestrictionApplied)
        );
    }

    #[test]
    fn no_evidence_falls_back_to_read_only_with_a_warning() {
        let result = resolve_resume_baseline(None, None, None);
        assert_eq!(result.baseline.mode, SandboxMode::ReadOnly);
        assert!(!result.baseline.promotable);
        assert_eq!(
            result.warning,
            Some(SandboxBaselineWarning::LegacyReadOnlyFallback)
        );
        assert!(result.should_persist());
    }

    #[test]
    fn a_trusted_live_controller_beats_a_stale_saved_baseline() {
        let saved = SandboxBaseline::new("danger-full-access", true).unwrap();
        let live = SandboxBaseline::new("read-only", false).unwrap();
        let result = resolve_resume_baseline(Some(saved), Some(live), None);
        assert_eq!(result.baseline, live);
        assert_eq!(result.warning, None);
    }

    #[test]
    fn read_only_normalizes_promotable_off() {
        let parsed = SandboxBaseline::new("read-only", true).unwrap();
        assert!(!parsed.promotable);
        let via_parts = SandboxBaseline::from_parts(SandboxMode::ReadOnly, true);
        assert!(!via_parts.promotable);
    }

    #[test]
    fn an_unknown_mode_is_rejected_without_echoing_the_input() {
        let err = SandboxBaseline::new("definitely-not-a-mode", false).unwrap_err();
        assert_eq!(err, "unknown sandbox mode");
        assert!(!err.contains("definitely"));
    }

    #[test]
    fn serialization_uses_the_kebab_case_wire_labels() {
        let json =
            serde_json::to_value(SandboxBaseline::new("workspace-write", true).unwrap()).unwrap();
        assert_eq!(json["mode"], "workspace-write");
        assert_eq!(json["promotable"], true);
    }

    #[test]
    fn deserialization_enforces_the_read_only_invariant() {
        let parsed: SandboxBaseline =
            serde_json::from_value(serde_json::json!({"mode": "read-only", "promotable": true}))
                .unwrap();
        assert!(!parsed.promotable);
    }

    #[test]
    fn a_historyless_meta_adopts_the_configured_default() {
        let configured = SandboxBaseline::new("workspace-write", true).unwrap();
        let result = resolve_activation_baseline(None, None, None, configured, false);
        assert_eq!(result.baseline, configured);
        assert_eq!(
            result.warning,
            Some(SandboxBaselineWarning::NewSessionAdoptedDefault)
        );
        assert!(result.should_persist());
    }

    #[test]
    fn a_meta_with_history_and_no_record_falls_back_to_read_only() {
        let configured = SandboxBaseline::new("workspace-write", true).unwrap();
        let result = resolve_activation_baseline(None, None, None, configured, true);
        assert_eq!(result.baseline.mode, SandboxMode::ReadOnly);
        assert!(!result.baseline.promotable);
        assert_eq!(
            result.warning,
            Some(SandboxBaselineWarning::LegacyReadOnlyFallback)
        );
    }

    #[test]
    fn a_recorded_baseline_ignores_todays_default() {
        let saved = SandboxBaseline::new("read-only", false).unwrap();
        let configured = SandboxBaseline::new("danger-full-access", true).unwrap();
        let result = resolve_activation_baseline(Some(saved), None, None, configured, true);
        assert_eq!(result.baseline, saved);
        assert_eq!(result.warning, None);
        assert!(!result.should_persist());
    }

    #[test]
    fn a_new_session_never_exceeds_the_host_restriction() {
        let configured = SandboxBaseline::new("danger-full-access", true).unwrap();
        let host = SandboxBaseline::new("workspace-write", true).unwrap();
        let built = new_session_baseline(configured, Some(host));
        assert_eq!(built.mode, SandboxMode::WorkspaceWrite);
        assert!(built.promotable);
    }

    #[test]
    fn promotion_requires_both_sides_to_allow_it() {
        let configured = SandboxBaseline::new("workspace-write", true).unwrap();
        let host = SandboxBaseline::new("workspace-write", false).unwrap();
        let built = new_session_baseline(configured, Some(host));
        assert!(!built.promotable);
    }
}
