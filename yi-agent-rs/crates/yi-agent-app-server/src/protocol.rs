//! JSON-RPC 2.0 信封与 yi-agent 协议类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC_VERSION: &str = "2.0";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 已配对遥控设备被授予的权限级。
///
/// `Ord` 从弱到强：`Observe < Control < Admin`。新配对设备默认 `Control`
/// （决策 A）。Task 3 会围绕它扩展配对/设备 RPC。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Observe,
    Control,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Num(i64),
    Str(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestEnvelope {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    /// 成功响应时省略;与 `error` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// 错误响应时省略;与 `result` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// 客户端 → 服务端、针对反向请求的响应(id 与请求匹配,无 `method`)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientResponse {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

/// 服务端 → 客户端的反向请求(带 `id`,需要客户端回响应)。
#[derive(Debug, Clone, Serialize)]
pub struct ReverseRequest<'a> {
    pub jsonrpc: &'a str,
    pub id: String,
    pub method: &'a str,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self::new(-32700, msg)
    }
    pub fn invalid_request(msg: impl Into<String>) -> Self {
        Self::new(-32600, msg)
    }
    pub fn method_not_found(m: &str) -> Self {
        Self::new(-32601, format!("method not found: {m}"))
    }
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self::new(-32602, msg)
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(-32603, msg)
    }
    /// 一次性配对码无效(不存在/已用/已过期),用于帧级 `pair/redeem`。与「无
    /// token」的说法一致,不泄露码是否存在。
    pub fn invalid_pairing_code() -> Self {
        Self::new(-32001, "invalid or expired pairing code")
    }
    pub fn not_initialized() -> Self {
        Self::new(-32010, "server not initialized")
    }
    pub fn unknown_thread(id: &str) -> Self {
        Self::new(-32011, format!("unknown thread: {id}"))
    }
    pub fn turn_in_progress(id: &str) -> Self {
        Self::new(-32012, format!("turn already in progress: {id}"))
    }
    /// `turn/interject` arrived when no turn was active. Distinct from
    /// `-32012`: that one means "a turn is busy", this one means "there is
    /// nothing to fold into".
    pub fn not_running() -> Self {
        Self::new(-32013, "no turn is running".to_string())
    }
    /// The client's [`Scope`] is below what the method requires.
    ///
    /// Distinct from `method_not_found`: the method exists and the caller is
    /// authenticated, it is simply not allowed to run *this* method on *its*
    /// connection. The required scope is in the message so a UI can explain it.
    pub fn insufficient_scope(required: Scope) -> Self {
        Self::new(-32014, format!("insufficient scope: requires {required:?}"))
    }
    /// A board query that reached no answer.
    ///
    /// The numeric code is only a coarse fallback for clients that read nothing
    /// else; `data.code` carries the stable vocabulary
    /// (`board_not_created` / `daemon_unavailable` / `plugin_unavailable` /
    /// `plugin_not_installed` / `plugin_rejected`) the UI branches on, so a new
    /// reason can be added without renumbering.
    pub fn board_query(code: &'static str, message: impl Into<String>) -> Self {
        let numeric = match code {
            "board_not_created" => -32020,
            "daemon_unavailable" => -32021,
            "plugin_unavailable" => -32022,
            "plugin_not_installed" => -32023,
            "plugin_rejected" => -32024,
            _ => -32603,
        };
        Self {
            code: numeric,
            message: message.into(),
            data: Some(serde_json::json!({ "code": code })),
        }
    }

    /// A `model/*` write named a model the catalog does not contain.
    ///
    /// Same shape as [`RpcError::board_query`]: the numeric code is a coarse
    /// fallback, `data.code = "model_not_found"` is the stable vocabulary the
    /// UI branches on (`desktop/src/lib/models.ts`).
    pub fn model_not_found(name: &str) -> Self {
        Self {
            code: -32025,
            message: format!("model not found: {name}"),
            data: Some(serde_json::json!({ "code": "model_not_found" })),
        }
    }

    /// A `model/upsert` payload failed validation (empty name, unknown provider,
    /// empty url/model). Numeric code stays in the `invalid_params` family;
    /// `data.code` gives the caller a stable reason.
    pub fn invalid_model(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: Some(serde_json::json!({ "code": "invalid_model" })),
        }
    }

    /// One `turn/start` image input could not be turned into a content block
    /// (an explicit `{type:"image", path}` that is missing, undecodable,
    /// oversized or not a regular file).
    ///
    /// Numeric code stays in the `invalid_params` family; `data.code` gives the
    /// client a stable discriminator so it can say *which* image was refused
    /// instead of substring-matching the human message. A stalled image upload
    /// (`{type:"uploaded_image"}` whose `uploadId` is unknown/expired) is **not**
    /// reported here — the server drops it and runs the turn (see
    /// `prepare_turn_core`), because that failure is retry-proof on the client.
    pub fn invalid_image(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: Some(serde_json::json!({ "code": "invalid_image" })),
        }
    }
}

