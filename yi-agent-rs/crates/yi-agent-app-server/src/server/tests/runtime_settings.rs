//! `settings/read`、`settings/write` 与 `settings/changed` 走真实 dispatcher。
//!
//! 单元测试（`settings_rpc`）只证明 view/receipt 的*形状*；这里证明*路由与广播*：
//! 写请求经共享实例提交（而不是无状态的 path facade），成功后真的广播了一条全局帧，
//! 失败时既不落盘也不广播。这两半任一缺失都不会让单元测试变红。
//!
//! 关键约束（来自 spec）：通知**不得携带设置取值**，`changed_keys` 为空也合法
//! （表示"记录可能变了但没有任何逻辑设置变化"），且提交成功不能因为广播失败而
//! 变成"未保存"。

use super::{Harness, initialize};
use yi_agent_runtime::live_settings::LiveSettings;
use yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding;
use yi_agent_runtime::live_settings::sources::FrozenRuntimeSources;

/// A binding over an isolated `settings.json`.
///
/// `FrozenRuntimeSources::default()` is the "no CLI/env override" host context, so
/// nothing shadows the file values: the responses below then describe exactly what
/// was saved, not what the test runner's environment happens to hold.
fn binding(dir: &tempfile::TempDir) -> RuntimeSettingsBinding {
    RuntimeSettingsBinding {
        live: LiveSettings::shared(dir.path().join("settings.json")),
        sources: std::sync::Arc::new(FrozenRuntimeSources::default()),
    }
}

async fn with_live() -> (Harness, tempfile::TempDir, RuntimeSettingsBinding) {
    let dir = tempfile::TempDir::new().unwrap();
    let binding = binding(&dir);
    let mut h = Harness::with_live_settings(binding.clone()).await;
    initialize(&mut h).await;
    (h, dir, binding)
}

/// Send one request and read the frame carrying its id.
async fn ask(
    h: &mut Harness,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    h.send(&serde_json::to_string(&frame).unwrap()).await;
    h.read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
        .await
}

/// Read the next matching frame, but fail if it does not arrive within `within`.
///
/// `next_notification` above waits `EVENT_WAIT` (30s), which is longer than the
/// power ticker (10s). Anything the power tick happens to refresh would then
/// pass a "did the observer publish this?" assertion. A bound below 10s is what
/// makes those two paths distinguishable.
async fn next_notification_within(
    h: &mut Harness,
    method: &str,
    within: std::time::Duration,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            panic!("no {method} frame within {within:?}");
        }
        let Some(frame) = h
            .try_read_value(left.min(std::time::Duration::from_millis(200)))
            .await
        else {
            continue;
        };
        if frame.get("method") == Some(&serde_json::json!(method)) {
            return frame;
        }
    }
}

/// Read the next frame whose `method` matches, ignoring interleaved responses.
async fn next_notification(h: &mut Harness, method: &str) -> serde_json::Value {
    h.read_matching(64, |v| v.get("method") == Some(&serde_json::json!(method)))
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_read_reports_capability_origin_and_health() {
    let (mut h, _dir, _binding) = with_live().await;

    let value = ask(&mut h, 10, "settings/read", serde_json::json!({})).await;
    let result = value.get("result").expect("read is an answer");

    assert_eq!(result["live_apply_version"], 1);
    assert_eq!(result["snapshot_origin"], "current");
    assert_eq!(result["health"]["state"], "healthy");
    assert!(result["revision"].is_string());
    // 旧形状必须仍在：老客户端按 `fields[].key/value` 渲染。
    assert!(result["path"].is_string());
    let fields = result["fields"].as_array().expect("fields array");
    assert!(!fields.is_empty());
    for field in fields {
        assert!(field["key"].is_string(), "{field}");
        assert!(
            field["value"].is_null() || field["value"].is_string(),
            "{field}"
        );
    }

    h.shutdown().await;
}

/// 没有 live binding 的宿主（旧装配）仍返回历史形状，且不带任何新字段——
/// 我们不希望老客户端因为多出的键而改行为，也不希望它误以为支持热更新。
#[tokio::test(flavor = "multi_thread")]
async fn a_host_without_live_settings_keeps_the_historic_read_shape() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let value = ask(&mut h, 11, "settings/read", serde_json::json!({})).await;
    let result = value.get("result").expect("read is an answer");
    assert!(result["fields"].is_array());
    assert!(result["path"].is_string());
    assert!(result.get("live_apply_version").is_none(), "{result}");
    assert!(result.get("health").is_none(), "{result}");

    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_successful_write_reports_its_receipt_and_broadcasts_the_key() {
    let (mut h, _dir, binding) = with_live().await;

    let value = ask(
        &mut h,
        12,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 41}}),
    )
    .await;
    let receipt = value.get("result").expect("the write succeeded");
    assert_eq!(receipt["ok"], true);
    assert_eq!(
        receipt["changed_keys"],
        serde_json::json!(["agent.max_turns"])
    );
    assert_eq!(
        receipt["apply_policy"]["agent.max_turns"],
        "next_safe_point"
    );
    assert!(receipt["revision"].is_string());

    // 响应之后必有一条同名通知，且键集合与 receipt 一致。
    let note = next_notification(&mut h, "settings/changed").await;
    assert_eq!(
        note["params"]["changed_keys"],
        serde_json::json!(["agent.max_turns"])
    );
    assert_eq!(note["params"]["revision"], receipt["revision"]);
    // 全局帧：没有 thread 键，桌面端所有会话都会收到。
    assert!(note["params"].get("threadId").is_none(), "{note}");

    // 提交真的落到了共享实例，而不只是回了一个 receipt。
    let view = binding.live.refresh();
    assert_eq!(view.settings.agent.max_turns, Some(41));

    h.shutdown().await;
}

