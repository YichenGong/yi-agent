use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

use yi_agent_web::api::{
    AppState, clear_real_llm_test_key, get_config, get_real_llm_test_config, index_html,
    put_config, put_real_llm_test_config, validate_real_llm_test_config,
};

/// 构建 axum app 用于测试（单一 `.env` 路径）
fn test_app(env_path: PathBuf) -> axum::Router {
    use axum::routing::get;
    let state = AppState {
        real_test_env_path: env_path.clone(),
        env_path,
    };
    axum::Router::new()
        .route("/", get(index_html))
        .route("/api/config", get(get_config).put(put_config))
        .route(
            "/api/real-llm-test-config",
            get(get_real_llm_test_config).put(put_real_llm_test_config),
        )
        .route(
            "/api/real-llm-test-config/validate",
            axum::routing::post(validate_real_llm_test_config),
        )
        .route(
            "/api/real-llm-test-config/api-key",
            axum::routing::delete(clear_real_llm_test_key),
        )
        .with_state(state)
}

#[tokio::test]
async fn real_llm_save_does_not_append_a_second_form() {
    let app = test_app(TempDir::new().unwrap().path().join("real-tests.env"));
    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8(html.to_vec()).unwrap();

    let save_handler = html
        .lines()
        .find(|line| line.contains("save.onclick"))
        .expect("real LLM save handler");
    assert!(
        !save_handler.contains("renderRealLlmTestsTab(main)"),
        "Save must not append another form by rerendering into the existing main element"
    );
}

#[tokio::test]
async fn real_llm_test_config_is_write_only_and_validates_without_network() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join("real-tests.env");
    let sentinel = "real-test-secret-must-not-appear";
    let app = test_app(env_path.clone());
    let request = Request::builder()
        .method("PUT")
        .uri("/api/real-llm-test-config")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "provider": "anthropic",
                "api_url": "https://api.example.test",
                "model": "test-model",
                "api_key": sentinel,
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = test_app(env_path.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/real-llm-test-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains(sentinel));
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["api_key_configured"], true);
    assert!(value.get("api_key").is_none());

    let app = test_app(env_path.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/real-llm-test-config/validate")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = test_app(env_path.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/real-llm-test-config/api-key")
                .header("content-type", "application/json")
                .body(Body::from(json!({"confirm": true}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !yi_agent_web::env_file::read(&env_path)
            .unwrap()
            .contains_key("YI_AGENT_REAL_LLM_API_KEY")
    );
}

#[tokio::test]
async fn real_llm_test_config_rejects_invalid_shape() {
    let tmp = TempDir::new().unwrap();
    let app = test_app(tmp.path().join("real-tests.env"));
    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/real-llm-test-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "provider": "invalid",
                        "api_url": "ftp://example.test",
                        "model": " ",
                        "api_key": "",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
    // 当前仍是 3 组 / 16 个变量；Task 9 从 env 编辑器移除 9 个模型键后变为 2 组 / 7。
    let total_vars: usize = groups
        .iter()
        .map(|g| g["vars"].as_array().unwrap().len())
        .sum();
    assert_eq!(total_vars, 16);
}

#[tokio::test]
async fn get_config_masks_secret_values() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "BOCHA_API_KEY=sk-ant-api03-xxxxxxxxxxxx\n").unwrap();
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

    // 找到 BOCHA_API_KEY（仍是 secret）
    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "BOCHA_API_KEY" {
                assert_eq!(var["masked"], true);
                assert!(var["value"].as_str().unwrap().contains("***"));
                return;
            }
        }
    }
    panic!("BOCHA_API_KEY not found in response");
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
    std::fs::write(&env_path, "BOCHA_API_KEY=sk-ant-real-key-12345\n").unwrap();
    let app = test_app(env_path.clone());

    // 发送掩码值（应被跳过）
    let body = json!({
        "updates": [
            { "key": "BOCHA_API_KEY", "value": "sk-a***2345" }
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
    assert_eq!(map.get("BOCHA_API_KEY").unwrap(), "sk-ant-real-key-12345");
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

#[tokio::test]
async fn index_html_has_no_scope_toggle() {
    // 单一 `.env` 后，全局/本地切换没有意义：页面不再渲染 scope 切换。
    let tmp = TempDir::new().unwrap();
    let app = test_app(tmp.path().join(".env"));

    let response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let html = String::from_utf8(body.to_vec()).unwrap();

    assert!(!html.contains("scopeGlobal"));
    assert!(!html.contains("scopeLocal"));
    assert!(!html.contains("currentScope"));
}

#[tokio::test]
async fn get_config_reads_the_single_file() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    std::fs::write(&env_path, "YI_AGENT_MAX_TURNS=42\n").unwrap();
    let app = test_app(env_path.clone());

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

    assert_eq!(json["envPath"], env_path.display().to_string());
    assert!(json.get("globalEnvPath").is_none());

    let mut found = false;
    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "YI_AGENT_MAX_TURNS" {
                assert_eq!(var["value"], "42");
                assert_eq!(var["source"], "local");
                found = true;
            }
        }
    }
    assert!(found, "YI_AGENT_MAX_TURNS should be in response");
}

#[tokio::test]
async fn put_config_writes_the_single_file() {
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path.clone());

    let body = json!({
        "updates": [
            { "key": "YI_AGENT_MAX_TURNS", "value": "7" }
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

    let content = std::fs::read_to_string(&env_path).unwrap();
    assert!(content.contains("YI_AGENT_MAX_TURNS=7"));
}

#[tokio::test]
async fn put_config_ignores_a_stale_scope_field() {
    // 老前端可能仍带 `scope`；单一 `.env` 后该字段被忽略，写入依然落到唯一路径。
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path.clone());

    let body = json!({
        "scope": "global",
        "updates": [
            { "key": "YI_AGENT_MAX_TURNS", "value": "9" }
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

    let content = std::fs::read_to_string(&env_path).unwrap();
    assert!(content.contains("YI_AGENT_MAX_TURNS=9"));
}
