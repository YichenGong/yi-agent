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

/// 解析实例根：`YI_AGENT_HOME` 优先，否则 `<HOME>/.yi-agent`。
///
/// 与 `yi_agent_store::paths::instance_home` 同一口径（含"值本身已叫
/// `.yi-agent` 时不重复拼接"这一条）。插件不链接那个 crate，所以在这里写一份
/// 同样的规则；`drain.rs` 只依赖它的**结果**目录形状。
///
/// 与 `home` 参数不同：`home` 是测试注入的全局层替身，实例根是宿主真正记账的
/// 地方。把两者混为一谈会让注入 `home` 的测试永远看不到宿主日志。
fn instance_home_from_env() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("YI_AGENT_HOME").filter(|value| !value.is_empty()) {
        let dir = PathBuf::from(value);
        return Some(
            if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
                dir
            } else {
                dir.join(".yi-agent")
            },
        );
    }
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".yi-agent"))
}

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
    /// 有安全记录还没结算:名额要等 daemon 按 journal + 观测放,这里什么都不能做。
    ///
    /// 这不是失败,而是"你说的那一步还缺证据"。保留这个动词是为了兼容旧提示,
    /// 但它**不再**能在合并回合中途把门放走（spec §8.2）。
    Waiting,
}

/// 一条被受理的合并记录，此刻能不能安全地放掉（spec §8.2）。
///
/// 抽成纯函数是为了让释放与"故障矩阵"用同一个判据：这五条里少任何一条都会
/// 变成"模型写个 done 就把门放走了"，而那正是旧行为的 bug。
fn can_release_merge(
    phase: &crate::merge_attempt::MergePhase,
    pending: bool,
    accepted_on_tick: Option<u64>,
    tick_seq: u64,
    card_matches_and_finished: bool,
    thread: Option<&ThreadFacts>,
) -> bool {
    *phase == crate::merge_attempt::MergePhase::Accepted
        // 还有在飞的续跑动作:结果都没回来,谈不上结束。
        && !pending
        // 必须**晚于**受理那一拍。同一个 `driver/next` 请求里带的 idle 快照
        // 可能发生在结果生效之前,拿它当"已经停了"就是另一个早放。
        && accepted_on_tick.is_some_and(|accepted| tick_seq > accepted)
        && card_matches_and_finished
        && thread.is_some_and(|fact| fact.alive && fact.idle)
}

/// 一张卡合并时的**来源分支**。
///
/// 独立合并卡把 source 落在 `source_ref` 上；实现卡没有这个字段——它的分支是
/// `next_launch` 建 worktree 时按 `kanban/<slug>` 起的。于是「空」不等于「没有
/// 来源」,只等于「按规则推导」:把空串拼进 objective 会让模型去合一个不存在的
/// 分支,而这正是合并回合唯一一次的输入。
fn merge_source(card: &superpowers_kanban_core::card::Card) -> String {
    card.source_ref
        .as_deref()
        .map(str::trim)
        .filter(|source| !source.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("kanban/{}", slugify(&card.id)))
}

/// 本拍宿主上报的会话事实里有没有这一个。
fn thread_fact<'a>(threads: &'a [ThreadFacts], thread_id: &str) -> Option<&'a ThreadFacts> {
    threads.iter().find(|fact| fact.thread_id == thread_id)
}

/// 一张排队合并卡这一拍该怎么处置（`promote_merge_queue` 的分派结果）。
#[derive(Debug, Clone)]
enum MergeBinding {
    /// 回到原开发会话追加一轮。动作 id 由调用方填。
    Resume(ResumeLedger),
    /// 独立合并卡（没有实现计划）:沿用 `Launch`。
    Standalone,
    /// 这一拍先不派,但保留排队与顺位（例如宿主还不支持所需动作）。
    Defer { reason: String },
    /// 这一拍明显不可用（会话正忙）,连候选地位都让出去。
    Skip,
    /// 绑定本身有问题,只能交给人工。
    NeedsBinding { reason: String },
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
    /// 恢复**原开发会话**（同一 thread id）后追加一轮。
    ///
    /// 与 `Launch` 的区别是「回到原来那个会话」：`thread_id`/`workdir` 是原会话
    /// 的精确定位（**不是**合并目标树），宿主必须用它恢复同一会话再投递。带上
    /// `card_id` 是因为插件要记账「这张卡的动作属于哪张卡」，它**不进 JSON**。
    ResumeAppend {
        action_id: String,
        card_id: String,
        thread_id: String,
        /// 原会话的精确定位目录（**不是** merge 目标树）。
        workdir: PathBuf,
        objective: String,
        /// 冷装配时注入工具进程的环境变量；热会话不重建工具，故不生效。
        env: Vec<(String, String)>,
    },
}

/// 宿主在 `driver/next` 请求里声明的能力（spec §5.2）。
///
/// `Default` 是**一个都不支持**——旧宿主不发这块时，插件必须保留排队并说明
/// 原因，绝不能猜「大概支持吧」然后交出一个宿主执行不了的动作，更不能退化成
/// `Launch`（那会切断上下文，正是这次要修的行为）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriverCapabilities {
    pub actions: Vec<String>,
}

impl DriverCapabilities {
    /// 精确匹配：能力名是跨进程契约的一部分，不做前缀/大小写归一。
    pub fn supports(&self, action: &str) -> bool {
        self.actions.iter().any(|known| known == action)
    }
}

impl DriverAction {
    /// 跨进程契约：字段名与宿主 `session_driver::parse_action` 逐字对齐（spec §4.3）。
    /// 用 `action_id` 作键而不是直接序列化枚举——枚举的形状是实现细节，不能外泄。
    pub fn to_json(&self) -> Value {
        match self {
            DriverAction::ResumeAppend {
                action_id,
                thread_id,
                workdir,
                objective,
                env,
                // `card_id` 是插件内部记账键，**不进 JSON**：宿主不认识看板。
                ..
            } => json!({
                "action_id": action_id,
                "kind": "resume_append",
                "thread_id": thread_id,
                "workdir": workdir.to_string_lossy(),
                "objective": objective,
                "env": env.iter().cloned().collect::<std::collections::BTreeMap<_, _>>(),
            }),
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
    /// 宿主**显式分类**过的失败码（spec §5.2）。
    ///
    /// `None`（或旧宿主根本没这个字段）表示「宿主没归类」——插件必须按不确定
    /// 处理，不能拿 `error` 文本猜「大概是 busy 吧」然后安全退队。
    #[serde(default)]
    pub error_code: Option<String>,
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
    /// 第几拍。每拍 `driver_step` 单调 +1，**只活在进程内**：重启后从 0 再来，
    /// 所以它绝不能是任何持久判据的一部分。它唯一的用途是回答「这条结果之后
    /// 至少又过了一拍吗」——同一个 `driver/next` 请求里带来的 idle 观测可能发生
    /// 在结果生效之前，用它卡住那次释放。
    driver_tick_seq: u64,
    /// 记录被受理（`Accepted`）在第几拍。`None` 表示还没有一条被受理的合并记录。
    merge_accepted_on_tick: Option<u64>,
    /// 本实例在整机活动表里的登记（`<quota-domain>/activity/<owner>.activity`）。
    ///
    /// 惰性建立：只有真的推进过一拍才写。`None` 表示配额域不可解析（`HOME`
    /// 缺失）或还没发布过——两者都不该让本实例"看起来在跑"。
    activity: Option<crate::quota::LocalActivity>,
}

/// 一个已交出、等回报的动作。
struct PendingAction {
    card_id: CardId,
    kind: PendingKind,
    /// `ResumeForMerge` 的派发身份。结算必须能证明「这条回报属于我刚才发的那一
    /// 次」：动作 id 由本进程的 nonce 保证不撞，加上 action id 与派发时绑定的
    /// `(thread, workdir)`，迟到的旧回报、串了线的回报都对不上。
    resume: Option<ResumeLedger>,
}

/// 一次续跑派发的账：动作 id、期望绑定的 thread/workdir、原入队时刻。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResumeLedger {
    action_id: String,
    thread_id: String,
    expected_workdir: PathBuf,
    merge_queued_at: Option<DateTime<Local>>,
}

/// 动作种类。回报的处置因种类而异：`launch` 靠 `thread_id` 让卡进入 `running`；
/// `launch-for-merge` 是「为一张**没有会话的独立合并卡**开合并会话」，回报只落
/// `card_id → thread_id` 映射——那张卡的状态由模型自己维护，插件不替它翻。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Launch,
    LaunchForMerge,
    /// 给**实现卡的原会话**追加一轮合并指令（本需求的核心动作）。
    ///
    /// 与 `LaunchForMerge` 的两点不同必须记住：① 它不占新会话，成功回报只落到
    /// 已有的 `card_id → thread_id` 上，而那张卡的绑定在派发时就已经定死；② 它的
    /// 失败分两类——busy 证明**没投递**（可以安全退队重排），其余会把这张卡交给
    /// 人工，但无论哪类都不能留下一个假的安全门。
    ResumeForMerge,
}

pub struct BoardService {
    state_dir: PathBuf,
    project_root: PathBuf,
    /// 实例根（`YI_AGENT_HOME` 解析出的 `<…>/.yi-agent`）。
    ///
    /// 只用来**读**宿主的事务日志（`plugins/lifecycle/<plugin>.json`）：排空
    /// 期间要放行收尾、挡住新任务，而排空的权威在宿主。`None` 表示解析不出实例
    /// 根——那时按"没在排空"处理，因为拿 `$HOME` 猜一个实例根会在多实例机器上
    /// 读到另一个实例的日志。
    instance_home: Option<PathBuf>,
    /// 通用常驻登记目录（`$HOME/.yi-agent`）。`home` 注入时为 `<home>/.yi-agent`，
    /// 于是测试不必碰真实 `$HOME`（与 `leases_dir` 同一手法）。
    resident_dir: Option<PathBuf>,
    /// 是否跑在宿主的受管上下文里。
    ///
    /// 只影响一件事：**常驻登记的归属**。设计 §7.1 把"常驻项目登记与后台服务
    /// 引用"划给宿主——插件通过能力接口请求，不直接改宿主登记文件。受管时我们
    /// 没有那条能力接口（宿主尚未提供），所以诚实的做法是**不写**那个文件并把
    /// 事实说出来，而不是继续用共享文件契约冒充"请求已经送达"。
    ///
    /// standalone（用户手敲命令、无 binding）时沿用旧行为：写共享登记文件，
    /// 因为那时没有宿主可以代劳。
    managed: bool,
    /// 全局配置所在的 home：`<home>/.yi-agent/plugins/superpowers-kanban/`。
    ///
    /// 注入后**不读**真实 `$HOME`——否则测试会读到跑测试那台机器上的真实配额，
    /// 结果随环境而变。`None` 时才回退到真实 `HOME`。
    home: Option<PathBuf>,
    inner: Mutex<Inner>,
}

impl BoardService {
    pub fn new(state_dir: PathBuf, project_root: PathBuf, home: Option<PathBuf>) -> Self {
        // 先按借用算 resident_dir，再让 leases_dir 的 `map` 消费 `home`。
        let resident_dir = home
            .as_ref()
            .map(|home| home.join(".yi-agent"))
            .or_else(resident::default_dir);
        // 实例根与 `home` 注入无关：`home` 是测试用的全局层替身，实例根是宿主
        // 真正记账的地方。注入 `home` 的测试若也把实例根指到那里，排空测试就
        // 写不了宿主日志的真实落点；所以这里只认 `YI_AGENT_HOME` / 真实 `HOME`，
        // 测试通过 `with_instance_home` 显式替换。
        let instance_home = instance_home_from_env();
        let leases_dir = home
            .as_ref()
            .map(|home| {
                home.join(".yi-agent")
                    .join("superpowers-kanban")
                    .join("leases")
            })
            .or_else(lease::global_leases_dir);
        Self {
            state_dir,
            project_root,
            instance_home,
            resident_dir,
            // `new` 被 dispatch 的进程内单例与测试大量调用，所以这里**不**重新
            // 解析环境（那会让每条单测都依赖开发机的环境变量）。默认 standalone，
            // 受管入口显式打开。
            managed: false,
            home,
            inner: Mutex::new(Inner {
                leases: Default::default(),
                leases_dir,
                pending_actions: Default::default(),
                merge_gate: None,
                next_action_seq: 0,
                driver_tick_seq: 0,
                merge_accepted_on_tick: None,
                action_nonce: format!(
                    "{}-{}",
                    std::process::id(),
                    chrono::Local::now().timestamp_millis()
                ),
                activity: None,
            }),
        }
    }

    /// 覆盖实例根。测试用：让排空判定读一个 TempDir 里的宿主日志，而不是
    /// 跑测试那台机器的真实实例。
    ///
    /// 生产路径不调它——`new` 已经从环境解析好了。
    pub fn with_instance_home(mut self, instance_home: Option<PathBuf>) -> Self {
        self.instance_home = instance_home;
        self
    }

    /// 声明本实例跑在宿主的受管上下文里：常驻登记改由宿主负责，插件不写
    /// 宿主登记文件（设计 §7.1）。
    pub fn with_managed(mut self, managed: bool) -> Self {
        self.managed = managed;
        self
    }

    /// 当前是否在宿主的排空屏障内（只读宿主事务日志）。
    ///
    /// 解析不出实例根时报 [`Drain::Idle`]：没有实例根就没有可读的日志，"没在
    /// 排空"是唯一不猜的答案。拿 `$HOME` 兜底会读到另一个实例的事务。
    pub fn drain(&self) -> crate::drain::Drain {
        match self.instance_home.as_deref() {
            Some(home) => crate::drain::read(home),
            None => crate::drain::Drain::Idle,
        }
    }

