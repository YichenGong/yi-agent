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
pub fn install_manifest_for(
    project: &Path,
    plugin_name: &str,
    manifest_json: &str,
) -> std::io::Result<PathBuf> {
    if !is_valid_plugin_name(plugin_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("`{plugin_name}` is not a usable plugin name"),
        ));
    }
    let dir = manifests_dir(project);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{plugin_name}.json"));
    write_atomic(&dir, &format!("{plugin_name}.json"), manifest_json)?;
    Ok(path)
}

/// 删掉一个已安装的清单；不存在也算成功。
///
/// 给「安装进行到一半失败」用：清单已落盘、开关却没打开时必须撤回，否则留下
/// 的正是「注册了、被拉起了、永不推进」的半安装态（spec §4.4.1 要防的病）。
pub fn remove_manifest(project: &Path, plugin_name: &str) -> std::io::Result<()> {
    if !is_valid_plugin_name(plugin_name) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("`{plugin_name}` is not a usable plugin name"),
        ));
    }
    match std::fs::remove_file(manifests_dir(project).join(format!("{plugin_name}.json"))) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 进程内递增序号，给每次写入的临时文件一个不同的后缀。
///
/// 与 `app-server` 的 `settings_store` / `thread_store` 同一约定：固定临时名
/// 会让同一目录的**并发写者**互相截断（A 写 tmp、B 用 `fs::write` 把同一个 tmp
/// 截断、A 把它 rename 成目标文件——最终文件就是半份内容）。
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `dir` 下逐次唯一的临时文件名：`<file>.<pid>.<seq>.tmp`。
fn temp_path(dir: &Path, file: &str, seq: u64) -> PathBuf {
    dir.join(format!("{file}.{}.{}.tmp", std::process::id(), seq))
}

/// 同目录 `tmp` + `rename` 的原子写。
fn write_atomic(dir: &Path, file: &str, contents: &str) -> std::io::Result<()> {
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = temp_path(dir, file, seq);
    if let Err(error) = std::fs::write(&tmp_path, contents) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&tmp_path, dir.join(file)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(error);
    }
    Ok(())
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
        let path = install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();

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
        install_manifest_for(dir.path(), "demo", second).unwrap();

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

    /// 临时名必须逐次唯一。
    ///
    /// `preferences.json` 是**多写者共享**的（运行时偏好、各插件开关）：固定名
    /// 会让并发写者互相截断——A 写 tmp，B 用 `fs::write` 把同一个 tmp 截断，A 再
    /// 把它 rename 成目标文件，最终文件就是半份内容，所有键一起坏掉。
    #[test]
    fn each_write_uses_a_unique_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = temp_path(dir.path(), "preferences.json", 0);
        let second = temp_path(dir.path(), "preferences.json", 1);
        assert_ne!(first, second);
        assert!(
            first
                .to_string_lossy()
                .contains(&std::process::id().to_string())
        );
    }

    /// 清单写入是原子的：覆盖写不会留下截断的旧文件。
    ///
    /// 截断的清单会被 supervisor 当成坏清单静默跳过，那等于插件凭空消失。
    #[test]
    fn reinstalling_replaces_the_manifest_atomically() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        let updated = r#"{"name":"demo","command":"/bin/false","switch_key":"demo_on"}"#;
        let path = install_manifest_for(dir.path(), "demo", updated).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        // 覆盖写走 tmp + rename，故目录里只剩最终文件。
        let names: Vec<_> = std::fs::read_dir(dir.path().join(".yi-agent").join("supervisors"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["demo.json".to_string()]);
    }

    /// 回滚只删清单，不动项目里的其它东西。
    #[test]
    fn removing_a_manifest_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        install_manifest_for(dir.path(), "demo", MANIFEST).unwrap();
        remove_manifest(dir.path(), "demo").unwrap();
        assert!(
            !dir.path()
                .join(".yi-agent")
                .join("supervisors")
                .join("demo.json")
                .exists()
        );
        // 再删一次不算错：回滚可能被重试。
        remove_manifest(dir.path(), "demo").unwrap();
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
