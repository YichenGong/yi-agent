//! `yi-agent completions <shell>` emits a shell completion script on stdout.
//!
//! These tests drive the real binary: the completion script is only meaningful
//! if the built CLI actually emits it. They assert the two things a user cares
//! about -- the command succeeds and the script mentions the CLI's own flags and
//! subcommands -- without pinning the exact (clap-version dependent) body.

use std::process::Command;

fn yi_agent_bin() -> std::path::PathBuf {
    std::env::var_os("CARGO_BIN_EXE_yi-agent")
        .map(Into::into)
        .expect("Cargo must provide the yi-agent binary")
}

fn completions(shell: &str) -> std::process::Output {
    Command::new(yi_agent_bin())
        .args(["completions", shell])
        .output()
        .expect("run yi-agent completions")
}

#[test]
fn completions_lists_the_supported_shells_in_help() {
    let output = Command::new(yi_agent_bin())
        .args(["completions", "--help"])
        .output()
        .expect("run yi-agent completions --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    for shell in ["bash", "zsh", "fish", "powershell"] {
        assert!(
            stdout.contains(shell),
            "completions help must advertise shell `{shell}`: {stdout}"
        );
    }
}

#[test]
fn zsh_completion_mentions_top_level_flags_and_subcommands() {
    let output = completions("zsh");
    assert!(
        output.status.success(),
        "zsh completion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = String::from_utf8_lossy(&output.stdout);
    assert!(!script.trim().is_empty(), "zsh completion script is empty");
    // A representative flag and subcommand must be present so tab completion in
    // a real shell would offer them.
    for token in ["--workdir", "--sandbox", "completions"] {
        assert!(
            script.contains(token),
            "zsh completion script missing `{token}`: {script}"
        );
    }
}

#[test]
fn bash_completion_is_non_empty() {
    let output = completions("bash");
    assert!(
        output.status.success(),
        "bash completion failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let script = String::from_utf8_lossy(&output.stdout);
    assert!(!script.trim().is_empty(), "bash completion script is empty");
    assert!(
        script.contains("yi-agent"),
        "bash completion script must reference the binary name: {script}"
    );
}

#[test]
fn unknown_shell_is_rejected_without_a_script() {
    let output = completions("not-a-shell");
    assert!(
        !output.status.success(),
        "an unknown shell must be rejected"
    );
    assert!(
        output.stdout.is_empty(),
        "a rejected shell must not emit a script on stdout"
    );
}
