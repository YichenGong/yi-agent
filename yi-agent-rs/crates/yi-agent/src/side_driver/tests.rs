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
use yi_agent_runtime::side_conversation::EffectiveSideSource;
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
        effective_config: None,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    provider: Arc<CountingProvider>,
    handles: SideDriverHandles,
}

/// 带「构造门」的 fixture：只有测试放行，`construct_side` 才会继续。
///
/// 用它断言的是**命令循环没被创建阻塞**，而不是「创建要花点时间」。后者用
/// sleep 也能伪造，前者不能。
fn gated_fixture() -> (Fixture, Arc<tokio::sync::Semaphore>) {
    let dir = tempfile::tempdir().unwrap();
    let provider = CountingProvider::new("旁支回答");
    let publisher = Arc::new(SideSourcePublisher::new(source(
        Arc::clone(&provider),
        dir.path(),
    )));
    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
    let resources = SideHostResources::new(dir.path().to_path_buf(), Arc::clone(&pool)).unwrap();
    let (commands_tx, commands_rx) = mpsc::channel(SIDE_COMMAND_CHANNEL);
    let (events_tx, events_rx) = mpsc::channel(SIDE_EVENT_CHANNEL);
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let join = SideDriver::spawn_with_creation_gate(
        publisher,
        resources,
        commands_rx,
        events_tx,
        Arc::clone(&gate),
    );
    (
        Fixture {
            _dir: dir,
            provider,
            handles: SideDriverHandles {
                commands: commands_tx,
                events: events_rx,
                join,
            },
        },
        gate,
    )
}

/// 从事件流里找第一条属于某个 entry 的帧。
async fn recv_until_entry(
    rx: &mut mpsc::Receiver<ConversationEvent>,
    entry: &str,
    mut predicate: impl FnMut(&ConversationEvent) -> bool,
) -> ConversationEvent {
    recv_until(rx, |event| {
        event.entry_id.as_deref() == Some(entry) && predicate(event)
    })
    .await
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
    let handles = spawn_side_driver(publisher, resources);
    Fixture {
        _dir: dir,
        provider,
        handles,
    }
}

fn request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 一次进入的标识。每个测试里的一次「进入」共用一个，重复提交沿用它——
/// 这正是生产代码里 TUI 的分配方式。
fn entry_id() -> String {
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
    let entry = entry_id();
    let id = request_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
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
    let entry = entry_id();
    let id = request_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
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
            entry_id: entry.clone(),
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
async fn leave_reports_closed_and_stops_the_runner() {
    let mut fx = fixture();
    let entry = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: Some("问题".into()),
        })
        .await
        .unwrap();
    let opened = recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry.clone(),
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
    assert!(opened.target == closed.target, "终态必须指向被退出的那一代");

    // 退出之后提问题必须被拒绝(没有旁支了),而不是悄悄开一个新的。
    let calls = fx.provider.calls();
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry.clone(),
            request_id: request_id(),
            text: "关闭之后".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fx.provider.calls(), calls, "退出之后不能再跑一轮");
    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

/// 正在回答时退出：必须**成功**，而不是反问「请先等它结束」。
///
/// spec §4.3 把 `/back`、`/side-close` 都定义为明确的破坏性意图，并明确
/// 「不再二次询问是否停止当前回答」。旧实现用调用方本地那份 `active_turn_id`
/// 做条件关闭，而那个 id 是异步回传的——用户提交问题后、事件被消费前退出，
/// 本地就是 `None`，于是被拒绝、退不出去，正是这次要修的症状。
#[tokio::test]
async fn leaving_a_running_side_destroys_it_without_asking() {
    let mut fx = fixture();
    let entry = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: Some("问题".into()),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // 让旁支跑起来，并且**不**等它的 turn 事件被消费。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry.clone(),
            request_id: request_id(),
            text: "第二个问题".into(),
        })
        .await
        .unwrap();

    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry.clone(),
        })
        .await
        .unwrap();

    let closed = recv_until_entry(&mut fx.handles.events, &entry, |event| {
        matches!(event.event, ConversationPayload::SideClosed(_))
    })
    .await;
    assert!(
        matches!(closed.event, ConversationPayload::SideClosed(_)),
        "退出必须真的完成，不能以「正在回答」为由拒绝"
    );

    // 退出之后这一代不能再被驱动。
    let calls = fx.provider.calls();
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry.clone(),
            request_id: request_id(),
            text: "退出之后".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fx.provider.calls(), calls, "退出之后旧进入不能再提问");

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}

/// 过期 entry 的退出不能杀掉用户之后重新进入建出来的那一套。
///
/// spec §4.4：旧退出重试不能关闭新环境。这里的判据是 entry 而不是 side_id：
/// side_id 由 driver 分配，TUI 在退出那一刻并不知道新的一代会长什么样，所以
/// 唯一稳定的「哪一次进入」标识就是 entry。
#[tokio::test]
async fn a_stale_leave_never_kills_the_newer_environment() {
    let mut fx = fixture();
    let entry_a = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            question: Some("A".into()),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // A 正常退出。
    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry_a.clone(),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(event.event, ConversationPayload::SideClosed(_))
    })
    .await;

    // 用户重新进入，得到新的一代。
    let entry_b = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            question: Some("B".into()),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // A 的迟到退出重试：必须什么都不做。
    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry_a.clone(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // B 仍然可用：能被驱动出一轮。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            text: "B 仍然能问".into(),
        })
        .await
        .unwrap();
    let turn = recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        event.turn_id.is_some()
    })
    .await;
    assert!(turn.turn_id.is_some(), "过期退出不能连带杀掉新环境");

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}

