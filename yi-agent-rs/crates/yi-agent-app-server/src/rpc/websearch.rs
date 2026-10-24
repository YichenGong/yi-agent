//! `websearch/*` 域分派。权限档位：`get` 只读盘且回掩码（`Observe`）；
//! `set` / `test` 触及密钥、会落盘或发网络请求（`Control`），与 `model/*` 同档。

use crate::protocol::{RequestEnvelope, RequestId, RpcError, Scope};
use crate::rpc::{AppState, Flow};
use crate::server::{err_response, ok_response, write_response};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "websearch/get" => {}
        "websearch/set" | "websearch/test" => {
            if state.client_scope < Scope::Control {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::insufficient_scope(Scope::Control)),
                )
                .await?;
                return Ok(Flow::Continue);
            }
        }
        _ => return Ok(Flow::Unknown),
    }
    match crate::websearch_rpc::handle_websearch_request(
        state.websearch_path,
        req.method.as_str(),
        &req.params,
    )
    .await
    {
        Ok(value) => write_response(state.hub, state.client, ok_response(id, value)).await?,
        Err(error) => write_response(state.hub, state.client, err_response(id, error)).await?,
    }
    Ok(Flow::Continue)
}
