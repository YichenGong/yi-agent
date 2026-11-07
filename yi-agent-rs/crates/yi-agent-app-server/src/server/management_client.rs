//! The sidecar's outbound half of the private management channel.
//!
//! The prepare path is a request *from* the shell, so it lives in
//! [`super::local_management`] next to the authentication. Authorization runs the
//! other way: the sidecar holds no lease, so it must *ask* the shell, and the answer
//! comes back on the same stdio it is already reading. That direction needs three
//! things that did not exist before, and each one is a rule rather than plumbing:
//!
//! 1. **A request corridor that is not the business write path.** A `_shell/` frame
//!    carries a conversation's intent and the reply carries a capability. Putting it
//!    through `hub.reply` would make it a broadcast frame -- visible to the WebView,
//!    answerable by the WebView.
//! 2. **A pending table keyed by a string id.** Business responses are routed by
//!    numeric `id`; both may be in flight at once, and a late management response must
//!    never complete a business request.
//! 3. **A deadline that does not resend.** Authorization asks the guardian to bind a
//!    root. A retry is a second bind attempt, which the guardian refuses by design
//!    once the first is in flight; the honest answer to "no reply" is the timeout.
//!
//! The response itself is delivered by a *different task* than the one waiting on it.
//! That is not an implementation detail: the main dispatch loop is a single task, and a
//! caller that awaited its own reply inline would be waiting for a frame only it could
//! have read.

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Value;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::production::{RootAuthorityEntry, RootIntentRequest};
use yi_agent_subagent::managed_binding::ManagedRootAuthority;

use super::local_management::AUTHORIZE_ROOT_METHOD;

/// Writes one frame to the sidecar's protocol output.
///
/// The same seam `PrepareTransport` uses on the shell side. Kept separate so the
/// pending table can be tested without a pipe. Async because production writes through
/// the broadcaster, whose send is async; a sync seam would have to block a worker to
/// use it.
#[async_trait::async_trait]
pub trait ManagementWrite: Send + Sync + 'static {
    async fn write_frame(&self, frame: &Value) -> Result<(), String>;
}

/// Writes management frames to the local stdio client through the broadcaster.
///
/// Not `broadcast`: a management request names one conversation and its answer carries
/// a capability. The shell is the only intended reader, and it is the `local` client.
pub struct HubWrite {
    hub: std::sync::Arc<crate::broadcast::Broadcaster>,
    client: crate::broadcast::ClientId,
}

impl HubWrite {
    pub fn new(
        hub: std::sync::Arc<crate::broadcast::Broadcaster>,
        client: crate::broadcast::ClientId,
    ) -> Self {
        Self { hub, client }
    }
}

#[async_trait::async_trait]
impl ManagementWrite for HubWrite {
    async fn write_frame(&self, frame: &Value) -> Result<(), String> {
        self.hub
            .reply(&self.client, frame.clone())
            .await
            .map_err(|_| "the local client is gone".to_string())
    }
}

/// The outbound management channel.
///
/// One per authenticated session. It refuses to issue anything before the bootstrap
/// succeeds, because a request written before then would be answered by nothing: the
/// shell does not know this sidecar's epoch yet.
pub struct ManagementClient {
    secret: String,
    sidecar_epoch: String,
    transport: std::sync::Arc<dyn ManagementWrite>,
    pending: Mutex<HashMap<String, PendingRequest>>,
    next_id: std::sync::atomic::AtomicU64,
}

struct PendingRequest {
    /// The epoch this request was issued under. A response naming another one is a
    /// response to a request from a sidecar generation that no longer exists.
    epoch: String,
    reply: Option<tokio::sync::oneshot::Sender<Result<Value, String>>>,
}

impl std::fmt::Debug for ManagementClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementClient")
            // Never the secret: this type is reachable from error paths.
            .field("secret", &"<redacted>")
            .field("sidecar_epoch", &self.sidecar_epoch)
            .field("pending", &self.pending_count())
            .finish()
    }
}

