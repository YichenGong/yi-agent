//! 本地 stdio 的私有管理面：认证 bootstrap、`_shell/` 保留命名空间与去重 prepare。
//!
//! 为什么需要一个独立模块，而不是塞进业务 RPC：这里的每条规则都是「业务面
//! **不得**具备的能力」，把它们混进 `rpc::*` 会让下面三条边界重新变得可绕过。
//!
//! 1. **只有壳能说话。** 管理帧要求首帧 bootstrap 携带一个只经继承 FD 传递的
//!    随机秘密。秘密不进 argv、不进普通环境、不进 WebView、也不进日志；一个
//!    自己 spawn 出 app-server 的进程没有这个 FD，因此无论它写什么 `_shell/`
//!    方法都只会得到 `unauthorized`。
//! 2. **命名空间封闭。** `_shell/` 是保留前缀，通配拒绝而不是白名单：列出今天
//!    认识的几个方法，明天新增的方法就默认可达。`newFutureMethod` 必须和
//!    `prepare` 一样被拒。
//! 3. **准备是幂等的、并且只缓存进行中的尝试。** 同一 scope 的两个并发 prepare
//!    共用一个进行中的结果，因此只启动一次 runtime；瞬时失败**不**缓存，否则
//!    一次网络抖动会让该项目在这一代际里再也起不来。不同 scope 之间不共享锁，
//!    否则两个项目会串行。
//!
//! 本模块只做判定与去重，真正的 bring-up 由注入的 [`PrepareBackend`] 完成：
//! 生产实现走 `ensure_managed`，测试实现只数调用次数。这样「未认证不得启动」
//! 这类断言才能验在**副作用**上，而不是验在一句错误文案上。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use yi_agent_lifecycle::deadline::Deadline;
use yi_agent_lifecycle::foreground::SecretCapability;
use yi_agent_lifecycle::production::{
    MigrationConfirmRequest, MigrationOperationRequest, MigrationPreviewRequest, PrepareOutcome,
    PrepareRequest,
};

/// 保留方法命名空间。
///
/// 单独成常量而不是散在各处的字面量：`_shell/` 被写错一个字符（`_shell.`、
/// `_SHELL/`）就等于把私有面重新开放给业务面，而那种错误只能靠一处定义来避免。
pub const RESERVED_PREFIX: &str = "_shell/";

/// 建立会话的那一帧用的方法名。
///
/// 它与其余管理方法同前缀，但**不是请求**：没有 `id`、只在会话建立时出现一次。
/// 传输层必须先按它判定，再走普通管理分发，否则凭据帧会被当成"未知管理方法"
/// 拒掉，随后的整条会话都认证不上。
pub const BOOTSTRAP_METHOD: &str = "_shell/bootstrap";

/// Read-only legacy-migration preview.
///
/// A separate method from `prepare` on purpose: a preview must be answerable for an
/// instance whose runtime this build cannot start, and it must never be the thing that
/// starts one.
pub const MIGRATION_PREVIEW_METHOD: &str = "_shell/migrationPreview";

/// Runs a confirmed migration, or reconciles the operation the caller already named.
pub const MIGRATION_CONFIRM_METHOD: &str = "_shell/migrationConfirm";

/// Reads one migration operation's progress without changing anything.
pub const MIGRATION_OPERATION_METHOD: &str = "_shell/migrationOperation";

/// Requests one conversation's root authority from the shell.
///
/// The sidecar cannot mint this itself: the shell holds the lease, and the guardian
/// only answers the party that holds it. This method is how a conversation asks; the
/// answer is a `RootAuthorityEntry` the sidecar installs in its session store and never
/// re-derives.
pub const AUTHORIZE_ROOT_METHOD: &str = "_shell/authorizeRoot";

// The direction matters for `AUTHORIZE_ROOT_METHOD` above: this module handles frames
// the *shell* sends, and that one travels the other way. It lives here anyway because
// it is part of the same wire namespace and both halves must agree on the spelling;
// the outbound corridor that actually uses it is `super::management_client`.

/// 管理会话的认证凭据。
///
/// `sidecar_epoch` 在会话建立时固定，之后任何来包都不得切换它：一个可以自报
/// epoch 的来包，等于一个可以决定「哪一代的旧响应还算数」的来包。
#[derive(Clone, Debug)]
pub struct ManagementCredential {
    secret: SecretCapability,
    sidecar_epoch: String,
}

impl ManagementCredential {
    pub fn new(secret: impl Into<String>, sidecar_epoch: impl Into<String>) -> Self {
        Self {
            secret: SecretCapability::new(secret),
            sidecar_epoch: sidecar_epoch.into(),
        }
    }

    pub fn sidecar_epoch(&self) -> &str {
        &self.sidecar_epoch
    }

    /// 常量时间比较：这个秘密是唯一把「壳」和「任何别的本地进程」区分开的东西，
    /// 逐字节短路比较会把它的前缀长度泄漏给计时观察者。
    fn matches(&self, candidate: &str) -> bool {
        let expected = self.secret.expose().as_bytes();
        let candidate = candidate.as_bytes();
        if expected.len() != candidate.len() {
            return false;
        }
        let mut diff = 0u8;
        for (left, right) in expected.iter().zip(candidate) {
            diff |= left ^ right;
        }
        diff == 0
    }
}

/// 一个已认证的管理会话。
///
/// `None` 表示这条连接没有完成 bootstrap：此时任何 `_shell/` 方法都必须被拒。
#[derive(Debug)]
pub struct ManagementSession {
    credential: Option<ManagementCredential>,
}

/// bootstrap 第一帧。
#[derive(Debug, serde::Deserialize)]
pub struct BootstrapFrame {
    pub secret: String,
    /// 发起方自认的 sidecar 代际。壳每次都新生成，用来把旧响应挡在新请求之外。
    #[serde(default)]
    pub sidecar_epoch: String,
}

/// 一条管理请求进入后的判定结果。
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// 可以处理。
    Accepted,
    /// 未完成 bootstrap，或秘密不匹配。
    Unauthorized,
    /// bootstrap 已完成，第二条 bootstrap 不接受（秘密只在会话建立时用一次）。
    AlreadyBootstrapped,
}

