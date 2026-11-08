//! JSON-RPC 2.0 信封与 yi-agent 协议类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use ts_rs::TS;

pub const PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC_VERSION: &str = "2.0";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 已配对遥控设备被授予的权限级。
///
/// `Ord` 从弱到强：`Observe < Control < Admin`。新配对设备默认 `Control`
/// （决策 A）。Task 3 会围绕它扩展配对/设备 RPC。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Observe,
    Control,
    Admin,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(untagged)]
pub enum RequestId {
    // 线上是 JSON 数字,与 `Str` 并列构成 `number | string`。
    #[ts(type = "number")]
    Num(i64),
    Str(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct RequestEnvelope {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    pub method: String,
    #[serde(default)]
    #[ts(type = "unknown")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct ResponseEnvelope {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    /// 成功响应时省略;与 `error` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "unknown")]
    pub result: Option<Value>,
    /// 错误响应时省略;与 `result` 互斥。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// 客户端 → 服务端、针对反向请求的响应(id 与请求匹配,无 `method`)。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct ClientResponse {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    pub id: RequestId,
    #[serde(default)]
    #[ts(type = "unknown")]
    pub result: Option<Value>,
    #[serde(default)]
    #[ts(type = "unknown")]
    pub error: Option<Value>,
}

/// 服务端 → 客户端的反向请求(带 `id`,需要客户端回响应)。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct ReverseRequest<'a> {
    pub jsonrpc: &'a str,
    pub id: String,
    pub method: &'a str,
    #[ts(type = "unknown")]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct RpcError {
    // JSON-RPC 错误码,线上是 JSON 数字。
    #[ts(type = "number")]
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    #[ts(type = "unknown")]
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

    /// 命中保留命名空间（`_shell/*`）的方法。
    ///
    /// 单独一个构造函数而不是复用 `method_not_found`：这条拒绝的语义是"你不
    /// 该碰这个前缀"，与"我不知道这个方法"不同。混在一起会让排障时看不出
    /// 是一次权限拒绝还是一次拼写错误。
    pub fn reserved_method() -> Self {
        Self::new(-32601, "reserved native management method")
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
    /// A plugin-channel query that reached no answer.
    ///
    /// The numeric code is only a coarse fallback for clients that read nothing
    /// else; `data.code` carries the stable vocabulary
    /// (`daemon_unavailable` / `plugin_unavailable` /
    /// `plugin_not_installed` / `plugin_rejected` / `plugin_disabled`) the UI
    /// branches on, so a new
    /// reason can be added without renumbering.
    ///
    /// **Generic on purpose.** The family used to be named after boards
    /// (`board_query`) and carried a `board_not_created` arm, from when the host
    /// gated the channel on "does this project have a board". The host no longer
    /// knows what a board is: whether an answer exists is the daemon's business,
    /// so "there is nothing here" is not an error the host can produce.
    pub fn plugin_query_error(code: &'static str, message: impl Into<String>) -> Self {
        let numeric = match code {
            "daemon_unavailable" => -32021,
            "plugin_unavailable" => -32022,
            "plugin_not_installed" => -32023,
            "plugin_rejected" => -32024,
            // 功能开关关着。与 `plugin_unavailable` 分开：那个让人去查安装，
            // 这个让人去设置页打开。
            "plugin_disabled" => -32025,
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
    /// Same shape as [`RpcError::plugin_query_error`]: the numeric code is a coarse
    /// fallback, `data.code = "model_not_found"` is the stable vocabulary the
    /// UI branches on (`desktop/src/lib/models.ts`).
    pub fn model_not_found(name: &str) -> Self {
        Self {
            code: -32025,
            message: format!("model not found: {name}"),
            data: Some(serde_json::json!({ "code": "model_not_found" })),
        }
    }

    /// 客户端的上下文锚点已失效：该会话在它不知情时被 `/clear`，它手上持有的是
    /// 服务端**不存在**的内容，其可见上下文与服务端实际上下文结构性不一致。
    ///
    /// 与 `plugin_query_error` / `model_not_found` 同族：数值码只是粗粒度兜底，
    /// `data.code` 才是 UI 分支的稳定词汇。注意「锚点落后但仍在」**不是**本错误
    /// ——落后是两端并存的正常形状（spec §7.2）。
    pub fn stale_context(thread_id: &str) -> Self {
        Self {
            code: -32026,
            message: format!("context anchor is stale for thread: {thread_id}"),
            data: Some(serde_json::json!({ "code": "stale_context" })),
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

    /// 保存模型前的一次连接测试失败。
    ///
    /// 与 `invalid_model` 共用 `-32602` 族(都属于"调用方给的配置不能用"),
    /// 但 `data.code` 是 UI 分支用的稳定词汇:它要把"连通性失败"和"字段非法"
    /// 说成两件事,否则用户会被引导去改字段而不是查网络/密钥。
    pub fn model_test_failed(reason: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: format!("connection test failed: {}", reason.into()),
            data: Some(serde_json::json!({ "code": "model_test_failed" })),
        }
    }

    /// 整台机器还没有可用模型(清单无默认条目,也没有显式注入)。
    ///
    /// 数值码自成一族:这不是参数错误,UI 应把用户导向设置/引导,而不是让他
    /// 去改当前这次调用的参数。
    pub fn model_not_configured(reasons: &[String]) -> Self {
        Self {
            code: -32027,
            message: "no model is configured; finish model setup first".to_string(),
            data: Some(serde_json::json!({
                "code": "model_not_configured",
                "reasons": reasons,
            })),
        }
    }

    /// A side-conversation failure, carrying a stable `data.reason`.
    ///
    /// Same family shape as [`RpcError::plugin_query_error`]: the numeric code is a
    /// coarse fallback, `data.reason` is the vocabulary the UI branches on. Codes
    /// live in `-3203x`/`-3204x`, which the table above does not use; the test
    /// `side_error_codes_are_unique_and_stable` pins that claim.
    ///
    /// Four reasons deliberately reuse an existing code rather than inventing a new
    /// one, because they are the same failure a caller already knows:
    /// `invalid_params` (-32602), `turn_in_progress` (-32012),
    /// `insufficient_scope` (-32014) and `unknown_thread` (-32011).
    pub fn side(error: &yi_agent_runtime::side_conversation::SideError) -> Self {
        use yi_agent_runtime::side_conversation::SideErrorReason as R;
        let code = match error.reason {
            R::SideNotFound => -32031,
            R::ParentNotStarted => -32032,
            R::SideLimit => -32033,
            R::SnapshotTooLarge => -32034,
            R::SourceChanged => -32035,
            R::SideClosing => -32036,
            R::CloseConflict => -32037,
            R::SnapshotExpired => -32038,
            R::SideCreating => -32039,
            R::RequestLimit => -32040,
            R::ResourceLimit => -32041,
            R::Cancelled => -32042,
            R::TurnFailed => -32043,
            R::InvalidParams => -32602,
            R::TurnInProgress => -32012,
            R::InsufficientScope => -32014,
            R::UnknownThread => -32011,
        };
        Self {
            code,
            message: error.message.clone(),
            data: Some(serde_json::json!({ "reason": error.reason_str() })),
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

/// Why a turn could not delegate, in the shape the client can act on.
///
/// A stable `code` plus a human `reason`: the client branches on the code (retry
/// next turn vs. this conversation is closed) and shows the reason, so no client
/// has to parse a sentence.
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct DelegationFailurePayload {
    pub code: String,
    pub reason: String,
}

/// 服务端 → 客户端通知(无 id)。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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
    /// Whether this turn may delegate, announced once the daemon confirmed it.
    ///
    /// Carries **no capability**: it is a status fact, not a credential, and the
    /// client only needs to know whether `spawn_agent` will work this turn. `ready`
    /// is `true` only after `begin` was confirmed, so a client never shows
    /// "delegation ready" for a turn the daemon never opened.
    #[serde(rename = "thread/delegationStatus/updated")]
    DelegationStatusUpdated {
        thread_id: String,
        turn_id: String,
        ready: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failure: Option<DelegationFailurePayload>,
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
        // `ThreadMode` 在 `thread_store.rs`,不在本模块的导出清单里;这里只标注它
        // 的线上形状(`#[serde(rename_all = "lowercase")]` 的外部标签)。
        #[ts(type = "\"normal\" | \"yolo\"")]
        mode: crate::thread_store::ThreadMode,
    },
    /// 某会话的**共享已读水位**已推进（任一端读过它）。
    ///
    /// 这是「手机点开、桌面蓝点同时消失」的唯一通道：未读由
    /// `last_completed_at > last_read_at` 派生，一端标记已读后必须让其余端
    /// 立刻合入新水位。与 `thread/modelChanged` / `thread/permissionModeChanged`
    /// 同族——都是会话元数据，只带时间戳、不带正文。
    ///
    /// 两个值一起发：客户端只需按 `max` 合入，就能在没有额外请求的情况下重算
    /// 「还该不该亮蓝点」，不必再拉一次 `thread/listAll`。
    #[serde(rename = "thread/readStateChanged")]
    ReadStateChanged {
        thread_id: String,
        // 线上是 JSON 数字（epoch 毫秒）。与 `RequestId::Num` / `RpcError::code`
        // 同一口径：`i64` 在本项目一律显式标注为 `number`，否则 ts-rs 会生成
        // `bigint`，而 wire 上来的、以及与 `Date.now()` 比较的都是普通 number。
        #[ts(type = "number")]
        last_read_at: i64,
        #[ts(type = "number")]
        last_completed_at: i64,
    },
    /// 旁支的状态修订已发布(spec §8.2)。
    ///
    /// `seq` 是**这一条 side generation 内**的单调修订号,每个已发布修订恰好
    /// 一帧。客户端据此排序与去重,不拿 token 数或 item 序号代替。
    ///
    /// 订阅过滤键是**父** threadId(负载里另有 sideId):桌面 Feed::All 直接收到,
    /// 手机把当前父 pin 在既有订阅窗口里,不额外占名额。
    #[serde(rename = "side/changed")]
    SideChanged {
        parent_thread_id: String,
        side_id: String,
        #[ts(type = "number")]
        generation: u64,
        #[ts(type = "number")]
        seq: u64,
        summary: SideSummaryWire,
    },
    /// 某个旁支 item 的当前完整版本(幂等 upsert,不是文本增量)。
    ///
    /// 客户端按 `item.id` 整体替换;重复收到同一版本是 no-op,不会追加重复文本。
    /// 丢帧**不能**被当成增量成功——缺号要全量对账。
    #[serde(rename = "side/itemUpdated")]
    SideItemUpdated {
        parent_thread_id: String,
        side_id: String,
        #[ts(type = "number")]
        generation: u64,
        #[ts(type = "number")]
        seq: u64,
        turn_id: String,
        item: Item,
    },
    /// 一轮旁支对话真正结束,带最终状态、错误与累计用量。
    ///
    /// 发布顺序是契约:先 flush 该轮所有 item 更新,再发本帧,最后才允许下一轮
    /// 受理。客户端据此把 `running` 复位并开放下一次发送。
    #[serde(rename = "side/turnCompleted")]
    SideTurnCompleted {
        parent_thread_id: String,
        side_id: String,
        #[ts(type = "number")]
        generation: u64,
        #[ts(type = "number")]
        seq: u64,
        turn_id: String,
        status: SideStatusWire,
        /// 该轮的失败原因(稳定 reason 串),成功时为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        usage: SideUsageWire,
    },
    /// 旁支已关闭,**终态**。
    ///
    /// 收到它就丢弃该 generation 的本地内容与在途分页,即使前序有缺号也不等待
    /// 已经不存在的快照。普通更新不得复活已关闭的 generation。
    #[serde(rename = "side/closed")]
    SideClosed {
        parent_thread_id: String,
        side_id: String,
        #[ts(type = "number")]
        generation: u64,
        #[ts(type = "number")]
        seq: u64,
        /// `CloseReason::as_str()` 的稳定串。
        reason: String,
    },
    /// 该会话的 reasoning effort 覆盖已切换（或清除）。
    ///
    /// 与 `thread/modelChanged` / `thread/permissionModeChanged` 并列：effort 是
    /// 会话元数据，切换后必须让**所有**客户端立即刷新各自的 effort 控件，否则
    /// 一端改了、另一端停在旧值。`effort: None` ＝ 已清除覆盖（回落条目默认）。
    #[serde(rename = "thread/reasoningEffortChanged")]
    ReasoningEffortChanged {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    /// 该会话的跨轮 CoT 保留开关已切换。
    ///
    /// 与 `thread/reasoningEffortChanged` 并列：开关同为会话元数据，切换后必须让
    /// **所有**客户端（桌面 + 手机）立即刷新各自的控件，否则一端改了、另一端停在
    /// 旧值。它只带布尔开关、不带正文。
    #[serde(rename = "thread/preserveThinkingChanged")]
    PreserveThinkingChanged { thread_id: String, preserve: bool },
    /// 某模型条目的 reasoning effort 能力探测完成。
    ///
    /// 与 `thread/*` 那族**不同**：能力是机器级清单属性（`models.json`），不属于
    /// 任何一条会话，故 `thread_key()` 恒为 `None`，所有客户端都要刷新——否则
    /// 一端刚探测完、另一端仍按旧能力平铺档位。
    ///
    /// **直接携带权威结果**：能力就是几个短字符串，客户端就地落笔即可，不必再付
    /// 一次全量 `model/list`。`None` = 条目已删或探测结果作废。
    ///
    /// 线上形状见 `desktop/src/lib/models.ts` 的 `ReasoningCapabilities`：
    ///
    /// ```jsonc
    /// { "chat": { "supported": ["low"], "probed_at": 1, "error": null } }
    /// ```
    ///
    /// 用 `#[ts(type = "unknown")]` 而不是导入 runtime 类型：`ReasoningCapabilities`
    /// 定义在 `yi-agent-runtime`，那里不依赖 ts-rs，协议层也不该为它引入反向依赖。
    #[serde(rename = "model/capabilitiesChanged")]
    ModelCapabilitiesChanged {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(type = "unknown")]
        reasoning_capabilities: Option<yi_agent_runtime::models::ReasoningCapabilities>,
    },
    /// 该会话的标题已变（LLM 首轮生成成功写入）。
    ///
    /// 与 `thread/modelChanged` 同族：标题是会话元数据，变更后必须让**所有**
    /// 客户端（桌面 + 手机）立即刷新侧栏，否则首轮结束生成的标题要等到一次
    /// **无关的**列表刷新才可见（`turn/completed` 早于标题落盘发出，桌面端在
    /// 那一刻重拉只能拿到兜底截断标题）。
    ///
    /// **直接携带权威标题字符串**（而非「该重拉了」的提示）：标题就是一个小
    /// 字符串，客户端就地落笔即可，不必再付一次全量 `thread/listAll`。
    #[serde(rename = "thread/titleChanged")]
    TitleChanged { thread_id: String, title: String },
    /// 该会话的颜色标签已变（`thread/setTag`）。
    ///
    /// 与 `thread/titleChanged` 同族：标签是会话元数据，变更后必须让**所有**客户端
    /// 立即刷新侧栏色点。**直接携带权威值**（`None` = 已清除），客户端就地落笔，
    /// 不必重拉全量 `thread/listAll`。
    #[serde(rename = "thread/tagChanged")]
    TagChanged {
        thread_id: String,
        tag: Option<String>,
    },
    /// 某 workspace 的会话分组已变（`thread/group/*`）。
    ///
    /// 与 `thread/tagChanged` **不同族**：分组属于一个目录而不是某条会话，故
    /// `thread_key()` 为 `None`。同时也不带分组正文——一个目录的组表可能很大，
    /// 客户端收到后重拉 `thread/listAll` 更省事；而 `Delivery::List` 恒推，
    /// 恰好匹配「列表要实时」。
    #[serde(rename = "thread/groupsChanged")]
    GroupsChanged { workspace: String },
    /// 非致命运行期告知（通道回退 / effort 降级），状态栏级一行提示。
    ///
    /// 与 `turn/retry` 同为**内容层**（按 thread 路由）：它描述的是某条会话这一次
    /// 运行走了哪条路，而不是会话元数据，窗口外的会话无需因此刷新控件。
    #[serde(rename = "thread/notice")]
    Notice {
        thread_id: String,
        // `NoticeLevel` 在 `yi-agent-core`,同上:只标注线上形状(外部标签,原样变体名)。
        #[ts(type = "\"Info\" | \"Warn\"")]
        level: yi_agent_core::NoticeLevel,
        text: String,
    },
    /// 归一化的思维链增量（两个端点都映射到它），展示用。
    ///
    /// 与 `item/delta` 同为内容层：它是仅展示的正文流，绝不回灌进后续轮次
    /// （见 core 的 `AgentEvent::ReasoningDelta` 文档）。
    #[serde(rename = "thread/reasoningDelta")]
    ReasoningDelta {
        thread_id: String,
        text: String,
        // `ReasoningKind` 同上:线上是外部标签的变体名。
        #[ts(type = "\"Content\" | \"Summary\"")]
        kind: yi_agent_core::ReasoningKind,
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
        #[ts(optional)]
        error: Option<String>,
        /// 本轮完成时刻（epoch 毫秒），与落盘的 `last_completed_at` **同源**。
        ///
        /// 客户端收到本帧会立刻重拉 `thread/listAll`，而那一次落盘**排在广播
        /// 之后**（广播在 driver 的流消费处，落盘在 `persist_and_finish_turn`），
        /// 于是列表里的 `last_completed_at` 可能仍是旧值。自带权威时间戳让客户端
        /// 按 `max` 合入即可收敛，无需再造一套「哪个来源更权威」的机制。
        ///
        /// 可选：旧服务端不发这个字段，客户端退回自己的墙钟（只会多亮一次蓝点，
        /// 不会漏），故不必为一个「更好一点」的时间戳把线协议变成破坏性变更。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        #[ts(type = "number | null")]
        completed_at: Option<i64>,
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
    /// `input_tokens` 等四个字段是**本轮最后一次 provider 调用**的快照（Anthropic
    /// 把一次调用拆成 `message_start` 的 input/cache 与 `message_delta` 的 output，
    /// 翻译层按字段合并），上下文占用据此计算。
    ///
    /// `total_*` 是该 thread 的**累计**用量（已落盘轮次 + 本轮已结束调用 + 当前调用），
    /// 每收到一次 provider 用量事件都会重算并推送，故界面上的累计数字会随每次调用增长。
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage {
        thread_id: String,
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        #[ts(type = "number")]
        total_input_tokens: u64,
        #[ts(type = "number")]
        total_output_tokens: u64,
        #[ts(type = "number")]
        total_cache_creation_input_tokens: u64,
        #[ts(type = "number")]
        total_cache_read_input_tokens: u64,
        /// 本会话所有 subagent 的累计，**不并入** `total_*`。
        ///
        /// 与主会话是两组独立的数：`/usage` 与展开面板要分开呈现，合并会让
        /// 「主 Agent 自己花了多少」无从得知。恒为具体数字，没有 subagent 时是 0。
        #[ts(type = "number")]
        subagent_input_tokens: u64,
        #[ts(type = "number")]
        subagent_output_tokens: u64,
        #[ts(type = "number")]
        subagent_cache_read_tokens: u64,
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
    /// 运行期设置已提交（本宿主或观察到的其他进程），供客户端失效重读。
    ///
    /// **全局**帧：设置是宿主级，与任何 thread 无关。`changed_keys` 为空时表示
    /// 「状态可能变了但没有逻辑设置改变」——健康状态的翻转正是这样通知的，
    /// 客户端据此重新 read 并读取 `health`，而不是从本帧推断健康值。
    ///
    /// 刻意不带任何设置取值：提示词等字段属于用户内容，通知会进日志与转发通道。
    /// `revision` 只用于同版本去重，客户端**不得**把它的大小当作跨进程提交顺序。
    #[serde(rename = "settings/changed")]
    SettingsChanged {
        revision: String,
        changed_keys: Vec<String>,
    },
    /// 颜色标签的显示名（颜色 id → 名称）已变。
    ///
    /// **全局**帧：标签名是对所有会话生效的宿主级偏好，与任何 thread 上下文无关。
    /// **携带用户覆盖的权威映射**（可能只含被改的键），客户端与自己的默认名表合并
    /// 后即可渲染。
    ///
    /// 为什么独立成一条而不是挤进 `ui/settings/updated`：后者由主题 watcher 从
    /// 主题广播通道泵出（`server.rs` 的 `pump_theme_notifications`），语义是「主题
    /// 变了」；标签名只经 `ui/settings/write` 变更、没有 watcher。塞进去会让该通知
    /// 时而带 `tag_names` 时而不带（watcher 路径没有它），语义含混。
    #[serde(rename = "ui/tagNames/updated")]
    TagNamesUpdated {
        // ts-rs 默认把 `BTreeMap<String, String>` 渲染成 `{ [key in string]?: string }`
        // （值可 `undefined`），比线上更宽：serde 序列化该 map 的值恒为字符串，
        // 线上的形状是 `Record<string, string>`。显式标注复现线上形状（同 §3.2 对
        // `u64`/`Option` 的处理），否则前端按 `Record<string, string>` 消费会报窄化错误。
        #[ts(type = "Record<string, string>")]
        tag_names: std::collections::BTreeMap<String, String>,
    },
    /// 模型（`show_git_diff` 工具）请求桌面端聚焦 Git Diff 视图。
    ///
    /// 不再携带 `base`：基准现在由**帧**承载（`thread/view/read` 读回），
    /// 前端不必自己解析基准。
    #[serde(rename = "ui/gitDiff/focus", rename_all = "camelCase")]
    UiGitDiffFocus {
        thread_id: Option<String>,
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
            | Notification::DelegationStatusUpdated { thread_id, .. }
            | Notification::ModelChanged { thread_id, .. }
            | Notification::PermissionModeChanged { thread_id, .. }
            | Notification::ReadStateChanged { thread_id, .. }
            | Notification::ReasoningEffortChanged { thread_id, .. }
            | Notification::PreserveThinkingChanged { thread_id, .. }
            | Notification::TitleChanged { thread_id, .. }
            | Notification::TagChanged { thread_id, .. }
            | Notification::Notice { thread_id, .. }
            | Notification::ReasoningDelta { thread_id, .. }
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
            Notification::SideChanged {
                parent_thread_id, ..
            }
            | Notification::SideItemUpdated {
                parent_thread_id, ..
            }
            | Notification::SideTurnCompleted {
                parent_thread_id, ..
            }
            | Notification::SideClosed {
                parent_thread_id, ..
            } => Some(parent_thread_id),
            Notification::UiGitDiffFocus { thread_id, .. } => thread_id.as_deref(),
            Notification::ToolCallApprovalResolved { .. }
            | Notification::UiSettingsUpdated { .. }
            | Notification::SettingsChanged { .. }
            | Notification::TagNamesUpdated { .. }
            | Notification::ModelCapabilitiesChanged { .. }
            | Notification::GroupsChanged { .. }
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
            | Notification::ReasoningEffortChanged { .. }
            | Notification::PreserveThinkingChanged { .. }
            | Notification::TitleChanged { .. }
            | Notification::TagChanged { .. }
            | Notification::GroupsChanged { .. }
            | Notification::ReadStateChanged { .. }
            | Notification::TurnCompleted { .. } => Delivery::List,
            Notification::UiSettingsUpdated { .. }
            | Notification::SettingsChanged { .. }
            | Notification::TagNamesUpdated { .. }
            | Notification::ModelCapabilitiesChanged { .. }
            | Notification::Error { .. }
            | Notification::ToolCallApprovalResolved { .. } => Delivery::Global,
            _ => Delivery::Content,
        }
    }
}

/// 一个子任务的累计用量。
///
/// 只带展示要用的三项：input / output / cache read。cache write 不在用户口径内，
/// 故既不记账也不传输。恒为具体数字（不是 Optional）：没有用量的子任务就是三个 0，
/// 客户端用 `> 0` 判断是否值得展示。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct AgentChildUsage {
    #[ts(type = "number")]
    pub input_tokens: u64,
    #[ts(type = "number")]
    pub output_tokens: u64,
    #[ts(type = "number")]
    pub cache_read_tokens: u64,
}

/// 一个会话里所有子任务的用量之和。
///
/// 与 [`AgentChildUsage`] 同形状但用途不同：后者是「一个子任务」的量，本类型是
/// 「整个会话」的合计。不直接复用前者，是因为合计值还要参与 `/clear` 基线相减。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SubagentTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
}

impl SubagentTotals {
    /// 逐项饱和相减，供 `/clear` 基线扣减用。
    ///
    /// 用饱和而非直接减：基线来自同一条数据流，理论上不会超过当前值，但 daemon
    /// 重启 / 任务被清理都可能让它短暂倒挂——此时报 0 好过 panic 或回绕成天文数字。
    pub(crate) fn saturating_sub(self, other: Self) -> Self {
        Self {
            input: self.input.saturating_sub(other.input),
            output: self.output.saturating_sub(other.output),
            cache_read: self.cache_read.saturating_sub(other.cache_read),
        }
    }
}

/// 一条子 agent 的列表项。字段沿用 JSON-RPC 的 camelCase。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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
    /// 该子任务的累计用量。`serde(default)` 让老 daemon 不回该字段时读出全零。
    #[serde(default)]
    pub usage: AgentChildUsage,
}

/// 一条轨迹行,原样透传 daemon 的 `IpcTraceRow`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase")]
pub struct AgentTraceRow {
    // daemon 的持久化行号,线上是 JSON 数字。
    #[ts(type = "number")]
    pub event_id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload_json: String,
}

/// 服务端 → 客户端通知的 JSON-RPC 2.0 信封。
///
/// `Notification` 自身只带 `method` / `params`,缺 `jsonrpc` 字段;发到线上
/// 前需经本信封补齐 `"jsonrpc":"2.0"`。
#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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

/// `side/open` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideOpenParams {
    pub parent_thread_id: String,
}

/// `side/list` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideListParams {
    pub parent_thread_id: String,
}

/// `side/readItems` 的参数。
///
/// `cursor` 必须绑定 `snapshotToken`:没有 token 的 cursor 无法判断它属于哪个
/// 快照,拼页就会拼出两个时刻的内容。校验在 `rpc/side.rs`(参数形状合法不代表
/// 组合合法)。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideReadItemsParams {
    pub side_id: String,
    /// 首读不传;续页时与 `cursor` 一起传。`#[ts(optional)]` 让 TS 侧真的是
    /// 可选字段(而不是「必填但可 null」),否则客户端每次首读都得显式写 null。
    #[serde(default)]
    #[ts(optional)]
    pub snapshot_token: Option<String>,
    #[serde(default)]
    #[ts(optional)]
    pub cursor: Option<String>,
}

/// `side/turnStart` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideTurnStartParams {
    pub side_id: String,
    pub text: String,
    /// 规范 UUID(36 ASCII 字符)。去重表用它做键,故大小写与连字符都必须规范。
    pub client_request_id: String,
}

/// `side/turnInterrupt` 的参数。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideTurnInterruptParams {
    pub side_id: String,
    pub turn_id: String,
}

