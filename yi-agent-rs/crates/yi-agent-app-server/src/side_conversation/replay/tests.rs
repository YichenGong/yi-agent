//! 分页、快照 token 与信封预算的单元测试。
//!
//! 这里钉的是 spec §8.2 里那些「看起来只是实现细节、实际是协议契约」的规则:
//! 一页必须是一次快照、cursor 不能跨快照、末页释放、超上限报 busy、信封按完整
//! 字节计。

use super::*;
use crate::protocol::Item;
use yi_agent_runtime::side_conversation::{SideItem, SideItemContent, SideTokenTotals};

fn user_message(text: &str, ordinal: u64) -> Arc<SideItem> {
    Arc::new(SideItem {
        id: format!("turn-1/userMessage/0/{ordinal}"),
        turn_id: "turn-1".to_string(),
        ordinal,
        content: SideItemContent::UserMessage {
            text: text.to_string(),
        },
    })
}

fn baseline(items: Vec<Arc<SideItem>>) -> ReplayBaseline {
    ReplayBaseline {
        side_id: "side-1".to_string(),
        parent_thread_id: "parent-1".to_string(),
        generation: 7,
        snapshot_seq: 42,
        status: SideStatus::Idle,
        usage: SideUsage {
            total: SideTokenTotals::default(),
            complete: true,
        },
        active_turn_id: None,
        accepted_requests: BTreeMap::new(),
        effective_config: SideEffectiveConfig {
            model: "test-model".to_string(),
            reasoning_effort: None,
            preserve_thinking: false,
            tool_names: Vec::new(),
        },
        items,
    }
}

