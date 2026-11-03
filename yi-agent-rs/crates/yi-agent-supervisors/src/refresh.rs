//! 启动时向插件重新自述，校准磁盘上的清单。
//!
//! # 为什么需要它
//!
//! 清单是**安装那一刻的快照**：`plugin/install` 跑一次 `<plugin> manifest`，
//! 把结果原样写进 `<项目>/.yi-agent/supervisors/<name>.json`。此后插件升级，
//! 二进制换了、自述变了，磁盘上那份却再没有人刷新——除非用户恰好重新安装一次。
//!
//! 这不是理论问题。清单里的 `stop_when_disabled` 从 `false` 改成 `true` 之后，
//! 旧快照仍让监督器走"开关关了也留着进程"的分支，于是用户关掉开关、进程还在跑，
//! 而且**没有任何界面能说明为什么**——磁盘上的清单是旧结论，代码里是新逻辑，
//! 两者不一致，日志里也看不出来。
//!
//! # 为什么由宿主做，而不是插件自己写
//!
//! 与安装同一条理由：插件在没被拉起之前无法写自己的清单，而清单正是拉起它的
//! 前提。宿主是唯一站在这个循环外面的一方。
//!
//! # 边界
//!
//! 只认清单里已有的 `command`，不认插件名；跑不起来、输出不是清单、名字对不上
//! 都只是**跳过并留一条 warn**，绝不删除或清空用户的清单——一次失败的刷新不该
//! 让插件凭空消失。

use std::path::Path;

use crate::manifest::{SupervisorManifest, load_manifests};

/// 自述执行器：有界双流捕获、单一截止时间、一次性进程组收口。
mod describe;

#[cfg(test)]
#[allow(dead_code)] // Shared fixture: each test crate uses a different subset of fields.
#[path = "refresh/describe/fixture.rs"]
mod fixture;

/// 一次刷新的结果，供调用方决定要不要记日志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 磁盘上那份已经和插件自述一致，没有改动。
    Current(String),
    /// 磁盘上那份过期，已用插件自述覆盖。
    Refreshed(String),
    /// 没刷新，附原因（插件跑不起来 / 输出不是清单 / 名字对不上）。
    Skipped { name: String, reason: String },
}

/// 用每个插件自己的 `<command> manifest` 校准目录下的清单。
///
/// 返回每个**可解析**清单的结果。解析失败的文件不在此列：连 `command` 都读不出来，
/// 也就无从问它是谁（`load_manifests` 已按既有规则 warn 并跳过）。
pub fn refresh_from_plugins(dir: &Path) -> Vec<RefreshOutcome> {
    refresh_from_plugins_until(dir, || false)
}

/// 与 [`refresh_from_plugins`] 相同，但每次可能开始新工作前问一次 `should_stop`。
///
/// 这是「永不取消」入口的带取消版本：停止请求会中断**当前**自述并阻止后续插件
/// 启动，已完成项的结果保留。已经提交的 `rename` 不会被撤销——那是一次原子写，
/// 取消不能假装它没发生。
pub fn refresh_from_plugins_until(
    dir: &Path,
    mut should_stop: impl FnMut() -> bool,
) -> Vec<RefreshOutcome> {
    refresh_records(dir, describe::DescribePolicy::production(), &mut |_| {
        should_stop()
    })
    .into_iter()
    .map(|record| record.outcome)
    .collect()
}

/// 批次里「可以安全停下来」的位置。
///
/// 私有：它不是公开 API，只是把取消检查点做成一个稳定的枚举，好让测试能精确
/// 落在「写回之前」这种闸门上，而不必靠 sleep 猜时机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RefreshPhase {
    BeforeLoad,
    BeforeItem,
    DescribeCheck,
    BeforeCompare,
    BeforeWrite,
    AfterItem,
}

/// 一项刷新的完整结论：公开结果 + 只有内部才知道的分类。
struct RefreshRecord {
    outcome: RefreshOutcome,
    cancelled: bool,
    cleanup_warning: Option<String>,
}

impl RefreshRecord {
    fn plain(outcome: RefreshOutcome) -> Self {
        Self {
            outcome,
            cancelled: false,
            cleanup_warning: None,
        }
    }

    fn cancelled(name: String, reason: String) -> Self {
        Self {
            outcome: RefreshOutcome::Skipped { name, reason },
            cancelled: true,
            cleanup_warning: None,
        }
    }
}

