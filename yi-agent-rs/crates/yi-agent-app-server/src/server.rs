//! app-server 主循环:读请求 → 分发 → 写响应/通知。
//!
//! 本模块实现协议主循环:`initialize` / `thread/start` / `config/read`,以及
//! `turn/start` / `turn/interrupt`。每个 thread 有一个独立的 driver task,
//! 串行消费 turn、驱动 `agent.run()` 的事件流,并经 `Translator` 写成协议通知。
//! 另有 `not_initialized` / `method_not_found` / 解析错误 / stdin EOF 优雅退出。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::StreamExt;
use serde_json::json;
use tokio::sync::{Mutex, mpsc, oneshot};

use yi_agent_core::permission::Decision;
use yi_agent_runtime::config::RuntimeConfig;

use crate::protocol::{
    ClientResponse, JSONRPC_VERSION, Notification, NotificationEnvelope, PROTOCOL_VERSION,
    RequestEnvelope, RequestId, ResponseEnvelope, ReverseRequest, RpcError,
};
use crate::session::{ThreadSession, TurnPrompt};
use crate::translate::Translator;
use crate::transport::{MessageReader, MessageWriter};
use crate::workspace_index::WorkspaceIndex;

/// 权限审批等待客户端响应的默认超时;超时按 Deny 处理。
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

/// driver task → 主循环的完成事件。
enum TurnEvent {
    Finished { thread_id: String, turn_id: String },
}

/// 工厂产出的 agent 及其权限决定通道。
struct BuiltAgent {
    agent: yi_agent_core::Agent,
    /// 交互模式下的权限决定回传端;None 表示该 agent 不需要审批。
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    /// 刷新 skills catalog 的句柄;无 skills 服务时为 `None`。
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    /// 该 agent 的运行时 yolo 开关;`ThreadSession` 存它以便 RPC 即时切换。
    yolo: yi_agent_core::autonomy::YoloSwitch,
}

/// app-server 入口:在 stdio(或任意读写流)上跑 JSON-RPC 主循环。
///
/// `cfg` 同时用于 `config/read` 响应与(每个 thread 的)`bootstrap_agent`。
pub async fn run<R, W>(reader: R, writer: W, cfg: RuntimeConfig) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let cfg_for_factory = cfg.clone();
    let workspaces = Arc::new(WorkspaceIndex::new(crate::workspace_index::default_path()));
    run_with(
        reader,
        writer,
        cfg,
        PERMISSION_TIMEOUT,
        workspaces,
        move |session, cwd, mode| {
            let mut thread_cfg = cfg_for_factory.clone();
            thread_cfg.workdir = cwd.to_path_buf();
            thread_cfg.yolo = mode == crate::thread_store::ThreadMode::Yolo;
            let built = yi_agent_runtime::bootstrap::bootstrap_agent(
                &thread_cfg,
                yi_agent_runtime::bootstrap::PermissionMode::Interactive,
            )?;
            Ok(BuiltAgent {
                agent: apply_session(built.agent, session),
                decision_tx: built.decision_tx,
                catalog: built.catalog,
                yolo: built.yolo,
            })
        },
    )
    .await
}

