//! `plugin/*` 与 `plugins/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 1)。臂体保持
//! 原样,只有三处改名:主循环局部量改走 [`AppState`] 字段(`hub` -> `state.hub`
//! 等),臂内的 `method.as_str()` 改成 `req.method.as_str()`(本模块的形参不再
//! 叫 `method`),以及 `continue` 换成 `return Ok(Flow::Continue)`——后者的语义由
//! `serve()` 翻回 `continue`,与搬移前逐字一致。
//!
//! Task 4 起本域**脱离主循环**执行:`serve()` 把处理搬进 `tokio::spawn`,故状态改为
//! [`crate::rpc::PluginChannelState`](全是 owned/`Arc`,可 move),应答改写
//! [`crate::rpc::write_or_fatal`]——写失败不再能靠 `?` 传播,改由 fatal 通道上报。

use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{Flow, PluginChannelState, write_or_fatal};
use crate::server::{
    err_response, list_installed_plugins, ok_response, plugin_query, plugin_query_self,
    plugin_scope, project_arg,
};

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂
/// 里的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle_shared(
    state: &PluginChannelState,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "plugin/query" => {
            // `project` names the project being queried, not the server's cwd:
            // the whole point of the RPC is to reach a plugin that lives under
            // another project's daemon.
            let Some(project) = project_arg(&req.params) else {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(id, RpcError::invalid_params("missing or empty project")),
                )
                .await;
                return Ok(Flow::Continue);
            };
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let method = req
                .params
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let params = req
                .params
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            match plugin_query(&project, method, plugin, params) {
                Ok(value) => write_or_fatal(state, id.clone(), ok_response(id, value)).await,
                Err(error) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(id, RpcError::plugin_query_error(error.code, error.message)),
                    )
                    .await
                }
            }
        }
        "plugin/install" => {
            // 把一个插件装进一个项目：让它自述清单，宿主落盘并打开它的开关。
            //
            // 为什么必须由宿主做：插件要能被调起来，前提是清单已在
            // `<项目>/.yi-agent/supervisors/<name>.json`；而「被调起来」正是它写
            // 清单的前提。全新项目没有清单，这个循环只能由宿主从外面打破。
            //
            // 顺序是「先校验、后落盘」：任何一步失败都**什么都没写**，用户重试
            // 不会留下半个安装。
            let Some(project) = project_arg(&req.params) else {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(id, RpcError::invalid_params("missing or empty project")),
                )
                .await;
                return Ok(Flow::Continue);
            };
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !yi_agent_supervisors::install::is_valid_plugin_name(&plugin) {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        RpcError::invalid_params("missing or unusable plugin name"),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }
            // **按请求里的名字解析**，不读 `state.plugin_command`：那个字段是启动
            // 时按一个具体插件名解析好的单一路径，用它等于把插件名写死进宿主。
            let Some(exe) = (state.resolve_plugin)(&plugin) else {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        RpcError::plugin_query_error(
                            "plugin_not_installed",
                            format!("{plugin} is not installed: not found on PATH"),
                        ),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            };
            // `<plugin> manifest` 是阻塞子进程调用；放进 blocking 池，别让一次
            // 安装卡住一个 async worker。
            let described = {
                let exe = exe.clone();
                tokio::task::spawn_blocking(move || {
                    std::process::Command::new(&exe).arg("manifest").output()
                })
                .await
            };
            let output = match described {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(
                            id,
                            RpcError::plugin_query_error(
                                "plugin_rejected",
                                format!("could not run `{plugin} manifest`: {error}"),
                            ),
                        ),
                    )
                    .await;
                    return Ok(Flow::Continue);
                }
                Err(error) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(
                            id,
                            RpcError::internal(format!("the manifest task failed: {error}")),
                        ),
                    )
                    .await;
                    return Ok(Flow::Continue);
                }
            };
            let reject = |message: String| RpcError::plugin_query_error("plugin_rejected", message);
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let first = stderr.lines().next().unwrap_or("").trim();
                let detail = if first.is_empty() {
                    format!("`{plugin} manifest` exited with {}", output.status)
                } else {
                    format!("`{plugin} manifest` failed: {first}")
                };
                write_or_fatal(state, id.clone(), err_response(id, reject(detail))).await;
                return Ok(Flow::Continue);
            }
            // 清单原样写盘（逐字保留插件给的格式），故这里只做「能不能安全使用」
            // 的校验，不解释插件的语义。
            let manifest_json = match String::from_utf8(output.stdout) {
                Ok(text) => text,
                Err(_) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(
                            id,
                            reject(format!("`{plugin} manifest` did not print UTF-8")),
                        ),
                    )
                    .await;
                    return Ok(Flow::Continue);
                }
            };
            let manifest: serde_json::Value = match serde_json::from_str(&manifest_json) {
                Ok(value) => value,
                Err(error) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(
                            id,
                            reject(format!("`{plugin} manifest` did not print JSON: {error}")),
                        ),
                    )
                    .await;
                    return Ok(Flow::Continue);
                }
            };
            let Some(object) = manifest.as_object() else {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        reject(format!("`{plugin} manifest` is not a JSON object")),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            };
            let described_name = object.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if described_name != plugin {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        reject(format!(
                            "`{plugin} manifest` describes {described_name:?}, not {plugin:?}"
                        )),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }
            let switch_key = object
                .get("switch_key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if switch_key.is_empty() {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        reject(format!("`{plugin} manifest` declares no switch_key")),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }

            // 先清单、后开关：清单让 supervisor 拉起进程，开关决定它要不要干活。
            // 两者缺一都会得到一个「注册了、起来了、却永不推进」的插件，
            // 故它们是一次安装的两半（spec §4.4.1）。
            if let Err(error) = yi_agent_supervisors::install::install_manifest_for(
                &project,
                &plugin,
                &manifest_json,
            ) {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        RpcError::internal(format!(
                            "could not install the {plugin} manifest: {error}"
                        )),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }
            if let Err(error) = yi_agent_supervisors::install::enable_switch(&project, switch_key) {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        RpcError::internal(format!(
                            "installed the {plugin} manifest but could not enable {switch_key}: {error}"
                        )),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }
            // 拉起 daemon 是 best-effort：清单与开关已经落盘，常驻监视线程每
            // ~30s 会再扫一遍。这里失败只是「这次没立刻起来」，不该让一次已经
            // 成功的安装回滚成失败。
            let ensure = std::sync::Arc::clone(&state.ensure_daemon);
            let daemon_project = project.clone();
            match tokio::task::spawn_blocking(move || ensure(&daemon_project)).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => tracing::warn!(
                    project = %project.display(),
                    plugin = %plugin,
                    "installed the plugin but its daemon did not become ready; the resident watch will retry"
                ),
                Ok(Err(error)) => tracing::warn!(
                    project = %project.display(),
                    plugin = %plugin,
                    "installed the plugin but could not start its daemon: {error}"
                ),
                Err(error) => tracing::warn!(
                    project = %project.display(),
                    plugin = %plugin,
                    "the daemon-start task failed: {error}"
                ),
            }
            write_or_fatal(
                state,
                id.clone(),
                ok_response(
                    id,
                    json!({
                        "installed": true,
                        // `PathBuf` 的 `Serialize` 在非 UTF-8 路径上会直接报错；
                        // 回包里的路径是给客户端看的，lossy 比整条响应失败好。
                        "manifest": yi_agent_supervisors::install::manifests_dir(&project)
                            .join(format!("{plugin}.json"))
                            .to_string_lossy(),
                    }),
                ),
            )
            .await;
        }
        "plugins/list" => {
            let workdir = state.theme_handle.workdir();
            // 清单按项目安装，故作用域可由调用方的 `project` 指定；
            // 缺省回落到宿主 workdir（TUI 与旧客户端）。
            let scope = plugin_scope(workdir, &req.params);
            let plugins: Vec<serde_json::Value> = list_installed_plugins(&scope)
                .into_iter()
                .map(|plugin| {
                    json!({
                        "name": plugin.name,
                        "queryable": plugin.queryable,
                        "switch_key": plugin.switch_key,
                    })
                })
                .collect();
            write_or_fatal(
                state,
                id.clone(),
                ok_response(id, json!({ "plugins": plugins })),
            )
            .await;
        }
        "plugin/settings/read" => {
            let workdir = state.theme_handle.workdir().to_path_buf();
            let scope = plugin_scope(&workdir, &req.params);
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match plugin_query_self(&scope, &plugin, "settings.read", json!({})) {
                Ok(value) => write_or_fatal(state, id.clone(), ok_response(id, value)).await,
                Err(error) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(id, RpcError::plugin_query_error(error.code, error.message)),
                    )
                    .await
                }
            }
        }
        "plugin/settings/write" => {
            let workdir = state.theme_handle.workdir().to_path_buf();
            let scope = plugin_scope(&workdir, &req.params);
            let plugin = req
                .params
                .get("plugin")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let settings = req.params.get("settings").cloned().unwrap_or(json!({}));
            match plugin_query_self(
                &scope,
                &plugin,
                "settings.write",
                json!({ "settings": settings }),
            ) {
                Ok(value) => write_or_fatal(state, id.clone(), ok_response(id, value)).await,
                Err(error) => {
                    write_or_fatal(
                        state,
                        id.clone(),
                        err_response(id, RpcError::plugin_query_error(error.code, error.message)),
                    )
                    .await
                }
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
