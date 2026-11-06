//! 运行参数的严格文档校验。
//!
//! 与 [`crate::settings::load_from`] 的宽容启动读取不同：这里把「已识别字段
//! 非法」当成整份候选被拒，而不是悄悄当默认值。设置页保存、运行期重读和
//! 观察器共用这一份判据，避免一个入口接受、另一个入口静默降级。

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::settings::{FIELDS, FieldSpec, Settings, SettingsError};

/// 一份通过严格校验的 `settings.json` 文档。
#[derive(Debug, Clone)]
pub struct ValidatedSettingsDocument {
    /// 规范化后的原文：已知旧键已搬走，未知键原样保留。
    pub raw: serde_json::Value,
    /// 由同一份原文构造的类型化设置。
    pub settings: crate::settings::Settings,
    /// 已识别字段（含未设置状态）的内容标识，不是序号。
    pub revision: String,
}

/// 字段的采用策略。
///
/// 序列化形状固定为 `next_safe_point` / `next_session`（Task 10 的 RPC 响应
/// 直接复用它），不要在协议层另造一套词汇。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyPolicy {
    NextSafePoint,
    NextSession,
}

/// 已知分组的固定顺序；错误里的键名按它排列，保证同一份输入产生同样的
/// 诊断次序。
const GROUPS: &[&str] = &["agent", "compact", "startup", "tui", "power"];

/// 旧键所在分组与字段名。字面量，不引用 `PREVENT_IDLE_SLEEP_KEY`——那个
/// 常量已经指向新键，引用它会让迁移随常量漂移。
const LEGACY_GROUP: &str = "agent";
const LEGACY_FIELD: &str = "prevent_idle_sleep";
/// 旧键的新家。与迁移函数里的 `NEW_GROUP` 同值，但保持各自字面量：两处
/// 语义不同（一处描述「旧键搬去哪」，一处描述「严格校验要保住哪个分组」）。
const NEW_GROUP: &str = "power";

/// 严格校验整份文档；任何已识别字段非法都整份拒绝。
///
/// 与 [`crate::settings::load_from`] 的宽容启动读取不同：这里不做「坏字段当
/// 默认」的逐字段降级，而是把全部非法键一次列出、整份不采用。错误只带键名，
/// 不回显用户填的值（可能含密钥）。
pub fn validate_document(raw: Value) -> Result<ValidatedSettingsDocument, SettingsError> {
    let mut normalized = raw;
    if !normalized.is_object() {
        // 根不是对象时整份文档无法解释。用文档级错误而不是 `Corrupt`：后者
        // 的文案是「not writable」，对一次纯校验读是误导。
        return Err(SettingsError::InvalidDocument {
            keys: vec!["<root>".to_string()],
        });
    }

    canonicalize_legacy_alias(&mut normalized);

    let mut invalid = Vec::new();

    // 已知分组必须是对象。形状坏掉时整组字段都无法解释，直接点名分组，
    // 不能回退成默认值把用户其它分组静默清空。
    for group in GROUPS {
        if normalized
            .get(*group)
            .is_some_and(|value| !value.is_object())
        {
            invalid.push((*group).to_string());
        }
    }
    // 逐字段在原始 JSON 上判断「未设置 / 合法 / 非法」。复用写入侧的
    // `validate_value`：同一套类型、范围与枚举规则，数字字符串在这里转成
    // 规范数字并写回原文，typed `Settings` 才不会悄悄丢值。
    //
    // 分组形状坏掉时仍继续扫其它分组：一次列全所有要修的键，而不是让用户
    // 修一个存一个。坏分组内的字段无法解释（`Value::get` 对非对象返回
    // `None`），只会以分组名出现在错误里。
    for field in FIELDS {
        let (group, _) = field
            .key
            .split_once('.')
            .expect("every FIELDS key is group.field");
        let Some(group_value) = normalized.get(group) else {
            continue;
        };
        if !group_value.is_object() {
            continue;
        }
        let Some(current) = field_value(&normalized, field).cloned() else {
            continue;
        };
        match super::validate_value(field, &current) {
            Ok(Some(value)) => super::apply_op(
                normalized
                    .as_object_mut()
                    .expect("validated as an object above"),
                field.key,
                Some(value),
            ),
            Ok(None) => super::apply_op(
                normalized
                    .as_object_mut()
                    .expect("validated as an object above"),
                field.key,
                None,
            ),
            // `validate_value` 的 reason 可能带上取值（Select 分支），这里只
            // 收键名，绝不把原文带进文档级错误。
            Err(_) => invalid.push(field.key.to_string()),
        }
    }
    if !invalid.is_empty() {
        return Err(SettingsError::InvalidDocument { keys: invalid });
    }

    // 跨字段规则必须在全部单字段都合法之后做：倒挂的两个值各自都在范围内，
    // 只有合起来才非法。
    check_cross_field(&normalized)?;

    // 空壳分组（用户把最后一个键删空、或值全是 null）不留在规范原文里。
    remove_empty_groups(&mut normalized);

    let settings = serde_json::from_value::<Settings>(normalized.clone()).map_err(|_| {
        SettingsError::Corrupt("validated settings document failed to deserialize".to_string())
    })?;

    let logical: BTreeMap<&str, Value> = FIELDS
        .iter()
        .map(|field| (field.key, canonical_value(&normalized, field)))
        .collect();
    let bytes = serde_json::to_vec(&logical)
        .map_err(|_| SettingsError::Corrupt("canonical serialization failed".to_string()))?;
    let revision = format!("{:x}", Sha256::digest(bytes));

    Ok(ValidatedSettingsDocument {
        raw: normalized,
        settings,
        revision,
    })
}

