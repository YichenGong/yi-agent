//! 插件侧查询的分派逻辑。
//!
//! 宿主只转发、不解释：`method` 与 `params` 原样送到这里，这里回一个
//! `serde_json::Value` 或一条错误消息。I/O 只发生在这个模块里，socket 层
//! 只负责编解码，因此全部分支都能用纯函数测试。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};
use superpowers_kanban_core::card_id::card_id_for;
use superpowers_kanban_core::inbox::{
    deliver_card, deliver_merge_card, deliver_prequeued_card, deliver_promote_card,
};
use superpowers_kanban_core::layout::project_preferences_path;
use superpowers_kanban_core::promotion::validate_promotion;
use superpowers_kanban_core::switch::{SwitchValue, read_layer, resolve, write_layer};

use crate::service::BoardService;

/// 进程内单例：按 `state_dir` 复用同一个 `Arc<BoardService>`。
///
/// 每个查询请求都新建 `BoardService` 会丢掉内存中的 lease——`flock` 随对象
/// drop 释放，于是 `next_launch` 连调两次都能拿到「同一个」名额，
/// `mark_running` 之后的槽位记账也随之漂移。所以查询分派与推进循环必须共享
/// 同一实例，`OnceLock<Mutex<HashMap<…>>>` 就是那个「同一实例」。
fn service_for(state_dir: &Path) -> Arc<BoardService> {
    static SERVICES: OnceLock<Mutex<HashMap<PathBuf, Arc<BoardService>>>> = OnceLock::new();
    let registry = SERVICES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut services = registry.lock().unwrap_or_else(|poison| poison.into_inner());
    services
        .entry(state_dir.to_path_buf())
        .or_insert_with(|| {
            // project_root 由 state_dir 推导；home 走真实的 `global_leases_dir`。
            Arc::new(BoardService::new(
                state_dir.to_path_buf(),
                superpowers_kanban_core::layout::project_root(state_dir),
                None,
            ))
        })
        .clone()
}

/// 卡片 id 的形状守卫：非空、且不含路径分隔符（`.` / `..` 也拒）。
///
/// id 正常是 slug，但 IPC 的 `promote` 会把调用方给的 id 直接拼进
/// `<inbox>/<id>.promote.json`；`"../../x"` 这类 id 会写到 inbox 之外，
/// 属于「输入即写入」。CLI 走的是「先查看板」，看板里没有这种 id，所以它在
/// 查板那步就挡住了；IPC 必须显式挡一次，才与 CLI 对同一输入一致。
fn valid_promote_id(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains('/') && !id.contains('\\')
}

/// 处理一条查询。`global` 是全局层开关（由调用方读好传入，测试因此不必碰 `HOME`）。
pub fn dispatch_with_global(
    state_dir: &Path,
    global: Option<SwitchValue>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    dispatch_with_service(&service_for(state_dir), state_dir, global, method, params)
}

