//! 把一个插件装进一个项目：写它的清单、打开它的开关。
//!
//! 本模块**不认识任何插件**：清单内容由调用方（最终来自插件自己的
//! `<plugin> manifest` 自述）原样给出，这里只负责落盘与开开关。
//!
//! # 为什么这是宿主的能力，而不是插件的能力
//!
//! 插件要被调起来，前提是清单已在
//! `<项目>/.yi-agent/supervisors/<name>.json`；而「调起来」正是它写清单的
//! 前提。这个循环必须由宿主从外面打破（spec §4.4 方案 A）。
//!
//! # 为什么还要开开关
//!
//! 清单只让 supervisor 拉起进程；**要不要干活**由清单声明的 `switch_key`
//! 决定，而它默认是关的。只装清单等于「进程起来了但队列永不推进」——
//! 用户看到的是一个死看板。因此安装 = 写清单 + 开开关，两件事不可分
//! （spec §4.4.1）。

use std::path::{Path, PathBuf};

/// 清单目录：`<project>/.yi-agent/supervisors`。
///
/// 与 [`crate::supervisor::Layout::manifests_dir`] 同一个落点，但这里不依赖
/// `Layout`：安装是纯路径操作，不需要一套监督器状态。
pub fn manifests_dir(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("supervisors")
}

/// 项目级偏好文件：`<project>/.yi-agent/preferences.json`。
///
/// 与 `Layout::project_preferences_path()` 以及插件侧读开关的同一路径约定
/// 必须一致：宿主写、双方读，路径不一致就会出现「开关开了但没人看见」。
pub fn project_preferences_path(project: &Path) -> PathBuf {
    project.join(".yi-agent").join("preferences.json")
}

/// 插件名是否可以安全地当作文件名。
///
/// 名字来自请求参数，而它会被拼进 `<project>/.yi-agent/supervisors/<name>.json`。
/// 一个含路径分隔符或 `..` 的名字（`../../etc/passwd`）会写到项目之外，
/// 故在这里就拒绝，而不是靠调用方自觉。
///
/// 规则刻意保守：只接受字母、数字、`.`、`_`、`-`，且不得以 `.` 开头。
/// 常见的插件名（小写 + 连字符）全部满足；更花哨的名字若真需要，应显式放宽
/// 而不是默默接受一个能穿越目录的字符串。
pub fn is_valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// 把插件自述的清单写到 `<project>/.yi-agent/supervisors/<plugin_name>.json`。
///
/// 文件名来自 `plugin_name` 参数，**不来自清单内容**：清单是插件自述的，
/// 若拿它决定落点，一个自述 `name: "../x"` 的插件就能写到目录外。清单里的
/// `name` 与请求名是否一致由调用方校验（那是策略，不是本函数的职责）。
///
/// 覆盖写：重复安装是幂等的，且以**新**内容为准——重装会刷新磁盘上的清单，
/// 但**不会**让正在跑的进程立刻换用新内容：supervisor 见 socket 仍有应答就
/// 直接返回，新清单要等该进程退出或失联、被重新拉起时才生效。
///
/// 写入是原子的（同目录 `tmp` + `rename`）：截断的清单会被 supervisor 当成
/// 坏清单**静默跳过**（只记一条 warn），那等于插件凭空消失，比安装失败更难查。
///
/// 返回的 [`ManifestSnapshot`] 记录安装**之前**那份清单的样子（或「本来没有」），
/// 供调用方在后续步骤失败时恢复——见 [`ManifestSnapshot::restore`]。
pub fn install_manifest_for(
    project: &Path,
    plugin_name: &str,
    manifest_json: &str,
) -> std::io::Result<(PathBuf, ManifestSnapshot)> {
    if !is_valid_plugin_name(plugin_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("`{plugin_name}` is not a usable plugin name"),
        ));
    }
    let dir = manifests_dir(project);
    std::fs::create_dir_all(&dir)?;
    let file = format!("{plugin_name}.json");
    let path = dir.join(&file);
    let previous = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    write_atomic(&dir, &file, manifest_json)?;
    Ok((path.clone(), ManifestSnapshot { path, previous }))
}

/// 安装前的清单快照，用来在安装后半程失败时**恢复原状**。
///
/// 为什么不是「失败就删清单」：重装一个**本来就能用**的插件时，若开关这一步恰好
/// 写不进去（磁盘满、权限、并发占位），删掉清单等于把一次失败的升级变成一次
/// 卸载——比原来那个半安装态更糟。只有原本没有清单时才应该删。
#[derive(Debug)]
pub struct ManifestSnapshot {
    path: PathBuf,
    previous: Option<Vec<u8>>,
}

