//! `model/*` RPC：机器级模型清单（`~/.yi-agent/models.json`）的读写接缝。
//!
//! 与 `ui/settings/*` 同类：**机器级全局、不带 project 参数**。清单本体与
//! 落盘约定在 `yi_agent_runtime::models`，本模块只负责把 JSON-RPC 的
//! `method` + `params` 翻译成对清单的读/校验/改，再交回 JSON 值或 [`RpcError`]。
//!
//! 纯函数、无 I/O 之外的状态：清单路径经 [`handle_model_request_at`] 注入，
//! 生产入口 [`handle_model_request`] 用 `models_path()`（`$HOME/.yi-agent/`）。
//! 之所以不让测试改 `HOME`：cargo 并行跑测试，进程全局变量会让用例互相竞态。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_runtime::config::RuntimeConfig;
use yi_agent_runtime::models::{
    ModelCatalog, ModelEntry, ModelProvider, effective_entry, load_catalog_from, mask_key,
    models_path, save_catalog_to,
};

use crate::protocol::RpcError;

/// 生产入口：读写 `~/.yi-agent/models.json`。
pub fn handle_model_request(
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    handle_model_request_at(&models_path(), method, params, fallback)
}

/// 可测核心：清单文件路径与兜底 cfg 均由调用方注入。
pub fn handle_model_request_at(
    path: &Path,
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    match method {
        "model/list" => {
            let catalog = load_catalog_from(path);
            Ok(json!({
                "models": catalog.models.iter().map(entry_view).collect::<Vec<_>>(),
                "default_model": catalog.default_model,
                "subagent_model": catalog.subagent_model,
                "effective": effective_view(&catalog, fallback),
            }))
        }
        "model/upsert" => upsert(path, params),
        "model/delete" => delete(path, params),
        "model/setDefault" => set_reference(path, params, Reference::Default),
        "model/setSubagent" => set_reference(path, params, Reference::Subagent),
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// 当前默认实际解析到哪里：命中清单条目就用条目，否则回退 cfg（`.env`）。
/// key 一律只出掩码——与 [`entry_view`] 同一条安全约定。
fn effective_view(catalog: &ModelCatalog, cfg: &RuntimeConfig) -> Value {
    match effective_entry(catalog, None) {
        Some(entry) => json!({
            "source": "catalog",
            "model_ref": entry.name,
            "provider": entry.provider.as_str(),
            "api_url": entry.api_url,
            "model": entry.model,
            "has_key": !entry.api_key.is_empty(),
            "api_key_masked": mask_key(&entry.api_key),
        }),
        None => json!({
            "source": "env",
            "model_ref": Value::Null,
            "provider": cfg.provider,
            "api_url": cfg.api_url,
            "model": cfg.model,
            "has_key": !cfg.api_key.is_empty(),
            "api_key_masked": mask_key(&cfg.api_key),
        }),
    }
}

/// One entry as the wire sees it: masked key + a `has_key` flag, **never** the
/// raw `api_key`.
fn entry_view(entry: &ModelEntry) -> Value {
    json!({
        "name": entry.name,
        "provider": entry.provider.as_str(),
        "api_url": entry.api_url,
        "model": entry.model,
        "has_key": !entry.api_key.is_empty(),
        "api_key_masked": mask_key(&entry.api_key),
    })
}

/// `model/upsert`：全量校验 → 改内存 → 落盘。校验失败直接 `Err`，
/// 一个字节都不写（调用方在 `save_catalog_to` 之前就返回）。
fn upsert(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let name = string_field(params, "name").unwrap_or_default();
    if name.trim().is_empty() {
        return Err(RpcError::invalid_model("model name must not be empty"));
    }
    let provider_raw = string_field(params, "provider").unwrap_or_default();
    let Some(provider) = ModelProvider::parse(&provider_raw) else {
        return Err(RpcError::invalid_model(format!(
            "unknown provider: {provider_raw}"
        )));
    };
    let api_url = string_field(params, "api_url").unwrap_or_default();
    if api_url.trim().is_empty() {
        return Err(RpcError::invalid_model("api_url must not be empty"));
    }
    let model = string_field(params, "model").unwrap_or_default();
    if model.trim().is_empty() {
        return Err(RpcError::invalid_model("model must not be empty"));
    }

    let mut catalog = load_catalog_from(path);
    // Key semantics: absent (or `null`) keeps the stored value; `""` clears it;
    // any other string overwrites. A brand-new entry with no key is keyless.
    let existing = catalog.models.iter().position(|m| m.name == name);
    let api_key = match key_field(params)? {
        KeyInput::Keep => existing
            .map(|i| catalog.models[i].api_key.clone())
            .unwrap_or_default(),
        KeyInput::Clear => String::new(),
        KeyInput::Set(key) => key,
    };

    let entry = ModelEntry {
        name,
        provider,
        api_url,
        model,
        api_key,
    };
    match existing {
        Some(i) => catalog.models[i] = entry,
        None => catalog.models.push(entry),
    }
    save_catalog_to(path, &catalog).map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// `model/delete`：命中才删；同时把指向它的 `default_model`/`subagent_model`
/// 一并清掉，否则会留下悬空引用（解析时静默回退，用户看不见）。
fn delete(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let name = string_field(params, "name").unwrap_or_default();
    let mut catalog = load_catalog_from(path);
    if !catalog.models.iter().any(|m| m.name == name) {
        return Err(RpcError::model_not_found(&name));
    }
    catalog.models.retain(|m| m.name != name);
    if catalog.default_model.as_deref() == Some(name.as_str()) {
        catalog.default_model = None;
    }
    if catalog.subagent_model.as_deref() == Some(name.as_str()) {
        catalog.subagent_model = None;
    }
    save_catalog_to(path, &catalog).map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({ "ok": true }))
}

#[derive(Clone, Copy)]
enum Reference {
    Default,
    Subagent,
}

/// `model/setDefault` / `model/setSubagent`：`null` 或缺失 → 清除；否则必须
/// 命中已存在条目，否则 `model_not_found`（且零落盘）。
fn set_reference(path: &Path, params: &Value, which: Reference) -> Result<Value, RpcError> {
    let mut catalog = load_catalog_from(path);
    let target = match params.get("name") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => {
            if !catalog.models.iter().any(|m| &m.name == name) {
                return Err(RpcError::model_not_found(name));
            }
            Some(name.clone())
        }
        Some(_) => return Err(RpcError::invalid_model("name must be a string or null")),
    };
    match which {
        Reference::Default => catalog.default_model = target,
        Reference::Subagent => catalog.subagent_model = target,
    }
    save_catalog_to(path, &catalog).map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// `api_key` 入参的三种形态；见 [`upsert`] 的语义注释。
enum KeyInput {
    Keep,
    Clear,
    Set(String),
}

fn key_field(params: &Value) -> Result<KeyInput, RpcError> {
    Ok(match params.get("api_key") {
        // 缺省或显式 `null` 都按「保留」处理：宁可沿用旧 key，也不静默抹掉。
        None | Some(Value::Null) => KeyInput::Keep,
        Some(Value::String(key)) if key.is_empty() => KeyInput::Clear,
        Some(Value::String(key)) => KeyInput::Set(key.clone()),
        // 非字符串按非法输入拒绝，避免把一个数组/对象悄悄当成 key。
        Some(_) => return Err(RpcError::invalid_model("api_key must be a string")),
    })
}

fn string_field(params: &Value, field: &str) -> Option<String> {
    params
        .get(field)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use yi_agent_runtime::config::RuntimeConfig;
    use yi_agent_runtime::models::ModelCatalog;

    /// 每条用例独占一份清单文件。路径注入而非改 `HOME`：并行测试下 `HOME` 是
    /// 进程全局量，改了会互相踩。
    fn catalog_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".yi-agent").join("models.json");
        (dir, path)
    }

    /// 兜底层 cfg：全部字段可辨，便于断言「生效值来自 cfg」。
    ///
    /// `RuntimeConfig` **未实现** `Default`，必须显式构造全部字段（字段表见
    /// `yi-agent-runtime/src/config.rs:17-40`）。用 `#[cfg(test)]` 的
    /// `yi_agent_runtime::config::sample_config()` 不可行——它是 runtime crate 的
    /// `pub(crate)`，跨 crate 用不了。
    fn fallback_cfg() -> RuntimeConfig {
        RuntimeConfig {
            provider: "openai".to_string(),
            api_url: "https://env.example".to_string(),
            api_key: "env-secret-9999".to_string(),
            model: "env-model".to_string(),
            max_turns: 20,
            max_resident_subagents: 8,
            workdir: std::path::PathBuf::from("/tmp/import-env-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
        }
    }

    fn call(path: &Path, method: &str, params: Value) -> Result<Value, RpcError> {
        handle_model_request_at(path, method, &params, &fallback_cfg())
    }

    /// 常用 upsert 载荷，省略 `api_key`（= 保留）。
    fn entry_json(name: &str) -> Value {
        json!({
            "name": name,
            "provider": "anthropic",
            "api_url": "https://a",
            "model": "m",
            "api_key": "sk-secret-1234",
        })
    }

    #[test]
    fn list_never_returns_the_raw_key() {
        let (_dir, path) = catalog_path();
        let out = call(&path, "model/upsert", entry_json("A")).unwrap();
        assert_eq!(out["ok"], true);
        let out = call(&path, "model/list", json!({})).unwrap();
        let text = out.to_string();
        assert!(
            !text.contains("sk-secret-1234"),
            "the raw key must never be returned: {text}"
        );
        assert_eq!(out["models"][0]["has_key"], true);
        assert_eq!(out["models"][0]["api_key_masked"], "••••1234");
        assert!(out["models"][0].get("api_key").is_none());
    }

    #[test]
    fn upsert_without_api_key_keeps_the_stored_one() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "openai",
                "api_url": "https://b", "model": "m2"
            }),
        )
        .unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"][0]["api_key_masked"], "••••1234");
        assert_eq!(list["models"][0]["provider"], "openai");
        assert_eq!(list["models"][0]["model"], "m2");
    }

    #[test]
    fn an_empty_key_clears_it() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "anthropic",
                "api_url": "https://a", "model": "m", "api_key": ""
            }),
        )
        .unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"][0]["has_key"], false);
    }

    #[test]
    fn invalid_input_writes_nothing() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        let err = call(
            &path,
            "model/upsert",
            json!({
                "name": "B", "provider": "gemini", "api_url": "u", "model": "m"
            }),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
        assert_eq!(list["models"][0]["name"], "A");
    }

    #[test]
    fn an_empty_name_is_rejected_without_writing() {
        let (_dir, path) = catalog_path();
        let err = call(
            &path,
            "model/upsert",
            json!({ "name": "  ", "provider": "openai", "api_url": "u", "model": "m" }),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["models"].as_array().unwrap().is_empty());
    }

    #[test]
    fn a_non_string_api_key_is_rejected_without_writing() {
        let (_dir, path) = catalog_path();
        let err = call(
            &path,
            "model/upsert",
            json!({ "name": "A", "provider": "openai", "api_url": "u", "model": "m", "api_key": 42 }),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["models"].as_array().unwrap().is_empty());
    }

    #[test]
    fn set_default_to_an_unknown_name_is_rejected() {
        let (_dir, path) = catalog_path();
        let err = call(&path, "model/setDefault", json!({ "name": "nope" })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "model_not_found");
        assert_eq!(err.code, -32025);
    }

    #[test]
    fn set_subagent_to_an_unknown_name_is_rejected() {
        let (_dir, path) = catalog_path();
        let err = call(&path, "model/setSubagent", json!({ "name": "nope" })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "model_not_found");
    }

    #[test]
    fn set_default_and_subagent_then_clear_with_null() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/upsert", entry_json("B")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        call(&path, "model/setSubagent", json!({ "name": "B" })).unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["default_model"], "A");
        assert_eq!(list["subagent_model"], "B");
        // null / 缺失 → 清除。
        call(&path, "model/setDefault", json!({ "name": null })).unwrap();
        call(&path, "model/setSubagent", json!({})).unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["default_model"].is_null());
        assert!(list["subagent_model"].is_null());
    }

    #[test]
    fn delete_removes_the_entry_and_clears_dangling_references() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/upsert", entry_json("B")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        call(&path, "model/setSubagent", json!({ "name": "A" })).unwrap();
        call(&path, "model/delete", json!({ "name": "A" })).unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        let models = list["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["name"], "B");
        // 悬空引用必须清掉，否则解析会静默回退。
        assert!(list["default_model"].is_null());
        assert!(list["subagent_model"].is_null());
    }

    #[test]
    fn delete_of_an_unknown_name_is_rejected() {
        let (_dir, path) = catalog_path();
        let err = call(&path, "model/delete", json!({ "name": "nope" })).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "model_not_found");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (_dir, path) = catalog_path();
        let err = call(&path, "model/bogus", json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[test]
    fn a_saved_catalog_round_trips_through_the_rpc_surface() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        assert!(path.exists(), "upsert must land the file");
        // A second, independent read through the public path-based entry.
        let list =
            handle_model_request_at(&path, "model/list", &json!({}), &fallback_cfg()).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn effective_is_the_catalog_default_when_one_resolves() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "catalog");
        assert_eq!(out["effective"]["model_ref"], "A");
        assert_eq!(out["effective"]["model"], "m");
        assert_eq!(out["effective"]["api_key_masked"], "••••1234");
        // 明文绝不在任何字段里。
        assert!(!out.to_string().contains("sk-secret-1234"));
    }

    #[test]
    fn effective_is_the_env_fallback_when_the_catalog_cannot_resolve() {
        let (_dir, path) = catalog_path();
        // 清单为空 → 必须落到 cfg。
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert!(out["effective"]["model_ref"].is_null());
        assert_eq!(out["effective"]["model"], "env-model");
        assert_eq!(out["effective"]["api_url"], "https://env.example");
        assert_eq!(out["effective"]["provider"], "openai");
        assert_eq!(out["effective"]["api_key_masked"], "••••9999");
        assert!(!out.to_string().contains("env-secret-9999"));
    }

    #[test]
    fn a_dangling_default_falls_back_to_env() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        call(&path, "model/delete", json!({ "name": "A" })).unwrap();
        // delete 会清掉悬空引用；再手动写一个悬空 default 覆盖该情形。
        let mut catalog = ModelCatalog::default();
        catalog.models.push(ModelEntry {
            name: "B".to_string(),
            provider: ModelProvider::parse("anthropic").unwrap(),
            api_url: "https://b".to_string(),
            model: "mb".to_string(),
            api_key: String::new(),
        });
        catalog.default_model = Some("gone".to_string());
        yi_agent_runtime::models::save_catalog_to(&path, &catalog).unwrap();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert!(out["effective"]["model_ref"].is_null());
    }
}
