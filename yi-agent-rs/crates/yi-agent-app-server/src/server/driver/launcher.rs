//! `ServeLauncher`：把插件的 `DriverAction`（`Launch` / `Append`）变成一次真实的
//! 会话准备与投递。两个 inherent/trait impl 合计 525 行，必须与本结构体同文件。

use crate::protocol::{Notification, ThreadStatus};
use crate::session::{ThreadSession, TurnPrompt};
use crate::session_driver::{DriverAction, DriverActionKind, SessionLauncher};
use crate::workspace_index::WorkspaceIndex;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc, oneshot};
use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_subagent::thread_root::ThreadRoot;

use crate::server::turn::failure::{fail_announced_turn, rollback_turn};

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{
    BuiltAgent, LeadTarget, ProcessWatch, ProjectRuntimes, ThreadRoots, TurnEvent, apply_session,
    emit_item, opening_user_item, prepare_turn_core, start_thread_core, turn_prepare_rpc_error,
    update_status, write_notification,
};

/// 逐字流合并器：按 thread 攒 `(item_id, text)`。跨 item_id 不合并（顺序优先）。
#[derive(Default)]
pub(crate) struct DeltaCoalescer {
    /// thread → 当前正在累加的 (item_id, text)。
    pending: std::collections::HashMap<String, (String, String)>,
}
impl DeltaCoalescer {
    pub(crate) const FLUSH_BYTES: usize = 4096;

    /// 追加一段 delta，返回**此刻应当立即发出去的**若干条（跨 item 或超限时）。
    pub(crate) fn push(
        &mut self,
        thread: &str,
        item_id: &str,
        delta: &str,
    ) -> Vec<(String, String)> {
        let mut out = Vec::new();
        match self.pending.get_mut(thread) {
            Some((pid, text)) if pid == item_id => text.push_str(delta),
            _ => {
                if let Some(prev) = self.pending.remove(thread) {
                    out.push(prev);
                }
                self.pending
                    .insert(thread.to_string(), (item_id.to_string(), delta.to_string()));
            }
        }
        if self.pending.get(thread).map(|(_, t)| t.len()).unwrap_or(0) >= Self::FLUSH_BYTES
            && let Some(p) = self.pending.remove(thread)
        {
            out.push(p);
        }
        out
    }

    /// 取出并清空该 thread 的待发（屏障/tick 调用）。
    pub(crate) fn take(&mut self, thread: &str) -> Option<(String, String)> {
        self.pending.remove(thread)
    }

