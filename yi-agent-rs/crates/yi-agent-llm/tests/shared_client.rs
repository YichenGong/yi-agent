mod common;

use std::time::Duration;

use common::{Body, ConnCounter};

/// 核心不变量：同一个 (base_url, timeout) 必须共用一条连接。
#[tokio::test]
async fn same_key_reuses_one_connection() {
    let server = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let url = server.uri();

    let a = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();
    let b = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();

    for client in [a, b] {
        let _ = client
            .post(format!("{url}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(
        server.accepts(),
        1,
        "two clients with the same key must share one TCP connection"
    );
}
