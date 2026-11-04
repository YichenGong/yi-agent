//! The narrow statement of who the local desktop session speaks for.
//!
//! The App's lease belongs to the desktop shell, not to this process: an
//! `app-server` restart must not change the App's identity, and an orphaned
//! sidecar must not be able to renew on the App's behalf. So the business
//! authority a session may use is not minted here — it is *received* from the
//! shell, once, on the local stdio `initialize` frame.
//!
//! Three properties make that safe, and all three are enforced here:
//!
//! * **Local stdio only.** The authorization is read from a field only the
//!   bridge can populate, and only for the stdio client. A websocket or relay
//!   client sending the same field is ignored, never forwarded.
//! * **Non-renewable.** What crosses the pipe is scope, generation and client
//!   identity. No credential crosses it, so a tool that inherits the process
//!   environment inherits nothing it could renew with.
//! * **Answerable without trusting it.** The value is only used to *name* the
//!   client; the guardian still authenticates the actual lease.

use serde::{Deserialize, Serialize};

/// What the desktop shell vouches for, for the whole life of one sidecar.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellAuthorization {
    /// The App run this session belongs to. Stable across sidecar restarts.
    pub client_run_id: String,
    /// The canonical scope this session was opened for, when the App had already
    /// picked one.
    ///
    /// Optional on purpose: the sidecar starts before the user has opened any
    /// project, so at handshake time there is usually no scope to name. An
    /// absent value means "this App run has not chosen a project yet", not
    /// "no authority".
    pub scope_key: Option<String>,
    /// The daemon generation the shell observed when it wrote this.
    ///
    /// A mismatch is not fatal — the guardian re-checks the live lease either
    /// way — but it lets a session refuse work that belongs to a runtime the
    /// shell is no longer attached to.
    pub daemon_instance_id: Option<String>,
    /// Proof the field came from the bridge rather than a business client.
    ///
    /// The sidecar is the only process reading this end of the pipe, so the
    /// token is not a secret from an attacker who already runs code here; it
    /// exists to make "this key was set by the bridge" a fact the initialize
    /// handler can check instead of inferring it from client type alone.
    pub issued_by_shell: bool,
    /// The runtime artifact this shell shipped with, when it could verify one.
    ///
    /// This is what makes `runtime/status` able to answer "is the running runtime
    /// the build I expect?" without asking the WebView and without falling back to
    /// the wrapper's version number. It is identity, not authority: it names bytes,
    /// cannot be renewed, and grants nothing -- a session that lies about it only
    /// gets a wrong banner about its own project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_runtime: Option<ExpectedRuntime>,
    /// Authority to attach one project root, minted by the guardian for this App
    /// run.
    ///
    /// Per project, not per session: the guardian signs a capability for a root in
    /// a generation, and the App may have several projects open. Each entry is
    /// exactly one such capability, and the daemon accepts an attach only when the
    /// key it presents resolves to a binding its own guardian pushed.
    ///
    /// This is the piece the plan calls for at "the shell passes a non-renewable,
    /// root- and generation-bound authorization on the existing local stdio
    /// `initialize`". It is not a credential: it cannot register, renew, or spawn,
    /// and an orphaned sidecar that kept it could not extend the App's lease.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub root_authorizations: Vec<RootAuthorization>,
}

/// One root's authority, as the sidecar receives it.
///
/// A local mirror of `yi_agent_lifecycle::types::RootAuthorization`, for the same
/// reason [`ExpectedRuntime`] is a mirror: the app-server must not export lifecycle
/// wire types to TypeScript, and this value never crosses that boundary. It is read
/// on the handshake and handed to the attach path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthorization {
    pub scope_key: String,
    pub daemon_instance_id: String,
    pub client_run_id: String,
    pub root_id: String,
    pub capability: String,
}