/// 通知里不得出现任何设置取值。提示词属于用户内容，会进日志与转发通道。
#[tokio::test(flavor = "multi_thread")]
async fn the_change_notification_carries_no_setting_values() {
    let (mut h, _dir, _binding) = with_live().await;

    ask(
        &mut h,
        13,
        "settings/write",
        serde_json::json!({"patch": {"agent.system_prompt": "PROMPT-SECRET-42"}}),
    )
    .await;
    let note = next_notification(&mut h, "settings/changed").await;
    let rendered = note.to_string();
    assert!(
        !rendered.contains("PROMPT-SECRET-42"),
        "notification leaked a value: {rendered}"
    );

    h.shutdown().await;
}

/// 非法 patch 整批拒绝：不落盘、不改 revision、不广播。
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_patch_writes_nothing_and_broadcasts_nothing() {
    let (mut h, _dir, binding) = with_live().await;

    // 先写一个合法基线，之后用它判断 revision 是否被动过。
    ask(
        &mut h,
        14,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 5}}),
    )
    .await;
    let _ = next_notification(&mut h, "settings/changed").await;
    let before = binding.live.refresh().revision.clone();

    let value = ask(
        &mut h,
        15,
        "settings/write",
        serde_json::json!({"patch": {
            "agent.max_turns": 6,
            "agent.max_direct_children": -1
        }}),
    )
    .await;
    let error = value.get("error").expect("invalid patch must be rejected");
    assert_eq!(error["data"]["code"], "invalid_setting", "{value}");
    assert_eq!(error["data"]["key"], "agent.max_direct_children", "{value}");

    // 同批次的合法键也不得落地：校验是整批的。
    assert_eq!(binding.live.refresh().revision, before);
    assert_eq!(binding.live.refresh().settings.agent.max_turns, Some(5));

    // 且**没有** settings/changed。用一个紧随其后的合法读请求当"栅栏"：
    // 主循环按序处理请求，读到它的响应就说明前面那条失败请求已经处理完毕，
    // 若它要广播，帧必已先于这条响应入队。
    let probe = ask(&mut h, 16, "settings/read", serde_json::json!({})).await;
    assert!(probe.get("result").is_some(), "{probe}");
    let slack = std::time::Duration::from_millis(200);
    while let Some(frame) = h.try_read_value(slack).await {
        assert!(
            frame.get("method") != Some(&serde_json::json!("settings/changed")),
            "a rejected write must not notify: {frame}"
        );
    }

    h.shutdown().await;
}

/// `changed_keys` 为空也发通知：它表示"记录可能变了（health / 元数据），但没有任何
/// 逻辑设置变化"，客户端据此重新 read 而不必重同步草稿。
#[tokio::test(flavor = "multi_thread")]
async fn an_empty_change_set_still_invalidates_the_view() {
    let (mut h, _dir, _binding) = with_live().await;

    // 先落一次，把"缺失"变成"显式 200"（这本身是一次逻辑变化）。
    ask(
        &mut h,
        17,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 200}}),
    )
    .await;
    let first = next_notification(&mut h, "settings/changed").await;
    assert_eq!(
        first["params"]["changed_keys"],
        serde_json::json!(["agent.max_turns"])
    );

    // 再提交同一个值：文档可能被重写（时间戳/格式），但没有任何逻辑设置变化。
    // 这条通知的唯一作用是让客户端重新 read，而不是去重同步草稿。
    ask(
        &mut h,
        19,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 200}}),
    )
    .await;
    let note = next_notification(&mut h, "settings/changed").await;
    assert_eq!(note["params"]["changed_keys"], serde_json::json!([]));
    assert!(note["params"]["revision"].is_string());

    h.shutdown().await;
}

