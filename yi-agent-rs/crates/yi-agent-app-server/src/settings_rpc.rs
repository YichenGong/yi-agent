//! `settings/read` 与 `settings/write`：`settings.json` 的 RPC 外壳。
//!
//! 逻辑全在 `yi_agent_runtime::settings`。本模块只做两件事：把类型化的
//! `Settings` 摊成前端能渲染的 JSON 数组，以及把 `SettingsError` 翻成 RPC 错误。
//! **校验规则只在这里调用一次**（`write_to` 内部），不另写一份——多一份就是
//! 多一处迟早漂移的规则。

use std::path::Path;

use serde_json::{Value, json};
use yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding;
use yi_agent_runtime::live_settings::sources::ValueSource;
use yi_agent_runtime::settings::document::{ApplyPolicy, apply_policy};
use yi_agent_runtime::settings::store::PatchMode;
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

/// Capability version of the live-settings protocol.
///
/// Absent means "this server predates live settings": a client must then show
/// only "saved", never promise that running conversations will adopt the change.
pub const LIVE_APPLY_VERSION: u32 = 1;

/// `settings/read` for a live binding: the historic `fields`/`path` shape plus
/// revision, health and per-field host context.
///
/// Everything added is additive. Old clients keep reading `fields` and `path`
/// unchanged; new clients only trust the live fields when
/// `live_apply_version` is present.
///
/// `host_value`/`host_source` describe the *host default context*, not any
/// running conversation's effective value: a conversation may be narrower
/// because of its own recorded baseline, and the UI must not read this as "what
/// my session is doing right now".
pub fn read_live_value(binding: &RuntimeSettingsBinding) -> Value {
    let path = binding.live.path().to_path_buf();
    let snapshot = binding.live.refresh();
    let effective = binding.sources.resolve(&snapshot.settings);

    let mut value = read_value(&path);
    let fields: Vec<Value> = settings::FIELDS
        .iter()
        .map(|spec| {
            let source = effective.sources.get(spec.key).copied();
            json!({
                "key": spec.key,
                "group": spec.group,
                "label": spec.label,
                "type": type_name(spec.field_type),
                "value": current_value(&snapshot.settings, spec),
                "default": spec.default_display,
                "description": spec.description,
                "options": spec.options,
                "min": spec.min,
                "max": spec.max,
                "apply_policy": apply_policy_name(spec.key),
                // The effective default for this process, which may be a frozen
                // CLI/env value or a field default rather than the file's value.
                "host_value": effective.values.get(spec.key).cloned().unwrap_or(Value::Null),
                "host_source": host_source_name(source),
            })
        })
        .collect();

    if let Some(object) = value.as_object_mut() {
        object.insert("fields".into(), Value::Array(fields));
        object.insert("live_apply_version".into(), json!(LIVE_APPLY_VERSION));
        object.insert("revision".into(), json!(snapshot.revision));
        object.insert(
            "snapshot_origin".into(),
            json!(snapshot_origin_name(snapshot.origin)),
        );
        object.insert(
            "health".into(),
            json!({
                "state": match snapshot.health.state {
                    yi_agent_runtime::live_settings::HealthState::Healthy => "healthy",
                    yi_agent_runtime::live_settings::HealthState::Degraded => "degraded",
                },
                // Only categories and key names. A code that echoed a value
                // would put user content (a prompt, say) into diagnostics.
                "code": snapshot.health.code,
                "invalid_keys": snapshot.health.invalid_keys,
            }),
        );
    }
    value
}

/// `settings/write` for a live binding: commit through the shared instance so
/// every conversation sees the new values, then describe the receipt.
///
/// Synchronous for the same reason as [`write_value`]: the caller must run it
/// off the reactor. Returning the receipt lets the caller build the response
/// *before* any broadcast, so a failing notification can never turn a committed
/// save into an error the user would retry.
pub fn write_live_value(
    binding: &RuntimeSettingsBinding,
    patch: &Value,
) -> Result<Value, SettingsError> {
    let receipt = binding.live.commit(patch, PatchMode::ReplaceProvided)?;
    let overridden: Vec<Value> = receipt
        .changed_keys
        .iter()
        .filter_map(|key| {
            binding
                .sources
                .resolve(&receipt.settings)
                .sources
                .get(key)
                .copied()
                .filter(|source| matches!(source, ValueSource::Override | ValueSource::Environment))
                .map(|source| {
                    json!({
                        "key": key,
                        "source": match source {
                            ValueSource::Override => "override",
                            ValueSource::Environment => "environment",
                            // resolve() only reports these two as host-level;
                            // settings/default come from the file itself.
                            _ => "override",
                        },
                    })
                })
        })
        .collect();
    let policies: serde_json::Map<String, Value> = receipt
        .apply_policy
        .iter()
        .map(|(key, policy)| (key.clone(), json!(policy_name(*policy))))
        .collect();

    Ok(json!({
        "ok": true,
        "live_apply_version": LIVE_APPLY_VERSION,
        "revision": receipt.revision,
        "changed_keys": receipt.changed_keys,
        "apply_policy": policies,
        "overridden_keys": overridden,
    }))
}

