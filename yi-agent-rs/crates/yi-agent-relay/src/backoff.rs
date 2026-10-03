//! 重连退避曲线。
//!
//! 中继客户端断开后按指数退避重连(spec §11.4:电脑侧断线重连 + App 侧
//! `thread/resume` 增量回放)。纯逻辑、无 IO,故单独成模块便于钉死。

use std::time::Duration;

/// 重连退避上限。
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// 首次重连的等待时长。
pub fn backoff_start() -> Duration {
    Duration::from_secs(1)
}

/// 给定本次退避时长,给出下一次:翻倍,但封顶 [`MAX_BACKOFF`]。
pub fn next_delay(current: Duration) -> Duration {
    (current * 2).min(MAX_BACKOFF)
}

/// 重连退避的**状态**。
///
/// 把「连续失败了几次」和「上次是否连上」记在一处,让「**成功后重置**」成为一条
/// 走不过去的必经步骤:`run_client` 只要在两端都连上时调用 [`on_connected`],就
/// 不可能再退化成「掉线一次、之后永远等 60s」。
///
/// 抽成单独类型而不是在循环里散着改 `delay` 变量,正是因为这个缺陷的实现方式就是
/// 「`delay = next_delay(delay)` 无条件推进」——状态一旦散落,重置就容易被漏掉。
#[derive(Debug, Clone)]
pub struct ReconnectSchedule {
    next: Duration,
}

impl ReconnectSchedule {
    /// 一份全新的退避:首次等待 [`backoff_start`]。
    pub fn new() -> Self {
        Self {
            next: backoff_start(),
        }
    }

    /// 取「本次失败后应等待多久」,并把下一次翻倍。
    pub fn next_wait(&mut self) -> Duration {
        let wait = self.next;
        self.next = next_delay(self.next);
        wait
    }

    /// 一次**成功**的连接:退避打回最短,下一次断开从 1s 重来。
    ///
    /// 退避的目的是避免对**持续失败**的端点狂打,而不是惩罚刚掉线一次的正常端点。
    /// 少了这一步,一串瞬时断开(电脑睡眠、中继重启、手机切网)会把延迟顶到上限,
    /// 此后即使故障早已恢复,重连仍要等满 60s。
    pub fn on_connected(&mut self) {
        self.next = backoff_start();
    }
}

impl Default for ReconnectSchedule {
    fn default() -> Self {
        Self::new()
    }
}
