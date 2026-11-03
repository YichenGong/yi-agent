//! 通用会话驱动：把「该起/该继续哪个会话」交给插件决定。
//!
//! 本模块不认识看板、卡片、合并。它只认识两件事：宿主能提供的通用事实
//! （某个会话闲不闲），与宿主能执行的通用动作（新建会话 / 追加一轮 / 恢复原会话
//! 后追加）。
//!
//! 主循环里的 `driver_tick` 是消费方：它按通用常驻登记表遍历项目、用插件清单
//! 发现「谁要驱动会话」（`driver_plugins`），再对每个 (项目, 插件) 组合拉一拍
//! （`driver_step`），由 `SessionLauncher` 的真实现执行动作。

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::protocol::ThreadStatus;
use crate::session::ThreadSession;

/// 本宿主能执行的通用动作（`driver/next` 请求的 `capabilities.actions`）。
///
/// 这是跨进程契约的一半：插件只在调度阶段读它，协议层不猜卡片语义。新增动作时
/// 只在这里登记一次，两端的契约测试会把名字钉住。
pub(crate) const SUPPORTED_ACTIONS: &[&str] = &["launch", "append", "resume_append"];

/// 宿主能执行的通用动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DriverActionKind {
    /// 在一个 workdir 里新建会话，并投递 `objective` 作为首轮。
    ///
    /// `driver_project` / `driver_ref` 是这个新会话的**归属**，由插件在动作里给出
    /// （宿主不知道也不该知道它是什么含义），宿主只负责落进会话 meta：
    /// - `driver_project` 是归属项目根：`thread/listAll` 只有靠它才能把会话折进
    ///   项目组（否则落进 `own` 桶）；
    /// - `driver_ref` 是驱动方给的不透明句柄，桌面侧栏据此把会话归入「驱动会话」
    ///   小节。
    ///
    /// 两者都是 `Option`：旧插件不发（前向兼容），此时行为与改动前一致
    /// （会话不进项目组），绝不因为缺字段把整条动作判废。
    Launch {
        workdir: String,
        title: String,
        objective: String,
        driver_project: Option<String>,
        driver_ref: Option<String>,
        /// 插件要注入本会话 shell 的环境变量（键值由插件定义，宿主只透传）。
        ///
        /// 为什么让插件给而不是宿主写死：环境变量的键名（例如「这张卡是哪张」）
        /// 是插件自己的概念。宿主一旦认识某个具体键名，就又变成「宿主懂看板」，
        /// 而这条边界正是本次改造要守住的。旧插件不发时为无。
        env: Vec<(String, String)>,
    },
    /// 向已有会话追加一轮。
    Append {
        thread_id: String,
        objective: String,
    },
    /// 恢复一个**本进程看不见**的原会话，然后向它追加一轮。
    ///
    /// 与 `Append` 的区别是「先恢复、后投递」：宿主必须按 `workdir` 精确找到该
    /// 会话的持久化历史，用**同一个** thread id 重新装配并登记，再走同一条投递
    /// 内核。它不允许降级成 `Launch`（那样会切断上下文，而上下文正是这条动作
    /// 存在的理由），也不允许落到 `cfg.cwd` 或别的目录里去找。
    ///
    /// 恢复失败时必须明确回报 `restore_failed`——调用方据此阻塞，而不是当作
    /// 「这一拍没事可做」继续往后推。
    ResumeAppend {
        thread_id: String,
        /// 原会话的精确定位目录（**不是**这次动作的目标目录）。
        workdir: String,
        objective: String,
        /// 冷装配时要注入工具进程的环境变量；热会话不重建工具，故不生效。
        env: Vec<(String, String)>,
    },
}

/// 动作失败时被宿主**分类**过的错误码。
///
/// 它存在的唯一理由：插件要分清「这次 objective 确定没进 prompt 通道」与
/// 「不知道有没有送到」。前者可以安全地退队/转 NeedsYou，后者必须保守阻塞。
/// 因此宿主不能靠错误文本猜——分类必须由抛出方显式给出，文本只给人看。
///
/// 拼写与 spec §5.2 及两端契约逐字对齐（`driver-protocol.json`）。
///
/// 契约本身（错误码拼写）在 Task 1 就钉住；驱动侧的内核
/// (`server/driver_resume.rs`) 现在按这些分类构造失败，插件据此分流。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriverErrorCode {
    /// 会话正忙（有 turn 在跑）。零副作用：没有取消、没有抢占、没有投递。
    Busy,
    /// 要续跑的会话不在本进程、也无法按要求恢复。
    ThreadNotFound,
    /// 动作给的 workdir 不存在或不可用。
    WorkdirMissing,
    /// 会话恢复/装配失败（历史损坏、partial 升格失败、锁拿不到……）。
    RestoreFailed,
    /// 投递（prepare/send）阶段失败，或准备阶段出现其他错误。
    DeliveryFailed,
}

impl DriverErrorCode {
    /// 线上的字符串拼写（spec §5.2 / 契约 `results[].error_code`）。
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::ThreadNotFound => "thread_not_found",
            Self::WorkdirMissing => "workdir_missing",
            Self::RestoreFailed => "restore_failed",
            Self::DeliveryFailed => "delivery_failed",
        }
    }
}

/// 带分类的失败。用 `anyhow` 承载、由调用方 `downcast_ref` 取回分类，
/// 而不是把错误塞成字符串再匹配前缀——字符串是给人看的，随时会改。
#[derive(Debug, Clone)]
pub(crate) struct DriverActionFailure {
    pub code: DriverErrorCode,
    pub message: String,
}

impl std::fmt::Display for DriverActionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DriverActionFailure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverAction {
    pub action_id: String,
    pub kind: DriverActionKind,
}

