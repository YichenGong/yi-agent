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
            match plugin_query(&project, method, plugin, params, state.plugin_gate.as_ref()) {
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
            // **按请求里的名字解析**：宿主源码里不出现任何具体插件名，也不存在
            // 「启动时为某个插件预先解析好的路径」这种东西。
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
            if !manifest.is_object() {
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
            }
            // 光看「是不是对象」不够：supervisor 用 `SupervisorManifest::parse`
            // 加载清单，缺 `command` 之类会被它 warn 后**静默跳过**。那会得到一个
            // 「安装成功、开关开着、进程永不启动」的假成功——比直接报错更难查。
            // 这个校验是通用的：它只检查所有插件都要有的字段，不解释插件语义。
            let parsed =
                match yi_agent_supervisors::manifest::SupervisorManifest::parse(&manifest_json) {
                    Ok(manifest) => manifest,
                    Err(error) => {
                        write_or_fatal(
                            state,
                            id.clone(),
                            err_response(
                                id,
                                reject(format!("`{plugin} manifest` is unusable: {error}")),
                            ),
                        )
                        .await;
                        return Ok(Flow::Continue);
                    }
                };
            if parsed.name != plugin {
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        reject(format!(
                            "`{plugin} manifest` describes {:?}, not {plugin:?}",
                            parsed.name
                        )),
                    ),
                )
                .await;
                return Ok(Flow::Continue);
            }
            let switch_key = parsed.switch_key.as_str();
            // 就绪等待要把它 move 进 blocking 池，故先克隆一份；`switch_key` 仍
            // 借自 `parsed`，在安装阶段还要用。
            let ready_manifest = parsed.clone();

            // 先清单、后开关：清单让 supervisor 拉起进程，开关决定它要不要干活。
            // 两者缺一都会得到一个「注册了、起来了、却永不推进」的插件，
            // 故它们是一次安装的两半（spec §4.4.1）。
            let snapshot = match yi_agent_supervisors::install::install_manifest_for(
                &project,
                &plugin,
                &manifest_json,
            ) {
                Ok((_, snapshot)) => snapshot,
                Err(error) => {
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
            };
            if let Err(error) = yi_agent_supervisors::install::enable_switch(&project, switch_key) {
                // 开关打不开就恢复到安装前的样子。留下的清单会被 supervisor
                // 拉起，而插件因开关关着永不推进——正是 §4.4.1 要防的半安装态，
                // 且用户无从从界面上分辨「没装」和「装了但死的」。
                //
                // 是**恢复**而不是一律删掉：重装一个本来能用的插件时，开关这一步
                // 恰好写不进去，删清单就把失败的升级变成了卸载。
                if let Err(rollback) = snapshot.restore() {
                    tracing::warn!(
                        project = %project.display(),
                        plugin = %plugin,
                        %rollback,
                        "could not restore the previous manifest after the switch failed to enable"
                    );
                }
                write_or_fatal(
                    state,
                    id.clone(),
                    err_response(
                        id,
                        RpcError::internal(format!(
                            "could not enable {switch_key} for {plugin}: {error}"
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
            // 等插件自己的查询通道就绪。只等 daemon 的 `Status` 不够：转发表由
            // daemon 的监督循环每 500ms 登记一次，插件还要 bind socket；调用方
            // （桌面建看板）紧接着就发 `plugin/query`。
            //
            // 阻塞轮询放进 blocking 池。超时不算失败：清单与开关已落盘，常驻
            // 监督会继续重试；回包用 `queryable` 如实告诉调用方。
            let queryable = {
                let ready_project = project.clone();
                let manifest = ready_manifest;
                match tokio::task::spawn_blocking(move || {
                    crate::server::wait_for_plugin_query_socket(&ready_project, &manifest)
                })
                .await
                {
                    Ok(ready) => ready,
                    Err(error) => {
                        tracing::warn!(%error, "the plugin-readiness task failed");
                        true
                    }
                }
            };
            if !queryable {
                tracing::warn!(
                    project = %project.display(),
                    plugin = %plugin,
                    "installed the plugin but its query channel was not ready in time; the resident watch will retry"
                );
            }
            // 值守的登录项：`board/create` 时代由建板路径 best-effort 安装，那
            // 条路径随域删除后没有替代落点。安装一个需要常驻的插件正是它的通用
            // 对应物——开关为开、plist 未指向当前 exe 时装一次。
            //
            // 放在安装收尾：`launchctl` 是同步子进程，别让一次安装卡住 async worker。
            // 失败只 warn：清单与开关已经落盘，登录自启装不上不该把成功改成失败。
            {
                let workdir = state.theme_handle.workdir().to_path_buf();
                let home = state.watchman_home.clone();
                let install = std::sync::Arc::clone(&state.watchman_install);
                if let Err(error) = tokio::task::spawn_blocking(move || {
                    crate::server::ensure_watchman_installed(&workdir, &home, &install)
                })
                .await
                {
                    tracing::warn!(%error, "the watchman-install task failed");
                }
            }
            write_or_fatal(
                state,
                id.clone(),
                ok_response(
                    id,
                    json!({
                        "installed": true,
                        // 装完这一刻插件能不能被查询。调用方据此决定是立刻发
                        // `plugin/query`，还是先等监督补上（超时为 false，但安装
                        // 本身是成功的）。
                        "queryable": queryable,
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
        "plugin/uninstall" => {
            // `plugin/install` 的镜像：把一个插件从这个项目里摘干净。
            //
            // 顺序是有讲究的，且每一步都对应一个「不这么做就会留下什么」：
            //
            // 1. 先问插件自己 `lifecycle/remove`（它还活着，socket 通）。撤销
            //    常驻登记是插件对自己的承诺，宿主不知道它登记过什么名字；
            //    状态目录也归它自己删。失败不算致命——下面宿主自己再删一遍，
            //    但那时已经拿不到「登记是谁写的」这个信息，故尽量在这里做成。
            // 2. 删状态目录。第 1 步之后可能又被写进一拍（插件还没停）。
            // 3. 删清单 + 开关 + 日志。
            //
            // **为什么不停 daemon**：daemon 是项目级的，同一个项目里跑着用户的
            // 其它会话。为一个插件的卸载把它停掉，等于顺手掐断用户没让我碰的
            // 东西。清单删掉后 supervisor 下一拍就会 `stop(&name, "manifest
            // removed")`，只停这一个子进程——这正是我们要的那一半，而且不动
            // 别人。插件自身的 `lifecycle/remove` 也已放弃常驻登记，值守不会
            // 再把它拉起来。
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
            // 开关名只从**已安装的清单**里读，不接受调用方传：卸载一个不存在
            // 的开关名会把别人的键删掉。没有清单时开关名未知，就没什么可删的。
            let manifest_path = yi_agent_supervisors::install::manifests_dir(&project)
                .join(format!("{plugin}.json"));
            let switch_key = std::fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|text| {
                    yi_agent_supervisors::manifest::SupervisorManifest::parse(&text).ok()
                })
                .map(|manifest| manifest.switch_key);

            let mut warnings: Vec<String> = Vec::new();
            // 第 1 步：插件自己撤登记 + 删状态目录。best-effort：它没装、没跑、
            // 或干脆不认这个方法，都不该让卸载失败——宿主下面照样能清干净
            // 文件层的痕迹，而「进程还在跑」会由删清单后的监督一拍收掉。
            //
            // 先探 daemon 是否在应答：不在时**静默跳过**，不记警告。卸载一个
            // 本来就没在跑的插件是常态（比如重启后），那不是「插件拒绝配合」，
            // 把它写成警告只会让一次正常的卸载看起来有问题。
            //
            // 也**不走门禁**（`plugin_gate`）：门禁的语义是「这个功能关着，别
            // 用了」，而这里要做的恰恰是把关着的东西收拾干净。用一个被关掉的
            // 开关挡住卸载，会让用户永远清不掉它。
            let retire = {
                let project = project.clone();
                let plugin = plugin.clone();
                tokio::task::spawn_blocking(move || {
                    if !yi_agent_supervisors::daemon::is_running(&project) {
                        return Ok(());
                    }
                    plugin_query(
                        &project,
                        "lifecycle/remove",
                        &plugin,
                        json!({ "project": project.to_string_lossy() }),
                        None,
                    )
                    .map(|_| ())
                })
                .await
            };
            match retire {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warnings.push(format!(
                    "the plugin could not retire itself ({}); its state is being removed by the host",
                    error.message
                )),
                Err(error) => {
                    warnings.push(format!("the plugin-retire task failed: {error}"));
                }
            }
            // 第 2 步：再删一次状态目录。落点与 supervisor 的约定一致
            // （`<project>/.yi-agent/<manifest name>`），不写死任何插件名。
            let state_dir =
                yi_agent_supervisors::supervisor::Layout::for_workdir(&project).state_dir(&plugin);
            match std::fs::remove_dir_all(&state_dir) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => warnings.push(format!(
                    "could not remove the plugin state dir {}: {error}",
                    state_dir.display()
                )),
            }
            // 第 3 步：清单 + 开关 + 日志。删清单就是「让 supervisor 停掉它」
            // 的那一下，故放在最后：前面都成功之后，才允许它不再被拉起。
            if let Some(switch_key) = switch_key {
                warnings.extend(yi_agent_supervisors::install::uninstall_manifest_for(
                    &project,
                    &plugin,
                    &switch_key,
                ));
            } else if let Err(error) =
                yi_agent_supervisors::install::remove_manifest(&project, &plugin)
            {
                warnings.push(format!("could not remove the supervisor manifest: {error}"));
            }
            write_or_fatal(
                state,
                id.clone(),
                ok_response(
                    id,
                    json!({
                        // 「文件层已摘干净」——清单不在了，监督器不会再拉起它。
                        // 进程本身可能还要几百毫秒才退出（下一拍 reconcile），
                        // 故不谎称「已停」。
                        "uninstalled": true,
                        "warnings": warnings,
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
            let plugins: Vec<serde_json::Value> =
                list_installed_plugins(&scope, state.plugin_gate.as_ref())
                    .into_iter()
                    .map(|plugin| {
                        json!({
                            "name": plugin.name,
                            "queryable": plugin.queryable,
                            "switch_key": plugin.switch_key,
                            // 关着时宿主仍列出来（用户要能在设置页看见它、把它打开），
                            // 只是标成停用。
                            "enabled": plugin.enabled,
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
