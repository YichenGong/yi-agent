//! 保证「登记在册的项目」各自有一个活着的 daemon。
//!
//! 一份逻辑、两个宿主：常驻 watchman（launchd 托管）与 app-server 内循环都调它。
//! 幂等——daemon 已应答就什么都不做；daemon 自身有独占锁，重复探测/拉起不会双起。

use std::path::{Path, PathBuf};

/// 生产入口：用真实的 daemon 生命周期（复用已应答的 daemon，否则拉起一个）。
pub fn ensure_daemons(projects: &[PathBuf]) {
    ensure_daemons_with(projects, &mut crate::daemon::ensure_running);
}

/// [`ensure_daemons`] on a shared budget: once it is spent, no further daemon is
/// started.
///
/// Checking the budget *between* projects rather than only once up front matters
/// for the contract the caller published: "I did not start anything after the
/// budget" must hold for every project, not just the first one.
pub fn ensure_daemons_until(
    projects: &[PathBuf],
    budget: &yi_agent_lifecycle::deadline::Deadline,
) -> Vec<PathBuf> {
    ensure_daemons_with(projects, &mut |project: &Path| {
        crate::daemon::ensure_running_until(project, budget)
    })
}

/// 注入版：返回本次真正拉起的项目。单个失败只记日志，不影响其余项目。
pub fn ensure_daemons_with(
    projects: &[PathBuf],
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
) -> Vec<PathBuf> {
    let mut started = Vec::new();
    for project in projects {
        match launcher(project) {
            Ok(true) => started.push(project.clone()),
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "resident watchman: could not ensure a daemon for {}: {error}",
                    project.display()
                );
            }
        }
    }
    started
}

/// Whether this instance may still start daemons nobody asked for.
///
/// The resident registration is *configuration*: it records that a project wants a
/// daemon when a frontend is around. It is not an instruction to keep one alive
/// with no frontend at all. Once an instance has migrated to the managed runtime,
/// "no client" must mean "no daemon", so every unattended starter consults this.
///
/// Two markers mean two different things and both stop this loop. Retired is the
/// finished migration; frozen is a migration that is running now, or one that failed
/// and is waiting for the user. A failed migration must not quietly reopen the
/// unattended path while an operator still has to look at the machine.
pub fn may_start_unattended(instance_home: &Path) -> bool {
    !crate::lifecycle::retirement::is_retired(instance_home)
        && !crate::lifecycle::retirement::is_frozen(instance_home)
}

/// How long this tick will wait for a migration before standing down.
///
/// Short on purpose: a migration is about to stop the very daemons this loop would
/// start, so waiting is pointless once it is already running. What the wait buys is
/// the *drain* -- a tick that got the gate first finishes its `ensure` before the
/// migration stops anything, instead of a daemon appearing in the middle of a stop.
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The unattended loop: read the registration and ensure each project has a daemon,
/// **unless** this instance no longer restarts itself.
///
/// Returns `None` when the loop must stand down, which is deliberately distinct
/// from `Some(0)` (nothing registered): the first means "this is not my job any
/// more", the second means "my job found no work". Collapsing them would make a
/// retired loop look like an idle one and keep sleeping forever.
///
/// The marker check and the `ensure` it authorises happen under the instance
/// migration gate. That ordering is the whole point: a loop that checked the marker
/// and then waited for the gate would start a daemon *after* a migration had already
/// decided the instance was clear, which is the resurrection the marker is supposed
/// to prevent. Holding the gate also means a migration waits for in-flight `ensure`
/// calls to finish rather than racing them.
pub fn once_unattended(
    instance_home: &Path,
    resident_dir: &Path,
    ensure: &mut dyn FnMut(&[PathBuf]),
) -> Option<usize> {
    once_unattended_as(instance_home, resident_dir, true, ensure)
}

/// [`once_unattended`] with the caller's own declaration of whether it is a
/// *managed* frontend.
///
/// A managed frontend must not start daemons here at all, and the retirement
/// marker cannot express that: the marker means "this instance finished moving to
/// the managed runtime", while a *fresh* instance has no marker and is already
/// managed by declaration. Conflating the two is what let a managed App start an
/// embedded `daemon serve` beside the guardian's daemon. Both bind the same
/// socket, so the guardian's daemon loses the race and dies -- and a dead managed
/// daemon means the project never becomes `managedReady`, so every conversation
/// runs without delegation.
///
/// `embedded` is passed in rather than read from a process global so the two
/// branches stay testable in one test binary; the process owner (the app-server)
/// already knows the answer from its own bring-up policy.
pub fn once_unattended_as(
    instance_home: &Path,
    resident_dir: &Path,
    embedded: bool,
    ensure: &mut dyn FnMut(&[PathBuf]),
) -> Option<usize> {
    once_unattended_within(
        instance_home,
        resident_dir,
        embedded,
        &yi_agent_lifecycle::deadline::Deadline::after(GATE_WAIT),
        ensure,
    )
}

