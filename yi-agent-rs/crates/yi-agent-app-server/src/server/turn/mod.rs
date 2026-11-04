//! turn 生命周期：起轮、准备、持久化、会话命令、标题生成、重放。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。本组 1,435 行，按职责分五片。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::{Notification, RequestId, RpcError, ThreadStatus};
use crate::server::{err_response, ok_response, update_status, write_notification, write_response};
use crate::session::{ThreadSession, TurnPrompt};
use serde_json::json;
use tokio::sync::mpsc;
use yi_agent_subagent::thread_root::ThreadRoot;

pub(crate) mod command;
pub(crate) mod failure;
#[cfg(test)]
mod failure_tests;
pub(crate) mod persist;
pub(crate) mod replay;
pub(crate) mod start;
pub(crate) mod title;

pub(crate) use start::start_thread_core;

pub(crate) use command::{apply_session, apply_session_command};
pub(crate) use persist::{build_partial, checkpoint, persist_and_finish_turn};
pub(crate) use replay::{
    REPLAY_CHUNK_BYTES, REPLAY_CHUNK_MAX_ITEMS, chunk_items_for_replay, completed_items_agent_text,
    finished_event, interrupt_and_wait_for_persist, lead_reply_text, linked_extra_roots,
    route_client_response, settle_pending_lead_replies,
};
pub(crate) use title::generate_title_for_first_input;

