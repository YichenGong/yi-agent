//! `side/*` 域:旁支会话的六个方法(spec §8.1)。
//!
//! 分派的两条硬规则:
//!
//! 1. **慢活不在请求路径上。** `open` 要读父会话的持久状态、复制附件、装工具集,
//!    `close` 要等核心任务真正收敛。这些都放 `side_tasks` 里跑,主循环只做参数
//!    校验与派发。主循环是单 task,内联 `await` 会把所有连接(含桌面 GUI)一起
//!    堵住——`side_creating_open_does_not_block_the_loop` 钉的就是这一点。
//! 2. **不借 `AppState<'a>` 跨 spawn。** 那个结构按请求借用主循环的局部量,
//!    move 进 task 就是悬空引用。派发前把它需要的东西克隆成 owned:
//!    `Arc<SideRegistry>` + `SideResponseSender`。
//!
//! 权限档:`list`/`readItems` 只读(`Observe`);`open`/`turnStart`/`turnInterrupt`/
//! `close` 会花真钱或改状态(`Control`)。认证先于 scope——`serve()` 在 dispatch
//! 之前就拒绝了未 `initialize` 的连接。
//!
//! 日志只记 method / request id / 稳定 reason。**不记**正文、item、配置凭据:
//! 旁支的正文与父会话一样是用户内容。

use std::sync::Arc;

use crate::protocol::{
    Item, RequestEnvelope, RequestId, RpcError, Scope, SideCloseParams, SideEffectiveConfigWire,
    SideListParams, SideListResult, SideOpenParams, SideReadItemsParams, SideReadItemsResult,
    SideStatusWire, SideSummaryWire, SideTokenTotalsWire, SideTurnInterruptParams,
    SideTurnStartParams, SideTurnStartResult, SideUsageWire, ToolStatus,
};
use crate::rpc::{AppState, Flow};
use crate::server::{SideResponseSender, err_response, ok_response, write_response};
use crate::side_conversation::{SideEffectiveConfig, SideRegistry};
use yi_agent_runtime::side_conversation::{
    SIDE_TRANSCRIPT_MAX_ITEM_BYTES, SideError, SideErrorReason, SideItem, SideItemContent,
    SideToolStatus, SideUsage,
};

/// 本域认领的方法(与 `side_methods_baseline.txt` 一一对应)。
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "side/open"
            | "side/list"
            | "side/readItems"
            | "side/turnStart"
            | "side/turnInterrupt"
            | "side/close"
    )
}

/// 该方法要求的最低权限档。
///
/// `open` 会创建 runner 并**读父会话内容**,所以它不是 Observe 能做的事;只读
/// 发现(`list`)与阅读(`readItems`)才是。
fn required_scope(method: &str) -> Scope {
    match method {
        "side/list" | "side/readItems" => Scope::Observe,
        _ => Scope::Control,
    }
}

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    if !owns(method) {
        return Ok(Flow::Unknown);
    }
    if state.client_scope < required_scope(method) {
        write_response(
            state.hub,
            state.client,
            err_response(id, RpcError::insufficient_scope(required_scope(method))),
        )
        .await?;
        return Ok(Flow::Continue);
    }

    // 参数在请求路径上解,因为「形状不对」必须立刻回给调用方,且不能占一个任务
    // 名额。读盘、装 runner、等收敛都在后面 spawn 出去。
    let registry = Arc::clone(state.side_registry);
    let responder = SideResponseSender::new(Arc::clone(state.hub), state.client.clone());
    let tasks = &mut state.side_tasks;

    match method {
        "side/open" => {
            let params: SideOpenParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .open(&params.parent_thread_id)
                    .await
                    .map(|summary| SideSummaryWire::from_registry(&summary));
                responder.send(id_for_task, result).await;
            });
        }
        "side/list" => {
            let params: SideListParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            // 列表是纯内存读,且客户端每 5 秒会对账一次;为它排一个任务只会让
            // 「旁支开了没有」这个判断多绕一次调度。
            let sides = registry
                .list(&params.parent_thread_id)
                .await
                .into_iter()
                .map(|summary| SideSummaryWire::from_registry(&summary))
                .collect();
            write_response(
                state.hub,
                state.client,
                ok_response(id, serde_json::json!(SideListResult { sides })),
            )
            .await?;
        }
        "side/readItems" => {
            let params: SideReadItemsParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            // cursor 必须绑定 token:没有 token 的 cursor 看不出属于哪个快照,
            // 拼页就会把两个时刻的内容接在一起。
            if params.cursor.is_some() && params.snapshot_token.is_none() {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::side(&SideError::new(
                            SideErrorReason::InvalidParams,
                            "cursor 必须与 snapshotToken 一起使用",
                        )),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = read_items(&registry, &params).await;
                responder.send(id_for_task, result).await;
            });
        }
        "side/turnStart" => {
            let params: SideTurnStartParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .turn_start(&params.side_id, params.text, params.client_request_id)
                    .await
                    .map(|turn_id| SideTurnStartResult { turn_id });
                responder.send(id_for_task, result).await;
            });
        }
        "side/turnInterrupt" => {
            let params: SideTurnInterruptParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .interrupt(&params.side_id, &params.turn_id)
                    .await
                    .map(|()| serde_json::json!({}));
                responder.send(id_for_task, result).await;
            });
        }
        "side/close" => {
            let params: SideCloseParams = match parse(&req.params) {
                Ok(params) => params,
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?;
                    return Ok(Flow::Continue);
                }
            };
            let id_for_task = id.clone();
            tasks.spawn(async move {
                let result = registry
                    .close(&params.side_id, params.expected_active_turn_id.as_deref())
                    .await
                    .map(|()| serde_json::json!({}));
                responder.send(id_for_task, result).await;
            });
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// 解请求参数,把 serde 的报错压成 `invalid_params`。
///
/// serde 的原文会带上字段名与类型,足以让客户端定位问题,且不含用户正文;
/// `deny_unknown_fields` 触发的「未知字段」也走这里。
fn parse<T: serde::de::DeserializeOwned>(params: &serde_json::Value) -> Result<T, RpcError> {
    serde_json::from_value(params.clone()).map_err(|error| {
        RpcError::side(&SideError::new(
            SideErrorReason::InvalidParams,
            format!("参数不合法：{error}"),
        ))
    })
}