/// [`once_unattended_as`] against a caller-supplied budget.
///
/// The budget matters because this whole tick runs *under* the instance gate: a
/// tick whose budget is spent before it acquires the gate must not go on to start
/// daemons, or the gate is held for work the caller already gave up on. The check
/// is inside the gate so the answer cannot change between check and use.
pub fn once_unattended_within(
    instance_home: &Path,
    resident_dir: &Path,
    embedded: bool,
    budget: &yi_agent_lifecycle::deadline::Deadline,
    ensure: &mut dyn FnMut(&[PathBuf]),
) -> Option<usize> {
    if !embedded {
        return None;
    }
    if !may_start_unattended(instance_home) {
        return None;
    }
    let deadline = budget.clone();
    let gate = match crate::lifecycle::migration_operation::RetirementGate::acquire(
        instance_home,
        deadline.clone(),
    ) {
        Ok(gate) => gate,
        // A migration holds the gate: it is either stopping daemons right now or
        // about to. Starting one here would be the resurrection this is meant to
        // prevent, so this tick does nothing. The next tick re-reads the markers.
        Err(_) => return None,
    };
    // Re-check under the gate: the marker may have been written between the first
    // check and the acquire.
    if !may_start_unattended(instance_home) {
        return None;
    }
    // The budget is re-read under the gate. Waiting for a migration may have spent
    // all of it, and starting daemons now would be work nobody is waiting for --
    // while still holding the gate.
    //
    // Only a *count* of registrations is reported in that case: nothing was
    // ensured, and `None` would wrongly tell the caller to stand down for good.
    let count = if deadline.is_expired() {
        0
    } else {
        once(resident_dir, ensure)
    };
    drop(gate);
    Some(count)
}