/// `prepare_turn_core` 的失败。每一种都对应 `turn/start` 的一条固定错误响应,
/// 由 `start_turn_core` 负责把它写出去(这样「校验」与「写响应」解耦,驱动器可以
/// 只复用前者而不向任何客户端广播)。
#[derive(Debug)]
pub(crate) enum TurnPrepareError {
    MissingThreadId,
    EmptyInput,
    UnknownThread(String),
    TurnInProgress(String),
    /// 插件正在排空（或准入被拒）。带原因，用户能看到"等这次维护结束"。
    NotAdmitted(String),
    /// 附件不可读 / 超出上限 / 不是文件。
    InvalidAttachment(String),
    /// 显式路径的图片输入（`{type:"image", path}`）不可用。带 `data.code =
    /// "invalid_image"`，客户端据此指认是哪张图被拒（见 `invalid_image`）。
    InvalidImage(String),
}
/// 一个已经「占位」好的 turn,但**尚未向任何客户端发帧**。
///
/// 校验通过后 `active_turn_id` 已被占成 `turn_id`;`prompt_tx` 是投递通道,
/// `status_handle` 供调用方推 Running 状态。字段全部 `pub(crate)`,以便
/// `ServeLauncher` 复用这条无帧路径。
pub(crate) struct PreparedTurn {
    pub(crate) thread_id: String,
    pub(crate) turn_id: String,
    pub(crate) prompt_tx: mpsc::Sender<TurnPrompt>,
    /// 送给模型的 prompt（含附件清单注入）。
    pub(crate) prompt: String,
    /// 气泡正文:用户原话，**不含**注入的清单。
    pub(crate) display_text: String,
    /// 本轮附件元数据；回显在开启项上，并随 `TurnPrompt` 交给 driver 落盘。
    pub(crate) attachments: Vec<crate::protocol::Attachment>,
    /// 本轮图片内容块（含 base64），随 `TurnPrompt` 进模型上下文。
    pub(crate) image_blocks: Vec<yi_agent_core::ContentBlock>,
    /// 本轮图片元数据；回显在开启项上（`Item::UserMessage.images`）。
    pub(crate) image_refs: Vec<crate::protocol::ImageRef>,
    pub(crate) activate: Option<Arc<ThreadRoot>>,
    pub(crate) status_handle: Arc<std::sync::Mutex<ThreadStatus>>,
}
/// 起一个 turn 的**无帧准备段**:校验 thread 存在、`active_turn_id` 空闲、
/// 提取 prompt、占 `active_turn_id = Some(turn_id)`、构造 `TurnPrompt`。
///
/// **不写任何通知 / 响应**——正因如此驱动器才能「起会话而不向客户端发帧」
/// (驱动器不得复用带帧的 `start_turn_core`)。失败返回
/// `Err(TurnPrepareError)`,由调用方决定怎么写错误响应(或对驱动器而言记日志)。
pub(crate) async fn prepare_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    params: &serde_json::Value,
    // 插件准入。`None` = 不设准入（RPC 起的普通会话，与改动前逐字一致）。
    //
    // # 为什么准入只对"已绑定插件"的会话生效
    //
    // 排空屏障要等的是**插件发起的回合**，普通用户聊天与插件无关，拿它去占
    // 排空名额会让"卸载插件"卡在一个正在打字的用户身上。所以调用方只在会话
    // 有插件绑定时才传 `Some`。
    admission: Option<(&yi_agent_plugins::Admission, yi_agent_plugins::Binding)>,
) -> Result<PreparedTurn, TurnPrepareError> {
    let thread_id = params
        .get("threadId")
        .and_then(|v| v.as_str())
        .ok_or(TurnPrepareError::MissingThreadId)?
        .to_string();
    // 一次性拆出「文本 + 附件源路径 + 图片输入」:文本为空但有附件或图片时仍然
    // 放行(附件-only / 图片-only)。
    let (text, source_paths) = crate::attachments::parse_input(params);
    let image_inputs = crate::attachments::parse_image_inputs(params);
    let text_opt = if text.trim().is_empty() {
        None
    } else {
        Some(text)
    };
    if text_opt.is_none() && source_paths.is_empty() && image_inputs.is_empty() {
        return Err(TurnPrepareError::EmptyInput);
    }

    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());
    // 内层作用域:让 `&mut threads` 的借用先结束(下面复制附件还要再借一次)。
    let (prompt_tx, status_handle, cwd) = {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        (
            session.prompt_tx.clone(),
            Arc::clone(&session.status),
            session.cwd.clone(),
        )
    };

    // 复制附件进工作区。放在占 `active_turn_id` **之前**:失败要能干净地拒绝,
    // 不留一个「已占用但起不来」的 turn。
    let max_bytes = crate::attachments::resolve_max_bytes();
    let mut attachments = Vec::with_capacity(source_paths.len());
    for source in &source_paths {
        match crate::attachments::store_attachment(
            Path::new(&cwd),
            &thread_id,
            Path::new(source),
            max_bytes,
        ) {
            Ok(attachment) => attachments.push(attachment),
            Err(crate::attachments::AttachmentError::TooLarge { size, max }) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {source} is {size} bytes, over the {max} byte limit"
                )));
            }
            Err(crate::attachments::AttachmentError::NotAFile(path)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {} is not a regular file",
                    path.display()
                )));
            }
            Err(crate::attachments::AttachmentError::Io(e)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "could not read attachment {source}: {e}"
                )));
            }
        }
    }

    // 复制 + 编码图片。同样放在占 `active_turn_id` **之前**。
    //
    // 两种来源在这里**收敛**到同一条管线（裁定 R3）：
    // - `Path`（桌面）：文件选择器给的根外绝对路径，先 `store_attachment` 复制进
    //   工作区；
    // - `Upload`（iOS 分片上传）：`image/upload/*` 已经把 staging 收拢成同一份
    //   `store_attachment` 结果（含 `size` = 存盘文件字节长度），这里只反查登记表。
    //
    // 之后一律读**根内副本**再编码：`prepare_image_file` 在根外读原文件也可以，
    // 但那样回显的 `path` 与真正读到的字节可能来自两个位置；统一读根内副本，
    // `path` 与内容才必然同源，`ImageRef.size` 也才必然等于该文件的字节长度。
    //
    // 坏图的两条处置**刻意不同**，判据是「失败能不能靠重试自愈」：
    // - `Upload`：`uploadId` 未知 / 未 commit / 已过期（10 分钟 TTL）都是**不可
    //   自愈**的——服务端手里已没有重试所需的任何东西，客户端每重发一次都只会拿
    //   到一模一样的失败，而它还会把 chip 留着，于是整轮对话被这一张图钉死。故
    //   **丢弃这张图**（warn 一条）并继续起 turn：用户照样能把消息发出去。
    //   注：上传路径的图已由 `begin` 的 `MAX_IMAGE_BYTES` 设闸，故「超大」不会
    //   走到这里；但「上传的字节其实不是图」会——`commit` 只收拢字节，判图是这
    //   一步的事。
    // - `Path`：路径是这次请求里的显式参数，指不到/不是图是**调用方请求本身**的
    //   问题（桌面端的图在本地盘上，重发同一路径多半就是同一个结果），仍按
    //   `InvalidImage` 硬拒，与文档附件的 `InvalidAttachment` 同口径。
    let mut image_blocks = Vec::with_capacity(image_inputs.len());
    let mut image_refs = Vec::with_capacity(image_inputs.len());
    for input in &image_inputs {
        let (label, stored) = match input {
            crate::attachments::ImageInput::Path(source) => {
                let stored = match crate::attachments::store_attachment(
                    Path::new(&cwd),
                    &thread_id,
                    Path::new(source),
                    max_bytes,
                ) {
                    Ok(stored) => stored,
                    Err(e) => {
                        return Err(TurnPrepareError::InvalidImage(format!(
                            "image {source}: {e:?}"
                        )));
                    }
                };
                (source.clone(), stored)
            }
            crate::attachments::ImageInput::Upload(upload_id) => {
                // 未 commit / 未知 / 已过期都落到这里。
                //
                // 反查必须带 `thread_id`：`uploadId` 是客户端手里的不透明句柄，
                // 只看 id 不看归属，就能把别条 thread 的图摄进本 turn（跨 thread
                // 取数）。归属不符与「未知」一样丢弃，不向调用方区分两者。
                match uploads.resolve_for(upload_id, &thread_id) {
                    Some(attachment) => (upload_id.clone(), attachment),
                    None => {
                        tracing::warn!(
                            %thread_id,
                            upload_id = %upload_id,
                            "dropping unresolvable uploaded image; continuing the turn",
                        );
                        continue;
                    }
                }
            }
        };
        let stored_abs = Path::new(&cwd).join(&stored.path);
        match yi_agent_tools::image_prep::prepare_image_file(
            &stored_abs,
            yi_agent_core::ImageDetail::High,
            yi_agent_tools::image_prep::resolve_budget(),
        ) {
            Ok(yi_agent_tools::image_prep::PreparedImage::Ready {
                media_type, data, ..
            }) => {
                image_blocks.push(yi_agent_core::ContentBlock::Image {
                    source: yi_agent_core::ImageSource::Base64 {
                        media_type: media_type.clone(),
                        data,
                    },
                    detail: yi_agent_core::ImageDetail::High,
                    path: Some(stored.path.clone()),
                });
                image_refs.push(crate::protocol::ImageRef {
                    path: stored.path,
                    media_type,
                    size: stored.size,
                    detail: Some(yi_agent_core::ImageDetail::High.as_wire_str().to_string()),
                });
            }
            // 已落盘、但读不出/超预算的上传图同样丢：它和「未知 uploadId」一样属于
            // 重发必然复现的失败，不该毒死整轮。
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message })
                if matches!(input, crate::attachments::ImageInput::Upload(_)) =>
            {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    %message,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Ok(yi_agent_tools::image_prep::PreparedImage::Omitted { message }) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {message}"
                )));
            }
            Err(e) if matches!(input, crate::attachments::ImageInput::Upload(_)) => {
                tracing::warn!(
                    %thread_id,
                    image = %label,
                    error = ?e,
                    "dropping an unusable uploaded image; continuing the turn",
                );
            }
            Err(e) => {
                return Err(TurnPrepareError::InvalidImage(format!(
                    "image {label}: {e:?}"
                )));
            }
        }
    }

    // 取准入凭据。必须在占位**之前**：被排空拒绝时不该留下 `active_turn_id`,
    // 否则这个会话会永远报 turn_in_progress 直到进程重启。
    let permit = match admission {
        Some((admission, binding)) => Some(
            admission
                .admit_turn(&turn_id, binding)
                .map_err(|error| TurnPrepareError::NotAdmitted(error.to_string()))?,
        ),
        None => None,
    };

    // 占位(复制成功之后)。
    {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        session.active_turn_id = Some(turn_id.clone());
        // permit 与 active_turn_id 同生共死：所有清空活跃 turn 的地方把它一起
        // 置 `None`，`TurnPermit::drop` 即释放 flock 与登记表条目。
        session.turn_permit = permit;
    }
    let activate = pending_activation.get(&thread_id).cloned().flatten();
    // prompt 送进模型时带上附件清单;气泡文本仍是原话(见 `display_text`)。
    let prompt = crate::attachments::prompt_with_attachments(
        text_opt.as_deref().unwrap_or(""),
        &attachments,
    );
    Ok(PreparedTurn {
        thread_id,
        turn_id,
        prompt_tx,
        prompt,
        display_text: text_opt.unwrap_or_default(),
        attachments,
        image_blocks,
        image_refs,
        activate,
        status_handle,
    })
}
/// 把 `prepare_turn_core` 的失败翻成 6a 建立的错误响应(逐字不变)。
pub(crate) fn turn_prepare_rpc_error(error: TurnPrepareError) -> RpcError {
    match error {
        TurnPrepareError::MissingThreadId => RpcError::invalid_params("missing threadId"),
        TurnPrepareError::EmptyInput => RpcError::invalid_params("missing or empty input text"),
        TurnPrepareError::UnknownThread(thread_id) => RpcError::unknown_thread(&thread_id),
        TurnPrepareError::TurnInProgress(thread_id) => RpcError::turn_in_progress(&thread_id),
        TurnPrepareError::NotAdmitted(reason) => RpcError::invalid_params(reason),
        TurnPrepareError::InvalidAttachment(msg) => RpcError::invalid_params(msg),
        TurnPrepareError::InvalidImage(msg) => RpcError::invalid_image(msg),
    }
}
/// The item that opens a turn: the user's own message.
///
/// The baseline server emitted this **only at persist time**
/// (`persist_and_finish_turn`), so a client watching a turn live saw the agent's
/// items stream in first and the prompt appended last — a viewer opening the
/// thread later found the opening prompt at the very bottom of the transcript.
/// Emitting it as the turn's *first* item, from the one helper both the RPC path
/// (`start_turn_core`) and the board path (`ServeLauncher`) call, fixes the order
/// at the source and keeps the id identical to the persisted one (`user-<turn_id>`),
/// so the desktop merges the live bubble with the replayed one by id.
pub(crate) fn opening_user_item(
    turn_id: &str,
    text: &str,
    attachments: Vec<crate::protocol::Attachment>,
    images: Vec<crate::protocol::ImageRef>,
) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage {
        id: format!("user-{turn_id}"),
        text: text.to_string(),
        attachments,
        images,
    }
}
/// Emit one item as both `item/started` and `item/completed`.
///
/// Content-layer notifications carry no response frame, so both the RPC path and
/// the board launcher may (and must, to stay identical) call this. Failures are
/// propagated: a broken hub is a real fault, not something to swallow.
pub(crate) async fn emit_item(
    hub: &Arc<crate::broadcast::Broadcaster>,
    thread_id: &str,
    item: crate::protocol::Item,
) -> anyhow::Result<()> {
    write_notification(
        hub,
        &Notification::ItemStarted {
            thread_id: thread_id.to_string(),
            item: item.clone(),
        },
    )
    .await?;
    write_notification(
        hub,
        &Notification::ItemCompleted {
            thread_id: thread_id.to_string(),
            item,
        },
    )
    .await
}
/// 起一个 turn 的 RPC 包装:调 `prepare_turn_core` → 发 `turn/started` →
/// 推 Running 状态 → 写响应 → 投递 prompt。
///
/// 校验失败时在此就地写错误响应并返回 `Ok(None)`,调用方据此 continue——帧的
/// 顺序与错误响应语义与 6a 抽取前逐字一致(响应急在 prompt 投递之前)。
/// 成功返回 `Ok(Some(thread_id))`。
pub(crate) async fn start_turn_core(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
) -> anyhow::Result<Option<String>> {
    start_turn_core_inner(
        threads,
        pending_activation,
        uploads,
        hub,
        client,
        params,
        id,
        None,
    )
    .await
}