/// 服务端 → 客户端通知(无 id)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum Notification {
    #[serde(rename = "thread/started")]
    ThreadStarted {
        thread_id: String,
        cwd: String,
        model: String,
        /// 该会话选中的清单条目显示名(`None`＝跟随全局默认)。`model` 是解析后的
        /// 生效串,`model_ref` 是用户在清单里选中的那一条,客户端据此回显。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_ref: Option<String>,
    },
    #[serde(rename = "turn/started")]
    TurnStarted { thread_id: String, turn_id: String },
    #[serde(rename = "thread/status/updated")]
    ThreadStatusUpdated {
        thread_id: String,
        status: ThreadStatus,
    },
    /// 该会话的生效模型已切换:driver 重建成功后发出。
    ///
    /// `model` 是**解析后的生效串**(清单条目的 `model` 字段),而非用户选中的
    /// 显示名(`model_ref`)——客户端据此显示"当前跑在哪个真实模型上"。
    #[serde(rename = "thread/modelChanged")]
    ModelChanged { thread_id: String, model: String },
    /// 该会话的权限模式（自主权）已切换。
    ///
    /// 与 `thread/modelChanged` 并列：模式与模型都是会话元数据，切换后必须让
    /// **所有**客户端（桌面 + 手机）立即刷新各自的 mode chip，否则一端改了
    /// YOLO、另一端停在旧值。它只带模式、不带正文。
    #[serde(rename = "thread/permissionModeChanged")]
    PermissionModeChanged {
        thread_id: String,
        mode: crate::thread_store::ThreadMode,
    },
    #[serde(rename = "item/started")]
    ItemStarted { thread_id: String, item: Item },
    #[serde(rename = "item/delta")]
    ItemDelta {
        thread_id: String,
        item_id: String,
        delta: String,
    },
    #[serde(rename = "item/completed")]
    ItemCompleted { thread_id: String, item: Item },
    /// 回放期批量下发历史 item：一次一帧，替代逐条 `item/completed`。
    /// 仅在 `thread/resume` 的历史回放期使用；实时流不带此方法。
    #[serde(rename = "items/completed")]
    ItemsCompleted { thread_id: String, items: Vec<Item> },
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        status: TurnStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Mid-turn user messages the server could not consume, handed back so the
    /// client can restore them to the input box. Carries the `tag` ids, because
    /// those are what the client minted and therefore knows about.
    #[serde(rename = "turn/interjectionsReturned")]
    InterjectionsReturned {
        thread_id: String,
        turn_id: String,
        items: Vec<String>,
    },
    #[serde(rename = "turn/retry")]
    TurnRetry {
        thread_id: String,
        turn_id: String,
        attempt: u16,
        max: u16,
        /// Why the turn is being retried: `"idle_stall"` or `"request_timeout"`.
        cause: String,
    },
    /// 用量更新通知。
    ///
    /// 携带的是**本轮累积快照**,而非单条原始 provider 事件:Anthropic 把一次
    /// 调用的用量拆成 `message_start`(input/cache)与 `message_delta`(output)
    /// 两个事件,翻译层按字段合并后发出,故每条通知都是迄今完整的本轮用量。
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage {
        thread_id: String,
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
    },
    /// 一条子 agent 轨迹行,推给订阅了该 thread 那个任务的客户端。
    #[serde(rename = "agent/trace/event", rename_all = "camelCase")]
    AgentTraceEvent {
        thread_id: String,
        task_id: String,
        row: AgentTraceRow,
    },
    /// 该 thread 的子 agent 列表发生了变化,整表推给客户端就地替换。
    ///
    /// 推整表而非增量:列表是模型可能在任何时刻改变的小集合(每次 spawn 一个
    /// 新行),整表让客户端无需对账顺序,也让"错过一帧"不会留下永久残影。
    #[serde(rename = "agent/children/updated", rename_all = "camelCase")]
    AgentChildrenUpdated {
        thread_id: String,
        children: Vec<AgentChild>,
    },
    /// 该 thread 的某个受管进程状态发生了变化。
    ///
    /// 只由 `Started / Ready / Exited / Killed` 触发，**不含 `Output`**：stdout
    /// 是高频流，只在用户打开详情时经 `process/read` 增量拉取，不灌进事件通道。
    /// 客户端收到本通知即重拉 `process/list` — 它是**提示刷新**，不是权威数据。
    #[serde(rename = "process/updated")]
    ProcessUpdated {
        thread_id: String,
        process_id: String,
        /// `ProcessStatus` 的 serde 标签：starting / running / ready / exited /
        /// killed / failed_to_start。
        state: String,
    },
    /// 某次审批已被**任一**客户端处理;其余客户端据此关闭弹窗。
    ///
    /// 广播而非定向,因为发起审批的 turn 属于共享的 app-server,而等待的
    /// 设备可能不止一台:谁先答谁生效,其余设备需要知道「这个弹窗已经没用了」。
    #[serde(rename = "item/toolCall/approvalResolved")]
    ToolCallApprovalResolved {
        perm_id: String,
        by: String,
        decision: String,
    },
    /// 桌面端 UI 偏好（当前只有主题）发生变化。
    ///
    /// 只由 `ThemeHandle` 的广播触发；客户端收到即把 `data-theme` 换成新值。
    #[serde(rename = "ui/settings/updated")]
    UiSettingsUpdated { theme: String },
    /// 模型（`show_git_diff` 工具）请求桌面端聚焦 Git Diff 视图。
    #[serde(rename = "ui/gitDiff/focus", rename_all = "camelCase")]
    UiGitDiffFocus {
        thread_id: Option<String>,
        base: Option<String>,
        note: Option<String>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

impl Notification {
    /// 该通知所属的 thread；`None`＝全局帧（主题/错误/审批已处理），恒放行。
    pub(crate) fn thread_key(&self) -> Option<&str> {
        match self {
            Notification::ThreadStarted { thread_id, .. }
            | Notification::TurnStarted { thread_id, .. }
            | Notification::ThreadStatusUpdated { thread_id, .. }
            | Notification::ModelChanged { thread_id, .. }
            | Notification::PermissionModeChanged { thread_id, .. }
            | Notification::ItemStarted { thread_id, .. }
            | Notification::ItemDelta { thread_id, .. }
            | Notification::ItemCompleted { thread_id, .. }
            | Notification::ItemsCompleted { thread_id, .. }
            | Notification::TurnCompleted { thread_id, .. }
            | Notification::InterjectionsReturned { thread_id, .. }
            | Notification::TurnRetry { thread_id, .. }
            | Notification::TokenUsage { thread_id, .. }
            | Notification::AgentTraceEvent { thread_id, .. }
            | Notification::AgentChildrenUpdated { thread_id, .. }
            | Notification::ProcessUpdated { thread_id, .. } => Some(thread_id),
            Notification::UiGitDiffFocus { thread_id, .. } => thread_id.as_deref(),
            Notification::ToolCallApprovalResolved { .. }
            | Notification::UiSettingsUpdated { .. }
            | Notification::Error { .. } => None,
        }
    }
}

/// 该通知的投递层级（S2）：决定 `write_notification` 用哪个广播键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// 全局帧：主题/错误/审批已处理。恒推。
    Global,
    /// 列表层：会话存在与状态，外加**回合结束**。**恒推**（列表要实时，与订阅无关）。
    ///
    /// `TurnCompleted` 刻意留在列表层：它是未读蓝点**唯一**的驱动（前端
    /// `ThreadView.unread` 只在 `turn/completed` 且非当前会话时置位），而落库的
    /// `ThreadSummary` 并不带未读位，`thread/listAll` 无从补齐。远程客户端只订阅
    /// 寥寥几条暖会话，若把它当内容层过滤，窗口外跑完的会话就永远不亮蓝点——用户
    /// 以为「还没跑完」。它只带状态/错误、不带正文，故恒推不违背「内容按需订阅」。
    List,
    /// 内容层：会话正文流。按 `thread_key()` 过滤。
    Content,
}