impl ManagementClient {
    pub fn new(
        secret: impl Into<String>,
        sidecar_epoch: impl Into<String>,
        transport: std::sync::Arc<dyn ManagementWrite>,
    ) -> Self {
        Self {
            secret: secret.into(),
            sidecar_epoch: sidecar_epoch.into(),
            transport,
            pending: Mutex::new(HashMap::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// Asks the shell for one conversation's root authority.
    ///
    /// `deadline` bounds the wait only. It is not sent: the shell applies its own
    /// budget after validating the intent, and a remote deadline the shell could not
    /// enforce would be a promise neither side keeps.
    pub async fn authorize(
        &self,
        request: RootIntentRequest,
        deadline: Deadline,
    ) -> Result<RootAuthorityEntry, String> {
        if request.sidecar_epoch != self.sidecar_epoch {
            return Err(format!(
                "this intent names sidecar epoch {}, but this session is {}",
                request.sidecar_epoch, self.sidecar_epoch
            ));
        }
        let budget = deadline.remaining();
        if budget.is_zero() {
            return Err("no budget remains to authorize this root".to_string());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = {
            let sequence = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            format!("authorize-{sequence}")
        };
        {
            let mut pending = self.lock_pending();
            pending.insert(
                id.clone(),
                PendingRequest {
                    epoch: self.sidecar_epoch.clone(),
                    reply: Some(tx),
                },
            );
        }
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": AUTHORIZE_ROOT_METHOD,
            "sidecar_epoch": self.sidecar_epoch,
            "secret": self.secret,
            "params": request,
        });
        if let Err(error) = self.transport.write_frame(&frame).await {
            // The request never left this process, so nothing will answer it. Remove
            // the slot instead of waiting out the whole budget for a frame that was
            // never written.
            self.lock_pending().remove(&id);
            return Err(format!("could not write the authorize request: {error}"));
        }
        match tokio::time::timeout(budget, rx).await {
            Ok(Ok(Ok(result))) => serde_json::from_value::<RootAuthorityEntry>(result)
                .map_err(|error| format!("the authorize reply is not an entry: {error}")),
            Ok(Ok(Err(error))) => Err(error),
            // Sender dropped without a value: the session closed or the slot was
            // cleared. Both mean "no conclusion", which is a refusal here rather than
            // a retry.
            Ok(Err(_)) => Err("the authorize request was abandoned".to_string()),
            Err(_) => {
                self.lock_pending().remove(&id);
                Err("the authorize request did not answer in time".to_string())
            }
        }
    }

    /// Delivers one inbound management response, or reports that it was not one.
    ///
    /// The caller (the stdio read loop) uses the return value to decide whether to
    /// keep reading: `false` means this frame belongs to the business protocol. A
    /// frame whose shape matches but whose id is unknown is consumed anyway -- an
    /// unrecognised management response must not fall through to `app.emit`, where
    /// the WebView could read the capability hanging off it.
    pub fn accept_response(&self, value: &Value) -> bool {
        if !looks_like_management_response(value) {
            return false;
        }
        let Some(id) = value.get("id").and_then(|id| id.as_str()) else {
            return false;
        };
        let mut pending = self.lock_pending();
        let Some(request) = pending.remove(id) else {
            // A response nobody is waiting for. It is still a management frame.
            return true;
        };
        let arrived_epoch = value
            .get("sidecar_epoch")
            .and_then(|epoch| epoch.as_str())
            .unwrap_or_default();
        let Some(reply) = request.reply else {
            return true;
        };
        if arrived_epoch != request.epoch {
            // A late reply from a replaced generation. Delivering it would install a
            // capability naming a root that generation owned.
            let _ = reply.send(Err(format!(
                "the authorize reply is for sidecar epoch {arrived_epoch}, not {}",
                request.epoch
            )));
            return true;
        }
        let outcome = match value.get("error") {
            Some(error) => Err(error
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| error.to_string())),
            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = reply.send(outcome);
        true
    }

    /// Drops every waiter. Called when the session closes: a waiter that is never
    /// answered would otherwise burn its whole budget after the reason was already
    /// known.
    pub fn close(&self) {
        let mut pending = self.lock_pending();
        for (_, request) in pending.drain() {
            if let Some(reply) = request.reply {
                let _ = reply.send(Err("the management session closed".to_string()));
            }
        }
    }

    /// How many requests are waiting for a reply.
    ///
    /// `pub(crate)` because the session-close path asserts that closing releases
    /// them: that assertion lives with the handle, not with the client.
    pub(crate) fn pending_count(&self) -> usize {
        self.lock_pending().len()
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingRequest>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Whether a frame is shaped like a management *response*.
///
/// Two structural facts, both required: no `method` (a response is not a request) and
/// a string `id` (business responses use numeric ids, and the business protocol never
/// uses a string one). Requiring both means a business frame cannot be mistaken for a
/// management response even before the pending table is consulted.
fn looks_like_management_response(value: &Value) -> bool {
    value.get("method").is_none()
        && value.get("id").and_then(|id| id.as_str()).is_some()
        && value.get("jsonrpc").is_some()
}

/// The production bridge: asks the shell, over the existing authenticated stdio.
pub struct ShellRootAuthority {
    client: std::sync::Arc<ManagementClient>,
}

impl ShellRootAuthority {
    pub fn new(client: std::sync::Arc<ManagementClient>) -> Self {
        Self { client }
    }
}

impl ManagedRootAuthority for ShellRootAuthority {
    fn authorize(
        &self,
        intent: &RootIntentRequest,
        deadline: Deadline,
    ) -> Result<RootAuthorityEntry, String> {
        // `resolve` is synchronous because the whole attach path is; the stdio round
        // trip is async. Blocking this thread on a reply another task must deliver is
        // the one thing that would deadlock, so the wait is moved onto a runtime
        // thread and this call parks. `block_in_place` keeps the runtime's other
        // workers free to run the reader that answers us.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return Err("authorization needs an async runtime to answer on".to_string());
        };
        // `block_in_place` is what makes this safe: the reply is read by another task,
        // so the caller cannot simply block the only worker. On a current-thread
        // runtime there is no other worker to move work to, and blocking would be a
        // guaranteed deadlock -- saying so is better than hanging.
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return Err(
                "authorization needs a multi-threaded runtime: the reply is delivered by \
                 another task, so the caller cannot block the only worker"
                    .to_string(),
            );
        }
        let client = std::sync::Arc::clone(&self.client);
        let request = intent.clone();
        tokio::task::block_in_place(move || {
            handle.block_on(async move { client.authorize(request, deadline).await })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use yi_agent_lifecycle::foreground::{RootBindIntent, SecretCapability};
    use yi_agent_lifecycle::production::{RootAuthorityEntry, RootAuthorityKey};

    const EPOCH: &str = "epoch-1";
    const SECRET: &str = "secret-1";

    /// Records what the sidecar wrote, so a test can answer it.
    #[derive(Default)]
    struct Recorder {
        frames: std::sync::Mutex<Vec<Value>>,
    }

    impl Recorder {
        fn frames(&self) -> Vec<Value> {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait::async_trait]
    impl ManagementWrite for Recorder {
        async fn write_frame(&self, frame: &Value) -> Result<(), String> {
            self.frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(frame.clone());
            Ok(())
        }
    }

    struct FailingWrite;

    #[async_trait::async_trait]
    impl ManagementWrite for FailingWrite {
        async fn write_frame(&self, _frame: &Value) -> Result<(), String> {
            Err("pipe is gone".to_string())
        }
    }

    fn intent(thread: &str) -> RootIntentRequest {
        RootIntentRequest {
            sidecar_epoch: EPOCH.to_string(),
            scope_key: "/p/.yi-agent/runtime".to_string(),
            client_run_id: "app-run-1".to_string(),
            intent: RootBindIntent {
                operation_id: format!("managed-root:{thread}"),
                thread_key: format!("thread:{thread}"),
                workspace: "/p".into(),
                extra_roots: Vec::new(),
                previous_root_capability: None,
            },
        }
    }

    fn entry(thread: &str) -> RootAuthorityEntry {
        RootAuthorityEntry {
            key: RootAuthorityKey {
                scope_key: "/p/.yi-agent/runtime".to_string(),
                generation: "gen-1".to_string(),
                client_run_id: "app-run-1".to_string(),
                thread_key: format!("thread:{thread}"),
                root_id: format!("root-{thread}"),
            },
            capability: SecretCapability::new(format!("cap-{thread}")),
        }
    }

    fn budget() -> Deadline {
        // The same budget the subagent builds its deadline from: one definition, so a
        // sidecar cannot end up waiting on the shell for less time than the shell takes.
        Deadline::after(yi_agent_lifecycle::AUTHORIZE_BUDGET)
    }

    fn client(recorder: Arc<Recorder>) -> Arc<ManagementClient> {
        Arc::new(ManagementClient::new(SECRET, EPOCH, recorder))
    }

    /// The request must be a management frame, not a business request: a business
    /// frame would be routed by the shell as a call to some RPC method.
    #[tokio::test]
    async fn an_authorize_is_written_as_a_management_request() {
        let recorder = Arc::new(Recorder::default());
        let client = client(Arc::clone(&recorder));
        let waiting = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.authorize(intent("a"), budget()).await })
        };
        // Give the request a chance to be written before answering it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let frames = recorder.frames();
        assert_eq!(frames.len(), 1, "{frames:?}");
        assert_eq!(frames[0]["method"], AUTHORIZE_ROOT_METHOD);
        assert_eq!(frames[0]["sidecar_epoch"], EPOCH);
        assert_eq!(frames[0]["secret"], SECRET);
        assert_eq!(frames[0]["params"]["intent"]["thread_key"], "thread:a");

        let id = frames[0]["id"].as_str().expect("a string id").to_string();
        let answered = client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "sidecar_epoch": EPOCH,
            "result": entry("a"),
        }));
        assert!(answered, "the reply must be consumed as a management frame");
        let resolved = waiting.await.unwrap().expect("the reply completes it");
        assert_eq!(resolved.key.thread_key, "thread:a");
        assert_eq!(resolved.capability.expose(), "cap-a");
    }

