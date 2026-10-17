//! 回归:`run_client` 连 `wss://` 中继时**不得 panic**。
//!
//! 背景:rustls 0.23 只在「恰好一个 provider feature 被启用」时才
//! 能从 crate features 自动判定进程级 provider。桌面 app-server 进程里 `ring`
//! (自有 `reqwest 0.12`)与 `aws_lc_rs`(`rmcp`→`reqwest 0.13`)并存,判定失败后
//! `ClientConfig::builder()` 直接 panic;中继客户端是进程里唯一建 `wss://` TLS 的地方,
//! 该 panic 发生在被 `spawn` 的桥接任务里、被运行时吞掉——于是「手机连不上中继,电脑
//! app 却一切正常」。本测试的依赖图复刻了该歧义组合(见 Cargo.toml 的 dev-dependency),
//! 因此能在 relay 自己的构建里稳定复现。
//!
//! panic 发生在 rustls **构造客户端配置**时(真正 TLS 握手之前),所以下层只需一个
//! 「接受连接后立刻关闭」的普通 TCP 端点,不必真的会 TLS。

use std::time::Duration;

use url::Url;

/// 起一个只接受连接、立刻丢弃的 TCP 端点,充当「wss 中继」的下层 socket。
async fn spawn_dummy_tcp_endpoint() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    addr
}

/// `run_client` 面对 `wss://` 中继必须**持续退避重连**,而不是 panic 退出。
#[tokio::test(flavor = "multi_thread")]
async fn run_client_toward_a_wss_relay_does_not_panic() {
    let addr = spawn_dummy_tcp_endpoint().await;
    let relay_url = Url::parse(&format!("wss://127.0.0.1:{}/connect", addr.port())).unwrap();
    // 本地端点故意不可达:`bridge_once` 先连中继,panic(若有)必在此之前发生。
    let local_url = Url::parse("ws://127.0.0.1:1/ws").unwrap();

    let mut task = tokio::spawn(async move {
        let _ = yi_agent_relay::run_client(relay_url, local_url, "s".into(), "t".into()).await;
    });

    // 修复前:桥接任务在首个 wss 握手处 panic,JoinHandle 立即以 `JoinError` 结束。
    // 修复后:连接失败 → 退避重连,任务持续存活,故这里会等到超时。
    let outcome = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
    assert!(
        outcome.is_err(),
        "run_client 必须持续退避重连,而不是 panic 退出: {outcome:?}"
    );
    task.abort();
}
