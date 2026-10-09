//! 保证「登记在册的项目」各自有一个活着的 daemon。
//!
//! 一份逻辑、两个宿主：常驻 watchman（launchd 托管）与 app-server 内循环都调它。
//! 幂等——daemon 已应答就什么都不做；daemon 自身有独占锁，重复探测/拉起不会双起。

use std::path::{Path, PathBuf};

/// 生产入口：用真实 `launch_if_absent`。
pub fn ensure_daemons(projects: &[PathBuf]) {
    ensure_daemons_with(projects, &mut crate::lifecycle::launch_if_absent);
}

/// 注入版：返回本次真正拉起的项目。单个失败只记日志，不影响其余项目。
pub fn ensure_daemons_with(
    projects: &[PathBuf],
    launcher: &mut dyn FnMut(&Path) -> Result<bool, String>,
) -> Vec<PathBuf> {
    let mut started = Vec::new();
    for project in projects {
        match launcher(project) {
            Ok(true) => started.push(project.clone()),
            Ok(false) => {}
            Err(error) => {
                eprintln!(
                    "board watchman: could not ensure a daemon for {}: {error}",
                    project.display()
                );
            }
        }
    }
    started
}

/// 读通用登记并交给 `ensure`；返回登记项目数。CLI 与 app-server 共用这一份，
/// 不各自复制「读登记 → ensure」的胶水。
pub fn once(resident_dir: &Path, ensure: &mut dyn FnMut(&[PathBuf])) -> usize {
    let projects = yi_agent_store::resident::list(resident_dir);
    ensure(&projects);
    projects.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn every_registered_project_is_ensured() {
        let projects = vec![PathBuf::from("/a"), PathBuf::from("/b")];
        let seen = RefCell::new(Vec::new());
        ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            Ok(true)
        });
        assert_eq!(*seen.borrow(), projects);
    }

    #[test]
    fn once_reads_the_registry_and_hands_it_to_the_ensurer() {
        let dir = tempfile::tempdir().unwrap();
        let a = PathBuf::from("/a");
        yi_agent_store::resident::require(dir.path(), &a, "superpowers-kanban").unwrap();
        let mut seen: Vec<PathBuf> = Vec::new();
        let count = once(dir.path(), &mut |projects| seen.extend_from_slice(projects));
        assert_eq!(count, 1);
        assert_eq!(seen, vec![a]);
    }

    #[test]
    fn one_failing_project_does_not_stop_the_others() {
        let projects = vec![
            PathBuf::from("/a"),
            PathBuf::from("/b"),
            PathBuf::from("/c"),
        ];
        let seen = RefCell::new(Vec::new());
        let started = ensure_daemons_with(&projects, &mut |project| {
            seen.borrow_mut().push(project.to_path_buf());
            if project == Path::new("/b") {
                return Err("boom".to_string());
            }
            Ok(true)
        });
        assert_eq!(seen.borrow().len(), 3, "all projects are attempted");
        assert_eq!(started, vec![PathBuf::from("/a"), PathBuf::from("/c")]);
    }
}
