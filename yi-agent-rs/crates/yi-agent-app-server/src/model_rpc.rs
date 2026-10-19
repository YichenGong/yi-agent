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
    ModelCatalog, ModelEntry, ModelProvider, effective_entry_for, load_catalog_from, mask_key,
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
        "model/importEnv" => import_env(path, fallback),
        _ => Err(RpcError::method_not_found(method)),
    }
}

/// 当前默认实际解析到哪里：命中条目就用条目，否则回退 cfg（`.env`）。
/// key 一律只出掩码——与 [`entry_view`] 同一条安全约定。
///
/// 条目选择走 [`effective_entry_for`]（而非无注入语境的 `effective_entry`）：
/// app-server 可能被整包注入（provider + key 由外部管道送进来），此时进程**实际
/// 运行**的是注入包，清单默认不生效——上报必须同口径（spec §1.5 目标 4
/// 「上报 == 实际运行」）。注入时 `effective_entry_for` 返回 `None`，落到的正是
/// `"source": "env"` 那支，即注入包本身。
///
/// `api_style` / `reasoning_effort` 同 `resolve_effective` 的语义：命中条目即
/// 取条目的值（缺省 = 未设置），否则回退 cfg。二者是「生效值」，UI 据此回显。
fn effective_view(catalog: &ModelCatalog, cfg: &RuntimeConfig) -> Value {
    match effective_entry_for(cfg, catalog, None) {
        Some(entry) => json!({
            "source": "catalog",
            "model_ref": entry.name,
            "provider": entry.provider.as_str(),
            "api_url": entry.api_url,
            "model": entry.model,
            "api_style": entry.api_style,
            "reasoning_effort": entry.reasoning_effort,
            "has_key": !entry.api_key.is_empty(),
            "api_key_masked": mask_key(&entry.api_key),
        }),
        None => json!({
            "source": "env",
            "model_ref": Value::Null,
            "provider": cfg.provider,
            "api_url": cfg.api_url,
            "model": cfg.model,
            "api_style": cfg.api_style.as_str(),
            "reasoning_effort": cfg.reasoning_effort,
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
        "api_style": entry.api_style,
        "reasoning_effort": entry.reasoning_effort,
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
    // 通道两项：缺省/null/非字符串都按「未声明」处理（`optional_string_field`）；
    // 一旦给了字符串就必须是已知值——未知值宁可报错，也不静默落盘一个打错的值
    // （`ApiStyle::parse` 会把未知值吞成 `auto`，那是解析期的语义，不是写入期的）。
    let api_style = optional_string_field(params, "api_style");
    if let Some(v) = &api_style {
        if !matches!(v.as_str(), "auto" | "responses" | "chat") {
            return Err(RpcError::invalid_model(
                "api_style must be auto|responses|chat",
            ));
        }
    }
    let reasoning_effort = optional_string_field(params, "reasoning_effort");
    if let Some(v) = &reasoning_effort {
        if !matches!(
            v.as_str(),
            "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
        ) {
            return Err(RpcError::invalid_model("unknown reasoning_effort"));
        }
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
        api_style,
        reasoning_effort,
        context_length: None,
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

/// `model/importEnv`：把兜底层 `cfg`（`.env`）当前的 provider/api_url/api_key/model
/// 原子地落成清单里的一条，并把它设为 `default_model`。
///
/// 一次性完成而不是前端两步（upsert + setDefault）：只有服务端握有明文 key，
/// 且两步之间可能半途失败留下「条目落了、默认没设」的中间态。
fn import_env(path: &Path, cfg: &RuntimeConfig) -> Result<Value, RpcError> {
    if cfg.api_url.trim().is_empty() {
        return Err(RpcError::invalid_model("api_url must not be empty"));
    }
    if cfg.model.trim().is_empty() {
        return Err(RpcError::invalid_model("model must not be empty"));
    }
    let Some(provider) = ModelProvider::parse(&cfg.provider) else {
        return Err(RpcError::invalid_model(format!(
            "unknown provider: {}",
            cfg.provider
        )));
    };

    let mut catalog = load_catalog_from(path);
    let base = cfg.model.trim().to_string();
    // 重名自动加后缀，静默成功（收边是「零摩擦固化」，不该被重名卡住）。
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(ModelEntry {
        name: name.clone(),
        provider,
        api_url: cfg.api_url.clone(),
        model: cfg.model.clone(),
        api_key: cfg.api_key.clone(),
        // 兜底层 cfg 的通道选择随条目固化（与 upsert 的默认语义一致）。
        api_style: Some(cfg.api_style.as_str().to_string()),
        reasoning_effort: cfg.reasoning_effort.clone(),
        reasoning_capabilities: None,
        context_length: None,
    });
    catalog.default_model = Some(name.clone());
    save_catalog_to(path, &catalog).map_err(|error| RpcError::internal(error.to_string()))?;
    Ok(json!({ "ok": true, "name": name, "default_model": name }))
}

/// 把 `model/test` 的入参解析成一份可探测的 [`ModelSettings`]。
///
/// 密钥解析是本函数的核心：桌面端的 key 框不回填（明文从不经过桌面端），
/// 因此「编辑已有条目但没改 key」时，只能由服务端按 `name` 取已存明文 key。
/// 两者都拿不到时明确拒绝，绝不用空 key 去发一次注定 401 的探测。
pub fn resolve_test_settings(
    path: &Path,
    params: &Value,
) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError> {
    let provider = string_field(params, "provider").unwrap_or_default();
    let model = string_field(params, "model").unwrap_or_default();
    let api_url = string_field(params, "api_url").unwrap_or_default();

    let explicit = string_field(params, "api_key").filter(|k| !k.is_empty());
    let api_key = match explicit {
        Some(key) => key,
        None => {
            let name = string_field(params, "name").unwrap_or_default();
            let stored = load_catalog_from(path)
                .models
                .into_iter()
                .find(|e| e.name == name)
                .map(|e| e.api_key)
                .filter(|k| !k.is_empty());
            stored.ok_or_else(|| RpcError::invalid_model("请填写 API 密钥"))?
        }
    };

    let settings = yi_agent_runtime::onboarding::ModelSettings {
        provider,
        model,
        api_url,
        api_key,
    };
    yi_agent_runtime::onboarding::validate_settings(&settings)
        .map_err(|e| RpcError::invalid_model(e.to_string()))?;
    Ok(settings)
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

/// `Option<String>` 入参的宽松读取：缺失 / `null` / 非字符串 → `None`；
/// 字符串（含空串）→ `Some`。
///
/// 用于 `api_style` / `reasoning_effort` 这类「未声明即默认」的字段：非字符串
/// 既不是可用的值，也不构成拒绝理由，因此按「未声明」处理。空串会原样返回，
/// 由调用方的白名单校验决定接受与否——不在这里替调用方猜意图。
fn optional_string_field(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(str::to_string)
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
            api_style: yi_agent_core::ApiStyle::Auto,
            reasoning_effort: None,
            max_turns: 20,
            max_resident_subagents: 8,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: std::path::PathBuf::from("/tmp/import-env-test"),
            system_prompt: None,
            compact_threshold: 160_000,
            compact_ratio: 80,
            model_context_length: None,
            compact_user_budget_tokens: 20_000,
            compact_tool_budget_tokens: 12_000,
            yolo: false,
            sandbox_promotable: true,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 8192,
            skills_catalog_budget_explicit: false,
            model_injected: false,
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
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        });
        catalog.default_model = Some("gone".to_string());
        yi_agent_runtime::models::save_catalog_to(&path, &catalog).unwrap();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert!(out["effective"]["model_ref"].is_null());
    }

    #[test]
    fn import_env_lands_the_fallback_and_makes_it_the_default() {
        let (_dir, path) = catalog_path();
        let out = call(&path, "model/importEnv", json!({})).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["name"], "env-model");
        assert_eq!(out["default_model"], "env-model");

        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
        assert_eq!(list["models"][0]["name"], "env-model");
        assert_eq!(list["models"][0]["api_url"], "https://env.example");
        assert_eq!(list["models"][0]["model"], "env-model");
        assert_eq!(list["models"][0]["api_key_masked"], "••••9999");
        // 导入后生效来源翻成清单，且明文不外泄。
        assert_eq!(list["effective"]["source"], "catalog");
        assert_eq!(list["effective"]["model_ref"], "env-model");
        assert!(!list.to_string().contains("env-secret-9999"));
    }

    #[test]
    fn import_env_dedupes_a_name_that_already_exists() {
        let (_dir, path) = catalog_path();
        // 清单里先有一条同名 "env-model"。
        call(
            &path,
            "model/upsert",
            json!({ "name": "env-model", "provider": "anthropic",
                    "api_url": "https://x", "model": "mx" }),
        )
        .unwrap();
        let out = call(&path, "model/importEnv", json!({})).unwrap();
        assert_eq!(out["name"], "env-model-2");
        let list = call(&path, "model/list", json!({})).unwrap();
        let names: Vec<&str> = list["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"env-model"));
        assert!(names.contains(&"env-model-2"));
        // 不覆盖既有条目，且默认指向新条目。
        assert_eq!(list["default_model"], "env-model-2");
    }

    #[test]
    fn import_env_rejects_an_invalid_fallback_without_writing() {
        let (_dir, path) = catalog_path();
        let mut cfg = fallback_cfg();
        cfg.api_url = String::new(); // 非法：url 为空
        let err = handle_model_request_at(&path, "model/importEnv", &json!({}), &cfg).unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        assert!(!path.exists(), "a rejected import must not write a catalog");
    }

    #[test]
    fn upsert_persists_and_lists_the_two_new_optional_fields() {
        let (_dir, path) = catalog_path();
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "openai", "api_url": "u", "model": "m",
                "api_style": "responses", "reasoning_effort": "low",
            }),
        )
        .unwrap();
        // 条目视图把两键原样回给前端。
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"][0]["api_style"], "responses");
        assert_eq!(list["models"][0]["reasoning_effort"], "low");
        // 且真的落进了清单文件（下一次独立读取仍能看到）。
        let reloaded = yi_agent_runtime::models::load_catalog_from(&path);
        assert_eq!(reloaded.models[0].api_style.as_deref(), Some("responses"));
        assert_eq!(reloaded.models[0].reasoning_effort.as_deref(), Some("low"));
    }

    /// `effective` 也带生效的 `api_style` / `reasoning_effort`（任务 2 的 deferred
    /// Minor）：命中条目即取条目值，供 UI 回显「当前默认跑在什么档位/通道上」。
    #[test]
    fn effective_carries_api_style_and_reasoning_effort_from_the_entry() {
        let (_dir, path) = catalog_path();
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "openai", "api_url": "u", "model": "m",
                "api_style": "chat", "reasoning_effort": "none",
            }),
        )
        .unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["effective"]["source"], "catalog");
        assert_eq!(list["effective"]["api_style"], "chat");
        assert_eq!(list["effective"]["reasoning_effort"], "none");
    }

    /// 未命中条目时回退 cfg：`api_style` 出解析后串，`reasoning_effort` 出 cfg 值。
    #[test]
    fn effective_fallback_reports_the_cfg_values() {
        let (_dir, path) = catalog_path();
        let out = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(out["effective"]["source"], "env");
        assert_eq!(out["effective"]["api_style"], "auto");
        assert!(out["effective"]["reasoning_effort"].is_null());
    }

    /// Task 3 review fix round 1 (Finding 3, spec §1.5 goal 4 「上报 == 实际运行」):
    /// an **injected** process must not report the catalog as its effective source.
    ///
    /// The app-server is handed the injected package (provider + key piped in), yet
    /// `effective_view` called the injection-unaware `effective_entry`, so
    /// `model/list` answered `source: "catalog"` with the *default entry's* masked
    /// key while the process actually ran the injected package. The entry selection
    /// must be `effective_entry_for(cfg, catalog, None)`: under injection it returns
    /// `None`, which is exactly the `source: "env"` shape — the injected package.
    #[test]
    fn an_injected_process_reports_env_not_catalog() {
        let (_dir, path) = catalog_path();
        // A catalog with a default entry that WOULD win without injection.
        call(&path, "model/upsert", entry_json("A")).unwrap();
        call(&path, "model/setDefault", json!({ "name": "A" })).unwrap();

        let mut cfg = fallback_cfg();
        cfg.model_injected = true;
        let out = handle_model_request_at(&path, "model/list", &json!({}), &cfg).unwrap();

        assert_eq!(
            out["effective"]["source"], "env",
            "an injected process runs the injected package, so it must not report the catalog"
        );
        assert!(out["effective"]["model_ref"].is_null());
        assert_eq!(out["effective"]["model"], "env-model");
        assert_eq!(out["effective"]["provider"], "openai");
        assert_eq!(out["effective"]["api_key_masked"], "••••9999");
        // The catalog default entry's key must NOT be presented as effective.
        assert!(!out["effective"].to_string().contains("••••1234"));
    }

    #[test]
    fn upsert_without_the_new_fields_leaves_them_unset() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["models"][0]["api_style"].is_null());
        assert!(list["models"][0]["reasoning_effort"].is_null());
    }

    #[test]
    fn upsert_rejects_an_unknown_api_style_without_writing() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        let err = call(
            &path,
            "model/upsert",
            json!({
                "name": "B", "provider": "openai", "api_url": "u", "model": "m",
                "api_style": "grpc",
            }),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        // 一个字节都不写：仍是原来那一条。
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
        assert_eq!(list["models"][0]["name"], "A");
    }

    #[test]
    fn upsert_rejects_an_unknown_reasoning_effort_without_writing() {
        let (_dir, path) = catalog_path();
        call(&path, "model/upsert", entry_json("A")).unwrap();
        let err = call(
            &path,
            "model/upsert",
            json!({
                "name": "B", "provider": "openai", "api_url": "u", "model": "m",
                "reasoning_effort": "ultra",
            }),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        let list = call(&path, "model/list", json!({})).unwrap();
        assert_eq!(list["models"].as_array().unwrap().len(), 1);
        assert_eq!(list["models"][0]["name"], "A");
    }

    #[test]
    fn upsert_accepts_every_documented_effort_tier() {
        let (_dir, path) = catalog_path();
        for (i, tier) in ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
            .iter()
            .enumerate()
        {
            let name = format!("t{i}");
            call(
                &path,
                "model/upsert",
                json!({
                    "name": name, "provider": "openai", "api_url": "u", "model": "m",
                    "reasoning_effort": tier,
                }),
            )
            .unwrap_or_else(|e| panic!("tier {tier} must be accepted: {e:?}"));
        }
    }

    #[test]
    fn upsert_accepts_the_three_api_styles() {
        let (_dir, path) = catalog_path();
        for (i, style) in ["auto", "responses", "chat"].iter().enumerate() {
            let name = format!("s{i}");
            call(
                &path,
                "model/upsert",
                json!({
                    "name": name, "provider": "openai", "api_url": "u", "model": "m",
                    "api_style": style,
                }),
            )
            .unwrap_or_else(|e| panic!("api_style {style} must be accepted: {e:?}"));
        }
    }

    #[test]
    fn upsert_treats_a_null_new_field_as_unset() {
        let (_dir, path) = catalog_path();
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "openai", "api_url": "u", "model": "m",
                "api_style": null, "reasoning_effort": null,
            }),
        )
        .unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["models"][0]["api_style"].is_null());
        assert!(list["models"][0]["reasoning_effort"].is_null());
    }

    #[test]
    fn upsert_treats_a_non_string_new_field_as_unset() {
        let (_dir, path) = catalog_path();
        // 非字符串既不是合法值也不是拒绝理由：按「未声明」处理（契约见
        // `optional_string_field`）。
        call(
            &path,
            "model/upsert",
            json!({
                "name": "A", "provider": "openai", "api_url": "u", "model": "m",
                "api_style": 42, "reasoning_effort": ["low"],
            }),
        )
        .unwrap();
        let list = call(&path, "model/list", json!({})).unwrap();
        assert!(list["models"][0]["api_style"].is_null());
        assert!(list["models"][0]["reasoning_effort"].is_null());
    }

    /// 显式给出 api_key 时用它——新增模型/改过 key 的场景。
    #[test]
    fn test_settings_prefer_explicit_api_key() {
        let (dir, path) = catalog_path();
        let _ = dir;
        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://x/v1",
            "api_key": "sk-draft",
        });
        let settings = resolve_test_settings(&path, &params).unwrap();
        assert_eq!(settings.api_key, "sk-draft");
        assert_eq!(settings.api_url, "https://x/v1");
    }

    /// 没给 api_key 时按条目名取已存明文 key——编辑但没改 key 的场景。
    #[test]
    fn test_settings_fall_back_to_named_entry_key() {
        let (_dir, path) = catalog_path();
        call(
            &path,
            "model/upsert",
            json!({"name":"A","provider":"openai","api_url":"https://a/v1","model":"m","api_key":"sk-stored"}),
        )
        .unwrap();

        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://a/v1",
            "name": "A",
        });
        let settings = resolve_test_settings(&path, &params).unwrap();
        assert_eq!(settings.api_key, "sk-stored");
    }

    /// 既没 api_key 也没可命中的 name → 结构化拒绝，绝不用空 key 去探测。
    #[test]
    fn test_settings_reject_when_no_key_available() {
        let (_dir, path) = catalog_path();
        let params = json!({
            "provider": "openai",
            "model": "m",
            "api_url": "https://a/v1",
            "name": "missing",
        });
        assert!(resolve_test_settings(&path, &params).is_err());
    }
}