    /// A response from the previous generation names a root that no longer exists.
    /// Installing it would attach under a dead authority.
    #[tokio::test]
    async fn a_reply_from_another_epoch_is_refused() {
        let recorder = Arc::new(Recorder::default());
        let client = client(Arc::clone(&recorder));
        let waiting = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.authorize(intent("a"), budget()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let frames = recorder.frames();
        let id = frames[0]["id"].as_str().unwrap().to_string();
        client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "sidecar_epoch": "epoch-0",
            "result": entry("a"),
        }));
        let error = waiting.await.unwrap().unwrap_err();
        assert!(error.contains("epoch-0"), "{error}");
    }

    /// The sidecar cannot speak for an epoch the shell did not authenticate it
    /// under; asking anyway would be a request the shell must refuse.
    #[tokio::test]
    async fn an_intent_for_another_epoch_never_reaches_the_shell() {
        let recorder = Arc::new(Recorder::default());
        let client = client(Arc::clone(&recorder));
        let mut stale = intent("a");
        stale.sidecar_epoch = "epoch-0".to_string();
        let error = client.authorize(stale, budget()).await.unwrap_err();
        assert!(error.contains("epoch-0"), "{error}");
        assert!(recorder.frames().is_empty(), "nothing may be written");
    }

    /// A pipe that is already broken will never answer. Waiting the full budget would
    /// look like a hung shell.
    #[tokio::test]
    async fn a_write_failure_fails_fast_and_leaves_no_slot() {
        let client = Arc::new(ManagementClient::new(SECRET, EPOCH, Arc::new(FailingWrite)));
        let error = client.authorize(intent("a"), budget()).await.unwrap_err();
        assert!(error.contains("pipe is gone"), "{error}");
        assert_eq!(client.pending_count(), 0);
    }

    /// Business responses are routed by numeric ids and must not be consumed here.
    #[tokio::test]
    async fn a_business_response_is_not_a_management_response() {
        let recorder = Arc::new(Recorder::default());
        let client = client(recorder);
        assert!(!client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {},
        })));
        assert!(!client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "thread/started",
        })));
    }

    /// An unknown string id is still a management response. Letting it fall through
    /// would hand the WebView a frame carrying a capability.
    #[tokio::test]
    async fn an_unrecognised_management_response_is_consumed_not_broadcast() {
        let recorder = Arc::new(Recorder::default());
        let client = client(recorder);
        assert!(client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "authorize-999",
            "sidecar_epoch": EPOCH,
            "result": entry("a"),
        })));
    }

    /// Closing the session must release waiters instead of making each one burn its
    /// whole budget for a reason that is already known.
    #[tokio::test]
    async fn closing_releases_every_waiter() {
        let recorder = Arc::new(Recorder::default());
        let client = client(Arc::clone(&recorder));
        let waiting = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.authorize(intent("a"), budget()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        client.close();
        let error = waiting.await.unwrap().unwrap_err();
        assert!(error.contains("closed"), "{error}");
        assert_eq!(client.pending_count(), 0);
    }

    /// Two conversations must not be able to complete each other's requests.
    #[tokio::test]
    async fn replies_land_on_the_request_that_asked() {
        let recorder = Arc::new(Recorder::default());
        let client = client(Arc::clone(&recorder));
        let first = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.authorize(intent("a"), budget()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.authorize(intent("b"), budget()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let frames = recorder.frames();
        assert_eq!(frames.len(), 2, "{frames:?}");
        let id_a = frames[0]["id"].as_str().unwrap().to_string();
        let id_b = frames[1]["id"].as_str().unwrap().to_string();
        // Answer out of order on purpose.
        client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0", "id": id_b, "sidecar_epoch": EPOCH, "result": entry("b"),
        }));
        client.accept_response(&serde_json::json!({
            "jsonrpc": "2.0", "id": id_a, "sidecar_epoch": EPOCH, "result": entry("a"),
        }));
        assert_eq!(first.await.unwrap().unwrap().key.thread_key, "thread:a");
        assert_eq!(second.await.unwrap().unwrap().key.thread_key, "thread:b");
    }
}
