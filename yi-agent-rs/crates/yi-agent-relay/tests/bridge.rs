//! 端到端:`run_client` 把一个「本地 ws echo 端点」与中继里的一条 phone 连接桥起来。
//!
//! 本测试**不**起真实 app-server(那在 `yi-agent-app-server` 的测试里覆盖);这里
//! 用一个行为等价的**假本地端点**(ws server):带 `?token=dev-token` 时它把收到的
//! 每一帧回显。于是:
//!
//! ```text
//!  phone ──▶ relay ─/connect── run_client ──ws──▶ 假本地端点(echo)
//!    ▲                                                │
//!    └──────────────── relay 转发回显 ◀───────────────┘
//! ```
//!
//! 断言 phone 发的帧经中继与 `run_client` 抵达本地端点、且回显原路返回。这钉死了
//! 双向桥接与 `?session=`/`?token=` 两条查询串的拼装。

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as ClientMessage;
use url::Url;
use yi_agent_relay::Relay;

/// 假本地 app-server:只接受带 `?token=dev-token` 的连接,并把收到的 Text 帧回显。
async fn spawn_fake_local_app_server() -> std::net::SocketAddr {
    use axum::Router;
    use axum::extract::Query;
    use axum::routing::get;

    #[derive(serde::Deserialize)]
    struct Q {
        token: Option<String>,
    }

    let app = Router::new().route(
        "/ws",
        get(
            move |Query(q): Query<Q>, upgrade: axum::extract::ws::WebSocketUpgrade| async move {
                if q.token.as_deref() != Some("dev-token") {
                    // 模拟真实 app-server 的 4401 拒绝。
                    return upgrade.on_upgrade(
                        |mut socket: axum::extract::ws::WebSocket| async move {
                            let _ = socket
                                .send(axum::extract::ws::Message::Close(Some(
                                    axum::extract::ws::CloseFrame {
                                        code: 4401,
                                        reason: "unauthorized".into(),
                                    },
                                )))
                                .await;
                        },
                    );
                }
                upgrade.on_upgrade(|socket: axum::extract::ws::WebSocket| async move {
                    use futures::{SinkExt, StreamExt};
                    let (mut sink, mut stream) = socket.split();
                    while let Some(Ok(msg)) = stream.next().await {
                        match msg {
                            axum::extract::ws::Message::Text(t) => {
                                let _ = sink.send(axum::extract::ws::Message::Text(t)).await;
                            }
                            axum::extract::ws::Message::Close(_) => break,
                            _ => {}
                        }
                    }
                })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// 起一个中继,返回其地址。
async fn spawn_relay() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = yi_agent_relay::server::serve(listener, Arc::new(Relay::new())).await;
    });
    addr
}

#[tokio::test(flavor = "multi_thread")]
async fn run_client_bridges_a_phone_frame_to_the_local_app_server_and_back() {
    let local = spawn_fake_local_app_server().await;
    let relay = spawn_relay().await;

    // run_client 永不返回;把它丢到一个任务里。连不上会重试,但本测试两端都活着。
    let relay_url = Url::parse(&format!("ws://{relay}/connect")).unwrap();
    let local_url = Url::parse(&format!("ws://{local}/ws")).unwrap();
    tokio::spawn(async move {
        let _ = yi_agent_relay::run_client(
            relay_url,
            local_url,
            "session-42".to_string(),
            "dev-token".to_string(),
        )
        .await;
    });

    // 手机侧连中继的 /ws?session=session-42。
    let (mut phone, _) =
        tokio_tungstenite::connect_async(format!("ws://{relay}/ws?session=session-42"))
            .await
            .expect("phone connect");

    // 手机发一帧;桥接就绪前中继可能先回一条 "no computer" 错误帧,故带重试。
    let echoed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            phone
                .send(ClientMessage::Text("hello-through-relay".into()))
                .await
                .unwrap();
            match tokio::time::timeout(Duration::from_millis(300), phone.next()).await {
                Ok(Some(Ok(ClientMessage::Text(t)))) if t == "hello-through-relay" => {
                    return t.to_string();
                }
                // 还没桥到电脑(错误帧 / 其它);稍等再试。
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("the frame must round-trip through relay + run_client + local endpoint");

    assert_eq!(echoed, "hello-through-relay");
}

/// 本地端点要求 `?token=dev-token`;`run_client` 必须带上它接口。
/// 若 token 错误,`run_client` 会连本地失败并一直重试,手机侧永远收不到回显。
/// 本测试从「错误 token」侧反向确认桥接确实走了认证路径。
#[tokio::test(flavor = "multi_thread")]
async fn run_client_with_a_bad_local_token_never_delivers() {
    let local = spawn_fake_local_app_server().await;
    let relay = spawn_relay().await;

    let relay_url = Url::parse(&format!("ws://{relay}/connect")).unwrap();
    let local_url = Url::parse(&format!("ws://{local}/ws")).unwrap();
    tokio::spawn(async move {
        let _ = yi_agent_relay::run_client(
            relay_url,
            local_url,
            "session-bad".to_string(),
            "wrong-token".to_string(),
        )
        .await;
    });

    let (mut phone, _) =
        tokio_tungstenite::connect_async(format!("ws://{relay}/ws?session=session-bad"))
            .await
            .expect("phone connect");
    phone
        .send(ClientMessage::Text("should-not-arrive".into()))
        .await
        .unwrap();

    // 窗口内不应收到回显(本地认证失败,桥未建立)。
    let got = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(msg)) = phone.next().await {
            if let ClientMessage::Text(t) = msg {
                if t == "should-not-arrive" {
                    return true;
                }
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(!got, "a bad local token must not yield a bridged echo");
}
