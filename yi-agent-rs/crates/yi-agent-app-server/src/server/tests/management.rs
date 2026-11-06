//! 本地 stdio 管理面的端到端行为。
//!
//! `local_management` 的单测证明的是**判定**（认证、去重、超时）；这里证明的是
//! **接线**，也就是单测看不见的那一半：
//!
//! - 私有帧真的先于业务路由被截获，且响应只回给本地连接；
//! - 私有帧不出现在 `hub` 广播里 —— 页面的 `app.emit` 读的是同一条出口，
//!   一旦私有响应进广播，它就等于把 epoch 与授权结果送给了 WebView；
//! - 未完成 bootstrap 的业务帧被拒，且**不**启动任何 runtime；
//! - 同一 scope 的并发 prepare 只调用一次后端；
//! - 保留命名空间在**没有**管理面时同样被拒（远端 / unmanaged stdio）。
//!
//! 断言"后端被调用了几次"用的是注入的计数后端，不是日志：日志可以被一次
//! `tracing` 过滤掉，而计数器不会。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::Harness;
use crate::server::local_management::{ManagementHandle, PrepareBackend};
// DTO 的权威定义在 lifecycle crate；这里只是把它带进作用域，不另造镜像。
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::production::{
    MigrationConfirmRequest, MigrationOperationRequest, MigrationPreviewRequest,
    MigrationPreviewView, MigrationWorkload, PrepareOutcome, PrepareRequest,
};
use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};

/// 数调用次数的后端。`delay` 用来把"并发"做成真的并发。
struct CountingBackend {
    calls: AtomicUsize,
    scopes: std::sync::Mutex<Vec<String>>,
    delay: Duration,
    outcome: PrepareOutcome,
}

