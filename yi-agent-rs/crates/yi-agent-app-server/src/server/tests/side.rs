//! `side/*` 的传输层集成测试:洗过真实 stdio 主循环的那些规则。
//!
//! 单元测试(`src/rpc/side/tests.rs`)钉的是形状与边界;这里钉的是**只有跑过
//! dispatch 与主循环才成立的三件事**:
//!
//! 1. 权限档真的在 dispatch 处生效(写反了就是手机读不了 / 只读端能花钱)。
//! 2. 慢活真的离开了请求路径(内联 `await` 会堵住所有连接)。
//! 3. 能力位与宿主 id 出自同一份宿主 resources(重连不该被当成宿主换代)。

use std::sync::Arc;
use std::time::Duration;

use crate::protocol::Scope;
use crate::server::tests::{Harness, initialize, test_side_registry};

/// 发一帧,读回同一 id 的响应。
async fn call(
    h: &mut Harness,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let frame = serde_json::json!({
        "jsonrpc": "2.0", "id": id, "method": method, "params": params
    });
    h.send(&frame.to_string()).await;
    h.read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
        .await
}

/// 只读视图能列,不能开。这是 spec §8.1 的权限划分:发现与阅读不花钱,创建与
/// 发问花钱。
#[tokio::test]
async fn side_list_is_observe_and_turn_start_is_control() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Observe, registry).await;
    initialize(&mut h).await;

    let listed = call(
        &mut h,
        2,
        "side/list",
        serde_json::json!({"parentThreadId": "p1"}),
    )
    .await;
    assert!(listed.get("result").is_some(), "Observe 必须能列: {listed}");
    assert_eq!(listed["result"]["sides"], serde_json::json!([]));

    // `turnStart` 需要 Control。用 Observe 连接发它必须被拒在 dispatch 处,
    // 而不是进到 registry 之后才失败(那样错误码会变成 side_not_found,客户端
    // 就无法区分「权限不够」与「旁支不存在」)。
    let refused = call(
        &mut h,
        3,
        "side/turnStart",
        serde_json::json!({
            "sideId": "s1",
            "text": "x",
            "clientRequestId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert_eq!(
        refused["error"]["code"],
        serde_json::json!(-32014),
        "{refused}"
    );

    h.shutdown().await;
}

/// 未识别的方法名走 `method_not_found`,不是静默成功也不是内部错误。
#[tokio::test]
async fn side_unknown_method_under_side_namespace_is_method_not_found() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Admin, registry).await;
    initialize(&mut h).await;

    let v = call(&mut h, 2, "side/nope", serde_json::json!({})).await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32601), "{v}");

    h.shutdown().await;
}

/// 同一个进程里,两条连接必须报**同一个**宿主 id。两条连接各报一个的话,
/// 客户端会把每次重连都当成宿主重启,从而丢掉全部本地基线。
#[tokio::test]
async fn side_open_from_two_connections_shares_one_host_instance_id() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let expected = registry.host_instance_id().to_string();

    let mut a = Harness::with_scope_and_side_registry(Scope::Admin, Arc::clone(&registry)).await;
    let mut b = Harness::with_scope_and_side_registry(Scope::Admin, Arc::clone(&registry)).await;
    initialize(&mut a).await;
    initialize(&mut b).await;

    let ia = call(&mut a, 2, "initialize", serde_json::json!({})).await;
    let ib = call(&mut b, 2, "initialize", serde_json::json!({})).await;
    assert_eq!(
        ia["result"]["sideHostInstanceId"],
        serde_json::json!(expected)
    );
    assert_eq!(
        ib["result"]["sideHostInstanceId"],
        serde_json::json!(expected)
    );

    a.shutdown().await;
    b.shutdown().await;
}

/// `open` 要读父会话的持久状态,这在慢盘上可以是几百毫秒。主循环是单 task:
/// 内联 `await` 会让**所有**连接一起卡住,包括桌面 GUI 的心跳与设置页。
///
/// 这里用 barrier 把解析卡住,再发一个纯内存的 `ui/settings/read`。它必须**先**
/// 回来——若 `open` 在请求路径上,它会排在后面永远回不来。
#[tokio::test]
async fn side_creating_open_does_not_block_the_loop() {
    let home = tempfile::TempDir::new().unwrap();
    // 两个参与者:主循环的 open 任务 + 测试自己。等到两者都到达,解析才放行。
    let gate = Arc::new(tokio::sync::Barrier::new(2));
    let registry = test_side_registry(home.path(), Some(Arc::clone(&gate)));
    let mut h = Harness::with_scope_and_side_registry(Scope::Admin, registry).await;
    initialize(&mut h).await;

    // 先拿一张票(纯内存,立即返回),再用它 open。旧形状的 open 现在就等于
    // `invalid_params`,不会再走到解析器上,那样这个测试就只是在测参数校验。
    let prepared = call(
        &mut h,
        1,
        "side/prepare",
        serde_json::json!({"parentThreadId": "p1"}),
    )
    .await;
    let entry_id = prepared["result"]["entryId"]
        .as_str()
        .expect("prepare must issue an entry ticket")
        .to_string();
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "side/open",
            "params": {"parentThreadId": "p1", "entryId": entry_id}
        })
        .to_string(),
    )
    .await;

    // 给主循环一个调度点:若 `open` 内联,这一帧就永远排不上。
    tokio::time::sleep(Duration::from_millis(50)).await;

    let settings = call(&mut h, 3, "ui/settings/read", serde_json::json!({})).await;
    assert!(
        settings.get("result").is_some(),
        "主循环被 open 堵住了: {settings}"
    );

    // 放行解析,`open` 最终必须回一个错误(替身不认 p1)。
    gate.wait().await;
    let opened = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    assert_eq!(
        opened["error"]["code"],
        serde_json::json!(-32011),
        "{opened}"
    );

    h.shutdown().await;
}

