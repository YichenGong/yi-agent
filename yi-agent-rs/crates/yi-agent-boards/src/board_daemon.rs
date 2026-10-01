//! Per-project board daemon lifecycle: start it in its own session so it
//! outlives the desktop app, probe whether it answers, and stop it.
//!
//! Decision 4 of the plan is "the board keeps running after the desktop app
//! quits". That holds only if the child leaves the app's session *and* process
//! group; a group signal aimed at the app would otherwise take the daemon down
//! with it. [`spawn_detached`] therefore calls `setsid` in the child.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use yi_agent_store::ipc::{IpcRequest, send_request, socket_path_for};

/// How often the readiness and shutdown loops re-probe the socket.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long [`stop`] waits for a daemon that answered `Stop` to actually go
/// quiet before giving up.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// The per-project runtime directory whose socket the board daemon owns.
fn runtime_dir(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("runtime")
}

/// Resolve the project's daemon socket path.
fn socket_for(project: &Path) -> Result<PathBuf, yi_agent_store::ipc::IpcError> {
    socket_path_for(&runtime_dir(project))
}

/// Spawn `exe daemon serve` for `project` in a new session, detached from the
/// caller's process group, with every stdio stream on `/dev/null`.
///
/// Returns the child's pid. The child deliberately outlives the caller: the
/// desktop app must be able to quit without stopping the board.
pub fn spawn_detached(exe: &Path, project: &Path) -> std::io::Result<u32> {
    let mut command = std::process::Command::new(exe);
    command
        .args(["daemon", "serve"])
        .current_dir(project)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // setsid: 让子进程成为新会话的首领，脱离父进程的会话与进程组。
    // 没有这一步，app 以进程组信号退出时会把 daemon 一并杀掉，
    // 「关掉 app 后看板继续跑」就不成立。
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command.spawn()?.id())
}

/// Poll `project`'s daemon socket until it answers a `Status`, or `timeout`
/// elapses. Returns whether it became ready in time.
pub fn wait_ready(project: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if is_running(project) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

/// Whether `project`'s daemon is up and answering right now.
///
/// "Answering" is the only honest signal available: the socket either accepts a
/// `Status` request or it does not.
pub fn is_running(project: &Path) -> bool {
    let Ok(socket) = socket_for(project) else {
        // An unresolvable socket path means there is nothing to talk to.
        return false;
    };
    send_request(&socket, IpcRequest::Status).is_ok()
}

/// Ask `project`'s daemon to stop, then wait until its socket goes quiet.
///
/// Idempotent: a project with no running daemon is already stopped. Errors only
/// when a daemon keeps answering past [`STOP_TIMEOUT`].
pub fn stop(project: &Path) -> Result<(), String> {
    let socket = socket_for(project).map_err(|error| {
        format!(
            "cannot resolve the board daemon socket for {}: {error}",
            project.display()
        )
    })?;

    if is_running(project) {
        if let Err(error) = send_request(&socket, IpcRequest::Stop) {
            // It died between the probe and the send: still a successful stop.
            if !is_running(project) {
                return Ok(());
            }
            return Err(format!(
                "could not tell the board daemon for {} to stop: {error}",
                project.display()
            ));
        }
    }

    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        if !is_running(project) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "board daemon for {} did not stop within {STOP_TIMEOUT:?}",
                project.display()
            ));
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_project() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        (dir, project)
    }

    #[test]
    fn a_project_with_no_daemon_is_not_running_and_never_becomes_ready() {
        let (_dir, project) = empty_project();
        assert!(!is_running(&project));
        assert!(!wait_ready(&project, Duration::from_millis(50)));
    }

    #[test]
    fn stopping_a_project_with_no_daemon_succeeds() {
        // `stop` is idempotent: it is called on removal paths that may already
        // have a quiet daemon, and must not turn that into a failure.
        let (_dir, project) = empty_project();
        assert_eq!(stop(&project), Ok(()));
    }

    #[test]
    fn a_detached_child_leads_its_own_session_and_runs_in_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let marker = dir.path().join("child.txt");
        let stub = dir.path().join("stub.sh");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\n{{ echo pid=$$; echo cwd=$(pwd); echo pgid=$(ps -o pgid= -p $$ | tr -d ' '); }} > {}\nsleep 30\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let pid = spawn_detached(&stub, &project).unwrap();

        // 等 stub 写出自己的身份。文件是重定向时先建、内容后写，只判 exists 会
        // 读到空文件、在下面 get 的 unwrap 上随机 panic，故按内容轮询。
        let text = {
            let mut text = String::new();
            for _ in 0..100 {
                text = std::fs::read_to_string(&marker).unwrap_or_default();
                if ["pid=", "cwd=", "pgid="]
                    .iter()
                    .all(|key| text.contains(key))
                {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            text
        };
        let get = |key: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(&format!("{key}=")))
                .unwrap()
                .to_string()
        };

        assert_eq!(
            get("cwd"),
            project.canonicalize().unwrap().display().to_string()
        );
        assert_eq!(
            get("pgid"),
            get("pid"),
            "setsid 没生效：子进程仍是父进程会话/进程组的成员，app 退出会把它一起带走"
        );

        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}