impl CountingBackend {
    fn new(delay: Duration, outcome: PrepareOutcome) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            scopes: std::sync::Mutex::new(Vec::new()),
            delay,
            outcome,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn scopes(&self) -> Vec<String> {
        self.scopes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl PrepareBackend for CountingBackend {
    async fn prepare(&self, request: PrepareRequest) -> PrepareOutcome {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.scopes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(request.scope.key.clone());
        tokio::time::sleep(self.delay).await;
        self.outcome.clone()
    }
}

/// A migration backend that counts what it was asked to do.
///
/// The counts are the assertion: "the preview was refused" is proven by the backend
/// never being reached, not by the wording of an error frame.
#[derive(Default)]
struct MigrationBackendCalls {
    previews: AtomicUsize,
    confirms: AtomicUsize,
    operations: AtomicUsize,
}

struct MigratingBackend {
    calls: Arc<MigrationBackendCalls>,
    preview: Result<MigrationPreviewView, String>,
}

impl MigratingBackend {
    fn new(
        preview: Result<MigrationPreviewView, String>,
    ) -> (Arc<Self>, Arc<MigrationBackendCalls>) {
        let calls = Arc::new(MigrationBackendCalls::default());
        (
            Arc::new(Self {
                calls: Arc::clone(&calls),
                preview,
            }),
            calls,
        )
    }
}

#[async_trait::async_trait]
impl PrepareBackend for MigratingBackend {
    async fn prepare(&self, request: PrepareRequest) -> PrepareOutcome {
        let _ = request;
        PrepareOutcome::Unknown {
            reason: "this backend only migrates".into(),
        }
    }

    async fn migration_preview(
        &self,
        request: MigrationPreviewRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        self.calls.previews.fetch_add(1, Ordering::SeqCst);
        let view = self.preview.clone()?;
        serde_json::to_value(view).map_err(|error| error.to_string())
    }

    async fn migration_confirm(
        &self,
        request: MigrationConfirmRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        self.calls.confirms.fetch_add(1, Ordering::SeqCst);
        Err("the test backend does not run migrations".into())
    }

    async fn migration_operation(
        &self,
        request: MigrationOperationRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        self.calls.operations.fetch_add(1, Ordering::SeqCst);
        Ok(serde_json::Value::Null)
    }
}

fn migration_preview_params(scope_key: &str) -> serde_json::Value {
    serde_json::to_value(MigrationPreviewRequest {
        scope: RuntimeScope {
            key: scope_key.to_string(),
            runtime_dir: std::path::PathBuf::from(scope_key),
            project_key: "/tmp/project-a".into(),
            instance_home: "/tmp/instance".into(),
        },
        target: ArtifactIdentity {
            build_id: "build-1".into(),
            display_version: "0.1.13".into(),
            executable: "/Applications/yi-agent.app/Contents/MacOS/yi-agent".into(),
            sha256: String::new(),
            schema_min: 0,
            schema_max: 15,
            handover_version: 1,
            business_protocol_min: 2,
            business_protocol_max: 2,
            capabilities: vec![],
        },
    })
    .expect("migration preview params serialize")
}

fn preview_view(scope_key: &str) -> MigrationPreviewView {
    MigrationPreviewView {
        token: "tok-1".into(),
        evidence_digest: "digest-1".into(),
        scope_key: scope_key.to_string(),
        affected: vec![yi_agent_lifecycle::production::MigrationAffected {
            scope_key: "/tmp/other".into(),
            workload: MigrationWorkload::Unverified("the registration file says nothing".into()),
        }],
        blockers: vec![],
        expires_at_unix: 0,
    }
}

fn connected() -> PrepareOutcome {
    PrepareOutcome::Connected(yi_agent_lifecycle::types::RuntimeIdentity {
        scope_key: "/tmp/project-a".into(),
        daemon_instance_id: None,
        guardian_build_id: None,
        daemon_build_id: None,
        mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
        business_protocol: Some(2),
        daemon_display_version: Some("0.1.13".into()),
        guardian_display_version: Some("0.1.13".into()),
        capabilities: vec![],
        schema_min: 0,
        schema_max: 15,
    })
}

fn prepare_params(scope_key: &str) -> serde_json::Value {
    serde_json::to_value(PrepareRequest {
        scope: RuntimeScope {
            key: scope_key.to_string(),
            runtime_dir: std::path::PathBuf::from(scope_key),
            project_key: scope_key.to_string(),
            instance_home: std::path::PathBuf::from("/tmp/instance"),
        },
        expected: ArtifactIdentity {
            build_id: "build-1".into(),
            display_version: "0.1.13".into(),
            executable: "/Applications/yi-agent.app/Contents/MacOS/yi-agent".into(),
            sha256: String::new(),
            schema_min: 0,
            schema_max: 15,
            handover_version: 1,
            business_protocol_min: 2,
            business_protocol_max: 2,
            capabilities: vec![],
        },
        attempt_id: "attempt-1".into(),
    })
    .expect("prepare params serialize")
}

fn bootstrap_frame(secret: &str, epoch: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "_shell/bootstrap",
        "params": { "secret": secret, "sidecarEpoch": epoch },
        "secret": secret,
        "sidecar_epoch": epoch,
    })
    .to_string()
}

fn management_frame(
    id: &str,
    method: &str,
    secret: &str,
    epoch: &str,
    params: serde_json::Value,
) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "secret": secret,
        "sidecar_epoch": epoch,
        "params": params,
    })
    .to_string()
}

/// 读一帧私有管理响应。
///
/// 只接受"看起来像管理帧"的那些：带 `sidecar_epoch`、`bootstrap` 或字符串
/// `id`。用宽松匹配而不是精确断言，是为了让失败信息落在调用处（"没等到
/// ACK"）而不是在这里指责一个无关的业务通知。
async fn next_private_frame(h: &mut Harness, what: &str) -> serde_json::Value {
    let frame = h
        .read_matching(64, |v| {
            v.get("sidecar_epoch").is_some()
                || v.get("bootstrap").is_some()
                || v.get("id").and_then(|id| id.as_str()).is_some()
        })
        .await;
    assert!(
        frame.get("id").and_then(|id| id.as_str()).is_some() || frame.get("bootstrap").is_some(),
        "expected a private management frame while waiting for {what}, got {frame}"
    );
    frame
}

/// 读一个业务响应，并**要求**它不是私有帧。
///
/// 这个辅助函数就是"私有帧不出现在业务流里"这条断言的形状：读到管理响应的
/// 测试会立刻失败，而不是在别处得到一个莫名其妙的断言错误。
async fn next_business_frame(h: &mut Harness, id: i64) -> serde_json::Value {
    let frame = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(id)))
        .await;
    assert!(
        !frame.to_string().contains("sidecar_epoch"),
        "a private management frame leaked into the business stream: {frame}"
    );
    frame
}

