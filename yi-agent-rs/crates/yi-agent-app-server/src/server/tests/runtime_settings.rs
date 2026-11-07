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
