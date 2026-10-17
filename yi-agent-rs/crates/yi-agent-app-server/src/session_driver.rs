//! 通用会话驱动：把「该起/该继续哪个会话」交给插件决定。
//!
//! 本模块不认识看板、卡片、合并。它只认识两件事：宿主能提供的通用事实
//! （某个会话闲不闲），与宿主能执行的两个通用动作（新建会话 / 追加一轮）。
//!
//! 本任务到达时消费方尚未接线（`driver_step` 在下一任务、`SessionLauncher` 的真
//! 实现更晚），所以下面这些类型在生产构建下暂时「未读」。它们是**跨进程协议面**
//! （字段名与插件逐字对齐），删掉才是错的，故照本 crate 既有的协议字段放行做法
//! 显式 `#[allow(dead_code)]`；消费方接入后应当把放行去掉。

use serde_json::Value;

/// 宿主能执行的通用动作。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DriverActionKind {
    /// 在一个 workdir 里新建会话，并投递 `objective` 作为首轮。
    Launch {
        workdir: String,
        title: String,
        objective: String,
    },
    /// 向已有会话追加一轮。
    Append {
        thread_id: String,
        objective: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct DriverAction {
    pub action_id: String,
    pub kind: DriverActionKind,
}

/// 解析插件的 `driver/next` 应答。
///
/// `{"action": null}`、缺 `action_id`、或未知 `kind` 一律返回 `None`：调度器
/// 前向兼容——插件日后新增动作类型时，旧宿主只是「这一拍什么都不做」。
#[allow(dead_code)]
pub(crate) fn parse_action(value: &Value) -> Option<DriverAction> {
    let action = value.get("action").unwrap_or(value);
    let action_id = action.get("action_id")?.as_str()?.to_string();
    let kind = action.get("kind")?.as_str()?;
    let kind = match kind {
        "launch" => DriverActionKind::Launch {
            workdir: action.get("workdir")?.as_str()?.to_string(),
            title: action
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
        },
        "append" => DriverActionKind::Append {
            thread_id: action.get("thread_id")?.as_str()?.to_string(),
            objective: action.get("objective")?.as_str()?.to_string(),
        },
        _ => return None,
    };
    Some(DriverAction { action_id, kind })
}

/// 宿主为本插件启动或追加过的会话（宿主自身的通用记账）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct DriverThread {
    pub thread_id: String,
}

/// 一次动作的结果，下一拍回传插件。
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) struct ActionResult {
    pub action_id: String,
    pub ok: bool,
    pub thread_id: Option<String>,
    pub error: Option<String>,
}

/// 执行动作的副作用。真实现复用 `start_thread_core` / `prepare_turn_core` 的
/// 无响应帧路径（Task 5 接线）；测试用替身驱动纯逻辑。
#[allow(dead_code)]
pub(crate) trait SessionLauncher {
    async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_null_answer_yields_no_action() {
        assert!(parse_action(&json!({ "action": null })).is_none());
    }

    #[test]
    fn a_launch_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
            } => {
                assert_eq!(workdir, "/w");
                assert_eq!(title, "T");
                assert_eq!(objective, "do it");
            }
            other => panic!("expected launch, got {other:?}"),
        }
    }

    #[test]
    fn an_append_answer_parses_its_fields() {
        let value = json!({
            "action_id": "a2", "kind": "append",
            "thread_id": "thread-1", "objective": "merge it"
        });
        match parse_action(&value).unwrap().kind {
            DriverActionKind::Append {
                thread_id,
                objective,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(objective, "merge it");
            }
            other => panic!("expected append, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_kind_is_ignored_rather_than_panicking() {
        assert!(parse_action(&json!({ "action_id": "a3", "kind": "reboot" })).is_none());
    }

    #[test]
    fn a_missing_action_id_is_ignored() {
        assert!(parse_action(&json!({ "kind": "launch", "workdir": "/w" })).is_none());
    }
}
