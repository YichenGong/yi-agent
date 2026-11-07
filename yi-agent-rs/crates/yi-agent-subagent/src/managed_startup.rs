//! The ordered start of a managed frontend, and what a refusal means.
//!
//! A managed runtime is not "an embedded runtime with a guardian attached": the
//! guardian authorizes *roots*, and it can only authorize a root for a client it
//! knows is present. So a frontend has an order it must follow before it is
//! allowed to do any work:
//!
//! ```text
//! prepare, register_business, heartbeat, authorize_root,
//! attach_root, bind_driver, open_managed_channel, admit_turn
//! ```
//!
//! Each step exists because skipping it produces a specific, quiet failure:
//!
//! * **prepare** -- a runtime this build may use. Skipping it attaches to a
//!   daemon of another generation and the first reclaim sweeps the root away.
//! * **register_business** -- the lease the guardian checks before it mints any
//!   authority. Skipping it means *every* authorize is refused, and a client that
//!   treated that as "no guardian" would fall back to an unmanaged spawn.
//! * **heartbeat** -- renewal. A lease that never renews looks alive for one
//!   window and then expires under running work; the reclaim that follows stops
//!   tools the user is watching.
//! * **authorize_root** -- the narrow capability for *this conversation*. Skipping
//!   it attaches a root nobody authorized; a managed daemon refuses the attach, and
//!   a client that retried with its own key would be asking for an unattributed root.
//! * **attach_root** -- the root itself, whose id must be the one authorized.
//! * **bind_driver** -- the foreground driver, which carries turn activity.
//! * **open_managed_channel** -- the private spawn channel. Without it a tool
//!   process belongs to no client, so nothing can reclaim it.
//! * **admit_turn** -- only now may the conversation run.
//!
//! The gate below enforces two things, and both are tested here rather than
//! inferred from a passing launch:
//!
//! 1. **Order.** A stage that is not the next one is refused. Calling `attach_root`
//!    before `authorize_root` cannot "work by accident".
//! 2. **Fail-closed.** A refusal ends the sequence: every later stage is refused
//!    *without running*, so no subsequent label can appear. That is what makes
//!    "the lease failed" mean "no direct spawn", instead of "spawn anyway and hope".
//!
//! The gate does not perform any of the steps. Each one is a closure the caller
//! supplies, so the CLI wires its real lease/authorize/attach calls through it and
//! tests can refuse any stage without a runtime at all.

use std::sync::Arc;

/// One step of a managed frontend's start, in the only order that is legal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ManagedStartStage {
    Prepare,
    RegisterBusiness,
    Heartbeat,
    AuthorizeRoot,
    AttachRoot,
    BindDriver,
    OpenManagedChannel,
    AdmitTurn,
}

impl ManagedStartStage {
    /// The label written to the trace. Stable, because a test asserts on it.
    pub fn label(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::RegisterBusiness => "register_business",
            Self::Heartbeat => "heartbeat",
            Self::AuthorizeRoot => "authorize_root",
            Self::AttachRoot => "attach_root",
            Self::BindDriver => "bind_driver",
            Self::OpenManagedChannel => "open_managed_channel",
            Self::AdmitTurn => "admit_turn",
        }
    }

    /// Every stage in order, for tests and diagnostics.
    pub fn all() -> [Self; 8] {
        [
            Self::Prepare,
            Self::RegisterBusiness,
            Self::Heartbeat,
            Self::AuthorizeRoot,
            Self::AttachRoot,
            Self::BindDriver,
            Self::OpenManagedChannel,
            Self::AdmitTurn,
        ]
    }

    fn index(self) -> usize {
        Self::all()
            .iter()
            .position(|stage| *stage == self)
            .expect("every stage is in the list")
    }
}

/// Watches a managed start so the order is observable from outside the process.
///
/// The CLI implements this over a file (`YI_AGENT_STARTUP_TRACE`) so an integration
/// test can read the sequence a real binary followed. Nothing in this crate decides
/// where the trace goes.
pub trait ManagedStartObserver: Send + Sync {
    /// The stage finished. Written only on success, so a trace line always means
    /// "this happened" rather than "this was attempted".
    fn completed(&self, stage: ManagedStartStage);
    /// The stage was refused; `cause` is the diagnostic, never a secret.
    ///
    /// Deliberately not paired with a `completed` line for the same stage: a refused
    /// stage did not happen, and a trace that showed both would leave a reader unable
    /// to tell a finished start from an abandoned one.
    fn refused(&self, stage: ManagedStartStage, cause: &str);
}

/// An observer that records nothing. The default, so a caller that does not care
/// about the trace does not have to invent one.
pub struct SilentObserver;

impl ManagedStartObserver for SilentObserver {
    fn completed(&self, _stage: ManagedStartStage) {}
    fn refused(&self, _stage: ManagedStartStage, _cause: &str) {}
}

