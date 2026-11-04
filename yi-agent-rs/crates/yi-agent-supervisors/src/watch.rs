//! 保证「登记在册的项目」各自有一个活着的 daemon。
//!
//! 一份逻辑、两个宿主：常驻 watchman（launchd 托管）与 app-server 内循环都调它。
//! 幂等——daemon 已应答就什么都不做；daemon 自身有独占锁，重复探测/拉起不会双起。

use std::path::{Path, PathBuf};

/// 生产入口：用真实的 daemon 生命周期（复用已应答的 daemon，否则拉起一个）。
pub fn ensure_daemons(projects: &[PathBuf]) {
    ensure_daemons_with(projects, &mut crate::daemon::ensure_running);
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
                    "resident watchman: could not ensure a daemon for {}: {error}",
                    project.display()
                );
            }
        }
    }
    started
}

/// Whether this instance may still start daemons nobody asked for.
///
/// The resident registration is *configuration*: it records that a project wants a
/// daemon when a frontend is around. It is not an instruction to keep one alive
/// with no frontend at all. Once an instance has migrated to the managed runtime,
/// "no client" must mean "no daemon", so every unattended starter consults this.
pub fn may_start_unattended(instance_home: &Path) -> bool {
    !crate::lifecycle::retirement::is_retired(instance_home)
}

/// The unattended loop: read the registration and ensure each project has a daemon,
/// **unless** this instance no longer restarts itself.
///
/// Returns `None` when the loop must stand down, which is deliberately distinct
/// from `Some(0)` (nothing registered): the first means "this is not my job any
/// more", the second means "my job found no work". Collapsing them would make a
/// retired loop look like an idle one and keep sleeping forever.
pub fn once_unattended(
    instance_home: &Path,
    resident_dir: &Path,
    ensure: &mut dyn FnMut(&[PathBuf]),
) -> Option<usize> {
    if !may_start_unattended(instance_home) {
        return None;
    }
    Some(once(resident_dir, ensure))
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

    /// 迁移后的实例不得再被「登记表里还有项目」驱动起 daemon。
    ///
    /// 登记是配置，不是无人值守的授权；把两者等同起来就是 spec 反对的
    /// 「旧自愈源继续复活 daemon」。
    #[test]
    fn a_retired_instance_starts_nothing_unattended() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            yi_agent_store::resident::registry_path(dir.path()),
            "{\"projects\":[]}",
        )
        .unwrap();
        crate::lifecycle::retirement::retire(instance.path(), "managed runtime took over").unwrap();
        let seen = RefCell::new(Vec::new());

        let outcome = once_unattended(instance.path(), dir.path(), &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });

        assert_eq!(outcome, None);
        assert!(seen.borrow().is_empty(), "no ensure call may happen");
    }

    /// 「没有登记」与「不再是我负责」必须是两个不同答案。
    #[test]
    fn an_empty_registry_still_reports_that_it_ran() {
        let instance = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let seen = RefCell::new(Vec::new());
        let outcome = once_unattended(instance.path(), dir.path(), &mut |projects| {
            seen.borrow_mut().push(projects.len());
        });
        assert_eq!(outcome, Some(0));
        assert_eq!(*seen.borrow(), vec![0]);
    }

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
        yi_agent_store::resident::require(dir.path(), &a, "example-plugin").unwrap();
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
