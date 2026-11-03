//! 有界地跑 `<command> manifest`：双流排空、单一截止时间、一次性收口。
//!
//! 只在 [`super::refresh`] 内部使用，不是通用进程框架。
//!
//! # 为什么不用 `wait_with_output`
//!
//! 旧实现先 `try_wait()` 轮询、**期间不读任何管道**，等直接子进程退出后才
//! `wait_with_output()`。这产生两个同源故障：
//!
//! - 输出超过内核管道容量时，插件卡在 write、退不出来，宿主等退出而读到超时，
//!   本有效的清单被判成「坏插件」；
//! - 后代继承了 stdout/stderr 写端时，直接子进程可以正常退出，但读取要等所有
//!   写端关闭，于是这次读取没有上限。
//!
//! 这里的状态机每轮同时排空两根流，用 `waitid(WNOWAIT)` **观察而不回收**退出，
//! 所有用户态状态都受同一个 deadline 约束，结束后由 [`process::ChildSession`]
//! 一次性清理进程组。

mod process;

use std::time::{Duration, Instant};

use self::process::ChildSession;

/// 错误诊断首行展示的字符上限。截断标记计入这个值。
const DIAGNOSTIC_CHARS: usize = 512;
/// 追加在截断诊断末尾的标记。
const TRUNCATION_MARKER: &str = " [truncated]";
/// [`drain_once`] 的固定读缓冲大小；`read_chunk` 不得超过它。
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// 一次自述操作的全部预算。私有：调用方只拿 [`DescribePolicy::production`]。
#[derive(Clone, Copy)]
pub(super) struct DescribePolicy {
    /// 整个自述操作的单调时间预算。
    pub timeout: Duration,
    /// 无进展时等待、以及取消检查的粒度。
    pub poll_interval: Duration,
    /// stdout 硬上限：恰好等于允许，下一字节失败。
    pub stdout_limit: usize,
    /// stderr 只保存这个长度的前缀，之后继续排空并丢弃。
    pub stderr_capture_limit: usize,
    /// 单次 `read` 的固定缓冲大小。
    pub read_chunk: usize,
    /// 每根流每轮最多读多少字节，防止持续输出饿死其他检查。
    pub drain_budget_per_stream: usize,
}

impl DescribePolicy {
    /// 生产预算。
    ///
    /// 15 秒是历史校准的结论：5 秒在冷进程启动 + CI 高并发下会被击穿（实测
    /// 并发跑 87 个测试时，一个只做 `cat` 的 shell 脚本也能超过 5 秒，校准被
    /// 误判成 `Skipped`）。这个预算只在**插件自己坏掉**时才会走满，走满的结果
    /// 是回退到磁盘上的旧清单，不影响对账循环，也不影响启动延迟之外的任何路径；
    /// 因此用「坏插件多等 10 秒」换「好插件不被误判」。
    ///
    /// 之前这个常量叫 `DESCRIBE_TIMEOUT`，现在它是这套预算的一部分：超时只是
    /// 若干约束之一，必须和轮询粒度、输出上限一起被传下去。
    pub(super) fn production() -> Self {
        Self::checked(
            Duration::from_secs(15),
            Duration::from_millis(20),
            1024 * 1024,
            64 * 1024,
            READ_BUFFER_BYTES,
            64 * 1024,
        )
    }

    /// 唯一的构造入口：把「排空能推进」的前提写成断言。
    ///
    /// `read_chunk == 0` 会让 `drain_once` 空转，`drain_budget_per_stream == 0`
    /// 会让每轮都读不到东西、只能等到 deadline；这两者都会把 bug 藏成超时。
    /// `read_chunk` 超过 [`READ_BUFFER_BYTES`] 会被固定缓冲悄悄截断，所以直接拒绝。
    fn checked(
        timeout: Duration,
        poll_interval: Duration,
        stdout_limit: usize,
        stderr_capture_limit: usize,
        read_chunk: usize,
        drain_budget_per_stream: usize,
    ) -> Self {
        assert!(poll_interval > Duration::ZERO, "poll interval must advance");
        assert!(read_chunk > 0, "read chunk must be non-zero");
        assert!(
            read_chunk <= READ_BUFFER_BYTES,
            "read chunk must fit the fixed buffer"
        );
        assert!(
            drain_budget_per_stream > 0,
            "each round must be allowed to read something"
        );
        assert!(stdout_limit > 0 && stderr_capture_limit > 0);
        Self {
            timeout,
            poll_interval,
            stdout_limit,
            stderr_capture_limit,
            read_chunk,
            drain_budget_per_stream,
        }
    }

    /// 测试用的短预算：仍走同一套断言。
    #[cfg(test)]
    fn shortened(timeout: Duration) -> Self {
        Self::with_stdout_limit(timeout, 1024 * 1024)
    }