impl Notification {
    /// 该通知的投递层级。见 [`Delivery`]。
    pub(crate) fn delivery(&self) -> Delivery {
        match self {
            Notification::ThreadStarted { .. }
            | Notification::ThreadStatusUpdated { .. }
            | Notification::ModelChanged { .. }
            | Notification::PermissionModeChanged { .. }
            | Notification::TurnCompleted { .. } => Delivery::List,
            Notification::UiSettingsUpdated { .. }
            | Notification::Error { .. }
            | Notification::ToolCallApprovalResolved { .. } => Delivery::Global,
            _ => Delivery::Content,
        }
    }
}

/// 一条子 agent 的列表项。字段沿用 JSON-RPC 的 camelCase。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentChild {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_step: Option<String>,
    /// The task's parent, absent for a top-level child. Lets a client walk the
    /// tree it was handed without a second request, so the detail's drill-down
    /// lists exactly the task's own children.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<String>,
}

/// 一条轨迹行,原样透传 daemon 的 `IpcTraceRow`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceRow {
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload_json: String,
}

/// 服务端 → 客户端通知的 JSON-RPC 2.0 信封。
///
/// `Notification` 自身只带 `method` / `params`,缺 `jsonrpc` 字段;发到线上
/// 前需经本信封补齐 `"jsonrpc":"2.0"`。
#[derive(Debug, Clone, Serialize)]
pub struct NotificationEnvelope<'a> {
    pub jsonrpc: &'a str,
    #[serde(flatten)]
    pub notification: &'a Notification,
}

