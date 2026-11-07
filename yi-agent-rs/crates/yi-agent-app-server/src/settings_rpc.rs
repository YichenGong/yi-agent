//! `settings/read` 与 `settings/write`：`settings.json` 的 RPC 外壳。
//!
//! 逻辑全在 `yi_agent_runtime::settings`。本模块只做两件事：把类型化的
//! `Settings` 摊成前端能渲染的 JSON 数组，以及把 `SettingsError` 翻成 RPC 错误。
//! **校验规则只在这里调用一次**（`write_to` 内部），不另写一份——多一份就是
//! 多一处迟早漂移的规则。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_runtime::settings::{self, FieldSpec, FieldType, SettingsError};

use crate::protocol::{RpcError, Scope};

/// `settings/read` 的响应体。
///
/// `value` 为 `null` 表示**未设置**——前端必须据此显示「默认 X」并禁用「恢复默认」
/// 按钮。不要把 `null` 归一成 `default`：那正是旧 `get_config` 的做法，它让
/// 「保存一次后所有字段都显示成已设置」。
pub fn read_value(path: &Path) -> Value {
    let current = settings::load_from(path);
    let fields: Vec<Value> = settings::FIELDS
        .iter()
        .map(|spec| {
            json!({
                "key": spec.key,
                "group": spec.group,
                "label": spec.label,
                "type": type_name(spec.field_type),
                "value": current_value(&current, spec),
                "default": spec.default_display,
                "description": spec.description,
                "options": spec.options,
                "min": spec.min,
                "max": spec.max,
            })
        })
        .collect();
    json!({
        "fields": fields,
        "path": path.display().to_string(),
    })
}

/// 从已加载的 `Settings` 里取某个字段的 JSON 值；`None` → `null`。
fn current_value(current: &settings::Settings, spec: &FieldSpec) -> Value {
    match spec.key {
        "agent.max_turns" => option_to_json(current.agent.max_turns),
        "agent.max_direct_children" => option_to_json(current.agent.max_direct_children),
        "agent.max_resident_subagents" => option_to_json(current.agent.max_resident_subagents),
        "agent.system_prompt" => option_to_json(current.agent.system_prompt.as_deref()),
        "agent.skills_catalog_budget" => option_to_json(current.agent.skills_catalog_budget),
        settings::PREVENT_IDLE_SLEEP_KEY => {
            option_to_json(current.power.prevent_idle_sleep.as_deref())
        }
        "power.idle_grace_minutes" => option_to_json(current.power.idle_grace_minutes),
        "power.remote_touch_minutes" => option_to_json(current.power.remote_touch_minutes),
        "compact.ratio" => option_to_json(current.compact.ratio),
        "compact.user_budget_tokens" => option_to_json(current.compact.user_budget_tokens),
        "compact.tool_budget_tokens" => option_to_json(current.compact.tool_budget_tokens),
        "startup.sandbox" => option_to_json(current.startup.sandbox.as_deref()),
        "tui.notifications" => option_to_json(current.tui.notifications.as_deref()),
        "tui.notification_method" => option_to_json(current.tui.notification_method.as_deref()),
        "tui.notification_condition" => {
            option_to_json(current.tui.notification_condition.as_deref())
        }
        "tui.terminal_title" => option_to_json(current.tui.terminal_title.as_deref()),
        // `FIELDS` 与这里的 match 必须同步。漏一个就会静默返回 null，
        // 故有一条测试遍历 FIELDS 断言每个键都能取到值。
        _ => Value::Null,
    }
}

fn option_to_json<T: Into<Value>>(value: Option<T>) -> Value {
    value.map(Into::into).unwrap_or(Value::Null)
}

fn type_name(field_type: FieldType) -> &'static str {
    match field_type {
        FieldType::Number => "number",
        FieldType::Text => "text",
        FieldType::Select => "select",
    }
}

/// `settings/write` 的响应。校验失败时零写入。
///
/// 同步版本；异步调用方必须走 [`write_value_async`]，否则等实例写锁（最多 5 秒）
/// 会直接占住 reactor 线程。
pub fn write_value(path: &Path, patch: &Value) -> Result<Value, SettingsError> {
    settings::write_to(path, patch)?;
    Ok(json!({"ok": true}))
}

/// RPC 入口使用的异步外壳：把受管写入整个搬到阻塞线程池。
///
/// 写锁等待是同步睡眠，不能直接跑在 async 任务里。`spawn_blocking` 的 join
/// 失败（panic / 运行时关闭）翻成受控 `Io` 错误，而不是让请求静默消失。
pub async fn write_value_async(
    path: std::path::PathBuf,
    patch: Value,
) -> Result<Value, SettingsError> {
    match tokio::task::spawn_blocking(move || write_value(&path, &patch)).await {
        Ok(result) => result,
        Err(error) => Err(SettingsError::Io(format!(
            "settings write task failed: {error}"
        ))),
    }
}

