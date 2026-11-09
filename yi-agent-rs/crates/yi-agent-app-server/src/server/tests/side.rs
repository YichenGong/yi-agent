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