/// The `config/read` keys that mirror a live runtime scalar one-for-one.
///
/// Deliberately short. `compact_threshold` is *derived* from the session's own
/// context length and the live ratio, so replacing it from the host context
/// would describe a different session; `sandbox` and the model/provider fields
/// are session permissions/identity and must not be restated from host defaults
/// (spec §7 诊断: 会话权限/模型不因该展示刷新改变).
const CONFIG_VIEW_LIVE_KEYS: [&str; 3] =
    ["max_turns", "max_resident_subagents", "max_direct_children"];

/// The live key each `config/read` key mirrors.
fn config_view_source_key(key: &str) -> Option<&'static str> {
    match key {
        "max_turns" => Some("agent.max_turns"),
        "max_resident_subagents" => Some("agent.max_resident_subagents"),
        "max_direct_children" => Some("agent.max_direct_children"),
        _ => None,
    }
}

/// Stamp a `config/read` view with the live settings context.
///
/// Two separate things, and conflating them would mislead a client:
///
/// 1. The three runtime tunables are replaced with the host's *current* effective
///    values, because they are consumed live — a card showing the start-up value
///    would be stale the moment the user saves.
/// 2. Everything else gains a `live_apply` block describing revision, health and
///    each key's source, so a client can say "currently overridden by
///    `--max-turns`" instead of silently showing a value that will not apply.
///
/// Nothing here changes any conversation's permissions or model: this is a
/// read-only restatement for diagnostics.
pub fn annotate_live_config_view(view: &mut Value, binding: &RuntimeSettingsBinding) {
    let snapshot = binding.live.refresh();
    let effective = binding.sources.resolve(&snapshot.settings);

    if let Some(object) = view.as_object_mut() {
        for key in CONFIG_VIEW_LIVE_KEYS {
            let Some(source_key) = config_view_source_key(key) else {
                continue;
            };
            let Some(value) = effective.values.get(source_key) else {
                continue;
            };
            if let Some(slot) = object.get_mut(key) {
                *slot = value.clone();
            }
        }

        let sources: serde_json::Map<String, Value> = CONFIG_VIEW_LIVE_KEYS
            .iter()
            .filter_map(|key| {
                let source_key = config_view_source_key(key)?;
                let source = effective.sources.get(source_key).copied()?;
                Some(((*key).to_string(), json!(host_source_name(Some(source)))))
            })
            .collect();

        object.insert(
            "live_apply".into(),
            json!({
                "live_apply_version": LIVE_APPLY_VERSION,
                "revision": snapshot.revision,
                "snapshot_origin": snapshot_origin_name(snapshot.origin),
                "health": {
                    "state": match snapshot.health.state {
                        yi_agent_runtime::live_settings::HealthState::Healthy => "healthy",
                        yi_agent_runtime::live_settings::HealthState::Degraded => "degraded",
                    },
                    "code": snapshot.health.code,
                    "invalid_keys": snapshot.health.invalid_keys,
                },
                "sources": sources,
            }),
        );
    }
}

pub fn apply_policy_name(key: &str) -> &'static str {
    match apply_policy(key) {
        Some(ApplyPolicy::NextSession) => "next_session",
        // Unknown keys cannot reach here (FIELDS is authoritative), and the safe
        // reading of "not a next-session item" is the live one.
        Some(ApplyPolicy::NextSafePoint) | None => "next_safe_point",
    }
}

fn policy_name(policy: ApplyPolicy) -> &'static str {
    match policy {
        ApplyPolicy::NextSafePoint => "next_safe_point",
        ApplyPolicy::NextSession => "next_session",
    }
}

fn host_source_name(source: Option<ValueSource>) -> &'static str {
    match source {
        Some(ValueSource::Override) => "override",
        Some(ValueSource::Environment) => "environment",
        Some(ValueSource::Settings) => "settings",
        Some(ValueSource::Default) => "default",
        None => "default",
    }
}

