//! 通用会话驱动：把「该起/该继续哪个会话」交给插件决定。
//!
//! 本模块不认识看板、卡片、合并。它只认识两件事：宿主能提供的通用事实
//! （某个会话闲不闲），与宿主能执行的两个通用动作（新建会话 / 追加一轮）。
//!
//! 主循环里的 `driver_tick` 是消费方：它按通用常驻登记表遍历项目、用插件清单
//! 发现「谁要驱动会话」（`driver_plugins`），再对每个 (项目, 插件) 组合拉一拍
//! （`driver_step`），由 `SessionLauncher` 的真实现执行动作。

use serde_json::Value;
use std::path::Path;

/// 宿主能执行的通用动作。
#[derive(Debug, Clone, PartialEq, Eq)]
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
pub(crate) struct DriverAction {
    pub action_id: String,
    pub kind: DriverActionKind,
}

/// 解析插件的 `driver/next` 应答。
///
/// `{"action": null}`、缺 `action_id`、或未知 `kind` 一律返回 `None`：调度器
/// 前向兼容——插件日后新增动作类型时，旧宿主只是「这一拍什么都不做」。
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

/// 插件有没有**明确**答「这一拍无事可做」（契约形状 `{"action": null}`）。
///
/// 它必须与「解析不出动作」分得开：`parse_action` 对两者都返回 `None`，可它们的
/// 日志价值完全相反——前者是正常空闲（每 3s 都会发生，记下来只会淹没日志），
/// 后者是真的断链（字段漂移、插件比宿主新、应答不是约定形状）。没有这个判据，
/// 宿主只能「`None` 就当没事」，那正是静默停摆的现场。
///
/// 只认契约形状（见 `contract/driver-protocol.json` 的 `answers.nothing`）：
/// 裸 `null`、`{}`、或 `action` 存在但非 null 都不是约定形状，一律算「说不清」，
/// 由调用方记一行。
pub(crate) fn is_explicit_no_action(value: &Value) -> bool {
    matches!(value.get("action"), Some(Value::Null))
}

/// 宿主为本插件启动或追加过的会话（宿主自身的通用记账）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DriverThread {
    pub thread_id: String,
}

/// 一次动作的结果，下一拍回传插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActionResult {
    pub action_id: String,
    pub ok: bool,
    pub thread_id: Option<String>,
    pub error: Option<String>,
}

/// 一个 (项目, 插件) 组合的驱动记账，随主循环存活、跨 tick 累积。
///
/// 之所以要持久：`results` 是「上一拍做完、本拍要回报插件」的结果（`driver_step`
/// 送出去之后才清空），`threads` 是宿主为该插件起过的会话——两者都必须比一次
/// tick 活得久，否则插件永远收不到 `thread_id`、也无从判断某个会话是否空闲。
///
/// 它不是共享状态：只被主循环的 tick 分支按顺序 `&mut` 借用（与 `threads` 同属
/// 主循环局部量），故不需要加锁，也没有任何任务并发访问它。
#[derive(Default)]
pub(crate) struct DriverState {
    pub threads: Vec<DriverThread>,
    pub results: Vec<ActionResult>,
}

/// 执行动作的副作用。真实现复用 `start_thread_core` / `prepare_turn_core` 的
/// 无响应帧路径；测试用替身驱动纯逻辑。
pub(crate) trait SessionLauncher {
    async fn launch(&mut self, action: &DriverAction) -> anyhow::Result<String>;
}