#[tokio::test]
async fn an_unbootstrapped_stdio_session_cannot_do_business_or_prepare() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    // 业务帧：被拒，且**没有**任何 prepare 被启动。
    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#)
        .await;
    let refusal = next_business_frame(&mut h, 7).await;
    assert!(
        refusal.get("error").is_some(),
        "business before bootstrap must be refused: {refusal}"
    );

    // 私有 prepare：同样被拒。未认证的来包连"启动一个 runtime"都不该能做到。
    h.send(&management_frame(
        "m-1",
        "_shell/prepare",
        "guess",
        "epoch-guess",
        prepare_params("/tmp/project-a"),
    ))
    .await;
    let response = next_private_frame(&mut h, "the unauthenticated prepare").await;
    assert!(
        response.get("error").is_some(),
        "prepare without bootstrap must be refused: {response}"
    );

    assert_eq!(
        backend.calls(),
        0,
        "no runtime may be started by an unauthenticated session"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn bootstrap_acks_with_the_session_epoch_and_then_admits_business() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    let ack = next_private_frame(&mut h, "the bootstrap ack").await;
    assert_eq!(
        ack.get("bootstrap").and_then(|v| v.as_str()),
        Some("accepted")
    );
    // ACK 必须回显**会话固定下来的** epoch，否则壳无法据此对账。
    assert_eq!(
        ack.get("sidecar_epoch").and_then(|v| v.as_str()),
        Some("epoch-1"),
        "the ack must carry the epoch the session fixed: {ack}"
    );

    h.send(r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{}}"#)
        .await;
    let reply = next_business_frame(&mut h, 7).await;
    assert_eq!(
        reply.get("id"),
        Some(&serde_json::json!(7)),
        "initialize must now be answered normally: {reply}"
    );
    h.shutdown().await;
}

/// After the bootstrap fixes an epoch and the shell names itself, the session holds
/// exactly one authority record -- and it is keyed by the App run the shell named.
///
/// This is the wiring half of `managed_binding`'s unit tests: those prove the store
/// refuses a foreign entry, this proves the handshake actually installs one, and that
/// it installs the *authenticated* epoch rather than anything a request could name.
#[tokio::test]
async fn the_handshake_installs_the_session_authority_under_the_bootstrap_epoch() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-7")).await;
    let ack = next_private_frame(&mut h, "the bootstrap ack").await;
    assert_eq!(
        ack.get("sidecar_epoch").and_then(|v| v.as_str()),
        Some("epoch-7")
    );

    h.send(
        r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-42","scope_key":"/p/.yi-agent/runtime","issued_by_shell":true}}}"#,
    )
    .await;
    let reply = next_business_frame(&mut h, 7).await;
    assert!(
        reply.get("result").is_some(),
        "initialize must succeed: {reply}"
    );

    let authority = crate::server::managed_snapshot(&h.managed)
        .expect("the handshake must install this session's authority");
    assert_eq!(authority.client_run_id, "app-run-42");
    // Read the epoch into a local and drop the guard before the await below: holding a
    // std `Mutex` across an await is a lock-order hazard, even in a test.
    let epoch = authority
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .epoch()
        .map(str::to_string);
    assert_eq!(
        epoch.as_deref(),
        Some("epoch-7"),
        "the epoch comes from the bootstrap, never from the handshake body"
    );
    h.shutdown().await;
}

/// A session that never bootstrapped has no epoch, so nothing may be installed: a
/// conversation in it attaches with its own key instead of pretending to be managed.
#[tokio::test]
async fn a_handshake_without_a_bootstrap_installs_no_authority() {
    let mut h = Harness::new();
    h.send(
        r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-42","issued_by_shell":true}}}"#,
    )
    .await;
    let reply = next_business_frame(&mut h, 7).await;
    assert!(reply.get("result").is_some(), "{reply}");
    assert!(
        crate::server::managed_snapshot(&h.managed).is_none(),
        "without an authenticated bootstrap there is no epoch, and without an epoch          no authority may be trusted"
    );
    h.shutdown().await;
}