/// `side/close` 的参数。
///
/// `expected_active_turn_id` 是**必填、可为 null** 的确认字段:Idle 时传 null,
/// Running 时传客户端确认过的 turnId。服务端据此做条件校验,状态已变则回
/// `close_conflict`,由客户端重新读取再确认——不能自动 force 重试。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SideCloseParams {
    pub side_id: String,
    #[serde(deserialize_with = "required_nullable")]
    pub expected_active_turn_id: Option<String>,
}

/// `side/turnStart` 的响应。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideTurnStartResult {
    pub turn_id: String,
}

/// `side/list` 的响应。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideListResult {
    pub sides: Vec<SideSummaryWire>,
}

/// 旁支生命周期的线上镜像。
///
/// runtime 的 `SideStatus` 刻意不依赖 ts-rs(桌面绑定生成器属于宿主层)。这里
/// 镜像一份,并由 [`SideStatusWire::from_runtime`] 的**穷尽 match** 保证同步:
/// runtime 新增一个变体,那个 match 就不再编译,不会被静默落下。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "snake_case")]
pub enum SideStatusWire {
    Creating,
    Idle,
    Running,
    Failed,
    Closing,
    Closed,
}

impl SideStatusWire {
    /// 见类型文档:这个 match 是镜像与 runtime 同步的**唯一**保证。
    pub fn from_runtime(status: yi_agent_runtime::side_conversation::SideStatus) -> Self {
        use yi_agent_runtime::side_conversation::SideStatus as S;
        match status {
            S::Creating => Self::Creating,
            S::Idle => Self::Idle,
            S::Running => Self::Running,
            S::Failed => Self::Failed,
            S::Closing => Self::Closing,
            S::Closed => Self::Closed,
        }
    }
}

