//! Ignored smoke coverage for real-provider subagent configuration.

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use common::{
    RealLlmTestConfig, resolve_real_llm_test_config, run_command_with_timeout, yi_agent_bin,
};

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

fn temporary_repository() -> tempfile::TempDir {
    let repository = tempfile::TempDir::new().expect("tempdir");
    std::fs::write(
        repository.path().join("README.md"),
        "real subagent fixture\n",
    )
    .expect("write baseline README");
    git(repository.path(), &["init"]);
    git(repository.path(), &["add", "README.md"]);
    git(
        repository.path(),
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
    repository
}

fn run_real_subagent(config: &RealLlmTestConfig, repository: &Path, prompt: &str) -> Output {
    let mut command = Command::new(yi_agent_bin());
    command
        .arg("--workdir")
        .arg(repository)
        .arg("run")
        .arg(prompt);
    config.apply_to_command(&mut command);
    run_command_with_timeout(&mut command, Duration::from_secs(300)).expect("real subagent timeout")
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
    let repository = temporary_repository();
    let output = run_real_subagent(
        &config,
        repository.path(),
        "Delegate README inspection to a subagent and return a non-empty report. Do not modify files.",
    );
    assert!(output.status.success(), "real subagent process failed");
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    assert_eq!(
        std::fs::read_to_string(repository.path().join("README.md")).unwrap(),
        "real subagent fixture\n"
    );
}