/// 响应必须**先**于 `settings/changed` 入队。
///
/// 这是「提交成功不会因为广播失败而变成错误」的可判别形式：`write_notification`
/// 对 load-bearing 帧（`Delivery::Global` 属于此类）本来就会**丢弃** `Closed`
/// 错误（见 `server/rpc_io.rs`），所以"广播失败"唯一现实的后果是"这一帧没送到"。
/// 只要响应先入队，客户端拿到的就是"已保存 + 请刷新"，重试也就不会被诱发。
///
/// 变异验证：把广播移到响应之前，本用例变红（先读到 `settings/changed`）。
#[tokio::test(flavor = "multi_thread")]
async fn the_response_is_queued_before_the_change_notification() {
    let (mut h, _dir, binding) = with_live().await;

    // 一个已死的 reliable 客户端：确保即使通知投递失败，也不会改变响应结果。
    let dead = crate::broadcast::ClientId::ws(uuid::Uuid::new_v4());
    drop(h.hub.register_reliable(dead));

    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 18,
            "method": "settings/write",
            "params": {"patch": {"compact.ratio": 55}},
        })
        .to_string(),
    )
    .await;

    // 按到达顺序记录：先到的那一帧就是先入队的。不做「等到两帧都齐」的循环——
    // 顺序错时那条通知会被当成无关帧丢掉，随后等响应只会超时，报错看不出真因。
    let mut order = Vec::new();
    for _ in 0..64 {
        let frame = h.read_value().await;
        if frame.get("id") == Some(&serde_json::json!(18)) {
            assert!(frame.get("result").is_some(), "{frame}");
            order.push("response");
            break;
        }
        if frame.get("method") == Some(&serde_json::json!("settings/changed")) {
            order.push("notification");
        }
    }
    assert_eq!(
        order,
        vec!["response"],
        "the response must be queued before the change notification"
    );

    // 提交确实落地。
    assert_eq!(binding.live.refresh().settings.compact.ratio, Some(55));

    h.shutdown().await;
}

/// `config/read` 在 live 宿主上把本 spec 的运行标量换成**当前**宿主有效值，
/// 并标注每个值的来源；其余字段（模型、权限、派生阈值）保持原样。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_restates_live_tunables_and_their_sources() {
    let (mut h, _dir, _binding) = with_live().await;

    // 保存之后，卡片必须看到新值——而不是 sidecar 启动时的快照。
    ask(
        &mut h,
        20,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 123}}),
    )
    .await;

    let value = ask(&mut h, 21, "config/read", serde_json::json!({})).await;
    let view = value.get("result").expect("config/read is an answer");

    assert_eq!(view["max_turns"], 123);
    let live = view.get("live_apply").expect("the live block is present");
    assert_eq!(live["live_apply_version"], 1);
    assert_eq!(live["snapshot_origin"], "current");
    assert_eq!(live["health"]["state"], "healthy");
    assert_eq!(live["sources"]["max_turns"], "settings");
    // 不在本 spec 的运行标量里：派生阈值与权限/模型不得被宿主默认改写。
    assert!(live["sources"].get("compact_threshold").is_none(), "{live}");
    assert!(live["sources"].get("sandbox").is_none(), "{live}");
    assert!(live["sources"].get("model").is_none(), "{live}");

    h.shutdown().await;
}

/// 被显式启动来源遮蔽时，卡片要显示覆盖来源，让用户明白"保存了但当前不生效"。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_reports_a_host_override_source() {
    let dir = tempfile::TempDir::new().unwrap();
    let binding = RuntimeSettingsBinding {
        live: LiveSettings::shared(dir.path().join("settings.json")),
        sources: std::sync::Arc::new(FrozenRuntimeSources::from_pairs(&[(
            "agent.max_turns",
            serde_json::json!(7),
            yi_agent_runtime::live_settings::sources::ValueSource::Override,
        )])),
    };
    let mut h = Harness::with_live_settings(binding).await;
    initialize(&mut h).await;

    let value = ask(&mut h, 22, "config/read", serde_json::json!({})).await;
    let view = value.get("result").unwrap();
    assert_eq!(view["max_turns"], 7);
    assert_eq!(view["live_apply"]["sources"]["max_turns"], "override");

    h.shutdown().await;
}