    /// Same production parameters, different deadline.
    ///
    /// Not `#[cfg(test)]`: integration tests in other crates compile this crate as
    /// an ordinary library, where test-only items do not exist (the same rule
    /// `serve_without_manifest_refresh` follows). Only the deadline differs, so a
    /// caller cannot use it to change what is checked -- just how long it may take.
    ///
    /// For the cases that walk the real batch path. Their question is "does
    /// draining work", not "is this machine fast": the production 15 s budget is
    /// calibrated to stop a plugin that is genuinely hung, and a full parallel
    /// test run can spend more than that just starting `python3`. Only the
    /// deadline moves -- the stdout limit and stderr capture stay at their
    /// production values, so the backpressure under test is the real one.
    pub(super) fn wide_budget(timeout: Duration) -> Self {
        Self::checked(
            timeout,
            Duration::from_millis(5),
            1024 * 1024,
            64 * 1024,
            READ_BUFFER_BYTES,
            64 * 1024,
        )
    }

    /// 测试用：短预算 + 指定的 stdout 硬上限。
    #[cfg(test)]
    fn with_stdout_limit(timeout: Duration, stdout_limit: usize) -> Self {
        Self::checked(
            timeout,
            Duration::from_millis(5),
            stdout_limit,
            64 * 1024,
            READ_BUFFER_BYTES,
            64 * 1024,
        )
    }
}

/// 观察到（但**尚未回收**）的直接子进程退出状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ObservedExit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

impl ObservedExit {
    /// 只有「正常退出且 code 为 0」算成功。被信号终止不是成功，即使没有 code。
    pub(super) fn success(self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }

    fn describe(self) -> String {
        match (self.code, self.signal) {
            (Some(code), _) => format!("exited with status {code}"),
            (None, Some(signal)) => format!("was killed by signal {signal}"),
            (None, None) => "exited without a status".to_string(),
        }
    }
}

/// 自述失败的结构化分类。
///
/// 分类是**枚举**而不是字符串：日志级别与「取消还是故障」的判断都读它，插件往
/// stderr 写一句 `cancelled` 不能改变分类。
#[derive(Debug)]
pub(super) enum DescribeError {
    Cancelled,
    Timeout(Duration),
    StdoutLimit(usize),
    InvalidUtf8,
    NonZero {
        exit: ObservedExit,
        stderr: String,
    },
    Io {
        stage: &'static str,
        source: std::io::Error,
    },
    OwnershipLost,
    CleanupFailed(String),
    WithCleanup {
        primary: Box<DescribeError>,
        cleanup: String,
    },
}

impl DescribeError {
    /// 转成给用户看的一行原因。**不包含 stdout**：那里面可能有敏感参数。
    pub(super) fn render(&self, command: &str) -> String {
        match self {
            Self::Cancelled => format!("`{command} manifest` was cancelled"),
            Self::Timeout(timeout) => {
                format!("`{command} manifest` did not answer within {timeout:?}")
            }
            Self::StdoutLimit(limit) => {
                format!("`{command} manifest` printed more than the {limit} byte stdout limit")
            }
            Self::InvalidUtf8 => format!("`{command} manifest` did not print UTF-8"),
            Self::NonZero { exit, stderr } => {
                if stderr.is_empty() {
                    format!("`{command} manifest` failed: {}", exit.describe())
                } else {
                    format!("`{command} manifest` failed: {stderr}")
                }
            }
            Self::Io { stage, source } => {
                format!("`{command} manifest` failed while trying to {stage}: {source}")
            }
            Self::OwnershipLost => format!(
                "`{command} manifest` ownership was lost: it was reaped outside this process"
            ),
            Self::CleanupFailed(cleanup) => {
                format!("`{command} manifest` finished but its cleanup failed: {cleanup}")
            }
            Self::WithCleanup { primary, cleanup } => {
                format!("{} (cleanup failed: {cleanup})", primary.render(command))
            }
        }
    }
}

impl DescribeError {
    /// 这次失败是不是**停止请求**造成的。
    ///
    /// 递归看主因：`WithCleanup` 保留的是主因的分类。日志级别必须读这个，
    /// 绝不能去匹配 reason 子串——插件往 stderr 写一句 `cancelled` 就能伪造级别。
    pub(super) fn is_cancelled(&self) -> bool {
        match self {
            Self::Cancelled => true,
            Self::WithCleanup { primary, .. } => primary.is_cancelled(),
            _ => false,
        }
    }