/// 出站阶段中可在测试里注入失败的那三个点。
///
/// 只影响测试：生产包装永远传 `None`，走的仍是同一段 async 流程（三个真实函数
/// 一个都没被替换掉），因此注入点不会把生产路径测成另一条逻辑。
///
/// 生产构建里没有任何调用方构造它的变体（包装永远传 `None`），故允许死代码；
/// 测试直接调用 `start_turn_core_inner` 并传入具体阶段。
#[allow(dead_code)]
#[derive(Clone, Copy)]
enum StartStage {
    Opening,
    Running,
    Response,
}

/// 参数与 `start_turn_core` 一一对应，另加一个测试注入点；拆成结构体只会让
/// 「哪些是真实参数」更难看清。
#[allow(clippy::too_many_arguments)]
async fn start_turn_core_inner(
    threads: &mut HashMap<String, ThreadSession>,
    pending_activation: &HashMap<String, Option<Arc<ThreadRoot>>>,
    uploads: &mut crate::image_upload::UploadRegistry,
    hub: &Arc<crate::broadcast::Broadcaster>,
    client: &crate::broadcast::ClientId,
    params: &serde_json::Value,
    id: RequestId,
    fault: Option<StartStage>,
) -> anyhow::Result<Option<String>> {
    // 普通 RPC 回合不占插件准入：用户聊天与插件排空无关。
    let prepared = match prepare_turn_core(threads, pending_activation, uploads, params, None).await
    {
        Ok(prepared) => prepared,
        Err(error) => {
            write_response(hub, client, err_response(id, turn_prepare_rpc_error(error))).await?;
            return Ok(None);
        }
    };

    // 失败收口要用的身份先存下来：`prepared.turn_id` 之后会随 `TurnPrompt` move 走。
    let thread_id = prepared.thread_id.clone();
    let turn_id = prepared.turn_id.clone();

    // 顺序确定:先 turn/started 通知,再发本轮开启项(用户提问),再推 Running
    // 状态,再响应,最后投递 prompt。开启项排在 agent 任何 item 之前,实时观看与
    // 事后回放的顺序才一致(见 `opening_user_item`)。
    //
    // `turn/started` 之前失败只需回滚自己的占位；一旦宣告成功，之后任何失败都
    // 必须补一次失败收口（否则界面永远停在「运行中」）。
    if let Err(error) = write_notification(
        hub,
        &Notification::TurnStarted {
            thread_id: thread_id.clone(),
            turn_id: turn_id.clone(),
        },
    )
    .await
    {
        super::turn::failure::rollback_turn(threads, hub, &thread_id, &turn_id).await;
        return Err(error);
    }

    let outbound = async {
        if matches!(fault, Some(StartStage::Opening)) {
            return Err(anyhow::anyhow!("injected opening failure"));
        }
        emit_item(
            hub,
            &thread_id,
            opening_user_item(
                &turn_id,
                &prepared.display_text,
                prepared.attachments.clone(),
                prepared.image_refs.clone(),
            ),
        )
        .await?;
        if matches!(fault, Some(StartStage::Running)) {
            return Err(anyhow::anyhow!("injected running failure"));
        }
        update_status(
            hub,
            &prepared.status_handle,
            &thread_id,
            ThreadStatus::Running,
        )
        .await?;
        if matches!(fault, Some(StartStage::Response)) {
            return Err(anyhow::anyhow!("injected response failure"));
        }
        write_response(
            hub,
            client,
            ok_response(id, json!({ "turn_id": turn_id.clone() })),
        )
        .await?;
        Ok(())
    }
    .await;

    if let Err(error) = outbound {
        // 已宣告的 turn 必须恰好一次失败收口：先落完成水位、广播 failed，再按
        // 身份回滚。原始错误仍向调用方传播（不伪装成成功）。
        super::turn::failure::fail_announced_turn(
            threads,
            hub,
            &thread_id,
            &turn_id,
            &error.to_string(),
        )
        .await;
        return Err(error);
    }

    if prepared
        .prompt_tx
        .send(TurnPrompt {
            turn_id: turn_id.clone(),
            prompt: prepared.prompt,
            activate: prepared.activate,
            attachments: prepared.attachments,
            image_blocks: prepared.image_blocks,
            image_refs: prepared.image_refs,
            display_text: prepared.display_text,
            lead_request_id: None,
        })
        .await
        .is_err()
    {
        // 成功响应已经发出，绝不能再补第二个同 id 的 RPC 响应；以失败完成通知
        // 终结这个已宣告的 turn，并按身份恢复 Idle。函数正常返回主循环，不把
        // 一次投递失败升级成 transport fatal（那会把整个 app-server 退出）。
        super::turn::failure::fail_announced_turn(
            threads,
            hub,
            &thread_id,
            &turn_id,
            "turn driver closed before prompt delivery",
        )
        .await;
        return Ok(Some(thread_id));
    }
    // send 成功即由 driver 接手：此处不得再发 failed、不得回滚。
    Ok(Some(thread_id))
}
