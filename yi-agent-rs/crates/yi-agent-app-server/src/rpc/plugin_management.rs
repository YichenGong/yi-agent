//! `plugins/v1/*`：受管插件的管理 RPC。
//!
//! # 与 `plugins/*` 的关系
//!
//! `plugin/*` 与 `plugins/*`（不带版本段）是**旧的** supervisor 协议：它按项目
//! 装清单、开关、起守护。那些方法保留原样，因为它们服务于既有的旧安装。
//!
//! `plugins/v1/*` 是新的**受管**协议：装、授权、激活、事务、卸载都作用在实例
//! 根的受管记录上，走签名验证与生命周期事务。两者**不共享**授权：旧的
//! `plugin/install` 不因为"装过"就在新协议里变成"已授权"。
//!
//! # 它与 CLI 是同一份实现
//!
//! 全部委托给 [`PluginService`]。这里只做三件事：从 `params` 取出参数、调用、
//! 把结果编成 JSON。任何"再判断一次"的逻辑都属于服务层，不属于这里——两份
//! 判断就是两条规则，迟早分叉。
//!
//! # 内存分配与阻塞
//!
//! 管理动作会读写磁盘（有时还会下载）。它们都在 `spawn_blocking` 里跑：
//! 一次安装不该卡住整个 RPC 主循环。

use serde_json::{Value, json};

use crate::plugin_manager::service::{self, PluginService};
use yi_agent_plugins::{PluginError, PluginId};

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{Flow, PluginChannelState, write_or_fatal};
use crate::server::{err_response, ok_response};

/// 方法前缀。旧协议的 `plugins/xxx` 不会被这里接住：`plugins/v1/...` 段数不同。
const PREFIX: &str = "plugins/v1/";

/// 本域是否处理这个方法。
pub(crate) fn handles(method: &str) -> bool {
    method.starts_with(PREFIX)
}

/// 处理一个 `plugins/v1/*` 请求。
pub(crate) async fn handle(
    state: &PluginChannelState,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    let Some(verb) = method.strip_prefix(PREFIX) else {
        return Ok(Flow::Unknown);
    };

    let home = state.watchman_home.clone();
    let params = req.params.clone();
    let verb = verb.to_string();

    // 管理动作是阻塞的（读盘、验签、有时下载）。放进 blocking 池，别让一次
    // 安装卡住整个 RPC 循环。`verb` 与 `params` 都先取成 owned：`req` 只在
    // 函数体内有效，搬不进 `'static` 的任务。
    let outcome = tokio::task::spawn_blocking(move || dispatch(&home, &verb, &params)).await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            write_or_fatal(
                state,
                id.clone(),
                err_response(
                    id,
                    RpcError::internal(format!("plugin management task failed: {error}")),
                ),
            )
            .await;
            return Ok(Flow::Continue);
        }
    };

    match outcome {
        Ok(value) => write_or_fatal(state, id.clone(), ok_response(id, value)).await,
        Err(error) => {
            write_or_fatal(state, id.clone(), err_response(id, error_response(&error))).await
        }
    }
    Ok(Flow::Continue)
}

/// 一个协议错误 → 线协议错误。
///
/// `kind` 是稳定判据（客户端按它分支），`message` 只是给人看的。绝不把
/// Rust 的 `Debug` 输出当线协议。
fn error_response(error: &PluginError) -> RpcError {
    RpcError::plugin_query_error(kind_code(error), error.to_string())
}

/// `PluginError::kind()` 是 PascalCase；RPC 沿用既有的 snake_case `code`。
fn kind_code(error: &PluginError) -> &'static str {
    match error {
        PluginError::InvalidManifest(_) => "plugin_invalid_manifest",
        PluginError::Integrity(_) => "plugin_integrity",
        PluginError::Untrusted(_) => "plugin_untrusted",
        PluginError::Expired(_) => "plugin_expired",
        PluginError::Revoked(_) => "plugin_revoked",
        PluginError::Incompatible(_) => "plugin_incompatible",
        PluginError::NotSupported(_) => "plugin_not_supported",
        PluginError::PermissionDenied(_) => "plugin_permission_denied",
        PluginError::StaleBinding(_) => "plugin_stale_binding",
        PluginError::Dependency(_) => "plugin_dependency",
        PluginError::Conflict(_) => "plugin_conflict",
        PluginError::Draining(_) => "plugin_draining",
        PluginError::Busy(_) => "plugin_busy",
        PluginError::ResultUnknown(_) => "plugin_result_unknown",
        PluginError::RecoveryRequired(_) => "plugin_recovery_required",
        PluginError::NotFound(_) => "plugin_not_found",
        PluginError::Io(_) => "plugin_io",
    }
}

