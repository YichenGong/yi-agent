use tempfile::TempDir;
use yi_agent_core::ToolRegistry;
use yi_agent_tools::register_builtin_tools;

#[tokio::test]
async fn register_all_tools_and_use_read() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("hello.txt"), "hello world").unwrap();

    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    let read = registry.get("read").expect("read tool registered");
    let result = read.call(serde_json::json!({"path": "hello.txt"})).await;
    assert!(!result.is_error);
    if let yi_agent_core::ContentBlock::Text(s) = &result.content[0] {
        assert!(s.contains("hello world"));
    } else {
        panic!("expected text block");
    }
}

#[tokio::test]
async fn all_six_tools_registered() {
    let tmp = TempDir::new().unwrap();
    let mut registry = ToolRegistry::new();
    register_builtin_tools(&mut registry, tmp.path().to_path_buf());

    for name in &["read", "write", "edit", "glob", "grep", "bash"] {
        assert!(registry.get(name).is_some(), "missing tool: {}", name);
    }
}