/// 解析插件的 `driver/next` 应答。
///
/// `{"action": null}`、缺 `action_id`、或未知 `kind` 一律返回 `None`：调度器
/// 前向兼容——插件日后新增动作类型时，旧宿主只是「这一拍什么都不做」。
pub(crate) fn parse_action(value: &Value) -> Option<DriverAction> {
    let action = value.get("action").unwrap_or(value);
    let action_id = action.get("action_id")?.as_str()?.to_string();
    let kind = action.get("kind")?.as_str()?;
    let kind = match kind {
        "launch" => DriverActionKind::Launch {
            workdir: action.get("workdir")?.as_str()?.to_string(),
            title: action
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
            driver_project: action
                .get("driver_project")
                .and_then(Value::as_str)
                .map(str::to_string),
            driver_ref: action
                .get("driver_ref")
                .and_then(Value::as_str)
                .map(str::to_string),
            env: parse_env(action),
        },
        "append" => DriverActionKind::Append {
            thread_id: action.get("thread_id")?.as_str()?.to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
        },
        "resume_append" => DriverActionKind::ResumeAppend {
            thread_id: action.get("thread_id")?.as_str()?.to_string(),
            workdir: action.get("workdir")?.as_str()?.to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
            env: parse_env(action),
        },
        _ => return None,
    };
    Some(DriverAction { action_id, kind })
}

/// `env` 的透传解析：插件给的键值对原样交给宿主，宿主不认识任何键。
///
/// 缺字段 / 非对象 / 非字符串值一律忽略（旧插件不发 `env`，此时为空集；
/// 宁可少注入一个变量，也不要因为一个值类型不对而把整条动作判废）。
fn parse_env(action: &Value) -> Vec<(String, String)> {
    action
        .get("env")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 插件有没有**明确**答「这一拍无事可做」（契约形状 `{"action": null}`）。
///
/// 它必须与「解析不出动作」分得开：`parse_action` 对两者都返回 `None`，可它们的
/// 日志价值完全相反——前者是正常空闲（每 3s 都会发生，记下来只会淹没日志），
/// 后者是真的断链（字段漂移、插件比宿主新、应答不是约定形状）。没有这个判据，
/// 宿主只能「`None` 就当没事」，那正是静默停摆的现场。
///
/// 只认契约形状（见 `contract/driver-protocol.json` 的 `answers.nothing`）：
/// 裸 `null`、`{}`、或 `action` 存在但非 null 都不是约定形状，一律算「说不清」，
/// 由调用方记一行。
pub(crate) fn is_explicit_no_action(value: &Value) -> bool {
    matches!(value.get("action"), Some(Value::Null))
}

/// 宿主为本插件启动过、且插件还没见过「它已经死了」的会话。
///
/// `alive` 由主循环每拍按 `threads` 表刷新（成员即活着）。它必须与 `idle` 分开：
/// `idle` 说「此刻没有 turn 在跑」，`alive` 说「这个会话还在」。插件用后者判
/// 并发名额与 stale，用前者判「这一轮跑完了没有」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverThread {
    pub thread_id: String,
    pub alive: bool,
}

/// 一次动作的结果，下一拍回传插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionResult {
    pub action_id: String,
    pub ok: bool,
    pub thread_id: Option<String>,
    pub error: Option<String>,
    /// 失败时宿主给出的**分类**（见 `DriverErrorCode`）。
    ///
    /// `None` 表示「宿主自己也没归类」——插件必须按不确定处理，不能拿错误文本
    /// 猜「大概是 busy 吧」然后安全重试。成功的动作不带这个字段。
    pub error_code: Option<String>,
}

/// 一个 (项目, 插件) 组合的驱动记账，随主循环存活、跨 tick 累积。
///
/// 之所以要持久：`results` 是「上一拍做完、本拍要回报插件」的结果（`driver_step`
/// 送出去之后才清空），`threads` 是宿主为该插件起过的会话——两者都必须比一次
/// tick 活得久，否则插件永远收不到 `thread_id`、也无从判断某个会话是否空闲。
///
/// 它不是共享状态：只被主循环的 tick 分支按顺序 `&mut` 借用（与 `threads` 同属
/// 主循环局部量），故不需要加锁，也没有任何任务并发访问它。
#[derive(Default)]
pub(crate) struct DriverState {
    pub threads: Vec<DriverThread>,
    pub results: Vec<ActionResult>,
}

/// 执行动作的副作用。真实现复用 `start_thread_core` / `prepare_turn_core` 的
/// 无响应帧路径；测试用替身驱动纯逻辑。
pub(crate) trait SessionLauncher {
    async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String>;
}

/// 一个会话此刻算不算「运行中」。
///
/// 口径由用户定义：正在调模型或正在执行进程算运行；**等待人决策不算**
/// （`AwaitingApproval`），闲置也不算。`status` 与 `active_turn_id` 取自宿主
/// 同一份 `ThreadSession`，与 `driver_tick` 采 idle 快照的判据同源。
///
/// 两个「不一致」的窗口都朝保守方向取（宁可少起一个会话，也不超发）：
/// 状态已 `Running` 但 turn id 尚未登记，或 turn id 还在、状态已先翻 `Idle`。
pub(crate) fn is_a_running_session(status: ThreadStatus, active_turn: Option<&str>) -> bool {
    if matches!(status, ThreadStatus::AwaitingApproval) {
        return false;
    }
    active_turn.is_some() || matches!(status, ThreadStatus::Running)
}

