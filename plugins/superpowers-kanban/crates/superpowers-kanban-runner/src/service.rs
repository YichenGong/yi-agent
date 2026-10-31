//! 看板服务：把 board.json 与全局并发槽位收敛到一把锁。
//!
//! 分工是这次改造的全部要点：**daemon 只写它亲手做过的事**——排队、启动成功、
//! 启动失败、以及「会话还活着」的观测；**判断类状态一律由模型自己声明**
//! （`set_state`，CLI 的 `state` 动词）。daemon 不再看 git、不再拿 idle 推断、
//! 不再自动合并，也不再从任务状态推导卡片状态。
//!
//! 并发槽位与卡片状态**解耦**：一张卡被标了 `done` 但会话还开着，它仍然占名额，
//! 因为它确实还在烧 token。租约按「活会话」持有（见 `reconcile_leases_locked`），
//! 释放的唯一依据是观测到的会话死亡，而不是卡片的某个状态字。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::{Value, json};

use superpowers_kanban_core::board::Board;
use superpowers_kanban_core::card::{Card, CardId, CardKind, CardState};

use crate::inbox::{self, InboxOutcome};
use crate::lease::{self, Lease};
use crate::merge;
use crate::persist;
use crate::resident;
use crate::worktree::{ensure_worktree, slugify};

/// 一次成功认领：卡片 id、它的 worktree、以及给会话用的标题。
pub struct LaunchClaim {
    pub card_id: String,
    pub workdir: PathBuf,
    pub title: String,
    /// 合并卡（`add-merge` 投递的那种）：没有实现计划，会话要做的是把
    /// `source` 合进 `base`。objective 因此是另一段文案。
    pub is_merge: bool,
}

/// 宿主报的一条会话事实。
///
/// `alive` 由宿主按「这一拍还看得见这个会话」给出，与 `idle` 是两件事：`idle`
/// 只说明「此刻没有 turn 在跑」，一张卡在等人回话时正是 idle 的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadFacts {
    pub thread_id: String,
    pub idle: bool,
    pub alive: bool,
}

/// 一次名额申请的结果。
///
/// `Granted` 是**授权**而不是「已经合并」：调用方（会话里的 agent）据 `workdir`
/// 去执行 `git merge --no-ff`，完成后必须回 `merge_finish` 让插件复核。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeRequest {
    /// 已排进合并队列。`position` 是前面还有几张（0 = 队首）。
    Queued { position: usize },
    /// 卡不在可合并状态（还没 `awaiting_merge`，或卡不存在）。
    Denied(String),
}

/// 释放合并名额的结果。
///
/// 只有两种结局：**名额本来就在我手上**（`Released`）与**本来就不在**（`Cleared`）。
/// 没有任何「合并成没成」的判断——那由模型自己看 git 得出并 `state ... done`。
#[derive(Debug, PartialEq, Eq)]
pub enum MergeFinish {
    Released,
    Cleared,
}

