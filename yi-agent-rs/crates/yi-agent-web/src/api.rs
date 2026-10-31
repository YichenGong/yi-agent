use std::path::PathBuf;

use axum::Json;
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use serde::Deserialize;
use serde_json::{Value, json};
use yi_agent_runtime::settings::{self, FieldType, SettingsError};

/// 共享状态：`settings.json` 路径（唯一）。
#[derive(Clone)]
pub struct AppState {
    pub settings_path: PathBuf,
}

/// GET / — 返回内嵌 HTML 页面
pub async fn index_html() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

/// GET /api/config — 返回所有字段的元数据 + 当前值。
///
/// `value: null` 表示未设置；`source` 是它的派生（`"default"` / `"local"`）。
/// **不要**把 `null` 归一成 `default`：旧实现这么做过，结果是「保存一次后所有
/// 字段都显示成已设置」。
pub async fn get_config(State(state): State<AppState>) -> impl IntoResponse {
    let current = settings::load_from(&state.settings_path);

    let mut groups: Vec<Value> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for spec in settings::FIELDS {
        if !seen.contains(&spec.group) {
            seen.push(spec.group);
        }
    }
    for group in seen {
        let vars: Vec<Value> = settings::FIELDS
            .iter()
            .filter(|spec| spec.group == group)
            .map(|spec| {
                let value = field_value(&current, spec);
                json!({
                    "key": spec.key,
                    "value": value,
                    "default": spec.default_display,
                    "type": match spec.field_type {
                        FieldType::Number => "number",
                        FieldType::Text => "text",
                        FieldType::Select => "select",
                    },
                    "group": spec.group,
                    "description": spec.description,
                    "options": spec.options,
                    "min": spec.min,
                    "max": spec.max,
                    "source": if value.is_null() { "default" } else { "local" },
                })
            })
            .collect();
        groups.push(json!({ "name": group, "vars": vars }));
    }

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "groups": groups,
            "path": state.settings_path.display().to_string(),
        })),
    )
}

/// 从 `Settings` 取某个字段的 JSON 值（`None` → `null`）。
///
/// 与 `settings_rpc::current_value` 同一个 match 结构。两处都覆盖全部 `FIELDS`，
/// 各有一条测试遍历断言，漏一个键会红。
fn field_value(current: &settings::Settings, spec: &settings::FieldSpec) -> Value {
    let opt = |value: Option<Value>| value.unwrap_or(Value::Null);
    match spec.key {
        "agent.max_turns" => opt(current.agent.max_turns.map(Value::from)),
        "agent.max_direct_children" => opt(current.agent.max_direct_children.map(Value::from)),
        "agent.max_resident_subagents" => {
            opt(current.agent.max_resident_subagents.map(Value::from))
        }
        "agent.system_prompt" => opt(current.agent.system_prompt.clone().map(Value::from)),
        "agent.skills_catalog_budget" => opt(current.agent.skills_catalog_budget.map(Value::from)),
        "compact.ratio" => opt(current.compact.ratio.map(Value::from)),
        "compact.user_budget_tokens" => opt(current.compact.user_budget_tokens.map(Value::from)),
        "compact.tool_budget_tokens" => opt(current.compact.tool_budget_tokens.map(Value::from)),
        "startup.sandbox" => opt(current.startup.sandbox.clone().map(Value::from)),
        _ => Value::Null,
    }
}

#[derive(Deserialize)]
pub struct UpdateItem {
    pub key: String,
    pub value: String,
}

#[derive(Deserialize)]
pub struct PutConfigRequest {
    pub updates: Vec<UpdateItem>,
}

/// PUT /api/config — 接收部分更新。
///
/// 空串与全空白串都表示「删掉这个键、回退默认」——由 `settings::write_to` 统一
/// 处理，这里不另写一份判据。
pub async fn put_config(
    State(state): State<AppState>,
    Json(request): Json<PutConfigRequest>,
) -> impl IntoResponse {
    let mut patch = serde_json::Map::new();
    for item in request.updates {
        patch.insert(item.key, Value::from(item.value));
    }

    match settings::write_to(&state.settings_path, &Value::Object(patch)) {
        Ok(()) => (axum::http::StatusCode::OK, Json(json!({ "ok": true }))),
        Err(SettingsError::InvalidSetting { key, reason }) => (
            axum::http::StatusCode::BAD_REQUEST,
            Json(json!({ "code": "invalid_setting", "key": key, "error": reason })),
        ),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}
