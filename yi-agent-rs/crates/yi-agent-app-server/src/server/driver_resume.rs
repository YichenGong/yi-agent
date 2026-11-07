//! 驱动侧「回到原会话」:热追加与冷装配。
//!
//! 插件说「这张实现卡已获授权合并」时,给的是一条 `resume_append` 动作。宿主必须
//! 把这一轮送回**原会话**——同一个 thread id、同一个 workdir——而不是新起一个
//! 会话顶替它。两种情形:
//!
//! - **热**:thread 已在本进程的 `threads` 表里。只核对其 id/cwd,然后走与
//!   `append` 完全相同的 `prepare_and_deliver`。绝不重建工具、绝不动运行时 env。
//! - **冷**:thread 不在内存(进程重启,或它的 driver 已因 `prompt_rx` 关闭而收尾)。
//!   按 [`crate::thread_store::ThreadStore::restore_for_driver`] 的**严格**读恢复
//!   历史,重建 session 与工具,再登记 driver。不调 `start_thread_core`、不
//!   `create` meta、不新铸 uuid。
//!
//! 三条边界在这里落地,任何一条破了都比「合并没跑」更糟:
//!
//! 1. **绝不退化成 `Launch`**。恢复失败必须报错并带上分类(`RestoreFailed`
//!    等),让插件把卡转 `NeedsYou`,而不是静默换一个失忆会话继续合并。
//! 2. **绝不碰别人的 turn**。`prepare_and_deliver` 投递失败时只回滚**本次**占位:
//!    先比对 `active_turn_id` 是否仍是本次那个 id,才清它并把状态收回 `Idle`。
//!    无条件清零会把另一个正在跑的 turn 变成无主(见 `rollback_turn`)。
//! 3. **env 不能悄悄丢**。冷装配走 `attach_delegation`;它失败会降级成「原 agent
//!    无委派」,那样注入给 shell 的 env 就不存在了。调用方带了 env 时这必须
//!    报 `RestoreFailed`,不能让合并 objective 跑在一个拿不到卡片句柄的会话里。

use super::*;
use crate::session_driver::{DriverActionFailure, DriverErrorCode};