    /// 上游依赖的就绪诊断（缺 skill / 同名冲突）。
    ///
    /// `instance_root` 是**实例根的父目录**（`skill_rpc::roots` 会再拼
    /// `.yi-agent/skills`）；解析不出时退化为"只看项目层"——只报项目里缺什么，
    /// 不凭空断言用户层也没有。
    pub fn readiness(&self) -> crate::readiness::Readiness {
        let declared = crate::readiness::declared_workflows(include_str!("../../../plugin.json"))
            .unwrap_or_default();
        let home = self
            .instance_home
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| self.project_root.clone());
        crate::readiness::check(&declared, &self.project_root, &home)
    }

    /// 为什么现在不能起新的自动任务（`None` 表示可以）。
    ///
    /// **只挡新会话**：只读、状态收尾、归档都不经过这里。两个来源：
    /// 上游依赖不就绪（设计 §7.2），或宿主正在排空（设计 §7）。
    pub fn launch_blocked_reason(&self) -> Option<String> {
        let drain = self.drain();
        if !drain.allows_new_work() {
            return drain.reason();
        }
        let readiness = self.readiness();
        readiness.view_only_reason()
    }

    /// 中毒的锁照样用：状态是一份纯数据，`panic` 不会留下半写状态，
    /// 因此毒化不值得让整个推进循环停摆。
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// 本拍**允许写进观测旁路**的会话 ID 集合。
    ///
    /// 只收「看板里真的有约束关系」的 ID，绝不把宿主整张热表抄进来：那样
    /// `observations.json` 会变成一份会话世界地图，谁在别的项目里聊过什么都要落
    /// 到本项目的 state-dir 里。
    ///
    /// 来源（全部要求非空 ID）：
    ///
    /// - 看板全体卡片的执行 `thread_id`（**含已归档**：归档卡的状态还要经
    ///   `list` 展示，剪掉就等于把它们的时间线抹了）；
    /// - 仍有效 merge attempt 记录里绑定的会话；
    /// - 本进程 `ResumeForMerge` 待办里的派发身份；
    /// - 命中本进程待办、且成功回报带来的 `Launch`/`LaunchForMerge` 会话。
    ///
    /// 明确排除：`origin_thread_id`（它是「用户在哪里点的」来源会话，不是执行
    /// 绑定）、陌生 `action_id` 带回来的 ID、purge 掉的孤儿记录、空/纯空白 ID。
    ///
    /// 锁顺序：先在短锁内复制待办标识，立刻释放，再去读 board/attempt。**绝不**
    /// 持 `inner` 读磁盘，也不持 board 锁等 `inner`。
    fn related_thread_ids(
        &self,
        results: &[DriverResult],
    ) -> Result<std::collections::BTreeSet<String>, String> {
        // 1) 短锁：只复制「动作 id → 种类/卡片/续跑身份」，马上放锁。
        let pending: Vec<(String, PendingKind, CardId, Option<String>)> = {
            let inner = self.lock();
            inner
                .pending_actions
                .iter()
                .map(|(action_id, action)| {
                    (
                        action_id.clone(),
                        action.kind,
                        action.card_id.clone(),
                        action
                            .resume
                            .as_ref()
                            .map(|ledger| ledger.thread_id.clone()),
                    )
                })
                .collect()
        };

        let mut relevant = std::collections::BTreeSet::new();
        let mut insert = |id: &str| {
            if !id.trim().is_empty() {
                relevant.insert(id.to_string());
            }
        };

        // 2) 看板：首次运行（文件不存在）= 空看板；坏读 = Err（回报还没消费，
        //    下一拍可以重试）。
        let board = persist::load_board_strict(&self.state_dir.join("board.json"))?;
        for card in board.cards() {
            if let Some(thread_id) = card.thread_id.as_deref() {
                insert(thread_id);
            }
        }

        // 3) 合并安全记录：它绑定的会话仍是本项目的一张卡在执行。
        if let Some(record) = crate::merge_attempt::read(&self.state_dir)? {
            if let Some(thread_id) = record.thread_id.as_deref() {
                insert(thread_id);
            }
        }

        // 4) 待办的续跑身份：那是「这条指令要投给谁」的既有绑定。
        for (_, kind, _, resume_thread) in &pending {
            if *kind == PendingKind::ResumeForMerge {
                if let Some(thread_id) = resume_thread.as_deref() {
                    insert(thread_id);
                }
            }
        }

        // 5) 本拍成功回报带来的新绑定：只认命中本进程待办的动作。
        for result in results.iter().filter(|result| result.ok) {
            let Some((_, kind, _, resume_thread)) = pending
                .iter()
                .find(|(action_id, ..)| action_id == &result.action_id)
            else {
                continue;
            };
            match kind {
                // 新会话：回报带来的 ID 本身就是绑定。
                PendingKind::Launch | PendingKind::LaunchForMerge => {
                    if let Some(thread_id) = result.thread_id.as_deref() {
                        insert(thread_id);
                    }
                }
                // 续跑：只认**等于**派发身份的 ID。回别的 id 属于串线，
                // 结算层会把它标成 uncertain，观测层更不能替它建立新绑定。
                PendingKind::ResumeForMerge => {
                    if let (Some(reported), Some(expected)) =
                        (result.thread_id.as_deref(), resume_thread.as_deref())
                    {
                        if reported == expected {
                            insert(reported);
                        }
                    }
                }
            }
        }

        Ok(relevant)
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
    /// 兼容入口：旧的 typed `threads` 调用面保留，内部委托给新入口。
    ///
    /// 显式 typed `ThreadFacts.alive` 是宿主**已提供**的证据（缺字段的保守默认
    /// 在 `dispatch` 解析时就已经分开了），所以这里把每个条目都算作
    /// `reported_alive`，`loaded` 为 `None`（没有完整快照能力）。
    ///
    /// 这不是「另一套流程」：归一化、保存、回报消费、gate、出租约全在
    /// [`Self::driver_step_with_loaded`] 里，只有一处顺序。
    pub fn driver_step(
        &self,
        threads: &[ThreadFacts],
        results: &[DriverResult],
        sessions_running: Option<u32>,
        // 宿主声明的能力。缺省（旧宿主）=> 一个都不支持：保留排队并说明原因，
        // 绝不猜、也绝不退化成本地能执行的动作。
        capabilities: &DriverCapabilities,
    ) -> Result<Option<DriverAction>, String> {
        let input = crate::driver_observation::ObservationInput {
            legacy: threads.to_vec(),
            reported_alive: threads
                .iter()
                .map(|thread| (thread.thread_id.clone(), thread.alive))
                .collect(),
            loaded: None,
        };
        self.driver_step_with_loaded(&input, results, sessions_running, capabilities)
    }

    /// 本插件**唯一**的推进入口（新签名）。
    ///
    /// 顺序是安全性质的一部分，不要重排：
    ///
    /// 1. 前置检查（租约目录）与相关 ID 读取；
    /// 2. 归一化本拍观测；
    /// 3. **原子保存**旁路——失败就整拍返回错误，绝不消费回报；
    /// 4. 推进 tick 序号、消费回报、gate 结算、补账、调度。
    ///
    /// 为什么保存必须早于消费：消费之后的租约对账读的是**已落盘**的旁路；
    /// 若先消费再落盘而落盘失败，回报已经没了、名额却还按旧文件算，那张卡
    /// 的账就永久丢了。失败时把错误交回宿主，下一拍会重发同一批 results。
    pub fn driver_step_with_loaded(
        &self,
        input: &crate::driver_observation::ObservationInput,
        results: &[DriverResult],
        sessions_running: Option<u32>,
        capabilities: &DriverCapabilities,
    ) -> Result<Option<DriverAction>, String> {
        // 前置检查必须**早于**消费回报：拿不到全局租约目录时 `next_launch` 只会
        // 整拍报错，而错误会让宿主把这批回报当作「没送到」重发；可回报已经消费
        // 掉了，重发的那份会被当成陌生动作丢弃，那次账就永久丢了。
        if self.lock().leases_dir.is_none() {
            return Err("no lease directory".into());
        }
        // 2) 相关 ID：只持久化看板真正绑定的执行会话，别把宿主全表抄进来。
        //    坏 board / 坏 attempt 在这里就报错——回报还没消费，下一拍可重试。
        let relevant = self.related_thread_ids(results)?;
        let previous = crate::observation::load(&self.state_dir);
        let normalized =
            crate::driver_observation::normalize(input, &relevant, &previous, chrono::Local::now());
        // 3) 旁路先落盘。失败 => 不推进序号、不消费回报、不释放任何资源。
        crate::observation::save(&self.state_dir, &normalized.file)
            .map_err(|error| format!("could not save session observations: {error}"))?;
        let threads = &normalized.facts;

        // 本拍序号先进。回报、观测、释放、派发都用同一个数字,所以"这一拍"
        // 在下面所有判断里含义一致。
        let tick_seq = {
            let mut inner = self.lock();
            inner.driver_tick_seq += 1;
            inner.driver_tick_seq
        };
        // 4) 回报：宿主上一拍执行的结果在这里落账。
        //
        // 这一拍要是结算了一次续跑合并，就**不再派新的合并**：退队的卡马上又
        // 满足条件，同一个 tick 里再派一次等于把"一拍一个动作"变成自旋重试，
        // 而这一拍还没有任何新观测能证明它这次不会又忙。
        let settled_merge = self.consume_results(results);
        // 合并回合一结束就还名额：这一步只能由持有 flock 的本进程做。
        self.release_gate_when_the_merge_is_over(threads, tick_seq);
        // 本拍的**全机余量**：峰值 − 全机运行会话数。合并队列与启动共用这一份——
        // 两者都是真实的模型回合，吃的是同一份额度。
        //
        // 扣减基准是宿主下发的全机计数，不是本看板自己的卡片状态：看板内那几张卡
        // 也在全机计数里，再减一次就是双重扣减；反过来，一张标了 `done` 但会话还
        // 开着的卡不算「在跑」也是对的——名额由租约按活会话记，状态只描述进展。
        let limit = self.calendar().limit_at(chrono::Local::now());
        // 先把本进程此刻的会话数**发布**到整机活动表，再算余量。顺序不能反：
        // 反了的话本拍读到的是上一拍的自己，刚起的会话会少算一个名额。
        self.publish_activity(sessions_running.unwrap_or(0));
        let budget = self.launch_budget(sessions_running);
        // 3) 合并队列：用户确认过的卡在这里排到队。放在启动之前——合并优先，
        //    且它一拍最多开一个、开完即返回，本拍不会再起新会话。
        //    余量为 0 时不动队：合并回合一样要跑一次真实的模型回合。
        if budget > 0 && !settled_merge {
            let mut inner = self.lock();
            if let Some(action) = self.promote_merge_queue(&mut inner, limit, capabilities, threads)
            {
                return Ok(Some(action));
            }
        }
        // 4) 启动：队首、有名额、日历放行 → 交出 `launch`。
        let cards = self.locked_cards();
        match self.next_launch(budget, chrono::Local::now()) {
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

    /// 消费宿主回报的动作结果（spec D12）。
    ///
    /// 一律按 `action_id` 取出**待办条目**：取不到（重复投递、跨进程遗留、不认识
    /// 的 id）就整条忽略。这就是 at-least-once 的幂等实现——宿主重复投递同一
    /// `action_id` 不会二次生效，也不会重复记账名额。
    ///
    /// 摘除条目本身就是判重：第二条同 id 的回报从此查无此条。
    fn consume_results(&self, results: &[DriverResult]) -> bool {
        let mut settled_merge = false;
        for result in results {
            let Some(pending) = self.take_pending(&result.action_id) else {
                continue;
            };
            if pending.kind == PendingKind::ResumeForMerge {
                settled_merge = true;
            }
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
        settled_merge
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
            // 续跑原会话的结果。**所有分支都必须先证明这条结果属于我刚才发的那
            // 一次**:动作 id 之外再核对绑定,迟到的旧结果、串线的结果才伤不到
            // 当前的 attempt（spec §6.10）。
            (PendingKind::ResumeForMerge, ok) => self.settle_resume_merge(pending, result, ok),
        }
    }

    /// 结算一次 `ResumeAppend` 的回报。
    ///
    /// 成功**不**写卡片状态:那张卡的状态是模型自己声明的（`state --self`）,
    /// 插件替它翻成 `done` 就等于篡改事实。成功只意味着"指令进到 prompt 了",
    /// 于是把安全记录从 `Pending` 升到 `Accepted`,并记下**第几拍**受理——清理
    /// 还要等一拍之后的 idle 观测（spec §8.2）。
    fn settle_resume_merge(
        &self,
        pending: &PendingAction,
        result: &DriverResult,
        ok: bool,
    ) -> Result<(), String> {
        let Some(ledger) = pending.resume.as_ref() else {
            return Err("a resume result arrived with no dispatch record".to_string());
        };
        // 安全记录必须仍是**我这一条**。别人的记录说明世界已经变了,
        // 这时唯一正确的动作是什么都不做(既不结算也不清门)。
        let Some(current) = crate::merge_attempt::read(&self.state_dir)? else {
            return Ok(());
        };
        if current.action_id != ledger.action_id
            || current.card_id != pending.card_id.0
            || current.thread_id.as_deref() != Some(ledger.thread_id.as_str())
        {
            eprintln!(
                "superpowers-kanban: ignoring a result for {} that does not match the \
                 merge attempt on record",
                ledger.action_id
            );
            return Ok(());
        }
        if ok {
            // 成功后必须回**原** thread id。回别的 id 等于把绑定写坏,按不安全处理。
            if result.thread_id.as_deref() != Some(ledger.thread_id.as_str()) {
                return self.mark_merge_uncertain(
                    &current,
                    "the resume reported a different thread id than the card's own session",
                );
            }
            let accepted = crate::merge_attempt::MergeAttemptRecord {
                phase: crate::merge_attempt::MergePhase::Accepted,
                ..current
            };
            crate::merge_attempt::save(&self.state_dir, &accepted)?;
            let mut inner = self.lock();
            let tick = inner.driver_tick_seq;
            inner.merge_accepted_on_tick = Some(tick);
            return Ok(());
        }
        match result.error_code.as_deref() {
            // busy 证明指令**没进** prompt 通道:可以安全地把卡退回队列,
            // 保留原入队时刻(与顺位),并清掉本次的门与安全记录。
            Some("busy") => self.requeue_after_busy_merge(pending, ledger, &current),
            // 绑定不可用:再试也不会成功,交给人工。
            Some("thread_not_found") | Some("workdir_missing") | Some("restore_failed") => self
                .hand_merge_to_human(
                    pending,
                    &current,
                    &format!(
                        "the card's own session could not be resumed ({})",
                        result.error_code.as_deref().unwrap_or_default()
                    ),
                ),
            // 明确没投递,但原因不是绑定:同样确定可以重试,交人工是为了一次
            // 只说清一次,而不是让队列反复抖。
            Some("delivery_failed") => self.hand_merge_to_human(
                pending,
                &current,
                "the merge instruction never reached the card's session",
            ),
            // 未知码 / 没有码:不知道有没有投递,只能保守阻塞。
            other => self.mark_merge_uncertain(
                &current,
                &format!(
                    "the resume failed without a recognised error code ({})",
                    other.unwrap_or("none reported")
                ),
            ),
        }
    }

    /// busy:退回 `MergeQueued`,**保留**原入队时刻与顺位。
    ///
    /// 只在卡确实还是本次派发推进去的 `Running` 且绑定没变时退。模型或用户
    /// 这一拍已经写了别的状态,就不覆盖他们的判断。
    fn requeue_after_busy_merge(
        &self,
        pending: &PendingAction,
        ledger: &ResumeLedger,
        current: &crate::merge_attempt::MergeAttemptRecord,
    ) -> Result<(), String> {
        let id = pending.card_id.clone();
        let restored = persist::with_board(&self.state_dir, |board| {
            let Some(live) = board.get(&id).cloned() else {
                return Ok(false);
            };
            if live.state != CardState::Running
                || live.thread_id.as_deref() != Some(ledger.thread_id.as_str())
                || live.workdir.as_ref() != Some(&ledger.expected_workdir)
            {
                // 卡已经不在我留下的那个状态上了:不覆盖。
                return Ok(false);
            }
            board
                .transition(&id, CardState::MergeQueued)
                .map_err(|error| error.to_string())?;
            if let Some(live) = board.get_mut(&id) {
                // 原时刻:重排的卡不该因为等过一次就掉到队尾。
                match ledger.merge_queued_at {
                    Some(at) => live.mark_merge_queued(at),
                    None => live.clear_merge_queued(),
                }
            }
            Ok(true)
        })?;
        // 卡已不归本次处置:记录也必须留着,因为它可能已经投递给别的 attempt。
        if !restored {
            return self.mark_merge_uncertain(
                current,
                "a busy result arrived after the card left the dispatching state",
            );
        }
        self.release_dispatch_resources(&current.card_id, &current.action_id);
        Ok(())
    }

    /// 确定不重试:交给人工，并清掉本次的资源与安全记录。
    fn hand_merge_to_human(
        &self,
        pending: &PendingAction,
        current: &crate::merge_attempt::MergeAttemptRecord,
        reason: &str,
    ) -> Result<(), String> {
        let _ = persist::with_board(&self.state_dir, |board| {
            if board
                .get(&pending.card_id)
                .is_some_and(|live| live.state == CardState::Running)
            {
                board
                    .transition(&pending.card_id, CardState::NeedsYou)
                    .map_err(|error| error.to_string())?;
            }
            board
                .set_detail(&pending.card_id, Some(reason.to_string()))
                .map_err(|error| error.to_string())
        });
        let _ = self.mark_terminal(&pending.card_id.0, "needs_you", Some(reason));
        self.release_dispatch_resources(&current.card_id, &current.action_id);
        Ok(())
    }

    /// 标成不确定:**保留**安全门，只把原因说出来。
    fn mark_merge_uncertain(
        &self,
        current: &crate::merge_attempt::MergeAttemptRecord,
        reason: &str,
    ) -> Result<(), String> {
        let uncertain = crate::merge_attempt::MergeAttemptRecord {
            phase: crate::merge_attempt::MergePhase::Uncertain,
            ..current.clone()
        };
        crate::merge_attempt::save(&self.state_dir, &uncertain)?;
        eprintln!(
            "superpowers-kanban: merge attempt {} is uncertain: {reason}",
            current.action_id
        );
        Ok(())
    }

    /// 收回本次派发占的进程内资源（gate、租约）与磁盘上的安全记录。
    ///
    /// **只清属于本次的东西**:安全记录走 CAS（card + action 都对上才删）,
    /// holder 走 CAS（card + 本进程 pid）。别人的记录一概不碰。
    fn release_dispatch_resources(&self, card_id: &str, action_id: &str) {
        let cleared = crate::merge_attempt::clear_if_matches(&self.state_dir, card_id, action_id);
        if let Err(error) = cleared {
            // 清不掉:安全门仍在,这是**正确**的保守结局,但必须让人看见。
            eprintln!("superpowers-kanban: {error}");
        }
        let mut inner = self.lock();
        inner.merge_gate = None;
        inner.merge_accepted_on_tick = None;
        inner.leases.remove(&CardId::new(card_id));
        crate::merge_lock::clear_holder_if_matches(&self.state_dir, card_id, std::process::id());
    }

    /// 看板快照（克隆）：一次决策取一份，避免边决策边读到漂移的状态。
    ///
    /// 直接读盘（`persist::read_board`）而不是读内存：board.json 是唯一真相
    /// （单源点），别的进程（CLI、另一个 daemon）可能刚刚改过它。
    /// 队列推进：把队首那张可用的合并卡交出去。
    ///
    /// 这里有两类卡,处置**必须不同**（spec §6.8）:
    ///
    /// - **实现卡**:回到它自己的开发会话(`ResumeAppend`)。合并是那张卡的最后一步,
    ///   上下文全在原会话里;另起一个会话去合并,等于让一个没有上下文的陌生人替
    ///   它收尾——这正是本需求要修的 bug。
    /// - **独立合并卡**(`kind == Merge`,没有实现计划):沿用 `Launch`。它的会话
    ///   只做合并,本来就没有"原会话"可回。
    ///
    /// **顺序**是"先会话名额、后 merge.lock"（spec §6.4）。反过来会占着合并锁
    /// 等额度,期间本项目所有合并都被堵住,而它们本可以排在后面等着。
    fn promote_merge_queue(
        &self,
        inner: &mut Inner,
        limit: u16,
        capabilities: &DriverCapabilities,
        threads: &[ThreadFacts],
    ) -> Option<DriverAction> {
        // 严格读:一份读不懂的看板不能让本拍凭"看起来是空的"就什么都不做,
        // 更不能让后续写入覆盖它。直接报出来。
        let board = match persist::load_board_strict(&self.state_dir.join("board.json")) {
            Ok(board) => board,
            Err(error) => {
                eprintln!("superpowers-kanban: {error}");
                return None;
            }
        };
        // 已经有人在合并:holder 指向任何卡,本拍就不动。锁本身是判定依据,
        // 不另设 `Merging` 状态（spec §8）。
        match crate::merge_lock::read_holder_strict(&self.state_dir) {
            Ok(Some(_)) => return None,
            Ok(None) => {}
            Err(error) => {
                // 记录写坏了:门的归属不可知,不能当成"没人合并"。
                eprintln!("superpowers-kanban: {error}");
                return None;
            }
        }
        // 在飞的安全记录(含上一条还没结算的):本拍不派任何合并动作。
        // 它独立于卡片,所以卡被归档/清掉了也照样拦（spec §8.1）。
        match crate::merge_attempt::read(&self.state_dir) {
            Ok(None) => {}
            Ok(Some(_)) => return None,
            Err(error) => {
                eprintln!("superpowers-kanban: {error}");
                return None;
            }
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
        // FIFO 只看入队时刻;同一时刻（旧数据没有时间戳）按 order 再按 id 兜底,
        // 保证每拍挑中的是同一张。
        candidates.sort_by(|a, b| {
            (a.merge_queued_at, a.order, &a.id).cmp(&(b.merge_queued_at, b.order, &b.id))
        });
        let leases_dir = inner.leases_dir.clone()?;
        for card in candidates {
            // 实现卡要先确认能回到原会话,否则连名额都不该拿:一张缺绑定的卡
            // 交给人工,而不是占着额度反复重试。
            let resume = match self.merge_binding(&card, capabilities, threads) {
                MergeBinding::Resume(ledger) => Some(ledger),
                MergeBinding::Standalone => None,
                MergeBinding::Defer { reason } => {
                    let _ = self.defer_merge(&card.id, &reason);
                    continue;
                }
                MergeBinding::Skip => {
                    // 这一拍明显不可用（会话正忙）:不占锁、不改时间,继续看后面的。
                    continue;
                }
                MergeBinding::NeedsBinding { reason } => {
                    let _ = self.mark_terminal(&card.id.0, "needs_you", Some(&reason));
                    // 退队时刻一并清掉,免得它下次又被当成"排了很久"的候选。
                    let _ = persist::with_board(&self.state_dir, |board| {
                        if let Some(live) = board.get_mut(&card.id) {
                            live.clear_merge_queued();
                        }
                        Ok(())
                    });
                    continue;
                }
            };
            // a) 会话名额。拿不到就整拍不推进——名额是全机共享的,换一张卡也
            //    一样拿不到,继续试只是白烧 CPU。
            //
            //    已经在跑的卡（绑定变了、或模型早写了一轮）不该再领第二份:
            //    `reconcile_leases_locked` 已经按活会话给它记过一份。
            let already_leased = inner.leases.contains_key(&card.id);
            // 名额拿不到就整拍不推进（换一张卡也一样拿不到）。用 `then` +
            // `?` 表达，而不是 `let...else`：后者在 `Option` 返回里会被
            // Clippy 要求改成 `?`，但 `?` 只能直接返回 `None`，包不进 `Some`。
            let lease = if already_leased {
                None
            } else {
                let lease = (!already_leased)
                    .then(|| lease::acquire_in(&leases_dir, limit as usize))
                    .flatten()?;
                Some(lease)
            };
            // b) 合并锁。拿不到说明别的进程刚抢到:把本次新领的名额放回,换下一张。
            let gate = match crate::merge_lock::acquire_with_holder(&self.state_dir, &card.id.0) {
                Ok(crate::merge_lock::MergeAttempt::Granted(lock))
                | Ok(crate::merge_lock::MergeAttempt::Reclaimed(lock)) => lock,
                Ok(crate::merge_lock::MergeAttempt::Busy { .. }) => {
                    if let Some(lease) = lease {
                        drop(lease);
                    }
                    continue;
                }
                Err(error) => {
                    // 记录读不懂 / 写不下:门的归属不可知,这一拍不能派合并。
                    // 只还本次名额,不动别人的记录。
                    eprintln!("superpowers-kanban: {error}");
                    drop(lease);
                    return None;
                }
            };
            // c) 目标树与 objective。
            let action_id = self.next_action_id(inner);
            match resume {
                Some(mut ledger) => {
                    ledger.action_id = action_id.clone();
                    if let Some(action) = self.dispatch_resume(inner, &card, &ledger, gate, lease) {
                        return Some(action);
                    }
                    // 落盘失败:安全门与名额都留着,本拍不再派别的。下一拍重来。
                    return None;
                }
                None => {
                    if let Some(action) =
                        self.dispatch_standalone(inner, &card, &action_id, gate, lease)
                    {
                        return Some(action);
                    }
                    return None;
                }
            }
        }
        None
    }

    /// 这张合并卡该走哪条路。
    ///
    /// 返回值刻意把"跳过"与"交给人工"分开:前者是这一拍的临时状态（会话正忙）,
    /// 后者是已确定不可自动处理的绑定问题。把两者混在一起会让一张忙卡被错误地
    /// 标成 `needs_you`。
    fn merge_binding(
        &self,
        card: &Card,
        capabilities: &DriverCapabilities,
        threads: &[ThreadFacts],
    ) -> MergeBinding {
        if card.kind == CardKind::Merge {
            return MergeBinding::Standalone;
        }
        // 能力先于预算:旧宿主不声明 `resume_append` 时,这一拍就算有余量也不能
        // 交出它执行不了的动作,更不能退化成 `Launch`（那会切断上下文）。
        if !capabilities.supports("resume_append") {
            return MergeBinding::Defer {
                reason: "the host does not declare the `resume_append` action; \
                         upgrade the host (and restart the old daemon) so an authorized \
                         merge can continue in this card's own session"
                    .to_string(),
            };
        }
        let thread_id = card
            .thread_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let workdir = card.workdir.clone();
        let (thread_id, workdir) = match (thread_id, workdir) {
            (Some(thread_id), Some(workdir)) => (thread_id.to_string(), workdir),
            _ => {
                // 不猜、不去别的目录找一个"看着像"的会话。缺绑定就只能人工处理。
                return MergeBinding::NeedsBinding {
                    reason: "this card has no bound development session (thread id or \
                             workdir is missing), so its authorized merge cannot continue \
                             in the original conversation; resume it by hand"
                        .to_string(),
                };
            }
        };
        // 观测说它活着且不空闲:很可能还在跑,这一拍不打断它。缺观测是允许的——
        // 最终由宿主的 busy 判断兜底（spec §6.2）。
        if let Some(fact) = thread_fact(threads, &thread_id)
            && fact.alive
            && !fact.idle
        {
            return MergeBinding::Skip;
        }
        MergeBinding::Resume(ResumeLedger {
            action_id: String::new(),
            thread_id,
            expected_workdir: workdir,
            merge_queued_at: card.merge_queued_at,
        })
    }

    /// 派发前的最后一次重验与落盘。**全部在 board 锁内**:先写安全记录、
    /// 再写 `Running`,两步都成功才把动作交出去。
    fn dispatch_resume(
        &self,
        inner: &mut Inner,
        card: &Card,
        ledger: &ResumeLedger,
        gate: crate::merge_lock::MergeLock,
        lease: Option<Lease>,
    ) -> Option<DriverAction> {
        // 目标树要在写记录**之前**准备好:准备失败就不该留下任何安全记录。
        // 合并在这棵树里做,而开发树只用来定位原会话。
        let base = card
            .base_ref
            .clone()
            .unwrap_or_else(|| crate::merge::default_branch(&self.project_root));
        let target = match merge::prepare(&self.project_root, &base) {
            Ok(tree) => tree.path,
            Err(error) => {
                eprintln!(
                    "superpowers-kanban: cannot prepare a tree for {}: {error}",
                    card.id.0
                );
                let _ = crate::merge_lock::clear_holder_if_matches(
                    &self.state_dir,
                    &card.id.0,
                    std::process::id(),
                );
                drop(gate);
                drop(lease);
                return None;
            }
        };
        let record = crate::merge_attempt::MergeAttemptRecord {
            version: crate::merge_attempt::ATTEMPT_VERSION,
            action_id: ledger.action_id.clone(),
            card_id: card.id.0.clone(),
            thread_id: Some(ledger.thread_id.clone()),
            expected_workdir: ledger.expected_workdir.clone(),
            phase: crate::merge_attempt::MergePhase::Pending,
            merge_queued_at: ledger.merge_queued_at,
        };
        let staged = persist::with_board(&self.state_dir, |board| {
            // CAS:重验这一张仍是 MergeQueued、绑定与原入队时间都没被换过。
            let live = board
                .get(&card.id)
                .cloned()
                .ok_or_else(|| "the card disappeared while dispatching".to_string())?;
            if live.state != CardState::MergeQueued {
                return Err(format!("the card is {:?}, not merge_queued", live.state));
            }
            if live.thread_id.as_deref() != Some(ledger.thread_id.as_str()) {
                return Err("the card's session binding changed while dispatching".to_string());
            }
            if live.workdir.as_ref() != Some(&ledger.expected_workdir) {
                return Err("the card's workdir changed while dispatching".to_string());
            }
            if live.merge_queued_at != ledger.merge_queued_at {
                return Err("the card's queue position changed while dispatching".to_string());
            }
            // 先只在内存里迁移;`with_board` 只在闭包返回 `Ok` 时才把看板落盘。
            // 于是「安全记录落盘 → 看板落盘(Running)」是同一次提交,而**记录在
            // 前**:中途崩溃留下的是「有记录 + 卡仍 MergeQueued」,重启会阻塞而不是
            // 重派（spec §8.1）。反过来（先写 Running）会留下一个没有记录、却已
            // 经在跑的合并。
            board
                .transition(&card.id, CardState::Running)
                .map_err(|error| error.to_string())?;
            crate::merge_attempt::save(&self.state_dir, &record)
                .map_err(|error| format!("could not record the merge attempt: {error}"))
        });
        if let Err(error) = staged {
            // 看板没提交 = `Running` 没落盘;动作也没交出去,名额与 holder 都不留。
            //
            // 记录可能已经写进去了（失败发生在写记录之后）。这里**能确认**动作没
            // 交出去,所以按 spec §8.1 的允许做一次 CAS 清理;留一条没人能自动结算的
            // 记录会把这个项目的合并永久堵死。
            let _ = crate::merge_attempt::clear_if_matches(
                &self.state_dir,
                &card.id.0,
                &ledger.action_id,
            );
            eprintln!(
                "superpowers-kanban: could not start the merge for {}: {error}",
                card.id
            );
            let _ = crate::merge_lock::clear_holder_if_matches(
                &self.state_dir,
                &card.id.0,
                std::process::id(),
            );
            drop(gate);
            drop(lease);
            return None;
        }
        // 记录与 `Running` 都已落盘,现在才把动作交出去。
        if let Err(error) = persist::with_board(&self.state_dir, |board| {
            if let Some(live) = board.get_mut(&card.id) {
                live.clear_merge_queued();
            }
            Ok(())
        }) {
            eprintln!(
                "superpowers-kanban: could not clear the queue mark for {}: {error}",
                card.id
            );
        }
        inner.merge_gate = Some(gate);
        if let Some(lease) = lease {
            inner.leases.insert(card.id.clone(), lease);
        }
        inner.pending_actions.insert(
            ledger.action_id.clone(),
            PendingAction {
                card_id: card.id.clone(),
                kind: PendingKind::ResumeForMerge,
                resume: Some(ledger.clone()),
            },
        );
        let source = merge_source(card);
        let base = card
            .base_ref
            .clone()
            .unwrap_or_else(|| crate::merge::default_branch(&self.project_root));
        Some(DriverAction::ResumeAppend {
            action_id: ledger.action_id.clone(),
            card_id: card.id.0.clone(),
            thread_id: ledger.thread_id.clone(),
            workdir: ledger.expected_workdir.clone(),
            objective: crate::tick::queued_merge_objective(
                &card.id.0,
                &source,
                &base,
                &ledger.expected_workdir,
                &target,
                &self.state_dir,
            ),
            env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), card.id.0.clone())],
        })
    }

    /// 独立合并卡:沿用既有 `Launch` 路径（spec §6.8）。
    fn dispatch_standalone(
        &self,
        inner: &mut Inner,
        card: &Card,
        action_id: &str,
        gate: crate::merge_lock::MergeLock,
        lease: Option<Lease>,
    ) -> Option<DriverAction> {
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
                let _ = crate::merge_lock::clear_holder_if_matches(
                    &self.state_dir,
                    &card.id.0,
                    std::process::id(),
                );
                drop(gate);
                drop(lease);
                return None;
            }
        };
        let record = crate::merge_attempt::MergeAttemptRecord {
            version: crate::merge_attempt::ATTEMPT_VERSION,
            action_id: action_id.to_string(),
            card_id: card.id.0.clone(),
            thread_id: card.thread_id.clone(),
            expected_workdir: workdir.clone(),
            phase: crate::merge_attempt::MergePhase::Pending,
            merge_queued_at: card.merge_queued_at,
        };
        // 与 `dispatch_resume` 同一顺序:先在内存里迁移,再写安全记录,最后才是
        // 看板提交。记录在前意味着崩溃留下「有记录、卡未 Running」——阻塞而非重派。
        let staged = persist::with_board(&self.state_dir, |board| {
            board
                .transition(&card.id, CardState::Running)
                .map_err(|error| error.to_string())?;
            if let Some(live) = board.get_mut(&card.id) {
                live.clear_merge_queued();
            }
            board
                .set_workdir(&card.id, workdir.clone())
                .map_err(|error| error.to_string())?;
            crate::merge_attempt::save(&self.state_dir, &record)
                .map_err(|error| format!("could not record the merge attempt: {error}"))
        });
        if let Err(error) = staged {
            // 同 `dispatch_resume`:动作没交出去,记录若已写就 CAS 清掉。
            let _ = crate::merge_attempt::clear_if_matches(&self.state_dir, &card.id.0, action_id);
            eprintln!(
                "superpowers-kanban: could not start the merge for {}: {error}",
                card.id
            );
            let _ = crate::merge_lock::clear_holder_if_matches(
                &self.state_dir,
                &card.id.0,
                std::process::id(),
            );
            drop(gate);
            drop(lease);
            return None;
        }
        inner.merge_gate = Some(gate);
        if let Some(lease) = lease {
            inner.leases.insert(card.id.clone(), lease);
        }
        inner.pending_actions.insert(
            action_id.to_string(),
            PendingAction {
                card_id: card.id.clone(),
                kind: PendingKind::LaunchForMerge,
                resume: None,
            },
        );
        Some(DriverAction::Launch {
            action_id: action_id.to_string(),
            driver_project: self.project_root.to_string_lossy().into_owned(),
            driver_ref: card.id.0.clone(),
            card_id: card.id.0.clone(),
            objective: crate::tick::queued_merge_objective(
                &card.id.0,
                card.source_ref.as_deref().unwrap_or_default(),
                &base,
                &card.workdir.clone().unwrap_or_default(),
                &workdir,
                &self.state_dir,
            ),
            workdir,
            title: String::new(),
            env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), card.id.0.clone())],
        })
    }

    /// 记下"这张卡这一拍先不动"的原因,但不反复刷盘/刷日志（spec §5.2）。
    fn defer_merge(&self, card_id: &CardId, reason: &str) -> Result<(), String> {
        persist::with_board(&self.state_dir, |board| {
            let current = board
                .get(card_id)
                .and_then(|card| card.detail.clone())
                .unwrap_or_default();
            if current == reason {
                return Ok(());
            }
            board
                .set_detail(card_id, Some(reason.to_string()))
                .map_err(|error| error.to_string())
        })
    }

    /// 取下一个动作 id。就地递增,避免在已持有 `inner` 的地方再锁一次。
    fn next_action_id(&self, inner: &mut Inner) -> String {
        inner.next_action_seq += 1;
        format!("act-{}-{}", inner.action_nonce, inner.next_action_seq)
    }

    /// 合并视图配置：并发字段来自注入的 home（`None` 时回退真实 `$HOME`）。
    ///
    /// 峰值**只认全局层**（限的是全机总流量，每项目各配一个峰值必然超发），
    /// 项目层只贡献节拍、归档宽限这些与配额无关的字段。
    fn calendar(&self) -> superpowers_kanban_core::calendar::ConcurrencyCalendar {
        use superpowers_kanban_core::calendar::ConcurrencyCalendar;
        match self.home.as_deref() {
            Some(home) => ConcurrencyCalendar::load_merged_in(&self.state_dir, home),
            None => ConcurrencyCalendar::load_merged(&self.state_dir),
        }
    }

    /// 本看板此刻处于 `Running` 的卡片数。
    ///
    /// 只给「没有宿主快照」的手工通路用（`board.next_launch`）。自动化调度用
    /// 宿主下发的**全机**运行数——本看板的数只是它的子集，拿它再减一次就是
    /// 双重扣减。
    ///
    /// 口径就是「卡在这一刻是不是 `Running`」，不看槽位绑定：`MergeQueued`
    /// 尚未开跑、`AwaitingMerge`/`NeedsYou`/终态都已停，都不算。
    pub fn board_running_count(&self) -> usize {
        self.locked_cards()
            .iter()
            .filter(|card| card.state == CardState::Running)
            .count()
    }

    /// 本拍还能起几个会话：全机峰值 − **整机**已用。
    ///
    /// # 为什么不能直接用 `sessions_running`
    ///
    /// 宿主下发的 `sessions.running` 是**它自己那个进程**看到的全机数字。两个
    /// 实例各自拿它去比同一个总上限，两边都会看到"还有余量"，合计就超了
    /// （G3 §6.3）。这台机器上同时可能跑着桌面壳、CLI、以及另一个实例的插件。
    ///
    /// 所以已用量取两处证据的**较大者**，不相加：
    ///
    /// 1. 整机活动表：每个活实例在 `<quota-domain>/activity/` 下持有 flock 并
    ///    写着自己的会话数。存活由锁判定（进程退出释放、`SIGSTOP` 不释放），
    ///    不按时间猜。
    /// 2. 宿主上报值：旧宿主、或活动目录写不进去时，这是唯一来源。
    ///
    /// 不是相加的理由：本进程的会话既在自己的活动行里、又在宿主上报值里，相加
    /// 会把它算两遍，于是"还有一个名额"变成"一个都没有"。
    ///
    /// 活动表读不出来（配额域不可解析、目录损坏）时**退回宿主上报值**，而不是
    /// 当成 0：当成 0 会让每个读失败的实例都以为整机空闲，那正是超发的形态。
    fn launch_budget(&self, sessions_running: Option<u32>) -> usize {
        let limit = self.calendar().limit_at(chrono::Local::now()) as usize;
        let Some(used) = self.machine_usage(sessions_running) else {
            // 配额都不存在就不该自动起任务（G3 §6.2）。没有配置时 `limit`
            // 来自内置默认值，此时保持原有「按日历限额」行为，但活动表读不到
            // 就别再假装知道整机状态。
            return match sessions_running {
                Some(running) => limit.saturating_sub(running as usize),
                None => limit,
            };
        };
        limit.saturating_sub(used as usize)
    }

    /// 整机已用会话数：活动表与宿主上报取**较大者**，不相加。`None` 表示
    /// 两处都没有可信证据。
    fn machine_usage(&self, sessions_running: Option<u32>) -> Option<u32> {
        let observed = self
            .quota_domain()
            .and_then(|domain| crate::quota::observe_machine(&domain).ok());
        match (observed, sessions_running) {
            (Some(observed), reported) => {
                let total = observed.total();
                // 本进程的会话已在活动行里；上报值可能更新（例如刚起了一个
                // 还没来的及发布的会话），取较大者既不会双扣也不会漏。
                Some(total.max(reported.unwrap_or(0)))
            }
            (None, Some(reported)) => Some(reported),
            (None, None) => None,
        }
    }

    /// 本实例的配额域。`None` 表示无法定位共享域——调用方必须**阻止**新的
    /// 自动任务，而不是自造一个池（G3 §6.2）。
    pub fn quota_domain(&self) -> Option<crate::quota::QuotaDomain> {
        let home = match self.home.clone() {
            Some(home) => home,
            None => PathBuf::from(std::env::var_os("HOME").filter(|home| !home.is_empty())?),
        };
        crate::quota::QuotaDomain::resolve(&home)
    }

    /// 把本实例此刻的会话数发布到整机活动表。
    ///
    /// 每拍调用一次。失败只打日志：活动登记是**观测**，它坏了应当让
    /// `launch_budget` 退回宿主上报值，而不是让整拍失败——整拍失败会让宿主
    /// 把已消费的回报当成没送到而重发。
    fn publish_activity(&self, sessions: u32) {
        let Some(domain) = self.quota_domain() else {
            eprintln!(
                "superpowers-kanban: no shared quota domain (HOME unset); \
                 cross-instance activity cannot be published"
            );
            return;
        };
        let owner = format!("{}-{}", self.instance_owner_prefix(), std::process::id());
        let mut inner = self.lock();
        let activity = inner
            .activity
            .get_or_insert_with(|| crate::quota::LocalActivity::new(domain, owner));
        if let Err(error) = activity.publish(sessions) {
            eprintln!("superpowers-kanban: could not publish activity: {error}");
        }
    }

    /// 本实例在整机活动表里的唯一名字。
    ///
    /// 只取项目目录末段 + pid **不够**：同一个宿主进程可以托管多个项目的看板
    /// （`dispatch_with_service` 里的两个 `BoardService` 就是），它们的 pid 相同、
    /// 项目名可能相同，于是两行活动会落到同一个文件——一个覆盖另一个，整机
    /// 用量少算一半，两个实例于是各自再拿一份额度。
    ///
    /// 所以名字里带**状态目录的全路径摘要**：路径本身已经是每个看板唯一的。
    /// 尾部仍留 pid，让人一眼看出是哪个进程写的。
    fn instance_owner_prefix(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.state_dir.to_string_lossy().as_bytes());
        let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
        format!("kanban-{hex}")
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
                resume: None,
            },
        );
    }

    /// 测试用：登记一条带派发身份的待办（生产侧只有 `dispatch_resume` 会建这种）。
    #[cfg(test)]
    fn remember_pending_with_resume(
        &self,
        action_id: &str,
        card_id: &CardId,
        kind: PendingKind,
        thread_id: &str,
    ) {
        self.lock().pending_actions.insert(
            action_id.to_string(),
            PendingAction {
                card_id: card_id.clone(),
                kind,
                resume: Some(ResumeLedger {
                    action_id: action_id.to_string(),
                    thread_id: thread_id.to_string(),
                    expected_workdir: self.project_root.clone(),
                    merge_queued_at: None,
                }),
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
        budget: usize,
        now: DateTime<Local>,
    ) -> Result<Option<LaunchClaim>, String> {
        // 全机余量为 0：这一拍不起会话。合并队列与启动共用这份余量，所以闸门
        // 必须在拿租约**之前**——否则会先占一个槽才发现起不了。
        if budget == 0 {
            return Ok(None);
        }
        // 自动任务闸门：排空中或上游依赖缺失时不认领新卡。
        //
        // 放在拿租约**之前**是硬要求：先占槽再发现起不了，槽会白占一拍，
        // 而排空期间那一拍正是屏障在等的。返回 `Ok(None)` 而不是 `Err`：
        // "本拍没有动作"是正常结论，报错会诱使宿主把已经消费的回报判成没送到。
        if let Some(reason) = self.launch_blocked_reason() {
            eprintln!("superpowers-kanban: not launching: {reason}");
            return Ok(None);
        }
        let mut inner = self.lock();
        let Some(dir) = inner.leases_dir.clone() else {
            return Err("no lease directory".into());
        };
        // 租约池按**峰值**开，不用 `budget`：`acquire_in` 按 `0..limit` 编号复用
        // 槽位，拿会变小的 `budget` 去开池会让两批会话撞到同一个槽。
        let limit = self.calendar().limit_at(now) as usize;
        // 先补账：内存里的租约可能已经对应到死掉的会话（或反过来说，磁盘上还有
        // 活会话但本进程是刚重启的，手里一个租约都没有）。这一步把「谁该占槽」
        // 与「实际占槽」对齐，否则重启后名额会被别的项目抢走。
        self.reconcile_leases_locked(&mut inner, limit as u16);
        // 再占槽：拿不到就说明名额已满（含本进程自己已持有的）。
        let Some(lease) = lease::acquire_in(&dir, limit) else {
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
        // 合并安全记录保护的卡集合：这三个阶段都说明「一次合并可能还在写目标
        // 树」。它们的会话这一拍哪怕没被完整快照命中，也**不能**凭一次未加载
        // 就把 lease 提前放走——放走的一瞬间别的项目就会在同一个名额上再起一个
        // 会话。记录坏读时同样不启动新动作：宁可本拍不动，也不猜。
        let protected: std::collections::HashSet<CardId> =
            match crate::merge_attempt::read(&self.state_dir) {
                Ok(Some(record)) => match record.phase {
                    crate::merge_attempt::MergePhase::Pending
                    | crate::merge_attempt::MergePhase::Accepted
                    | crate::merge_attempt::MergePhase::Uncertain => {
                        std::collections::HashSet::from([CardId::new(record.card_id)])
                    } // 未知阶段是坏记录，上面 `read` 已经会报错；这里保守不保护。
                },
                // 坏 journal：不新增动作（下面的 retain/acquire 全部跳过）。
                Err(error) => {
                    eprintln!("superpowers-kanban: {error}; leaving leases untouched");
                    return;
                }
                Ok(None) => std::collections::HashSet::new(),
            };
        let live: std::collections::HashSet<CardId> = cards
            .iter()
            .filter(
                |card| match card.thread_id.as_deref().filter(|id| !id.is_empty()) {
                    Some(thread) => match observations.threads.get(thread) {
                        Some(observation) => observation.alive,
                        // **没观测到**：对已交付的卡按「不活」算，其余按「活」算。
                        //
                        // 保守地把「缺证据」当「还活着」是为了不超发并发位——
                        // 一张刚认领、宿主还没上报的卡，若凭缺失就放名额，
                        // 同一拍里会凭空多出一个位子。但这个保守只对**可能还在
                        // 起跑线上**的卡成立。
                        //
                        // `done` / `cancelled` 的卡没有在飞的动作，它身上的
                        // thread_id 只是历史记录。线上出过事故：三张 `done` 卡带着
                        // 旧 thread_id，`observations.json` 恰是空的（daemon 刚重启、
                        // 宿主还没上报），于是它们被永久判为「活」，全局名额被占满，
                        // 后面 `queued` 的卡一张都起不来，日志只说
                        // 「no slot could be re-adopted」——看起来像没有名额，
                        // 实际是名额被早就跑完的卡占着。
                        //
                        // 这里**不**用 `is_terminal()`：`failed` 虽也是终态，却常是
                        // 「会话还活着但报了失败」（见 `release` 的契约与用例
                        // `a_released_card_keeps_its_slot_while_its_session_is_alive`），
                        // 那种情况名额必须继续算它头上。
                        None => !matches!(card.state, CardState::Done | CardState::Cancelled),
                    },
                    None => card.state == CardState::Running && awaiting_thread.contains(&card.id),
                },
            )
            .map(|card| card.id.clone())
            .collect();
        // 放：不再对应活会话的租约一律 drop（drop 即归还名额）。受保护的合并卡
        // 例外——见 `protected` 的注释。
        inner
            .leases
            .retain(|card_id, _| live.contains(card_id) || protected.contains(card_id));
        // 领：活的卡还没有租约就补一个（领不到只做尽力而为，不 panic）。
        // 领：只给「活着的」卡补租约。受保护但这一拍看起来不活的卡不在 `live`
        // 里，所以也不会被领第二份——同卡永远只持一份 lease。
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
        if self.managed {
            // 受管：登记属于宿主（设计 §7.1）。这里只建自己的状态目录。悄悄写
            // 共享文件会让"谁在管理常驻"变成两个写者，宿主下一次整表重写就把
            // 插件刚写的条目抹掉，而插件还以为登记成功了。
            return Ok(());
        }
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
        if !self.managed {
            // 受管时不碰宿主登记（同 `lifecycle_create`）；只删自己的状态目录。
            resident::release(&resident_dir, project, Self::REQUESTER)
                .map_err(|error| format!("could not release the project: {error}"))?;
        }
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
        // 安全记录还在:这一轮有没有结束不是调用方说了算。daemon 会在
        // 「匹配结果之后的一拍、匹配会话明确 idle」时自己放门。
        let attempt = crate::merge_attempt::read(&self.state_dir)?;
        if let Some(record) = &attempt {
            if record.card_id != card_id {
                // 不是这张卡的账:零副作用（不碰 holder、不碰 gate）。
                return Ok(MergeFinish::Waiting);
            }
            return Ok(MergeFinish::Waiting);
        }
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
    fn release_gate_when_the_merge_is_over(&self, threads: &[ThreadFacts], tick_seq: u64) {
        // 先看盘上的安全记录:它才是"这一轮到底结束没有"的真相。旧版只凭
        // "卡不再是 Running"就放门,模型早写一个 `done` 就能把还在写目标树的
        // 合并回合放走（spec §8.2）。
        let record = match crate::merge_attempt::read(&self.state_dir) {
            Ok(Some(record)) => record,
            Ok(None) => {
                // 没有安全记录:那是旧版抛下的 holder。只做"死了就清"的回收,
                // 不凭它推断任何合并已经结束。
                return;
            }
            Err(error) => {
                eprintln!("superpowers-kanban: {error}");
                return;
            }
        };
        let card_id = CardId::new(record.card_id.clone());
        let board = match persist::load_board_strict(&self.state_dir.join("board.json")) {
            Ok(board) => board,
            Err(error) => {
                eprintln!("superpowers-kanban: {error}");
                return;
            }
        };
        // 卡不存在:无从证明 git 子进程已经停。保留记录,等人工核对。
        let Some(card) = board.get(&card_id) else {
            return;
        };
        // "卡确实收尾了"= 存在、绑定的 thread/workdir 都没被换过、且已离开
        // Running。绑定变了说明这张卡已经在过另一种生活,本次记录不再是它的。
        let binding_holds = card.thread_id.as_deref() == record.thread_id.as_deref()
            && card.workdir.as_ref() == Some(&record.expected_workdir);
        let finished = binding_holds && card.state != CardState::Running;
        // 只认**这个** thread 的观测:别的会话 idle 不能证明它在写的那棵树停了。
        let fact = record
            .thread_id
            .as_deref()
            .and_then(|thread_id| thread_fact(threads, thread_id));
        let pending = self
            .lock()
            .pending_actions
            .values()
            .any(|pending| pending.kind == PendingKind::ResumeForMerge);
        let accepted_on_tick = self.lock().merge_accepted_on_tick;
        if can_release_merge(
            &record.phase,
            pending,
            accepted_on_tick,
            tick_seq,
            finished,
            fact,
        ) {
            // 先清记录:清不掉就不放门,继续阻塞。
            match crate::merge_attempt::clear_if_matches(
                &self.state_dir,
                &record.card_id,
                &record.action_id,
            ) {
                Ok(true) => {
                    let mut inner = self.lock();
                    inner.merge_gate = None;
                    inner.merge_accepted_on_tick = None;
                    inner
                        .leases
                        .retain(|id, _| id != &CardId::new(record.card_id.clone()));
                    drop(inner);
                    crate::merge_lock::clear_holder_if_matches(
                        &self.state_dir,
                        &record.card_id,
                        std::process::id(),
                    );
                    if let Ok(mut board) = persist::with_board(&self.state_dir, |board| {
                        board
                            .set_detail(&CardId::new(record.card_id.clone()), None)
                            .map_err(|error| error.to_string())
                    }) {
                        let _ = &mut board;
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("superpowers-kanban: {error}"),
            }
            return;
        }
        // 该收尾了却没有足够证据:转 Uncertain,让人来核对,而不是继续等。
        if record.phase == crate::merge_attempt::MergePhase::Accepted
            && !pending
            && finished
            && fact.is_some_and(|fact| !fact.alive)
        {
            let uncertain = crate::merge_attempt::MergeAttemptRecord {
                phase: crate::merge_attempt::MergePhase::Uncertain,
                ..record
            };
            if let Err(error) = crate::merge_attempt::save(&self.state_dir, &uncertain) {
                eprintln!("superpowers-kanban: {error}");
            }
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
        // 安全记录是**顶层**字段:它必须看得见,哪怕卡已经被归档/清掉。
        // 卡片的 detail 承载不了这件事,因为那时已经没有那张卡了。
        let (merge_attempt, merge_blocked_reason) =
            match crate::merge_attempt::read(&self.state_dir) {
                Ok(Some(record)) => (
                    json!({
                        "action_id": record.action_id,
                        "card_id": record.card_id,
                        "thread_id": record.thread_id,
                        "phase": record.phase,
                        "expected_workdir": record.expected_workdir,
                    }),
                    None,
                ),
                Ok(None) => (Value::Null, None),
                Err(error) => (Value::Null, Some(error)),
            };
        let drain = self.drain();
        let readiness = self.readiness();
        json!({
            "merge_attempt": merge_attempt,
            "merge_blocked_reason": merge_blocked_reason,
            // 为什么现在不起新卡。`null` 表示没有阻塞。页面拿它显示一条横幅，
            // 而不是让用户面对一个"什么都没发生"的队列。
            "launch_blocked": self.launch_blocked_reason(),
            "drain": match &drain {
                crate::drain::Drain::Idle => json!({"state": "idle"}),
                crate::drain::Drain::OtherPlugin => json!({"state": "other_plugin"}),
                crate::drain::Drain::Draining { phase } => json!({"state": "draining", "phase": phase}),
                crate::drain::Drain::Unknown { detail } => json!({"state": "unknown", "detail": detail}),
            },
            "dependencies": readiness.dependencies,
            "conflicts": readiness.conflicts,
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
                // 加载态投影：**唯一**的解读入口在 `observation`，这里不许
                // 自己再拿 `alive` 猜一遍。非 running / 无绑定 ID 就是 unknown
                // ——「没会话」和「没观测到」都不该冒充「已经没了」。
                "session_observation": card
                    .thread_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .map(|_| crate::observation::for_card(card, &observations).as_str()),
                // 这条观测是什么时候采的。unknown 不报时间：旧格式文件里的
                // `last_alive_at` 不是「这一拍看见了」的证据。
                "observed_at": card
                    .thread_id
                    .as_deref()
                    .and_then(|thread| observations.threads.get(thread))
                    .filter(|observation| {
                        observation.presentation()
                            != crate::observation::SessionObservation::Unknown
                    })
                    .and_then(crate::observation::Observation::presentation_at)
                    .map(|at| at.to_rfc3339()),
                "last_alive_at": card
                    .thread_id
                    .as_deref()
                    .and_then(|thread| observations.threads.get(thread))
                    .and_then(|observation| observation.last_seen_alive_at)
                    .map(|at| at.to_rfc3339()),
            })).collect::<Vec<_>>(),
        })
    }

    /// daemon 启动时的合并安全检查（spec §8.3）。
    ///
    /// **只能由 daemon 在取得 `plugin.lock` 之后、开始回答查询之前调用。**
    /// 不能放进 `BoardService::new`：CLI 也会构造 service，那样一次 `list` 就会
    /// 把一个活跃 attempt 标成 uncertain、误伤正在合并的项目。
    pub fn recover_merge_attempt_on_startup(&self) -> Result<Option<String>, String> {
        crate::merge_attempt::block_on_startup(&self.state_dir)
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
        // home 指到临时目录，让 lease 落在隔离目录，不污染真实 HOME。实例根也
        // 一起钉住：`new` 会从环境解析它，而测试绝不能读到开发机上的真实实例
        // （那会让"排空中"和"依赖缺失"两条断言随机器状态而变）。
        let service = BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
            .with_instance_home(Some(dir.join("home/.yi-agent")));
        // 自动任务闸门会看上游依赖，而这些测试要测的是租约/队列本身。先把声明的
        // 技能装齐，免得每条测试都变成"依赖缺失"的重复验证；专门验证缺依赖的
        // 测试用 `service_with_missing_dependencies`。
        make_ready(&dir.join("home/.yi-agent"));
        service
    }

    /// 与 [`service_with_card`] 相同，但实例根里一个上游技能都没有。
    fn service_with_missing_dependencies(dir: &std::path::Path) -> BoardService {
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
        BoardService::new(state_dir, dir.to_path_buf(), Some(dir.join("home")))
            .with_instance_home(Some(dir.join("home/.yi-agent")))
    }

    /// 再加一张排队卡（走内存 board + 落盘，与 `service_with_card` 同一份）。
    fn facts(thread_id: &str, idle: bool, alive: bool) -> crate::service::ThreadFacts {
        crate::service::ThreadFacts {
            thread_id: thread_id.to_string(),
            idle,
            alive,
        }
    }

    /// 宿主一个动作都不会（旧宿主、或契约里没声明能力）。
    fn no_caps() -> DriverCapabilities {
        DriverCapabilities::default()
    }

    /// 宿主声明支持这些动作。
    fn caps(actions: &[&str]) -> DriverCapabilities {
        DriverCapabilities {
            actions: actions.iter().map(|action| action.to_string()).collect(),
        }
    }

    /// 一拍「宿主给了完整热表快照」的输入。
    fn loaded_input(ids: &[&str]) -> crate::driver_observation::ObservationInput {
        crate::driver_observation::ObservationInput {
            legacy: Vec::new(),
            reported_alive: Default::default(),
            loaded: Some(
                ids.iter()
                    .map(|id| crate::driver_observation::LoadedThreadFacts {
                        thread_id: (*id).to_string(),
                        idle: true,
                    })
                    .collect(),
            ),
        }
    }

    /// 宿主说这些会话都还忙着。
    fn busy(ids: &[&str]) -> Vec<crate::service::ThreadFacts> {
        ids.iter().map(|id| facts(id, false, true)).collect()
    }

    /// 一拍 `driver_step`，断言它交出了一个 launch 并把它交出来。
    fn launched(service: &BoardService) -> DriverAction {
        service
            .driver_step(&[], &[], None, &no_caps())
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

    /// 把并发峰值钉成 1，用来验「名额限制」。
    ///
    /// 峰值**只认全局层**，所以写进注入的 home（`service_with_card` 给的
    /// `<dir>/home`），不是项目目录——写项目目录现在读不到。
    fn one_slot_only(service: &BoardService) {
        peak_of(service, 1);
    }

    /// 把全机峰值钉成 `peak`（写进注入的 home，即全局层）。
    fn peak_of(service: &BoardService, peak: u16) {
        let calendar = superpowers_kanban_core::calendar::ConcurrencyCalendar {
            default_max_tasks: peak,
            ..Default::default()
        };
        let home = service.home.as_deref().expect("tests inject a home");
        calendar.save_global_in(home).unwrap();
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
        // 名额池按日历峰值开（不再是 `next_launch` 的参数），先钉成 1。
        one_slot_only(&service);
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
        one_slot_only(&service);
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

    /// 回归：**终态卡不得占用名额**。
    ///
    /// 线上症状：三张 `done` 的卡各自带着历史 `thread_id`，而 `observations.json`
    /// 是空的（daemon 刚重启、宿主还没上报）。`live` 判定把「没观测到」当成
    /// 「还活着」（`is_none_or`），于是这三张早就跑完的卡**永久**占住了全局名额，
    /// 后面 `queued` 的卡一张都起不来。日志里只有两行
    /// `has a live session but no slot could be re-adopted`，看起来像「没槽位」，
    /// 实际是「槽位被已完成的卡占着」。
    ///
    /// 判据本该看「会话是否还在」，但终态卡已经交付完毕，它上面的 thread_id 只是
    /// 历史记录。会话确实还在跑也好、已经不在了也好，都不该再挡住后面的卡：
    /// `is_terminal()` 优先于观测缺失。
    #[test]
    fn a_finished_card_never_holds_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let state_dir = dir.path().join(".yi-agent/superpowers-kanban");
        std::fs::create_dir_all(&state_dir).unwrap();

        // 复刻线上 board：一张 done 且留着 thread_id，一张 queued。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("card-done"),
            PathBuf::from("a.spec.md"),
            PathBuf::from("a.plan.md"),
            at(),
        );
        board
            .transition(&CardId::new("card-done"), CardState::Running)
            .unwrap();
        board
            .set_thread_id(&CardId::new("card-done"), "thread-gone".to_string())
            .unwrap();
        board
            .transition(&CardId::new("card-done"), CardState::Done)
            .unwrap();
        board.enqueue(
            CardId::new("card-next"),
            PathBuf::from("b.spec.md"),
            PathBuf::from("b.plan.md"),
            at(),
        );
        persist::save_board(&state_dir.join("board.json"), &board).unwrap();
        // 观测文件留空，与线上一致：没有任何 thread 的记录。

        let service = BoardService::new(state_dir, project, Some(dir.path().join("home")));
        one_slot_only(&service);
        service.adopt_running_leases(1);

        // 唯一的名额必须还在池子里：那张 done 的卡不许占着它。
        assert!(
            service.next_launch(1, at()).unwrap().is_some(),
            "a finished card must not keep a queued card from starting"
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

    /// 「实现卡」夹具：一张 `awaiting_merge` 卡，**并且**已经绑定真实开发树。
    ///
    /// 与 [`service_with_awaiting_card`] 的区别只在 workdir：那张只写 thread、没有
    /// workdir 的卡是**缺绑定**的反例，续跑必须拒绝它。成功续跑需要一份真实存在的
    /// 原开发树——合并要落到那里，而不是新检出的目标树。
    fn service_with_merge_bound_card(project: &std::path::Path) -> (BoardService, String) {
        let (service, source) = service_with_awaiting_card(project);
        let id = CardId::new("card-1");
        let development = crate::worktree::ensure_worktree(project, &id, &source).unwrap();
        persist::with_board(&service.state_dir, |board| {
            board
                .set_workdir(&id, development)
                .map_err(|error| error.to_string())
        })
        .unwrap();
        (service, source)
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
            Ok(crate::merge_lock::MergeAttempt::Granted(lock)) => {
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
                Ok(crate::merge_lock::MergeAttempt::Granted(_))
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

    /// 实现卡没有 `source_ref`,它的分支是 `kanban/<slug>`。指令里若拼进空串,
    /// 模型就会去合一个不存在的分支——而这是合并回合唯一的输入。
    #[test]
    fn merge_resume_names_the_derived_branch_of_an_implementation_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = service
            .driver_step(
                &[facts("thread-1", true, true)],
                &[],
                None,
                &caps(&["resume_append"]),
            )
            .unwrap()
            .expect("a merge turn");
        let DriverAction::ResumeAppend { objective, .. } = action else {
            panic!("expected a resume");
        };
        assert_eq!(source, "kanban/card-1");
        assert!(
            objective.contains(&format!("take '{source}' into")),
            "the derived branch must be named verbatim: {objective}"
        );
        assert!(!objective.contains("take '' into"), "{objective}");
    }

    #[test]
    fn the_queue_head_starts_merging_when_the_gate_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = service
            .driver_step(
                &[facts("thread-1", true, true)],
                &[],
                None,
                &caps(&["resume_append"]),
            )
            .unwrap()
            .expect("a merge turn");
        let DriverAction::ResumeAppend {
            card_id, objective, ..
        } = action
        else {
            panic!("expected a resume, got {action:?}");
        };
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

    /// 全机峰值 4、宿主说全机已有 4 个会话在跑、看板内 0 个 → 一个都不许起。
    ///
    /// 这是本改造最核心的回归锁：限的是**总流量**，看板只是调度工具。旧口径
    /// （`free_slots(4)`）会看到「本看板 0 个在跑」，于是再起 4 个——全机变成 8。
    #[test]
    fn outside_sessions_consume_the_same_budget() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        peak_of(&service, 4);

        let action = service.driver_step(&[], &[], Some(4), &no_caps()).unwrap();
        assert!(
            action.is_none(),
            "全机 4 个在跑、峰值 4：看板再起就是超发，实际得到 {action:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "queued");
        assert_eq!(state_of(&service, "card-2"), "queued");
    }
    /// 同上但峰值 5：全机已有 4 个，余量 1 → 恰好起一张。
    #[test]
    fn a_launch_is_allowed_when_the_whole_machine_has_room() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        peak_of(&service, 5);

        let action = service.driver_step(&[], &[], Some(4), &no_caps()).unwrap();
        assert!(
            matches!(action, Some(DriverAction::Launch { ref card_id, .. }) if card_id == "card-1"),
            "余量 1 → 队首那张走"
        );
        assert_eq!(state_of(&service, "card-2"), "queued", "一拍只走一张");
    }
    /// 看板内已有 1 张在跑、宿主报的全机数里**已经包含**它 → 不得再扣一次。
    ///
    /// 峰值 4、全机 4（含本看板那张）→ 余量 0。若实现仍在 `claim_next_launch` 里
    /// 减一遍看板内的 `running_count`，就会算出「还有 3 个空位」而超发。
    #[test]
    fn a_running_card_is_not_subtracted_twice() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.enqueue_card_for_test("card-2");
        peak_of(&service, 4);
        let claim = service.next_launch(4, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        assert_eq!(service.board_running_count(), 1);

        let action = service
            .driver_step(&busy(&["thread-1"]), &[], Some(4), &no_caps())
            .unwrap();
        assert!(
            action.is_none(),
            "全机 4 已达峰值，本看板那张已在其中，不能再起：{action:?}"
        );
        assert_eq!(state_of(&service, "card-2"), "queued");
    }
    /// 旧宿主不带 `sessions` → 退回**全机租约**口径（`budget = 峰值`），照常调度。
    ///
    /// 租约本来就按活会话记、跨项目共享，比按卡片状态数「在跑」更接近真相；
    /// 这里唯一不能做的是因为缺字段就整拍拒单——那会让旧宿主上的看板彻底停摆。
    #[test]
    fn a_host_that_does_not_report_sessions_falls_back_to_the_lease_pool() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        peak_of(&service, 4);

        let action = service.driver_step(&[], &[], None, &no_caps()).unwrap();
        assert!(
            matches!(action, Some(DriverAction::Launch { ref card_id, .. }) if card_id == "card-1"),
            "缺字段不能拒绝调度，否则旧宿主上的看板彻底停摆"
        );
    }
    /// 全机运行数超过峰值（例如别的进程先占了额度）：余量必须归零，不能下溢。
    #[test]
    fn a_running_count_above_the_peak_yields_zero_budget() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        peak_of(&service, 2);

        let action = service.driver_step(&[], &[], Some(7), &no_caps()).unwrap();
        assert!(action.is_none(), "saturating_sub：多于峰值也只会是 0");
        assert_eq!(state_of(&service, "card-1"), "queued");
    }
    /// 合并回合也吃全机额度：余量为 0 时队首留在队列里，不进 `running`。
    ///
    /// 合并要跑一次真实的模型回合，和实现会话抢的是同一份额度。不设这道闸，
    /// 峰值 4 的机器上会出现「4 个实现会话 + 1 个合并」= 5 个在跑。
    /// 队首必须原地等（仍在队列），而不是被改成 `running` 白占合并名额。
    #[test]
    fn the_merge_queue_waits_when_the_whole_machine_has_no_room() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        peak_of(&service, 4);
        service.merge_request("card-1").unwrap();
        assert_eq!(state_of(&service, "card-1"), "merge_queued");

        // 全机 4 个会话在跑 → 余量 0：本拍不动队。
        assert!(
            service
                .driver_step(
                    &[facts("thread-1", true, true)],
                    &[],
                    Some(4),
                    &caps(&["resume_append"])
                )
                .unwrap()
                .is_none(),
            "no room on the whole machine means no merge turn"
        );
        assert_eq!(
            state_of(&service, "card-1"),
            "merge_queued",
            "the head stays queued instead of grabbing the gate for nothing"
        );
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_none(),
            "no gate may be taken while the turn cannot run"
        );

        // 余量回来（全机 3 个在跑）→ 队首照常开局。
        let action = service
            .driver_step(
                &[facts("thread-1", true, true)],
                &[],
                Some(3),
                &caps(&["resume_append"]),
            )
            .unwrap()
            .expect("room is back: the queue head starts");
        assert!(
            matches!(action, DriverAction::ResumeAppend { ref card_id, .. } if card_id == "card-1"),
            "{action:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "running");
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
            .driver_step(&[], &[], None, &no_caps())
            .unwrap()
            .expect("a merge turn");
        let DriverAction::Launch {
            card_id, objective, ..
        } = action
        else {
            panic!("expected a launch, got {action:?}");
        };
        assert_eq!(card_id, merge_id);
        assert!(objective.contains("feat/manual"), "{objective}");
        assert_eq!(state_of(&service, &merge_id), "running");
    }

    /// `list` 要能告诉人「谁在合并」——只靠状态看不出来：合并期间卡就是 running。
    #[test]
    fn list_marks_the_card_that_holds_the_merge_gate() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
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
        let _ = service
            .driver_step(&[], &[], None, &caps(&["resume_append"]))
            .unwrap();
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
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        service
            .driver_step(&[], &[], None, &caps(&["resume_append"]))
            .unwrap()
            .expect("a merge turn");
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_some());

        // 回合还没收尾（卡仍是 running）→ 名额继续握着。
        service
            .driver_step(&[], &[], None, &caps(&["resume_append"]))
            .unwrap();
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_some(),
            "a merge still in flight must keep the gate"
        );

        // 宿主的成功回报到达：记录由 pending 升到 accepted，但**仍然**不放门。
        let action_id = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .expect("the attempt on record")
            .action_id;
        let ok = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
            error_code: None,
        };
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[ok],
                Some(1),
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_some(),
            "a reported merge is not a finished merge"
        );

        // 模型判定合并成功并落 `done`，再下一拍匹配会话明确 idle → 名额归还。
        service.set_state("card-1", CardState::Done, None).unwrap();
        service
            .driver_step(
                &[facts("thread-1", true, true)],
                &[],
                None,
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert!(
            wait_until(|| crate::merge_lock::read_holder(&service.state_dir).is_none()),
            "the settled merge must hand the gate back"
        );
        assert!(
            wait_until(|| matches!(
                crate::merge_lock::acquire_with_holder(&service.state_dir, "next"),
                Ok(crate::merge_lock::MergeAttempt::Granted(_))
            )),
            "the next project merge must be able to take the gate"
        );
    }

    /// 授权合并必须回到**原开发会话**（spec §6.9）。这是本需求的核心：
    /// 换一个新会话去合并，等于让一个没有上下文的陌生人替卡片收尾。
    #[test]
    fn an_implementation_merge_resumes_the_original_thread() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        let before = service.list()["cards"][0]["workdir"].clone();
        service.merge_request("card-1").unwrap();
        let resume_caps = caps(&["resume_append"]);
        let action = service
            .driver_step(&[facts("thread-1", true, true)], &[], Some(0), &resume_caps)
            .unwrap()
            .expect("authorized merge turn");
        assert!(
            matches!(action, DriverAction::ResumeAppend {
                ref thread_id, ref card_id, ..
            } if thread_id == "thread-1" && card_id == "card-1"),
            "the merge must resume the card's own thread, got {action:?}"
        );
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-1"));
        assert_eq!(
            service.list()["cards"][0]["workdir"],
            before,
            "resuming must not rebind the card to the merge tree"
        );
    }

    // ---- Task 6.5:释放与崩溃恢复 ----

    fn attempt_for(
        card: &str,
        action: &str,
        phase: crate::merge_attempt::MergePhase,
    ) -> crate::merge_attempt::MergeAttemptRecord {
        crate::merge_attempt::MergeAttemptRecord {
            version: crate::merge_attempt::ATTEMPT_VERSION,
            action_id: action.to_string(),
            card_id: card.to_string(),
            thread_id: Some("thread-1".to_string()),
            expected_workdir: PathBuf::from("/dev/tree"),
            phase,
            merge_queued_at: None,
        }
    }

    #[test]
    fn merge_recovery_blocks_a_graph_that_was_left_in_flight() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        // 进程在写完记录、还没收到结果时死掉。
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-1",
                "act-old",
                crate::merge_attempt::MergePhase::Pending,
            ),
        )
        .unwrap();
        let reason = service
            .recover_merge_attempt_on_startup()
            .unwrap()
            .expect("a leftover attempt blocks merges");
        assert!(reason.contains("card-1"), "{reason}");
        let record = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .unwrap();
        assert_eq!(record.phase, crate::merge_attempt::MergePhase::Uncertain);
    }

    #[test]
    fn merge_recovery_does_not_redispatch_after_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-1",
                "act-old",
                crate::merge_attempt::MergePhase::Accepted,
            ),
        )
        .unwrap();
        service.recover_merge_attempt_on_startup().unwrap();
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]);
        assert!(
            action.is_none(),
            "an unsettled attempt must stop every new merge: {action:?}"
        );
    }

    #[test]
    fn merge_recovery_blocks_on_an_old_holder_without_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        // 旧版:只有 holder,没有 journal。pid 已经死了也不算"工具停了"。
        std::fs::write(
            service.state_dir.join("merge.lock.holder"),
            r#"{"card_id":"card-1","pid":0,"since":"2026-10-01T00:00:00+08:00"}"#,
        )
        .unwrap();
        let reason = service.recover_merge_attempt_on_startup().unwrap();
        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains("holder")),
            "an orphaned holder must block, not be assumed harmless: {reason:?}"
        );
    }

    #[test]
    fn merge_recovery_blocks_on_a_corrupt_record_instead_of_dropping_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        std::fs::write(service.state_dir.join("merge.attempt.json"), b"{").unwrap();
        assert!(service.recover_merge_attempt_on_startup().is_err());
        assert!(service.recover_merge_attempt_on_startup().is_err());
        assert_eq!(
            std::fs::read(service.state_dir.join("merge.attempt.json")).unwrap(),
            b"{",
            "a corrupt record is never silently discarded"
        );
    }

    #[test]
    fn merge_recovery_refuses_without_acknowledging_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-1",
                "act-old",
                crate::merge_attempt::MergePhase::Uncertain,
            ),
        )
        .unwrap();
        let outcome =
            crate::merge_attempt::recover_offline(&service.state_dir, "card-1", "act-old", false);
        assert!(
            outcome.is_err(),
            "recovery needs an explicit stop confirmation"
        );
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn merge_recovery_requires_the_exact_action_and_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-1",
                "act-old",
                crate::merge_attempt::MergePhase::Uncertain,
            ),
        )
        .unwrap();
        assert!(
            crate::merge_attempt::recover_offline(&service.state_dir, "card-1", "act-other", true)
                .is_err(),
            "a near-miss action id must not clear the record"
        );
        assert!(
            crate::merge_attempt::recover_offline(&service.state_dir, "card-9", "act-old", true)
                .is_err(),
            "the wrong card must not clear the record"
        );
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn merge_recovery_refuses_while_the_card_is_still_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let action_id = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .unwrap()
            .action_id;
        assert_eq!(state_of(&service, "card-1"), "running");
        let outcome =
            crate::merge_attempt::recover_offline(&service.state_dir, "card-1", &action_id, true);
        assert!(
            outcome.is_err(),
            "the outcome must be decided before the record is cleared"
        );
    }

    #[test]
    fn merge_recovery_clears_an_unknown_card_after_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        // 卡被 purge 了:记录独立于卡片,所以还在;人核对过就能清。
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-gone",
                "act-old",
                crate::merge_attempt::MergePhase::Uncertain,
            ),
        )
        .unwrap();
        crate::merge_attempt::recover_offline(&service.state_dir, "card-gone", "act-old", true)
            .unwrap();
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none()
        );
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_none());
    }

    #[test]
    fn merge_recovery_refuses_while_the_daemon_still_holds_the_instance_lock() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-1",
                "act-old",
                crate::merge_attempt::MergePhase::Uncertain,
            ),
        )
        .unwrap();
        let daemon = crate::single_instance::acquire(&service.state_dir).expect("daemon lock");
        let outcome =
            crate::merge_attempt::recover_offline(&service.state_dir, "card-1", "act-old", true);
        assert!(
            outcome.is_err(),
            "recovering under a live daemon would race it"
        );
        drop(daemon);
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn merge_recovery_leaves_a_pending_record_for_the_daemon_to_settle() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let action_id = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .unwrap()
            .action_id;
        // 普通 merge-finish 不能清掉一条还没结算的记录。
        assert_eq!(
            service.merge_finish("card-1").unwrap(),
            MergeFinish::Waiting
        );
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_some()
        );
        // 另一张卡调用它:零副作用。
        assert_eq!(
            service.merge_finish("card-2").unwrap(),
            MergeFinish::Waiting
        );
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_some());
        let _ = action_id;
    }

    #[test]
    fn merge_recovery_lists_the_blocking_record_even_without_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        crate::merge_attempt::save(
            &service.state_dir,
            &attempt_for(
                "card-gone",
                "act-old",
                crate::merge_attempt::MergePhase::Uncertain,
            ),
        )
        .unwrap();
        let listed = service.list();
        assert_eq!(listed["merge_attempt"]["card_id"], "card-gone");
        assert_eq!(listed["merge_attempt"]["action_id"], "act-old");
        assert_eq!(listed["merge_attempt"]["phase"], "uncertain");
    }

    #[test]
    fn merge_recovery_reports_a_corrupt_record_through_list() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        std::fs::write(service.state_dir.join("merge.attempt.json"), b"{").unwrap();
        let listed = service.list();
        assert!(
            listed["merge_blocked_reason"]
                .as_str()
                .is_some_and(|r| r.contains("corrupt")),
            "{listed:?}"
        );
    }

    // Task 6 的纯判据:五条缺一条就不该放。
    #[test]
    fn merge_recovery_release_predicate_needs_every_condition() {
        use crate::merge_attempt::MergePhase;
        let idle = ThreadFacts {
            thread_id: "t".into(),
            idle: true,
            alive: true,
        };
        let busy = ThreadFacts {
            thread_id: "t".into(),
            idle: false,
            alive: true,
        };
        assert!(can_release_merge(
            &MergePhase::Accepted,
            false,
            Some(2),
            3,
            true,
            Some(&idle)
        ));
        // pending 结果未回
        assert!(!can_release_merge(
            &MergePhase::Accepted,
            true,
            Some(2),
            3,
            true,
            Some(&idle)
        ));
        // 同一拍:结果生效前带来的 idle 快照不能算
        assert!(!can_release_merge(
            &MergePhase::Accepted,
            false,
            Some(3),
            3,
            true,
            Some(&idle)
        ));
        // 卡还在 running / 绑定变了
        assert!(!can_release_merge(
            &MergePhase::Accepted,
            false,
            Some(2),
            3,
            false,
            Some(&idle)
        ));
        // 会话还忙
        assert!(!can_release_merge(
            &MergePhase::Accepted,
            false,
            Some(2),
            3,
            true,
            Some(&busy)
        ));
        // 缺观测
        assert!(!can_release_merge(
            &MergePhase::Accepted,
            false,
            Some(2),
            3,
            true,
            None
        ));
        // 还没受理
        assert!(!can_release_merge(
            &MergePhase::Pending,
            false,
            None,
            3,
            true,
            Some(&idle)
        ));
        // 已经不确定的地方不自动放
        assert!(!can_release_merge(
            &MergePhase::Uncertain,
            false,
            Some(2),
            3,
            true,
            Some(&idle)
        ));
    }

    // ---- Task 5.5:续跑派发的边界 ----

    fn resume_action(service: &BoardService, threads: &[ThreadFacts]) -> Option<DriverAction> {
        service
            .driver_step(threads, &[], Some(0), &caps(&["resume_append"]))
            .unwrap()
    }

    fn resume_ok(action_id: &str) -> DriverResult {
        DriverResult {
            action_id: action_id.to_string(),
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
            error_code: None,
        }
    }

    #[test]
    fn merge_resume_skips_a_busy_head_and_starts_the_next_card() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.enqueue_card_for_test("card-2");
        // card-2 也有自己的开发树与 thread:忙的 card-1 不该挡住它。
        let id = CardId::new("card-2");
        let branch = format!("kanban/{}", crate::worktree::slugify(&id));
        let tree = crate::worktree::ensure_worktree(&project, &id, &branch).unwrap();
        persist::with_board(&service.state_dir, |board| {
            board.set_workdir(&id, tree).map_err(|e| e.to_string())?;
            board
                .set_thread_id(&id, "thread-2".to_string())
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .unwrap();
        service
            .set_state("card-2", CardState::AwaitingMerge, None)
            .unwrap();
        service.merge_request("card-1").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        service.merge_request("card-2").unwrap();
        let action = resume_action(
            &service,
            &[
                facts("thread-1", false, true),
                facts("thread-2", true, true),
            ],
        )
        .expect("the next usable card must start");
        assert!(
            matches!(action, DriverAction::ResumeAppend { ref card_id, .. } if card_id == "card-2"),
            "{action:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
    }

    #[test]
    fn merge_resume_dispatches_cold_without_any_observation() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        // 缺观测:允许尝试,最终由宿主的 busy 判断兜底。
        let action = resume_action(&service, &[]).expect("a cold resume must be attempted");
        assert!(
            matches!(action, DriverAction::ResumeAppend { ref thread_id, .. } if thread_id == "thread-1"),
            "{action:?}"
        );
    }

    #[test]
    fn merge_resume_keeps_the_queue_and_explains_when_the_host_cannot_resume() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        // 旧宿主:零能力、零预算。仍然要把原因说清楚,且不派任何动作。
        let action = service
            .driver_step(&[facts("thread-1", true, true)], &[], None, &no_caps())
            .unwrap();
        assert!(action.is_none(), "{action:?}");
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        let detail = service.list()["cards"][0]["detail"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        assert!(detail.contains("resume_append"), "{detail}");
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_none());
    }

    #[test]
    fn merge_resume_never_launches_a_replacement_when_the_host_cannot_resume() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        for _ in 0..3 {
            let action = service
                .driver_step(&[facts("thread-1", true, true)], &[], Some(0), &no_caps())
                .unwrap();
            assert!(action.is_none(), "no fallback to a new session: {action:?}");
        }
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-1"));
    }

    #[test]
    fn merge_resume_sends_the_card_to_you_when_the_binding_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 只写 thread、没有 workdir 的卡:绑定不全,不能猜一个目录去恢复。
        let (service, _source) = service_with_awaiting_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]);
        assert!(action.is_none(), "{action:?}");
        assert_eq!(state_of(&service, "card-1"), "needs_you");
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_none());
    }

    #[test]
    fn merge_resume_reuses_an_existing_lease_instead_of_taking_a_second() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        peak_of(&service, 1);
        service.merge_request("card-1").unwrap();
        // 峰值 1,而这张卡自己的会话已经在跑并占着那一份。
        // 先取租约再进锁:同一个表达式里锁两次就是自锁。
        let leases_dir = service.lock().leases_dir.clone().unwrap();
        let own_lease = crate::lease::acquire_in(&leases_dir, 1).expect("the card's own lease");
        service
            .lock()
            .leases
            .insert(CardId::new("card-1"), own_lease);
        let action = resume_action(&service, &[facts("thread-1", true, true)]);
        assert!(
            action.is_some(),
            "an already-leased card must still merge: {action:?}"
        );
    }

    #[test]
    fn merge_resume_writes_the_attempt_before_it_reports_running() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("expected a resume, got {action:?}");
        };
        let record = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .expect("the attempt must be on disk before the action is handed out");
        assert_eq!(record.action_id, *action_id);
        assert_eq!(record.card_id, "card-1");
        assert_eq!(record.thread_id.as_deref(), Some("thread-1"));
        assert_eq!(record.phase, crate::merge_attempt::MergePhase::Pending);
        assert_eq!(state_of(&service, "card-1"), "running");
    }

    /// 顺序的**可观测**证明:安全记录写不下去时,卡绝不能已经变成 `Running`。
    ///
    /// 把 `merge.attempt.json` 占成一个目录,`save` 的 rename 必然失败。若实现
    /// 先落 `Running`,这里就会看到一张「在跑但没有记录」的卡——正是 spec §8.1
    /// 禁止的中间态:重启后没人知道有合并可能已经在飞。
    #[test]
    fn merge_resume_does_not_report_running_when_the_attempt_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        // 目录占位:create/rename 都会失败。
        std::fs::create_dir_all(
            service
                .state_dir
                .join(crate::merge_attempt::ATTEMPT_FILE)
                .with_extension("json.tmp"),
        )
        .unwrap();

        assert!(
            resume_action(&service, &[facts("thread-1", true, true)]).is_none(),
            "without a durable attempt no action may be handed out"
        );
        assert_eq!(
            state_of(&service, "card-1"),
            "merge_queued",
            "a card must not look running when its safety record never landed"
        );
        assert!(
            matches!(crate::merge_attempt::read(&service.state_dir), Ok(None)),
            "no record may be left behind when the write failed before delivery"
        );
    }

    #[test]
    fn merge_resume_keeps_the_original_queue_time_when_the_session_is_busy() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let queued_at = service.list()["cards"][0]["merge_queued_at"].clone();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        let busy = DriverResult {
            action_id: action_id.clone(),
            ok: false,
            thread_id: None,
            error: Some("the session is busy".to_string()),
            error_code: Some("busy".to_string()),
        };
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[busy],
                Some(1),
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        assert_eq!(
            service.list()["cards"][0]["merge_queued_at"],
            queued_at,
            "requeueing must keep the original position"
        );
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none(),
            "a provably undelivered merge must not keep the gate"
        );
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_none());
    }

    #[test]
    fn merge_resume_does_not_overwrite_a_state_the_model_already_wrote() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        // 用户在宿主的 busy 回报之前就把它标成了 done。
        service.set_state("card-1", CardState::Done, None).unwrap();
        let busy = DriverResult {
            action_id: action_id.clone(),
            ok: false,
            thread_id: None,
            error: Some("busy".to_string()),
            error_code: Some("busy".to_string()),
        };
        service
            .driver_step(&[], &[busy], Some(0), &caps(&["resume_append"]))
            .unwrap();
        assert_eq!(
            state_of(&service, "card-1"),
            "done",
            "the user's verdict wins"
        );
    }

    #[test]
    fn merge_resume_success_does_not_write_the_card_state() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        service.set_state("card-1", CardState::Done, None).unwrap();
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[resume_ok(action_id.as_str())],
                Some(1),
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "done");
        let record = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .unwrap();
        assert_eq!(record.phase, crate::merge_attempt::MergePhase::Accepted);
        assert_eq!(
            record.thread_id.as_deref(),
            Some("thread-1"),
            "an accepted merge keeps the card's original session"
        );
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-1"));
    }

    #[test]
    fn merge_resume_treats_an_unknown_failure_code_as_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        let unknown = DriverResult {
            action_id: action_id.clone(),
            ok: false,
            thread_id: None,
            error: Some("something odd".to_string()),
            error_code: None,
        };
        service
            .driver_step(&[], &[unknown], Some(0), &caps(&["resume_append"]))
            .unwrap();
        let record = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .expect("an unclassified failure must keep the safety gate");
        assert_eq!(record.phase, crate::merge_attempt::MergePhase::Uncertain);
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_some(),
            "an uncertain merge must keep the gate"
        );
    }

    #[test]
    fn merge_resume_sends_the_card_to_you_when_the_session_cannot_be_found() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        let missing = DriverResult {
            action_id: action_id.clone(),
            ok: false,
            thread_id: None,
            error: Some("no such thread".to_string()),
            error_code: Some("thread_not_found".to_string()),
        };
        service
            .driver_step(&[], &[missing], Some(0), &caps(&["resume_append"]))
            .unwrap();
        assert_eq!(state_of(&service, "card-1"), "needs_you");
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none(),
            "a binding that cannot come back must not leave a gate behind"
        );
        assert!(crate::merge_lock::read_holder(&service.state_dir).is_none());
    }

    #[test]
    fn merge_resume_ignores_a_result_that_does_not_match_the_attempt_on_record() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        // 一份指向别处的"成功"回报:错 thread id 不能把绑定写坏。
        let wrong_thread = DriverResult {
            action_id: action_id.clone(),
            ok: true,
            thread_id: Some("thread-999".to_string()),
            error: None,
            error_code: None,
        };
        service
            .driver_step(&[], &[wrong_thread], Some(0), &caps(&["resume_append"]))
            .unwrap();
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-1"));
        let record = crate::merge_attempt::read(&service.state_dir)
            .unwrap()
            .unwrap();
        assert_eq!(record.phase, crate::merge_attempt::MergePhase::Uncertain);
    }

    #[test]
    fn merge_resume_does_not_settle_a_late_result_for_an_older_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("{action:?}");
        };
        // 先让 busy 把它干净退队(本次安全记录已 CAS 清掉)。
        let busy = DriverResult {
            action_id: action_id.clone(),
            ok: false,
            thread_id: None,
            error: Some("busy".to_string()),
            error_code: Some("busy".to_string()),
        };
        service
            .driver_step(&[], &[busy], Some(0), &caps(&["resume_append"]))
            .unwrap();
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none()
        );
        // 同一份结果重投一遍:没有 pending 认领它,它绝不能把那次已经结算的
        // 动作重新变成"已受理"。卡退队后确实可以被下一次派发重新拿下,所以
        // 这里只断言"没有为旧动作伪造出 Accepted"。
        service
            .driver_step(
                &[],
                &[resume_ok(action_id.as_str())],
                Some(0),
                &caps(&["resume_append"]),
            )
            .unwrap();
        if let Some(record) = crate::merge_attempt::read(&service.state_dir).unwrap() {
            assert_ne!(
                record.action_id.as_str(),
                action_id.as_str(),
                "a late result must not revive the settled attempt"
            );
            assert_eq!(
                record.phase,
                crate::merge_attempt::MergePhase::Pending,
                "a new attempt can only start from pending"
            );
        }
    }

    #[test]
    fn merge_resume_does_not_dispatch_while_an_attempt_is_still_unsettled() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        // 记录已在盘上:哪怕卡片回到队列,也不能再派一个合并。
        service
            .set_state("card-1", CardState::AwaitingMerge, None)
            .unwrap();
        service.merge_request("card-1").unwrap();
        let again = resume_action(&service, &[facts("thread-1", true, true)]);
        assert!(
            again.is_none(),
            "a project with an unsettled attempt must not start another merge: {again:?}"
        );
    }

    /// P7:模型早早写了 `done`,但会话还在跑那个合并回合——名额不能放手。
    /// 只有"结果之后的**新**一拍里,匹配的 thread 明确 alive 且 idle"才证明它停了。
    #[test]
    fn merge_resume_done_while_busy_keeps_the_project_gate() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)])
            .expect("a merge turn")
            .to_json();
        let result = DriverResult {
            action_id: action["action_id"].as_str().unwrap().to_string(),
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
            error_code: None,
        };
        // 模型先写 done,同一拍里回报到达,而观测说会话仍然忙。
        service.set_state("card-1", CardState::Done, None).unwrap();
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[result],
                Some(1),
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert!(
            crate::merge_lock::acquire(&service.state_dir).is_none(),
            "a merge still running must keep the project gate"
        );
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_some(),
            "and its safety record"
        );
        // 下一拍:会话明确 idle → 记录与门一起归还。
        service
            .driver_step(
                &[facts("thread-1", true, true)],
                &[],
                Some(0),
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none(),
            "an idle, finished card releases the safety record"
        );
        assert!(
            crate::merge_lock::acquire(&service.state_dir).is_some(),
            "and the project gate becomes acquirable again"
        );
    }

    #[test]
    fn merge_resume_standalone_merge_cards_still_launch() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        git_run(&project, &["branch", "feat/manual"]);
        // 独立合并卡就是 `add-merge` 投递的那种:没有实现计划,本来就没有
        // "原会话"可回,所以它必须继续走 `Launch`（spec §6.8）。
        crate::dispatch::dispatch_with_service(
            &service,
            &service.state_dir,
            None,
            "enqueue_merge",
            &json!({ "source": "feat/manual" }),
        )
        .unwrap();
        service.consume_inbox(chrono::Local::now());
        let delivered = service.list()["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["kind"] == "merge")
            .and_then(|card| card["id"].as_str().map(str::to_string))
            .expect("the merge card was delivered");
        let action = resume_action(&service, &[]).expect("a standalone merge launches");
        assert!(
            matches!(action, DriverAction::Launch { ref card_id, .. } if *card_id == delivered),
            "{action:?}"
        );
    }

    #[test]
    fn only_one_merge_starts_per_tick() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
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
        let _ = service
            .driver_step(&[], &[], None, &caps(&["resume_append"]))
            .unwrap()
            .expect("first merge");
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
        assert!(matches!(
            gate,
            Ok(crate::merge_lock::MergeAttempt::Granted(_))
        ));
        assert!(
            service
                .driver_step(&[], &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        drop(gate);
    }

    /// 合并回合开在**新会话**里，所以旧会话忙不忙都不影响：一张排着队的卡
    /// 不该因为「它原来的会话还在跑」而被无限期压住。
    #[test]
    /// 原会话正忙时**不占门、不改时间**:它的候选地位让给后面那几张,
    /// 最终由宿主的 busy 判断兜底（spec §6.2）。旧行为是"忙就另起一个会话",
    /// 那正是这次要修掉的上下文断层。
    fn a_busy_original_session_does_not_take_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        service.merge_request("card-1").unwrap();
        let action = service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[],
                None,
                &caps(&["resume_append"]),
            )
            .unwrap();
        assert!(
            action.is_none(),
            "a busy session is not interrupted: {action:?}"
        );
        assert_eq!(state_of(&service, "card-1"), "merge_queued");
        assert!(
            crate::merge_lock::read_holder(&service.state_dir).is_none(),
            "a skipped candidate must not hold the project gate"
        );
        assert!(
            service.list()["cards"][0]["merge_queued_at"].is_string(),
            "skipping must not rewrite the original queue time"
        );
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
        } = service
            .driver_step(&[], &[], None, &no_caps())
            .unwrap()
            .expect("a launch")
        else {
            panic!("expected a launch");
        };
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

        let action = service
            .driver_step(&[], &[], None, &no_caps())
            .unwrap()
            .expect("a launch");
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
            .driver_step(&[], &[], None, &no_caps())
            .unwrap()
            .expect("the head launches");
        assert!(matches!(action, DriverAction::Launch { ref card_id, .. } if card_id == "card-1"));
        // 第一张还持着名额（`Launching` 也占租约）→ 第二张不出。
        assert!(
            service
                .driver_step(&[], &[], None, &no_caps())
                .unwrap()
                .is_none(),
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
                .driver_step(&busy(&["thread-1"]), &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running", "还忙就不动它");
        // 宿主**完全没报**这台会话（重启后它不记得任何 thread）：状态照旧不动，
        // 展示降级为 unknown。旧宿主「这一拍没提」不是「它已经没了」的证据——
        // 那正是本次要根除的误报，所以不标 stale，也不声称加载过。
        assert!(
            service
                .driver_step(&[], &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        let card = &service.list()["cards"][0];
        assert_eq!(card["stale"], false, "缺证据不等于会话已经不存在");
        assert_eq!(card["session_observation"], "unknown");
    }

    /// 一拍完整的「Launch → 回报」：绑定还没落盘时，观测旁路就已经记下了这个
    /// 会话。
    ///
    /// 顺序是安全性质：`related_thread_ids` 会把命中本进程待办的成功回报 ID
    /// 纳入本拍范围，而保存发生在消费之前，所以同一拍的 `list` 就看得到
    /// `loaded`。反过来（先消费再保存），这里会看到 `unknown`。
    #[test]
    fn kanban_observation_observes_a_launch_before_the_binding_lands() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        // 手写一条待办，模拟 `driver_step` 刚刚交出这个 launch。
        service.remember_pending(
            "act-1",
            &superpowers_kanban_core::card::CardId::new(claim.card_id.clone()),
            PendingKind::Launch,
        );
        let result = DriverResult {
            action_id: "act-1".to_string(),
            ok: true,
            thread_id: Some("thread-9".to_string()),
            error: None,
            error_code: None,
        };
        service
            .driver_step_with_loaded(
                &loaded_input(&["thread-9"]),
                std::slice::from_ref(&result),
                Some(1),
                &no_caps(),
            )
            .unwrap();
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-9"));
        assert_eq!(
            service.list()["cards"][0]["session_observation"],
            "loaded",
            "绑定落盘前就该被观测到"
        );

        // 同一批回报再来一次：待办已摘除，不得重新生效，也不得写旁路。
        let before = std::fs::read_to_string(service.state_dir.join("observations.json")).unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[result], Some(1), &no_caps())
            .unwrap();
        let after = std::fs::read_to_string(service.state_dir.join("observations.json")).unwrap();
        assert_ne!(before, after, "本拍的完整快照仍然会推进观测时间");
        assert_eq!(
            service.list()["cards"][0]["session_observation"],
            "not_loaded",
            "重复回报不再把 ID 带进本拍范围"
        );
    }

    /// 陌生 `action_id` 带回的会话不进旁路：宿主手里可能有别的项目的动作 id，
    /// 照单全收就把无关会话写进了本项目。
    #[test]
    fn kanban_observation_ignores_a_stranger_action_id() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let result = DriverResult {
            action_id: "act-stranger".to_string(),
            ok: true,
            thread_id: Some("thread-foreign".to_string()),
            error: None,
            error_code: None,
        };
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[result], None, &no_caps())
            .unwrap();
        let file = crate::observation::load(&service.state_dir);
        assert!(
            !file.threads.contains_key("thread-foreign"),
            "陌生动作带来的会话不得落进本项目旁路"
        );
    }

    /// 续跑回报带回的 ID 必须**等于**派发身份；串线的 ID 不建立新绑定。
    #[test]
    fn kanban_observation_rejects_a_resume_with_the_wrong_thread() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        service.mark_running("card-1", "thread-1").unwrap();
        service.remember_pending_with_resume(
            "act-resume",
            &superpowers_kanban_core::card::CardId::new("card-1"),
            PendingKind::ResumeForMerge,
            "thread-1",
        );
        let result = DriverResult {
            action_id: "act-resume".to_string(),
            ok: true,
            thread_id: Some("thread-elsewhere".to_string()),
            error: None,
            error_code: None,
        };
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[result], None, &no_caps())
            .unwrap();
        let file = crate::observation::load(&service.state_dir);
        assert!(
            !file.threads.contains_key("thread-elsewhere"),
            "串线的续跑 ID 不能写进旁路"
        );
        assert!(
            file.threads.contains_key("thread-1"),
            "卡自己的绑定仍在观测范围内"
        );
    }

    /// 已受理（`Accepted`）那一拍就算 idle 也**不放**门：同一份请求里的 idle
    /// 快照可能发生在结果生效之前。下一拍合规 idle 才按旧规则释放。
    #[test]
    fn kanban_observation_accepted_idle_waits_one_tick() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let (service, _source) = service_with_merge_bound_card(&project);
        peak_of(&service, 1);
        service.merge_request("card-1").unwrap();
        let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
        let DriverAction::ResumeAppend { action_id, .. } = &action else {
            panic!("expected a resume, got {action:?}");
        };
        // 模型收尾 + 回报：这一拍受理，idle 观测**不得**当拍放门。
        service.set_state("card-1", CardState::Done, None).unwrap();
        assert!(
            service
                .driver_step(
                    &[facts("thread-1", true, true)],
                    &[resume_ok(action_id)],
                    Some(1),
                    &no_caps(),
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .map(|record| record.phase),
            Some(crate::merge_attempt::MergePhase::Accepted),
            "同一拍不得结算"
        );
        // 下一拍：合规 idle → 记录清掉、门释放。
        service
            .driver_step(&[facts("thread-1", true, true)], &[], Some(1), &no_caps())
            .unwrap();
        assert!(
            crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .is_none(),
            "晚于受理一拍且明确 idle 才允许释放"
        );
    }

    /// `Pending` / `Uncertain` 阶段：新事实里缺合并会话也**不得**提前放租约。
    ///
    /// 这是本次改动最容易出事的边界——完整快照的「未命中」看起来像死亡，
    /// 但一次未加载不能证明目标树那边已经停了。
    #[test]
    fn kanban_observation_merge_attempt_keeps_its_lease() {
        for phase in [
            crate::merge_attempt::MergePhase::Pending,
            crate::merge_attempt::MergePhase::Uncertain,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let project = project_with_worktree(dir.path());
            let (service, _source) = service_with_merge_bound_card(&project);
            peak_of(&service, 1);
            service.merge_request("card-1").unwrap();
            let action = resume_action(&service, &[facts("thread-1", true, true)]).unwrap();
            let DriverAction::ResumeAppend { action_id, .. } = &action else {
                panic!("expected a resume, got {action:?}");
            };
            // 先把安全记录改成要测的阶段。
            let record = crate::merge_attempt::read(&service.state_dir)
                .unwrap()
                .expect("an attempt");
            crate::merge_attempt::save(
                &service.state_dir,
                &crate::merge_attempt::MergeAttemptRecord {
                    phase: phase.clone(),
                    ..record
                },
            )
            .unwrap();
            // 卡自己的会话这一拍没被完整快照命中。
            let before = service.lock().leases.len();
            assert_eq!(before, 1, "派发时就该拿到那份 lease");
            service
                .driver_step_with_loaded(&loaded_input(&[]), &[], Some(0), &no_caps())
                .unwrap();
            assert_eq!(
                service.lock().leases.len(),
                1,
                "{phase:?} 期间不得因一次未加载就把 lease 放走"
            );
            assert!(
                crate::merge_attempt::read(&service.state_dir)
                    .unwrap()
                    .is_some(),
                "{phase:?} 期间安全记录必须还在"
            );
            let _ = action_id;
        }
    }

    /// 热 ID 从宿主热表消失、但会话历史还在磁盘上：`list` 必须降级成
    /// `not_loaded`，而卡的绑定与历史字节一个不动。
    ///
    /// 这是「宿主忘了它」这一侧的证据：完整快照是权威的，未命中就是明确未加载；
    /// 但看板**不许**因此改写 `thread_id`、`state`、`workdir`——会话历史仍然存在，
    /// 人还能点开看（`thread_id` 就是入口）。
    #[test]
    fn kanban_observation_forgotten_hot_thread_keeps_card_binding_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 会话历史（夹具里的「磁盘上的字节」）：观测读不读它都必须一字不动。
        let history = project.join(".yi-agent/threads/thread-1.jsonl");
        std::fs::create_dir_all(history.parent().unwrap()).unwrap();
        std::fs::write(&history, "{\"type\":\"user\"}\n").unwrap();
        let bytes_before = std::fs::read(&history).unwrap();
        let workdir_before = service.list()["cards"][0]["workdir"].clone();

        service
            .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
            .unwrap();
        let loaded = service.list();
        assert_eq!(loaded["cards"][0]["session_observation"], "loaded");
        assert_eq!(loaded["cards"][0]["stale"], false);

        // 宿主不再报告它（完整快照里没有）。
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[], Some(1), &no_caps())
            .unwrap();
        let after = service.list();
        assert_eq!(
            after["cards"][0]["session_observation"], "not_loaded",
            "a complete snapshot that omits the thread is explicit not-loaded"
        );
        assert_eq!(after["cards"][0]["stale"], true);
        assert_eq!(
            after["cards"][0]["state"], "running",
            "never migrates state"
        );
        assert_eq!(after["cards"][0]["thread_id"], "thread-1");
        assert_eq!(
            after["cards"][0]["workdir"], workdir_before,
            "observation never rewrites the workdir"
        );
        assert_eq!(
            std::fs::read(&history).unwrap(),
            bytes_before,
            "observation must not touch the session history"
        );
    }

    /// 归档一张**热**卡：它从人看的列表里消失，但会话仍占着名额，所以那份
    /// lease 必须继续算它。
    ///
    /// 归档只接受终态卡（`Board::archive` 明确拒绝 queued/running），所以这里
    /// 用一张 `failed` 卡：它已交付、可以归档，而 `is_terminal()` 不能当作
    /// 「不占名额」的依据——`release` 的契约就是「会话还活着就继续占」。
    ///
    /// 关键点是归档**不**把卡排除出观测集合：`related_thread_ids` 收的是看板
    /// 全体（含 archived），否则下一次原子保存就会把这张卡唯一的热记录剪掉，
    /// 名额随之被放开。
    #[test]
    fn kanban_observation_archived_card_still_holds_its_lease() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
            .unwrap();
        assert_eq!(service.lock().leases.len(), 1, "the launch holds one slot");

        // 报失败（会话可能还活着，`release` 契约如此），然后归档。
        service
            .mark_terminal(&claim.card_id, "failed", Some("boom"))
            .unwrap();
        persist::with_board(&service.state_dir, |board| {
            board
                .archive(&CardId::new(&claim.card_id))
                .map_err(|error| error.to_string())
        })
        .unwrap();
        assert!(
            !service.list()["cards"]
                .as_array()
                .unwrap()
                .iter()
                .any(|card| card["id"] == claim.card_id
                    && card["archived"] == false
                    && card["state"] != "failed"),
            "the card is archived"
        );

        // 它这一拍仍然被宿主报告为已加载 → 名额必须留着。
        service
            .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
            .unwrap();
        assert!(
            crate::observation::load(&service.state_dir)
                .threads
                .contains_key("thread-1"),
            "an archived card stays in the observed set"
        );
        assert_eq!(
            service.lock().leases.len(),
            1,
            "an archived card whose session is still loaded keeps its slot"
        );
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "the pool stays full, so nothing new may start"
        );
    }

    /// `purge_archived` 之后的旁路清理：没有 attempt 引用的记录必须走，
    /// **有** attempt 引用的那张必须留。
    #[test]
    fn kanban_observation_purge_drops_only_unreferenced_observations() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
            .unwrap();
        assert!(
            crate::observation::load(&service.state_dir)
                .threads
                .contains_key("thread-1")
        );

        // 一次仍在飞的合并安全记录把 `thread-1` 钉住。
        crate::merge_attempt::save(
            &service.state_dir,
            &crate::merge_attempt::MergeAttemptRecord {
                version: crate::merge_attempt::ATTEMPT_VERSION,
                action_id: "a1".to_string(),
                card_id: claim.card_id.clone(),
                thread_id: Some("thread-1".to_string()),
                expected_workdir: project.canonicalize().unwrap(),
                phase: crate::merge_attempt::MergePhase::Uncertain,
                merge_queued_at: None,
            },
        )
        .unwrap();
        // 卡已交付并归档 → 从活跃列表消失；只剩 attempt 还引用着那个会话。
        service
            .mark_terminal(&claim.card_id, "failed", Some("boom"))
            .unwrap();
        persist::with_board(&service.state_dir, |board| {
            board
                .archive(&CardId::new(&claim.card_id))
                .map_err(|error| error.to_string())?;
            board.purge_archived();
            Ok(())
        })
        .unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[], Some(1), &no_caps())
            .unwrap();
        assert!(
            crate::observation::load(&service.state_dir)
                .threads
                .contains_key("thread-1"),
            "a thread named by a live merge attempt must keep its record"
        );

        // 记录撤掉、卡也不再引用它：旁路随之清干净。
        std::fs::remove_file(service.state_dir.join(crate::merge_attempt::ATTEMPT_FILE)).unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&[]), &[], Some(1), &no_caps())
            .unwrap();
        assert!(
            crate::observation::load(&service.state_dir)
                .threads
                .is_empty(),
            "an unreferenced record must be pruned"
        );
    }

    /// 余量为 0 时**不派发**，但重启补账照做：已经跑着的会话必须占住名额，
    /// 而且同一张卡永远只有**一份** lease。
    #[test]
    fn kanban_observation_zero_budget_still_adopts_exactly_one_lease() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        // 模拟重启：进程内的租约账本清空，磁盘上头还活着。
        service.lock().leases.clear();
        assert_eq!(service.lock().leases.len(), 0);

        // 全机已跑满（limit=1，sessions_running=1）→ 余量 0。
        let action = service
            .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
            .unwrap();
        assert!(action.is_none(), "budget 0 must not dispatch anything");
        assert_eq!(
            service.lock().leases.len(),
            1,
            "the running session's slot is re-adopted even with no budget"
        );

        // 再拍两次都仍是已加载：绝不允许补出第二份。
        for _ in 0..2 {
            service
                .driver_step_with_loaded(&loaded_input(&["thread-1"]), &[], Some(1), &no_caps())
                .unwrap();
            assert_eq!(
                service.lock().leases.len(),
                1,
                "one card holds exactly one lease across repeated loaded ticks"
            );
        }
    }

    /// 坏 journal：整拍**前置拒绝**（回报未消费），租约保持原样。
    ///
    /// 为什么不「跳过合并、照常派发」：读不懂的记录说明门的归属不可知。这一拍
    /// 仍然可以派新会话，但一旦派了，宿主就会认为上一批 results 已送达；而
    /// 我们其实连「那次合并到底交没交出去」都答不上来。
    #[test]
    fn kanban_observation_corrupt_attempt_is_rejected_before_any_effect() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .driver_step(&[facts("thread-1", true, true)], &[], Some(1), &no_caps())
            .unwrap();
        assert_eq!(service.lock().leases.len(), 1);
        std::fs::write(
            service.state_dir.join(crate::merge_attempt::ATTEMPT_FILE),
            "{ not json",
        )
        .unwrap();
        let before = std::fs::read_to_string(service.state_dir.join("observations.json")).unwrap();
        let error = service
            .driver_step(&[facts("thread-1", true, true)], &[], Some(1), &no_caps())
            .unwrap_err();
        assert!(error.contains("corrupt"), "{error}");
        assert_eq!(
            service.lock().leases.len(),
            1,
            "坏记录不得导致租约被提前释放"
        );
        assert_eq!(
            std::fs::read_to_string(service.state_dir.join("observations.json")).unwrap(),
            before,
            "前置拒绝发生在保存之前"
        );
    }

    /// 观测落盘失败时**整拍不生效**：不消费回报、不推进序号、不动租约。
    ///
    /// 用目录占据临时文件名让 `fs::write` 明确失败（不用 chmod：root／ACL
    /// 环境下不可靠）。去掉占位后重试必须成功一次——证明失败是可恢复的，
    /// 宿主重发同一批 results 就能把账补回来。
    #[test]
    fn kanban_observation_save_failure_consumes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 先让一张在跑的卡占住一个租约，好让「租约没动」有东西可比。
        let claim = service.next_launch(1, at()).unwrap().unwrap();
        service.mark_running(&claim.card_id, "thread-1").unwrap();
        service
            .driver_step(&[facts("thread-1", true, true)], &[], Some(1), &no_caps())
            .unwrap();
        let observations_before =
            std::fs::read(service.state_dir.join("observations.json")).unwrap();
        let leases_before = service.lock().leases.len();
        let tick_before = service.lock().driver_tick_seq;
        service.remember_pending(
            "act-2",
            &superpowers_kanban_core::card::CardId::new("card-1"),
            PendingKind::Launch,
        );
        let pending_before = service.lock().pending_actions.len();
        let result = DriverResult {
            action_id: "act-2".to_string(),
            ok: true,
            thread_id: Some("thread-2".to_string()),
            error: None,
            error_code: None,
        };

        // 占住临时文件名：`observations.json.tmp` 是目录 => 写入失败。
        let blocker = service.state_dir.join("observations.json.tmp");
        std::fs::create_dir_all(&blocker).unwrap();

        let error = service
            .driver_step_with_loaded(
                &loaded_input(&["thread-2"]),
                std::slice::from_ref(&result),
                Some(1),
                &no_caps(),
            )
            .unwrap_err();
        assert!(
            error.contains("could not save session observations"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(service.state_dir.join("observations.json")).unwrap(),
            observations_before,
            "旁路文件必须原样不动"
        );
        assert_eq!(
            service.lock().pending_actions.len(),
            pending_before,
            "回报不得被消费：宿主重发时还要能对上"
        );
        assert_eq!(service.lock().driver_tick_seq, tick_before, "序号不得推进");
        assert_eq!(service.lock().leases.len(), leases_before, "租约不得变动");
        assert_eq!(
            thread_of(&service, "card-1").as_deref(),
            Some("thread-1"),
            "绑定不得被覆盖"
        );

        // 去掉占位后重试：这一拍成功，回报生效。
        std::fs::remove_dir_all(&blocker).unwrap();
        service
            .driver_step_with_loaded(&loaded_input(&["thread-2"]), &[result], Some(1), &no_caps())
            .unwrap();
        assert_eq!(thread_of(&service, "card-1").as_deref(), Some("thread-2"));
        assert!(service.lock().pending_actions.is_empty());
    }

    /// 坏 board：同样在前置阶段拒绝，观测文件都不该被创建或改写。
    #[test]
    fn kanban_observation_corrupt_board_is_rejected_before_saving() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        std::fs::write(service.state_dir.join("board.json"), "{ not json").unwrap();
        let error = service
            .driver_step_with_loaded(&loaded_input(&[]), &[], None, &no_caps())
            .unwrap_err();
        assert!(error.contains("corrupt"), "{error}");
        assert!(
            !service.state_dir.join("observations.json").exists(),
            "前置读取失败不得留下半份观测"
        );
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
        assert!(
            service
                .driver_step(&threads, &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
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
        assert!(
            service
                .driver_step(&[], &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
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
        assert!(
            service
                .driver_step(&threads, &[], None, &no_caps())
                .unwrap()
                .is_none()
        );
        // 会话死了 → 名额归还，第二张起得来。
        let threads = vec![facts("thread-1", false, false)];
        assert!(
            wait_until(|| service
                .driver_step(&threads, &[], None, &no_caps())
                .unwrap()
                .is_some_and(|action| matches!(
                    action,
                    DriverAction::Launch { ref card_id, .. } if card_id == "card-2"
                ))),
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
            .driver_step(&[facts("thread-1", false, true)], &[], None, &no_caps())
            .unwrap();
        assert_eq!(service.list()["cards"][0]["stale"], false);
        service
            .driver_step(&[facts("thread-1", false, false)], &[], None, &no_caps())
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
        } = service
            .driver_step(&[], &[], None, &no_caps())
            .unwrap()
            .expect("launch")
        else {
            panic!("expected a launch");
        };
        // 回报带回 thread id（launch 已把卡置成 running）。
        service
            .driver_step(
                &[facts("thread-1", false, true)],
                &[DriverResult {
                    action_id,
                    ok: true,
                    thread_id: Some("thread-1".into()),
                    error: None,
                    error_code: None,
                }],
                None,
                &no_caps(),
            )
            .unwrap();
        assert_eq!(state_of(&service, &card_id), "running");

        // 会话空闲、模型什么都没写 → 状态不动。
        assert!(
            service
                .driver_step(&[facts("thread-1", true, true)], &[], None, &no_caps())
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
            Ok(crate::merge_lock::MergeAttempt::Granted(lock)) => {
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
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            panic!("expected a launch");
        };
        assert_eq!(state_of(&service, "card-1"), "running");

        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
            error_code: None,
        };
        assert!(
            service
                .driver_step(&busy(&["thread-1"]), &[result], None, &no_caps())
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
            panic!("expected a launch");
        };

        let result = DriverResult {
            action_id,
            ok: false,
            thread_id: None,
            error: Some("worktree not found".to_string()),
            error_code: None,
        };
        // 同一拍就把名额让出来给队里下一张：失败的启动不是死胡同。
        let next = service
            .driver_step(&[], &[result], None, &no_caps())
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
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            panic!("expected a launch");
        };

        let result = DriverResult {
            action_id,
            ok: false,
            thread_id: None,
            error: None,
            error_code: None,
        };
        service
            .driver_step(&[], &[result], None, &no_caps())
            .unwrap();

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
        let DriverAction::Launch { action_id, .. } = launched(&service) else {
            panic!("expected a launch");
        };

        let result = DriverResult {
            action_id,
            ok: true,
            thread_id: Some("thread-1".to_string()),
            error: None,
            error_code: None,
        };
        assert!(
            service
                .driver_step(
                    &busy(&["thread-1"]),
                    std::slice::from_ref(&result),
                    None,
                    &no_caps()
                )
                .unwrap()
                .is_none()
        );
        assert_eq!(state_of(&service, "card-1"), "running");
        // 重复投递：不报错、不改状态、不多占一个名额。
        assert!(
            service
                .driver_step(
                    &busy(&["thread-1"]),
                    std::slice::from_ref(&result),
                    None,
                    &no_caps()
                )
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
    /// 同一份公共夹具**，位于 `plugins/contracts/fixtures/`）。
    ///
    /// 这里钉住插件这一侧的两件事：① `DriverAction::to_json` 交出的形状与契约字面量
    /// **按值**相等（键序不是契约，故比较解析后的 `Value`）；② 契约里的 `results[]`
    /// 条目能反序列化成 `DriverResult`。插件若改字段名，这两条就红——否则宿主
    /// `parse_action` 会悄悄回 `None`，或插件的 `DriverResult` 反序列化失败。
    const DRIVER_CONTRACT: &str =
        include_str!("../../../../contracts/fixtures/driver-protocol.json");

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
            title: "item · a.spec".to_string(),
            objective: "do the item".to_string(),
            driver_project: "/proj".to_string(),
            driver_ref: "card-1".to_string(),
            env: vec![("PLUGIN_ITEM".to_string(), "card-1".to_string())],
        };
        // 公共夹具只冻结**形状**，取值是通用示例；插件产出的是自己的业务取值
        // （标题、env 键名都不该被公共契约绑死）。因此比较键集合 + 值类型，
        // 字段名漂移照样会红——那才是这条契约要防的东西。
        let emitted = launch.to_json();
        assert_eq!(
            sorted_keys(&emitted),
            sorted_keys(&answers["launch"]),
            "launch 的字段名与契约必须一致"
        );
        for key in [
            "action_id",
            "kind",
            "workdir",
            "title",
            "objective",
            "driver_project",
            "driver_ref",
        ] {
            assert!(emitted[key].is_string(), "`{key}` 必须是字符串：{emitted}");
        }
        assert!(emitted["env"].is_object(), "`env` 必须是对象：{emitted}");

        // 插件不再发 `append`：合并由用户在会话里发话触发，插件只负责启动。
        assert!(
            answers.get("append").is_none(),
            "契约里不该再有 append 应答"
        );

        // 「无事可做」的契约形状：`{"action": null}`。
        assert_eq!(answers["nothing"], serde_json::json!({ "action": null }));
    }

    /// `resume_append` 的线上形状同样是契约：宿主 `parse_action` 认的就是这几个
    /// 键（`thread_id` / `workdir` / `objective` / `env`）。尤其 `card_id` 是插件
    /// 内部记账键，**绝不能**出现在 JSON 里——宿主不认识看板。
    #[test]
    fn the_driver_contract_resume_append_matches_what_the_plugin_emits() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        let expected = &contract["answers"]["resume_append"];

        let action = DriverAction::ResumeAppend {
            action_id: "act-resume".to_string(),
            card_id: "card-1".to_string(),
            thread_id: "thread-1".to_string(),
            workdir: PathBuf::from("/w/card"),
            objective: "merge authorized".to_string(),
            env: vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
        };
        assert_eq!(
            action.to_json(),
            *expected,
            "resume_append 的线上形状必须与契约逐字段一致"
        );
        assert!(
            action.to_json().get("card_id").is_none(),
            "card_id 是插件内部记账键，不能外泄给宿主：{}",
            action.to_json()
        );

        // 两端同一份 JSON：宿主的 `resume_append_is_a_distinct_action` 断言的是
        // 同一组字段值（thread_id/workdir/objective/env）。
        assert_eq!(expected["thread_id"], "thread-1");
        assert_eq!(expected["workdir"], "/w/card");
        assert_eq!(expected["objective"], "merge authorized");
    }

    /// 契约里声明的能力列表就是插件认的那几个动作名：宿主少报/多报都会让调度
    /// 做出错误决定（永远排队，或派一个执行不了的动作）。
    #[test]
    fn the_driver_contract_declares_the_actions_the_plugin_understands() {
        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        let actions = contract["request"]["capabilities"]["actions"]
            .as_array()
            .expect("契约必须钉住 capabilities.actions");
        let names: Vec<&str> = actions
            .iter()
            .map(|action| action.as_str().expect("能力名必须是字符串"))
            .collect();
        assert_eq!(names, vec!["launch", "append", "resume_append"]);

        let capabilities = caps(&names);
        assert!(capabilities.supports("resume_append"));
        assert!(capabilities.supports("launch"));
        assert!(!capabilities.supports("rebase"));
    }

    /// **缺能力就当不支持**：旧宿主不发表格时，默认值必须是空集。默认成「全都
    /// 支持」会让插件派出宿主执行不了的动作，最后卡在不确定状态。
    #[test]
    fn a_host_that_declares_no_capabilities_supports_nothing() {
        let capabilities = DriverCapabilities::default();
        assert!(capabilities.actions.is_empty());
        assert!(!capabilities.supports("launch"));
        assert!(!capabilities.supports("append"));
        assert!(!capabilities.supports("resume_append"));
    }

    /// 能力名是精确匹配：大小写或前缀不同的名字不能被当成同一个动作。
    #[test]
    fn capabilities_are_matched_exactly() {
        let capabilities = &caps(&["resume_append"]);
        assert!(capabilities.supports("resume_append"));
        assert!(!capabilities.supports("Resume_Append"));
        assert!(!capabilities.supports("resume"));
        assert!(!capabilities.supports("resume_append "));
    }

    /// 旧宿主回报的 `results[]` 没有 `error_code`：必须默认为 `None`（= 未归类），
    /// 而不是反序列化失败——回报丢了比回报形状旧要贵得多。
    #[test]
    fn an_old_result_without_error_code_defaults_to_none() {
        let old = serde_json::json!({
            "action_id": "act-1",
            "ok": false,
            "thread_id": null,
            "error": "boom"
        });
        let result: DriverResult = serde_json::from_value(old).expect("旧形状必须仍可解析");
        assert_eq!(result.error_code, None);

        let contract: Value = serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON");
        let failure: DriverResult =
            serde_json::from_value(contract["results"]["failure"].clone()).expect("契约失败样例");
        assert_eq!(failure.error_code.as_deref(), Some("busy"));
    }

    /// `capabilities` 真的从 `driver/next` 的参数进了调度：解析出的集合与参数
    /// 一致，缺字段时为空集（不是「全都支持」）。
    #[test]
    fn driver_next_reads_capabilities_from_the_request() {
        let params = serde_json::json!({
            "threads": [], "results": [], "sessions": { "running": 0 },
            "capabilities": { "actions": ["launch", "resume_append"] }
        });
        let capabilities = crate::dispatch::parse_capabilities(&params);
        assert_eq!(capabilities.actions, vec!["launch", "resume_append"]);
        assert!(capabilities.supports("resume_append"));

        let old = serde_json::json!({ "threads": [], "results": [] });
        let capabilities = crate::dispatch::parse_capabilities(&old);
        assert!(capabilities.actions.is_empty(), "缺字段必须按不支持处理");
    }

    /// 一个 JSON 对象的键，排序后返回：键序不是契约，键集合是。
    fn sorted_keys(value: &Value) -> Vec<String> {
        let mut keys: Vec<String> = value
            .as_object()
            .expect("契约比较的对象")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    /// 把宿主的事务日志写进实例根，形状与 `JournalStore` 落盘的一致。
    fn write_journal(home: &Path, phase: &str) {
        let path = crate::drain::journal_path(home);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "transactionId": "tx-00000000-aaaa",
                "plugin": "superpowers-kanban",
                "action": "update",
                "phase": phase,
                "target": {
                    "release": {
                        "plugin": "superpowers-kanban",
                        "version": "0.1.0",
                        "target": "darwin-aarch64",
                        "sha256": "0".repeat(64),
                    },
                    "activation": 1,
                },
                "blocking": {"projects": [], "turns": []},
            }))
            .unwrap(),
        )
        .unwrap();
    }

    /// 把声明的上游技能都装进实例根，让自动任务闸门放行。
    fn make_ready(instance_home: &Path) {
        for name in crate::readiness::declared_workflows(include_str!("../../../plugin.json"))
            .unwrap()
            .iter()
            .map(|declared| declared.name.clone())
        {
            let skill = instance_home.join("skills").join(&name);
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        }
    }

    #[test]
    fn a_missing_upstream_skill_stops_new_launches_but_not_the_board() {
        // 设计 §7.2：缺依赖不启动自动任务，但允许查看与收尾。
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        // 这个装置的实例根是空的：声明的上游技能一个都不在。
        let service = service_with_missing_dependencies(&project);

        let blocked = service.launch_blocked_reason().expect("a reason");
        assert!(blocked.contains("brainstorming"), "{blocked}");
        assert!(
            service.next_launch(1, at()).unwrap().is_none(),
            "no card may be claimed while an upstream dependency is missing"
        );
        assert!(
            service.driver_step(&[], &[], Some(0)).unwrap().is_none(),
            "the tick must yield no launch either"
        );
        // 看板本身仍可读，且卡片状态没被改动。
        assert_eq!(state_of(&service, "card-1"), "queued");
        let listed = service.list();
        assert!(listed["launch_blocked"].is_string());
        assert_eq!(listed["dependencies"][0]["name"], "brainstorming");
        assert_eq!(
            listed["dependencies"][0]["found_at"],
            serde_json::Value::Null
        );
    }

    #[test]
    fn a_ready_installation_launches_normally() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);

        assert!(service.launch_blocked_reason().is_none());
        assert!(service.next_launch(1, at()).unwrap().is_some());
    }

    #[test]
    fn a_draining_host_stops_new_launches_but_lets_a_running_card_finish() {
        // 排空的死锁点：旧 turn 必须还能写状态、放名额，否则屏障等不到零。
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        // 排空前就已经在跑的一张卡。
        let claim = service.next_launch(1, at()).unwrap().expect("the head");
        service.mark_running(&claim.card_id, "thread-1").unwrap();

        write_journal(
            &service.instance_home.clone().expect("fixture sets it"),
            "draining",
        );
        assert!(!service.drain().allows_new_work());
        assert!(service.launch_blocked_reason().is_some());

        // 新卡不被认领。
        service.enqueue_card_for_test("card-2");
        assert!(
            service
                .driver_step(&busy(&["thread-1"]), &[], Some(0))
                .unwrap()
                .is_none(),
            "draining must stop new launches"
        );
        assert_eq!(state_of(&service, "card-2"), "queued");

        // 但旧卡的收尾照常：状态写 + 名额释放都成功。
        service
            .set_state("card-1", CardState::AwaitingMerge, Some("done"))
            .expect("a draining plugin must let the running card report its state");
        assert_eq!(state_of(&service, "card-1"), "awaiting_merge");
        service
            .merge_finish("card-1")
            .expect("releasing the merge slot must work while draining");
    }

    #[test]
    fn a_drain_that_has_finished_does_not_block_forever() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        write_journal(
            &service.instance_home.clone().expect("fixture sets it"),
            "done",
        );
        assert!(service.launch_blocked_reason().is_none());
    }

    #[test]
    fn the_board_says_which_lifecycle_phase_is_holding_it() {
        let dir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(dir.path());
        let service = service_with_card(&project);
        write_journal(
            &service.instance_home.clone().expect("fixture sets it"),
            "backing_up",
        );
        let listed = service.list();
        assert_eq!(listed["drain"]["state"], "draining");
        assert_eq!(listed["drain"]["phase"], "backing_up");
    }

    #[test]
    fn a_plugin_may_not_override_a_reserved_environment_variable() {
        // 宿主侧 `managed::build_command` 已经挡一次；插件侧再挡一次，因为插件
        // 可能不经过那个 launcher 而被直接执行。
        let rejected = crate::managed::rejected_env([
            ("SUPERPOWERS_KANBAN_CARD", "card-1"),
            ("YI_AGENT_HOME", "/somewhere/else"),
        ]);
        assert_eq!(rejected, vec!["YI_AGENT_HOME".to_string()]);
        assert!(
            crate::managed::rejected_env([("SUPERPOWERS_KANBAN_CARD", "card-1")]).is_empty(),
            "non-reserved keys pass through"
        );
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// 受管实例：登记归宿主，插件一个字节都不写宿主登记文件。
    #[test]
    fn a_managed_install_does_not_write_the_hosts_registry() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let service = BoardService::new(
            dir.path().join(".yi-agent/superpowers-kanban"),
            project.clone(),
            Some(dir.path().join("home")),
        )
        .with_managed(true);

        service.lifecycle_create(&project).unwrap();

        // 自己的状态目录照建——那是插件的数据目录，不归宿主。
        assert!(project.join(".yi-agent/superpowers-kanban").is_dir());
        // 宿主登记文件**不存在**：设计 §7.1 说这由宿主管。
        assert!(
            !registry(dir.path()).exists(),
            "a managed plugin must not write the host's resident registry"
        );
    }

    /// 受管实例的 remove 也不去动宿主登记（否则会把宿主别的条目一起重写）。
    #[test]
    fn a_managed_install_leaves_the_hosts_registry_alone_on_remove() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        // 先由"宿主"写一条别的项目的登记，再让受管插件 remove。
        std::fs::create_dir_all(dir.path().join("home/.yi-agent")).unwrap();
        let other = dir.path().join("other");
        resident::require(&dir.path().join("home/.yi-agent"), &other, "someone-else").unwrap();
        let before = std::fs::read_to_string(registry(dir.path())).unwrap();

        let service = BoardService::new(
            dir.path().join(".yi-agent/superpowers-kanban"),
            project.clone(),
            Some(dir.path().join("home")),
        )
        .with_managed(true);
        service.lifecycle_remove(&project).unwrap();

        let after = std::fs::read_to_string(registry(dir.path())).unwrap();
        assert_eq!(before, after, "the host's registry must be byte-identical");
    }

    /// standalone（无宿主）仍写共享登记：那时没有宿主可以代劳。
    #[test]
    fn a_standalone_install_still_registers_as_resident() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let service = BoardService::new(
            dir.path().join(".yi-agent/superpowers-kanban"),
            project.clone(),
            Some(dir.path().join("home")),
        );

        service.lifecycle_create(&project).unwrap();

        assert!(registry(dir.path()).exists());
    }

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