/// 把宿主持有的会话事实编成 `driver/next` 的 `threads` 参数。
pub(crate) fn threads_payload(
    threads: &[DriverThread],
    idle: &(dyn Fn(&str) -> bool + Sync),
) -> Value {
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
/// `idle` 是宿主对「某个会话此刻闲不闲」的回答（主循环按 tick 采一次快照，
/// 见 `driver_tick`）：插件靠它判断某一轮跑完了没有。它是宿主唯一能提供的
/// 通用事实，故由调用方传入而非在本函数里读取任何 thread 表。
///
/// `results` 里是上一拍尚未回报的结果；本拍把它们上送，**送达之后**才清空
/// （`query` 失败时保留，下一拍重发）。
pub(crate) async fn driver_step<L: SessionLauncher>(
    query: &(dyn Fn(&Value) -> Result<Value, String> + Sync),
    threads: &mut Vec<DriverThread>,
    results: &mut Vec<ActionResult>,
    launcher: &mut L,
    idle: &(dyn Fn(&str) -> bool + Sync),
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
        "threads": threads_payload(threads, idle),
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

    /// 「插件明确答了无事可做」必须与「宿主解析不出动作」分得开：前者是正常空闲，
    /// 后者要留一行日志（否则每 3s 一次的断链在日志里与空闲同型）。
    #[test]
    fn only_the_contract_null_answer_counts_as_deliberately_idle() {
        assert!(is_explicit_no_action(&json!({ "action": null })));
        // 解析不出动作的几种「说不清」形状都不是约定的空闲形状。
        for value in [
            json!(null),
            json!({}),
            json!({ "kind": "reboot", "action_id": "a1" }),
            json!({ "action": { "kind": "reboot" } }),
        ] {
            assert!(
                !is_explicit_no_action(&value),
                "{value} 不是约定的 `{{\"action\": null}}`，应当留痕"
            );
        }
    }

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

    /// 测试用的空闲快照：默认「谁都不闲」。
    ///
    /// 与接线前 `driver_step` 内部写死的 `&|_| false` 同义，故这些既有测试
    /// 断言的仍是同一套行为；真的空闲来源由主循环的 `driver_tick` 采集。
    fn nobody_idle() -> impl Fn(&str) -> bool {
        |_| false
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

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
        )
        .await;

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

        let outcome = driver_step(
            &answering(reply),
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
        )
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
            &nobody_idle(),
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
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(reply.clone())
        };
        let mut launcher = FakeLauncher::default();

        driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
        )
        .await;

        let sent = seen.lock().unwrap().clone();
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

        let outcome = driver_step(
            &query,
            &mut threads,
            &mut results,
            &mut launcher,
            &nobody_idle(),
        )
        .await;

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
            &nobody_idle(),
        )
        .await;

        assert!(outcome.is_none());
        assert!(
            results.is_empty(),
            "the results were delivered, so a null answer must not re-send them"
        );
    }

    /// 空闲快照必须真的进到 `threads[]` 里：插件唯一的「这轮跑完了」信号就是它
    /// （spec §4.6）。接线时若把参数丢掉、退回写死的 `false`，插件就永远推进不了
    /// 卡片状态，故这里钉住快照与 thread_id 的对应关系。
    #[tokio::test]
    async fn the_idle_snapshot_reaches_the_wire_per_thread() {
        let mut threads = vec![
            DriverThread {
                thread_id: "busy".into(),
            },
            DriverThread {
                thread_id: "done".into(),
            },
        ];
        let mut results = Vec::new();
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        let idle = |thread_id: &str| thread_id == "done";

        driver_step(&query, &mut threads, &mut results, &mut launcher, &idle).await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["threads"][0]["idle"], false,
            "a busy thread must be reported busy: {sent:?}"
        );
        assert_eq!(sent[0]["threads"][0]["thread_id"], "busy");
        assert_eq!(
            sent[0]["threads"][1]["idle"], true,
            "the snapshot must answer per thread, not globally: {sent:?}"
        );
        assert_eq!(sent[0]["threads"][1]["thread_id"], "done");
    }

    /// 宿主与插件共用的 `driver/next` 线协议契约（**与插件 `service.rs` 的测试
    /// 读的是同一份文件**，插件侧用 `include_str!`）。
    ///
    /// 契约此前只存在于两侧手写的 JSON 字面量里：改一侧的字段名，两边测试照样全绿，
    /// 而生产上宿主 `parse_action` 悄悄回 `None`（这一拍什么都不做）、插件的
    /// `DriverResult` 反序列化失败（卡永远停在 `launching`）。这里把两侧钉到同一份
    /// 检出文件上。
    ///
    /// 运行时读取而非 `include_str!`：插件是可独立安装/卸载的，宿主单独 checkout
    /// 时这个目录可能不存在，编译期依赖会让宿主构建直接失败（与
    /// `yi-agent-supervisors` 读 socket 契约同一取舍）。文件缺失就跳过——本测试锁的
    /// 是「monorepo 内两侧一致」，不是宿主的构建前提。
    fn driver_contract() -> Option<Value> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../../../plugins/superpowers-kanban/crates/superpowers-kanban-runner/contract/driver-protocol.json",
        );
        let text = std::fs::read_to_string(path).ok()?;
        Some(serde_json::from_str(&text).expect("契约必须是合法 JSON"))
    }

    /// 契约里插件那三种应答的字面量，宿主都必须解析得出来（`launch` / `append`
    /// 解出字段，`{"action": null}` 解出「无事可做」）。宿主若改了 `parse_action`
    /// 认的键名，这条就红。
    #[test]
    fn the_contract_answers_parse_into_the_actions_the_host_expects() {
        let Some(contract) = driver_contract() else {
            return; // 宿主单独 checkout：没有插件目录，无从对照。
        };
        let answers = &contract["answers"];

        let launch = parse_action(&answers["launch"]).expect("launch 字面量必须解析得出");
        assert_eq!(launch.action_id, "act-2");
        match launch.kind {
            DriverActionKind::Launch {
                workdir,
                title,
                objective,
            } => {
                assert_eq!(workdir, "/w/card-1");
                assert_eq!(title, "看板 · a.spec");
                assert_eq!(objective, "do the card");
            }
            other => panic!("expected launch, got {other:?}"),
        }

        let append = parse_action(&answers["append"]).expect("append 字面量必须解析得出");
        assert_eq!(append.action_id, "act-3");
        match append.kind {
            DriverActionKind::Append {
                thread_id,
                objective,
            } => {
                assert_eq!(thread_id, "thread-1");
                assert_eq!(objective, "merge it");
            }
            other => panic!("expected append, got {other:?}"),
        }

        assert!(
            parse_action(&answers["nothing"]).is_none(),
            "`{{\"action\": null}}` 是「这一拍无事可做」的契约形状"
        );
    }

    /// 宿主**实际发出**的请求必须与契约里的 `request` 逐字一致：插件对
    /// `threads[].thread_id` / `idle` 是手工解析的（`dispatch.rs`），`results[]`
    /// 又由宿主手工编 JSON——两侧改名都不会有编译错误，只有这条对照会发现。
    #[tokio::test]
    async fn the_contract_request_matches_what_the_host_actually_sends() {
        let Some(contract) = driver_contract() else {
            return;
        };
        let mut threads = vec![DriverThread {
            thread_id: "thread-1".into(),
        }];
        let mut results = vec![ActionResult {
            action_id: "act-1".into(),
            ok: true,
            thread_id: Some("thread-1".into()),
            error: None,
        }];
        let seen = std::sync::Mutex::new(Vec::new());
        let query = |params: &Value| {
            seen.lock().unwrap().push(params.clone());
            Ok(json!({ "action": null }))
        };
        let mut launcher = FakeLauncher::default();
        let idle = |thread_id: &str| thread_id == "thread-1";

        driver_step(&query, &mut threads, &mut results, &mut launcher, &idle).await;

        let sent = seen.lock().unwrap().clone();
        assert_eq!(
            sent[0]["threads"], contract["request"]["threads"],
            "宿主发出的 threads[] 与契约必须逐字一致"
        );
        assert_eq!(
            sent[0]["results"], contract["request"]["results"],
            "宿主回报的 results[] 与契约必须逐字一致"
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