/// 旧代的中断不能停掉当前轮次，错误必须带旧目标回去。
#[tokio::test]
async fn stale_interrupt_does_not_stop_the_live_turn() {
    let mut fx = fixture();
    let entry = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: Some("问题".into()),
        })
        .await
        .unwrap();
    let opened = recv_until(&mut fx.handles.events, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    let ConversationTarget::Side { side_id, .. } = opened.target.clone() else {
        panic!("旁支事件必须带 Side 目标")
    };
    let stale = ConversationTarget::Side {
        side_id,
        generation: 999,
    };

    fx.handles
        .commands
        .send(SideDriverCommand::Interrupt {
            expected_target: stale.clone(),
            // 轮次 id 故意留空：旧实现里空 id 会让比较退化，从而误停新轮次。
            turn_id: String::new(),
        })
        .await
        .unwrap();

    let error = recv_until(&mut fx.handles.events, |event| {
        matches!(event.event, ConversationPayload::SideOperationError { .. })
    })
    .await;
    assert_eq!(error.target, stale);
    match error.event {
        ConversationPayload::SideOperationError { operation, .. } => {
            assert_eq!(operation, SideOperation::Interrupt)
        }
        other => panic!("unexpected {other:?}"),
    }

    // 旁支仍然能正常答完一轮（没被打断）。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry.clone(),
            request_id: request_id(),
            text: "旧中断之后仍能提问".into(),
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

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

/// 屏障期间开旁支：必须产生一条主目标的 `Open` 错误，而不是静默失败。
///
/// 旧实现里 `Open` 失败时还没有 runner，`report_operation_error` 直接 return，
/// 用户按完 `/side` 看到的是「什么都没发生」。
#[tokio::test]
async fn failed_open_reports_a_main_target_error() {
    let mut fx = fixture();
    let entry = entry_id();
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

    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: None,
        })
        .await
        .unwrap();

    let error = recv_until(&mut fx.handles.events, |event| {
        matches!(event.event, ConversationPayload::SideOperationError { .. })
    })
    .await;
    assert_eq!(
        error.target,
        ConversationTarget::Main,
        "还没有旁支身份时，错误属于主视图那次 /side 请求"
    );
    match error.event {
        ConversationPayload::SideOperationError { operation, message } => {
            assert_eq!(operation, SideOperation::Open);
            assert!(!message.is_empty(), "错误必须有可读文案");
        }
        other => panic!("unexpected {other:?}"),
    }

    permit.commit();
    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), fx.handles.join).await;
}

#[test]
fn side_tools_are_empty_so_no_capability_can_leak() {
    // Not "read-only tools minus the dangerous ones": a side has none at all, so
    // editing a list can never re-grant the parent's file, shell or web access.
    let allowed = allowed_side_tools();
    assert!(allowed.is_empty(), "旁支不能有任何工具，实际为 {allowed:?}");
}

/// 创建被卡住时，`Leave` 必须立刻被受理，且**不会**把环境造出来。
///
/// 这是 Task 12 的核心断言，也是旧实现真的做不到的一条：旧 `open` 直接
/// `await` 完整个构造，命令循环在此期间看不到任何命令，于是「用户按了返回」
/// 只能等构造结束——而构造慢的原因（快照、资源准备）恰恰是最需要能退出的
/// 时候。测试用显式门而不是 sleep：门没放行，构造就停在已知位置。
#[tokio::test]
async fn leave_during_creation_cancels_and_creates_nothing() {
    let (mut fx, gate) = gated_fixture();
    let entry_a = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            question: Some("会被取消的问题".into()),
        })
        .await
        .unwrap();

    // 让出调度，确保 actor 已经进入「构造中」。这里不靠时间判断，而是靠
    // 之后的 Leave 有没有被处理：若循环被构造阻塞，Leave 根本发不进 select。
    tokio::task::yield_now().await;

    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry_a.clone(),
        })
        .await
        .unwrap();

    // 放行门：如果取消生效，构造任务看到 token 已取消会直接返回，不建 runner。
    gate.add_permits(1);

    // 唯一的终态是「这次进入没有产出环境」。断言方式：一段时间内没有任何
    // 属于 A 的 Side 帧（没有 announce），且 provider 一次都没被调用。
    let mut saw_side_frame_for_a = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(100), fx.handles.events.recv()).await {
            Ok(Some(event)) => {
                if event.entry_id.as_deref() == Some(entry_a.as_str())
                    && matches!(event.target, ConversationTarget::Side { .. })
                {
                    saw_side_frame_for_a = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        !saw_side_frame_for_a,
        "退出之后构造产物不能 announce 成一个旁支"
    );
    assert_eq!(
        fx.provider.calls(),
        0,
        "被取消的创建不能发出任何模型请求（首问也不许发）"
    );

    // B 必须能进：A 的退出不得留下「永远在创建」的状态。
    let entry_b = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            question: Some("B 的问题".into()),
        })
        .await
        .unwrap();
    // B 也要过门：放行一次。
    gate.add_permits(1);
    let opened_b = recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    assert!(
        matches!(opened_b.target, ConversationTarget::Side { .. }),
        "B 必须建立自己的旁支"
    );

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}