/// 分派入口本体。`service` 必须是推进循环持有的那个实例——`list` 与 `board.*`
/// 都从它读写看板与槽位租约，换成别的实例会让内存里的 lease 对不上账。
pub fn dispatch_with_service(
    service: &BoardService,
    state_dir: &Path,
    global: Option<SwitchValue>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    match method {
        "list" => Ok(service.list()),
        "enqueue" => {
            let spec = params
                .get("spec_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue needs a `spec_path`".to_string())?;
            let plan = params
                .get("plan_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue needs a `plan_path`".to_string())?;
            // 校验先做：拒绝的投递必须一个字节都不落盘，否则 runner 会替调用方
            // 把一张注定失败的卡片排进队列。
            validate_promotion(Path::new(spec), Path::new(plan))
                .map_err(|error| error.to_string())?;
            let id = card_id_for(spec, plan);
            deliver_card(state_dir, &id, spec, plan)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
        "enqueue_prequeued" => {
            let title = params
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue_prequeued needs a `title`".to_string())?;
            if title.trim().is_empty() {
                return Err("enqueue_prequeued needs a non-empty `title`".to_string());
            }
            // 来源会话可选：桌面面板带上当前会话，其它调用方可以不带。
            let origin_thread_id = params
                .get("origin_thread_id")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let id = service.next_free_prequeued_id(title, chrono::Local::now());
            deliver_prequeued_card(state_dir, &id, title, origin_thread_id)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
        "promote" => {
            let card_id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs an `id`".to_string())?;
            let spec = params
                .get("spec_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs a `spec_path`".to_string())?;
            let plan = params
                .get("plan_path")
                .and_then(Value::as_str)
                .ok_or_else(|| "promote needs a `plan_path`".to_string())?;
            // 校验先做：拒绝的投递必须一个字节都不落盘。
            validate_promotion(Path::new(spec), Path::new(plan))
                .map_err(|error| error.to_string())?;
            // 再做 id 形状守卫与查板——两者都必须在任何投递落盘之前，否则调用方
            // 给的 `"../../x"` 会先在 inbox 外留下一个杂散文件（见 `valid_promote_id`）。
            if !valid_promote_id(card_id) {
                return Err("promote needs a valid card id".to_string());
            }
            let board = crate::persist::load_board(&state_dir.join("board.json"));
            let id = superpowers_kanban_core::card::CardId::new(card_id);
            match board.get(&id) {
                None => return Err(format!("unknown card: {card_id}")),
                Some(card) if card.state != superpowers_kanban_core::card::CardState::PreQueued => {
                    return Err(format!(
                        "card {card_id} is not pre-queued (state: {:?})",
                        card.state
                    ));
                }
                Some(_) => {}
            }
            let title = params.get("title").and_then(Value::as_str);
            deliver_promote_card(state_dir, card_id, spec, plan, title)
                .map_err(|error| format!("could not deliver the promote: {error}"))?;
            Ok(json!({ "id": card_id }))
        }
        "enqueue_merge" => {
            let source = params
                .get("source")
                .and_then(Value::as_str)
                .ok_or_else(|| "enqueue_merge needs a `source`".to_string())?;
            let project_root = superpowers_kanban_core::layout::project_root(state_dir);
            let base = match params.get("base").and_then(Value::as_str) {
                Some(base) => base.to_string(),
                None => crate::merge::default_branch(&project_root),
            };
            // 校验先做：被拒的投递必须一个字节都不落盘（与 `enqueue` 同理）。
            superpowers_kanban_core::promotion::validate_merge_refs(source, &base)
                .map_err(|error| error.to_string())?;
            if !crate::merge::source_branch_exists(&project_root, source) {
                return Err(format!("source branch does not exist: {source}"));
            }
            let id = service.next_free_merge_id(source, &base);
            deliver_merge_card(state_dir, &id, source, &base, None)
                .map_err(|error| format!("could not deliver the card: {error}"))?;
            Ok(json!({ "id": id }))
        }
        "switch.read" => {
            let project = read_layer(&project_preferences_path(state_dir));
            // 来源要如实报告：调用方据此知道"关着"是默认、还是有人显式关过。
            let scope = if project.is_some() {
                "project"
            } else if global.is_some() {
                "global"
            } else {
                "default"
            };
            let resolved = resolve(global, project);
            Ok(json!({ "on": resolved.is_enabled(), "source": scope }))
        }
        "switch.write" => {
            let on = params
                .get("on")
                .and_then(Value::as_bool)
                .ok_or_else(|| "switch.write needs an `on` boolean".to_string())?;
            let value = if on {
                SwitchValue::Enabled
            } else {
                SwitchValue::Disabled
            };
            let path = project_preferences_path(state_dir);
            write_layer(&path, value)
                .map_err(|error| format!("could not write {}: {error}", path.display()))?;
            Ok(json!({ "on": on }))
        }
        "driver/next" => {
            // 宿主每 tick 拉一拍：给它一份会话空闲快照与上一拍的结果，回至多一个
            // 通用动作（spec §4.2/§4.3）。参数形状不对时把话说清楚——静默当成空
            // 一拍会让宿主以为「插件没事可做」而永远推不动。
            let threads = params
                .get("threads")
                .and_then(Value::as_array)
                .ok_or_else(|| "driver/next needs a `threads` array".to_string())?;
            let mut snapshot: Vec<(String, bool)> = Vec::with_capacity(threads.len());
            for thread in threads {
                let thread_id = thread
                    .get("thread_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "driver/next threads need a `thread_id`".to_string())?;
                // 缺 `idle` 当作「不闲」：唯一安全的默认——把忙当闲会让插件对
                // 一台正在跑的会话下结论。
                let idle = thread.get("idle").and_then(Value::as_bool).unwrap_or(false);
                snapshot.push((thread_id.to_string(), idle));
            }
            let results: Vec<crate::service::DriverResult> = params
                .get("results")
                .map(|value| {
                    serde_json::from_value(value.clone())
                        .map_err(|error| format!("driver/next could not read `results`: {error}"))
                })
                .transpose()?
                .unwrap_or_default();

            match service.driver_step(&snapshot, &results)? {
                Some(action) => Ok(action.to_json()),
                // 一拍无事可做：契约形状是 `{"action": null}`。
                None => Ok(json!({ "action": null })),
            }
        }
        "board.next_launch" => {
            // 日历上限按当前时刻算；服务内不读时钟，交由这里注入。
            let calendar =
                superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
                    state_dir,
                );
            let limit = calendar.limit_at(chrono::Local::now());
            match service.next_launch(limit, chrono::Local::now()) {
                Ok(Some(claim)) => Ok(json!({
                    "card_id": claim.card_id,
                    "workdir": claim.workdir,
                    "title": claim.title,
                })),
                Ok(None) => Ok(Value::Null),
                Err(error) => Err(error),
            }
        }
        "board.mark_running" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "board.mark_running needs a card_id".to_string())?;
            let thread_id = params
                .get("thread_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "board.mark_running needs a thread_id".to_string())?;
            service.mark_running(card_id, thread_id)?;
            Ok(json!({"ok": true}))
        }
        "board.mark_terminal" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "board.mark_terminal needs a card_id".to_string())?;
            let outcome = params
                .get("outcome")
                .and_then(Value::as_str)
                .ok_or_else(|| "board.mark_terminal needs an outcome".to_string())?;
            let detail = params.get("detail").and_then(Value::as_str);
            service.mark_terminal(card_id, outcome, detail)?;
            Ok(json!({"ok": true}))
        }
        "board.release" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "board.release needs a card_id".to_string())?;
            let detail = params
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("launch failed");
            service.release(card_id, detail)?;
            Ok(json!({"ok": true}))
        }
        "merge_request" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "merge_request needs a card_id".to_string())?;
            match service.merge_request(card_id)? {
                crate::service::MergeRequest::Granted {
                    workdir,
                    source,
                    base,
                } => Ok(json!({
                    "status": "granted",
                    "workdir": workdir.to_string_lossy(),
                    "source": source,
                    "base": base,
                })),
                crate::service::MergeRequest::Busy => Ok(json!({ "status": "busy" })),
                crate::service::MergeRequest::Denied(reason) => {
                    Ok(json!({ "status": "denied", "reason": reason }))
                }
            }
        }
        "merge_finish" => {
            let card_id = params
                .get("card_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "merge_finish needs a card_id".to_string())?;
            let status = match service.merge_finish(card_id)? {
                crate::service::MergeFinish::Done => "done",
                crate::service::MergeFinish::NeedsYou => "needs_you",
                crate::service::MergeFinish::Cleared => "cleared",
            };
            Ok(json!({ "status": status }))
        }
        "lifecycle/create" => {
            let project = params
                .get("project")
                .and_then(Value::as_str)
                .filter(|project| !project.is_empty())
                .ok_or_else(|| "lifecycle/create needs a `project` path".to_string())?;
            service.lifecycle_create(Path::new(project))?;
            Ok(json!({ "ok": true }))
        }
        "lifecycle/remove" => {
            let project = params
                .get("project")
                .and_then(Value::as_str)
                .filter(|project| !project.is_empty())
                .ok_or_else(|| "lifecycle/remove needs a `project` path".to_string())?;
            service.lifecycle_remove(Path::new(project))?;
            Ok(json!({ "ok": true }))
        }
        "settings.read" => {
            let calendar =
                superpowers_kanban_core::calendar::ConcurrencyCalendar::load_preferring_new(
                    state_dir,
                );
            Ok(json!({ "settings": calendar.settings_json() }))
        }
        "settings.write" => {
            let settings = params
                .get("settings")
                .ok_or_else(|| "settings.write needs a `settings` object".to_string())?;
            // 先校验后落盘：非法输入必须一个字节都不写。
            let calendar =
                superpowers_kanban_core::calendar::ConcurrencyCalendar::from_settings_json(
                    settings,
                )
                .map_err(|error| error.to_string())?;
            calendar
                .save(state_dir)
                .map_err(|error| format!("could not save settings: {error}"))?;
            Ok(json!({ "ok": true }))
        }
        other => Err(format!("unknown method: {other}")),
    }
}

