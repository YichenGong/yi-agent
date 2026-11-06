//! 构建期注入的版本标识。

pub const VERSION: &str = env!("YI_AGENT_VERSION");
pub const BUILD_KIND: &str = env!("YI_AGENT_BUILD_KIND");

/// clap 需要 `&'static str`；`env!` 正是静态值。
pub const CLAP_VERSION: &str = VERSION;

/// The daemon executable's artifact identity, embedded at build time.
///
/// This is what a client compares against the bundled sidecar manifest. It is
/// deliberately not [`VERSION`]: a wrapper point release can ship identical
/// sidecar bytes, and a rebuild at an unchanged git SHA can differ.
pub const DAEMON_BUILD_ID: &str = env!("YI_AGENT_DAEMON_BUILD_ID");

/// The identity as a wire DTO, resolved against a concrete executable path at
/// runtime. The path is supplied by the caller because the artifact may live in
/// the app bundle, in a controlled cache copy, or in `target/debug`.
pub fn artifact_identity(
    executable: impl Into<std::path::PathBuf>,
) -> yi_agent_lifecycle::types::ArtifactIdentity {
    yi_agent_lifecycle::types::ArtifactIdentity {
        build_id: DAEMON_BUILD_ID.to_string(),
        display_version: VERSION.to_string(),
        executable: executable.into(),
        // Filled in by whoever can actually hash the file: the build script must
        // not bake a digest that changes with every re-sign.
        sha256: String::new(),
        schema_min: yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MIN,
        schema_max: yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MAX,
        handover_version: 1,
        business_protocol_min: yi_agent_store::ipc::PROTOCOL_VERSION,
        business_protocol_max: yi_agent_store::ipc::PROTOCOL_VERSION,
        capabilities: runtime_capabilities(),
    }
}

/// Capabilities this build offers beyond the baseline protocol. Kept as a plain
/// sorted list so two builds' capability sets can be intersected.
pub fn runtime_capabilities() -> Vec<String> {
    let mut capabilities = vec![
        "control.v1".to_string(),
        "lease.v1".to_string(),
        "switch.preview.v1".to_string(),
        // Static, not conditional: this is what the *build* can do. Whether this
        // particular runtime may act on it is decided where the identity is assembled
        // (`runtime_identity`), because advertising it from the artifact alone would
        // let a client register for a guarantee this runtime's bridge has not
        // established.
        yi_agent_lifecycle::foreground::FOREGROUND_ROOT_CAPABILITY.to_string(),
        // The fenced-write contract. Every host in this build starts its daemon
        // with `enforce_session_turns`, so the promise this names is already kept
        // before it is made -- advertising it from the artifact alone is safe for
        // the same reason the foreground one is listed here: the capability says
        // what the build can do, not what a particular runtime is configured for.
        yi_agent_lifecycle::session_delegation::SESSION_DELEGATION_CAPABILITY.to_string(),
    ];
    capabilities.sort();
    capabilities
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn the_build_kind_is_one_of_three() {
        assert!(
            matches!(BUILD_KIND, "release" | "dev" | "nogit"),
            "unexpected build kind: {BUILD_KIND}"
        );
    }

    #[test]
    fn only_a_release_build_reports_a_bare_version() {
        if BUILD_KIND == "release" {
            assert!(
                !VERSION.contains("-dev") && !VERSION.contains("+nogit"),
                "release build must not carry a dev marker: {VERSION}"
            );
            assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        }
    }

    #[test]
    fn the_daemon_build_id_is_a_hex_digest() {
        assert_eq!(DAEMON_BUILD_ID.len(), 32, "{DAEMON_BUILD_ID}");
        assert!(DAEMON_BUILD_ID.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn the_daemon_build_id_is_not_the_wrapper_version() {
        assert_ne!(DAEMON_BUILD_ID, VERSION);
    }

    #[test]
    fn the_identity_carries_the_build_id_and_an_empty_digest() {
        let identity = artifact_identity("/tmp/yi-agent");
        assert_eq!(identity.build_id, DAEMON_BUILD_ID);
        assert_eq!(identity.display_version, VERSION);
        assert!(
            identity.sha256.is_empty(),
            "the build script must not bake a digest that a re-sign invalidates"
        );
    }

    #[test]
    fn the_schema_range_comes_from_the_migration_contract() {
        // Hardcoding 15 here is what would silently pass after a new migration.
        let identity = artifact_identity("/tmp/yi-agent");
        assert_eq!(
            identity.schema_max,
            yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MAX
        );
        assert_eq!(
            identity.schema_min,
            yi_agent_store::schema_compat::SUPPORTED_SCHEMA_MIN
        );
    }

    /// The build must name the fenced-write contract it actually enforces.
    ///
    /// A client that checks this advertisement before offering delegation would
    /// otherwise be told "not available" by a daemon that would honour every fenced
    /// write it sent.
    #[test]
    fn the_session_delegation_capability_is_advertised() {
        assert!(
            runtime_capabilities().iter().any(|capability| capability
                == yi_agent_lifecycle::session_delegation::SESSION_DELEGATION_CAPABILITY),
            "a build that fences every container write must advertise that contract"
        );
    }

    #[test]
    fn the_capability_list_is_sorted_and_has_no_duplicates() {
        let capabilities = runtime_capabilities();
        let mut sorted = capabilities.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(capabilities, sorted);
        assert!(capabilities.iter().any(|c| c == "control.v1"));
    }
}
