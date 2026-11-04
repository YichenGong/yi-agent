//! 一次自述命令的进程身份与收口。
//!
//! 只有 [`super::describe`] 能看见这里：FD、PID、进程组号不出这个模块。
//!
//! # 为什么用具名进程组
//!
//! `manifest` 自述可能拉起后台后代（例如插件顺手起了个 warm-up 进程）。只
//! `child.kill()` 会留下这些后代：它们可能仍然持有 stdout/stderr 写端，把宿主的
//! 读取拖住，甚至在自述结束后继续跑。独立进程组让「这次自述」成为一个可整体
//! 收口的单元——但**只收口这一次**，绝不碰正常 `run` 进程或宿主的组。
//!
//! # 为什么不提前回收
//!
//! 进程组身份靠「直接子进程还没被 wait」保持占用：只要 leader 未被回收，它的
//! PID 就不会被复用，负 pgid 的信号就仍然指向这一组。所以退出只用
//! `waitid(WNOWAIT)` **观察**，清理完组之后才真正 `wait()` 回收。
//! 反过来，一旦已经回收，就不能再凭缓存的 pgid 发信号——那个号可能已经属于
//! 别人了。

use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, Stdio};

use super::{DescribeError, ObservedExit};

/// 一次自述的直接子进程及其读端。`Drop` 兜底展开清理。
pub(super) struct ChildSession {
    /// 直接子进程。`None` 表示已经被 `finish` 回收。
    child: Option<Child>,
    /// 本会话是否仍持有 child 的回收权。外部回收（ECHILD）后为 `false`。
    owns_child: bool,
    /// 是否仍可对缓存 pgid 发信号。回收后必须解除。
    may_signal_group: bool,
    /// 已观察（但未回收）的退出状态；重复观察返回同一份。
    observed_exit: Option<ObservedExit>,
    /// `finish` 是否已经跑过（幂等，且 Drop 不会补第二刀）。
    finished: bool,

    pub(super) stdout: Option<ChildStdout>,
    pub(super) stderr: Option<ChildStderr>,

    #[cfg(test)]
    events: Vec<Event>,
}

/// 清理过程中的顺序记录（仅测试使用）。
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Event {
    SignalGroup,
    ClosePipes,
    SignalChild,
    DisarmGroup,
    Reap,
}

/// 回收强度：由「是否还持有 child」「是否已确认它不会再活」决定。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReapMode {
    None,
    Blocking,
    NonBlocking,
}

/// 选回收强度。
///
/// 只有「已观察退出」或「定向信号发送成功」才敢阻塞等待；两者都没有时只做一次
/// 非阻塞尝试——在无法确认 child 已死的前提下阻塞 `wait()`，等于把超时换成
/// 又一处无界等待。
pub(super) fn reap_mode(owns_child: bool, observed_exit: bool, child_kill_ok: bool) -> ReapMode {
    if !owns_child {
        ReapMode::None
    } else if observed_exit || child_kill_ok {
        ReapMode::Blocking
    } else {
        ReapMode::NonBlocking
    }
}

impl ChildSession {
    pub(super) fn spawn(command: &str) -> Result<Self, DescribeError> {
        let child = Command::new(command)
            .arg("manifest")
            // 独立进程组：`pgid == child pid`，收口时按组发信号。
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| DescribeError::Io {
                stage: "spawn the plugin",
                source,
            })?;

        // spawn 成功之后**立刻**进 guard：接管或设 flags 失败时也要走同一套清理，
        // 不能只靠不返回诊断的 Drop。
        let mut session = Self {
            child: Some(child),
            owns_child: true,
            may_signal_group: true,
            observed_exit: None,
            finished: false,
            stdout: None,
            stderr: None,
            #[cfg(test)]
            events: Vec::new(),
        };

        let taken = (|| -> Result<(), DescribeError> {
            let child = session.child.as_mut().expect("just spawned");
            let stdout = child.stdout.take().ok_or_else(|| DescribeError::Io {
                stage: "take the plugin stdout pipe",
                source: std::io::Error::other("stdout was not captured as a pipe"),
            })?;
            let stderr = child.stderr.take().ok_or_else(|| DescribeError::Io {
                stage: "take the plugin stderr pipe",
                source: std::io::Error::other("stderr was not captured as a pipe"),
            })?;
            set_non_blocking(&stdout, "set the plugin stdout pipe non-blocking")?;
            set_non_blocking(&stderr, "set the plugin stderr pipe non-blocking")?;
            session.stdout = Some(stdout);
            session.stderr = Some(stderr);
            Ok(())
        })();

