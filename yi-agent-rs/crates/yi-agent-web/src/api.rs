//! HTTP API handlers for config read/write.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
pub struct RealLlmTestConfigRequest {
    pub provider: String,
    pub api_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
}

#[derive(Deserialize)]
pub struct ClearRealLlmTestKeyRequest {
    pub confirm: bool,
}

use crate::config_meta::{ALL_VARS, VarType, groups};
use crate::env_file;

/// 共享状态：.env 文件路径
#[derive(Clone)]
pub struct AppState {
    pub env_path: PathBuf,
    pub global_env_path: Option<PathBuf>,
}

/// GET / — 返回内嵌 HTML 页面
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

pub async fn get_real_llm_test_config(State(state): State<AppState>) -> impl IntoResponse {
    let values = env_file::read(&state.env_path).unwrap_or_default();
    (
        StatusCode::OK,
        Json(json!({
            "provider": values.get("YI_AGENT_REAL_LLM_PROVIDER").cloned().unwrap_or_default(),
            "api_url": values.get("YI_AGENT_REAL_LLM_API_URL").cloned().unwrap_or_default(),
            "model": values.get("YI_AGENT_REAL_LLM_MODEL").cloned().unwrap_or_default(),
            "api_key_configured": values.get("YI_AGENT_REAL_LLM_API_KEY").is_some_and(|key| !key.trim().is_empty()),
        })),
    )
}

pub async fn put_real_llm_test_config(
    State(state): State<AppState>,
    Json(request): Json<RealLlmTestConfigRequest>,
) -> impl IntoResponse {
    let mut values = match env_file::read(&state.env_path) {
        Ok(values) => values,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"cannot read real test configuration"})),
            );
        }
    };
    let key_present = !request.api_key.trim().is_empty()
        || values
            .get("YI_AGENT_REAL_LLM_API_KEY")
            .is_some_and(|key| !key.trim().is_empty());
    if let Err(error) = validate_real_config(
        &request.provider,
        &request.api_url,
        &request.model,
        key_present,
    ) {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
    }
    values.insert("YI_AGENT_REAL_LLM_PROVIDER".into(), request.provider);
    values.insert("YI_AGENT_REAL_LLM_API_URL".into(), request.api_url);
    values.insert("YI_AGENT_REAL_LLM_MODEL".into(), request.model);
    if !request.api_key.trim().is_empty() {
        values.insert("YI_AGENT_REAL_LLM_API_KEY".into(), request.api_key);
    }
    match env_file::write_selected(&state.env_path, &values) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok":true}))),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"cannot write real test configuration"})),
        ),
    }
}

pub async fn validate_real_llm_test_config(State(state): State<AppState>) -> impl IntoResponse {
    let values = match env_file::read(&state.env_path) {
        Ok(values) => values,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"cannot read real test configuration"})),
            );
        }
    };
    let result = validate_real_config(
        values
            .get("YI_AGENT_REAL_LLM_PROVIDER")
            .map(String::as_str)
            .unwrap_or(""),
        values
            .get("YI_AGENT_REAL_LLM_API_URL")
            .map(String::as_str)
            .unwrap_or(""),
        values
            .get("YI_AGENT_REAL_LLM_MODEL")
            .map(String::as_str)
            .unwrap_or(""),
        values
            .get("YI_AGENT_REAL_LLM_API_KEY")
            .is_some_and(|key| !key.trim().is_empty()),
    );
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({"ok":true}))),
        Err(error) => (StatusCode::BAD_REQUEST, Json(json!({"error":error}))),
    }
}

pub async fn clear_real_llm_test_key(
    State(state): State<AppState>,
    Json(request): Json<ClearRealLlmTestKeyRequest>,
) -> impl IntoResponse {
    if !request.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"clear confirmation is required"})),
        );
    }
    let mut values = match env_file::read(&state.env_path) {
        Ok(values) => values,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"cannot read real test configuration"})),
            );
        }
    };
    values.remove("YI_AGENT_REAL_LLM_API_KEY");
    match env_file::write_selected(&state.env_path, &values) {
        Ok(()) => (StatusCode::OK, Json(json!({"ok":true}))),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"cannot write real test configuration"})),
        ),
    }
}

