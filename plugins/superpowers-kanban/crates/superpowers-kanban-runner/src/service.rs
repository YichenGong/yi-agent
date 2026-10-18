//! 看板服务：把 board.json 与全局并发槽位收敛到一把锁。
//!
//! 推进循环与查询分派共享同一 `BoardService`，所以 `next_launch` 的
//! 「判名额 → 建 worktree → 置 Launching」相对推进循环是原子的，
//! 不会两张卡抢到同一个槽位。
//!
//! 槽位记账有两处，必须一致：`Board` 里只有 `Running` 占槽，而本服务另外
//! 为每张**已认领但尚未 Running**（即 `Launching`）的卡片持有一个
//! `flock` 租约。`Claiming` 不占槽，所以单靠 `Board::claim_next_launch`
//! 连调两次会认领超过 `limit` 张——`Inner::leases` 正是补这个洞：只有领到
//! 租约才会认领，认领成功就把租约存进 map 一直持有到卡片终态。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::{Value, json};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{Card, CardId, CardKind, CardState};

use crate::inbox::{self, InboxOutcome};
use crate::lease::{self, Lease};
use crate::merge::{self, MergeOutcome};
use crate::persist;
use crate::worktree::{ensure_worktree, slugify};

/// 一次成功认领：卡片 id、它的 worktree、以及给会话用的标题。
pub struct LaunchClaim {
    pub card_id: String,
    pub workdir: PathBuf,
    pub title: String,
}

/// 一次合并尝试的结果：卡片 id 与合并结局。
pub struct MergeClaim {
    pub card_id: String,
    pub outcome: crate::merge::MergeOutcome,
}

/// 一次名额申请的结果。
///
/// `Granted` 是**授权**而不是「已经合并」：调用方（会话里的 agent）据 `workdir`
/// 去执行 `git merge --no-ff`，完成后必须回 `merge_finish` 让插件复核。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeRequest {
    Granted {
        /// 执行合并的 base worktree（base 已被检出的那个，通常是主检出）。
        workdir: PathBuf,
        source: String,
        base: String,
    },
    /// 本项目已有合并在跑：卡不动，排队。
    Busy,
    /// 卡不在可合并状态（或 refs 不全）。
    Denied(String),
}

/// 一次合并轮收尾的复核结果。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeFinish {
    /// git 复核确认 source 已并入 base。
    Done,
    /// git 复核不通过：卡退回 `needs_you` 等用户发话。
    NeedsYou,
    /// 卡不在 `Merging`：不落状态，交由调用方决定。
    Cleared,
}

/// 插件交给宿主的**通用**动作（spec D4）。宿主不认识看板：它只执行、不解释。
///
/// `card_id` 是插件内部的记账键（D5：`card_id → thread_id` 映射由插件持有），
/// **不进 JSON**——宿主只认 `action_id`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverAction {
    /// 新建会话：宿主在 `workdir` 里起会话，首轮投递 `objective`。
    Launch {
        action_id: String,
        card_id: String,
        workdir: PathBuf,
        title: String,
        objective: String,
    },
    /// 向已有会话追加一轮（合并轮），不新建会话。
    Append {
        action_id: String,
        card_id: String,
        thread_id: String,
        objective: String,
    },
}

impl DriverAction {
    /// 跨进程契约：字段名与宿主 `session_driver::parse_action` 逐字对齐（spec §4.3）。
    /// 用 `action_id` 作键而不是直接序列化枚举——枚举的形状是实现细节，不能外泄。
    pub fn to_json(&self) -> Value {
        match self {
            DriverAction::Launch {
                action_id,
                workdir,
                title,
                objective,
                ..
            } => json!({
                "action_id": action_id,
                "kind": "launch",
                "workdir": workdir.to_string_lossy(),
                "title": title,
                "objective": objective,
            }),
            DriverAction::Append {
                action_id,
                thread_id,
                objective,
                ..
            } => json!({
                "action_id": action_id,
                "kind": "append",
                "thread_id": thread_id,
                "objective": objective,
            }),
        }
    }
}

/// 宿主回传的一次动作结果（spec D12 / §4.2）。
///
/// 字段名与宿主的 `ActionResult` 逐字对齐——这是跨进程契约，改名等于断链。
/// 三个 `#[serde(default)]` 只覆盖这个结构自己的 `ok` / `thread_id` / `error`：
/// 宿主回报失败时可以只带 `action_id` + `ok:false`（甚至只带 action_id），
/// 整条回报不该因为缺字段而反序列化失败——回报丢了比回报格式怪要贵得多。
/// （会话空闲与否**不在这里**：那是 `driver/next` 请求里的 `threads[].idle`，
/// 由 `dispatch.rs` 手工解析并缺省为「不闲」。）
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DriverResult {
    pub action_id: String,
    /// 动作是否执行成功（启动失败、会话忙等）。
    #[serde(default)]
    pub ok: bool,
    /// launch 成功时带回的会话 id（D5：插件的映射只能从这里得到）。
    #[serde(default)]
    pub thread_id: Option<String>,
    /// `ok: false` 时的失败原因。
    ///
    /// 它不是给日志看的：启动失败时经 `mark_terminal` 落到卡片的 `detail` 上，
    /// 由 `list` 暴露给人和桌面（spec §4.6「失败 → 卡 `failed`（原因取 `error`）」）。
    #[serde(default)]
    pub error: Option<String>,
}

struct Inner {
    board: Board,
    /// 已认领卡片持有的槽位租约。`Running` 卡片也仍在这里，直到终态释放。
    leases: std::collections::HashMap<CardId, Lease>,
    /// `None` 表示拿不到全局租约目录：`next_launch` 必须报错而不是私自
    /// 造一个目录，否则会和别的进程用两个互不相干的池子、悄悄超配额。
    leases_dir: Option<PathBuf>,
    /// 已提示过「分支缺失」的卡：同一 daemon 生命周期内每卡至多提示一次。
    /// 纯内存、不落盘——重启后重发一次是可接受的（该提示要求人工动作）。
    notified_missing_branch: std::collections::HashSet<CardId>,
    /// 已交给宿主、还没收到回报的动作，按 `action_id` 索引。
    ///
    /// 两副担子：① 它是 `card_id → thread_id` 映射的另一半——`thread_id` 只由
    /// 回报带回（spec D5），所以「哪个动作属于哪张卡」必须记在这里；
    /// ② 它是 at-least-once 的幂等账——回报一到就摘除条目，宿主重复投递的那份
    /// 找不到条目、直接忽略，因而不会重复生效。
    pending_actions: std::collections::HashMap<String, PendingAction>,
    /// **合并轮失败**退回来的卡：已用掉的自动重试次数 + 最近一次失败原因（spec D9）。
    ///
    /// 只记「合并轮失败」，不记会话自己跑完落下的 `needs_you`：后者没有产出
    /// 可合，自动进合并轮会把它悄悄判成 `done`（分支与 base 同点，
    /// `merge-base --is-ancestor` 恒真）。
    merge_retries: std::collections::HashMap<CardId, (u32, String)>,
    /// 动作 id 序号，配合 `action_nonce` 保证跨进程不撞名：重启后序号从 1 再来，
    /// 若 id 只有序号，上一进程遗留的回报会被误认成本进程的动作。
    next_action_seq: u64,
    action_nonce: String,
}

/// 一个已交出、等回报的动作。
struct PendingAction {
    card_id: CardId,
    kind: PendingKind,
}

/// 动作种类。回报的处置因种类而异：`launch` 靠 `thread_id` 让卡进入 `running`；
/// `append`（合并轮）只说明回合已投递，成不成要等会话空闲后由 git 复核裁定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Launch,
    Append,
}

pub struct BoardService {
    state_dir: PathBuf,
    project_root: PathBuf,
    inner: Mutex<Inner>,
}

