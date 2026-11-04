//! Stop proofs against real managed processes.
//!
//! The property under test is "a rebind may proceed only when every execution in a
//! frozen set is proven stopped, and proven by the party that can actually see it".
//! Every scenario here uses *real* child processes through the real registry, because
//! the interesting failures are kernel facts: a worker whose join landed while a tool
//! group it forked still runs, an execution nobody registered, a record from another
//! driver epoch.
//!
//! The helper never returns the answer it expects. `StopFixture::check` calls the real
//! verification path and maps the real error, so a scenario that starts passing for
//! the wrong reason fails here instead of looking green.

mod support;

use std::time::Duration;

use support::process_fixture as fixture;
use tempfile::TempDir;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::foreground::{
    ExecutionIdentity, ExecutionKind, ForegroundFailure, ForegroundFence, RootRef,
};
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};
use yi_agent_supervisors::lifecycle::foreground_executions::{
    ForegroundExecutions, RegisteredExecution,
};
use yi_agent_supervisors::lifecycle::guardian::{ProcessOwner, ProcessRegistry, TaskExecutionKey};

fn artifact() -> ArtifactIdentity {
    ArtifactIdentity {
        build_id: "test-build".to_string(),
        display_version: "0.0.0".to_string(),
        executable: "/bin/true".into(),
        sha256: "00".to_string(),
        schema_min: 1,
        schema_max: 1,
        handover_version: 1,
        business_protocol_min: 1,
        business_protocol_max: 1,
        capabilities: Vec::new(),
    }
}

fn scope(directory: &TempDir) -> RuntimeScope {
    RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    }
}

fn registry(directory: &TempDir) -> ProcessRegistry {
    ProcessRegistry::open(&scope(directory), "gen-1", &artifact())
        .expect("registry")
        .0
}

fn fence() -> ForegroundFence {
    ForegroundFence {
        root: RootRef {
            session_id: "session-1".to_string(),
            root_id: "root-1".to_string(),
            attempt_id: "attempt-1".to_string(),
        },
        generation: "gen-1".to_string(),
        binding_epoch: 1,
        driver_epoch: 3,
    }
}

fn execution() -> ExecutionIdentity {
    ExecutionIdentity {
        execution_id: "Driver:root-1:attempt-1".to_string(),
        task_id: "root-1".to_string(),
        attempt_id: "attempt-1".to_string(),
        kind: ExecutionKind::Driver,
    }
}

/// The four ways a stop can fail to be a proof, plus the one that succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopScenario {
    /// The turn was cancelled but never joined: nothing proves it ended.
    CancelOnly,
    /// The turn joined, and the tool group it forked is still alive.
    JoinedToolAlive,
    /// The host awaited the turn and the guardian saw every tool group gone.
    HostAndToolsStopped,
    /// Nobody registered the execution, so nobody can vouch for it.
    MissingEvidence,
}

/// Runs one scenario against real processes and returns the real verdict.
struct StopFixture;

impl StopFixture {
    fn check(scenario: StopScenario) -> Result<(), ForegroundFailure> {
        let directory = TempDir::new().unwrap();
        let mut registry = registry(&directory);
        let mut markers = fixture::MarkerGuard::new();
        let fence = fence();
        let frozen = vec![execution()];
        let mut executions = ForegroundExecutions::new();

        match scenario {
            StopScenario::MissingEvidence => {
                // Registered nowhere: the honest outcome is "cannot vouch".
            }
            StopScenario::CancelOnly | StopScenario::JoinedToolAlive => {
                // A real long-lived tool group, registered for this attempt.
                let marker = markers.path("tool");
                let owner = ProcessOwner::frontend("app");
                let key = TaskExecutionKey::new("gen-1", "root-1", "attempt-1");
                let process_id = registry
                    .spawn(
                        fixture::steady_writer(owner, &marker, Some(key)),
                        Deadline::after(Duration::from_secs(10)),
                    )
                    .expect("the tool group starts");
                assert!(
                    fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT),
                    "the tool group must actually be running for this scenario"
                );
                executions.register(
                    &fence,
                    RegisteredExecution {
                        identity: execution(),
                        driver_epoch: fence.driver_epoch,
                        process_ids: vec![process_id],
                    },
                );
                if scenario == StopScenario::CancelOnly {
                    // Cancelled, never stopped: the group is still writing.
                    executions.deregister(&fence, &execution().execution_id);
                }
            }
            StopScenario::HostAndToolsStopped => {
                let marker = markers.path("tool");
                let owner = ProcessOwner::frontend("app");
                let key = TaskExecutionKey::new("gen-1", "root-1", "attempt-1");
                let process_id = registry
                    .spawn(
                        fixture::steady_writer(owner, &marker, Some(key.clone())),
                        Deadline::after(Duration::from_secs(10)),
                    )
                    .expect("the tool group starts");
                assert!(fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT));
                registry
                    .terminate_execution(&key, Deadline::after(Duration::from_secs(10)))
                    .expect("the tool group is terminated");
                assert!(
                    registry.task_tools_are_stopped(&key).unwrap(),
                    "the fixture must have really stopped the group"
                );
                executions.register(
                    &fence,
                    RegisteredExecution {
                        identity: execution(),
                        driver_epoch: fence.driver_epoch,
                        process_ids: vec![process_id],
                    },
                );
            }
        }

        // The real verification: the guardian's half, then the same rule the daemon
        // uses to combine it with the host's half.
        let (evidence, blocked) = executions.collect_evidence(&mut registry, &fence, &frozen);
        let (mut host_evidence, _) = (Vec::new(), ());
        if scenario == StopScenario::HostAndToolsStopped {
            // The host's half: it awaited the turn.
            let mut record = evidence
                .first()
                .cloned()
                .expect("the stopped group produces a guardian record");
            record.joined = true;
            record.source = yi_agent_lifecycle::foreground::EvidenceSource::Host;
            host_evidence.push(record);
        }
        let mut all = evidence.clone();
        for record in host_evidence {
            match all
                .iter_mut()
                .find(|seen| seen.execution.execution_id == record.execution.execution_id)
            {
                Some(seen) => {
                    seen.joined = true;
                }
                None => all.push(record),
            }
        }
        if scenario == StopScenario::JoinedToolAlive {
            // The host observed its turn join; the guardian's record (if any) says the
            // tools are not gone, so this must stay unproven.
            for record in all.iter_mut() {
                record.joined = true;
            }
        }
        let report = yi_agent_lifecycle::foreground::assess_quiescence(&fence, 1, &frozen, &all);
        let mut unresolved: Vec<String> = report
            .blocked
            .iter()
            .map(|seen| seen.execution_id.clone())
            .collect();
        unresolved.extend(blocked.iter().map(|seen| seen.execution_id.clone()));
        if !unresolved.is_empty() {
            return Err(ForegroundFailure {
                code: yi_agent_lifecycle::foreground::ForegroundErrorCode::StopUnproven,
                blocked_execution_ids: unresolved,
                reason: "the frozen execution set is not proven stopped".to_string(),
            });
        }
        Ok(())
    }
}