fn snapshot_origin_name(origin: yi_agent_runtime::live_settings::SnapshotOrigin) -> &'static str {
    use yi_agent_runtime::live_settings::SnapshotOrigin;
    match origin {
        SnapshotOrigin::Current => "current",
        SnapshotOrigin::LastKnownGood => "last_known_good",
        SnapshotOrigin::StartupFallback => "startup_fallback",
    }
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

/// The live counterpart of [`write_value_async`]: commit through the shared
/// instance off the reactor, then report the receipt.
///
/// The receipt is returned rather than broadcast from here so the caller can
/// write the response first. A committed save must never be reported as a
/// failure because a notification could not be delivered.
pub async fn write_live_value_async(
    binding: RuntimeSettingsBinding,
    patch: Value,
) -> Result<Value, SettingsError> {
    match tokio::task::spawn_blocking(move || write_live_value(&binding, &patch)).await {
        Ok(result) => result,
        Err(error) => Err(SettingsError::Io(format!(
            "settings write task failed: {error}"
        ))),
    }
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

    fn live_binding(dir: &tempfile::TempDir) -> RuntimeSettingsBinding {
        RuntimeSettingsBinding {
            live: yi_agent_runtime::live_settings::LiveSettings::shared(
                dir.path().join("settings.json"),
            ),
            sources: std::sync::Arc::new(
                yi_agent_runtime::live_settings::sources::FrozenRuntimeSources::default(),
            ),
        }
    }

    #[test]
    fn a_live_read_reports_capability_origin_and_health() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding = live_binding(&dir);
        let view = read_live_value(&binding);
        assert_eq!(view["live_apply_version"], 1);
        assert_eq!(view["snapshot_origin"], "current");
        assert_eq!(view["health"]["state"], "healthy");
        assert_eq!(view["health"]["code"], Value::Null);
        assert_eq!(view["health"]["invalid_keys"], json!([]));
        // The historic shape is untouched, so old clients keep working.
        assert!(view["path"].is_string());
        assert!(view["fields"].is_array());
    }

    #[test]
    fn a_live_write_reports_policy_and_changed_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding = live_binding(&dir);
        let saved =
            write_live_value(&binding, &json!({"agent.max_turns": 41})).expect("valid patch");
        assert_eq!(saved["ok"], true);
        assert_eq!(saved["live_apply_version"], 1);
        assert_eq!(saved["changed_keys"], json!(["agent.max_turns"]));
        assert_eq!(saved["apply_policy"]["agent.max_turns"], "next_safe_point");
        assert!(saved["revision"].is_string());
        assert_eq!(saved["overridden_keys"], json!([]));
    }

    #[test]
    fn startup_sandbox_is_a_next_session_item() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding = live_binding(&dir);
        let saved = write_live_value(&binding, &json!({"startup.sandbox": "read-only"})).unwrap();
        assert_eq!(saved["apply_policy"]["startup.sandbox"], "next_session");
        let view = read_live_value(&binding);
        let field = view["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "startup.sandbox")
            .expect("the field is listed");
        assert_eq!(field["apply_policy"], "next_session");
    }

    #[test]
    fn every_field_reports_a_policy_and_a_host_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding = live_binding(&dir);
        let view = read_live_value(&binding);
        for field in view["fields"].as_array().unwrap() {
            assert!(
                matches!(
                    field["apply_policy"].as_str(),
                    Some("next_safe_point" | "next_session")
                ),
                "{field:?}"
            );
            assert!(
                matches!(
                    field["host_source"].as_str(),
                    Some("override" | "environment" | "settings" | "default")
                ),
                "{field:?}"
            );
            // A field default always exists, so the host context is never null
            // for a number/text/select field except `system_prompt`'s null.
            assert!(field.get("host_value").is_some(), "{field:?}");
        }
    }

    /// A frozen CLI/env value must be reported as the host default *and* as an
    /// override for the changed key — without exposing the value in diagnostics.
    #[test]
    fn a_host_override_shadows_the_file_value() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding =
            RuntimeSettingsBinding {
                live: yi_agent_runtime::live_settings::LiveSettings::shared(
                    dir.path().join("settings.json"),
                ),
                sources: std::sync::Arc::new(
                    yi_agent_runtime::live_settings::sources::FrozenRuntimeSources::from_pairs(&[
                        ("agent.max_turns", json!(7), ValueSource::Environment),
                    ]),
                ),
            };
        let view = read_live_value(&binding);
        let field = view["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "agent.max_turns")
            .unwrap();
        assert_eq!(field["host_value"], 7);
        assert_eq!(field["host_source"], "environment");

        let saved = write_live_value(&binding, &json!({"agent.max_turns": 41})).unwrap();
        assert_eq!(
            saved["overridden_keys"],
            json!([{"key": "agent.max_turns", "source": "environment"}])
        );
    }

    #[test]
    fn an_invalid_patch_writes_nothing_and_reports_no_changes() {
        let dir = tempfile::TempDir::new().unwrap();
        let binding = live_binding(&dir);
        write_live_value(&binding, &json!({"agent.max_turns": 5})).unwrap();
        let before = read_live_value(&binding)["revision"].clone();

        let error = write_live_value(
            &binding,
            &json!({"agent.max_turns": 6, "agent.max_direct_children": -1}),
        )
        .expect_err("one invalid key must reject the whole patch");
        assert!(!error.to_string().contains("USER-SECRET"));

        // The good key in the same batch must not have landed: validation is
        // whole-patch, so a partial write would contradict "zero write".
        let after = read_live_value(&binding);
        assert_eq!(
            after["revision"], before,
            "a rejected patch must not commit"
        );
        let field = after["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["key"] == "agent.max_turns")
            .unwrap();
        assert_eq!(field["value"], 5);
    }

    /// Health diagnostics carry key names and a category, never a value. A
    /// corrupt document whose text contains user content must not leak it.
    #[test]
    fn degraded_health_reports_no_settings_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"agent":{"max_turns":"USER-SECRET","system_prompt":"USER-SECRET"}}"#,
        )
        .unwrap();
        let binding = live_binding(&dir);
        let view = read_live_value(&binding);
        assert_eq!(view["health"]["state"], "degraded");
        // Only the health sub-object must be value-free. `fields[].value` is the
        // settings content the client renders, so it may of course contain it.
        let health = view["health"].to_string();
        assert!(
            !health.contains("USER-SECRET"),
            "health diagnostics must not echo values: {health}"
        );
        assert_eq!(view["health"]["code"], "invalid_document");
        assert_eq!(view["health"]["invalid_keys"], json!(["agent.max_turns"]));
    }

    /// 错误里绝不能出现设置取值。spec §9：不把设置值拼进错误或日志。
    ///
    /// `Corrupt` 的 reason 来自 `serde_json` 的定位信息（行/列），`Io` 来自
    /// `std::io`。两者都不带原文；这条测试把该保证钉住，防止有人为了"更好
    /// 的报错"把文档内容拼进去。
    #[test]
    fn write_errors_never_echo_setting_values() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"agent\": {\"max_turns\": \"VALUE-SECRET\"}").unwrap();
        let binding = live_binding(&dir);
        let error = write_live_value(&binding, &json!({"agent.max_turns": 3}))
            .expect_err("an unparseable document must refuse the write");
        let rpc = write_error(error);
        let rendered = serde_json::json!({
            "message": rpc.message,
            "data": rpc.data,
        })
        .to_string();
        assert!(
            !rendered.contains("VALUE-SECRET"),
            "error leaked a value: {rendered}"
        );

        // 非法值同理：只带键名与原因分类，不带用户填的原文。
        let error = write_live_value(&binding, &json!({"agent.max_turns": "NOT-A-NUMBER"}))
            .expect_err("a wrong type must be rejected");
        let rpc = write_error(error);
        let rendered = serde_json::json!({"message": rpc.message, "data": rpc.data}).to_string();
        assert!(
            !rendered.contains("NOT-A-NUMBER"),
            "error leaked: {rendered}"
        );
    }

    /// 每个错误类别都有稳定的 `data.code`，UI 据此内联提示而不是弹整页错误。
    #[test]
    fn every_error_class_has_a_stable_code() {
        let cases = [
            (
                SettingsError::InvalidSetting {
                    key: "agent.max_turns".into(),
                    reason: "out of range".into(),
                },
                "invalid_setting",
                -32602,
            ),
            (
                SettingsError::Corrupt("bad json".into()),
                "settings_corrupt",
                -32603,
            ),
            (
                SettingsError::InvalidDocument {
                    keys: vec!["agent.max_turns".into()],
                },
                "invalid_document",
                -32602,
            ),
            (SettingsError::LockTimeout, "settings_lock_timeout", -32603),
            (SettingsError::TooLarge, "settings_too_large", -32602),
        ];
        for (error, code, rpc_code) in cases {
            let rpc = write_error(error);
            assert_eq!(rpc.data.as_ref().unwrap()["code"], code);
            assert_eq!(rpc.code, rpc_code, "{code}");
        }
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