/// 无 live binding 时不插 `live_apply`：旧客户端的视图逐字节不变。
#[tokio::test(flavor = "multi_thread")]
async fn config_read_without_a_binding_keeps_its_historic_shape() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let value = ask(&mut h, 23, "config/read", serde_json::json!({})).await;
    let view = value.get("result").expect("an answer");
    assert!(view.get("live_apply").is_none(), "{view}");
    assert!(view.get("max_turns").is_some(), "{view}");

    h.shutdown().await;
}

/// 另一个进程改了 `settings.json`：观察器在下个周期看到它，宿主广播
/// `settings/changed`，客户端据此重新 read。
///
/// 这条不能靠「本进程的写请求」冒充：写请求自己会广播，即使跨进程观察完全没
/// 接线也照样绿。所以这里**只**改文件，不经任何 RPC。
#[tokio::test(flavor = "multi_thread")]
async fn an_external_edit_reaches_clients_through_the_observer() {
    let (mut h, dir, binding) = with_live().await;
    let path = dir.path().join("settings.json");

    // 先落一个基线并经 RPC 读一次。这次读本身可能已经触发一条失效帧
    // （服务启动时还没发布过快照），所以把它收掉，后面的断言才有唯一对应的
    // 变化——否则「收到第一条通知」可能属于基线而不是被测的那次编辑。
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"agent": {"max_turns": 5}})).unwrap(),
    )
    .unwrap();
    let _ = ask(&mut h, 30, "settings/read", serde_json::json!({})).await;
    let baseline = next_notification(&mut h, "settings/changed").await;
    let baseline_revision = baseline["params"]["revision"].clone();

    // 外部替换（模拟另一个进程保存）。只有一个字节变了：5 → 6。
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"agent": {"max_turns": 6}})).unwrap(),
    )
    .unwrap();

    // 等到一条属于新版本的帧，而不是「下一条恰好到达的帧」。
    //
    // 上限 4 秒，**故意短于 10 秒的保活拍**：那条拍也会 `refresh()` 并触发同一条
    // 外部信号，若这里等 30 秒，"观察器没接线" 也会因为保活拍而通过（实测：变异成
    // 不启动观察器后，本用例在 10.13s 绿）。4 秒只可能由观察器周期（1 秒）满足。
    let mut note = next_notification_within(
        &mut h,
        "settings/changed",
        std::time::Duration::from_secs(4),
    )
    .await;
    while note["params"]["revision"] == baseline_revision {
        note = next_notification_within(
            &mut h,
            "settings/changed",
            std::time::Duration::from_secs(2),
        )
        .await;
    }
    assert_eq!(
        note["params"]["changed_keys"],
        serde_json::json!(["agent.max_turns"]),
        "{note}"
    );
    assert!(note["params"]["revision"].is_string(), "{note}");
    // 只允许出现键名。值本身不许出现在帧里——注意不能拿单个字符判，
    // `revision` 是哈希，本来就可能含 `6`。
    let rendered = note.to_string();
    assert!(
        !rendered.contains("\"agent\"") && !rendered.contains(":6"),
        "the notification must not carry values: {note}"
    );

    // 观察器确实发布了新值（而不只是广播了一条空帧）。
    assert_eq!(
        binding.live.published().map(|s| s.settings.agent.max_turns),
        Some(Some(6))
    );

    h.shutdown().await;
}