/// 宿主热会话表里的一条事实，与插件侧的共读契约同名同形。
///
/// 只有 ID 与「此刻是否空闲」：宿主对看板没有归属信息，插件也不该从这份快照
/// 推断某会话属于哪个插件。这里刻意不复用 `DriverThread`——后者是「宿主为该
/// 插件起过哪些会话」的记账，与「本进程此刻还加载着谁」是两件事。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedThreadFacts {
    pub thread_id: String,
    pub idle: bool,
}

/// 本进程此刻加载着的**全部**会话快照（按 ID 排序，便于断言与比对）。
///
/// 「完整」是这条协议的全部价值：插件据此区分「宿主明确说它不在」与「宿主没
/// 能力回答」。所以这里**只能**读内存里的会话表，不读磁盘、不读插件元数据，
/// 也不按任何归属过滤——少报一条就等于替插件伪造一次「没加载」。
///
/// `idle` 的口径与 `driver_tick` 的旧 idle 快照逐字一致：`ThreadStatus::Idle`
/// 且没有活跃 turn。`AwaitingApproval` 与 `Running` 都不算 idle。
pub(crate) fn loaded_threads_snapshot(
    threads: &HashMap<String, ThreadSession>,
) -> Vec<LoadedThreadFacts> {
    let mut out: Vec<LoadedThreadFacts> = threads
        .iter()
        .map(|(thread_id, session)| {
            let status = *session
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            LoadedThreadFacts {
                thread_id: thread_id.clone(),
                idle: matches!(status, ThreadStatus::Idle) && session.active_turn_id.is_none(),
            }
        })
        .collect();
    out.sort_by(|left, right| left.thread_id.cmp(&right.thread_id));
    out
}

/// 把快照编成 `driver/next` 的 `loaded_threads` 参数。
pub(crate) fn loaded_threads_payload(threads: &[LoadedThreadFacts]) -> Value {
    Value::Array(
        threads
            .iter()
            .map(|fact| {
                serde_json::json!({
                    "thread_id": fact.thread_id,
                    "idle": fact.idle,
                })
            })
            .collect(),
    )
}

/// 本进程此刻有几个会话在跑。
///
/// 宿主不认识看板：**所有归属**的会话都在内（手工开的、别的插件起的、看板起的）。
/// 这正是「限的是总流量」所需的口径——插件只看看板自己的卡片，看不到这些会话。
pub(crate) fn running_sessions(threads: &HashMap<String, ThreadSession>) -> u32 {
    threads
        .values()
        .filter(|session| {
            let status = *session
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            is_a_running_session(status, session.active_turn_id.as_deref())
        })
        .count() as u32
}

/// 把宿主持有的会话事实编成 `driver/next` 的 `threads` 参数。
pub(crate) fn threads_payload(
    threads: &[DriverThread],
    idle: &(dyn Fn(&str) -> bool + Sync),
) -> Value {
    Value::Array(
        threads
            .iter()
            .map(|thread| {
                serde_json::json!({
                    "thread_id": thread.thread_id,
                    "idle": idle(&thread.thread_id),
                    "alive": thread.alive,
                })
            })
            .collect(),
    )
}

/// 拉一拍：问插件「下一步做什么」，执行至多一个动作，返回其结果。
///
/// `idle` 是宿主对「某个会话此刻闲不闲」的回答（主循环按 tick 采一次快照，
/// 见 `driver_tick`）：插件靠它判断某一轮跑完了没有。它是宿主唯一能提供的
/// 通用事实，故由调用方传入而非在本函数里读取任何 thread 表。
///
/// `results` 里是上一拍尚未回报的结果；本拍把它们上送，**送达之后**才清空
/// （`query` 失败时保留，下一拍重发）。
pub(crate) async fn driver_step<L: SessionLauncher>(
    query: &(dyn Fn(&Value) -> Result<Value, String> + Sync),
    threads: &mut Vec<DriverThread>,
    results: &mut Vec<ActionResult>,
    launcher: &mut L,
    idle: &(dyn Fn(&str) -> bool + Sync),
    sessions_running: u32,
    // 本进程此刻加载着的全部会话。与 `threads`（本插件起过哪些）是两件事：
    // 前者回答「插件那张卡绑定的会话现在还加载着吗」，后者只能回答「我派过的
    // 会话还活着吗」。宿主重启/会话被别处关闭后，前者才是唯一可信的来源。
    loaded_threads: &[LoadedThreadFacts],
) -> Option<ActionResult> {
    let results_payload = Value::Array(
        results
            .iter()
            .map(|result| {
                serde_json::json!({
                    "action_id": result.action_id,
                    "ok": result.ok,
                    "thread_id": result.thread_id,
                    "error": result.error,
                    "error_code": result.error_code,
                })
            })
            .collect(),
    );
    let params = serde_json::json!({
        "threads": threads_payload(threads, idle),
        // 完整热表快照。它是可选的：旧插件不读就忽略；新插件凭它区分
        // 「明确不在」与「宿主没能力回答」。空数组是**有信息**的答案（热表
        // 确实为空），与「不带这个字段」不是一回事。
        "loaded_threads": loaded_threads_payload(loaded_threads),
        "results": results_payload,
        // 本进程此刻有几个会话在跑（口径见 `is_a_running_session`）。插件拿它
        // 算「全机余量」：限的是总流量，看板外的会话与看板会话吃同一份额度。
        "sessions": { "running": sessions_running },
        // 本宿主能执行哪些通用动作。插件据此决定「这张卡该不该现在派」：
        // 缺这一块（旧宿主）按**一个都不会**处理，插件保留排队并说明原因，
        // 绝不猜「大概支持吧」然后交出一个自己执行不了的动作。
        "capabilities": { "actions": SUPPORTED_ACTIONS },
    });
    let answer = match query(&params) {
        Ok(answer) => answer,
        // 本拍无法推进：动作留给下一拍重试，待回报的 results 保留不丢。
        Err(_) => return None,
    };
    // 只在成功上送之后消费：`query` 失败时上面已提前返回，`results` 原样留给
    // 下一拍重发。注意清空必须早于下面的 `?`——答案虽为空/未知动作，results 却
    // 已经送达插件，绝不能再重发一次。
    results.clear();

    let action = parse_action(&answer)?;

    match launcher.launch(&action).await {
        Ok(thread_id) => {
            // `Launch` 与 `ResumeAppend` 都产生/确认一个本进程看得见的会话：
            // 前者是新起的，后者是刚被恢复的。两者都要登记进 `threads`，
            // 否则插件永远收不到「这个会话还活着」的观测，也拿不到 idle 信号。
            // `Append` 不登记：它只续跑**已经登记过**的会话，多推一次纯属噪声。
            if matches!(
                action.kind,
                DriverActionKind::Launch { .. } | DriverActionKind::ResumeAppend { .. }
            ) {
                // 去重：同一 thread 反复出现时只刷新 `alive`，不能让插件看到
                // 两条同 id 的观测（那会让它以为有两个会话，算错名额）。
                if let Some(existing) = threads
                    .iter_mut()
                    .find(|thread| thread.thread_id == thread_id)
                {
                    existing.alive = true;
                } else {
                    threads.push(DriverThread {
                        thread_id: thread_id.clone(),
                        // 刚起的/刚恢复的会话当然活着；此后每拍由主循环按
                        // `threads` 表刷新。
                        alive: true,
                    });
                }
            }
            Some(ActionResult {
                action_id: action.action_id,
                ok: true,
                thread_id: Some(thread_id),
                error: None,
                error_code: None,
            })
        }
        // 只有宿主**显式分类**过的失败才带 `error_code`；其他 anyhow 错误一律
        // `None`（插件按不确定处理）。不要在这里按错误文本补分类——那会把
        // 「不知道送没送到」伪装成「确定没送到」，正是这次要防的误重试。
        Err(error) => {
            let failure = error.downcast_ref::<DriverActionFailure>();
            Some(ActionResult {
                action_id: action.action_id,
                ok: false,
                thread_id: None,
                error: Some(error.to_string()),
                error_code: failure.map(|failure| failure.code.as_str().to_string()),
            })
        }
    }
}

