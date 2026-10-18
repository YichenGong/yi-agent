//! 通用会话驱动：把「该起/该继续哪个会话」交给插件决定。
//!
//! 本模块不认识看板、卡片、合并。它只认识两件事：宿主能提供的通用事实
//! （某个会话闲不闲），与宿主能执行的两个通用动作（新建会话 / 追加一轮）。
//!
//! 本任务到达时消费方尚未接线：拉拍循环 `driver_step` 已就位但主循环要到 Task 6
//! 才调它，`SessionLauncher` 的真实现更晚（Task 5）。所以下面这些类型、以及
//! `driver_step` / `threads_payload` 在生产构建下暂时「未读」。它们是**跨进程协议
//! 面**（字段名与插件逐字对齐），删掉才是错的，故照本 crate 既有的协议字段放行
//! 做法显式 `#[allow(dead_code)]`；消费方接入后应当把放行去掉。

use serde_json::Value;
use std::path::Path;

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

/// 把宿主持有的会话事实编成 `driver/next` 的 `threads` 参数。
#[allow(dead_code)]
pub(crate) fn threads_payload(threads: &[DriverThread], idle: &dyn Fn(&str) -> bool) -> Value {
    Value::Array(
        threads
            .iter()
            .map(|thread| {
                serde_json::json!({
                    "thread_id": thread.thread_id,
                    "idle": idle(&thread.thread_id),
                })
            })
            .collect(),
    )
}

/// 拉一拍：问插件「下一步做什么」，执行至多一个动作，返回其结果。
///
/// `results` 里是上一拍尚未回报的结果；本拍把它们上送，**送达之后**才清空
/// （`query` 失败时保留，下一拍重发）。
#[allow(dead_code)]
pub(crate) async fn driver_step<L: SessionLauncher>(
    query: &dyn Fn(&Value) -> Result<Value, String>,
    threads: &mut Vec<DriverThread>,
    results: &mut Vec<ActionResult>,
    launcher: &mut L,
) -> Option<ActionResult> {
    let results_payload = Value::Array(
        results
            .iter()
            .map(|result| {
                serde_json::json!({
                    "action_id": result.action_id,
                    "ok": result.ok,
                    "thread_id": result.thread_id,
                    "error": result.error,
                })
            })
            .collect(),
    );
    let params = serde_json::json!({
        "threads": threads_payload(threads, &|_| false),
        "results": results_payload,
    });
    let answer = match query(&params) {
        Ok(answer) => answer,
        // 本拍无法推进：动作留给下一拍重试，待回报的 results 保留不丢。
        Err(_) => return None,
    };
    // 只在成功上送之后消费：`query` 失败时上面已提前返回，`results` 原样留给
    // 下一拍重发。注意清空必须早于下面的 `?`——答案虽为空/未知动作，results 却
    // 已经送达插件，绝不能再重发一次。
    results.clear();

    let action = parse_action(&answer)?;

    match launcher.launch(&action).await {
        Ok(thread_id) => {
            if let DriverActionKind::Launch { .. } = &action.kind {
                threads.push(DriverThread {
                    thread_id: thread_id.clone(),
                });
            }
            Some(ActionResult {
                action_id: action.action_id,
                ok: true,
                thread_id: Some(thread_id),
                error: None,
            })
        }
        Err(error) => Some(ActionResult {
            action_id: action.action_id,
            ok: false,
            thread_id: None,
            error: Some(error.to_string()),
        }),
    }
}

