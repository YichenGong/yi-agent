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

/// 不同 timeout 必须分池：探索/探测用短线，正式对话用长线，不能互相污染。
#[tokio::test]
async fn different_timeouts_use_separate_pools() {
    let server = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let url = server.uri();

    let short = yi_agent_llm::http::shared_client(&url, Duration::from_secs(15)).unwrap();
    let long = yi_agent_llm::http::shared_client(&url, Duration::from_secs(300)).unwrap();

    for client in [short, long] {
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
        2,
        "different timeouts must not share a pool"
    );
}

/// 契约测试：归一化由 provider 的 `new()` 负责，`shared_client` 不做。
/// 这条锁住设计决定——将来有人在 shared_client 里"顺手"加 trim 会失败。
#[tokio::test]
async fn shared_client_does_not_normalise_trailing_slash() {
    let server = ConnCounter::start(vec![("/v1/chat/completions", 200, Body::Empty)]).await;
    let bare = format!("{}/v1", server.uri());
    let slashed = format!("{}/v1/", server.uri());

    let a = yi_agent_llm::http::shared_client(&bare, Duration::from_secs(5)).unwrap();
    let b = yi_agent_llm::http::shared_client(&slashed, Duration::from_secs(5)).unwrap();

    // 两个键不同 → 两个池 → 两条连接。provider 侧会先 trim 再传，所以生产路径上
    // 不会出现这种差；这里只锁住 shared_client 的职责边界。
    for client in [a, b] {
        let _ = client
            .post(format!("{bare}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await;
    }

    assert_eq!(
        server.accepts(),
        2,
        "shared_client must not normalise; that is the provider's job"
    );
}

/// 不同 base_url 必须落在不同的池，互不干扰。
#[tokio::test]
async fn different_base_urls_are_isolated() {
    let one = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;
    let two = ConnCounter::start(vec![("/chat/completions", 200, Body::Empty)]).await;

    for server in [&one, &two] {
        let url = server.uri();
        let client = yi_agent_llm::http::shared_client(&url, Duration::from_secs(5)).unwrap();
        let _ = client
            .post(format!("{url}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(one.accepts(), 1);
    assert_eq!(two.accepts(), 1);
    assert_eq!(one.path_hits("/chat/completions"), 1);
    assert_eq!(two.path_hits("/chat/completions"), 1);
}

use yi_agent_llm::{OpenaiProvider, OpenaiProviderOpts};

/// 真正的验收点：两个 provider（不同 api_key）共用同一条 TCP 连接。
#[tokio::test]
async fn two_providers_share_one_connection() {
    let server = ConnCounter::start(vec![("/v1/chat/completions", 200, Body::Empty)]).await;
    let base = format!("{}/v1", server.uri());

    for key in ["key-one", "key-two"] {
        let provider = OpenaiProvider::new(OpenaiProviderOpts {
            base_url: Some(base.clone()),
            api_key: Some(key.to_string()),
            timeout: Some(Duration::from_secs(5)),
            api_style: Some("chat".into()),
            ..Default::default()
        })
        .expect("provider construction");

        // 只发请求不消费流：body 是否合法 SSE 与连接复用无关。
        let _ = provider
            .client()
            .post(format!("{base}/chat/completions"))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .expect("request should succeed");
    }

    assert_eq!(
        server.accepts(),
        1,
        "two providers with the same base_url and timeout must share one connection"
    );
}

/// 通道判定必须跨 provider 共享：同一 gateway 的能力不会因为换了会话而变。
/// 否则每个新会话都要先吃一次 `/responses` 404 才回退 chat，多付一个 RTT。
#[tokio::test]
async fn channel_detection_is_shared_across_providers() {
    let chat_sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let server = ConnCounter::start(vec![
        // `/responses` 故意不配置 → 服务端回 404 空 body，正是
        // `looks_like_missing_route` 认的「路由不存在」。
        ("/v1/chat/completions", 200, Body::Sse(chat_sse.to_string())),
    ])
    .await;
    let base = format!("{}/v1", server.uri());

    use futures::StreamExt;
    use yi_agent_core::{GenParams, Message, Provider, ProviderRequest};

    let request = || ProviderRequest {
        model: "test-model".into(),
        system: None,
        messages: vec![Message::user("ping")],
        tools: Vec::new(),
        params: GenParams {
            max_tokens: Some(8),
            ..Default::default()
        },
    };

    for _ in 0..2 {
        let provider = OpenaiProvider::new(OpenaiProviderOpts {
            base_url: Some(base.clone()),
            api_key: Some("k".into()),
            timeout: Some(Duration::from_secs(5)),
            api_style: Some("auto".into()),
            ..Default::default()
        })
        .expect("provider construction");

        let mut stream = provider.call_stream(request()).await.expect("stream ok");
        while stream.next().await.is_some() {}
    }

    assert_eq!(
        server.path_hits("/v1/responses"),
        1,
        "the /responses probe must run once per process, not once per provider"
    );
    assert_eq!(server.path_hits("/v1/chat/completions"), 2);
    assert_eq!(
        server.accepts(),
        1,
        "same base_url and timeout must still share one connection"
    );
}
