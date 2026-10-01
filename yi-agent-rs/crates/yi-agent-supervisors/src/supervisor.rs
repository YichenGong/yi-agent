use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::manifest::{SupervisorManifest, load_manifests};
use crate::switch::{read_bool_key, resolve_bool};

/// 监督运行所需的目录布局。目录名对外固定，方便清单的占位展开。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub workdir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Layout {
    pub fn for_workdir(workdir: &Path) -> Self {
        let state_dir = workdir.join(".yi-agent").join("board");
        let runtime_dir = workdir.join(".yi-agent").join("runtime");
        Self {
            workdir: workdir.to_path_buf(),
            state_dir,
            runtime_dir,
        }
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.workdir.join(".yi-agent").join("supervisors")
    }

    fn global_preferences_path(&self) -> Option<PathBuf> {
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join(".yi-agent")
                    .join("preferences.json")
            })
    }

    fn project_preferences_path(&self) -> PathBuf {
        self.workdir.join(".yi-agent").join("preferences.json")
    }
}

struct RunningChild {
    child: Child,
    last_started: Instant,
}

/// 按清单 + 开关键托管子进程。生命周期与 daemon 一致。
pub struct Supervisor {
    layout: Layout,
    running: BTreeMap<String, RunningChild>,
    /// 一次 `spawn` 的暂存槽：先在无借用冲突处起进程，再搬进 `running`。
    spawned: Option<Child>,
}

impl Supervisor {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            running: BTreeMap::new(),
            spawned: None,
        }
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// 生效开关：项目层覆盖全局层，两层都缺省→关闭。
    pub fn desired_on(&self, manifest: &SupervisorManifest) -> bool {
        let global = self
            .layout
            .global_preferences_path()
            .and_then(|path| read_bool_key(&path, &manifest.switch_key));
        let project = read_bool_key(
            &self.layout.project_preferences_path(),
            &manifest.switch_key,
        );
        resolve_bool(global, project)
    }

    /// 一次对齐：重新扫描清单，起应起、停应停。
    ///
    /// 崩溃的子进程按清单的退避参数重启；开关关闭或清单被删除的进程会被停掉。
    pub fn reconcile(&mut self) {
        let manifests = load_manifests(&self.layout.manifests_dir());
        let known: std::collections::BTreeSet<String> =
            manifests.iter().map(|m| m.name.clone()).collect();

        // 清单被删除 → 停止并移除。
        let stale: Vec<String> = self
            .running
            .keys()
            .filter(|name| !known.contains(*name))
            .cloned()
            .collect();
        for name in stale {
            self.stop(&name);
        }

        for manifest in manifests {
            if !self.desired_on(&manifest) {
                self.stop(&manifest.name);
                continue;
            }
            self.ensure_running(&manifest);
        }
    }

    fn ensure_running(&mut self, manifest: &SupervisorManifest) {
        if let Some(running) = self.running.get_mut(&manifest.name) {
            match running.child.try_wait() {
                Ok(None) => return, // 仍在运行
                Ok(Some(_)) | Err(_) => {
                    // 已退出：按退避重启。
                    let backoff = Duration::from_millis(manifest.restart_backoff_ms);
                    if running.last_started.elapsed() < backoff {
                        return;
                    }
                }
            }
        }
        if self.spawn(manifest).is_ok() {
            if let Some(child) = self.spawned.take() {
                self.running.insert(
                    manifest.name.clone(),
                    RunningChild {
                        child,
                        last_started: Instant::now(),
                    },
                );
            }
        }
    }

    fn spawn(&mut self, manifest: &SupervisorManifest) -> std::io::Result<()> {
        let args = manifest.expand_args(
            &self.layout.workdir,
            &self.layout.state_dir,
            &self.layout.runtime_dir,
        );
        let child = Command::new(&manifest.command)
            .args(args)
            .current_dir(&self.layout.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.spawned = Some(child);
        Ok(())
    }

    fn stop(&mut self, name: &str) {
        if let Some(mut running) = self.running.remove(name) {
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// daemon 退出时回收全部子进程。
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.running.keys().cloned().collect();
        for name in names {
            self.stop(&name);
        }
    }
}
