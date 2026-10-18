//! `ui/*` 与 `config/read` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 1)。臂体保持
//! 原样,只有三处改名:主循环局部量改走 [`AppState`] 字段(`hub` -> `state.hub`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再
//! 叫 `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。

use serde_json::json;

use crate::protocol::{Notification, RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    TAG_NAME_MAX_LEN, err_response, ok_response, write_notification, write_response,
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
        "config/read" => {
            write_response(
                state.hub,
                state.client,
                ok_response(id, state.cfg.redacted_view()),
            )
            .await?;
        }
        "ui/settings/read" => {
            // 以共享句柄的当前值为权威（与 `set_theme` 工具同一实例）;
            // 句柄在构造时已从磁盘载入，故首次读即反映持久化值。
            // 值守开关是宿主级偏好，与 theme 同处一个文件，目录取
            // 句柄的 workdir 以保证两者读写的是同一个 preferences.json。
            let theme = state.theme_handle.current();
            let workdir = state.theme_handle.workdir();
            let watchman_enabled = crate::settings_store::load_watchman_enabled(workdir);
            // 侧车中继地址（桌面设置页写入）：与 theme/值守同处一个
            // preferences.json，故一并从同一个 workdir 读。未配置即
            // `null`，桌面首屏据此回填空串。
            let relay_url = crate::settings_store::load_relay_url(workdir);
            // 标签显示名：与 theme/值守/中继同处一个 preferences.json，
            // 故同样从 workdir 读。只回**用户覆盖项**，默认名由前端持有。
            let tag_names = crate::settings_store::load_tag_names(workdir);
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "theme": theme.as_str(),
                        "board_watchman_enabled": watchman_enabled,
                        "relay_url": relay_url,
                        "tag_names": tag_names,
                    }),
                ),
            )
            .await?;
        }
        "ui/settings/write" => {
            if let Some(enabled) = req
                .params
                .get("board_watchman_enabled")
                .and_then(|v| v.as_bool())
            {
                let workdir = state.theme_handle.workdir();
                // 落盘失败就整体失败：偏好没写成功却去动系统状态，
                // 会让开关与磁盘记录不一致。
                if let Err(error) = crate::settings_store::save_watchman_enabled(workdir, enabled) {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(error.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
                // 落盘成功但装/卸失败不能静默：开关状态与系统实际
                // 状态已经不一致，前端需要拿到原因做内联提示。
                let outcome = if enabled {
                    match std::env::current_exe() {
                        Ok(exe) => (state.watchman_install)(&exe, state.watchman_home),
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    (state.watchman_uninstall)(state.watchman_home)
                };
                let warning = outcome.err();
                if let Some(warning) = &warning {
                    tracing::warn!(%warning, enabled, "board watchman switch did not take effect");
                }
                write_response(
                    state.hub,
                    state.client,
                    ok_response(id, json!({ "ok": true, "warning": warning })),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // theme 只在请求里显式带上时才校验/应用：只写 relay_url
            // 的请求不该因为「没有 theme」而被拒绝。先校验再落盘，
            // 免得非法 theme 把一个有效的 relay_url 写了一半。
            let requested_theme = if req.params.get("theme").is_some() {
                let requested = req
                    .params
                    .get("theme")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match requested.trim().to_ascii_lowercase().as_str() {
                    "dark" => Some(crate::settings_store::Theme::Dark),
                    "light" => Some(crate::settings_store::Theme::Light),
                    other => {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(
                                id,
                                RpcError::invalid_params(format!(
                                    "unsupported theme '{other}': expected 'dark' or 'light'"
                                )),
                            ),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                }
            } else {
                None
            };

            // 标签显示名：键必须是 6 色之一，值 trim 后非空且不超 24 字符。
            // 校验先于落盘——非法名字不该把一次有效的 relay_url 写了一半。
            let tag_updates: Option<std::collections::BTreeMap<String, String>> =
                match req.params.get("tag_names") {
                    None => None,
                    Some(value) => {
                        let Some(object) = value.as_object() else {
                            write_response(
                                state.hub,
                                state.client,
                                err_response(
                                    id,
                                    RpcError::invalid_params(
                                        "tag_names must be an object of color id -> name",
                                    ),
                                ),
                            )
                            .await?;
                            return Ok(Flow::Continue);
                        };
                        let mut updates = std::collections::BTreeMap::new();
                        let mut error: Option<String> = None;
                        for (key, value) in object {
                            if !crate::thread_store::is_valid_tag(key) {
                                error = Some(format!("unknown tag id '{key}'"));
                                break;
                            }
                            let Some(name) = value.as_str() else {
                                error = Some(format!("tag name for '{key}' must be a string"));
                                break;
                            };
                            let name = name.trim();
                            if name.is_empty() {
                                error = Some(format!("tag name for '{key}' must not be blank"));
                                break;
                            }
                            if name.chars().count() > TAG_NAME_MAX_LEN {
                                error = Some(format!(
                                    "tag name for '{key}' exceeds {TAG_NAME_MAX_LEN} characters"
                                ));
                                break;
                            }
                            updates.insert(key.clone(), name.to_string());
                        }
                        match error {
                            Some(message) => {
                                write_response(
                                    state.hub,
                                    state.client,
                                    err_response(id, RpcError::invalid_params(message)),
                                )
                                .await?;
                                return Ok(Flow::Continue);
                            }
                            None => Some(updates),
                        }
                    }
                };
            if let Some(updates) = &tag_updates {
                let workdir = state.theme_handle.workdir();
                if let Err(error) = crate::settings_store::save_tag_names(workdir, updates) {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::internal(error.to_string())),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            }

            // `relay_url`（桌面设置页）：与 theme 各写各的键，共处一个
            // 文件，故两条路径都走 settings_store 的读-改-写。
            // `null` / 空 / 空白 → 清除该键。非串非 null（客户端写错
            // 类型）不改动既有配置——静默清除是破坏性的。
            if let Some(value) = req.params.get("relay_url") {
                if value.is_null() || value.is_string() {
                    let workdir = state.theme_handle.workdir();
                    if let Err(error) =
                        crate::settings_store::save_relay_url(workdir, value.as_str())
                    {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(id, RpcError::internal(error.to_string())),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                }
            }

            if let Some(theme) = requested_theme {
                state.theme_handle.set(theme);
            }
            write_response(
                state.hub,
                state.client,
                ok_response(id, json!({ "ok": true })),
            )
            .await?;
            // 名字落盘成功后广播权威覆盖映射（全局帧），让其它设备
            // 立即刷新菜单与搜索。放在最后：主题/中继的写入都已生效，
            // 且响应已先入队——读响应的一方必先见到 id，再见到通知。
            if tag_updates.is_some() {
                let names = crate::settings_store::load_tag_names(state.theme_handle.workdir());
                let _ = write_notification(
                    state.hub,
                    &Notification::TagNamesUpdated { tag_names: names },
                )
                .await;
            }
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
