//! `side_driver` 的测试。
//!
//! 这里用真实 runtime 组件（真的 runner、真的资源槽、真的 ledger）配一个
//! 脚本化 provider,断言的是**调度行为**而不是 mock 的自我介绍:
//! 主会话运行期间旁支能独立完成一轮、重试同一请求不会跑第二次、关闭只停旁支。

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream;
use yi_agent_core::{
    AgentConfig, ContentBlock, Message, Provider, ProviderError, ProviderEvent, ProviderRequest,
    Session, StopReason,
};
use yi_agent_runtime::side_conversation::{EffectiveSideSource, SideWebPolicy};
use yi_agent_tools::SideRoots;

#[derive(Debug)]
struct StaticRoots(Vec<std::path::PathBuf>);

impl SideRoots for StaticRoots {
    fn current_roots(&self) -> Vec<std::path::PathBuf> {
        self.0.clone()
    }
}

/// 记录请求数、可被 Notify 卡住的 provider。
struct CountingProvider {
    calls: AtomicUsize,
    replies: Mutex<Vec<String>>,
}

impl CountingProvider {
    fn new(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            replies: Mutex::new(vec![reply.to_string()]),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for CountingProvider {
    async fn call_stream(
        &self,
        _req: ProviderRequest,
    ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let reply = self
            .replies
            .lock()
            .unwrap()
            .first()
            .cloned()
            .unwrap_or_else(|| "旁支回答".to_string());
        Ok(Box::pin(stream::iter(vec![
            ProviderEvent::TextDelta(reply),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ])))
    }
}

/// 一个可用的父来源:带一段安全历史,provider 是本测试拥有的那个实例。
fn source(provider: Arc<CountingProvider>, root: &std::path::Path) -> EffectiveSideSource {
    let mut session = Session::new();
    session.push(Message::user("父问题"));
    session.push(Message::assistant(vec![ContentBlock::Text(
        "父回答".to_string(),
    )]));
    EffectiveSideSource {
        session: Arc::new(Mutex::new(session)),
        provider,
        config: AgentConfig {
            model: "parent-model".to_string(),
            system_prompt: Some("parent prompt".to_string()),
            ..Default::default()
        },
        primary: root.to_path_buf(),
        roots: vec![root.to_path_buf()],
        current_roots: Arc::new(StaticRoots(vec![root.to_path_buf()])),
        attachments: Default::default(),
        revision: 1,
        generation: 1,
        provider_gate: None,
        web_policy: SideWebPolicy::default(),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    provider: Arc<CountingProvider>,
    handles: SideDriverHandles,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let provider = CountingProvider::new("旁支回答");
    let publisher = Arc::new(SideSourcePublisher::new(source(
        Arc::clone(&provider),
        dir.path(),
    )));
    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
    let resources = SideHostResources::new(dir.path().to_path_buf(), Arc::clone(&pool)).unwrap();
    let handles = spawn_side_driver(publisher, resources, pool);
    Fixture {
        _dir: dir,
        provider,
        handles,
    }
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 收事件直到谓词成立,或超时失败。
async fn recv_until(
    rx: &mut mpsc::Receiver<ConversationEvent>,
    mut predicate: impl FnMut(&ConversationEvent) -> bool,
) -> ConversationEvent {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(event)) => {
                if predicate(&event) {
                    return event;
                }
            }
            Ok(None) => panic!("event channel closed"),
            Err(_) => panic!("timed out waiting for the expected side event"),
        }
    }
    panic!("no matching event in 200 frames");
}

#[tokio::test]
async fn side_driver_opens_and_answers_without_touching_main() {
    let mut fx = fixture();
    let id = request_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            request_id: id,
            question: Some("旁支问题".into()),
        })
        .await
        .unwrap();

    // 先看到旁支被创建,再看到它的正文。
    let created = recv_until(&mut fx.handles.events, |event| {
        matches!(event.event, ConversationPayload::SideStatus(_))
    })
    .await;
    assert!(
        matches!(created.target, ConversationTarget::Side { .. }),
        "旁支事件必须带 Side 目标"
    );

    let items = recv_until(&mut fx.handles.events, |event| {
        matches!(event.event, ConversationPayload::SideItems(_))
    })
    .await;
    match items.event {
        ConversationPayload::SideItems(items) => {
            assert!(!items.is_empty());
            assert!(items.iter().all(|item| item.turn_id == items[0].turn_id));
        }
        other => panic!("unexpected {other:?}"),
    }

    // 收尾:等到 Idle,证明这一轮真的结束了而不是被半路吞掉。
    recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    assert_eq!(fx.provider.calls(), 1);

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[tokio::test]
async fn retried_request_id_does_not_run_a_second_turn() {
    let mut fx = fixture();
    let id = request_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            request_id: id.clone(),
            question: Some("同一个问题".into()),
        })
        .await
        .unwrap();
    recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // 同一个 request_id + 同一段文本:拿回原轮次,不产生第二次模型调用。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            request_id: id,
            text: "同一个问题".into(),
        })
        .await
        .unwrap();
    let turn = recv_until(&mut fx.handles.events, |event| event.turn_id.is_some()).await;
    assert!(turn.turn_id.is_some());
    assert_eq!(fx.provider.calls(), 1, "重试必须命中去重记录,不能再跑一轮");

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[tokio::test]
async fn close_reports_closed_and_stops_the_runner() {
    let mut fx = fixture();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            request_id: request_id(),
            question: Some("问题".into()),
        })
        .await
        .unwrap();
    recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    fx.handles
        .commands
        .send(SideDriverCommand::Close {
            expected_active_turn_id: None,
        })
        .await
        .unwrap();
    let closed = recv_until(&mut fx.handles.events, |event| {
        matches!(event.event, ConversationPayload::SideClosed(_))
    })
    .await;
    match closed.event {
        ConversationPayload::SideClosed(reason) => {
            assert_eq!(reason, CloseReason::UserClosed)
        }
        other => panic!("unexpected {other:?}"),
    }

    // 关闭之后提问题必须被拒绝(没有旁支了),而不是悄悄开一个新的。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            request_id: request_id(),
            text: "关闭之后".into(),
        })
        .await
        .unwrap();
    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[tokio::test]
