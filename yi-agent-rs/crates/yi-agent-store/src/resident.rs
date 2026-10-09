//! 通用「常驻 daemon」登记：哪些项目需要常驻 daemon、由谁请求。
//!
//! 与任何具体插件/看板无关：字段只有 `project` 与 `required_by`。写它的是
//! 需要常驻 daemon 的那个组件，读它的是值守者（watchman 与 app 内循环）。
//! 放 store 层而非某个功能 crate，是为了让登记本身保持通用。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 宿主通用目录：`$HOME/.yi-agent`。`HOME` 缺失返回 `None`。
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".yi-agent"))
}

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

fn write(dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    let tmp = dir.join("resident-daemons.json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, registry_path(dir))
}

/// 登记「`project` 需要常驻 daemon，请求者 `requester`」。幂等。
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

/// 读侧宽容：损坏按空表，绝不因一份坏文件让值守者崩掉。
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

    #[test]
    fn require_is_idempotent_and_lists_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/proj/a");
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        assert_eq!(list(dir.path()), vec![project]);
    }

    #[test]
    fn two_requesters_share_one_entry_and_release_only_their_own() {
        let dir = tempfile::tempdir().unwrap();
        let project = PathBuf::from("/proj/a");
        require(dir.path(), &project, "superpowers-kanban").unwrap();
        require(dir.path(), &project, "another-plugin").unwrap();
        release(dir.path(), &project, "superpowers-kanban").unwrap();
        assert_eq!(
            list(dir.path()),
            vec![project.clone()],
            "still needed by the other plugin"
        );
        release(dir.path(), &project, "another-plugin").unwrap();
        assert!(
            list(dir.path()).is_empty(),
            "entry drops when nothing needs it"
        );
    }

    #[test]
    fn a_corrupt_registry_reads_empty_and_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(registry_path(dir.path()), "{ not json").unwrap();
        assert!(list(dir.path()).is_empty(), "readers must not crash");
        let error =
            require(dir.path(), &PathBuf::from("/proj/a"), "superpowers-kanban").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert_eq!(
            std::fs::read_to_string(registry_path(dir.path())).unwrap(),
            "{ not json",
            "a corrupt file must not be clobbered"
        );
    }
}
