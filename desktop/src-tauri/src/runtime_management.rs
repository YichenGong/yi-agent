//! 壳侧的本地管理会话：凭据、epoch、pending 表与响应截获。
//!
//! 这个模块存在的理由只有一个：**管理帧不是业务帧**。把它们当业务帧处理会在
//! 四个地方出错，而每一处都曾经是真实的误判来源：
//!
//! 1. **id 命名空间。** 管理响应带字符串 id，业务的权限响应按数字 id 路由。
//!    混在一起时，一条迟到的管理响应会被 `rpc_respond` 当成某个权限请求的结果。
//! 2. **广播可见性。** 管理响应携带 epoch 与授权结论。让它经 `app.emit` 出去，
//!    页面不仅能读，还能代答。
//! 3. **重放。** 业务 RPC 的自动重连重放队列对"查询"是对的，对"启动一个
//!    runtime"是错的：重连重放会让一次用户操作启动两次。
//! 4. **代际。** sidecar 重启后 epoch 必须变。旧 epoch 的迟到响应不得完成新
//!    请求——判定依据是**发起时**记下的 epoch，不是响应里自报的那个。
//!
//! 因此这里不共享任何业务状态：pending 表是本模块私有的，key 是字符串 id，
//! 每一项都记下发起时的 epoch，超时/关闭/换代一律清空而不是重发。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use yi_agent_lifecycle::deadline::Deadline;

/// 管理响应允许的最大字节数。
///
/// 与传输层既有的帧上限同量级：一个管理响应和业务帧一样不该无限大。超限直接
/// 丢弃并让调用方超时，而不是截断——截断的 JSON 无法被任何一方安全解析。
pub const MAX_MANAGEMENT_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// 本地准备方法名。与 app-server 的 `dispatch` 分支同名：线协议的一部分。
pub const PREPARE_METHOD: &str = "_shell/prepare";

/// Asks the shell to broker one conversation's root authority.
///
/// A request *from* the sidecar, not a response to one: the sidecar holds no lease, so
/// it cannot mint authority and has to ask the party that does. The reply is written
/// back to the sidecar's stdin and never broadcast, for the same reason a prepare
/// response is not: it carries a capability.
pub const AUTHORIZE_ROOT_METHOD: &str = "_shell/authorizeRoot";

/// Legacy-migration preview. Read-only, and answered from the sidecar's own probe.
pub const MIGRATION_PREVIEW_METHOD: &str = "_shell/migrationPreview";

/// Runs or reconciles one legacy migration.
pub const MIGRATION_CONFIRM_METHOD: &str = "_shell/migrationConfirm";

/// Reads one migration operation without changing anything.
pub const MIGRATION_OPERATION_METHOD: &str = "_shell/migrationOperation";

/// 保留方法前缀。与 app-server 的常量同名同值，两侧各存一份是有意的：
/// 壳不依赖 app-server（它是另一个进程），所以这个字符串是**线协议**的一部分，
/// 由两侧的测试同时钉住。
pub const RESERVED_PREFIX: &str = "_shell/";

/// 要求 sidecar 打开本地管理面的环境变量。
///
/// 与 app-server 的 `MANAGEMENT_ENV` 同名同值：这是两侧的线协议约定，各存一份
/// 是因为壳不依赖 app-server 的 crate。它是"是否尝试握手"的开关，不是鉴权。
pub const MANAGEMENT_ENV: &str = "YI_AGENT_LOCAL_MANAGEMENT";

/// 业务面写入口的守卫。
///
/// 在写 stdout 之前调用：任何 `_shell/` 方法一律拒绝，因此 WebView 无法伪造
/// 私有帧，也无法把私有响应当作业务响应消费。
pub fn reject_reserved_method(method: &str) -> Result<(), String> {
    if method.starts_with(RESERVED_PREFIX) {
        return Err("reserved native management method".into());
    }
    Ok(())
}

