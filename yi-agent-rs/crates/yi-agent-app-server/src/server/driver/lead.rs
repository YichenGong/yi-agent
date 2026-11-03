//! 组长 tick：按项目算 digest、投递给组长会话。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 7）。

use crate::lead_loop::{ThreadSnapshot, decide_wake, render_digest};
use crate::session::ThreadSession;
use std::collections::HashMap;
use std::path::Path;

/// 一次组长投递的目标:该项目当前应承载消息的组长 thread。
///
/// `Clone`(而非借用 `self.lead_target`)是刻意的:`deliver_to_lead` 随后要把
/// `&mut self.threads` 交给内核,任何未结束的 `&self.lead_target` 借用都会与它
/// 冲突。三个 `String` 的克隆相对一次 agent 装配可以忽略。
#[derive(Clone)]
pub(crate) struct LeadTarget {
    pub(crate) cwd: String,
    pub(crate) thread_id: String,
}
/// 一轮后台 tick 对**某个项目**要做的一次投递:投给谁、投什么文本。
///
/// 用 owned 值而不是借用 `threads`:投递要拿 `&mut threads`(走
/// `ServeLauncher::deliver_to_lead`),任何未结束的借用都会与之冲突。这也是
/// 把「算」与「投」分成两步的全部理由。
pub(crate) struct LeadTickDelivery {
    pub(crate) project_key: String,
    pub(crate) lead: LeadTarget,
    pub(crate) text: String,
}
/// 算一轮时按项目聚起来的一组中间量。
pub(crate) struct LeadGroup {
    pub(crate) project_key: String,
    /// 该项目的组长;`None` = 还没有组长 → 该组整体跳过(**不创建**)。
    pub(crate) lead: Option<LeadTarget>,
    pub(crate) snapshots: Vec<ThreadSnapshot>,
}
/// 一个会话的机械快照。
///
/// 只取**进程内事实**:状态只活在 `ThreadSession.status` 里(磁盘 `ThreadMeta`
/// 没有 status 字段)。因此「本进程没打开过的 thread」天然不在 `threads` 里,也就
/// 不会被算成「卡住」——这正是想要的:后台循环不为看不见的会话花钱。
///
/// `title` / `last_user_text` / `updated_at` 刻意留空:本轮唯一的判据是 status
/// (`pending_asks` 恒 0,见 [`lead_tick_once`]),读盘只会把每 30s 一次兜底变成
/// N 次 IO,而 `render_digest` 也不用这三个字段。
pub(crate) fn snapshot_of(session: &ThreadSession) -> ThreadSnapshot {
    ThreadSnapshot {
        thread_id: session.thread_id.clone(),
        title: None,
        status: *session.status.lock().unwrap_or_else(|p| p.into_inner()),
        last_user_text: None,
        updated_at: 0,
    }
}
/// 由项目键(状态目录 `<项目根>/.yi-agent`)反推项目根,与 `resolve_lead_target`
/// 同一口径。
pub(crate) fn project_root_of(project_key: &str) -> Option<String> {
    Path::new(project_key)
        .parent()
        .map(|root| root.display().to_string())
}
/// 组长后台循环一轮的**纯计算**部分:算出哪些项目的组长该被唤醒、该注入什么文本。
///
/// 抽成独立函数是为了**可测**:测试只断言这个返回值,不必观测一次真实的 LLM 轮
/// (spec §6.1 要的是「后台循环住主循环」,不是「测一次唤醒就得烧 token」)。它
/// 不落盘、不发帧、不 spawn,主循环的 `select!` 分支只负责把结果逐条投递。
///
/// 三条刻意约束:
/// - **不懒创建组长**:判定「有没有组长」只用 `ThreadStore::find_lead()`(纯读)。
///   绝不能用 `resolve_lead_target` —— 它内部会 `ensure_lead_meta`,于是每 30s
///   的兜底都会替每个项目落一个组长 meta,把「谁拉起组长」变成后台循环。没有
///   组长就是没有:跳过。
/// - **`pending_asks` 传 0**:阻塞式 `ask_lead` 的投递已由主循环的 `lead_rx`
///   分支实时处理(那条路径本来就最及时)。这里再报一次「有 ask」只会把同一次
///   求助变成两轮,既贵又吵。
/// - **只认本进程已打开的会话**:分组键取自会话自己的 store,快照只含
///   `status`;磁盘上未被打开的 thread 看不到实时状态,按 Idle 处理(不唤醒)。
pub(crate) fn lead_tick_once(
    threads: &mut HashMap<String, ThreadSession>,
) -> Vec<LeadTickDelivery> {
    // 阶段一(只读):按项目分组,给每组定出组长与一组 owned 快照。
    // 先读完再算,`threads` 的借用在本阶段结束时就归还,不会与调用方的 `&mut`
    // 冲突(与看板 tick 的「先快照成 owned,再借 &mut」是同一手法)。
    let mut groups: Vec<LeadGroup> = Vec::new();
    for session in threads.values() {
        let project_key = session.store.project_key();
        let Some(root) = project_root_of(&project_key) else {
            // 项目键没有 parent(理论上不可达):无处找组长,跳过。
            continue;
        };
        let idx = match groups.iter().position(|g| g.project_key == project_key) {
            Some(idx) => idx,
            None => {
                let lead = match session.store.find_lead() {
                    Ok(Some(meta)) => Some(LeadTarget {
                        cwd: root,
                        thread_id: meta.thread_id,
                    }),
                    _ => None,
                };
                groups.push(LeadGroup {
                    project_key,
                    lead,
                    snapshots: Vec::new(),
                });
                groups.len() - 1
            }
        };
        // 组长不是「它自己的 thread」:把组长列进去会让每一轮都自我唤醒,
        // 那等于把低频兜底变成永久烧钱的自激循环。
        if groups[idx]
            .lead
            .as_ref()
            .is_some_and(|lead| lead.thread_id == session.thread_id)
        {
            continue;
        }
        groups[idx].snapshots.push(snapshot_of(session));
    }

    // 阶段二(纯算):每组判一次「要不要起一轮」,要就把注入文本渲出来。
    let mut deliveries = Vec::new();
    for group in groups {
        // 没有组长 → 整组跳过。这里**不创建**组长。
        let Some(lead) = group.lead else {
            continue;
        };
        let wakes = decide_wake(&group.snapshots, 0);
        if wakes.is_empty() {
            continue;
        }
        deliveries.push(LeadTickDelivery {
            project_key: group.project_key,
            lead,
            text: render_digest(&group.snapshots, &wakes),
        });
    }
    deliveries
}
/// 过滤掉与上次**已投递**文本相同的投递项,并更新闩锁。
///
/// 背景:`decide_wake` 对每个 `AwaitingApproval` 无条件产出 `ThreadStuck`,而
/// `LEAD_TICK = 30s`、`PERMISSION_TIMEOUT = 300s`——一次审批窗会让组长被同一条
/// 消息唤醒最多 10 次,每次都是带全部上下文的完整 LLM turn(纯噪声 + 真金白银)。
///
/// 去重键取**文本本身**(而非 thread_id/原因):不用理解状态机,天然覆盖所有
/// wake reason;态势仍每轮现算(`render_digest` 照常跑),只是不重复**投递**
/// 一模一样的文本。内容一变(状态变了 / 新 thread 卡住)即恢复投递。
///
/// 这里刻意**先写闩锁再返回**:投递失败的下一轮不该重投同一文本——失败也已经
/// 花了那一轮的成本,无限重试只会放大(与 `lead_rx` 分支那套 request_id 回滚
/// 不同,那条路径有请求方在等回信,这条是单向 digest)。
pub(crate) fn take_changed_digests(
    deliveries: Vec<LeadTickDelivery>,
    latch: &mut HashMap<String, String>,
) -> Vec<LeadTickDelivery> {
    let mut out = Vec::new();
    for delivery in deliveries {
        if latch.get(&delivery.project_key) == Some(&delivery.text) {
            continue;
        }
        latch.insert(delivery.project_key.clone(), delivery.text.clone());
        out.push(delivery);
    }
    out
}
/// 由项目键反推该项目当前应承载消息的组长。
///
/// 项目键本身就是 `<cwd>/.yi-agent` 的 canonical 路径(见 `ThreadStore::project_key`),
/// 故 cwd 可由它去掉末段得到。组长 meta 不存在时当场补建——用户可能直接让某个
/// 会话给组长留言,而从未打开过组长对话。
///
/// 只经 `store` 落盘 meta,不触碰 agent / runtime / 任何 `cfg` 字段,故**不需要**
/// `RuntimeConfig`:把它列在签名里只会是一个没人读的参数(brief 的签名多带了
/// `cfg`,经核实无消费方,已去掉)。
pub(crate) fn resolve_lead_target(project_key: &str) -> anyhow::Result<LeadTarget> {
    let state_dir = Path::new(project_key);
    let cwd = state_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("project key has no parent: {project_key}"))?;
    let store = crate::thread_store::ThreadStore::new(cwd);
    let (meta, _created) = crate::lead::ensure_lead_meta(&store, cwd)?;
    Ok(LeadTarget {
        cwd: cwd.display().to_string(),
        thread_id: meta.thread_id,
    })
}
