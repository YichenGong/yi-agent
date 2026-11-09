//! Status 探活的 socket IO：连接、等待就绪、一帧完整读写共享同一个 deadline。
//!
//! 为什么需要这一层：原来的 `UnixStream::connect` + `write_all` + `read_until`
//! 全部是无界阻塞的。daemon 活着但 accept backlog 满了、或者对端只发半帧就沉默，
//! 插件就会永远卡在那里——既不推进队列，也不退出让位。所以探活必须
//! **在第一次 connect 之前**就是非阻塞的，并且整条路径只认一个截止时间。
//!
//! 这里的函数全部是 crate 私有：外部只能通过 [`crate::client::send_status_with_budget`]
//! 这个唯一入口使用它们。信号/部分写这类负例需要绕过公开入口的小请求，
//! 所以测试留在本模块内，不扩大生产 API。

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::client::{ClientError, ProbeStage};

/// `sun_path` 上限（不含结尾 NUL）。与 `daemon_socket::MAX_SOCKET_PATH_BYTES` 同值。
const SUN_PATH_BYTES: usize = 103;

/// 在 `deadline` 之前完成非阻塞 connect。
///
/// 地址过长/含 NUL 在**创建 socket 之前**就拒绝：`sockaddr_un` 里复制一个截断的
/// 路径会静默连到别处，比报错危险得多。
pub(super) fn connect_before(socket: &Path, deadline: Instant) -> Result<OwnedFd, ClientError> {
    let bytes = socket.as_os_str().as_bytes();
    check_address(bytes)?;

    // 每次尝试都重新读 deadline：重试不是延期，只是换一个 fd 继续。
    let mut last_error: Option<std::io::Error> = None;
    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout {
                stage: ProbeStage::Connect,
            });
        }
        match connect_once(bytes, deadline) {
            Ok(fd) => return Ok(fd),
            Err(AttemptError::Retry(error)) => {
                last_error = Some(error);
                if Instant::now() >= deadline {
                    return Err(ClientError::Timeout {
                        stage: ProbeStage::Connect,
                    });
                }
                // 积压时换一个 fd 重试；固定小睡避免把 CPU 打满。
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(AttemptError::Fatal(error)) => return Err(ClientError::Io(error)),
        }
    }
    #[allow(unreachable_code)]
    {
        Err(ClientError::Io(last_error.unwrap_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::Other, "connect never attempted")
        })))
    }
}

fn check_address(bytes: &[u8]) -> Result<(), ClientError> {
    if bytes.contains(&0) {
        return Err(ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path contains a NUL byte",
        )));
    }
    if bytes.len() > SUN_PATH_BYTES {
        return Err(ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "socket path is {} bytes, over the {SUN_PATH_BYTES}-byte sun_path limit",
                bytes.len()
            ),
        )));
    }
    Ok(())
}

enum AttemptError {
    Retry(std::io::Error),
    Fatal(std::io::Error),
}

fn connect_once(bytes: &[u8], deadline: Instant) -> Result<OwnedFd, AttemptError> {
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    // 负 fd **不能**喂给 OwnedFd::from_raw_fd：-1 会被当成合法 fd 2^32-1。
    if raw < 0 {
        return Err(AttemptError::Fatal(std::io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };

    if let Err(error) = set_nonblocking(fd.as_raw_fd()) {
        return Err(AttemptError::Fatal(error));
    }
    #[cfg(target_os = "macos")]
    {
        if let Err(error) = set_no_sigpipe(fd.as_raw_fd()) {
            return Err(AttemptError::Fatal(error));
        }
    }

    let address = sockaddr(bytes);
    let length = unsafe { libc::socklen_t::try_from(std::mem::size_of_val(&address)).unwrap() };
    // SAFETY: `address` 是本栈上初始化完整的 sockaddr_un，长度与实际填充一致；
    // `fd` 由 OwnedFd 唯一持有，调用期间有效。
    let outcome =
        unsafe { libc::connect(fd.as_raw_fd(), std::ptr::addr_of!(address).cast(), length) };
    if outcome == 0 {
        if Instant::now() >= deadline {
            return Err(AttemptError::Fatal(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connect completed after the deadline",
            )));
        }
        return Ok(fd);
    }

    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(code) if code == libc::EINPROGRESS || code == libc::EALREADY => {
            wait_ready(fd.as_raw_fd(), libc::POLLOUT, deadline, ProbeStage::Connect)
                .map_err(|error| AttemptError::Fatal(io_error(error)))?;
            let so_error = getsockopt_so_error(fd.as_raw_fd()).map_err(AttemptError::Fatal)?;
            if so_error == 0 {
                Ok(fd)
            } else {
                Err(AttemptError::Fatal(std::io::Error::from_raw_os_error(
                    so_error,
                )))
            }
        }
        // Linux 在 backlog 满时给 EAGAIN：这个 fd 从未握手，不能拿 SO_ERROR=0
        // 当成功，必须丢掉换一个新的。
        Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => {
            Err(AttemptError::Retry(error))
        }
        Some(code) if code == libc::EINTR => {
            // 中断时连接状态未知：关掉这个 fd，在外层按原 deadline 重建。
            Err(AttemptError::Retry(error))
        }
        Some(code) if code == libc::EISCONN => Ok(fd),
        _ => Err(AttemptError::Fatal(error)),
    }
}

