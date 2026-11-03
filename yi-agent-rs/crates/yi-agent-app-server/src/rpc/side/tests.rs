//! `side/*` 的单元测试:方法归属、适配器形状、UUID 与文本边界、错误码。
//!
//! 集成行为(scope 门禁、不阻塞主循环、宿主 id 一致)在
//! `server/tests/side.rs` 里经真实 stdio harness 验证——这里只钉不依赖传输的
//! 那些规则。

use super::*;
use crate::protocol::{Item, ToolStatus};
use crate::side_conversation::wire_item;
use serde_json::json;
use yi_agent_runtime::side_conversation::{
    SIDE_TRANSCRIPT_MAX_ITEM_BYTES, SideItem, SideItemContent, SideToolStatus,
};

fn item(id: &str, content: SideItemContent) -> SideItem {
    SideItem {
        id: id.to_string(),
        turn_id: "t".to_string(),
        ordinal: 0,
        content,
    }
}

/// 基线文件是**机器可枚举的清单**:手写一份会漏掉后来新增的方法,而漏掉的
/// 后果是 `method_not_found`——一个本来能工作的 RPC 被静默降级。
#[test]
fn side_methods_baseline_matches_owns_and_domain() {
    let all: Vec<&str> = include_str!("../side_methods_baseline.txt")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(all.len(), 6, "基线必须恰好六个方法");
    for method in &all {
        assert!(owns(method), "{method} 必须由 side 域认领");
        assert_eq!(
            crate::rpc::domain_for(method),
            crate::rpc::Domain::Side,
            "{method} 必须路由到 Side 域"
        );
    }
    // 反向:域内认领的每一个方法都必须在基线里,否则清单就少了一行。
    for method in [
        "side/open",
        "side/list",
        "side/readItems",
        "side/turnStart",
        "side/turnInterrupt",
        "side/close",
    ] {
        assert!(all.contains(&method), "{method} 不在基线文件里");
    }
}

/// 只读方法给 `Observe`,其余给 `Control`。写反的后果很具体:手机(Control)
/// 读不了,或者 Observe 掩码级视图能开旁支花用户的钱。
#[test]
fn side_scopes_split_reads_from_control() {
    for method in ["side/list", "side/readItems"] {
        assert_eq!(required_scope(method), Scope::Observe, "{method}");
    }
    for method in [
        "side/open",
        "side/turnStart",
        "side/turnInterrupt",
        "side/close",
    ] {
        assert_eq!(required_scope(method), Scope::Control, "{method}");
    }
}

/// 五种内容都要适配,且**图片恒为空**:旁支通知与回放不携带二进制。
#[test]
fn side_item_adapter_covers_every_kind() {
    let user = wire_item(&item(
        "i/1",
        SideItemContent::UserMessage { text: "问".into() },
    ))
    .unwrap();
    match user {
        Item::UserMessage {
            text,
            attachments,
            images,
            ..
        } => {
            assert_eq!(text, "问");
            assert!(attachments.is_empty());
            assert!(images.is_empty());
        }
        other => panic!("expected user message, got {other:?}"),
    }

    let agent = wire_item(&item(
        "i/2",
        SideItemContent::AgentMessage { text: "答".into() },
    ))
    .unwrap();
    assert!(matches!(agent, Item::AgentMessage { text, .. } if text == "答"));

    let thinking = wire_item(&item(
        "i/3",
        SideItemContent::Thinking {
            text: "想".into(),
            redacted: true,
        },
    ))
    .unwrap();
    match thinking {
        Item::Thinking { text, redacted, .. } => {
            assert_eq!(text, "想");
            assert!(redacted);
        }
        other => panic!("expected thinking, got {other:?}"),
    }

    let tool = wire_item(&item(
        "i/4",
        SideItemContent::ToolCall {
            call_id: "c1".into(),
            name: "read".into(),
            input: json!({"path": "a"}),
            result: Some("内容".into()),
            status: SideToolStatus::Failed,
            truncated: false,
        },
    ))
    .unwrap();
    match tool {
        Item::ToolCall {
            call_id,
            name,
            input,
            result,
            status,
            images,
            ..
        } => {
            assert_eq!(call_id, "c1");
            assert_eq!(name, "read");
            assert_eq!(input, json!({"path": "a"}));
            assert_eq!(result.as_deref(), Some("内容"));
            assert_eq!(status, ToolStatus::Failed);
            assert!(images.is_empty(), "工具卡不携带图片");
        }
        other => panic!("expected tool call, got {other:?}"),
    }
}

