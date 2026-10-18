//! 通用「常驻 daemon」登记——**宿主与插件共享的文件契约**。
//!
//! 登记文件是 `$HOME/.yi-agent/resident-daemons.json`，回答的是「哪些项目需要
//! 一个常驻 daemon、由谁请求」。格式与宿主的 `yi_agent_store::resident` 逐字段
//! 一致（`{"projects":[{"project":…,"required_by":[…]}]}`），枚举它的是宿主的
//! 值守者（watchman 与 app 内循环）。
//!
//! **为什么插件自带一份读写而不是依赖宿主的 crate**：插件对主程序保持
//! **零依赖**（Spec 4 §2「插件对主程序：零依赖（已达标）」）。这个文件是**两侧
//! 共享的通用契约**——契约是 JSON 形状，不是代码；各自实现、各自测，改动
//! 两边都要跟。谁改这个形状，谁就要同时改
//! `yi-agent-rs/crates/yi-agent-store/src/resident.rs` 与这里的测试。
//!
//! 写侧原子（tmp + rename），读侧**宽容**：缺文件当空表；**损坏不覆盖**——
//! 与宿主同一口径，否则一份读到一半的坏文件会被下一次写入抹掉，登记就此
//! 消失。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 宿主通用目录：`$HOME/.yi-agent`。`HOME` 缺失返回 `None`。
///
/// 与 `yi_agent_store::resident::default_dir` 同义：这个目录是所有项目共享的
/// 宿主级状态目录，登记文件只是它里面的一个文件。
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".yi-agent"))
}

/// 登记文件路径：`<dir>/resident-daemons.json`。
pub fn registry_path(dir: &Path) -> PathBuf {
    dir.join("resident-daemons.json")
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    projects: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    project: PathBuf,
    #[serde(default)]
    required_by: Vec<String>,
}

/// 严格读：缺文件→空表；损坏→报错（写路径不得覆盖损坏文件）。
///
/// 与宿主 `yi_agent_store::resident::read_strict` 同义。这不是「读侧宽容」的
/// 那一半：调用方要么拿它去**写**（必须知道自己在覆盖什么），要么走
/// [`list`] / [`entries`] 的宽容投影。
fn read_strict(dir: &Path) -> std::io::Result<Registry> {
    let path = registry_path(dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            std::io::Error::other(format!(
                "resident registry is corrupt ({error}); refusing to overwrite {}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(error) => Err(error),
    }
}

/// 原子写：先写 tmp 再 rename，读侧看不到半截文件。
fn write(dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    let tmp = dir.join("resident-daemons.json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, registry_path(dir))
}

/// 登记「`project` 需要常驻 daemon，请求者 `requester`」。幂等：重复登记只把
/// `requester` 并进 `required_by`，不产生重复项。
pub fn require(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()> {
    let mut registry = read_strict(dir)?;
    match registry
        .projects
        .iter_mut()
        .find(|entry| entry.project == project)
    {
        Some(entry) => {
            if !entry.required_by.iter().any(|name| name == requester) {
                entry.required_by.push(requester.to_string());
                entry.required_by.sort();
            }
        }
        None => registry.projects.push(Entry {
            project: project.to_path_buf(),
            required_by: vec![requester.to_string()],
        }),
    }
    write(dir, &registry)
}

/// 撤销 `requester` 对该项目的需要；没人再需要就删掉该项目项。
///
/// 只动 `requester` 自己那一票：别的插件/组件还需要这个项目时，登记必须留下，
/// 否则会误停别人要的 daemon。
pub fn release(dir: &Path, project: &Path, requester: &str) -> std::io::Result<()> {
    let mut registry = read_strict(dir)?;
    registry.projects.retain_mut(|entry| {
        if entry.project != project {
            return true;
        }
        entry.required_by.retain(|name| name != requester);
        !entry.required_by.is_empty()
    });
    write(dir, &registry)
}

/// 读侧宽容：缺文件/损坏一律当空表。
///
/// 消费方是「要不要拉起 daemon」的决策，一份坏文件不该让它崩掉——最坏情况是
/// 这一轮不拉起，下一轮或人工修好后再拉。
pub fn list(dir: &Path) -> Vec<PathBuf> {
    read_strict(dir)
        .unwrap_or_default()
        .projects
        .into_iter()
        .map(|entry| entry.project)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn an_absent_registry_lists_nothing() {
        let dir = temp();
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn require_then_list_round_trips_the_project() {
        let dir = temp();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        assert_eq!(list(dir.path()), vec![PathBuf::from("/p")]);
    }

    #[test]
    fn require_is_idempotent_and_does_not_duplicate_the_requester() {
        let dir = temp();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        let text = std::fs::read_to_string(registry_path(dir.path())).unwrap();
        assert_eq!(text.matches("superpowers-kanban").count(), 1, "{text}");
    }

    #[test]
    fn two_requesters_of_the_same_project_are_unioned() {
        let dir = temp();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        require(dir.path(), Path::new("/p"), "another-plugin").unwrap();
        let text = std::fs::read_to_string(registry_path(dir.path())).unwrap();
        assert!(text.contains("superpowers-kanban"), "{text}");
        assert!(text.contains("another-plugin"), "{text}");
        assert_eq!(list(dir.path()), vec![PathBuf::from("/p")]);
    }

    #[test]
    fn release_drops_only_its_own_requester() {
        let dir = temp();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        require(dir.path(), Path::new("/p"), "another-plugin").unwrap();
        release(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        // 别人还需要这个项目：登记必须留下。
        assert_eq!(list(dir.path()), vec![PathBuf::from("/p")]);
        let text = std::fs::read_to_string(registry_path(dir.path())).unwrap();
        assert!(!text.contains("superpowers-kanban"), "{text}");
        assert!(text.contains("another-plugin"), "{text}");
    }

    #[test]
    fn release_removes_the_entry_once_nobody_needs_it() {
        let dir = temp();
        require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        release(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap();
        assert!(list(dir.path()).is_empty());
    }

    #[test]
    fn a_corrupt_registry_reads_empty_and_is_not_overwritten() {
        let dir = temp();
        std::fs::write(registry_path(dir.path()), "{ not json").unwrap();
        assert!(list(dir.path()).is_empty());
        // 写路径必须拒绝覆盖：损坏文件原样保留，等人工修。
        let error = require(dir.path(), Path::new("/p"), "superpowers-kanban").unwrap_err();
        assert!(error.to_string().contains("corrupt"), "{error}");
        assert_eq!(
            std::fs::read_to_string(registry_path(dir.path())).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn the_registry_path_matches_the_host_contract() {
        // 宿主的 yi_agent_store::resident::registry_path 就是 dir.join(这个文件名)。
        // 契约是「同一个文件」，所以这个名字两边都不能各自漂移。
        assert_eq!(
            registry_path(Path::new("/home/u/.yi-agent")),
            PathBuf::from("/home/u/.yi-agent/resident-daemons.json")
        );
    }
}