/// A shell that names itself without the bootstrap secret gets no authority either,
/// even on the local channel: identity alone is not a credential.
#[tokio::test]
async fn an_unbootstrapped_identity_claim_installs_no_authority() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    // No `_shell/bootstrap` first. The frame is refused before it reaches `initialize`.
    h.send(
        r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-42","issued_by_shell":true}}}"#,
    )
    .await;
    // The refusal echoes the request's own numeric id, because it is the business frame
    // that was refused: it never became a management request, so it gets no private
    // envelope.
    let refused = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(7)))
        .await;
    assert!(
        refused.get("error").is_some(),
        "an unbootstrapped session must refuse the frame: {refused}"
    );
    assert!(
        crate::server::managed_snapshot(&h.managed).is_none(),
        "a refused frame must install nothing"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn a_second_bootstrap_does_not_take_over_the_session() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(Arc::clone(&handle)).await;

    h.send(&bootstrap_frame("first", "epoch-1")).await;
    let first = next_private_frame(&mut h, "the first ack").await;
    assert_eq!(
        first.get("bootstrap").and_then(|v| v.as_str()),
        Some("accepted")
    );

    h.send(&bootstrap_frame("second", "epoch-2")).await;
    let second = next_private_frame(&mut h, "the second ack").await;
    assert_eq!(
        second.get("bootstrap").and_then(|v| v.as_str()),
        Some("already-bootstrapped"),
        "a second bootstrap must not silently replace the session: {second}"
    );
    // 会话 epoch 没被顶掉。
    assert_eq!(
        second.get("sidecar_epoch").and_then(|v| v.as_str()),
        Some("epoch-1")
    );
    h.shutdown().await;
}