impl<'a> NotificationEnvelope<'a> {
    pub fn new(notification: &'a Notification) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION,
            notification,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// thread 级实时状态。`failed` 刻意缺席：失败是事件不是状态，失败后 thread
/// 立刻回 `Idle`；"失败未读"由前端从 `turn/completed.params.status` 自行表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Running,
    AwaitingApproval,
}

/// 一条消息附带的文档（只带元数据与 in-root 路径，**不带内容**）。
///
/// `path` 相对工作区根，形如 `.yi-agent/attachments/<thread_id>/<hash>-<name>`；
/// 服务端在起 turn 前把用户选中的文件复制到该位置，工具据此读取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    pub size: u64,
}

/// 对工作区内一张图片的引用——**只带路径与元数据，绝不含 base64 字节**。
///
/// `Item` 会随每条通知下发,而传输帧上限为 1 MiB;内联图片字节日志会把帧撑爆。
/// 服务端在起 turn 前把用户选中的图片复制到 `.yi-agent/attachments/<thread_id>/
/// <hash>-<name>`,客户端据此自行读取并渲染。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: String,
    pub media_type: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    UserMessage {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    AgentMessage {
        id: String,
        text: String,
    },
    ToolCall {
        id: String,
        call_id: String,
        name: String,
        input: Value,
        status: ToolStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
    /// A user message that arrived mid-turn, rendered as its own bubble so it is
    /// not confused with the message that opened the turn.
    UserInterjection {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ImageRef>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_parses_numeric_id() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(req.method, "initialize");
        assert!(matches!(req.id, RequestId::Num(1)));
    }

    /// `plugin_rejected` is a new reason alongside `plugin_not_installed`; it
    /// gets its own numeric fallback while every existing mapping stays put.
    #[test]
    fn board_query_numbers_each_reason_without_renumbering() {
        assert_eq!(RpcError::board_query("plugin_rejected", "no").code, -32024);
        assert_eq!(
            RpcError::board_query("plugin_not_installed", "no").code,
            -32023
        );
        assert_eq!(
            RpcError::board_query("plugin_unavailable", "no").code,
            -32022
        );
        assert_eq!(
            RpcError::board_query("daemon_unavailable", "no").code,
            -32021
        );
        assert_eq!(
            RpcError::board_query("board_not_created", "no").code,
            -32020
        );
        let unknown = RpcError::board_query("something_else", "no");
        assert_eq!(unknown.code, -32603);
        assert_eq!(unknown.data.unwrap()["code"], "something_else");
    }

    /// `model_not_found` gets its own numeric code without renumbering the
    /// board/plugin vocabulary; the UI only reads `data.code`.
    #[test]
    fn model_not_found_has_a_stable_numeric_code_and_reason() {
        let error = RpcError::model_not_found("A");
        assert_eq!(error.code, -32025);
        assert_eq!(error.data.unwrap()["code"], "model_not_found");
        // The reason string is what a caller prints, but it never leaks a key.
        assert!(error.message.contains("A"));

        let invalid = RpcError::invalid_model("provider must be anthropic or openai");
        assert_eq!(invalid.code, -32602);
        assert_eq!(invalid.data.unwrap()["code"], "invalid_model");
    }

    #[test]
    fn request_envelope_parses_string_id() {
        let raw = r#"{"jsonrpc":"2.0","id":"perm-3","method":"x","params":{}}"#;
        let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert!(matches!(req.id, RequestId::Str(ref s) if s == "perm-3"));
    }

    #[test]
    fn client_response_parses_reverse_request_reply() {
        let raw = r#"{"jsonrpc":"2.0","id":"perm-3","result":{"decision":"allow_once"}}"#;
        let resp: ClientResponse = serde_json::from_str(raw).unwrap();
        assert!(matches!(resp.id, RequestId::Str(ref s) if s == "perm-3"));
        assert_eq!(resp.result.as_ref().unwrap()["decision"], "allow_once");
        assert!(resp.error.is_none());
    }

    #[test]
    fn reverse_request_serializes_with_flat_envelope() {
        let reverse = ReverseRequest {
            jsonrpc: JSONRPC_VERSION,
            id: "perm-1".into(),
            method: "item/toolCall/requestApproval",
            params: serde_json::json!({"tool_name": "bash"}),
        };
        let v: Value = serde_json::to_value(&reverse).unwrap();
        assert_eq!(v["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(v["id"], "perm-1");
        assert_eq!(v["method"], "item/toolCall/requestApproval");
        assert_eq!(v["params"]["tool_name"], "bash");
        assert!(
            v.get("reverse_request").is_none(),
            "reverse request must not nest: {v}"
        );
    }

    #[test]
    fn request_envelope_params_defaults_to_null() {
        let raw = r#"{"id":7,"method":"ping"}"#;
        let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert!(req.params.is_null());
        assert!(req.jsonrpc.is_none());
    }

    #[test]
    fn notification_serializes_with_jsonrpc_and_method_tag() {
        let n = Notification::TurnStarted {
            thread_id: "t1".into(),
            turn_id: "u1".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(v["method"], "turn/started");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert!(
            v.get("notification").is_none(),
            "flattened envelope must not nest under `notification`"
        );
    }

    #[test]
    fn item_serializes_with_camel_case_type_tag() {
        let item = Item::UserMessage {
            id: "i1".into(),
            text: "hi".into(),
            attachments: Vec::new(),
            images: Vec::new(),
        };
        let v: Value = serde_json::to_value(&item).unwrap();
        assert_eq!(v["type"], "userMessage");
        assert_eq!(v["text"], "hi");
    }

    #[test]
    fn item_tool_call_omits_none_result() {
        let item = Item::ToolCall {
            id: "i1".into(),
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"cmd":"ls"}),
            status: ToolStatus::Running,
            result: None,
            images: Vec::new(),
        };
        let v: Value = serde_json::to_value(&item).unwrap();
        assert_eq!(v["type"], "toolCall");
        assert_eq!(v["status"], "running");
        assert!(v.get("result").is_none(), "None result must be omitted");
    }

    #[test]
    fn turn_completed_omits_none_error() {
        let n = Notification::TurnCompleted {
            thread_id: "t1".into(),
            turn_id: "u1".into(),
            status: TurnStatus::Completed,
            error: None,
        };
        let v: Value = serde_json::to_value(&n).unwrap();
        assert_eq!(v["params"]["status"], "completed");
        assert!(v["params"].get("error").is_none());
    }

    #[test]
    fn token_usage_notification_includes_cache_fields() {
        let n = Notification::TokenUsage {
            thread_id: "t1".into(),
            model: "m".into(),
            input_tokens: 10,
            output_tokens: 3,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tokenUsage/updated");
        assert_eq!(v["params"]["input_tokens"], 10);
        assert_eq!(v["params"]["output_tokens"], 3);
        assert_eq!(v["params"]["cache_creation_input_tokens"], 100);
        assert_eq!(v["params"]["cache_read_input_tokens"], 200);
    }

    #[test]
    fn model_changed_notification_carries_the_method_and_model() {
        let n = Notification::ModelChanged {
            thread_id: "t1".into(),
            model: "model-b".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/modelChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["model"], "model-b");
    }

    /// mode 变更通知的 wire 形状:`thread/permissionModeChanged`,参数 snake_case。
    #[test]
    fn permission_mode_changed_notification_carries_method_and_mode() {
        let n = Notification::PermissionModeChanged {
            thread_id: "t1".into(),
            mode: crate::thread_store::ThreadMode::Yolo,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/permissionModeChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["mode"], "yolo");
        // 投递层级:与 `modelChanged` 同为列表层。远程客户端只订阅少量暖会话,
        // 若按内容层过滤,窗口外会话的模式变更永远收不到——「概率性不同步」的成因。
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    #[test]
    fn thread_status_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(ThreadStatus::Idle).unwrap(),
            serde_json::json!("idle")
        );
        assert_eq!(
            serde_json::to_value(ThreadStatus::Running).unwrap(),
            serde_json::json!("running")
        );
        assert_eq!(
            serde_json::to_value(ThreadStatus::AwaitingApproval).unwrap(),
            serde_json::json!("awaiting_approval")
        );
    }

    #[test]
    fn thread_status_notification_serializes_with_method_tag() {
        let n = Notification::ThreadStatusUpdated {
            thread_id: "t1".into(),
            status: ThreadStatus::AwaitingApproval,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/status/updated");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["status"], "awaiting_approval");
    }

    #[test]
    fn process_updated_notification_serializes_with_snake_case_params() {
        let n = Notification::ProcessUpdated {
            thread_id: "t1".into(),
            process_id: "proc_1".into(),
            state: "running".into(),
        };
        let v = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "process/updated");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["process_id"], "proc_1");
        assert_eq!(v["params"]["state"], "running");
    }

    #[test]
    fn ui_settings_notification_uses_its_wire_shape() {
        let n = Notification::UiSettingsUpdated {
            theme: "light".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "ui/settings/updated");
        assert_eq!(v["params"]["theme"], "light");
    }

    #[test]
    fn user_message_attachments_round_trip() {
        let item = Item::UserMessage {
            id: "user-1".into(),
            text: "总结这份文件".into(),
            attachments: vec![Attachment {
                name: "报告.pdf".into(),
                path: ".yi-agent/attachments/t1/a1b2c3d4-报告.pdf".into(),
                mime: Some("application/pdf".into()),
                size: 1234,
            }],
            images: Vec::new(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["attachments"][0]["name"], "报告.pdf");
        assert_eq!(json["attachments"][0]["size"], 1234);
        assert_eq!(serde_json::from_value::<Item>(json).unwrap(), item);
    }

    #[test]
    fn empty_attachments_are_omitted_on_the_wire() {
        // 旧前端/旧服务端兼容：没有附件时不得多出一个 null 字段。
        let item = Item::UserMessage {
            id: "u".into(),
            text: "hi".into(),
            attachments: Vec::new(),
            images: Vec::new(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(json.get("attachments").is_none(), "got: {json}");
        // 旧线协议（缺字段）必须仍能反序列化。
        let legacy: Item =
            serde_json::from_str(r#"{"type":"userMessage","id":"u","text":"hi"}"#).unwrap();
        assert_eq!(
            legacy, item,
            "legacy wire data without `attachments` must deserialize to an empty vec"
        );
    }

    #[test]
    fn an_item_carries_image_refs_but_never_base64() {
        let item = Item::UserMessage {
            id: "user-turn-1".into(),
            text: "看图".into(),
            attachments: Vec::new(),
            images: vec![ImageRef {
                path: ".yi-agent/attachments/t1/a1b2-x.png".into(),
                media_type: "image/png".into(),
                size: 1234,
                detail: Some("high".into()),
            }],
        };
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(
            v["images"][0]["path"],
            ".yi-agent/attachments/t1/a1b2-x.png"
        );
        assert_eq!(v["images"][0]["media_type"], "image/png");
        let raw = v.to_string();
        assert!(
            !raw.contains("base64"),
            "an item must never inline image bytes: {raw}"
        );
    }

    #[test]
    fn an_item_without_images_omits_the_field() {
        let item = Item::UserMessage {
            id: "user-turn-1".into(),
            text: "hi".into(),
            attachments: Vec::new(),
            images: Vec::new(),
        };
        assert!(serde_json::to_value(&item).unwrap().get("images").is_none());
    }

    #[test]
    fn items_completed_serializes_with_method_and_params() {
        let n = Notification::ItemsCompleted {
            thread_id: "t1".to_string(),
            items: vec![Item::UserMessage {
                id: "user-1".to_string(),
                text: "hi".to_string(),
                attachments: Vec::new(),
                images: Vec::new(),
            }],
        };
        let v = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], serde_json::json!("items/completed"));
        assert_eq!(v["params"]["thread_id"], serde_json::json!("t1"));
        assert_eq!(
            v["params"]["items"][0]["type"],
            serde_json::json!("userMessage")
        );
        assert_eq!(n.thread_key(), Some("t1"));
    }
}
