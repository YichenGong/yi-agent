use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::error::ToolsError;

/// Filesystem and network permissions for commands run by builtin tools.
///
/// These names intentionally match Codex's public sandbox modes. Restricted
/// modes deny network access and fail closed when the host has no backend.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

#[derive(Clone, Debug)]
pub struct SandboxPolicy {
    mode: SandboxMode,
    writable_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    pub fn new(
        mode: SandboxMode,
        workspace_root: &Path,
        extra_writable_roots: Vec<PathBuf>,
    ) -> Self {
        let mut writable_roots = Vec::with_capacity(1 + extra_writable_roots.len());
        writable_roots.push(canonicalize_root(workspace_root));
        writable_roots.extend(
            extra_writable_roots
                .into_iter()
                .map(|root| canonicalize_root(&root)),
        );
        writable_roots.sort();
        writable_roots.dedup();
        Self {
            mode,
            writable_roots,
        }
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    pub fn allows_writes(&self) -> bool {
        self.mode != SandboxMode::ReadOnly
    }

    /// Wrap a shell command in the host-native sandbox launcher.
    pub fn command(
        &self,
        shell_command: &str,
        cwd: &Path,
    ) -> Result<(String, Vec<String>), ToolsError> {
        match self.mode {
            SandboxMode::DangerFullAccess => {
                Ok(("sh".into(), vec!["-c".into(), shell_command.into()]))
            }
            SandboxMode::ReadOnly | SandboxMode::WorkspaceWrite => {
                platform_command(self.mode, &self.writable_roots, shell_command, cwd)
            }
        }
    }
}

fn canonicalize_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

#[cfg(target_os = "macos")]
fn platform_command(
    mode: SandboxMode,
    writable_roots: &[PathBuf],
    shell_command: &str,
    _cwd: &Path,
) -> Result<(String, Vec<String>), ToolsError> {
    const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
    if !Path::new(SANDBOX_EXEC).is_file() {
        return Err(ToolsError::SandboxUnavailable(
            "macOS sandbox-exec is unavailable".into(),
        ));
    }

    let mut policy = String::from("(version 1)\n(allow default)\n(deny network*)\n");
    if mode == SandboxMode::ReadOnly {
        policy.push_str("(deny file-write*)\n");
    } else {
        policy.push_str("(deny file-write* (subpath \"/\"))\n");
        for root in writable_roots {
            policy.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                escape_sbpl_path(root)
            ));
        }
    }
    Ok((
        SANDBOX_EXEC.into(),
        vec![
            "-p".into(),
            policy,
            "--".into(),
            "sh".into(),
            "-c".into(),
            shell_command.into(),
        ],
    ))
}

#[cfg(target_os = "macos")]
fn escape_sbpl_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn platform_command(
    mode: SandboxMode,
    writable_roots: &[PathBuf],
    shell_command: &str,
    cwd: &Path,
) -> Result<(String, Vec<String>), ToolsError> {
    let bwrap = find_bwrap().ok_or_else(|| {
        ToolsError::SandboxUnavailable(
            "Bubblewrap (bwrap) is required for sandboxed commands".into(),
        )
    })?;
    let mut args = vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--unshare-net".into(),
    ];
    if mode == SandboxMode::WorkspaceWrite {
        for root in writable_roots {
            let root = root.to_string_lossy().into_owned();
            args.extend(["--bind".into(), root.clone(), root]);
        }
    }
    args.extend([
        "--chdir".into(),
        cwd.to_string_lossy().into_owned(),
        "--".into(),
        "sh".into(),
        "-c".into(),
        shell_command.into(),
    ]);
    Ok((bwrap, args))
}

#[cfg(target_os = "linux")]
fn find_bwrap() -> Option<String> {
    ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .map(str::to_owned)
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join("bwrap"))
                    .find(|path| path.is_file())
                    .map(|path| path.to_string_lossy().into_owned())
            })
        })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn platform_command(
    _mode: SandboxMode,
    _writable_roots: &[PathBuf],
    _shell_command: &str,
    _cwd: &Path,
) -> Result<(String, Vec<String>), ToolsError> {
    Err(ToolsError::SandboxUnavailable(
        "this platform has no supported sandbox backend".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn workspace_write_canonicalizes_and_deduplicates_roots() {
        let temp = TempDir::new().unwrap();
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            temp.path(),
            vec![temp.path().to_path_buf()],
        );
        assert_eq!(
            policy.writable_roots,
            vec![temp.path().canonicalize().unwrap()]
        );
    }

    #[test]
    fn dangerous_mode_runs_sh_without_a_wrapper() {
        let policy = SandboxPolicy::new(SandboxMode::DangerFullAccess, Path::new("/tmp"), vec![]);
        assert_eq!(
            policy.command("echo ok", Path::new("/tmp")).unwrap().0,
            "sh"
        );
    }
}