/// `SideSummary` 的线上镜像(spec §8.1)。
///
/// 与 [`SideStatusWire`] 同一理由:summary 是 registry 的内部结构,不该为了导出
/// 绑定而把 ts-rs 拖进 registry。字段逐项对应,`#[ts(type = "number")]` 让
/// `u64`/`i64` 在 TS 里是 `number` 而不是 `bigint`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideSummaryWire {
    pub side_id: String,
    pub parent_thread_id: String,
    pub status: SideStatusWire,
    #[ts(type = "number")]
    pub snapshot_revision: u64,
    #[ts(type = "number")]
    pub last_seq: u64,
    #[serde(default)]
    pub active_turn_id: Option<String>,
    /// Epoch 毫秒;`Creating`/`Running` 时为 `None`。
    #[serde(default)]
    #[ts(type = "number | null")]
    pub expires_at: Option<i64>,
    #[ts(type = "number")]
    pub generation: u64,
    pub stopping: bool,
}

impl SideSummaryWire {
    pub fn from_registry(summary: &crate::side_conversation::SideSummary) -> Self {
        Self {
            side_id: summary.side_id.clone(),
            parent_thread_id: summary.parent_thread_id.clone(),
            status: SideStatusWire::from_runtime(summary.status),
            snapshot_revision: summary.snapshot_revision,
            last_seq: summary.last_seq,
            active_turn_id: summary.active_turn_id.clone(),
            expires_at: summary.expires_at,
            generation: summary.generation,
            stopping: summary.stopping,
        }
    }
}

