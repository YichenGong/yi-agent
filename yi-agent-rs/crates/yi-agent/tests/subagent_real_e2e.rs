//! Ignored smoke coverage for real-provider subagent configuration.

mod common;

use std::process::Command;
use std::time::Duration;

use common::{resolve_real_llm_test_config, run_command_with_timeout, yi_agent_bin};

#[test]
#[ignore]
fn real_subagent_configuration_skips_without_keys_or_runs_without_leaking_key() {
    let Some(config) =
        resolve_real_llm_test_config().expect("real-test configuration must validate")
    else {
        eprintln!("SKIPPED: no real LLM API key configured");
        return;
    };
    let repository = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(
        repository.path().join("README.md"),
        "real subagent smoke fixture\n",
    )
    .unwrap();
    let mut command = Command::new(yi_agent_bin());
    command
        .arg("--workdir")
        .arg(repository.path())
        .arg("run")
        .arg("Delegate README inspection to a subagent and return a non-empty report. Do not modify files.");
    config.apply_to_command(&mut command);
    let output = run_command_with_timeout(&mut command, Duration::from_secs(300))
        .expect("real subagent timeout");
    assert!(output.status.success(), "real subagent process failed");
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(repository.path().join("README.md")).unwrap(),
        "real subagent smoke fixture\n"
    );
}