impl Default for ManagementSession {
    fn default() -> Self {
        Self::new()
    }
}

impl ManagementSession {
    pub fn new() -> Self {
        Self { credential: None }
    }

    /// 是否已完成 bootstrap。本地槽位只有在它为真之后才可被业务写入。
    pub fn is_bootstrapped(&self) -> bool {
        self.credential.is_some()
    }

    pub fn sidecar_epoch(&self) -> Option<&str> {
        self.credential
            .as_ref()
            .map(ManagementCredential::sidecar_epoch)
    }

    /// 用**第一帧**建立会话。
    ///
    /// 这里刻意不拿一个"期望值"去比对：stdin 的写端本来就只有持有它的进程，
    /// 而那个进程就是壳。所以秘密的作用不是证明"我知道一个事先约定的值"，而是
    /// **成为**这条会话的凭据——壳只把它写进 fd 0，从不写进 WebView 参数、
    /// 也不写进环境变量，于是页面即使能经 `rpc` 往同一个 stdin 写业务帧，也
    /// 拿不到这个值去伪造管理帧。
    ///
    /// 第二条 bootstrap 被拒而不是覆盖：覆盖会让一条已认证的会话在运行中被
    /// 另一条连接顶掉，连同它固定下来的 epoch 一起。
    pub fn bootstrap(&mut self, frame: &BootstrapFrame) -> Admission {
        if self.is_bootstrapped() {
            return Admission::AlreadyBootstrapped;
        }
        if frame.secret.is_empty() {
            // 空凭据等于没有凭据：任何进程都能用空串通过。
            return Admission::Unauthorized;
        }
        self.credential = Some(ManagementCredential::new(
            frame.secret.clone(),
            frame.sidecar_epoch.clone(),
        ));
        Admission::Accepted
    }

    /// 判定一条管理请求能否被受理。
    ///
    /// `secret` 是来包自带的秘密。它必须与 bootstrap 建立的那个一致，且来包声明
    /// 的 epoch 必须等于会话固定的 epoch —— 后者是「旧响应不得完成新请求」的
    /// 会话侧一半。
    pub fn admit(&self, secret: &str, sidecar_epoch: &str) -> Admission {
        let Some(credential) = self.credential.as_ref() else {
            return Admission::Unauthorized;
        };
        if !credential.matches(secret) || credential.sidecar_epoch() != sidecar_epoch {
            return Admission::Unauthorized;
        }
        Admission::Accepted
    }

    /// 业务帧能否被受理。
    ///
    /// bootstrap 未完成时一律拒绝。壳在拿到 ACK 之前本来就不会发业务帧，这条
    /// 判定是服务端那一半：它让"壳忘了等 ACK"变成一个立刻可见的错误，而不是
    /// 一段安静地在未认证会话上跑起来的业务流量。
    pub fn admit_business(&self) -> Admission {
        if self.is_bootstrapped() {
            Admission::Accepted
        } else {
            Admission::Unauthorized
        }
    }
}

/// 方法名是否落在保留命名空间里。
///
/// 通配判定，不是逐个方法名列举：新增的私有方法默认被保护，而忘了登记一个
/// 业务方法只会得到一个 `method_not_found`，两边的失败代价不对称。
pub fn is_reserved(method: &str) -> bool {
    method.starts_with(RESERVED_PREFIX)
}

/// 管理响应允许的最大字节数。
///
/// 复用传输层既有的帧上限语义：一个管理响应和业务帧一样不该无限大，否则
/// 一个恶意的 prepare 结果可以直接把宿主内存打满。
pub const MAX_MANAGEMENT_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// 一次 prepare 的落点：真正启动 runtime 的地方。
///
/// 抽成 trait 的唯一目的是把「有没有启动」变成可数的副作用。认证、去重、超时
/// 这些判定不该再靠日志断言。
#[async_trait::async_trait]
pub trait PrepareBackend: Send + Sync {
    async fn prepare(&self, request: PrepareRequest) -> PrepareOutcome;

    /// Read-only: mints a preview over a fresh probe of the instance.
    ///
    /// Defaulted to an explicit refusal so a backend that never implemented
    /// migration cannot answer "there is nothing to migrate" by accident.
    async fn migration_preview(
        &self,
        request: MigrationPreviewRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        Err("this backend cannot preview a legacy migration".to_string())
    }

    /// Runs or reconciles one migration.
    ///
    /// The operation id is supplied by the caller and is checked against existing
    /// journals first, so a repeated request after a lost answer reconciles instead of
    /// stopping anything twice.
    async fn migration_confirm(
        &self,
        request: MigrationConfirmRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        Err("this backend cannot confirm a legacy migration".to_string())
    }

    /// One read of an operation. `Ok(None)` means "no record", never "it did not run".
    async fn migration_operation(
        &self,
        request: MigrationOperationRequest,
    ) -> Result<serde_json::Value, String> {
        let _ = request;
        Err("this backend cannot read a legacy migration operation".to_string())
    }
}

/// 已认证的管理操作执行器。
///
/// 每个 scope 一个进行中的槽位：并发的同 scope 请求等待**同一个**结果，因此
/// runtime 只会被启动一次。不同 scope 不共享任何锁。
pub struct ManagementHandle {
    session: Arc<Mutex<ManagementSession>>,
    /// 进行中的 prepare，按 scope key 索引。
    ///
    /// 用 std 的锁而不是 tokio 的：临界区里没有 `await`（只查表、插表、删表），
    /// 而 `close_now` 必须能在同步上下文里清空它。用 async 锁会让"进程收尾时
    /// 清空"变成一件没法同步做完的事。
    inflight: std::sync::Mutex<HashMap<String, Arc<Inflight>>>,
    /// 本会话的出站管理通道，bootstrap 成功后才有。
    ///
    /// 放在句柄里而不是传输层的局部变量：读取任务要在**每一帧**到达时问一次
    /// 「这是不是一条管理应答」，而它先于 bootstrap 启动（第一帧就是凭据帧）。
    /// 一个在握手完成后才填上的共享槽位，是唯一能让两者都与同一份会话状态对齐
    /// 的形状。
    outbound: std::sync::Mutex<Option<Arc<super::management_client::ManagementClient>>>,
    /// The epoch fixed by the accepted bootstrap, mirrored out of the session.
    ///
    /// The session owns the authoritative copy; this one exists so the handshake can
    /// read the epoch without awaiting the session lock, and it is written once, on
    /// acceptance.
    epoch: std::sync::Mutex<Option<String>>,
    backend: Arc<dyn PrepareBackend>,
}

