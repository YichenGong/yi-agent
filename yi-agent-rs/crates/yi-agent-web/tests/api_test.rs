use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn app(settings_path: std::path::PathBuf) -> axum::Router {
    yi_agent_web::router(settings_path)
}

#[tokio::test]
async fn get_config_reports_null_for_unset_and_default_source() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    let all: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();
    let max_turns = all.iter().find(|v| v["key"] == "agent.max_turns").unwrap();
    assert_eq!(max_turns["value"], 500);
    assert_eq!(max_turns["source"], "local");

    let ratio = all.iter().find(|v| v["key"] == "compact.ratio").unwrap();
    assert!(ratio["value"].is_null());
    assert_eq!(ratio["source"], "default");
    assert_eq!(ratio["default"], "80");
}

/// 一号缺陷的回归网：保存一次不能删掉不认识的键。
#[tokio::test]
async fn put_config_preserves_unknown_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"my_note":"keep me"}"#).unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["my_note"], "keep me");
    assert_eq!(value["agent"]["max_turns"], 500);
}

/// 未设置的字段不得被写成空值行。
#[tokio::test]
async fn put_config_does_not_materialize_unset_fields() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let agent = value["agent"].as_object().unwrap();
    assert_eq!(agent.len(), 1);
    assert!(value.get("compact").is_none());
}

#[tokio::test]
async fn put_config_rejects_an_invalid_value_with_400() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"compact.ratio","value":"0"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!path.exists(), "a rejected write must not create the file");
}

/// 「真实 LLM 测试」面板已删除：路由必须是 404。
#[tokio::test]
async fn the_real_llm_test_routes_are_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    for (method, url) in [
        ("GET", "/api/real-llm-test-config"),
        ("PUT", "/api/real-llm-test-config"),
        ("POST", "/api/real-llm-test-config/validate"),
        ("DELETE", "/api/real-llm-test-config/api-key"),
    ] {
        let request = match method {
            "GET" => Request::get(url),
            "PUT" => Request::put(url),
            "POST" => Request::post(url),
            _ => Request::delete(url),
        }
        .body(Body::empty())
        .unwrap();
        let response = app(dir.path().join("settings.json"))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {url} must be gone"
        );
    }
}
