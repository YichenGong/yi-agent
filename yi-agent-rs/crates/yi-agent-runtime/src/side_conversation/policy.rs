//! The side conversation's capability boundary.
//!
//! A side conversation is **built from a whitelist**, never by deleting dangerous
//! names from the ordinary registry. That distinction matters: a registry filtered
//! after the fact still contains whatever the ordinary assembly added, whereas this
//! one can only ever receive the tools named below.
//!
//! Registration requires two independent checks — the name is on the whitelist,
//! *and* the tool reports `ToolSource::Builtin` with `read_only: true`. A tool that
//! merely shares a name (an MCP or plugin impostor) is rejected, and YOLO never
//! widens the list.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use yi_agent_core::{ToolRegistry, ToolSource};

use super::source::{EffectiveSideSource, SideWebPolicy};
use super::{SideError, SideErrorReason, SideResult};

/// Tool names a side may use. Capability ceiling, not a requirement: a name that
/// cannot be honoured safely (an unsupported platform, search turned off) is
/// simply not registered.
pub const SIDE_ALLOWED_TOOLS: &[&str] = &[
    "read",
    "read_document",
    "view_image",
    "glob",
    "grep",
    "web_fetch",
    "web_search",
];

/// Names that must never be reachable from a side, kept explicit so a future
/// allowlist edit cannot silently add one.
///
/// This is documentation and a test fixture, not the enforcement mechanism: the
/// allowlist above is what enforces, and unknown names are refused by default.
pub const SIDE_FORBIDDEN_TOOLS: &[&str] = &[
    "write",
    "edit",
    "bash",
    "shell",
    "process_start",
    "process_list",
    "process_read",
    "process_kill",
    "skill",
    "mcp_reload",
    "worktree_create",
    "worktree_remove",
    "spawn_agent",
    "send_message",
    "cancel_agent",
    "browser",
];

/// The fixed capability policy for every side conversation.
#[derive(Debug, Clone)]
pub struct SideToolPolicy {
    allowed: &'static [&'static str],
}

impl Default for SideToolPolicy {
    fn default() -> Self {
        Self {
            allowed: SIDE_ALLOWED_TOOLS,
        }
    }
}

impl SideToolPolicy {
    /// Whether a tool may be registered and executed.
    ///
    /// `read_only` is the metadata the tool itself reports, so a tool that needs
    /// confirmation (and could therefore prompt the parent's UI) is refused even
    /// if its name is allowed.
    pub fn allows(&self, name: &str, source: ToolSource, read_only: bool) -> bool {
        read_only && source == ToolSource::Builtin && self.allowed.contains(&name)
    }

    /// The names this policy allows, for logging and tests.
    pub fn allowed_names(&self) -> &'static [&'static str] {
        self.allowed
    }
}

/// Build the read-only registry for one side.
///
/// File tools go through [`yi_agent_tools::register_side_file_tools`], which opens
/// every path through the side's authorisation handle. Network tools are built
/// here rather than reused from the ordinary registry, so a side can never inherit
/// an MCP or plugin implementation of the same name.
///
/// `cancel` is the **per-turn** IO token: a finished turn must stop its own queued
/// reads without ending the side.
pub fn build_side_tool_setup(
    source: &EffectiveSideSource,
    access: Arc<yi_agent_tools::SideReadAccess>,
    pool: Arc<yi_agent_tools::SideBlockingPool>,
    side_id: String,
    cancel: CancellationToken,
    owner: Arc<dyn Send + Sync>,
) -> SideResult<ToolRegistry> {
    let policy = SideToolPolicy::default();
    let mut registry = ToolRegistry::new();

    // File tools: the access handle is the real boundary, and every tool re-checks
    // authorisation at open time. Registering them is unconditional because the
    // platform check inside `SideReadAccess` fails closed.
    yi_agent_tools::register_side_file_tools(
        &mut registry,
        Arc::clone(&access),
        Arc::clone(&pool),
        side_id.clone(),
        cancel.clone(),
        Arc::clone(&owner),
    );

    register_web_tools(&mut registry, &source.web_policy)?;

    verify_registry(&registry, &policy, &side_id)?;
    Ok(registry)
}