        if let Err(primary) = taken {
            // 与正常出口共用 combine：主因保留，清理诊断只做附注。
            let cleanup = session.finish();
            return Err(match cleanup {
                Ok(()) => primary,
                Err(cleanup) => DescribeError::WithCleanup {
                    primary: Box::new(primary),
                    cleanup: super::cleanup_message(cleanup),
                },
            });
        }
        Ok(session)
    }

    /// 观察退出，但**不回收**。结果缓存，重复调用返回同一份。
    pub(super) fn observe(&mut self) -> Result<Option<ObservedExit>, DescribeError> {
        if self.finished {
            return Ok(self.observed_exit);
        }
        if let Some(exit) = self.observed_exit {
            return Ok(Some(exit));
        }
        if !self.owns_child {
            // 所有权早就不在本进程：再猜一次只会掩盖「谁收的」这个问题。
            return Err(DescribeError::OwnershipLost);
        }
        let pid = self.pid();
        match observe_exit(pid) {
            Ok(Some(exit)) => {
                self.observed_exit = Some(exit);
                Ok(Some(exit))
            }
            Ok(None) => Ok(None),
            Err(source) if source.raw_os_error() == Some(libc::ECHILD) => {
                // child 被外部回收（或平台自动回收）。立刻解除组信号能力、
                // 关闭读端：不再按缓存 PID/pgid 发信号，也不作阻塞 wait——
                // 那是唯一可能误杀别人的分支。
                self.owns_child = false;
                self.may_signal_group = false;
                self.stdout = None;
                self.stderr = None;
                Err(DescribeError::OwnershipLost)
            }
            Err(source) => Err(DescribeError::Io {
                stage: "wait for the plugin",
                source,
            }),
        }
    }

    /// 一次性收口：组清理先于回收；之后不再使用缓存 pgid。
    pub(super) fn finish(&mut self) -> Result<(), DescribeError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.cleanup()
    }

    /// 收口的实际次序。所有出口（正常返回、初始化失败、panic 展开）共用它。
    fn cleanup(&mut self) -> Result<(), DescribeError> {
        // 先解除「后续还能发信号」的能力，确保所有出口都不会对旧 pgid 重试；
        // 真正需要发信号的分支在下面显式重新判定。
        let may_signal = std::mem::replace(&mut self.may_signal_group, false);
        let owns_child = std::mem::replace(&mut self.owns_child, false);

        if !owns_child {
            // 所有权已丢失：只关本地读端，不发任何信号。
            self.close_pipes();
            return Ok(());
        }

        let observed = self.observed_exit.is_some();
        let mut failures: Vec<String> = Vec::new();

        // 1. 先清组：直接子进程活着、或已退出但未回收时，pgid 仍属于这一组。
        //
        // 不能因为「已观察到 leader 退出」就跳过：那时后代可能仍活着，而正是
        // 这种情况（后代继承写端）才需要收口。
        if may_signal {
            match signal_group(self.pid(), libc::SIGKILL) {
                Ok(()) => {}
                Err(source) if source.raw_os_error() == Some(libc::ESRCH) => {
                    // 组已不存在：可接受的「没有东西可杀」，但**不是** child 已死
                    // 的证明——下面仍按规则定向补刀。
                }
                // macOS 实况（独立探针验证）：组里只剩一个僵尸 leader 时
                // `kill(-pgid)` 返回 EPERM 而不是 ESRCH；组里有存活后代时返回 0。
                // 因此 EPERM 在这里的含义是「组内没有可信号的活进程」，与 ESRCH
                // 同属「没有东西可杀」。它**不**证明 leader 已死——下面仍按规则
                // 定向补刀并用一次 try_wait 确认。
                Err(source) if source.raw_os_error() == Some(libc::EPERM) => {}
                Err(source) => failures.push(format!(
                    "could not signal the plugin process group: {source}"
                )),
            }
            #[cfg(test)]
            self.events.push(Event::SignalGroup);
        }

        // 2. 关闭本地读端：即使后代逃逸并继续持写端，也不再挡住宿主。
        self.close_pipes();

        // 3. 未观察到退出时，再定向一次直接 child。
        //
        // 组信号成功**不**等于 leader 已死，所以不拿它当作可以阻塞 wait 的依据。
        let mut child_kill_ok = false;
        if !observed {
            match signal_child(self.pid(), libc::SIGKILL) {
                Ok(()) => {
                    child_kill_ok = true;
                    #[cfg(test)]
                    self.events.push(Event::SignalChild);
                }
                Err(source) if source.raw_os_error() == Some(libc::ESRCH) => {
                    // 只有一次 try_wait 确认它确实已经退出，ESRCH 才算消解。
                    #[cfg(test)]
                    self.events.push(Event::SignalChild);
                    match self.try_wait_once() {
                        Ok(true) => child_kill_ok = true,
                        Ok(false) => failures.push(
                            "the plugin process could not be confirmed as reaped".to_string(),
                        ),
                        Err(source) => {
                            failures.push(format!("could not confirm the plugin exited: {source}"))
                        }
                    }
                }
                Err(source) => {
                    failures.push(format!("could not signal the plugin process: {source}"));
                    #[cfg(test)]
                    self.events.push(Event::SignalChild);
                }
            }
        }

        // 4. 回收。能力已解除，这里之后不再有任何针对旧 pgid 的信号。
        #[cfg(test)]
        self.events.push(Event::DisarmGroup);
        let mode = reap_mode(true, observed, child_kill_ok);
        match self.reap(mode) {
            Ok(()) => {}
            Err(message) => failures.push(message),
        }
        #[cfg(test)]
        self.events.push(Event::Reap);

        if failures.is_empty() {
            Ok(())
        } else {
            Err(DescribeError::CleanupFailed(failures.join("; ")))
        }
    }

    fn reap(&mut self, mode: ReapMode) -> Result<(), String> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        match mode {
            ReapMode::None => {
                // 不该到这里（ownes_child 为真才有 reap）；保守放回并把 child 交回
                // 给操作系统，绝不阻塞。
                self.child = Some(child);
                Ok(())
            }
            ReapMode::Blocking => loop {
                match child.wait() {
                    Ok(_) => return Ok(()),
                    Err(source) if source.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(source) => {
                        return Err(format!("could not reap the plugin process: {source}"));
                    }
                }
            },
            ReapMode::NonBlocking => match child.try_wait() {
                Ok(Some(_)) => Ok(()),
                Ok(None) => Err("the plugin process did not exit after the cleanup signal".into()),
                Err(source) if source.kind() == std::io::ErrorKind::Interrupted => {
                    Err("the plugin process could not be confirmed as reaped".into())
                }
                Err(source) => Err(format!("could not reap the plugin process: {source}")),
            },
        }
    }

    fn try_wait_once(&mut self) -> std::io::Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(true);
        };
        child.try_wait().map(|status| status.is_some())
    }

    fn close_pipes(&mut self) {
        if self.stdout.take().is_some() || self.stderr.take().is_some() {
            #[cfg(test)]
            self.events.push(Event::ClosePipes);
        }
    }

    pub(super) fn pid(&self) -> libc::pid_t {
        let raw = self.child.as_ref().map(Child::id).unwrap_or(0);
        // 负组号要能表示：u32 → pid_t 必须检查，不能直接 cast 成负数。
        libc::pid_t::try_from(raw).unwrap_or(libc::pid_t::MAX)
    }

    /// 测试用：清理过程的顺序记录。
    #[cfg(test)]
    pub(super) fn events(&self) -> &[Event] {
        &self.events
    }
}

