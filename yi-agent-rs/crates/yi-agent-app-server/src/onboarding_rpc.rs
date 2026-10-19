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

pub(crate) fn missing_name(missing: yi_agent_runtime::onboarding::Missing) -> &'static str {
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
    _env_path: &Path,
    models_path: &Path,
    params: &Value,
    _fallback: &RuntimeConfig,
) -> Result<Value, RpcError> {
    // 注意:这里**只做校验 + 写盘**,不做连接探测。
    // 探测是 async 的,必须由 `rpc/onboarding.rs` 的 async 分支负责:
    // 同步函数里 `block_on` 会在 tokio worker 上 panic,而且会和 RPC 层
    // 各探一次、白发一轮请求。客户端只能经 RPC 到达这里,门禁不会漏。
    let settings = settings_from_params(params)?;
    // 单写清单：`.env` 不再承载模型键，`imported` / `import_error` / `env_written`
    // 这些「双写」时代的字段一并消失。写失败即整个 apply 失败（无半成品）。
    let name = yi_agent_runtime::onboarding::import_settings_into_catalog(models_path, &settings)
        .map_err(RpcError::internal)?;
    Ok(json!({ "ok": true, "name": name }))
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
            api_style: yi_agent_core::ApiStyle::Auto,
            reasoning_effort: None,
            max_turns: 20,
            max_resident_subagents: yi_agent_runtime::config::RESIDENT_SUBAGENTS_DEFAULT,
            max_direct_children: yi_agent_runtime::config::DIRECT_CHILDREN_DEFAULT,
            workdir: PathBuf::from("/tmp/onboarding-rpc-test"),
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
    fn status_reports_ready_once_the_catalog_is_complete() {
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
    fn apply_writes_the_catalog_and_never_the_env() {
        let p = paths();
        let out = call(
            &p,
            "onboarding/apply",
            json!({"provider":"openai","model":"gpt-4o","api_url":"https://u","api_key":"sk-x"}),
        )
        .unwrap();
        assert_eq!(out["ok"], true);
        assert!(out.get("name").is_some());
        assert!(out.get("env_written").is_none());
        assert!(out.get("imported").is_none());
        assert!(out.get("import_error").is_none());

        assert!(
            !p.env.exists()
                || !std::fs::read_to_string(&p.env)
                    .unwrap()
                    .contains("MODEL_API_KEY"),
            "apply must not write model keys into .env"
        );

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