/// 主循环的可测核心:agent 工厂由调用方注入(测试用 mock provider)。
///
/// **取消安全**:`MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,
/// 因此这里不直接在 `select!` 上轮询它;而是 spawn 一个独占 reader 的读取任务,
/// 把完整行转发到 channel,主循环只 select 两个 `recv`(均 cancel-safe)。
async fn run_with<R, W, F>(
    reader: R,
    writer: W,
    cfg: RuntimeConfig,
    permission_timeout: Duration,
    workspaces: Arc<WorkspaceIndex>,
    build_agent: F,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>
        + Send
        + 'static,
{
    // channel 里携带 `Result`,区分「读到一行」「EOF(channel 关闭)」与
    // 「读/传输错误」。若不区分,超大帧或 broken pipe 会被误当成干净 EOF。
    let (req_tx, mut req_rx) = mpsc::channel::<anyhow::Result<String>>(64);
    tokio::spawn(async move {
        let mut reader = MessageReader::new(reader);
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    if req_tx.send(Ok(line)).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // EOF → 丢弃 req_tx → 主循环优雅退出
                Err(e) => {
                    tracing::error!("app-server read error: {e}");
                    let _ = req_tx.send(Err(e)).await;
                    break;
                }
            }
        }
        // 丢弃 req_tx → 主循环的 req_rx.recv() 返回 None,触发优雅退出。
    });

    let writer = Arc::new(MessageWriter::new(writer));
    // driver task 会 clone 该 sender 上报 turn 完成事件;主循环持有它,
    // 保证 `turn_rx` 不会提前关闭。
    let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(64);

    // 反向权限请求的等待登记表:key 为 `perm-<seq>`,value 为向 driver
    // 回传决定的 oneshot 端。主循环在读到客户端响应时据此路由。
    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    // 审批 id 必须跨 thread 全局唯一:每个 thread 的 PermissionChecker 都从
    // request_id=1 开始,若用 request_id 直接做 key 会跨 thread 碰撞。
    let perm_seq = Arc::new(AtomicU64::new(1));

    let mut initialized = false;
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();

    loop {
        tokio::select! {
            line = req_rx.recv() => {
                let Some(item) = line else { break }; // EOF → graceful exit
                let line = match item {
                    Ok(l) => l,
                    Err(e) => {
                        // 尽力告知客户端我们为何退出,再把失败向上抛出,
                        // 避免传输错误被伪装成干净退出。
                        let _ = write_response(
                            &writer,
                            err_response(
                                RequestId::Num(0),
                                RpcError::invalid_request(format!("transport error: {e}")),
                            ),
                        )
                        .await;
                        return Err(e);
                    }
                };
                if line.trim().is_empty() {
                    continue;
                }
                let value: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("app-server parse error: {e}");
                        // 畸形帧里没有可用的 id。JSON-RPC 2.0 要求此时响应
                        // 的 id 为 `null`,但 `RequestId` 目前没有 Null 变体,
                        // 故暂以 id 0 代替(见 docs/bug-list.md)。
                        write_response(
                            &writer,
                            err_response(RequestId::Num(0), RpcError::parse_error(e.to_string())),
                        )
                        .await?;
                        continue;
                    }
                };
                let req: RequestEnvelope = match serde_json::from_value(value.clone()) {
                    Ok(r) => r,
                    Err(_) => {
                        // 没有 `method`:可能是客户端对反向请求的响应。必须带
                        // `result` 或 `error` 才算响应,否则视为畸形帧报错。
                        match serde_json::from_value::<ClientResponse>(value) {
                            Ok(resp) if resp.result.is_some() || resp.error.is_some() => {
                                route_client_response(resp, &pending).await;
                            }
                            _ => {
                                write_response(
                                    &writer,
                                    err_response(
                                        RequestId::Num(0),
                                        RpcError::parse_error("malformed frame"),
                                    ),
                                )
                                .await?;
                            }
                        }
                        continue;
                    }
                };
                let id = req.id.clone();
                let method = req.method.clone();

                // 未 initialize 前,除 `initialize` 外的请求一律拒绝。
                if !initialized && method != "initialize" {
                    write_response(&writer, err_response(id, RpcError::not_initialized())).await?;
                    continue;
                }

                match method.as_str() {
                    "initialize" => {
                        initialized = true;
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "serverInfo": {
                                        "name": "yi-agent-app-server",
                                        "version": env!("CARGO_PKG_VERSION"),
                                    },
                                    "protocolVersion": PROTOCOL_VERSION,
                                    "capabilities": {},
                                }),
                            ),
                        )
                        .await?;
                    }
                    "config/read" => {
                        write_response(&writer, ok_response(id, cfg.redacted_view())).await?;
                    }
                    "workspace/list" => {
                        let list: Vec<serde_json::Value> = workspaces
                            .list()
                            .into_iter()
                            .map(|p| json!({ "path": p, "exists": Path::new(&p).is_dir() }))
                            .collect();
                        write_response(&writer, ok_response(id, json!({ "workspaces": list })))
                            .await?;
                    }
                    "workspace/add" => {
                        let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        match std::fs::canonicalize(raw) {
                            Ok(p) if p.is_dir() => {
                                let value = p.to_string_lossy().to_string();
                                match workspaces.add(&p) {
                                    Ok(()) => {
                                        write_response(&writer, ok_response(id, json!({ "path": value })))
                                            .await?
                                    }
                                    Err(e) => {
                                        write_response(
                                            &writer,
                                            err_response(id, RpcError::internal(e.to_string())),
                                        )
                                        .await?
                                    }
                                }
                            }
                            _ => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("path is not a directory"),
                                    ),
                                )
                                .await?;
                            }
                        }
                    }
                    "workspace/remove" => {
                        let raw = req.params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                        // add 存的是 canonical 路径;remove 也必须规范化,否则
                        // 符号链接 / 相对路径 / 尾斜杠会静默 no-op 却仍回成功。
                        // canonicalize 失败(路径已不存在)时回退原始串,保持幂等。
                        let key = std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw));
                        match workspaces.remove(&key) {
                            Ok(()) => write_response(&writer, ok_response(id, json!({}))).await?,
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?
                            }
                        }
                    }
                    "thread/list" => {
                        // 单目录列表:仍是 `cfg.workdir` 的 store。跨目录分组见下方
                        // `thread/listAll`。
                        let store = crate::thread_store::ThreadStore::new(&cfg.workdir);
                        match store.list() {
                        Ok(metas) => {
                            // 显式映射而非直接序列化 ThreadMeta:wire 契约与存储结构解耦,
                            // 存储字段重命名不会悄悄改变 RPC 输出。
                            let threads: Vec<serde_json::Value> = metas
                                .into_iter()
                                .map(|m| {
                                    json!({
                                        "thread_id": m.thread_id,
                                        "cwd": m.cwd,
                                        "model": m.model,
                                        "created_at": m.created_at,
                                        "updated_at": m.updated_at,
                                        "title": m.title,
                                        "permission_mode": m.permission_mode,
                                    })
                                })
                                .collect();
                            write_response(&writer, ok_response(id, json!({ "threads": threads })))
                                .await?;
                        }
                        Err(e) => {
                            write_response(&writer, err_response(id, RpcError::internal(e.to_string())))
                                .await?;
                        }
                        }
                    }
                    "thread/listAll" => {
                        // 跨目录汇总:按索引顺序(最近的在前)遍历每个 workspace,
                        // 组内是该目录 store 的 thread(updated_at 降序)。
                        // 失效目录先 stat 跳过,不做深扫,但仍报表该组(exists:false),
                        // 供侧栏置灰展示。
                        let mut groups: Vec<serde_json::Value> = Vec::new();
                        for dir in workspaces.list() {
                            let path = Path::new(&dir);
                            let exists = path.is_dir();
                            let threads: Vec<serde_json::Value> = if exists {
                                // 读取错误(如权限拒绝)不能静默等同于「无 thread」:
                                // 记 stderr 后再降级为空组,与 thread/list 的错误可见性一致。
                                match crate::thread_store::ThreadStore::new(path).list() {
                                    Ok(metas) => metas
                                        .into_iter()
                                        .map(|m| {
                                            json!({
                                                "thread_id": m.thread_id,
                                                "cwd": m.cwd,
                                                "model": m.model,
                                                "created_at": m.created_at,
                                                "updated_at": m.updated_at,
                                                "title": m.title,
                                                "permission_mode": m.permission_mode,
                                            })
                                        })
                                        .collect(),
                                    Err(e) => {
                                        eprintln!(
                                            "[app-server] thread/listAll failed to list {dir}: {e}"
                                        );
                                        Vec::new()
                                    }
                                }
                            } else {
                                Vec::new()
                            };
                            groups.push(json!({
                                "workspace": dir,
                                "exists": exists,
                                "threads": threads,
                            }));
                        }
                        write_response(&writer, ok_response(id, json!({ "groups": groups })))
                            .await?;
                    }
                    "thread/start" => {
                        let thread_id = format!("thread-{}", uuid::Uuid::new_v4());

                        // 目录决定 agent / store / 权限 / 沙箱 / skills 的根。
                        let cwd = match resolve_thread_cwd(&req.params, &cfg, &writer, id.clone()).await? {
                            Some(c) => c,
                            None => continue,
                        };
                        let thread_store =
                            Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

                        // 新线程一律以 Normal 起步;同一值既用于建 agent,也落盘 meta,
                        // 抽成局部量避免两处字面量漂移。
                        let mode = crate::thread_store::ThreadMode::Normal;

                        let BuiltAgent { agent, decision_tx, catalog, yolo } =
                            match build_agent(None, Path::new(&cwd), mode) {
                                Ok(a) => a,
                                Err(e) => {
                                    write_response(&writer, err_response(id, RpcError::internal(e.to_string()))).await?;
                                    continue;
                                }
                            };

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);

                        let model = cfg.model.clone();

                        let now = crate::thread_store::now_millis();
                        let meta = crate::thread_store::ThreadMeta {
                            thread_id: thread_id.clone(),
                            cwd: cwd.clone(),
                            model: model.clone(),
                            created_at: now,
                            updated_at: now,
                            title: None,
                            permission_mode: mode,
                        };
                        if let Err(e) = thread_store.create(&meta) {
                            // 持久化是尽力而为:写失败不阻断 thread 创建。
                            eprintln!("[app-server] failed to create thread meta for {thread_id}: {e}");
                        }
                        // 记录到全局「最近目录」索引,供 thread/listAll 与侧栏复用。
                        // 索引键统一为 canonical,与 workspace/remove 的规范化对齐;
                        // thread 的 cwd / meta 仍保持 `resolve_thread_cwd` 的原样。
                        let index_path =
                            std::fs::canonicalize(&cwd).unwrap_or_else(|_| PathBuf::from(&cwd));
                        if let Err(e) = workspaces.add(&index_path) {
                            eprintln!(
                                "[app-server] failed to record workspace {}: {e}",
                                index_path.display()
                            );
                        }

                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                yolo,
                                prompt_tx,
                                interrupt_tx,
                                store: Arc::clone(&thread_store),
                            },
                        );

                        // 每个 thread 一个 driver task:独占 agent 与两个 receiver,
                        // 串行驱动 turn。
                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            driver_writer,
                            driver_turn_tx,
                            decision_tx,
                            Arc::clone(&pending),
                            permission_timeout,
                            Arc::clone(&perm_seq),
                            catalog,
                            Arc::clone(&thread_store),
                        ));

                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "thread/resume" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // Design §8:内存中的 thread 知道其权威 cwd,若目标目录已被删除,
                        // 必须明确报错,而不是经 store_lookup 回退到 cfg.workdir(那会让
                        // 后续 load 落空,退化成含混的 -32011,甚至悄悄换到别的目录)。
                        // 冷 thread 的 meta 存在 workspace 目录内,删目录后本就不可读,
                        // 仍走下方的 -32011,不在此覆盖。
                        if let Some(session) = threads.get(&thread_id) {
                            if !Path::new(&session.cwd).is_dir() {
                                write_response(
                                    &writer,
                                    err_response(
                                        id.clone(),
                                        RpcError::invalid_params(format!(
                                            "working directory no longer exists: {}",
                                            session.cwd
                                        )),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        }
                        // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
                        // 冷 thread 才按全局索引 / cfg.workdir 定位,再从中载入历史。
                        let thread_store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        // 若该 thread 仍在内存且有活跃 turn,先请求中断,再等待 driver
                        // 落盘完成,否则紧随 turn/completed 的 resume 会读到尚未写入
                        // 的历史。详见 `interrupt_and_wait_for_persist`。
                        interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;

                        let loaded = match thread_store.load(&thread_id) {
                            Ok(Some(l)) => l,
                            Ok(None) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };

                        // 该 thread 持久化的自主权模式:决定重建 agent 时的 yolo 初值。
                        let mode = loaded.meta.permission_mode;

                        // meta 缺 cwd/model 时(损坏重建)用当前配置兜底。
                        let cwd = if loaded.meta.cwd.is_empty() {
                            cfg.workdir.display().to_string()
                        } else {
                            loaded.meta.cwd.clone()
                        };
                        let model = if loaded.meta.model.is_empty() {
                            cfg.model.clone()
                        } else {
                            loaded.meta.model.clone()
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
                        let thread_store =
                            Arc::new(crate::thread_store::ThreadStore::new(Path::new(&cwd)));

                        let BuiltAgent { agent, decision_tx, catalog, yolo } =
                            match build_agent(Some(session), Path::new(&cwd), mode) {
                            Ok(a) => a,
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                                continue;
                            }
                        };

                        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
                        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
                        threads.insert(
                            thread_id.clone(),
                            ThreadSession {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                                active_turn_id: None,
                                yolo,
                                prompt_tx,
                                interrupt_tx,
                                store: Arc::clone(&thread_store),
                            },
                        );

                        let driver_writer = Arc::clone(&writer);
                        let driver_turn_tx = turn_tx.clone();
                        let driver_thread_id = thread_id.clone();
                        tokio::spawn(run_thread_driver(
                            driver_thread_id,
                            agent,
                            prompt_rx,
                            interrupt_rx,
                            driver_writer,
                            driver_turn_tx,
                            decision_tx,
                            Arc::clone(&pending),
                            permission_timeout,
                            Arc::clone(&perm_seq),
                            catalog,
                            Arc::clone(&thread_store),
                        ));

                        // 回放:thread/started → 每条历史 item/completed → 最近用量 → 响应。
                        write_notification(
                            &writer,
                            &Notification::ThreadStarted {
                                thread_id: thread_id.clone(),
                                cwd: cwd.clone(),
                                model: model.clone(),
                            },
                        )
                        .await?;
                        for item in loaded.items {
                            write_notification(
                                &writer,
                                &Notification::ItemCompleted {
                                    thread_id: thread_id.clone(),
                                    item,
                                },
                            )
                            .await?;
                        }
                        if let Some(u) = loaded.usage {
                            write_notification(
                                &writer,
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
                        write_response(
                            &writer,
                            ok_response(
                                id,
                                json!({
                                    "thread_id": thread_id,
                                    "cwd": cwd,
                                    "model": model,
                                }),
                            ),
                        )
                        .await?;
                    }
                    "thread/rename" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
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
                                &writer,
                                err_response(id, RpcError::invalid_params("title must not be empty")),
                            )
                            .await?;
                            continue;
                        }
                        match store_lookup(&threads, &workspaces, &cfg, &thread_id)
                            .rename(&thread_id, &title)
                        {
                            Ok(true) => {
                                write_response(&writer, ok_response(id, json!({}))).await?;
                            }
                            Ok(false) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                            }
                            Err(e) => {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::internal(e.to_string())),
                                )
                                .await?;
                            }
                        }
                    }
                    "thread/setPermissionMode" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // mode 校验先于 thread 存在性:非法 mode 一律 `-32602`,
                        // 不因 threadId 未知而改变错误码。
                        let mode = match req.params.get("mode").and_then(|v| v.as_str()) {
                            Some("normal") => crate::thread_store::ThreadMode::Normal,
                            Some("yolo") => crate::thread_store::ThreadMode::Yolo,
                            _ => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params(
                                            "mode must be \"normal\" or \"yolo\"",
                                        ),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };
                        // 仅内存中的活跃线程可切换:UI 的 mode chip 只对当前打开的线程可见,而该
                        // 线程必先经 thread/start 或 thread/resume 进入内存。冷线程没有运行期
                        // switch 可翻转(其持久化模式在 resume 时被读取),故此处不为其落盘。
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        // 运行期开关立即生效:与权限层、沙箱共享同一 `Arc`,故
                        // 无需重建 agent 本轮即可放行。
                        session.yolo.set(mode == crate::thread_store::ThreadMode::Yolo);
                        // 最佳努力落盘:运行期开关已生效,落盘失败不阻断本轮切换。
                        match session.store.set_permission_mode(&thread_id, mode) {
                            Ok(true) => {}
                            Ok(false) => eprintln!(
                                "[app-server] permission_mode not persisted for {thread_id}: thread meta missing"
                            ),
                            Err(e) => eprintln!(
                                "[app-server] failed to persist permission_mode for {thread_id}: {e}"
                            ),
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    "thread/delete" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        // 优先用内存中该 thread 的权威 store(与其 driver 共享 lock),
                        // 冷 thread 才按全局索引 / cfg.workdir 定位;存在性与删除都
                        // 作用在该 thread 的真实目录上。
                        let thread_store = store_lookup(&threads, &workspaces, &cfg, &thread_id);
                        let in_memory = threads.contains_key(&thread_id);
                        let on_disk = thread_store.exists(&thread_id);
                        if !in_memory && !on_disk {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        }
                        // 活跃 thread:先中断,再等 driver 落盘完成才删文件。若像旧
                        // 实现那样在 driver 落盘前就删文件,driver 的 `append_turn`
                        // (`create(true)`)会把 `<id>.jsonl` 复活,导致删除后
                        // `store.exists` 仍为真、`thread/resume` 能把已删 thread 拉回。
                        // 故复用 resume 的等待模式,等落盘后再删。
                        interrupt_and_wait_for_persist(&mut threads, &mut turn_rx, &thread_id).await;
                        // 落盘已结束:现在从内存移除(drop prompt_tx 让 driver 收尾)并删文件。
                        threads.remove(&thread_id);
                        if let Err(e) = thread_store.delete(&thread_id) {
                            eprintln!("[app-server] failed to delete thread files for {thread_id}: {e}");
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    "turn/start" => {
                        // `id` 后续响应仍需使用,故传 clone。
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let prompt = match extract_prompt(&req.params) {
                            Some(p) => p,
                            None => {
                                write_response(
                                    &writer,
                                    err_response(
                                        id,
                                        RpcError::invalid_params("missing or empty input text"),
                                    ),
                                )
                                .await?;
                                continue;
                            }
                        };

                        let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
                        // 内层作用域:让 `&mut threads` 的借用先结束,后续错误
                        // 路径才能再次 `threads.get_mut`。
                        let prompt_tx = {
                            let Some(session) = threads.get_mut(&thread_id) else {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::unknown_thread(&thread_id)),
                                )
                                .await?;
                                continue;
                            };
                            if session.active_turn_id.is_some() {
                                write_response(
                                    &writer,
                                    err_response(id, RpcError::turn_in_progress(&thread_id)),
                                )
                                .await?;
                                continue;
                            }
                            session.active_turn_id = Some(turn_id.clone());
                            session.prompt_tx.clone()
                        };

                        // 顺序确定:先 turn/started 通知,再响应,最后投递 prompt。
                        write_notification(
                            &writer,
                            &Notification::TurnStarted {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                            },
                        )
                        .await?;
                        write_response(
                            &writer,
                            ok_response(id, json!({ "turn_id": turn_id.clone() })),
                        )
                        .await?;

                        if prompt_tx.send(TurnPrompt { turn_id, prompt }).await.is_err() {
                            // driver 已退出(理论上不会):清掉活跃标记,
                            // 避免后续 turn 永远报 turn_in_progress。
                            if let Some(s) = threads.get_mut(&thread_id) {
                                s.active_turn_id = None;
                            }
                        }
                    }
                    "turn/interrupt" => {
                        let Some(thread_id) =
                            require_thread_id(&writer, &req.params, id.clone()).await?
                        else {
                            continue;
                        };
                        let Some(session) = threads.get(&thread_id) else {
                            write_response(
                                &writer,
                                err_response(id, RpcError::unknown_thread(&thread_id)),
                            )
                            .await?;
                            continue;
                        };
                        if let Some(turn_id) = session.active_turn_id.clone() {
                            // 幂等 level 信号:用 try_send 避免主循环在满队列上阻塞。
                            let _ = session.interrupt_tx.try_send(turn_id);
                        }
                        write_response(&writer, ok_response(id, json!({}))).await?;
                    }
                    _ => {
                        write_response(&writer, err_response(id, RpcError::method_not_found(&method)))
                            .await?;
                    }
                }
            }
            ev = turn_rx.recv() => {
                if let Some(TurnEvent::Finished { thread_id, turn_id }) = ev {
                    if let Some(s) = threads.get_mut(&thread_id) {
                        // 仅当完成的正是当前活跃 turn 时才清除,避免迟到的
                        // 完成事件误清掉已开始的下一个 turn。
                        if s.active_turn_id.as_deref() == Some(turn_id.as_str()) {
                            s.active_turn_id = None;
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// 恢复会话:`Some` 时用载入的历史覆盖 agent 的 session,`None` 时保持新建的空 session。
fn apply_session(
    agent: yi_agent_core::Agent,
    session: Option<yi_agent_core::Session>,
) -> yi_agent_core::Agent {
    match session {
        Some(s) => agent.with_session(s),
        None => agent,
    }
}

fn ok_response(id: RequestId, result: serde_json::Value) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: Some(result),
        error: None,
    }
}

fn err_response(id: RequestId, error: RpcError) -> ResponseEnvelope {
    ResponseEnvelope {
        jsonrpc: Some(JSONRPC_VERSION.to_string()),
        id,
        result: None,
        error: Some(error),
    }
}

async fn write_response<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    resp: ResponseEnvelope,
) -> anyhow::Result<()> {
    writer.write_value(&resp).await
}

async fn write_notification<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    n: &Notification,
) -> anyhow::Result<()> {
    writer.write_value(&NotificationEnvelope::new(n)).await
}

