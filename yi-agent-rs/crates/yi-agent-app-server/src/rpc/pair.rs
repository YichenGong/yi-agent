//! `pair/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 2)。配对码铸造/兑换与设备注册表——plan 把 `pair/*` 与 `device/*` 归入同一个模块(两者共享同一 `Arc<PairingState>`)。
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue`
//! 换成 `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,
//! 与搬移前逐字一致。

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, forget_ws_device, ok_response, write_response, ws_client_for_device,
};

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂
/// 里的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "pair/create" => {
            // 铸一枚一次性配对码,交桌面端渲染二维码。设备注册属于
            // `pair/redeem`(见下),这里只铸码。
            let code = state.pairing.create_code();
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({ "code": code.code, "expires_in": code.expires_in }),
                ),
            )
            .await?;
        }
        "pair/redeem" => {
            // 帧级兑换:一次性配对码换设备 token。与 ws 升级时的
            // `?pair=<code>` 等价,但走**帧**——这是**经中继**配对的唯一
            // 通路:中继只转发 ws 帧、不改写升级查询串,所以手机的
            // `?pair=` 到不了本机 app-server;而手机连上中继后发的这条
            // RPC 会被中继桥原样转发过来,本机据此铸设备。
            //
            // **不在 `ADMIN_METHODS` 内**:调用者(含中继桥)无需 admin——
            // 凭证是那枚一次性码本身(只有桌面能铸),无码即无效。
            let Some(code) = req
                .params
                .get("code")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing code")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let name = req
                .params
                .get("device_name")
                .and_then(|v| v.as_str())
                .unwrap_or("remote device")
                .to_string();
            match state.pairing.redeem(&code, &name) {
                Ok((device, token)) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "device_id": device.id,
                                "token": token,
                                "scope": device.scope,
                            }),
                        ),
                    )
                    .await?;
                }
                Err(_) => {
                    // 码不存在/已用/已过期:与"无 token"一致的说法,
                    // 不泄露码是否存在。
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_pairing_code()),
                    )
                    .await?;
                }
            }
        }
        "device/list" => {
            let devices: Vec<serde_json::Value> = state
                .pairing
                .store()
                .list()
                .into_iter()
                .map(|d| {
                    json!({
                        "id": d.id,
                        "name": d.name,
                        "scope": d.scope,
                        "created_at": d.created_at,
                        "last_seen_at": d.last_seen_at,
                    })
                })
                .collect();
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "devices": devices })),
            )
            .await?;
        }
        "device/revoke" => {
            let Some(device_id) = req
                .params
                .get("device_id")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("missing device_id")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let revoked = state.pairing.revoke(&device_id).unwrap_or(false);
            if revoked {
                // 先取当前连接:撤销即掉线由「`hub.unregister` + 读循环
                // 的 disconnected tick」共同完成。用 `get` + `forget`
                // 两步是为了兼顾两种语义——`ws_client_for_device` 仍是
                // 「此刻该设备连在哪」的权威读法(测试与将来 RPC 复用),
                // `forget_ws_device` 顺带把该映射从全局表摘掉,不留陈旧项。
                let cid = ws_client_for_device(&device_id);
                forget_ws_device(&device_id);
                if let Some(cid) = cid {
                    state.hub.unregister(&cid);
                }
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "revoked": revoked })),
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