/// 插件交给宿主的**通用**动作（spec D4）。宿主不认识看板：它只执行、不解释。
///
/// `card_id` 是插件内部的记账键（D5：`card_id → thread_id` 映射由插件持有），
/// **不进 JSON**——宿主只认 `action_id`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverAction {
    /// 新建会话：宿主在 `workdir` 里起会话，首轮投递 `objective`。
    ///
    /// `driver_project` / `driver_ref` 是会话的**归属**，随动作交给宿主落进 meta。
    /// 必须填：卡片会话的 cwd 是插件自建的 worktree（不是用户项目），宿主
    /// `thread/listAll` 只有拿 `driver_project` 才能把它折进项目组，而该 worktree
    /// 自己的目录组会被抑制——两头落空，会话在侧栏**彻底不可见**。宿主不认识看板，
    /// 唯一同时知道「项目根」与「卡片 id」的就是插件（spec §5.5）。
    Launch {
        action_id: String,
        card_id: String,
        workdir: PathBuf,
        title: String,
        objective: String,
        /// 会话所属项目根（宿主据此归组）。
        driver_project: String,
        /// 驱动方给的不透明句柄（这里是卡片 id；桌面据此归入「看板会话」小节）。
        driver_ref: String,
        /// 注入本会话 shell 的环境变量。宿主只透传键值，不解释任何键——
        /// 「这张卡是哪张」是插件自己的概念（`state --self` 靠它免去手写 id）。
        env: Vec<(String, String)>,
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
                driver_project,
                driver_ref,
                env,
                ..
            } => json!({
                "action_id": action_id,
                "kind": "launch",
                "workdir": workdir.to_string_lossy(),
                "title": title,
                "objective": objective,
                "driver_project": driver_project,
                "driver_ref": driver_ref,
                "env": env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
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
    /// 已认领卡片持有的槽位租约。`Running` 卡片也仍在这里，直到终态释放。
    ///
    /// 这是**进程态**：`flock` fd 只能活在持有它的进程里，因此绝不进
    /// board.json。看板数据本身是共享态，只在磁盘上有一份真相。
    leases: std::collections::HashMap<CardId, Lease>,
    /// `None` 表示拿不到全局租约目录：`next_launch` 必须报错而不是私自
    /// 造一个目录，否则会和别的进程用两个互不相干的池子、悄悄超配额。
    leases_dir: Option<PathBuf>,
    /// 已交给宿主、还没收到回报的动作，按 `action_id` 索引。
    ///
    /// 两副担子：① 它是 `card_id → thread_id` 映射的另一半——`thread_id` 只由
    /// 回报带回（spec D5），所以「哪个动作属于哪张卡」必须记在这里；
    /// ② 它是 at-least-once 的幂等账——回报一到就摘除条目，宿主重复投递的那份
    /// 找不到条目、直接忽略，因而不会重复生效。
    pending_actions: std::collections::HashMap<String, PendingAction>,
    /// 本进程持有的合并名额。`flock` 的 fd 必须活到 `merge_finish`，所以它
    /// 存进 `Inner` 而不是随函数返回——合并跨会话数分钟，无人持有就等于没锁。
    merge_gate: Option<crate::merge_lock::MergeLock>,
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
/// `launch-for-merge` 是「为一张**没有会话的独立合并卡**开合并会话」，回报只落
/// `card_id → thread_id` 映射——那张卡的状态由模型自己维护，插件不替它翻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Launch,
    LaunchForMerge,
}

pub struct BoardService {
    state_dir: PathBuf,
    project_root: PathBuf,
    /// 通用常驻登记目录（`$HOME/.yi-agent`）。`home` 注入时为 `<home>/.yi-agent`，
    /// 于是测试不必碰真实 `$HOME`（与 `leases_dir` 同一手法）。
    resident_dir: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl BoardService {
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        // 先按借用算 resident_dir，再让 leases_dir 的 `map` 消费 `home`。
        let resident_dir = home
            .as_ref()
            .map(|home| home.join(".yi-agent"))
            .or_else(resident::default_dir);
        let leases_dir = home
            .map(|home| {
                home.join(".yi-agent")
                    .join("superpowers-kanban")
                    .join("leases")
            })
            .or_else(lease::global_leases_dir);
        Self {
            state_dir,
            project_root,
            resident_dir,
            inner: Mutex::new(Inner {
                leases: Default::default(),
                leases_dir,
                pending_actions: Default::default(),
                merge_gate: None,
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

    /// 本插件**唯一**的推进入口（spec D1 / §4.6）。
    ///
    /// 宿主每 tick 调一次。这里只做三件事：
    ///
    /// 1. **回报落账**（`card_id → thread_id` 只有这一个来源）；
    /// 2. **写观测旁路**（哪张卡还会话还活着——只写 `observations.json`）；
    /// 3. **启动下一张排队卡**（名额由全局租约按活会话计）。
    ///
    /// 它**不**判断任何卡片的意图：会话空闲不等于跑完了，git 有新提交也不等于
    /// 该验收，宿主重启更不等于卡死了。这些判断只有模型做得了，所以它们全部走
    /// `set_state`（CLI 的 `state` 动词）。
    pub fn driver_step(
        &self,
        threads: &[ThreadFacts],
        results: &[DriverResult],
    ) -> Result<Option<DriverAction>, String> {
        // 前置检查必须**早于**消费回报：拿不到全局租约目录时 `next_launch` 只会
        // 整拍报错，而错误会让宿主把这批回报当作「没送到」重发；可回报已经消费
        // 掉了，重发的那份会被当成陌生动作丢弃，那次账就永久丢了。
        if self.lock().leases_dir.is_none() {
            return Err("no lease directory".into());
        }
        // 1) 回报：宿主上一拍执行的结果在这里落账。
        self.consume_results(results);
        // 2) 观测：只写旁路文件，绝不回写卡片状态。
        self.write_observations(threads);
        // 合并回合一结束就还名额：这一步只能由持有 flock 的本进程做。
        self.release_gate_when_the_merge_is_over();
        let limit = superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
            &self.state_dir,
        )
        .limit_at(chrono::Local::now());
        // 3) 合并队列：用户确认过的卡在这里排到队。放在启动之前——合并优先，
        //    且它一拍最多开一个、开完即返回，本拍不会再起新会话。
        {
            let mut inner = self.lock();
            if let Some(action) = self.promote_merge_queue(&mut inner, limit) {
                return Ok(Some(action));
            }
        }
        // 4) 启动：队首、有名额、日历放行 → 交出 `launch`。
        let cards = self.locked_cards();
        match self.next_launch(limit, chrono::Local::now()) {
            Ok(Some(claim)) => {
                let card = cards.iter().find(|card| card.id.0 == claim.card_id);
                let objective = if claim.is_merge {
                    let (source, base) = card
                        .map(|card| {
                            (
                                card.source_ref.clone().unwrap_or_default(),
                                card.base_ref.clone().unwrap_or_default(),
                            )
                        })
                        .unwrap_or_default();
                    crate::tick::merge_objective(&source, &base, &claim.workdir)
                } else {
                    card.map(crate::tick::engagement_objective)
                        .unwrap_or_else(|| crate::tick::engagement_objective_for_paths("", ""))
                };
                let action_id = self.mint_action_id();
                let card_id = CardId::new(claim.card_id.clone());
                let kind = if claim.is_merge {
                    PendingKind::LaunchForMerge
                } else {
                    PendingKind::Launch
                };
                self.remember_pending(&action_id, &card_id, kind);
                Ok(Some(DriverAction::Launch {
                    action_id,
                    driver_project: self.project_root.to_string_lossy().into_owned(),
                    driver_ref: claim.card_id.clone(),
                    card_id: claim.card_id.clone(),
                    workdir: claim.workdir,
                    title: claim.title,
                    objective,
                    // `state --self` 用它，省得模型手写卡片 id。键名是插件的，
                    // 宿主只把它塞进 bash 的环境。
                    env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), claim.card_id)],
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

    /// 把本拍的会话事实写进观测旁路。**只写 observations.json**，绝不改 board.json。
    fn write_observations(&self, threads: &[ThreadFacts]) {
        let now = chrono::Local::now();
        let mut file = crate::observation::load(&self.state_dir);
        let seen: std::collections::HashSet<&str> = threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect();
        for thread in threads {
            file.observe(&thread.thread_id, thread.alive, now);
        }
        // 这一拍完全没被提到的会话按「已经没了」记，但保留最后一次活着的时刻
        // （`observe(false)` 正是这个语义）。宿主重启后 `threads` 为空，这里的
        // 结果就是「全体 stale」——这是**展示**，不是状态迁移。
        let missing: Vec<String> = file
            .threads
            .keys()
            .filter(|id| !seen.contains(id.as_str()))
            .cloned()
            .collect();
        for id in missing {
            file.observe(&id, false, now);
        }
        if let Err(error) = crate::observation::save(&self.state_dir, &file) {
            eprintln!("superpowers-kanban: could not write observations: {error}");
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
            // 为独立合并卡开出的会话回来了：**只**落 `card_id → thread_id`
            // 映射（D5），不碰状态——那张卡的状态由模型自己维护。
            // 回报没带 thread id 就无从记账，维持原样（与 `Launch` 同型）。
            (PendingKind::LaunchForMerge, true) => match result.thread_id.as_deref() {
                Some(thread_id) => persist::with_board(&self.state_dir, |board| {
                    board
                        .set_thread_id(&pending.card_id, thread_id.to_string())
                        .map_err(|error| error.to_string())
                }),
                None => {
                    eprintln!(
                        "superpowers-kanban: merge launch of {} reported ok without a thread_id",
                        pending.card_id
                    );
                    Ok(())
                }
            },
            // 会话**从没起来**：合并名额还抱在本进程手里，却没有会话去用它。
            // 立刻放掉，否则这个项目的合并会永远开不了局。状态不动——模型
            // 什么都没做，看板不该替它编一个。
            (PendingKind::LaunchForMerge, false) => {
                let _ = self.merge_finish(&pending.card_id.0);
                Ok(())
            }
        }
    }

    /// 看板快照（克隆）：一次决策取一份，避免边决策边读到漂移的状态。
    ///
    /// 直接读盘（`persist::read_board`）而不是读内存：board.json 是唯一真相
    /// （单源点），别的进程（CLI、另一个 daemon）可能刚刚改过它。
    /// 队列推进：名额空时把队首那张推进到 `Running` 并交出合并回合。
    ///
    /// **顺序是「先会话名额、后 merge.lock」**（spec §4.5）。反过来会占着合并锁
    /// 等额度，期间本项目所有合并都被堵住，而它们本可以排在后面等着。
    ///
    /// 合并回合一律开**新会话**：旧的卡片会话早已跑完（甚至可能已经被回收），
    /// 而合并要在一棵检出 base 的树里做，那棵树是 `merge::prepare` 现建的。
    fn promote_merge_queue(&self, inner: &mut Inner, limit: u16) -> Option<DriverAction> {
        let board = persist::read_board(&self.state_dir);
        // 已经有人在合并：holder 指向任何卡，本拍就不动。锁本身是判定依据，
        // 不另设 `Merging` 状态（spec §8）。
        if crate::merge_lock::read_holder(&self.state_dir).is_some() {
            return None;
        }
        let mut candidates: Vec<Card> = board
            .cards()
            .iter()
            .filter(|card| card.state == CardState::MergeQueued)
            .cloned()
            .collect();
        if candidates.is_empty() {
            return None;
        }
        // FIFO 只看入队时刻；同一时刻（旧数据没有时间戳）按 order 再按 id 兜底，
        // 保证每拍挑中的是同一张。
        candidates.sort_by(|a, b| {
            (a.merge_queued_at, a.order, &a.id).cmp(&(b.merge_queued_at, b.order, &b.id))
        });
        let Some(leases_dir) = inner.leases_dir.clone() else {
            return None;
        };
        for card in candidates {
            // a) 会话名额。拿不到就整拍不推进——名额是全机共享的，换一张卡也
            //    一样拿不到，继续试只是白烧 CPU。
            let Some(lease) = lease::acquire_in(&leases_dir, limit as usize) else {
                return None;
            };
            // b) 合并锁。拿不到说明别的进程刚抢到：把会话名额放回，换下一张。
            let gate = match crate::merge_lock::acquire_with_holder(&self.state_dir, &card.id.0) {
                crate::merge_lock::MergeAttempt::Granted(lock)
                | crate::merge_lock::MergeAttempt::Reclaimed(lock) => lock,
                crate::merge_lock::MergeAttempt::Busy { .. } => {
                    drop(lease);
                    continue;
                }
            };
            let base = card
                .base_ref
                .clone()
                .unwrap_or_else(|| crate::merge::default_branch(&self.project_root));
            let workdir = match merge::prepare(&self.project_root, &base) {
                Ok(tree) => tree.path,
                Err(error) => {
                    eprintln!(
                        "superpowers-kanban: cannot prepare a tree for {}: {error}",
                        card.id.0
                    );
                    drop(gate);
                    drop(lease);
                    continue;
                }
            };
            // 落到 `Running`：合并本来就是一次会话回合，执行态就是 running。
            if persist::with_board(&self.state_dir, |board| {
                board
                    .transition(&card.id, CardState::Running)
                    .map_err(|error| error.to_string())?;
                if let Some(live) = board.get_mut(&card.id) {
                    live.clear_merge_queued();
                }
                board
                    .set_workdir(&card.id, workdir.clone())
                    .map_err(|error| error.to_string())
            })
            .is_err()
            {
                drop(gate);
                drop(lease);
                continue;
            }
            inner.merge_gate = Some(gate);
            inner.leases.insert(card.id.clone(), lease);
            // 动作 id 与待办条目就地写进 `inner`：`mint_action_id` /
            // `remember_pending` 都会再取同一把锁，在这里调用就是自锁。
            inner.next_action_seq += 1;
            let action_id = format!("act-{}-{}", inner.action_nonce, inner.next_action_seq);
            inner.pending_actions.insert(
                action_id.clone(),
                PendingAction {
                    card_id: card.id.clone(),
                    kind: PendingKind::LaunchForMerge,
                },
            );
            // daemon 已代持名额，所以用「不必再申请」的那版文案。
            let objective = crate::tick::queued_merge_objective(
                card.source_ref.as_deref().unwrap_or_default(),
                &base,
                &workdir,
            );
            return Some(DriverAction::Launch {
                action_id,
                driver_project: self.project_root.to_string_lossy().into_owned(),
                driver_ref: card.id.0.clone(),
                card_id: card.id.0.clone(),
                workdir,
                title: String::new(),
                objective,
                env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), card.id.0.clone())],
            });
        }
        None
    }

    fn locked_cards(&self) -> Vec<Card> {
        persist::read_board(&self.state_dir).cards().to_vec()
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
        persist::with_board(&self.state_dir, |board| Ok(board.archive_due(now, grace)))
            .unwrap_or_default()
    }

    /// 认领下一张排队卡：先补账本进程已占的槽位，再领新租约、建 worktree。
    ///
    /// 名额的判据只有一条：**全局租约池还有空位**。卡片状态不参与——一张标了
    /// `done` 但会话还开着的卡仍然占着它的租约（见 `reconcile_leases_locked`）。
    pub fn next_launch(
        &self,
        limit: u16,
        _now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        // 先补账：内存里的租约可能已经对应到死掉的会话（或反过来说，磁盘上还有
        // 活会话但本进程是刚重启的，手里一个租约都没有）。这一步把「谁该占槽」
        // 与「实际占槽」对齐，否则重启后名额会被别的项目抢走。
        self.reconcile_leases_locked(&mut inner, limit);
        // 再占槽：拿不到就说明名额已满（含本进程自己已持有的）。
        let Some(lease) = lease::acquire_in(&dir, limit as usize) else {
            return Ok(None);
        };
        let project_root = self.project_root.clone();
        // 建 worktree 失败的错误在这里暂存：`with_board` 在闭包返回 `Err` 时**不
        // 落盘**，而失败路径必须先落盘 `Failed`，所以闭包必须成功返回、错误留到
        // 锁外再抛（见下）。
        let mut worktree_failure: Option<String> = None;
        // 队空时把刚拿到的租约原地丢掉（drop 即释放）。
        let claimed = persist::with_board(&self.state_dir, |board| {
            let Some(card_id) = board.claim_next_launch() else {
                return Ok(None);
            };
            let is_merge = board
                .get(&card_id)
                .is_some_and(|card| card.kind == CardKind::Merge);
            // 合并卡不建 kanban 分支：它的活儿是在**检出 base 的那棵树**里做合并。
            // 实现卡才需要一张自己的 worktree 与 `kanban/<id>` 分支。
            let workdir = if is_merge {
                let base = board
                    .get(&card_id)
                    .and_then(|card| card.base_ref.clone())
                    .unwrap_or_else(|| merge::default_branch(&project_root));
                merge::prepare(&project_root, &base).map(|worktree| worktree.path)
            } else {
                let branch = format!("kanban/{}", slugify(&card_id));
                ensure_worktree(&project_root, &card_id, &branch).map_err(|error| error.to_string())
            };
            let workdir = match workdir {
                Ok(path) => path,
                Err(error) => {
                    // 认领即占槽：建 worktree 失败必须把卡片标为 Failed **并落盘**，
                    // 否则它会卡在 Running 永远不再被认领。
                    //
                    // 用 `return Ok(None)`（闭包成功）而不是 `return Err(..)`：后者会让
                    // `with_board` 丢弃上面这次 `transition(Failed)`，卡片就卡死了。
                    //
                    // 判词一并落盘：这是全自动路径上最常见的启动失败，只写 stderr
                    // 的话重启后无处可查。
                    let reason = format!("worktree for {card_id} failed: {error}");
                    let _ = board.transition(&card_id, CardState::Failed);
                    let _ = board.set_detail(&card_id, Some(reason.clone()));
                    worktree_failure = Some(reason);
                    return Ok(None);
                }
            };
            let _ = board.set_workdir(&card_id, workdir.clone());
            let title = title_for(board, &card_id);
            Ok(Some(LaunchClaim {
                card_id: card_id.0,
                workdir,
                title,
                is_merge,
            }))
        })?;
        // 建 worktree 失败：状态已落盘为 `Failed`，现在才把错误抛出去（时机见上）。
        if let Some(error) = worktree_failure {
            return Err(error);
        }
        if let Some(claim) = &claimed {
            inner
                .leases
                .insert(CardId::new(claim.card_id.clone()), lease);
        }
        Ok(claimed)
    }

    /// 把本进程持有的租约与「观测到的活会话」对齐。
    ///
    /// 判据只有一个：**这张卡还有没有活会话**。卡片的 `state` 完全不参与——一张
    /// 被模型标成 `done` 但会话还在跑的卡仍然烧 token，名额就该继续占着。这正是
    /// 用户裁决的「并发只看是否在运行」。
    ///
    /// 两个方向都要补：
    ///
    /// - **放**：租约对应的会话已经不在观测里（或已不活）——名额还回去；
    /// - **领**：board.json 里有 `running` 卡、观测说它的会话还活着，而本进程手里
    ///   没有它的租约——daemon 重启后的常态，不补领的话别的项目会把名额抢走。
    ///
    /// 三类卡**永远**保留租约：
    ///
    /// 1. 会话观测为活；
    /// 2. 会话还没被观测过（daemon 刚起、宿主还没上报）——缺证据不是死亡的证据，
    ///    凭它放名额会直接超限；
    /// 3. 刚认领、`thread_id` 还没回来的卡（它有一个在飞的 `launch` 动作）——
    ///    这是最容易漏的一类：若不保留，每拍都会先丢掉自己的名额再抢回来，
    ///    中间那一瞬足以让另一张卡启动。
    fn reconcile_leases_locked(&self, inner: &mut Inner, limit: u16) {
        let Some(dir) = inner.leases_dir.clone() else {
            return;
        };
        let observations = crate::observation::load(&self.state_dir);
        let cards = persist::read_board(&self.state_dir).cards().to_vec();
        let awaiting_thread: std::collections::HashSet<CardId> = inner
            .pending_actions
            .values()
            .filter(|pending| pending.kind == PendingKind::Launch)
            .map(|pending| pending.card_id.clone())
            .collect();
        let live: std::collections::HashSet<CardId> = cards
            .iter()
            .filter(
                |card| match card.thread_id.as_deref().filter(|id| !id.is_empty()) {
                    Some(thread) => observations
                        .threads
                        .get(thread)
                        .is_none_or(|observation| observation.alive),
                    None => card.state == CardState::Running && awaiting_thread.contains(&card.id),
                },
            )
            .map(|card| card.id.clone())
            .collect();
        // 放：不再对应活会话的租约一律 drop（drop 即归还名额）。
        inner.leases.retain(|card_id, _| live.contains(card_id));
        // 领：活的卡还没有租约就补一个（领不到只做尽力而为，不 panic）。
        for card in cards.iter().filter(|card| live.contains(&card.id)) {
            if inner.leases.contains_key(&card.id) {
                continue;
            }
            match lease::acquire_in(&dir, limit as usize) {
                Some(lease) => {
                    inner.leases.insert(card.id.clone(), lease);
                }
                None => {
                    eprintln!(
                        "superpowers-kanban: {} has a live session but no slot could be re-adopted",
                        card.id.0
                    );
                }
            }
        }
    }

    /// 启动时补领本进程既有活会话的全局名额（`next_launch` 每拍也会做一次）。
    pub fn adopt_running_leases(&self, limit: u16) {
        let mut inner = self.lock();
        self.reconcile_leases_locked(&mut inner, limit);
    }

    /// 会话已起来：记下 thread id。
    ///
    /// **不碰卡片状态**：会话起来这件事只说明「插件交出去的动作成功了」，卡的
    /// 状态由模型声明。`Running` 是模型通常会写的第一个状态，但那是它的选择，
    /// 不是 daemon 的推断。
    pub fn mark_running(&self, card_id: &str, thread_id: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        let thread_id = thread_id.to_string();
        persist::with_board(&self.state_dir, |board| {
            board
                .set_thread_id(&id, thread_id)
                .map_err(|error| error.to_string())?;
            let _ = board.set_detail(&id, None);
            Ok(())
        })
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
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            board
                .transition(&id, next)
                .map_err(|error| error.to_string())?;
            // 判词落盘（spec §4.6）：人问「这张卡为什么停在这」只有这里答得出。
            board
                .set_detail(&id, detail.map(str::to_string))
                .map_err(|error| error.to_string())
        })?;
        // 名额不在这里放：会话可能还活着（模型只是提前声明了状态）。租约由
        // `reconcile_leases_locked` 按观测统一收放。
        Ok(())
    }

    /// 启动没成：把卡片标为 `Failed` 并释放槽位。`detail` 是失败原因，同样落盘可读。
    ///
    /// **保留（不退役）**：它只经 `board.release` 这一个 IPC 方法到达
    /// （`dispatch.rs`），而宿主的驱动器已不再调任何 `board.*` 方法——但这不构成
    /// 退役理由。spec §6 明确把 `next_launch` / `mark_running` / `mark_terminal` /
    /// `release` **保留为插件内部函数**，由 `driver/next` 的决策逻辑调用；`board.release`
    /// 是插件查询面上仍在的方法（与 `board.mark_terminal` 同一组），CLI/桌面/后续
    /// 调试都可经它把一次启动失败落成「`failed` + 可读原因」。它也不是
    /// `mark_terminal(.., "failed", ..)` 的同义词：`release` 是**专用**契约
    /// （`Detail` 必填、语义就是「启动没成」），而 `mark_terminal` 收三种结果。
    /// 退役它会连带删掉一个 spec 保留的 IPC 面，收益只是少一个函数——不值得。
    pub fn release(&self, card_id: &str, detail: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            board
                .transition(&id, CardState::Failed)
                .map_err(|error| error.to_string())?;
            board
                .set_detail(&id, Some(detail.to_string()))
                .map_err(|error| error.to_string())
        })?;
        // 名额不在这里放：`release` 就是启动没成，本来没有会话，但只要卡片还可能
        // 带着 thread id（调用方拿它当通用失败口用），当场归还就会凭空多出一个
        // 并发位。租约统一由 `reconcile_leases_locked` 按观测收放，只有一个释放者。
        Ok(())
    }

    /// 这台宿主的管路里，请求常驻 daemon 的名字。写进登记文件的 `required_by`。
    ///
    /// 是一个**磁盘契约字符串**：宿主侧用同一个名字读写（`yi-agent-store` 的
    /// `resident::import_legacy_projects(…, requester)` 参数就是这个值），改它要
    /// 两边同改。
    const REQUESTER: &'static str = "superpowers-kanban";

    /// 本插件的状态目录落点：`<project>/.yi-agent/superpowers-kanban`。
    ///
    /// 与 `main.rs` 的 `default_state_dir` 和清单的 `--state-dir` 同一个落点。
    /// 只写一处，create 与 remove 就不可能各算各的。
    fn state_dir_for(project: &Path) -> PathBuf {
        project.join(".yi-agent").join(Self::REQUESTER)
    }

    /// 建盘（生命周期归插件，Spec 4 §4.2）：建自己的状态目录 + 登记常驻。
    ///
    /// **不写清单**。清单（`<项目>/.yi-agent/supervisors/<name>.json`）由**宿主**
    /// 安装——「把某个插件装进某个项目」是宿主通用能力，插件在清单就位前根本
    /// 起不来，写不了它（spec §4.4 的鸡生蛋）。因此本方法只做插件自己那份：
    /// 建状态目录、登记「这个项目要常驻」。
    ///
    /// 幂等：重复调用两次不产生重复的 `required_by`（`resident::require` 的
    /// 并集语义），状态目录的创建也是幂等的。
    pub fn lifecycle_create(&self, project: &Path) -> Result<(), String> {
        let resident_dir = self
            .resident_dir
            .clone()
            .ok_or_else(|| "cannot locate the resident registry: HOME is unset".to_string())?;
        // 状态目录落点与 `main.rs` 的 `default_state_dir`/清单的 `--state-dir` 一致：
        // `<项目>/.yi-agent/superpowers-kanban`。
        let state_dir = Self::state_dir_for(project);
        std::fs::create_dir_all(&state_dir).map_err(|error| {
            format!(
                "could not create the board state dir {}: {error}",
                state_dir.display()
            )
        })?;
        resident::require(&resident_dir, project, Self::REQUESTER)
            .map_err(|error| format!("could not register the project as resident: {error}"))
    }

    /// 删盘：撤销常驻登记 + 删自己的状态目录。
    ///
    /// **分工**：插件**不**去停宿主的 per-project daemon——那是宿主/监督者的事
    /// （谁成功启动 daemon，谁负责监督）。插件只说「我不再需要它了」，宿主观察到
    /// 登记消失后停进程。所以这里的验收是「release 已发生 + 状态目录已删」，
    /// 而不是「进程已停」。
    ///
    /// 可重复调用：登记本已不在、目录本已不在，都当成功。
    pub fn lifecycle_remove(&self, project: &Path) -> Result<(), String> {
        let resident_dir = self
            .resident_dir
            .clone()
            .ok_or_else(|| "cannot locate the resident registry: HOME is unset".to_string())?;
        resident::release(&resident_dir, project, Self::REQUESTER)
            .map_err(|error| format!("could not release the project: {error}"))?;
        let state_dir = Self::state_dir_for(project);
        match std::fs::remove_dir_all(&state_dir) {
            Ok(()) => Ok(()),
            // 本就不在 = 目标已达成。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!(
                "could not remove the board state dir {}: {error}",
                state_dir.display()
            )),
        }
    }

    /// 启动迁移：把「写着 running 但没有会话」的历史卡片收成 `needs_you`。
    ///
    /// 这类卡片来自旧版本的记录（那时 `thread_id` 可能为空），实际上没有会话在
    /// 跑。收成 `needs_you` 而不是自动重开：重开可能造出第二个会话重复跑同一张
    /// 卡，把事实摆到人面前才是诚实的收场。
    ///
    /// 名额不在这里处理：租约按观测收放（`reconcile_leases_locked`），没有会话
    /// 的卡本来就不该占着名额。
    pub fn migrate_legacy_running(&self) {
        let orphaned: Vec<CardId> = persist::with_board(&self.state_dir, |board| {
            let ids: Vec<CardId> = board
                .cards()
                .iter()
                .filter(|card| {
                    card.state == CardState::Running
                        && card.thread_id.as_deref().unwrap_or("").is_empty()
                })
                .map(|card| card.id.clone())
                .collect();
            let mut moved = Vec::new();
            for id in ids {
                if board.transition(&id, CardState::NeedsYou).is_ok() {
                    let _ = board.set_detail(
                        &id,
                        Some(
                            "started in an older version and has no session; needs a fresh look"
                                .to_string(),
                        ),
                    );
                    moved.push(id);
                }
            }
            Ok(moved)
        })
        .unwrap_or_default();
        let _ = orphaned;
    }

    /// 消费 inbox 把投递排进**内存里的同一份看板**并落盘，返回每张投递的结果。
    ///
    /// 推进循环与查询服务共享同一实例，所以这里必须改内存板而不是另 load 一份
    /// `board.json` 再写回——否则会覆盖掉持有 lease 的卡片状态（`Launching`）。
    pub fn consume_inbox(&self, now: DateTime<Local>) -> Vec<InboxOutcome> {
        let state_dir = self.state_dir.clone();
        persist::with_board(&state_dir, |board| {
            Ok(inbox::consume(&state_dir, board, now))
        })
        .unwrap_or_default()
    }

    /// 申请一次合并名额。
    ///
    /// 名额由 `<state_dir>/merge.lock`（`flock`）承载，**持有者记录**另落一个小
    /// 文件说明「谁在合」。合并跨会话、持续数分钟，所以锁的 fd 必须存进 `Inner`
    /// 一直活着，直到 `merge_finish` 放掉——随函数返回就等于没锁。
    ///
    /// 上一位持有者崩在两次写之间时，`flock` 已经由内核释放，而 holder 记录里的
    /// pid 也查无此人：视为残留，直接接管。
    ///
    /// 不做 git 校验：分支存不存在、能不能合，是模型的判断，不是插件的。
    /// 把卡放进本项目的合并队列。**只改状态，不拿 `merge.lock`**——抢锁是 daemon
    /// 在开跑那一拍的事（`promote_merge_queue`）。申请与执行分开，「排队」才有
    /// 地方存在：名额忙时卡就一直在队列里，且能被人看见排在第几。
    pub fn merge_request(&self, card_id: &str) -> Result<MergeRequest, String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id).cloned() else {
                return Ok(MergeRequest::Denied(format!("unknown card: {card_id}")));
            };
            // 已在队列里：幂等，报告当前名次。绝不能重写时间戳——那等于让重复
            // 申请把自己排到队尾。
            if card.state == CardState::MergeQueued {
                let position = board
                    .cards()
                    .iter()
                    .filter(|other| other.state == CardState::MergeQueued)
                    .filter(|other| other.merge_queued_at < card.merge_queued_at)
                    .count();
                return Ok(MergeRequest::Queued { position });
            }
            // 授权的唯一入口：用户发话 → 模型跑这个动词。所以卡必须已经
            // 「活儿干完了」(`awaiting_merge`)。别的状态说明调用方走错了路，
            // 报清当前状态让它自己纠正——尤其是排队中的实现卡，它必须先在
            // 会话里干活。
            if card.state != CardState::AwaitingMerge {
                return Ok(MergeRequest::Denied(format!(
                    "card {card_id} is {:?}, not awaiting_merge",
                    card.state
                )));
            }
            let position = board
                .cards()
                .iter()
                .filter(|other| other.state == CardState::MergeQueued)
                .count();
            board
                .transition(&id, CardState::MergeQueued)
                .map_err(|error| error.to_string())?;
            if let Some(card) = board.get_mut(&id) {
                card.mark_merge_queued(chrono::Local::now());
            }
            Ok(MergeRequest::Queued { position })
        })
    }

    /// 退队。**只在 `MergeQueued` 上生效**——这个前置条件是它与 `set_state` 的
    /// 全部区别。队列由 daemon 每拍自动推进，若退队走无条件写，就会在「用户反悔」
    /// 与「daemon 跑了下一拍」之间留下竞态：卡可能已被推进到 `Running`，那时再改
    /// 回去会与一个正在跑的合并回合打架。两边对同一张卡的判定互斥，就没有这个窗口。
    pub fn merge_cancel(&self, card_id: &str) -> Result<(), String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id).cloned() else {
                return Err(format!("unknown card: {card_id}"));
            };
            if card.state != CardState::MergeQueued {
                return Err(format!(
                    "card {card_id} is {:?}, not merge_queued; there is nothing to cancel",
                    card.state
                ));
            }
            board
                .transition(&id, CardState::AwaitingMerge)
                .map_err(|error| error.to_string())?;
            if let Some(card) = board.get_mut(&id) {
                card.clear_merge_queued();
            }
            Ok(())
        })
    }

    /// 释放合并名额。**不碰 git、不改卡片状态**。
    ///
    /// 合并成没成由模型自己看 git 判断并 `state <card> done`；这里只回答「名额
    /// 还在不在我手上」。`Cleared` 表示本进程根本没持有名额（别人的合并、或
    /// 已经释放过一次），不是错误。
    pub fn merge_finish(&self, card_id: &str) -> Result<MergeFinish, String> {
        let mut inner = self.lock();
        let Some(gate) = inner.merge_gate.take() else {
            // 本进程没有名额。但记录的持有者可能已经死了（崩溃、或被换掉了）：
            // 那是条无从释放的残留，留着只会让 `list` 永远显示 `[merging]`、
            // 让用户以为还有人在合。`flock` 本身由内核释放，清记录是安全的。
            if crate::merge_lock::read_holder(&self.state_dir).is_some_and(|holder| {
                holder.card_id == card_id && !crate::merge_lock::holder_alive(&holder)
            }) {
                crate::merge_lock::clear_holder(&self.state_dir);
            }
            return Ok(MergeFinish::Cleared);
        };
        // 只清自己那份记录：holder 若已指向别人，别把别人的账抹了。
        if crate::merge_lock::read_holder(&self.state_dir)
            .is_some_and(|holder| holder.card_id == card_id)
        {
            crate::merge_lock::clear_holder(&self.state_dir);
        }
        drop(gate);
        Ok(MergeFinish::Released)
    }

    /// 合并回合结束后放掉本进程持有的名额。
    ///
    /// **必须由 daemon 自己做。** 名额是 `flock` 的 fd，只能被持有它的进程释放；
    /// 合并回合一跑完，会话里那句 `merge-finish` 落在**另一个进程**里，只会拿到
    /// `cleared`，锁还在 daemon 手里。若这里不回收，本项目此后再也合并不了，
    /// 而界面上只会看到一张「没在合并」的卡和一个永远被占的名额。
    ///
    /// 判定只看卡片自己写下的状态：模型把合并收尾成 `done` / `failed` /
    /// `needs_you`（或退回 `awaiting_merge`）就说明这一轮结束了。
    fn release_gate_when_the_merge_is_over(&self) {
        let held = self.lock().merge_gate.is_some();
        if !held {
            return;
        }
        let card_id = match crate::merge_lock::read_holder(&self.state_dir) {
            Some(holder) => holder.card_id,
            None => return,
        };
        // holder 记录缺失时无从判断是哪张卡：宁可再等一拍，也不误放别人的名额。
        if card_id.is_empty() {
            return;
        }
        let board = persist::read_board(&self.state_dir);
        let Some(card) = board.get(&CardId::new(card_id.clone())) else {
            let _ = self.merge_finish(&card_id);
            return;
        };
        // 合并回合期间卡就是 `Running`（队列推进时置的）。模型一收尾——`done`、
        // `needs_you`、`failed`，或干脆退回别的状态——这一轮就结束了。
        if card.state != CardState::Running {
            let _ = self.merge_finish(&card_id);
        }
    }

    /// 模型声明这张卡现在处于什么状态。这是除启动之外**唯一**写状态的入口。
    ///
    /// 不做迁移校验（只有一条：终态不可复活，由 `CardState::can_transition_to`
    /// 兜住）。模型有上下文，整张迁移表只会在正当场景上挡住它。
    pub fn set_state(
        &self,
        card_id: &str,
        state: CardState,
        detail: Option<&str>,
    ) -> Result<(), String> {
        let id = CardId::new(card_id);
        persist::with_board(&self.state_dir, |board| {
            let Some(card) = board.get(&id) else {
                return Err(format!("unknown card: {card_id}"));
            };
            if card.state.is_terminal() && card.state != state {
                return Err(format!(
                    "card {card_id} is {:?}; a terminal state cannot be revived",
                    card.state
                ));
            }
            board
                .transition(&id, state)
                .map_err(|error| error.to_string())?;
            // 终态不该挂着一句为「还没做完」写的判词；其余状态用调用方给的判词
            // （`None` 表示无判词）。
            let detail = if state.is_terminal() {
                None
            } else {
                detail.map(str::to_string)
            };
            board
                .set_detail(&id, detail)
                .map_err(|error| error.to_string())
        })
    }

    /// 看板快照。控制面读的就是这个形状。
    pub fn list(&self) -> Value {
        let board = persist::read_board(&self.state_dir);
        let observations = crate::observation::load(&self.state_dir);
        // 一次读出，别在逐卡闭包里反复开文件。
        let holder = crate::merge_lock::read_holder(&self.state_dir);
        json!({
            "cards": board.visible().map(|card| json!({
                "id": card.id.0,
                "state": card.state,
                "spec_path": card.spec_path,
                "plan_path": card.plan_path,
                "workdir": card.workdir,
                "thread_id": card.thread_id,
                "origin_thread_id": card.origin_thread_id,
                "kind": card.kind,
                "source": card.source_ref,
                "base": card.base_ref,
                "origin_card": card.origin_card.as_ref().map(|id| id.0.clone()),
                "detail": card.detail,
                "title": card.title,
                // 观测是**派生**的：卡写着 running 但会话没了就标 stale。
                // 它永远不写回卡片状态，只是个给人和桌面的角标。
                // 排队时刻（FIFO 依据）。不在队列里就是 null。
                "merge_queued_at": card.merge_queued_at.map(|at| at.to_rfc3339()),
                // 谁在合并由 holder 说话，不靠状态：合并是一次会话回合，
                // 回合期间卡就是 running。
                "merging": holder
                    .as_ref()
                    .is_some_and(|holder| holder.card_id == card.id.0),
                "stale": crate::observation::stale(card, &observations).is_some(),
                "last_alive_at": card
                    .thread_id
                    .as_deref()
                    .and_then(|thread| observations.threads.get(thread))
                    .and_then(|observation| observation.last_seen_alive_at)
                    .map(|at| at.to_rfc3339()),
            })).collect::<Vec<_>>(),
        })
    }

    /// 看板上未被占用的合并卡 id（供 dispatch / CLI 共用）。
    pub fn next_free_merge_id(&self, source: &str, base: &str) -> String {
        persist::read_board(&self.state_dir)
            .next_free_merge_id(source, base)
            .0
    }

    /// 未被占用的预排队卡 id（供 dispatch / CLI 共用）。
    ///
    /// 走 core 的合体分配器：它既看看板、也看 `inbox/` 里尚未消费的投递
    /// （见 `inbox::next_free_prequeued_id` 的注释）。
    pub fn next_free_prequeued_id(&self, title: &str, now: DateTime<Local>) -> String {
        superpowers_kanban_core::inbox::next_free_prequeued_id(
            &persist::read_board(&self.state_dir),
            &self.state_dir,
            title,
            now,
        )
        .0
    }

    /// 仅测试用：把某张卡片的 `thread_id` 清成 `None`，模拟旧数据。
    ///
    /// 只搬字段、不碰产品逻辑；走一遍序列化契约而不是给 `Board` 开新的
    /// `&mut Card` 通道（那会为了测试扩大产品 API）。
    #[cfg(test)]
    pub(crate) fn clear_thread_id_for_test(&self, card_id: &str) {
        let state_dir = self.state_dir.clone();
        let _ = persist::with_board(&state_dir, |board| {
            let Ok(text) = std::fs::read_to_string(state_dir.join("board.json")) else {
                return Ok(());
            };
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
                return Ok(());
            };
            if let Some(cards) = value.get_mut("cards").and_then(Value::as_array_mut) {
                for card in cards {
                    if card.get("id").and_then(Value::as_str) == Some(card_id) {
                        card["thread_id"] = Value::Null;
                    }
                }
            }
            if let Ok(reloaded) = serde_json::from_value::<Board>(value) {
                *board = reloaded;
            }
            Ok(())
        });
    }

    /// 仅测试用：再排一张排队卡进 board 并落盘，供队列/名额用例搭台。
    #[cfg(test)]
    pub(crate) fn enqueue_card_for_test(&self, id: &str) {
        let _ = persist::with_board(&self.state_dir, |board| {
            board.enqueue(
                CardId::new(id),
                PathBuf::from(format!("{id}.spec.md")),
                PathBuf::from(format!("{id}.plan.md")),
                chrono::Local::now(),
            );
            Ok(())
        });
    }
}

