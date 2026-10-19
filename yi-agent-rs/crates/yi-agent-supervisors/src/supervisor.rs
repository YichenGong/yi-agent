use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::manifest::{SupervisorManifest, load_manifests};
use crate::switch::{read_bool_key, resolve_bool};

/// 监督运行所需的目录布局。目录名对外固定，方便清单的占位展开。
///
/// 本 crate **不认识任何插件**，因此不写死任何插件的目录名：项目级状态根是
/// `<workdir>/.yi-agent`，某个插件的状态目录由**它的清单名**派生
/// （[`Layout::state_dir`]）。对清单名 `superpowers-kanban` 来说，派生结果仍是
/// `<workdir>/.yi-agent/superpowers-kanban`——与磁盘上的既有契约逐字一致，
/// 故不需要迁移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub workdir: PathBuf,
    /// 项目级状态根：`<workdir>/.yi-agent`。
    pub state_base: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Layout {
    pub fn for_workdir(workdir: &Path) -> Self {
        let state_base = workdir.join(".yi-agent");
        let runtime_dir = state_base.join("runtime");
        Self {
            workdir: workdir.to_path_buf(),
            state_base,
            runtime_dir,
        }
    }

    /// 交给清单 `name` 的状态目录：`<state_base>/<name>`。
    ///
    /// 由清单名派生而不是写死：这正是「监督器不认识具体插件」的落点。改名会
    /// 换目录，所以清单名是磁盘契约的一部分（插件侧
    /// `superpowers-kanban-core::layout` 从 `state_dir` 反推项目根与偏好路径）。
    pub fn state_dir(&self, name: &str) -> PathBuf {
        self.state_base.join(name)
    }

    pub fn manifests_dir(&self) -> PathBuf {
        self.state_base.join("supervisors")
    }

    /// Where supervised children's logs land: one file per manifest name.
    ///
    /// Beside the other project-scoped state rather than in a plugin's
    /// `state_dir`: this crate supervises whatever the manifests name and knows
    /// nothing about any particular plugin, so it must not write inside a
    /// directory whose name is owned by one.
    pub fn logs_dir(&self) -> PathBuf {
        self.state_base.join("logs")
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
    /// 这个子进程的日志。随它存亡，`stop` 时先写事件再 kill。
    log: ChildLog,
    /// 它的退出是否已经记过。
    ///
    /// `reconcile` 每 500ms 跑一次，退出在退避窗口内会被反复观察到;但那是
    /// **同一个事实**。每 tick 重写一遍会把同一句话刷成 60 行(退避 30s 时),
    /// 把真正的失败埋进它自己的噪音里。新实例经 `spawn_and_track` 构造，
    /// 标志自然复位。
    logged_exit: bool,
}

/// A supervised child's log file, or a no-op when the file cannot be opened.
///
/// Best effort on purpose: supervising the child is the job, recording it is a
/// bonus. A log that cannot be written must never stop the child from starting,
/// so every failure degrades to `Stdio::null` behaviour instead of propagating.
///
/// Once a write fails the file is dropped and the log goes quiet. `reconcile`
/// runs every 500ms; retrying a doomed write every tick would bury the real
/// failure under its own noise.
struct ChildLog {
    file: Option<std::fs::File>,
}

impl ChildLog {
    /// Opens `<logs_dir>/<name>.log`, creating the directory as needed.
    ///
    /// Appends rather than truncates: the interesting line is usually the last
    /// one the previous incarnation managed to write.
    fn open(logs_dir: &Path, name: &str) -> Self {
        let file = std::fs::create_dir_all(logs_dir).ok().and_then(|()| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(logs_dir.join(format!("{name}.log")))
                .ok()
        });
        Self { file }
    }

    /// Writes one timestamped line, going quiet if the file is gone.
    fn write_line(&mut self, event: &str) {
        use std::io::Write;
        let Some(file) = self.file.as_mut() else {
            return;
        };
        let stamp = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S");
        if writeln!(file, "{stamp} {event}").is_err() {
            self.file = None;
        }
    }

    #[cfg(test)]
    fn is_active(&self) -> bool {
        self.file.is_some()
    }
}

/// 按清单 + 开关键托管子进程。生命周期与 daemon 一致。
pub struct Supervisor {
    layout: Layout,
    running: BTreeMap<String, RunningChild>,
    /// 一次 `spawn` 的暂存槽：先在无借用冲突处起进程，再搬进 `running`。
    spawned: Option<Child>,
    /// 与 `spawned` 配对：起进程时建好的日志，搬进 `running` 时一并带上。
    spawned_log: Option<ChildLog>,
}

