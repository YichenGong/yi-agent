//! thread driver 的主循环、tick、组长 tick、空闲 runtime 回收与通用 action 发起器。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 7）。本组 1,884 行，按职责分五片。

use crate::protocol::ThreadStatus;
use crate::session::ThreadSession;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use yi_agent_subagent::binding::RuntimeBinding;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    IdleFacts, ProjectRuntimes, project_has_plugin_manifest, project_key, should_evict,
};

pub(crate) mod launcher;
pub(crate) mod lead;
pub(crate) mod tick;
pub(crate) mod turn_driver;

pub(crate) use launcher::{DeltaCoalescer, PartialTurnInput, ServeLauncher};
pub(crate) use lead::{
    LeadGroup, LeadTarget, LeadTickDelivery, lead_tick_once, project_root_of, resolve_lead_target,
    snapshot_of, take_changed_digests,
};
pub(crate) use tick::driver_tick;
pub(crate) use turn_driver::run_thread_driver;

/// 权限审批等待客户端响应的默认超时;超时按 Deny 处理。
pub(crate) const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);
/// 会话驱动在主循环里跑一轮的间隔。
///
/// 采用「serve 主循环内周期性 tick」而非让驱动器并发访问 `threads`:整张
/// `threads` 表是主循环的局部量,重构成可共享结构会把改动面扩散到全部 RPC 分支。
/// 3s 足够跟上插件给出的动作,又远短于一个会话的生命周期。
pub(crate) const DRIVER_TICK: Duration = Duration::from_secs(3);
pub(crate) const EVICT_IDLE_AFTER: Duration = Duration::from_secs(600);
/// 保活策略的采样间隔。
///
/// 与 `DRIVER_TICK`（3s）刻意不同：电源/网络判定不需要 3s 的精度，而每拍都要
/// 走一次 `getifaddrs`（可能还有 `route` 子进程），3s 太密。10s 仍远小于 15 分钟
/// 的宽限窗口，迟到一拍不改变结论。
pub(crate) const POWER_TICK: Duration = Duration::from_secs(10);
/// 回收空闲项目的 runtime。
///
/// 每个「有活干」的事实都由调用方采好：`runtimes` 里的 binding 提供空闲时长，
/// `threads` 提供是否有回合在跑，磁盘与常驻登记提供两条豁免。
///
/// 移除条目即完成回收：`Arc<RuntimeBinding>` 的最后一个引用落在
/// `AttachedProjectRuntime` 上，它的 Drop 会停掉本进程起的 daemon 与监督线程，
/// 路由分区也随之清空。之后该项目再有委派时，`attach_cwd_runtime` 会懒重建。
///
/// 返回被回收的项目（供调用方记日志、也供测试断言）。
pub(crate) fn evict_idle_runtimes(
    runtimes: &ProjectRuntimes,
    threads: &HashMap<String, ThreadSession>,
    resident_dir: &Path,
    now: Instant,
    ttl: Duration,
) -> Vec<PathBuf> {
    // 快照：只取「哪个项目」「空了多久」。持锁期间不做磁盘 IO，也不调用任何
    // 需要再取这把锁的方法（`project_key` 会 canonicalize，是 IO）。
    let snapshot: Vec<(PathBuf, Duration)> = runtimes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .filter_map(|(key, entry)| {
            entry
                .as_ref()
                .ok()
                .map(|binding| (key.clone(), binding.idle_for()))
        })
        .collect();
    if snapshot.is_empty() {
        return Vec::new();
    }

    // 哪些项目还有回合在跑。threads 的 `cwd` 先统一解析成项目键，避免对每个
    // (项目, 会话) 组合都 canonicalize 一次。
    let busy_projects: Vec<PathBuf> = threads
        .values()
        .filter(|session| {
            session.active_turn_id.is_some()
                || !matches!(
                    *session
                        .status
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()),
                    ThreadStatus::Idle
                )
        })
        .map(|session| project_key(Path::new(&session.cwd)))
        .collect();

    let residents: Vec<PathBuf> = yi_agent_store::resident::list(resident_dir)
        .iter()
        .map(|resident| project_key(resident))
        .collect();

    let doomed: Vec<PathBuf> = snapshot
        .into_iter()
        .filter(|(key, idle_for)| {
            let facts = IdleFacts {
                last_active: now.checked_sub(*idle_for).unwrap_or(now),
                has_plugin_manifest: project_has_plugin_manifest(key),
                is_resident: residents.iter().any(|resident| resident == key),
                has_running_turn: busy_projects.iter().any(|busy| busy == key),
            };
            should_evict(&facts, now, ttl)
        })
        .map(|(key, _)| key)
        .collect();
    if doomed.is_empty() {
        return doomed;
    }

    // 回收分两步，顺序与「为什么这样才有效」见下。
    //
    // 1) 先让 binding **原地退休**：丢掉它自己的 daemon 与监督线程。
    //
    //    这一步不可省，也不能靠后续的 Drop 代替。会话的 driver 里，委派工具各持
    //    一份 `Arc<ThreadRoot>`，root 又持着 `Arc<RuntimeBinding>`——用户切走会话
    //    之后这些引用依然在（**会话本身必须留着**，否则对话就丢了）。只从
    //    `runtimes` 表里删条目只是少了一份引用，`Arc` 计数归不了零，`Drop` 永不
    //    触发，daemon 与每 500ms 一轮的监督线程继续跑到底——那正是「监督线程只增
    //    不减」本身。停进程必须显式做。
    //
    // 2) 再从表里摘掉条目。之后再有人按项目查 binding 会查不到，下次委派走
    //    `attach_cwd_runtime` 重建一个（冷启动）。
    //
    // 会话根**不动**：`retire` 推进了 generation，挂在旧 runtime 上的 root 会在
    // 下一次 `attach` 时被自己判定为失效并重挂，不需要宿主替它们清理。
    let retiring: Vec<Arc<RuntimeBinding>> = {
        let mut table = runtimes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        doomed
            .iter()
            .filter_map(|key| table.remove(key))
            .filter_map(Result::ok)
            .collect()
    };
    for binding in retiring {
        binding.retire();
    }
    doomed
}
/// 组长后台循环的低频兜底 tick。
///
/// 与看板调度器刻意分开:**唤醒组长要真的花一次 token**。看板的 3s 是「状态机
/// 对齐」的节奏,而组长的后台循环主要是**兜底**——真正及时的唤醒来自事件路径
/// (前台 `ask_lead` 实时投递、`lead_rx` 分支),tick 只负责捞那些事件没覆盖到的
/// 变化(例如某个 thread 卡在审批上、没有任何新消息)。30s 既能让兜底在一个可
/// 接受的时间内生效,又不会让它变成「每 3s 烧一次钱」。
pub(crate) const LEAD_TICK: Duration = Duration::from_secs(30);
/// 后台循环投递时在 `LeadEnvelope.from_thread_id` 里用的标识。
///
/// `deliver_to_lead` 会把它渲染成注入文本的前缀 `[来自会话 <id>] ...`。用一
/// 个**显然不是 thread id** 的固定串(而不是某个真实 thread),是为了让组长一眼
/// 看出这轮是后台兜底喂进来的、而不是某个具体会话在找它;也避免下游把某个普通
/// 会话误认成「这条 digest 的来源会话」。
pub(crate) const LEAD_LOOP_SENDER: &str = "lead-loop";
/// 首轮 LLM 标题生成的后台任务超时:超时即回退兜底标题。
///
/// 这个上限保护的是**后台标题任务**本身（不让它无限期挂着一个 task），与 turn
/// 收尾无关——标题生成已不在收尾路径上，超时与否都不影响 turn 完成。
pub(crate) const TITLE_TIMEOUT: Duration = Duration::from_secs(5);