/// The artifact identity the shell was built against, as the sidecar needs it.
///
/// A local mirror of `yi_agent_lifecycle::types::ArtifactIdentity` reduced to what
/// a comparison actually uses. The app-server could not reuse that struct directly
/// without exporting it to TypeScript, and this one never crosses that boundary: it
/// is read on the handshake and thrown away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedRuntime {
    /// The daemon artifact's build id. Never the wrapper's version.
    pub build_id: String,
    /// The user-facing version that goes with `build_id`.
    pub display_version: String,
    pub schema_min: u32,
    pub schema_max: u32,
    pub business_protocol_min: u32,
    pub business_protocol_max: u32,
    #[serde(default)]
    pub capabilities: Vec<String>,
}

impl ShellAuthorization {
    /// A well-formed authorization, as the bridge writes it.
    pub fn from_shell(client_run_id: impl Into<String>) -> Self {
        Self {
            client_run_id: client_run_id.into(),
            scope_key: None,
            daemon_instance_id: None,
            issued_by_shell: true,
            expected_runtime: None,
            root_authorizations: Vec::new(),
        }
    }

    /// The capability this session may attach `root` with, when the shell sent one.
    ///
    /// Matched on the canonical project path because that is what the guardian
    /// signed: a capability for `/p` must not attach `/q`, and comparing here means
    /// the attach path does not have to re-derive the rule.
    pub fn capability_for_root(&self, root: &std::path::Path) -> Option<&str> {
        let wanted = canonical_or_self(root);
        self.root_authorizations
            .iter()
            .find(|authorization| {
                canonical_or_self(std::path::Path::new(&authorization.root_id)) == wanted
            })
            .map(|authorization| authorization.capability.as_str())
    }

    /// The same authorization, carrying the artifact this shell shipped with.
    pub fn expecting(mut self, expected: ExpectedRuntime) -> Self {
        self.expected_runtime = Some(expected);
        self
    }

    /// The same authorization, narrowed to one project.
    pub fn for_scope(mut self, scope_key: impl Into<String>) -> Self {
        self.scope_key = Some(scope_key.into());
        self
    }

    /// Rejects anything a business client could have sent.
    ///
    /// `is_local_stdio` is the load-bearing argument: over a websocket the same
    /// JSON is a claim by a remote device, not a statement by the shell.
    pub fn accept(&self, is_local_stdio: bool) -> Result<&Self, String> {
        if !is_local_stdio {
            return Err(
                "shell authorization is only accepted from the local stdio client".to_string(),
            );
        }
        if !self.issued_by_shell {
            return Err("shell authorization was not issued by the desktop bridge".to_string());
        }
        if self.client_run_id.trim().is_empty() {
            return Err("shell authorization has no client run id".to_string());
        }
        if self
            .scope_key
            .as_deref()
            .is_some_and(|key| key.trim().is_empty())
        {
            return Err("shell authorization has an empty scope key".to_string());
        }
        // A capability that names nothing cannot be matched to a root, and accepting
        // it would leave the attach path silently unauthenticated. Refusing the whole
        // field is right: a half-broken authorization is worse than none, because the
        // session would look authorized and fail at the first attach.
        for authorization in &self.root_authorizations {
            if authorization.root_id.trim().is_empty() {
                return Err("a root authorization names no root".to_string());
            }
            if authorization.capability.trim().is_empty() {
                return Err("a root authorization carries no capability".to_string());
            }
            if authorization.client_run_id != self.client_run_id {
                return Err(format!(
                    "a root authorization is for client {}, not {}",
                    authorization.client_run_id, self.client_run_id
                ));
            }
        }
        // An expectation with an empty build id would classify every runtime as a
        // mismatch; better to have no opinion than a wrong one.
        if self
            .expected_runtime
            .as_ref()
            .is_some_and(|expected| expected.build_id.trim().is_empty())
        {
            return Err("shell authorization has an empty expected build id".to_string());
        }
        Ok(self)
    }
}