    /// 取出全部待发（tick 用）。
    pub(crate) fn take_all(&mut self) -> Vec<(String, (String, String))> {
        self.pending.drain().collect()
    }
}
/// `build_partial` 的入参分组：提问本身、已完成的 item、用量。
///
/// 拆成结构体是为了让 `build_partial` 的参数个数落在 clippy 的 7 个门槛内，
/// 同时把「这三组各自是什么」写在类型上，比一串同类型的 `&[...]` 好读。
pub(crate) struct PartialTurnInput<'a> {
    pub(crate) turn_id: &'a str,
    pub(crate) user_prompt: &'a str,
    pub(crate) user_attachments: &'a [crate::protocol::Attachment],
    pub(crate) user_image_refs: &'a [crate::protocol::ImageRef],
}
/// 驱动器起会话的真实实现:复用无帧内核(`start_thread_core` +
/// `prepare_turn_core`),**不**调用会向客户端发帧的 `start_turn_core`。
///
/// 它借用主循环的局部量(`threads` / `pending_activation` / `process_watches`)
/// 直接登记会话,故驱动器无需另起一个并发访问它们的任务。
///
/// `build_agent` 是 `serve` 的工厂参数(按引用借入,不要求 `'static`,也不要求
/// launcher 自己拥有它)——与 `thread/start` 分支用同一个工厂,因此被驱动的会话与
/// 手工会话的 agent 构造完全同源。
pub(crate) struct ServeLauncher<'a, F> {
    pub(crate) threads: &'a mut HashMap<String, ThreadSession>,
    pub(crate) pending_activation: &'a mut HashMap<String, Option<Arc<ThreadRoot>>>,
    pub(crate) process_watches: &'a mut HashMap<String, ProcessWatch>,
    /// 上传登记表：被驱动的会话不起于 RPC，但 `prepare_turn_core` 的签名要求它
    /// （`{type:"uploaded_image"}` 只可能来自 RPC；`driver/next` 的动作不带上传句柄）。
    pub(crate) uploads: &'a mut crate::image_upload::UploadRegistry,
    pub(crate) runtimes: &'a ProjectRuntimes,
    pub(crate) thread_roots: &'a ThreadRoots,
    /// The shell-brokered authority for this session, once the bootstrap installed it.
    pub(crate) managed: &'a crate::server::ManagedSlot,
    /// The bring-up choice resolved once by the entry point.
    ///
    /// The driven session must attach with the same policy as a hand-started one;
    /// re-reading the process policy here would let the two disagree.
    pub(crate) bringup: &'a std::sync::Arc<yi_agent_subagent::binding::BringUpChoice>,
    /// 机器级模型清单路径:被驱动的会话的委派链路与手工会话读同一份,而非各自
    /// 重读 `$HOME`。
    pub(crate) models_path: &'a Path,
    /// 进程级运行期设置绑定:交给 `start_thread_core` → `attach_delegation`，
    /// 使被驱动会话所在项目的内嵌 daemon 也跟随设置保存。`None` = 静态行为。
    pub(crate) live_settings:
        Option<&'a yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
    pub(crate) cfg: &'a RuntimeConfig,
    pub(crate) hub: &'a Arc<crate::broadcast::Broadcaster>,
    pub(crate) turn_tx: &'a mpsc::Sender<TurnEvent>,
    pub(crate) pending: &'a Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    pub(crate) permission_timeout: Duration,
    pub(crate) perm_seq: &'a Arc<AtomicU64>,
    pub(crate) theme: &'a crate::theme_tool::ThemeHandle,
    pub(crate) git_diff: &'a crate::git_diff_tool::GitDiffHandle,
    pub(crate) workspaces: &'a WorkspaceIndex,
    pub(crate) build_agent: &'a F,
    /// 组长信箱:普通会话经 `ask_lead` 投递的消息从这里被消费。与主循环共享
    /// 同一个实例(见 `serve` 的注入),自造一份会投进没人听的通道。
    pub(crate) lead_inbox: &'a crate::lead::LeadInbox,
    /// 阻塞式 `ask_lead` 的回信登记表:与 `lead_inbox` 同一实例、同一理由。
    /// 它经 `start_thread_core` 一路进 `AskLeadTool::with_replies`。
    pub(crate) lead_replies: &'a crate::lead::LeadReplies,
    /// 该 launcher 对应项目的组长 thread id 与 cwd,由消费方在调用前解析。
    /// 用 owned 值而非从 `threads` 反查:组长可能尚未被打开,不在 `threads` 里。
    /// `None` = 该 launcher 不承载组长投递(目前只有 `driver_tick` 构造的
    /// launcher,见其构造点)。
    pub(crate) lead_target: Option<LeadTarget>,
    /// 组长写通道：工具侧把「取消 / 打回」投进这里，主循环消费它去动 `threads`。
    /// 与 `lead_inbox` 相对（那个是 thread→lead，这个是 lead→thread），同一实例、
    /// 同一注入理由——自造一份就投进没人听的通道。
    ///
    /// 它经 `start_thread_core` / `attach_delegation` 一路进 `build_runtime_tooling`，
    /// 在装配期注册两个写工具（与上面 `lead_replies` 同一条通路）。
    pub(crate) lead_control: &'a crate::lead_control::LeadControlInbox,
    /// 写结果登记表：与 `lead_control` 成对转交（工具侧登记 `request_id`，主循环
    /// 执行完回填）。只传通道而不传登记表，组长那侧将永远等不到结果。
    pub(crate) lead_control_replies: &'a crate::lead_control::LeadControlReplies,
    /// 组长开关的生效值。交给 `start_thread_core`，使被驱动的会话与手工会话
    /// 用同一个判定。
    pub(crate) lead_agent_enabled: bool,
    /// 驱动动作的准入闸门。`None` = 不设闸（RPC 路径用不着它：`turn/start` 的
    /// 权限由 RPC scope 管，与插件无关）。
    ///
    /// # 为什么闸门在 launcher 里而不是在 driver_step 里
    ///
    /// `driver_step` 是纯协议逻辑（问插件、解析动作），它不该认识"谁能起会话"。
    /// 但**执行**动作的是 launcher，所以判定必须紧贴执行点：解析成功但没权限时，
    /// 既不能执行，也不能把失败记成"插件给了坏动作"——那会把一次权限拒绝伪装成
    /// 插件 bug。放在这里，"拒绝"发生在任何会话被创建之前，且原因能原样上送。
    pub(crate) driver_gate: Option<&'a (dyn crate::session_driver::DriverGate + 'a)>,
    /// 本 launcher 起出的插件会话一律绑这条发布（`launch_inner` 算好后设上）。
    ///
    /// 用 owned 值而不是每次从 meta 读：`launch_inner` 刚算过一次，重读只会
    /// 多一次磁盘往返，还会在"刚写 meta 但读回来的两份不一致"时给出两种答案。
    pub(crate) session_binding: Option<yi_agent_plugins::Binding>,
    /// 插件准入。驱动会话的每一轮都要在这里占一个 permit（排空要等它）。
    pub(crate) admission: Option<&'a yi_agent_plugins::Admission>,
    /// 宿主实例根。驱动会话要在这里解析「该绑哪个已提交发布」并写进 meta。
    ///
    /// 与 `driver_gate` 同一条注入链、同一个理由：读它下面的授权表/生命周期，
    /// 测试必须能指到隔离目录。`None` = 不写绑定（RPC 路径、组长投递）。
    pub(crate) instance_home: Option<&'a Path>,
    /// 测试专用：在**真实** prepare 完成之后、**真实** send 之前插入一次实例级
    /// 回调。回调只替换本次 `prepared` 的发送端为「接收端已关闭」的真实 mpsc，
    /// 让 `send` 自己失败——不模拟 `send` 的结果，也不替换 prepare / launcher
    /// 的整条逻辑。生产构造永远为 `None`。
    #[cfg(test)]
    pub(crate) before_prompt_send: Option<fn(&mut crate::server::turn::PreparedTurn)>,
}
/// 通用动作的执行者:插件说「起一个会话」或「往已有会话追加一轮」,这里照做。
///
/// 这就是宿主与插件的分界:**动作由插件给出**(`DriverAction`),宿主不认识卡片、
/// 不认识队列;`Launch` 建会话、`Append` 复用会话,两者共用同一段「准备 + 通知 +
/// 投递」(`prepare_and_deliver`),差别只在 thread 是否已存在。
///
/// **一个 RPC 响应帧都不发**:动作不是客户端请求,没有可应答的 id。可发的只有
/// thread-keyed 通知(`turn/started`、开启项、`thread/status/updated` 的 Running)。
impl<F> SessionLauncher for ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    async fn launch(&mut self, plugin: &str, action: &DriverAction) -> anyhow::Result<String> {
        match &action.kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
                driver_project,
                driver_ref,
                env,
            } => {
                // 起会话之前先过闸。顺序是"先判权限，再解析归属/建会话"：
                // 反过来的话，一次被拒绝的启动已经动了 `threads` 并落了盘，
                // "拒绝"就只是给用户看的字。
                if let Some(gate) = self.driver_gate {
                    gate.may_launch(Path::new(workdir), plugin)
                        .map_err(|reason| anyhow::anyhow!(reason))?;
                }
                self
                    // 归属由插件在动作里给出（`driver_project` 项目根 / `driver_ref` 驱动
                    // 方句柄），宿主原样落进 meta。缺字段（旧插件）时为 `None`，退化成
                    // 「未分组」，与改动前一致——绝不让缺字段把整条动作判废。
                    .launch_inner(LaunchRequest {
                        plugin,
                        workdir,
                        title,
                        objective,
                        driver_project: driver_project.as_deref(),
                        driver_ref: driver_ref.as_deref(),
                        env: env.clone(),
                    })
                    .await
                    .inspect_err(|error| {
                        // 插件只把失败记成 `{ok:false, error}`,自己不留日志;不在这里说清
                        // 原因,「这个会话为什么没起来」就没有任何线索。
                        eprintln!("[app-server] could not launch a session in {workdir}: {error}");
                    })
            }
            DriverActionKind::Append {
                thread_id,
                objective,
            } => {
                // 会话必须已经在 `threads` 里:append 的语义就是「续跑一个**本进程
                // 看得见**的会话」,冷会话无从续起(唤醒它要读盘恢复历史,那是另一
                // 个动作)。故先探一次,把「未知 thread」在**投递之前**判掉——否则
                // `prepare_turn_core` 会回一个「turn 校验失败」,掩盖真正的病因。
                if !self.threads.contains_key(thread_id) {
                    return Err(anyhow::anyhow!("unknown thread: {thread_id}"));
                }
                let thread_id = thread_id.clone();
                self.prepare_and_deliver(&thread_id, objective)
                    .await
                    .inspect_err(|error| {
                        eprintln!("[app-server] could not append a turn to {thread_id}: {error}");
                    })
            }
            DriverActionKind::ResumeAppend {
                thread_id,
                workdir,
                objective,
                env,
            } => {
                // 合并授权必须回到**原会话**。内核负责热追加与冷装配(见
                // `server/driver_resume.rs`)；本臂只做透传。
                //
                // 这里**绝不**退化成分派 `Launch`：那会用新会话顶替原会话，静默
                // 切断上下文，正是本次要修的行为。所有失败都带分类
                // (`busy` / `thread_not_found` / `workdir_missing` /
                // `restore_failed` / `delivery_failed`)，让插件能把卡转 NeedsYou
                // 而不是盲目重派。
                let thread_id = thread_id.clone();
                self.resume_append_inner(&thread_id, workdir, objective, env.clone())
                    .await
                    .inspect_err(|error| {
                        eprintln!(
                            "[app-server] could not resume thread {thread_id} for a merge turn: \
                         {error}"
                        );
                    })
            }
        }
    }
}
impl<F> ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    /// 起一个会话并投递它的首轮。
    ///
    /// 会话与 RPC 起的 thread 完全同型:同一内核、同一 driver、同一侧栏视角。唯一的
    /// 不同是 mode(驱动会话跑 Yolo)与「不发任何 RPC 帧」。
    ///
    /// `driver_project` / `driver_ref` 是落进 meta 的两个通用归属值,由插件在
    /// `driver/next` 的 `launch` 动作里给出(spec §5.5:唯一同时知道项目根与驱动
    /// 方句柄的是插件)。宿主只做透传:`driver_project` 让 `thread/listAll` 把会话折
    /// 进项目组,`driver_ref` 让桌面侧栏把它归入「驱动会话」小节。旧插件不发时为
    /// `None`,退化成未分组(与改动前逐字一致)。
    async fn launch_inner(&mut self, request: LaunchRequest<'_>) -> anyhow::Result<String> {
        let LaunchRequest {
            plugin,
            workdir,
            title,
            objective,
            driver_project,
            driver_ref,
            env,
        } = request;
        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

        // 先算出「这个会话绑哪个发布」。必须在建 agent 之前：会话的 skill 来源
        // 要按这个发布给，先建 agent 再补绑定会让第一个回合看到错的 skill。
        //
        // 失败处理分两种：
        // - 解析不出（未安装/未激活/未授权）：不是错误，会话照建，只是**不绑**
        //   插件。驱动动作能否执行已由闸门判过；这里判的是"会话该记住哪个
        //   版本"，判不出来时记空比记一个猜的版本安全。
        // - 解析出来：写进 meta，并让本次会话的 skill 发现带上该发布的受管根。
        let plugin_plan = match self.instance_home {
            Some(home) => plugin_binding_for_session(home, workdir, plugin),
            None => None,
        };
        // 记住本会话的绑定：本函数接下来要投递首轮，那一轮必须用同一条绑定
        // 占准入（见 `prepare_and_deliver`）。用刚算出来的值，而不是再读回
        // meta——两次读在"刚写完还没落盘"的窗口里可能给出不同答案。
        self.session_binding = plugin_plan.as_ref().map(|plan| plan.binding.clone());

        let built = (self.build_agent)(
            None,
            Path::new(workdir),
            crate::thread_store::ThreadMode::Yolo,
        )?;
        // 生效串取自实际建出的 agent,与 `thread/start`/`thread/resume` 同口径,避免
        // 工厂快照与另读磁盘不一致而上报一个未真正采用的模型。
        let model = built.agent.config().model.clone();

        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.managed,
            self.models_path,
            self.bringup,
            self.cfg,
            workdir,
            crate::thread_store::ThreadMode::Yolo,
            thread_id.clone(),
            // 看板会话起在卡片 worktree 里，那不是用户登记的「主根」，没有关联根
            // 可言；空集 = 单根会话（与改动前逐字一致，spec §7「看板项目列表行为
            // 不变」）。
            Vec::new(),
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.git_diff,
            self.workspaces,
            // 驱动会话不带 model_ref;生效串取自上面实际建出的 agent,与
            // `thread/start`/`thread/resume` 同口径。
            model,
            None,
            // 通用化的两个 meta 字段:项目根,以及「驱动方给的不透明句柄」。
            driver_project,
            driver_ref,
            self.lead_inbox,
            self.lead_replies,
            self.lead_control,
            self.lead_control_replies,
            Some(self.lead_agent_enabled),
            env,
            // 本次会话绑定的插件发布（`None` = 普通/未绑定会话）。
            plugin_plan.map(|plan| plan.binding).into_iter().collect(),
            // 进程级运行期设置绑定:被驱动会话所在项目的内嵌 daemon 也跟随保存。
            self.live_settings,
        )
        .await?;

        // 侧栏显示的名片:插件给的 `看板 · <spec stem>`。不设的话,这个会话在
        // 首轮落盘前标题为空、用户只看到一串 thread id——「可见会话」就不成立了。
        // 失败只记日志:标题是展示层信息,不该让一个会话起不来。
        if !title.is_empty()
            && let Some(session) = self.threads.get(&thread_id)
            && let Err(error) = session.store.rename(&thread_id, title)
        {
            eprintln!("[app-server] could not title driven thread {thread_id}: {error}");
        }

        self.prepare_and_deliver(&thread_id, objective).await
    }

    /// 执行一条组长写指令。
    ///
    /// 只有主循环能调：它持有 `&mut self.threads`。工具侧永远够不到这条路径
    /// （它们只往 `lead_control` 投递），这就是「守住 §6.1 独占」的落点。
    pub(crate) async fn apply_lead_control(
        &mut self,
        cmd: crate::lead_control::LeadControlCmd,
    ) -> Result<String, String> {
        use crate::lead_control::{ControlTarget, LeadControlKind, classify_target};

        // 目标 thread 所属的项目键：从它的 store 现取（每个 thread 有自己的
        // `cwd` → store，`threads` 里可能有多个项目的 thread）。取不到 = 目标不在场。
        let target_project_key = self
            .threads
            .get(&cmd.thread_id)
            .map(|s| s.store.project_key());
        let target = classify_target(
            &cmd.project_key,
            target_project_key.as_deref().unwrap_or(""),
            target_project_key.is_some(),
        );
        if target == ControlTarget::NotOpen {
            return Err(format!(
                "目标会话 {} 当前未打开（或不属于本项目），无法下达指令",
                cmd.thread_id
            ));
        }

        match cmd.kind {
            LeadControlKind::Interrupt => {
                let Some(session) = self.threads.get(&cmd.thread_id) else {
                    return Err("目标会话不在场".to_string());
                };
                let Some(turn_id) = session.active_turn_id.clone() else {
                    // 无活跃轮：报错，不静默 no-op（spec §3.2）。
                    return Err("目标当前没有活跃轮，无法打断".to_string());
                };
                // 与 `turn/interrupt` 逐字同源：幂等 level 信号 + try_send 不阻塞。
                let _ = session.interrupt_tx.try_send(turn_id);
                Ok(format!("已打断 {} 的当前轮", cmd.thread_id))
            }
            LeadControlKind::Reject => {
                let text = format!("[组长打回] {}", cmd.reason);
                // 有活跃轮 → 走 interjection（与用户中途打字同一路径）；
                // 无活跃轮 → 起一轮（spec §3.1）。
                if let Some(session) = self.threads.get(&cmd.thread_id)
                    && let Some(turn_id) = session.active_turn_id.clone()
                {
                    let interjection_id = format!("interject-{}-{}", turn_id, uuid::Uuid::new_v4());
                    if session
                        .interject_tx
                        .send(crate::session::InterjectionRequest {
                            turn_id,
                            interjection_id,
                            text,
                        })
                        .await
                        .is_err()
                    {
                        return Err("目标 driver 已退出".to_string());
                    }
                    return Ok(format!("已把打回指令并入 {} 的当前轮", cmd.thread_id));
                }
                // 无活跃轮（或目标刚转为 idle）：起一轮，走 prepare_and_deliver。
                self.prepare_and_deliver(&cmd.thread_id, &text)
                    .await
                    .map(|tid| format!("已向 {tid} 投递打回指令（起了新的一轮）"))
                    .map_err(|e| e.to_string())
            }
        }
    }

    /// 准备并投递一轮,**不发任何 RPC 响应帧**。
    ///
    /// 首轮(新建会话)与追问轮(已有会话)共用:两者只差 thread 是否已存在——
    /// 调用方负责保证它存在(首轮刚由 `start_thread_core` 登记,追问轮由调用方先探)。
    pub(crate) async fn prepare_and_deliver(
        &mut self,
        thread_id: &str,
        objective: &str,
    ) -> anyhow::Result<String> {
        let hub = Arc::clone(self.hub);
        let params = json!({
            "threadId": thread_id,
            "input": [{ "type": "text", "text": objective }],
        });
        // 插件会话的首轮必须过准入：它正是"排空时要等的那一轮"。
        // 绑定来自 launcher 起会话时算好的那条（`launch_inner` 已写进 meta），
        // 这里从 meta 读回来的代价不值得——调用方已持有同一个 plan。
        let admission = match (self.admission, self.session_binding.as_ref()) {
            (Some(admission), Some(binding)) => Some((admission, binding.clone())),
            _ => None,
        };
        #[allow(unused_mut)]
        let mut prepared = match prepare_turn_core(
            self.threads,
            self.pending_activation,
            self.uploads,
            &params,
            admission,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                // **绝不在这里碰 `active_turn_id`**(曾有一版在此清占位,是错的)。
                //
                // `prepare_turn_core` 只在**最后一步**(复制附件成功之后)才写占位
                // (`server.rs` 的「占位(复制成功之后)」),而它所有失败返回
                // (`UnknownThread` / `TurnInProgress` / `InvalidAttachment` /
                // `InvalidImage` …)都在那之前,之后没有任何可能失败的步骤。所以
                // 走到这里意味着**本次调用从未设定过占位**:该 thread 上的
                // `active_turn_id` 只可能属于**另一个正在跑的 turn**——追问轮撞上
                // 忙会话的 TOCTOU(spec §10)。
                //
                // 清零它就是把那个活 turn 变成无主:`turn/cancel` 静默失效、
                // `interrupt_and_wait_for_persist` 立即返回(`thread/delete` 于是可能
                // 拆掉一个还在跑的会话)、`is_idle` 谎报空闲(下一拍驱动会收到假的
                // 「这轮已结束」)。spec §10 对本行的处置只有一条:干净地回报
                // `ok:false`,对既有会话零副作用。
                //
                // 把具体原因带上:调用方只把它写进 `{ok:false, error}`,
                // 「准备失败」四个字不足以定位(是 thread 不在?还是 turn 撞车?)。
                return Err(anyhow::anyhow!(
                    "could not prepare a turn for {thread_id}: {}",
                    turn_prepare_rpc_error(error).message
                ));
            }
        };
        // 照 spec §4.4 的可见性要求:两个动作(首轮与追问轮)都发 `turn/started`,
        // 且与 `turn/start` 的 RPC 路径同序——它是该轮的第一帧。这是 thread-keyed
        // 的**内容通知**,不是 RPC 响应帧(不引入任何 `write_response`):桌面
        // `desktop/src/lib/session.ts` 用它断开 reasoning run 并置 `turnActive`,
        // 少了它,被驱动会话在侧栏里「运行中」的标志就只剩余额更弱的
        // `thread/status/updated`。
        // 从这里开始,**本次调用拥有** `prepared.turn_id` 的占位。任何后续失败都
        // 只回滚这一个占位(见 `rollback_turn`),绝不碰别的 turn。
        let announced = write_notification(
            &hub,
            &Notification::TurnStarted {
                thread_id: prepared.thread_id.clone(),
                turn_id: prepared.turn_id.clone(),
            },
        )
        .await;
        if let Err(error) = announced {
            // `turn/started` 都没发出去,界面上没有「运行中」需要收尾,但占位必须
            // 撤回,否则该会话会永远显示忙、后续每一拍驱动都拿到假 idle。
            rollback_turn(
                self.threads,
                self.hub,
                &prepared.thread_id,
                &prepared.turn_id,
            )
            .await;
            return Err(error);
        }
        // 状态走 `thread/status/updated`(通知),不是向某个 client 的响应,故可发:
        // 侧栏据此看到该会话正在跑。用 prepared 的句柄,保证与 `active_turn_id`
        // 的占位来自同一个 session。
        let _ = update_status(
            &hub,
            &prepared.status_handle,
            &prepared.thread_id,
            ThreadStatus::Running,
        )
        .await;
        // 照 $18 发本轮的开启项(用户提问=插件给的 objective),再投递 prompt。
        // 这不是「向客户端发 RPC 帧」——它是 thread-keyed 的 content 通知,与
        // RPC 客户端收到的同一条流;少了它,会话点开后 objective 会像修复前
        // 那样被迟到追到最底部(见 `opening_user_item`)。
        let opened = emit_item(
            &hub,
            &prepared.thread_id,
            opening_user_item(
                &prepared.turn_id,
                &prepared.display_text,
                prepared.attachments.clone(),
                prepared.image_refs.clone(),
            ),
        )
        .await;
        if let Err(error) = opened {
            // 已经宣告了这一轮,必须补一条结束通知,否则界面会永久停在「运行中」。
            fail_announced_turn(
                self.threads,
                self.hub,
                &prepared.thread_id,
                &prepared.turn_id,
                &error.to_string(),
            )
            .await;
            return Err(error);
        }
        #[cfg(test)]
        if let Some(hook) = self.before_prompt_send {
            hook(&mut prepared);
        }
        if let Err(error) = prepared
            .prompt_tx
            .send(TurnPrompt {
                // 失败收口要用身份，`turn_id` 随即被 move 进 prompt，故先 clone。
                turn_id: prepared.turn_id.clone(),
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
                lead_request_id: None,
            })
            .await
        {
            let _ = error;
            let message = format!(
                "thread driver for {} is gone before its prompt",
                prepared.thread_id
            );
            // 已经宣告过这一轮,必须补结束通知并撤回占位(只撤回本次那一个)。
            fail_announced_turn(
                self.threads,
                self.hub,
                &prepared.thread_id,
                &prepared.turn_id,
                &message,
            )
            .await;
            return Err(anyhow::anyhow!(message));
        }
        // send 成功就是成功:此后绝不允许把这一次再回报成「未接收」。
        Ok(prepared.thread_id)
    }

    /// 把一条投递给组长的消息转成组长 thread 的一次 turn。
    ///
    /// 与 `launch_inner` 的三点差异,每一点都是刻意的:
    /// - 复用**已存在的**组长 thread id(不新铸 uuid),因此重复投递落在同一个会话;
    /// - `mode` 用 Normal,而非被驱动会话所用的 Yolo 强制值(组长会话不是被驱动的会话);
    /// - 不设 `driver_project` / `driver_ref`,组长不是被驱动的会话。
    ///
    /// **不发任何 RPC 帧**:投递不是客户端请求,没有可应答的 id。唯一可发的是
    /// `thread/status/updated` 这类不带 id 的通知,这里也不发——投递方(某个普通
    /// 会话的 `ask_lead`)不需要它,而多发一帧会让「谁触发这轮」变得含糊。
    pub(crate) async fn deliver_to_lead(
        &mut self,
        envelope: &crate::lead::LeadEnvelope,
    ) -> anyhow::Result<String> {
        // 先取出目标(克隆),此后整段函数都不再借用 `self.lead_target`:
        // `start_thread_core` 要 `&mut self.threads`,未结束的 `&self.…` 借用
        // 会让它借不到。
        let target = self
            .lead_target
            .clone()
            .ok_or_else(|| anyhow::anyhow!("this launcher has no lead target"))?;
        // `mut` 只为下方冷启动的历史恢复:「先建空会话、再就地填入历史」是
        // `apply_session` 的形式,它保住的正是 session 的 `Arc`(见那里的说明)。
        let mut built = (self.build_agent)(
            None,
            Path::new(&target.cwd),
            crate::thread_store::ThreadMode::Normal,
        )?;
        let model = built.agent.config().model.clone();

        // 组长已在本进程内打开时,直接往它已有的 driver 投递,不必重建
        // (重建会丢掉该会话已累积的上下文,并让正在跑的那轮被替换)。
        if let Some(session) = self.threads.get(&target.thread_id) {
            let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
            let text = format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text);
            // 与 `thread/resume` 同款:激活只在本 thread 首次起 driver 时做。
            let activate = self
                .pending_activation
                .get(&target.thread_id)
                .cloned()
                .flatten();
            let send = session
                .prompt_tx
                .send(TurnPrompt {
                    turn_id,
                    prompt: text.clone(),
                    activate,
                    attachments: Vec::new(),
                    image_blocks: Vec::new(),
                    image_refs: Vec::new(),
                    display_text: text,
                    // 唯一会带身份的构造点:本轮就是替这条阻塞投递跑的,收尾时
                    // 只回填这一个 request_id(见 `settle_pending_lead_replies`)。
                    lead_request_id: envelope.request_id.clone(),
                })
                .await;
            return match send {
                Ok(()) => Ok(target.thread_id),
                Err(_) => Err(anyhow::anyhow!("lead driver is gone")),
            };
        }

        // 冷启动:该组长的 driver 不在本进程(进程重启,或它的 driver 因
        // `prompt_rx` 关闭而收尾)。`build_agent` 此刻建出的是**空会话**,而磁盘上的
        // `.jsonl` 记着此前投递过的每一轮。不填回去,同一条 thread 就会有两种语义:
        // warm 路径有记忆、cold 路径失忆。故这里镜像 `thread/resume` 的恢复三件事:
        // 载入 → `replace_messages` → 恢复上次用量(使 auto-compact 在恢复后的首轮
        // 即生效)。
        //
        // 组长的 meta 刚由 `ensure_lead_meta` 建、尚无 `.jsonl` 时 `load` 回 `None`:
        // 维持现状(空会话),与一次全新 `thread/start` 完全一致。
        // 关联根是**会话属性**（建会话时快照，之后不追改），所以这里取该会话自己
        // meta 里的那份，而**不**现读索引——与 `thread/resume` 同源。组长由
        // `thread/lead` 懒创建（未经 `thread/start` 的快照路径），故其 meta 里通常
        // 是空集：这是如实反映"这个会话创建时没有关联根"，不是降级。
        let lead_extra_roots: Vec<PathBuf> =
            match crate::thread_store::ThreadStore::new(Path::new(&target.cwd))
                .load(&target.thread_id)
            {
                Ok(Some(loaded)) => {
                    let roots: Vec<PathBuf> = loaded
                        .meta
                        .extra_roots
                        .iter()
                        .map(PathBuf::from)
                        .filter(|p| p.is_dir())
                        .collect();
                    let mut restored = yi_agent_core::Session::new();
                    if let Some(usage) = &loaded.usage {
                        restored.set_last_input_tokens(Some(usage.input_tokens));
                    }
                    restored.replace_messages(loaded.messages);
                    // `apply_session` 而非 `with_session`:前者就地替换内容、保住 session
                    // 的 `Arc`,与 `thread/resume` 的重建路径走同一个助手,因此委派工具
                    // 持有的 `CallerContext` 不会在恢复后指向一个被丢弃的旧会话。
                    apply_session(&mut built.agent, Some(restored));
                    roots
                }
                Ok(None) => Vec::new(),
                // 按 `thread/resume` 的口径:读不出来就明确失败,绝不静默地起一个
                // 失忆的组长。(id 由 `ensure_lead_meta` 铸造,恒合法;`load` 的读错误
                // 内部已吞成空历史,故此处实际不可达。)
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "could not load the lead's history for {}: {e}",
                        target.thread_id
                    ));
                }
            };

        // 冷启动:走与 `launch_inner` 相同的无帧内核(登记会话 + spawn driver)。
        let hub = Arc::clone(self.hub);
        let client = crate::broadcast::ClientId::local();
        start_thread_core(
            self.threads,
            self.pending_activation,
            self.process_watches,
            self.runtimes,
            self.thread_roots,
            self.managed,
            self.models_path,
            self.bringup,
            self.cfg,
            &target.cwd,
            crate::thread_store::ThreadMode::Normal,
            target.thread_id.clone(),
            lead_extra_roots,
            built,
            &hub,
            &client,
            self.turn_tx,
            self.pending,
            self.permission_timeout,
            self.perm_seq,
            self.theme,
            self.git_diff,
            self.workspaces,
            model,
            None,
            None,
            None,
            self.lead_inbox,
            self.lead_replies,
            self.lead_control,
            self.lead_control_replies,
            Some(self.lead_agent_enabled),
            // 组长会话不是驱动会话，没有插件句柄可注入。
            Vec::new(),
            // 组长会话不绑插件。
            Vec::new(),
            self.live_settings,
        )
        .await?;

        // 无帧地占好首个 turn,再自行投递 prompt——与 `launch_inner` 同一顺序。
        let params = json!({
            "threadId": target.thread_id,
            "input": [{ "type": "text",
                        "text": format!("[来自会话 {}] {}", envelope.from_thread_id, envelope.text) }],
        });
        #[allow(unused_mut)]
        let mut prepared =
            // 组长投递不占插件准入：它不属于任何插件。
            prepare_turn_core(self.threads, self.pending_activation, self.uploads, &params, None)
                .await
                .map_err(|error| {
                    anyhow::anyhow!(
                        "could not prepare the lead's first turn: {}",
                        turn_prepare_rpc_error(error).message
                    )
                })?;
        #[cfg(test)]
        if let Some(hook) = self.before_prompt_send {
            hook(&mut prepared);
        }
        // 冷启动这条路径**不**发 `turn/started`（投递不是客户端请求，见上文），
        // 所以失败时不该编造开始/完成事件：只按身份撤回本次占位，保留原字符串
        // 作为对外错误。
        let send_result = prepared
            .prompt_tx
            .send(TurnPrompt {
                turn_id: prepared.turn_id.clone(),
                prompt: prepared.prompt,
                activate: prepared.activate,
                attachments: prepared.attachments,
                image_blocks: prepared.image_blocks,
                image_refs: prepared.image_refs,
                display_text: prepared.display_text,
                lead_request_id: envelope.request_id.clone(),
            })
            .await;
        if send_result.is_err() {
            rollback_turn(
                self.threads,
                self.hub,
                &prepared.thread_id,
                &prepared.turn_id,
            )
            .await;
            return Err(anyhow::anyhow!(
                "lead driver is gone before its first prompt"
            ));
        }
        Ok(target.thread_id)
    }
}