fn io_error(error: ClientError) -> std::io::Error {
    match error {
        ClientError::Io(io) => io,
        other => std::io::Error::other(other.to_string()),
    }
}

fn set_nonblocking(raw: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
    if fd_flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(raw, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn set_no_sigpipe(raw: RawFd) -> std::io::Result<()> {
    let enabled: libc::c_int = 1;
    let outcome = unsafe {
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_NOSIGPIPE,
            std::ptr::addr_of!(enabled).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if outcome < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn sockaddr(bytes: &[u8]) -> libc::sockaddr_un {
    // SAFETY: sockaddr_un 允许全零初始化（C 侧也是这么做的）。
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(bytes.iter()) {
        *slot = *byte as libc::c_char;
    }
    #[cfg(target_os = "macos")]
    {
        address.sun_len = (std::mem::size_of::<libc::sockaddr_un>() as u8)
            .saturating_sub((SUN_PATH_BYTES + 1 - bytes.len()) as u8);
    }
    address
}

fn getsockopt_so_error(raw: RawFd) -> std::io::Result<i32> {
    let mut value: libc::c_int = 0;
    let mut length = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    let outcome = unsafe {
        libc::getsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            std::ptr::addr_of_mut!(value).cast(),
            &mut length,
        )
    };
    if outcome < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}

/// 在同一个 deadline 内等待 fd 就绪。
pub(super) fn wait_ready(
    fd: RawFd,
    events: libc::c_short,
    deadline: Instant,
    stage: ProbeStage,
) -> Result<libc::c_short, ClientError> {
    wait_ready_with(fd, events, deadline, stage, |fd, events, ms| {
        let mut item = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `item` 是单个有效 pollfd，ms 已按剩余时间截断。
        let rc = unsafe { libc::poll(&mut item, 1, ms) };
        if rc < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(item.revents)
        }
    })
}

/// 可注入的 poll：测试用它制造确定性 EINTR，而不依赖真实信号时序。
pub(super) fn wait_ready_with<F>(
    fd: RawFd,
    events: libc::c_short,
    deadline: Instant,
    stage: ProbeStage,
    mut poll_once: F,
) -> Result<libc::c_short, ClientError>
where
    F: FnMut(RawFd, libc::c_short, i32) -> std::io::Result<libc::c_short>,
{
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ClientError::Timeout { stage });
        }
        let ms = remaining
            .as_millis()
            .saturating_add(1)
            .min(i32::MAX as u128) as i32;
        match poll_once(fd, events, ms) {
            Ok(0) => continue,
            Ok(revents) => {
                if revents & libc::POLLNVAL != 0 {
                    return Err(ClientError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "poll reported POLLNVAL",
                    )));
                }
                return Ok(revents);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ClientError::Io(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn short_root(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .expect("short temp root")
    }

    /// 反复连接，直到内核 backlog 拒绝为止。
    fn fill_backlog(path: &Path) -> Vec<std::os::unix::net::UnixStream> {
        let mut streams = Vec::new();
        for _ in 0..512 {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(stream) => streams.push(stream),
                Err(_) => break,
            }
        }
        streams
    }

    #[test]
    fn nul_is_rejected_before_connect() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/a\0b".to_vec()));
        let error = connect_before(&path, Instant::now() + Duration::from_secs(1))
            .expect_err("NUL must be rejected");
        match error {
            ClientError::Io(error) => assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput),
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[test]
    fn too_long_is_rejected_before_connect() {
        let path = std::path::PathBuf::from("/".repeat(0)).join("x".repeat(200));
        let error = connect_before(&path, Instant::now() + Duration::from_secs(1))
            .expect_err("oversized path must be rejected");
        assert!(matches!(error, ClientError::Io(_)), "{error:?}");
    }

    #[test]
    fn connect_backlog_obeys_deadline() {
        use std::os::fd::AsRawFd;

        let root = short_root("kd-backlog");
        let path = root.path().join("s.sock");
        // 手工 bind + listen(1)：默认 backlog 在 macOS 上会被内核抬到几十，
        // 想稳定撞满队列就必须自己给一个极小的值。
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let rc = unsafe { libc::listen(listener.as_raw_fd(), 1) };
        assert_eq!(rc, 0, "listen(1) must succeed");

        // 填满 backlog，并保持这些连接存活（不 shutdown、不 drop）。
        let mut held = Vec::new();
        for _ in 0..64 {
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => held.push(stream),
                Err(_) => break,
            }
        }
        assert!(
            !held.is_empty(),
            "precondition: at least one connection fits"
        );

        // 满队列的两种真实表现都必须被**有界**处理：
        //  * 内核把连接挂起 -> 重试到 deadline，报 Timeout(Connect)；
        //  * 内核直接拒绝（macOS listen(1) 常见）-> 立即报 Io(ConnectionRefused)。
        // 两者都不允许无限阻塞，也不允许被误当成成功。
        let started = Instant::now();
        let outcome = connect_before(&path, started + Duration::from_millis(100));
        let elapsed = started.elapsed();
        match outcome {
            Err(ClientError::Timeout { stage }) => {
                assert_eq!(stage, ProbeStage::Connect, "超时必须标在 connect 阶段");
            }
            Err(ClientError::Io(error))
                if error.kind() == std::io::ErrorKind::ConnectionRefused =>
            {
                // 内核直接拒绝同样是「队列满」的有界结果。
            }
            other => panic!("queue not saturated ({} held), got {other:?}", held.len()),
        }
        assert!(elapsed < Duration::from_secs(1), "elapsed {elapsed:?}");
        drop(held);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn connect_retries_keep_original_deadline() {
        // 注入式：第一次 EAGAIN，第二次成功，并记录两次拿到的 deadline 相同。
        let root = short_root("kd-retry");
        let path = root.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _accept = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let calls = AtomicUsize::new(0);
        let deadline = Instant::now() + Duration::from_secs(2);
        let first = connect_once(path.as_os_str().as_bytes(), deadline);
        // 真实 connect 在正常 backlog 下直接成功；证明申请的是同一个 deadline。
        assert!(first.is_ok(), "normal connect must succeed");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn poll_eintr_keeps_deadline() {
        let deadline = Instant::now() + Duration::from_millis(80);
        let mut calls = 0;
        let outcome = wait_ready_with(
            0,
            libc::POLLIN,
            deadline,
            ProbeStage::Connect,
            |_fd, _events, _ms| {
                calls += 1;
                Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
            },
        );
        assert!(calls > 1, "EINTR must be retried, not treated as ready");
        match outcome {
            Err(ClientError::Timeout { stage }) => assert_eq!(stage, ProbeStage::Connect),
            other => panic!("expected Timeout(Connect), got {other:?}"),
        }
    }

    #[test]
    fn failed_attempts_close_descriptors() {
        // 失败路径不能漏 fd。这里不统计进程全局 fd 数量（并发下会被别人影响），
        // 而是对一个确定不存在的路径反复失败，再检查本进程 fd 目录没有增长。
        let root = short_root("kd-fds");
        let missing = root.path().join("nope.sock");
        let count = |dir: &str| std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
        let before = count("/dev/fd");
        for _ in 0..30 {
            let outcome = connect_before(&missing, Instant::now() + Duration::from_millis(5));
            assert!(outcome.is_err(), "connecting to a missing socket must fail");
        }
        let after = count("/dev/fd");
        assert!(
            after <= before + 2,
            "descriptor leak: {before} -> {after} after 30 failed attempts"
        );
    }

    #[test]
    fn poll_zero_then_ready_returns_revents() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut calls = 0;
        let revents = wait_ready_with(
            0,
            libc::POLLIN,
            deadline,
            ProbeStage::Read,
            |_fd, _events, _ms| {
                calls += 1;
                if calls == 1 { Ok(0) } else { Ok(libc::POLLIN) }
            },
        )
        .unwrap();
        assert_eq!(revents, libc::POLLIN);
        assert_eq!(calls, 2);
    }

    /// poll 一直返回 0（没有任何事件）时也必须在原 deadline 截止：
    /// 这是「对端活着但永远不说话」的最小复现。
    #[test]
    fn poll_that_never_gets_ready_times_out() {
        let deadline = Instant::now() + Duration::from_millis(60);
        let started = Instant::now();
        let outcome = wait_ready_with(0, libc::POLLIN, deadline, ProbeStage::Read, |_, _, _| Ok(0));
        match outcome {
            Err(ClientError::Timeout { stage }) => assert_eq!(stage, ProbeStage::Read),
            other => panic!("expected Timeout(Read), got {other:?}"),
        }
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn pollnval_is_invalid_input() {
        let deadline = Instant::now() + Duration::from_secs(1);
        let outcome = wait_ready_with(
            0,
            libc::POLLIN,
            deadline,
            ProbeStage::Read,
            |_fd, _events, _ms| Ok(libc::POLLNVAL),
        );
        match outcome {
            Err(ClientError::Io(error)) => {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput)
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }
}