/// 能力位与宿主 id 必须出自同一份宿主 resources。
///
/// 两个断言合在一起才有意义:客户端拿到的语义是「这个宿主是谁、支持哪一版旁支
/// 协议」。只翻能力位而不给 id,客户端就无法区分「重连」与「宿主换代」,会丢掉
/// 全部本地基线。
#[tokio::test]
async fn side_initialize_advertises_capability_and_a_host_id() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let expected = registry.host_instance_id().to_string();
    let mut h = Harness::with_scope_and_side_registry(Scope::Admin, registry).await;

    let v = call(&mut h, 1, "initialize", serde_json::json!({})).await;
    assert_eq!(
        v["result"]["capabilities"]["side_conversation_v1"],
        serde_json::json!(crate::protocol::SIDE_CONVERSATION_V1)
    );
    assert_eq!(
        v["result"]["sideHostInstanceId"],
        serde_json::json!(expected),
        "{v}"
    );

    h.shutdown().await;
}

/// 旁支不存在时 `readItems` 必须是稳定的 `side_not_found`,而不是一台「看起来
/// 通了但永远回空页」的分页器。客户端据此决定「清掉旧临时视图」。
#[tokio::test]
async fn side_read_items_reports_a_missing_side() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Observe, registry).await;
    initialize(&mut h).await;

    let v = call(
        &mut h,
        2,
        "side/readItems",
        serde_json::json!({"sideId": "nope"}),
    )
    .await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32031), "{v}");
    assert_eq!(
        v["error"]["data"]["reason"],
        serde_json::json!("side_not_found"),
        "{v}"
    );

    h.shutdown().await;
}

/// `cursor` 不带 `snapshotToken` 必须在请求路径上就被拒:那个组合看不出属于
/// 哪个快照,放过去就等于允许把两个时刻的页拼起来。
#[tokio::test]
async fn side_read_items_rejects_a_cursor_without_a_token() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Observe, registry).await;
    initialize(&mut h).await;

    let v = call(
        &mut h,
        2,
        "side/readItems",
        serde_json::json!({"sideId": "s1", "cursor": "12"}),
    )
    .await;
    assert_eq!(v["error"]["code"], serde_json::json!(-32602), "{v}");
    assert_eq!(
        v["error"]["data"]["reason"],
        serde_json::json!("invalid_params"),
        "{v}"
    );

    h.shutdown().await;
}

/// 重连回放:同一个快照 token 只能被**同一条连接**续页。另一条连接拿着它读,
/// 得到的必须是 `snapshot_expired`——否则两条连接会共用一条游标,页会互相覆盖。
#[tokio::test]
async fn side_snapshot_token_does_not_cross_connections() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut a = Harness::with_scope_and_side_registry(Scope::Observe, Arc::clone(&registry)).await;
    let mut b = Harness::with_scope_and_side_registry(Scope::Observe, Arc::clone(&registry)).await;
    initialize(&mut a).await;
    initialize(&mut b).await;

    // 两台连接对同一个不存在的 side 读,错误必须一致且不泄漏「谁先读过」。
    for h in [&mut a, &mut b] {
        let v = call(
            h,
            2,
            "side/readItems",
            serde_json::json!({"sideId": "s1", "snapshotToken": "made-up", "cursor": "1"}),
        )
        .await;
        assert_eq!(
            v["error"]["data"]["reason"],
            serde_json::json!("snapshot_expired"),
            "{v}"
        );
    }

    a.shutdown().await;
    b.shutdown().await;
}

/// 能力位打开后,`initialize` 仍然要与 `side/list` 一起可用:客户端在握手的那
/// 一刻就要能把「这个宿主支持旁支」与「当前有哪些 side」连起来。
#[tokio::test]
async fn side_handshake_and_list_work_together() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Observe, registry).await;

    let hello = call(&mut h, 1, "initialize", serde_json::json!({})).await;
    assert_eq!(
        hello["result"]["capabilities"]["side_conversation_v1"],
        serde_json::json!(crate::protocol::SIDE_CONVERSATION_V1)
    );
    assert!(hello["result"]["sideHostInstanceId"].is_string(), "{hello}");

    let listed = call(
        &mut h,
        2,
        "side/list",
        serde_json::json!({"parentThreadId": "p1"}),
    )
    .await;
    assert_eq!(listed["result"]["sides"], serde_json::json!([]));

    h.shutdown().await;
}

/// 关掉开关后，六个方法都必须报「不存在」，而不是静默成功或「权限不够」。
///
/// 逐条断言不抽样：漏掉一个方法就是一个仍然可用的入口。
#[tokio::test]
async fn every_side_method_is_method_not_found_when_the_flag_is_off() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_side_flag_and_registry(Scope::Admin, registry, false).await;
    initialize(&mut h).await;

    for (i, method) in [
        "side/prepare",
        "side/open",
        "side/exit",
        "side/list",
        "side/readItems",
        "side/turnStart",
        "side/turnInterrupt",
        "side/close",
    ]
    .iter()
    .enumerate()
    {
        let answer = call(&mut h, 10 + i as u64, method, serde_json::json!({})).await;
        assert_eq!(
            answer["error"]["code"],
            serde_json::json!(-32601),
            "{method} must not exist while the flag is off: {answer}"
        );
    }

    h.shutdown().await;
}

