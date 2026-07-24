//! HTTP API handlers for config read/write.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config_meta::{ALL_VARS, VarType, groups};
use crate::env_file;

/// 共享状态：.env 文件路径
#[derive(Clone)]
pub struct AppState {
    pub env_path: PathBuf,
}

/// GET / — 返回内嵌 HTML 页面
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

/// GET /api/config — 返回所有变量元数据 + 当前值
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let vars = match env_file::read(&state.env_path) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to read .env: {e}") })),
            );
        }
    };

    let mut group_list: Vec<Value> = Vec::new();
    for group_name in groups() {
        let mut var_list: Vec<Value> = Vec::new();
        for var in ALL_VARS.iter().filter(|v| v.group == group_name) {
            let raw_value = vars.get(var.key).cloned().unwrap_or_default();
            let (display_value, masked) = if var.var_type == VarType::Secret && !raw_value.is_empty() {
                (env_file::mask(&raw_value), true)
            } else {
                (raw_value.clone(), false)
            };
            var_list.push(json!({
                "key": var.key,
                "value": display_value,
                "default": var.default,
                "type": format!("{:?}", var.var_type).to_lowercase(),
                "description": var.description,
                "options": var.options,
                "masked": masked,
            }));
        }
        group_list.push(json!({
            "name": group_name,
            "vars": var_list,
        }));
    }

    (
        StatusCode::OK,
        Json(json!({
            "groups": group_list,
            "envPath": state.env_path.display().to_string(),
        })),
    )
}

#[derive(Deserialize)]
pub struct PutConfigRequest {
    pub updates: Vec<(String, String)>,
}

/// PUT /api/config — 接收部分更新，写入 .env
pub async fn put_config(
    State(state): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> impl IntoResponse {
    let current = match env_file::read(&state.env_path) {
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
    for (key, value) in req.updates {
        if let Some(meta) = crate::config_meta::find(&key) {
            if meta.var_type == VarType::Secret && env_file::is_masked(&value) {
                // 掩码值跳过，不写入
                continue;
            }
        }
        filtered_updates.push((key, value));
    }

    match env_file::write(&state.env_path, &current, &filtered_updates) {
        Ok(()) => (StatusCode::OK, Json(json!({ "ok": true }))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to write .env: {e}") })),
        ),
    }
}
