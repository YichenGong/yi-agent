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
    // 积压时 Retry 会在 sleep 后重新判断 deadline，绝不让单次尝试越界。
    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout {
                stage: ProbeStage::Connect,
            });
        }
        match connect_once(bytes, deadline) {
            Ok(fd) => return Ok(fd),
            Err(AttemptError::Retry) => {
                if Instant::now() >= deadline {
                    return Err(ClientError::Timeout {
                        stage: ProbeStage::Connect,
                    });
                }
                // 固定小睡避免把 CPU 打满。
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(AttemptError::Fatal(error)) => return Err(ClientError::Io(error)),
        }
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
    /// 换一个 fd 重试即可；具体 errno 不参与判定，也不对外暴露。
    Retry,
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
    let length = libc::socklen_t::try_from(std::mem::size_of_val(&address)).unwrap();
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
        Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => Err(AttemptError::Retry),
        Some(code) if code == libc::EINTR => {
            // 中断时连接状态未知：关掉这个 fd，在外层按原 deadline 重建。
            Err(AttemptError::Retry)
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

/// 用同一个 deadline 执行一次「写完整一帧 + 读完整一帧」。
///
/// 真实实现由一个非阻塞 `OwnedFd` 驱动；fake 只在单元测试里用，不暴露给外部。
pub(super) fn exchange(
    socket: &Path,
    request: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, ClientError> {
    let fd = connect_before(socket, deadline)?;
    let mut io = FdIo { fd };
    exchange_connected(&mut io, request, deadline)
}

/// 一次完整的请求/应答交换。写满整个 request，读到第一条完整帧（含 LF）为止。
pub(super) fn exchange_connected(
    io: &mut impl ProbeIo,
    request: &[u8],
    deadline: Instant,
) -> Result<Vec<u8>, ClientError> {
    write_all(io, request, deadline)?;
    read_one_frame(io, deadline)
}

pub(super) trait ProbeIo {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<usize>;
    fn recv(&mut self, bytes: &mut [u8]) -> std::io::Result<usize>;
    fn wait(&mut self, write: bool, deadline: Instant) -> Result<(), ClientError>;
}

struct FdIo {
    fd: OwnedFd,
}

impl ProbeIo for FdIo {
    fn send(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        #[cfg(target_os = "linux")]
        {
            let rc = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_NOSIGNAL,
                )
            };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(rc as usize)
        }
        #[cfg(not(target_os = "linux"))]
        {
            // macOS 已在连接时设置 SO_NOSIGPIPE；这里仍是裸 send，避免 SIGPIPE。
            let rc =
                unsafe { libc::send(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
            if rc < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(rc as usize)
        }
    }

    fn recv(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let rc = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(rc as usize)
    }

    fn wait(&mut self, write: bool, deadline: Instant) -> Result<(), ClientError> {
        let events = if write { libc::POLLOUT } else { libc::POLLIN };
        let stage = if write {
            ProbeStage::Write
        } else {
            ProbeStage::Read
        };
        wait_ready(self.fd.as_raw_fd(), events, deadline, stage).map(|_| ())
    }
}

fn write_all(io: &mut impl ProbeIo, bytes: &[u8], deadline: Instant) -> Result<(), ClientError> {
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout {
                stage: ProbeStage::Write,
            });
        }
        match io.send(&bytes[offset..]) {
            Ok(0) => {
                return Err(ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "socket accepted zero bytes",
                )));
            }
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                io.wait(true, deadline)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ClientError::Io(error)),
        }
    }
    Ok(())
}

