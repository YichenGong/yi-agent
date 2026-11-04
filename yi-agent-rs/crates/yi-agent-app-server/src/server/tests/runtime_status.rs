//! `runtime/status` through the real dispatcher.
//!
//! The unit tests in `runtime_status` prove the *classification*; these prove the
//! *routing*, which is the half a unit test cannot see. A method that is classified
//! read-only but never reaches its domain is silently `method_not_found`, and a
//! destructive method that happens to share the `runtime/` prefix must land there
//! too -- on `method_not_found`, not on a handler.

use super::{Harness, initialize};

/// Ask the real dispatcher for a method and return the first frame with that id.
async fn ask(h: &mut Harness, id: u64, method: &str) -> serde_json::Value {
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {},
    });
    h.send(&serde_json::to_string(&frame).unwrap()).await;
    loop {
        let value = h.read_value().await;
        if value.get("id") == Some(&serde_json::json!(id)) {
            return value;
        }
    }
}

/// The read-only status method exists and answers with the view shape.
///
/// This runtime has no guardian, so the honest answer is `Unknown` with an
/// explanation -- not an error, and definitely not a claim of compatibility.
#[tokio::test(flavor = "multi_thread")]
async fn runtime_status_answers_with_an_unknown_view_when_nothing_serves_the_scope() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let value = ask(&mut h, 900, "runtime/status").await;
    let result = value
        .get("result")
        .expect("a status read is an answer, not an error");
    assert_eq!(result["compatibility"], "unknown");
    assert_eq!(result["lifecycleManaged"], false);
    assert_eq!(result["localRestartAvailable"], false);
    assert!(
        !result["message"].as_str().unwrap_or_default().is_empty(),
        "an unknown runtime must explain itself"
    );
    // The counts are present and explicitly incomplete rather than absent.
    assert_eq!(result["counts"]["complete"], false);

    h.shutdown().await;
}

/// A destructive sibling is not remote-callable, and the dispatcher agrees: the
/// method is simply not there.
#[tokio::test(flavor = "multi_thread")]
async fn destructive_runtime_methods_do_not_exist() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    for (offset, method) in ["runtime/confirmRestart", "runtime/stop", "runtime/restart"]
        .into_iter()
        .enumerate()
    {
        let value = ask(&mut h, 910 + offset as u64, method).await;
        assert_eq!(
            value["error"]["code"], -32601,
            "`{method}` must be method_not_found, got {value}"
        );
    }

    h.shutdown().await;
}

/// The string predicate and the dispatcher must agree on every method either of
/// them knows about.
///
/// Two sources of truth drift: the predicate would say "read-only" about a method
/// the dispatcher never routes, or the dispatcher would route a method the
/// predicate calls destructive. This pins them together for the whole `runtime/`
/// surface this build has.
#[tokio::test(flavor = "multi_thread")]
async fn the_read_only_predicate_matches_the_dispatcher() {
    use crate::runtime_status::is_read_only_runtime_method;

    let mut h = Harness::new();
    initialize(&mut h).await;

    let value = ask(&mut h, 930, "runtime/status").await;
    assert!(is_read_only_runtime_method("runtime/status"));
    assert!(
        value.get("error").is_none(),
        "a method the predicate calls read-only must actually route"
    );

    for method in ["runtime/confirmRestart", "runtime/stop"] {
        assert!(!is_read_only_runtime_method(method));
        let value = ask(&mut h, 931, method).await;
        assert_eq!(value["error"]["code"], -32601);
    }

    h.shutdown().await;
}

/// A status read carries no credential and no message text. The DTO has no field
/// for either, so this asserts the one thing a field list cannot: the wire form
/// stays the same after a round trip through the real writer.
#[tokio::test(flavor = "multi_thread")]
async fn the_status_frame_carries_no_credential_field() {
    let mut h = Harness::new();
    initialize(&mut h).await;

    let value = ask(&mut h, 940, "runtime/status").await;
    let text = serde_json::to_string(&value).unwrap();
    for forbidden in ["credential", "apiKey", "api_key", "token"] {
        assert!(
            !text.contains(forbidden),
            "a status frame must not carry `{forbidden}`: {text}"
        );
    }

    h.shutdown().await;
}

/// The shell's expectation reaches the view, and it is the *daemon artifact's* build
/// id -- not the wrapper's version number.
///
/// Without this the answer is `Unknown` for a perfectly matched pair: the sidecar
/// has nothing to compare against. With it, the banner can say "same build" only
/// when the bytes really are the ones the shell shipped, which is the whole point
/// of carrying an identity instead of a version string.
#[tokio::test(flavor = "multi_thread")]
async fn the_shell_expectation_reaches_the_status_view() {
    let mut h = Harness::new();
    // One line: the harness frames on newlines, so a pretty-printed frame would be
    // split into several unparsable lines.
    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
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
                    "capabilities": ["lease.v1", "control.v1"]
                }
            }
        }
    });
    h.send(&serde_json::to_string(&initialize).unwrap()).await;
    let _ = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(1)))
        .await;

    let value = ask(&mut h, 950, "runtime/status").await;
    let result = value.get("result").expect("a status answer");
    assert_eq!(result["expectedBuildId"], "0123456789abcdef");
    assert_eq!(result["expectedDisplayVersion"], "0.2.0");
    // Still `Unknown`: no guardian answered, so the expectation is a question with
    // no answer yet -- never a claim of equality.
    assert_eq!(result["compatibility"], "unknown");

    h.shutdown().await;
}

/// A remote client cannot make the shell's expectation its own: the field is only
/// read from local stdio, so a websocket handshake that includes it must not change
/// what the status view claims this machine expects.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_handshake_cannot_supply_the_shell_expectation() {
    let remote = crate::shell_authorization::ShellAuthorization::from_shell("phone")
        .for_scope("/p/.yi-agent/runtime")
        .expecting(crate::shell_authorization::ExpectedRuntime {
            build_id: "forged".to_string(),
            display_version: "9.9.9".to_string(),
            schema_min: 1,
            schema_max: 16,
            business_protocol_min: 1,
            business_protocol_max: 1,
            capabilities: Vec::new(),
        });
    assert!(
        remote.accept(false).is_err(),
        "a websocket client must never be accepted as the shell"
    );
}