/// 一次进行中的 prepare。
///
/// 用 `watch` 而不是 `oneshot`：晚到的等待者在结果已就绪时仍要能拿到它，而
/// `oneshot` 只有第一个接收者能得到值。第三个并发请求不该因为「没抢到」而失败。
struct Inflight {
    outcome: tokio::sync::watch::Sender<Option<PrepareOutcome>>,
}

impl ManagementHandle {
    pub fn new(backend: Arc<dyn PrepareBackend>) -> Self {
        Self {
            session: Arc::new(Mutex::new(ManagementSession::new())),
            inflight: std::sync::Mutex::new(HashMap::new()),
            outbound: std::sync::Mutex::new(None),
            epoch: std::sync::Mutex::new(None),
            backend,
        }
    }

    /// Installs the outbound channel once the bootstrap has fixed the credential.
    ///
    /// Replacing an existing one closes it first: a second bootstrap is refused by the
    /// session, so this only ever happens when the caller is rebuilding the same
    /// session, and a stale channel would answer into a generation that is gone.
    pub fn install_outbound(
        &self,
        client: Arc<super::management_client::ManagementClient>,
    ) -> Arc<super::management_client::ManagementClient> {
        let mut slot = self
            .outbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(previous) = slot.replace(Arc::clone(&client)) {
            previous.close();
        }
        client
    }