/// 宿主报的这条会话闲不闲；`None` = 宿主账上没有这台会话，判不了。
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
    /// 看板的状态目录就落在项目根下的 `.yi-agent/`，若不忽略，主检出会被插件
    /// 自己的运行时状态弄「脏」。合并用的 base worktree 就是这棵主检出
    /// （`merge::prepare` 在它已被检出时复用，不再新开一棵），故这棵树的干净是
    /// 「git 事实可信」的前提——测试 fixture 必须复刻生产项目的这一布置。
    ///
    /// （曾经的合并前置校验 `merge::is_dirty` 已随「合并在会话里执行」退役，
    /// 见 `merge.rs` 测试模块里的同名助手。）
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
    fn facts(thread_id: &str, idle: bool, alive: bool) -> crate::service::ThreadFacts {
        crate::service::ThreadFacts {
            thread_id: thread_id.to_string(),
            idle,
            alive,
        }
    }

    /// 宿主说这些会话都还忙着。
    fn busy(ids: &[&str]) -> Vec<crate::service::ThreadFacts> {
        ids.iter().map(|id| facts(id, false, true)).collect()
    }

    /// 一拍 `driver_step`，断言它交出了一个 launch 并把它交出来。
    fn launched(service: &BoardService) -> DriverAction {
        service
            .driver_step(&[], &[])
            .unwrap()
            .expect("a launch action")
    }

    /// 把并发配额钉成 1（写进项目日历），用来验「名额限制」。
    /// 有界轮询直到 `condition` 成立。
    ///
    /// 测试并行跑在**同一进程**里，任一处 `Command::new("git")` 都会 fork，
    /// 把此刻全进程持有的 `flock` fd 一并复制给子进程；子进程 exec 前的这一
    /// 小段窗口里，别的线程释放了锁也仍算被占。这是 flock 的既有性质，不是
    /// 被测逻辑的问题，所以断言「名额已释放」时轮询即可（同 `merge_lock`
    /// 自己那条 `one_project_admits_a_single_merger`）。
    fn wait_until(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        condition()
    }

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

        // `home` 必须隔离：传 `None` 会落到真实的全局租约池，与同机
        // 其它 cargo 测试抢同一批 slot，结果随并行度时过时不过。
        let service = BoardService::new(
            state_dir.clone(),
            dir.path().to_path_buf(),
            Some(dir.path().join("home")),
        );
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

    /// R1：`next_launch` 建 worktree 失败时，卡片必须**落盘**成 `Failed`。
    ///
    /// 这是本任务最容易被重新写坏的一处：`with_board` 的契约是「闭包返回 `Err`
    /// 即不落盘」，而失败路径必须先落盘 `Failed`，所以闭包内必须成功返回
    /// （`Ok(None)`）并把错误留到锁外再抛。谁若改回 `return Err(..)`，卡片会停在
    /// `Launching`、永远不再被认领——下面直读 `board.json` 的断言正是钉这一点。
    ///
    /// 触发方式：预先占用分支名 `kanban/<slug>`，`git worktree add -b` 必因分支
    /// 已存在而失败（`worktree.rs` 在该情形返回 `Err`）。
    #[test]
    fn next_launch_persists_failed_when_the_worktree_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // service_with_card 的 state_dir / home 落点（与它保持一致）。
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        let leases_dir = dir.path().join("home/.yi-agent/superpowers-kanban/leases");
        let slug = crate::worktree::slugify(&CardId::new("card-1"));
        let branch = format!("kanban/{slug}");
        git_run(&project, &["branch", &branch]);

        // (a) 失败必须报出来（消息里带 "worktree for"）。
        // 用 match 而不是 `expect_err`：`LaunchClaim` 未实现 `Debug`，为一条测试去
        // 给产品类型加 derive 是扩大产品 API，这里避开即可。
        let error = match service.next_launch(1, at()) {
            Err(error) => error,
            Ok(_) => panic!("a pre-existing branch must fail the worktree creation"),
        };
        assert!(error.contains("worktree for"), "{error}");

        // (b) 经 list 看到 failed。
        assert_eq!(state_of(&service, "card-1"), "failed");

        // (c) 直读磁盘：`Failed` 必须已经落盘，而不是只活在闭包里的内存副本。
        // （这正是 `return Err(..)` 会静默破坏的那条：闭包失败 → 改动被丢弃。）
        let on_disk = persist::read_board(&state_dir);
        assert_eq!(
            on_disk.get(&CardId::new("card-1")).unwrap().state,
            CardState::Failed,
            "worktree failure must be persisted, not merely staged inside the closure"
        );

        // (d) 名额已释放：认领失败不能泄漏槽位（`claimed` 为 None → 租约就地 drop）。
        // macOS 上 flock 的释放是异步的，故有界重试而不是裸断言。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match lease::acquire_in(&leases_dir, 1) {
                Some(lease) => {
                    drop(lease);
                    break;
                }
                None => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the slot of a failed worktree creation was never released"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }

        // 失败后队空、名额空闲：下一轮必须如实返回 None（而不是又抢一次槽）。
        assert!(service.next_launch(1, at()).unwrap().is_none());
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

    /// `mark_running` 只记 thread id，**不改状态**（状态归模型）。
    #[test]
    fn mark_running_records_the_thread_without_touching_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        let state_before = state_of(&service, &claim.card_id);
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 重复上报不报错、幂等。
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert_eq!(
            thread_of(&service, &claim.card_id).as_deref(),
            Some("thread-1")
        );
        assert_eq!(state_of(&service, &claim.card_id), state_before);
    }

    #[test]
    fn release_fails_the_card_and_frees_the_slot() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        // 启动没成 → 没有会话，下一拍观测不到它，名额必须归还。
        service
            .release(&claim.card_id, "thread/start failed")
            .unwrap();
        assert_eq!(service.list()["cards"][0]["state"], "failed");
        service.enqueue_card_for_test("card-2");
        assert!(
            wait_until(|| service
                .next_launch(1, at())
                .unwrap()
                .is_some_and(|claim| claim.card_id == "card-2")),
            "a failed launch must hand its slot to the next queued card"
        );
    }

    /// `release` 是「启动没成」的专用口。若调用方把它当通用失败口、在会话已
    /// 存在时调用，名额不能当场放——否则同一拍里会凭空多出一个并发位。
    #[test]
    fn a_released_card_keeps_its_slot_while_its_session_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service.release(&claim.card_id, "boom").unwrap();
        assert_eq!(state_of(&service, &claim.card_id), "failed");
        // 会话还在，名额仍被它占着：下一张排队卡抢不到那唯一一个位子。
        service.enqueue_card_for_test("card-2");
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "a card whose session is still alive must keep its slot"
        );
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

    /// 读一张卡记下的 `card_id → thread_id` 映射（解析结果，不是原始文本）。
    /// 用 `list` 的口径：映射由插件持有（D5），`null` 就是「还没有会话」。
    fn thread_of(service: &BoardService, id: &str) -> Option<String> {
        service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["id"] == id)
            .unwrap_or_else(|| panic!("card {id} not on the board"))["thread_id"]
            .as_str()
            .map(str::to_string)
    }

    /// 读一张卡的判词（经 `list`：这就是「人能读到的地方」）。    ///
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

    /// daemon 不得按 git 事实改卡片状态：分支已并入 base，卡也必须原样停着等
    /// 模型/人发话。这条测试是回归锁——自动收敛一旦回来就编译失败。
    #[test]
    fn a_merged_branch_never_flips_a_card_to_done_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
    }

    /// 排队中的**实现卡**还不能合并：它必须先在会话里干活。这是唯一还留着的
    /// 准入限制——其余状态由模型自己负责。
    #[test]
    fn merge_request_denies_a_queued_implementation_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => {
                assert!(reason.contains("not awaiting_merge"), "{reason}")
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    /// 入队不拿锁：两张卡可以同时排着队。这是「排队」存在的全部前提。
    #[test]
    fn merge_request_only_queues_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        assert_eq!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Queued { position: 0 }
        );
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        service.enqueue_card_for_test("card-2");
        service
            .set_state("card-2", CardState::AwaitingMerge, None)
            .unwrap();
        assert_eq!(
            service.merge_request("card-2").unwrap(),
            MergeRequest::Queued { position: 1 },
            "a second card must be able to queue: queueing takes no lock"
        );
    }

    #[test]
    fn merge_request_refuses_a_card_that_is_not_awaiting_merge() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service
            .set_state("card-1", CardState::Running, None)
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "running");
        match service.merge_request("card-1").unwrap() {
            MergeRequest::Denied(reason) => {
                assert!(
                    reason.contains("not awaiting_merge"),
                    "must name why: {reason}"
                )
            }
            other => panic!("expected Denied, got {other:?}"),
        }
    }

    #[test]
    fn merge_request_is_idempotent_and_keeps_the_queue_position() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        assert_eq!(
            service.merge_request("card-1").unwrap(),
            MergeRequest::Queued { position: 0 },
            "asking twice must not move the card to the back of the queue"
        );
    }

    /// 入队只写「排队」这一个状态：它不会顺手把卡推到执行态，也不碰 `merge.lock`。
    #[test]
    fn merge_request_never_starts_the_merge_itself() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_none(),
            "queueing must not take the gate"
        );
    }

    /// 释放名额：只放锁，状态一动不动，且放掉之后别人能拿到。
    #[test]
    fn merge_finish_only_releases_the_gate_and_never_touches_git_or_state() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        // 名额由 daemon 在开跑那一拍拿到（这里模拟那一拍）。
        match crate::merge_lock::acquire_with_holder(&service.state_dir, "card-1") {
            crate::merge_lock::MergeAttempt::Granted(lock) => {
                service.lock().merge_gate = Some(lock)
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::Released
        );
        // 状态一动不动：合没合上由模型自己判断并 `state ... done`。
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        // 名额已放掉，别人能拿到。`drop` 经内核解 flock 是同步的，但同一进程里
        // 另一条线程若恰在扫描同名 slot 也不该影响这里——所以直接断言，失败时
        // 把真实结局打出来。
        assert!(
            wait_until(|| matches!(
                crate::merge_lock::acquire_with_holder(&service.state_dir, "other"),
                crate::merge_lock::MergeAttempt::Granted(_)
            )),
            "the gate must be free after merge_finish"
        );
    }

    /// 记录在、人已死：`merge-finish` 要顺手清掉这条残留，否则界面永远显示
    /// `[merging]`。`flock` 本身早由内核释放，清记录不会制造「假免费」。
    #[test]
    fn merge_finish_clears_a_holder_record_left_by_a_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        std::fs::write(
            service.state_dir.join("merge.lock.holder"),
            r#"{"card_id":"card-1","pid":0,"since":"2026-10-01T00:00:00+08:00"}"#,
        )
        .unwrap();
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::Cleared
        );
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_none(),
            "a dead holder's note must not outlive it"
        );
    }

    #[test]
    fn merge_finish_reports_cleared_when_nothing_was_held() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::Cleared
        );
    }

    #[test]
    fn the_queue_head_starts_merging_when_the_gate_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        let action = service
            .driver_step(&[facts("thread-1", true, true)], &[])
            .unwrap()
            .expect("a merge turn");
        let DriverAction::Launch {
            card_id, objective, ..
        } = action;
        assert_eq!(card_id, "card-1");
        assert!(objective.contains("merge"), "{objective}");
        assert!(
            objective
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("already held"),
            "the daemon already holds the gate: {objective}"
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        let holder = crate::merge_lock::read_holder(&service.state_dir).expect("holder");
        assert_eq!(holder.card_id, "card-1");
    }

    /// 独立的合并卡（`add-merge`）出生即入合并队列，所以它必须由队列推进开跑，
    /// 而不是走实现卡的会话通路——那条路上的会话拿不到名额，卡会永远合不进去。
    #[test]
    fn a_delivered_merge_card_is_started_by_the_queue() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        git_run(&project, &["branch", "feat/manual"]);
        crate::dispatch::dispatch_with_service(
            &service,
            &service.state_dir,
            None,
            "enqueue_merge",
            &serde_json::json!({ "source": "feat/manual" }),
        )
        .unwrap();
        // 投递只是往 inbox 写文件；排进看板要等 daemon 消费那一拍。
        service.consume_inbox(chrono::Local::now());
        let board = persist::read_board(&service.state_dir);
        let merge_card = board
            .cards()
            .iter()
            .find(|card| card.kind == CardKind::Merge)
            .expect("the merge card was delivered")
            .clone();
        assert_eq!(merge_card.state, CardState::MergeQueued);
        let merge_id = merge_card.id.0.clone();
        let action = service
            .driver_step(&[], &[])
            .unwrap()
            .expect("a merge turn");
        let DriverAction::Launch {
            card_id, objective, ..
        } = action;
        assert_eq!(card_id, merge_id);
        assert!(objective.contains("feat/manual"), "{objective}");
        assert_eq!(state_of(&service, &merge_id), "running");
    }

    /// `list` 要能告诉人「谁在合并」——只靠状态看不出来：合并期间卡就是 running。
    #[test]
    fn list_marks_the_card_that_holds_the_merge_gate() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        let card = |service: &BoardService| {
            service.list()["cards"]
                .as_array()
                .unwrap()
                .iter()
                .find(|card| card["id"] == "card-1")
                .cloned()
                .unwrap()
        };
        assert_eq!(card(&service)["merging"], false);
        assert!(card(&service)["merge_queued_at"].is_string());
        let _ = service.driver_step(&[], &[]).unwrap();
        assert_eq!(card(&service)["merging"], true);
        // 已经开跑，队列时间就清掉了。
        assert!(card(&service)["merge_queued_at"].is_null());
    }

    /// 名额只能由持有 flock 的进程放掉。合并回合一跑完，会话里那句
    /// `merge-finish` 落在**另一个进程**里、只会回 `cleared`；若 daemon 自己不回收，
    /// 本项目此后再也合并不了。
    #[test]
    fn the_daemon_releases_the_gate_once_the_merge_turn_settles() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        service
            .driver_step(&[], &[])
            .unwrap()
            .expect("a merge turn");
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_some());

        // 回合还没收尾（卡仍是 running）→ 名额继续握着。
        service.driver_step(&[], &[]).unwrap();
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_some(),
            "a merge still in flight must keep the gate"
        );

        // 模型判定合并成功并落 `done` → 下一拍名额归还。
        service.set_state("card-1", CardState::Done, None).unwrap();
        service.driver_step(&[], &[]).unwrap();
        assert!(
            wait_until(|| crate::merge_lock::read_holder(&service.state_dir).is_none()),
            "the settled merge must hand the gate back"
        );
        assert!(
            wait_until(|| matches!(
                crate::merge_lock::acquire_with_holder(&service.state_dir, "next"),
                crate::merge_lock::MergeAttempt::Granted(_)
            )),
            "the next project merge must be able to take the gate"
        );
    }

    #[test]
    fn only_one_merge_starts_per_tick() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.enqueue_card_for_test("card-2");
        service
            .set_state("card-2", CardState::AwaitingMerge, None)
            .unwrap();
        service.merge_request("card-1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        service.merge_request("card-2").unwrap();
        assert_eq!(
            service.merge_request("card-2").unwrap(),
            MergeRequest::Queued { position: 1 }
        );
        let _ = service.driver_step(&[], &[]).unwrap().expect("first merge");
        assert_eq!(state_of(&service, "card-2"), "merge_queued");
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    #[test]
    fn a_held_gate_leaves_the_queue_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        let gate = crate::merge_lock::acquire_with_holder(&service.state_dir, "someone-else");
        assert!(matches!(gate, crate::merge_lock::MergeAttempt::Granted(_)));
        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        drop(gate);
    }

    /// 合并回合开在**新会话**里，所以旧会话忙不忙都不影响：一张排着队的卡
    /// 不该因为「它原来的会话还在跑」而被无限期压住。
    #[test]
    fn a_busy_old_session_does_not_block_the_merge_turn() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        let action = service
            .driver_step(&[facts("thread-1", false, true)], &[])
            .unwrap()
            .expect("a merge turn");
        let DriverAction::Launch { card_id, .. } = action;
        assert_eq!(card_id, "card-1");
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    #[test]
    fn merge_cancel_returns_a_queued_card_to_awaiting_merge() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        service.merge_cancel("card-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        let card = service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["id"] == "card-1")
            .cloned()
            .unwrap();
        assert!(
            card["merge_queued_at"].is_null(),
            "the timestamp must be cleared too"
        );
    }

    /// 已经开跑的合并不能再退：那会与一个正在跑的回合打架（spec §4.2）。
    #[test]
    fn merge_cancel_refuses_a_card_that_is_already_running_the_merge() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_awaiting_card(&project);
        git_run(&project, &["branch", &source]);
        service.merge_request("card-1").unwrap();
        service
            .set_state("card-1", CardState::Running, None)
            .unwrap();
        let error = service.merge_cancel("card-1").unwrap_err();
        assert!(error.contains("merge_queued"), "{error}");
    }

    #[test]
    fn merge_request_rejects_an_unknown_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(matches!(
            service.merge_request("nope").unwrap(),
            MergeRequest::Denied(_)
        ));
    }

    #[test]
    fn a_write_through_with_board_is_visible_to_the_service_list() {
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
        crate::persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        // `home` 必须隔离：传 `None` 会落到真实的全局租约池，与同机
        // 其它 cargo 测试抢同一批 slot，结果随并行度时过时不过。
        let service = BoardService::new(
            state_dir.clone(),
            dir.path().to_path_buf(),
            Some(dir.path().join("home")),
        );
        // 先建好 service（其内不再缓存看板），再由「外部写者」经 with_board 改盘。
        crate::persist::with_board(&state_dir, |board| {
            board
                .transition(&id, superpowers_kanban_core::card::CardState::Running)
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            service.list()["cards"][0]["state"],
            "running",
            "service 必须读到外部写入，而不是启动时的内存快照"
        );
    }

    /// 队列里两张 `queued` 卡：`driver_step` 只交出 `order` 最小的那张 launch，
    /// 且至多一个动作（spec D1）。
    #[test]
    fn driver_step_launches_the_head_of_the_queue_when_a_slot_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");

        let DriverAction::Launch {
            card_id,
            workdir,
            objective,
            ..
        } = service.driver_step(&[], &[]).unwrap().expect("a launch");
        assert_eq!(card_id, "card-1", "队首（order 最小）先走");
        assert!(workdir.join(".git").exists(), "worktree 由插件建好");
        // objective 由插件给出，措辞是契约（宿主不再复刻）。
        assert!(objective.contains("a.plan.md"), "{objective}");
        assert!(objective.contains("a.spec.md"), "{objective}");
        assert!(objective.contains("Never merge yourself"), "{objective}");
        assert_eq!(
            state_of(&service, "card-1"),
            "running",
            "认领即进入 running（launching 这个中间态已删）"
        );
        assert_eq!(state_of(&service, "card-2"), "queued", "一拍只走一张");
    }

    /// launch 必须带上**归属**（会话所属项目根 + 卡片句柄），否则被驱动会话在
    /// 侧栏不可见：宿主 `thread/listAll` 只在 `driver_project` 有值时才把会话折进
    /// 项目组，而卡片 worktree 自己的目录组会被抑制——两头落空就彻底消失，桌面
    /// 也就拿不到这条会话。宿主不认识看板，唯一同时知道「项目根」与「卡片 id」的
    /// 就是插件（spec §5.5「由插件侧在 driver/next 的动作里填值」）。
    #[test]
    fn a_launch_action_carries_the_project_root_and_the_card_ref() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);

        let action = service.driver_step(&[], &[]).unwrap().expect("a launch");
        let value = action.to_json();
        assert_eq!(value["kind"], "launch");
        assert_eq!(
            value["driver_project"],
            project.to_string_lossy().as_ref(),
            "launch 必须带上项目根，否则宿主折不进项目组、侧栏看不到这条会话"
        );
        assert_eq!(
            value["driver_ref"], "card-1",
            "launch 必须带上卡片句柄，桌面才会把它归进「看板会话」小节"
        );
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
        // 宿主**完全没报**这台会话（重启后它不记得任何 thread）：状态照旧不动，
        // 只在观测里标 stale——这是展示，不是迁移。
        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(state_of(&service, "card-1"), "running");
        assert_eq!(service.list()["cards"][0]["stale"], true);
    }

    /// 会话空闲**不再**等于「跑完了」：一张 running 卡在会话 idle 时状态必须
    /// 原样不动。这是误报的核心回归锁。
    #[test]
    fn an_idle_session_never_changes_a_running_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        let threads = vec![facts("thread-1", true, true)];
        assert!(service.driver_step(&threads, &[]).unwrap().is_none());
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    /// 宿主重启后它什么都不记得：卡也**不得**降级，只标 stale。
    #[test]
    fn a_session_the_host_forgot_never_changes_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert!(service.driver_step(&[], &[]).unwrap().is_none());
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    /// 并发按**活会话**计：一张卡被模型标了 done，但会话还活着，仍占名额。
    #[test]
    fn a_live_session_keeps_its_slot_even_after_the_card_is_done() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        one_slot_only(&service);

        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service.set_state("card-1", CardState::Done, None).unwrap();
        // 会话仍活着 → 名额继续占着，第二张起不来。
        let threads = vec![facts("thread-1", false, true)];
        assert!(service.driver_step(&threads, &[]).unwrap().is_none());
        // 会话死了 → 名额归还，第二张起得来。
        let threads = vec![facts("thread-1", false, false)];
        assert!(
            wait_until(|| service.driver_step(&threads, &[]).unwrap().is_some_and(
                |action| matches!(
                    action,
                    DriverAction::Launch { ref card_id, .. } if card_id == "card-2"
                )
            )),
            "the slot frees for card-2"
        );
    }

    /// 模型可以设任意非终态，但终态不可复活。
    #[test]
    fn the_model_may_set_any_live_state_but_not_revive_a_terminal_one() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service
            .set_state("card-1", CardState::AwaitingMerge, Some("plan done"))
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        assert_eq!(detail_of(&service, "card-1").as_deref(), Some("plan done"));
        service.set_state("card-1", CardState::Done, None).unwrap();
        let error = service
            .set_state("card-1", CardState::Running, None)
            .unwrap_err();
        assert!(error.contains("terminal"), "{error}");
    }

    /// 终态不挂旧判词：`done` 的卡不该还写着「为什么还没做完」。
    #[test]
    fn setting_a_terminal_state_clears_the_stale_detail() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service
            .set_state("card-1", CardState::Running, Some("work in progress"))
            .unwrap();
        service.set_state("card-1", CardState::Done, None).unwrap();
        assert_eq!(detail_of(&service, "card-1"), None);
    }

    #[test]
    fn set_state_rejects_an_unknown_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        assert!(
            service
                .set_state("nope", CardState::Done, None)
                .unwrap_err()
                .contains("unknown card")
        );
    }

    /// `list` 把 stale 与最后活着时刻一并透出。
    #[test]
    fn list_marks_a_running_card_stale_when_its_session_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .driver_step(&[facts("thread-1", false, true)], &[])
            .unwrap();
        assert_eq!(service.list()["cards"][0]["stale"], false);
        service
            .driver_step(&[facts("thread-1", false, false)], &[])
            .unwrap();
        let card = &service.list()["cards"][0];
        assert_eq!(card["stale"], true);
        assert!(
            card["last_alive_at"].is_string(),
            "要带上最后活着的时刻: {card}"
        );
        // 观测**永不**写回状态。
        assert_eq!(card["state"], "running");
    }

    /// 端到端：一张卡从排队到 done 的通路里，daemon 只负责启动，状态全部由
    /// 「模型」（这里由测试代劳调 `set_state`）声明。
    #[test]
    fn a_card_moves_only_where_the_model_puts_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);

        let DriverAction::Launch {
            card_id, action_id, ..
        } = service.driver_step(&[], &[]).unwrap().expect("launch");
        // 回报带回 thread id（launch 已把卡置成 running）。
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[DriverResult {
                    action_id,
                    ok: true,
                    thread_id: Some("thread-1".into()),
                    error: None,
                }],
            )
            .unwrap();
        assert_eq!(state_of(&service, &card_id), "running");

        // 会话空闲、模型什么都没写 → 状态不动。
        assert!(
            service
                .driver_step(&[facts("thread-1", true, true)], &[])
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, &card_id), "running");

        // 模型问用户问题 → needs_you；回话后继续 → running。
        service
            .set_state(&card_id, CardState::NeedsYou, Some("which DB?"))
            .unwrap();
        assert_eq!(state_of(&service, &card_id), "needs_you");
        service
            .set_state(&card_id, CardState::Running, None)
            .unwrap();
        // 干完 → awaiting_merge；申请名额、合并、释放、自己判 done。
        service
            .set_state(&card_id, CardState::AwaitingMerge, None)
            .unwrap();
        assert_eq!(
            service.merge_request(&card_id).unwrap(),
            MergeRequest::Queued { position: 0 }
        );
        assert_eq!(state_of(&service, &card_id), "merge_queued");
        // 下一拍：名额空 → daemon 把卡推进到 running 并追一回合（见 promote_merge_queue）。
        match crate::merge_lock::acquire_with_holder(&service.state_dir, &card_id) {
            crate::merge_lock::MergeAttempt::Granted(lock) => {
                service.lock().merge_gate = Some(lock)
            }
            other => panic!("expected Granted, got {other:?}"),
        }
        service
            .set_state(&card_id, CardState::Running, None)
            .unwrap();
        assert_eq!(
            service.merge_finish(&card_id).unwrap(),
            MergeFinish::Released
        );
        service.set_state(&card_id, CardState::Done, None).unwrap();
        assert_eq!(state_of(&service, &card_id), "done");
    }

    /// D12/D5：launch 结果带回 `thread_id` → `mark_running`（映射由插件持有）。
    #[test]
    fn driver_step_marks_the_card_running_from_a_launch_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let DriverAction::Launch { action_id, .. } = launched(&service);
        assert_eq!(state_of(&service, "card-1"), "running");

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
        let DriverAction::Launch { action_id, .. } = launched(&service);

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
        assert_eq!(state_of(&service, "card-2"), "running", "认领即 running");
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
        let DriverAction::Launch { action_id, .. } = launched(&service);

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

    /// `board.release` 与 CMD 启动通路是同一处旧缺口（启动没成 → `failed`）：它的
    /// `detail` 也必须落到卡片上，否则「为什么失败」在这条通路上照样无人可答。
    /// （该方法的去留见 `BoardService::release` 上的说明：spec §6 保留它。）
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
    ///
    /// 宿主**仍认得**这台会话（它把 `thread-1` 报了回来且报「忙」），故恢复规则
    /// 不介入（那一类见 `driver_step_does_not_recover_a_card_with_an_action_in_flight`）；
    /// 本用例只管回报的幂等：第二条同 id 的回报找不到待办条目、原地落空。
    #[test]
    fn driver_step_ignores_a_redelivered_launch_result() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        one_slot_only(&service);
        let DriverAction::Launch { action_id, .. } = launched(&service);

        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
        };
        assert!(
            service
                .driver_step(&busy(&["thread-1"]), std::slice::from_ref(&result))
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        // 重复投递：不报错、不改状态、不多占一个名额。
        assert!(
            service
                .driver_step(&busy(&["thread-1"]), std::slice::from_ref(&result))
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
            driver_project: "/proj".to_string(),
            driver_ref: "card-1".to_string(),
            env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
        };
        let value = launch.to_json();
        assert_eq!(value["action_id"], "act-1");
        assert_eq!(value["kind"], "launch");
        assert_eq!(value["workdir"], "/w/card-1");
        assert_eq!(value["title"], "看板 · a.spec");
        assert_eq!(value["objective"], "objective");
        assert!(value.get("card_id").is_none(), "{value}");
        assert_eq!(
            value["env"]["SUPERPOWERS_KANBAN_CARD"], "card-1",
            "插件自己决定要注入的键，宿主只透传: {value}"
        );
    }

    /// 宿主与插件共用的 `driver/next` 契约（**与宿主 `session_driver.rs` 的测试读
    /// 同一份文件**，两侧都用 `include_str!`）。
    ///
    /// 这里钉住插件这一侧的两件事：① `DriverAction::to_json` 交出的形状与契约字面量
    /// **按值**相等（键序不是契约，故比较解析后的 `Value`）；② 契约里的 `results[]`
    /// 条目能反序列化成 `DriverResult`。插件若改字段名，这两条就红——否则宿主
    /// `parse_action` 会悄悄回 `None`，或插件的 `DriverResult` 反序列化失败。
    const DRIVER_CONTRACT: &str = include_str!("../contract/driver-protocol.json");

    /// 契约里的**请求**字段名，插件手工解析 `driver/next` 参数时认的就是它们
    /// （`dispatch.rs`）。宿主改了发出侧的名字，这条会红。
    #[test]
    fn the_driver_contract_request_uses_the_field_names_the_plugin_parses() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        let threads = contract["request"]["threads"]
            .as_array()
            .expect("契约必须钉住 threads[] 条目形状");
        assert!(
            threads
                .iter()
                .all(|thread| thread.get("thread_id").is_some() && thread.get("idle").is_some()),
            "threads[] 条目只由 `thread_id` + `idle` 构成：{threads:?}"
        );
        let results = contract["request"]["results"]
            .as_array()
            .expect("契约必须钉住 results[] 条目形状");
        for entry in results {
            for key in ["action_id", "ok", "thread_id", "error"] {
                assert!(
                    entry.get(key).is_some(),
                    "results[] 条目必须带 `{key}`：{entry}"
                );
            }
        }
        // 契约的 results[] 条目就是插件 `DriverResult` 认的形状。
        let parsed: Vec<DriverResult> =
            serde_json::from_value(contract["request"]["results"].clone())
                .expect("契约的 results[] 必须能反序列化成 DriverResult");
        assert_eq!(parsed[0].action_id, "act-1");
        assert!(parsed[0].ok);
        assert_eq!(parsed[0].thread_id.as_deref(), Some("thread-1"));
        assert_eq!(parsed[0].error, None);
    }

    /// 插件**交出**的动作形状与契约字面量按值相等：宿主 `parse_action` 认的就是这份
    /// 契约里的键名（`launch` 的 workdir/title/objective，`append` 的 thread_id/
    /// objective）。改一侧的名字而没同步契约，这条就红。
    #[test]
    fn the_driver_contract_answers_match_what_the_plugin_emits() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        let answers = &contract["answers"];

        let launch = DriverAction::Launch {
            action_id: "act-2".to_string(),
            card_id: "card-1".to_string(),
            workdir: PathBuf::from("/w/card-1"),
            title: "看板 · a.spec".to_string(),
            objective: "do the card".to_string(),
            driver_project: "/proj".to_string(),
            driver_ref: "card-1".to_string(),
            env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
        };
        assert_eq!(
            launch.to_json(),
            answers["launch"],
            "launch 的字段名与契约必须一致"
        );

        // 插件不再发 `append`：合并由用户在会话里发话触发，插件只负责启动。
        assert!(
            answers.get("append").is_none(),
            "契约里不该再有 append 应答"
        );

        // 「无事可做」的契约形状：`{"action": null}`。
        assert_eq!(answers["nothing"], serde_json::json!({ "action": null }));
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// 建一个把 `home` 指到临时目录的 service（登记与租约都不碰真实 HOME）。
    fn service(dir: &Path) -> BoardService {
        let state_dir = dir.join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();
        BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
    }

    fn registry(dir: &Path) -> PathBuf {
        dir.join("home/.yi-agent/resident-daemons.json")
    }

    #[test]
    fn lifecycle_create_registers_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        service.lifecycle_create(&project).unwrap();

        // 登记落在宿主读的那个文件里，形状与宿主契约一致。
        let text = std::fs::read_to_string(registry(dir.path())).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["projects"][0]["project"],
            project.to_string_lossy().as_ref()
        );
        assert_eq!(value["projects"][0]["required_by"][0], "superpowers-kanban");
        // 插件自己的状态目录也建出来了。
        assert!(project.join(".yi-agent/superpowers-kanban").is_dir());
    }

    #[test]
    fn lifecycle_create_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        service.lifecycle_create(&project).unwrap();
        service.lifecycle_create(&project).unwrap();

        let text = std::fs::read_to_string(registry(dir.path())).unwrap();
        assert_eq!(text.matches("superpowers-kanban").count(), 1, "{text}");
    }

    #[test]
    fn lifecycle_remove_unregisters_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        service.lifecycle_create(&project).unwrap();

        service.lifecycle_remove(&project).unwrap();

        // 「不再登记」+「状态目录已删」——不是「进程已停」（那是宿主的活）。
        let projects = resident::list(&dir.path().join("home/.yi-agent"));
        assert!(projects.is_empty(), "{projects:?}");
        assert!(!project.join(".yi-agent/superpowers-kanban").exists());
    }

    #[test]
    fn lifecycle_remove_is_safe_to_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let service = service(dir.path());
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();

        // 从未 create 过也能 remove（幂等），再来一次也安全。
        service.lifecycle_remove(&project).unwrap();
        service.lifecycle_remove(&project).unwrap();
    }

    #[test]
    fn the_dispatch_channel_exposes_lifecycle_create_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        // 走注入版分派：生产入口 `dispatch()` 里的 `service_for` 用 `home = None`，
        // 那会落到**真实** `$HOME/.yi-agent`——测试绝不能写用户的真登记表
        // （失败时会把临时目录的登记永久漏进去）。
        let service = BoardService::new(
            state_dir.clone(),
            project.clone(),
            Some(dir.path().join("home")),
        );

        let created = crate::dispatch::dispatch_with_service(
            &service,
            &state_dir,
            None,
            "lifecycle/create",
            &serde_json::json!({ "project": project.to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(created["ok"], true);

        let removed = crate::dispatch::dispatch_with_service(
            &service,
            &state_dir,
            None,
            "lifecycle/remove",
            &serde_json::json!({ "project": project.to_string_lossy() }),
        )
        .unwrap();
        assert_eq!(removed["ok"], true);

        // 登记写在注入的 home 下，真实 `$HOME` 未被碰过。
        assert!(registry(dir.path()).exists());
        assert!(resident::list(&dir.path().join("home/.yi-agent")).is_empty());
    }

    #[test]
    fn lifecycle_needs_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        // 缺 project：给一条说清楚的错，而不是静默 no-op。create 与 remove 都要挡。
        for method in ["lifecycle/create", "lifecycle/remove"] {
            let error =
                crate::dispatch::dispatch(&state_dir, method, &serde_json::json!({})).unwrap_err();
            assert!(error.contains("project"), "{method}: {error}");
            let error = crate::dispatch::dispatch(
                &state_dir,
                method,
                &serde_json::json!({ "project": "" }),
            )
            .unwrap_err();
            assert!(error.contains("project"), "{method}: {error}");
        }
    }
}