/// 声明了 `session_driver` 的插件名（按插件名排序，稳定）。
///
/// 读的是**通用**清单目录；宿主不认识任何具体插件。
pub(crate) fn driver_plugins(scope: &Path) -> Vec<String> {
    let dir = scope.join(".yi-agent").join("supervisors");
    let mut names: Vec<String> = yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .filter(|manifest| manifest.session_driver)
        .map(|manifest| manifest.name)
        .collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_running_turn_counts_as_one_running_session() {
        assert!(is_a_running_session(ThreadStatus::Running, Some("t1")));
    }

    #[test]
    fn a_session_waiting_for_human_approval_does_not_count() {
        assert!(
            !is_a_running_session(ThreadStatus::AwaitingApproval, Some("t1")),
            "等决策不是运行"
        );
    }

    #[test]
    fn an_idle_session_does_not_count() {
        assert!(!is_a_running_session(ThreadStatus::Idle, None));
    }

    #[test]
    fn a_status_of_running_without_a_live_turn_still_counts() {
        // 状态已推进到 Running 但 turn id 尚未登记的窗口期：保守按在跑算，
        // 宁可少起一个会话，也不超发。
        assert!(is_a_running_session(ThreadStatus::Running, None));
    }

    #[test]
    fn an_idle_status_with_a_leftover_turn_id_still_counts() {
        // 对称的保守方向：turn 还在就不信「空闲」。
        assert!(is_a_running_session(ThreadStatus::Idle, Some("t1")));
    }

    #[test]
    fn a_null_answer_yields_no_action() {
        assert!(parse_action(&json!({ "action": null })).is_none());
    }

    #[test]
    fn a_launch_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it",
            "driver_project": "/proj", "driver_ref": "card-1"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
                driver_project,
                driver_ref,
                env,
            } => {
                assert_eq!(workdir, "/w");
                assert_eq!(title, "T");
                assert_eq!(objective, "do it");
                assert_eq!(driver_project.as_deref(), Some("/proj"));
                assert_eq!(driver_ref.as_deref(), Some("card-1"));
                assert!(env.is_empty(), "这条字面量没带 env");
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    /// 归属字段是**后加**的：旧插件（升级前）不发这两个键，宿主必须照旧起会话，
    /// 只是落在「未分组」——不能因为缺字段就整条动作判废。这正是前向兼容面。
    #[test]
    fn a_launch_answer_without_ownership_still_parses() {
        let value = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Launch {
                driver_project,
                driver_ref,
                env,
                ..
            } => {
                assert_eq!(driver_project, None);
                assert_eq!(driver_ref, None);
                assert!(env.is_empty(), "旧插件不发 env，按空集处理");
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn an_append_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a2", "kind": "append",
            "thread_id": "thread-1", "objective": "merge it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Append {
                thread_id,
                objective,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(objective, "merge it");
            }
            other => panic!("expected append, got {other:?}"),
        }
    }

    /// `resume_append` 是与 `append` 不同的动作：`append` 只续跑本进程**看得见**
    /// 的会话，`resume_append` 要求宿主先按精确 workdir 恢复原会话（同 ID）再投递。
    /// 两者若被解析成同一个 variant，插件就无法表达「冷会话也必须回到原 thread」，
    /// 调度会悄悄退化成起一个新会话。
    #[test]
    fn resume_append_is_a_distinct_action() {
        let value = serde_json::json!({
            "action_id": "act-resume", "kind": "resume_append",
            "thread_id": "thread-original", "workdir": "/w/card",
            "objective": "merge authorized", "env": {"CARD": "card-1"}
        });
        let action = parse_action(&value).expect("new action must parse");
        assert!(matches!(action.kind, DriverActionKind::ResumeAppend {
            thread_id, workdir, objective, env
        } if thread_id == "thread-original" && workdir == "/w/card"
            && objective == "merge authorized"
            && env == vec![("CARD".into(), "card-1".into())]));
    }

    #[test]
    fn an_unknown_kind_is_ignored_rather_than_panicking() {
        assert!(parse_action(&json!({ "action_id": "a3", "kind": "reboot" })).is_none());
    }

    #[test]
    fn a_missing_action_id_is_ignored() {
        assert!(parse_action(&json!({ "kind": "launch", "workdir": "/w" })).is_none());
    }

    /// 「插件明确答了无事可做」必须与「宿主解析不出动作」分得开：前者是正常空闲，
    /// 后者要留一行日志（否则每 3s 一次的断链在日志里与空闲同型）。
    #[test]
    fn only_the_contract_null_answer_counts_as_deliberately_idle() {
        assert!(is_explicit_no_action(&json!({ "action": null })));
        // 解析不出动作的几种「说不清」形状都不是约定的空闲形状。
        for value in [
            json!(null),
            json!({}),
            json!({ "kind": "reboot", "action_id": "a1" }),
            json!({ "action": { "kind": "reboot" } }),
        ] {
            assert!(
                !is_explicit_no_action(&value),
                "{value} 不是约定的 `{{\"action\": null}}`，应当留痕"
            );
        }
    }

    #[derive(Default)]
    struct FakeLauncher {
        seen: Vec<DriverAction>,
        thread_id: String,
        fail: Option<String>,
        /// 分类失败：用 `DriverActionFailure` 承载，模拟真实 launcher 的用法。
        fail_with: Option<DriverActionFailure>,
    }

    impl FakeLauncher {
        fn failing_with(code: DriverErrorCode, message: &str) -> Self {
            Self {
                fail_with: Some(DriverActionFailure {
                    code,
                    message: message.to_string(),
                }),
                ..Default::default()
            }
        }
    }

    impl SessionLauncher for FakeLauncher {
        async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String> {
            self.seen.push(action.clone());
            if let Some(failure) = &self.fail_with {
                return Err(anyhow::Error::new(failure.clone()));
            }
            match &self.fail {
                Some(detail) => Err(anyhow::anyhow!(detail.clone())),
                None => Ok(self.thread_id.clone()),
            }
        }
    }

    fn answering(reply: Value) -> impl Fn(&Value) -> Result<Value, String> {
        move |_params: &Value| Ok(reply.clone())
    }

    /// 测试用的空闲快照：默认「谁都不闲」。
    ///
    /// 与接线前 `driver_step` 内部写死的 `&|_| false` 同义，故这些既有测试
    /// 断言的仍是同一套行为；真的空闲来源由主循环的 `driver_tick` 采集。
    fn nobody_idle() -> impl Fn(&str) -> bool {
        |_| false
    }

    #[tokio::test]
    async fn a_launch_action_is_executed_and_reported_ok() {
        let reply = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-9".into(),
            ..Default::default()
        };

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        let outcome = outcome.unwrap();
        assert_eq!(outcome.action_id, "a1");
        assert!(outcome.ok);
        assert_eq!(launcher.seen.len(), 1, "the launcher runs exactly once");
    }

    #[tokio::test]
    async fn a_failed_launch_is_reported_with_the_error_and_no_thread() {
        let reply = json!({
            "action_id": "a2", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            fail: Some("worktree gone".into()),
            ..Default::default()
        };

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .unwrap();

        assert!(!outcome.ok);
        assert_eq!(outcome.error.as_deref(), Some("worktree gone"));
        assert!(outcome.thread_id.is_none());
    }

    #[tokio::test]
    async fn a_null_answer_performs_no_side_effect() {
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(
            &answering(json!({ "action": null })),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        assert!(outcome.is_none());
        assert!(launcher.seen.is_empty());
    }

    #[tokio::test]
    async fn pending_results_are_sent_on_the_next_call_and_then_cleared() {
        let reply = json!({ "action": null });
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: false,
            thread_id: None,
            error: Some("boom".into()),
            error_code: Some("delivery_failed".into()),
        }];
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(reply.clone())
        };
        let mut launcher = FakeLauncher::default();

        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent[0]["results"][0]["action_id"], "a0");
        assert_eq!(sent[0]["results"][0]["ok"], false);
        assert!(results.is_empty(), "consumed results are cleared");
    }

    #[tokio::test]
    async fn pending_results_survive_a_failed_query() {
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: true,
            thread_id: Some("thread-1".into()),
            error: None,
            error_code: None,
        }];
        let query = |_params: &Value| Err("daemon down".to_string());
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        assert!(outcome.is_none(), "a failed query cannot advance the tick");
        assert!(launcher.seen.is_empty(), "no action is executed");
        assert_eq!(
            results.len(),
            1,
            "results that were never delivered must be kept for the next tick"
        );
        assert_eq!(results[0].action_id, "a0");
    }

    #[tokio::test]
    async fn a_delivered_null_answer_still_consumes_the_results() {
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: false,
            thread_id: None,
            error: Some("boom".into()),
            error_code: Some("delivery_failed".into()),
        }];
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(
            &answering(json!({ "action": null })),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        assert!(outcome.is_none());
        assert!(
            results.is_empty(),
            "the results were delivered, so a null answer must not re-send them"
        );
    }

    /// `alive` 必须逐条进线：插件靠它计并发、判 stale。若宿主漏发或写死 `true`，
    /// 死掉的会话会永久占着名额，卡片也永远不会显示 stale。
    #[test]
    fn the_payload_reports_which_sessions_are_still_alive() {
        let threads = vec![
            DriverThread {
                thread_id: "t1".into(),
                alive: true,
            },
            DriverThread {
                thread_id: "t2".into(),
                alive: false,
            },
        ];
        let payload = threads_payload(&threads, &|_| true);
        assert_eq!(payload[0]["alive"], true);
        assert_eq!(payload[1]["alive"], false);
        assert_eq!(payload[0]["idle"], true, "idle 语义不变");
    }

    /// 完整热表快照真的上送：字段名、排序、逐条 `idle`。
    ///
    /// 这条防的是「实现了快照函数但忘了接到请求里」——那种情况下插件永远看不到
    /// `loaded_threads`，会一直按旧宿主处理，热发现永远不发生，而所有别的测试
    /// 都能通过。
    #[test]
    fn kanban_observation_loaded_snapshot_reaches_the_wire() {
        let snapshot = vec![
            LoadedThreadFacts {
                thread_id: "a-busy".into(),
                idle: false,
            },
            LoadedThreadFacts {
                thread_id: "b-idle".into(),
                idle: true,
            },
        ];
        let payload = loaded_threads_payload(&snapshot);
        assert_eq!(payload[0]["thread_id"], "a-busy");
        assert_eq!(payload[0]["idle"], false);
        assert_eq!(payload[1]["thread_id"], "b-idle");
        assert_eq!(payload[1]["idle"], true);
        // 数组顺序即快照顺序（快照函数自己按 ID 排序）。
        assert_eq!(payload.as_array().unwrap().len(), 2);
    }

    /// 快照进请求体的位置：与 `threads[]` 并列，一条不少、一条不多。
    #[tokio::test]
    async fn kanban_observation_snapshot_is_sent_with_every_tick() {
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let snapshot = vec![LoadedThreadFacts {
            thread_id: "hot".into(),
            idle: true,
        }];
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &snapshot,
        )
        .await;
        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["loaded_threads"],
            json!([{"thread_id": "hot", "idle": true}]),
            "热表快照必须进请求：{sent:?}"
        );
    }

    /// 空热表是**有信息**的答案：要发空数组，不能省掉字段。
    #[tokio::test]
    async fn kanban_observation_empty_snapshot_is_still_sent() {
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;
        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["loaded_threads"],
            json!([]),
            "「一个都没加载」必须显式给出，不能靠缺字段表达"
        );
    }

    /// 空闲快照必须真的进到 `threads[]` 里：插件唯一的「这轮跑完了」信号就是它
    /// （spec §4.6）。接线时若把参数丢掉、退回写死的 `false`，插件就永远推进不了
    /// 卡片状态，故这里钉住快照与 thread_id 的对应关系。
    #[tokio::test]
    async fn the_idle_snapshot_reaches_the_wire_per_thread() {
        let mut threads = vec![
            DriverThread {
                thread_id: "busy".into(),
                alive: true,
            },
            DriverThread {
                thread_id: "done".into(),
                alive: true,
            },
        ];
        let mut results = Vec::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        let idle = |thread_id: &str| thread_id == "done";

        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &idle,
            0,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["threads"][0]["idle"], false,
            "a busy thread must be reported busy: {sent:?}"
        );
        assert_eq!(sent[0]["threads"][0]["thread_id"], "busy");
        assert_eq!(
            sent[0]["threads"][1]["idle"], true,
            "the snapshot must answer per thread, not globally: {sent:?}"
        );
        assert_eq!(sent[0]["threads"][1]["thread_id"], "done");
    }

    /// 宿主与插件共用的 `driver/next` 线协议契约（**与插件 `service.rs` 的测试读的是
    /// 同一份文件**，两侧都用 `include_str!`，与 `plugin-socket.json` 同一套做法）。
    ///
    /// 契约此前只存在于两侧手写的 JSON 字面量里：改一侧的字段名，两边测试照样全绿，
    /// 而生产上宿主 `parse_action` 悄悄回 `None`（这一拍什么都不做）、插件的
    /// `DriverResult` 反序列化失败（卡永远停在 `launching`）。这里把两侧钉到同一份
    /// 检出文件上。
    ///
    /// `include_str!` 而非运行时读取：编译期依赖能同时钉住「文件还在不在」——契约
    /// 被改名/删除时**编译**就失败，运行时读取会静默跳过而失去意义。它只作用于
    /// `#[cfg(test)]`，`cargo build` 不受影响。
    const DRIVER_CONTRACT: &str = include_str!(
        "../../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json"
    );

    fn driver_contract() -> Value {
        serde_json::from_str(DRIVER_CONTRACT).expect("契约必须是合法 JSON")
    }

    /// 宿主上送的完整快照与契约样例**逐字对齐**：键名、条数、`idle` 取值。
    ///
    /// 两侧读同一份文件（插件侧 `driver_observation.rs` 也 `include_str!` 它），
    /// 所以这条同时钉住「宿主发的形状」与「插件期望的形状」不是各写各的。
    #[test]
    fn kanban_observation_contract_request_matches_the_host_payload() {
        let contract = driver_contract();
        let request = &contract["request"];
        let loaded = request["loaded_threads"]
            .as_array()
            .expect("契约的 request 必须带 loaded_threads 样例");
        assert!(!loaded.is_empty(), "样例不能是空的，否则形状没被钉住");

        let facts: Vec<LoadedThreadFacts> = loaded
            .iter()
            .map(|fact| LoadedThreadFacts {
                thread_id: fact["thread_id"].as_str().expect("thread_id").to_string(),
                idle: fact["idle"].as_bool().expect("idle"),
            })
            .collect();
        assert_eq!(
            loaded_threads_payload(&facts),
            Value::Array(loaded.clone()),
            "宿主编码必须与契约样例逐字一致"
        );

        // 旧宿主样例：没有 `loaded_threads`，插件读成「没能力回答」。
        assert!(
            contract["legacy_request"].get("loaded_threads").is_none(),
            "legacy_request 必须**不带**这个字段"
        );
        assert!(
            contract["legacy_request"]["threads"].is_array(),
            "legacy_request 仍要有旧 threads 数组"
        );
    }

    /// 契约里插件那三种应答的字面量，宿主都必须解析得出来（`launch` / `append`
    /// 解出字段，`{"action": null}` 解出「无事可做」）。宿主若改了 `parse_action`
    /// 认的键名，这条就红。
    #[test]
    fn the_contract_answers_parse_into_the_actions_the_host_expects() {
        let contract = driver_contract();
        let answers = &contract["answers"];

        let launch = parse_action(&answers["launch"]).expect("launch 字面量必须解析得出");
        assert_eq!(launch.action_id, "act-2");
        match launch.kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
                driver_project,
                driver_ref,
                env,
            } => {
                assert_eq!(workdir, "/w/card-1");
                assert_eq!(title, "看板 · a.spec");
                assert_eq!(objective, "do the card");
                assert_eq!(driver_project.as_deref(), Some("/proj"));
                assert_eq!(driver_ref.as_deref(), Some("card-1"));
                assert_eq!(
                    env,
                    vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
                    "插件的 env 必须原样透传，宿主不解释键名"
                );
            }
            other => panic!("expected launch, got {other:?}"),
        }

        assert!(
            answers.get("append").is_none(),
            "append 已从契约里退役：合并由用户在会话里发话触发，插件不再追加一轮"
        );

        // 新动作：恢复原会话后追加。宿主必须解析出全部四个字段——尤其 `workdir`
        // 是**原会话的定位目录**，漏解析会让宿主去目标树里找一个不存在的会话。
        let resume = parse_action(&answers["resume_append"]).expect("resume_append 必须解析得出");
        assert_eq!(resume.action_id, "act-resume");
        match resume.kind {
            DriverActionKind::ResumeAppend {
                thread_id,
                workdir,
                objective,
                env,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(workdir, "/w/card");
                assert_eq!(objective, "merge authorized");
                assert_eq!(
                    env,
                    vec![("SUPERPOWERS_KANBAN_CARD".to_string(), "card-1".to_string())],
                    "resume_append 的 env 同样原样透传"
                );
            }
            other => panic!("expected resume_append, got {other:?}"),
        }

        assert!(
            parse_action(&answers["nothing"]).is_none(),
            "`{{\"action\": null}}` 是「这一拍无事可做」的契约形状"
        );
    }

    /// 契约里声明的能力必须与宿主实际发出的完全一致：宿主少报一个，插件就会
    /// 把本来能做的动作当成不支持而永远排队；多报一个，插件会交出一个宿主执行
    /// 不了的动作。
    #[test]
    fn the_contract_declares_exactly_the_actions_the_host_supports() {
        let contract = driver_contract();
        let declared = contract["request"]["capabilities"]["actions"]
            .as_array()
            .expect("契约必须钉住 capabilities.actions");
        let declared: Vec<&str> = declared
            .iter()
            .map(|action| action.as_str().expect("能力名必须是字符串"))
            .collect();
        assert_eq!(declared, SUPPORTED_ACTIONS);
    }

    /// 契约里的失败样例必须能带上分类，且分类拼写与 `DriverErrorCode` 一致——
    /// 插件按这些字符串分流「可安全退队」与「必须阻塞」，拼错一个就等于永不退队。
    #[test]
    fn the_contract_failure_sample_uses_a_known_error_code() {
        let contract = driver_contract();
        let failure = &contract["results"]["failure"];
        let code = failure["error_code"]
            .as_str()
            .expect("失败样例必须带 error_code");
        let known: Vec<&str> = [
            DriverErrorCode::Busy,
            DriverErrorCode::ThreadNotFound,
            DriverErrorCode::WorkdirMissing,
            DriverErrorCode::RestoreFailed,
            DriverErrorCode::DeliveryFailed,
        ]
        .iter()
        .map(DriverErrorCode::as_str)
        .collect();
        assert!(
            known.contains(&code),
            "契约里的 error_code `{code}` 不在已知分类 {:?} 里",
            known
        );
        assert_eq!(code, DriverErrorCode::Busy.as_str());
    }

    /// 宿主**实际发出**的请求必须与契约里的 `request` 逐字一致：插件对
    /// `threads[].thread_id` / `idle` 是手工解析的（`dispatch.rs`），`results[]`
    /// 又由宿主手工编 JSON——两侧改名都不会有编译错误，只有这条对照会发现。
    #[tokio::test]
    async fn the_contract_request_matches_what_the_host_actually_sends() {
        let contract = driver_contract();
        let mut threads = vec![DriverThread {
            thread_id: "thread-1".into(),
            alive: true,
        }];
        let mut results = vec![ActionResult {
            action_id: "act-1".into(),
            ok: true,
            thread_id: Some("thread-1".into()),
            error: None,
            error_code: None,
        }];
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        let idle = |thread_id: &str| thread_id == "thread-1";

        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &idle,
            3,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["threads"], contract["request"]["threads"],
            "宿主发出的 threads[] 与契约必须逐字一致"
        );
        assert_eq!(
            sent[0]["results"], contract["request"]["results"],
            "宿主回报的 results[] 与契约必须逐字一致"
        );
        assert_eq!(
            sent[0]["sessions"], contract["request"]["sessions"],
            "宿主上报的全机运行会话数与契约必须逐字一致"
        );
    }

    /// 全机运行数必须真的编进线上 payload，而不是被丢掉。
    ///
    /// 与会话表的具体构造解耦：`running_sessions` 的口径已由上面五条单测钉住，
    /// 这里只钉「数出来的值确实进了 `sessions.running`」。
    #[tokio::test]
    async fn the_wire_carries_the_counted_running_sessions() {
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut driver_threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::default();
        driver_step(
            &query,
            &mut driver_threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            2,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["sessions"]["running"], 2,
            "计出来的在跑数必须原样上线"
        );
    }

    /// 能力必须真的编进每一拍请求。插件靠它决定「这张卡该不该现在派」——
    /// 缺了这一块，插件会把所有动作当成不支持而永远只排队。
    #[tokio::test]
    async fn the_wire_declares_the_supported_actions() {
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut driver_threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::default();
        driver_step(
            &query,
            &mut driver_threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["capabilities"]["actions"],
            json!(["launch", "append", "resume_append"]),
            "宿主必须每拍声明它支持的动作"
        );
    }

    /// 恢复原会话后追加：`launcher` 回的是**原** thread id，宿主必须原样回报，
    /// 并把该 thread 登记进观测表（否则插件永远看不到它活着）。
    #[tokio::test]
    async fn a_resume_append_success_keeps_the_original_thread_identity() {
        let reply = json!({
            "action_id": "act-resume", "kind": "resume_append",
            "thread_id": "thread-original", "workdir": "/w/card",
            "objective": "merge authorized"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-original".into(),
            ..Default::default()
        };

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .unwrap();

        assert!(outcome.ok);
        assert_eq!(outcome.thread_id.as_deref(), Some("thread-original"));
        assert_eq!(
            threads,
            vec![DriverThread {
                thread_id: "thread-original".into(),
                alive: true,
            }],
            "恢复出来的原会话必须登记进观测表"
        );
    }

    /// 同一个 thread 被恢复两次不能登记成两条——插件会把它们当成两个会话，
    /// 名额就算错了。
    #[tokio::test]
    async fn a_repeated_resume_append_registers_the_thread_only_once() {
        let reply = json!({
            "action_id": "act-resume", "kind": "resume_append",
            "thread_id": "thread-original", "workdir": "/w/card",
            "objective": "merge authorized"
        });
        let mut threads = vec![DriverThread {
            thread_id: "thread-original".into(),
            alive: false,
        }];
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-original".into(),
            ..Default::default()
        };

        driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .unwrap();

        assert_eq!(threads.len(), 1, "同 id 只能有一条观测：{threads:?}");
        assert!(threads[0].alive, "恢复成功即视为活着");
    }

    /// `append` 不登记新 thread：它只续跑**已经登记过**的会话。行为必须与改动前
    /// 一致，否则插件这两条动作的区别就被抹平了。
    #[tokio::test]
    async fn an_append_success_does_not_register_a_thread() {
        let reply = json!({
            "action_id": "act-append", "kind": "append",
            "thread_id": "thread-1", "objective": "keep going"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-1".into(),
            ..Default::default()
        };

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .unwrap();

        assert!(outcome.ok);
        assert!(threads.is_empty(), "append 不该登记会话：{threads:?}");
    }

    /// 分类过的失败必须把 `error_code` 送到线上：插件靠它区分「确定没投递」
    /// （可安全退队）与「不知道」（必须阻塞）。
    #[tokio::test]
    async fn a_classified_failure_carries_its_error_code_on_the_wire() {
        let reply = json!({
            "action_id": "act-resume", "kind": "resume_append",
            "thread_id": "thread-original", "workdir": "/w/card",
            "objective": "merge authorized"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::failing_with(DriverErrorCode::Busy, "thread is busy");

        // 本拍的回报要下一拍才上线：`driver_step` 把结果**返回**给调用方（主循环
        // 存入 `results`），故这里模拟主循环这一手，再跑一拍把它送出去。
        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .expect("a failed action still produces a result to report");
        assert!(!outcome.ok);
        assert_eq!(outcome.error_code.as_deref(), Some("busy"));
        results.push(outcome);

        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["results"][0]["error_code"], "busy",
            "分类必须原样上线：{sent:?}"
        );
    }

    /// 没被分类的错误不许猜：`error_code` 必须是 `None`，让插件按不确定处理。
    #[tokio::test]
    async fn an_unclassified_failure_has_no_error_code() {
        let reply = json!({
            "action_id": "act-resume", "kind": "resume_append",
            "thread_id": "thread-original", "workdir": "/w/card",
            "objective": "merge authorized"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            fail: Some("some unknown breakage".into()),
            ..Default::default()
        };

        // 本拍的回报由调用方（主循环）收进 `results`，下一拍才上线。
        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
            0,
            &[],
        )
        .await
        .expect("a failed action still produces a result to report");

        assert!(!outcome.ok);
        assert_eq!(
            outcome.error_code, None,
            "未分类的错误不能被冒充成某个确定分类"
        );
    }

    #[test]
    fn only_plugins_declaring_session_driver_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path();
        let manifests = scope.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("driver.json"),
            r#"{"name":"driver","command":"/bin/d","args":[],"switch_key":"d",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false,"session_driver":true}"#,
        )
        .unwrap();
        std::fs::write(
            manifests.join("plain.json"),
            r#"{"name":"plain","command":"/bin/p","args":[],"switch_key":"p",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false}"#,
        )
        .unwrap();

        assert_eq!(driver_plugins(scope), vec!["driver".to_string()]);
    }
}