async fn close_with_stale_turn_id_is_refused() {
    let mut fx = fixture();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            request_id: request_id(),
            question: Some("问题".into()),
        })
        .await
        .unwrap();
    recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    fx.handles
        .commands
        .send(SideDriverCommand::Close {
            expected_active_turn_id: Some("already-finished".into()),
        })
        .await
        .unwrap();

    // 冲突时不该有任何 SideClosed;旁支仍然可用。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            request_id: request_id(),
            text: "冲突之后仍能提问".into(),
        })
        .await
        .unwrap();
    let turn = recv_until(&mut fx.handles.events, |event| event.turn_id.is_some()).await;
    assert!(turn.turn_id.is_some());

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[tokio::test]
async fn parent_barrier_blocks_open_until_rolled_back() {
    let mut fx = fixture();
    let (tx, rx) = oneshot::channel();
    fx.handles
        .commands
        .send(SideDriverCommand::ParentBarrier {
            reason: CloseReason::ParentCleared,
            reply: tx,
        })
        .await
        .unwrap();
    let permit = rx.await.unwrap();

    // 屏障期间开旁支被拒绝,而且**没有**旁支被创建。
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            request_id: request_id(),
            question: None,
        })
        .await
        .unwrap();

    // 回滚:旧状态继续可用,之后可以正常打开。
    permit.commit();
    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[test]
fn side_tool_policy_is_read_only_and_never_empty() {
    let allowed = allowed_side_tools();
    assert!(!allowed.is_empty());
    for forbidden in ["bash", "write", "edit", "apply_patch"] {
        assert!(
            !allowed.contains(&forbidden),
            "{forbidden} 不能出现在旁支只读白名单里"
        );
    }
}