/// Why a managed frontend did not start.
///
/// Carries the stages that completed, because "it failed" is not actionable on its
/// own: a failure at `heartbeat` means the runtime was found and joined, while a
/// failure at `prepare` means nothing was ever touched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedStartRefusal {
    /// The stage that answered no -- or the first stage attempted after a refusal.
    pub stage: ManagedStartStage,
    pub cause: String,
    /// Stages completed before the refusal, in order.
    pub completed: Vec<ManagedStartStage>,
}

impl std::fmt::Display for ManagedStartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let done = self
            .completed
            .iter()
            .map(|stage| stage.label())
            .collect::<Vec<_>>()
            .join(", ");
        if done.is_empty() {
            write!(
                f,
                "managed start refused at {}: {}",
                self.stage.label(),
                self.cause
            )
        } else {
            write!(
                f,
                "managed start refused at {} after {done}: {}",
                self.stage.label(),
                self.cause
            )
        }
    }
}

impl std::error::Error for ManagedStartRefusal {}

/// Runs the stages in order and refuses everything after the first no.
///
/// One flat type rather than eight typestate parameters: the closure inputs differ
/// per stage (a lease, an authorization, a root), so a typestate would only encode
/// the order the `next` field already enforces -- while making every call site
/// unreadable.
pub struct ManagedStart {
    observer: Arc<dyn ManagedStartObserver>,
    completed: Vec<ManagedStartStage>,
    /// Set by the first refusal. While set, every stage is refused untouched.
    stopped: Option<String>,
}

impl ManagedStart {
    pub fn new(observer: Arc<dyn ManagedStartObserver>) -> Self {
        Self {
            observer,
            completed: Vec::new(),
            stopped: None,
        }
    }

    /// The stages that completed, in order.
    pub fn completed(&self) -> &[ManagedStartStage] {
        &self.completed
    }

    /// The refusal that ended this sequence, if it ended.
    pub fn stopped(&self) -> Option<&str> {
        self.stopped.as_deref()
    }

    /// Ends the sequence with a cause, without running a stage.
    ///
    /// For failures that happen between stages -- building a value from a stage's
    /// output, say. The stage named is the one that was about to run, so the trace
    /// records where the start stopped rather than inventing a completed label.
    pub fn refuse(&mut self, stage: ManagedStartStage, cause: String) -> ManagedStartRefusal {
        self.observer.refused(stage, &cause);
        self.stopped = Some(cause.clone());
        ManagedStartRefusal {
            stage,
            cause,
            completed: self.completed.clone(),
        }
    }