/// 旁支累计用量(线上形状)。
///
/// 逐项抄 runtime 的 `SideTokenTotals`,不借 `TokenUsage`:后者是**单次回复**的
/// `u32` 计数,拿它装旁支终身的累计量既会丢精度、也会把「这一轮」和「这一生」
/// 混成一个数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideTokenTotalsWire {
    #[ts(type = "number")]
    pub input: u64,
    #[ts(type = "number")]
    pub output: u64,
    #[ts(type = "number")]
    pub cache_creation: u64,
    #[ts(type = "number")]
    pub cache_read: u64,
    #[ts(type = "number")]
    pub reasoning: u64,
}

/// 旁支用量:数字 + 「这些数字可不可信」。
///
/// `complete == false` 表示至少有一轮没有拿到供应商的最终用量(被取消或被中止)。
/// 把缺数报成 0 是撒谎,所以标志位跟数字一起走。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideUsageWire {
    pub total: SideTokenTotalsWire,
    pub complete: bool,
}

/// 旁支创建时固定的生效配置(脱敏)。
///
/// 只含能给人看、且**创建那一刻**就定下来的值:没有密钥、没有 API headers、
/// 没有完整 system prompt。客户端据此画 `/config`,而不是去读父会话的
/// `config/read`——父的配置此后可能已经变了,旁支不会跟着变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideEffectiveConfigWire {
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub preserve_thinking: bool,
    /// 旁支恒为只读装配;这个字段随配置走是为了让前端能直说,而不是靠约定。
    pub read_only: bool,
    pub tool_names: Vec<String>,
}