    /// The outbound channel, once the bootstrap installed one.
    pub fn outbound(&self) -> Option<Arc<super::management_client::ManagementClient>> {
        self.outbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Forgets the outbound channel, releasing everyone waiting on it.
    pub fn drop_outbound(&self) {
        let mut slot = self
            .outbound
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(client) = slot.take() {
            client.close();
        }
    }

    pub fn session(&self) -> Arc<Mutex<ManagementSession>> {
        Arc::clone(&self.session)
    }

    /// The epoch this session was bootstrapped under, if it was.
    ///
    /// A plain `std` lock rather than the session's async lock: the caller reads it
    /// while deciding what to install after the handshake, and awaiting the session
    /// lock there would make a synchronous decision depend on an async ordering. The
    /// value never changes after bootstrap -- a second bootstrap is refused -- so a
    /// second copy cannot drift from the session's.
    pub fn session_epoch(&self) -> Option<String> {
        self.epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Records the epoch the session was bootstrapped under.
    pub fn fix_epoch(&self, epoch: &str) {
        let mut slot = self
            .epoch
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(epoch.to_string());
        }
    }

    /// 执行一次 `_shell/prepare`。
    ///
    /// 认证在去重**之前**：未认证的请求不得占用或改变任何 in-flight 槽位，否则
    /// 一个本地进程可以用垃圾请求让合法准备的等待者提前拿到别人的结果。
    pub async fn prepare(
        &self,
        secret: &str,
        sidecar_epoch: &str,
        request: PrepareRequest,
        deadline: Deadline,
    ) -> Result<PrepareOutcome, String> {
        if self.session.lock().await.admit(secret, sidecar_epoch) != Admission::Accepted {
            return Err("unauthorized".into());
        }
        if deadline.is_expired() {
            // 预算已经用尽：不启动、不登记，直接说不确定。把它报成失败会诱使
            // 调用方在清除失败缓存后重试，那正好是「过期 epoch 也能触发启动」。
            return Err("the management deadline expired before the attempt started".into());
        }

        let key = request.scope.key.clone();
        let (inflight, started_here) = {
            let mut locks = self.lock_inflight();
            match locks.get(&key) {
                Some(existing) => (Arc::clone(existing), false),
                None => {
                    let (tx, _) = tokio::sync::watch::channel(None);
                    let entry = Arc::new(Inflight { outcome: tx });
                    locks.insert(key.clone(), Arc::clone(&entry));
                    (entry, true)
                }
            }
        };

        if !started_here {
            // 有人已经在为这个 scope 准备。等它的结果，绝不另起一个 daemon。
            let mut rx = inflight.outcome.subscribe();
            loop {
                if let Some(outcome) = rx.borrow().clone() {
                    return Ok(outcome);
                }
                let wait =
                    tokio::time::timeout(deadline.clamp(Duration::from_millis(200)), rx.changed());
                match wait.await {
                    Ok(Ok(())) => continue,
                    // 发送端消失（发起者被取消）或超时：都不替它猜结论。
                    Ok(Err(_)) | Err(_) if deadline.is_expired() => {
                        return Err("the prepare did not answer in time".into());
                    }
                    Ok(Err(_)) => return Err("the prepare was abandoned".into()),
                    Err(_) => continue,
                }
            }
        }

        // 只有发起者写结果并清理槽位。清理放在这里而不是 Drop：`Inflight` 还被
        // 等待者持有，Drop 时机不可控，槽位会因此滞留成一个永久的假「进行中」。
        let outcome = self.backend.prepare(request.clone()).await;
        // 瞬时失败不缓存：槽位清掉之后，下一次请求会真的重试。把失败留在表里
        // 会让一次网络抖动把这个项目钉死到进程退出。
        {
            let mut locks = self.lock_inflight();
            locks.remove(&key);
        }
        let _ = inflight.outcome.send(Some(outcome.clone()));
        Ok(outcome)
    }

    /// Runs one authenticated migration operation.
    ///
    /// Authentication is the same check `prepare` uses, and it happens before
    /// anything touches the instance: an unauthenticated frame must not be able to
    /// make a migration observable, let alone run one.
    async fn authorised(&self, secret: &str, sidecar_epoch: &str) -> Result<(), String> {
        if self.session.lock().await.admit(secret, sidecar_epoch) != Admission::Accepted {
            return Err("unauthorized".into());
        }
        Ok(())
    }

    /// Read-only migration preview.
    pub async fn migration_preview(
        &self,
        secret: &str,
        sidecar_epoch: &str,
        request: MigrationPreviewRequest,
        deadline: Deadline,
    ) -> Result<serde_json::Value, String> {
        self.authorised(secret, sidecar_epoch).await?;
        if deadline.is_expired() {
            return Err("the management deadline expired before the preview started".into());
        }
        self.backend.migration_preview(request).await
    }

    /// Confirms or reconciles one migration.
    pub async fn migration_confirm(
        &self,
        secret: &str,
        sidecar_epoch: &str,
        request: MigrationConfirmRequest,
        deadline: Deadline,
    ) -> Result<serde_json::Value, String> {
        self.authorised(secret, sidecar_epoch).await?;
        if deadline.is_expired() {
            return Err("the management deadline expired before the confirmation started".into());
        }
        self.backend.migration_confirm(request).await
    }

    /// Reads one migration operation.
    pub async fn migration_operation(
        &self,
        secret: &str,
        sidecar_epoch: &str,
        request: MigrationOperationRequest,
        deadline: Deadline,
    ) -> Result<serde_json::Value, String> {
        self.authorised(secret, sidecar_epoch).await?;
        if deadline.is_expired() {
            return Err("the management deadline expired before the read started".into());
        }
        self.backend.migration_operation(request).await
    }

    fn lock_inflight(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Inflight>>> {
        self.inflight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 当前进行中的 scope 数。会话关闭后必须回到 0。
    ///
    /// 只在测试里读：生产路径没有"数一数还剩几个"的需要，而它一旦被生产代码
    /// 依赖，就会诱使调用方轮询这个数字来代替等结果。
    #[cfg(test)]
    pub fn inflight_count(&self) -> usize {
        self.lock_inflight().len()
    }

    /// 同步关闭：摘掉所有进行中的槽位，让等待者立刻离开。
    ///
    /// 与 [`Self::close`] 的差别只在能否 await：`uninstall` 在同步上下文里被
    /// 调用（进程收尾），不能等一把 async 锁。
    pub fn close_now(&self) {
        let mut locks = self.lock_inflight();
        for (_, entry) in locks.drain() {
            let _ = entry.outcome.send(None);
        }
        // The outbound corridor goes with the session. A waiter parked on a root
        // authorization would otherwise burn its whole budget after the process had
        // already decided to exit, and the reply it waits for could only come from a
        // shell that is itself going away.
        self.drop_outbound();
    }

    /// 关闭会话并清空进行中的槽位（异步调用点用）。
    ///
    /// 等待者会因此看到「被放弃」而不是一个编造的结论：进程要退出了，替它们
    /// 断言 runtime 已就绪是撒谎。
    #[cfg(test)]
    pub fn close(&self) {
        self.close_now();
    }
}

/// 生产后端：真正走受管 bring-up。
///
/// 只有一条路径：`ensure_managed` + `BringUpIntent::Business`，且**不** attach
/// root、不恢复 worker、不申请租约。准备只回答"这个 scope 的 runtime 能不能用"，
/// 租约与 root 授权是调用方接下来各自的成功条件；把两件事合进 prepare 会让
/// "还在准备"看起来像"已经可用"。
pub struct ManagedPrepareBackend;

#[async_trait::async_trait]
impl PrepareBackend for ManagedPrepareBackend {
    async fn prepare(&self, request: PrepareRequest) -> PrepareOutcome {
        use yi_agent_supervisors::lifecycle::bringup::{
            BringUpIntent, BringUpResult, ensure_managed,
        };

        let deadline = Deadline::after(yi_agent_supervisors::lifecycle::bringup::STARTUP_BUDGET);
        let result = tokio::task::spawn_blocking(move || {
            ensure_managed(
                &request.scope,
                &request.expected,
                BringUpIntent::Business,
                deadline,
            )
        })
        .await;

        let result = match result {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                // An I/O failure is not a policy answer. Reporting it as `Blocked`
                // would claim we know the runtime cannot work; we only know this
                // attempt did not.
                return PrepareOutcome::Unknown {
                    reason: error.to_string(),
                };
            }
            Err(error) => {
                return PrepareOutcome::Unknown {
                    reason: format!("the bring-up task did not finish: {error}"),
                };
            }
        };

        match result {
            BringUpResult::Connected { identity, .. } => PrepareOutcome::Connected(identity),
            BringUpResult::VersionChoiceRequired { identity } => {
                PrepareOutcome::VersionChoiceRequired(identity)
            }
            BringUpResult::ManualActionRequired { reason } => PrepareOutcome::Blocked { reason },
        }
    }

    async fn migration_preview(
        &self,
        request: MigrationPreviewRequest,
    ) -> Result<serde_json::Value, String> {
        let gateway = production_gateway()?;
        // Blocking: the probe asks the platform (launchd, peer credentials, process
        // starts). Running it on the async runtime would stall every other management
        // frame for the length of a probe.
        let view = tokio::task::spawn_blocking(move || gateway.preview(&request))
            .await
            .map_err(|error| format!("the preview task did not finish: {error}"))??;
        serde_json::to_value(view).map_err(|error| format!("unserialisable preview: {error}"))
    }

    async fn migration_confirm(
        &self,
        request: MigrationConfirmRequest,
    ) -> Result<serde_json::Value, String> {
        let gateway = production_gateway()?;
        let view = tokio::task::spawn_blocking(move || gateway.confirm(&request))
            .await
            .map_err(|error| format!("the confirmation task did not finish: {error}"))??;
        serde_json::to_value(view).map_err(|error| format!("unserialisable operation: {error}"))
    }

    async fn migration_operation(
        &self,
        request: MigrationOperationRequest,
    ) -> Result<serde_json::Value, String> {
        let gateway = production_gateway()?;
        let operation_id = request.operation_id.clone();
        let view = tokio::task::spawn_blocking(move || gateway.operation(&operation_id))
            .await
            .map_err(|error| format!("the operation read did not finish: {error}"))??;
        serde_json::to_value(view).map_err(|error| format!("unserialisable operation: {error}"))
    }
}

/// The migration gateway for this sidecar's own instance.
///
/// One per process, not one per call: a preview token is an in-memory fact, and a
/// gateway rebuilt per call would always answer "this preview is not known here" --
/// turning every confirmation into a refusal that looks like a stale token. The
/// instance home is fixed for the life of the process (`YI_AGENT_HOME` is read at
/// startup by the launcher), so caching it cannot answer for a different home.
fn production_gateway()
-> Result<&'static yi_agent_supervisors::lifecycle::migration_gateway::Gateway, String> {
    use std::sync::OnceLock;
    static GATEWAY: OnceLock<
        Result<yi_agent_supervisors::lifecycle::migration_gateway::Gateway, String>,
    > = OnceLock::new();
    GATEWAY
        .get_or_init(|| {
            let home = yi_agent_store::paths::instance_home().ok_or_else(|| {
                "this process has no instance home, so it cannot manage a migration".to_string()
            })?;
            Ok(yi_agent_supervisors::lifecycle::migration_gateway::Gateway::system(home))
        })
        .as_ref()
        .map_err(|error| error.clone())
}

/// 壳启动 sidecar 时设置的环境标记。/// 壳启动 sidecar 时设置的环境标记。
///
/// 这个标记**不是**鉴权（任何人设一个环境变量都能设它），它只决定"是否尝试
/// 私有握手"。真正的边界是第一帧的秘密：没有它，`_shell/` 仍然全部拒绝。
pub const MANAGEMENT_ENV: &str = "YI_AGENT_LOCAL_MANAGEMENT";

/// 这个进程是否被要求在本地 stdio 上提供管理面。
pub fn requested_by_launcher() -> bool {
    std::env::var_os(MANAGEMENT_ENV).is_some_and(|value| !value.is_empty())
}

/// 判断一个已解析的 JSON 值是否是保留命名空间的帧。
///
/// 只看 `method` 字段，且**不**要求帧的其余部分合法：一段自称 `_shell/` 的畸形
/// 帧也必须留在私有面里被拒绝，而不是掉进业务解析的错误臂后得到一个
/// `parse_error` —— 那会让"畸形"成为一条绕过命名空间的路。
pub fn is_reserved_method(value: &serde_json::Value) -> bool {
    value
        .get("method")
        .and_then(|method| method.as_str())
        .is_some_and(is_reserved)
}

/// bootstrap 的 ACK。
///
/// 它没有 `id`：bootstrap 不是一次请求，而是这条会话的凭据。把它写成响应会
/// 诱使调用方用 id 去 correlate，而凭据的建立本来就不属于任何一个业务 id。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BootstrapAck {
    pub jsonrpc: &'static str,
    pub bootstrap: &'static str,
    /// 会话固定下来的 epoch。壳据此对账，不以自己记的为准。
    pub sidecar_epoch: String,
}