impl ManifestSnapshot {
    /// 撤回这次安装：原本有清单就写回原字节，原本没有就删掉。
    ///
    /// 恢复也走 `create_new` + `rename`，故不会与并发写者互相截断。
    pub fn restore(self) -> std::io::Result<()> {
        match self.previous {
            Some(bytes) => {
                let dir = self.path.parent().unwrap_or(Path::new("."));
                let file = self
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match std::str::from_utf8(&bytes) {
                    Ok(text) => write_atomic(dir, &file, text),
                    // 原清单不是 UTF-8：按字节写回，别做任何转码。
                    Err(_) => {
                        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let tmp = dir.join(format!(
                            "{file}.{TEMP_WRITER}.{}.{seq}.tmp",
                            std::process::id()
                        ));
                        std::fs::write(&tmp, &bytes)?;
                        std::fs::rename(&tmp, &self.path)
                    }
                }
            }
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        }
    }
}

/// 本模块在临时文件名里留的写者标记。
///
/// 不能只靠进程内计数器：`settings_store` 也写同一个 `preferences.json`，而它
/// 自己有一个从 0 开始的计数器。两边格式若都是 `<file>.<pid>.<seq>.tmp`，同一
/// 进程里两个模块的**第一次**写入会算出**同一个路径**——那正是固定临时名的毛病，
/// 只是换了个样子。写者标记把两边分开。
const TEMP_WRITER: &str = "install";

