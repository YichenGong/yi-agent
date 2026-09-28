//! Registration entry point: config load, cache probe, tool registration.

use yi_agent_core::ToolRegistry;

#[test]
fn no_config_registers_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    let mgr = yi_agent_mcp::register_mcp_tools(&mut reg, dir.path()).unwrap();
    assert!(mgr.is_none());
    assert!(reg.is_empty());
}

#[test]
fn config_with_no_cache_and_bad_command_still_succeeds_without_tools() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join(".yi-agent");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("mcp.json"),
        r#"{"mcpServers":{"broken":{"command":"/nonexistent/definitely-not-here"}}}"#,
    )
    .unwrap();

    let mut reg = ToolRegistry::new();
    let mgr = yi_agent_mcp::register_mcp_tools(&mut reg, dir.path()).unwrap();
    assert!(
        mgr.is_some(),
        "manager exists even if the server fails to probe"
    );
    assert!(
        reg.is_empty(),
        "a failed probe must not register tools or panic"
    );
}

#[tokio::test]
async fn registering_inside_a_runtime_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join(".yi-agent");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("mcp.json"),
        r#"{"mcpServers":{"broken":{"command":"/nonexistent/definitely-not-here"}}}"#,
    )
    .unwrap();
    let mut reg = ToolRegistry::new();
    let mgr = yi_agent_mcp::register_mcp_tools(&mut reg, dir.path()).unwrap();
    assert!(mgr.is_some());
}