/// 生产入口：自己读全局层，再交给 `dispatch_with_global`。
pub fn dispatch(state_dir: &Path, method: &str, params: &Value) -> Result<Value, String> {
    let global = superpowers_kanban_core::layout::global_preferences_path()
        .as_deref()
        .and_then(superpowers_kanban_core::switch::read_layer);
    dispatch_with_global(state_dir, global, method, params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;
    use superpowers_kanban_core::switch::{SwitchValue, read_layer};

    /// 一个已经存在的一对 spec + plan，用来喂 `enqueue`。
    fn write_pair(dir: &Path) -> (String, String) {
        let spec = dir.join("2026-10-01-feature.spec.md");
        let plan = dir.join("2026-10-01-feature.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        (
            spec.to_string_lossy().into_owned(),
            plan.to_string_lossy().into_owned(),
        )
    }

    #[test]
    fn list_returns_the_cards_on_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());
        // 直接落一份 board.json：list 的职责是读出队列，而不是复述 inbox。
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            spec.clone().into(),
            plan.clone().into(),
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();

        let result = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        let cards = result["cards"]
            .as_array()
            .expect("list returns a `cards` array");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["id"], "card-1");
        assert_eq!(cards[0]["state"], "queued");
        assert_eq!(cards[0]["spec_path"], spec);
        assert_eq!(cards[0]["plan_path"], plan);
    }

    #[test]
    fn enqueue_rejects_a_missing_spec_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&plan, "# plan").unwrap();

        let error = dispatch_with_global(
            dir.path(),
            None,
            "enqueue",
            &json!({"spec_path": dir.path().join("missing.spec.md"), "plan_path": plan}),
        )
        .unwrap_err();
        assert!(
            error.contains("missing.spec.md"),
            "the error must name the file: {error}"
        );
        assert!(
            !superpowers_kanban_core::inbox::inbox_dir(dir.path()).exists(),
            "a rejected enqueue must not leave a delivery behind"
        );
    }

    #[test]
    fn enqueue_delivers_a_valid_pair_into_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());

        let result = dispatch_with_global(
            dir.path(),
            None,
            "enqueue",
            &json!({"spec_path": spec, "plan_path": plan}),
        )
        .unwrap();
        let id = result
            .get("id")
            .and_then(Value::as_str)
            .expect("enqueue must return an id");
        let delivered = superpowers_kanban_core::inbox::enqueue_path(dir.path(), id);
        assert!(delivered.is_file(), "expected a delivery at {delivered:?}");
    }

    /// 项目层偏好落在状态目录**旁边**，所以测试必须用真实的 `<workdir>/.yi-agent/
    /// superpowers-kanban` 布局：把临时根当 state_dir 会把文件写到临时根之外。
    fn state_dir(workdir: &Path) -> PathBuf {
        workdir.join(".yi-agent").join("superpowers-kanban")
    }

    #[test]
    fn switch_read_reports_the_value_and_which_layer_set_it() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        // 两层都没设 -> 默认关闭，来源是 default。
        let result = dispatch_with_global(&state, None, "switch.read", &json!({})).unwrap();
        assert_eq!(result["on"], false);
        assert_eq!(result["source"], "default");

        // 只有全局层 -> 来源是 global。
        let result = dispatch_with_global(
            &state,
            Some(SwitchValue::Enabled),
            "switch.read",
            &json!({}),
        )
        .unwrap();
        assert_eq!(result["on"], true);
        assert_eq!(result["source"], "global");

        // 项目层压过全局层 -> 来源是 project。
        write_layer(
            &superpowers_kanban_core::layout::project_preferences_path(&state),
            SwitchValue::Disabled,
        )
        .unwrap();
        let result = dispatch_with_global(
            &state,
            Some(SwitchValue::Enabled),
            "switch.read",
            &json!({}),
        )
        .unwrap();
        assert_eq!(result["on"], false);
        assert_eq!(result["source"], "project");
    }

    #[test]
    fn switch_write_lands_in_the_project_layer() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let result =
            dispatch_with_global(&state, None, "switch.write", &json!({"on": true})).unwrap();
        assert_eq!(result["on"], true);
        let path = superpowers_kanban_core::layout::project_preferences_path(&state);
        assert_eq!(read_layer(&path), Some(SwitchValue::Enabled));
        assert!(
            path.starts_with(workdir.path()),
            "the project layer must stay inside the workdir: {path:?}"
        );
    }

    #[test]
    fn settings_read_returns_the_calendar_settings() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let value = dispatch_with_global(&state, None, "settings.read", &json!({})).unwrap();
        assert_eq!(value["settings"]["default_max_tasks"], 3);
        assert_eq!(value["settings"]["interval_secs"], 10);
        assert!(value["settings"]["windows"].as_array().unwrap().is_empty());
    }

    #[test]
    fn settings_write_then_read_round_trips() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let payload = json!({
            "default_max_tasks": 5,
            "interval_secs": 25,
            "windows": [
                { "days": "Mon-Fri", "start": "09:00", "end": "24:00", "max_tasks": 3 }
            ]
        });
        dispatch_with_global(
            &state,
            None,
            "settings.write",
            &json!({ "settings": payload }),
        )
        .unwrap();
        // 文件确实落在新名下。
        assert!(state.join("superpowers-kanban.toml").is_file());
        let value = dispatch_with_global(&state, None, "settings.read", &json!({})).unwrap();
        assert_eq!(value["settings"]["default_max_tasks"], 5);
        assert_eq!(value["settings"]["interval_secs"], 25);
        assert_eq!(value["settings"]["windows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn an_invalid_settings_write_leaves_no_file() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let bad = json!({
            "default_max_tasks": 3,
            "interval_secs": 0,
            "windows": []
        });
        let error =
            dispatch_with_global(&state, None, "settings.write", &json!({ "settings": bad }))
                .unwrap_err();
        assert!(error.contains("interval_secs"), "{error}");
        assert!(
            !state.join("superpowers-kanban.toml").exists(),
            "a rejected write must not leave the settings file behind"
        );
    }

    #[test]
    fn an_unknown_method_is_an_error_not_a_silent_null() {
        let dir = tempfile::tempdir().unwrap();
        let error =
            dispatch_with_global(dir.path(), None, "destroy.everything", &json!({})).unwrap_err();
        assert!(error.contains("destroy.everything"), "{error}");
    }

    #[test]
    fn next_launch_is_gated_by_the_switch_and_reports_no_claim_when_disabled() {
        let dir = tempfile::tempdir().unwrap();
        // 未建仓库、未开开关：next_launch 不应抛错，而是回 null（无名额/无卡）。
        let value = dispatch(dir.path(), "board.next_launch", &serde_json::json!({})).unwrap();
        assert!(value.is_null());
    }

    #[test]
    fn mark_terminal_rejects_an_unknown_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let err = dispatch(
            dir.path(),
            "board.mark_terminal",
            &serde_json::json!({"card_id":"card-1","outcome":"nonsense"}),
        )
        .unwrap_err();
        assert!(err.contains("unknown outcome"), "{err}");
    }

    /// 单例是硬约束：`BoardService` 的内存 lease 随对象 drop 释放，若每个查询都新建
    /// 一个实例，`board.next_launch` 连调就能重复拿到同一名额。这里直接断言
    /// 同一 `state_dir` 复用同一实例、不同 `state_dir` 互不串用。
    #[test]
    fn one_state_directory_gets_one_shared_service_instance() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (
            dir.path().join(".yi-agent/superpowers-kanban"),
            dir.path().join("other/.yi-agent/superpowers-kanban"),
        );
        assert!(
            Arc::ptr_eq(&service_for(&a), &service_for(&a)),
            "a repeated lookup must reuse the same BoardService"
        );
        assert!(
            !Arc::ptr_eq(&service_for(&a), &service_for(&b)),
            "distinct state dirs must not share leases"
        );
    }

    #[test]
    fn enqueue_merge_refuses_a_missing_source_branch_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["init", "-q", "-b", "main"])
                .status()
                .unwrap()
                .success()
        );
        let state = dir.path().join(".yi-agent/superpowers-kanban");
        let error = dispatch_with_global(
            &state,
            None,
            "enqueue_merge",
            &json!({"source": "feat/x", "base": "main"}),
        )
        .unwrap_err();
        assert!(error.contains("feat/x"), "{error}");
        assert!(!superpowers_kanban_core::inbox::inbox_dir(&state).exists());
    }

    #[test]
    fn list_reports_the_merge_payload_and_nulls_for_implementation_cards() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("impl"),
            spec.into(),
            plan.into(),
            chrono::Local::now(),
        );
        board.enqueue_merge(
            superpowers_kanban_core::card::CardId::new("m1"),
            "kanban/a".into(),
            "main".into(),
            Some(superpowers_kanban_core::card::CardId::new("impl")),
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();
        let value = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        let cards = value["cards"].as_array().unwrap();
        let merge = cards.iter().find(|c| c["id"] == "m1").unwrap();
        assert_eq!(merge["kind"], "merge");
        assert_eq!(merge["source"], "kanban/a");
        assert_eq!(merge["origin_card"], "impl");
        let impl_card = cards.iter().find(|c| c["id"] == "impl").unwrap();
        assert_eq!(impl_card["kind"], "implementation");
        assert!(impl_card["source"].is_null());
    }

    #[test]
    fn merge_request_reports_denied_for_an_unknown_card() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_dir(dir.path());
        let value =
            dispatch_with_global(&state, None, "merge_request", &json!({ "card_id": "nope" }))
                .unwrap();
        assert_eq!(value["status"], "denied");
        assert!(value["reason"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn merge_finish_errors_on_an_unknown_card_and_reports_cleared_otherwise() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        // 先落盘一张排队卡：BoardService 是进程单例，构造时读一次 board.json，
        // 之后再写文件不会被它看见（生产路径经 dispatch 改内存板）。
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            chrono::Local::now(),
        );
        crate::persist::save_board(&state.join("board.json"), &board).unwrap();

        // 卡不在 merging → cleared。
        let value = dispatch_with_global(
            &state,
            None,
            "merge_finish",
            &json!({ "card_id": "card-1" }),
        )
        .unwrap();
        assert_eq!(value["status"], "cleared");

        // 卡根本不存在 → 明确报错，而不是静默 cleared。
        let error =
            dispatch_with_global(&state, None, "merge_finish", &json!({ "card_id": "nope" }))
                .unwrap_err();
        assert!(error.contains("unknown card"), "{error}");
    }

    /// 真实的最小 git 仓库（与 `service.rs` 的测试搭台同型）：driver/next 要
    /// 真建 worktree，喂一个假目录不行。
    fn project_with_worktree(dir: &Path) -> PathBuf {
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?}"
            );
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("README.md"), "seed\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
        git(&["add", "README.md", ".gitignore"]);
        git(&[
            "-c",
            "user.email=e@e",
            "-c",
            "user.name=E",
            "commit",
            "-q",
            "-m",
            "seed",
        ]);
        dir.to_path_buf()
    }

    /// 隔离的 `BoardService`（lease 落在临时 home 里）：单例服务共用真实 `HOME`
    /// 的全局名额池，并行跑用例时会互相抢名额、把「有名额」的断言变 flaky。
    fn service_with_one_card(project: &Path) -> crate::service::BoardService {
        let state = state_dir(project);
        std::fs::create_dir_all(&state).unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue(
            superpowers_kanban_core::card::CardId::new("card-1"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            chrono::Local::now(),
        );
        crate::persist::save_board(&state.join("board.json"), &board).unwrap();
        crate::service::BoardService::new(state, project.to_path_buf(), Some(project.join("home")))
    }

    /// 一拍无事可做（空板）时，契约形状是 `{"action": null}`。
    #[test]
    fn driver_next_reports_no_action_as_a_null_action() {
        let workdir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(workdir.path());
        let state = state_dir(&project);
        std::fs::create_dir_all(&state).unwrap();
        let service = crate::service::BoardService::new(
            state.clone(),
            project.clone(),
            Some(project.join("home")),
        );
        let value = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({ "threads": [], "results": [] }),
        )
        .unwrap();
        assert_eq!(value["action"], Value::Null, "{value}");
    }

    /// 端到端跨进程契约：一张排队卡 → 恰好一个 `launch`，字段名逐字对齐宿主
    /// `session_driver::parse_action`（spec §4.3）。
    #[test]
    fn driver_next_hands_a_launch_action_in_the_agreed_shape() {
        let workdir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(workdir.path());
        let state = state_dir(&project);
        let service = service_with_one_card(&project);

        let value = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({ "threads": [], "results": [] }),
        )
        .unwrap();
        assert_eq!(value["kind"], "launch", "{value}");
        assert!(value["action_id"].as_str().is_some_and(|id| !id.is_empty()));
        assert!(
            Path::new(value["workdir"].as_str().unwrap())
                .join(".git")
                .exists(),
            "workdir 是插件建好的 worktree：{value}"
        );
        assert_eq!(value["title"], "看板 · a.spec");
        let objective = value["objective"].as_str().unwrap();
        assert!(objective.contains("a.plan.md"), "{objective}");
        assert!(objective.contains("Never merge yourself"), "{objective}");
        // 宿主不认识看板：动作里没有卡片语义（spec D3）。
        assert!(value.get("card_id").is_none(), "{value}");

        // 一拍只走一个动作：再问一遍，卡已在 launching，不再交出动作。
        let again = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({ "threads": [], "results": [] }),
        )
        .unwrap();
        assert_eq!(again["action"], Value::Null, "{again}");
    }

    /// 回报的形状就是宿主 `ActionResult` 的序列化：按 `action_id` 认领，
    /// 带回 `thread_id` → 卡 `running`（D5：映射由插件持有）。
    #[test]
    fn driver_next_applies_a_launch_result_by_action_id() {
        let workdir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(workdir.path());
        let state = state_dir(&project);
        let service = service_with_one_card(&project);

        let launch = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({ "threads": [], "results": [] }),
        )
        .unwrap();
        let action_id = launch["action_id"].as_str().unwrap().to_string();

        // 下一拍：会话快照里多了一台忙着的会话，并回传上一拍的结果。
        let value = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({
                "threads": [{ "thread_id": "thread-1", "idle": false }],
                "results": [{ "action_id": action_id, "ok": true, "thread_id": "thread-1" }],
            }),
        )
        .unwrap();
        assert_eq!(value["action"], Value::Null, "{value}");
        let listed = service.list();
        assert_eq!(listed["cards"][0]["state"], "running");
        assert_eq!(listed["cards"][0]["thread_id"], "thread-1");

        // 重复投递同一条回报（宿主可能重发）：不报错，也不改坏状态。
        let again = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({
                "threads": [{ "thread_id": "thread-1", "idle": false }],
                "results": [{ "action_id": action_id, "ok": true, "thread_id": "thread-1" }],
            }),
        )
        .unwrap();
        assert_eq!(again["action"], Value::Null, "{again}");
        assert_eq!(service.list()["cards"][0]["state"], "running");
    }

    /// 线程对象缺 `idle` 时**按「不闲」处理**：唯一安全的默认——把忙当闲会让
    /// 插件对一台正在跑的会话下结论（例如把 `running` 卡错判成跑完了）。
    /// 这里用一张不可能被正确判定的卡来钉住这半边：卡是 `merging`，若缺省被当成
    /// 空闲，`driver_step` 会走 git 复核把卡收尾；按「不闲」则原样不动。
    #[test]
    fn driver_next_treats_a_thread_without_idle_as_busy() {
        let workdir = tempfile::tempdir().unwrap();
        let project = project_with_worktree(workdir.path());
        let service = service_with_one_card(&project);
        let state = state_dir(&project);

        // 让 card-1 走到 `merging`（合并轮进行中）：先把它的 source 分支建出来，
        // 否则 `merge_request` 会因分支缺失而拒绝。
        let slug = crate::worktree::slugify(&superpowers_kanban_core::card::CardId::new("card-1"));
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["branch", &format!("kanban/{slug}")])
                .status()
                .unwrap()
                .success()
        );
        service
            .mark_running("card-1", "thread-1")
            .expect("card-1 runs");
        service
            .mark_terminal("card-1", "awaiting_merge", None)
            .unwrap();
        assert!(matches!(
            service.merge_request("card-1").unwrap(),
            crate::service::MergeRequest::Granted { .. }
        ));
        assert_eq!(service.list()["cards"][0]["state"], "merging");

        // 会话对象**没有** `idle` 字段：只能按「不闲」理解，卡原样留在 merging。
        let value = dispatch_with_service(
            &service,
            &state,
            None,
            "driver/next",
            &json!({ "threads": [{ "thread_id": "thread-1" }], "results": [] }),
        )
        .unwrap();
        assert_eq!(value["action"], Value::Null, "{value}");
        assert_eq!(
            service.list()["cards"][0]["state"],
            "merging",
            "缺 `idle` 不得被当成空闲（否则会误判一个还在跑的会话）"
        );
    }

    /// 缺 `threads` 是调用方的错，必须明确报错而不是静默回一个空拍：
    /// 静默会让宿主以为「插件没事可做」而永远推不动。
    #[test]
    fn driver_next_rejects_a_request_without_threads() {
        let workdir = tempfile::tempdir().unwrap();
        let state = state_dir(workdir.path());
        let error = dispatch_with_global(&state, None, "driver/next", &json!({ "results": [] }))
            .unwrap_err();
        assert!(error.contains("threads"), "{error}");
    }
    #[test]
    fn list_reports_a_prequeued_card_with_its_title() {
        // 本任务产出的另一半契约：`list()` 的每张卡多一个 `title` 字段，
        // 且 `PreQueued` 状态如实透出（宿主据它画「预排队」列）。
        let dir = tempfile::tempdir().unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue_prequeued(
            superpowers_kanban_core::card::CardId::new("idea"),
            "我的想法".into(),
            None,
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();

        let value = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        let card = &value["cards"][0];
        assert_eq!(card["id"], "idea");
        assert_eq!(card["state"], "pre_queued");
        assert_eq!(card["title"], "我的想法");
    }

    #[test]
    fn enqueue_prequeued_delivers_a_titled_card() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["kind"], "prequeued");
        assert_eq!(delivery["title"], "my idea");
    }

    #[test]
    fn enqueue_prequeued_gives_non_ascii_titles_distinct_ids() {
        // 中文标题 slug 成空串：必须回退到时间戳 id，且两张卡不同 id。
        let dir = tempfile::tempdir().unwrap();
        let first = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "一个想法" }),
        )
        .unwrap();
        assert!(
            first["id"].as_str().unwrap().starts_with("prequeued-"),
            "{first}"
        );
        let delivery: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                dir.path()
                    .join("inbox")
                    .join(format!("{}.json", first["id"].as_str().unwrap())),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            delivery["title"], "一个想法",
            "id 是 slug 回退，标题仍是原文"
        );
    }

    #[test]
    fn two_quick_enqueues_in_one_window_get_distinct_ids() {
        let dir = tempfile::tempdir().unwrap();
        let first = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "一个想法" }),
        )
        .unwrap();
        let second = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "另一个想法" }),
        )
        .unwrap();
        assert_ne!(first["id"], second["id"], "{first} vs {second}");
        let inbox = dir.path().join("inbox");
        let count = std::fs::read_dir(&inbox)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .count();
        assert_eq!(count, 2, "两次捕获必须都留下投递文件");
    }

    #[test]
    fn enqueue_prequeued_rejects_an_empty_title() {
        let dir = tempfile::tempdir().unwrap();
        let error = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "   " }),
        )
        .unwrap_err();
        assert!(error.contains("title"), "{error}");
        assert!(!dir.path().join("inbox").exists(), "拒绝的投递不落盘");
    }

    #[test]
    fn promote_delivers_for_a_prequeued_card_without_touching_the_board() {
        let dir = tempfile::tempdir().unwrap();
        let spec = dir.path().join("a.spec.md");
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&spec, "# spec").unwrap();
        std::fs::write(&plan, "# plan").unwrap();
        // promote 现在先查看板：卡必须在板上且是预排队，投递才会落盘。
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue_prequeued(
            superpowers_kanban_core::card::CardId::new("idea"),
            "旧标题".to_string(),
            None,
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "idea",
                "spec_path": spec.to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
                "title": "新标题",
            }),
        )
        .unwrap();
        assert_eq!(value["id"], "idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/idea.promote.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["kind"], "promote");
        assert_eq!(delivery["title"], "新标题");
        // 投递不改看板：翻转由 runner 消费时做。
        let board = crate::persist::load_board(&dir.path().join("board.json"));
        assert_eq!(
            board
                .get(&superpowers_kanban_core::card::CardId::new("idea"))
                .unwrap()
                .state,
            superpowers_kanban_core::card::CardState::PreQueued,
        );
    }

    #[test]
    fn promote_rejects_a_missing_pair_before_any_disk_write() {
        let dir = tempfile::tempdir().unwrap();
        let plan = dir.path().join("a.plan.md");
        std::fs::write(&plan, "# plan").unwrap();
        let error = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "idea",
                "spec_path": dir.path().join("missing.spec.md").to_str().unwrap(),
                "plan_path": plan.to_str().unwrap(),
            }),
        )
        .unwrap_err();
        assert!(error.contains("spec file does not exist"), "{error}");
        assert!(!dir.path().join("inbox").exists(), "拒绝的投递不落盘");
    }

    #[test]
    fn promote_rejects_an_unknown_card_id_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());
        // 板上有一张别的预排队卡：id 不在板上（而不是「板不存在」）也要拒。
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue_prequeued(
            superpowers_kanban_core::card::CardId::new("known"),
            "标题".to_string(),
            None,
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();

        let error = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "ghost",
                "spec_path": spec,
                "plan_path": plan,
            }),
        )
        .unwrap_err();
        assert!(error.contains("unknown card: ghost"), "{error}");
        assert!(
            !dir.path().join("inbox").exists(),
            "板外 id 的投递必须一个字节都不落盘"
        );
    }

    #[test]
    fn promote_rejects_an_id_with_a_separator_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, plan) = write_pair(dir.path());
        // `../../x` 会被拼进 `<inbox>/<id>.promote.json`，写到 inbox 之外。
        let error = dispatch_with_global(
            dir.path(),
            None,
            "promote",
            &json!({
                "id": "../../x",
                "spec_path": spec,
                "plan_path": plan,
            }),
        )
        .unwrap_err();
        assert!(error.contains("valid card id"), "{error}");
        assert!(
            !dir.path().join("inbox").exists(),
            "非法 id 的投递必须一个字节都不落盘"
        );
        assert!(
            !dir.path().join("x.promote.json").exists()
                && !dir.path().parent().unwrap().join("x.promote.json").exists(),
            "inbox 之外不得出现杂散投递"
        );
    }

    #[test]
    fn enqueue_prequeued_passes_an_optional_origin_thread() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea", "origin_thread_id": "thread-abc" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
        let text = std::fs::read_to_string(dir.path().join("inbox/my-idea.json")).unwrap();
        let delivery: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(delivery["origin_thread_id"], "thread-abc");
    }

    #[test]
    fn enqueue_prequeued_without_an_origin_thread_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let value = dispatch_with_global(
            dir.path(),
            None,
            "enqueue_prequeued",
            &json!({ "title": "my idea" }),
        )
        .unwrap();
        assert_eq!(value["id"], "my-idea");
    }

    #[test]
    fn list_reports_the_origin_thread_of_a_prequeued_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut board = superpowers_kanban_core::board::Board::new();
        board.enqueue_prequeued(
            superpowers_kanban_core::card::CardId::new("idea"),
            "我的想法".into(),
            Some("thread-abc".into()),
            chrono::Local::now(),
        );
        crate::persist::save_board(&dir.path().join("board.json"), &board).unwrap();
        let value = dispatch_with_global(dir.path(), None, "list", &json!({})).unwrap();
        assert_eq!(value["cards"][0]["origin_thread_id"], "thread-abc");
    }
}
