//! `thread/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 3,最后一批)。会话的
//! 全生命周期:列表(单目录 / 跨目录分组)、新建 / 恢复、改名、模型档位(模型 /
//! reasoning effort / 跨轮 CoT / 权限模式)、置顶与其重排、删除、清空、压缩、
//! diff 读取、订阅与 items 补齐。
//!
//! 臂体保持原样,只有三处语言强制的改名:主循环局部量改走 [`AppState`] 字段
//! (`hub` -> `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉)、`continue`
//! 换成 `return Ok(Flow::Continue)`(语义由 `serve()` 翻回 `continue`),以及
//! 模块自身的 `match` 键改用 `req.method`。57 处回主循环的 `continue` 换成了返回值
//! (55 处独占一行,`thread/start` 与 `thread/lead` 各内联 1 处),另有 1 处残留的
//! 臂**内层** `continue`(`thread/listAll` 的 `for dir in &dirs`)与 3 处臂**内层**
//! 的 `break`(`thread/resume`、`thread/reorderPinned`),目标不变。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError, Scope, TurnStatus};
use crate::rpc::{AppState, Flow};
use crate::server::{
    BuiltAgent, ProcessWatch, ThreadCancelOutcome, attach_delegation, cancel_thread_children,
    chunk_items_for_replay, collect_pinned, detach_unused_runtimes, err_response,
    interrupt_and_wait_for_persist, item_id, linked_extra_roots, ok_response, require_known_thread,
    require_thread_id, resolve_thread_cwd, run_thread_driver, start_thread_core, store_lookup,
    thread_summary_json, watch_processes, with_session_extra_roots, with_session_model_ref,
    with_session_preserve_thinking, with_session_reasoning_effort, worktree_json,
    write_notification, write_notification_await, write_response,
};
use crate::session::{
    CompactOutcome, InterjectionRequest, SessionCommand, ThreadSession, TurnPrompt,
};
/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂里
/// 的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/list" => {
            // 单目录列表:仍是 `cfg.workdir` 的 store。跨目录分组见下方
            // `thread/listAll`。
            let store = crate::thread_store::ThreadStore::new(&state.cfg.workdir);
            match store.list() {
                Ok(metas) => {
                    // 显式映射而非直接序列化 ThreadMeta:wire 契约与存储结构解耦,
                    // 存储字段重命名不会悄悄改变 RPC 输出。
                    let threads: Vec<serde_json::Value> = metas
                        .into_iter()
                        .map(|m| thread_summary_json(&m, state.threads))
                        .collect();
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(id, json!({ "threads": threads })),
                    )
                    .await?;
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/listAll" => {
            // 分组两段式,与抽取前逐字等价,只多一层「归属优先」的折拢:
            // ① 按索引顺序(最近在前)为**每个**目录建组——含失效目录
            //   (exists:false、threads 空,不深扫),普通会话留在本目录组;
            // ② 带 `board_project` 的卡片会话折进它所属的项目组(项目可不在
            //   索引里,则合成一组);只含卡片会话的目录(卡片 worktree)
            //   第一段被抑制,不再顶层成组。
            // **不动索引**:目录仍留在 workspaces 里,冷会话定位靠它。
            let dirs = state.workspaces.list();
            // 组顺序:先索引目录(保持最近在前),再追加索引外的新项目组。
            let mut order: Vec<String> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut exists_of: HashMap<String, bool> = HashMap::new();
            let mut plain: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            let mut cards: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
            for dir in &dirs {
                if seen.insert(dir.clone()) {
                    order.push(dir.clone());
                    exists_of.insert(dir.clone(), Path::new(dir).is_dir());
                }
                // 失效目录:不深扫,组照留(threads 空),与抽取前一致。
                let path = Path::new(dir);
                if !path.is_dir() {
                    continue;
                }
                // 读取错误(如权限拒绝)不能静默等同于「无 thread」:
                // 记 stderr 后再降级为空组,与 thread/list 的错误可见性一致。
                let mut read_ok = true;
                let mut metas = match crate::thread_store::ThreadStore::new(path).list() {
                    Ok(metas) => metas,
                    Err(e) => {
                        eprintln!("[app-server] thread/listAll failed to list {dir}: {e}");
                        read_ok = false;
                        Vec::new()
                    }
                };
                // 每个被列举的**空**项目目录都确保有组长,让侧栏里本就没有
                // 任何条目的项目也有一个组长入口。失败只记日志——列表查询
                // 不得因单个目录不可写而整体失败。
                //
                // 为什么限定「没有任何会话」:组长 meta 的 `board_project`
                // 是 `None`,一旦写进已有会话的目录,它就会被算作「该目录
                // 自己的会话」,从而①把「每个目录组恰好一个会话」变成两个、
                // ②让只含卡片会话的 worktree 不再被抑制成顶层组。两者都是
                // `listAll` 既有行为的一部分,列表查询不得改写它。已有会话的
                // 项目仍是懒创建的目标,只是触发点更准:用户点组长入口
                // (`thread/lead`)或某个会话给组长留言(`resolve_lead_target`)。
                //
                // 读失败的目录(`read_ok == false`)不写:读不到就不该有副作用。
                if read_ok && metas.is_empty() {
                    if let Err(e) = crate::lead::ensure_lead_meta(
                        &crate::thread_store::ThreadStore::new(path),
                        path,
                    ) {
                        eprintln!(
                            "[app-server] thread/listAll could not ensure a lead for {dir}: {e}"
                        );
                    }
                    // 新落的组长 meta 并入本轮列举结果,否则「本目录的
                    // 会话」是懒创建**之前**的快照,刚种下的组长在这一
                    // 次响应里就不可见(要等下一次刷新才出现)。
                    if let Ok(fresh) = crate::thread_store::ThreadStore::new(path).list() {
                        metas = fresh;
                    }
                }
                let own: Vec<_> = metas.iter().filter(|m| m.board_project.is_none()).collect();
                // 该目录有「自己的」普通会话(或本就为空)才作为顶层组保留;
                // 否则它只是卡片 worktree,归拢后被抑制。
                if !own.is_empty() || metas.is_empty() {
                    for m in own {
                        plain
                            .entry(dir.clone())
                            .or_default()
                            .push(thread_summary_json(m, state.threads));
                    }
                } else {
                    // 只含卡片会话:抑制顶层空组(稍后不产出)。
                    seen.remove(dir);
                    order.retain(|d| d != dir);
                }
                // 卡片会话:归入 board_project 指定的组(可为索引外的项目)。
                for m in metas.iter().filter(|m| m.board_project.is_some()) {
                    let target = m.board_project.clone().unwrap();
                    if seen.insert(target.clone()) {
                        order.push(target.clone());
                        exists_of.insert(target.clone(), Path::new(&target).is_dir());
                    }
                    exists_of
                        .entry(target.clone())
                        .or_insert_with(|| Path::new(&target).is_dir());
                    cards
                        .entry(target)
                        .or_default()
                        .push(thread_summary_json(m, state.threads));
                }
            }
            let groups: Vec<serde_json::Value> = order
                .iter()
                .map(|ws| {
                    let mut merged = plain.remove(ws).unwrap_or_default();
                    if let Some(mut folded) = cards.remove(ws) {
                        merged.append(&mut folded);
                    }
                    json!({
                        "workspace": ws,
                        "exists": exists_of.get(ws).copied().unwrap_or(false),
                        "threads": merged,
                    })
                })
                .collect();
            let pinned: Vec<serde_json::Value> = collect_pinned(state.workspaces)
                .iter()
                .map(|m| thread_summary_json(m, state.threads))
                .collect();
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "groups": groups, "pinned": pinned })),
            )
            .await?;
        }
        "thread/start" => {
            let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

            // 目录决定 agent / store / 权限 / 沙箱 / skills 的根。
            let cwd = match resolve_thread_cwd(
                &req.params,
                state.cfg,
                state.hub,
                state.client,
                id.clone(),
            )
            .await?
            {
                Some(c) => c,
                None => return Ok(Flow::Continue),
            };

            // 新线程一律以 Normal 起步;同一值既用于建 agent,也落盘 meta,
            // 抽成局部量避免两处字面量漂移。
            let mode = crate::thread_store::ThreadMode::Normal;

            // 可选的会话级模型覆盖(清单显示名)。wire 用 camelCase
            // `modelRef`,兼容 snake_case `model_ref`;二者都缺省即
            // `None`(跟随清单全局默认,再回退 cfg)。
            let model_ref = req
                .params
                .get("modelRef")
                .or_else(|| req.params.get("model_ref"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            // 本次会话的关联根：`thread/start` 从索引快照一次，之后**只**用这个局部量
            // ——既进工厂槽位（决定沙箱/工具/权限的根集），也进 meta（落盘快照）与
            // `attach_delegation`（委派路径的根集）。三处必须同源，故不各自重读索引。
            let extra_roots = linked_extra_roots(state.workspaces, &cwd);

            // 本次会话的 `model_ref` 经 thread-local 交给工厂解析 provider /
            // api_url / api_key / model(命中清单默认则用条目,否则回退 cfg)。
            // 关联根同路经 thread-local 传入:工厂是按目录构建、被同目录多个
            // 会话共享的,把根集写进共享 `cfg` 会让它泄漏给别的会话。
            let built = match with_session_model_ref(model_ref.clone(), || {
                with_session_extra_roots(extra_roots.clone(), || {
                    with_session_reasoning_effort(None, || {
                        with_session_preserve_thinking(true, || {
                            (state.build_agent)(None, Path::new(&cwd), mode)
                        })
                    })
                })
            }) {
                Ok(a) => a,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };

            // 落盘/上报的生效串取自**实际建出的 agent**,而非另按清单解析一次。
            // `production_factory` 只在进程启动时快照一次清单,运行期经
            // `model/upsert` 新增的条目不在其快照内;此处若再读一次磁盘,就可能
            // 上报一个工厂实际未采用的模型(任务 7 评审 Finding 1)。用
            // `agent.config().model` 才能保证「上报 == 实际运行」。
            let model = built.agent.config().model.clone();

            // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。
            // 用硬编码 null 会在条目带默认档时谎报(本会话其实跑着该档),
            // 且与 resume 的口径不一致。
            let reasoning_effort = built.agent.config().gen_params.reasoning_effort.clone();

            // 与 `model` 同源:上报**实际建出**的 agent 的开关值。
            let preserve_thinking = built.agent.config().preserve_thinking;

            // 内核负责「attach_delegation → 建 driver 通道 → insert 到
            // `threads` → spawn 守望者与 driver」;RPC 层的通知与响应仍在
            // 本分支完成,与抽取前逐字一致。
            start_thread_core(
                state.threads,
                state.pending_activation,
                state.process_watches,
                state.runtimes,
                state.thread_roots,
                state.models_path,
                state.cfg,
                &cwd,
                mode,
                thread_id.clone(),
                // 本会话的关联根：上面已捕获的同一个局部量，内核据此写
                // meta 并传给 `attach_delegation`（不在那里重读槽位）。
                extra_roots,
                built,
                state.hub,
                state.client,
                state.turn_tx,
                state.pending,
                state.permission_timeout,
                state.perm_seq,
                state.theme_handle,
                state.git_diff_handle,
                state.workspaces,
                model.clone(),
                model_ref.clone(),
                None,
                None,
                state.lead_inbox,
                state.lead_replies,
            )
            .await?;

            write_notification(
                state.hub,
                &Notification::ThreadStarted {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    model_ref: model_ref.clone(),
                },
            )
            .await?;
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "thread_id": thread_id,
                        "cwd": cwd,
                        "model": model,
                        "model_ref": model_ref,
                        "permission_mode": mode,
                        // 新会话无覆盖 ⇒ 上报实际运行值(解析出的条目默认
                        // 或 cfg;都没有则 null),与实际建出的 agent 同源。
                        "reasoning_effort": reasoning_effort,
                        "preserve_thinking": preserve_thinking,
                    }),
                ),
            )
            .await?;
        }
        "thread/lead" => {
            // 组长归属当前请求的项目根。与 `thread/start` 同源,保证
            // 「用户在哪个项目里问组长」与「组长管哪个项目」一致。
            let cwd = match resolve_thread_cwd(
                &req.params,
                state.cfg,
                state.hub,
                state.client,
                id.clone(),
            )
            .await?
            {
                Some(c) => c,
                None => return Ok(Flow::Continue),
            };
            let store = crate::thread_store::ThreadStore::new(Path::new(&cwd));
            match crate::lead::ensure_lead_meta(&store, Path::new(&cwd)) {
                Ok((meta, created)) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "thread_id": meta.thread_id,
                                "created": created,
                            }),
                        ),
                    )
                    .await?;
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/resume" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // Design §8:内存中的 thread 知道其权威 cwd,若目标目录已被删除,
            // 必须明确报错,而不是经 store_lookup 回退到 cfg.workdir(那会让
            // 后续 load 落空,退化成含混的 -32011,甚至悄悄换到别的目录)。
            // 冷 thread 的 meta 存在 workspace 目录内,删目录后本就不可读,
            // 仍走下方的 -32011,不在此覆盖。
            if let Some(session) = state.threads.get(&thread_id) {
                if !Path::new(&session.cwd).is_dir() {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id.clone(),
                            RpcError::invalid_params(format!(
                                "working directory no longer exists: {}",
                                session.cwd
                            )),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
            // 冷 thread 才按全局索引 / cfg.workdir 定位,再从中载入历史。
            let thread_store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            // 若该 thread 仍在内存且有活跃 turn,先请求中断,再等待 driver
            // 落盘完成,否则紧随 turn/completed 的 resume 会读到尚未写入
            // 的历史。详见 `interrupt_and_wait_for_persist`。
            interrupt_and_wait_for_persist(
                state.threads,
                state.turn_rx,
                &thread_id,
                state.pending_lead_replies,
                state.lead_replies,
            )
            .await;

            let loaded = match thread_store.load(&thread_id) {
                Ok(Some(l)) => l,
                Ok(None) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };

            // 该 thread 持久化的自主权模式:决定重建 agent 时的 yolo 初值。
            let mode = loaded.meta.permission_mode;

            // meta 缺 cwd/model 时(损坏重建)用当前配置兜底。
            let cwd = if loaded.meta.cwd.is_empty() {
                state.cfg.workdir.display().to_string()
            } else {
                loaded.meta.cwd.clone()
            };

            let mut session = yi_agent_core::Session::new();
            // 恢复上次用量,使 auto-compact 在 resume 后的首轮即生效。
            if let Some(u) = &loaded.usage {
                session.set_last_input_tokens(Some(u.input_tokens));
            }
            session.replace_messages(loaded.messages);

            // 按 cwd(`meta.cwd`,损坏时用 cfg.workdir 兜底)重建本 thread
            // 的 store,让 agent 与 driver 都跑在对话真实目录,而非全局
            // cfg.workdir——这正是此前 resume 的隐患所在。
            let thread_store = Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

            // 附属根从 meta 快照重建（不在 resume 时重读索引：关联是**会话属性**，
            // 建会话时定了就不再追改，否则同一会话的权限根集会随索引漂移）。
            // 已删的附属根静默丢弃——它是能力增量，不该让会话打不开；
            // cwd 已删仍走上方既有报错（那是权威基点）。
            let extra_roots: Vec<PathBuf> = loaded
                .meta
                .extra_roots
                .iter()
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .collect();

            let built = match with_session_model_ref(loaded.meta.model_ref.clone(), || {
                with_session_extra_roots(extra_roots.clone(), || {
                    with_session_reasoning_effort(loaded.meta.reasoning_effort.clone(), || {
                        with_session_preserve_thinking(loaded.meta.preserve_thinking, || {
                            (state.build_agent)(Some(session), Path::new(&cwd), mode)
                        })
                    })
                })
            }) {
                Ok(a) => a,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 与 `thread/start` 同款:上报/落盘的生效串取自**实际建出的 agent**
            // (`agent.config().model`),而非按清单再解析一次。工厂持有的是启动时
            // 快照,运行期新增的条目会让「另解析一次」与实际运行不一致
            // (任务 7 评审 Finding 1);不照抄 meta.model 也是为了让旧库/异常库里
            // 一个与当前清单不符的陈旧串被本次实际生效值取代。
            let model = built.agent.config().model.clone();
            // 与 `model` 同源:上报**实际建出**的 agent 的生效档位。清除覆盖
            // 后的 meta 是 `None`,但工厂此时已回落到条目默认,故响应必须报
            // 该实际值,而非照抄 meta 的 null(否则客户端挂着的控件会骗人)。
            let reasoning_effort = built.agent.config().gen_params.reasoning_effort.clone();
            // 与 `reasoning_effort` 同源:上报**实际建出**的 agent 的开关
            // 值(取自 meta,经工厂落到 config),让重开的客户端立刻拿到
            // 初始状态,而不必等一次广播。
            let preserve_thinking = built.agent.config().preserve_thinking;
            let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(Path::new(&cwd));
            let activation = attach_delegation(
                state.runtimes,
                state.thread_roots,
                &runtime_dir,
                state.models_path,
                state.cfg,
                &cwd,
                &thread_id,
                state.theme_handle,
                state.git_diff_handle,
                // 与工厂槽位**同一个值**：resume 侧的委派路径必须与
                // 重建 agent 时下发到工厂的根集一致。
                extra_roots,
                state.lead_inbox,
                state.lead_replies,
                // resume 路径：meta 已在盘上，起点取已落盘的 `anchor_sha`。
                // 旧会话没有该字段时是 `None`——那表示「无起点」，按方案 1 不回填。
                loaded.meta.anchor_sha.clone(),
                built,
            );
            let BuiltAgent {
                agent,
                provider,
                config,
                decision_tx,
                catalog,
                yolo,
                process_manager,
                ..
            } = activation.built;
            state
                .pending_activation
                .insert(thread_id.clone(), activation.runtime);

            let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
            let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
            let (interject_tx, interject_rx) = mpsc::channel::<InterjectionRequest>(16);
            let (session_tx, session_rx) = mpsc::channel::<SessionCommand>(8);
            // 同一 `Arc` 句柄:session 存一份供 `thread/list` 读,
            // driver 拿一份用于推送 `thread/status/updated`。
            let store_status = ThreadSession::new_status();
            state.threads.insert(
                thread_id.clone(),
                ThreadSession {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    active_turn_id: None,
                    yolo,
                    process_manager: Arc::clone(&process_manager),
                    prompt_tx,
                    interrupt_tx,
                    interject_tx,
                    session_tx,
                    store: Arc::clone(&thread_store),
                    status: Arc::clone(&store_status),
                },
            );

            // 与 `thread/start` 同款:先停旧守望者、再对本次生效的 manager 重订。
            // 这里正是缺陷所在——resume 一个活 thread 会新建 manager,若沿用
            // 「有守望者就不重建」的守卫,旧守望者会继续订阅被丢弃的 manager。
            if let Some(previous) = state.process_watches.remove(&thread_id) {
                previous.stop().await;
            }
            let rx = process_manager.subscribe();
            let handle = tokio::spawn(watch_processes(
                Arc::clone(state.hub),
                thread_id.clone(),
                Arc::clone(&process_manager),
                rx,
            ));
            state
                .process_watches
                .insert(thread_id.clone(), ProcessWatch { task: handle });

            let driver_hub = Arc::clone(state.hub);
            let driver_client = state.client.clone();
            let driver_turn_tx = state.turn_tx.clone();
            let driver_thread_id = thread_id.clone();
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
                Arc::clone(state.pending),
                state.permission_timeout,
                Arc::clone(state.perm_seq),
                catalog,
                Arc::clone(&thread_store),
                Arc::clone(&store_status),
                provider,
                config,
            ));

            // 回放:thread/started → 若干批量 items/completed → 最近用量 → 响应。
            write_notification(
                state.hub,
                &Notification::ThreadStarted {
                    thread_id: thread_id.clone(),
                    cwd: cwd.clone(),
                    model: model.clone(),
                    model_ref: loaded.meta.model_ref.clone(),
                },
            )
            .await?;
            // 回放：分块批量下发（每块一条 `items/completed`），经带背压的
            // 投递——本地 reliable 客户端队列满时等待而非丢帧。旧实现逐条
            // try_send,超过 256 格队列即静默丢帧,长会话尾部就此消失。
            for chunk in chunk_items_for_replay(loaded.items) {
                if let Err(e) = write_notification_await(
                    state.hub,
                    &Notification::ItemsCompleted {
                        thread_id: thread_id.clone(),
                        items: chunk,
                    },
                )
                .await
                {
                    eprintln!("[app-server] replay aborted for {thread_id}: {e}");
                    break;
                }
            }
            if let Some(u) = loaded.usage {
                write_notification(
                    state.hub,
                    &Notification::TokenUsage {
                        thread_id: thread_id.clone(),
                        model: u.model,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                    },
                )
                .await?;
            }
            // 末尾若是崩溃残留的未收尾 turn，补一条 interrupted 标记，
            // 让客户端把它显示为"这一轮被中断"，而不是当成正常轮次。
            if loaded.pending_turn {
                // 采纳的 partial 必须在这一刻**升格**进主 jsonl：它是这一轮
                // 唯一的持久副本，若不 append 就返回，用户下一条消息的 turn-start
                // checkpoint 会原子覆盖同一文件，该轮 items 就此永久丢失（而
                // session 上下文仍"记得"它们，item 与上下文就此不一致）。
                // 失败只记 stderr：恢复出不完整好过让整个 resume 报错；
                // 判据与 load 共用，重复调用不会重复 append。
                if let Err(e) = thread_store.promote_partial(&thread_id) {
                    eprintln!("[app-server] failed to promote recovered turn ({thread_id}): {e}");
                }
                write_notification(
                    state.hub,
                    &Notification::TurnCompleted {
                        thread_id: thread_id.clone(),
                        turn_id: "crashed".to_string(),
                        status: TurnStatus::Interrupted,
                        error: None,
                    },
                )
                .await?;
            }
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "thread_id": thread_id,
                        "cwd": cwd,
                        "model": model,
                        "model_ref": loaded.meta.model_ref,
                        "permission_mode": loaded.meta.permission_mode,
                        "reasoning_effort": reasoning_effort,
                        "preserve_thinking": preserve_thinking,
                    }),
                ),
            )
            .await?;
        }
        "thread/rename" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let title = req
                .params
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if title.is_empty() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("title must not be empty")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .rename(&thread_id, &title)
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/setModel" => {
            // 与 `model/*` 写路径同档:切模型会改 provider/凭据,需 `Control`。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // `name: null`(或缺失)= 清覆盖;字符串 = 选显示名。非字符串/非
            // null 一律 `-32602`,不静默当作清覆盖。
            let name = match req.params.get("name") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(n)) => Some(n.clone()),
                Some(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("name must be a string or null"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 会话覆盖必须命中清单条目,否则 `model_not_found` 且零副作用。
            // 读清单路径走注入的 `models_path`(与 `model/*` 同一份),
            // 故 upsert 刚落盘的条目立刻可见。
            let catalog = yi_agent_runtime::models::load_catalog_from(state.models_path);
            if let Some(n) = &name {
                if !catalog.models.iter().any(|m| &m.name == n) {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::model_not_found(n)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 生效串按 清单条目 → 全局默认 → cfg 解析;校验通过后再落盘。
            let effective =
                yi_agent_runtime::models::resolve_effective(state.cfg, &catalog, name.as_deref());
            // 先让在场 driver 用新覆盖重建 agent,成功后再落盘。铁律
            // 「上报/落盘 == 实际运行」:若 provider 重建失败(build_provider
            // 抛错),agent 仍跑旧模型,此时若已落盘就会让磁盘与响应谎报新模型
            // (任务 7 评审 Finding 3)。故先重建、后落盘;失败即回错误且零磁盘
            // 副作用,由客户端重试。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.model_ref 重建,故跳过本段。
            if state.threads.contains_key(&thread_id) {
                let (reply_tx, reply_rx) = oneshot::channel();
                // 借用只活在下面这个块内(send 的 future 借 `session`),
                // 出块即释放,后续 `threads.get_mut` 才不被占用。
                let send_result = {
                    let session = state
                        .threads
                        .get(&thread_id)
                        .expect("contains_key was just checked");
                    session
                        .session_tx
                        .send(SessionCommand::SetModel {
                            effective: Box::new(effective.clone()),
                            model_ref: name.clone(),
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let switched = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !switched {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch to the requested \
                                 model; it is still running the previous one"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:一次原子写把 `model_ref` 与
            // 生效 `model` 同进同出,避免并发 `touch` 读到自相矛盾的组合。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_model(&thread_id, name.as_deref(), &effective.model) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                } else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
                return Ok(Flow::Continue);
            }
            // 让内存里的 `ThreadSession.model` 与实际生效值保持一致。注意:
            // `thread/list(All)` 目前是从**盘上 meta**(`thread_summary_json`)
            // 渲染 model 的,`ThreadSession.model` 在当前 crate 内尚无读取方;
            // 这里仍同步刷新,是为了让内存视图与刚落盘的 meta 同源、不残留旧串
            // (供将来的读取方或排查时使用),而不是因为 list 会读它。
            if let Some(session) = state.threads.get_mut(&thread_id) {
                session.model = effective.model.clone();
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "model": effective.model })),
            )
            .await?;
        }
        "thread/setReasoningEffort" => {
            // 与 `thread/setModel` 同档:effort 会改推理行为(能力面/计费),
            // 需 `Control`。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // `effort: null`(或缺失)= 清覆盖;字符串必须在七档白名单内,
            // 否则 `-32602`;其余 JSON 类型同样 `-32602`,不静默当作清覆盖。
            // 白名单与 `model_rpc::upsert` 的条目校验逐字一致。
            let effort = match req.params.get("effort") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(s)) => {
                    if !matches!(
                        s.as_str(),
                        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                    ) {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(
                                id,
                                RpcError::invalid_params(
                                    "effort must be one of \
                                     none|minimal|low|medium|high|xhigh|max",
                                ),
                            ),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                    Some(s.clone())
                }
                Some(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("effort must be a string or null"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 先让在场 driver 用新 effort 重建 agent（复用当前 provider——
            // effort 不烤进 URL/key），成功后再落盘。铁律「上报/落盘 == 实际
            // 运行」同 setModel:若重建失败,agent 仍跑旧档位,此时落盘就会让
            // 磁盘与响应谎报。故先重建、后落盘;失败即回错误且零磁盘副作用。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.reasoning_effort 重建,故跳过本段。
            if state.threads.contains_key(&thread_id) {
                let (reply_tx, reply_rx) = oneshot::channel();
                // 借用只活在下面这个块内(send 的 future 借 `session`),
                // 出块即释放,后续 `threads.get_mut` 才不被占用。
                let send_result = {
                    let session = state
                        .threads
                        .get(&thread_id)
                        .expect("contains_key was just checked");
                    session
                        .session_tx
                        .send(SessionCommand::SetReasoningEffort {
                            effort: effort.clone(),
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let applied = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !applied {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch to the requested \
                                 reasoning effort; it is still running the previous one"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
            // `set_model` 同锁,避免并发 `touch` / `rename` 互相覆盖。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_reasoning_effort(&thread_id, effort.as_deref()) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                } else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
                return Ok(Flow::Continue);
            }
            // 列表层恒推(同 `thread/permissionModeChanged`):桌面改 → 手机
            // effort 控件同步,反之亦然。
            let _ = write_notification(
                state.hub,
                &Notification::ReasoningEffortChanged {
                    thread_id: thread_id.clone(),
                    effort: effort.clone(),
                },
            )
            .await;
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "effort": effort })),
            )
            .await?;
        }
        "thread/setPreserveThinking" => {
            // 与 `thread/setReasoningEffort` 同档:开关会改请求体(是否回传
            // 历史轮的推理凭证),需 `Control`。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // `preserve` 必须是 JSON 布尔;缺失/null/字符串/数字一律
            // `-32602`,**不**静默取默认值——静默默认会让调用方以为开关
            // 已按其所愿设置,而实际可能停在相反的值。
            let preserve = match req.params.get("preserve") {
                Some(serde_json::Value::Bool(b)) => *b,
                _ => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("preserve must be a boolean")),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 先让在场 driver 用新开关重建 agent(复用当前 provider——
            // 开关不烤进 URL/key),成功后再落盘。铁律「上报/落盘 == 实际
            // 运行」同 setReasoningEffort:若重建失败,agent 仍跑旧值,此时
            // 落盘就会让磁盘与响应谎报。故先重建、后落盘;失败即回错误且
            // 零磁盘副作用。
            //
            // 冷 thread(不在内存)没有 driver 可发:只能先落盘,resume 时会按
            // meta.preserve_thinking 重建,故跳过本段。
            if state.threads.contains_key(&thread_id) {
                let (reply_tx, reply_rx) = oneshot::channel();
                // 借用只活在下面这个块内(send 的 future 借 `session`),
                // 出块即释放,后续 `threads.get_mut` 才不被占用。
                let send_result = {
                    let session = state
                        .threads
                        .get(&thread_id)
                        .expect("contains_key was just checked");
                    session
                        .session_tx
                        .send(SessionCommand::SetPreserveThinking {
                            preserve,
                            reply: reply_tx,
                        })
                        .await
                };
                // reply 为 `Ok(Err(_))` 表示 driver 重建失败;发送失败(通道关闭)
                // 同样意味着无法保证生效,一并按失败处理。
                let applied = send_result.is_ok() && matches!(reply_rx.await, Ok(Ok(())));
                if !applied {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::internal(
                                "the live session could not switch the \
                                 preserve-thinking flag; it is still running the \
                                 previous value"
                                    .to_string(),
                            ),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }
            // 重建成功后(冷 thread 则直接)落盘:读-改-写 + 原子写,与
            // `set_reasoning_effort` 同锁,避免并发 `touch` / `rename` 互相覆盖。
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            if let Err(e) = store.set_preserve_thinking(&thread_id, preserve) {
                if e.kind() == std::io::ErrorKind::NotFound {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                } else {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
                return Ok(Flow::Continue);
            }
            // 列表层恒推(同 `thread/reasoningEffortChanged`):桌面改 → 手机
            // 控件同步,反之亦然。
            let _ = write_notification(
                state.hub,
                &Notification::PreserveThinkingChanged {
                    thread_id: thread_id.clone(),
                    preserve,
                },
            )
            .await;
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true, "preserve": preserve })),
            )
            .await?;
        }
        "thread/setPermissionMode" => {
            // 与 `thread/setModel` 同档:Control 起。Observe 拿到的是
            // 掩码级视图,不得放开 OS 沙箱。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // mode 校验先于 thread 存在性:非法 mode 一律 `-32602`,
            // 不因 threadId 未知而改变错误码。
            let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                Some("normal") => crate::thread_store::ThreadMode::Normal,
                Some("yolo") => crate::thread_store::ThreadMode::Yolo,
                _ => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("mode must be \"normal\" or \"yolo\""),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 仅内存中的活跃线程可切换:UI 的 mode chip 只对当前打开的线程可见,而该
            // 线程必先经 thread/start 或 thread/resume 进入内存。冷线程没有运行期
            // switch 可翻转(其持久化模式在 resume 时被读取),故此处不为其落盘。
            let Some(session) = state.threads.get(&thread_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 运行期开关立即生效:与权限层、沙箱共享同一 `Arc`,故
            // 无需重建 agent 本轮即可放行。
            session
                .yolo
                .set(mode == crate::thread_store::ThreadMode::Yolo);
            // 最佳努力落盘:运行期开关已生效,落盘失败不阻断本轮切换。
            match session.store.set_permission_mode(&thread_id, mode) {
                Ok(true) => {}
                Ok(false) => eprintln!(
                    "[app-server] permission_mode not persisted for {thread_id}: thread meta missing"
                ),
                Err(e) => {
                    eprintln!("[app-server] failed to persist permission_mode for {thread_id}: {e}")
                }
            }
            // 广播给所有客户端(列表层恒推):桌面改 → 手机 chip 同步,
            // 手机改 → 桌面 chip 同步。与 `setModel` 的 ModelChanged
            // 广播同一手法。
            let _ = write_notification(
                state.hub,
                &Notification::PermissionModeChanged {
                    thread_id: thread_id.clone(),
                    mode,
                },
            )
            .await;
            write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
        }
        "thread/setPinned" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 参数校验先于 thread 存在性:非布尔一律 `-32602`。
            let Some(pinned) = req.params.get("pinned").and_then(|v| v.as_bool()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("pinned must be a boolean")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 降序契约:now_millis 是当前最大值 → 新置顶项天然在最顶。
            let seq = if pinned {
                Some(crate::thread_store::now_millis())
            } else {
                None
            };
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .set_pin_seq(&thread_id, seq)
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/setTag" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 参数校验先于 thread 存在性：非法颜色一律 `-32602`，
            // 与 setPinned 的「非布尔先报错」同一顺序。
            let raw = req
                .params
                .get("tag")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let tag: Option<String> = match raw {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) if crate::thread_store::is_valid_tag(&s) => Some(s),
                other => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!(
                                "tag must be null or one of the 6 palette ids, got {other}"
                            )),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            match store_lookup(state.threads, state.workspaces, state.cfg, &thread_id)
                .set_tag(&thread_id, tag.as_deref())
            {
                Ok(true) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                    // 广播给所有客户端（含本端）：列表层恒推，就在地
                    // 更新色点，无需各自重拉 thread/listAll。
                    let _ = write_notification(
                        state.hub,
                        &Notification::TagChanged {
                            thread_id: thread_id.clone(),
                            tag: tag.clone(),
                        },
                    )
                    .await;
                }
                Ok(false) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                }
            }
        }
        "thread/reorderPinned" => {
            let arr = match req.params.get("threadIds").and_then(|v| v.as_array()) {
                Some(a) => a,
                None => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("threadIds must be an array of strings"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 全部必须是字符串,否则拒绝(不允许静默丢弃非字符串项)。
            let ids: Vec<String> = match arr
                .iter()
                .map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Option<Vec<_>>>()
            {
                Some(v) => v,
                None => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params("threadIds must be an array of strings"),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 校验:与「当前全部置顶集合」完全一致,且无重复。
            let pinned_now = collect_pinned(state.workspaces);
            let current: HashSet<&str> = pinned_now.iter().map(|m| m.thread_id.as_str()).collect();
            let unique: HashSet<&str> = ids.iter().map(|s| s.as_str()).collect();
            let valid = unique.len() == ids.len()
                && ids.len() == current.len()
                && ids.iter().all(|i| current.contains(i.as_str()));
            if !valid {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(
                            "threadIds must list exactly the pinned threads, no duplicates",
                        ),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let current_seq: HashMap<String, Option<i64>> = pinned_now
                .iter()
                .map(|m| (m.thread_id.clone(), m.pin_seq))
                .collect();
            let assignments = crate::thread_store::assign_pin_seqs(&ids, &current_seq);
            let mut err: Option<RpcError> = None;
            for (tid, seq) in assignments {
                let store = store_lookup(state.threads, state.workspaces, state.cfg, &tid);
                match store.set_pin_seq(&tid, Some(seq)) {
                    Ok(true) => {}
                    Ok(false) => {
                        err = Some(RpcError::unknown_thread(&tid));
                        break;
                    }
                    Err(e) => {
                        err = Some(RpcError::internal(e.to_string()));
                        break;
                    }
                }
            }
            match err {
                None => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
                }
                Some(e) => {
                    write_response(state.hub, state.client, err_response(id, e)).await?;
                }
            }
        }
        "thread/delete" => {
            // 与 `thread/rename` / `thread/setModel` 同档:Control 起。
            // Observe 是掩码级只读视图,不得删除会话。
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
            // 冷 thread 才按全局索引 / cfg.workdir 定位;存在性与删除都
            // 作用在该 thread 的真实目录上。
            let thread_store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let in_memory = state.threads.contains_key(&thread_id);
            let on_disk = thread_store.exists(&thread_id);
            if !in_memory && !on_disk {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 删除前先问一句:这个会话还有子代理在跑吗?要问在中断 turn
            // 之前——被拒的前提是不该动它分毫。子代理跑在项目 daemon 里,
            // 对话文件不是它的权威,只删文件会把它留在无人可见的地方继续跑。
            // force=true 才真的回收,由客户端在用户确认后携带。
            let force = req
                .params
                .get("force")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            // 卡片会话的 worktree 判定：只读会话自带 meta，不碰插件。
            // 顺手把 board_project 拷到局部变量：会话文件随后即被删除，
            // 到执行删除时再 load 只会失败，故必须在此刻留下。
            let mut board_project: Option<String> = None;
            let reclaim = match thread_store.load(&thread_id) {
                Ok(Some(loaded)) => {
                    board_project = loaded.meta.board_project.clone();
                    crate::worktree_reclaim::decide(
                        loaded.meta.card_id.as_deref(),
                        loaded.meta.board_project.as_deref(),
                        Path::new(&loaded.meta.cwd),
                    )
                }
                _ => crate::worktree_reclaim::WorktreeReclaim::None,
            };
            // 「需确认」= 有活跃子代理（既有）或 worktree 处置为破坏性/硬保留（新）。
            let children = if force {
                None
            } else {
                match cancel_thread_children(state.runtimes, state.threads, &thread_id, false) {
                    Ok(ThreadCancelOutcome::NeedsConfirmation(count)) => Some(count),
                    // 无事可取消时按 0 处理，交给 worktree 判定决定是否确认。
                    Ok(ThreadCancelOutcome::Cancelled(_)) => Some(0),
                    // 查询失败（如会话不在内存）按 0 处理，但照旧记日志——与既有行为一致。
                    Err(cause) => {
                        eprintln!(
                            "[app-server] could not cancel subagents for {thread_id}: {cause}"
                        );
                        Some(0)
                    }
                }
            };
            let worktree_needs_confirm = matches!(
                reclaim,
                crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { .. }
                    | crate::worktree_reclaim::WorktreeReclaim::Keep { .. }
            );
            if !force && (children.unwrap_or(0) > 0 || worktree_needs_confirm) {
                let mut payload = json!({
                    "status": "needs_confirmation",
                    "active_children": children.unwrap_or(0),
                });
                if let Some(worktree) = worktree_json(&reclaim) {
                    payload["worktree"] = worktree;
                }
                write_response(state.hub, state.client, ok_response(id, payload)).await?;
                return Ok(Flow::Continue);
            }
            if force {
                // force 下真的回收子代理（既有语义）。
                match cancel_thread_children(state.runtimes, state.threads, &thread_id, true) {
                    Ok(ThreadCancelOutcome::Cancelled(cancelled)) if cancelled > 0 => {
                        eprintln!(
                            "[app-server] cancelled {cancelled} subagent task(s) for {thread_id}"
                        );
                    }
                    Err(cause) => eprintln!(
                        "[app-server] could not cancel subagents for {thread_id}: {cause}"
                    ),
                    _ => {}
                }
            }
            // 活跃 thread:先中断,再等 driver 落盘完成才删文件。若像旧
            // 实现那样在 driver 落盘前就删文件,driver 的 `append_turn`
            // (`create(true)`)会把 `<id>.jsonl` 复活,导致删除后
            // `store.exists` 仍为真、`thread/resume` 能把已删 thread 拉回。
            // 故复用 resume 的等待模式,等落盘后再删。
            interrupt_and_wait_for_persist(
                state.threads,
                state.turn_rx,
                &thread_id,
                state.pending_lead_replies,
                state.lead_replies,
            )
            .await;
            // 落盘已结束:现在从内存移除(drop prompt_tx 让 driver 收尾)并删文件。
            state.threads.remove(&thread_id);
            state.pending_activation.remove(&thread_id);
            // End this conversation's own root before the shared binding is
            // considered for detach: the root is per conversation, the
            // binding is per project and outlives any single conversation.
            if let Some(root) = state
                .thread_roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&thread_id)
            {
                root.detach();
            }
            if let Some(watch) = state.trace_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.children_watches.remove(&thread_id) {
                watch.stop().await;
            }
            if let Some(watch) = state.process_watches.remove(&thread_id) {
                watch.stop().await;
            }
            let live_cwds = state
                .threads
                .values()
                .map(|session| session.cwd.clone())
                .collect::<Vec<_>>();
            detach_unused_runtimes(state.runtimes, &live_cwds);
            if let Err(e) = thread_store.delete(&thread_id) {
                eprintln!("[app-server] failed to delete thread files for {thread_id}: {e}");
            }
            // 收走该会话的附件:与 thread 文件同一生命周期。放在 delete
            // 之后(driver 已结束落盘),避免竞态——工作区被挪走时
            // `remove_attachments` 尽力而为,不让删除失败。这里拿不到
            // cwd(见 `detach_unused_runtimes` 的注释:不为它改
            // `store_lookup` 签名),故由 store 的 root
            // (`<cwd>/.yi-agent/threads`)上溯两级得到工作区。
            if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                crate::attachments::remove_attachments(cwd, &thread_id);
            }
            // 上传登记表也随 thread 一起 prune：否则一条 `Committed`
            // 记录会活到 10 分钟 TTL，让被删 thread 的 `uploadId` 仍能
            // 反查到已被删掉的附件路径（随后 `prepare_image_file` 读空），
            // 未 commit 的 staging 槽与进程级会话名额也被死 thread 白占。
            // `drop_thread` 只需内存与每条会话已记的 staging 路径，故这里
            // 拿不到 cwd 也无妨（已落盘的附件由上面 `remove_attachments` 收）。
            state.uploads.drop_thread(&thread_id);
            // 会话文件已摘除：按判定处置 worktree。删除失败只记日志，不阻断会话删除（D9）。
            // board_project 已在进分支时留好——此处绝不能再 load（会话文件已不存在）。
            // 但失败这件事本身必须回报：用户刚被告知该 worktree 会被删掉，此刻会话
            // 已摘除、应用内再也删不掉它，又按 D10 让卡片上的 thread_id 悬空——
            // 若默默失败，worktree 就无声滞留、用户永远看不见。故除宿主日志外，
            // 把这一次"该删却没删成"如实回给客户端（失败才现身，成功仍为空）。
            let mut worktree_failure: Option<serde_json::Value> = None;
            if let (
                crate::worktree_reclaim::WorktreeReclaim::Delete { path }
                | crate::worktree_reclaim::WorktreeReclaim::DestructiveDelete { path, .. },
                Some(project),
            ) = (&reclaim, board_project.as_deref())
            {
                if let Err(error) = crate::worktree_reclaim::remove(path, Path::new(project), force)
                {
                    eprintln!(
                        "[app-server] could not remove the worktree for {thread_id}: {error}"
                    );
                    worktree_failure = Some(json!({
                        "action": "remove",
                        "removed": false,
                        "path": path.to_string_lossy(),
                        "reason": error,
                    }));
                }
            }
            // 只有"被承诺会删、且确实没删成"才带字段；普通会话（reclaim=None）、
            // Keep、以及删除成功一律仍是空成功体，既有形状零变化。
            let body = match worktree_failure {
                Some(worktree) => json!({ "worktree": worktree }),
                None => json!({}),
            };
            write_response(state.hub, state.client, ok_response(id, body)).await?;
        }
        "thread/clear" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let Some(session) = state.threads.get(&thread_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 清空跑在半个 turn 上会产出不自洽的历史，直接拒绝。
            if session.active_turn_id.is_some() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            if session
                .session_tx
                .send(SessionCommand::Clear { reply: reply_tx })
                .await
                .is_err()
            {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::internal("thread driver is gone")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match reply_rx.await {
                Ok(Ok(())) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?
                }
                Ok(Err(message)) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(message)),
                    )
                    .await?
                }
                Err(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal("thread driver dropped")),
                    )
                    .await?
                }
            }
        }
        "thread/compact" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let Some(session) = state.threads.get(&thread_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::unknown_thread(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 压缩会重写整段历史,turn 进行中不接受。
            if session.active_turn_id.is_some() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            if session
                .session_tx
                .send(SessionCommand::Compact { reply: reply_tx })
                .await
                .is_err()
            {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::internal("thread driver is gone")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // compact 要调一次 provider 生成摘要,属于长请求;不设短超时。
            let result = match reply_rx.await {
                Ok(CompactOutcome::Compacted) => json!({"status": "compacted"}),
                Ok(CompactOutcome::NotReduced) => json!({"status": "not_reduced"}),
                Ok(CompactOutcome::Failed(message)) => {
                    json!({"status": "failed", "error": message})
                }
                Err(_) => json!({"status": "failed", "error": "thread driver dropped"}),
            };
            write_response(state.hub, state.client, ok_response(id, result)).await?;
        }
        "thread/diff/read" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            if !require_known_thread(
                state.hub,
                state.client,
                state.threads,
                &thread_id,
                id.clone(),
            )
            .await?
            {
                return Ok(Flow::Continue);
            }
            // cwd 是 diff 的唯一来源：thread 的 cwd，缺省兜底 cfg.workdir。
            let cwd = state
                .threads
                .get(&thread_id)
                .map(|t| t.cwd.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| state.cfg.workdir.display().to_string());
            let repo = Path::new(&cwd);
            let base = req.params.get("base").and_then(|v| v.as_str());
            let result = if let Some(commit) = req.params.get("commit").and_then(|v| v.as_str()) {
                let (unified_diff, truncated) = crate::git_diff::commit_diff(repo, commit);
                json!({ "unifiedDiff": unified_diff, "truncated": truncated })
            } else if let Some(path) = req.params.get("path").and_then(|v| v.as_str()) {
                // 锚点由 Task 2 从 thread meta 接上；本任务先传 `None`（行为不变）。
                let d = crate::git_diff::thread_diff(repo, base, None);
                let (unified_diff, truncated) =
                    crate::git_diff::file_diff(repo, d.merge_base.as_deref(), path);
                json!({ "unifiedDiff": unified_diff, "truncated": truncated })
            } else {
                serde_json::to_value(crate::git_diff::thread_diff(repo, base, None))
                    .unwrap_or(json!(null))
            };
            write_response(state.hub, state.client, ok_response(id, result)).await?;
        }
        "thread/view/read" => {
            // 读回**帧**（模型给命令 / 结构化默认），与 `thread/diff/read` 的
            // 「结构化取数」是两条不同的路径，故是两个方法而不是一个方法的开关。
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            if !require_known_thread(
                state.hub,
                state.client,
                state.threads,
                &thread_id,
                id.clone(),
            )
            .await?
            {
                return Ok(Flow::Continue);
            }
            let refresh = req
                .params
                .get("refresh")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if refresh {
                // 重算这一帧：**分叉**在「这一帧原本是命令帧还是结构化默认」上。
                // 命令帧重跑同一条命令（模型可重复观察同一视角）；结构化默认则
                // 重算锚点 diff（工作区在这期间变了，重读必须反映新内容）。
                let cwd = state
                    .threads
                    .get(&thread_id)
                    .map(|t| t.cwd.clone())
                    .filter(|c| !c.is_empty())
                    .unwrap_or_else(|| state.cfg.workdir.display().to_string());
                let repo = Path::new(&cwd);
                let anchor = state
                    .threads
                    .get(&thread_id)
                    .map(|t| t.store.load(&thread_id).ok().flatten())
                    .flatten()
                    .and_then(|l| l.meta.anchor_sha);
                // 分两次短暂持锁，**不**把 guard 跨过 `command_frame` 的 await：
                // `serve()` 跑在 `tokio::spawn` 里，guard 过 await 会让整个
                // future 非 `Send`，直接编译不过。
                // 快照连**基准来历**一起取：结构化帧可能是「模型指定的基准」
                // (`base_kind == "explicit"`)，重算时必须原样再用那个 base——
                // 只传 `explicit=None` 会让它退化成会话锚点，帧与回执就从
                // 「跟 origin/main 比」变成「本次会话以来」，正是 D3 的误导。
                let snapshot = state
                    .view_frames
                    .lock()
                    .await
                    .get(&thread_id)
                    .map(|f| (f.command.clone(), f.base.clone(), f.base_kind.clone()));
                let command = snapshot.as_ref().and_then(|(c, _, _)| c.clone());
                let explicit = snapshot.as_ref().and_then(|(_, base, kind)| {
                    (kind == "explicit").then(|| base.clone()).flatten()
                });
                let next = match command.as_deref() {
                    Some(command) => {
                        crate::git_diff::command_frame(repo, command, None, None).await
                    }
                    None => crate::git_diff::structured_frame(
                        repo,
                        explicit.as_deref(),
                        anchor.as_deref(),
                    ),
                };
                // 写回用 CAS，不是无条件覆盖：「读缓存命令」与「写回新帧」之间隔了
                // 一次 await（最多 `VIEW_TIMEOUT`）。期间别的写入者（另一次 refresh、
                // 或 Task 4 的 watcher）可能已经写了更新的帧；若无条件覆盖，面板就会
                // 显示与「刚刚请求的命令」不符的内容——正是 D3 要杜绝的误导。
                // 判定：槽里的命令仍等于我们读到的快照才写。
                // 不覆盖的情形：同一命令连续两次 refresh 无法区分，但那是同一条命令，
                // 无害；命令一旦不同，一定让位给更晚的写入者（下面的 result 会读到它）。
                let mut frames = state.view_frames.lock().await;
                let unchanged =
                    frames.get(&thread_id).and_then(|f| f.command.as_deref()) == command.as_deref();
                if unchanged {
                    frames.insert(thread_id.clone(), next);
                }
                drop(frames);
            }
            let result = state
                .view_frames
                .lock()
                .await
                .get(&thread_id)
                .map(|f| serde_json::to_value(f).unwrap_or(json!(null)))
                .unwrap_or(json!(null));
            write_response(state.hub, state.client, ok_response(id, result)).await?;
        }
        "thread/subscribe" => {
            // 整体替换该客户端的订阅集合。**不在 ADMIN_METHODS 内**:
            // 任何已初始化客户端都能收窄自己的 feed。未调用过的客户端
            // 保持 `Feed::All`(桌面行为不变)。
            let ids: Option<Vec<String>> = req
                .params
                .get("threadIds")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                });
            let Some(ids) = ids else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing threadIds")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            if ids.len() > crate::broadcast::MAX_SUBSCRIPTIONS {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(format!(
                            "at most {} threadIds",
                            crate::broadcast::MAX_SUBSCRIPTIONS
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            state.hub.subscribe(state.client, ids.clone());
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "subscribed": ids })),
            )
            .await?;
        }
        "thread/readItems" => {
            // 只读补齐：读已落盘的 items，**不 resume、不重建 agent、
            // 不中断正在跑的回合**（这正是它相对 thread/resume 的意义）。
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            let after = req
                .params
                .get("afterItemId")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let store = store_lookup(state.threads, state.workspaces, state.cfg, &thread_id);
            let loaded = match store.load(&thread_id) {
                Ok(Some(l)) => l,
                Ok(None) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::unknown_thread(&thread_id)),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(e.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            let items: Vec<crate::protocol::Item> = match after {
                Some(aid) => match loaded
                    .items
                    .iter()
                    .position(|it| item_id(it) == Some(aid.as_str()))
                {
                    // 找到锚点：只给它之后的部分。
                    Some(i) => loaded.items.into_iter().skip(i + 1).collect(),
                    // 锚点缺失（可能被 compact 丢弃）：返回全部，由客户端按 id 去重。
                    None => loaded.items,
                },
                None => loaded.items,
            };
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "items": items })),
            )
            .await?;
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
