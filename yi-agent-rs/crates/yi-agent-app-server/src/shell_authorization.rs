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
}

impl ShellAuthorization {
    /// A well-formed authorization, as the bridge writes it.
    pub fn from_shell(client_run_id: impl Into<String>) -> Self {
        Self {
            client_run_id: client_run_id.into(),
            scope_key: None,
            daemon_instance_id: None,
            issued_by_shell: true,
        }
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
    }

    /// Before the user opens a project the App is still one identifiable client;
    /// refusing that would leave the sidecar with no owner at all.
    #[test]
    fn an_authorization_without_a_scope_still_names_the_client() {
        let authorization = ShellAuthorization::from_shell("app-run-1");
        assert!(authorization.accept(true).is_ok());
        assert!(authorization.scope_key.is_none());
    }

    /// No credential may cross the pipe: what the shell sends is identity, and a
    /// credential in this struct would end up in argv-adjacent logs and in tool
    /// environments.
    #[test]
    fn nothing_secret_is_serialized() {
        let json = serde_json::to_string(&good()).unwrap();
        assert!(!json.contains("credential"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("token"));
    }
}