/// worktree 当前 HEAD，作为「有无新提交」的对照基线。非 git 目录返回 `None`。
pub fn effective_head(workdir: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(workdir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl BoardService {
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        let leases_dir = home
            .map(|home| {
                home.join(".yi-agent")
                    .join("superpowers-kanban")
                    .join("leases")
            })
            .or_else(lease::global_leases_dir);
        let board = persist::load_board(&state_dir.join("board.json"));
        Self {
            state_dir,
            project_root,
            inner: Mutex::new(Inner {
                board,
                leases: Default::default(),
                leases_dir,
                notified_missing_branch: Default::default(),
                pending_actions: Default::default(),
                merge_retries: Default::default(),
                next_action_seq: 0,
                action_nonce: format!(
                    "{}-{}",
                    std::process::id(),
                    chrono::Local::now().timestamp_millis()
                ),
            }),
        }
    }

    /// 中毒的锁照样用：状态是一份纯数据，`panic` 不会留下半写状态，
    /// 因此毒化不值得让整个推进循环停摆。
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn board_path(&self) -> PathBuf {
        self.state_dir.join("board.json")
    }

    fn save(&self, inner: &Inner) {
        let _ = persist::save_board(&self.board_path(), &inner.board);
    }

    /// 一次合并轮自动重试的上限（spec D9 的「可自动重试」要有界）。
    ///
    /// 每次重试都是一个真实的模型回合（分钟级、要花钱），而失败若来自环境
    /// （分支没建出来、冲突解决不了），重试只会烧钱。到顶后卡停在 `needs_you`
    /// 等人发话——人工介入后 `merge_retries` 计数随会话重新跑起来而清零。
    const MAX_MERGE_RETRIES: u32 = 3;

    /// 本插件**唯一**的推进入口（spec D1 / §4.6）。
    ///
    /// 宿主每 tick 调一次；这里回**至多一个**通用动作（`Launch` / `Append`），
    /// 无事可做回 `None`。宿主不认识看板：队列顺序、并发名额、卡片状态迁移、
    /// objective 文案，全都在这里定。
    ///
    /// 一拍内的固定次序：**回报 → 合并轮收尾 → 对账 → 合并轮开局 → 启动**。
    /// 「对账」与「合并轮开局」在同一拍互斥：一张卡若在这一拍刚从 `running`
    /// 落到 `awaiting_merge`，它的合并轮要等下一拍——刚跑完的下一步是等验收，
    /// 不是立刻又塞一个回合进去（spec 风险表也接受最多晚一拍）。
    pub fn driver_step(
        &self,
        threads: &[(String, bool)],
        results: &[DriverResult],
    ) -> Result<Option<DriverAction>, String> {
        // 前置检查必须**早于**消费回报：拿不到全局租约目录时 `next_launch` 只会
        // 整拍报错，而错误会让宿主把这批回报当作「没送到」重发；可回报已经消费
        // 掉了，重发的那份会被当成陌生动作丢弃，那次状态迁移就永久丢了。
        if self.lock().leases_dir.is_none() {
            return Err("no lease directory".into());
        }
        // 一拍一份快照：下面的判断都基于同一份账，免得边决策边被别处改状态。
        let cards = self.locked_cards();

        // 1) 回报：宿主上一拍执行的结果在这里落账（`card_id → thread_id` 映射
        //    只有这一个来源，spec D5）。重复投递的回报找不到待办条目 → 忽略。
        self.consume_results(results);

        // 2) 合并轮收尾：`merging` 卡的会话再次空闲 → 交既有的 git 复核
        //    （`merge_finish`）裁定，绝不信模型自述。
        for card in cards.iter().filter(|card| card.state == CardState::Merging) {
            let Some(thread_id) = card.thread_id.as_deref() else {
                continue;
            };
            if idle_of(threads, thread_id) != Some(true) {
                continue;
            }
            // 刚交出的合并回合还没回报 → 不抢着收尾：那会儿宿主可能连 turn 都
            // 还没起来，复核会把一个还没跑的合并判成失败。
            if self.pending_kind(&card.id) == Some(PendingKind::Append) {
                continue;
            }
            let _ = self.merge_finish(&card.id.0);
        }

        // 3) 对账：`running` 且会话空闲 → 有提交等验收、无提交等人发话；
        //    停在验收/等人态而会话又忙起来 → 卡跟着会话翻回 `running`
        //    （2026-10-03 看板状态误判的回归锁）。对账本身**不交动作**。
        for card in cards
            .iter()
            .filter(|card| card.kind == CardKind::Implementation)
        {
            let Some(thread_id) = card.thread_id.as_deref() else {
                continue;
            };
            match card.state {
                CardState::Running => {
                    if idle_of(threads, thread_id) != Some(true) {
                        continue;
                    }
                    // 判词跟着状态走：`needs_you` 的答案是「会话跑完了但没产出」，
                    // `awaiting_merge` 的答案是「有产出等验收」。人问的
                    // 「为什么停在这」在两种情况下都能从 `list` 里读到。
                    let outcome = if has_commits(card) {
                        "awaiting_merge"
                    } else {
                        "needs_you"
                    };
                    let detail = if outcome == "needs_you" {
                        Some("session ended without new commits")
                    } else {
                        None
                    };
                    let _ = self.mark_terminal(&card.id.0, outcome, detail);
                }
                CardState::AwaitingMerge | CardState::NeedsYou
                    if idle_of(threads, thread_id) == Some(false) =>
                {
                    let _ = self.mark_running(&card.id.0, thread_id);
                }
                _ => {}
            }
        }

        // 4) 合并轮开局：一次只跑一个合并（spec D8）。已有卡在 `merging`——它是
        //    名额的**持久载体**（`merge.lock` 只在临界区里活着）——就不开局；
        //    第二张 `awaiting_merge` 卡原地等它释放。
        if !self.any_merging() {
            let mut candidates: Vec<&Card> = cards
                .iter()
                .filter(|card| match card.state {
                    CardState::AwaitingMerge => true,
                    // `needs_you` 只有**合并轮失败**退回来的才可自动重试（D9）。
                    // 会话自己跑完落下的 `needs_you` 没有产出可合（spec §4.6），
                    // 自动进合并轮会把它悄悄判成 `done`——分支与 base 同点，
                    // `merge-base --is-ancestor` 恒真。
                    CardState::NeedsYou => {
                        (1..=Self::MAX_MERGE_RETRIES).contains(&self.merge_retry_count(&card.id))
                    }
                    _ => false,
                })
                .collect();
            // 确定性：同 `order`（旧数据）时按 id 排，免得每拍挑中不同的卡。
            candidates.sort_by(|a, b| (a.order, &a.id).cmp(&(b.order, &b.id)));
            for card in candidates {
                let Some(thread_id) = card.thread_id.clone() else {
                    // 没有会话就没有「最后一轮」可追加（合并卡的裸 git 通路属
                    // Task 8 退役的范围）。
                    continue;
                };
                if idle_of(threads, &thread_id) != Some(true) {
                    continue;
                }
                match self.merge_request(&card.id.0) {
                    Ok(MergeRequest::Granted {
                        workdir,
                        source,
                        base,
                    }) => {
                        let action_id = self.mint_action_id();
                        self.remember_pending(&action_id, &card.id, PendingKind::Append);
                        return Ok(Some(DriverAction::Append {
                            action_id,
                            card_id: card.id.0.clone(),
                            thread_id,
                            objective: crate::tick::merge_objective(&source, &base, &workdir),
                        }));
                    }
                    // `Busy`（本进程/别的进程正持着 `merge.lock`）与 `Denied`
                    // （分支缺失等）都不是整拍失败：换下一张，或等下一拍。
                    Ok(_) | Err(_) => continue,
                }
            }
        }

        // 5) 启动：队首、有名额、日历放行 → 交出 `launch`。名额/租约/worktree
        //    的既有记账全在 `next_launch` 里，这里不重造（brief 规则 2）。
        let limit = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .limit_at(chrono::Local::now());
        match self.next_launch(limit, chrono::Local::now()) {
            Ok(Some(claim)) => {
                let objective = cards
                    .iter()
                    .find(|card| card.id.0 == claim.card_id)
                    .map(crate::tick::engagement_objective)
                    .unwrap_or_else(|| crate::tick::engagement_objective_for_paths("", ""));
                let action_id = self.mint_action_id();
                let card_id = CardId::new(claim.card_id.clone());
                self.remember_pending(&action_id, &card_id, PendingKind::Launch);
                Ok(Some(DriverAction::Launch {
                    action_id,
                    card_id: claim.card_id,
                    workdir: claim.workdir,
                    title: claim.title,
                    objective,
                }))
            }
            Ok(None) => Ok(None),
            Err(error) => {
                // worktree 建不出来时 `next_launch` 已经把卡记为 `failed`，所以
                // 这只是「本拍没有动作」，**不是**查询失败——回报的账已经记完，
                // 返回 Err 会诱使宿主重发那批回报。
                eprintln!("superpowers-kanban: launch failed: {error}");
                Ok(None)
            }
        }
    }

    /// 消费宿主回报的动作结果（spec D12）。
    ///
    /// 一律按 `action_id` 取出**待办条目**：取不到（重复投递、跨进程遗留、不认识
    /// 的 id）就整条忽略。这就是 at-least-once 的幂等实现——宿主重复投递同一
    /// `action_id` 不会二次生效，也不会重复记账名额。
    ///
    /// 摘除条目本身就是判重：第二条同 id 的回报从此查无此条。
    fn consume_results(&self, results: &[DriverResult]) {
        for result in results {
            let Some(pending) = self.take_pending(&result.action_id) else {
                continue;
            };
            // 迁移失败不再把条目放回：能失败的都是**不会再成功**的那种
            // （卡不存在、当前状态不接受该迁移），重试只是原地打转；真正还要
            // 推进的卡，下一拍的对账会重新看到它。
            if let Err(error) = self.apply_result(&pending, result) {
                eprintln!(
                    "superpowers-kanban: could not apply result for {}: {error}",
                    pending.card_id
                );
            }
        }
    }

    /// 把一条已认领的回报落到看板上。
    fn apply_result(&self, pending: &PendingAction, result: &DriverResult) -> Result<(), String> {
        match (pending.kind, result.ok) {
            (PendingKind::Launch, true) => match result.thread_id.as_deref() {
                // 映射落地：`running` + 记下 thread id。
                Some(thread_id) => self.mark_running(&pending.card_id.0, thread_id),
                // `ok` 却没有 thread id：无从记账，只能不落状态（会话确实起了，
                // 但它不在宿主的 `threads` 里，谁也对不上）。
                None => {
                    eprintln!(
                        "superpowers-kanban: launch of {} reported ok without a thread_id",
                        pending.card_id
                    );
                    Ok(())
                }
            },
            // 启动失败：卡 `failed` 并让出名额——失败的启动是可恢复的，
            // 不是死胡同（旧宿主在 `release` 里做的是同一件事）。原因经
            // `mark_terminal` 落到卡片上，`list` 读得到（spec §4.6）。
            (PendingKind::Launch, false) => {
                let reason = result
                    .error
                    .clone()
                    .unwrap_or_else(|| "launch failed".to_string());
                self.mark_terminal(&pending.card_id.0, "failed", Some(&reason))
            }
            // 合并回合：结局一律由 git 复核裁定（D10），不采信回报里的成败。
            // `ok: true` 只说明这一轮投递出去了——会话还没跑完，什么都不做；
            // `ok: false`（会话正忙等）立刻走一次复核把名额放掉：分支若恰好已
            // 并入就是 `done`，否则退回 `needs_you` 走重试（D9）。
            (PendingKind::Append, true) => Ok(()),
            (PendingKind::Append, false) => {
                let _ = self.merge_finish(&pending.card_id.0);
                Ok(())
            }
        }
    }

    /// 看板快照（克隆）：一次决策取一份，避免边决策边读到漂移的状态。
    fn locked_cards(&self) -> Vec<Card> {
        self.lock().board.cards().to_vec()
    }

    /// 是否已有卡在合并中——名额的持久载体（`merge.lock` 只在临界区活着）。
    fn any_merging(&self) -> bool {
        self.lock()
            .board
            .cards()
            .iter()
            .any(|card| card.state == CardState::Merging)
    }

    /// 这张卡因**合并轮失败**退回 `needs_you` 的次数（`0` 表示不是那么退回来的）。
    fn merge_retry_count(&self, id: &CardId) -> u32 {
        self.lock()
            .merge_retries
            .get(id)
            .map(|(count, _)| *count)
            .unwrap_or(0)
    }

    /// 新动作 id。带 `action_nonce`（pid + 时间戳）才不会与上一进程遗留的
    /// 动作撞击：若 id 只是本进程内的序号，重启后旧回报会被误认成新动作。
    fn mint_action_id(&self) -> String {
        let mut inner = self.lock();
        inner.next_action_seq += 1;
        format!("act-{}-{}", inner.action_nonce, inner.next_action_seq)
    }

    fn remember_pending(&self, action_id: &str, card_id: &CardId, kind: PendingKind) {
        self.lock().pending_actions.insert(
            action_id.to_string(),
            PendingAction {
                card_id: card_id.clone(),
                kind,
            },
        );
    }

    /// 取出并摘除待办条目：摘除即「这条回报已生效」，重复投递自然落空。
    fn take_pending(&self, action_id: &str) -> Option<PendingAction> {
        self.lock().pending_actions.remove(action_id)
    }

    /// 该卡是否有一个还没回报的动作（及其种类）。
    fn pending_kind(&self, card_id: &CardId) -> Option<PendingKind> {
        self.lock()
            .pending_actions
            .values()
            .find(|pending| &pending.card_id == card_id)
            .map(|pending| pending.kind)
    }

    /// 自动归档「已到终态且超过宽限期」的卡；返回被归档的 id。
    ///
    /// 宽限期取自项目自己的日历（`archive_grace_hours`，默认 24）。每次 tick 读一次，
    /// 不缓存进 `Inner`——`Inner` 只放看板与租约，多一个字段要多一处同步。
    /// **永不**归档 `awaiting_merge`/`needs_you`/`running`/`queued`（`is_terminal` 已排除）。
    pub fn archive_due(&self, now: DateTime<Local>) -> Vec<CardId> {
        let hours = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .archive_grace_hours;
        let grace = chrono::Duration::hours(hours.min(24 * 30) as i64);
        let mut inner = self.lock();
        let swept = inner.board.archive_due(now, grace);
        if !swept.is_empty() {
            self.save(&inner);
        }
        swept
    }

    /// 认领下一张排队卡：先拿全局槽位租约，再判名额、建 worktree、置 `Launching`。
    ///
    /// 全程在同一把锁内，且认领与持租约是同一件事的两面，所以同一个
    /// `BoardService` 连续调用不会认领超过 `limit` 张——租约被占住后
    /// `acquire_in` 就再也拿不到新的槽位了。
    pub fn next_launch(
        &self,
        limit: u16,
        _now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        // 先占槽：拿不到就说明名额已满（含本进程自己已持有的）。
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        // 再认领：队空或无名额时把刚拿到的租约原地丢掉（drop 即释放）。
        let Some(card_id) = inner.board.claim_next_launch(limit) else {
            return Ok(None);
        };
        let branch = format!("kanban/{}", slugify(&card_id));
        let workdir = match ensure_worktree(&self.project_root, &card_id, &branch) {
            Ok(path) => path,
            Err(error) => {
                // 认领即占槽：建 worktree 失败必须把卡片标为 Failed，
                // 否则它会卡在 Launching 永远不再被认领。
                let reason = format!("worktree for {card_id} failed: {error}");
                let _ = inner.board.transition(&card_id, CardState::Failed);
                // 这条原因同样要可读：这是全自动路径上最常见的启动失败，
                // 只写 stderr 的话重启后无处可查（spec §4.6）。
                let _ = inner.board.set_detail(&card_id, Some(reason.clone()));
                self.save(&inner);
                return Err(reason);
            }
        };
        if let Some(base) = effective_head(&workdir) {
            let _ = inner.board.set_base_commit(&card_id, base);
        }
        let _ = inner.board.set_workdir(&card_id, workdir.clone());
        let title = title_for(&inner.board, &card_id);
        inner.leases.insert(card_id.clone(), lease);
        self.save(&inner);
        Ok(Some(LaunchClaim {
            card_id: card_id.0,
            workdir,
            title,
        }))
    }

    /// 启动时补领本进程既有 `Running` 卡片的全局名额。
    ///
    /// 重启后 `BoardService` 是新进程、内存 lease 为空，而 board.json 里可能仍有
    /// `Running` 卡片。`next_launch` 的 `free_slots` 只看 `Running` 计数，但全局池
    /// 是空手起家——别的项目会把本该属于这些在跑卡片的名额抢走。为每张
    /// `Running` 卡片补领一个 lease 记进 `Inner::leases`，全局池的占用才与实际
    /// 在跑数量一致。受 `limit` 与 `acquire_in` 约束：领不到就只做尽力而为
    /// （`limit` 缩小时可能领不全），不 panic。
    pub fn adopt_running_leases(&self, limit: u16) {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return;
        };
        let ids: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Running)
            .map(|card| card.id.clone())
            .collect();
        for id in ids {
            if inner.leases.contains_key(&id) {
                continue;
            }
            match lease::acquire_in(&dir, limit as usize) {
                Some(lease) => {
                    inner.leases.insert(id, lease);
                }
                None => {
                    eprintln!(
                        "superpowers-kanban: {} is running but no slot could be re-adopted",
                        id.0
                    );
                }
            }
        }
    }

    /// 会话已起来：卡片进入 `Running`（从此由 `Running` 记账占槽），并记下 thread id。
    ///
    /// **幂等**：一张已 `Running` 的卡再次被上报（同一会话或追问后的续跑）不算
    /// 非法迁移，仅刷新 thread id。对账新增的 `awaiting_merge`/`needs_you → running`
    /// 回流正靠这里落地（见 2026-10-03 看板状态误判）。
    pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let already_running = inner
            .board
            .get(&id)
            .map(|card| card.state == CardState::Running)
            .unwrap_or(false);
        if !already_running {
            inner
                .board
                .transition(&id, CardState::Running)
                .map_err(|error| error.to_string())?;
        }
        inner
            .board
            .set_thread_id(&id, thread_id.to_string())
            .map_err(|error| error.to_string())?;
        // 会话活着 = 新一轮尝试：上一串合并轮失败的重试账清零，旧判词也不再成立。
        inner.merge_retries.remove(&id);
        let _ = inner.board.set_detail(&id, None);
        self.save(&inner);
        Ok(())
    }

    /// 会话收尾：迁移到某个非终态结果并释放槽位。
    ///
    /// `detail` 是这张卡的判词（失败时就是原因）；它落盘并经 `list` 暴露，
    /// 否则 spec §4.6 要求的「原因取 `error`」在对账通路上就悄悄丢了
    /// （`running + idle → needs_you` 这一步也用它说明「为什么没人合」）。
    pub fn mark_terminal(
        &self,
        card_id: &str,
        outcome: &str,
        detail: Option<&str>,
    ) -> Result<(), String> {
        let next = match outcome {
            "awaiting_merge" => CardState::AwaitingMerge,
            "needs_you" => CardState::NeedsYou,
            "failed" => CardState::Failed,
            other => return Err(format!("unknown outcome: {other}")),
        };
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner
            .board
            .transition(&id, next)
            .map_err(|error| error.to_string())?;
        inner
            .board
            .set_detail(&id, detail.map(str::to_string))
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id); // 释放槽位：drop 掉 flock
        self.save(&inner);
        Ok(())
    }

    /// 启动没成：把卡片标为 `Failed` 并释放槽位。`detail` 是失败原因，同样落盘可读。
    pub fn release(&self, card_id: &str, detail: &str) -> Result<(), String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        inner
            .board
            .transition(&id, CardState::Failed)
            .map_err(|error| error.to_string())?;
        inner
            .board
            .set_detail(&id, Some(detail.to_string()))
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id);
        self.save(&inner);
        Ok(())
    }

    /// 启动迁移：把两类「重启后已死」的卡片收干净，启动时调用一次。
    ///
    /// 1. 仍是 `Running` 但 `thread_id` 为空的历史卡片 → `NeedsYou`。这类卡片占用
    ///    着一个槽位，但实际上没有会话在跑（旧版本的记录里没有 thread id），迁移
    ///    它时必须一并把槽位让出来。
    /// 2. 仍是 `Launching` 的僵尸卡 → `Failed`。崩溃若发生在「save(board →
    ///    Launching)」之后、「mark_running」之前，board.json 会留下 `Launching`；
    ///    它既不被本函数（只看 `Running`）回收，也不被 `claim_next_launch`
    ///    （只选 `Queued`）选中，会永远卡住。启动时收成 `Failed` 并释放其可能
    ///    存在的 lease。
    /// 3. 仍是 `Merging` 的僵尸卡 → `NeedsYou`。崩溃若发生在「置 Merging」之后、
    ///    「回写终态」之前，合并可能只做了一半，需人确认，故不放行也不判死。
    pub fn migrate_legacy_running(&self) {
        let mut inner = self.lock();
        let legacy_running: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| {
                card.state == CardState::Running
                    && card.thread_id.as_deref().unwrap_or("").is_empty()
            })
            .map(|card| card.id.clone())
            .collect();
        for id in legacy_running {
            if inner.board.transition(&id, CardState::NeedsYou).is_ok() {
                inner.leases.remove(&id);
            }
        }
        let launching_zombies: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Launching)
            .map(|card| card.id.clone())
            .collect();
        for id in launching_zombies {
            if inner.board.transition(&id, CardState::Failed).is_ok() {
                inner.leases.remove(&id);
            }
        }
        // 崩溃若发生在「置 Merging」之后、「回写终态」之前，会留下 Merging 僵尸。
        // 合并可能半途，需人确认，故收成 NeedsYou（不是 Failed）。先尽力 abort。
        let merging: Vec<CardId> = inner
            .board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::Merging)
            .map(|card| card.id.clone())
            .collect();
        for id in merging {
            if let Some(card) = inner.board.get(&id).cloned() {
                if let Some(base) = card.base_ref.as_deref() {
                    if let Ok(wt) = merge::prepare(&self.project_root, base) {
                        let _ = std::process::Command::new("git")
                            .arg("-C")
                            .arg(&wt.path)
                            .args(["merge", "--abort"])
                            .output();
                        merge::cleanup(&wt);
                    }
                }
            }
            if inner.board.transition(&id, CardState::NeedsYou).is_ok() {
                inner.leases.remove(&id);
            }
        }
        self.save(&inner);
    }

    /// 消费 inbox 把投递排进**内存里的同一份看板**并落盘，返回每张投递的结果。
    ///
    /// 推进循环与查询服务共享同一实例，所以这里必须改内存板而不是另 load 一份
    /// `board.json` 再写回——否则会覆盖掉持有 lease 的卡片状态（`Launching`）。
    pub fn consume_inbox(&self, now: DateTime<Local>) -> Vec<InboxOutcome> {
        let mut inner = self.lock();
        let outcomes = inbox::consume(&self.state_dir, &mut inner.board, now);
        self.save(&inner);
        outcomes
    }

    /// 试做下一张合并卡。拿不到每项目合并闸即返回 `None`（本项目已有合并在跑）。
    ///
    /// 全程持 `merge.lock`：认领 → 在 base worktree 里 `git merge --no-ff` → 回写终态。
    /// 合并成功且卡片带 `origin_card` 时，把那张实现卡一并送到 `Done`。
    pub fn merge_next(&self) -> Result<Option<MergeClaim>, String> {
        let Some(_gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(None);
        };
        let mut inner = self.lock();
        let Some(card_id) = inner.board.claim_next_merge(false) else {
            return Ok(None);
        };
        let card = inner.board.get(&card_id).cloned().expect("just claimed");
        let source = card.source_ref.clone().unwrap_or_default();
        let base = card.base_ref.clone().unwrap_or_default();
        let origin = card.origin_card.clone();

        let finish = |inner: &mut Inner, next: CardState| {
            let _ = inner.board.transition(&card_id, next);
            if next == CardState::Done {
                if let Some(origin) = &origin {
                    let _ = inner.board.transition(origin, CardState::Done);
                }
            }
        };

        if !merge::source_branch_exists(&self.project_root, &source) {
            let detail = format!("source branch '{source}' does not exist");
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim {
                card_id: card_id.0,
                outcome: MergeOutcome::GitError(detail),
            }));
        }
        let base_wt = match merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt,
            Err(detail) => {
                finish(&mut inner, CardState::NeedsYou);
                self.save(&inner);
                return Ok(Some(MergeClaim {
                    card_id: card_id.0,
                    outcome: MergeOutcome::GitError(detail),
                }));
            }
        };
        if merge::is_dirty(&base_wt.path) {
            let detail = format!("base worktree {} is dirty", base_wt.path.display());
            merge::cleanup(&base_wt);
            finish(&mut inner, CardState::NeedsYou);
            self.save(&inner);
            return Ok(Some(MergeClaim {
                card_id: card_id.0,
                outcome: MergeOutcome::GitError(detail),
            }));
        }
        let outcome = merge::run(&base_wt.path, &source, &base);
        merge::cleanup(&base_wt);
        let next = match outcome {
            MergeOutcome::Merged => CardState::Done,
            MergeOutcome::Conflict => CardState::NeedsYou,
            MergeOutcome::GitError(_) => CardState::Failed,
        };
        finish(&mut inner, next);
        self.save(&inner);
        Ok(Some(MergeClaim {
            card_id: card_id.0,
            outcome,
        }))
    }

    /// 申请一次合并名额。
    ///
    /// 名额的两个来源：`merge.lock`（flock）挡住**同一进程/跨进程同时申请**，
    /// 卡片的 `Merging` 状态挡住**整轮合并期间**的第二次申请。二者缺一不可：
    /// 合并轮跨会话、持续数分钟，锁的 fd 无法跨轮保活，所以持久载体是卡片状态；
    /// 而状态检查本身有 TOCTOU 窗口，锁保证它原子。
    ///
    /// 拿不到锁即 `Busy`（本项目已有合并在跑），**不排队等待**。
    pub fn merge_request(&self, card_id: &str) -> Result<MergeRequest, String> {
        let Some(gate) = crate::merge_lock::acquire(&self.state_dir) else {
            return Ok(MergeRequest::Busy);
        };
        let result = self.merge_request_locked(card_id);
        drop(gate);
        result
    }

    /// `merge_request` 的临界区（调用方已持名颏锁）。
    fn merge_request_locked(&self, card_id: &str) -> Result<MergeRequest, String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let Some(card) = inner.board.get(&id).cloned() else {
            return Ok(MergeRequest::Denied(format!("unknown card: {card_id}")));
        };
        // 准入：等待验收、需要用户决定、以及**正在跑**的卡都可以申请名额。
        // 正在跑也要接受，是因为合并已改为「原会话里发话触发」——用户发话本身
        // 会让会话变忙，宿主对账随即把卡翻回 running；若这里仍拒绝 running，
        // 合并就永远申请不到名额（本 bug）。真正的闸是「用户显式发话」
        // （只有 CLI/skill 会调 merge_request，宿主从不自动调）、source 分支存在、
        // 以及每项目一把 merge.lock。
        if !matches!(
            card.state,
            CardState::AwaitingMerge | CardState::NeedsYou | CardState::Running
        ) {
            return Ok(MergeRequest::Denied(format!(
                "card {card_id} is {:?}, not awaiting_merge/needs_you/running",
                card.state
            )));
        }
        // refs：合并卡自带；实现卡按 id 推导 source、按项目默认分支取 base。
        let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
            (Some(source), Some(base)) => (source, base),
            _ => {
                if card.kind != CardKind::Implementation {
                    return Ok(MergeRequest::Denied(format!(
                        "card {card_id} has no source/base to merge"
                    )));
                }
                (
                    format!("kanban/{}", crate::worktree::slugify(&card.id)),
                    crate::merge::default_branch(&self.project_root),
                )
            }
        };
        if !merge::source_branch_exists(&self.project_root, &source) {
            // 同理，把「分支缺失」记进重试账：合并轮还没跑，卡仍是 awaiting_merge，
            // 自动重试只会在同一个坑里循环，该让 `needs_you` 的处置被看见。
            inner
                .merge_retries
                .entry(id.clone())
                .or_insert((0, String::new()))
                .1 = format!("source branch '{source}' does not exist");
            // 判词同样要可读：这张卡卡在 awaiting_merge 的原因就在这里。
            let _ = inner.board.set_detail(
                &id,
                Some(format!("source branch '{source}' does not exist")),
            );
            self.save(&inner);
            return Ok(MergeRequest::Denied(format!(
                "source branch '{source}' does not exist"
            )));
        }
        // 定位执行合并的 base worktree：base 已被检出就复用它（通常是主检出）——
        // git 不允许同一分支被两个 worktree 同时检出。仅当 base 无人检出时才新建。
        let workdir = match merge::prepare(&self.project_root, &base) {
            Ok(wt) => wt.path,
            Err(error) => return Ok(MergeRequest::Denied(error)),
        };
        inner
            .board
            .transition(&id, CardState::Merging)
            .map_err(|error| error.to_string())?;
        self.save(&inner);
        Ok(MergeRequest::Granted {
            workdir,
            source,
            base,
        })
    }

    /// 收尾一次合并轮：一律以 git 事实裁定，不看模型自述。
    ///
    /// `source` 已并入 `base` → `done`；否则退回 `needs_you` 等用户发话。
    /// 卡不在 `Merging`（例如已被别处推进）→ `Cleared`，不落状态。
    pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String> {
        let mut inner = self.lock();
        let id = CardId::new(card_id);
        let Some(card) = inner.board.get(&id).cloned() else {
            return Err(format!("unknown card: {card_id}"));
        };
        if card.state != CardState::Merging {
            return Ok(MergeFinish::Cleared);
        }
        let (source, base) = match (card.source_ref.clone(), card.base_ref.clone()) {
            (Some(source), Some(base)) => (source, base),
            _ => (
                format!("kanban/{}", crate::worktree::slugify(&card.id)),
                merge::default_branch(&self.project_root),
            ),
        };
        let merged = merge::source_branch_exists(&self.project_root, &source)
            && merge::branch_merged_into(&self.project_root, &source, &base);
        let (next, state) = if merged {
            (MergeFinish::Done, CardState::Done)
        } else {
            (MergeFinish::NeedsYou, CardState::NeedsYou)
        };
        inner
            .board
            .transition(&id, state)
            .map_err(|error| error.to_string())?;
        inner.leases.remove(&id);
        if next == MergeFinish::Done {
            // 终态：重试账不再有意义，旧判词也不该挂在一张已完成的卡上。
            inner.merge_retries.remove(&id);
            let _ = inner.board.set_detail(&id, None);
        } else {
            // git 判否的判词必须可读：人问「这张卡为什么停在这」的答案是
            // 一条 git 事实，而不是模型的复述（spec D10）。
            let _ = inner.board.set_detail(
                &id,
                Some(format!("git: {source} is not merged into {base}")),
            );
            // 合并轮失败：记下这是「可自动重试的 needs_you」（spec D9），
            // 由 `driver_step` 数着次数重开合并轮。这里也是唯一的记账点——
            // 不论复核是被 driver 还是被 CLI/桌面触发的。
            //
            // 之前记的原因保留：复核失败（分支未并入）是模糊事实，说不出具体
            // 缘故，带着上一轮的 git 错误更有用（例如「source 分支不存在」）。
            let entry = inner
                .merge_retries
                .entry(id.clone())
                .or_insert((0, String::new()));
            entry.0 = entry.0.saturating_add(1);
        }
        self.save(&inner);
        Ok(next)
    }

    /// 收敛扫描：把 `awaiting_merge` 的实现卡按 git 事实落地。
    ///
    /// 分支已并入 base → `done`；分支缺失 → 记入去重集（每 daemon 至多提示一次）。
    /// 返回**该打印的报告行**；本方法不打印（打印在 tick 循环），故报告可直接单测。
    /// 不占任何并发名额——收敛是簿记，`occupies_slot()` 仍只认 `Running`。
    pub fn reconcile_merged(&self) -> Vec<crate::reconcile::Reconcile> {
        use crate::reconcile::{self, RealGit, Reconcile};
        let mut inner = self.lock();
        let git = RealGit {
            project_root: &self.project_root,
        };
        // 1) 在共享借用 `inner.board` 内算候选报告；此步不碰去重集（借用规则）。
        let candidates = reconcile::plan(&inner.board, &git);
        // 2) 去重缺分支提示：命中即视为已提示，不再上报。
        let mut reports: Vec<Reconcile> = Vec::new();
        for report in candidates {
            match report {
                Reconcile::MissingBranch { ref id, .. } => {
                    if inner.notified_missing_branch.insert(id.clone()) {
                        reports.push(report);
                    }
                }
                other => reports.push(other),
            }
        }
        // 3) 落地 `done`；报告随之裁剪——只有真正迁移成功的卡才留报告。
        //    否则 tick 会为一张没动的卡打印「auto-converged to done」。
        let mut changed = false;
        reports.retain(|report| match report {
            Reconcile::Converged { id, .. } => {
                let moved = inner.board.transition(id, CardState::Done).is_ok();
                changed = changed || moved;
                moved
            }
            // 缺分支提示不动状态，恒留。
            Reconcile::MissingBranch { .. } => true,
        });
        if changed {
            self.save(&inner);
        }
        reports
    }

    /// 看板快照。控制面读的就是这个形状。
    pub fn list(&self) -> Value {
        let inner = self.lock();
        json!({
            "cards": inner.board.visible().map(|card| json!({
                "id": card.id.0,
                "state": card.state,
                "spec_path": card.spec_path,
                "plan_path": card.plan_path,
                "workdir": card.workdir,
                "thread_id": card.thread_id,
                "kind": card.kind,
                "source": card.source_ref,
                "base": card.base_ref,
                "origin_card": card.origin_card.as_ref().map(|id| id.0.clone()),
                "detail": card.detail,
            })).collect::<Vec<_>>(),
        })
    }

    /// 看板上未被占用的合并卡 id（供 dispatch / CLI 共用）。
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> String {
        let inner = self.lock();
        inner.board.next_free_merge_id(source, base).0
    }

    /// 仅测试用：把某张卡片的 `thread_id` 清成 `None`，模拟旧数据。
    ///
    /// 只搬字段、不碰产品逻辑；走一遍序列化契约而不是给 `Board` 开新的
    /// `&mut Card` 通道（那会为了测试扩大产品 API）。
    #[cfg(test)]
    pub(crate) fn clear_thread_id_for_test(&self, card_id: &str) {
        let mut inner = self.lock();
        let Ok(text) = std::fs::read_to_string(self.board_path()) else {
            return;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(cards) = value.get_mut("cards").and_then(Value::as_array_mut) {
            for card in cards {
                if card.get("id").and_then(Value::as_str) == Some(card_id) {
                    card["thread_id"] = Value::Null;
                }
            }
        }
        if let Ok(board) = serde_json::from_value::<Board>(value) {
            inner.board = board;
        }
        self.save(&inner);
    }

    /// 仅测试用：再排一张排队卡进内存 board 并落盘，供队列/名额用例搭台。
    #[cfg(test)]
    pub(crate) fn enqueue_card_for_test(&self, id: &str) {
        let mut inner = self.lock();
        inner.board.enqueue(
            CardId::new(id),
            PathBuf::from(format!("{id}.spec.md")),
            PathBuf::from(format!("{id}.plan.md")),
            chrono::Local::now(),
        );
        self.save(&inner);
    }

    /// 仅测试用：直接往内存 board 排一张合并卡并落盘，绕开 inbox 通路。
    #[cfg(test)]
    pub(crate) fn enqueue_merge_card_for_test(
        &self,
        id: &str,
        source: &str,
        base: &str,
        origin: Option<&str>,
    ) {
        let mut inner = self.lock();
        inner.board.enqueue_merge(
            CardId::new(id),
            source.to_string(),
            base.to_string(),
            origin.map(CardId::new),
            chrono::Local::now(),
        );
        self.save(&inner);
    }
}

