//! `bash` under a managed runtime, through a real guardian channel.
//!
//! The assertion that matters is not "the command ran" -- it always ran. It is
//! "the process is registered under an owner and its whole group stops", which is
//! what makes quitting a frontend actually stop that frontend's work. So the test
//! runs a real channel server and inspects the registry it built.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use yi_agent_core::{Tool, ToolEvent};
use yi_agent_lifecycle::types::ArtifactIdentity;
use yi_agent_lifecycle::types::RuntimeScope;
use yi_agent_supervisors::lifecycle::guardian::{ProcessOwner, ProcessRegistry};
use yi_agent_supervisors::lifecycle::managed_channel::{ChannelGrant, serve_channel};
use yi_agent_tools::ManagedSpawner;
use yi_agent_tools::{BashTool, ToolsContext};

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

/// The spawn requests the test's channel server observed, as `(program, arg)`.
type ObservedSpawns = Arc<Mutex<Vec<(String, String)>>>;
/// The owner names the test's channel server registered, in order.
type ObservedOwners = Arc<Mutex<Vec<String>>>;

/// Starts a channel server and returns a factory that dials it, plus the record
/// handle the test inspects.
fn channel(directory: &tempfile::TempDir) -> (ManagedSpawner, ObservedSpawns, ObservedOwners) {
    let (guardian_side, client_side) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    let scope = RuntimeScope {
        key: directory.path().to_string_lossy().into_owned(),
        runtime_dir: directory.path().to_path_buf(),
        project_key: "/project".to_string(),
        instance_home: directory.path().to_path_buf(),
    };
    let (registry, report) = ProcessRegistry::open(&scope, "gen-1", &artifact()).expect("registry");
    assert!(report.is_safe());

    let observed = Arc::new(Mutex::new(Vec::new()));
    let observed_in_thread = Arc::clone(&observed);
    let owners = Arc::new(Mutex::new(Vec::new()));
    let owners_in_thread = Arc::clone(&owners);
    std::thread::spawn(move || {
        let mut registry = registry;
        let record_owner = |registry: &ProcessRegistry, owners: &Arc<Mutex<Vec<String>>>| {
            for record in registry.records() {
                let text = format!("{:?}", record.owner);
                let mut owners = owners.lock().unwrap();
                if !owners.contains(&text) {
                    owners.push(text);
                }
                let mut observed = observed_in_thread.lock().unwrap();
                observed.push((record.process_id.clone(), record.pgid.to_string()));
            }
        };
        // One request at a time: serve_channel handles a spawn, we record, then it
        // waits for the next request (the terminate that ends the group).
        let _ = serve_channel(
            &mut registry,
            &ChannelGrant::new(ProcessOwner::frontend("app")),
            guardian_side,
            Duration::from_secs(20),
        );
        record_owner(&registry, &owners_in_thread);
    });

    // A factory has to hand out a fresh connection per child, but this test starts
    // exactly one, so the single pre-made peer is enough.
    let handed = Arc::new(Mutex::new(Some(client_side)));
    let factory = ManagedSpawner::new(Arc::new(move || {
        handed
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| std::io::Error::other("only one child was expected"))
    }));
    (factory, observed, owners)
}

#[tokio::test]
async fn a_managed_bash_command_is_registered_under_its_owner_and_stops_as_a_group() {
    let directory = tempfile::TempDir::new().unwrap();
    let (factory, observed, owners) = channel(&directory);

    let ctx = Arc::new(
        ToolsContext::new(directory.path().to_path_buf()).into_managed(
            yi_agent_tools::ManagedProcessPolicy::Frontend {
                client_run_id: "app".into(),
            },
            factory,
        ),
    );
    let tool = BashTool::new(ctx);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let args = serde_json::json!({ "command": "echo managed-out; echo managed-err 1>&2" });
    let result = tokio::time::timeout(Duration::from_secs(30), tool.call_stream(args, tx))
        .await
        .expect("bash must not hang");

    let text = format!("{:?}", result.content);
    assert!(text.contains("managed-out"), "stdout must arrive: {text}");
    assert!(text.contains("managed-err"), "stderr must arrive: {text}");

    // Drain the events so the channel body is not left mid-send.
    let mut saw_exit = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, ToolEvent::Exit { .. }) {
            saw_exit = true;
        }
    }
    assert!(saw_exit, "an exit event must be emitted");

    // The child really went through the guardian: it is in the registry under the
    // frontend that owns the channel.
    // The server thread records after its channel ends, and the child's `Drop`
    // sends that terminate on a detached thread. Poll instead of assuming an
    // ordering the test does not control.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let owners = owners.lock().unwrap().clone();
        let observed = observed.lock().unwrap().clone();
        if owners.iter().any(|owner| owner.contains("app")) && !observed.is_empty() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the spawned process was never registered under the frontend: owners={owners:?} \
             records={observed:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_failed_managed_spawn_reports_an_error_instead_of_running_locally() {
    let directory = tempfile::TempDir::new().unwrap();
    // A factory that refuses: this models a guardian that is gone. The tool must
    // fail rather than quietly fall back to a local unregistered child, which
    // would be exactly the leak the managed path exists to prevent.
    let factory = ManagedSpawner::new(Arc::new(|| {
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "no guardian",
        ))
    }));
    let ctx = Arc::new(
        ToolsContext::new(directory.path().to_path_buf()).into_managed(
            yi_agent_tools::ManagedProcessPolicy::Frontend {
                client_run_id: "app".into(),
            },
            factory,
        ),
    );
    let tool = BashTool::new(ctx);

    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let result = tool
        .call_stream(serde_json::json!({ "command": "echo leaked" }), tx)
        .await;
    let text = format!("{:?}", result.content);
    assert!(!text.contains("leaked"), "nothing must have run: {text}");
    assert!(
        text.contains("no guardian") || text.to_lowercase().contains("error"),
        "the failure must be reported: {text}"
    );
    // The tool still emits Exit so the UI finalizes the task.
    let mut saw_exit = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, ToolEvent::Exit { .. }) {
            saw_exit = true;
        }
    }
    assert!(saw_exit, "a failed spawn must still emit Exit");
}

#[tokio::test]
async fn a_managed_command_with_slow_output_does_not_stall_the_wait_loop() {
    // The wait loop rebuilds its branches on every pass. A managed wait performed
    // inside a branch would queue one guardian request per pass, and the second
    // request would sit behind the first until it timed out. This command emits
    // output for about a second, so the loop passes many times before it ends.
    let directory = tempfile::TempDir::new().unwrap();
    let (factory, _observed, _owners) = channel(&directory);

    let ctx = Arc::new(
        ToolsContext::new(directory.path().to_path_buf()).into_managed(
            yi_agent_tools::ManagedProcessPolicy::Frontend {
                client_run_id: "app".into(),
            },
            factory,
        ),
    );
    let tool = BashTool::new(ctx);

    let (tx, _rx) = tokio::sync::mpsc::channel(256);
    let args = serde_json::json!({
        "command": "i=0; while [ \"$i\" -lt 10 ]; do printf \"tick-%s\\n\" \"$i\"; i=$((i+1)); sleep 0.1; done"
    });
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(20), tool.call_stream(args, tx))
        .await
        .expect("a slow managed command must not hang");
    let elapsed = started.elapsed();
    let text = format!("{:?}", result.content);
    assert!(text.contains("tick-9"), "all output must arrive: {text}");
    assert!(
        text.contains("exit: 0"),
        "the exit code must be reported: {text}"
    );
    assert!(
        elapsed < Duration::from_secs(15),
        "the loop must not queue one wait request per pass: took {elapsed:?}"
    );
}