/// `side/readItems` 的响应(一致分页快照的一页,spec §8.2)。
///
/// 每一页都携同一份不可变 metadata:后续页重复它不改变快照,却让「客户端只拿到
/// 第二页」这种情形仍然有状态可画。空 `items` 的单页同样带 metadata。
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct SideReadItemsResult {
    pub items: Vec<Item>,
    #[ts(type = "number")]
    pub snapshot_seq: u64,
    pub status: SideStatusWire,
    pub usage: SideUsageWire,
    #[serde(default)]
    pub active_turn_id: Option<String>,
    /// `requestId -> turnId`。客户端重连后据此确认「那条消息到底受理了没有」,
    /// 不重发、也不新生成 id。
    pub accepted_requests: std::collections::BTreeMap<String, String>,
    pub effective_config: SideEffectiveConfigWire,
    pub snapshot_token: String,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

// ---------------------------------------------------------------------------
// 旁支会话的线上类型(spec §8.1 / §8.2)。
//
// 命名约定与全仓一致:**请求**参数 camelCase(`#[serde(rename_all = "camelCase")]`
// 且 `deny_unknown_fields`),**响应**字段 snake_case(serde 默认)。
//
// 数值一律 `#[ts(type = "number")]`:ts-rs 见到 `u64`/`i64` 会生成 `bigint`,
// 而线上是普通 JSON number,前端也按 number 比较(同 `RequestId::Num` 与
// `RpcError::code` 的口径)。
// ---------------------------------------------------------------------------

/// 本协议版本**支持**旁支协议（不是「当前启用」）。
///
/// 实际下发值由 `side_gate` 决定：能力位是逐宿主、逐时刻的判定，常量只声明
/// 协议存在。`assert!(SIDE_CONVERSATION_V1)` 保留在测试里钉住协议未被移除。
///
/// 打开它的前提是**回放与广播都通了**(Task 13):客户端拿到 `true` 就会把
/// `side/*` 当成可用能力,后续所有视图都建立在「能分页读正文、能收到逐条修订」
/// 之上。半通的宿主谎报 `true`,前端会停在一条永远收不到更新的路径上。
pub const SIDE_CONVERSATION_V1: bool = true;

/// 缺省即错的「必填可空」字段。
///
/// `Option<T>` 加 serde 默认无法区分「键缺失」与「显式 null」:两者都解成
/// `None`。而 `side/close` 的 `expectedActiveTurnId` 必须**显式给值**(Idle 传
/// null、Running 传所确认的 turnId),否则客户端就无法表达「我确认过当前状态」。
/// 所以这里要求键存在,再把 `null` 解成 `None`。
pub fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
}

/// thread 级实时状态。`failed` 刻意缺席：失败是事件不是状态，失败后 thread
/// 立刻回 `Idle`；"失败未读"由前端从 `turn/completed.params.status` 自行表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct Attachment {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub mime: Option<String>,
    // 文件字节数;线上是 JSON 数字(远小于 2^53)。
    #[ts(type = "number")]
    pub size: u64,
}

