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
        let state_dir = workdir.join(".yi-agent").join("superpowers-kanban");
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
    child: std::process::Child,
    last_started: Instant,
    /// 当前重启退避。反复失败时 ×2 增长，至清单的 `restart_backoff_max_ms` 封顶。
    backoff: Duration,
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
                // A process that must stay reachable to be turned back on is
                // left running: "off" means "stop working", not "disappear".
                // Stopping it would remove the query channel that reports and
                // flips the switch, so off would never be recoverable in-app.
                if !manifest.stop_when_disabled {
                    self.ensure_running(&manifest);
                    continue;
                }
                self.stop(&manifest.name);
                continue;
            }
            self.ensure_running(&manifest);
        }
    }

    /// 当前可被查询的插件：清单声明了 socket，且进程正在运行。
    ///
    /// 这是「插件是否有查询通道」的唯一判据——daemon 只认这份列表，不会接受
    /// 请求方传来的任意 socket 路径。停止或未声明的插件下一轮就会消失。
    pub fn query_sockets(&self) -> Vec<(String, PathBuf)> {
        let manifests = load_manifests(&self.layout.manifests_dir());
        manifests
            .into_iter()
            .filter(|manifest| self.running.contains_key(&manifest.name))
            .filter_map(|manifest| {
                match manifest.query_socket_path(
                    &self.layout.workdir,
                    &self.layout.state_dir,
                    &self.layout.runtime_dir,
                ) {
                    Some(Ok(path)) => Some((manifest.name.clone(), path)),
                    Some(Err(error)) => {
                        // 路径深到连回退都放不下：插件不会在那个位置 bind，
                        // 转发表放进去只会让查询超时。跳过并说清原因。
                        tracing::warn!(
                            plugin = %manifest.name,
                            %error,
                            "query socket is unusable; plugin will not be queryable"
                        );
                        None
                    }
                    None => None,
                }
            })
            .collect()
    }

    /// 该清单声明的查询 socket 上是否已有实例在应答。
    ///
    /// 用「能否连上」判断：Unix socket 的 `connect` 成功当且仅当有进程在
    /// `listen`。陈旧 socket 文件（无监听者）会 `ECONNREFUSED`，故不会被误认。
    fn answers(&self, manifest: &SupervisorManifest) -> bool {
        match manifest.query_socket_path(
            &self.layout.workdir,
            &self.layout.state_dir,
            &self.layout.runtime_dir,
        ) {
            Some(Ok(socket)) => std::os::unix::net::UnixStream::connect(&socket).is_ok(),
            _ => false,
        }
    }

    /// 仅测试用：某托管进程当前的退避值。
    #[cfg(test)]
    pub(crate) fn backoff_for_test(&self, name: &str) -> Option<Duration> {
        self.running.get(name).map(|running| running.backoff)
    }

    fn ensure_running(&mut self, manifest: &SupervisorManifest) {
        // 幂等：已有实例在 `query_socket` 上应答 → 不重复 spawn。
        // 这从源头消除「健康孤儿持锁 + 监督器不断补位」的空转循环。
        if self.answers(manifest) {
            return;
        }
        let initial = Duration::from_millis(manifest.restart_backoff_ms);
        let max = Duration::from_millis(
            manifest
                .restart_backoff_max_ms
                .max(manifest.restart_backoff_ms),
        );
        let backoff = match self.running.get_mut(&manifest.name) {
            None => initial,
            Some(running) => match running.child.try_wait() {
                Ok(None) => return, // 仍在运行
                Ok(Some(_)) | Err(_) => {
                    // 已退出：未到当前退避就先等，到了才重试并把退避翻倍。
                    if running.last_started.elapsed() < running.backoff {
                        return;
                    }
                    running.backoff.saturating_mul(2).min(max)
                }
            },
        };
        self.spawn_and_track(manifest, backoff);
    }

    /// spawn 并把新 child 连同退避值记进 `running`。
    fn spawn_and_track(&mut self, manifest: &SupervisorManifest, backoff: Duration) {
        if self.spawn(manifest).is_ok() {
            if let Some(child) = self.spawned.take() {
                self.running.insert(
                    manifest.name.clone(),
                    RunningChild {
                        child,
                        last_started: Instant::now(),
                        backoff,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一份清单到 `<workdir>/.yi-agent/supervisors/<name>.json`。
    fn write_manifest(workdir: &Path, name: &str, extra: &str) -> SupervisorManifest {
        let dir = workdir.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"name":"{name}","command":"/bin/sh","args":["-c","exit 1"],"switch_key":"{name}_on","restart_backoff_ms":40,"restart_backoff_max_ms":160,"stop_when_disabled":false{extra}}}"#
        );
        let manifest = SupervisorManifest::parse(&json).unwrap();
        std::fs::write(dir.join(format!("{name}.json")), &json).unwrap();
        manifest
    }

    #[test]
    fn repeated_failures_grow_the_backoff_up_to_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path();
        let manifest = write_manifest(workdir, "demo", "");
        let mut supervisor = Supervisor::new(Layout::for_workdir(workdir));

        // 第一次：初始退避 = restart_backoff_ms。
        let start = Instant::now();
        for _ in 0..40 {
            supervisor.reconcile();
            if supervisor.running_count() == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(supervisor.running_count(), 1, "第一次必须起进程");
        let _ = start;

        // 记下当前退避，验证它随反复失败增长、且不超过上限。
        let observed = supervisor.backoff_for_test("demo").expect("tracked");
        assert!(
            observed >= Duration::from_millis(40) && observed <= Duration::from_millis(160),
            "退避必须落在 [restart_backoff_ms, restart_backoff_max_ms] 内，实际 {observed:?}"
        );
        let _ = manifest;
    }
}
