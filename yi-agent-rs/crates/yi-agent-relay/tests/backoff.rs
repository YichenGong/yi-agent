//! `crate::backoff` 的接线约束:重连延迟必须从 1s 起**翻倍**、封顶 60s。
//!
//! 中继客户端在手机切网、电脑睡眠后要能自愈(spec §11.4:「电脑侧断线重连」)。
//! v1 = 指数退避 + 周期 ping;退避曲线是本模块唯一的纯逻辑,先把它钉死。

use std::time::Duration;

use yi_agent_relay::backoff::{MAX_BACKOFF, backoff_start, next_delay};

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
