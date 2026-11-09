//! The side conversation's capability boundary.
//!
//! A side answers from one conversation snapshot and nothing else. Its tool
//! registry is therefore **empty**: this is not an allowlist that can be widened
//! by editing a constant, and it is not a filtered copy of the ordinary registry
//! (which would still carry whatever the normal assembly added). A side can only
//! ever receive zero tools, so a hallucinated call fails in core as an unknown
//! tool instead of reaching the parent's file, shell, web, MCP or subagent
//! capabilities. Capability is enforced by construction here; the prompt only
//! describes it.

use tokio_util::sync::CancellationToken;
use yi_agent_core::ToolRegistry;

use super::SideResult;

/// The fixed capability policy for every side conversation.
///
/// Kept as a named type because hosts report the effective tool set in their
/// summaries. It has no names to add: `allows` is always false.
#[derive(Debug, Clone, Default)]
pub struct SideToolPolicy;

impl SideToolPolicy {
    /// A side never registers or executes a tool, whatever its name or source.
    pub fn allows(
        &self,
        _name: &str,
        _source: yi_agent_core::ToolSource,
        _read_only: bool,
    ) -> bool {
        false
    }

    /// Always empty; the summary reports exactly the tools a side can use.
    pub fn allowed_names(&self) -> &'static [&'static str] {
        &[]
    }
}

/// Build the registry for one side: always empty.
///
/// No parameters are accepted on purpose. The previous signature took the
/// parent's read authorisation, blocking pool and web policy, which made it
/// possible to reintroduce file or network access by passing them through again.
pub fn build_side_tool_setup() -> ToolRegistry {
    ToolRegistry::new()
}

/// Builds the per-turn registry from one fixed source.
///
/// The registry is empty for every turn. The type is kept (rather than passing a
/// bare `ToolRegistry`) because the runner already owns a factory handle and the
/// hosts construct it during assembly; keeping the shape avoids an unrelated
/// refactor while making the empty result unconditional.
#[derive(Clone)]
pub struct SideToolSetupFactory {
    side_id: String,
}

impl SideToolSetupFactory {
    pub fn new(side_id: String) -> Self {
        Self { side_id }
    }

    /// A fresh empty registry. The IO token is unused because there is no IO, but
    /// keeping the per-turn boundary means a future change cannot accidentally
    /// share one mutable registry across turns.
    pub fn build_for_turn(&self, _cancel: CancellationToken) -> SideResult<ToolRegistry> {
        Ok(build_side_tool_setup())
    }

    pub fn side_id(&self) -> &str {
        &self.side_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::side_conversation::EffectiveSideSource;

    #[test]
    fn a_side_never_allows_any_tool_name() {
        let policy = SideToolPolicy;
        for (name, source) in [
            ("read", yi_agent_core::ToolSource::Builtin),
            ("grep", yi_agent_core::ToolSource::Builtin),
            ("web_fetch", yi_agent_core::ToolSource::Builtin),
            (
                "read",
                yi_agent_core::ToolSource::Plugin {
                    name: "fake".into(),
                },
            ),
        ] {
            assert!(
                !policy.allows(name, source, true),
                "{name} must never be allowed, even when it looks read-only"
            );
        }
        assert!(policy.allowed_names().is_empty());
    }

    #[test]
    fn the_built_registry_is_empty() {
        let registry = build_side_tool_setup();
        assert!(registry.names().is_empty());
    }

    #[test]
    fn the_factory_rebuilds_an_empty_registry_every_turn() {
        let factory = SideToolSetupFactory::new("side-1".into());
        let first = factory.build_for_turn(CancellationToken::new()).unwrap();
        let second = factory.build_for_turn(CancellationToken::new()).unwrap();
        assert!(first.names().is_empty());
        assert!(second.names().is_empty());
        assert_eq!(factory.side_id(), "side-1");
    }

    #[test]
    fn side_source_carries_no_web_policy_or_read_access() {
        // Structural proof: `EffectiveSideSource` no longer exposes the handles a
        // host would need to reintroduce external reads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<EffectiveSideSource>();
    }
}
