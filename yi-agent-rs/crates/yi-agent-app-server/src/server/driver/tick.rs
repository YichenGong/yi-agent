//! driver tick：每轮推进一次 driver，处理 action 与空闲回收。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 7）。

use crate::session::ThreadSession;
use crate::session_driver::DriverState;
use crate::workspace_index::WorkspaceIndex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::thread_root::ThreadRoot;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    BuiltAgent, ProcessWatch, ProjectRuntimes, ServeLauncher, ThreadRoots, TurnEvent,
    plugin_query_self,
};

/// 通用会话驱动的一轮:对每个「需要常驻 daemon」的项目、每个声明了
/// `session_driver` 的插件,拉一拍(`driver_step`)。
///
/// **内联在主循环里**(3s interval tick),不与任何 RPC 分支并发——`threads`
/// 等仍是主循环的局部量,`&mut` 借用按顺序发生(见设计约束:不让驱动另起任务
/// 并发访问 `threads`)。
///
/// 发现来源全是**通用**事实:项目来自 `yi_agent_store::resident::list`(哪些项目
/// 需要常驻 daemon),插件来自 `driver_plugins`(清单里 `session_driver: true`)。
/// 宿主不认识看板、卡片或合并——它只转发 `driver/next` 并执行插件给出的通用动作。
///
/// 会话空闲快照(`idle`)在**循环内**按 tick 采集:一轮 tick 里若刚起了一个会话,
/// 下一拍插件才看得到它的状态;且闭包持有的是 owned `HashMap`,不与 launcher
/// 需要的 `&mut threads` 冲突。
///
/// 两处失败**只记日志**,绝不中断本拍、也不影响 RPC:项目在常驻登记表里却没有任何
/// 驱动插件(宿主这一拍**根本没问**),以及某个插件这一拍问不通(问了但没打通)。
/// 二者必须分得开,否则「我没人可问」在日志里与「我问了、它失败了」一模一样。
/// 与主循环其它失败点同款:一个插件不可用不得影响其它插件。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn driver_tick<F>(
    resident_dir: &Path,
    drivers: &mut HashMap<(PathBuf, String), DriverState>,
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &mut HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: &mut HashMap<String, ProcessWatch>,
    uploads: &mut crate::image_upload::UploadRegistry,
    runtimes: &ProjectRuntimes,
    thread_roots: &ThreadRoots,
    models_path: &Path,
    cfg: &RuntimeConfig,
    hub: &Arc<crate::broadcast::Broadcaster>,
    turn_tx: &mpsc::Sender<TurnEvent>,
    pending: &Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: &Arc<AtomicU64>,
    theme: &crate::theme_tool::ThemeHandle,
    git_diff: &crate::git_diff_tool::GitDiffHandle,
    workspaces: &WorkspaceIndex,
    build_agent: &F,
    lead_inbox: &crate::lead::LeadInbox,
    lead_replies: &crate::lead::LeadReplies,
    lead_control: &crate::lead_control::LeadControlInbox,
    lead_control_replies: &crate::lead_control::LeadControlReplies,
    // 组长开关：被驱动的会话与手工会话必须是同一个判定，否则「关掉开关」
    // 只对用户手工开的会话生效，而看板/驱动会话仍会注册组长工具。
    lead_agent_enabled: bool,
) where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    for project in yi_agent_store::resident::list(resident_dir) {
        // 「没人可问」必须与「问了、失败了」在日志里分得开:项目已在常驻登记表里,
        // 却拿不出任何声明 `session_driver` 的插件(清单目录缺失,或里面每份清单都
        // 损坏被 `load_manifests` 丢掉)。若只在这里静默 `continue`,一个健康的 daemon
        // 就会永远不被问起而无一行日志——那正是「注册过了却什么都没发生」的现场。
        let plugins = crate::session_driver::driver_plugins(&project);
        if plugins.is_empty() {
            eprintln!(
                "[app-server] driver tick: {} is registered as resident but has no \
                 session_driver plugin, so nothing was asked",
                project.display()
            );
        }
        for plugin in plugins {
            // 先采**一次**热表快照（owned），两处用途共用它：`loaded_threads`
            // 原样上送，旧 `threads[].idle` 由同一份读数导出。这样同一个 ID 在
            // 一拍里的两个字段不可能给出互相矛盾的答案（一个说闲、一个说忙），
            // 也不会因为采两次而让「这一拍」有两个时刻。
            //
            // 采样必须在构造 launcher **之前**：launcher 持有 `&mut threads`，
            // 之后再借 `threads` 就是借用冲突（这正是原来那份 owned `HashMap`
            // 存在的原因）。
            let snapshot = crate::session_driver::loaded_threads_snapshot(threads);
            let idle_snapshot: HashMap<String, bool> = snapshot
                .iter()
                .map(|fact| (fact.thread_id.clone(), fact.idle))
                .collect();
            let idle =
                move |thread_id: &str| idle_snapshot.get(thread_id).copied().unwrap_or(false);
            // 全机运行会话数:必须在构造 launcher **之前**算——launcher 持有
            // `&mut threads`,之后再借 `threads` 就是借用冲突。口径与上面的 idle
            // 快照同源(同一份 `ThreadSession`),所有归属的会话都在内:手动开的、
            // 别的插件起的、看板起的。插件靠它算「全机余量」,限的是总流量。
            let sessions_running = crate::session_driver::running_sessions(threads);
            // 该 (项目, 插件) 的驱动记账跨 tick 存活:`results` 要在下一拍回报,
            // `threads` 要让插件看见自己起过的会话。
            let state = drivers
                .entry((project.clone(), plugin.clone()))
                .or_default();
            // 活着与否只能由宿主回答:`threads` 表的成员即活会话(会话结束就从表里
            // 摘掉)。每拍刷新一次,插件据此持有/释放并发名额、判 stale——它**不能**
            // 从 idle 推断(空闲的会话还活着),更不能从卡片状态推断。
            for driven in &mut state.threads {
                driven.alive = threads.contains_key(&driven.thread_id);
            }
            let mut launcher = ServeLauncher {
                threads,
                pending_activation,
                process_watches,
                uploads,
                runtimes,
                thread_roots,
                models_path,
                lead_agent_enabled,
                cfg,
                hub,
                turn_tx,
                pending,
                permission_timeout,
                perm_seq,
                theme,
                git_diff,
                workspaces,
                build_agent,
                lead_inbox,
                // 驱动器从不对组长投递,故回信登记表原样借入即可(签名要求)。
                lead_replies,
                // 写通道与结果登记表同理:驱动器不执行写指令,只为满足 launcher 签名。
                lead_control,
                lead_control_replies,
                // 驱动器从不对组长投递:`LeadTarget` 的解析会**落盘**一个组长
                // meta(见 `resolve_lead_target`),而驱动器每 3s 就要为每个
                // (项目, 插件) 构造一次 launcher——在这里解析会让每次 tick 都
                // 替所有项目写一遍 meta,并把「谁拉起组长」变成驱动器。`None`
                // 是诚实取值:该 launcher 只负责起被驱动的会话。
                lead_target: None,
            };
            // 查询闭包走**通用**插件通道(`plugin_query_self`):宿主只转发
            // `method`/`params`,不解释应答,也不受任何看板登记表门控。
            let scope = project.clone();
            let log_project = project.clone();
            let log_plugin = plugin.clone();
            // 本拍插件的**原样应答**。`driver_step` 把「解析不出动作」与「插件说
            // 无事可做」都压成 `None`,故要分辨第三种无声失败(问通了、字面能读、
            // 但对不上契约)只能由调用方自己留一份原答——见下面 `None` 分支的日志。
            // 闭包按引用捕获它(`driver_step` 只借 `&dyn Fn`,无 `'static` 要求),
            // 故本拍结束即可读回,不必用线程局部量或全局状态。
            let last_answer: StdMutex<Option<serde_json::Value>> = StdMutex::new(None);
            let query = |params: &serde_json::Value| {
                match plugin_query_self(&scope, &plugin, "driver/next", params.clone()) {
                    Ok(answer) => {
                        *last_answer
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(answer.clone());
                        Ok(answer)
                    }
                    Err(error) => {
                        // `driver_step` 把查询失败整体吞成「这一拍不推进」(`Err(_) =>
                        // return None`),它的签名与语义是插件契约的一部分,这里不改。
                        // 但失败必须留痕:否则「问了、没打通」与「问了、插件说无事可做」
                        // 在日志里完全同型,坏清单/daemon 掉线就成了无声停摆。
                        eprintln!(
                            "[app-server] driver tick: could not reach {} for {}: {}",
                            log_plugin,
                            log_project.display(),
                            error.message
                        );
                        Err(error.message)
                    }
                }
            };
            let outcome = crate::session_driver::driver_step(
                &query,
                &mut state.threads,
                &mut state.results,
                &mut launcher,
                &idle,
                sessions_running,
                &snapshot,
            )
            .await;
            // 本拍的结果**不在此处收割**:插件靠 `results` 才知道动作成没成、
            // 起了哪个 thread(spec §4.3),而 `driver_step` 只把它交回调用方。
            // 存进跨 tick 的记账里,下一拍才回报——早一拍回报等于插件还没被告知
            // 结果就先被问下一步。
            if let Some(result) = outcome {
                state.results.push(result);
            } else if state.results.is_empty() {
                // 第三种无声失败:查询**成功了**,但宿主解析不出动作(字段漂移、
                // 插件比宿主新、应答根本不是约定形状)。它在 `driver_step` 里与
                // 「插件说无事可做」同型(都回 `None`),不在这里分辨的话,断链就
                // 每 3s 无声重复一次。前两种失败(没人可问 / 问不通)已各有日志,
                // 这里补第三处。
                //
                // 两个前提使它不误报:本拍 `results` 为空——非空就说明至少交出了
                // 一个动作,或上一拍的动作还在等回报;且只认契约形状
                // `{"action": null}` 为「正常空闲」(见 driver-protocol.json 的
                // `answers.nothing`),那是唯一能合法回 `None` 的稀有情形。
                // 剩下仍回 `None` 的,只能是对不上契约的应答。
                let answer = last_answer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if let Some(answer) = answer
                    && !crate::session_driver::is_explicit_no_action(&answer)
                {
                    let keys = match answer.as_object() {
                        Some(map) => map.keys().cloned().collect::<Vec<_>>().join(", "),
                        None => "<not a JSON object>".to_string(),
                    };
                    eprintln!(
                        "[app-server] driver tick: {log_plugin} for {} answered driver/next \
                             with a shape the host cannot parse (top-level keys: {keys}), so this \
                             tick did nothing",
                        log_project.display()
                    );
                }
            }
        }
    }
}
