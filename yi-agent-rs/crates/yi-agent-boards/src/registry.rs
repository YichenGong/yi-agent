//! Global registry of which projects own a Superpowers kanban board.
//!
//! The registry is a single JSON file, `<dir>/boards.json`, shaped like
//! `{"boards":[{"project":"<abs>","created_at":"..."}]}`. Every mutation is a
//! read-modify-write whose final step is a `tmp` + `rename`, so a crash can
//! never leave a half-written file behind.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One registered project board.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Board {
    /// Absolute path of the project the board belongs to.
    pub project: PathBuf,
    /// RFC 3339 timestamp of when the board was first registered.
    pub created_at: String,
}

/// On-disk shape. A wrapper object (not a bare array) so the file can grow
/// more keys later without breaking existing readers.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub boards: Vec<Board>,
}

/// Every registered board.
pub fn list(dir: &Path) -> std::io::Result<Vec<Board>> {
    Ok(read(dir).boards)
}

/// Whether `project` has a board.
pub fn contains(dir: &Path, project: &Path) -> std::io::Result<bool> {
    Ok(read(dir)
        .boards
        .iter()
        .any(|board| board.project == project))
}

/// Register `project`, stamping `created_at`.
///
/// Idempotent: registering a project that already has a board leaves its
/// original `created_at` untouched instead of appending a duplicate.
///
/// Refuses to run against a corrupt registry. [`read`] would see it as empty,
/// so this write would silently drop every other project's entry — an error the
/// caller can report beats destroying boards nobody asked to touch.
pub fn register(dir: &Path, project: &Path) -> std::io::Result<()> {
    if is_corrupt(dir) {
        return Err(std::io::Error::other(
            "看板登记表已损坏，拒绝覆盖；请先修复 boards.json",
        ));
    }
    let mut registry = read(dir);
    if registry.boards.iter().any(|board| board.project == project) {
        return Ok(());
    }
    registry.boards.push(Board {
        project: project.to_path_buf(),
        created_at: chrono::Local::now().to_rfc3339(),
    });
    write(dir, &registry)
}

/// Unregister `project`; other boards are left alone.
///
/// Idempotent: removing a project that has no board is a no-op.
pub fn unregister(dir: &Path, project: &Path) -> std::io::Result<()> {
    let mut registry = read(dir);
    registry.boards.retain(|board| board.project != project);
    write(dir, &registry)
}

fn registry_path(dir: &Path) -> PathBuf {
    dir.join("boards.json")
}

/// A missing, unreadable, or corrupt registry reads as empty: a hand-edited
/// file must not keep the whole sidebar from starting.
fn read(dir: &Path) -> Registry {
    match std::fs::read_to_string(registry_path(dir)) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
        Err(_) => Registry::default(),
    }
}

/// Whether a registry file is present but unusable (unreadable or unparseable).
///
/// [`read`] deliberately collapses that case to an empty registry so one bad
/// file cannot take the whole sidebar down. A caller that is about to *act* on
/// "this project has no board" can consult this first: telling a user to
/// `create` a board they may already have would send them through
/// [`register`], which reads the same damaged file as empty and overwrites it,
/// dropping every other project's entry.
pub fn is_corrupt(dir: &Path) -> bool {
    match std::fs::read_to_string(registry_path(dir)) {
        Ok(text) => serde_json::from_str::<Registry>(&text).is_err(),
        Err(_) => false,
    }
}

fn write(dir: &Path, registry: &Registry) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = serde_json::to_string_pretty(registry).map_err(std::io::Error::other)?;
    let tmp_path = dir.join("boards.json.tmp");
    std::fs::write(&tmp_path, &text)?;
    std::fs::rename(&tmp_path, registry_path(dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn a_fresh_registry_lists_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
        assert!(!contains(dir.path(), Path::new("/projects/a")).unwrap());
    }

    #[test]
    fn registering_is_idempotent_and_survives_a_reread() {
        let dir = tempfile::tempdir().unwrap();
        let project = Path::new("/projects/a");
        register(dir.path(), project).unwrap();
        register(dir.path(), project).unwrap();
        let boards = list(dir.path()).unwrap();
        assert_eq!(boards.len(), 1, "重复登记不得出现两条");
        assert!(contains(dir.path(), project).unwrap());
    }

    #[test]
    fn unregistering_removes_only_the_named_project() {
        let dir = tempfile::tempdir().unwrap();
        register(dir.path(), Path::new("/projects/a")).unwrap();
        register(dir.path(), Path::new("/projects/b")).unwrap();
        unregister(dir.path(), Path::new("/projects/a")).unwrap();
        let boards = list(dir.path()).unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].project, PathBuf::from("/projects/b"));
    }

    #[test]
    fn a_corrupt_registry_reads_as_empty_instead_of_failing() {
        // 手改坏了的登记表不该让整个侧栏起不来。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("boards.json"), "{ not json").unwrap();
        assert!(list(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn is_corrupt_only_for_a_file_that_is_present_and_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_corrupt(dir.path()), "登记表缺失不是损坏");
        register(dir.path(), Path::new("/projects/a")).unwrap();
        assert!(!is_corrupt(dir.path()), "正常登记表不是损坏");
        std::fs::write(dir.path().join("boards.json"), "{ not json").unwrap();
        assert!(is_corrupt(dir.path()), "手改坏了的登记表必须被认出");
    }

    #[test]
    fn registering_against_a_corrupt_registry_refuses_instead_of_overwriting() {
        // read() 会把坏表看成空表；如果不拦，register 就会把别项目的登记一并抹掉。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("boards.json"), "{ not json").unwrap();
        let error = register(dir.path(), Path::new("/projects/a")).unwrap_err();
        assert!(
            error.to_string().contains("损坏"),
            "must name the corrupt registry: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("boards.json")).unwrap(),
            "{ not json",
            "the damaged file must be left untouched"
        );
    }

    #[test]
    fn registering_stamps_created_at_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        register(dir.path(), Path::new("/projects/a")).unwrap();
        let boards = list(dir.path()).unwrap();
        assert!(!boards[0].created_at.is_empty(), "created_at 必须被写入");
        assert!(
            !dir.path().join("boards.json.tmp").exists(),
            "tmp 文件必须被 rename 掉"
        );
    }

    #[test]
    fn unregistering_an_unknown_project_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        register(dir.path(), Path::new("/projects/a")).unwrap();
        unregister(dir.path(), Path::new("/projects/zzz")).unwrap();
        assert_eq!(list(dir.path()).unwrap().len(), 1);
    }
}
