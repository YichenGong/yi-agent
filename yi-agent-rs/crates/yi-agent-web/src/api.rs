//! HTTP API handlers for config read/write.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use serde::Deserialize;
use serde_json::{Value, json};

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
