//! JSON-RPC 2.0 信封与 yi-agent 协议类型。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const JSONRPC_VERSION: &str = "2.0";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

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
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
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
    pub fn not_initialized() -> Self {
        Self::new(-32010, "server not initialized")
    }
    pub fn unknown_thread(id: &str) -> Self {
        Self::new(-32011, format!("unknown thread: {id}"))
    }
    pub fn turn_in_progress(id: &str) -> Self {
        Self::new(-32012, format!("turn already in progress: {id}"))
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
    },
    #[serde(rename = "turn/started")]
    TurnStarted { thread_id: String, turn_id: String },
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
    #[serde(rename = "turn/completed")]
    TurnCompleted {
        thread_id: String,
        turn_id: String,
        status: TurnStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage {
        thread_id: String,
        model: String,
        input_tokens: u32,
        output_tokens: u32,
    },
    #[serde(rename = "error")]
    Error { message: String },
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Item {
    UserMessage {
        id: String,
        text: String,
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
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    #[test]
    fn request_envelope_parses_string_id() {
        let raw = r#"{"jsonrpc":"2.0","id":"perm-3","method":"x","params":{}}"#;
        let req: RequestEnvelope = serde_json::from_str(raw).unwrap();
        assert!(matches!(req.id, RequestId::Str(ref s) if s == "perm-3"));
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
}