/// 已知字段的采用策略；`FIELDS` 之外的键返回 `None`。
///
/// 只有 `startup.sandbox` 是下一会话项：它决定新会话的权限，不能中途给已经
/// 跑起来的任务换档。其余字段都在下一个安全执行点采用。
pub fn apply_policy(key: &str) -> Option<ApplyPolicy> {
    let field = crate::settings::find_spec(key)?;
    Some(if field.key == "startup.sandbox" {
        ApplyPolicy::NextSession
    } else {
        ApplyPolicy::NextSafePoint
    })
}

/// 应用旧键映射，并保证旧键不会残留在规范原文里。
///
/// `migrate_legacy_keys` 只在新键缺失时搬运；两个键同时存在时它按「新键优先」
/// 直接返回，旧键会留在 `agent` 组里。严格文档必须把旧键清掉：规范原文是
/// 后续写入和 revision 的底稿，留着它会让下次保存把旧值又搬回来。
fn canonicalize_legacy_alias(value: &mut Value) {
    // 目标分组存在但不是对象时不能交给 `migrate_legacy_keys`：它对「已存在的
    // 非对象分组」会 `expect("just inserted an object")` 并 panic。一次手改
    // 文件（`{"power": null}`）就足以构造这个形状，而本函数服务的是观察器
    // 与保存路径——那里 panic 等于让后台线程死掉。坏形状留给下面的分组校验，
    // 由它作为文档错误点名。
    if value.get(NEW_GROUP).is_none_or(Value::is_object) {
        super::migrate_legacy_keys(value);
    }

    let Some(object) = value.as_object_mut() else {
        return;
    };
    let agent_now_empty = match object
        .get_mut(LEGACY_GROUP)
        .and_then(|group| group.as_object_mut())
    {
        Some(group) => {
            group.remove(LEGACY_FIELD);
            group.is_empty()
        }
        None => false,
    };
    if agent_now_empty {
        object.remove(LEGACY_GROUP);
    }
}

/// 读取一个字段的原始值；分组或字段缺失都表示「未设置」。
fn field_value<'a>(normalized: &'a Value, field: &FieldSpec) -> Option<&'a Value> {
    let (group, name) = field.key.split_once('.')?;
    normalized.get(group)?.get(name)
}