/// 把客户端对反向请求的响应路由到等待中的 driver。
async fn route_client_response(
    resp: ClientResponse,
    pending: &Mutex<HashMap<String, oneshot::Sender<Decision>>>,
) {
    let RequestId::Str(key) = resp.id else {
        tracing::warn!("ignoring client response with non-string id");
        return;
    };
    let Some(tx) = pending.lock().await.remove(&key) else {
        tracing::warn!("no pending approval request for id {key}");
        return;
    };
    let _ = tx.send(parse_client_decision(resp.result.as_ref()));
}

/// 解析客户端的权限决定;任何未知/畸形取值一律按 Deny 处理(fail-safe)。
fn parse_client_decision(result: Option<&serde_json::Value>) -> Decision {
    let Some(result) = result else {
        return Decision::Deny;
    };
    match result.get("decision").and_then(|d| d.as_str()) {
        Some("allow_once") => Decision::AllowOnce,
        Some("always_allow_tool") => Decision::AlwaysAllowTool,
        Some("always_allow_prefix") => match result.get("prefix").and_then(|p| p.as_str()) {
            Some(p) => Decision::AlwaysAllowPrefix(p.to_string()),
            None => Decision::Deny,
        },
        _ => Decision::Deny,
    }
}

/// 构造一个 `TurnEvent::Finished`,避免在 driver 里重复拼字段。
fn finished_event(thread_id: &str, turn_id: &str) -> TurnEvent {
    TurnEvent::Finished {
        thread_id: thread_id.to_string(),
        turn_id: turn_id.to_string(),
    }
}

/// 若 `thread_id` 有活跃 turn,中断它并等待 driver 落盘完成。
///
/// driver 是「先发 turn/completed,后 append/touch」,而 `TurnEvent::Finished`
/// 在落盘之后才发出;因此本函数返回后,调用方可以依赖「该 turn 已落盘」。
/// 有界等待:driver 异常卡死时不至于拖垮整个请求循环。
async fn interrupt_and_wait_for_persist(
    threads: &mut HashMap<String, ThreadSession>,
    turn_rx: &mut mpsc::Receiver<TurnEvent>,
    thread_id: &str,
) {
    let Some(tid) = threads
        .get(thread_id)
        .and_then(|s| s.active_turn_id.clone())
    else {
        return;
    };
    if let Some(s) = threads.get(thread_id) {
        let _ = s.interrupt_tx.try_send(tid.clone());
    }
    let wait_for_persist = async {
        while let Some(TurnEvent::Finished {
            thread_id: done_id,
            turn_id: done_turn,
        }) = turn_rx.recv().await
        {
            if let Some(s) = threads.get_mut(&done_id) {
                if s.active_turn_id.as_deref() == Some(done_turn.as_str()) {
                    s.active_turn_id = None;
                }
            }
            if done_id == thread_id && done_turn == tid {
                break;
            }
        }
    };
    if tokio::time::timeout(std::time::Duration::from_secs(5), wait_for_persist)
        .await
        .is_err()
    {
        eprintln!(
            "[app-server] timed out waiting for turn of {thread_id} to persist; \
             a late driver append may resurrect its files"
        );
    }
}

/// 单个 thread 的 driver task:串行消费 turn,驱动 `agent.run()` 的 stream,
/// 经 `Translator` 写成协议通知。
///
/// **取消安全**:`Agent::run()` 每次都会重置 cancel token,因此必须在
/// `run().await` 返回**之后**再取 `cancel_token()`,否则 `turn/interrupt` 无效。
///
/// **权限审批闭环**:遇到 `AgentEvent::PermissionRequest` 时,driver 发出反向
/// 请求 `item/toolCall/requestApproval`(id 取自进程级 `perm_seq`)并等待客户端
/// 经 `pending` 登记表回传的决定;超时或中断按 `Deny` 处理。
#[allow(clippy::too_many_arguments)]
async fn run_thread_driver<W>(
    thread_id: String,
    mut agent: yi_agent_core::Agent,
    mut prompt_rx: mpsc::Receiver<TurnPrompt>,
    mut interrupt_rx: mpsc::Receiver<String>,
    writer: Arc<MessageWriter<W>>,
    turn_tx: mpsc::Sender<TurnEvent>,
    decision_tx: Option<mpsc::Sender<(u64, Decision)>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    permission_timeout: Duration,
    perm_seq: Arc<AtomicU64>,
    catalog: Option<yi_agent_runtime::bootstrap::SkillsCatalogHandle>,
    store: Arc<crate::thread_store::ThreadStore>,
) where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // 每个 thread 一个 translator:item id 带 turn_id 前缀(`item-<turn_id>-<n>`),
    // 故即便 resume 后计数器归 1,新 item 也不会与回放的历史 id 冲突。
    let mut translator = Translator::new(thread_id.clone());
    while let Some(TurnPrompt { turn_id, prompt }) = prompt_rx.recv().await {
        // 本轮累加器:最终 item 与最近一次用量(用于落盘)。
        // `last_usage` 记录的是本轮**最后一次** provider 调用的完整快照
        // (input 来自 `message_start`、output 来自 `message_delta`,由 Translator 合并)。
        let mut completed_items: Vec<crate::protocol::Item> = Vec::new();
        let mut last_usage: Option<crate::thread_store::TurnUsage> = None;
        let user_prompt = prompt.clone();
        translator.set_turn(turn_id.clone());

        // 每轮开跑前刷新 skills catalog:skills 热重载,让本轮看到最新的
        // system prompt(catalog 未变时输出逐字节相同,不破坏 prompt cache)。
        if let Some(handle) = &catalog {
            if let Some(new_prompt) = handle.current_system_prompt() {
                agent.set_system_prompt(Some(new_prompt));
            }
        }

        let mut stream = match agent.run(prompt).await {
            Ok(s) => s,
            Err(e) => {
                // run() 本身失败:翻译成 Error → turn/completed(failed)。
                for n in translator.on_event(yi_agent_core::AgentEvent::Error(e)) {
                    let _ = write_notification(&writer, &n).await;
                }
                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                continue;
            }
        };

        // 必须在 run() 之后捕获:run() 内部会重建 cancel token。
        let cancel_token = agent.cancel_token();
        let mut cancel_sent = false;

        loop {
            tokio::select! {
                ev = stream.next() => {
                    match ev {
                        Some(yi_agent_core::AgentEvent::PermissionRequest {
                            request_id, tool_name, tool_input, prefix_suggestion, kind,
                        }) => {
                            let perm_id = format!("perm-{}", perm_seq.fetch_add(1, Ordering::SeqCst));
                            let (dtx, mut drx) = oneshot::channel::<Decision>();
                            pending.lock().await.insert(perm_id.clone(), dtx);

                            let reverse = ReverseRequest {
                                jsonrpc: JSONRPC_VERSION,
                                id: perm_id.clone(),
                                method: "item/toolCall/requestApproval",
                                params: json!({
                                    "thread_id": thread_id,
                                    "turn_id": turn_id,
                                    "request_id": request_id,
                                    "tool_name": tool_name,
                                    "tool_input": tool_input,
                                    "prefix_suggestion": prefix_suggestion,
                                    "kind": kind,
                                }),
                            };
                            if writer.write_value(&reverse).await.is_err() {
                                pending.lock().await.remove(&perm_id);
                                let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                return;
                            }

                            // 等客户端决定;超时或中断按 Deny 处理。只接受针对当前 turn 的中断,
                            // 其它 turn 的残留信号忽略后继续等待(deadline 不重置)。
                            let timeout = tokio::time::sleep(permission_timeout);
                            tokio::pin!(timeout);
                            let decision = loop {
                                tokio::select! {
                                    d = &mut drx => break d.unwrap_or(Decision::Deny),
                                    _ = &mut timeout => {
                                        pending.lock().await.remove(&perm_id);
                                        break Decision::Deny;
                                    }
                                    Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                                        if target == turn_id {
                                            cancel_sent = true;
                                            cancel_token.cancel();
                                            pending.lock().await.remove(&perm_id);
                                            break Decision::Deny;
                                        }
                                        // 其它 turn 的残留信号:忽略,继续等待。
                                    }
                                }
                            };

                            if let Some(tx) = &decision_tx {
                                let _ = tx.send((request_id, decision)).await;
                            }
                        }
                        Some(e) => {
                            for n in translator.on_event(e) {
                                if let crate::protocol::Notification::ItemCompleted { item, .. } = &n {
                                    completed_items.push(item.clone());
                                }
                                // 用量通知携带本轮累积快照(见 Translator::on_event),最后一条即落盘用的完整用量。
                                if let crate::protocol::Notification::TokenUsage {
                                    model,
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens,
                                    cache_read_input_tokens,
                                    ..
                                } = &n
                                {
                                    last_usage = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: *input_tokens,
                                        output_tokens: *output_tokens,
                                        cache_creation_input_tokens: *cache_creation_input_tokens,
                                        cache_read_input_tokens: *cache_read_input_tokens,
                                    });
                                }
                                if write_notification(&writer, &n).await.is_err() {
                                    // 客户端可能已断开;先上报 Finished,
                                    // 避免 active_turn_id 永久卡住。
                                    let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
                                    return;
                                }
                            }
                        }
                        None => break,
                    }
                }
                Some(target) = interrupt_rx.recv(), if !cancel_sent => {
                    // 只接受针对当前 turn 的中断;忽略上一轮残留的信号。
                    if target == turn_id {
                        cancel_sent = true;
                        cancel_token.cancel();
                    }
                    // 继续消费 stream,直到 run loop 发 Cancelled 并结束。
                }
            }
        }

        // 落盘(尽力而为):合成一条 userMessage item 放在首部,再拼本轮最终 item。
        // 基线 server 不 emit userMessage,必须在此补齐,否则 resume 会丢用户提问。
        let mut items = Vec::with_capacity(completed_items.len() + 1);
        items.push(crate::protocol::Item::UserMessage {
            id: format!("user-{turn_id}"),
            text: user_prompt.clone(),
        });
        items.append(&mut completed_items);

        let record = crate::thread_store::TurnLine::Turn {
            items,
            usage: last_usage,
            messages: agent.session().messages().to_vec(),
        };
        // append 失败则跳过 touch:避免 updated_at/title 被推进却无日志内容,
        // 留下 `thread/list` 会列出的"幽灵" thread。
        if let Err(e) = store.append_turn(&thread_id, &record) {
            eprintln!("[app-server] failed to persist turn {turn_id} of {thread_id}: {e}");
        } else if let Err(e) = store.touch(&thread_id, Some(user_prompt.as_str())) {
            eprintln!("[app-server] failed to update meta for {thread_id}: {e}");
        }

        let _ = turn_tx.send(finished_event(&thread_id, &turn_id)).await;
    }
}