/// 收成结构体而不是七个位置参数：认位置会错（`driver_project` 与 `driver_ref` 都是
/// `Option<&str>`，写反了编译器一声不响），而这两者的含义完全不同——一个决定会话
/// 折进哪个项目组，一个决定它归入哪条驱动小节。命名参数把这件事交给编译器。
struct LaunchRequest<'a> {
    plugin: &'a str,
    workdir: &'a str,
    title: &'a str,
    objective: &'a str,
    /// 落进 meta 的项目根；让 `thread/listAll` 把会话折进项目组。
    driver_project: Option<&'a str>,
    /// 落进 meta 的驱动方句柄；让桌面侧栏归入「驱动会话」。
    driver_ref: Option<&'a str>,
    /// 插件要注入本会话 shell 的环境变量。宿主不解释任何一个键：它们的含义
    /// 属于插件（「这张卡是哪张」不是宿主的通用事实）。
    env: Vec<(String, String)>,
}

/// 按已提交激活算出「这个会话该绑哪个发布」。
///
/// 任何一步判不出来都返回 `None`，而不是让会话创建失败：
/// - 插件没装/没激活 → 会话照建，只是不绑；
/// - 授权表里没有该项目 → 同上（能否起会话已由闸门判过，这里只回答"绑哪个"）。
///
/// 反过来，**判出来就一定绑对**：计划里的 release/activation 与 skill 根来自
/// 同一次 `managed::resolve`，不会出现"绑 1.4.0、看 1.5.0 的 skill"。
pub(crate) fn plugin_binding_for_session(
    instance_home: &Path,
    workdir: &str,
    plugin: &str,
) -> Option<crate::plugin_manager::SessionPlan> {
    // 项目键用 canonical：授权表按 canonical 项目路径索引，动作给的是原样路径。
    let project = std::fs::canonicalize(workdir).unwrap_or_else(|_| PathBuf::from(workdir));
    let project_id = yi_agent_plugins::ProjectId::parse(&project.to_string_lossy()).ok()?;
    match crate::plugin_manager::plan_session(instance_home, &project_id, plugin) {
        Ok(plan) => Some(plan),
        Err(error) => {
            // 不静默：会话不带绑定是有后果的（它之后调插件会得到"未绑定"）。
            // 但这不该阻断起会话本身——闸门已经判过能不能起。
            eprintln!(
                "[app-server] session in {workdir} will not be bound to plugin `{plugin}`: {error}"
            );
            None
        }
    }
}
