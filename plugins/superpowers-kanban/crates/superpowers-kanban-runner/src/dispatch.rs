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
use superpowers_kanban_core::inbox::deliver_card;
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
}
