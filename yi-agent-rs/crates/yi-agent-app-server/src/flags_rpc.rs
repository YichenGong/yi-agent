//! `flags/*` 与 `build/info` 的 RPC 接缝。
//!
//! 与 `model_rpc` 同类：机器级、不带 project 参数；路径由调用方注入以便测试。
//! **stable 构建里这些方法不存在**（`-32601`），而不是「存在但拒绝」——
//! 这样前端不需要任何权限判断，只需要看 `build/info`。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_flags::FlagError;

use crate::protocol::RpcError;

pub fn handle_flags_request(method: &str, params: &Value) -> Result<Value, RpcError> {
    let path = yi_agent_flags::store::flags_path()
        .ok_or_else(|| RpcError::internal("cannot resolve the state directory"))?;
    handle_flags_request_at(&path, yi_agent_flags::build_kind(), method, params)
}

pub fn handle_flags_request_at(
    path: &Path,
    build_kind: &str,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    // `build/info` 在任何构建里都存在：前端靠它判断自己是不是 dev。
    if method == "build/info" {
        return Ok(json!({
            "version": yi_agent_flags::version(),
            "build_kind": build_kind,
        }));
    }

    if build_kind != "dev" {
        // 方法在 stable 上**不存在**：前端根本不会渲染入口。
        return Err(RpcError::method_not_found(method));
    }

    match method {
        "flags/list" => Ok(json!({ "flags": views(path, build_kind) })),
        "flags/set" => {
            let name = params
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| RpcError::invalid_params("`name` is required"))?;
            let enabled = params
                .get("enabled")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| RpcError::invalid_params("`enabled` is required"))?;
            match yi_agent_flags::write_at(path, name, enabled) {
                Ok(()) => Ok(json!({ "name": name, "enabled": enabled })),
                Err(FlagError::UnknownFlag(name)) => Err(RpcError::invalid_params(format!(
                    "unknown flag `{name}`: it is not in the registry"
                ))),
                Err(FlagError::Io(e)) => Err(RpcError::internal(format!(
                    "could not write flags.json: {e}"
                ))),
            }
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn views(path: &Path, build_kind: &str) -> Vec<Value> {
    yi_agent_flags::all_at(path, build_kind)
        .into_iter()
        .map(|view| {
            json!({
                "name": view.name,
                "summary": view.summary,
                "enabled": view.enabled,
                "expires_on": view.expires_on,
                "source": view.source.as_str(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_returns_the_registry_with_sources() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let value = handle_flags_request_at(&path, "dev", "flags/list", &json!({})).unwrap();
        assert!(value.get("flags").unwrap().is_array());
    }

    #[test]
    fn set_refuses_an_unregistered_name_with_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = handle_flags_request_at(
            &path,
            "dev",
            "flags/set",
            &json!({"name": "bogus", "enabled": true}),
        )
        .unwrap_err();
        assert_eq!(
            err.code,
            crate::protocol::RpcError::invalid_params("x").code
        );
    }

    #[test]
    fn flags_are_method_not_found_in_a_release_build() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let err = handle_flags_request_at(&path, "release", "flags/set", &json!({})).unwrap_err();
        assert_eq!(
            err.code,
            crate::protocol::RpcError::method_not_found("x").code
        );
    }

    #[test]
    fn build_info_reports_the_kind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("flags.json");
        let value = handle_flags_request_at(&path, "dev", "build/info", &json!({})).unwrap();
        assert_eq!(value.get("build_kind").unwrap(), "dev");
        assert!(value.get("version").unwrap().is_string());
    }
}