/// A project that opens after the handshake joins the same session.
///
/// The shell named itself once. A second project must not need a second
/// `initialize`, a second App run id, or a second bootstrap: it is another scope
/// under the *same* identity, and a prepare for it must be accepted under the
/// epoch the first bootstrap fixed.
#[tokio::test]
async fn a_new_project_after_initialize_is_prepared_under_the_same_identity() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(Arc::clone(&handle)).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    next_private_frame(&mut h, "the ack").await;
    h.send(
        r#"{"jsonrpc":"2.0","id":7,"method":"initialize","params":{"shellAuthorization":{"client_run_id":"app-run-42","issued_by_shell":true}}}"#,
    )
    .await;
    next_business_frame(&mut h, 7).await;

    let first_authority = crate::server::managed_snapshot(&h.managed)
        .expect("initialize installs the session identity");
    assert_eq!(first_authority.client_run_id, "app-run-42");

    // The new project opens later, over the same stdio session. No second
    // handshake is sent: if the design needed one, this prepare would be refused.
    h.send(&management_frame(
        "p-new",
        "_shell/prepare",
        "s3cret",
        "epoch-1",
        prepare_params("/tmp/project-b"),
    ))
    .await;
    let reply = next_private_frame(&mut h, "the new project's prepare").await;
    assert!(
        reply.get("error").is_none(),
        "a later project must be admitted under the session's own epoch: {reply}"
    );
    assert_eq!(backend.scopes(), vec!["/tmp/project-b".to_string()]);

    // The session kept exactly the identity it was given. A second handshake would
    // have replaced the record; nothing here may have moved it.
    let still = crate::server::managed_snapshot(&h.managed).expect("the identity survives");
    assert_eq!(still.client_run_id, "app-run-42");
    let still_epoch = still
        .store
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .epoch()
        .map(str::to_string);
    assert_eq!(
        still_epoch.as_deref(),
        Some("epoch-1"),
        "the epoch is fixed by the bootstrap and does not move for a new project"
    );

    // A prepare still naming a retired epoch is refused, so "joined the session"
    // cannot be confused with "any epoch will do".
    h.send(&management_frame(
        "p-stale",
        "_shell/prepare",
        "s3cret",
        "epoch-0",
        prepare_params("/tmp/project-c"),
    ))
    .await;
    let refused = next_private_frame(&mut h, "the stale prepare").await;
    assert!(
        refused.get("error").is_some(),
        "a prepare from a previous epoch must be refused: {refused}"
    );
    assert_eq!(
        backend.scopes(),
        vec!["/tmp/project-b".to_string()],
        "the refused prepare must not start a runtime"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn two_prepares_for_one_scope_start_the_runtime_once() {
    // 后端的延迟远大于一次往返，两条请求因此必然重叠。
    let backend = CountingBackend::new(Duration::from_millis(300), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    next_private_frame(&mut h, "the ack").await;

    for id in ["p-1", "p-2"] {
        h.send(&management_frame(
            id,
            "_shell/prepare",
            "s3cret",
            "epoch-1",
            prepare_params("/tmp/project-a"),
        ))
        .await;
    }

    let mut ids = Vec::new();
    for _ in 0..2 {
        let frame = next_private_frame(&mut h, "a prepare response").await;
        ids.push(
            frame
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }
    ids.sort();
    assert_eq!(ids, vec!["p-1".to_string(), "p-2".to_string()]);

    assert_eq!(
        backend.calls(),
        1,
        "two concurrent prepares for one scope must start the runtime exactly once"
    );
    assert_eq!(backend.scopes(), vec!["/tmp/project-a".to_string()]);
    h.shutdown().await;
}

#[tokio::test]
async fn a_prepare_with_the_wrong_epoch_is_refused() {
    let backend = CountingBackend::new(Duration::from_millis(1), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    next_private_frame(&mut h, "the ack").await;

    h.send(&management_frame(
        "p-1",
        "_shell/prepare",
        "s3cret",
        "epoch-0",
        prepare_params("/tmp/project-a"),
    ))
    .await;
    let response = next_private_frame(&mut h, "the stale-epoch prepare").await;
    assert!(
        response.get("error").is_some(),
        "a request carrying another epoch must not be admitted: {response}"
    );
    assert_eq!(backend.calls(), 0, "and it must not start anything");
    h.shutdown().await;
}

#[tokio::test]
async fn the_private_namespace_is_refused_when_no_management_surface_exists() {
    // 远端 / unmanaged stdio：`management == None`。保留前缀仍然不许落进业务路由。
    let mut h = Harness::new();
    super::initialize(&mut h).await;

    for method in ["_shell/prepare", "_shell/bootstrap", "_shell/somethingNew"] {
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": method,
            "params": {},
        });
        h.send(&serde_json::to_string(&frame).unwrap()).await;
        let reply = h
            .read_matching(32, |v| v.get("id") == Some(&serde_json::json!(99)))
            .await;
        let error = reply
            .get("error")
            .unwrap_or_else(|| panic!("{method} must be refused, got {reply}"));
        assert_eq!(
            error.get("code").and_then(|c| c.as_i64()),
            Some(-32601),
            "{method}: {reply}"
        );
    }
    h.shutdown().await;
}

#[tokio::test]
async fn closing_the_session_drains_pending_prepares() {
    // 后端比本用例的 deadline 慢得多：请求会真的卡在 in-flight 里。
    let backend = CountingBackend::new(Duration::from_secs(30), connected());
    let handle = Arc::new(ManagementHandle::new(backend.clone()));
    let mut h = Harness::with_management(Arc::clone(&handle)).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    next_private_frame(&mut h, "the ack").await;
    h.send(&management_frame(
        "p-1",
        "_shell/prepare",
        "s3cret",
        "epoch-1",
        prepare_params("/tmp/project-a"),
    ))
    .await;

    // 等到它真的进了 in-flight 表，否则下面那句断言是在测空表。
    let entered = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if handle.inflight_count() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    assert!(entered.is_ok(), "the prepare never registered as in-flight");

    handle.close();
    assert_eq!(
        handle.inflight_count(),
        0,
        "closing the session must drop every in-flight slot"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn the_default_deadline_budget_is_bounded() {
    // 管理请求的预算不该是"等到底"：一个卡住的 prepare 会把退出拖过壳的 15s
    // 全局预算。这里把常量本身钉住，避免有人悄悄放大它。
    let budget = yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT;
    assert!(
        budget <= Duration::from_secs(30),
        "control budget {budget:?}"
    );
    let _ = Deadline::after(budget);
}

/// A migration is not reachable before the credential is established.
///
/// The count is the assertion: an unauthenticated frame must not even reach the
/// backend, so "nothing ran" cannot be confused with "the wording was right".
#[tokio::test]
async fn an_unbootstrapped_session_cannot_preview_a_migration() {
    let (backend, calls) = MigratingBackend::new(Ok(preview_view("/tmp/runtime")));
    let handle = Arc::new(ManagementHandle::new(backend));
    let mut h = Harness::with_management(handle).await;

    h.send(&management_frame(
        "m-1",
        "_shell/migrationPreview",
        "s3cret",
        "epoch-1",
        migration_preview_params("/tmp/runtime"),
    ))
    .await;
    let frame = next_private_frame(&mut h, "the migration preview refusal").await;
    assert!(
        frame
            .get("error")
            .and_then(|error| error.as_str())
            .is_some(),
        "an unbootstrapped preview must be refused: {frame}"
    );
    assert_eq!(
        calls.previews.load(Ordering::SeqCst),
        0,
        "an unauthenticated frame reached the migration backend"
    );
    h.shutdown().await;
}

/// The preview is answered by the backend, and it is answerable for an instance whose
/// runtime could not be started: nothing here takes a lease or launches anything.
#[tokio::test]
async fn a_bootstrapped_session_can_preview_a_migration() {
    let (backend, calls) = MigratingBackend::new(Ok(preview_view("/tmp/runtime")));
    let handle = Arc::new(ManagementHandle::new(backend));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    let ack = next_private_frame(&mut h, "the bootstrap ack").await;
    assert_eq!(
        ack.get("bootstrap").and_then(|v| v.as_str()),
        Some("accepted")
    );

    h.send(&management_frame(
        "m-2",
        "_shell/migrationPreview",
        "s3cret",
        "epoch-1",
        migration_preview_params("/tmp/runtime"),
    ))
    .await;
    let frame = next_private_frame(&mut h, "the migration preview").await;
    assert!(frame.get("error").is_none(), "the preview failed: {frame}");
    let result = frame.get("result").expect("a preview result");
    assert_eq!(result.get("token"), Some(&serde_json::json!("tok-1")));
    // An unverifiable workload must survive the wire as "unverified", never as idle.
    let affected = result
        .get("affected")
        .and_then(|value| value.as_array())
        .expect("affected projects");
    assert_eq!(affected.len(), 1);
    assert!(
        affected[0]
            .get("workload")
            .and_then(|workload| workload.get("unverified"))
            .is_some(),
        "an unverified workload was flattened into something else: {affected:?}"
    );
    assert_eq!(calls.previews.load(Ordering::SeqCst), 1);
    h.shutdown().await;
}

/// A confirmation with nothing to confirm is refused by the backend, and the refusal
/// is carried back rather than reported as success.
#[tokio::test]
async fn a_migration_confirmation_carries_the_backend_refusal() {
    let (backend, calls) = MigratingBackend::new(Ok(preview_view("/tmp/runtime")));
    let handle = Arc::new(ManagementHandle::new(backend));
    let mut h = Harness::with_management(handle).await;

    h.send(&bootstrap_frame("s3cret", "epoch-1")).await;
    let _ = next_private_frame(&mut h, "the bootstrap ack").await;

    let params = serde_json::json!({
        "scope": {
            "key": "/tmp/runtime",
            "runtime_dir": "/tmp/runtime",
            "project_key": "/tmp/project-a",
            "instance_home": "/tmp/instance",
        },
        "target": {
            "build_id": "build-1",
            "display_version": "0.1.13",
            "executable": "/Applications/yi-agent.app/Contents/MacOS/yi-agent",
            "sha256": "",
            "schema_min": 0,
            "schema_max": 15,
            "handover_version": 1,
            "business_protocol_min": 2,
            "business_protocol_max": 2,
            "capabilities": [],
        },
        "token": "tok-1",
        "operation_id": "op-1",
    });
    h.send(&management_frame(
        "m-3",
        "_shell/migrationConfirm",
        "s3cret",
        "epoch-1",
        params,
    ))
    .await;
    let frame = next_private_frame(&mut h, "the migration confirmation").await;
    assert!(
        frame
            .get("error")
            .and_then(|error| error.as_str())
            .is_some_and(|error| error.contains("does not run migrations")),
        "the backend refusal was not carried back: {frame}"
    );
    assert_eq!(calls.confirms.load(Ordering::SeqCst), 1);
    h.shutdown().await;
}