/// A 的迟到状态帧不能把 B 的视图建起来，也不能改 B 的状态。
///
/// 判据是 entry，不是 SideStatus：任何一条 `SideStatus` 都自动建视图，就等于
/// 让已经销毁的进入把界面拉回去。
#[tokio::test]
async fn a_late_frame_from_an_older_entry_does_not_rebuild_a_view() {
    let mut fx = fixture();
    let entry_a = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            question: Some("A".into()),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry_a.clone(),
        })
        .await
        .unwrap();
    // 等 A 的终态，确认这次进入真的结束了。
    recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(event.event, ConversationPayload::SideClosed(_))
    })
    .await;

    // 一次新的进入。
    let entry_b = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            question: Some("B".into()),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // A 的迟到提交必须被拒绝，且不能产生第二次模型请求。
    let calls_before = fx.provider.calls();
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            text: "A 的迟到问题".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fx.provider.calls(),
        calls_before,
        "过期 entry 的提交不能被当成对 B 的提问"
    );

    // B 仍然可用。
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            text: "B 的第二个问题".into(),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        event.turn_id.is_some()
    })
    .await;

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}

/// `Leave` 必须是「退出即销毁」，而不是「返回但保留」。
///
/// 旧 `/back` 只切视图、环境留在后台继续跑，接着还能问——那正是用户明确
/// 否掉的行为。断言点：退出后同一 entry 再提交一律被拒，再进必须拿到新环境
/// （新 side_id），且旧问答不在新环境里。
#[tokio::test]
async fn leave_destroys_the_environment_and_reentry_starts_fresh() {
    let mut fx = fixture();
    let entry_a = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            question: Some("第一问".into()),
        })
        .await
        .unwrap();
    let opened_a = recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    let ConversationTarget::Side {
        side_id: side_id_a, ..
    } = opened_a.target.clone()
    else {
        panic!("旁支事件必须带 Side 目标")
    };

    fx.handles
        .commands
        .send(SideDriverCommand::Leave {
            entry_id: entry_a.clone(),
        })
        .await
        .unwrap();
    recv_until_entry(&mut fx.handles.events, &entry_a, |event| {
        matches!(event.event, ConversationPayload::SideClosed(_))
    })
    .await;

    // 旧 entry 的提交不再有任何效果。
    let calls_after_leave = fx.provider.calls();
    fx.handles
        .commands
        .send(SideDriverCommand::Submit {
            entry_id: entry_a.clone(),
            request_id: request_id(),
            text: "退出之后还想问".into(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fx.provider.calls(),
        calls_after_leave,
        "退出之后旧 entry 不能再驱动任何一轮"
    );

    // 重新进入：必须是一套新环境。
    let entry_b = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry_b.clone(),
            request_id: request_id(),
            question: Some("第二问".into()),
        })
        .await
        .unwrap();
    let opened_b = recv_until_entry(&mut fx.handles.events, &entry_b, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    let ConversationTarget::Side {
        side_id: side_id_b, ..
    } = opened_b.target
    else {
        panic!("旁支事件必须带 Side 目标")
    };
    assert_ne!(
        side_id_a, side_id_b,
        "重新进入必须分配新的 side 身份，不能复用已销毁的那一套"
    );

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}

/// 同一次进入里重复 `/side` 只定位，不重建、不刷新快照。
#[tokio::test]
async fn repeated_open_in_the_same_entry_only_reannounces() {
    let mut fx = fixture();
    let entry = entry_id();
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: Some("首问".into()),
        })
        .await
        .unwrap();
    let first = recv_until_entry(&mut fx.handles.events, &entry, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;

    // 同一个 entry 再 open 一次（不带问题）：只补一帧状态，不新建环境。
    fx.handles
        .commands
        .send(SideDriverCommand::Open {
            entry_id: entry.clone(),
            request_id: request_id(),
            question: None,
        })
        .await
        .unwrap();
    let again = recv_until_entry(&mut fx.handles.events, &entry, |event| {
        matches!(
            event.event,
            ConversationPayload::SideStatus(SideStatus::Idle)
        )
    })
    .await;
    assert_eq!(first.target, again.target, "重复进入必须定位到同一套环境");
    assert_eq!(fx.provider.calls(), 1, "重复进入不能重跑首问");

    let _ = fx.handles.commands.send(SideDriverCommand::Exit).await;
    let _ = tokio::time::timeout(Duration::from_secs(10), fx.handles.join).await;
}
