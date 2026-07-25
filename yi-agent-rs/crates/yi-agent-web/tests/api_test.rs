use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

use yi_agent_web::api::{AppState, get_config, index_html, put_config};

/// 构建 axum app 用于测试
fn test_app(env_path: PathBuf) -> axum::Router {
    test_app_with_global(env_path, None)
}

/// 构建 axum app 用于测试(带 global path)
fn test_app_with_global(env_path: PathBuf, global_env_path: Option<PathBuf>) -> axum::Router {
    use axum::routing::get;
    let state = AppState {
        env_path,
        global_env_path,
    };
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
    assert_eq!(groups.len(), 3); // Model Provider, Agent, Tools

    // 验证包含所有 15 个变量
    let total_vars: usize = groups
        .iter()
        .map(|g| g["vars"].as_array().unwrap().len())
        .sum();
    assert_eq!(total_vars, 15);
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

#[tokio::test]
async fn get_config_merges_global_and_local() {
    // global 有 MODEL_API_KEY, local 有 YI_AGENT_MODEL
    // GET 应返回两者合并, source 标注来源
    let tmp_local = TempDir::new().unwrap();
    let tmp_global = TempDir::new().unwrap();
    let local_path = tmp_local.path().join(".env");
    let global_path = tmp_global.path().join(".env");
    std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();
    std::fs::write(&local_path, "YI_AGENT_MODEL=from-local\n").unwrap();
    let app = test_app_with_global(local_path, Some(global_path));

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

    // 找到 MODEL_API_KEY(source 应为 "global")
    let mut found_global = false;
    let mut found_local = false;
    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "MODEL_API_KEY" {
                assert_eq!(var["source"], "global");
                found_global = true;
            }
            if var["key"] == "YI_AGENT_MODEL" {
                assert_eq!(var["source"], "local");
                found_local = true;
            }
        }
    }
    assert!(found_global, "MODEL_API_KEY should be in response");
    assert!(found_local, "YI_AGENT_MODEL should be in response");
}

#[tokio::test]
async fn get_config_local_overrides_global_source() {
    // local 和 global 都有 MODEL_API_KEY → 值用 local 的, source 标 "local"
    let tmp_local = TempDir::new().unwrap();
    let tmp_global = TempDir::new().unwrap();
    let local_path = tmp_local.path().join(".env");
    let global_path = tmp_global.path().join(".env");
    std::fs::write(&global_path, "MODEL_API_KEY=from-global\n").unwrap();
    std::fs::write(&local_path, "MODEL_API_KEY=from-local\n").unwrap();
    let app = test_app_with_global(local_path, Some(global_path));

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

    for group in json["groups"].as_array().unwrap() {
        for var in group["vars"].as_array().unwrap() {
            if var["key"] == "MODEL_API_KEY" {
                // 值应该是 local 的(masked, 因为是 secret)
                assert_eq!(var["source"], "local");
                assert_eq!(var["masked"], true);
                return;
            }
        }
    }
    panic!("MODEL_API_KEY not found");
}

#[tokio::test]
async fn get_config_returns_env_path_and_global_path() {
    let tmp_local = TempDir::new().unwrap();
    let tmp_global = TempDir::new().unwrap();
    let local_path = tmp_local.path().join(".env");
    let global_path = tmp_global.path().join(".env");
    let app = test_app_with_global(local_path.clone(), Some(global_path.clone()));

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

    assert_eq!(json["envPath"], local_path.display().to_string());
    assert_eq!(json["globalEnvPath"], global_path.display().to_string());
}

#[tokio::test]
async fn get_config_no_global_path_when_none() {
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

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json.get("globalEnvPath").is_none() || json["globalEnvPath"].is_null());
}

#[tokio::test]
async fn put_config_writes_to_global_scope() {
    let tmp_local = TempDir::new().unwrap();
    let tmp_global = TempDir::new().unwrap();
    let local_path = tmp_local.path().join(".env");
    let global_path = tmp_global.path().join(".env");
    let app = test_app_with_global(local_path.clone(), Some(global_path.clone()));

    let body = json!({
        "scope": "global",
        "updates": [
            { "key": "YI_AGENT_MODEL", "value": "global-model" }
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

    // 验证写入 global,local 未被修改
    let global_content = std::fs::read_to_string(&global_path).unwrap();
    assert!(global_content.contains("YI_AGENT_MODEL=global-model"));
    assert!(!local_path.exists() || std::fs::read_to_string(&local_path).unwrap().is_empty());
}

#[tokio::test]
async fn put_config_writes_to_local_scope_by_default() {
    let tmp_local = TempDir::new().unwrap();
    let tmp_global = TempDir::new().unwrap();
    let local_path = tmp_local.path().join(".env");
    let global_path = tmp_global.path().join(".env");
    let app = test_app_with_global(local_path.clone(), Some(global_path.clone()));

    // 不传 scope,默认 local
    let body = json!({
        "updates": [
            { "key": "YI_AGENT_MODEL", "value": "local-model" }
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

    let local_content = std::fs::read_to_string(&local_path).unwrap();
    assert!(local_content.contains("YI_AGENT_MODEL=local-model"));
    // global 未被修改(文件不存在,说明没写)
    assert!(
        !global_path.exists()
            || !std::fs::read_to_string(&global_path)
                .unwrap()
                .contains("YI_AGENT_MODEL")
    );
}

#[tokio::test]
async fn put_config_global_scope_returns_400_when_no_global() {
    // 显式 --workdir 模式,global_path 为 None
    let tmp = TempDir::new().unwrap();
    let env_path = tmp.path().join(".env");
    let app = test_app(env_path); // global_env_path = None

    let body = json!({
        "scope": "global",
        "updates": [
            { "key": "YI_AGENT_MODEL", "value": "should-fail" }
        ]
    });
    let request = Request::builder()
        .method("PUT")
        .uri("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
