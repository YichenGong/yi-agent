//! 进程组机制：让工具 spawn 的子进程自成进程组，并能整组回收。
//!
//! 只放「机制」，不放策略：发什么信号、何时发，由各调用点决定。

use tokio::process::Command;

/// 让 `cmd` spawn 出的子进程自任组长（等价于子进程内 `setpgid(0, 0)`）。
///
/// 使子进程组 pgid == 子进程 pid，从而与 yi-agent 自身进程组隔离。
#[cfg(unix)]
pub(crate) fn configure_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub(crate) fn configure_process_group(_cmd: &mut Command) {}

/// 向进程组 `pid` 发送信号 `sig`。`pid` 为组长 pid（即子进程自身 pid）。
///
/// 对已消失的组返回 `ESRCH`，属正常情况，调用方应忽略。
#[cfg(unix)]
pub(crate) fn signal_process_group(pid: u32, sig: i32) {
    unsafe {
        libc::kill(-(pid as libc::pid_t), sig);
    }
}

#[cfg(not(unix))]
pub(crate) fn signal_process_group(_pid: u32, _sig: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::process::Command;

    #[cfg(unix)]
    #[tokio::test]
    async fn configure_process_group_puts_child_in_its_own_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        configure_process_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("child id while running");

        // 子进程自成一组：pgid == 自身 pid。
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        assert_eq!(pgid, pid as libc::pid_t, "child should lead its own group");

        // 且不与当前进程同组。
        let own = unsafe { libc::getpgid(0) };
        assert_ne!(pgid, own, "child must not share the caller's group");

        signal_process_group(pid, libc::SIGKILL);
        let _ = child.wait().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn signal_process_group_kills_a_surviving_background_child() {
        // shell 自任组长，内部再起一个后台子孙；杀整组应把子孙一并收掉。
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sh -c 'sleep 30' & wait");
        configure_process_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("child id");

        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        signal_process_group(pid, libc::SIGKILL);
        let _ = child.wait().await;

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // 整组已被清掉：组长 pid 不再可查。
        let gone = unsafe { libc::kill(pid as libc::pid_t, 0) };
        assert_eq!(gone, -1, "group leader must be gone after group kill");
    }
}