impl Drop for ChildSession {
    fn drop(&mut self) {
        // 兜底：正常路径已经 finish 过，这里是唯一剩下的出口（例如 panic 展开）。
        if self.finished {
            return;
        }
        self.finished = true;
        // 清理失败只 warn，绝不 panic——不覆盖正在展开的 panic，也不会二次展开。
        if let Err(error) = self.cleanup() {
            tracing::warn!(
                error = %super::cleanup_message(error),
                "plugin manifest description cleanup failed while unwinding"
            );
        }
    }
}

/// 把 FD 设成非阻塞，**保留**原有 flags。
fn set_non_blocking<F: std::os::unix::io::AsRawFd>(
    file: &F,
    stage: &'static str,
) -> Result<(), DescribeError> {
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is an open file descriptor owned by this session's child pipe,
    // and F_GETFL/F_SETFL only read and write that descriptor's status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(DescribeError::Io {
            stage,
            source: std::io::Error::last_os_error(),
        });
    }
    // SAFETY: same descriptor; `flags` came from a successful F_GETFL on it.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(DescribeError::Io {
            stage,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// 观察直接子进程的退出，但不回收它。
fn observe_exit(pid: libc::pid_t) -> std::io::Result<Option<ObservedExit>> {
    // SAFETY: `siginfo_t` is plain old data; zeroing it is the documented way to
    // hand `waitid` a writable buffer, and it is only read back after a
    // successful call.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` is the positive PID of this session's own child, `info` is a
    // writable `siginfo_t`, and WNOWAIT keeps the child waitable for the final reap.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `waitid` succeeded; `si_pid` is then a valid field. A zero value
    // means "no state change yet", which is checked before reading si_status.
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    // SAFETY: si_pid is non-zero, so si_status/si_code carry this event.
    let status = unsafe { info.si_status() };
    match info.si_code {
        libc::CLD_EXITED => Ok(Some(ObservedExit {
            code: Some(status),
            signal: None,
        })),
        libc::CLD_KILLED | libc::CLD_DUMPED => Ok(Some(ObservedExit {
            code: None,
            signal: Some(status),
        })),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "waitid returned a non-exit event",
        )),
    }
}

/// 向「以直接子进程为 leader」的进程组发信号。
fn signal_group(pid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    if pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the plugin PID cannot be used as a process group",
        ));
    }
    // SAFETY: `-pid` names the process group led by this session's own child,
    // created with `process_group(0)`; kill only sends a signal.
    let rc = unsafe { libc::kill(-pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 向直接子进程发信号（组不存在时的定向补刀）。
fn signal_child(pid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    if pid <= 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the plugin PID is unknown",
        ));
    }
    // SAFETY: `pid` is the positive PID of this session's own, not yet reaped child.
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// 测试用：读出一次 session 的清理次序。
#[cfg(test)]
pub(super) fn debug_events(session: &ChildSession) -> Vec<Event> {
    session.events().to_vec()
}
