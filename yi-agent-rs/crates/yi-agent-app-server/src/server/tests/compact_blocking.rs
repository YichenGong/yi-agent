use super::*;

/// 一次 `/compact` 不得堵住**其它会话**的 RPC。
///
/// `/compact` 会调 provider 生成摘要，属于长请求（线上可达十几秒）。RPC 主循环
/// 是单 task：任何在域 handler 里内联 `await` 的请求都会把之后到达的所有请求
/// 串行排在它后面。`thread/compact` 属于 `Domain::Thread`，走的是
/// `dispatch_domain!`（内联 await），而 `Domain::Plugin` 特意 `tokio::spawn`
/// 脱钩——两者对「长请求是否该堵主循环」的判断不一致。
///
/// 用户看到的现象正是这一条：A 会话压缩时，B 会话点 Send 没反应、按钮一直灰。
/// 本测试用一个可控 provider 把压缩钉在半路，然后验证另一条会话的
/// `thread/rename` 能否在不放开压缩的前提下拿到响应。
#[tokio::test(flavor = "multi_thread")]
async fn a_pending_compact_must_not_block_other_threads() {
    use std::sync::atomic::{AtomicBool, Ordering};

    // 压缩时的 provider 调用停在这里，直到测试放行。
    let release = Arc::new(tokio::sync::Notify::new());
    // 记录 provider 是否真的被调用过（= 压缩确实进入了长请求段）。
    let entered = Arc::new(AtomicBool::new(false));

    struct GatedCompactProvider {
        release: Arc<tokio::sync::Notify>,
        entered: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl yi_agent_core::Provider for GatedCompactProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            self.entered.store(true, Ordering::SeqCst);
            // 挂住：压缩在这一点上还没返回，正是用户看到的"正在压缩"。
            self.release.notified().await;
            Ok(futures::stream::iter(Vec::new()).boxed())
        }
    }

    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(GatedCompactProvider {
        release: Arc::clone(&release),
        entered: Arc::clone(&entered),
    });

    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let factory_provider = Arc::clone(&provider);
    let mut h = Harness::with_config(
        cfg,
        move |session, cwd, mode| {
            // 压缩只有在历史足够长时才会真的调 provider（短历史直接判
            // not_reduced）。这里给一条超过默认 user budget 的历史，让
            // `thread/compact` 确实走进那段长请求。
            let session = session.or_else(|| {
                let mut s = yi_agent_core::Session::new();
                let mut messages = Vec::new();
                for i in 0..60 {
                    messages.push(yi_agent_core::Message::user(format!(
                        "old user message {i} {}",
                        "filler detail ".repeat(60)
                    )));
                    messages.push(yi_agent_core::Message::assistant(vec![
                        yi_agent_core::ContentBlock::Text(format!(
                            "old assistant reply {i} {}",
                            "filler detail ".repeat(60)
                        )),
                    ]));
                }
                s.replace_messages(messages);
                Some(s)
            });
            build_test_agent_with_provider(Arc::clone(&factory_provider), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
    );

    // 会话 A（要被压缩的那条）与 B（另一个用户在等的会话）。
    let tid_a = start_thread(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":40,"method":"thread/start","params":{}}"#)
        .await;
    let started_b = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(40)))
        .await;
    let tid_b = started_b["result"]["thread_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 让 A 有足够历史，压缩才不会立刻判成 not_reduced。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":41,"method":"thread/compact","params":{{"threadId":"{tid_a}"}}}}"#
    ))
    .await;

    // 等到 provider 真被调用：此刻 `thread/compact` 正卡在主循环里等摘要。
    for _ in 0..200 {
        if entered.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        entered.load(Ordering::SeqCst),
        "the compaction must reach the provider for this test to mean anything"
    );

    // 关键断言：压缩仍挂起时，另一条会话的请求必须能得到响应。
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":42,"method":"thread/rename","params":{{"threadId":"{tid_b}","title":"B"}}}}"#
    ))
    .await;

    let rename = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        h.read_matching(64, |v| v.get("id") == Some(&serde_json::json!(42))),
    )
    .await;

    // 先放行压缩，再断言：失败时也要让 driver 收尾，否则 shutdown 会挂住。
    release.notify_waiters();

    let rename = rename.expect(
        "a pending /compact must not block another thread's RPC: the rename never \
         answered while the compaction was still waiting on the provider",
    );
    assert!(
        rename.get("error").is_none(),
        "renaming another thread must succeed: {rename}"
    );

    h.shutdown().await;
}
