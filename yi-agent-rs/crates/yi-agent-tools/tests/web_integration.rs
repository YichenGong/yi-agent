use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
use yi_agent_core::ToolRegistry;
use yi_agent_tools::register_builtin_tools;

#[tokio::test]
async fn register_includes_web_fetch() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    assert!(
        registry.get("web_fetch").is_some(),
        "web_fetch should be registered"
    );
}

#[tokio::test]
async fn register_includes_all_tools() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    // FS + Shell tools (6) + WebFetch (1) = 7 minimum
    // WebSearch only if BOCHA_API_KEY set
    for name in &["read", "write", "edit", "glob", "grep", "bash", "web_fetch"] {
        assert!(registry.get(name).is_some(), "missing tool: {}", name);
    }
}

#[tokio::test]
async fn web_fetch_works_via_registry() {
    let tmp = TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html")
                .set_body_string("<h1>Test</h1>"),
        )
        .mount(&server)
        .await;

    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    let fetch = registry.get("web_fetch").unwrap();
    let result = fetch
        .call(serde_json::json!({
            "url": format!("{}/page", server.uri())
        }))
        .await;
    assert!(!result.is_error);
    if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
        assert!(s.to_lowercase().contains("test"));
    }
}