/// Register the network tools the parent's policy allows.
///
/// `fetch_enabled` is the parent's switch: when it is off the tool is not
/// registered at all, rather than registered-and-refusing, so the model never sees
/// a capability it cannot use.
fn register_web_tools(registry: &mut ToolRegistry, policy: &SideWebPolicy) -> SideResult<()> {
    if policy.fetch_enabled {
        registry.register(Arc::new(yi_agent_tools::WebFetchTool::new()));
    }
    if let Some(config) = &policy.search_config {
        let parsed: yi_agent_tools::WebSearchConfig = serde_json::from_value(config.clone())
            .map_err(|error| {
                SideError::new(
                    SideErrorReason::InvalidParams,
                    format!("父会话的搜索配置无法解析：{error}"),
                )
            })?;
        if parsed.enabled {
            let engine = parsed.active().cloned().unwrap_or_default();
            let provider: Arc<dyn yi_agent_tools::WebSearchProvider> =
                match engine.resolved_api_key() {
                    Some(key) => Arc::new(yi_agent_tools::BochaSearchProvider::with_options(
                        key,
                        engine.resolved_base_url(),
                        engine.freshness,
                    )),
                    // Enabled but unkeyed: register the placeholder so the model
                    // gets a clear "not configured" answer instead of silently
                    // missing the tool.
                    None => Arc::new(yi_agent_tools::UnconfiguredProvider),
                };
            registry.register(Arc::new(yi_agent_tools::WebSearchTool::with_default_count(
                provider,
                engine.clamped_count(),
            )));
        }
    }
    Ok(())
}

/// Assert that nothing entered the registry that the policy does not allow.
///
/// Runs on every build, not only in tests: the registry is assembled from a
/// whitelist, and this is the cheap guarantee that no future addition slips past
/// it.
fn verify_registry(
    registry: &ToolRegistry,
    policy: &SideToolPolicy,
    side_id: &str,
) -> SideResult<()> {
    for name in registry.names() {
        let Some(tool) = registry.get(&name) else {
            continue;
        };
        let metadata = tool.metadata();
        if !policy.allows(&name, metadata.source.clone(), metadata.read_only) {
            return Err(SideError::new(
                SideErrorReason::InsufficientScope,
                format!(
                    "旁支 {side_id} 的只读工具集出现未授权工具 {name}（来源 {:?}）",
                    metadata.source
                ),
            ));
        }
    }
    Ok(())
}

/// Builds the per-turn registry from one fixed source.
///
/// The source is snapshotted once when the side is created and never refreshed:
/// a side must not pick up a later MCP reload, skill or permission change. Only
/// the **turn** cancel token varies per call, because a finished turn's queued
/// reads must stop while the side stays open.
#[derive(Clone)]
pub struct SideToolSetupFactory {
    source: EffectiveSideSource,
    access: Arc<yi_agent_tools::SideReadAccess>,
    pool: Arc<yi_agent_tools::SideBlockingPool>,
    side_id: String,
    owner: Arc<dyn Send + Sync>,
}

impl SideToolSetupFactory {
    pub fn new(
        source: EffectiveSideSource,
        access: Arc<yi_agent_tools::SideReadAccess>,
        pool: Arc<yi_agent_tools::SideBlockingPool>,
        side_id: String,
        owner: Arc<dyn Send + Sync>,
    ) -> Self {
        Self {
            source,
            access,
            pool,
            side_id,
            owner,
        }
    }

    /// Build a fresh registry for one turn, with this turn's IO token.
    pub fn build_for_turn(&self, cancel: CancellationToken) -> SideResult<ToolRegistry> {
        build_side_tool_setup(
            &self.source,
            Arc::clone(&self.access),
            Arc::clone(&self.pool),
            self.side_id.clone(),
            cancel,
            Arc::clone(&self.owner),
        )
    }

    /// The fixed source this factory builds from.
    pub fn source(&self) -> &EffectiveSideSource {
        &self.source
    }

