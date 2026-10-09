//! `onboarding/*` RPC：首次安装引导。路径可注入，测试不碰真实 HOME。
//!
//! 与 `model_rpc.rs` 同类：把 `method` + `params` 翻成对 runtime 引导模块的调用。

use crate::protocol::RpcError;
use serde_json::{Value, json};
use std::path::Path;
use yi_agent_runtime::config::RuntimeConfig;

/// `onboarding/*` 的唯一入口。路径与兜底 cfg 均由调用方注入（测试不碰真实 HOME）。
pub fn handle_onboarding_request_at(
    env_path: &Path,
    preferences_path: &Path,
    models_path: &Path,
    method: &str,
    params: &Value,
    fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    match method {
        "onboarding/status" => {
            let env = yi_agent_runtime::onboarding::read_env_fields(env_path)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            let catalog = yi_agent_runtime::models::load_catalog_from(models_path);
            let assessment = yi_agent_runtime::onboarding::assess(&env, Some(&catalog));
            let dismissed = yi_agent_runtime::onboarding::load_dismissed(preferences_path);
            let (needed, reasons) = match assessment {
                yi_agent_runtime::onboarding::Assessment::Ready { .. } => (false, Vec::new()),
                yi_agent_runtime::onboarding::Assessment::Needed { reasons } => (
                    true,
                    reasons
                        .into_iter()
                        .map(missing_name)
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                ),
            };
            Ok(json!({ "needed": needed, "dismissed": dismissed, "reasons": reasons }))
        }
        "onboarding/apply" => apply(env_path, models_path, params, fallback),
        "onboarding/dismiss" => {
            yi_agent_runtime::onboarding::save_dismissed(preferences_path, true)
                .map_err(|e| RpcError::internal(e.to_string()))?;
            Ok(json!({ "ok": true }))
        }
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn missing_name(missing: yi_agent_runtime::onboarding::Missing) -> &'static str {
    use yi_agent_runtime::onboarding::Missing;
    match missing {
        Missing::Provider => "provider",
        Missing::Model => "model",
        Missing::ApiUrl => "api_url",
        Missing::ApiKey => "api_key",
    }
}

/// 把 `params` 翻成 `ModelSettings` 并校验；校验失败 → `invalid_model`、零写入。
/// `server.rs` 的 `onboarding/test` 分支也复用它（本轮不落盘，只探测）。
pub fn settings_from_params(
    params: &Value,
) -> Result<yi_agent_runtime::onboarding::ModelSettings, RpcError> {
    let settings = yi_agent_runtime::onboarding::ModelSettings {
        provider: string_field(params, "provider").unwrap_or_default(),
        model: string_field(params, "model").unwrap_or_default(),
        api_url: string_field(params, "api_url").unwrap_or_default(),
        api_key: string_field(params, "api_key").unwrap_or_default(),
    };
    yi_agent_runtime::onboarding::validate_settings(&settings)
        .map_err(|e| RpcError::invalid_model(e.to_string()))?;
    Ok(settings)
}

fn apply(
    env_path: &Path,
    models_path: &Path,
    params: &Value,
    _fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    let settings = settings_from_params(params)?;

    // 1) 写全局 .env（行级保留式、原子）。
    yi_agent_runtime::onboarding::write_model_settings(env_path, &settings)
        .map_err(|e| RpcError::internal(e.to_string()))?;

    // 2) 尝试收边到清单。失败不吞：如实回包 imported=false + 原因。
    let import_result = import_into_catalog(models_path, &settings);
    match import_result {
        Ok(name) => Ok(json!({
            "ok": true,
            "env_written": true,
            "imported": true,
            "name": name,
        })),
        Err(error) => Ok(json!({
            "ok": true,
            "env_written": true,
            "imported": false,
            "import_error": error,
        })),
    }
}

/// 把配置落成清单里的一条并设为默认；重名自动加后缀。语义与
/// `model_rpc::import_env` 一致，但入参是引导收敛出的 settings。
fn import_into_catalog(
    models_path: &Path,
    settings: &yi_agent_runtime::onboarding::ModelSettings,
) -> Result<String, String> {
    use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider, save_catalog_to};

    let Some(provider) = ModelProvider::parse(&settings.provider) else {
        return Err("unknown provider".to_string());
    };
    let mut catalog: ModelCatalog = yi_agent_runtime::models::load_catalog_from(models_path);
    let base = settings.model.trim().to_string();
    let mut name = base.clone();
    let mut n = 2u32;
    while catalog.models.iter().any(|m| m.name == name) {
        name = format!("{base}-{n}");
        n += 1;
    }
    catalog.models.push(ModelEntry {
        name: name.clone(),
        provider,
        api_url: settings.api_url.clone(),
        model: settings.model.clone(),
        api_key: settings.api_key.clone(),
    });
    catalog.default_model = Some(name.clone());
    save_catalog_to(models_path, &catalog).map_err(|e| e.to_string())?;
    Ok(name)
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

    struct Paths {
        _dir: tempfile::TempDir,
        env: PathBuf,
        prefs: PathBuf,
        models: PathBuf,
    }

    fn paths() -> Paths {
        let dir = tempfile::TempDir::new().unwrap();
        let env = dir.path().join(".yi-agent").join(".env");
        let prefs = dir.path().join(".yi-agent").join("preferences.json");
        let models = dir.path().join(".yi-agent").join("models.json");
        Paths {
            _dir: dir,
            env,
            prefs,
            models,
        }
    }

    fn fallback() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".to_string(),
            api_url: "https://api.anthropic.com".to_string(),
            api_key: String::new(),
            model: "cfg-model".to_string(),
            max_turns: 20,
            max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: PathBuf::from("/tmp/onboarding-rpc-test"),
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

    fn call(p: &Paths, method: &str, params: Value) -> Result<Value, RpcError> {
        handle_onboarding_request_at(&p.env, &p.prefs, &p.models, method, &params, &fallback())
    }

    #[test]
    fn status_reports_needed_with_reasons_on_a_blank_machine() {
        let p = paths();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["needed"], true);
        assert_eq!(out["dismissed"], false);
        let reasons = out["reasons"].as_array().unwrap();
        assert!(reasons.iter().any(|r| r == "api_key"), "got {reasons:?}");
    }

    #[test]
    fn status_reports_ready_when_the_env_is_complete() {
        let p = paths();
        call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-x"}),
        )
        .unwrap();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["needed"], false);
    }

    #[test]
    fn apply_writes_the_env_and_lands_the_catalog_entry() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"https://u","api_key":"sk-x"}),
        )
        .unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["env_written"], true);
        assert_eq!(out["imported"], true);

        let text = std::fs::read_to_string(&p.env).unwrap();
        assert!(text.contains("YI_AGENT_PROVIDER=openai"));
        assert!(text.contains("MODEL_API_KEY=sk-x"));

        let catalog = yi_agent_runtime::models::load_catalog_from(&p.models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(catalog.models[0].api_key, "sk-x");
    }

    #[test]
    fn apply_with_an_invalid_provider_writes_nothing() {
        let p = paths();
        let err = call(
            &p,
            "onboarding/apply",
            json!({"provider":"gemini","model":"m","api_url":"","api_key":"k"}),
        )
        .unwrap_err();
        assert_eq!(err.data.unwrap()["code"], "invalid_model");
        assert!(!p.env.exists(), "a rejected apply must not write .env");
        assert!(
            !p.models.exists(),
            "a rejected apply must not write models.json"
        );
    }

    #[test]
    fn apply_never_returns_the_raw_key() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"","api_key":"sk-secret-1234"}),
        )
        .unwrap();
        assert!(!out.to_string().contains("sk-secret-1234"), "got {out}");
    }

    #[test]
    fn dismiss_marks_and_loads_back() {
        let p = paths();
        call(&p, "onboarding/dismiss", json!({})).unwrap();
        let out = call(&p, "onboarding/status", json!({})).unwrap();
        assert_eq!(out["dismissed"], true);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let p = paths();
        let err = call(&p, "onboarding/bogus", json!({})).unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
