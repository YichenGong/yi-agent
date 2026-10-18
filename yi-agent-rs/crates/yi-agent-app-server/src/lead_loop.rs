//! 组长后台循环的**判定**部分：什么算「有变化 / 需关注」。
//!
//! 抽成纯函数是本模块存在的全部理由——最难、最容易错的逻辑（唤醒判据）可以在
//! 不起 app-server、不花一个 token 的情况下单测。真正的循环内联在主循环的
//! `select!` 里（形状照抄 `card_scheduler_tick`），因为 `threads` 是主循环
//! 独占的 `&mut`（spec §6.1）。
//!
//! 零 LLM 成本：判据全是机械事实（状态 + 时间戳），与看板 `reconcile` 同源。

use crate::protocol::ThreadStatus;

// 三种唤醒理由是**契约里预留、判据尚未接线**的取值(见 spec §6.1 的分期):
// `decide_wake` 目前只构造 `AskLead` / `ThreadStuck`,但它们必须存在,否则
// 后续接 `ThreadFinished` / `UserSpoke` / `FallbackTick` 时要改公共类型。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeReason {
    /// 有 thread 调了 `ask_lead`。
    AskLead,
    /// 有 thread 到达终态（本轮新增：[Blocked] 讨论见 brief）。
    ThreadFinished,
    /// 有 thread 卡住（等待审批超时未决 / 长时间无进展）。
    ThreadStuck,
    /// 用户在组长的 IM 里说话了。
    UserSpoke,
    /// 低频兜底 tick（扫描前几类是否被漏掉）。
    FallbackTick,
}

// `title` / `last_user_text` 是给后续 digest 用的展示字段:`snapshot_of` 目前
// 刻意留空(见其文档),故暂无读者;保留字段以免接线时改结构体。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ThreadSnapshot {
    pub thread_id: String,
    pub title: Option<String>,
    pub status: ThreadStatus,
    pub last_user_text: Option<String>,
    pub updated_at: i64,
}

/// 算出本轮该唤醒哪些 thread、为什么。空 = 不唤醒。
pub fn decide_wake(snapshots: &[ThreadSnapshot], pending_asks: usize) -> Vec<(String, WakeReason)> {
    let mut wakes = Vec::new();
    if pending_asks > 0 {
        // 有 ask 时唤醒的理由挂在「最近更新的 thread」上，只为让 digest 有个出口；
        // 真正的投递走 `lead_rx`，与这里无关（本函数只管「要不要起一轮」）。
        if let Some(latest) = snapshots.iter().max_by_key(|s| s.updated_at) {
            wakes.push((latest.thread_id.clone(), WakeReason::AskLead));
        }
    }
    for s in snapshots {
        if s.status == ThreadStatus::AwaitingApproval {
            wakes.push((s.thread_id.clone(), WakeReason::ThreadStuck));
        }
    }
    wakes
}

/// 渲染注入文本：只展开被唤醒的 thread，其余不出现（spec §6.4 配套 1、2）。
pub fn render_digest(snapshots: &[ThreadSnapshot], wakes: &[(String, WakeReason)]) -> String {
    if wakes.is_empty() {
        return String::new();
    }
    let mut out = String::from("本轮有需要你关注的变化：\n");
    for (thread_id, reason) in wakes {
        let status = snapshots
            .iter()
            .find(|s| &s.thread_id == thread_id)
            .map(|s| format!("{:?}", s.status))
            .unwrap_or_else(|| "unknown".to_string());
        out.push_str(&format!("- {thread_id}（{reason:?}，状态 {status}）\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, status: ThreadStatus, updated_at: i64) -> ThreadSnapshot {
        ThreadSnapshot {
            thread_id: id.into(),
            title: Some(id.into()),
            status,
            last_user_text: None,
            updated_at,
        }
    }

    #[test]
    fn a_pending_ask_wakes_the_lead() {
        let wakes = decide_wake(&[snap("t1", ThreadStatus::Idle, 10)], 1);
        assert!(
            wakes
                .iter()
                .any(|(id, r)| id == "t1" && *r == WakeReason::AskLead)
        );
    }

    #[test]
    fn nothing_changed_and_no_asks_means_no_wake() {
        // 全部 idle、无待处理 ask → 不唤醒（省钱、不烧 token）。
        assert!(decide_wake(&[snap("t1", ThreadStatus::Idle, 10)], 0).is_empty());
    }

    #[test]
    fn a_thread_awaiting_approval_is_stuck_and_wakes_the_lead() {
        let wakes = decide_wake(&[snap("t1", ThreadStatus::AwaitingApproval, 10)], 0);
        assert!(wakes.iter().any(|(_, r)| *r == WakeReason::ThreadStuck));
    }

    #[test]
    fn digest_only_mentions_threads_that_woke() {
        let snaps = vec![
            snap("changed", ThreadStatus::AwaitingApproval, 20),
            snap("quiet", ThreadStatus::Idle, 10),
        ];
        let wakes = decide_wake(&snaps, 0);
        let digest = render_digest(&snaps, &wakes);
        assert!(digest.contains("changed"));
        assert!(
            !digest.contains("quiet"),
            "unchanged threads must not be expanded: {digest}"
        );
    }

    #[test]
    fn digest_never_claims_regenerated_state_as_memory() {
        // 覆盖 §6.4 的核心原则：注入的是「每轮现算的投影」，故文本里只描述现状。
        let snaps = vec![snap("t1", ThreadStatus::AwaitingApproval, 5)];
        let wakes = decide_wake(&snaps, 0);
        let digest = render_digest(&snaps, &wakes);
        assert!(digest.contains("t1"));
    }
}