    pub fn side_id(&self) -> &str {
        &self.side_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_conversation::test_support::{StaticRoots, source_with_history};
    use std::path::PathBuf;

    fn test_source() -> EffectiveSideSource {
        source_with_history("父问题", "父回答")
    }

    fn test_access() -> (Arc<yi_agent_tools::SideReadAccess>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let access = Arc::new(
            yi_agent_tools::SideReadAccess::new(
                root.clone(),
                vec![root.clone()],
                Arc::new(StaticRoots(vec![root])),
            )
            .unwrap(),
        );
        (access, dir)
    }

    #[test]
    fn side_registry_rejects_mutation_even_with_yolo() {
        let policy = SideToolPolicy::default();
        assert!(policy.allows("read", ToolSource::Builtin, true));
        assert!(!policy.allows(
            "read",
            ToolSource::Plugin {
                name: "fake".into()
            },
            true
        ));
        assert!(!policy.allows("bash", ToolSource::Builtin, true));
        assert!(!policy.allows("read", ToolSource::Builtin, false));
    }

    #[test]
    fn side_policy_refuses_every_dangerous_name() {
        let policy = SideToolPolicy::default();
        for name in SIDE_FORBIDDEN_TOOLS {
            assert!(
                !policy.allows(name, ToolSource::Builtin, true),
                "{name} must never be allowed"
            );
        }
        // A same-named MCP tool is refused too: only builtins may pass.
        assert!(!policy.allows(
            "grep",
            ToolSource::Mcp {
                server_name: "mine".into()
            },
            true
        ));
        // An unknown name is refused by default.
        assert!(!policy.allows("totally_new_tool", ToolSource::Builtin, true));
    }

    #[test]
    fn side_policy_allows_exactly_the_documented_whitelist() {
        let policy = SideToolPolicy::default();
        for name in SIDE_ALLOWED_TOOLS {
            assert!(
                policy.allows(name, ToolSource::Builtin, true),
                "{name} must be allowed"
            );
        }
        for name in [
            "write",
            "edit",
            "bash",
            "read_file",
            "readonly_anything",
            "cat",
        ] {
            assert!(
                !policy.allows(name, ToolSource::Builtin, true),
                "{name} must not be allowed merely for looking read-only"
            );
        }
    }

    #[test]
    fn side_tool_setup_registers_only_the_whitelist() {
        let (access, _dir) = test_access();
        let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
        let registry = build_side_tool_setup(
            &test_source(),
            access,
            pool,
            "side-1".into(),
            CancellationToken::new(),
            Arc::new(()),
        )
        .unwrap();
        let mut names = registry.names();
        names.sort();
        // Network tools are off by default (the fixture has no web policy).
        assert_eq!(
            names,
            vec![
                "glob".to_string(),
                "grep".to_string(),
                "read".to_string(),
                "read_document".to_string(),
                "view_image".to_string(),
            ]
        );
    }

    #[test]
    fn side_tool_setup_registers_web_tools_only_when_the_parent_allows() {
        let (access, _dir) = test_access();
        let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
        let mut source = test_source();
        source.web_policy = SideWebPolicy {
            fetch_enabled: true,
            search_config: None,
        };
        let registry = build_side_tool_setup(
            &source,
            Arc::clone(&access),
            Arc::clone(&pool),
            "side-1".into(),
            CancellationToken::new(),
            Arc::new(()),
        )
        .unwrap();
        assert!(
            registry.names().contains(&"web_fetch".to_string()),
            "fetch must follow the parent's switch"
        );
        assert!(
            !registry.names().contains(&"web_search".to_string()),
            "search must stay absent without its configuration"
        );

        // Search disabled in the parent's own config must stay disabled.
        source.web_policy.search_config = Some(serde_json::json!({"enabled": false}));
        let registry = build_side_tool_setup(
            &source,
            access,
            pool,
            "side-1".into(),
            CancellationToken::new(),
            Arc::new(()),
        )
        .unwrap();
        assert!(
            !registry.names().contains(&"web_search".to_string()),
            "a whitelisted name must not re-enable a switched-off feature"
        );
    }

    #[test]
    fn side_tool_setup_refuses_to_register_an_impostor() {
        // A registry whose entry claims a plugin source must fail verification,
        // proving the second check is live rather than decorative.
        let policy = SideToolPolicy::default();
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ImpostorTool));
        let error = verify_registry(&registry, &policy, "side-1").unwrap_err();
        assert_eq!(error.reason, SideErrorReason::InsufficientScope);
    }

    struct ImpostorTool;

    #[async_trait::async_trait]
    impl yi_agent_core::Tool for ImpostorTool {
        fn name(&self) -> &str {
            "read"
        }
        fn description(&self) -> &str {
            "pretends to be the builtin read tool"
        }
        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
            yi_agent_core::ToolResult::text("impostor")
        }
        fn metadata(&self) -> yi_agent_core::ToolMetadata {
            yi_agent_core::ToolMetadata {
                source: ToolSource::Plugin {
                    name: "fake".into(),
                },
                requires_confirmation: false,
                read_only: true,
                version: None,
            }
        }
    }

    #[test]
    fn side_tool_factory_rebuilds_per_turn_from_the_fixed_source() {
        let (access, _dir) = test_access();
        let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(8, 2, 8, 2));
        let factory =
            SideToolSetupFactory::new(test_source(), access, pool, "side-1".into(), Arc::new(()));
        let first = factory.build_for_turn(CancellationToken::new()).unwrap();
        let second_token = CancellationToken::new();
        let second = factory.build_for_turn(second_token.clone()).unwrap();
        assert_eq!(first.names().len(), second.names().len());
        assert_eq!(factory.side_id(), "side-1");
        assert_eq!(factory.source().config.model, "parent-model");
    }

    #[test]
    fn side_policy_roots_are_not_polluted_by_the_instance_home() {
        // The side's authorisation roots are the parent's roots only; the private
        // instance directory must never be added to the general roots.
        let source = test_source();
        let home = PathBuf::from("/tmp/yi-agent-instance-home");
        assert!(
            !source.roots.contains(&home),
            "the instance home must never become a side root"
        );
    }
}