/// 一页最多装多少字节的**完整信封**。
///
/// 512 KiB 是线上信封上限,这里留出四分之一给 JSON-RPC 外壳、转义膨胀与其它
/// 字段。分页按真实序列化字节试装,不只按条数——一条 128 KiB 的 item 与一百条
/// 1 KiB 的 item 条数差两个数量级,字节数却可能相同。
const SIDE_PAGE_BUDGET_BYTES: usize = 384 * 1024;

async fn read_items(
    registry: &SideRegistry,
    params: &SideReadItemsParams,
) -> Result<SideReadItemsResult, SideError> {
    let snapshot = registry.snapshot(&params.side_id).await?;
    let mut items = Vec::with_capacity(snapshot.items.len());
    let mut used = 0usize;
    for item in &snapshot.items {
        let wire = wire_item(item)?;
        let size = serde_json::to_vec(&wire)
            .map_err(|error| {
                SideError::new(
                    SideErrorReason::SnapshotTooLarge,
                    format!("旁支条目无法序列化：{error}"),
                )
            })?
            .len();
        if !items.is_empty() && used + size > SIDE_PAGE_BUDGET_BYTES {
            break;
        }
        used += size;
        items.push(wire);
    }

    Ok(SideReadItemsResult {
        items,
        snapshot_seq: snapshot.summary.last_seq,
        status: SideStatusWire::from_runtime(snapshot.summary.status),
        usage: wire_usage(snapshot.usage),
        active_turn_id: snapshot.summary.active_turn_id.clone(),
        accepted_requests: snapshot.accepted.into_iter().collect(),
        effective_config: wire_config(&snapshot.config),
        snapshot_token: uuid::Uuid::new_v4().to_string(),
        // 本任务只回一页(Task 13 的 ReplayStore 接手分页与 token 续期)。诚实地
        // 回 `None`,而不是给一个客户端永远用不上的游标。
        next_cursor: None,
    })
}

fn wire_usage(usage: SideUsage) -> SideUsageWire {
    SideUsageWire {
        total: SideTokenTotalsWire {
            input: usage.total.input,
            output: usage.total.output,
            cache_creation: usage.total.cache_creation,
            cache_read: usage.total.cache_read,
            reasoning: usage.total.reasoning,
        },
        complete: usage.complete,
    }
}

fn wire_config(config: &SideEffectiveConfig) -> SideEffectiveConfigWire {
    SideEffectiveConfigWire {
        model: config.model.clone(),
        reasoning_effort: config.reasoning_effort.clone(),
        preserve_thinking: config.preserve_thinking,
        // 旁支的工具集由白名单装配,且每个工具都必须自报只读才允许注册。
        read_only: true,
        tool_names: config.tool_names.clone(),
    }
}

/// 把共享转录里的一个条目适配成协议 `Item`。
///
/// **只做形状适配,不重写切分或预算**:转录那边已经按 128 KiB 序列化预算切好,
/// 这里再截断一次就会让「同一段文本在两端的边界」不一致。适配后再核一次字节数,
/// 超了就是转录层的缺陷,如实报 `snapshot_too_large`,而不是悄悄砍掉。
pub(crate) fn wire_item(item: &SideItem) -> Result<Item, SideError> {
    let wire = match &item.content {
        SideItemContent::UserMessage { text } => Item::UserMessage {
            id: item.id.clone(),
            text: text.clone(),
            attachments: Vec::new(),
            images: Vec::new(),
        },
        SideItemContent::AgentMessage { text } => Item::AgentMessage {
            id: item.id.clone(),
            text: text.clone(),
        },
        SideItemContent::Thinking { text, redacted } => Item::Thinking {
            id: item.id.clone(),
            text: text.clone(),
            redacted: *redacted,
        },
        SideItemContent::ToolCall {
            call_id,
            name,
            input,
            result,
            status,
            ..
        } => Item::ToolCall {
            id: item.id.clone(),
            call_id: call_id.clone(),
            name: name.clone(),
            input: input.clone(),
            status: match status {
                SideToolStatus::Running => ToolStatus::Running,
                SideToolStatus::Completed => ToolStatus::Completed,
                SideToolStatus::Failed => ToolStatus::Failed,
            },
            result: result.clone(),
            // 图片二进制不进旁支通知或回放(spec §8.2):模型读图不等于两端要下载原图。
            images: Vec::new(),
        },
        // 提示在被切成 item 时就已计入预算,这里只加一次固定前缀。
        SideItemContent::Notice { text } => Item::AgentMessage {
            id: item.id.clone(),
            text: format!("提示：{text}"),
        },
    };

    let size = serde_json::to_vec(&wire)
        .map_err(|error| {
            SideError::new(
                SideErrorReason::SnapshotTooLarge,
                format!("旁支条目无法序列化：{error}"),
            )
        })?
        .len();
    if size > SIDE_TRANSCRIPT_MAX_ITEM_BYTES {
        return Err(SideError::new(
            SideErrorReason::SnapshotTooLarge,
            format!("旁支条目序列化后 {size} 字节，超出单条上限"),
        ));
    }
    Ok(wire)
}

#[cfg(test)]
mod tests;