fn validate_real_config(
    provider: &str,
    api_url: &str,
    model: &str,
    key_present: bool,
) -> Result<(), String> {
    if !matches!(provider, "anthropic" | "openai") {
        return Err("provider must be anthropic or openai".into());
    }
    if !(api_url.starts_with("https://") || api_url.starts_with("http://")) {
        return Err("API URL must be an absolute HTTP(S) URL".into());
    }
    if model.trim().is_empty() {
        return Err("model is required".into());
    }
    if !key_present {
        return Err("API key is required".into());
    }
    Ok(())
}

/// GET /api/config — 返回所有变量元数据 + 合并后的值(local 覆盖 global)
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    // 读 global(可选)和 local,合并:local 覆盖 global
    let global_vars = state
        .global_env_path
        .as_ref()
        .map(|p| env_file::read(p).unwrap_or_default())
        .unwrap_or_default();
    let local_vars = env_file::read(&state.env_path).unwrap_or_default();

    let mut group_list: Vec<Value> = Vec::new();
    for group_name in groups() {
        let mut var_list: Vec<Value> = Vec::new();
        for var in ALL_VARS.iter().filter(|v| v.group == group_name) {
            // 确定 source:local 优先,然后 global,再 default
            let (raw_value, source) = if let Some(v) = local_vars.get(var.key) {
                (v.clone(), "local")
            } else if let Some(v) = global_vars.get(var.key) {
                (v.clone(), "global")
            } else {
                (String::new(), "default")
            };
            let (display_value, masked) =
                if var.var_type == VarType::Secret && !raw_value.is_empty() {
                    (env_file::mask(&raw_value), true)
                } else {
                    (raw_value.clone(), false)
                };
            var_list.push(json!({
                "key": var.key,
                "value": display_value,
                "default": var.default,
                "type": format!("{:?}", var.var_type).to_lowercase(),
                "group": var.group,
                "description": var.description,
                "options": var.options,
                "masked": masked,
                "source": source,
            }));
        }
        group_list.push(json!({
            "name": group_name,
            "vars": var_list,
        }));
    }

    let mut response = json!({
        "groups": group_list,
        "envPath": state.env_path.display().to_string(),
    });
    if let Some(g) = &state.global_env_path {
        response["globalEnvPath"] = json!(g.display().to_string());
    }

    (StatusCode::OK, Json(response))
}

#[derive(Deserialize)]
pub struct UpdateItem {
    pub key: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct PutConfigRequest {
    pub updates: Vec<UpdateItem>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// PUT /api/config — 接收部分更新，写入 .env
/// scope: "local"(默认) 写本地, "global" 写全局
pub async fn put_config(
    State(state): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> impl IntoResponse {
    let scope = req.scope.as_deref().unwrap_or("local");
    let target_path = match scope {
        "global" => match &state.global_env_path {
            Some(p) => p.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({ "error": "global scope not available (no global path configured)" }),
                    ),
                );
            }
        },
        _ => state.env_path.clone(),
    };

    let current = match env_file::read(&target_path) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to read .env: {e}") })),
            );
        }
    };

    // 过滤掉掩码值（secret 字段未修改时前端会发回掩码值）
    let mut filtered_updates: Vec<(String, String)> = Vec::new();
    for item in req.updates {
        if let Some(meta) = crate::config_meta::find(&item.key) {
            if meta.var_type == VarType::Secret && env_file::is_masked(&item.value) {
                // 掩码值跳过，不写入
                continue;
            }
        }
        filtered_updates.push((item.key, item.value));
    }

    match env_file::write(&target_path, &current, &filtered_updates) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to write .env: {e}") })),
        ),
    }
}