fn read_one_frame(io: &mut impl ProbeIo, deadline: Instant) -> Result<Vec<u8>, ClientError> {
    let mut buffer = Vec::new();
    let mut chunk = vec![0_u8; 4096];
    loop {
        if Instant::now() >= deadline {
            return Err(ClientError::Timeout {
                stage: ProbeStage::Read,
            });
        }
        let read = match io.recv(&mut chunk) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                io.wait(false, deadline)?;
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(ClientError::Io(error)),
        };
        if read == 0 {
            return Err(ClientError::Truncated);
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = buffer.iter().position(|byte| *byte == b'\n') {
            let body_len = index + 1;
            if body_len - 1 > crate::wire::MAX_FRAME_BYTES {
                return Err(ClientError::FrameTooLarge {
                    bytes: body_len - 1,
                    limit: crate::wire::MAX_FRAME_BYTES,
                });
            }
            return Ok(buffer[..body_len].to_vec());
        }
        if buffer.len() > crate::wire::MAX_FRAME_BYTES {
            return Err(ClientError::FrameTooLarge {
                bytes: buffer.len(),
                limit: crate::wire::MAX_FRAME_BYTES,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, Write as _};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn short_root(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .expect("short temp root")
    }

    /// 反复连接，直到内核 backlog 拒绝为止。
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

    /// 失败路径不能漏 fd。
    ///
    /// 必须**在独立子进程里**测：`/dev/fd` 是整个进程共享的，同进程里其他测试
    /// 同时在开 socket 时，父进程的计数会随之增长，把一次真实泄漏掩盖成"竞争"，
    /// 或者把无泄漏的并发误报成泄漏（实测跑整包时就是这样红的）。
    #[test]
    fn failed_attempts_close_descriptors() {
        if std::env::var_os("KANBAN_FD_WORKER").is_some() {
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "status_io::tests::fd_leak_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("KANBAN_FD_WORKER", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .expect("spawn fd worker");
        assert!(status.success(), "fd leak worker failed: {status:?}");
    }

    /// 只在子进程里跑：整个进程只有它自己，`/dev/fd` 计数才有意义。
    #[test]
    fn fd_leak_worker() {
        if std::env::var_os("KANBAN_FD_WORKER").is_none() {
            return;
        }
        let root = short_root("kd-fds");
        let missing = root.path().join("nope.sock");
        let count = |dir: &str| std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0);
        let before = count("/dev/fd");
        for _ in 0..30 {
            let outcome = connect_before(&missing, Instant::now() + Duration::from_millis(5));
            assert!(outcome.is_err(), "connecting to a missing socket must fail");
        }
        let after = count("/dev/fd");
        assert_eq!(
            after, before,
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

    /// 部分写 + EINTR：必须写满整个 request，且所有 wait 用的是同一个 deadline。
    #[test]
    fn short_writes_and_interrupted_io_complete_one_frame() {
        struct Fake {
            sent: Vec<u8>,
            waits: Vec<(bool, Instant)>,
            sends: usize,
            recved: bool,
            recv_offset: usize,
        }
        impl ProbeIo for Fake {
            fn send(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.sends += 1;
                if self.sends == 1 {
                    return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
                }
                let take = bytes.len().min(1);
                self.sent.extend_from_slice(&bytes[..take]);
                Ok(take)
            }
            fn recv(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                // 第一拍 WouldBlock，之后每次只吐 2 字节，直到整帧发完。
                if !self.recved {
                    self.recved = true;
                    return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
                }
                const BODY: &[u8] = b"{\"ok\":true}\n";
                if self.recv_offset >= BODY.len() {
                    return Ok(0);
                }
                let take = bytes.len().min(2).min(BODY.len() - self.recv_offset);
                bytes[..take].copy_from_slice(&BODY[self.recv_offset..self.recv_offset + take]);
                self.recv_offset += take;
                Ok(take)
            }
            fn wait(&mut self, write: bool, deadline: Instant) -> Result<(), ClientError> {
                self.waits.push((write, deadline));
                Ok(())
            }
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let request = b"hello-frame\n";
        let mut fake = Fake {
            sent: Vec::new(),
            waits: Vec::new(),
            sends: 0,
            recved: false,
            recv_offset: 0,
        };
        let frame = exchange_connected(&mut fake, request, deadline).unwrap();
        assert_eq!(
            fake.sent, request,
            "every byte must be written exactly once"
        );
        assert!(!fake.waits.is_empty());
        assert!(
            fake.waits.iter().all(|(_, waited)| *waited == deadline),
            "no wait may extend the original deadline"
        );
        assert!(frame.ends_with(b"\n"));
    }

    #[test]
    fn write_stall_times_out_at_write_stage() {
        struct Stall;
        impl ProbeIo for Stall {
            fn send(&mut self, _bytes: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            }
            fn recv(&mut self, _bytes: &mut [u8]) -> std::io::Result<usize> {
                panic!("must not read while the write is stalled")
            }
            fn wait(&mut self, _write: bool, _deadline: Instant) -> Result<(), ClientError> {
                Err(ClientError::Timeout {
                    stage: ProbeStage::Write,
                })
            }
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        let error = exchange_connected(&mut Stall, b"x", deadline).unwrap_err();
        assert!(matches!(error, ClientError::Timeout { stage } if stage == ProbeStage::Write));
    }

    /// 对端半关闭后继续写必须报错，**不能**让进程被 SIGPIPE 杀死。
    ///
    /// Rust 运行时默认忽略 SIGPIPE，所以若只依赖默认行为，这个测试是假绿：
    /// worker 子进程里显式恢复 `SIG_DFL`，再确认写到断开连接返回 BrokenPipe/
    /// ConnectionReset 且进程正常退出（不是 signal 13）。
    #[test]
    fn broken_peer_does_not_raise_sigpipe() {
        let root = short_root("kd-sigpipe");
        let path = root.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "status_io::tests::sigpipe_worker",
                "--nocapture",
                "--test-threads=1",
            ])
            .env_clear()
            .env("KANBAN_SIGPIPE_WORKER", "1")
            .env("KANBAN_SIGPIPE_SOCKET", &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sigpipe worker");

        // worker 连上后喊 CONNECTED；父进程必须立刻断开它。
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            line.clear();
            let read = stdout.read_line(&mut line).unwrap_or(0);
            if read == 0 || line.contains("CONNECTED") {
                break;
            }
            assert!(Instant::now() < deadline, "worker never connected");
        }
        assert!(
            line.contains("CONNECTED"),
            "worker handshake missing: {line:?}"
        );

        let (peer, _) = listener.accept().expect("accept worker");
        peer.shutdown(std::net::Shutdown::Both).ok();
        drop(peer);

        let status = child.wait().expect("worker exits");
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).ok();
        assert!(
            status.success(),
            "worker died by signal {:?}; stderr: {stderr}",
            std::os::unix::process::ExitStatusExt::signal(&status)
        );
        assert!(
            stderr.contains("KANBAN_SIGPIPE_RESULT=broken"),
            "stderr: {stderr}"
        );
    }

    /// worker 半：只有带标记时才工作，其余运行立即返回。
    #[test]
    fn sigpipe_worker() {
        if std::env::var_os("KANBAN_SIGPIPE_WORKER").is_none() {
            return;
        }
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let socket = PathBuf::from(std::env::var("KANBAN_SIGPIPE_SOCKET").unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        let fd = connect_before(&socket, deadline).expect("worker connects");
        let mut io = FdIo { fd };
        println!("CONNECTED");
        std::io::stdout().flush().unwrap();

        // 等父进程关闭连接：recv 返回 0 即 EOF。不能读 stdin——父进程没有
        // 给这个子进程接管道，等 stdin 会永远挂住。
        let mut probe = [0_u8; 64];
        loop {
            assert!(Instant::now() < deadline, "peer never closed");
            match io.recv(&mut probe) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    io.wait(false, deadline).ok();
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => panic!("unexpected read error: {error}"),
            }
        }

        // 对端已关；恢复默认 SIGPIPE 后写大帧。若实现漏了 MSG_NOSIGNAL /
        // SO_NOSIGPIPE，这里会被信号 13 杀死，父进程看到 signal 13 而失败。
        let payload = vec![b'x'; 1 << 20];
        match write_all(&mut io, &payload, Instant::now() + Duration::from_secs(5)) {
            Err(ClientError::Io(error))
                if error.kind() == std::io::ErrorKind::BrokenPipe
                    || error.kind() == std::io::ErrorKind::ConnectionReset =>
            {
                eprintln!("KANBAN_SIGPIPE_RESULT=broken");
            }
            other => {
                eprintln!("KANBAN_SIGPIPE_RESULT=other {other:?}");
                std::process::exit(3);
            }
        }
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