/// 宿主报的这条会话闲不闲；`None` = 宿主账上没有这台会话，判不了。
fn idle_of(threads: &[(String, bool)], thread_id: &str) -> Option<bool> {
    threads
        .iter()
        .find(|(id, _)| id == thread_id)
        .map(|(_, idle)| *idle)
}

/// 这张卡启动后**有没有新提交**——`awaiting_merge`（有）与 `needs_you`（无）的唯一区别。
///
/// 对照基线是启动时记下的 `base_commit`，事实取自 worktree 当前的 HEAD。
/// 没有基线（迁移来的旧卡）或读不到 HEAD（worktree 没了）时保守取「有提交」：
/// 判成 `awaiting_merge` 还会经 git 复核，判错只多一个回合；
/// 而把真做完的会话判成「没产出」会让它干等人发话。
fn has_commits(card: &Card) -> bool {
    let Some(workdir) = card.workdir.as_deref() else {
        return true;
    };
    let Some(head) = effective_head(workdir) else {
        // worktree 没了：无从判断，保守取「有提交」。
        return true;
    };
    match card.base_commit.as_deref() {
        Some(base) => base != head,
        None => true,
    }
}

/// 会话标题：由 spec 文件名派生，如 `a.spec.md` → `看板 · a.spec`。
fn title_for(board: &Board, id: &CardId) -> String {
    let spec = board
        .get(id)
        .map(|card| card.spec_path.clone())
        .unwrap_or_default();
    let stem = spec
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| id.0.clone());
    format!("看板 · {stem}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::PathBuf;

    fn at() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 10, 1, 12, 0, 0)
            .single()
            .unwrap()
    }

    /// 建一个真实的最小 git 仓库，作为 project_root 与 worktree 的来源。
    ///
    /// 一并提交 `.gitignore`（忽略 `.yi-agent/`、`.worktrees/`），与真实项目一致：
    /// 看板的状态目录就落在项目根下的 `.yi-agent/`，若不忽略，项目主检出会被
    /// 自己的运行时状态显示成「脏」，合并前置校验（`merge::is_dirty`）会误判。
    fn project_with_worktree(dir: &std::path::Path) -> PathBuf {
        let out = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git init failed");
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        for args in [
            vec!["add", "README.md", ".gitignore"],
            vec![
                "-c",
                "user.email=e@e",
                "-c",
                "user.name=E",
                "commit",
                "-q",
                "-m",
                "seed",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        dir.to_path_buf()
    }

    fn service_with_card(dir: &std::path::Path) -> BoardService {
        let state_dir = dir.join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        // home 指到临时目录，让 lease 落在隔离目录，不污染真实 HOME。
        BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
    }

    /// 再加一张排队卡（走内存 board + 落盘，与 `service_with_card` 同一份）。
    fn idle(ids: &[&str]) -> Vec<(String, bool)> {
        ids.iter().map(|id| (id.to_string(), true)).collect()
    }

    /// 宿主说这些会话都还忙着。
    fn busy(ids: &[&str]) -> Vec<(String, bool)> {
        ids.iter().map(|id| (id.to_string(), false)).collect()
    }

    /// 一拍 `driver_step`，断言它交出了一个 launch 并把它交出来。
    fn launched(service: &BoardService) -> DriverAction {
        service
            .driver_step(&[], &[])
            .unwrap()
            .expect("a launch action")
    }

    /// 把并发配额钉成 1（写进项目日历），用来验「名额限制」。
    fn one_slot_only(service: &BoardService) {
        let calendar = superpowers_kanban_core::calendar::ConcurrencyCalendar {
            default_max_tasks: 1,
            ..Default::default()
        };
        calendar.save(&service.state_dir).unwrap();
    }

    #[test]
    fn list_hides_archived_cards_but_archive_due_sweeps_terminal_ones() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        let id = superpowers_kanban_core::card::CardId::new("c1");
        board.enqueue(
            id.clone(),
            "c.spec.md".into(),
            "c.plan.md".into(),
            chrono::Local::now(),
        );
        board
            .transition(&id, superpowers_kanban_core::card::CardState::Running)
            .unwrap();
        board
            .transition(&id, superpowers_kanban_core::card::CardState::Cancelled)
            .unwrap();
        crate::persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir.clone(), dir.path().to_path_buf(), None);
        // 默认宽限 24h；把「现在」推到 3 天后即超期。
        let swept = service.archive_due(chrono::Local::now() + chrono::Duration::days(3));
        assert_eq!(swept, vec![id], "超期终态卡被自动归档");
        assert_eq!(
            service.list()["cards"].as_array().unwrap().len(),
            0,
            "归档卡不出现在 list（宿主/桌面随之不显示）"
        );
    }

    #[test]
    fn next_launch_claims_the_head_card_and_creates_its_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);

        let claim = service.next_launch(3, at()).unwrap().expect("a claim");
        assert_eq!(claim.card_id, "card-1");
        assert!(claim.workdir.join(".git").exists(), "worktree was created");
        assert_eq!(claim.title, "看板 · a.spec");

        // 再取一次：没有第二张排队卡 → None。
        assert!(service.next_launch(3, at()).unwrap().is_none());
    }

    #[test]
    fn next_launch_is_gated_by_the_slot_limit() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(
            service.next_launch(0, at()).unwrap().is_none(),
            "no slot -> no claim"
        );
        // 占用那张卡后，名额用尽。
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "slot taken"
        );
    }

    #[test]
    fn mark_terminal_releases_the_slot_and_records_the_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();
        let listed = service.list();
        let card = &listed["cards"][0];
        assert_eq!(card["state"], "awaiting_merge");
        assert_eq!(card["thread_id"], "thread-1");
        // 名额已释放：再放一张排队卡即可启动（此处队空，验证 free_slots 间接由 next_launch None 体现）。
        assert!(service.next_launch(1, at()).unwrap().is_none());
    }

    #[test]
    fn mark_running_is_idempotent_and_revives_a_reconciled_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();
        // 会话续跑（追问）：卡片必须能翻回 running，且不因重复上报报错。
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert_eq!(service.list()["cards"][0]["state"], "running");
    }

    #[test]
    fn release_fails_the_card_and_frees_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service
            .release(&claim.card_id, "thread/start failed")
            .unwrap();
        assert_eq!(service.list()["cards"][0]["state"], "failed");
    }

    #[test]
    fn a_legacy_running_card_without_a_thread_becomes_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 模拟旧数据：把 thread_id 清成 None（在 BoardService 上加一个
        // `#[cfg(test)] pub(crate) fn clear_thread_id_for_test`，只搬字段，不碰产品逻辑）。
        service.clear_thread_id_for_test("card-1");
        service.migrate_legacy_running();
        assert_eq!(service.list()["cards"][0]["state"], "needs_you");
    }

    /// Step A：重启后进程内存 lease 为空，board 里仍在 `Running` 的卡片必须补领
    /// 全局名额；否则全局池少算了它的占用，别的项目会拿到本该属于它的名额而超发。
    #[test]
    fn adopt_running_leases_reclaims_the_slot_of_a_card_that_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();
        let leases_dir = dir.path().join("home/.yi-agent/superpowers-kanban/leases");

        // board.json：一张正在跑（有 thread id）、一张排队。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        board.enqueue(
            CardId::new("card-2"),
            PathBuf::from("b.spec.md"),
            PathBuf::from("b.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("card-1"), CardState::Running)
            .unwrap();
        board
            .set_thread_id(&CardId::new("card-1"), "thread-1".to_string())
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        // 补领之前：全局池是空的，别的领取者能抢到那张在跑卡片本该占的名额。
        // 探针拿到的租约必须在 `adopt_running_leases` **之前** drop，好让池子
        // 显得空的；但 `drop` 之后 flock 是内核**异步**释放的，紧接着的补领
        // 会撞上正在释放的锁而误拿到名额（实测并行跑全套时偶发，症状为下面
        // 「the running card's slot is re-adopted」断言失败）。所以先探出
        // 「池子空」，再**有界等待**该探针租约真正释放，确认池子干净，才 adopt。
        let probe = lease::acquire_in(&leases_dir, 1);
        assert!(probe.is_some(), "restart starts with an empty pool");
        drop(probe);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Some(lease) = lease::acquire_in(&leases_dir, 1) {
                drop(lease);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        service.adopt_running_leases(1);
        // 补领之后：唯一的名额被在跑的卡片占住。
        assert!(
            lease::acquire_in(&leases_dir, 1).is_none(),
            "the running card's slot is re-adopted"
        );
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "limit=1 and the running card already fills it"
        );
        assert_eq!(
            service.list()["cards"][1]["state"],
            "queued",
            "card-2 must not be claimed while the slot is taken"
        );
    }

    /// Step B：崩溃若发生在 save(board → Launching) 之后、mark_running 之前，
    /// board.json 会留下 `Launching` 僵尸卡；启动迁移必须把它收成 `Failed`，
    /// 否则 `migrate_legacy_running` 只认 `Running`、`claim_next_launch` 只认
    /// `Queued`，没人再回收它。
    #[test]
    fn a_launching_card_left_by_a_crash_becomes_failed_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut board = Board::new();
        board.enqueue(
            CardId::new("card-1"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("card-1"), CardState::Launching)
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        service.migrate_legacy_running();
        assert_eq!(
            service.list()["cards"][0]["state"],
            "failed",
            "a Launching zombie must be reaped at startup"
        );
    }

    #[test]
    fn a_merge_card_merges_its_source_and_sends_the_origin_card_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 造一个 source 分支：feat/x 在 main 之外加一个文件。
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["checkout", "-qb", "feat/x"]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git(&["add", "x.txt"]);
        git(&["commit", "-qm", "feat"]);
        git(&["checkout", "-q", "main"]);

        let service = service_with_card(&project);
        // 加一张实现卡（配对的 origin）与一张合并卡。
        service.enqueue_merge_card_for_test("m1", "feat/x", "main", Some("card-1"));
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();

        let claim = service.merge_next().unwrap().expect("a merge claim");
        assert_eq!(claim.card_id, "m1");
        let listed = service.list();
        let cards = listed["cards"].as_array().unwrap();
        let states: std::collections::HashMap<_, _> = cards
            .iter()
            .map(|c| {
                (
                    c["id"].as_str().unwrap().to_string(),
                    c["state"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(states["m1"], "done");
        assert_eq!(
            states["card-1"], "done",
            "merge success drives the origin card to done"
        );
    }

    /// 崩溃若发生在「置 Merging」之后、「回写终态」之前，board.json 会留下 `Merging`
    /// 僵尸卡。合并可能只做了一半，需人确认，故启动迁移收成 `NeedsYou`（不是 `Failed`）。
    #[test]
    fn a_merging_card_left_by_a_crash_becomes_needs_you_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        let mut board = Board::new();
        board.enqueue_merge(
            CardId::new("m1"),
            "feat/x".to_string(),
            "main".to_string(),
            Some(CardId::new("card-1")),
            at(),
        );
        board
            .transition(&CardId::new("m1"), CardState::Merging)
            .unwrap();
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        service.migrate_legacy_running();
        assert_eq!(
            service.list()["cards"][0]["state"],
            "needs_you",
            "a Merging zombie must be reaped for human confirmation at startup"
        );
    }

    /// `board_auto_merge` 打开也不再派生合并卡：合并已改为会话驱动（手动），
    /// 自动派生会让同一目标出现第二条无人发话就能改 main 的路径。
    #[test]
    fn awaiting_merge_never_derives_a_merge_card_even_with_the_preference_on() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 打开开关：旧行为会在这里派生一张合并卡。
        let pref = superpowers_kanban_core::layout::project_preferences_path(&service.state_dir);
        superpowers_kanban_core::switch::write_bool(&pref, "board_auto_merge", true).unwrap();

        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .mark_terminal(&claim.card_id, "awaiting_merge", None)
            .unwrap();

        let cards = service.list()["cards"].as_array().unwrap().clone();
        assert_eq!(cards.len(), 1, "只该有那张实现卡：{cards:?}");
        assert_eq!(cards[0]["kind"], "implementation");
    }

    /// 一张停在 `AwaitingMerge` 的实现卡，source 分支按 `kanban/<slug>` 推导。
    /// 走真实生产路径：Queued → Running → AwaitingMerge。
    fn service_with_awaiting_card(project: &std::path::Path) -> (BoardService, String) {
        let service = service_with_card(project);
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();
        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        (service, format!("kanban/{slug}"))
    }

    fn git_run(project: &std::path::Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(project)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    }

    /// 读一张卡的当前状态字符串（经 `list`，与桌面/宿主同口径）。
    fn state_of(service: &BoardService, id: &str) -> String {
        service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["id"] == id)
            .map(|card| card["state"].as_str().unwrap().to_string())
            .unwrap_or_else(|| panic!("card {id} not on the board"))
    }

    /// 读一张卡的判词（经 `list`：这就是「人能读到的地方」）。
    ///
    /// 「这张卡为什么失败」必须能从这里问出答案（spec §4.6：失败 → 原因取
    /// `error`），而不是只活在 daemon 的 stderr 里。
    fn detail_of(service: &BoardService, id: &str) -> Option<String> {
        service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["id"] == id)
            .and_then(|card| card["detail"].as_str().map(str::to_string))
    }

    /// 分支已并入 base（分支指向与 main 同一提交）→ 卡落 `done`。
    #[test]
    fn reconcile_converges_a_merged_card_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);

        let reports = service.reconcile_merged();
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(
            matches!(reports[0], crate::reconcile::Reconcile::Converged { .. }),
            "{reports:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "done");
    }

    /// 分支存在但未并入 → 卡保持 `awaiting_merge`，不产报告。
    #[test]
    fn reconcile_waits_on_an_existing_unmerged_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 分支上多一个未进 main 的提交。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("more.txt"), "wip").unwrap();
        git_run(&project, &["add", "-A"]);
        git_run(&project, &["commit", "-q", "-m", "wip"]);
        git_run(&project, &["checkout", "-q", "main"]);

        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
    }

    /// 分支缺失 → 卡保持 `awaiting_merge`，且只提示一次。
    #[test]
    fn reconcile_reports_a_missing_branch_once() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_awaiting_card(&project);
        // 不建分支 → 缺分支。

        let first = service.reconcile_merged();
        assert_eq!(first.len(), 1, "{first:?}");
        assert!(
            matches!(first[0], crate::reconcile::Reconcile::MissingBranch { .. }),
            "{first:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        // 去重：第二次不再报告。
        assert!(service.reconcile_merged().is_empty());
    }

    /// `needs_you` 的卡不是候选：无论如何都不动。
    #[test]
    fn reconcile_leaves_a_needs_you_card_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        service.mark_running("card-1", "thread-2").unwrap();
        service.mark_terminal("card-1", "needs_you", None).unwrap();
        git_run(&project, &["branch", &source]); // 即使分支已并入
        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "card-1"), "needs_you");
    }

    /// 合并卡不是候选：收敛扫描绝不碰它。
    #[test]
    fn reconcile_leaves_merge_cards_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_merge_card_for_test("m1", "kanban/card-1", "main", None);
        assert!(service.reconcile_merged().is_empty());
        assert_eq!(state_of(&service, "m1"), "queued");
    }

    #[test]
    fn merge_request_denies_a_card_that_is_not_waiting_to_merge() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 一张仍排队的实现卡：没到 awaiting_merge/needs_you。
        let service = service_with_card(&project);
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => {
                assert!(reason.contains("not awaiting_merge"), "{reason}")
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn merge_request_is_busy_while_the_gate_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        // 模拟「另一处正持着 merge.lock」。
        let held = crate::merge_lock::acquire(&service.state_dir).expect("hold the gate");
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Busy => {}
            other => panic!("expected Busy, got {other:?}"),
        }
        drop(held);
    }

    #[test]
    fn merge_request_grants_and_marks_the_card_merging() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);

        match service.merge_request("card-1").unwrap() {
            MergeRequest::Granted {
                workdir,
                source: s,
                base,
            } => {
                assert_eq!(s, source);
                assert_eq!(base, "main");
                // base=main 由主检出占用 → 复用主检出，不新建 worktree。
                assert_eq!(workdir, std::fs::canonicalize(&project).unwrap());
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert_eq!(service.list()["cards"][0]["state"], "merging");
        // 已在 merging：第二次申请被拒（名额的持久载体是卡片状态）。
        // 这里要有界重试：第一次 `merge_request` 在返回前 drop 了 merge.lock，
        // 而内核在 `close()` 后**异步**释放 flock，负载下紧随其后的获取会撞上
        // 正在释放的那把锁而拿到 `Busy`（实测并行跑全套时偶发）。`Busy` 与
        // 「卡已在 merging」是两回事，只有 `Denied` 才说明额度确实被钉住；
        // 但 `Busy` 是瞬时窗口，重试即可穿过。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match service.merge_request("card-1").unwrap() {
                MergeRequest::Denied(reason) => {
                    assert!(reason.contains("not awaiting_merge"), "{reason}");
                    break;
                }
                MergeRequest::Busy if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                other => panic!("expected Denied on second request, got {other:?}"),
            }
        }
    }

    /// 一张正在跑的实现卡（宿主对账已把它翻回 running）：用户发话要求合并
    /// 仍应拿到名额。这是本 bug 的回归锁。
    #[test]
    fn merge_request_grants_for_a_running_card_whose_session_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 走生产路径：启动 → awaiting_merge → 会话又忙（对账翻回 running）。
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();
        service.mark_running("card-1", "thread-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "running");

        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        let source = format!("kanban/{slug}");
        git_run(&project, &["branch", &source]);

        match service.merge_request("card-1").unwrap() {
            MergeRequest::Granted {
                source: s, base, ..
            } => {
                assert_eq!(s, source);
                assert_eq!(base, "main");
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert_eq!(state_of(&service, "card-1"), "merging");
        // 已在 merging 的卡绝不能回流会话通路：宿主对账即便把卡报成 running，
        // 这条迁移也会被状态机拒绝。额度已由卡片状态钉住（第二次申请会因卡不在
        // 准入集而被拒），此处不再补一次 `merge_request`——那会依赖 flock 释放的
        // 时序（macOS 上 drop 后释放有延迟，既有 `lease.rs` 用例亦因此偶发），
        // 把本用例变成 flaky。
        assert!(
            !superpowers_kanban_core::card::CardState::Merging
                .can_transition_to(superpowers_kanban_core::card::CardState::Running),
            "merging → running 必须非法，否则合并中会被对账翻回会话"
        );
    }

    #[test]
    fn merge_request_denies_when_the_source_branch_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_awaiting_card(&project);
        // 不建分支 → refuse。
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => assert!(reason.contains("does not exist"), "{reason}"),
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn merge_finish_verifies_with_git_and_settles_the_card_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 造分支、加提交、合进 main。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git_run(&project, &["add", "x.txt"]);
        git_run(&project, &["commit", "-qm", "feat"]);
        git_run(&project, &["checkout", "-q", "main"]);
        git_run(&project, &["merge", "--no-ff", "-m", "merge", &source]);

        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));
        assert_eq!(service.merge_finish("card-1").unwrap(), MergeFinish::Done);
        assert_eq!(service.list()["cards"][0]["state"], "done");
    }

    #[test]
    fn merge_finish_sends_an_unmerged_card_back_to_needs_you() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 分支存在但**没**合进 main。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git_run(&project, &["add", "x.txt"]);
        git_run(&project, &["commit", "-qm", "feat"]);
        git_run(&project, &["checkout", "-q", "main"]);

        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::NeedsYou
        );
        assert_eq!(service.list()["cards"][0]["state"], "needs_you");
    }

    #[test]
    fn merge_finish_reports_cleared_when_the_card_is_not_merging() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 排队中的卡不在 merging → Cleared，且状态不动。
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::Cleared
        );
        assert_eq!(service.list()["cards"][0]["state"], "queued");
    }

    #[test]
    fn merge_finish_rejects_an_unknown_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(
            service
                .merge_finish("nope")
                .unwrap_err()
                .contains("unknown card")
        );
    }

    #[test]
    fn a_merging_card_does_not_consume_a_session_slot() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));
        // 合并中不占会话名额：全文唯一占槽判定仍是 Running。
        let board = persist::load_board(&service.state_dir.join("board.json"));
        let card = board.get(&CardId::new("card-1")).unwrap();
        assert!(!card.state.occupies_slot());
    }

    /// 队列里两张 `queued` 卡：`driver_step` 只交出 `order` 最小的那张 launch，
    /// 且至多一个动作（spec D1）。
    #[test]
    fn driver_step_launches_the_head_of_the_queue_when_a_slot_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");

        let action = service.driver_step(&[], &[]).unwrap().expect("a launch");
        match action {
            DriverAction::Launch {
                card_id,
                workdir,
                objective,
                ..
            } => {
                assert_eq!(card_id, "card-1", "队首（order 最小）先走");
                assert!(workdir.join(".git").exists(), "worktree 由插件建好");
                // objective 由插件给出，措辞是契约（宿主不再复刻）。
                assert!(objective.contains("a.plan.md"), "{objective}");
                assert!(objective.contains("a.spec.md"), "{objective}");
                assert!(objective.contains("Never merge yourself"), "{objective}");
            }
            other => panic!("expected a launch, got {other:?}"),
        }
        assert_eq!(state_of(&service, "card-1"), "launching");
        assert_eq!(state_of(&service, "card-2"), "queued", "一拍只走一张");
    }

    /// 名额（复用 `next_launch` 的租约记账）：名额占满时不再交出 launch。
    #[test]
    fn driver_step_stops_launching_at_the_slot_limit() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        one_slot_only(&service);

        let action = service
            .driver_step(&[], &[])
            .unwrap()
            .expect("the head launches");
        assert!(matches!(action, DriverAction::Launch { ref card_id, .. } if card_id == "card-1"));
        // 第一张还持着名额（`Launching` 也占租约）→ 第二张不出。
        assert!(
            service.driver_step(&[], &[]).unwrap().is_none(),
            "no slot -> no launch"
        );
        assert_eq!(state_of(&service, "card-2"), "queued");
    }

    /// 卡 `running` + 它的会话 `idle` → 状态变 `awaiting_merge`（对账本身不交动作）。
    #[test]
    fn driver_step_marks_a_running_card_awaiting_merge_when_its_session_goes_idle() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 会话在 worktree 里留下一个提交：有产出 → 等验收。
        std::fs::write(claim.workdir.join("x.txt"), "x\n").unwrap();
        git_run(&claim.workdir, &["add", "-A"]);
        git_run(&claim.workdir, &["commit", "-qm", "feat"]);

        let action = service.driver_step(&idle(&["thread-1"]), &[]).unwrap();
        assert!(action.is_none(), "对账不向宿主交动作：{:?}", action);
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
    }

    /// 会话跑完但没有提交 → `needs_you`（等人发话），不是 `awaiting_merge`（spec §4.6）。
    #[test]
    fn driver_step_marks_a_running_card_needs_you_when_its_session_ended_without_commits() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 不提交：worktree 仍停在启动基线。

        let action = service.driver_step(&idle(&["thread-1"]), &[]).unwrap();
        assert!(action.is_none(), "{action:?}");
        assert_eq!(state_of(&service, "card-1"), "needs_you");
    }

    /// 宿主说会话还忙、或这台会话压根不在宿主账上：都不下结论（宿主只报 `idle`）。
    #[test]
    fn driver_step_leaves_a_running_card_alone_while_its_session_is_not_idle() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        std::fs::write(claim.workdir.join("x.txt"), "x\n").unwrap();

        assert!(
            service
                .driver_step(&busy(&["thread-1"]), &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running", "还忙就不动它");
        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(
            state_of(&service, "card-1"),
            "running",
            "宿主不认识的会话不动它"
        );
    }

    /// 停在验收态而会话又忙起来（追问/续跑）→ 卡必须翻回 `running`：
    /// 否则看板会长期停在错误的状态（2026-10-03 看板状态误判）。
    #[test]
    fn driver_step_revives_an_awaiting_merge_card_whose_session_is_busy_again() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_awaiting_card(&project);

        let action = service.driver_step(&busy(&["thread-1"]), &[]).unwrap();
        assert!(action.is_none(), "{action:?}");
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    /// 卡 `awaiting_merge` + 其会话空闲 → 交出 `append`（objective 指名 source 与 base），
    /// 且卡转 `merging`（名额的持久载体是卡片状态）。
    #[test]
    fn driver_step_emits_an_append_for_a_card_awaiting_merge_once_the_gate_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);

        let action = service
            .driver_step(&idle(&["thread-1"]), &[])
            .unwrap()
            .expect("an append for the merge turn");
        match action {
            DriverAction::Append {
                card_id,
                thread_id,
                objective,
                ..
            } => {
                assert_eq!(card_id, "card-1");
                assert_eq!(thread_id, "thread-1", "追加回原会话，不新建");
                assert!(
                    objective.contains(&source),
                    "objective 要指名 source：{objective}"
                );
                assert!(
                    objective.contains("main"),
                    "objective 要指名 base：{objective}"
                );
                assert!(
                    !objective.contains("Never merge yourself"),
                    "合并轮要明确要求执行合并：{objective}"
                );
            }
            other => panic!("expected an append, got {other:?}"),
        }
        assert_eq!(state_of(&service, "card-1"), "merging");
    }

    /// 已有卡在 `merging`（名额被占）→ 第二张 `awaiting_merge` 卡不出 `append`。
    #[test]
    fn driver_step_does_not_append_twice_while_a_merge_is_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        // card-1：走真实路径进合并轮（名额被占）。
        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        let source = format!("kanban/{slug}");
        git_run(&project, &["branch", &source]);
        service.mark_running("card-1", "thread-1").unwrap();
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();
        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));
        // card-2：也停在 awaiting_merge，会话空闲，够资格合并。
        service.mark_running("card-2", "thread-2").unwrap();
        service
            .mark_terminal("card-2", "awaiting_merge", None)
            .unwrap();

        // card-1 的会话仍忙（合并进行中）；card-2 空闲。
        let action = service
            .driver_step(
                &[
                    ("thread-1".to_string(), false),
                    ("thread-2".to_string(), true),
                ],
                &[],
            )
            .unwrap();
        assert!(action.is_none(), "一次只跑一个合并：{action:?}");
        assert_eq!(state_of(&service, "card-1"), "merging");
        assert_eq!(
            state_of(&service, "card-2"),
            "awaiting_merge",
            "第二张原地等名额"
        );
    }

    /// 合并轮收尾：`merging` 卡的会话再次空闲 → 插件代调 `merge_finish`，
    /// 以 git 事实裁定（分支已并入 → `done`）。
    #[test]
    fn driver_step_settles_a_merging_card_with_the_git_verified_merge_finish() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));

        let action = service.driver_step(&idle(&["thread-1"]), &[]).unwrap();
        assert!(action.is_none(), "{action:?}");
        assert_eq!(state_of(&service, "card-1"), "done", "以 git 事实收尾");
    }

    /// 本拍刚交出的合并轮：回报还没回来时不得抢着收尾——那时 turn 可能尚未真
    /// 正启动，`merge_finish` 会把一个还没跑的合并判成失败。
    #[test]
    fn driver_step_does_not_settle_a_merge_turn_it_just_dispatched() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);

        let action = service
            .driver_step(&idle(&["thread-1"]), &[])
            .unwrap()
            .expect("an append");
        let DriverAction::Append { action_id, .. } = action else {
            panic!("expected an append")
        };
        // 下一拍：会话快照仍空闲（turn 尚未登记），但回报未到 → 不收尾。
        assert!(
            service
                .driver_step(&idle(&["thread-1"]), &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state_of(&service, "card-1"),
            "merging",
            "不能抢在 turn 之前收尾"
        );
        // 回报到了（turn 确实被投递）→ 会话空闲即可收尾。
        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
        };
        assert!(
            service
                .driver_step(&idle(&["thread-1"]), &[result])
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "done");
    }

    /// 宿主账上没有这台会话 → 判不了它跑完没有，不动（与宿主旧 `plan()` 同型）。
    #[test]
    fn driver_step_leaves_an_untracked_merging_card_alone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Granted { .. }
        ));

        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(state_of(&service, "card-1"), "merging");
    }

    /// D12/D5：launch 结果带回 `thread_id` → `mark_running`（映射由插件持有）。
    #[test]
    fn driver_step_marks_the_card_running_from_a_launch_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            unreachable!()
        };
        assert_eq!(state_of(&service, "card-1"), "launching");

        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
        };
        assert!(
            service
                .driver_step(&busy(&["thread-1"]), &[result])
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        assert_eq!(
            service.list()["cards"][0]["thread_id"],
            "thread-1",
            "thread_id 由结果回报带回"
        );
    }

    /// launch 失败回报 → 卡 `failed`（原因取 `error`）**并让出名额**：
    /// 失败的启动是可恢复的，不是死胡同。
    #[test]
    fn driver_step_fails_the_card_when_a_launch_result_reports_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            unreachable!()
        };

        let result = DriverResult {
            action_id,
            ok: false,
            thread_id: None,
            error: Some("worktree not found".to_string()),
        };
        // 同一拍就把名额让出来给队里下一张：失败的启动不是死胡同。
        let next = service
            .driver_step(&[], &[result])
            .unwrap()
            .expect("the freed slot goes to the next card");
        assert!(
            matches!(next, DriverAction::Launch { ref card_id, .. } if card_id == "card-2"),
            "{next:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "failed");
        assert_eq!(state_of(&service, "card-2"), "launching");
        // spec §4.6：「失败 → 卡 `failed`（原因取 `error`）」。原因必须**可读**——
        // 经 `list` 落到卡片上，而不是只出现在 daemon 的 stderr 里（人问的是
        // 「这张卡为什么失败」）。
        assert_eq!(
            detail_of(&service, "card-1").as_deref(),
            Some("worktree not found"),
            "失败原因取回报的 `error`，且经 list 可读"
        );
        assert_eq!(
            detail_of(&service, "card-2"),
            None,
            "刚交出去、还在跑的卡不该挂着别人的判词"
        );
    }

    /// 回报 `ok:false` 却没带 `error`：仍要有话说（兜底判词），不能留一张
    /// 「失败了但说不出为什么」的卡。
    #[test]
    fn a_launch_failure_without_a_reason_still_records_one() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            unreachable!()
        };

        let result = DriverResult {
            action_id,
            ok: false,
            thread_id: None,
            error: None,
        };
        service.driver_step(&[], &[result]).unwrap();

        assert_eq!(state_of(&service, "card-1"), "failed");
        assert_eq!(
            detail_of(&service, "card-1").as_deref(),
            Some("launch failed")
        );
    }

    /// `board.release` 是同一处旧缺口（启动没成 → `failed`）：它的 `detail`
    /// 也必须落到卡片上，否则「为什么失败」在这条通路上照样无人可答。
    #[test]
    fn release_records_the_detail_it_was_given() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service
            .release(&claim.card_id, "thread/start failed")
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "failed");
        assert_eq!(
            detail_of(&service, "card-1").as_deref(),
            Some("thread/start failed")
        );
    }

    /// worktree 建不出来时 `next_launch` 自己把卡记为 `failed`：那条原因同样
    /// 要留下来（这是全自动路径上最常见的失败）。
    #[test]
    fn a_worktree_that_cannot_be_created_is_recorded_as_the_failure_reason() {
        let dir = tempfile::tempdir().unwrap();
        // 故意不是 git 仓库：`git worktree add` 必然失败。
        let service = service_with_card(dir.path());
        let error = match service.next_launch(1, at()) {
            Err(error) => error,
            Ok(_) => panic!("不是 git 仓库，worktree 必然建不出来"),
        };
        assert!(error.contains("worktree for card-1 failed"), "{error}");
        assert_eq!(state_of(&service, "card-1"), "failed");
        let detail = detail_of(&service, "card-1").expect("原因必须可读");
        assert!(detail.contains("worktree for card-1 failed"), "{detail}");
    }

    /// at-least-once：同一条回报重复投递只生效一次，且不重复占名额。
    #[test]
    fn driver_step_ignores_a_redelivered_launch_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        one_slot_only(&service);
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            unreachable!()
        };

        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
        };
        assert!(
            service
                .driver_step(&[], std::slice::from_ref(&result))
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        // 重复投递：不报错、不改状态、不多占一个名额。
        assert!(
            service
                .driver_step(&[], std::slice::from_ref(&result))
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        assert_eq!(
            service.list()["cards"][0]["thread_id"],
            "thread-1",
            "重复上报只刷新，不添乱"
        );
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "名额仍只有一个被占着"
        );
    }

    /// 交出的动作 JSON 就是跨进程契约：字段名与宿主逐字对齐，且**不含** `card_id`
    /// （宿主不认识看板，spec D3）。
    #[test]
    fn the_driver_action_json_uses_the_agreed_field_names() {
        let launch = DriverAction::Launch {
            action_id: "act-1".to_string(),
            card_id: "card-1".to_string(),
            workdir: PathBuf::from("/w/card-1"),
            title: "看板 · a.spec".to_string(),
            objective: "objective".to_string(),
        };
        let value = launch.to_json();
        assert_eq!(value["action_id"], "act-1");
        assert_eq!(value["kind"], "launch");
        assert_eq!(value["workdir"], "/w/card-1");
        assert_eq!(value["title"], "看板 · a.spec");
        assert_eq!(value["objective"], "objective");
        assert!(value.get("card_id").is_none(), "{value}");

        let append = DriverAction::Append {
            action_id: "act-2".to_string(),
            card_id: "card-1".to_string(),
            thread_id: "thread-1".to_string(),
            objective: "objective".to_string(),
        };
        let value = append.to_json();
        assert_eq!(value["action_id"], "act-2");
        assert_eq!(value["kind"], "append");
        assert_eq!(value["thread_id"], "thread-1");
        assert_eq!(value["objective"], "objective");
        assert!(value.get("card_id").is_none(), "{value}");
    }

    /// 合并轮失败（分支未并入）→ 卡退回 `needs_you`，但它是**可自动重试**的
    /// （spec D9）；重试有界，免得环境性失败一直烧模型回合。旧行为是「无入口的
    /// 死胡同」，这条用例同时锁住「能自动再入轮」与「有上限」两面。
    #[test]
    fn driver_step_retries_a_failed_merge_round_but_only_a_bounded_number_of_times() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 分支存在但**没**并入 main：每轮复核都判失败。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git_run(&project, &["add", "x.txt"]);
        git_run(&project, &["commit", "-qm", "feat"]);
        git_run(&project, &["checkout", "-q", "main"]);

        let mut appends = 0;
        let mut pending: Option<DriverResult> = None;
        // 跑足够多拍：每轮合并都要「开局 → 回报 → 空闲复核判失败 → 再开局」，
        // 不在首次 None 上就停（收尾那一拍本来就没有动作可交）。
        for _ in 0..30 {
            let results: Vec<DriverResult> = pending.take().into_iter().collect();
            match service.driver_step(&idle(&["thread-1"]), &results).unwrap() {
                Some(DriverAction::Append { action_id, .. }) => {
                    appends += 1;
                    pending = Some(DriverResult {
                        action_id,
                        ok: true,
                        thread_id: Some("thread-1".to_string()),
                        error: None,
                    });
                }
                Some(other) => panic!("unexpected action: {other:?}"),
                None => {}
            }
        }
        assert_eq!(
            appends,
            BoardService::MAX_MERGE_RETRIES as usize + 1,
            "首轮 + N 次自动重试，此后不再自动进轮"
        );
        assert_eq!(state_of(&service, "card-1"), "needs_you", "到顶后等人发话");
    }

    /// spec §8 第 8 条否定那一半的**判词**断言：分支存在但**没**并入 base 时，
    /// `driver_step` 让 `merge_finish` 跑出来的结论必须真是「git 判否」——
    /// 卡落到 `needs_you`。
    ///
    /// 为什么不能只数 append 次数：`Cleared`（卡不在 `Merging`，不落状态）会让卡
    /// 停在 `merging`，而计数、最终态可能照样「看起来对」；`merge_retries` 若被
    /// 重复计数，次数会偏少但终态仍是 `needs_you`。判词要单独钉住。
    ///
    /// 判词与「能再次入轮」分两拍看：收尾那一拍的合并轮开局用的是**收尾前**的
    /// 快照，卡当时还是 `merging`，所以那一拍只落判词、不开新轮——判负的
    /// `needs_you` 因此是可观察的；下一拍才重新入轮（D9 的可重试）。
    #[test]
    fn driver_step_settles_a_git_negative_merge_round_as_needs_you_and_retries_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        // 分支存在，但带着 main 没有的提交：git 事实就是「没并入」。
        git_run(&project, &["checkout", "-q", "-b", &source]);
        std::fs::write(project.join("x.txt"), "x\n").unwrap();
        git_run(&project, &["add", "x.txt"]);
        git_run(&project, &["commit", "-qm", "feat"]);
        git_run(&project, &["checkout", "-q", "main"]);
        assert!(
            !crate::merge::branch_merged_into(&project, &source, "main"),
            "用例前提：这轮合并的 git 事实必须是「否」"
        );

        // 第一拍：开出合并轮（卡 merging），拿到这一轮的 action_id。
        let DriverAction::Append {
            action_id,
            card_id,
            objective,
            ..
        } = service
            .driver_step(&idle(&["thread-1"]), &[])
            .unwrap()
            .expect("the merge round opens")
        else {
            unreachable!()
        };
        assert_eq!(card_id, "card-1");
        assert!(objective.contains(&source), "{objective}");
        assert_eq!(state_of(&service, "card-1"), "merging");

        // 第二拍：回合投递成功（`ok: true` 只证明投递过），会话空闲 → 复核。
        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
        };
        assert!(
            service
                .driver_step(&idle(&["thread-1"]), &[result])
                .unwrap()
                .is_none(),
            "收尾那一拍只有判词、没有动作"
        );
        // 判词：git 判否 → `needs_you`（不是 `Cleared` 留在 `merging`，也不是 `done`）。
        assert_eq!(
            state_of(&service, "card-1"),
            "needs_you",
            "git 判否的判词必须是 needs_you"
        );

        // 下一拍这张卡必须能**再入轮**（spec §8 第 8 条的「且能再次入轮」）。
        let DriverAction::Append {
            card_id, objective, ..
        } = service
            .driver_step(&idle(&["thread-1"]), &[])
            .unwrap()
            .expect("a git-negative round is retried")
        else {
            unreachable!()
        };
        assert_eq!(card_id, "card-1");
        assert!(objective.contains(&source), "{objective}");
        assert_eq!(state_of(&service, "card-1"), "merging");
    }

    /// 会话自己跑完、没有提交 → `needs_you`。它**不是**合并轮失败，没有产出可合，
    /// 绝不能被自动拖进合并轮：分支与 base 同点时 `merge-base --is-ancestor` 恒真，
    /// 那会把一张什么都没做的卡悄悄判成 `done`（D9 的自动重试只限合并轮失败）。
    #[test]
    fn driver_step_never_drags_a_session_ended_needs_you_card_into_a_merge_round() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 分支与 main 同点：一旦被自动合，复核会判「已并入」→ done（错）。
        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        git_run(&project, &["branch", &format!("kanban/{slug}")]);
        service.mark_running("card-1", "thread-1").unwrap();
        service.mark_terminal("card-1", "needs_you", None).unwrap();

        assert!(
            service
                .driver_step(&idle(&["thread-1"]), &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state_of(&service, "card-1"),
            "needs_you",
            "等人发话，不被自动合掉"
        );
    }

    /// 缺分支的卡申请不到合并名额（`Denied`）：不是整拍失败，也不该卡住别的卡。
    #[test]
    fn driver_step_skips_a_merge_candidate_whose_source_branch_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_awaiting_card(&project);
        // 不建分支 → merge_request 拒绝。

        assert!(
            service
                .driver_step(&idle(&["thread-1"]), &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state_of(&service, "card-1"),
            "awaiting_merge",
            "卡原地等分支"
        );
    }
}