    /// Runs one stage, if it is the next legal one.
    ///
    /// Three refusals are possible and they are deliberately distinct in the error:
    ///
    /// * a stage after an earlier refusal -- listed under `completed` as far as it got,
    ///   with the original cause in the message, because a caller retrying needs to
    ///   know whether anything happened at all;
    /// * a stage out of order -- a programming error the gate turns into a refusal
    ///   rather than a silently accepted call;
    /// * the closure's own refusal, which is the real one.
    pub fn step<T>(
        &mut self,
        stage: ManagedStartStage,
        run: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, ManagedStartRefusal> {
        if let Some(cause) = &self.stopped {
            // Do not observe this stage: it did not run, and a trace line for it
            // would be the very "subsequent label" a refusal must not produce.
            return Err(ManagedStartRefusal {
                stage,
                cause: format!("an earlier stage was refused: {cause}"),
                completed: self.completed.clone(),
            });
        }
        let expected = self.completed.len();
        if stage.index() != expected {
            let cause = format!(
                "{} cannot run here; the next legal stage is {}",
                stage.label(),
                ManagedStartStage::all()[expected].label()
            );
            self.stopped = Some(cause.clone());
            self.observer.refused(stage, &cause);
            return Err(ManagedStartRefusal {
                stage,
                cause,
                completed: self.completed.clone(),
            });
        }
        match run() {
            Ok(value) => {
                self.completed.push(stage);
                self.observer.completed(stage);
                Ok(value)
            }
            Err(cause) => {
                self.observer.refused(stage, &cause);
                self.stopped = Some(cause.clone());
                Err(ManagedStartRefusal {
                    stage,
                    cause,
                    completed: self.completed.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what happened, so the assertions read as the trace contract.
    #[derive(Default)]
    struct Recording {
        lines: Mutex<Vec<String>>,
    }

    impl Recording {
        fn lines(&self) -> Vec<String> {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    impl ManagedStartObserver for Recording {
        fn completed(&self, stage: ManagedStartStage) {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(stage.label().to_string());
        }

        fn refused(&self, stage: ManagedStartStage, _cause: &str) {
            self.lines
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("refused:{}", stage.label()));
        }
    }

    fn gate() -> (ManagedStart, Arc<Recording>) {
        let recording = Arc::new(Recording::default());
        (ManagedStart::new(recording.clone()), recording)
    }

    /// The happy path is exactly the plan's expected trace.
    #[test]
    fn the_stages_run_in_the_only_legal_order() {
        let (mut start, recording) = gate();
        for stage in ManagedStartStage::all() {
            start
                .step(stage, || Ok::<_, String>(()))
                .unwrap_or_else(|refusal| panic!("{refusal}"));
        }
        assert_eq!(
            recording.lines(),
            vec![
                "prepare",
                "register_business",
                "heartbeat",
                "authorize_root",
                "attach_root",
                "bind_driver",
                "open_managed_channel",
                "admit_turn",
            ]
        );
        assert_eq!(start.completed().len(), 8);
        assert!(start.stopped().is_none());
    }

    /// A refusal at *any* stage leaves every later label absent, and the later
    /// stages do not even run their closures.
    #[test]
    fn a_refusal_at_any_stage_removes_every_later_label() {
        for refused in ManagedStartStage::all() {
            let (mut start, recording) = gate();
            let mut ran_after = Vec::new();
            let mut failure = None;
            for stage in ManagedStartStage::all() {
                if stage > refused {
                    ran_after.push(stage);
                }
                let outcome = start.step(stage, || {
                    if stage == refused {
                        Err("no".to_string())
                    } else {
                        Ok(())
                    }
                });
                if let Err(refusal) = outcome {
                    assert_eq!(refusal.stage, stage, "{refusal}");
                    if failure.is_none() {
                        failure = Some(refusal);
                    }
                }
            }

            let refusal = failure.expect("the refused stage must refuse");
            assert_eq!(refusal.stage, refused);
            assert_eq!(
                refusal.completed,
                ManagedStartStage::all()[..refused.index()].to_vec()
            );
            // Every stage strictly after the refusal was attempted by this loop and
            // refused as a consequence -- so none of them ran.
            for stage in ran_after {
                assert!(
                    refusal.completed.iter().all(|done| *done < stage),
                    "{stage:?} was recorded as completed after a refusal"
                );
            }

            let mut expected: Vec<String> = ManagedStartStage::all()[..refused.index()]
                .iter()
                .map(|stage| stage.label().to_string())
                .collect();
            expected.push(format!("refused:{}", refused.label()));
            assert_eq!(recording.lines(), expected, "refusal at {refused:?}");
        }
    }

    /// The stages after a refusal never execute, which is the difference between
    /// "refused" and "refused but spawned anyway".
    #[test]
    fn nothing_runs_after_the_first_refusal() {
        let (mut start, _recording) = gate();
        start
            .step(ManagedStartStage::Prepare, || Ok::<_, String>(()))
            .unwrap();
        start
            .step(ManagedStartStage::RegisterBusiness, || {
                Err::<(), _>("the guardian did not register a lease".to_string())
            })
            .unwrap_err();

        let mut reached_attach = false;
        let refusal = start
            .step(ManagedStartStage::AttachRoot, || {
                reached_attach = true;
                Ok::<_, String>(())
            })
            .unwrap_err();
        assert!(!reached_attach, "a stage after a refusal must not run");
        assert_eq!(refusal.stage, ManagedStartStage::AttachRoot);
        assert!(
            refusal
                .cause
                .contains("the guardian did not register a lease")
        );
        assert_eq!(refusal.completed, vec![ManagedStartStage::Prepare]);
    }

    /// Out of order is a refusal, and it also ends the sequence: accepting it would
    /// let a later correct call look like a successful start.
    #[test]
    fn an_out_of_order_stage_is_refused_and_stops_the_sequence() {
        let (mut start, recording) = gate();
        let refusal = start
            .step(ManagedStartStage::AuthorizeRoot, || Ok::<_, String>(()))
            .unwrap_err();
        assert_eq!(refusal.stage, ManagedStartStage::AuthorizeRoot);
        assert!(refusal.cause.contains("next legal stage is prepare"));
        assert!(refusal.completed.is_empty());
        assert_eq!(recording.lines(), vec!["refused:authorize_root"]);

        let later = start
            .step(ManagedStartStage::Prepare, || Ok::<_, String>(()))
            .unwrap_err();
        assert_eq!(later.stage, ManagedStartStage::Prepare);
        assert!(start.stopped().is_some());
    }

    /// A repeated stage is not a retry: the lease, the authorization and the channel
    /// are each taken once, and a second `heartbeat` would silently no-op.
    #[test]
    fn a_stage_cannot_run_twice() {
        let (mut start, _recording) = gate();
        start
            .step(ManagedStartStage::Prepare, || Ok::<_, String>(()))
            .unwrap();
        let refusal = start
            .step(ManagedStartStage::Prepare, || Ok::<_, String>(()))
            .unwrap_err();
        assert!(
            refusal
                .cause
                .contains("next legal stage is register_business")
        );
    }

    /// The labels are a wire-ish contract for the trace file; renaming one silently
    /// would break the assertion that a refusal removed it.
    #[test]
    fn the_labels_are_the_ones_the_plan_names() {
        assert_eq!(
            ManagedStartStage::all().map(|stage| stage.label()),
            [
                "prepare",
                "register_business",
                "heartbeat",
                "authorize_root",
                "attach_root",
                "bind_driver",
                "open_managed_channel",
                "admit_turn",
            ]
        );
    }
}