/// 读通用登记并交给 `ensure`；返回登记项目数。CLI 与 app-server 共用这一份，
/// 不各自复制「读登记 → ensure」的胶水。
pub fn once(resident_dir: &Path, ensure: &mut dyn FnMut(&[PathBuf])) -> usize {
    let projects = yi_agent_store::resident::list(resident_dir);
    ensure(&projects);
    projects.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// 迁移后的实例不得再被「登记表里还有项目」驱动起 daemon。
    ///
    /// 登记是配置，不是无人值守的授权；把两者等同起来就是 spec 反对的
    /// 「旧自愈源继续复活 daemon」。
    #[test]
    fn a_retired_instance_starts_nothing_unattended() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()),
            "{\"projects\":[]}",
        )
        .unwrap();
        crate::lifecycle::retirement::retire(instance.path(), "managed runtime took over").unwrap();
        let seen = RefCell::new(Vec::new());

        let outcome = once_unattended(instance.path(), dir.path(), &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });

        assert_eq!(outcome, None);
        assert!(seen.borrow().is_empty(), "no ensure call may happen");
    }

    /// A tick must not start daemons on a budget somebody else already spent.
    ///
    /// The gate is held for the whole tick, so starting work after the caller gave
    /// up would keep *every* cooperating start blocked behind a tick that is not
    /// serving anyone. `Some(0)` rather than `None`: the instance is still this
    /// loop's job, it just had no time this round.
    #[test]
    fn an_expired_budget_starts_nothing_unattended() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()),
            "{\"projects\":[{\"project\":\"/tmp/a\"},{\"project\":\"/tmp/b\"}]}",
        )
        .unwrap();
        let seen = RefCell::new(Vec::new());
        let budget = yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::ZERO);

        let outcome = once_unattended_within(
            instance.path(),
            dir.path(),
            true,
            &budget,
            &mut |projects| {
                seen.borrow_mut().push(projects.len());
            },
        );

        assert_eq!(
            outcome,
            Some(0),
            "an expired tick did no work, but still ran"
        );
        assert!(
            seen.borrow().is_empty(),
            "an expired tick must not ensure any daemon"
        );
    }

    /// The positive witness: the same call with budget left *does* ensure, so the
    /// assertion above is about the budget and not about a tick that never works.
    #[test]
    fn a_live_budget_still_ensures() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()),
            "{\"projects\":[{\"project\":\"/tmp/a\"}]}",
        )
        .unwrap();
        let seen = RefCell::new(Vec::new());
        let budget =
            yi_agent_lifecycle::deadline::Deadline::after(std::time::Duration::from_secs(5));

        let outcome = once_unattended_within(
            instance.path(),
            dir.path(),
            true,
            &budget,
            &mut |projects| {
                seen.borrow_mut().push(projects.len());
            },
        );

        assert_eq!(outcome, Some(1));
        assert_eq!(seen.borrow().as_slice(), &[1], "the tick must ensure once");
    }

    /// 「没有登记」与「不再是我负责」必须是两个不同答案。
    /// A running (or failed) migration freezes unattended starting without claiming
    /// the migration finished.
    #[test]
    fn a_frozen_instance_starts_nothing_unattended_either() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::lifecycle::retirement::freeze(instance.path(), "a migration is running").unwrap();
        assert!(!crate::lifecycle::retirement::is_retired(instance.path()));
        let seen = RefCell::new(Vec::new());

        let outcome = once_unattended(instance.path(), dir.path(), &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });

        assert_eq!(outcome, None);
        assert!(
            seen.borrow().is_empty(),
            "a frozen instance does not ensure"
        );
    }

    /// A managed frontend must never start a daemon nobody supervises.
    ///
    /// The registration says a project *wants* a daemon when a frontend is around.
    /// A managed frontend already has one -- the guardian's -- and starting a
    /// second, embedded one here is not merely redundant: both bind the same
    /// socket, so the guardian's daemon loses the race, dies, and the project can
    /// never reach `managedReady`. The App then runs every conversation without
    /// delegation, which is what "the subagent tool is missing" looked like.
    ///
    /// Distinguishing this from the retirement marker matters: a *fresh* instance
    /// has no marker at all (nothing was migrated yet), so the marker cannot be
    /// what protects the managed path.
    #[test]
    fn a_managed_frontend_starts_nothing_unattended() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        yi_agent_store::resident::require(dir.path(), Path::new("/a"), "example-plugin").unwrap();
        // No retirement or freeze marker: this is a fresh instance, exactly the
        // shape the marker-only check let through.
        assert!(may_start_unattended(instance.path()));
        let seen = RefCell::new(Vec::new());

        let outcome = once_unattended_as(instance.path(), dir.path(), false, &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });

        assert_eq!(outcome, None);
        assert!(
            seen.borrow().is_empty(),
            "a managed frontend must not ensure any daemon behind the guardian's back"
        );
    }

    /// An embedded frontend keeps the old behaviour, so the fix cannot silently
    /// disable self-healing for the callers that really do own their daemon.
    #[test]
    fn an_embedded_frontend_still_ensures_registered_projects() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        yi_agent_store::resident::require(dir.path(), Path::new("/a"), "example-plugin").unwrap();
        let seen = RefCell::new(Vec::new());

        let outcome = once_unattended_as(instance.path(), dir.path(), true, &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });

        assert_eq!(outcome, Some(1));
        assert_eq!(*seen.borrow(), vec![1]);
    }

    #[test]
    fn an_empty_registry_still_reports_that_it_ran() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let seen = RefCell::new(Vec::new());
        let outcome = once_unattended(instance.path(), dir.path(), &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });
        assert_eq!(outcome, Some(0));
        assert_eq!(*seen.borrow(), vec![0]);
    }

    #[test]
    fn every_registered_project_is_ensured() {
        let projects = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let seen = RefCell::new(Vec::new());
        ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            Ok(true)
        });
        assert_eq!(*seen.borrow(), projects);
    }

    #[test]
    fn once_reads_the_registry_and_hands_it_to_the_ensurer() {
        let dir = tempfile::tempdir().unwrap();
        let a = PathBuf::from("/a");
        yi_agent_store::resident::require(dir.path(), &a, "example-plugin").unwrap();
        let mut seen: Vec<PathBuf> = Vec::new();
        let count = once(dir.path(), &mut |projects| seen.extend_from_slice(projects));
        assert_eq!(count, 1);
        assert_eq!(seen, vec![a]);
    }

    #[test]
    fn one_failing_project_does_not_stop_the_others() {
        let projects = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let seen = RefCell::new(Vec::new());
        let started = ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            if project == Path::new("/b") {
                return Err("boom".to_string());
            }
            Ok(true)
        });
        assert_eq!(seen.borrow().len(), 3, "all projects are attempted");
        assert_eq!(started, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }
}