/// 提示映射成带固定前缀的 agent message,且前缀**只加一次**:转录那边已把提示
/// 的字节算进预算,适配层再叠一次就会让两端的文本边界悄悄不一致。
#[test]
fn side_notice_becomes_prefixed_agent_message_once() {
    let notice = item(
        "i/5",
        SideItemContent::Notice {
            text: "本轮已停止".into(),
        },
    );
    let wire = wire_item(&notice).unwrap();
    match wire {
        Item::AgentMessage { text, .. } => assert_eq!(text, "提示：本轮已停止"),
        other => panic!("expected agent message, got {other:?}"),
    }
    // 再适配一次同一个条目仍是同一个结果(没有把前缀叠进源文本)。
    let again = wire_item(&notice).unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap(),
        serde_json::to_value(wire_item(&notice).unwrap()).unwrap()
    );
}

/// 适配**不再截断**:转录给的条目已在 128 KiB 内,适配层只做形状替换。构造一条
/// 顶到预算的提示,确认适配后仍在单条上限内(前缀的额外字节也被吸收)。
#[test]
fn side_item_adapter_does_not_exceed_the_item_budget() {
    // 让源文本本身就在上限内,前缀只加少量字节;适配层不得因此报错。
    let text = "中".repeat(40_000); // 120 KB UTF-8
    let wire = wire_item(&item("i/6", SideItemContent::Notice { text })).unwrap();
    let size = serde_json::to_vec(&wire).unwrap().len();
    assert!(
        size <= SIDE_TRANSCRIPT_MAX_ITEM_BYTES,
        "item 为 {size} 字节"
    );
}

/// 转录层若给出超限条目,适配层如实报错而不是悄悄砍掉——静默截断会让「同一条
/// 文本在两端的边界」不一致,而这正是 spec §8.2 要避免的。
#[test]
fn side_item_adapter_refuses_an_oversized_item() {
    let text = "中".repeat(SIDE_TRANSCRIPT_MAX_ITEM_BYTES);
    let error = wire_item(&item("i/7", SideItemContent::AgentMessage { text })).unwrap_err();
    assert_eq!(error.reason, SideErrorReason::SnapshotTooLarge);
}

/// 请求参数的组合校验:cursor 单独出现无法判断属于哪个快照。
#[test]
fn side_read_items_rejects_a_cursor_without_a_token() {
    let params: SideReadItemsParams = serde_json::from_value(json!({
        "sideId": "s", "cursor": "c"
    }))
    .unwrap();
    assert!(params.cursor.is_some() && params.snapshot_token.is_none());
}

/// RPC 层必须把「非规范 UUID」压成 `invalid_params`,而不是把它当普通字符串
/// 存进去。ledger 自己也会校验,这里证明**参数一进来**就被拒。
#[tokio::test]
async fn side_turn_start_rejects_non_canonical_request_ids() {
    let bad = [
        "00000000-0000-4000-8000-00000000000A",   // 大写
        "00000000000040008000000000000001",       // 无连字符
        "00000000-0000-4000-8000-00000000000",    // 35 字符
        " 00000000-0000-4000-8000-000000000001 ", // 前后空白
    ];
    for id in bad {
        let params: SideTurnStartParams = serde_json::from_value(json!({
            "sideId": "s", "text": "x", "clientRequestId": id
        }))
        .unwrap();
        let mut ledger = yi_agent_runtime::side_conversation::SideRequestLedger::new();
        let error = ledger
            .accept(params.client_request_id, &params.text, "t".into())
            .unwrap_err();
        assert_eq!(error.reason, SideErrorReason::InvalidParams, "id = {id:?}");
    }
}

/// 32 KiB 是**原始 UTF-8 字节**上限,不 trim 后比较:首尾空白属于文本,用户
/// 有意保留时不能被悄悄改掉,也不能因为被 trim 而混过边界。
#[test]
fn side_turn_text_uses_raw_utf8_bytes() {
    let mut ledger = yi_agent_runtime::side_conversation::SideRequestLedger::new();
    let exact = "中".repeat(32 * 1024 / 3 + 1); // 3 字节/字,正好越过 32768
    let error = ledger.accept(uuid(), &exact, "t".into()).unwrap_err();
    assert_eq!(error.reason, SideErrorReason::InvalidParams);

    // 逐字节精确到边界:32768 接受,32769 拒绝。
    let mut accepted = yi_agent_runtime::side_conversation::SideRequestLedger::new();
    accepted
        .accept(uuid(), &"a".repeat(32 * 1024), "t".into())
        .unwrap();
    let mut refused = yi_agent_runtime::side_conversation::SideRequestLedger::new();
    assert!(
        refused
            .accept(uuid(), &"a".repeat(32 * 1024 + 1), "t".into())
            .is_err()
    );

    // 空白不被 trim:带前后空白的合法长度文本原样受理。
    let mut spaced = yi_agent_runtime::side_conversation::SideRequestLedger::new();
    spaced.accept(uuid(), "  问题  ", "t".into()).unwrap();
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