/// 跑完整个批次并保留分类。
fn refresh_records(
    dir: &Path,
    policy: describe::DescribePolicy,
    should_stop: &mut dyn FnMut(RefreshPhase) -> bool,
) -> Vec<RefreshRecord> {
    // 一开始就要求停止：什么都不做，也**不读目录**。返回空结果，而不是伪造一串
    // Skipped——那些插件根本没被考虑过。
    if should_stop(RefreshPhase::BeforeLoad) {
        return Vec::new();
    }
    let mut records = Vec::new();
    for stored in load_manifests(dir) {
        if should_stop(RefreshPhase::BeforeItem) {
            break;
        }
        let record = refresh_one(dir, &stored, policy, should_stop);
        let stop_after = record.cancelled || should_stop(RefreshPhase::AfterItem);
        records.push(record);
        if stop_after {
            break;
        }
    }
    records
}

fn refresh_one(
    dir: &Path,
    stored: &SupervisorManifest,
    policy: describe::DescribePolicy,
    should_stop: &mut dyn FnMut(RefreshPhase) -> bool,
) -> RefreshRecord {
    let name = stored.name.clone();
    let described = match describe::describe(&stored.command, policy, &mut || {
        should_stop(RefreshPhase::DescribeCheck)
    }) {
        Ok(text) => text,
        Err(error) => {
            let cancelled = error.is_cancelled();
            let warning = error.cleanup_warning();
            return RefreshRecord {
                outcome: RefreshOutcome::Skipped {
                    name,
                    reason: error.render(&stored.command),
                },
                cancelled,
                cleanup_warning: warning,
            };
        }
    };
    let fresh = match SupervisorManifest::parse(&described) {
        Ok(manifest) => manifest,
        Err(error) => {
            return RefreshRecord::plain(RefreshOutcome::Skipped {
                name,
                reason: format!(
                    "`{} manifest` printed an unusable manifest: {error}",
                    stored.command
                ),
            });
        }
    };
    if fresh.name != name {
        // 命令自述了**另一个**插件：那是配置错位，不是本文件的更新来源。
        // 拿它覆盖会把 A 的清单写成 B，比不刷新危险得多。
        let reason = format!(
            "`{} manifest` describes {:?}, not {name:?}",
            stored.command, fresh.name
        );
        return RefreshRecord::plain(RefreshOutcome::Skipped { name, reason });
    }
    if should_stop(RefreshPhase::BeforeCompare) {
        return RefreshRecord::cancelled(name, cancelled_reason(&stored.command));
    }
    if fresh == *stored {
        return RefreshRecord::plain(RefreshOutcome::Current(name));
    }
    // 写回闸门：检查之后已经进入的那一次 `rename` 允许完成，取消不撤销它。
    if should_stop(RefreshPhase::BeforeWrite) {
        return RefreshRecord::cancelled(name, cancelled_reason(&stored.command));
    }
    match write_atomic(dir, &format!("{name}.json"), &described) {
        Ok(()) => RefreshRecord::plain(RefreshOutcome::Refreshed(name)),
        Err(error) => RefreshRecord::plain(RefreshOutcome::Skipped {
            name,
            reason: format!("could not rewrite the manifest: {error}"),
        }),
    }
}

/// 批次在写回前被取消时的公开原因（与执行器的 `render` 保持同一措辞）。
fn cancelled_reason(command: &str) -> String {
    describe::DescribeError::Cancelled.render(command)
}