/// 对工作区内一张图片的引用——**只带路径与元数据，绝不含 base64 字节**。
///
/// `Item` 会随每条通知下发,而传输帧上限为 1 MiB;内联图片字节日志会把帧撑爆。
/// 服务端在起 turn 前把用户选中的图片复制到 `.yi-agent/attachments/<thread_id>/
/// <hash>-<name>`,客户端据此自行读取并渲染。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
pub struct ImageRef {
    pub path: String,
    pub media_type: String,
    // 图片文件字节数;线上是 JSON 数字(远小于 2^53)。
    #[ts(type = "number")]
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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
        #[ts(type = "unknown")]
        input: Value,
        status: ToolStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
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
    /// 一次推理的展示条目（design §6.1 的「思考」卡片）。**不携带凭证**——
    /// signature / encrypted_content 是服务端回传 API 用的协议数据，前端渲染
    /// 不需要，也不应持有（避免经客户端往返被意外改写）。
    Thinking {
        id: String,
        /// 展示用明文（`display:"omitted"` 时为空串）。
        text: String,
        /// redacted 占位（前端渲染「已加密」，无明文可展示）。
        redacted: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../../desktop/src/lib/generated/")]
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
    ///
    /// The family lost its `board_not_created` arm when the host stopped gating
    /// the channel on a board registry: the host can no longer answer "there is
    /// nothing here". No surviving code was renumbered, so `-32020` is simply
    /// retired rather than reused — reusing it would let an old client read a
    /// new reason as "no board".
    #[test]
    fn plugin_query_error_numbers_each_reason_without_renumbering() {
        assert_eq!(
            RpcError::plugin_query_error("plugin_rejected", "no").code,
            -32024
        );
        assert_eq!(
            RpcError::plugin_query_error("plugin_not_installed", "no").code,
            -32023
        );
        assert_eq!(
            RpcError::plugin_query_error("plugin_unavailable", "no").code,
            -32022
        );
        assert_eq!(
            RpcError::plugin_query_error("daemon_unavailable", "no").code,
            -32021
        );
        // The retired board reason must not come back through the catch-all.
        assert_eq!(
            RpcError::plugin_query_error("board_not_created", "no").code,
            -32603,
            "the retired reason must not keep a dedicated numeric code"
        );
        let unknown = RpcError::plugin_query_error("something_else", "no");
        assert_eq!(unknown.code, -32603);
        assert_eq!(unknown.data.unwrap()["code"], "something_else");
    }

    /// `model_not_found` gets its own numeric code without renumbering the
    /// board/plugin vocabulary; the UI only reads `data.code`.
    #[test]
    fn model_capabilities_changed_notification_carries_method_and_payload() {
        let n = Notification::ModelCapabilitiesChanged {
            name: "A".into(),
            reasoning_capabilities: Some(yi_agent_runtime::models::ReasoningCapabilities {
                chat: Some(yi_agent_runtime::models::ChannelCapability {
                    supported: vec!["low".into()],
                    probed_at: 7,
                    error: None,
                }),
                ..Default::default()
            }),
        };
        let v = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "model/capabilitiesChanged");
        assert_eq!(v["params"]["name"], "A");
        assert_eq!(
            v["params"]["reasoning_capabilities"]["chat"]["supported"][0],
            "low"
        );
        // 机器级清单属性：无 thread 键，恒推。
        assert!(n.thread_key().is_none());
        assert!(matches!(n.delivery(), Delivery::Global));
    }

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

    /// 客户端上下文锚点失效（该会话在其不知情时被 clear）。数值码取下一个空位，
    /// UI 只读 `data.code`——与 `plugin_query_error` / `model_not_found` 同一约定。
    #[test]
    fn stale_context_has_a_stable_numeric_code_and_reason() {
        let error = RpcError::stale_context("thread-a");
        assert_eq!(
            error.code, -32026,
            "-32026 must stay reserved for stale_context"
        );
        assert_eq!(error.data.unwrap()["code"], "stale_context");
        assert!(
            error.message.contains("thread-a"),
            "message must name the thread"
        );
    }