/// 进程内递增序号，用来在写者标记之后再加一层区分。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 同目录 `tmp` + `rename` 的原子写。
///
/// 临时文件用 `create_new` 取得：唯一性由文件系统裁决，而不是由进程内计数器
/// 推断。即使另一个模块（或另一个进程）碰巧算出同样的名字，也只会得到
/// `AlreadyExists` 并换下一个序号，而不会把别人正在写的文件截断。
fn write_atomic(dir: &Path, file: &str, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let mut attempt = 0_u32;
    loop {
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = dir.join(format!(
            "{file}.{TEMP_WRITER}.{}.{seq}.tmp",
            std::process::id()
        ));
        let mut handle = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
        {
            Ok(handle) => handle,
            // 名字被占用：换下一个序号。真撞上几十次说明目录里有异常堆积，
            // 那时失败比无限重试好。
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempt < 32 => {
                attempt += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = handle
            .write_all(contents.as_bytes())
            .and_then(|()| handle.sync_all())
        {
            drop(handle);
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }
        drop(handle);
        if let Err(error) = std::fs::rename(&tmp_path, dir.join(file)) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }
        return Ok(());
    }
}

/// 把 `switch_key` 置为 `true`，保留 `preferences.json` 里的其它键。
///
/// read-modify-write + 同级 `tmp` + `rename`，与 `runtime_prefs::save` 同型：
/// 这个文件是**多写者共享**的（运行时偏好、各插件的开关），整份覆盖会静默
/// 抹掉别人的键。缺失或损坏的文件当作空对象——一个坏文件不该让安装失败，
/// 也不该让别人的键陪葬。
pub fn enable_switch(project: &Path, switch_key: &str) -> std::io::Result<()> {
    if switch_key.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a manifest without a switch_key cannot be enabled",
        ));
    }
    let dir = project.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let path = project_preferences_path(project);
    let mut object = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        Err(_) => serde_json::Map::new(),
    };
    object.insert(switch_key.to_string(), serde_json::Value::Bool(true));
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    // 临时名逐次唯一：`preferences.json` 是多写者共享文件（运行时偏好、各插件
    // 开关），固定名会让并发写者互相截断，最终文件可能整份坏掉。
    write_atomic(&dir, "preferences.json", &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"{"name":"demo","command":"/bin/true","switch_key":"demo_on"}"#;

    #[test]
    fn installing_writes_the_manifest_under_the_projects_supervisors_dir() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();

        assert_eq!(
            path,
            dir.path()
                .join(".yi-agent")
                .join("supervisors")
                .join("demo.json")
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), MANIFEST);
    }

    #[test]
    fn installing_twice_overwrites_instead_of_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        let second = r#"{"name":"demo","command":"/bin/false","switch_key":"demo_on"}"#;
        let _ = install_manifest_for(dir.path(), "demo", second).unwrap();

        let entries: Vec<_> = std::fs::read_dir(manifests_dir(dir.path()))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("demo.json")]);
        assert_eq!(
            std::fs::read_to_string(manifests_dir(dir.path()).join("demo.json")).unwrap(),
            second
        );
    }

    #[test]
    fn enabling_the_switch_preserves_unrelated_keys() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = project_preferences_path(dir.path());
        std::fs::create_dir_all(preferences.parent().unwrap()).unwrap();
        std::fs::write(&preferences, r#"{"subagent_runtime":"x"}"#).unwrap();

        enable_switch(dir.path(), "demo_on").unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&preferences).unwrap()).unwrap();
        assert_eq!(value["subagent_runtime"], "x");
        assert_eq!(value["demo_on"], true);
    }

    #[test]
    fn enabling_the_switch_creates_the_file_when_it_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        enable_switch(dir.path(), "demo_on").unwrap();

        let value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(project_preferences_path(dir.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(value["demo_on"], true);
    }

    /// 损坏的偏好文件不该让安装失败，也不该让「别人的键」陪葬——但坏内容
    /// 本身已无法保留，只能当作空对象重写。
    #[test]
    fn a_corrupt_preferences_file_does_not_stop_the_switch() {
        let dir = tempfile::tempdir().unwrap();
        let preferences = project_preferences_path(dir.path());
        std::fs::create_dir_all(preferences.parent().unwrap()).unwrap();
        std::fs::write(&preferences, "not json").unwrap();

        enable_switch(dir.path(), "demo_on").unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&preferences).unwrap()).unwrap();
        assert_eq!(value["demo_on"], true);
    }

    #[test]
    fn enabling_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        enable_switch(dir.path(), "demo_on").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".yi-agent"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// 临时名不得与 `settings_store` 的写法重合。
    ///
    /// `preferences.json` 是**多写者共享**的（运行时偏好、各插件开关），而
    /// `settings_store` 也有一个从 0 开始的计数器。若两边格式相同，同一进程里
    /// 两个模块的第一次写入会算出同一个临时路径，互相截断。写者标记保证两边
    /// 永远落在不同文件上。
    #[test]
    fn the_temp_name_cannot_collide_with_another_writer() {
        let dir = tempfile::tempdir().unwrap();
        write_atomic(dir.path(), "preferences.json", "{}").unwrap();
        assert_eq!(TEMP_WRITER, "install");
        // 另一个写者（同进程、同序号、同格式）会算出这个路径；两者必须不同。
        let other = dir
            .path()
            .join(format!("preferences.json.{}.{}.tmp", std::process::id(), 0));
        let ours = dir.path().join(format!(
            "preferences.json.{TEMP_WRITER}.{}.{}.tmp",
            std::process::id(),
            0
        ));
        assert_ne!(ours, other);
    }

    /// 并发写同一个 preferences 文件不会互相截断。
    #[test]
    fn concurrent_switch_writes_leave_a_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let handles: Vec<_> = (0..8)
            .map(|index| {
                let root = root.clone();
                std::thread::spawn(move || {
                    enable_switch(&root, &format!("switch_{index}")).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let text = std::fs::read_to_string(project_preferences_path(&root)).unwrap();
        // 每个写者都做 read-modify-write，具体活下来几个键取决于写入顺序；
        // 关键是文件必须始终是**完整**的 JSON，绝不能是半份。
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(!value.as_object().unwrap().is_empty());
    }

    /// 清单写入是原子的：覆盖写不会留下截断的旧文件。
    ///
    /// 截断的清单会被 supervisor 当成坏清单静默跳过，那等于插件凭空消失。
    #[test]
    fn reinstalling_replaces_the_manifest_atomically() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        let updated = r#"{"name":"demo","command":"/bin/false","switch_key":"demo_on"}"#;
        let (path, _) = install_manifest_for(dir.path(), "demo", updated).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        // 覆盖写走 tmp + rename，故目录里只剩最终文件。
        let names: Vec<_> = std::fs::read_dir(dir.path().join(".yi-agent").join("supervisors"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["demo.json".to_string()]);
    }

    /// 回滚：原本没有清单时删掉。
    #[test]
    fn a_snapshot_of_nothing_restores_to_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (_, snapshot) = install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        snapshot.restore().unwrap();
        assert!(!manifests_dir(dir.path()).join("demo.json").exists());
    }

    /// 回滚：原本有清单时写回**原字节**，而不是删掉。
    ///
    /// 重装一个本来能用的插件、开关那一步却失败时，删清单等于把失败的升级变成
    /// 卸载——比半安装态更糟。
    #[test]
    fn a_snapshot_of_a_previous_manifest_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let _ = install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        let (_, snapshot) = install_manifest_for(
            dir.path(),
            "demo",
            r#"{"name":"demo","command":"/bin/false","switch_key":"demo_on"}"#,
        )
        .unwrap();
        snapshot.restore().unwrap();
        assert_eq!(
            std::fs::read_to_string(manifests_dir(dir.path()).join("demo.json")).unwrap(),
            MANIFEST
        );
    }

    /// 名字会被拼进路径，故含分隔符或 `..` 的名字必须在落盘前拒绝。
    #[test]
    fn a_name_that_could_escape_the_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["../escape", "a/b", "..", ".hidden", ""] {
            let error = install_manifest_for(dir.path(), name, MANIFEST).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name}");
        }
        // 什么都没写出去。
        assert!(!dir.path().join(".yi-agent").exists());
    }

    #[test]
    fn an_empty_switch_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = enable_switch(dir.path(), "").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}