/// 同目录 `tmp` + `rename`：截断的清单会被 supervisor 当成坏清单**静默跳过**，
/// 那等于插件凭空消失。
fn write_atomic(dir: &Path, file: &str, contents: &str) -> std::io::Result<()> {
    let tmp = dir.join(format!(".{file}.refresh.tmp"));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, dir.join(file)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// 刷新目录下的清单，并把结果记进日志。返回被真正改写的插件名。
///
/// 只给调用点用：它要把「什么都没变」与「改了什么」区分开，前者不该刷日志。
pub fn refresh_and_log(dir: &Path) -> Vec<String> {
    refresh_and_log_until(dir, || false)
}

/// [`refresh_and_log`] 的带取消版本。取消走 debug，正常停机不该被报成插件故障。
pub fn refresh_and_log_until(dir: &Path, mut should_stop: impl FnMut() -> bool) -> Vec<String> {
    let records = refresh_records(dir, describe::DescribePolicy::production(), &mut |_| {
        should_stop()
    });
    log_records(records)
}

/// 把一批结论投影成日志，并返回真正被改写的插件名。
///
/// 级别由**私有分类**决定，绝不匹配 reason 子串：插件往 stderr 写一句
/// `cancelled` 不能把一条故障降级成 debug。
fn log_records(records: Vec<RefreshRecord>) -> Vec<String> {
    let mut refreshed = Vec::new();
    for record in records {
        // 清理失败单独告警：主因是正常取消时也不能把它吞掉。
        if let Some(warning) = record.cleanup_warning {
            let plugin = outcome_name(&record.outcome);
            tracing::warn!(
                plugin = %plugin,
                warning = %warning,
                "plugin manifest description cleanup failed"
            );
        }
        match record.outcome {
            RefreshOutcome::Refreshed(name) => {
                tracing::info!(plugin = %name, "refreshed a stale plugin manifest");
                refreshed.push(name);
            }
            RefreshOutcome::Skipped { name, reason } => {
                if record.cancelled {
                    tracing::debug!(
                        plugin = %name,
                        %reason,
                        "stopped refreshing plugin manifests"
                    );
                } else {
                    // 不删除、不覆盖：一次失败的刷新不该让插件消失。
                    tracing::warn!(
                        plugin = %name,
                        %reason,
                        "could not refresh a plugin manifest"
                    );
                }
            }
            RefreshOutcome::Current(_) => {}
        }
    }
    refreshed
}

fn outcome_name(outcome: &RefreshOutcome) -> &str {
    match outcome {
        RefreshOutcome::Current(name)
        | RefreshOutcome::Refreshed(name)
        | RefreshOutcome::Skipped { name, .. } => name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const STALE: &str =
        r#"{"name":"demo","command":"CMD","switch_key":"demo_on","stop_when_disabled":false}"#;
    const CURRENT: &str =
        r#"{"name":"demo","command":"CMD","switch_key":"demo_on","stop_when_disabled":true}"#;

    /// 造一个"插件"：它自述的内容由 `manifest_body` 决定。
    fn plugin(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("plugin.sh");
        std::fs::write(&script, format!("#!/bin/sh\ncat <<'JSON'\n{body}\nJSON\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    fn write_manifest(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("demo.json"), body).unwrap();
    }

    /// 现场回归：插件升级后自述变了，磁盘上那份仍是旧结论。启动时必须校准，
    /// 否则 `stop_when_disabled:false` 会让"关掉开关"永远停不掉进程。
    #[test]
    fn a_stale_manifest_is_replaced_by_the_plugins_own_description() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let exe = plugin(
            dir.path(),
            &CURRENT.replace("CMD", &dir.path().join("plugin.sh").display().to_string()),
        );
        write_manifest(
            &manifests,
            &STALE.replace("CMD", &exe.display().to_string()),
        );

        let outcomes = refresh_from_plugins(&manifests);

        assert_eq!(outcomes, vec![RefreshOutcome::Refreshed("demo".into())]);
        let text = std::fs::read_to_string(manifests.join("demo.json")).unwrap();
        // 断言语义而不是字节格式：插件怎么排空格是它自己的事，这里要钉住的
        // 是"新自述赢了"——即 `stop_when_disabled` 从 false 变成 true。
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            parsed["stop_when_disabled"], true,
            "the fresh self-description must win: {text}"
        );
    }

    /// 已经一致就不写：避免每次启动都改文件时间戳（有些用户会盯这个）。
    #[test]
    fn a_current_manifest_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let exe = plugin(
            dir.path(),
            &CURRENT.replace("CMD", &dir.path().join("plugin.sh").display().to_string()),
        );
        write_manifest(
            &manifests,
            &CURRENT.replace("CMD", &exe.display().to_string()),
        );

        let outcomes = refresh_from_plugins(&manifests);

        assert_eq!(outcomes, vec![RefreshOutcome::Current("demo".into())]);
    }

    /// 插件跑不起来时**保留**原清单：一次失败的刷新不该让插件消失。
    #[test]
    fn an_unrunnable_plugin_keeps_the_stored_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        write_manifest(&manifests, STALE);

        let outcomes = refresh_from_plugins(&manifests);

        assert!(matches!(
            outcomes.as_slice(),
            [RefreshOutcome::Skipped { name, .. }] if name == "demo"
        ));
        assert_eq!(
            std::fs::read_to_string(manifests.join("demo.json")).unwrap(),
            STALE,
            "a failed refresh must not erase or blank the manifest"
        );
    }

    /// 自述了另一个插件时拒绝覆盖：那会把 A 的清单写成 B。
    #[test]
    fn a_description_for_another_plugin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let body = r#"{"name":"other","command":"CMD","switch_key":"other_on"}"#;
        let exe = plugin(
            dir.path(),
            &body.replace("CMD", &dir.path().join("plugin.sh").display().to_string()),
        );
        let stale = STALE.replace("CMD", &exe.display().to_string());
        write_manifest(&manifests, &stale);

        let outcomes = refresh_from_plugins(&manifests);

        assert!(matches!(
            outcomes.as_slice(),
            [RefreshOutcome::Skipped { name, .. }] if name == "demo"
        ));
        assert_eq!(
            std::fs::read_to_string(manifests.join("demo.json")).unwrap(),
            stale
        );
    }

    /// 输出不是清单时同样保留原文件。
    #[test]
    fn garbage_output_keeps_the_stored_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let exe = plugin(dir.path(), "not a manifest at all");
        let stale = STALE.replace("CMD", &exe.display().to_string());
        write_manifest(&manifests, &stale);

        let outcomes = refresh_from_plugins(&manifests);

        assert!(matches!(
            outcomes.as_slice(),
            [RefreshOutcome::Skipped { .. }]
        ));
        assert_eq!(
            std::fs::read_to_string(manifests.join("demo.json")).unwrap(),
            stale
        );
    }

    /// 刷新是原子的：目录里不能留下临时文件。
    #[test]
    fn refreshing_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let manifests = dir.path().join("manifests");
        let exe = plugin(
            dir.path(),
            &CURRENT.replace("CMD", &dir.path().join("plugin.sh").display().to_string()),
        );
        write_manifest(
            &manifests,
            &STALE.replace("CMD", &exe.display().to_string()),
        );

        refresh_from_plugins(&manifests);

        let names: Vec<_> = std::fs::read_dir(&manifests)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["demo.json".to_string()]);
    }

    // -----------------------------------------------------------------------
    // 取消批次（Task 2）
    // -----------------------------------------------------------------------

    use crate::refresh::fixture::PluginFixture;

    /// 一个插件正在自述时要求停止：它被取消、原文件不变、**下一个插件不启动**。
    #[test]
    fn cancelled_refresh_does_not_start_the_next_plugin() {
        let fixture = PluginFixture::new();
        let a = fixture.plugin("a", "hang", 0, 0);
        let b = fixture.plugin("b", "small", 0, 0);
        let before = std::fs::read(&a.stored).unwrap();
        let result =
            refresh_from_plugins_until(&fixture.workdir().join(".yi-agent/supervisors"), || {
                a.manifest_marker.exists()
            });
        assert_eq!(
            result.len(),
            1,
            "nothing may be reported for a plugin that never started"
        );
        assert!(
            matches!(&result[0], RefreshOutcome::Skipped { name, reason }
            if name == "a" && reason.contains("cancelled"))
        );
        assert_eq!(std::fs::read(&a.stored).unwrap(), before);
        assert!(
            !b.manifest_marker.exists(),
            "b must not be started after a stop"
        );
    }

    /// 已经成功刷新的项不回滚，停止只影响之后的项。
    #[test]
    fn a_cancelled_batch_keeps_earlier_successes() {
        let fixture = PluginFixture::new();
        let done = fixture.plugin("0_done", "small", 0, 0);
        let a = fixture.plugin("a", "hang", 0, 0);
        let b = fixture.plugin("b", "small", 0, 0);
        let outcomes =
            refresh_from_plugins_until(&fixture.workdir().join(".yi-agent/supervisors"), || {
                a.manifest_marker.exists()
            });
        let done_after = std::fs::read_to_string(&done.stored).unwrap();
        let parsed = SupervisorManifest::parse(&done_after).unwrap();
        assert!(
            parsed.stop_when_disabled,
            "the earlier refresh must be kept"
        );
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, RefreshOutcome::Refreshed(name) if name == "0_done")),
            "earlier successes are reported: {outcomes:?}"
        );
        assert!(
            outcomes
                .iter()
                .any(|o| matches!(o, RefreshOutcome::Skipped { name, .. } if name == "a")),
            "{outcomes:?}"
        );
        assert!(!b.manifest_marker.exists());
    }

    /// **一次性**停止请求也必须中止整个批次。
    ///
    /// 这条用例让「已取消就 break」本身成为可检出的逻辑：停止谓词只返回一次
    /// true（之后恢复 false），因此 `BeforeItem` 那道闸门不会再挡住 b——只有
    /// 批次的 break 能让 b 不启动。
    #[test]
    fn a_one_shot_stop_still_ends_the_batch() {
        let fixture = PluginFixture::new();
        let a = fixture.plugin("a", "hang", 0, 0);
        let b = fixture.plugin("b", "small", 0, 0);
        let mut fired = false;
        let records = refresh_records(
            &fixture.workdir().join(".yi-agent/supervisors"),
            describe::DescribePolicy::production(),
            &mut |phase| {
                if phase == RefreshPhase::DescribeCheck && !fired && a.manifest_marker.exists() {
                    fired = true;
                    return true;
                }
                false
            },
        );
        assert!(fired, "the stop request must have fired mid-description");
        assert!(matches!(
            records.as_slice(),
            [record] if record.cancelled
                && matches!(&record.outcome, RefreshOutcome::Skipped { name, .. } if name == "a")
        ));
        assert!(
            !b.manifest_marker.exists(),
            "a cancelled batch must not start another plugin, even if the stop flag clears"
        );
    }

    /// 一开始就取消：空结果，一个 marker 都不出现。
    #[test]
    fn cancelling_before_the_batch_starts_touches_nothing() {
        let fixture = PluginFixture::new();
        let a = fixture.plugin("a", "small", 0, 0);
        let b = fixture.plugin("b", "small", 0, 0);
        let before_a = std::fs::read(&a.stored).unwrap();
        let before_b = std::fs::read(&b.stored).unwrap();
        let result =
            refresh_from_plugins_until(&fixture.workdir().join(".yi-agent/supervisors"), || true);
        assert!(result.is_empty());
        assert!(!a.manifest_marker.exists());
        assert!(!b.manifest_marker.exists());
        assert_eq!(std::fs::read(&a.stored).unwrap(), before_a);
        assert_eq!(std::fs::read(&b.stored).unwrap(), before_b);
    }

    /// 写回闸门：检查点取消后不得落盘。
    #[test]
    fn cancellation_at_the_write_gate_preserves_stored_bytes() {
        let fixture = PluginFixture::new();
        let a = fixture.plugin("a", "small", 0, 0);
        let before = std::fs::read(&a.stored).unwrap();
        let mut reached_write = false;
        let records = refresh_records(
            &fixture.workdir().join(".yi-agent/supervisors"),
            describe::DescribePolicy::production(),
            &mut |phase| {
                if phase == RefreshPhase::BeforeWrite {
                    reached_write = true;
                    true
                } else {
                    false
                }
            },
        );
        assert!(reached_write, "the batch must reach the write gate");
        assert_eq!(records.len(), 1);
        assert!(records[0].cancelled);
        assert!(matches!(&records[0].outcome, RefreshOutcome::Skipped { name, .. } if name == "a"));
        assert_eq!(std::fs::read(&a.stored).unwrap(), before);
    }

    /// 日志级别由分类决定：插件 stderr 里写着 `cancelled` 也只能是 warn。
    #[test]
    fn plugin_stderr_cannot_forge_the_log_level() {
        let fixture = PluginFixture::new();
        let _a = fixture.plugin("a", "nonzero", 0, 3);
        let levels = Levels::default();
        let subscriber = levels.clone();
        tracing::subscriber::with_default(subscriber, || {
            let refreshed =
                refresh_and_log_until(&fixture.workdir().join(".yi-agent/supervisors"), || false);
            assert!(
                refreshed.is_empty(),
                "a nonzero exit must not report a rewrite"
            );
        });
        let recorded = levels.take();
        assert!(
            recorded.contains(&tracing::Level::WARN),
            "a failed description is a warning: {recorded:?}"
        );
        assert!(
            !recorded.contains(&tracing::Level::DEBUG),
            "plugin text must not demote the event to debug: {recorded:?}"
        );
    }

    /// 真正的取消只有 debug（没有清理异常时不额外 warn）。
    #[test]
    fn a_real_cancellation_is_logged_at_debug_only() {
        let fixture = PluginFixture::new();
        let a = fixture.plugin("a", "hang", 0, 0);
        let levels = Levels::default();
        let subscriber = levels.clone();
        tracing::subscriber::with_default(subscriber, || {
            refresh_and_log_until(&fixture.workdir().join(".yi-agent/supervisors"), || {
                a.manifest_marker.exists()
            });
        });
        let recorded = levels.take();
        assert!(
            recorded.contains(&tracing::Level::DEBUG),
            "a stop is a debug event: {recorded:?}"
        );
        assert!(
            !recorded.contains(&tracing::Level::WARN),
            "a clean stop must not warn: {recorded:?}"
        );
    }

    /// 只记录级别的最小 subscriber：不引入 tracing-subscriber 依赖。
    #[derive(Clone, Default)]
    struct Levels(std::sync::Arc<std::sync::Mutex<Vec<tracing::Level>>>);

    impl Levels {
        fn take(&self) -> Vec<tracing::Level> {
            let mut guard = self.0.lock().unwrap();
            std::mem::take(&mut *guard)
        }
    }

    impl tracing::Subscriber for Levels {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            self.0.lock().unwrap().push(*event.metadata().level());
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }
}