/// 构造 bootstrap 的应答。
///
/// `sidecar_epoch` 取**会话固定下来的**值，不回显请求自带的值：壳把一个和
/// 会话不同的 epoch 写进请求里时，ACK 必须让它看见差异，而不是附和它。
pub fn bootstrap_ack(outcome: &str, session_epoch: Option<&str>) -> BootstrapAck {
    BootstrapAck {
        jsonrpc: "2.0",
        bootstrap: match outcome {
            "accepted" => "accepted",
            "already-bootstrapped" => "already-bootstrapped",
            _ => "unauthorized",
        },
        sidecar_epoch: session_epoch.unwrap_or_default().to_string(),
    }
}

/// 一条拒绝帧。
///
/// `id` 只在被拒请求本身带 id 时才回显：编造一个 id 会让调用方以为某个它没发过
/// 的请求得到了答复。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Refusal {
    pub jsonrpc: &'static str,
    /// 被拒帧的 `id`（若它是一条带 id 的请求）。
    ///
    /// 必须回显：壳按 id 把结果投递给等待者，一条不带 id 的拒绝会让它一直等到
    /// 自己的 deadline，然后把"超时"报成原因——真正的拒绝理由就此丢失。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub error: String,
}

pub fn refusal(message: impl Into<String>) -> Refusal {
    Refusal {
        jsonrpc: "2.0",
        id: None,
        error: message.into(),
    }
}

/// 同 [`refusal`]，但回显被拒请求的 id。
pub fn refusal_for(id: Option<&serde_json::Value>, message: impl Into<String>) -> Refusal {
    Refusal {
        jsonrpc: "2.0",
        id: id.cloned(),
        error: message.into(),
    }
}

/// 一条 `_shell/` 请求。
///
/// `id` 是**字符串**，与业务的数字 id 分开：一条迟到的管理响应必须不可能被
/// 业务层的 `route_client_response` 当成某个权限请求的结果。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ManagementRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub sidecar_epoch: String,
    /// 建立会话时用过的那个凭据。壳在会话建立后每条请求都带上它。
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// 一条 `_shell/` 响应。
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct ManagementResponse {
    pub jsonrpc: &'static str,
    pub id: String,
    pub sidecar_epoch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ManagementResponse {
    fn ok(id: &str, epoch: &str, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.to_string(),
            sidecar_epoch: epoch.to_string(),
            result: Some(result),
            error: None,
        }
    }

    fn err(id: &str, epoch: &str, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.to_string(),
            sidecar_epoch: epoch.to_string(),
            result: None,
            error: Some(message.into()),
        }
    }
}