/// 声明了 `session_driver` 的插件名（按插件名排序，稳定）。
///
/// 读的是**通用**清单目录；宿主不认识任何具体插件。
///
/// 本任务（Task 4）到达时消费方尚未接线：把插件列表接进主循环是 Task 6 的事，
/// 在此之前生产构建下它「未读」。删除才是错的（这是宿主发现驱动插件的唯一入口），
/// 故照本 crate 既有做法显式 `#[allow(dead_code)]`；Task 6 接线后应把放行去掉。
#[allow(dead_code)]
pub(crate) fn driver_plugins(scope: &Path) -> Vec<String> {
    let dir = scope.join(".yi-agent").join("supervisors");
    let mut names: Vec<String> = yi_agent_supervisors::manifest::load_manifests(&dir)
        .into_iter()
        .filter(|manifest| manifest.session_driver)
        .map(|manifest| manifest.name)
        .collect();
    names.sort();
    names
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

    use std::cell::RefCell;

    #[derive(Default)]
    struct FakeLauncher {
        seen: Vec<DriverAction>,
        thread_id: String,
        fail: Option<String>,
    }

    impl SessionLauncher for FakeLauncher {
        async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String> {
            self.seen.push(action.clone());
            match &self.fail {
                Some(detail) => Err(anyhow::anyhow!(detail.clone())),
                None => Ok(self.thread_id.clone()),
            }
        }
    }

    fn answering(reply: Value) -> impl Fn(&Value) -> Result<Value, String> {
        move |_params: &Value| Ok(reply.clone())
    }

    #[tokio::test]
    async fn a_launch_action_is_executed_and_reported_ok() {
        let reply = json!({
            "action_id": "a1", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            thread_id: "thread-9".into(),
            ..Default::default()
        };

        let outcome =
            driver_step(&answering(reply), &mut threads, &mut results, &mut launcher).await;

        let outcome = outcome.unwrap();
        assert_eq!(outcome.action_id, "a1");
        assert!(outcome.ok);
        assert_eq!(launcher.seen.len(), 1, "the launcher runs exactly once");
    }

    #[tokio::test]
    async fn a_failed_launch_is_reported_with_the_error_and_no_thread() {
        let reply = json!({
            "action_id": "a2", "kind": "launch",
            "workdir": "/w", "title": "T", "objective": "do it"
        });
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher {
            fail: Some("worktree gone".into()),
            ..Default::default()
        };

        let outcome = driver_step(&answering(reply), &mut threads, &mut results, &mut launcher)
            .await
            .unwrap();

        assert!(!outcome.ok);
        assert_eq!(outcome.error.as_deref(), Some("worktree gone"));
        assert!(outcome.thread_id.is_none());
    }

    #[tokio::test]
    async fn a_null_answer_performs_no_side_effect() {
        let mut threads = Vec::new();
        let mut results = Vec::new();
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(
            &answering(json!({ "action": null })),
            &mut threads,
            &mut results,
            &mut launcher,
        )
        .await;

        assert!(outcome.is_none());
        assert!(launcher.seen.is_empty());
    }

    #[tokio::test]
    async fn pending_results_are_sent_on_the_next_call_and_then_cleared() {
        let reply = json!({ "action": null });
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: false,
            thread_id: None,
            error: Some("boom".into()),
        }];
        let seen: RefCell<Vec<Value>> = RefCell::new(Vec::new());
        let query = |params: &Value| {
            seen.borrow_mut().push(params.clone());
            Ok(reply.clone())
        };
        let mut launcher = FakeLauncher::default();

        driver_step(&query, &mut threads, &mut results, &mut launcher).await;

        let sent = seen.borrow().clone();
        assert_eq!(sent[0]["results"][0]["action_id"], "a0");
        assert_eq!(sent[0]["results"][0]["ok"], false);
        assert!(results.is_empty(), "consumed results are cleared");
    }

    #[tokio::test]
    async fn pending_results_survive_a_failed_query() {
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: true,
            thread_id: Some("thread-1".into()),
            error: None,
        }];
        let query = |_params: &Value| Err("daemon down".to_string());
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(&query, &mut threads, &mut results, &mut launcher).await;

        assert!(outcome.is_none(), "a failed query cannot advance the tick");
        assert!(launcher.seen.is_empty(), "no action is executed");
        assert_eq!(
            results.len(),
            1,
            "results that were never delivered must be kept for the next tick"
        );
        assert_eq!(results[0].action_id, "a0");
    }

    #[tokio::test]
    async fn a_delivered_null_answer_still_consumes_the_results() {
        let mut threads = Vec::new();
        let mut results = vec![ActionResult {
            action_id: "a0".into(),
            ok: false,
            thread_id: None,
            error: Some("boom".into()),
        }];
        let mut launcher = FakeLauncher::default();

        let outcome = driver_step(
            &answering(json!({ "action": null })),
            &mut threads,
            &mut results,
            &mut launcher,
        )
        .await;

        assert!(outcome.is_none());
        assert!(
            results.is_empty(),
            "the results were delivered, so a null answer must not re-send them"
        );
    }

    #[test]
    fn only_plugins_declaring_session_driver_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let scope = dir.path();
        let manifests = scope.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("driver.json"),
            r#"{"name":"driver","command":"/bin/d","args":[],"switch_key":"d",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false,"session_driver":true}"#,
        )
        .unwrap();
        std::fs::write(
            manifests.join("plain.json"),
            r#"{"name":"plain","command":"/bin/p","args":[],"switch_key":"p",
                "restart_backoff_ms":1000,"restart_backoff_max_ms":30000,
                "stop_when_disabled":false}"#,
        )
        .unwrap();

        assert_eq!(driver_plugins(scope), vec!["driver".to_string()]);
    }
}