    /// 新码不得与既有码撞车。
    #[test]
    fn stale_context_does_not_collide_with_existing_codes() {
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::not_initialized().code
        );
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::unknown_thread("t").code
        );
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::turn_in_progress("t").code
        );
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::model_not_found("m").code
        );
        assert_ne!(
            RpcError::stale_context("t").code,
            RpcError::plugin_query_error("plugin_rejected", "x").code
        );
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
            completed_at: None,
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
            total_input_tokens: 1_000,
            total_output_tokens: 300,
            total_cache_creation_input_tokens: 500,
            total_cache_read_input_tokens: 700,
            subagent_input_tokens: 11,
            subagent_output_tokens: 22,
            subagent_cache_read_tokens: 33,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tokenUsage/updated");
        assert_eq!(v["params"]["input_tokens"], 10);
        assert_eq!(v["params"]["output_tokens"], 3);
        assert_eq!(v["params"]["cache_creation_input_tokens"], 100);
        assert_eq!(v["params"]["cache_read_input_tokens"], 200);
        assert_eq!(v["params"]["total_input_tokens"], 1_000);
        assert_eq!(v["params"]["total_output_tokens"], 300);
        // subagent 三个值走 snake_case（与其它用量字段同款），且不并入 total_*。
        assert_eq!(v["params"]["subagent_input_tokens"], 11);
        assert_eq!(v["params"]["subagent_output_tokens"], 22);
        assert_eq!(v["params"]["subagent_cache_read_tokens"], 33);
        assert_eq!(v["params"]["total_cache_creation_input_tokens"], 500);
        assert_eq!(v["params"]["total_cache_read_input_tokens"], 700);
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

    /// effort 变更通知的 wire 形状:`thread/reasoningEffortChanged`,参数
    /// snake_case。与 `permissionModeChanged` 同为列表层恒推,且按 thread 路由。
    #[test]
    fn reasoning_effort_changed_notification_carries_method_and_effort() {
        let n = Notification::ReasoningEffortChanged {
            thread_id: "t1".into(),
            effort: Some("high".into()),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/reasoningEffortChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["effort"], "high");
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    /// 清覆盖(`effort: None`)时 `effort` 键必须缺省,而不是显式 `null`:
    /// 客户端据此区分「没有值」与「本次没带」——与 `model_ref` 的既有约定一致。
    #[test]
    fn cleared_reasoning_effort_omits_the_field() {
        let n = Notification::ReasoningEffortChanged {
            thread_id: "t1".into(),
            effort: None,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/reasoningEffortChanged");
        assert!(
            v["params"].get("effort").is_none(),
            "a cleared override must omit the field: {v}"
        );
    }

    /// 保留开关通知的 wire 形状:`thread/preserveThinkingChanged`,参数 snake_case。
    /// 与 `reasoningEffortChanged` 同为列表层恒推,且按 thread 路由。
    #[test]
    fn preserve_thinking_changed_notification_carries_method_and_flag() {
        let n = Notification::PreserveThinkingChanged {
            thread_id: "t1".into(),
            preserve: false,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/preserveThinkingChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["preserve"], false);
        // 与 `reasoningEffortChanged` 同为会话元数据:必须列表层恒推,否则窗口外
        // 会话的控件在不同客户端之间「概率性不同步」。
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    /// 标题变更通知的 wire 形状:`thread/titleChanged`,参数 snake_case。
    ///
    /// 与 `modelChanged` / `permissionModeChanged` 同为**会话元数据**:必须列表层
    /// 恒推,否则窗口外会话的标题在手机上「概率性不同步」;并按 thread 路由。
    /// 它**直接携带权威标题**,客户端就地落笔,不必再拉一次 `thread/listAll`。
    #[test]
    fn title_changed_notification_carries_method_and_title() {
        let n = Notification::TitleChanged {
            thread_id: "t1".into(),
            title: "修复登录报错".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/titleChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["title"], "修复登录报错");
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    /// 标签变更通知的 wire 形状：`thread/tagChanged`，参数 snake_case。
    ///
    /// 与 `titleChanged` 同族：会话元数据、列表层恒推、按 thread 路由、直接携带
    /// 权威值（`null` = 已清除）。窗口外会话的标签变更同样要送达，否则手机端
    /// 「概率性不同步」。
    #[test]
    fn tag_changed_notification_carries_method_and_tag() {
        let n = Notification::TagChanged {
            thread_id: "t1".into(),
            tag: Some("red".into()),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tagChanged");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["tag"], "red");
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    /// 分组变更通知的 wire 形状：`thread/groupsChanged`，参数 snake_case。
    ///
    /// 它**不带** thread_id（分组属于目录不属于某条会话），故 `thread_key()` 为
    /// `None`，但仍在列表层恒推——它只关一个目录，不关正文流。
    #[test]
    fn groups_changed_notification_carries_the_workspace() {
        let n = Notification::GroupsChanged {
            workspace: "/p".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/groupsChanged");
        assert_eq!(v["params"]["workspace"], "/p");
        assert_eq!(n.delivery(), Delivery::List);
        assert_eq!(n.thread_key(), None);
    }

    #[test]
    fn tag_changed_notification_serializes_a_cleared_tag_as_null() {
        let n = Notification::TagChanged {
            thread_id: "t1".into(),
            tag: None,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        // 必须**存在且为 null**，而不是整个键被省略：前者客户端读 `tag === null`
        // 即知「清除」，后者只能靠 `undefined`，两者在跨端解码里并不等价。
        assert!(
            v["params"].as_object().unwrap().contains_key("tag"),
            "tag 键必须存在: {v}"
        );
        assert!(v["params"]["tag"].is_null(), "清除必须是显式 null: {v}");
    }

    /// 标签显示名变更通知：**全局**帧，不属于任何 thread。
    #[test]
    fn tag_names_updated_notification_is_global() {
        let mut names = std::collections::BTreeMap::new();
        names.insert("red".to_string(), "紧急".to_string());
        let n = Notification::TagNamesUpdated { tag_names: names };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "ui/tagNames/updated");
        assert_eq!(v["params"]["tag_names"]["red"], "紧急");
        assert_eq!(n.delivery(), Delivery::Global);
        assert_eq!(n.thread_key(), None);
    }

    /// 运行期告知的 wire 形状:`thread/notice`,参数 snake_case,带 level。
    ///
    /// 投递层级与 `item/delta` 一致为**内容层**:它描述这一次运行的走法,按 thread
    /// 路由,不该把窗口外会话的控件刷成脏。
    #[test]
    fn notice_notification_carries_method_level_and_text() {
        let n = Notification::Notice {
            thread_id: "t1".into(),
            level: yi_agent_core::NoticeLevel::Warn,
            text: "该模型不支持所配推理档，已忽略".into(),
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/notice");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["level"], "Warn");
        assert_eq!(v["params"]["text"], "该模型不支持所配推理档，已忽略");
        assert_eq!(n.delivery(), Delivery::Content);
        assert_eq!(n.thread_key(), Some("t1"));
    }

    /// 推理增量的 wire 形状:`thread/reasoningDelta`,参数带 kind 判别。
    #[test]
    fn reasoning_delta_notification_carries_method_text_and_kind() {
        let n = Notification::ReasoningDelta {
            thread_id: "t1".into(),
            text: "thinking".into(),
            kind: yi_agent_core::ReasoningKind::Summary,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/reasoningDelta");
        assert_eq!(v["params"]["thread_id"], "t1");
        assert_eq!(v["params"]["text"], "thinking");
        assert_eq!(v["params"]["kind"], "Summary");
        assert_eq!(n.delivery(), Delivery::Content);
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

    /// thinking 展示条目**绝不**携带凭证：signature / encrypted_content 是服务端
    /// 回传 API 用的协议数据，前端渲染不需要，也不该持有（避免经客户端往返被改写）。
    /// 这条测试是「凭证不下发前端」的回归钉。
    #[test]
    fn thinking_item_serializes_without_credentials() {
        let item = Item::Thinking {
            id: "th_1".into(),
            text: "summary".into(),
            redacted: false,
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["type"], "thinking");
        assert_eq!(json["id"], "th_1");
        assert_eq!(json["text"], "summary");
        assert_eq!(json["redacted"], false);
        assert!(json.get("signature").is_none(), "凭证不得下发前端");
        assert!(json.get("encrypted_content").is_none(), "凭证不得下发前端");
        let raw = json.to_string();
        assert!(
            !raw.contains("signature") && !raw.contains("encrypted_content"),
            "thinking item 的 wire 形状不得出现任何凭证字段: {raw}"
        );
    }

    /// The close confirmation is *required and nullable*: a missing key means the
    /// client never confirmed the current state, an explicit null means it confirmed
    /// "idle". `Option` alone cannot tell those apart, hence the helper.
    #[test]
    fn side_close_requires_explicit_nullable_confirmation() {
        let missing = serde_json::json!({"sideId":"00000000-0000-4000-8000-000000000001"});
        assert!(decode_side_close(missing).is_err());
        let idle = serde_json::json!({"sideId":"00000000-0000-4000-8000-000000000001",
            "expectedActiveTurnId":null});
        assert!(decode_side_close(idle).is_ok());
        let running = serde_json::json!({"sideId":"00000000-0000-4000-8000-000000000001",
            "expectedActiveTurnId":"00000000-0000-4000-8000-000000000002"});
        let parsed = decode_side_close(running).unwrap();
        assert_eq!(
            parsed.expected_active_turn_id.as_deref(),
            Some("00000000-0000-4000-8000-000000000002")
        );
    }

    fn decode_side_close(value: serde_json::Value) -> Result<SideCloseParams, serde_json::Error> {
        serde_json::from_value(value)
    }

    /// A turn must not carry attachments: a side answer never accepts files, and
    /// silently ignoring the field would let a client believe it did.
    #[test]
    fn side_turn_start_rejects_unknown_fields() {
        let with_attachments = serde_json::json!({
            "sideId": "00000000-0000-4000-8000-000000000001",
            "text": "问题",
            "clientRequestId": "00000000-0000-4000-8000-000000000002",
            "attachments": [],
        });
        assert!(serde_json::from_value::<SideTurnStartParams>(with_attachments).is_err());
    }

    /// Requests are camelCase on the wire, responses snake_case. Getting this
    /// backwards would make every client silently read nulls.
    #[test]
    fn side_request_and_response_field_casing() {
        let params: SideReadItemsParams = serde_json::from_value(serde_json::json!({
            "sideId": "s", "snapshotToken": "t", "cursor": "c"
        }))
        .unwrap();
        assert_eq!(params.side_id, "s");
        assert_eq!(params.snapshot_token.as_deref(), Some("t"));

        let result = SideReadItemsResult {
            items: Vec::new(),
            snapshot_seq: 7,
            status: SideStatusWire::Idle,
            usage: SideUsageWire::default(),
            active_turn_id: None,
            accepted_requests: Default::default(),
            effective_config: SideEffectiveConfigWire {
                model: "m".into(),
                reasoning_effort: None,
                preserve_thinking: true,
                read_only: true,
                tool_names: vec!["read".into()],
            },
            snapshot_token: "tok".into(),
            next_cursor: None,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["snapshot_seq"], serde_json::json!(7));
        assert_eq!(value["snapshot_token"], serde_json::json!("tok"));
        assert!(
            value.get("snapshotSeq").is_none(),
            "responses are snake_case"
        );
    }

    /// The numeric fields must survive as plain JSON numbers (not strings) and the
    /// TS side must see `number`, so the annotations are load-bearing.
    #[test]
    fn side_summary_serde_shape() {
        let summary = crate::side_conversation::SideSummary {
            side_id: "s".into(),
            parent_thread_id: "p".into(),
            status: yi_agent_runtime::side_conversation::SideStatus::Idle,
            snapshot_revision: 3,
            last_seq: 9,
            active_turn_id: None,
            expires_at: Some(1_700_000_000_000),
            generation: 2,
            stopping: false,
        };
        let wire = SideSummaryWire::from_registry(&summary);
        let value = serde_json::to_value(&wire).unwrap();
        assert_eq!(value["last_seq"], serde_json::json!(9));
        assert_eq!(value["expires_at"], serde_json::json!(1_700_000_000_000i64));
        assert_eq!(value["status"], serde_json::json!("idle"));
        assert_eq!(value["parent_thread_id"], serde_json::json!("p"));
    }

    /// Every side notification must be filterable by its **parent** thread id, or it
    /// would either be dropped for the parent's subscribers or broadcast globally.
    #[test]
    fn side_notifications_key_on_the_parent_thread() {
        let summary = SideSummaryWire::from_registry(&crate::side_conversation::SideSummary {
            side_id: "s".into(),
            parent_thread_id: "p".into(),
            status: yi_agent_runtime::side_conversation::SideStatus::Running,
            snapshot_revision: 1,
            last_seq: 1,
            active_turn_id: Some("t".into()),
            expires_at: None,
            generation: 1,
            stopping: false,
        });
        let cases: Vec<(Notification, &str)> = vec![
            (
                Notification::SideChanged {
                    parent_thread_id: "p".into(),
                    side_id: "s".into(),
                    generation: 1,
                    seq: 1,
                    summary,
                },
                "side/changed",
            ),
            (
                Notification::SideItemUpdated {
                    parent_thread_id: "p".into(),
                    side_id: "s".into(),
                    generation: 1,
                    seq: 2,
                    turn_id: "t".into(),
                    item: Item::AgentMessage {
                        id: "i".into(),
                        text: "x".into(),
                    },
                },
                "side/itemUpdated",
            ),
            (
                Notification::SideTurnCompleted {
                    parent_thread_id: "p".into(),
                    side_id: "s".into(),
                    generation: 1,
                    seq: 3,
                    turn_id: "t".into(),
                    status: SideStatusWire::Idle,
                    reason: None,
                    error: None,
                    usage: SideUsageWire::default(),
                },
                "side/turnCompleted",
            ),
            (
                Notification::SideClosed {
                    parent_thread_id: "p".into(),
                    side_id: "s".into(),
                    generation: 1,
                    seq: 4,
                    reason: "user_closed".into(),
                },
                "side/closed",
            ),
        ];
        for (notification, method) in cases {
            let envelope = serde_json::to_value(NotificationEnvelope::new(&notification)).unwrap();
            assert_eq!(envelope["method"], serde_json::json!(method));
            assert_eq!(
                envelope["params"]["parent_thread_id"],
                serde_json::json!("p")
            );
            assert_eq!(envelope["params"]["side_id"], serde_json::json!("s"));
            assert_eq!(
                envelope["params"]["seq"],
                serde_json::json!(notification_seq(&notification))
            );
            assert_eq!(notification.thread_key(), Some("p"));
        }
    }

    fn notification_seq(notification: &Notification) -> u64 {
        match notification {
            Notification::SideChanged { seq, .. }
            | Notification::SideItemUpdated { seq, .. }
            | Notification::SideTurnCompleted { seq, .. }
            | Notification::SideClosed { seq, .. } => *seq,
            _ => panic!("not a side notification"),
        }
    }

    /// The error table must be *allocated*, not guessed: every side reason gets a
    /// code, no two reasons share one unless they deliberately reuse the existing
    /// generic code, and none of the new codes collides with a pre-existing one.
    #[test]
    fn side_error_codes_are_unique_and_stable() {
        use yi_agent_runtime::side_conversation::{SideError, SideErrorReason as R};
        let reasons = [
            (R::SideNotFound, -32031),
            (R::ParentNotStarted, -32032),
            (R::SideLimit, -32033),
            (R::SnapshotTooLarge, -32034),
            (R::SourceChanged, -32035),
            (R::SideClosing, -32036),
            (R::CloseConflict, -32037),
            (R::SnapshotExpired, -32038),
            (R::SideCreating, -32039),
            (R::RequestLimit, -32040),
            (R::ResourceLimit, -32041),
            (R::Cancelled, -32042),
            (R::TurnFailed, -32043),
            (R::InvalidParams, -32602),
            (R::TurnInProgress, -32012),
            (R::InsufficientScope, -32014),
            (R::UnknownThread, -32011),
        ];
        let existing = [
            -32700, -32600, -32601, -32602, -32603, -32001, -32010, -32011, -32012, -32013, -32014,
            -32020, -32021, -32022, -32023, -32024, -32025, -32026, -32027,
        ];
        let mut seen = std::collections::BTreeMap::new();
        for (reason, expected) in reasons {
            let error = RpcError::side(&SideError::new(reason, "x"));
            assert_eq!(error.code, expected, "{reason:?} must map to {expected}");
            assert_eq!(
                error.data.as_ref().unwrap()["reason"],
                serde_json::json!(reason.as_str()),
                "data.reason must be the stable string"
            );
            // Reuse is allowed only where the reason *is* the existing failure.
            let reuse = matches!(
                reason,
                R::InvalidParams | R::TurnInProgress | R::InsufficientScope | R::UnknownThread
            );
            if !reuse {
                assert!(
                    !existing.contains(&expected),
                    "{reason:?} reuses the pre-existing code {expected}"
                );
            }
            assert!(
                seen.insert(expected, reason).is_none(),
                "{reason:?} duplicates a code another reason already owns"
            );
        }
    }

    /// 能力位现在必须是开着的:回放与广播已经落地,客户端要据此启用旁支视图。
    ///
    /// 常量断言正是本测试的全部意图:它钉住「没有回放就别打开」这条前置条件在
    /// 代码里的落点。`assertions_on_constants` 在这里是误报。
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn side_capability_is_on_once_replay_lands() {
        assert!(SIDE_CONVERSATION_V1);
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