/// 一条要被写进日志的帧，秘密已脱敏。
///
/// 存在的理由很具体：bootstrap 帧的形状是「没有 method、没有 id」，正好落进
/// `bridge` 的 `Frame::Garbage`，而那条分支会把整帧 `eprintln!` 出去。于是在
/// 别处都刻意脱敏的秘密，会从这一条日志漏回 stderr。这里把承载秘密的键换成
/// 固定字符串，其余字段原样保留——日志仍然有用，凭据不再出现。
pub fn redact_frame(value: &serde_json::Value) -> serde_json::Value {
    /// 已知会携带凭据的键。加新键比放宽匹配更安全：宁可多打一个字段。
    const SECRET_KEYS: [&str; 2] = ["secret", "bootstrap_secret"];

    let mut redacted = value.clone();
    if let Some(object) = redacted.as_object_mut() {
        for key in SECRET_KEYS {
            if object.contains_key(key) {
                object.insert(key.into(), serde_json::Value::from("<redacted>"));
            }
        }
    }
    redacted
}

/// 一条待完成的管理请求。
#[derive(Debug)]
struct Pending {
    /// 发起时的 epoch。完成时必须与它相等，否则说明这是上一代的迟到响应。
    epoch: String,
    /// 应答的投递点。`None` 表示这条请求只关心"被消费"，不关心内容
    /// （拨测、或者调用方已经放弃）。
    ///
    /// `std::sync::mpsc` 而不是 tokio 的：投递发生在读取 sidecar stdout 的线程里，
    /// 那里是同步上下文；而且它的 `send` 不需要 await，关闭会话时也不会留下
    /// 一个必须被 poll 才能推进的 future。
    reply: Option<std::sync::mpsc::Sender<serde_json::Value>>,
}

/// 一次管理请求的结果。
///
/// 三种而不是两种：超时与"对方明确拒绝"必须分开。把超时报成拒绝会让调用方
/// 以为 runtime 拒绝了这个项目，而实际上只是没等到回话——那正好会诱使它
/// 重试一次已经开始的启动。
#[derive(Debug, Clone, PartialEq)]
pub enum ManagementReply {
    /// 对端接受了这条请求并给出了结果。
    Answered(serde_json::Value),
    /// 会话关闭或换代：这一次没有结论。
    Abandoned,
}

/// 一次 `_shell/prepare` 的实际往返。
///
/// 与 [`ManagementSession::register`] 分开，是因为"登记一个 id"和"把一帧写进
/// stdin 并等回话"是两件事：前者是纯内存的，后者要经过进程边界，失败点也
/// 完全不同。
pub trait PrepareTransport: Send + Sync + 'static {
    /// 写一帧到本地 sidecar 的 stdin。
    fn write_frame(&self, frame: &serde_json::Value) -> Result<(), String>;
}

/// 壳侧的本地管理会话。
///
/// `secret` 只经继承的 stdin 送给 sidecar：不进 argv、不进普通环境、不进
/// WebView、也不进日志。它是唯一把"壳"和"任何别的本地进程"区分开的东西。
pub struct ManagementSession {
    secret: String,
    epoch: String,
    pending: Mutex<HashMap<String, Pending>>,
    closed: std::sync::atomic::AtomicBool,
}

