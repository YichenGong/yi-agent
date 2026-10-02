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