/// 能力位跟着开关走，但宿主 id 与它无关：客户端用后者区分「重连」与「宿主
/// 换代」，翻能力位时把 id 一起改掉会让它丢掉全部本地基线。
#[tokio::test]
async fn the_capability_bit_follows_the_flag_but_the_host_id_does_not() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let expected = registry.host_instance_id().to_string();
    let mut h = Harness::with_scope_side_flag_and_registry(Scope::Admin, registry, false).await;

    let hello = call(&mut h, 1, "initialize", serde_json::json!({})).await;
    assert_eq!(
        hello["result"]["capabilities"]["side_conversation_v1"],
        serde_json::json!(false),
        "{hello}"
    );
    assert_eq!(
        hello["result"]["sideHostInstanceId"],
        serde_json::json!(expected),
        "the host id must not depend on the flag: {hello}"
    );

    h.shutdown().await;
}

/// 一个真实的父会话被本进程驱动时,旁问**不能**退化成再读一遍磁盘。
///
/// 断言三件事,任何一件失败都意味着用户会看到「旁问看不到当前进度」:
///
/// 1. `thread/start` 一返回,live 表里就有这个 thread——此刻用户可能刚敲 `/side`。
/// 2. 一轮跑完之后,发布出来的快照包含这一轮的问答(文件副本尚未来得及被读)。
/// 3. 会话从 `threads` 摘掉、driver 退出之后,表里不再残留它(否则 resolver
///    会对着一个没有 driver 的旧发布器工作)。
#[tokio::test(flavor = "multi_thread")]
async fn a_live_parent_publishes_its_progress_and_unpublishes_on_exit() {
    use crate::server::tests::read_response;

    let mut h = Harness::new();
    initialize(&mut h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let started = read_response(&mut h, 2).await;
    let thread_id = started["result"]["thread_id"]
        .as_str()
        .expect("thread/start returns an id")
        .to_string();

    // (1) 发布早于第一个回合：插入 `threads` 与发布是同一步。
    let publisher = h
        .live_side_parents
        .get(&thread_id)
        .expect("a started thread is a live parent at once");

    // 一轮真实对话。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "父会话的当前问题"}],
            }
        })
        .to_string(),
    )
    .await;
    let _ = read_response(&mut h, 3).await;
    // 等 driver 把这一轮收尾并重发布。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut rendered = String::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok(source) = publisher.current() {
            let session = source.session.lock().unwrap();
            rendered = serde_json::to_string(&session.messages().to_vec()).unwrap();
            if rendered.contains("父会话的当前问题") {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // (2) 这一轮真的进了发布出来的快照。
    assert!(
        rendered.contains("父会话的当前问题"),
        "a live parent's committed turn must reach a later side: {rendered}"
    );

    // (3) 会话被删除 → driver 的接收端关闭 → 发布器随 driver 一起消失。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "thread/delete",
            "params": {"threadId": thread_id}
        })
        .to_string(),
    )
    .await;
    let deleted = read_response(&mut h, 4).await;
    assert!(deleted.get("result").is_some(), "{deleted}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline && h.live_side_parents.get(&thread_id).is_some() {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        h.live_side_parents.get(&thread_id).is_none(),
        "an exited driver must not leave a live parent behind"
    );

    h.shutdown().await;
}

/// 两条连接各签一张票,open 到同一父必须共享一代;一端 exit 后旧票失效,
/// 再 prepare 得到新的一代。
#[tokio::test]
async fn prepare_then_open_shares_one_generation_and_exit_retires_the_ticket() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let expected = registry.host_instance_id().to_string();
    let mut a = Harness::with_scope_and_side_registry(Scope::Admin, Arc::clone(&registry)).await;
    let mut b = Harness::with_scope_and_side_registry(Scope::Admin, Arc::clone(&registry)).await;
    initialize(&mut a).await;
    initialize(&mut b).await;

    let pa = call(
        &mut a,
        10,
        "side/prepare",
        serde_json::json!({"parentThreadId": "p1"}),
    )
    .await;
    let pb = call(
        &mut b,
        11,
        "side/prepare",
        serde_json::json!({"parentThreadId": "p1"}),
    )
    .await;
    assert_eq!(pa["result"]["hostInstanceId"], serde_json::json!(expected));
    assert_eq!(pa["result"]["parentThreadId"], serde_json::json!("p1"));
    let entry_a = pa["result"]["entryId"].as_str().unwrap().to_string();
    let entry_b = pb["result"]["entryId"].as_str().unwrap().to_string();
    assert_ne!(entry_a, entry_b, "两张票是两张不同的能力");

    let sa = call(
        &mut a,
        12,
        "side/open",
        serde_json::json!({
            "parentThreadId": "p1", "entryId": entry_a
        }),
    )
    .await;
    let sb = call(
        &mut b,
        13,
        "side/open",
        serde_json::json!({
            "parentThreadId": "p1", "entryId": entry_b
        }),
    )
    .await;
    assert_eq!(
        sa["result"]["side_id"], sb["result"]["side_id"],
        "{sa} {sb}"
    );
    assert_eq!(sa["result"]["generation"], sb["result"]["generation"]);

    // 一端退出:立刻 closing,旧票随后失效。
    let exit = call(
        &mut a,
        14,
        "side/exit",
        serde_json::json!({
            "hostInstanceId": expected, "parentThreadId": "p1", "entryId": entry_a
        }),
    )
    .await;
    assert!(
        matches!(
            exit["result"]["state"].as_str(),
            Some("closing") | Some("closed")
        ),
        "{exit}"
    );

    // 旧票重放 open 必须被拒:它指向的那一代已经不在。
    let stale = call(
        &mut a,
        15,
        "side/open",
        serde_json::json!({
            "parentThreadId": "p1", "entryId": entry_a
        }),
    )
    .await;
    assert!(stale.get("error").is_some(), "旧票不得创建新环境: {stale}");

    a.shutdown().await;
    b.shutdown().await;
}

