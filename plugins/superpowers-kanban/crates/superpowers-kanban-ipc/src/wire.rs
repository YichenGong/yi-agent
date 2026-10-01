use serde::{Deserialize, Serialize};

/// 插件说的协议版本。**必须与宿主的 `yi-agent-store::ipc::PROTOCOL_VERSION` 同步**。
///
/// 插件零 `yi-agent-*` 依赖，协议只能手写复刻，版本号也只能手抄——这正是它
/// 曾经停在 1、而宿主已到 2 的原因：daemon 严格拒绝版本不匹配，于是"建会话"
/// 这条路从 v2 起一直是断的，卡片永远停在 queued。
///
/// 宿主的 `wire` 形状（信封字段、命令/回复变体）在两版之间**没有变**，
/// 变版本号只是为了拒绝旧 daemon 时给出清晰错误而非解析错误。
/// 宿主侧有测试读这个文件比对版本，漂移会立刻炸。
pub const PROTOCOL_VERSION: u32 = 2;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Command {
    CreateAutonomousSession {
        objective: String,
        workdir: String,
    },
    ListTaskSummaries {
        session_id: Option<String>,
        active_only: bool,
    },
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<serde_json::Value>,
    pub result: Reply,
}

/// 响应结果。用 `Reply` 而非 `Result` 以免与 `std::result::Result` 冲突。
/// `Other` 兜住未来新增的变体，使插件不会因 daemon 升级而解析失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type")]
pub enum Reply {
    AutonomousSessionCreated {
        session_id: String,
        root_task_id: String,
    },
    TaskSummaries {
        tasks: Vec<TaskSummary>,
    },
    Status {
        high_water_event_id: i64,
    },
    Error {
        code: String,
        message: Option<String>,
    },
    Other {
        value: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub state: String,
    pub is_root: bool,
}

impl<'de> Deserialize<'de> for Reply {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let value = serde_json::Value::deserialize(deserializer)?;
        let tag = value
            .get("type")
            .and_then(|tag| tag.as_str())
            .ok_or_else(|| D::Error::custom("reply is missing its `type` tag"))?
            .to_owned();
        match tag.as_str() {
            "AutonomousSessionCreated" => {
                #[derive(Deserialize)]
                struct Shape {
                    session_id: String,
                    root_task_id: String,
                }
                let shape: Shape = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::AutonomousSessionCreated {
                    session_id: shape.session_id,
                    root_task_id: shape.root_task_id,
                })
            }
            "TaskSummaries" => {
                #[derive(Deserialize)]
                struct Shape {
                    tasks: Vec<TaskSummary>,
                }
                let shape: Shape = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::TaskSummaries { tasks: shape.tasks })
            }
            "Status" => {
                #[derive(Deserialize)]
                struct Shape {
                    high_water_event_id: i64,
                }
                let shape: Shape = serde_json::from_value(value).map_err(D::Error::custom)?;
                Ok(Reply::Status {
                    high_water_event_id: shape.high_water_event_id,
                })
            }
            "Error" => {
                let code = value
                    .get("code")
                    .and_then(|code| code.as_str())
                    .unwrap_or("Internal")
                    .to_owned();
                let message = value
                    .get("message")
                    .and_then(|message| message.as_str())
                    .map(str::to_owned);
                Ok(Reply::Error { code, message })
            }
            _ => Ok(Reply::Other { value }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_request_serializes_with_the_daemons_tag() {
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "1".into(),
            command: Command::CreateAutonomousSession {
                objective: "implement the plan".into(),
                workdir: "/tmp/worktree".into(),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"1","command":{{"type":"CreateAutonomousSession","objective":"implement the plan","workdir":"/tmp/worktree"}}}}"#)
        );
    }

    #[test]
    fn a_task_summaries_request_serializes_both_fields() {
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request_id: "2".into(),
            command: Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        assert_eq!(
            json,
            format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"2","command":{{"type":"ListTaskSummaries","session_id":null,"active_only":false}}}}"#)
        );
    }

    #[test]
    fn a_create_response_round_trips() {
        let json = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"1","result":{{"type":"AutonomousSessionCreated","session_id":"s1","root_task_id":"t1"}}}}"#);
        let envelope: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope.protocol_version, PROTOCOL_VERSION);
        assert_eq!(envelope.request_id, "1");
        assert_eq!(
            envelope.result,
            Reply::AutonomousSessionCreated {
                session_id: "s1".into(),
                root_task_id: "t1".into(),
            }
        );
    }

    #[test]
    fn a_task_summaries_response_round_trips() {
        let json = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"2","result":{{"type":"TaskSummaries","tasks":[{{"task_id":"t1","state":"running","is_root":true}}]}}}}"#);
        let envelope: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(
            envelope.result,
            Reply::TaskSummaries {
                tasks: vec![TaskSummary {
                    task_id: "t1".into(),
                    state: "running".into(),
                    is_root: true,
                }],
            }
        );
    }

    #[test]
    fn an_error_response_round_trips() {
        let json = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"3","result":{{"type":"Error","code":"InvalidState","message":"workdir does not exist: /nope"}}}}"#);
        let envelope: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(
            envelope.result,
            Reply::Error {
                code: "InvalidState".into(),
                message: Some("workdir does not exist: /nope".into()),
            }
        );
    }

    #[test]
    fn an_unknown_reply_variant_decodes_as_other_instead_of_failing() {
        // 前向兼容：daemon 新增变体不应让插件解析崩溃。
        let json = format!(r#"{{"protocol_version":{PROTOCOL_VERSION},"request_id":"4","result":{{"type":"SomethingBrandNew","x":1}}}}"#);
        let envelope: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert!(matches!(envelope.result, Reply::Other { .. }));
    }

    #[test]
    fn the_frame_limit_matches_the_daemon() {
        assert_eq!(MAX_FRAME_BYTES, 1_048_576);
    }
}