/// 规范值：未设置是 `null`，已设置是规范化后的原文值。
///
/// 只在完整字段校验成功后调用。
fn canonical_value(normalized: &Value, field: &FieldSpec) -> Value {
    field_value(normalized, field)
        .cloned()
        .unwrap_or(Value::Null)
}

/// 跨字段：远端有效窗口不得长于保活宽限，否则「没活就放睡」的宽限在电池
/// 场景下形同虚设。未设置的一侧按默认值参与比较，与写入侧口径一致。
fn check_cross_field(normalized: &Value) -> Result<(), SettingsError> {
    let grace = field_value(normalized, spec("power.idle_grace_minutes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::settings::POWER_IDLE_GRACE_MINUTES_DEFAULT);
    let remote = field_value(normalized, spec("power.remote_touch_minutes"))
        .and_then(Value::as_u64)
        .unwrap_or(crate::settings::POWER_REMOTE_TOUCH_MINUTES_DEFAULT);
    if remote > grace {
        return Err(SettingsError::InvalidDocument {
            keys: vec!["power.remote_touch_minutes".to_string()],
        });
    }
    Ok(())
}

/// `FIELDS` 中的元数据；键来自本模块常量，取不到即为编码错误。
fn spec(key: &'static str) -> &'static FieldSpec {
    crate::settings::find_spec(key).expect("key must exist in FIELDS")
}

/// 删掉只剩空壳的已知分组，避免规范原文里留下 `{"agent":{}}` 噪音。
fn remove_empty_groups(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for group in GROUPS {
        if object
            .get(*group)
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty)
        {
            object.remove(*group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyPolicy, apply_policy, validate_document};

    fn revision_for(document: &serde_json::Value) -> String {
        validate_document(document.clone())
            .expect("the document must be valid")
            .revision
    }

    /// 已知字段非法时整份拒绝，并点名键；错误文本不得回显用户填的值。
    ///
    /// 抓的破坏：把严格校验换成宽容反序列化，坏值被当默认，保存悄悄成功。
    #[test]
    fn live_document_rejects_bad_known_field_instead_of_defaulting() {
        let error = validate_document(serde_json::json!({
            "agent": {"max_turns": "not-a-number"},
            "unknown": {"future": true}
        }))
        .expect_err("known invalid value must be rejected");
        assert!(error.to_string().contains("agent.max_turns"));
        assert!(!error.to_string().contains("not-a-number"));
    }

    /// `FIELDS` 的每一项都必须有且只有一个采用策略，沙箱是唯一的下一会话项。
    ///
    /// 抓的破坏：新增字段漏配策略（`None`），或把 `startup.sandbox` 误标成
    /// `next_safe_point`，让已有会话被新默认改权限。
    #[test]
    fn every_runtime_field_has_one_apply_policy() {
        assert_eq!(crate::settings::FIELDS.len(), 16);
        for field in crate::settings::FIELDS {
            let policy = apply_policy(field.key).expect(field.key);
            assert_eq!(
                policy == ApplyPolicy::NextSession,
                field.key == "startup.sandbox"
            );
        }
    }

    /// revision 是内容标识、不是序号：A→B→A 必须回到 A 的标识。
    ///
    /// 抓的破坏：把 revision 实现成递增计数器或时间戳，重放旧内容被当成新提交。
    #[test]
    fn revision_repeats_for_a_b_a() {
        let a = serde_json::json!({"agent": {"max_turns": 200}});
        let b = serde_json::json!({"agent": {"max_turns": 201}});

        let first = revision_for(&a);
        let changed = revision_for(&b);
        let again = revision_for(&a);

        assert_ne!(first, changed, "different content must change the revision");
        assert_eq!(
            first, again,
            "the same logical content must keep its revision"
        );
    }

    /// 数字字符串必须换成规范数字进入类型化设置，不能只在校验表里规范化。
    ///
    /// 抓的破坏：revision 用规范值算、`Settings` 却仍从原始 JSON 反序列化，
    /// 于是保存成功但 typed 值悄悄丢成 `None`（用默认）。
    #[test]
    fn numeric_strings_become_numbers_in_the_typed_settings() {
        let document = validate_document(serde_json::json!({"agent": {"max_turns": "37"}}))
            .expect("a numeric string is accepted, as the write path accepts it");

        assert_eq!(document.settings.agent.max_turns, Some(37));
        assert_eq!(document.raw["agent"]["max_turns"], serde_json::json!(37));
        assert_eq!(
            document.revision,
            revision_for(&serde_json::json!({"agent": {"max_turns": 37}}))
        );
    }

    /// 坏分组与另一个分组里的坏字段必须一次性全部列出。
    ///
    /// 抓的破坏：见到坏分组就提前返回，用户把 `agent` 修好后才发现
    /// `compact.ratio` 也不合法，要存两轮。
    #[test]
    fn a_bad_group_and_bad_fields_are_reported_together() {
        let error = validate_document(serde_json::json!({
            "agent": 5,
            "compact": {"ratio": 0},
            "tui": {"notification_method": "carrier-pigeon"}
        }))
        .expect_err("every bad group and field must be rejected");

        let message = error.to_string();
        for key in ["agent", "compact.ratio", "tui.notification_method"] {
            assert!(message.contains(key), "{key} missing from {message}");
        }
    }

    /// 根不是对象时按文档级错误拒绝，且不回显原文。
    ///
    /// 抓的破坏：把根形状错误报成 `Corrupt`（"not writable"），或者把整个
    /// 数组塞进错误消息。
    #[test]
    fn a_non_object_root_is_a_document_error() {
        let error = validate_document(serde_json::json!(["TOPSECRET-ROOT"]))
            .expect_err("a non-object root must be rejected");

        assert!(
            matches!(
                error,
                crate::settings::SettingsError::InvalidDocument { .. }
            ),
            "got {error:?}"
        );
        assert!(error.to_string().contains("<root>"), "got {error}");
        assert!(!error.to_string().contains("TOPSECRET-ROOT"), "got {error}");
    }

    /// 已知分组不是对象时整份拒绝，并点名该分组。
    ///
    /// 抓的破坏：分组形状坏掉时回退成默认值，用户其它字段被静默清空。
    #[test]
    fn a_known_group_that_is_not_an_object_is_rejected() {
        let error = validate_document(serde_json::json!({"agent": 5, "compact": {"ratio": 70}}))
            .expect_err("a non-object known group must be rejected");

        assert!(error.to_string().contains("agent"), "got {error}");
        assert!(!error.to_string().contains('5'), "got {error}");
    }

    /// 旧别名要搬运、目标分组却不是对象时，整份拒绝而不是 panic。
    ///
    /// 抓的破坏：直接复用迁移函数而不自己兜住形状——`migrate_legacy_keys` 对
    /// 「`power` 已存在但不是对象」会 `expect("just inserted an object")`，
    /// 一次手改文件就能让校验线程 panic，而不是给用户一条可读的诊断。
    #[test]
    fn a_legacy_alias_with_a_non_object_target_group_is_rejected() {
        let error = validate_document(serde_json::json!({
            "agent": {"prevent_idle_sleep": "true"},
            "power": null
        }))
        .expect_err("the destination group has the wrong shape");

        assert!(error.to_string().contains("power"), "got {error}");
    }

    /// 未知键不参与内容标识：加一个未来字段不能让 revision 变化。
    ///
    /// 抓的破坏：revision 直接对整份原文做 hash，别的工具写个无关键就让所有
    /// 客户端收到一次假的「设置变了」。
    #[test]
    fn revision_ignores_unknown_keys() {
        let without = serde_json::json!({"agent": {"max_turns": 200}});
        let with_unknown = serde_json::json!({
            "agent": {"max_turns": 200},
            "unknown": {"future": true}
        });

        assert_eq!(revision_for(&without), revision_for(&with_unknown));
    }

    /// 旧别名搬成新键后逻辑内容不变，revision 必须相同。
    ///
    /// 抓的破坏：把「写文件时的规范化」也算进 revision，用户重存一次没改任何
    /// 值就收到逻辑变更通知。
    #[test]
    fn a_legacy_alias_keeps_the_revision_of_its_canonical_key() {
        let legacy = serde_json::json!({"agent": {"prevent_idle_sleep": "true"}});
        let canonical = serde_json::json!({"power": {"prevent_idle_sleep": "true"}});

        assert_eq!(revision_for(&legacy), revision_for(&canonical));
    }

    /// 多个非法已知字段时一次列出全部键名，且不泄露任何取值。
    ///
    /// 抓的破坏：遇到第一个坏字段就返回，用户要修一轮存一轮；或错误里带上
    /// 用户填的原文（可能含密钥）。
    #[test]
    fn a_rejected_document_names_every_bad_key_without_values() {
        let error = validate_document(serde_json::json!({
            "agent": {"max_turns": "TOPSECRET-TURNS"},
            "compact": {"ratio": "TOPSECRET-RATIO"},
            "power": {"idle_grace_minutes": 9_999}
        }))
        .expect_err("every bad known field must be rejected");

        let message = error.to_string();
        for key in [
            "agent.max_turns",
            "compact.ratio",
            "power.idle_grace_minutes",
        ] {
            assert!(message.contains(key), "{key} missing from {message}");
        }
        for value in ["TOPSECRET-TURNS", "TOPSECRET-RATIO", "9999"] {
            assert!(!message.contains(value), "{value} leaked into {message}");
        }
    }

    /// 采用策略的序列化词汇就是协议词汇；改名或加变体都必须显式改协议。
    ///
    /// 抓的破坏：serde 默认按 Rust 变体名序列化成 `NextSafePoint`，旧客户端
    /// 无法识别，热更新承诺静默失效。
    #[test]
    fn apply_policies_serialize_with_the_protocol_vocabulary() {
        assert_eq!(
            serde_json::to_value(ApplyPolicy::NextSafePoint).unwrap(),
            serde_json::json!("next_safe_point")
        );
        assert_eq!(
            serde_json::to_value(ApplyPolicy::NextSession).unwrap(),
            serde_json::json!("next_session")
        );
    }

    /// 两个各自合法的保活窗口倒挂时整份拒绝，并点名远端窗口。
    ///
    /// 抓的破坏：只做单字段范围校验、漏掉跨字段规则，倒挂配置被接受后宽限
    /// 在电池场景下形同虚设。
    #[test]
    fn an_inverted_power_window_pair_is_rejected() {
        let error = validate_document(serde_json::json!({
            "power": {"idle_grace_minutes": 10, "remote_touch_minutes": 30}
        }))
        .expect_err("remote window longer than grace must be rejected");

        assert!(
            error.to_string().contains("power.remote_touch_minutes"),
            "got {error}"
        );
        assert!(!error.to_string().contains("30"), "got {error}");
        assert!(!error.to_string().contains("10"), "got {error}");
    }

    /// 两个防休眠键冲突时新键胜出；规范化后旧键消失、未知键保留。
    ///
    /// 抓的破坏：迁移方向反了（旧键覆盖新键），或规范化顺手丢掉未知键。
    #[test]
    fn new_power_key_wins_and_unknown_keys_survive() {
        let document = validate_document(serde_json::json!({
            "agent": {"prevent_idle_sleep": "false"},
            "power": {"prevent_idle_sleep": "true"},
            "unknown": {"future": true}
        }))
        .expect("a document with both power keys is valid");

        assert_eq!(
            document.settings.power.prevent_idle_sleep.as_deref(),
            Some("true")
        );
        assert_eq!(document.raw["power"]["prevent_idle_sleep"], "true");
        assert!(
            document.raw.get("agent").is_none(),
            "the legacy key must not survive canonicalisation: {}",
            document.raw
        );
        assert_eq!(document.raw["unknown"], serde_json::json!({"future": true}));
    }
}