/// 在全局索引的目录里定位 `thread_id` 所属目录。
fn find_thread_dir(workspaces: &WorkspaceIndex, thread_id: &str) -> Option<PathBuf> {
    workspaces
        .list()
        .into_iter()
        .map(PathBuf::from)
        .find(|d| crate::thread_store::ThreadStore::new(d).exists(thread_id))
}

/// 按 thread 定位其 store;索引找不到时回退 `cfg.workdir`。
fn store_for(
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    let dir = find_thread_dir(workspaces, thread_id).unwrap_or_else(|| cfg.workdir.clone());
    Arc::new(crate::thread_store::ThreadStore::new(&dir))
}

/// 取 thread 的 store:内存中的 thread 用其权威实例(与 driver 共享 lock);
/// 冷 thread 按全局索引定位,索引找不到再回退 `cfg.workdir`。
fn store_lookup(
    threads: &HashMap<String, ThreadSession>,
    workspaces: &WorkspaceIndex,
    cfg: &RuntimeConfig,
    thread_id: &str,
) -> Arc<crate::thread_store::ThreadStore> {
    match threads.get(thread_id) {
        Some(s) => Arc::clone(&s.store),
        None => store_for(workspaces, cfg, thread_id),
    }
}

/// 解析 `thread/start` 的目标目录:显式 `params.cwd` 优先,缺省用 `cfg.workdir`。
///
/// 显式 `cwd` canonicalize + 校验是目录;失败写 `-32602` 并返回 `Ok(None)`
/// (调用方 continue)。缺省时沿用 `cfg.workdir` 原样,不 canonicalize:保持旧的
/// 单目录行为,避免 macOS `/var` → `/private/var` 之类改写破坏既有路径语义。
async fn resolve_thread_cwd<W: tokio::io::AsyncWrite + Unpin>(
    params: &serde_json::Value,
    cfg: &RuntimeConfig,
    writer: &MessageWriter<W>,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("cwd").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => match std::fs::canonicalize(s) {
            Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
            _ => {
                write_response(
                    writer,
                    err_response(id, RpcError::invalid_params("cwd is not a valid directory")),
                )
                .await?;
                Ok(None)
            }
        },
        _ => Ok(Some(cfg.workdir.display().to_string())),
    }
}

/// 从 params 提取 `threadId`;缺失时写 `-32602` 并返回 `Ok(None)`。
async fn require_thread_id<W: tokio::io::AsyncWrite + Unpin>(
    writer: &MessageWriter<W>,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    match params.get("threadId").and_then(|v| v.as_str()) {
        Some(s) => Ok(Some(s.to_string())),
        None => {
            write_response(
                writer,
                err_response(id, RpcError::invalid_params("missing threadId")),
            )
            .await?;
            Ok(None)
        }
    }
}

