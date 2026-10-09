//! `crate::backoff` 的接线约束:重连延迟必须从 1s 起**翻倍**、封顶 60s。
//!
//! 中继客户端在手机切网、电脑睡眠后要能自愈(spec §11.4:「电脑侧断线重连」)。
//! v1 = 指数退避 + 周期 ping;退避曲线是本模块唯一的纯逻辑,先把它钉死。

use std::time::Duration;

use yi_agent_relay::backoff::{MAX_BACKOFF, ReconnectSchedule, backoff_start, next_delay};

#[test]
fn backoff_doubles_from_one_second_and_caps_at_sixty() {
    assert_eq!(
        backoff_start(),
        Duration::from_secs(1),
        "first retry waits 1s"
    );

    let mut d = backoff_start();
    let mut seen = vec![d];
    for _ in 0..10 {
        d = next_delay(d);
        seen.push(d);
    }

    assert_eq!(seen[0], Duration::from_secs(1));
    assert_eq!(seen[1], Duration::from_secs(2));
    assert_eq!(seen[2], Duration::from_secs(4));
    assert_eq!(seen[3], Duration::from_secs(8));
    assert_eq!(seen[4], Duration::from_secs(16));
    assert_eq!(seen[5], Duration::from_secs(32));
    // 64s 会被封顶。
    assert_eq!(seen[6], MAX_BACKOFF);
    assert_eq!(MAX_BACKOFF, Duration::from_secs(60));
    // 封顶后保持恒定,不再翻倍。
    assert_eq!(seen[10], MAX_BACKOFF);
}

/// 一次**成功**的桥接必须把退避打回最短。
///
/// 这是实际踩到的故障:旧循环 `delay = next_delay(delay)` 无条件推进,于是一串
/// 瞬时断开(电脑睡眠、中继重启、手机切网)把延迟一路顶到 60s 上限——此后中继
/// 恢复了,电脑却仍要等满 60s 才回来,表现为「中继已重启但手机长时间连不上」。
/// 退避的意义是避免对**持续失败**的端点狂打,不是惩罚刚掉线一次的正常端点。
#[test]
fn a_successful_bridge_resets_the_backoff_to_the_shortest_wait() {
    let mut schedule = ReconnectSchedule::new();

    // 连续失败:1s → 2s → 4s → 8s。
    assert_eq!(schedule.next_wait(), Duration::from_secs(1));
    assert_eq!(schedule.next_wait(), Duration::from_secs(2));
    assert_eq!(schedule.next_wait(), Duration::from_secs(4));
    assert_eq!(schedule.next_wait(), Duration::from_secs(8));

    // 这次连上了。
    schedule.on_connected();

    // 下一次断开必须从 1s 重来,而不是接着 16s。
    assert_eq!(schedule.next_wait(), Duration::from_secs(1));
    assert_eq!(schedule.next_wait(), Duration::from_secs(2));
}

/// 反复「连上又掉」不能把等待越攒越长——上限由失败**连续**次数决定,而非历史总数。
#[test]
fn repeated_drops_never_accumulate_beyond_one_second_after_each_success() {
    let mut schedule = ReconnectSchedule::new();
    for _ in 0..20 {
        schedule.on_connected();
        assert_eq!(schedule.next_wait(), Duration::from_secs(1));
    }
}