    /// 清理本身的诊断（如果有）。与主因**分开**报告：主因是正常取消时，清理失败
    /// 也不能被吞掉。
    pub(super) fn cleanup_warning(&self) -> Option<String> {
        match self {
            Self::WithCleanup { cleanup, .. } => Some(cleanup.clone()),
            Self::CleanupFailed(cleanup) => Some(cleanup.clone()),
            _ => None,
        }
    }
}

impl std::fmt::Display for DescribeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `render` 需要一个命令名做主语；这里没有，就不硬编一个。
        write!(f, "{self:?}")
    }
}

/// 有界捕获。stdout 用 `hard_limit`，stderr 用软上限（截断但继续排空）。
struct Capture {
    bytes: Vec<u8>,
    limit: usize,
    hard_limit: bool,
    truncated: bool,
}

impl Capture {
    fn new(limit: usize, hard_limit: bool) -> Self {
        Self {
            bytes: Vec::with_capacity(limit),
            limit,
            hard_limit,
            truncated: false,
        }
    }

    /// 追加一段字节。
    ///
    /// 硬上限超出一个字节就是错误（不能截断后当成清单去解析）；软上限只截断保存，
    /// **不停止读取**——否则 stderr 多的插件会把自己的 stdout 堵死。
    fn push(&mut self, chunk: &[u8]) -> Result<(), DescribeError> {
        let remaining = self.limit.saturating_sub(self.bytes.len());
        if self.hard_limit && chunk.len() > remaining {
            return Err(DescribeError::StdoutLimit(self.limit));
        }
        let keep = remaining.min(chunk.len());
        self.bytes.extend_from_slice(&chunk[..keep]);
        self.truncated |= keep < chunk.len();
        Ok(())
    }

    /// 严格 UTF-8：清单不能用 lossy 解读，否则坏字节会被替换成看起来合法的文本。
    fn text(&self) -> Result<String, DescribeError> {
        std::str::from_utf8(&self.bytes)
            .map(str::to_owned)
            .map_err(|_| DescribeError::InvalidUtf8)
    }

    /// 失败诊断：stderr 首行，最多 [`DIAGNOSTIC_CHARS`] 个字符，截断标记计入限值。
    fn diagnostic(&self) -> String {
        let lossy = String::from_utf8_lossy(&self.bytes);
        let first = lossy.lines().next().unwrap_or("").trim();
        let needs_marker = self.truncated || first.chars().count() > DIAGNOSTIC_CHARS;
        if !needs_marker {
            return first.to_owned();
        }
        let budget = DIAGNOSTIC_CHARS - TRUNCATION_MARKER.chars().count();
        let head: String = first.chars().take(budget).collect();
        if head.is_empty() {
            TRUNCATION_MARKER.trim().to_owned()
        } else {
            format!("{head}{TRUNCATION_MARKER}")
        }
    }
}

/// 每轮统一的时间判据。**同一个检查点取消优先**：先看停止请求，再看 deadline。
fn check_limits(
    deadline: Instant,
    now: Instant,
    timeout: Duration,
    cancelled: bool,
) -> Result<(), DescribeError> {
    if cancelled {
        return Err(DescribeError::Cancelled);
    }
    if now >= deadline {
        return Err(DescribeError::Timeout(timeout));
    }
    Ok(())
}

/// 一轮排空的结果。
struct DrainProgress {
    bytes: usize,
    eof: bool,
    interrupted: bool,
}

/// 从一根流里读至多 `drain_budget_per_stream` 字节。
///
/// 只传预计算长度的片段：`read_chunk` 小于缓冲时不能把整个缓冲都递过去。
fn drain_once<R: std::io::Read>(
    reader: &mut R,
    capture: &mut Capture,
    policy: &DescribePolicy,
) -> Result<DrainProgress, DescribeError> {
    let mut buf = [0u8; READ_BUFFER_BYTES];
    let mut bytes = 0;
    while bytes < policy.drain_budget_per_stream {
        let remaining_budget = policy.drain_budget_per_stream - bytes;
        let want = policy.read_chunk.min(buf.len()).min(remaining_budget);
        match reader.read(&mut buf[..want]) {
            Ok(0) => {
                return Ok(DrainProgress {
                    bytes,
                    eof: true,
                    interrupted: false,
                });
            }
            Ok(read) => {
                capture.push(&buf[..read])?;
                bytes += read;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Ok(DrainProgress {
                    bytes,
                    eof: false,
                    interrupted: false,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                return Ok(DrainProgress {
                    bytes,
                    eof: false,
                    interrupted: true,
                });
            }
            Err(source) => {
                return Err(DescribeError::Io {
                    stage: "read the plugin output",
                    source,
                });
            }
        }
    }
    Ok(DrainProgress {
        bytes,
        eof: false,
        interrupted: false,
    })
}

/// 状态机里所有「该看一眼时间/停止请求」的位置。
///
/// 它们不是可选的调试钩子：测试用同一个枚举把时钟冻结到目标状态再推进，生产
/// 时钟永远返回真实时间。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Checkpoint {
    Start,
    Loop,
    AfterIo {
        observed_exit: bool,
        stdout_eof: bool,
        stderr_eof: bool,
        bytes_read: usize,
    },
    BeforeSleep,
    Final,
}

/// 把操作结果与清理结果合并成一次返回。
///
/// 主因优先：清理失败只作为附注，不能让「为什么失败」被清理噪声顶掉；但原本成功
/// 而清理失败必须改判失败——留下进程比不刷新更糟。
fn combine<T>(
    operation: Result<T, DescribeError>,
    cleanup: Result<(), DescribeError>,
) -> Result<T, DescribeError> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(DescribeError::WithCleanup {
            primary: Box::new(primary),
            cleanup: cleanup_message(cleanup),
        }),
    }
}