/// 未知票据是一个稳定的 reason,而不是内部错误。
#[tokio::test]
async fn unknown_entry_ticket_is_entry_expired() {
    let home = tempfile::TempDir::new().unwrap();
    let registry = test_side_registry(home.path(), None);
    let mut h = Harness::with_scope_and_side_registry(Scope::Admin, registry).await;
    initialize(&mut h).await;

    let opened = call(
        &mut h,
        20,
        "side/open",
        serde_json::json!({
            "parentThreadId": "p1", "entryId": "never-issued"
        }),
    )
    .await;
    assert_eq!(
        opened["error"]["data"]["reason"],
        serde_json::json!("entry_expired"),
        "{opened}"
    );
    // exit 用同样的答案:未知票据既不能创建也不能杀任何东西。
    let exited = call(
        &mut h,
        21,
        "side/exit",
        serde_json::json!({
            "hostInstanceId": "whatever", "parentThreadId": "p1", "entryId": "never-issued"
        }),
    )
    .await;
    assert_eq!(
        exited["error"]["data"]["reason"],
        serde_json::json!("entry_expired"),
        "{exited}"
    );

    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// 垂直集成:真实主循环 + 真实旁支注册表 + 可观察 provider
//
// 上面那些用例钉的是协议形状;这一节钉的是 spec §5.1/§5.3 的**链路事实**:
// 父在跑、进度未落盘时旁支就能看到它;旁支的请求里没有工具、也不再写父历史;
// 退出之后本代的 stream/资源槽真的归零,重入拿到的是新快照。
//
// 为什么必须拼到主循环这一层:每一环单独都有单元测试,但把它们串起来会多出
// 「父的快照何时重发布」「哪一份活父表被 resolver 查询」这类只有装配才暴露的
// 接线错误——正是本任务要防的回归。
// ---------------------------------------------------------------------------

use std::sync::Mutex as StdMutex;

use yi_agent_core::provider::{ProviderEvent, ProviderRequest, StopReason, TokenUsage};

/// 记录每一份真正发往 provider 的请求。
///
/// 断言必须落在 `ProviderRequest` 上而不是旁支内部状态:spec §5.3 要求的是
/// 「模型收到的历史里没有它不该有的东西」,内部 session 对不对只是手段。
struct RecordingRequestProvider {
    seen: Arc<StdMutex<Vec<ProviderRequest>>>,
    /// 父与旁支共用一份记录表时,按 system prompt 里的旁支政策区分两者。
    reply: &'static str,
}

#[async_trait::async_trait]
impl yi_agent_core::Provider for RecordingRequestProvider {
    async fn call_stream(
        &self,
        req: ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题生成走同一条 provider;它既不是父回合也不是旁问,记进去会让
        // 「第几条请求是哪一轮」变得依赖时序。
        let is_title = req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS);
        if !is_title {
            self.seen
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(req);
        }
        Ok(Box::pin(futures::stream::iter(vec![
            ProviderEvent::TextDelta(self.reply.to_string()),
            ProviderEvent::Usage(TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_tokens: None,
            }),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ])))
    }
}

/// 一份旁支请求的判据:它带着旁支政策,而不是父的系统提示。
fn is_side_request(req: &ProviderRequest) -> bool {
    req.system
        .as_deref()
        .is_some_and(|system| system.contains(yi_agent_core::SIDE_SYSTEM_POLICY))
}

