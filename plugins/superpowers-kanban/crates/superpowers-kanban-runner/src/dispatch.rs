//! 插件侧查询的分派逻辑。
//!
//! 宿主只转发、不解释：`method` 与 `params` 原样送到这里，这里回一个
//! `serde_json::Value` 或一条错误消息。I/O 只发生在这个模块里，socket 层
//! 只负责编解码，因此全部分支都能用纯函数测试。

use std::path::Path;

use serde_json::{Value, json};
use superpowers_kanban_core::card_id::card_id_for;
use superpowers_kanban_core::inbox::deliver_card;
use superpowers_kanban_core::layout::project_preferences_path;
use superpowers_kanban_core::promotion::validate_promotion;
use superpowers_kanban_core::switch::{SwitchValue, read_layer, resolve, write_layer};

/// 处理一条查询。`global` 是全局层开关（由调用方读好传入，测试因此不必碰 `HOME`）。
pub fn dispatch_with_global(
    state_dir: &Path,
    global: Option<SwitchValue>,
    method: &str,
    params: &Value,
) -> Result<Value, String> {
    match method {
        "list" => Ok(json!({
            "cards": crate::persist::load_board(&state_dir.join("board.json"))
                .cards()
                .iter()
                .map(|card| json!({
                    "id": card.id.0,
                    "state": card.state,
                    "spec_path": card.spec_path,
                    "plan_path": card.plan_path,
                    "workdir": card.workdir,
                }))
                .collect::<Vec<_>>(),
        })),
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
}