/// 把清理错误降级成一段说明文字（`WithCleanup` 只存字符串）。
fn cleanup_message(error: DescribeError) -> String {
    match error {
        DescribeError::CleanupFailed(message) => message,
        other => format!("{other:?}"),
    }
}

/// 可注入时钟的执行器：**唯一的**真源，所有预算判据都调用同一个 `now`。
fn describe_with_clock(
    command: &str,
    policy: DescribePolicy,
    should_stop: &mut dyn FnMut() -> bool,
    now: &mut dyn FnMut(Checkpoint) -> Instant,
) -> Result<String, DescribeError> {
    let deadline = now(Checkpoint::Start) + policy.timeout;
    // spawn 之前就看一眼：已经要求停止时不该再起子进程。
    check_limits(
        deadline,
        now(Checkpoint::Loop),
        policy.timeout,
        should_stop(),
    )?;
    let mut session = ChildSession::spawn(command)?;
    let mut out = Capture::new(policy.stdout_limit, true);
    let mut err = Capture::new(policy.stderr_capture_limit, false);
    let mut out_eof = false;
    let mut err_eof = false;

    // 操作放进闭包，好让**所有** `?` 出口都先走一次 `finish`。
    let operation = (|| -> Result<String, DescribeError> {
        loop {
            check_limits(
                deadline,
                now(Checkpoint::Loop),
                policy.timeout,
                should_stop(),
            )?;
            let mut progress = 0;
            if !out_eof {
                let stdout = session.stdout.as_mut().expect("stdout is a pipe");
                let drained = drain_once(stdout, &mut out, &policy)?;
                out_eof = drained.eof;
                progress += drained.bytes;
                if drained.interrupted {
                    continue;
                }
            }
            if !err_eof {
                let stderr = session.stderr.as_mut().expect("stderr is a pipe");
                let drained = drain_once(stderr, &mut err, &policy)?;
                err_eof = drained.eof;
                progress += drained.bytes;
                if drained.interrupted {
                    continue;
                }
            }
            let exit = match session.observe() {
                // EINTR 只说明这次等待被打断，回到循环重查时间与停止请求。
                Err(DescribeError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::Interrupted =>
                {
                    continue;
                }
                result => result?,
            };
            // 退出观察之后、判成功之前，必须再查一次：已经到期不允许因为「看到
            // 退出状态」而改判成功。
            check_limits(
                deadline,
                now(Checkpoint::AfterIo {
                    observed_exit: exit.is_some(),
                    stdout_eof: out_eof,
                    stderr_eof: err_eof,
                    bytes_read: progress,
                }),
                policy.timeout,
                should_stop(),
            )?;
            if let Some(exit) = exit {
                // 直接子进程退出不是完成：两根流都 EOF 才算收完输出。
                if out_eof && err_eof {
                    if !exit.success() {
                        return Err(DescribeError::NonZero {
                            exit,
                            stderr: err.diagnostic(),
                        });
                    }
                    return out.text();
                }
            }
            if progress == 0 {
                let remain = deadline.saturating_duration_since(now(Checkpoint::BeforeSleep));
                std::thread::sleep(policy.poll_interval.min(remain));
            }
        }
    })();

    let cleanup = session.finish();
    let result = combine(operation, cleanup);
    if result.is_ok() {
        // 成功的线性化点是这里的最终检查，不是读到 EOF 的那一刻。
        check_limits(
            deadline,
            now(Checkpoint::Final),
            policy.timeout,
            should_stop(),
        )?;
    }
    result
}

/// 生产入口：真实时钟、生产预算。
pub(super) fn describe(
    command: &str,
    policy: DescribePolicy,
    should_stop: &mut dyn FnMut() -> bool,
) -> Result<String, DescribeError> {
    describe_with_clock(command, policy, should_stop, &mut |_| Instant::now())
}

#[cfg(test)]
mod tests;
