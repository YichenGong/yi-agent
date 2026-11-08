//! `flags/*` 与 `build/*` 域的 RPC 处理函数。

use std::sync::Arc;

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "flags/list" | "build/info" => {
            match crate::flags_rpc::handle_flags_request(req.method.as_str(), &req.params) {
                Ok(value) => {
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        "flags/set" => {
            // 改配置与 `model/upsert` 同档：需要 Control。
            if state.client_scope < crate::protocol::Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        crate::protocol::RpcError::insufficient_scope(
                            crate::protocol::Scope::Control,
                        ),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            match crate::flags_rpc::handle_flags_request(req.method.as_str(), &req.params) {
                Ok(value) => {
                    if turns_side_conversation_off(&req.params) {
                        // 放在请求路径之外：`close_all` 要等 runner 收敛，内联
                        // `await` 会堵住单 task 主循环（与 `side/open` 的纪律相同）。
                        //
                        // 放在宿主而不是前端：开关也可能经 CLI 拨（那时前端根本
                        // 不存在）。CLI 拨完正在跑的 TUI 不会热重载，但**这个
                        // 进程里的旁支会立刻停下**——那是「立即关闭」的含义。
                        let registry = Arc::clone(state.side_registry);
                        tokio::spawn(async move {
                            registry.close_all().await;
                        });
                    }
                    write_response(state.hub, state.client, ok_response(id, value)).await?
                }
                Err(error) => {
                    write_response(state.hub, state.client, err_response(id, error)).await?
                }
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}

/// 这次 `flags/set` 是不是「把旁问关掉」。
///
/// 独立成纯函数是为了可测：`flags/set` 的实际写盘在 `flags_rpc` 里受
/// build kind 硬闸约束，测试二进制永远走不到那条成功路径。
pub(crate) fn turns_side_conversation_off(params: &serde_json::Value) -> bool {
    params.get("name").and_then(|v| v.as_str()) == Some(crate::side_gate::SIDE_CONVERSATION_FLAG)
        && params.get("enabled").and_then(|v| v.as_bool()) == Some(false)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn turning_the_side_flag_off_is_recognized() {
        assert!(turns_side_conversation_off(&json!({
            "name": crate::side_gate::SIDE_CONVERSATION_FLAG,
            "enabled": false,
        })));
    }

    #[test]
    fn other_names_and_directions_are_not_mistaken_for_it() {
        // 打开不是清扫时机：那正是要允许用旁支的时候。
        assert!(!turns_side_conversation_off(&json!({
            "name": crate::side_gate::SIDE_CONVERSATION_FLAG,
            "enabled": true,
        })));
        // 关掉别的开关不该连坐。
        assert!(!turns_side_conversation_off(
            &json!({"name": "remote_ssh", "enabled": false})
        ));
        // 形状不对一律不算：宁可漏一次清扫（下次拨开关还会触发），
        // 也不要在参数畸形时误清。
        assert!(!turns_side_conversation_off(&json!({})));
        assert!(!turns_side_conversation_off(
            &json!({"name": "side_conversation"})
        ));
    }
}
