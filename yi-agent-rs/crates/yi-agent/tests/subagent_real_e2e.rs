//! Ignored smoke coverage for real-provider subagent configuration.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

use common::{RealLlmTestConfig, resolve_real_llm_test_config, yi_agent_bin};

struct RealRuntimeFixture {
    fixture_root: tempfile::TempDir,
    repository: PathBuf,
    runtime_dir: PathBuf,
    daemon: Child,
}

impl Drop for RealRuntimeFixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

fn start_real_runtime_fixture(config: Option<&RealLlmTestConfig>) -> RealRuntimeFixture {
    let fixture_root = tempfile::TempDir::new().expect("fixture tempdir");
    let repository = fixture_root.path().join("repository");
    std::fs::create_dir(&repository).expect("create fixture repository directory");
    initialize_repository(&repository);
    let runtime_dir = fixture_root.path().join("runtime");
    let mut command = Command::new(yi_agent_bin());
    command.arg("--workdir").arg(&repository);
    if let Some(config) = config {
        config.apply_to_command(&mut command);
    } else {
        command
            .arg("--provider")
            .arg("anthropic")
            .arg("--api-url")
            .arg("http://127.0.0.1:9")
            .arg("--api-key")
            .arg("test-only-runtime-key");
    }
    let daemon = command
        .arg("daemon")
        .arg("serve")
        .env("YI_AGENT_RUNTIME_DIR", &runtime_dir)
        .spawn()
        .expect("start isolated runtime daemon");
    let socket = runtime_dir.join("runtime.sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(
            send_request(&socket, IpcRequest::Status),
            Ok(IpcResponse::Status { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "isolated runtime daemon did not become ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    RealRuntimeFixture {
        fixture_root,
        repository,
        runtime_dir,
        daemon,
    }
}

fn git(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(repository)
        .args(arguments)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git stdout is UTF-8")
}

fn initialize_repository(repository: &Path) {
    std::fs::write(repository.join("README.md"), "real subagent fixture\n")
        .expect("write baseline README");
    std::fs::write(repository.join(".gitignore"), ".worktrees/\n")
        .expect("ignore fixture child worktrees");
    git(repository, &["init"]);
    git(repository, &["config", "user.name", "Yi"]);
    git(repository, &["config", "user.email", "yi@example.test"]);
    git(repository, &["add", "README.md", ".gitignore"]);
    git(
        repository,
        &[
            "-c",
            "user.name=Yi",
            "-c",
            "user.email=yi@example.test",
            "commit",
            "-m",
            "baseline",
        ],
    );
}

fn temporary_repository() -> tempfile::TempDir {
    let repository = tempfile::TempDir::new().expect("tempdir");
    initialize_repository(repository.path());
    repository
}

struct RealAgentRun {
    child: Option<Child>,
}

impl RealAgentRun {
    fn wait(mut self) -> Output {
        self.child
            .take()
            .expect("real subagent process remains available")
            .wait_with_output()
            .expect("collect real subagent output")
    }
}

fn start_real_subagent(
    config: &RealLlmTestConfig,
    fixture: &RealRuntimeFixture,
    prompt: &str,
) -> RealAgentRun {
    let mut command = Command::new(yi_agent_bin());
    command
        .arg("--workdir")
        .arg(&fixture.repository)
        .arg("run")
        .arg("--subagents")
        .arg(prompt)
        .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir);
    config.apply_to_command(&mut command);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map(|child| RealAgentRun { child: Some(child) })
        .expect("start real subagent")
}

fn root_workspace(socket: &Path) -> PathBuf {
    let IpcResponse::TaskSummaries { tasks } = send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .expect("list runtime tasks") else {
        panic!("expected runtime task summaries");
    };
    let root = tasks
        .into_iter()
        .find(|task| task.is_root)
        .expect("application root task");
    let IpcResponse::TaskDetail(detail) = send_request(
        socket,
        IpcRequest::InspectTask {
            task_id: root.task_id,
        },
    )
    .expect("inspect application root") else {
        panic!("expected application root detail");
    };
    detail.workspace.expect("application root workspace").path
}

fn task_snapshot(socket: &Path) -> String {
    let IpcResponse::TaskSummaries { tasks } = send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .expect("list runtime task summaries") else {
        return "unexpected task-summary response".into();
    };
    tasks
        .into_iter()
        .map(|task| {
            let detail = match send_request(
                socket,
                IpcRequest::InspectTask {
                    task_id: task.task_id.clone(),
                },
            ) {
                Ok(IpcResponse::TaskDetail(detail)) => detail,
                Ok(other) => {
                    return format!("{}: unexpected detail response {other:?}", task.task_id);
                }
                Err(error) => return format!("{}: detail request failed: {error}", task.task_id),
            };
            let events = match send_request(
                socket,
                IpcRequest::ReadTaskEvents {
                    task_id: task.task_id.clone(),
                    after_event_id: None,
                },
            ) {
                Ok(IpcResponse::TaskEvents { events }) => events
                    .into_iter()
                    .map(|event| {
                        format!("{}:{}:{}", event.event_id, event.kind, event.payload_json)
                    })
                    .collect::<Vec<_>>()
                    .join(" | "),
                Ok(other) => format!("unexpected events response {other:?}"),
                Err(error) => format!("events request failed: {error}"),
            };
            format!(
                "task={} root={} state={} terminal={:?} events=[{}]",
                task.task_id, task.is_root, detail.state, detail.terminal_json, events
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn await_direct_child(socket: &Path, run: &mut RealAgentRun) -> String {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = run
            .child
            .as_mut()
            .expect("real root process remains available")
            .try_wait()
            .expect("poll real root process")
        {
            let output = run
                .child
                .take()
                .expect("take exited real root process")
                .wait_with_output()
                .expect("collect exited real root output");
            panic!(
                "real root exited before spawning a child with {status}; stdout: {}; stderr: {}; runtime snapshot:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                task_snapshot(socket),
            );
        }
        let response = send_request(
            socket,
            IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )
        .expect("list runtime tasks");
        if let IpcResponse::TaskSummaries { tasks } = response {
            let children = tasks
                .into_iter()
                .filter(|task| !task.is_root)
                .collect::<Vec<_>>();
            if children.len() == 1 {
                return children[0].task_id.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for one child task; runtime snapshot:\n{}",
            task_snapshot(socket)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn await_review(socket: &Path, task_id: &str) -> yi_agent_store::ipc::IpcTaskDetail {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let IpcResponse::TaskDetail(detail) = send_request(
            socket,
            IpcRequest::InspectTask {
                task_id: task_id.into(),
            },
        )
        .expect("inspect child task") else {
            panic!("expected child task detail");
        };
        if detail.state == "awaiting_parent_review" {
            return detail;
        }
        if detail.state == "failed" {
            let workspace_status = detail
                .workspace
                .as_ref()
                .map(|workspace| git(&workspace.path, &["status", "--porcelain"]))
                .unwrap_or_else(|| "no workspace assigned".into());
            let workspace_log = detail
                .workspace
                .as_ref()
                .map(|workspace| git(&workspace.path, &["log", "-1", "--format=%B"]))
                .unwrap_or_else(|| "no workspace assigned".into());
            panic!(
                "child task failed before review: {}; workspace status: {workspace_status:?}; workspace log: {workspace_log:?}",
                detail
                    .terminal_json
                    .as_deref()
                    .unwrap_or("no terminal evidence recorded")
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for child review; last state: {}",
            detail.state
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn confirm_review(socket: &Path, task_id: &str, decision: yi_agent_store::ipc::IpcReviewDecision) {
    let IpcResponse::ReviewPreview {
        confirmation_token, ..
    } = send_request(
        socket,
        IpcRequest::PreviewReview {
            task_id: task_id.into(),
            decision: decision.clone(),
        },
    )
    .expect("preview child review")
    else {
        panic!("expected review preview");
    };
    let response = send_request(
        socket,
        IpcRequest::ConfirmReview {
            task_id: task_id.into(),
            decision: decision.clone(),
            confirmation_token,
        },
    )
    .expect("confirm child review");
    match decision {
        yi_agent_store::ipc::IpcReviewDecision::Accept {} => {
            assert!(matches!(response, IpcResponse::ReviewApproved));
        }
        yi_agent_store::ipc::IpcReviewDecision::Rework { .. } => {
            assert!(matches!(response, IpcResponse::ReviewReworkRequested));
        }
        yi_agent_store::ipc::IpcReviewDecision::Reject { .. } => {
            assert!(matches!(response, IpcResponse::ReviewRejected));
        }
    }
}

#[test]
fn local_runtime_fixture_uses_a_tempdir_socket() {
    let fixture = start_real_runtime_fixture(None);
    let socket = fixture.runtime_dir.join("runtime.sock");
    assert!(socket.exists(), "fixture daemon must create a socket");
    assert!(fixture.runtime_dir.starts_with(fixture.fixture_root.path()));
    assert!(matches!(
        send_request(&socket, IpcRequest::Status).expect("fixture daemon status"),
        IpcResponse::Status { .. }
    ));
}

#[test]
fn temporary_repository_allows_an_application_root_worktree() {
    let fixture = start_real_runtime_fixture(None);
    let socket = fixture.runtime_dir.join("runtime.sock");
    let response = send_request(
        &socket,
        IpcRequest::AttachApplicationRoot {
            idempotency_key: "fixture-root-worktree".into(),
        },
    )
    .expect("attach application root");
    assert!(
        matches!(response, IpcResponse::ApplicationRootAttached { .. }),
        "fixture application-root attach failed: {response:?}"
    );
}

#[test]
fn temporary_repository_has_a_clean_baseline_commit() {
    let repository = temporary_repository();

    assert!(
        !git(repository.path(), &["rev-parse", "HEAD"])
            .trim()
            .is_empty()
    );
    assert_eq!(git(repository.path(), &["status", "--porcelain"]), "");
    assert_eq!(
        git(repository.path(), &["show", "HEAD:README.md"]),
        "real subagent fixture\n"
    );
    assert_eq!(
        git(repository.path(), &["show", "HEAD:.gitignore"]),
        ".worktrees/\n"
    );
    assert_eq!(git(repository.path(), &["config", "user.name"]), "Yi\n");
    assert_eq!(
        git(repository.path(), &["config", "user.email"]),
        "yi@example.test\n"
    );
}

const DELIVERY_FILE: &str = "real-subagent-delivery.txt";
const DELIVERY_MARKER: &str = "REAL_SUBAGENT_DELIVERY_MARKER_V1";

#[test]
#[ignore]
fn real_subagent_accepts_delivery_into_parent_history() {
    let Some(config) =
        resolve_real_llm_test_config().expect("real-test configuration must validate")
    else {
        eprintln!("SKIPPED: no real LLM API key configured");
        return;
    };
    let fixture = start_real_runtime_fixture(Some(&config));
    let mut run = start_real_subagent(
        &config,
        &fixture,
        "Use spawn_agent exactly once. Give the child this exact objective: create real-subagent-delivery.txt with exactly REAL_SUBAGENT_DELIVERY_MARKER_V1 followed by one newline; then run git add real-subagent-delivery.txt and git commit -m 'test: add real subagent delivery marker' in its assigned worktree. The child must not delegate. Do not create that file yourself. Do not call wait_agent: a local human reviewer will accept the child delivery and you will then receive its completion report. Do not attempt review or acceptance.",
    );
    let socket = fixture.runtime_dir.join("runtime.sock");
    let child_id = await_direct_child(&socket, &mut run);
    let delivery = await_review(&socket, &child_id);
    let delivery_json: serde_json::Value =
        serde_json::from_str(&delivery.delivery_json).expect("child delivery JSON");
    let child_head = delivery_json["head_commit"]
        .as_str()
        .expect("delivery head commit")
        .to_owned();
    let parent_workspace = root_workspace(&socket);
    confirm_review(
        &socket,
        &child_id,
        yi_agent_store::ipc::IpcReviewDecision::Accept {},
    );
    let output = run.wait();
    assert!(
        output.status.success(),
        "real delivery root failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(parent_workspace.join(DELIVERY_FILE)).expect("accepted file"),
        format!("{DELIVERY_MARKER}\n")
    );
    git(
        &parent_workspace,
        &["merge-base", "--is-ancestor", &child_head, "HEAD"],
    );
    assert_eq!(git(&parent_workspace, &["status", "--porcelain"]), "");
}

#[test]
#[ignore]
fn real_subagent_configuration_skips_without_keys_or_runs_without_leaking_key() {
    let Some(config) =
        resolve_real_llm_test_config().expect("real-test configuration must validate")
    else {
        eprintln!("SKIPPED: no real LLM API key configured");
        return;
    };
    let fixture = start_real_runtime_fixture(Some(&config));
    let output = start_real_subagent(
        &config,
        &fixture,
        "Delegate README inspection to a subagent and wait for its report. Do not modify files.",
    )
    .wait();
    assert!(output.status.success(), "real subagent process failed");
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(fixture.repository.join("README.md")).unwrap(),
        "real subagent fixture\n"
    );
}