/// 分派一条 `_shell/` 请求。
///
/// 未知方法**不**回落到业务路由：保留命名空间是封闭的，一个本进程不认识的
/// `_shell/*` 只会得到 `unknown management method`。把未知方法交给业务分发
/// 会让"先发一个没人处理的名字"成为探测私有面的手段。
pub async fn dispatch(
    handle: &ManagementHandle,
    request: ManagementRequest,
    deadline: Deadline,
) -> ManagementResponse {
    // epoch 一律用会话固定的那个回显，不回显请求里自报的值：让响应携带请求方
    // 选定的 epoch，会把"旧响应不得完成新请求"这条判定重新交回给来包。
    let epoch = handle
        .session()
        .lock()
        .await
        .sidecar_epoch()
        .map(str::to_owned)
        .unwrap_or_default();
    let id = request.id.clone();
    // Read before the match consumes `request`: every arm authenticates with the
    // frame's own secret and epoch, never with a value the handler invents.
    let request_secret = request.secret.clone();
    let request_epoch = request.sidecar_epoch.clone();
    if !is_reserved(&request.method) {
        return ManagementResponse::err(&id, &epoch, "not a management method");
    }

    match request.method.as_str() {
        "_shell/prepare" => {
            let prepare: PrepareRequest = match serde_json::from_value(request.params.clone()) {
                Ok(prepare) => prepare,
                Err(error) => {
                    return ManagementResponse::err(
                        &id,
                        &epoch,
                        format!("malformed prepare request: {error}"),
                    );
                }
            };
            match handle
                .prepare(&request.secret, &request.sidecar_epoch, prepare, deadline)
                .await
            {
                Ok(outcome) => match serde_json::to_value(&outcome) {
                    Ok(value) => ManagementResponse::ok(&id, &epoch, value),
                    Err(error) => {
                        ManagementResponse::err(&id, &epoch, format!("unserialisable: {error}"))
                    }
                },
                Err(error) => ManagementResponse::err(&id, &epoch, error),
            }
        }
        MIGRATION_PREVIEW_METHOD => {
            let request: MigrationPreviewRequest =
                match serde_json::from_value(request.params.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        return ManagementResponse::err(
                            &id,
                            &epoch,
                            format!("malformed migration preview request: {error}"),
                        );
                    }
                };
            match handle
                .migration_preview(&request_secret, &request_epoch, request, deadline)
                .await
            {
                Ok(value) => ManagementResponse::ok(&id, &epoch, value),
                Err(error) => ManagementResponse::err(&id, &epoch, error),
            }
        }
        MIGRATION_CONFIRM_METHOD => {
            let request: MigrationConfirmRequest =
                match serde_json::from_value(request.params.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        return ManagementResponse::err(
                            &id,
                            &epoch,
                            format!("malformed migration confirm request: {error}"),
                        );
                    }
                };
            match handle
                .migration_confirm(&request_secret, &request_epoch, request, deadline)
                .await
            {
                Ok(value) => ManagementResponse::ok(&id, &epoch, value),
                Err(error) => ManagementResponse::err(&id, &epoch, error),
            }
        }
        MIGRATION_OPERATION_METHOD => {
            let request: MigrationOperationRequest =
                match serde_json::from_value(request.params.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        return ManagementResponse::err(
                            &id,
                            &epoch,
                            format!("malformed migration operation request: {error}"),
                        );
                    }
                };
            match handle
                .migration_operation(&request_secret, &request_epoch, request, deadline)
                .await
            {
                Ok(value) => ManagementResponse::ok(&id, &epoch, value),
                Err(error) => ManagementResponse::err(&id, &epoch, error),
            }
        }
        "_shell/bootstrap" => ManagementResponse::err(
            &id,
            &epoch,
            "bootstrap must be the first frame and carries no id",
        ),
        other => {
            ManagementResponse::err(&id, &epoch, format!("unknown management method `{other}`"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use yi_agent_lifecycle::types::{ArtifactIdentity, RuntimeScope};

    struct CountingBackend {
        calls: AtomicUsize,
        scopes: Mutex<Vec<String>>,
        delay: Duration,
        outcome: PrepareOutcome,
    }

    impl CountingBackend {
        fn new(outcome: PrepareOutcome) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                scopes: Mutex::new(Vec::new()),
                delay: Duration::from_millis(50),
                outcome,
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl PrepareBackend for CountingBackend {
        async fn prepare(&self, request: PrepareRequest) -> PrepareOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.scopes.lock().await.push(request.scope.key.clone());
            tokio::time::sleep(self.delay).await;
            self.outcome.clone()
        }
    }

    fn request(scope_key: &str, attempt: &str) -> PrepareRequest {
        PrepareRequest {
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
            attempt_id: attempt.to_string(),
        }
    }

    fn budget() -> Deadline {
        Deadline::after(Duration::from_secs(5))
    }

    #[tokio::test]
    async fn an_unbootstrapped_session_cannot_prepare() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "should not run".into(),
        });
        let handle = ManagementHandle::new(backend.clone());

        let result = handle
            .prepare("secret", "epoch-1", request("/a", "attempt-1"), budget())
            .await;

        assert_eq!(result.unwrap_err(), "unauthorized");
        assert_eq!(backend.calls(), 0, "a stranger must not start a runtime");
    }

    #[tokio::test]
    async fn a_wrong_secret_is_refused() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "should not run".into(),
        });
        let handle = ManagementHandle::new(backend.clone());
        let mut session = ManagementSession::new();
        assert_eq!(
            session.bootstrap(&BootstrapFrame {
                secret: "right".into(),
                sidecar_epoch: "epoch-1".into()
            }),
            Admission::Accepted
        );
        *handle.session().lock().await = session;

        let result = handle
            .prepare("wrong", "epoch-1", request("/a", "attempt-1"), budget())
            .await;

        assert_eq!(result.unwrap_err(), "unauthorized");
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn an_expired_epoch_is_refused() {
        let backend = CountingBackend::new(PrepareOutcome::Connected(
            yi_agent_lifecycle::types::RuntimeIdentity {
                scope_key: "/a".into(),
                daemon_instance_id: None,
                guardian_build_id: None,
                daemon_build_id: None,
                mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                business_protocol: Some(2),
                daemon_display_version: None,
                guardian_display_version: None,
                capabilities: vec![],
                schema_min: 0,
                schema_max: 15,
            },
        ));
        let handle = ManagementHandle::new(backend.clone());
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        // 同一个秘密、旧 epoch：一条跨越代际的迟到请求不能被当成新请求。
        let result = handle
            .prepare("s", "epoch-0", request("/a", "attempt-1"), budget())
            .await;

        assert_eq!(result.unwrap_err(), "unauthorized");
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn an_expired_deadline_starts_nothing() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "should not run".into(),
        });
        let handle = ManagementHandle::new(backend.clone());
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        let expired = Deadline::after(Duration::ZERO);
        let result = handle
            .prepare("s", "epoch-1", request("/a", "attempt-1"), expired)
            .await;

        assert!(result.is_err());
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn two_concurrent_prepares_of_one_scope_start_it_once() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "transient".into(),
        });
        let handle = Arc::new(ManagementHandle::new(backend.clone()));
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        let first = {
            let handle = Arc::clone(&handle);
            tokio::spawn(async move {
                handle
                    .prepare("s", "epoch-1", request("/a", "attempt-1"), budget())
                    .await
            })
        };
        let second = {
            let handle = Arc::clone(&handle);
            tokio::spawn(async move {
                handle
                    .prepare("s", "epoch-1", request("/a", "attempt-2"), budget())
                    .await
            })
        };
        let (first, second) = (first.await.unwrap(), second.await.unwrap());

        assert_eq!(backend.calls(), 1, "one runtime start per scope");
        assert_eq!(first, second, "both callers see the same attempt's result");
        assert_eq!(handle.inflight_count(), 0);
    }

    #[tokio::test]
    async fn different_scopes_do_not_serialize_behind_one_another() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "transient".into(),
        });
        let handle = Arc::new(ManagementHandle::new(backend.clone()));
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        let started = std::time::Instant::now();
        let handles: Vec<_> = (0..4)
            .map(|index| {
                let handle = Arc::clone(&handle);
                tokio::spawn(async move {
                    handle
                        .prepare(
                            "s",
                            "epoch-1",
                            request(&format!("/scope-{index}"), "attempt"),
                            budget(),
                        )
                        .await
                })
            })
            .collect();
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        assert_eq!(backend.calls(), 4);
        // Each backend call sleeps 50ms. Serialising four would need 200ms.
        assert!(
            started.elapsed() < Duration::from_millis(180),
            "different scopes must not queue: took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_transient_failure_is_not_cached() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "transient".into(),
        });
        let handle = ManagementHandle::new(backend.clone());
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        handle
            .prepare("s", "epoch-1", request("/a", "attempt-1"), budget())
            .await
            .unwrap();
        handle
            .prepare("s", "epoch-1", request("/a", "attempt-2"), budget())
            .await
            .unwrap();

        assert_eq!(
            backend.calls(),
            2,
            "a failure must not pin the project until the process exits"
        );
    }

    #[tokio::test]
    async fn closing_the_session_drains_inflight_attempts() {
        let backend: Arc<dyn PrepareBackend> = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "transient".into(),
        });
        let handle = Arc::new(ManagementHandle::new(backend));
        let mut session = ManagementSession::new();
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        *handle.session().lock().await = session;

        let waiter = {
            let handle = Arc::clone(&handle);
            tokio::spawn(async move {
                handle
                    .prepare("s", "epoch-1", request("/a", "attempt"), budget())
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(handle.inflight_count(), 1);
        handle.close();

        // The backend still finishes; the slot must be gone afterwards.
        let _ = waiter.await.unwrap();
        assert_eq!(handle.inflight_count(), 0);
    }

    /// A root authorization parked on the corridor must not outlive the session.
    ///
    /// The waiter's own budget is five seconds, and the reply it wants can only come
    /// from a shell that is going away with us. Closing has to wake it *now*, with a
    /// refusal, rather than let it sit out the whole budget after the reason is known.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn closing_the_session_releases_a_waiting_root_authorization() {
        use crate::server::management_client::{
            ManagementClient, ManagementWrite, ShellRootAuthority,
        };
        use yi_agent_lifecycle::foreground::RootBindIntent;
        use yi_agent_lifecycle::production::RootIntentRequest;
        use yi_agent_subagent::managed_binding::ManagedRootAuthority;

        struct Sink;

        #[async_trait::async_trait]
        impl ManagementWrite for Sink {
            async fn write_frame(&self, _frame: &serde_json::Value) -> Result<(), String> {
                Ok(())
            }
        }

        let handle = Arc::new(ManagementHandle::new(CountingBackend::new(
            PrepareOutcome::Unknown {
                reason: "unused".into(),
            },
        )));
        let client = Arc::new(ManagementClient::new("s", "epoch-1", Arc::new(Sink)));
        handle.install_outbound(Arc::clone(&client));

        // Started on the multi-threaded test runtime because `ShellRootAuthority`
        // parks its caller while another task answers; that is exactly the shape that
        // must not be left waiting after the session closes.
        let authority = ShellRootAuthority::new(Arc::clone(&client));
        let waiting = tokio::spawn(async move {
            let intent = RootIntentRequest {
                sidecar_epoch: "epoch-1".into(),
                scope_key: "/p/runtime".into(),
                client_run_id: "run-1".into(),
                intent: RootBindIntent {
                    operation_id: "op".into(),
                    thread_key: "thread:a".into(),
                    workspace: "/p".into(),
                    extra_roots: Vec::new(),
                    previous_root_capability: None,
                },
            };
            authority.authorize(&intent, Deadline::after(Duration::from_secs(30)))
        });
        // Let the request reach the pending table before closing.
        let entered = tokio::time::timeout(Duration::from_secs(5), async {
            while client.pending_count() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(entered.is_ok(), "the authorize never registered as pending");

        let started = std::time::Instant::now();
        handle.close_now();
        let outcome = tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("closing must wake the waiter, not leave it to its budget")
            .unwrap();
        assert!(outcome.is_err(), "a closed session cannot grant authority");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the waiter must be released at close, not at its own deadline"
        );
        assert!(
            handle.outbound().is_none(),
            "the corridor must not outlive the session it belongs to"
        );
    }

    #[test]
    fn the_reserved_prefix_is_closed_not_enumerated() {
        // A whitelist would pass this one and then quietly admit tomorrow's method.
        for method in [
            "_shell/",
            "_shell/rootIntent",
            "_shell/replaceRoots",
            "_shell/migrationPreview",
            "_shell/migrationConfirm",
            "_shell/migrationOperation",
            "_shell/bootstrap",
        ] {
            assert!(is_reserved(method), "{method} must be reserved");
        }
        // Near-misses are business methods: rejecting them would break real calls.
        for method in ["_shell", "shell/prepare", "_SHELL/prepare", "thread/start"] {
            assert!(!is_reserved(method), "{method} must not be reserved");
        }
    }

    #[tokio::test]
    async fn an_unknown_reserved_method_is_not_routed_to_business() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "should not run".into(),
        });
        let handle = Arc::new(ManagementHandle::new(backend.clone()));
        {
            let session_handle = handle.session();
            let mut session = session_handle.lock().await;
            session.bootstrap(&BootstrapFrame {
                secret: "s".into(),
                sidecar_epoch: "epoch-1".into(),
            });
        }

        // Tomorrow's method name, today: it must be refused by the namespace
        // rather than fall through to business dispatch.
        let response = dispatch(
            &handle,
            ManagementRequest {
                id: "m-1".into(),
                method: "_shell/newFutureMethod".into(),
                sidecar_epoch: "epoch-1".into(),
                secret: "s".into(),
                params: serde_json::json!({}),
            },
            budget(),
        )
        .await;

        assert!(
            response
                .error
                .unwrap()
                .contains("unknown management method"),
            "an unregistered reserved method must not reach business routing"
        );
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn a_prepare_without_the_bootstrap_secret_is_refused() {
        let backend = CountingBackend::new(PrepareOutcome::Unknown {
            reason: "should not run".into(),
        });
        let handle = Arc::new(ManagementHandle::new(backend.clone()));
        {
            let session_handle = handle.session();
            let mut session = session_handle.lock().await;
            session.bootstrap(&BootstrapFrame {
                secret: "s".into(),
                sidecar_epoch: "epoch-1".into(),
            });
        }

        let response = dispatch(
            &handle,
            ManagementRequest {
                id: "m-2".into(),
                method: "_shell/prepare".into(),
                sidecar_epoch: "epoch-1".into(),
                secret: "guessed".into(),
                params: serde_json::to_value(request("/a", "attempt")).unwrap(),
            },
            budget(),
        )
        .await;

        assert_eq!(response.error.as_deref(), Some("unauthorized"));
        assert_eq!(response.id, "m-2", "the id is echoed even on refusal");
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn a_response_carries_the_session_epoch_not_the_requests() {
        let backend = CountingBackend::new(PrepareOutcome::Connected(
            yi_agent_lifecycle::types::RuntimeIdentity {
                scope_key: "/a".into(),
                daemon_instance_id: None,
                guardian_build_id: None,
                daemon_build_id: None,
                mode: yi_agent_lifecycle::types::RuntimeMode::Managed,
                business_protocol: Some(2),
                daemon_display_version: None,
                guardian_display_version: None,
                capabilities: vec![],
                schema_min: 0,
                schema_max: 15,
            },
        ));
        let handle = Arc::new(ManagementHandle::new(backend));
        {
            let session_handle = handle.session();
            let mut session = session_handle.lock().await;
            session.bootstrap(&BootstrapFrame {
                secret: "s".into(),
                sidecar_epoch: "epoch-1".into(),
            });
        }

        let response = dispatch(
            &handle,
            ManagementRequest {
                id: "m-3".into(),
                method: "_shell/prepare".into(),
                // A caller claiming a different epoch must not get it echoed back:
                // the shell correlates on the epoch it minted, not on ours.
                sidecar_epoch: "epoch-1".into(),
                secret: "s".into(),
                params: serde_json::to_value(request("/a", "attempt")).unwrap(),
            },
            budget(),
        )
        .await;

        assert_eq!(response.sidecar_epoch, "epoch-1");
        assert!(response.error.is_none(), "{response:?}");
    }

    #[test]
    fn an_empty_secret_does_not_establish_a_session() {
        let mut session = ManagementSession::new();
        assert_eq!(
            session.bootstrap(&BootstrapFrame {
                secret: String::new(),
                sidecar_epoch: "epoch-1".into()
            }),
            Admission::Unauthorized
        );
        assert!(!session.is_bootstrapped());
    }

    #[test]
    fn business_frames_are_refused_until_bootstrap_completes() {
        let mut session = ManagementSession::new();
        assert_eq!(session.admit_business(), Admission::Unauthorized);
        session.bootstrap(&BootstrapFrame {
            secret: "s".into(),
            sidecar_epoch: "epoch-1".into(),
        });
        assert_eq!(session.admit_business(), Admission::Accepted);
    }

    #[test]
    fn a_session_cannot_be_rebootstrapped() {
        let mut session = ManagementSession::new();
        assert_eq!(
            session.bootstrap(&BootstrapFrame {
                secret: "s".into(),
                sidecar_epoch: "epoch-1".into()
            }),
            Admission::Accepted
        );
        // A second bootstrap must not displace the first: the shell owns this
        // connection, and letting a later caller take it over would let it also
        // take over the epoch.
        assert_eq!(
            session.bootstrap(&BootstrapFrame {
                secret: "s".into(),
                sidecar_epoch: "epoch-2".into()
            }),
            Admission::AlreadyBootstrapped
        );
        assert_eq!(session.sidecar_epoch(), Some("epoch-1"));
    }

    #[test]
    fn the_credential_is_never_printed() {
        let session = {
            let mut session = ManagementSession::new();
            session.bootstrap(&BootstrapFrame {
                secret: "plaintext-secret".into(),
                sidecar_epoch: "epoch-1".into(),
            });
            session
        };
        assert!(!format!("{session:?}").contains("plaintext-secret"));
    }
}