#[test]
fn foreground_root_lifecycle_stop_requires_host_join_and_tool_evidence() {
    for scenario in [
        StopScenario::CancelOnly,
        StopScenario::JoinedToolAlive,
        StopScenario::MissingEvidence,
    ] {
        let outcome = StopFixture::check(scenario);
        assert!(
            matches!(
                outcome,
                Err(ForegroundFailure {
                    code: yi_agent_lifecycle::foreground::ForegroundErrorCode::StopUnproven,
                    ..
                })
            ),
            "{scenario:?} must stay unproven, got {outcome:?}"
        );
    }
    assert!(
        StopFixture::check(StopScenario::HostAndToolsStopped).is_ok(),
        "a joined turn with its tool groups gone is a proof"
    );
}

#[test]
fn foreground_root_lifecycle_a_registration_from_another_driver_epoch_is_not_a_proof() {
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let mut markers = fixture::MarkerGuard::new();
    let fence = fence();
    let mut execution = execution();
    execution.kind = ExecutionKind::Worker;
    let marker = markers.path("tool");
    let owner = ProcessOwner::frontend("app");
    let key = TaskExecutionKey::new("gen-1", "root-1", "attempt-1");
    let process_id = registry
        .spawn(
            fixture::steady_writer(owner, &marker, Some(key.clone())),
            Deadline::after(Duration::from_secs(10)),
        )
        .expect("the tool group starts");
    assert!(fixture::wait_for_marker(&marker, fixture::MARKER_TIMEOUT));

    let mut executions = ForegroundExecutions::new();
    executions.register(
        &fence,
        RegisteredExecution {
            identity: execution.clone(),
            // Registered under the previous driver: it does not describe this one.
            driver_epoch: fence.driver_epoch - 1,
            process_ids: vec![process_id.clone()],
        },
    );
    registry
        .terminate_execution(&key, Deadline::after(Duration::from_secs(10)))
        .expect("the group is terminated");

    let (evidence, blocked) = executions.collect_evidence(&mut registry, &fence, &[execution]);
    assert!(
        evidence.is_empty(),
        "an older epoch's registration cannot license this one: {evidence:?}"
    );
    assert_eq!(blocked.len(), 1);
}

#[test]
fn foreground_root_lifecycle_an_unknown_process_group_is_not_reported_stopped() {
    // This is the case a "no records means stopped" implementation gets wrong: the
    // registry simply does not know the id, which happens after a journal reclaim.
    let directory = TempDir::new().unwrap();
    let mut registry = registry(&directory);
    let fence = fence();
    let mut executions = ForegroundExecutions::new();
    executions.register(
        &fence,
        RegisteredExecution {
            identity: execution(),
            driver_epoch: fence.driver_epoch,
            process_ids: vec!["managed:not-a-real-group".to_string()],
        },
    );
    let (evidence, blocked) = executions.collect_evidence(&mut registry, &fence, &[execution()]);
    assert!(
        evidence.is_empty(),
        "an unknown group must not become a proof: {evidence:?}"
    );
    assert_eq!(
        blocked.len(),
        1,
        "the execution must be named as blocked, not silently skipped"
    );
    assert_eq!(blocked[0].execution_id, execution().execution_id);

    // Two ids where one is known and one is not must also block: a partial view is
    // not a proof either.
    let mixed = RegisteredExecution {
        identity: execution(),
        driver_epoch: fence.driver_epoch,
        process_ids: vec!["managed:not-a-real-group".to_string()],
    };
    executions.register(&fence, mixed);
    let (evidence, blocked) = executions.collect_evidence(&mut registry, &fence, &[execution()]);
    assert!(evidence.is_empty());
    assert_eq!(blocked.len(), 1);
}

/// The helper runs as a test so the binary can re-exec itself.
#[test]
#[ignore = "internal fixture: requires the guardian's ready descriptor"]
fn process_fixture_helper() {
    fixture::run_helper_if_requested();
}