impl<F> ServeLauncher<'_, F>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    /// `resume_append` 的内核:校验 → (冷装配) → 同一投递路径。
    ///
    /// 校验分两层,顺序是刻意的:
    /// 1. **动作自身**是否可执行(id 合法、workdir 现在还在)。这不需要任何会话
    ///    状态,故先判,错误分类也最精确。
    /// 2. **目标会话**是否可用。热会话核 cwd;冷会话由严格读再核一遍(它会比较
    ///    落盘 meta 的 cwd 与本次 workdir 的 canonical 形式)。
    ///
    /// 只有两关都过才动 `threads`/盘。失败时除了「冷装配已登记但投递失败」这一种
    /// (那种保留可手工打开的 idle 会话是有意为之),不留任何痕迹。
    pub(crate) async fn resume_append_inner(
        &mut self,
        thread_id: &str,
        workdir: &str,
        objective: &str,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<String> {
        validate_resume_target(thread_id, workdir)?;

        if let Some(session) = self.threads.get(thread_id) {
            // 热路径:目录必须与会话实际所属目录一致。只比 canonical 形式——
            // 同一个目录可以有尾斜杠/符号链接等写法,但指向别处就是别处。
            let wanted = crate::thread_store::canonical_or_self(Path::new(workdir));
            let held = crate::thread_store::canonical_or_self(Path::new(&session.cwd));
            if wanted != held {
                return Err(failed(
                    DriverErrorCode::RestoreFailed,
                    format!(
                        "thread {thread_id} lives in {} but the action names {workdir}; \
                         refusing to append a merge turn to a conversation in another tree",
                        session.cwd
                    ),
                ));
            }
            // 热会话不重建工具,动作里的 env 无从生效(它只对冷装配有意义)。
            // 这里**不报错**:插件对热会话也总会带 env(它无从知道宿主这边是否
            // 热),拒收会让「原会话还活着」这个最好的情形反而失败。
            return self.deliver_resume_turn(thread_id, objective).await;
        }

        // 冷路径:先把会话装回来(它只装配,不投递)。
        self.restore_driver_thread(thread_id, workdir, env).await?;
        self.deliver_resume_turn(thread_id, objective).await
    }

    /// 投递本轮,并把 `prepare_and_deliver` 的失败翻译成动作分类。
    ///
    /// 为什么不直接透传 `prepare_and_deliver` 的 anyhow 错误:插件要靠
    /// `error_code` 决定「这张卡能不能原地重试」。`Busy` 是唯一可安全退回队列的
    /// 分类,把 busy 混进泛泛的 `delivery_failed` 会让插件在别人正忙时反复重派。
    async fn deliver_resume_turn(
        &mut self,
        thread_id: &str,
        objective: &str,
    ) -> anyhow::Result<String> {
        // 先自己看一眼忙碌:这样分类精确,而且**零副作用**——不取消、不抢占、
        // 不投递。`prepare_and_deliver` 内部也会发现(它再查一次),那时错误文本
        // 已经不是结构化的了,故这里先判。
        if let Some(session) = self.threads.get(thread_id)
            && session.active_turn_id.is_some()
        {
            return Err(failed(
                DriverErrorCode::Busy,
                format!("thread {thread_id} is running another turn; not appending the merge turn"),
            ));
        }
        match self.prepare_and_deliver(thread_id, objective).await {
            Ok(thread_id) => Ok(thread_id),
            Err(error) => Err(classify_delivery_failure(
                error,
                // 结构化判 busy,不解析错误文本:`prepare_turn_core` 撞上
                // `TurnInProgress` 时**从未**设定过本次占位(它的所有失败返回都在
                // 写占位之前),所以此刻表里若仍有活跃 turn,那只可能是别人的。
                // 这正是 busy,而且与文案措辞无关,改文案不会悄悄改分类。
                self.threads
                    .get(thread_id)
                    .and_then(|session| session.active_turn_id.clone()),
            )),
        }
    }

    /// 冷装配:严格恢复一个已落盘的会话并登记它的 driver。
    ///
    /// 只装配,不投递。顺序与 spec §7 逐条对应:
    ///
    /// 1. workdir/ID 校验(目录存在、id 合法);
    /// 2. **先取跨进程 writer 锁**——锁必须覆盖读取、partial 升格、装配、登记。
    ///    先读后锁会让「另一个进程正在写它」的窗口成真。`Busy` ⇒ 报 busy;
    ///    `Io` ⇒ 报 `RestoreFailed`(不无锁继续:锁建不出来的会话不该被当权威);
    /// 3. 严格读一次权威存储(它自己会核 meta 的 id/cwd、坏行直接报错);
    /// 4. 用持久化配置构造 Session(消息、上次输入用量)与 agent(model_ref /
    ///    extra_roots / effort / preserve_thinking 四个逐会话槽位);
    /// 5. `attach_delegation` 重建工具,带持久化的 anchor 与本次的 env;
    /// 6. 登记 `ThreadSession`、pending activation、进程守望者、driver task。
    ///
    /// 失败时的清理**只限本次**:本函数拿到的所有登记都发生在最后一步,故中途
    /// 任何错误返回时,`threads`/`thread_roots`/`process_watches` 里都还没有
    /// 本次的条目——不需要、也**不允许**去删别的 thread 共用的项目 runtime。
    pub(crate) async fn restore_driver_thread(
        &mut self,
        thread_id: &str,
        workdir: &str,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        let store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(workdir)));

        // 顺序即防线:先锁、再读。锁句柄在成功路径上交给 `ThreadSession` 持有
        // (RAII:随会话从表里移除而释放)。
        let writer_lock = match crate::thread_store::acquire_thread_lock(store.root(), thread_id) {
            Ok(lock) => Some(lock),
            Err(crate::thread_store::ThreadLockError::Busy(busy)) => {
                return Err(failed(DriverErrorCode::Busy, busy.to_string()));
            }
            // 与 RPC resume 的取舍不同:那边 `Io` 退化无锁(会话照样要能打开);
            // 这边是**冷装配一个要拿去干活的会话**,无锁意味着可能双写历史,故拒绝。
            Err(crate::thread_store::ThreadLockError::Io(error)) => {
                return Err(failed(
                    DriverErrorCode::RestoreFailed,
                    format!("could not lock thread {thread_id} before restoring it: {error}"),
                ));
            }
        };

        let loaded = match store.restore_for_driver(thread_id, Path::new(workdir)) {
            // `None` = 这个会话从未落过盘。它不是「空会话」,是「没有这个会话」。
            Ok(None) => {
                return Err(failed(
                    DriverErrorCode::ThreadNotFound,
                    format!("thread {thread_id} has no persisted history in {workdir}"),
                ));
            }
            Ok(Some(loaded)) => loaded,
            Err(error) => {
                return Err(failed(
                    DriverErrorCode::RestoreFailed,
                    format!("could not restore thread {thread_id}: {error}"),
                ));
            }
        };

        let mode = loaded.meta.permission_mode;
        // cwd 用**落盘 meta 的**值:它是这个会话的真实归属(严格读已核过它与本次
        // workdir 一致,故这里两者等价;取 meta 是为了让后续所有路径用同一个字符串)。
        let cwd = loaded.meta.cwd.clone();
        let extra_roots: Vec<PathBuf> = loaded
            .meta
            .extra_roots
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .collect();

        // Captured before the messages move into the session: this decides whether a
        // missing baseline is "never ran" (safe to adopt today's default) or "ran under
        // unrecorded values" (must fall back to read-only).
        let has_history = !loaded.messages.is_empty();
        let mut session = yi_agent_core::Session::new();
        // 恢复上次用量,使 auto-compact 在恢复后的首轮即生效(与 RPC resume 同源)。
        if let Some(usage) = &loaded.usage {
            session.set_last_input_tokens(Some(usage.input_tokens));
        }
        session.replace_messages(loaded.messages);

        let built = with_session_model_ref(loaded.meta.model_ref.clone(), || {
            with_session_extra_roots(extra_roots.clone(), || {
                with_session_reasoning_effort(loaded.meta.reasoning_effort.clone(), || {
                    with_session_preserve_thinking(loaded.meta.preserve_thinking, || {
                        (self.build_agent)(Some(session), Path::new(&cwd), mode)
                    })
                })
            })
        })
        .map_err(|error| {
            failed(
                DriverErrorCode::RestoreFailed,
                format!("could not rebuild the agent for thread {thread_id}: {error}"),
            )
        })?;
        let model = built.agent.config().model.clone();

        let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
        let hub = Arc::clone(self.hub);
        // Resolve the sandbox from this conversation's own record, never from
        // `self.cfg.sandbox` (today's default for new conversations). This path
        // restores a conversation that is not in `threads` yet, so there is no
        // live controller to consult: the persisted baseline is the evidence,
        // and a pre-baseline meta with real history falls back to read-only.
        let resolved_baseline = crate::session_sandbox::resolve_activation_baseline(
            loaded.meta.sandbox_baseline,
            None,
            None,
            crate::session_sandbox::SandboxBaseline::from_parts(
                self.cfg.sandbox,
                self.cfg.sandbox_promotable,
            ),
            has_history,
        );
        if let Some(warning) = resolved_baseline.warning {
            tracing::warn!(
                thread_id,
                ?warning,
                "sandbox baseline resolved without full evidence"
            );
        }
        // Same policy as the RPC resume path: record the decision, but never
        // fail the restore because recording it failed.
        if resolved_baseline.should_persist()
            && let Err(error) = store.set_sandbox_baseline(thread_id, resolved_baseline.baseline)
        {
            tracing::warn!(thread_id, %error, "could not persist the resolved sandbox baseline");
        }
        let sandbox_baseline = Some(resolved_baseline.baseline);
        let activation = attach_delegation(
            self.runtimes,
            self.thread_roots,
            &runtime_dir,
            self.models_path,
            self.cfg,
            &cwd,
            thread_id,
            self.theme,
            self.git_diff,
            extra_roots,
            self.lead_inbox,
            self.lead_replies,
            self.lead_control,
            self.lead_control_replies,
            // resume 路径:meta 已在盘上,起点取已落盘的 `anchor_sha`。旧会话没有
            // 该字段时是 `None`——「无起点」,不回填(与 RPC resume 同一口径)。
            loaded.meta.anchor_sha.clone(),
            Some(self.lead_agent_enabled),
            env.clone(),
            self.live_settings,
            sandbox_baseline,
            built,
        );
        // spec §5.1:冷装配必须在**实际构造 bash 工具**时应用 env;装配失败就
        // 不得降级成「遗漏 env 的 BuiltAgent」再宣称成功。调用方带了 env 时,
        // 降级等于让合并命令落在一个拿不到卡片句柄的 shell 里,必须报错。
        if !env.is_empty()
            && let Some(reason) = &activation.tooling_error
        {
            // `attach_delegation` 可能已经为**本次** thread 建好并登记了一个 root
            // (tooling 失败时 root 本身是成功的)。不回滚它,这个 thread id 会在
            // `thread_roots` 里留一条无人持有的记录。只删本次这一个键——项目共享的
            // runtime binding 归别的会话一起用,绝不能动。
            self.thread_roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(thread_id);
            return Err(failed(
                DriverErrorCode::RestoreFailed,
                format!(
                    "thread {thread_id} cannot receive its session environment: {reason}; \
                     refusing to merge in a shell without it"
                ),
            ));
        }

        let BuiltAgent {
            agent,
            provider,
            config,
            decision_tx,
            catalog,
            yolo,
            process_manager,
            mcp,
            ..
        } = activation.built;
        self.pending_activation
            .insert(thread_id.to_string(), activation.runtime);

        let mcp_workdir = PathBuf::from(cwd.as_str());
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
        let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
        let store_status = ThreadSession::new_status();
        // 与 driver 同处创建:resume 会重建 driver,跨 resume 复用 sender 会让新
        // driver 永远收不到(旧 sender 的接收端已随旧 driver 退出)。
        let (subagent_tx, subagent_rx) =
            tokio::sync::watch::channel(crate::protocol::SubagentTotals::default());

        self.threads.insert(
            thread_id.to_string(),
            ThreadSession {
                thread_id: thread_id.to_string(),
                cwd: cwd.clone(),
                model,
                active_turn_id: None,
                yolo,
                process_manager: Arc::clone(&process_manager),
                prompt_tx,
                interrupt_tx,
                interject_tx,
                session_tx,
                store: Arc::clone(&store),
                status: Arc::clone(&store_status),
                subagent_tx,
                // 锁已经在上面拿到了,交给会话持有;它 drop 时 flock 释放。
                writer_lock,
            },
        );

        // 与 `thread/resume` 同款:先停旧守望者(本次是冷装配,通常没有),再对
        // 本次生效的 manager 重订——沿用「有守望者就不重建」的守卫会让旧守望者
        // 继续订阅被丢弃的那份 manager,`process/updated` 就此静默失联。
        if let Some(previous) = self.process_watches.remove(thread_id) {
            previous.stop().await;
        }
        let watch_hub = Arc::clone(&hub);
        let rx = process_manager.subscribe();
        let handle = tokio::spawn(watch_processes(
            watch_hub,
            thread_id.to_string(),
            Arc::clone(&process_manager),
            rx,
        ));
        self.process_watches
            .insert(thread_id.to_string(), ProcessWatch { task: handle });

        let driver_hub = Arc::clone(&hub);
        let driver_client = crate::broadcast::ClientId::local();
        let driver_turn_tx = self.turn_tx.clone();
        let driver_thread_id = thread_id.to_string();
        tokio::spawn(run_thread_driver(
            driver_thread_id,
            agent,
            prompt_rx,
            interrupt_rx,
            interject_rx,
            session_rx,
            driver_hub,
            driver_client,
            driver_turn_tx,
            decision_tx,
            Arc::clone(self.pending),
            self.permission_timeout,
            Arc::clone(self.perm_seq),
            catalog,
            Arc::clone(&store),
            Arc::clone(&store_status),
            provider,
            config,
            subagent_rx,
            mcp,
            mcp_workdir,
        ));

        Ok(())
    }
}

