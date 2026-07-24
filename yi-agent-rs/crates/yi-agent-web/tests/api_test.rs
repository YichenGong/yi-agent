use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

use yi_agent_web::api::{AppState, get_config, index_html, put_config};

/// 构建 axum app 用于测试
fn test_app(env_path: PathBuf) -> axum::Router {
    use axum::routing::get;
    let state = AppState { env_path };
    axum::Router::new()
        .route("/", get(index_html))
        .route("/api/config", get(get_config).put(put_config))
        .with_state(state)
}

#[tokio::test]
async fn get_config_returns_all_groups() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let groups = json["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 5); // Provider, Agent, Anthropic, OpenAI, Tools

    // 验证包含所有 14 个变量
    let total_vars: usize = groups
        .iter()
        .map(|g| g["vars"].as_array().unwrap().len())
        .sum();
    assert_eq!(total_vars, 14);
}

#[tokio::test]
async fn get_config_masks_secret_values() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "MODEL_API_KEY=sk-ant-api03-xxxxxxxxxxxx\n").unwrap();
    let app = test_app(env_path);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // 找到 MODEL_API_KEY
    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "MODEL_API_KEY" {
                assert_eq!(var["masked"], true);
                assert!(var["value"].as_str().unwrap().contains("***"));
                return;
            }
        }
    }
    panic!("MODEL_API_KEY not found in response");
}

#[tokio::test]
async fn put_config_writes_updates() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path.clone());

    let body = json!({
        "updates": [
            { "key": "YI_AGENT_MODEL", "value": "test-model-123" }
        ]
    });
    let request = Request::builder()
        .method("PUT")
        .uri("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 验证文件写入
    let content = std::fs::read_to_string(&env_path).unwrap();
    assert!(content.contains("YI_AGENT_MODEL=test-model-123"));
}

#[tokio::test]
async fn put_config_skips_masked_secrets() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "MODEL_API_KEY=sk-ant-real-key-12345\n").unwrap();
    let app = test_app(env_path.clone());

    // 发送掩码值（应被跳过）
    let body = json!({
        "updates": [
            { "key": "MODEL_API_KEY", "value": "sk-a***2345" }
        ]
    });
    let request = Request::builder()
        .method("PUT")
        .uri("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // 验证原值未被覆盖
    let map = yi_agent_web::env_file::read(&env_path).unwrap();
    assert_eq!(map.get("MODEL_API_KEY").unwrap(), "sk-ant-real-key-12345");
}

#[tokio::test]
async fn index_html_returns_html() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path);

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();
    assert!(html.contains("<html") || html.contains("<!DOCTYPE"));
}