fn rendered(req: &ProviderRequest) -> String {
    serde_json::to_string(
        &req.messages
            .iter()
            .map(|m| (m.role, &m.content))
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

/// 父侧的假工具:只为了让「旁支请求里工具集为空」这条断言不是空集对空集。
struct RecordingParentTool;

#[async_trait::async_trait]
impl yi_agent_core::Tool for RecordingParentTool {
    fn name(&self) -> &str {
        "parent_tool"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }
    fn description(&self) -> &str {
        "parent-only tool"
    }
    async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
        yi_agent_core::ToolResult::text("parent tool ran")
    }
}

/// 带一个父工具的装配工厂。父会话因此有真实工具,旁支仍然必须是零工具。
fn build_recording_parent(
    provider: Arc<dyn yi_agent_core::Provider>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &std::path::Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<crate::server::BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, _cwd, _mode| {
        let mut registry = yi_agent_core::ToolRegistry::new();
        registry.register(Arc::new(RecordingParentTool));
        let config = yi_agent_core::AgentConfig {
            model: crate::test_support::TEST_MODEL.to_string(),
            ..yi_agent_core::AgentConfig::default()
        };
        let mut agent =
            yi_agent_core::Agent::new(Arc::clone(&provider), Arc::new(registry), config.clone());
        if let Some(session) = session {
            crate::server::turn::apply_session(&mut agent, Some(session));
        }
        Ok(crate::server::BuiltAgent {
            agent,
            provider: Arc::clone(&provider),
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            mcp: None,
            sandbox: yi_agent_tools::SandboxController::new(
                yi_agent_core::autonomy::YoloSwitch::new(false),
                yi_agent_tools::SandboxMode::WorkspaceWrite,
                true,
            ),
        })
    }
}

/// 父/旁支共用的最近一条 user 文本,用来判断「这一轮问的是什么」。
fn last_user_text(req: &ProviderRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| m.role == yi_agent_core::message::Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    yi_agent_core::message::ContentBlock::Text(text) => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// 等一帧满足条件的通知;超时即失败。用于等 `side/turnCompleted`。
async fn wait_side_turn_completed(h: &mut Harness, side_id: &str) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "side/turnCompleted 没有在预算内到达");
        let Some(v) = h.try_read_value(left.min(Duration::from_secs(5))).await else {
            continue;
        };
        if v.get("method").and_then(|m| m.as_str()) == Some("side/turnCompleted")
            && v["params"]["side_id"].as_str() == Some(side_id)
        {
            return v;
        }
    }
}