/// 把 `SettingsError` 翻成 RPC 错误。`invalid_setting` 是稳定词汇，UI 据此
/// 在对应输入框旁内联提示，而不是弹一个整页错误。
pub fn write_error(error: SettingsError) -> RpcError {
    match error {
        SettingsError::InvalidSetting { key, reason } => RpcError {
            code: -32602,
            message: format!("invalid setting {key}: {reason}"),
            data: Some(json!({ "code": "invalid_setting", "key": key, "reason": reason })),
        },
        SettingsError::Corrupt(reason) => RpcError {
            code: -32603,
            message: format!("settings.json is not writable: {reason}"),
            data: Some(json!({ "code": "settings_corrupt" })),
        },
        SettingsError::Io(reason) => RpcError::internal(reason),
        // Task 1 只保证枚举穷尽匹配可编译并给出受控错误码；精确的健康/采用
        // 协议由 Task 10 补齐。
        SettingsError::InvalidDocument { keys } => RpcError {
            code: -32602,
            message: format!("invalid settings.json keys: {}", keys.join(", ")),
            data: Some(json!({ "code": "invalid_document", "keys": keys })),
        },
        SettingsError::LockTimeout => RpcError {
            code: -32603,
            message: "settings.json is locked by another writer; try again later".to_string(),
            data: Some(json!({ "code": "settings_lock_timeout" })),
        },
        SettingsError::TooLarge => RpcError {
            code: -32602,
            message: "settings.json exceeds the 1 MiB limit".to_string(),
            data: Some(json!({ "code": "settings_too_large" })),
        },
    }
}

/// `settings/write` 需要 `Control`：它改的是本机的长期运行参数。
pub fn write_scope() -> Scope {
    Scope::Control
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `FIELDS` 与 `current_value` 的 match 必须同步：漏一个键会让它静默
    /// 返回 null，前端显示「未设置」而文件里其实有值。
    #[test]
    fn every_field_in_fields_can_be_read_back() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        // 给每个可持久化字段都写一个合法值。
        let mut patch = serde_json::Map::new();
        for spec in settings::FIELDS {
            let value = match spec.field_type {
                FieldType::Number => json!(spec.min.unwrap_or(1)),
                FieldType::Text => json!("set"),
                FieldType::Select => json!(spec.options[0]),
            };
            patch.insert(spec.key.to_string(), value);
        }
        settings::write_to(&path, &Value::Object(patch)).unwrap();

        let current = settings::load_from(&path);
        for spec in settings::FIELDS {
            assert_ne!(
                current_value(&current, spec),
                Value::Null,
                "{} is in FIELDS but current_value returned null for it",
                spec.key
            );
        }
    }

    /// 受管写入必须真的离开 reactor：等 `flock` 期间定时器仍能按时触发。
    ///
    /// 抓的破坏：把 `write_value` 直接 `.await` 在 async 任务里。在单线程
    /// runtime 上，5 秒的同步等锁会卡住整个 reactor，这个 50ms 定时器就只能
    /// 在写之后才跑。
    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn the_write_runs_off_the_reactor() {
        use std::os::unix::io::AsRawFd;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        write_value(&path, &json!({"agent.max_turns": 1})).unwrap();

        let lock = path.with_file_name(".settings.json.lock");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(rc, 0, "the test must own the lock: {rc}");

        // 在阻塞线程池里持有 flock 150ms 后释放：写请求会在这段窗口里等锁。
        let release = tokio::task::spawn_blocking(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(held);
        });

        let write = tokio::spawn(write_value_async(
            path.clone(),
            json!({"agent.max_turns": 2}),
        ));
        // 定时器必须在写入完成之前到期。若写入占住 reactor，这里会先等到写
        // 完成（或者永远等不到），断言随即失败。
        tokio::time::timeout(std::time::Duration::from_millis(80), write)
            .await
            .expect_err("the write must still be waiting when the timer fires");
        release.await.unwrap();
    }

    #[test]
    fn read_reports_the_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let value = read_value(&path);
        assert_eq!(value["path"], path.display().to_string());
    }

    /// 未设置的字段必须是 `null`，不能归一成默认值。
    #[test]
    fn an_absent_field_reads_as_null_not_as_its_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let value = read_value(&path);
        let fields = value["fields"].as_array().unwrap();
        for field in fields {
            assert!(
                field["value"].is_null(),
                "nothing is set, so every value must be null: {field}"
            );
            // `agent.system_prompt` 的默认就是「不覆盖」，故这里只要求
            // `default` 是个字符串（空串合法），不要求非空。
            assert!(
                field["default"].is_string(),
                "every field carries its default text: {field}"
            );
        }
    }

    /// 未知键与越界值都翻成 `invalid_setting`，并带字段名。
    #[test]
    fn errors_carry_the_offending_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        let error = write_value(&path, &json!({"no.such_key": 1})).unwrap_err();
        let rpc = write_error(error);
        assert_eq!(rpc.data.as_ref().unwrap()["code"], "invalid_setting");
        assert_eq!(rpc.data.as_ref().unwrap()["key"], "no.such_key");

        let error = write_value(&path, &json!({"agent.max_turns": 0})).unwrap_err();
        let rpc = write_error(error);
        assert_eq!(rpc.data.as_ref().unwrap()["key"], "agent.max_turns");
    }
}