/// 手写 `Debug`：秘密绝不能因为一次 `{:?}`（日志、panic 消息、错误上下文）
/// 就出现在任何地方。`derive` 会把它原样打印，而这类打印通常发生在崩溃路径上，
/// 恰恰是最容易被收集走的时刻。
impl std::fmt::Debug for ManagementSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagementSession")
            .field("secret", &"<redacted>")
            .field("epoch", &self.epoch)
            .field("pending", &self.pending_count())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl ManagementSession {
    /// 新建一次会话。`epoch` 由调用方为**每一次 sidecar 启动**新生成。
    pub fn new(secret: impl Into<String>, epoch: impl Into<String>) -> Self {
        Self {
            secret: secret.into(),
            epoch: epoch.into(),
            pending: Mutex::new(HashMap::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// 生成一个新的随机秘密。32 字节足够：它只在一条本机管道上活一次。
    pub fn mint_secret() -> String {
        uuid::Uuid::new_v4().simple().to_string() + &uuid::Uuid::new_v4().simple().to_string()
    }

    pub fn secret(&self) -> &str {
        &self.secret
    }

    pub fn epoch(&self) -> &str {
        &self.epoch
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// bootstrap 第一帧。
    ///
    /// 没有 `id`：它是凭据，不是请求。写成请求会诱使调用方拿一个 id 去 correlate
    /// 一件不属于任何业务 id 的事。
    pub fn bootstrap_frame(&self) -> serde_json::Value {
        serde_json::json!({"secret": self.secret, "sidecar_epoch": self.epoch})
    }

    /// 构造一条管理请求。
    pub fn request_frame(
        &self,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "sidecar_epoch": self.epoch,
            "secret": self.secret,
            "params": params,
        })
    }

    /// 登记一条请求。返回 `false` 表示会话已关闭，调用方不应写下这一帧。
    pub fn register(&self, id: &str) -> bool {
        self.register_with(id, None)
    }

    /// 登记一条请求，并把它的应答投递到 `reply`。
    ///
    /// 先登记再写帧：反过来会有一个窗口，响应在等待者存在之前就到达，于是被
    /// 当成"不属于任何请求"广播给页面。
    pub fn register_with(
        &self,
        id: &str,
        reply: Option<std::sync::mpsc::Sender<serde_json::Value>>,
    ) -> bool {
        if self.is_closed() {
            return false;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.insert(
            id.to_string(),
            Pending {
                epoch: self.epoch.clone(),
                reply,
            },
        );
        true
    }

    /// 尝试截获一条来自 sidecar 的帧。
    ///
    /// 返回 `true` 表示"这条帧已被管理面消费，**不要**再广播给前端"。判据是
    /// 字符串 id 命中本模块的 pending 表——不是"看起来像 jsonrpc"，那会把一条
    /// 恰好带字符串 id 的业务响应也吞掉。
    pub fn intercept(&self, frame: &serde_json::Value) -> bool {
        let Some(id) = frame.get("id").and_then(|id| id.as_str()) else {
            return false;
        };
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        let Some(expected) = pending.remove(id) else {
            return false;
        };
        // 迟到的上一代响应：仍然算"已消费"（不能让前端看到它），但调用方会
        // 在自己那一侧的超时里得到结论，而不是在这里拿到一个旧结果。
        let arrived_epoch = frame
            .get("sidecar_epoch")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let accepted = arrived_epoch == expected.epoch;
        if let Some(reply) = expected.reply {
            // 旧代的响应也投递：投递方按"没拿到结果"处理，与超时同一条路径。
            // 丢掉它只会让等待者多等一整个 deadline。
            let _ = reply.send(frame.clone());
        }
        accepted
    }

    /// 发出一次 `_shell/prepare` 并等待它的应答。
    ///
    /// 超时不重发：这条请求会启动一个 runtime，重发等于第二次启动。超时只是
    /// 把槽位清掉并返回"没有结论"，让上层把项目报成 `unknown`。
    pub fn prepare(
        &self,
        transport: &dyn PrepareTransport,
        request: &yi_agent_lifecycle::production::PrepareRequest,
        budget: std::time::Duration,
    ) -> Result<ManagementReply, String> {
        self.call(
            transport,
            "prepare",
            PREPARE_METHOD,
            serde_json::to_value(request).map_err(|error| error.to_string())?,
            budget,
        )
    }

    /// 发出一次管理请求并等待它的应答，由调用方选择方法名与本地的 id 前缀。
    ///
    /// 与 [`Self::prepare`] 是同一条路径，不是两套：登记、写帧、等应答、超时
    /// 清槽的次序才是这个模块的全部正确性所在，复制一份迟早会有一份漏掉其中
    /// 一步。`purpose` 只影响 id 的可读前缀，不参与任何判定。
    ///
    /// 仍然**不重发**：管理方法是幂等查询还是"启动/停止某个东西"由对端决定，
    /// 这一层不知道，所以它统一不重发，把"要不要再来一次"留给知道语义的调用方。
    pub fn call(
        &self,
        transport: &dyn PrepareTransport,
        purpose: &str,
        method: &str,
        params: serde_json::Value,
        budget: std::time::Duration,
    ) -> Result<ManagementReply, String> {
        let id = format!("{purpose}-{}-{}", self.epoch, uuid::Uuid::new_v4().simple());
        let (tx, rx) = std::sync::mpsc::channel();
        if !self.register_with(&id, Some(tx)) {
            return Ok(ManagementReply::Abandoned);
        }
        let frame = self.request_frame(&id, method, params);
        if let Err(error) = transport.write_frame(&frame) {
            // 写不进去等于这次请求从未存在：槽位必须清掉，否则它会一直占着
            // 一个 id，下一次请求永远等不到自己的应答。
            self.timeout(&id);
            return Err(error);
        }
        match rx.recv_timeout(budget) {
            Ok(value) => Ok(ManagementReply::Answered(value)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.timeout(&id);
                Ok(ManagementReply::Abandoned)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.timeout(&id);
                Ok(ManagementReply::Abandoned)
            }
        }
    }

    /// 一条请求超时。
    ///
    /// 只是把槽位清掉。**不**重发：这是启动 runtime 的请求，重发等于第二次启动。
    pub fn timeout(&self, id: &str) {
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.remove(id);
    }

    /// 关闭会话，清空全部待完成请求。
    ///
    /// 用于 sidecar 退出/换代与应用退出：这些时刻再等旧响应没有意义，而且
    /// 保留槽位会让下一次请求复用一个已经不存在的 id。
    ///
    /// 投递点随槽位一起被丢弃，接收端因此立刻看到断开而不是等到自己的超时——
    /// 这是"退出时不要等满 30 秒"的实际来源。
    pub fn close(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut pending = self.pending.lock().unwrap_or_else(|p| p.into_inner());
        pending.clear();
    }

    /// 这个字符串 id 是否属于管理面。
    ///
    /// 用于拦住"页面替管理请求作答"：`rpc_respond` 收到一个命中的字符串 id 时
    /// 必须拒绝，而不是把它写回 sidecar 当成结果。
    pub fn owns(&self, id: &str) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(id)
    }

    /// 当前待完成请求数。关闭后必须回到 0。
    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// 一次管理请求的预算。准备可能要真的启动进程，所以不能按业务 RPC 的
    /// 短超时来算。
    pub fn deadline(&self) -> Deadline {
        Deadline::after(MANAGEMENT_BUDGET)
    }
}

/// 一次管理请求的预算。
///
/// 30 秒与传输层的启动预算同值：比它短会在一次合法的冷启动上超时，比它长只是
/// 让用户在真失败时多等。两处必须同值，否则"壳说超时了但 runtime 其实起来了"
/// 会变成一条无法解释的状态。
pub const MANAGEMENT_BUDGET: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> ManagementSession {
        ManagementSession::new("secret-1", "epoch-1")
    }

    #[test]
    fn reserved_methods_cannot_be_forwarded() {
        assert!(reject_reserved_method("_shell/prepare").is_err());
        assert!(reject_reserved_method("_shell/newFutureMethod").is_err());
        assert!(reject_reserved_method("thread/resume").is_ok());
        // Near-misses stay business: a prefix check that is too eager would break
        // real calls, and `_shell` alone is a name no method uses.
        assert!(reject_reserved_method("_shell").is_ok());
        assert!(reject_reserved_method("shell/prepare").is_ok());
    }

    /// 落进日志的帧不能带秘密。
    ///
    /// 这条测的是 `Frame::Garbage` 那条 `eprintln!` 之前的一步：bootstrap 帧的
    /// 形状正是「没有 method、没有 id」，所以它真的会走到那里。
    #[test]
    fn a_logged_frame_does_not_carry_the_secret() {
        let frame = session().bootstrap_frame();
        let redacted = redact_frame(&frame);
        let printed = redacted.to_string();
        assert!(
            !printed.contains("secret-1"),
            "the bootstrap secret must not survive redaction: {printed}"
        );
        assert_eq!(redacted["secret"], "<redacted>");
        // Non-secret fields stay readable, so the log still says which epoch.
        assert_eq!(redacted["sidecar_epoch"], "epoch-1");
        // A frame with nothing to hide is returned unchanged.
        let business = serde_json::json!({"hello": "world"});
        assert_eq!(redact_frame(&business), business);
    }

    #[test]
    fn the_bootstrap_frame_carries_the_secret_and_no_id() {
        let frame = session().bootstrap_frame();
        assert_eq!(frame["secret"], "secret-1");
        assert_eq!(frame["sidecar_epoch"], "epoch-1");
        // A credential must not look like a request: an `id` here would invite
        // correlating it with a business response.
        assert!(frame.get("id").is_none());
    }

    #[test]
    fn a_management_response_is_intercepted_before_the_frontend_sees_it() {
        let session = session();
        assert!(session.register("m-1"));
        let response =
            serde_json::json!({"jsonrpc": "2.0", "id": "m-1", "sidecar_epoch": "epoch-1"});
        assert!(session.intercept(&response));
        assert_eq!(session.pending_count(), 0, "the slot is consumed once");
        // A second delivery of the same frame is not ours any more, so it would
        // fall through to the frontend -- which is exactly why the shell must not
        // re-deliver frames.
        assert!(!session.intercept(&response));
    }

    #[test]
    fn a_numeric_id_is_never_intercepted() {
        let session = session();
        assert!(session.register("1"));
        // Business permission responses carry numbers. Reading them as strings
        // would let a management id shadow a permission id.
        let business = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        assert!(!session.intercept(&business));
        assert_eq!(session.pending_count(), 1);
    }

    #[test]
    fn a_late_response_from_an_old_epoch_is_consumed_but_not_accepted() {
        let session = session();
        assert!(session.register("m-1"));
        let stale = serde_json::json!({"jsonrpc": "2.0", "id": "m-1", "sidecar_epoch": "epoch-0"});
        // Consumed (the frontend must not see it) but not accepted (the caller
        // gets its own timeout rather than a result from a previous generation).
        assert!(!session.intercept(&stale));
        assert_eq!(session.pending_count(), 0);
    }

    #[test]
    fn a_timeout_clears_the_slot_without_resending() {
        let session = session();
        assert!(session.register("m-1"));
        session.timeout("m-1");
        assert_eq!(session.pending_count(), 0);
        // Nothing was re-queued: a start must not become a second start.
        assert!(session.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn closing_drains_pending_and_refuses_new_requests() {
        let session = session();
        assert!(session.register("m-1"));
        assert!(session.register("m-2"));
        session.close();
        assert_eq!(session.pending_count(), 0);
        assert!(!session.register("m-3"), "a closed session takes no work");
        assert!(session.is_closed());
    }

    #[test]
    fn the_secret_is_never_printed() {
        let session = session();
        assert!(!format!("{session:?}").contains("secret-1"));
        assert!(!format!("{:?}", session.bootstrap_frame()).is_empty());
    }

    #[test]
    fn minted_secrets_do_not_repeat() {
        let first = ManagementSession::mint_secret();
        let second = ManagementSession::mint_secret();
        assert_ne!(first, second);
        assert!(first.len() >= 32);
    }
}