/// Reads the authorization out of `initialize` params.
///
/// `None` is the normal answer for every non-desktop client (and for the CLI):
/// the field is simply absent. A *malformed* field is also `None` rather than an
/// error, because a bad hint must not turn a working handshake into a failed
/// one — the session then behaves exactly like one with no shell behind it.
pub fn from_initialize_params(params: &serde_json::Value) -> Option<ShellAuthorization> {
    let value = params.get("shellAuthorization")?;
    serde_json::from_value(value.clone()).ok()
}

/// Resolves a project path for comparison, falling back to the raw path.
///
/// The shell records the project as the user picked it and the guardian signs that
/// same string, so usually the comparison is textual. Canonicalising both sides
/// covers the case where one went through a symlink: silently treating two names of
/// one directory as different roots would refuse an attach that should work.
fn canonical_or_self(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> ShellAuthorization {
        ShellAuthorization::from_shell("app-run-1").for_scope("/p/.yi-agent/runtime")
    }

    #[test]
    fn the_local_stdio_client_may_carry_an_authorization() {
        assert!(good().accept(true).is_ok());
    }

    /// The whole point: a remote device sending the same JSON must not be
    /// believed. Otherwise "who owns this work" would be self-declared over the
    /// network.
    #[test]
    fn a_websocket_client_cannot_claim_to_be_the_shell() {
        let error = good().accept(false).unwrap_err();
        assert!(error.contains("local stdio"), "{error}");
    }

    #[test]
    fn an_authorization_not_issued_by_the_bridge_is_refused() {
        let mut forged = good();
        forged.issued_by_shell = false;
        assert!(forged.accept(true).is_err());
    }

    #[test]
    fn empty_identity_fields_are_refused() {
        let mut empty_id = good();
        empty_id.client_run_id = "  ".to_string();
        assert!(empty_id.accept(true).is_err());

        let mut empty_scope = good();
        empty_scope.scope_key = Some(String::new());
        assert!(empty_scope.accept(true).is_err());
    }

    #[test]
    fn a_missing_field_is_absent_not_an_error() {
        let params = serde_json::json!({});
        assert!(from_initialize_params(&params).is_none());
    }

    #[test]
    fn a_malformed_field_is_ignored_rather_than_failing_the_handshake() {
        let params = serde_json::json!({ "shellAuthorization": "not an object" });
        assert!(from_initialize_params(&params).is_none());
    }

    #[test]
    fn the_round_trip_preserves_every_field() {
        let params = serde_json::json!({
            "shellAuthorization": {
                "client_run_id": "app-run-1",
                "scope_key": "/p/.yi-agent/runtime",
                "daemon_instance_id": "daemon-7",
                "issued_by_shell": true,
            }
        });
        let parsed = from_initialize_params(&params).expect("a valid authorization");
        assert_eq!(parsed.client_run_id, "app-run-1");
        assert_eq!(parsed.scope_key.as_deref(), Some("/p/.yi-agent/runtime"));
        assert_eq!(parsed.daemon_instance_id.as_deref(), Some("daemon-7"));
        assert!(parsed.issued_by_shell);
        assert!(parsed.expected_runtime.is_none());
    }

    /// Before the user opens a project the App is still one identifiable client;
    /// refusing that would leave the sidecar with no owner at all.
    #[test]
    fn an_authorization_without_a_scope_still_names_the_client() {
        let authorization = ShellAuthorization::from_shell("app-run-1");
        assert!(authorization.accept(true).is_ok());
        assert!(authorization.scope_key.is_none());
    }

    /// The expectation travels without a credential and survives the handshake.
    ///
    /// This is the value that lets the status view say "same build" instead of
    /// `Unknown`, so it must round-trip exactly -- but it is not authority, and the
    /// secret test below still applies to it.
    #[test]
    fn the_expected_artifact_round_trips() {
        let params = serde_json::json!({
            "shellAuthorization": {
                "client_run_id": "app-run-1",
                "issued_by_shell": true,
                "expected_runtime": {
                    "build_id": "0123456789abcdef",
                    "display_version": "0.2.0",
                    "schema_min": 1,
                    "schema_max": 16,
                    "business_protocol_min": 1,
                    "business_protocol_max": 1,
                    "capabilities": ["lease.v1", "control.v1"],
                }
            }
        });
        let parsed = from_initialize_params(&params).expect("a valid authorization");
        let expected = parsed
            .expected_runtime
            .clone()
            .expect("the expectation must survive");
        assert_eq!(expected.build_id, "0123456789abcdef");
        assert_eq!(expected.display_version, "0.2.0");
        assert_eq!(expected.schema_max, 16);
        assert_eq!(expected.capabilities.len(), 2);

        let again = serde_json::to_value(&parsed).unwrap();
        assert_eq!(again["expected_runtime"]["build_id"], "0123456789abcdef");
    }

    /// An expectation the shell cannot substantiate is worse than none: it would
    /// label every runtime a mismatch. The empty id is refused at accept time.
    #[test]
    fn an_empty_expected_build_id_is_refused() {
        let mut authorization = good();
        authorization.expected_runtime = Some(ExpectedRuntime {
            build_id: "  ".to_string(),
            display_version: "0.2.0".to_string(),
            schema_min: 1,
            schema_max: 16,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        });
        assert!(authorization.accept(true).is_err());
    }

    /// A capability the shell minted is forwarded to the attach path for its root.
    #[test]
    fn a_root_capability_is_matched_to_its_project() {
        let root = std::env::temp_dir();
        let mut authorization = good();
        authorization.root_authorizations.push(RootAuthorization {
            scope_key: "/p/.yi-agent/runtime".to_string(),
            daemon_instance_id: "daemon-7".to_string(),
            client_run_id: "app-run-1".to_string(),
            root_id: root.to_string_lossy().into_owned(),
            capability: "rootcap1:abc".to_string(),
        });
        authorization.accept(true).unwrap();
        assert_eq!(
            authorization.capability_for_root(&root),
            Some("rootcap1:abc")
        );
        // A different project must not borrow this one's authority.
        assert_eq!(
            authorization.capability_for_root(std::path::Path::new("/definitely/not/this")),
            None
        );
    }

    /// A capability for a *different* client is a contradiction: the shell speaks
    /// for one run, and accepting both would let two App runs' authority mix.
    #[test]
    fn a_capability_for_another_client_is_refused() {
        let mut authorization = good();
        authorization.root_authorizations.push(RootAuthorization {
            scope_key: "/p/.yi-agent/runtime".to_string(),
            daemon_instance_id: "daemon-7".to_string(),
            client_run_id: "someone-else".to_string(),
            root_id: "/p".to_string(),
            capability: "rootcap1:abc".to_string(),
        });
        let error = authorization.accept(true).unwrap_err();
        assert!(error.contains("not app-run-1"), "{error}");
    }

    #[test]
    fn an_empty_capability_is_refused() {
        let mut authorization = good();
        authorization.root_authorizations.push(RootAuthorization {
            scope_key: "/p/.yi-agent/runtime".to_string(),
            daemon_instance_id: "daemon-7".to_string(),
            client_run_id: "app-run-1".to_string(),
            root_id: "/p".to_string(),
            capability: "  ".to_string(),
        });
        assert!(authorization.accept(true).is_err());
    }

    /// No credential may cross the pipe: what the shell sends is identity, and a
    /// credential in this struct would end up in argv-adjacent logs and in tool
    /// environments.
    #[test]
    fn nothing_secret_is_serialized() {
        let with_expectation = good().expecting(ExpectedRuntime {
            build_id: "0123456789abcdef".to_string(),
            display_version: "0.2.0".to_string(),
            schema_min: 1,
            schema_max: 16,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: vec!["lease.v1".to_string()],
        });
        let json = serde_json::to_string(&with_expectation).unwrap();
        assert!(!json.contains("credential"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }
}