/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let input = params.get("input")?.as_array()?;
    let mut text = String::new();
    for block in input {
        if block.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
            text.push_str(t);
        }
    }
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use futures::StreamExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    /// 极简 provider:只回一段文本后结束。
    struct MockProvider;

    #[async_trait]
    impl yi_agent_core::Provider for MockProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 记录每次调用收到的 message 数,并回显 `n=<count>` 作为 agent 文本。
    struct RecordingProvider {
        seen: Arc<std::sync::Mutex<Vec<usize>>>,
    }

    #[async_trait]
    impl yi_agent_core::Provider for RecordingProvider {
        async fn call_stream(
            &self,
            req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let n = req.messages.len();
            self.seen.lock().unwrap().push(n);
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta(format!("n={n}")),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 永不自行结束的 provider:每 5ms 吐一个 delta,turn 会一直活跃,
    /// 直到被 `turn/interrupt` 取消。
    struct SlowProvider;

    #[async_trait]
    impl yi_agent_core::Provider for SlowProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::unfold(0u64, |i| async move {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((
                    yi_agent_core::provider::ProviderEvent::TextDelta("x".into()),
                    i + 1,
                ))
            });
            Ok(events.boxed())
        }
    }

    /// 首帧延迟 20ms 的 provider:保证 driver 的 interrupt 分支在 stream
    /// 首次 yield 之前已被轮询到(用于确定性地测试中断标记)。
    struct DelayedProvider;

    #[async_trait]
    impl yi_agent_core::Provider for DelayedProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            let events = futures::stream::once(async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                yi_agent_core::provider::ProviderEvent::TextDelta("hi".into())
            })
            .chain(futures::stream::once(async {
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                }
            }));
            Ok(events.boxed())
        }
    }

    fn build_test_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    Arc::new(MockProvider),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    yi_agent_core::AgentConfig::default(),
                ),
                session,
            ),
            decision_tx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        })
    }

    fn build_slow_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    Arc::new(SlowProvider),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    yi_agent_core::AgentConfig::default(),
                ),
                session,
            ),
            decision_tx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        })
    }

    fn build_delayed_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        Ok(BuiltAgent {
            agent: apply_session(
                yi_agent_core::Agent::new(
                    Arc::new(DelayedProvider),
                    Arc::new(yi_agent_core::ToolRegistry::new()),
                    yi_agent_core::AgentConfig::default(),
                ),
                session,
            ),
            decision_tx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        })
    }

    fn test_config() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".to_string(),
            api_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "test-model".to_string(),
            max_turns: 20,
            workdir: std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::default(),
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    /// 用两条 `duplex` 管道把 server 与测试客户端对接。
    struct Harness {
        client_w: tokio::io::DuplexStream,
        client_r: BufReader<tokio::io::DuplexStream>,
        handle: tokio::task::JoinHandle<anyhow::Result<()>>,
        /// 隔离的全局目录索引;持有它保证 tempdir 存活到 harness 结束。
        _index_dir: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)
        }

        /// 用自定义 agent 工厂搭建 harness(慢 provider / 中断 / 权限测试需要)。
        fn with_factory<F>(build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + 'static,
        {
            Self::with_config(test_config(), build, permission_timeout)
        }

        /// 用自定义 config + agent 工厂搭建 harness(持久化测试需要自定义 workdir)。
        fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
        where
            F: Fn(
                    Option<yi_agent_core::Session>,
                    &std::path::Path,
                    crate::thread_store::ThreadMode,
                ) -> anyhow::Result<BuiltAgent>
                + Send
                + 'static,
        {
            let (client_w, server_r) = tokio::io::duplex(64 * 1024);
            let (server_w, client_r) = tokio::io::duplex(64 * 1024);
            let index_dir = tempfile::TempDir::new().unwrap();
            let workspaces = Arc::new(WorkspaceIndex::new(
                index_dir.path().join("workspaces.json"),
            ));
            let handle = tokio::spawn(run_with(
                server_r,
                server_w,
                cfg,
                permission_timeout,
                workspaces,
                build,
            ));
            Self {
                client_w,
                client_r: BufReader::new(client_r),
                handle,
                _index_dir: index_dir,
            }
        }

        async fn send(&mut self, line: &str) {
            self.client_w.write_all(line.as_bytes()).await.unwrap();
            self.client_w.write_all(b"\n").await.unwrap();
            self.client_w.flush().await.unwrap();
        }

        async fn read_value(&mut self) -> serde_json::Value {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), self.client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a message")
                .expect("read_line failed");
            assert!(n > 0, "unexpected EOF while waiting for a message");
            serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
        }

        /// 关掉客户端写端(触发 EOF)并等待 server 任务结束。
        async fn shutdown(self) {
            let Harness {
                client_w, handle, ..
            } = self;
            drop(client_w);
            let res = tokio::time::timeout(Duration::from_secs(5), handle)
                .await
                .expect("server must shut down within the timeout")
                .expect("server task must not panic");
            assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
        }
    }

    async fn initialize(h: &mut Harness) {
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "initialize must respond with the request id");
    }

    /// `initialize` + `thread/start`,返回新建 thread 的 id。
    async fn start_thread(h: &mut Harness) -> String {
        initialize(h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        // 依次读到 thread/started 通知与 thread/start 响应,取响应里的 thread_id。
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                return v["result"]["thread_id"].as_str().unwrap().to_string();
            }
        }
        panic!("no thread/start response");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn initialize_responds_with_server_info() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        assert_eq!(v["result"]["protocolVersion"], 1);
        assert!(
            v.get("error").is_none(),
            "success response must omit `error`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn request_before_initialize_returns_not_initialized() {
        let mut h = Harness::new();
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1);
        assert_eq!(v["error"]["code"], -32010);
        assert!(
            v.get("result").is_none(),
            "error response must omit `result`: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_method_returns_method_not_found() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"bogus","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32601);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_emits_thread_started_and_responds() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;

        let mut notif = None;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("thread/started") {
                notif = Some(v);
            } else if v.get("id") == Some(&serde_json::json!(2)) {
                resp = Some(v);
            }
            if notif.is_some() && resp.is_some() {
                break;
            }
        }

        let notif = notif.expect("expected a thread/started notification");
        let resp = resp.expect("expected a thread/start response");
        let notif_thread_id = notif["params"]["thread_id"]
            .as_str()
            .expect("notification thread_id must be a string");
        assert!(!notif_thread_id.is_empty(), "thread_id must be non-empty");
        assert_eq!(notif["params"]["model"], "test-model");
        assert_eq!(resp["result"]["thread_id"], notif_thread_id);
        assert_eq!(resp["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_read_returns_redacted_view() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"config/read","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["api_key"], "");
        assert_eq!(v["result"]["model"], "test-model");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_add_list_remove_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"][0]["path"],
            cwd.to_string_lossy().to_string()
        );
        assert_eq!(v["result"]["workspaces"][0]["exists"], true);

        let rm = serde_json::json!({"jsonrpc":"2.0","id":4,"method":"workspace/remove",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&rm.to_string()).await;
        let v = h.read_value().await;
        assert!(v.get("result").is_some());

        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"].as_array().unwrap().len(),
            0,
            "removed workspace must be gone from the index: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_list_marks_missing_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        // 目录在磁盘上消失后,索引仍保留该条目,但 list 必须标记 exists=false。
        std::fs::remove_dir_all(&cwd).unwrap();

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"][0]["path"],
            cwd.to_string_lossy().to_string()
        );
        assert_eq!(
            v["result"]["workspaces"][0]["exists"], false,
            "missing directory must be flagged as not existing: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_remove_accepts_alternate_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let mut h = Harness::new();
        initialize(&mut h).await;

        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": cwd.to_string_lossy()}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["result"]["path"], cwd.to_string_lossy().to_string());

        // 用与存储值不同、但 canonicalize 后等价的路径 remove,必须命中同一条目。
        // `dir.path()` 是非 canonical 前缀(如 macOS 的 /var → /private/var),
        // 末尾再缀 `/.`,因此字符串与 canonical 存储值不同、解析后却相同。
        let alternate = dir.path().join(".").to_string_lossy().to_string();
        assert_ne!(alternate, cwd.to_string_lossy().to_string());

        let rm = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"workspace/remove",
            "params":{"path": alternate}});
        h.send(&rm.to_string()).await;
        let v = h.read_value().await;
        assert!(v.get("result").is_some(), "remove must succeed: {v}");

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"workspace/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(
            v["result"]["workspaces"].as_array().unwrap().len(),
            0,
            "alternate path must remove the canonicalized entry: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_add_rejects_non_directory() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":2,"method":"workspace/add","params":{"path":"/nope/nope"}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn eof_exits_gracefully() {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            build_test_agent,
        ));

        // 什么都不发,直接关掉写端 → server 应看到 EOF 并 Ok(()) 退出。
        drop(client_w);
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_ok(),
            "run_with should return Ok on EOF: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_json_returns_parse_error() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send("not json").await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32700, "expected parse error: {v}");
        assert_eq!(v["id"], 0, "malformed frame must be answered with id 0");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn blank_lines_are_ignored() {
        let mut h = Harness::new();
        // 先发一个空行,再发合法的 initialize;空行不应产生任何帧。
        h.send("").await;
        h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 1, "blank line must not produce a frame: {v}");
        assert_eq!(v["result"]["serverInfo"]["name"], "yi-agent-app-server");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_factory_failure_returns_internal_error() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            |_s: Option<yi_agent_core::Session>,
             _cwd: &std::path::Path,
             _mode: crate::thread_store::ThreadMode| {
                Err::<BuiltAgent, _>(anyhow::anyhow!("boom"))
            },
        ));

        let mut client_r = BufReader::new(client_r);

        client_w
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let init: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(init["id"], 1);

        client_w
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"thread/start\",\"params\":{}}\n",
            )
            .await
            .unwrap();
        client_w.flush().await.unwrap();
        let mut buf = String::new();
        client_r.read_line(&mut buf).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
        assert_eq!(v["id"], 2);
        assert_eq!(
            v["error"]["code"], -32603,
            "factory failure must map to internal error: {v}"
        );

        drop(client_w);
        let res = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(res.is_ok(), "run_with should return Ok on EOF: {res:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn oversized_frame_returns_err() {
        let (mut client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, _client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let handle = tokio::spawn(run_with(
            server_r,
            server_w,
            test_config(),
            PERMISSION_TIMEOUT,
            workspaces,
            build_test_agent,
        ));

        let mut frame = vec![b'x'; crate::protocol::MAX_FRAME_BYTES + 16];
        frame.push(b'\n');
        // duplex 缓冲小于帧,server 读到超限即报错并退出读端;写侧随后可能
        // 因 broken pipe 失败,这里忽略写错误——真正断言的是 server 的结果。
        let _ = client_w.write_all(&frame).await;
        let _ = client_w.flush().await;

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("server must exit within the timeout")
            .expect("server task must not panic");
        assert!(
            result.is_err(),
            "oversized frame must surface an error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_emits_full_notification_sequence() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut methods: Vec<String> = Vec::new();
        let mut resp_turn_id: Option<String> = None;
        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..12 {
            let v = h.read_value().await;
            if let Some(m) = v.get("method").and_then(|m| m.as_str()) {
                methods.push(m.to_string());
                if m == "turn/completed" {
                    completed = Some(v);
                    break;
                }
            } else if v.get("id") == Some(&serde_json::json!(3)) {
                resp_turn_id = v["result"]["turn_id"].as_str().map(|s| s.to_string());
            }
        }

        assert_eq!(
            methods,
            vec![
                "turn/started",
                "item/started",
                "item/delta",
                // `Done` 会先 finalize 打开的 agentMessage,故 turn/completed
                // 之前必有一条 item/completed(见 Translator::finish_turn)。
                "item/completed",
                "turn/completed"
            ],
            "unexpected notification sequence"
        );
        let resp_turn_id = resp_turn_id.expect("turn/start response must carry turn_id");
        assert!(!resp_turn_id.is_empty(), "turn_id must be non-empty");
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "completed");
        assert_eq!(completed["params"]["turn_id"], resp_turn_id);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_unknown_thread_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"threadId":"nope","input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32011, "expected unknown thread: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_missing_thread_id_returns_invalid_params() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(
            r#"{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{"input":[{"type":"text","text":"hi"}]}}"#,
        )
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_empty_input_returns_invalid_params() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[]}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert_eq!(v["error"]["code"], -32602, "expected invalid params: {v}");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_start_while_turn_in_progress_returns_turn_in_progress() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 读到 id==3 的响应(跳过 turn/started 通知与 item/* 帧)。
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                assert!(v["result"]["turn_id"].is_string(), "expected turn_id: {v}");
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"again"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["error"]["code"], -32012, "expected turn in progress: {v}");
                break;
            }
        }
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_interrupt_cancels_active_turn() {
        let mut h = Harness::with_factory(build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        // 等 turn/started,确认 turn 已活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"turn/interrupt","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                completed = Some(v);
                break;
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "interrupted");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_ignores_interrupt_tagged_with_other_turn() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let writer = Arc::new(MessageWriter::new(server_w));

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            build_delayed_agent(
                None,
                std::path::Path::new("/tmp"),
                crate::thread_store::ThreadMode::Normal,
            )
            .unwrap()
            .agent,
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
        ));

        // 上一轮残留的中断(属于 turn-0)必须被忽略。
        interrupt_tx.try_send("turn-0".into()).unwrap();
        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
            })
            .await
            .unwrap();

        let mut client_r = BufReader::new(client_r);
        let mut status = None;
        for _ in 0..10 {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                .await
                .expect("timed out waiting for a notification")
                .unwrap();
            if n == 0 {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v["method"] == "turn/completed" {
                status = Some(v["params"]["status"].as_str().unwrap().to_string());
                break;
            }
        }
        assert_eq!(
            status.as_deref(),
            Some("completed"),
            "a stale interrupt must not cancel the new turn"
        );

        drop(prompt_tx);
        drop(interrupt_tx);
        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { .. }));
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn driver_reports_finished_when_writer_fails() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        drop(client_r); // 断开读端 → 写通知失败
        let writer = Arc::new(MessageWriter::new(server_w));

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            build_test_agent(
                None,
                std::path::Path::new("/tmp"),
                crate::thread_store::ThreadMode::Normal,
            )
            .unwrap()
            .agent,
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
            })
            .await
            .unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("driver must report Finished even when writes fail")
            .expect("turn channel must stay open");
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// 一个 thread 的 driver 复用同一个 `Translator`,因此 item id 跨 turn 单调
    /// 递增,不会出现两轮都用 `item-1` 的碰撞(回归 Fix #4)。
    #[tokio::test(flavor = "multi_thread")]
    async fn driver_uses_unique_item_ids_across_turns() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (_interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let writer = Arc::new(MessageWriter::new(server_w));

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            build_test_agent(
                None,
                std::path::Path::new("/tmp"),
                crate::thread_store::ThreadMode::Normal,
            )
            .unwrap()
            .agent,
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
            None,
            Arc::new(Mutex::new(HashMap::new())),
            Duration::from_secs(60),
            Arc::new(AtomicU64::new(1)),
            None,
            store,
        ));

        let mut client_r = BufReader::new(client_r);
        // 每轮取第一个 `item/started` 的 item id(agentMessage 的起始项)。
        let mut item_ids: Vec<String> = Vec::new();
        for turn in ["turn-1", "turn-2"] {
            prompt_tx
                .send(TurnPrompt {
                    turn_id: turn.into(),
                    prompt: "hi".into(),
                })
                .await
                .unwrap();
            loop {
                let mut buf = String::new();
                let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                    .await
                    .expect("timed out waiting for a notification")
                    .unwrap();
                assert!(n > 0, "unexpected EOF while waiting for turn/completed");
                let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
                let method = v["method"].as_str().unwrap_or("");
                if method == "item/started" {
                    if let Some(id) = v["params"]["item"]["id"].as_str() {
                        item_ids.push(id.to_string());
                    }
                }
                if method == "turn/completed" {
                    break;
                }
            }
            let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(ev, TurnEvent::Finished { .. }));
        }

        assert!(
            item_ids.len() >= 2,
            "expected items from both turns: {item_ids:?}"
        );
        let mut unique = item_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            item_ids.len(),
            "item ids must be unique across turns: {item_ids:?}"
        );

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// 第一次调用触发一个受权限管控的 bash 工具调用;第二次调用返回文本并结束。
    struct PermissionMockProvider {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl yi_agent_core::Provider for PermissionMockProvider {
        async fn call_stream(
            &self,
            _req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            use yi_agent_core::provider::{ProviderEvent, StopReason};
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = if n == 0 {
                vec![
                    ProviderEvent::ToolUseStart {
                        id: "c1".into(),
                        name: "bash".into(),
                    },
                    ProviderEvent::ToolUseDelta {
                        id: "c1".into(),
                        partial_json: r#"{"cmd":"ls"}"#.into(),
                    },
                    ProviderEvent::ToolUseEnd { id: "c1".into() },
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            } else {
                vec![
                    ProviderEvent::TextDelta("done".into()),
                    ProviderEvent::Stop {
                        reason: StopReason::EndTurn,
                    },
                ]
            };
            Ok(futures::stream::iter(events).boxed())
        }
    }

    struct FakeBash;

    #[async_trait]
    impl yi_agent_core::Tool for FakeBash {
        fn name(&self) -> &str {
            "bash"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object" })
        }
        fn description(&self) -> &str {
            "fake bash"
        }
        async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
            yi_agent_core::ToolResult::text("ran")
        }
    }

    /// 构造一个会触发 bash 审批的 agent,并把决定通道交给 driver。
    fn build_permission_agent(
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let provider = Arc::new(PermissionMockProvider {
            calls: AtomicUsize::new(0),
        });
        let mut registry = yi_agent_core::ToolRegistry::new();
        registry.register(Arc::new(FakeBash));
        let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
            yi_agent_core::permission::PermissionsConfig::default(),
            yi_agent_core::autonomy::YoloSwitch::new(false),
            std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
            Arc::new(|_cmd: &str| None),
        ));
        let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
        let rx_arc = Arc::new(Mutex::new(decision_rx));
        let agent = apply_session(
            yi_agent_core::Agent::new(
                provider,
                Arc::new(registry),
                yi_agent_core::AgentConfig::default(),
            )
            .with_permission(checker, rx_arc),
            session,
        );
        Ok(BuiltAgent {
            agent,
            decision_tx: Some(decision_tx),
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_request_round_trip_allows_tool() {
        let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut saw_request = false;
        let mut saw_tool_started = false;
        let mut perm_id: Option<String> = None;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/toolCall/requestApproval") => {
                    assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                    assert_eq!(v["params"]["tool_name"], "bash", "reverse params: {v}");
                    perm_id = v["id"].as_str().map(|s| s.to_string());
                    saw_request = true;
                    break;
                }
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_request,
            "expected an item/toolCall/requestApproval request"
        );
        let perm_id = perm_id.expect("reverse request must carry a string id");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":"{perm_id}","result":{{"decision":"allow_once"}}}}"#
        ))
        .await;

        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                Some("turn/completed") => {
                    completed = Some(v);
                    break;
                }
                _ => {}
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(completed["params"]["status"], "completed");
        assert!(
            saw_tool_started,
            "an allowed tool must emit an item/started toolCall item"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permission_timeout_defaults_to_deny() {
        let mut h = Harness::with_factory(build_permission_agent, Duration::from_millis(150));
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;

        let mut saw_request = false;
        let mut saw_tool_started = false;
        for _ in 0..20 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/toolCall/requestApproval") => {
                    assert_eq!(v["id"], "perm-1", "reverse request id: {v}");
                    saw_request = true;
                    break;
                }
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_request,
            "expected an item/toolCall/requestApproval request"
        );

        // 不回复:driver 应在超时后按 Deny 处理,turn 仍能正常完成。
        let mut completed: Option<serde_json::Value> = None;
        for _ in 0..40 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("item/started") if v["params"]["item"]["type"] == "toolCall" => {
                    saw_tool_started = true;
                }
                Some("turn/completed") => {
                    completed = Some(v);
                    break;
                }
                _ => {}
            }
        }
        let completed = completed.expect("expected a turn/completed notification");
        assert_eq!(
            completed["params"]["status"], "completed",
            "the agent must not hang after a permission timeout"
        );
        assert!(
            !saw_tool_started,
            "a denied tool must not emit an item/started toolCall item"
        );
        h.shutdown().await;
    }

    /// 读到下一个 `item/toolCall/requestApproval` 通知,返回其 id。
    async fn read_until_approval(h: &mut Harness) -> String {
        for _ in 0..20 {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/toolCall/requestApproval") {
                return v["id"]
                    .as_str()
                    .expect("approval id must be a string")
                    .to_string();
            }
        }
        panic!("expected an item/toolCall/requestApproval notification");
    }

    /// 读帧直到出现 `id == want` 的响应,返回其 `result.thread_id`。
    async fn read_thread_start_response(h: &mut Harness, want: u64) -> String {
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(want)) {
                return v["result"]["thread_id"]
                    .as_str()
                    .expect("thread/start response must carry thread_id")
                    .to_string();
            }
        }
        panic!("no thread/start response for id {want}");
    }

    /// 两个并发 thread 的审批 id 必须不同:否则一个 thread 的 Allow 会被
    /// 应用到另一个 thread 的工具上(跨 thread 授权错配)。
    #[tokio::test(flavor = "multi_thread")]
    async fn permission_request_ids_are_unique_across_threads() {
        let mut h = Harness::with_factory(build_permission_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        // thread A。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_a = read_thread_start_response(&mut h, 2).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid_a}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let id_a = read_until_approval(&mut h).await;

        // thread B(其 PermissionChecker 的 request_id 也从 1 开始)。
        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{}}"#)
            .await;
        let tid_b = read_thread_start_response(&mut h, 4).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid_b}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let id_b = read_until_approval(&mut h).await;

        assert_ne!(id_a, id_b, "approval ids must be unique across threads");
        h.shutdown().await;
    }

    /// 上一轮残留的中断信号不得取消当前 turn 的审批等待(同 `e6068b0` 的
    /// bug 类:中断必须按目标 turn id 过滤)。
    #[tokio::test(flavor = "multi_thread")]
    async fn stale_interrupt_does_not_cancel_turn_during_approval() {
        let (prompt_tx, prompt_rx) = mpsc::channel::<TurnPrompt>(8);
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<String>(8);
        let (turn_tx, mut turn_rx) = mpsc::channel::<TurnEvent>(8);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let writer = Arc::new(MessageWriter::new(server_w));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let perm_seq = Arc::new(AtomicU64::new(1));

        let store_dir = tempfile::TempDir::new().unwrap();
        let store = Arc::new(crate::thread_store::ThreadStore::new(store_dir.path()));

        let built = build_permission_agent(
            None,
            std::path::Path::new("/tmp"),
            crate::thread_store::ThreadMode::Normal,
        )
        .unwrap();
        let handle = tokio::spawn(run_thread_driver(
            "thread-1".into(),
            built.agent,
            prompt_rx,
            interrupt_rx,
            writer,
            turn_tx,
            built.decision_tx,
            Arc::clone(&pending),
            Duration::from_secs(60),
            Arc::clone(&perm_seq),
            None,
            store,
        ));

        prompt_tx
            .send(TurnPrompt {
                turn_id: "turn-1".into(),
                prompt: "hi".into(),
            })
            .await
            .unwrap();

        // 读到反向请求 id(此时 driver 已进入内层审批等待)。
        let mut client_r = BufReader::new(client_r);
        let mut approval_id = None;
        for _ in 0..20 {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(5), client_r.read_line(&mut buf))
                .await
                .expect("timed out")
                .unwrap();
            if n == 0 {
                break;
            }
            let v: serde_json::Value = serde_json::from_str(buf.trim()).unwrap();
            if v["method"] == "item/toolCall/requestApproval" {
                approval_id = Some(v["id"].as_str().unwrap().to_string());
                break;
            }
        }
        let approval_id = approval_id.expect("expected a reverse request");

        // 现在投递一个属于上一轮(turn-0)的残留中断:它必须被忽略。
        interrupt_tx.try_send("turn-0".into()).unwrap();

        // 给 driver 足够时间处理该信号;若残留中断未被过滤,它会在此刻
        // 取走 pending 项并取消本轮。
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 用正确的决定回应;若 pending 项已被残留中断移除,这里为 None → 断言失败。
        let sender = pending
            .lock()
            .await
            .remove(&approval_id)
            .expect("pending approval must survive a stale interrupt");
        sender.send(Decision::AllowOnce).unwrap();

        let ev = tokio::time::timeout(Duration::from_secs(5), turn_rx.recv())
            .await
            .expect("turn must finish")
            .unwrap();
        assert!(matches!(ev, TurnEvent::Finished { ref turn_id, .. } if turn_id == "turn-1"));

        drop(prompt_tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_allocates_uuid_id_and_writes_meta() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);

        let tid = start_thread(&mut h).await;
        let uuid_part = tid
            .strip_prefix("thread-")
            .unwrap_or_else(|| panic!("expected thread-<uuid>, got {tid}"));
        assert!(
            uuid_part.parse::<uuid::Uuid>().is_ok(),
            "suffix must be a uuid: {tid}"
        );

        // 通过 store API 读取,锁住落盘内容(不依赖文件布局细节)。
        let loaded = crate::thread_store::ThreadStore::new(dir.path())
            .load(&tid)
            .expect("load must not fail")
            .expect("thread/start must persist meta");
        assert_eq!(loaded.meta.thread_id, tid);
        assert_eq!(loaded.meta.model, "test-model");
        assert_eq!(loaded.meta.cwd, dir.path().display().to_string());
        assert!(loaded.meta.title.is_none(), "title starts empty");
        assert!(loaded.items.is_empty(), "no turns yet");

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_empty_returns_empty_array() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/list","params":{}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["result"]["threads"], serde_json::json!([]));
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_returns_created_threads() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_a = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let tid_b = read_thread_start_response(&mut h, 3).await;

        h.send(r#"{"jsonrpc":"2.0","id":4,"method":"thread/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/list must respond");
        let threads = v["result"]["threads"].as_array().unwrap();
        assert_eq!(threads.len(), 2);
        let ids: std::collections::HashSet<&str> = threads
            .iter()
            .map(|t| t["thread_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(tid_a.as_str()) && ids.contains(tid_b.as_str()));
        // 两次 start 可能同毫秒,顺序不定(排序由 thread_store 单测锁定);
        // 此处只断言每个条目都带时间戳字段。
        for t in threads {
            assert!(t["created_at"].is_number());
            assert!(t["updated_at"].is_number());
        }
        h.shutdown().await;
    }

    /// Task 11:`thread/list` / `thread/listAll` 的条目必须显式带出
    /// `permission_mode`(wire 契约与存储解耦,但字段必须存在且为 lowercase)。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_exposes_permission_mode() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid_normal = read_thread_start_response(&mut h, 2).await;
        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/start","params":{}}"#)
            .await;
        let tid_yolo = read_thread_start_response(&mut h, 3).await;

        // 把第二个线程切成 yolo。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid_yolo}","mode":"yolo"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );

        // thread/list:每个条目带 lowercase permission_mode。
        h.send(r#"{"jsonrpc":"2.0","id":5,"method":"thread/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/list must respond");
        let threads = v["result"]["threads"].as_array().unwrap();
        let mode_of = |tid: &str| -> serde_json::Value {
            threads
                .iter()
                .find(|t| t["thread_id"].as_str() == Some(tid))
                .unwrap_or_else(|| panic!("thread {tid} missing from thread/list: {v}"))["permission_mode"]
                .clone()
        };
        assert_eq!(mode_of(&tid_normal), serde_json::json!("normal"));
        assert_eq!(mode_of(&tid_yolo), serde_json::json!("yolo"));

        // thread/listAll:分组内的条目同样带该字段。
        h.send(r#"{"jsonrpc":"2.0","id":6,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("thread/listAll must respond");
        let all_threads: Vec<&serde_json::Value> = v["result"]["groups"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|g| g["threads"].as_array().unwrap().iter())
            .collect();
        let mode_of_all = |tid: &str| -> serde_json::Value {
            all_threads
                .iter()
                .find(|t| t["thread_id"].as_str() == Some(tid))
                .unwrap_or_else(|| panic!("thread {tid} missing from thread/listAll: {v}"))["permission_mode"]
                .clone()
        };
        assert_eq!(mode_of_all(&tid_normal), serde_json::json!("normal"));
        assert_eq!(mode_of_all(&tid_yolo), serde_json::json!("yolo"));

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_with_cwd_writes_meta_cwd() {
        // 全局 workdir 与显式 cwd 指向不同目录:用于区分「按 thread 的 cwd 路由」
        // 与旧的「一律写全局 workdir」行为。
        let global = tempfile::TempDir::new().unwrap();
        let target = tempfile::TempDir::new().unwrap();
        let w = global.path().canonicalize().unwrap();
        let c = target.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = w.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": c.to_string_lossy() }});
        h.send(&req.to_string()).await;

        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                resp = Some(v);
                break;
            }
        }
        let resp = resp.expect("thread/start response");
        assert_eq!(
            resp["result"]["cwd"].as_str().unwrap(),
            c.to_string_lossy().to_string()
        );
        let thread_id = resp["result"]["thread_id"].as_str().unwrap().to_string();

        let store = crate::thread_store::ThreadStore::new(&c);
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].cwd, c.to_string_lossy().to_string());
        assert!(
            !crate::thread_store::ThreadStore::new(&w).exists(&thread_id),
            "thread must not leak into the global workdir store"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_with_bad_cwd_returns_invalid_params() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": "/nonexistent/definitely/not/here" }});
        h.send(&req.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32602);
        h.shutdown().await;
    }

    /// 索引键必须规范化为 canonical,与 `workspace/remove` 的规范化对齐:否则符号
    /// 链接形式的缺省 cwd 会以非 canonical 形式进索引,后续 remove 静默 no-op。
    /// 用符号链接制造 `canonical != raw`,确保本测试确实覆盖该回归面。
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_default_cwd_indexes_canonical_path() {
        let real = tempfile::TempDir::new().unwrap();
        let link_parent = tempfile::TempDir::new().unwrap();
        let link = link_parent.path().join("linked");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let raw = link.to_string_lossy().to_string();
        let canonical = link.canonicalize().unwrap();
        assert_ne!(
            raw,
            canonical.to_string_lossy().to_string(),
            "symlink must be non-canonical for this test to be meaningful"
        );

        let mut cfg = test_config();
        cfg.workdir = link.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        // 不带 cwd → 走缺省 cfg.workdir(即符号链接路径)。
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let _tid = read_thread_start_response(&mut h, 2).await;

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"workspace/list","params":{}}"#)
            .await;
        let mut listed = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                listed = Some(v);
                break;
            }
        }
        let v = listed.expect("workspace/list must respond");
        let path = v["result"]["workspaces"][0]["path"]
            .as_str()
            .expect("index must contain the default cwd");
        let recanon = Path::new(path)
            .canonicalize()
            .expect("indexed path must be canonicalizable");
        assert_eq!(
            recanon.to_string_lossy(),
            path,
            "index entry must already be canonical: {path}"
        );
        h.shutdown().await;
    }

    /// Design §11 的头号风险是「cwd 传递正确」:agent 工厂必须拿到该 thread 的 cwd,
    /// 而非全局 workdir。用记录型工厂把实参钉死。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_start_factory_receives_thread_cwd() {
        let target = tempfile::TempDir::new().unwrap();
        let c = target.path().canonicalize().unwrap();

        let seen: Arc<std::sync::Mutex<Vec<std::path::PathBuf>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_factory = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: crate::thread_store::ThreadMode| {
            seen_factory.lock().unwrap().push(cwd.to_path_buf());
            build_test_agent(session, cwd, mode)
        };
        let mut h = Harness::with_factory(build, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": c.to_string_lossy() }});
        h.send(&req.to_string()).await;
        let _tid = read_thread_start_response(&mut h, 2).await;

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![c.clone()],
            "agent factory must receive the thread's canonical cwd"
        );
        h.shutdown().await;
    }

    /// 两个目录各起一个 thread:`thread/listAll` 必须按 workspace 分成两组,每组
    /// 恰好含该目录的 thread(cwd 与 group.workspace 一致,thread_id 与 start 响应一致)。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_all_groups_by_workspace() {
        let dir_a = tempfile::TempDir::new().unwrap();
        let dir_b = tempfile::TempDir::new().unwrap();
        let a = dir_a
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let b = dir_b
            .path()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let mut h = Harness::new();
        initialize(&mut h).await;

        // workspace 路径 → 在该目录 start 出的 thread_id。
        let mut expected: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (id, path) in [(2u64, &a), (3u64, &b)] {
            let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":"thread/start",
                "params":{"cwd": path}});
            h.send(&req.to_string()).await;
            let tid = read_thread_start_response(&mut h, id).await;
            expected.insert(path.clone(), tid);
        }

        h.send(r#"{"jsonrpc":"2.0","id":9,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(9)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/listAll response");
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "两个目录 → 两个分组: {v}");

        // 索引按「最近使用在前」排序:先 start a 再 start b → 组顺序为 [b, a]。
        // start 请求串行(读罢 a 的响应才发 b),故 workspace/add 的先后确定。
        assert_eq!(
            groups[0]["workspace"],
            b.as_str(),
            "most-recent workspace must come first"
        );
        assert_eq!(
            groups[1]["workspace"],
            a.as_str(),
            "the earlier workspace must follow"
        );

        for g in groups {
            let ws = g["workspace"].as_str().unwrap();
            assert!(
                ws == a.as_str() || ws == b.as_str(),
                "unexpected workspace {ws}"
            );
            assert_eq!(g["exists"], true, "canonical tempdir must exist: {g}");
            let threads = g["threads"].as_array().unwrap();
            assert_eq!(
                threads.len(),
                1,
                "each workspace holds exactly one thread: {g}"
            );
            assert_eq!(
                threads[0]["cwd"].as_str().unwrap(),
                ws,
                "thread cwd must match its group workspace"
            );
            assert_eq!(
                threads[0]["thread_id"].as_str().unwrap(),
                expected[ws].as_str(),
                "group must carry the thread started in that workspace"
            );
        }
        h.shutdown().await;
    }

    /// 失效目录(索引里有、磁盘上已删)仍必须出现为一个组:`exists:false` 且
    /// `threads` 为空(不深扫)。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_list_all_includes_missing_dir_with_empty_threads() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().canonicalize().unwrap();
        let path_str = path.to_string_lossy().to_string();

        let mut h = Harness::new();
        initialize(&mut h).await;

        // 先让目录存在时写入索引,再从磁盘删除。
        let add = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"workspace/add",
            "params":{"path": path_str}});
        h.send(&add.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert!(v.get("error").is_none(), "workspace/add must succeed: {v}");
        std::fs::remove_dir_all(&path).unwrap();

        h.send(r#"{"jsonrpc":"2.0","id":3,"method":"thread/listAll","params":{}}"#)
            .await;
        let mut resp = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/listAll response");
        let groups = v["result"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "索引里恰有一个目录: {v}");
        let g = &groups[0];
        assert_eq!(g["workspace"].as_str().unwrap(), path_str);
        assert_eq!(g["exists"], false, "失效目录必须报 exists:false: {g}");
        assert_eq!(
            g["threads"].as_array().unwrap().len(),
            0,
            "失效目录不深扫,threads 必须为空: {g}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn turn_completion_persists_items_and_messages() {
        let dir = tempfile::TempDir::new().unwrap();
        let cfg = {
            let mut c = test_config();
            c.workdir = dir.path().to_path_buf();
            c
        };
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hello"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let log = dir
            .path()
            .join(".yi-agent/threads")
            .join(format!("{tid}.jsonl"));
        // 落盘是尽力而为且发生在 driver 写完 turn/completed 之后,故轮询等待。
        let mut text = String::new();
        for _ in 0..100 {
            match std::fs::read_to_string(&log) {
                Ok(t) if !t.trim().is_empty() => {
                    text = t;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        assert!(!text.is_empty(), "turn must be persisted");
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "one turn = one line");

        // 第一行必须含合成的 userMessage item 与 agent 回复。
        assert!(
            text.contains(r#""type":"userMessage""#),
            "missing user item: {text}"
        );
        assert!(text.contains("hello"), "missing prompt text: {text}");
        assert!(
            text.contains(r#""type":"agentMessage""#),
            "missing agent item: {text}"
        );
        assert!(
            text.contains(r#""role":"User""#),
            "missing core message: {text}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_replays_history_and_restores_context() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_factory = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          _cwd: &std::path::Path,
                          _mode: crate::thread_store::ThreadMode| {
            Ok(BuiltAgent {
                agent: apply_session(
                    yi_agent_core::Agent::new(
                        Arc::new(RecordingProvider {
                            seen: Arc::clone(&seen_factory),
                        }),
                        Arc::new(yi_agent_core::ToolRegistry::new()),
                        yi_agent_core::AgentConfig::default(),
                    ),
                    session,
                ),
                decision_tx: None,
                catalog: None,
                yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            })
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // turn 1
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"first"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        // resume:期望 thread/started → item/completed(含 user + agent) → 响应
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut replayed: Vec<String> = Vec::new();
        let mut item_ids: Vec<String> = Vec::new();
        let mut resumed = false;
        for _ in 0..12 {
            let v = h.read_value().await;
            match v.get("method").and_then(|m| m.as_str()) {
                Some("thread/started") => {}
                Some("item/completed") => {
                    replayed.push(v["params"]["item"]["type"].as_str().unwrap().to_string());
                    item_ids.push(v["params"]["item"]["id"].as_str().unwrap().to_string());
                }
                _ => {}
            }
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["result"]["thread_id"], tid);
                resumed = true;
                break;
            }
        }
        assert!(resumed, "thread/resume must respond");
        assert!(
            replayed.contains(&"userMessage".to_string()),
            "replay must include the user item: {replayed:?}"
        );
        assert!(
            replayed.contains(&"agentMessage".to_string()),
            "replay must include the agent item: {replayed:?}"
        );

        // turn 2:provider 应看到恢复后的完整上下文(user + assistant + 新 user)
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"second"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("item/completed") {
                item_ids.push(v["params"]["item"]["id"].as_str().unwrap().to_string());
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                break;
            }
        }

        let counts = seen.lock().unwrap().clone();
        assert_eq!(
            counts,
            vec![1, 3],
            "resumed turn must carry prior context (user + assistant + new user): {counts:?}"
        );

        // 回归:resume 后新 turn 的 item id 不得与回放的历史 id 冲突,
        // 否则前端按 id upsert 会覆盖历史。
        let mut unique = item_ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            item_ids.len(),
            "item ids must be unique across replay + resumed turn: {item_ids:?}"
        );

        h.shutdown().await;
    }

    /// Task 9:resume 必须读取该 thread 持久化的 `permission_mode` 并透传给 agent
    /// 工厂,使 yolo 线程重开后仍以 yolo 重建。工厂内部 `mode → cfg.yolo → 共享
    /// YoloSwitch` 的映射由 `bootstrap.rs::interactive_yolo_config_starts_switch_on`
    /// 钉死;此处只钉死「app-server 侧 resume 读了持久化模式并透传」这一契约
    /// (server 内存中的 switch 不可从外部观测,故用记录型工厂观察传参)。
    #[tokio::test(flavor = "multi_thread")]
    async fn resume_passes_persisted_mode_to_factory() {
        use crate::thread_store::{ThreadMode, ThreadStore};

        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();

        let seen: Arc<std::sync::Mutex<Vec<ThreadMode>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: ThreadMode| {
            seen_c.lock().unwrap().push(mode);
            build_test_agent(session, cwd, mode)
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // thread/start 必须走 Normal。
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            &[ThreadMode::Normal],
            "thread/start must build with Normal"
        );

        // 绕过 RPC,直接把该线程持久化的 permission_mode 改成 Yolo。
        let store = ThreadStore::new(dir.path());
        assert!(
            store.set_permission_mode(&tid, ThreadMode::Yolo).unwrap(),
            "thread meta must exist after thread/start"
        );

        // resume 应读取持久化的 Yolo 并透传给工厂。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resumed = false;
        for _ in 0..8 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert_eq!(v["result"]["thread_id"], tid);
                resumed = true;
                break;
            }
        }
        assert!(resumed, "thread/resume must respond");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen.last(),
            Some(&ThreadMode::Yolo),
            "resume must pass the persisted mode to the factory: {seen:?}"
        );
        h.shutdown().await;
    }

    /// Task 10:`thread/setPermissionMode` 必须实时翻转该线程的 `YoloSwitch`
    /// (运行期立即生效)并把 `permission_mode` 落盘(resume 后仍读得到)。
    /// server 内存里的 switch 外部不可观测,故用记录型工厂捕获工厂实际交给
    /// 该线程的同一个 `YoloSwitch`,以它断言运行期开关被翻转。
    #[tokio::test(flavor = "multi_thread")]
    async fn set_permission_mode_toggles_and_persists() {
        use crate::thread_store::{ThreadMode, ThreadStore};

        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();

        let seen: Arc<std::sync::Mutex<Vec<yi_agent_core::autonomy::YoloSwitch>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_c = Arc::clone(&seen);
        let build = move |session: Option<yi_agent_core::Session>,
                          cwd: &std::path::Path,
                          mode: ThreadMode| {
            let sw = yi_agent_core::autonomy::YoloSwitch::new(mode == ThreadMode::Yolo);
            seen_c.lock().unwrap().push(sw.clone());
            let mut built = build_test_agent(session, cwd, mode)?;
            built.yolo = sw;
            Ok(built)
        };
        let mut h = Harness::with_config(cfg, build, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        let sw = seen.lock().unwrap().last().cloned().unwrap();
        assert!(!sw.get(), "a freshly started thread must not be in yolo");

        // 切到 yolo:响应成功,运行期开关立即为 true,且已落盘。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"yolo"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );
        assert_eq!(v["result"], serde_json::json!({}));
        assert!(sw.get(), "the thread's YoloSwitch must turn on immediately");
        let store = ThreadStore::new(dir.path());
        assert_eq!(
            store
                .load(&tid)
                .unwrap()
                .expect("thread meta must exist")
                .meta
                .permission_mode,
            ThreadMode::Yolo,
            "yolo must persist to disk"
        );

        // 切回 normal:开关关掉,落盘也回到 normal。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"normal"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert!(
            v.get("error").is_none(),
            "setPermissionMode must succeed: {v}"
        );
        assert!(
            !sw.get(),
            "the thread's YoloSwitch must turn off immediately"
        );
        assert_eq!(
            store
                .load(&tid)
                .unwrap()
                .expect("thread meta must exist")
                .meta
                .permission_mode,
            ThreadMode::Normal,
            "normal must persist to disk"
        );

        h.shutdown().await;
    }

    /// Task 10:非法 `mode` 报 `-32602`(且优先于 thread 存在性校验);未知
    /// `threadId` 报 `-32011`。两者分别用独立请求覆盖。
    #[tokio::test(flavor = "multi_thread")]
    async fn set_permission_mode_rejects_bad_mode_and_unknown_thread() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        // 非法 mode:即便 thread 真实存在也必须 `-32602`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":"bogus"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "invalid mode must be invalid_params: {v}"
        );

        // 未知 thread + 合法 mode → `-32011`。
        h.send(
            r#"{"jsonrpc":"2.0","id":4,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"yolo"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32011,
            "unknown thread must be unknown_thread: {v}"
        );

        // 非法 mode + 未知 thread → 仍是 `-32602`:证明 mode 校验先于 thread
        // 存在性,不会因 threadId 未知而退化成 `-32011`。
        h.send(
            r#"{"jsonrpc":"2.0","id":5,"method":"thread/setPermissionMode","params":{"threadId":"nope","mode":"bogus"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "invalid mode must win over thread existence: {v}"
        );

        // 缺少 threadId → `-32602`(invalid params)。
        h.send(
            r#"{"jsonrpc":"2.0","id":6,"method":"thread/setPermissionMode","params":{"mode":"yolo"}}"#,
        )
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "missing threadId must be invalid_params: {v}"
        );

        // 非字符串 mode(数字)→ `-32602`:`as_str()` 落空,走 `_` 分支。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":7,"method":"thread/setPermissionMode","params":{{"threadId":"{tid}","mode":5}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(7)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("setPermissionMode must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "non-string mode must be invalid_params: {v}"
        );

        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/resume","params":{"threadId":"nope"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 2);
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    /// Design §8:resume 的目标目录已不存在时必须明确报错,不得静默回退 `$HOME`。
    /// 对内存中的 thread,其权威 cwd 已知(见 `ThreadSession`),故这是可达且必须
    /// 报错的路径。冷 thread 的元数据本就存在 workspace 目录内,目录删除后不可读,
    /// 那种情况仍走 `-32011`,不在此覆盖。
    #[tokio::test(flavor = "multi_thread")]
    async fn thread_resume_missing_cwd_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = cwd.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let req = serde_json::json!({"jsonrpc":"2.0","id":2,"method":"thread/start",
            "params":{ "cwd": cwd.to_string_lossy() }});
        h.send(&req.to_string()).await;
        let tid = read_thread_start_response(&mut h, 2).await;

        // 目录从磁盘消失:thread 仍在内存,但 cwd 已不可用。
        std::fs::remove_dir_all(&cwd).unwrap();

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut resp = None;
        for _ in 0..6 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(3)) {
                resp = Some(v);
                break;
            }
        }
        let v = resp.expect("thread/resume must respond");
        assert_eq!(
            v["error"]["code"], -32602,
            "resume into a vanished cwd must be invalid_params, not a silent fallback: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_ops_target_thread_cwd_not_global_workdir() {
        // 两个不同目录:thread 建在 `d`,全局 workdir 在 `w`。用于锁住
        // rename/delete 走 thread 自身 cwd,而不是全局 workdir / 索引回退。
        let w = tempfile::TempDir::new().unwrap();
        let d = tempfile::TempDir::new().unwrap();
        let w_dir = w.path().canonicalize().unwrap();
        let d_dir = d.path().canonicalize().unwrap();

        let mut cfg = test_config();
        cfg.workdir = w_dir.clone();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        initialize(&mut h).await;

        let start = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/start",
            "params": { "cwd": d_dir.to_string_lossy() },
        });
        h.send(&start.to_string()).await;
        let mut tid = None;
        for _ in 0..4 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(2)) {
                assert!(v.get("error").is_none(), "thread/start must succeed: {v}");
                tid = Some(v["result"]["thread_id"].as_str().unwrap().to_string());
                break;
            }
        }
        let tid = tid.expect("no thread/start response");

        // 把 `d` 从全局索引移除。此后只有「内存中该 thread 的权威 store」还能
        // 指向 `d`;若 rename/delete 退化成 store_for(索引 → cfg.workdir),
        // 就会落到全局 workdir `w` 上,从而暴露路由回归。
        let remove = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "workspace/remove",
            "params": { "path": d_dir.to_string_lossy() },
        });
        h.send(&remove.to_string()).await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert!(
            v.get("error").is_none(),
            "workspace/remove must succeed: {v}"
        );

        // rename 必须落在 `d`,而不是全局 workdir `w`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/rename","params":{{"threadId":"{tid}","title":"scoped"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 4);
        assert!(v.get("error").is_none(), "rename must succeed: {v}");

        let in_d = crate::thread_store::ThreadStore::new(&d_dir)
            .load(&tid)
            .expect("load in d must not fail")
            .expect("thread must live in d");
        assert_eq!(in_d.meta.title.as_deref(), Some("scoped"));
        assert!(
            !crate::thread_store::ThreadStore::new(&w_dir).exists(&tid),
            "nothing must leak into the global workdir"
        );

        // delete 同样必须作用在 `d`。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 5);
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(
            !crate::thread_store::ThreadStore::new(&d_dir).exists(&tid),
            "thread must be actually deleted in d"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_updates_title() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"my chat"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["id"], 3);
        assert!(v.get("error").is_none(), "rename must succeed: {v}");

        let meta = std::fs::read_to_string(
            dir.path()
                .join(".yi-agent/threads")
                .join(format!("{tid}.meta.json")),
        )
        .unwrap();
        assert!(
            meta.contains("my chat"),
            "meta must carry the title: {meta}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_rejects_empty_title() {
        let mut h = Harness::new();
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/rename","params":{{"threadId":"{tid}","title":"   "}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(
            v["error"]["code"], -32602,
            "blank title must be rejected: {v}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_rename_unknown_returns_unknown_thread() {
        let mut h = Harness::new();
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/rename","params":{"threadId":"nope","title":"x"}}"#)
            .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_removes_files_and_unknown_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert!(v.get("error").is_none(), "delete must succeed: {v}");
        assert!(
            !dir.path()
                .join(".yi-agent/threads")
                .join(format!("{tid}.meta.json"))
                .exists(),
            "meta must be gone"
        );

        // 再次删除:thread 已完全未知 → -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let v = h.read_value().await;
        assert_eq!(v["error"]["code"], -32011);
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn thread_delete_active_thread_removes_it_from_memory() {
        // 用隔离 workdir:删除活跃 thread 时必须能断言磁盘状态。
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let mut h = Harness::with_config(cfg, build_slow_agent, PERMISSION_TIMEOUT);
        let tid = start_thread(&mut h).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        // 等 turn 活跃。
        loop {
            let v = h.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/started") {
                break;
            }
        }
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        let mut deleted = false;
        for _ in 0..40 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(4)) {
                assert!(v.get("error").is_none(), "delete must succeed: {v}");
                deleted = true;
                break;
            }
        }
        assert!(deleted, "thread/delete must respond");

        // 留出窗口让「未等待落盘」的实现迟到写入:driver 收到中断后仍会
        // append_turn,若 delete 抢在它之前删文件,日志会被复活。
        tokio::time::sleep(Duration::from_millis(300)).await;

        // 删除后磁盘上两个文件都必须消失(活跃 turn 落盘不得复活 .jsonl)。
        let threads_dir = dir.path().join(".yi-agent/threads");
        assert!(
            !threads_dir.join(format!("{tid}.meta.json")).exists(),
            "meta must be gone after deleting an active thread"
        );
        assert!(
            !threads_dir.join(format!("{tid}.jsonl")).exists(),
            "log must be gone after deleting an active thread"
        );

        // 删除后该 thread 已不在内存:再发 turn 应得 -32011。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"x"}}]}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(5)) {
                assert_eq!(
                    v["error"]["code"], -32011,
                    "deleted thread must be unknown: {v}"
                );
                break;
            }
        }

        // 磁盘无日志 → resume 不能复活已删除的 thread。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":6,"method":"thread/resume","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(6)) {
                assert_eq!(
                    v["error"]["code"], -32011,
                    "resume must not resurrect a deleted thread: {v}"
                );
                break;
            }
        }
        h.shutdown().await;
    }
}