/// 真正的分发。纯同步，便于在 blocking 池里跑，也便于单测。
fn dispatch(home: &std::path::Path, verb: &str, params: &Value) -> Result<Value, PluginError> {
    let service = PluginService::open(home);
    match verb {
        "catalog" => catalog(&service),
        "list" => list(&service),
        "install" => install(&service, params),
        "enableProject" => enable_project(&service, params),
        "changeGrant" => change_grant(&service, params),
        "disableProject" => disable_project(&service, params),
        "activate" => activate(&service, params),
        "rollback" => rollback(&service, params),
        "transaction" => transaction(&service, params),
        "cancelTransaction" => cancel_transaction(&service, params),
        "uninstall" => uninstall(&service, params),
        "clearData" => clear_data(&service, params),
        "gc" => gc(&service),
        // 下游任务的两种方法：如实说"还没有"，不伪装成功。
        "inspectLegacy" | "adoptLegacy" => Err(PluginError::NotSupported(
            "legacy inspection and adoption land with the migration task (T25)".to_string(),
        )),
        "assetRelease" => Err(PluginError::NotSupported(
            "serving release assets needs the desktop asset cache, which lands in T17".to_string(),
        )),
        "operationResult" => Err(PluginError::NotSupported(
            "operation results are served by the operation ledger path, which lands with the \
             managed kanban migration (T23)"
                .to_string(),
        )),
        other => Err(PluginError::NotSupported(format!(
            "`plugins/v1/{other}` is not a management method"
        ))),
    }
}

// ---------------------------------------------------------------------------
// 各方法
// ---------------------------------------------------------------------------

/// 目录内容。**只读**：不安装、不启用、不激活。
///
/// 目录条目不是信任凭据——它只说明"官方源上有这么个版本"。真正的授权仍来自
/// 每个包的签名。
fn catalog(service: &PluginService) -> Result<Value, PluginError> {
    let _ = service;
    // 目录的位置由宿主钉死（`YI_AGENT_PLUGIN_CATALOG` 或实例根下的默认位置），
    // 不接受请求传入路径：一个能指定"从哪读目录"的调用方，等于能指定
    // "哪些版本算官方"。
    let path = catalog_path();
    let bytes =
        std::fs::read(&path).map_err(|e| PluginError::Io(format!("{}: {e}", path.display())))?;
    let catalog: yi_agent_plugins::Catalog = serde_json::from_slice(&bytes).map_err(|e| {
        PluginError::InvalidManifest(format!("catalog at {} is not valid: {e}", path.display()))
    })?;
    let entries: Vec<Value> = catalog
        .entries
        .iter()
        .map(|entry| {
            json!({
                "plugin": entry.release.plugin.as_str(),
                "version": entry.release.version,
                "target": entry.release.target,
                "sha256": entry.release.sha256,
                "sizeBytes": entry.size_bytes,
                "publishedAt": entry.published_at,
                "expiresAt": entry.expires_at,
            })
        })
        .collect();
    Ok(json!({ "serial": catalog.serial, "entries": entries }))
}

fn list(service: &PluginService) -> Result<Value, PluginError> {
    let rows: Vec<Value> = service
        .list()?
        .into_iter()
        .map(|summary| {
            json!({
                "plugin": summary.plugin,
                "selected": summary.selected,
                "installed": summary.installed,
                "projects": summary.projects,
            })
        })
        .collect();
    Ok(json!({ "plugins": rows }))
}

/// 安装。**不**启用、**不**激活、**不**写 supervisor 清单。
///
/// 二选一：`{release: {plugin, version, target, sha256}}` 或
/// `{bundlePath, metadataPath}`。两条路都走同一个验签与落盘。
fn install(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let root = root_envelope()?;
    if let (Some(bundle), Some(metadata)) = (
        params.get("bundlePath").and_then(Value::as_str),
        params.get("metadataPath").and_then(Value::as_str),
    ) {
        let line = service.install_local(
            std::path::Path::new(bundle),
            std::path::Path::new(metadata),
            &root,
        )?;
        return Ok(json!({ "installed": true, "detail": line }));
    }

    if let Some(release) = params.get("release") {
        let spec = release
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                Some(format!(
                    "{}@{}#{}",
                    release.get("plugin")?.as_str()?,
                    release.get("version")?.as_str()?,
                    release.get("sha256")?.as_str()?
                ))
            })
            .ok_or_else(|| {
                PluginError::InvalidManifest(
                    "`release` must be `plugin@version#sha256` or an object with those fields"
                        .to_string(),
                )
            })?;
        return Err(PluginError::NotSupported(format!(
            "downloading {spec} needs an injected fetcher; use the headless `yi-agent plugins \
             install --release {spec}` path until the desktop download coordinator lands"
        )));
    }

    Err(PluginError::InvalidManifest(
        "install needs either `release` or both `bundlePath` and `metadataPath`".to_string(),
    ))
}