/// A health-only change must still reach clients, with an empty key set.
///
/// Breaking the file keeps the last good content, so `revision` and every setting
/// value are unchanged -- the only difference is `health`. A notification keyed
/// on the revision alone would drop this frame, and a client would keep showing
/// "healthy" for a file that no longer parses. `changed_keys: []` is exactly the
/// legal shape for "re-read and look at health".
#[tokio::test(flavor = "multi_thread")]
async fn a_health_regression_is_broadcast_even_with_no_changed_keys() {
    let (mut h, dir, binding) = with_live().await;
    let path = dir.path().join("settings.json");

    // Baseline: a valid file, read once, and its frame consumed.
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"agent": {"max_turns": 5}})).unwrap(),
    )
    .unwrap();
    let _ = ask(&mut h, 40, "settings/read", serde_json::json!({})).await;
    let baseline = next_notification(&mut h, "settings/changed").await;
    let healthy_revision = baseline["params"]["revision"].clone();

    // Now corrupt it. Same settings, same revision: only health moves.
    std::fs::write(&path, b"{ this is not json").unwrap();

    let mut note = next_notification_within(
        &mut h,
        "settings/changed",
        std::time::Duration::from_secs(4),
    )
    .await;
    // 保活拍也会读盘，所以只认「健康确实降级」之后的那一帧。
    while note["params"]["revision"] != healthy_revision {
        note = next_notification_within(
            &mut h,
            "settings/changed",
            std::time::Duration::from_secs(2),
        )
        .await;
    }
    assert_eq!(
        note["params"]["changed_keys"],
        serde_json::json!([]),
        "a health-only change carries no logical keys: {note}"
    );
    assert_eq!(
        note["params"]["revision"], healthy_revision,
        "the revision must not move when the content is unchanged: {note}"
    );

    // 发布出来的快照确实是降级态，而不是我们只看了一条空帧。
    let published = binding.live.published().expect("the observer published");
    assert_eq!(
        published.health.state,
        yi_agent_runtime::live_settings::HealthState::Degraded
    );
    assert_eq!(published.settings.agent.max_turns, Some(5));

    h.shutdown().await;
}

/// The pump's start-up handshake must be baseline → subscribe → catch up.
///
/// Both naive orders have a real window, so the ordering is the guarantee and a
/// source assertion is what can pin it:
///
/// * subscribe-then-baseline: the baseline read already returns the new content,
///   so when `changed()` fires the comparison says "no difference" and the frame
///   is dropped.
/// * baseline-then-subscribe without the catch-up: `refresh()` publishes *then*
///   notifies, so a publish that lands before the subscription is invisible to
///   `changed()` forever.
///
/// Timing that window from a test would need to suspend the task between two
/// adjacent statements, which no public API allows; asserting the sequence reads
/// the actual guarantee instead of a proxy for it.
#[test]
fn the_notification_pump_subscribes_between_baseline_and_catch_up() {
    let source = include_str!("../transport/notifications.rs");
    let body = source
        .split("pub(crate) async fn pump_settings_notifications(")
        .nth(1)
        .and_then(|rest| rest.split("\n}\n").next())
        .expect("pump_settings_notifications must exist");
    let baseline = body
        .find("let mut last = live.published()")
        .expect("the pump must take a baseline before subscribing");
    let subscribe = body
        .find("let mut changes = live.subscribe_external()")
        .expect("the pump must subscribe to external changes");
    let catch_up = body
        .find("// 补齐")
        .expect("the pump must catch up on content published before its subscription");
    let pump_loop = body.find("loop {").expect("the pump must loop");
    assert!(
        baseline < subscribe,
        "the baseline must be read before subscribing, otherwise the first wake-up \
         has nothing to compare against"
    );
    assert!(
        subscribe < catch_up && catch_up < pump_loop,
        "the catch-up must run after subscribing and before the loop: it is what \
         recovers a publish that landed inside the window"
    );
    assert!(
        !body[..subscribe].contains("changes.changed()"),
        "no frame may be emitted before the baseline exists"
    );
}

/// 本地 `settings/write` 只广播一次：观察器读到同一版本时不得再播一遍。
///
/// 两个独立通道（本地提交自己播、外部观察另外播）很容易写成双播——用户在设置页
/// 每保存一次就收到两条同版本帧。这里按顺序取两帧：第二帧若是同版本
/// `settings/changed` 就变红。
#[tokio::test(flavor = "multi_thread")]
async fn a_local_write_is_not_broadcast_twice() {
    let (mut h, _dir, _binding) = with_live().await;

    ask(
        &mut h,
        31,
        "settings/write",
        serde_json::json!({"patch": {"agent.max_turns": 42}}),
    )
    .await;
    let first = next_notification(&mut h, "settings/changed").await;
    let first_revision = first["params"]["revision"].clone();

    // 观察器周期是 1 秒：给足两轮，任何重复广播都必然落在这个窗口里。
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2500);
    while std::time::Instant::now() < deadline {
        match h
            .try_read_value(std::time::Duration::from_millis(150))
            .await
        {
            Some(frame) => {
                if frame.get("method") == Some(&serde_json::json!("settings/changed")) {
                    panic!("the same commit was broadcast twice: {frame}");
                }
            }
            None => continue,
        }
    }
    // 第一帧仍然是那次提交。
    assert_eq!(
        first["params"]["changed_keys"],
        serde_json::json!(["agent.max_turns"])
    );
    assert!(first_revision.is_string());

    h.shutdown().await;
}