/// 一条完整的真实链路:父在跑且进度只在内存里 → 旁支看得到它,但看不到之后
/// 新增的进展;旁支请求零工具;父历史逐字节不变;退出后本代资源归零,重入是
/// 新快照。
///
/// 这条用例替代「各自在替身上过一遍」的十几个小断言:它只有把 start / turn /
/// refresh / prepare / open / turnStart / exit 串在**同一个宿主**上才成立。
#[tokio::test(flavor = "multi_thread")]
async fn a_side_sees_the_live_progress_and_dies_on_exit_without_touching_the_parent() {
    use crate::server::tests::read_response;

    let home = tempfile::TempDir::new().unwrap();
    let live = Arc::new(crate::side_conversation::LiveSideParents::new());
    let (registry, resources) =
        crate::server::tests::live_parent_side_registry(home.path(), Arc::clone(&live));
    let host_id = registry.host_instance_id().to_string();
    // 注册表要留一份句柄:验收必须断言「退出之后既没有活代,也没有残留占位」,
    // 而 `side_count`/`tombstone_count` 只在注册表上可读。把它整个 move 进
    // Harness 就只能靠槽位间接推断,那证明不了代次真的被摘掉了。
    let registry_view = Arc::clone(&registry);
    let seen: Arc<StdMutex<Vec<ProviderRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingRequestProvider {
        seen: Arc::clone(&seen),
        reply: "收到",
    });
    let mut h = Harness::with_live_side_registry(
        Scope::Admin,
        registry,
        Arc::clone(&live),
        build_recording_parent(provider),
    )
    .await;
    initialize(&mut h).await;

    // 1. 父会话真的跑起来:开局就发布,于是 `/side` 不依赖磁盘。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let started = read_response(&mut h, 2).await;
    let thread_id = started["result"]["thread_id"]
        .as_str()
        .expect("thread/start returns an id")
        .to_string();
    assert!(
        live.get(&thread_id).is_some(),
        "thread/start 之后必须立刻是活跃父:旁支只能从它取快照"
    );

    // 2. 父跑第一轮:进度 A 进的是内存 session,此刻还没有任何旁支。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "进度A:已经改完 side_driver"}],
            }
        })
        .to_string(),
    )
    .await;
    let _ = read_response(&mut h, 3).await;
    let parent_turn = h.read_until_turn_completed().await;
    // 旁支的存在与退出都不得动父那一轮。父的回合状态是 `completed`,
    // 没有任何一次 `turn/interrupt` 落到它身上:计数为 0 的证据就是它自己
    // 正常跑完,而不是被打断后仍报完成。
    assert_eq!(
        parent_turn["params"]["status"],
        serde_json::json!("completed"),
        "旁支不得打断父回合: {parent_turn}"
    );
    // 父请求带着真实工具集:旁支那条 `tools==[]` 才有对照物。
    let parent_requests: Vec<ProviderRequest> = {
        let guard = seen.lock().unwrap();
        guard
            .iter()
            .filter(|r| !is_side_request(r))
            .cloned()
            .collect()
    };
    assert!(
        parent_requests
            .iter()
            .any(|r| !r.tools.is_empty() && last_user_text(r).contains("进度A")),
        "父回合必须带着它自己的工具集跑过一次: {parent_requests:?}"
    );

    // 3. prepare → open:票据是唯一入口。
    let prepared = call(
        &mut h,
        4,
        "side/prepare",
        serde_json::json!({"parentThreadId": thread_id}),
    )
    .await;
    let entry_id = prepared["result"]["entryId"]
        .as_str()
        .expect("prepare issues a ticket")
        .to_string();
    let opened = call(
        &mut h,
        5,
        "side/open",
        serde_json::json!({"parentThreadId": thread_id, "entryId": entry_id}),
    )
    .await;
    let side_id = opened["result"]["side_id"]
        .as_str()
        .unwrap_or_else(|| panic!("open must return a side: {opened}"))
        .to_string();

    // 4. 旁支首问。命中快照里的 A,且请求里没有任何工具。
    let first = call(
        &mut h,
        6,
        "side/turnStart",
        serde_json::json!({
            "sideId": side_id,
            "text": "现在做到哪一步了?",
            "clientRequestId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert!(first.get("result").is_some(), "旁支必须受理首问: {first}");
    let _ = wait_side_turn_completed(&mut h, &side_id).await;

    let first_side = {
        let guard = seen.lock().unwrap();
        guard
            .iter()
            .filter(|r| is_side_request(r))
            .find(|r| last_user_text(r).contains("现在做到哪一步了?"))
            .cloned()
            .expect("旁支首问必须真的发到 provider")
    };
    assert!(
        first_side.tools.is_empty(),
        "旁支请求不得携带任何工具: {:?}",
        first_side
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        rendered(&first_side).contains("进度A"),
        "旁支必须看到父未落盘的进度 A: {}",
        rendered(&first_side)
    );

    // 5. 父继续:新增 B。已开的旁支**不跟随**。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "turn/start",
            "params": {
                "threadId": thread_id,
                "input": [{"type": "text", "text": "进度B:顺手修了 help 文案"}],
            }
        })
        .to_string(),
    )
    .await;
    let _ = read_response(&mut h, 7).await;
    let parent_turn_b = h.read_until_turn_completed().await;
    assert_eq!(
        parent_turn_b["params"]["status"],
        serde_json::json!("completed"),
        "旁支两轮问答之后父那一轮仍须是正常完成: {parent_turn_b}"
    );

    // 6. 父历史:边问边比,旁支不能改父一个字。
    let parent_after_b = live
        .get(&thread_id)
        .expect("父仍在驱动中")
        .snapshot()
        .await
        .expect("父快照可用");
    let parent_before_side = serde_json::to_string(
        &parent_after_b
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messages()
            .to_vec(),
    )
    .unwrap();
    assert!(
        parent_before_side.contains("进度B"),
        "父的 B 必须已进入它自己的快照: {parent_before_side}"
    );

    let second = call(
        &mut h,
        8,
        "side/turnStart",
        serde_json::json!({
            "sideId": side_id,
            "text": "那 B 呢?",
            "clientRequestId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert!(second.get("result").is_some(), "{second}");
    let _ = wait_side_turn_completed(&mut h, &side_id).await;

    let second_side = {
        let guard = seen.lock().unwrap();
        guard
            .iter()
            .filter(|r| is_side_request(r))
            .find(|r| last_user_text(r).contains("那 B 呢?"))
            .cloned()
            .expect("旁支第二问必须真的发到 provider")
    };
    assert!(
        !rendered(&second_side).contains("进度B"),
        "同一代旁支不得看到进入之后的新进展: {}",
        rendered(&second_side)
    );
    assert!(
        second_side.tools.is_empty(),
        "第二问同样零工具: {:?}",
        second_side
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>()
    );

    let parent_after_side = serde_json::to_string(
        &live
            .get(&thread_id)
            .expect("父仍在驱动中")
            .snapshot()
            .await
            .expect("父快照可用")
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .messages()
            .to_vec(),
    )
    .unwrap();
    assert_eq!(
        parent_before_side, parent_after_side,
        "旁支的两轮问答不得改动父历史(逐字节)"
    );

    // 7. 退出:同义于 /back,销毁本代。必须先等到 Closed 才谈重入。
    let exited = call(
        &mut h,
        9,
        "side/exit",
        serde_json::json!({
            "hostInstanceId": host_id,
            "parentThreadId": thread_id,
            "entryId": entry_id,
        }),
    )
    .await;
    let exit_state = exited["result"]["state"].as_str().map(str::to_string);
    assert!(
        matches!(exit_state.as_deref(), Some("closing") | Some("closed")),
        "{exited}"
    );

    // `closed` 之前不得放行第二环境。
    //
    // 这条在这里只做**观察**,不下断言:探测用的 open 一旦成功,它自己就是一条
    // 新的活代,会占掉资源槽,把这个用例后面对「槽位归零」的验收污染成假的。
    // 确定性证明在 registry 的单元测试里——`exit_during_creation_cancels_before_the_question`
    // 用门控 resolver 把构造卡住,于是「closing 期间 open 必被拒」不依赖时序。
    if exit_state.as_deref() == Some("closing") {
        assert!(
            registry_view.side_count().await <= 1,
            "同一父不得同时存在两代旁支"
        );
    }

    // 本代必须真的从注册表里摘掉:槽位归零只说明资源回收了,一个仍挂在
    // `sides`/`parents` 里的死代会让后续 `side/list` 报一个永远 Creating 的
    // 环境,而那正是「按了退出却没退出去」。排空的第 6 步交回槽位、第 7 步才
    // 摘代次,所以两个条件要一起等。
    wait_for_side_gone(&resources, &registry_view).await;
    // 排空之后不得再有清理任务在跑:槽位归零只说明账平了,残留的清理任务
    // 仍可能持有目录句柄。
    assert_eq!(resources.live_cleanups(), 0, "退出之后不得残留清理任务");

    // 8. 关闭之后不得留下可读的本代内容。
    let read = call(
        &mut h,
        10,
        "side/readItems",
        serde_json::json!({"sideId": side_id}),
    )
    .await;
    let reason = read["error"]["data"]["reason"].as_str().unwrap_or_default();
    assert!(
        matches!(reason, "side_closing" | "side_not_found"),
        "退出之后不得再读到本代正文: {read}"
    );

    // 9. 重入是全新环境:含 B,不含旧旁问。
    let prepared = call(
        &mut h,
        11,
        "side/prepare",
        serde_json::json!({"parentThreadId": thread_id}),
    )
    .await;
    let entry2 = prepared["result"]["entryId"].as_str().unwrap().to_string();
    let reopened = call(
        &mut h,
        12,
        "side/open",
        serde_json::json!({"parentThreadId": thread_id, "entryId": entry2}),
    )
    .await;
    let side2 = reopened["result"]["side_id"]
        .as_str()
        .unwrap_or_else(|| panic!("重入必须成功: {reopened}"))
        .to_string();
    assert_ne!(side2, side_id, "重入必须是新的一代");

    let third = call(
        &mut h,
        13,
        "side/turnStart",
        serde_json::json!({
            "sideId": side2,
            "text": "重新看一下进度",
            "clientRequestId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert!(third.get("result").is_some(), "{third}");
    let _ = wait_side_turn_completed(&mut h, &side2).await;

    let third_side = {
        let guard = seen.lock().unwrap();
        guard
            .iter()
            .filter(|r| is_side_request(r))
            .rfind(|r| last_user_text(r).contains("重新看一下进度"))
            .cloned()
            .expect("重入后的首问必须真的发到 provider")
    };
    assert!(
        rendered(&third_side).contains("进度B"),
        "新快照必须包含父在此期间的进展 B: {}",
        rendered(&third_side)
    );
    assert!(
        !rendered(&third_side).contains("现在做到哪一步了?"),
        "新环境不得继承上一次旁问: {}",
        rendered(&third_side)
    );

    // 10. 退出这一代,把资源槽交回去。
    let _ = call(
        &mut h,
        14,
        "side/exit",
        serde_json::json!({
            "hostInstanceId": host_id,
            "parentThreadId": thread_id,
            "entryId": entry2,
        }),
    )
    .await;
    wait_for_side_gone(&resources, &registry_view).await;
    assert_eq!(
        registry_view.side_count().await,
        0,
        "两代先后退出后不得留下活代"
    );
    assert!(
        registry_view.tombstone_count().await >= 2,
        "两次退出都必须留下墓碑:重放的迟到请求要能拿到确定的答案,而不是被当成从未存在"
    );

    h.shutdown().await;
}

/// 同一宿主的第二端(手机 / 另一台桌面)只能看,不能开、不能问、不能退。
///
/// 这是 spec §7「同父多端」与 §8.1 权限划分合成的验收:第一端 Control 建环境、
/// 提问;第二端 Observe 必须能 `side/list` 与 `side/readItems`(否则它连「有没有
/// 旁支在跑」都看不到),但 `prepare`/`open`/`turnStart`/`exit` 一律在 dispatch
/// 处被拒。最后第一端退出,两端都必须看到本代消失。
#[tokio::test]
async fn a_second_client_reads_the_same_generation_but_cannot_control_it() {
    use crate::server::tests::read_response;

    let home = tempfile::TempDir::new().unwrap();
    let live = Arc::new(crate::side_conversation::LiveSideParents::new());
    let (registry, resources) =
        crate::server::tests::live_parent_side_registry(home.path(), Arc::clone(&live));
    let host_id = registry.host_instance_id().to_string();
    let registry_view = Arc::clone(&registry);
    let seen: Arc<StdMutex<Vec<ProviderRequest>>> = Arc::new(StdMutex::new(Vec::new()));
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingRequestProvider {
        seen: Arc::clone(&seen),
        reply: "收到",
    });
    // 两条连接共用同一个 registry 与同一个活父表:这才是「同一台宿主的两个端」。
    //
    // 第二条连接用 Observe 建:它的 agent 工厂不会被调用(`prepare`/`open` 在
    // dispatch 处就被拒),共享的是同一份宿主资源。
    let mut control = Harness::with_live_side_registry(
        Scope::Control,
        Arc::clone(&registry),
        Arc::clone(&live),
        build_recording_parent(provider),
    )
    .await;
    let mut observe = Harness::with_scope_and_side_registry(Scope::Observe, registry).await;
    initialize(&mut control).await;
    initialize(&mut observe).await;

    // 父在 Control 连接上跑起来,进度 A 只在内存里。
    control
        .send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    let started = read_response(&mut control, 2).await;
    let thread_id = started["result"]["thread_id"].as_str().unwrap().to_string();
    control
        .send(
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 3, "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "进度A:跨端验收"}],
                }
            })
            .to_string(),
        )
        .await;
    let _ = read_response(&mut control, 3).await;
    let _ = control.read_until_turn_completed().await;

    let prepared = call(
        &mut control,
        4,
        "side/prepare",
        serde_json::json!({"parentThreadId": thread_id}),
    )
    .await;
    let entry_id = prepared["result"]["entryId"].as_str().unwrap().to_string();
    let opened = call(
        &mut control,
        5,
        "side/open",
        serde_json::json!({"parentThreadId": thread_id, "entryId": entry_id}),
    )
    .await;
    let side_id = opened["result"]["side_id"]
        .as_str()
        .unwrap_or_else(|| panic!("Control 端必须能建立环境: {opened}"))
        .to_string();

    let asked = call(
        &mut control,
        6,
        "side/turnStart",
        serde_json::json!({
            "sideId": side_id,
            "text": "现在做到哪一步了?",
            "clientRequestId": uuid::Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert!(asked.get("result").is_some(), "{asked}");

    // 等旁支这一轮真的跑完。不能用 `side/turnCompleted` 通知:两条 Harness 各自
    // 起一个 serve,`attach_hub` 是后装覆盖先装,通知只会送到其中一条连接。这是
    // 测试装配的假象(生产里一个进程只有一个 hub),所以这里改用事实证据:
    // provider 收到了那条请求,且注册表把它记成了已受理。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let answered = {
            let guard = seen.lock().unwrap();
            guard
                .iter()
                .filter(|r| is_side_request(r))
                .any(|r| last_user_text(r).contains("现在做到哪一步了?"))
        };
        let accepted = registry_view
            .accepted_requests(&side_id)
            .await
            .map(|table| !table.is_empty())
            .unwrap_or(false);
        if answered && accepted {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "旁支首问没有在预算内被受理"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Observe 端:读得到同一代,读得到正文。
    let listed = call(
        &mut observe,
        10,
        "side/list",
        serde_json::json!({"parentThreadId": thread_id}),
    )
    .await;
    let listed_sides = listed["result"]["sides"]
        .as_array()
        .unwrap_or_else(|| panic!("Observe 必须能列: {listed}"));
    assert_eq!(listed_sides.len(), 1, "{listed}");
    assert_eq!(listed_sides[0]["side_id"], serde_json::json!(side_id));

    let read = call(
        &mut observe,
        11,
        "side/readItems",
        serde_json::json!({"sideId": side_id}),
    )
    .await;
    assert!(
        read.get("result").is_some(),
        "Observe 必须能读到同一代正文: {read}"
    );
    let body = serde_json::to_string(&read["result"]["items"]).unwrap();
    assert!(body.contains("现在做到哪一步了?"), "{body}");

    // Observe 端:任何 Control 动作都必须在 dispatch 处被拒(-32014),
    // 而不是走到注册表里再报一个「找不到」——那会让客户端以为旁支消失了。
    let refused = [
        (
            "side/prepare",
            serde_json::json!({"parentThreadId": thread_id}),
        ),
        (
            "side/open",
            serde_json::json!({"parentThreadId": thread_id, "entryId": entry_id}),
        ),
        (
            "side/turnStart",
            serde_json::json!({
                "sideId": side_id,
                "text": "越权提问",
                "clientRequestId": uuid::Uuid::new_v4().to_string(),
            }),
        ),
        (
            "side/exit",
            serde_json::json!({
                "hostInstanceId": host_id,
                "parentThreadId": thread_id,
                "entryId": entry_id,
            }),
        ),
    ];
    for (offset, (method, params)) in refused.into_iter().enumerate() {
        let v = call(&mut observe, 20 + offset as u64, method, params).await;
        assert_eq!(
            v["error"]["code"],
            serde_json::json!(-32014),
            "{method} 必须在 Observe 端被拒: {v}"
        );
    }
    // 越权尝试不得改变本代:它还活着,还能继续答。
    assert_eq!(registry_view.side_count().await, 1, "越权请求不得动到本代");

    // Control 端退出:两端看到的本代都必须消失。
    let exited = call(
        &mut control,
        30,
        "side/exit",
        serde_json::json!({
            "hostInstanceId": host_id,
            "parentThreadId": thread_id,
            "entryId": entry_id,
        }),
    )
    .await;
    assert!(
        matches!(
            exited["result"]["state"].as_str(),
            Some("closing") | Some("closed")
        ),
        "{exited}"
    );
    wait_for_side_gone(&resources, &registry_view).await;
    assert_eq!(registry_view.side_count().await, 0);

    for (h, name) in [(&mut control, "Control"), (&mut observe, "Observe")] {
        let listed = call(
            h,
            40,
            "side/list",
            serde_json::json!({"parentThreadId": thread_id}),
        )
        .await;
        assert_eq!(
            listed["result"]["sides"],
            serde_json::json!([]),
            "{name} 端不得再看到已退出的本代: {listed}"
        );
        let read = call(
            h,
            41,
            "side/readItems",
            serde_json::json!({"sideId": side_id}),
        )
        .await;
        let reason = read["error"]["data"]["reason"].as_str().unwrap_or_default();
        assert!(
            matches!(reason, "side_closing" | "side_not_found"),
            "{name} 端退出后不得再读到正文: {read}"
        );
    }

    control.shutdown().await;
    observe.shutdown().await;
}

/// 等「槽位归零且代次摘掉」同时成立。
///
/// 排空不是一次原子的:第 6 步交回资源槽,第 7 步才从 `sides`/`parents` 摘代次
/// 并写墓碑。只等槽位会落在这两步之间——单独跑常常撞不上,并发跑就稳定变红。
/// 验收要的是「退出真的完成」,所以两个条件都要等。
async fn wait_for_side_gone(
    resources: &Arc<yi_agent_runtime::side_conversation::SideHostResources>,
    registry: &Arc<crate::side_conversation::SideRegistry>,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if resources.slots_in_use() == 0 && registry.side_count().await == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "退出之后必须同时满足槽位归零与代次摘除,实际槽位 {} 个、活代 {} 个",
        resources.slots_in_use(),
        registry.side_count().await
    );
}