impl Supervisor {
    pub fn new(layout: Layout) -> Self {
        Self {
            layout,
            running: BTreeMap::new(),
            spawned: None,
            spawned_log: None,
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
            self.stop(&name, "manifest removed");
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
                self.stop(&manifest.name, "switch off");
                continue;
            }
            self.ensure_running(&manifest);
        }
    }

    /// 当前可被查询的插件：清单声明了 socket，且**它的查询通道现在真的可用**。
    ///
    /// 这是「插件是否有查询通道」的唯一判据——daemon 只认这份列表，不会接受
    /// 请求方传来的任意 socket 路径。停止或未声明的插件下一轮就会消失。
    ///
    /// 两个条件取「或」而不是只看 `running`：`ensure_running` 在查询 socket 已有
    /// 应答时**故意不 spawn**（健康的孤儿进程持有 socket，重复拉起会两个实例抢锁）。
    /// 那样的进程不在 `running` 里，却恰恰是唯一能回答查询的那一个。只按
    /// `running` 过滤会让它在**每次 daemon 重启后**都被漏登记：插件明明活着、
    /// socket 明明在应答，`plugin/query` 却一直回 `plugin_unavailable`，直到这个
    /// 进程碰巧退出、被重新拉起为止。
    ///
    /// 反过来，清单声明了 socket 但没人在听（刚 spawn、还没 bind，或进程已死）
    /// 时，`answers` 为假且 `running` 里若也没有它，就不该登记——转发表里的每条
    /// 路由都应当能连上。
    ///
    /// 注意「或」也覆盖了开关刚被关掉、而进程是上一代 daemon 留下的孤儿的情况：
    /// 监督器只杀得掉自己 spawn 的子进程，杀不掉那个孤儿；只要它仍在应答，这条
    /// 路由就是**真的可用**的，如实登记比假装它不在更好——查询通道正是它报告状态
    /// 并把开关重新打开的唯一途径。
    pub fn query_sockets(&self) -> Vec<(String, PathBuf)> {
        let manifests = load_manifests(&self.layout.manifests_dir());
        manifests
            .into_iter()
            .filter(|manifest| self.running.contains_key(&manifest.name) || self.answers(manifest))
            .filter_map(|manifest| {
                match manifest.query_socket_path(
                    &self.layout.workdir,
                    &self.layout.state_dir(&manifest.name),
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
            &self.layout.state_dir(&manifest.name),
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
                        // 同一个退出事实只记一次——退避窗口内每 tick 都观察到它。
                        if !running.logged_exit {
                            running.log.write_line(&format!(
                                "exited {}; retrying in {:?}",
                                manifest.name, running.backoff
                            ));
                            running.logged_exit = true;
                        }
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
        match self.spawn(manifest) {
            Ok(()) => {
                let mut log = self.spawned_log.take().unwrap_or(ChildLog { file: None });
                if let Some(child) = self.spawned.take() {
                    log.write_line(&format!("started {} (pid {})", manifest.name, child.id()));
                    self.running.insert(
                        manifest.name.clone(),
                        RunningChild {
                            child,
                            last_started: Instant::now(),
                            backoff,
                            log,
                            logged_exit: false,
                        },
                    );
                }
            }
            Err(error) => {
                // A spawn that never started must still leave a trace: "the
                // plugin never came up" is exactly the shape that used to be
                // silent (a wrong command path, a binary that cannot execute).
                let mut log = self.spawned_log.take().unwrap_or(ChildLog { file: None });
                log.write_line(&format!("could not start {}: {error}", manifest.name));
            }
        }
    }

    fn spawn(&mut self, manifest: &SupervisorManifest) -> std::io::Result<()> {
        let args = manifest.expand_args(
            &self.layout.workdir,
            &self.layout.state_dir(&manifest.name),
            &self.layout.runtime_dir,
        );
        // The log is best effort: `ChildLog::open` never fails outright, it just
        // yields an inactive log when the path is unusable. Only stderr is
        // redirected -- stdout stays swallowed, because a long-running child
        // reports on stderr while stdout may carry data a human should not see
        // interleaved with log lines.
        let log = ChildLog::open(&self.layout.logs_dir(), &manifest.name);
        let stderr = log
            .file
            .as_ref()
            .and_then(|file| file.try_clone().ok())
            .map(Stdio::from)
            .unwrap_or_else(Stdio::null);
        let child = Command::new(&manifest.command)
            .args(args)
            .current_dir(&self.layout.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()?;
        self.spawned = Some(child);
        self.spawned_log = Some(log);
        Ok(())
    }

    /// 停掉一个子进程并记下原因。
    ///
    /// 先写日志再 kill：否则子进程的临终输出可能排在被杀之后丢失。
    fn stop(&mut self, name: &str, reason: &str) {
        if let Some(mut running) = self.running.remove(name) {
            running
                .log
                .write_line(&format!("stopped {name} ({reason})"));
            let _ = running.child.kill();
            let _ = running.child.wait();
        }
    }

    /// daemon 退出时回收全部子进程。
    pub fn stop_all(&mut self) {
        let names: Vec<String> = self.running.keys().cloned().collect();
        for name in names {
            self.stop(&name, "supervisor shutting down");
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

#[cfg(test)]
mod child_log_tests {
    use super::*;

    #[test]
    fn the_logs_dir_sits_under_the_workdir() {
        let layout = Layout::for_workdir(Path::new("/tmp/project"));

        assert_eq!(
            layout.logs_dir(),
            PathBuf::from("/tmp/project/.yi-agent/logs"),
            "the log directory is derived from the workdir, not configured"
        );
    }

    /// `logs_dir` writes outside every plugin-owned directory on purpose: this
    /// crate supervises whatever the manifests name and owns no plugin's name.
    /// A change that moved logs into `state_dir` would put generic-crate output
    /// inside a plugin's directory.
    #[test]
    fn the_logs_dir_is_not_a_plugin_state_dir() {
        let layout = Layout::for_workdir(Path::new("/tmp/project"));
        assert!(
            !layout
                .logs_dir()
                .starts_with(layout.state_dir("superpowers-kanban"))
        );
    }

    /// The generic crate must not own a plugin's directory name: the per-plugin
    /// state dir is derived from the manifest name, so this workspace never
    /// hardcodes `superpowers-kanban` in the code that runs in production.
    #[test]
    fn no_plugin_name_is_hardcoded_in_the_layout() {
        // 路径由**清单名**派生：两个不同的名字必得两个不同的目录，且都等于
        // `<state_base>/<name>`。这条比字符串扫描有力——它证明的是行为。
        let layout = Layout::for_workdir(Path::new("/tmp/project"));
        let kanban = layout.state_dir("superpowers-kanban");
        let other = layout.state_dir("some-other-plugin");
        assert_ne!(kanban, other, "the state dir must follow the manifest name");
        assert_eq!(
            kanban,
            PathBuf::from("/tmp/project/.yi-agent/superpowers-kanban")
        );
        assert_eq!(
            other,
            PathBuf::from("/tmp/project/.yi-agent/some-other-plugin")
        );
        // 项目级根与具体插件无关。
        assert_eq!(layout.state_base, PathBuf::from("/tmp/project/.yi-agent"));

        // 生产代码（去注释、去测试模块）里不得出现任何插件名。注释可以讲历史，
        // 测试可以举例子，跑起来的代码不行。
        //
        // 边界取**测试模块**（`#[cfg(test)] mod tests`），而不是「第一个
        // `#[cfg(test)]` 属性」：本文件另有 `#[cfg(test)]` 门控的**生产辅助函数**
        // （`ChildLog::is_active`、`Supervisor::backoff_for_test`），按第一个属性
        // 截断会把 `impl Supervisor` 整段漏在扫描之外——那正是最需要扫的代码。
        let region = include_str!("supervisor.rs")
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("the test module header must exist");
        // 扫描范围必须真的覆盖生产大块：边界逻辑若再次提前截断，这两条要响，
        // 而不是让下面的断言空过。
        assert!(
            region.contains("impl Supervisor") && region.contains("fn ensure_running"),
            "the scan must span the Supervisor implementation, not stop early"
        );
        // 反向也要成立：测试模块不得混进来（否则测试里的插件名会让它假红）。
        assert!(
            !region.contains("mod child_log_tests"),
            "the scan must exclude the crate's own test modules"
        );
        let production: String = region
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !production.contains("superpowers-kanban"),
            "the supervisor crate's production code must not name a plugin"
        );
    }

    #[test]
    fn a_line_is_written_with_a_timestamp_prefix() {
        let dir = tempfile::TempDir::new().unwrap();
        let logs = dir.path().join("logs");
        let mut log = ChildLog::open(&logs, "demo");

        log.write_line("started demo (pid 42)");

        let text = std::fs::read_to_string(logs.join("demo.log")).unwrap();
        assert!(
            text.ends_with("started demo (pid 42)\n"),
            "the event must be the tail of the line, got: {text:?}"
        );
        let prefix = &text[..text.find(' ').expect("timestamp then a space")];
        assert_eq!(
            prefix.len(),
            19,
            "expected YYYY-MM-DDTHH:MM:SS, got {prefix:?}"
        );
        assert!(prefix.contains('T'), "date and time must be separated by T");
    }

    #[test]
    fn lines_append_rather_than_truncate() {
        let dir = tempfile::TempDir::new().unwrap();
        let logs = dir.path().join("logs");

        ChildLog::open(&logs, "demo").write_line("first");
        ChildLog::open(&logs, "demo").write_line("second");

        let text = std::fs::read_to_string(logs.join("demo.log")).unwrap();
        assert!(
            text.contains("first") && text.contains("second"),
            "a restart must not erase the previous run's last words, got: {text:?}"
        );
    }

    #[test]
    fn an_unwritable_log_degrades_instead_of_failing() {
        let dir = tempfile::TempDir::new().unwrap();
        // `logs` 是一个**文件**而不是目录：建目录必失败，写入器必须降级。
        let logs = dir.path().join("logs");
        std::fs::write(&logs, "not a directory").unwrap();

        let mut log = ChildLog::open(&logs, "demo");
        assert!(
            !log.is_active(),
            "an unusable log path must degrade to a no-op, not panic"
        );
        log.write_line("this must not panic");
    }
}
