//! `flags/*` 与 `build/*` 域的 RPC 处理函数。

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
