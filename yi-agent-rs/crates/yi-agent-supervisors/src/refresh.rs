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

use std::path::{Path, PathBuf};

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
    load_manifests(dir)
        .into_iter()
        .map(|stored| refresh_one(dir, &stored))
        .collect()
}

fn refresh_one(dir: &Path, stored: &SupervisorManifest) -> RefreshOutcome {
    let name = stored.name.clone();
    let described = match describe(&stored.command) {
        Ok(text) => text,
        Err(reason) => {
            return RefreshOutcome::Skipped {
                name: name.clone(),
                reason,
            };
        }
    };
    let fresh = match SupervisorManifest::parse(&described) {
        Ok(manifest) => manifest,
        Err(error) => {
            return RefreshOutcome::Skipped {
                name: name.clone(),
                reason: format!(
                    "`{} manifest` printed an unusable manifest: {error}",
                    stored.command
                ),
            };
        }
    };
    if fresh.name != name {
        // 命令自述了**另一个**插件：那是配置错位，不是本文件的更新来源。
        // 拿它覆盖会把 A 的清单写成 B，比不刷新危险得多。
        return RefreshOutcome::Skipped {
            name: name.clone(),
            reason: format!(
                "`{} manifest` describes {:?}, not {name:?}",
                stored.command, fresh.name
            ),
        };
    }
    if fresh == *stored {
        return RefreshOutcome::Current(name);
    }
    match write_atomic(dir, &format!("{name}.json"), &described) {
        Ok(()) => RefreshOutcome::Refreshed(name),
        Err(error) => RefreshOutcome::Skipped {
            name,
            reason: format!("could not rewrite the manifest: {error}"),
        },
    }
}

/// 跑 `<command> manifest`，超时就杀掉。
///
/// 历史：这里曾是 `try_wait()` 轮询 + `wait_with_output()`，不读管道也把读取
/// 放在 deadline 之外，于是大输出被误判超时、后代持写端时读取无界。现在实际
/// 执行在私有模块 [`describe`]，预算见 [`DescribePolicy::production`]。
fn describe(command: &str) -> Result<String, String> {
    describe::describe(command, describe::DescribePolicy::production(), &mut || {
        false
    })
    .map_err(|error| error.render(command))
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
    refresh_from_plugins(dir)
        .into_iter()
        .filter_map(|outcome| match outcome {
            RefreshOutcome::Refreshed(name) => {
                tracing::info!(plugin = %name, "refreshed a stale plugin manifest");
                Some(name)
            }
            RefreshOutcome::Skipped { name, reason } => {
                // 不删除、不覆盖：一次失败的刷新不该让插件消失。
                tracing::warn!(plugin = %name, %reason, "could not refresh a plugin manifest");
                None
            }
            RefreshOutcome::Current(_) => None,
        })
        .collect()
}

/// 目录本身：与 `install::manifests_dir` 同一落点。
pub fn manifests_dir(project: &Path) -> PathBuf {
    crate::install::manifests_dir(project)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