/// 一页的条目字节必须受页预算约束,而预算来自 512 KiB 的**完整信封**上限。
#[test]
fn pages_are_cut_by_bytes_not_by_count() {
    // 每条 ~4 KiB,足够多到必须翻页。
    let chunk = "x".repeat(4 * 1024);
    let items: Vec<_> = (0..400)
        .map(|ordinal| user_message(&chunk, ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();

    let mut read = 0usize;
    let mut pages = 0usize;
    let mut token = None;
    let mut cursor = None;
    loop {
        let page = store
            .read_page(
                "conn-1",
                "side-1",
                token.as_deref(),
                cursor.as_deref(),
                token.is_none().then(|| baseline(items.clone())),
            )
            .expect("page must be readable");
        // 每页都自带同一份一致性元数据,客户端不必再单独问一次。
        assert_eq!(page.result.snapshot_seq, 42);
        assert_eq!(page.result.effective_config.model, "test-model");
        for item in &page.result.items {
            let size = serde_json::to_vec(item).unwrap().len();
            assert!(size <= SIDE_TRANSCRIPT_MAX_ITEM_BYTES);
        }
        let used: usize = page
            .result
            .items
            .iter()
            .map(|item| serde_json::to_vec(item).unwrap().len())
            .sum();
        assert!(
            used <= SIDE_PAGE_BUDGET_BYTES,
            "page exceeded budget: {used} > {SIDE_PAGE_BUDGET_BYTES}"
        );
        read += page.page_len;
        pages += 1;
        token = Some(page.result.snapshot_token.clone());
        cursor = page.result.next_cursor.clone();
        if page.finished {
            break;
        }
    }
    assert_eq!(read, 400);
    assert!(pages > 1, "400 x 4 KiB must not fit in one 512 KiB page");
}

/// cursor 是「已发条数」,与 token 一起绑定快照。任意别的数字都必须拒绝,否则两页
/// 会重叠或漏条。
#[test]
fn cursor_must_match_the_snapshot_position() {
    let items: Vec<_> = (0..200)
        .map(|ordinal| user_message(&"y".repeat(4 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    let first = store
        .read_page(
            "conn-1",
            "side-1",
            None,
            None,
            Some(baseline(items.clone())),
        )
        .unwrap();
    assert!(!first.finished);
    let token = first.result.snapshot_token.clone();
    let next = first.result.next_cursor.clone().unwrap();

    let skipped = store.read_page("conn-1", "side-1", Some(&token), Some("999"), None);
    assert_eq!(
        skipped.unwrap_err().reason,
        SideErrorReason::SnapshotExpired,
        "a foreign cursor must not be accepted"
    );
    // 合法 cursor 仍可用:拒绝非法值不应连带作废快照。
    let second = store
        .read_page("conn-1", "side-1", Some(&token), Some(&next), None)
        .unwrap();
    assert!(!second.result.items.is_empty());
}

/// token 绑定 side 与连接:另一条连接拿同一个 token 读必须失败,而不是接着别人的
/// 游标读。
#[test]
fn token_is_bound_to_connection_and_side() {
    let items: Vec<_> = (0..200)
        .map(|ordinal| user_message(&"z".repeat(4 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    let first = store
        .read_page("conn-1", "side-1", None, None, Some(baseline(items)))
        .unwrap();
    let token = first.result.snapshot_token.clone();
    let cursor = first.result.next_cursor.clone();

    let other_connection =
        store.read_page("conn-2", "side-1", Some(&token), cursor.as_deref(), None);
    assert_eq!(
        other_connection.unwrap_err().reason,
        SideErrorReason::SnapshotExpired
    );

    let other_side = store.read_page("conn-1", "side-2", Some(&token), cursor.as_deref(), None);
    assert_eq!(
        other_side.unwrap_err().reason,
        SideErrorReason::SnapshotExpired
    );
}

/// 首读必须带来源快照;续页必须**不带**快照。两者反过来都是调用方的错。
#[test]
fn first_read_needs_a_baseline_and_later_reads_must_not_have_one() {
    let mut store = ReplayStore::new();
    let no_baseline = store.read_page("conn-1", "side-1", None, None, None);
    assert_eq!(
        no_baseline.unwrap_err().reason,
        SideErrorReason::InvalidParams
    );

    let page = store
        .read_page(
            "conn-1",
            "side-1",
            None,
            None,
            Some(baseline(vec![user_message("hello", 0)])),
        )
        .unwrap();
    // 单条即末页:token 已释放,拿它再读必须过期。
    assert!(page.finished);
    assert!(page.result.next_cursor.is_none());
    let token = page.result.snapshot_token.clone();
    let again = store.read_page("conn-1", "side-1", Some(&token), None, None);
    assert_eq!(again.unwrap_err().reason, SideErrorReason::SnapshotExpired);
}

/// 末页释放 token 与它钉住的字节。客户端已拿全,继续占着宿主额度没有意义。
#[test]
fn the_last_page_releases_the_token_and_its_bytes() {
    let items: Vec<_> = (0..120)
        .map(|ordinal| user_message(&"q".repeat(8 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    let first = store
        .read_page("conn-1", "side-1", None, None, Some(baseline(items)))
        .unwrap();
    assert_eq!(store.token_count(), 1);
    assert!(store.retained_bytes() > 0);

    let mut token = first.result.snapshot_token.clone();
    let mut cursor = first.result.next_cursor.clone();
    let mut finished = first.finished;
    while !finished {
        let page = store
            .read_page("conn-1", "side-1", Some(&token), cursor.as_deref(), None)
            .unwrap();
        finished = page.finished;
        token = page.result.snapshot_token.clone();
        cursor = page.result.next_cursor.clone();
    }
    assert_eq!(store.token_count(), 0);
    assert_eq!(store.retained_bytes(), 0);
}

/// 断连与关闭都要各自释放。
#[test]
fn connection_and_side_release_drop_their_tokens() {
    let items: Vec<_> = (0..120)
        .map(|ordinal| user_message(&"w".repeat(8 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    store
        .read_page(
            "conn-1",
            "side-1",
            None,
            None,
            Some(baseline(items.clone())),
        )
        .unwrap();
    assert_eq!(store.token_count(), 1);
    store.release_connection("conn-2");
    assert_eq!(store.token_count(), 1, "别的连接不该被波及");
    store.release_connection("conn-1");
    assert_eq!(store.token_count(), 0);
    assert_eq!(store.retained_bytes(), 0);

    let mut store = ReplayStore::new();
    store
        .read_page("conn-1", "side-1", None, None, Some(baseline(items)))
        .unwrap();
    store.release_side("side-1");
    assert_eq!(store.token_count(), 0);
}

/// 宿主级 token 上限是 8。第 9 份必须回 `resource_limit`,而不是无限增长。
#[test]
fn host_token_cap_is_enforced() {
    let items: Vec<_> = (0..120)
        .map(|ordinal| user_message(&"e".repeat(8 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    for index in 0..SIDE_TOKENS_PER_HOST {
        store
            .read_page(
                &format!("conn-{index}"),
                "side-1",
                None,
                None,
                Some(baseline(items.clone())),
            )
            .expect("within cap");
    }
    let overflow = store.read_page("conn-overflow", "side-1", None, None, Some(baseline(items)));
    assert_eq!(
        overflow.unwrap_err().reason,
        SideErrorReason::ResourceLimit,
        "the ninth concurrent snapshot must be refused"
    );
}

/// 同一连接重开快照是替换,不是叠加:否则客户端每对账一次就吃掉一份宿主额度。
#[test]
fn reopening_on_one_connection_replaces_its_token() {
    let items: Vec<_> = (0..120)
        .map(|ordinal| user_message(&"r".repeat(8 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    let first = store
        .read_page(
            "conn-1",
            "side-1",
            None,
            None,
            Some(baseline(items.clone())),
        )
        .unwrap();
    let second = store
        .read_page("conn-1", "side-1", None, None, Some(baseline(items)))
        .unwrap();
    assert_eq!(store.token_count(), 1);
    // 旧 token 已经作废。
    let stale = store.read_page(
        "conn-1",
        "side-1",
        Some(&first.result.snapshot_token),
        None,
        None,
    );
    assert_eq!(stale.unwrap_err().reason, SideErrorReason::SnapshotExpired);
    assert_ne!(first.result.snapshot_token, second.result.snapshot_token);
}

/// 快照保留字节独立受限:光限 token 个数挡不住「8 份各 16 MiB」。
///
/// 这里的判据是**准入**:一份快照的保留字节一旦与已保留的合计超过 128 MiB,
/// 就必须回 `resource_limit`,而不是先收下再想办法。
#[test]
fn retained_bytes_are_capped_independently_of_token_count() {
    // 200 条 × 1 MiB 条目。基线是 `Arc` 索引,这里只关心估算出的保留字节。
    let items: Vec<_> = (0..200)
        .map(|ordinal| user_message(&"t".repeat(1024 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    // 第一份就在预算内(200 MiB > 128 MiB 时才会被拒),先手工确认估算口径。
    let error = store.read_page("conn-1", "side-1", None, None, Some(baseline(items)));
    assert_eq!(
        error.unwrap_err().reason,
        SideErrorReason::ResourceLimit,
        "a snapshot that alone exceeds the host byte budget must be refused"
    );
    // 被拒的快照不得留下字节账:否则一次失败会永久吃掉宿主额度。
    assert_eq!(store.retained_bytes(), 0);
    assert_eq!(store.token_count(), 0);
}

/// 末页释放之后,保留字节必须回到零,而不是泄漏到下一次快照。
#[test]
fn releasing_a_snapshot_returns_its_bytes() {
    let items: Vec<_> = (0..64)
        .map(|ordinal| user_message(&"t".repeat(100 * 1024), ordinal as u64))
        .collect();
    let mut store = ReplayStore::new();
    let mut token = None;
    let mut cursor = None;
    loop {
        let page = store
            .read_page(
                "conn-1",
                "side-1",
                token.as_deref(),
                cursor.as_deref(),
                token.is_none().then(|| baseline(items.clone())),
            )
            .unwrap();
        token = Some(page.result.snapshot_token.clone());
        cursor = page.result.next_cursor.clone();
        if page.finished {
            break;
        }
    }
    assert_eq!(store.token_count(), 0);
    assert_eq!(store.retained_bytes(), 0);
}

/// 快照之后 item 被更新,不影响已签发的那一页:索引是不可变 `Arc`。
#[test]
fn a_snapshot_keeps_the_version_it_captured() {
    let old = user_message("old text", 0);
    let mut store = ReplayStore::new();
    let page = store
        .read_page(
            "conn-1",
            "side-1",
            None,
            None,
            Some(baseline(vec![Arc::clone(&old)])),
        )
        .unwrap();
    // 转录层对同一 logical 位置会换一个 `Arc`,旧的那个必须原样可读。
    let replaced = user_message("new text", 0);
    assert_ne!(
        serde_json::to_vec(&*old).unwrap(),
        serde_json::to_vec(&*replaced).unwrap()
    );
    assert_eq!(page.result.items.len(), 1);
    match &page.result.items[0] {
        Item::UserMessage { text, .. } => assert_eq!(text, "old text"),
        other => panic!("unexpected item: {other:?}"),
    }
}

/// `encode_side_envelope` 按**完整信封**判定,不是只看正文。
#[test]
fn side_frame_size_uses_full_envelope() {
    let payload = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{
        "items":[{"text":"\\\"中".repeat(180_000)}]}});
    assert!(encode_side_envelope(&payload).is_err());
}

/// 接近上限但仍在预算内的信封必须通过,否则会把正常页也拒掉。
#[test]
fn envelope_within_budget_is_accepted() {
    let payload = serde_json::json!({"jsonrpc":"2.0","id":1,"result":{"items":[]}});
    let encoded = encode_side_envelope(&payload).expect("small envelope");
    assert!(encoded.len() < SIDE_ENVELOPE_BUDGET_BYTES);
}

/// 序号耗尽报 `resource_limit`,不回绕。
#[test]
fn seq_exhaustion_is_a_resource_limit_not_a_wrap() {
    assert_eq!(next_seq(1).unwrap(), 2);
    assert_eq!(
        next_seq(u64::MAX).unwrap_err().reason,
        SideErrorReason::ResourceLimit
    );
}

/// 每个已发布修订恰好一帧,且 `seq` 就钉在这次修订上。
#[test]
fn each_published_update_becomes_exactly_one_frame() {
    let update = SidePublishedUpdate::Item {
        side_id: "side-1".to_string(),
        turn_id: "turn-1".to_string(),
        item: Item::AgentMessage {
            id: "i1".to_string(),
            text: "hi".to_string(),
        },
    };
    let notification = update.into_notification(9, "parent-1", "side-1", 3);
    match notification {
        crate::protocol::Notification::SideItemUpdated {
            parent_thread_id,
            side_id,
            generation,
            seq,
            turn_id,
            ..
        } => {
            assert_eq!(parent_thread_id, "parent-1");
            assert_eq!(side_id, "side-1");
            assert_eq!(generation, 3);
            assert_eq!(seq, 9);
            assert_eq!(turn_id, "turn-1");
        }
        other => panic!("unexpected notification: {other:?}"),
    }
}

/// 旁支帧的订阅键是**父** threadId。
#[test]
fn side_frames_key_on_the_parent_thread() {
    let notification = SidePublishedUpdate::Closed {
        side_id: "side-1".to_string(),
        reason: CloseReason::UserClosed,
    }
    .into_notification(1, "parent-1", "side-1", 3);
    assert_eq!(notification.thread_key(), Some("parent-1"));
}

/// 超长正文在适配后仍受单条上限约束:诚实报错,不静默截断。
#[test]
fn oversized_single_item_is_refused_not_truncated() {
    let item = SideItem {
        id: "i1".to_string(),
        turn_id: "t1".to_string(),
        ordinal: 0,
        content: SideItemContent::AgentMessage {
            text: "a".repeat(SIDE_TRANSCRIPT_MAX_ITEM_BYTES + 1024),
        },
    };
    assert_eq!(
        wire_item(&item).unwrap_err().reason,
        SideErrorReason::SnapshotTooLarge
    );
}