/// 动作自身可执行吗:ID 合法、workdir 现在仍然是个目录。
///
/// 这两条与任何会话状态无关,故先判、单独分类:
/// - 非法 id ⇒ `ThreadNotFound`(插件给的绑定本身坏了,换台机器也一样);
/// - 目录不在 ⇒ `WorkdirMissing`(开发 worktree 可能已被删/移走;此时只能人工处理,
///   绝不去别处找一个「像原会话」的目录)。
fn validate_resume_target(thread_id: &str, workdir: &str) -> anyhow::Result<()> {
    if !crate::thread_store::is_valid_thread_id(thread_id) {
        return Err(failed(
            DriverErrorCode::ThreadNotFound,
            format!("{thread_id:?} is not a valid thread id"),
        ));
    }
    let path = Path::new(workdir);
    if !path.is_dir() {
        return Err(failed(
            DriverErrorCode::WorkdirMissing,
            format!("the original session directory {workdir} does not exist"),
        ));
    }
    Ok(())
}

/// 投递阶段的失败归类。
///
/// `occupied_by` 是失败之后该 thread 上仍被占用的 turn id(没有则为 `None`):
/// 有 = 别人的活 turn 挡着 ⇒ `busy`(插件可安全退回队列);没有 = 别的投递问题
/// ⇒ `delivery_failed`。用状态而不是错误文本,是因为文本会随文案改动漂移,而
/// 插件只被允许信 `error_code`。
fn classify_delivery_failure(error: anyhow::Error, occupied_by: Option<String>) -> anyhow::Error {
    // 已经是分类过的错误就原样透传:调用方给的分类比这里更精确。
    if error.downcast_ref::<DriverActionFailure>().is_some() {
        return error;
    }
    let text = error.to_string();
    match occupied_by {
        Some(turn_id) => failed(
            DriverErrorCode::Busy,
            format!("thread is running turn {turn_id}; not appending the merge turn: {text}"),
        ),
        None => failed(DriverErrorCode::DeliveryFailed, text),
    }
}

/// 造一个带分类的失败。分类是插件分流唯一可靠的依据(它不许拿错误文本猜)。
fn failed(code: DriverErrorCode, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(DriverActionFailure {
        code,
        message: message.into(),
    })
}
