//! End-to-end tests calling `yi-agent run` with a real LLM.
//! All tests are #[ignore]'d; run with: cargo test -p yi-agent --test e2e_real -- --ignored

use std::path::PathBuf;
use std::process::Command;

/// Path to the compiled yi-agent binary.
fn yi_agent_bin() -> PathBuf {
    // CARGO_BIN_EXE_yi-agent is set by cargo when running integration tests.
    // Fallback to target/debug/yi-agent for manual runs.
    option_env!("CARGO_BIN_EXE_yi-agent")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/yi-agent"))
}

#[test]
#[ignore]
fn e2e_error_no_api_key() {
    // No API key in env: yi-agent run should fail with non-zero exit and
    // an auth/provider error message on stderr.
    let output = Command::new(yi_agent_bin())
        .arg("run")
        .arg("hi")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("MODEL_API_KEY")
        .output()
        .expect("failed to spawn yi-agent");

    assert!(!output.status.success(), "should fail without API key");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("error")
            || stderr.to_lowercase().contains("auth")
            || stderr.to_lowercase().contains("api key"),
        "stderr should mention error/auth/api key, got: {stderr}"
    );
}