fn enable_project(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let host = resolve_host(service, params)?;
    let effects = capabilities_arg(params)?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.enable_project(&plugin, project.clone(), &host, &effects)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn change_grant(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let host = resolve_host(service, params)?;
    let effects = capabilities_arg(params)?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.change_grant(&plugin, &project, &host, &effects)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn disable_project(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let project_path = string_arg(params, "project")?;
    let project = service.resolve_project(&project_path)?;
    let generation = service.disable_project(&plugin, &project)?;
    Ok(json!({
        "plugin": plugin.as_str(),
        "project": project.as_str(),
        "generation": generation,
    }))
}

fn activate(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let version = string_arg(params, "version")?;
    let activation = service.activate(&plugin, &version)?;
    Ok(json!({
        "transaction": activation.transaction,
        "activation": activation.activation,
        // 恒为 false：推进需要运行期控制面，那是 daemon 的事。客户端据此知道
        // "事务已建立，但还没走完"，而不是以为激活完成。
        "advanced": activation.advanced,
    }))
}

fn rollback(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let view = service.transaction(&plugin)?;
    match view {
        Some(view) if !view.rollbackable => Err(PluginError::RecoveryRequired(format!(
            "transaction `{}` is at phase {}; rolling back after `projecting` would be a second \
             commit, so it must be repaired instead",
            view.transaction, view.phase
        ))),
        _ => Err(PluginError::NotSupported(
            "rolling back needs the runtime control plane (stop the candidate, restore \
             admission); the daemon drives it"
                .to_string(),
        )),
    }
}

fn transaction(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    match service.transaction(&plugin)? {
        Some(view) => Ok(json!({
            "plugin": plugin.as_str(),
            "transaction": view.transaction,
            "phase": view.phase,
            "admissionOpen": view.admission_open,
            "rollbackable": view.rollbackable,
        })),
        None => Ok(json!({
            "plugin": plugin.as_str(),
            "transaction": Value::Null,
        })),
    }
}

fn cancel_transaction(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let outcome = service.cancel_transaction(&plugin)?;
    // 前置判断通过了，但真正的回滚需要运行期控制面。如实说"可以取消、但
    // 尚未执行"，而不是假装已经回滚。
    Err(PluginError::NotSupported(format!(
        "transaction {} is at {} and can be cancelled, but undoing it needs the runtime control \
         plane; the daemon performs the rollback",
        outcome.transaction, outcome.phase
    )))
}

fn uninstall(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let plugin = plugin_arg(params)?;
    let remove_data = params
        .get("removeData")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let outcome = service.uninstall(&plugin, remove_data)?;
    Ok(json!({
        "plugin": outcome.plugin,
        "versionsRemoved": outcome.versions_removed,
        "grantsRevoked": outcome.grants_revoked,
        "retained": outcome
            .retained
            .iter()
            .map(|data| json!({ "project": data.project, "path": data.path, "why": data.why }))
            .collect::<Vec<_>>(),
    }))
}

/// 清除数据：独立于卸载的一次显式动作。
///
/// `confirm` 与 `projects` 两样缺一样就不动磁盘。"把所有数据都删了"不该是
/// 一个可以被顺手敲出来的默认行为。
fn clear_data(service: &PluginService, params: &Value) -> Result<Value, PluginError> {
    let confirmed = params
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !confirmed {
        return Err(PluginError::PermissionDenied(
            "clearing plugin data is irreversible; pass `confirm: true`".to_string(),
        ));
    }
    let projects: Vec<String> = params
        .get("projects")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if projects.is_empty() {
        return Err(PluginError::PermissionDenied(
            "name at least one project whose plugin data should be deleted".to_string(),
        ));
    }
    let plugin = plugin_arg(params)?;
    let mut outcome = service.uninstall(&plugin, true)?;
    let named: std::collections::BTreeSet<&str> = projects.iter().map(String::as_str).collect();
    outcome
        .retained
        .retain(|data| named.contains(data.project.as_str()));
    Ok(json!({ "plugin": outcome.plugin, "projects": projects }))
}

fn gc(service: &PluginService) -> Result<Value, PluginError> {
    let outcome = service.gc()?;
    Ok(json!({
        "removed": outcome.removed,
        "kept": outcome
            .kept
            .iter()
            .map(|kept| json!({ "path": kept.path, "why": kept.why }))
            .collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// 参数
// ---------------------------------------------------------------------------

fn plugin_arg(params: &Value) -> Result<PluginId, PluginError> {
    PluginId::parse(string_arg(params, "plugin")?.as_str())
}

fn string_arg(params: &Value, name: &str) -> Result<String, PluginError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| PluginError::InvalidManifest(format!("missing or empty `{name}` parameter")))
}

/// 宿主身份：请求可覆盖（多实例诊断用），缺省取实例级 id。
///
/// 允许请求覆盖是**有意的**：它不是授权凭据（授权记录里同时写着 project 与
/// 代次），而多个宿主共用一个实例根时需要能分辨。不覆盖时取 supervisor 的
/// 实例 id，那是同一份配置。
fn resolve_host(service: &PluginService, params: &Value) -> Result<String, PluginError> {
    match params.get("hostId").and_then(Value::as_str) {
        Some(host) if !host.is_empty() => Ok(host.to_string()),
        _ => service.host_id(),
    }
}

fn capabilities_arg(params: &Value) -> Result<Vec<yi_agent_plugins::Effect>, PluginError> {
    let raw = params
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    raw.into_iter().map(service::parse_capability).collect()
}

fn catalog_path() -> std::path::PathBuf {
    match std::env::var_os("YI_AGENT_PLUGIN_CATALOG") {
        Some(path) => std::path::PathBuf::from(path),
        None => yi_agent_store::paths::instance_home()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("plugins")
            .join("catalog.json"),
    }
}

fn root_envelope() -> Result<Vec<u8>, PluginError> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .join("plugins/contracts/fixtures/root-v1.json");
    std::fs::read(&path).map_err(|e| {
        PluginError::Untrusted(format!(
            "no trust root available at {}: {e}",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `plugins/v1/*` 与旧的 `plugins/*` 不共用名称空间。
    #[test]
    fn only_versioned_methods_are_handled_here() {
        assert!(handles("plugins/v1/list"));
        // 旧的 `plugins/list`（supervisor 协议）必须留给旧的处理函数。
        assert!(!handles("plugins/list"));
        assert!(!handles("plugin/install"));
    }

    /// 每个错误都有稳定的 snake_case code。客户端按它分支。
    #[test]
    fn every_error_kind_maps_to_a_stable_code() {
        let cases = [
            (PluginError::Integrity("x".into()), "plugin_integrity"),
            (PluginError::Untrusted("x".into()), "plugin_untrusted"),
            (PluginError::NotFound("x".into()), "plugin_not_found"),
            (
                PluginError::RecoveryRequired("x".into()),
                "plugin_recovery_required",
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(kind_code(&error), expected);
        }
    }

    /// 下游任务的方法如实返回 NotSupported，而不是伪装成功。
    #[test]
    fn downstream_methods_report_not_supported() {
        for verb in [
            "inspectLegacy",
            "adoptLegacy",
            "assetRelease",
            "operationResult",
        ] {
            let error =
                dispatch(std::path::Path::new("/nonexistent"), verb, &json!({})).expect_err(verb);
            assert!(
                matches!(error, PluginError::NotSupported(_)),
                "{verb} must be NotSupported, got {error:?}"
            );
        }
    }

    /// 清理数据要二次确认：没确认就什么都不做。
    #[test]
    fn clear_data_without_confirmation_refuses() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error =
            clear_data(&service, &json!({ "plugin": "example-counter" })).expect_err("must refuse");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }

    #[test]
    fn clear_data_without_named_projects_refuses() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error = clear_data(
            &service,
            &json!({ "plugin": "example-counter", "confirm": true }),
        )
        .expect_err("must refuse");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }

    /// 安装必须二选一，缺参数是明确错误而不是"什么都没装却说成功"。
    #[test]
    fn install_without_a_source_is_an_error() {
        let service = PluginService::open(std::path::Path::new("/nonexistent"));
        let error = install(&service, &json!({})).expect_err("must refuse");
        assert!(matches!(error, PluginError::InvalidManifest(_)));
    }

    #[test]
    fn a_missing_plugin_parameter_is_named_in_the_error() {
        let error = plugin_arg(&json!({})).expect_err("must refuse");
        assert!(error.to_string().contains("plugin"), "got {error}");
    }

    #[test]
    fn capabilities_are_parsed_and_maintenance_is_refused() {
        let effects = capabilities_arg(&json!({ "capabilities": ["read", "write"] }))
            .expect("read and write are user-facing");
        assert_eq!(
            effects,
            vec![
                yi_agent_plugins::Effect::Read,
                yi_agent_plugins::Effect::Write
            ]
        );
        let error = capabilities_arg(&json!({ "capabilities": ["maintenance"] }))
            .expect_err("maintenance is not user-grantable");
        assert!(matches!(error, PluginError::PermissionDenied(_)));
    }
}
