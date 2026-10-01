//! 监督插件的胶水：把清单/开关驱动的进程管理，接上 daemon 的插件转发表。
//!
//! 机制本身在 `yi-agent-supervisors`（扫清单、按开关起停子进程），转发表本身在
//! `yi-agent-store::ipc`（`register_plugin_sockets`）。这里只负责把两者接起来，
//! 并规定**谁**该调用它：成功启动了 daemon 的那个进程。
//!
//! 之所以放在本 crate：`yi-agent`（TUI 与 `daemon serve`）与 `yi-agent-subagent`
//! （桌面路径）都以它为上游依赖，放这里两端能共用且不成环。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use yi_agent_supervisors::supervisor::{Layout, Supervisor};

/// 一轮对账的间隔。与既有 `daemon serve` 的实现保持一致（500ms）。
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 监督循环的句柄。持有它就等于"这个进程在监督"。
pub struct SuperviseHandle {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    supervising: bool,
}

impl SuperviseHandle {
    /// 本进程是否真的在监督（用于测试与调用点自检）。
    pub fn is_supervising(&self) -> bool {
        self.supervising
    }

    /// 停止监督：先清空转发表（不留指向将死 socket 的路由），再停子进程。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 一个"不监督"的占位句柄——用在借用了别人 daemon 的路径上。
///
/// 明确返回一个句柄而不是 `Option::None`，是为了让调用点无分支：
/// 「我起了 daemon → serve；别人起了 → idle」，两者都得到一个句柄，生命周期处理一致。
pub fn idle() -> SuperviseHandle {
    SuperviseHandle {
        stop: Arc::new(AtomicBool::new(false)),
        join: None,
        supervising: false,
    }
}

/// 在后台线程里按固定间隔对账托管子进程，并维护 daemon 的插件转发表。
///
/// 生命周期与调用它的那个 daemon 一致：daemon 在，监督在；`stop()` 之后
/// 转发表被清空、子进程被停止。
pub fn serve(workdir: &Path) -> SuperviseHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let layout = Layout::for_workdir(workdir);
    let join = std::thread::spawn(move || {
        // 监督线程的 panic 不得拖垮宿主（TUI/桌面）——记录后结束，
        // `stop()` 仍能安全 join 到这个已结束的线程。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut supervisor = Supervisor::new(layout);
            while !flag.load(Ordering::SeqCst) {
                supervisor.reconcile();
                // 转发表只有运行中的、且声明了 query_socket 的插件才该出现；
                // 每轮整体替换，停止或未声明的插件自然消失。
                yi_agent_store::ipc::register_plugin_sockets(supervisor.query_sockets());
                std::thread::sleep(RECONCILE_INTERVAL);
            }
            // 已经没有在跑的东西了，也就不该留下任何可转发的路由。
            yi_agent_store::ipc::clear_plugin_sockets();
            supervisor.stop_all();
        }));
        if outcome.is_err() {
            tracing::error!("plugin supervision loop panicked; plugins are no longer supervised");
            // 表里可能还留着本进程登记过的路由，清掉免得指向死 socket。
            yi_agent_store::ipc::clear_plugin_sockets();
        }
    });
    SuperviseHandle {
        stop,
        join: Some(join),
        supervising: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// The forwarding table is one process-wide static, and every `serve()` in
    /// this module publishes to it. Running these tests in parallel would let
    /// one supervisor's round overwrite another's, so they take turns. In
    /// production there is one daemon per process, so the shared table is not a
    /// problem there — it is only a test-isolation concern.
    static TABLE: Mutex<()> = Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        TABLE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 一个假子进程：把 pid 写进 marker 后睡到被杀。与 supervisors 的测试同款，
    /// 免得这条测试依赖真实插件二进制。
    fn write_fake_child(dir: &Path, marker: &Path) -> PathBuf {
        let script = dir.join("child.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", marker.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    /// 就位一个"开关打开 + 清单指向假子进程"的项目目录。
    fn armed_project(workdir: &Path, marker: &Path) -> PathBuf {
        let child = write_fake_child(workdir, marker);
        let manifests = workdir.join(".yi-agent").join("supervisors");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::write(
            manifests.join("demo.json"),
            format!(
                r#"{{"name":"demo","command":"{}","args":[],"switch_key":"demo_on","restart_backoff_ms":50,"restart_backoff_max_ms":200,"query_socket":"{{state_dir}}/demo.sock"}}"#,
                child.display()
            ),
        )
        .unwrap();
        std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
        std::fs::write(
            workdir.join(".yi-agent/preferences.json"),
            r#"{"demo_on":true}"#,
        )
        .unwrap();
        child
    }

    fn wait_for(path: &Path, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// 清掉可能由别的测试留下的路由，让断言只反映本次监督。
    fn clear_table() {
        yi_agent_store::ipc::clear_plugin_sockets();
    }

    #[test]
    fn a_served_project_starts_its_switched_on_child() {
        let _guard = exclusive();
        clear_table();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        armed_project(dir.path(), &marker);

        let handle = serve(dir.path());
        assert!(handle.is_supervising());
        assert!(
            wait_for(&marker, Duration::from_secs(3)),
            "a switched-on manifest must have its child started"
        );
        handle.stop();
    }

    #[test]
    fn stopping_clears_the_forwarding_table() {
        let _guard = exclusive();
        clear_table();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        armed_project(dir.path(), &marker);

        let handle = serve(dir.path());
        assert!(wait_for(&marker, Duration::from_secs(3)));
        // 运行中：清单声明了 query_socket 且进程在跑，路由必须出现。
        // （先证明"表里本来有东西"，否则"停后被清空"是句空话。）
        let registered = {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if yi_agent_store::ipc::plugin_socket_for("demo").is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            yi_agent_store::ipc::plugin_socket_for("demo")
        };
        assert!(
            registered.is_some(),
            "a running, socket-declaring plugin must be routable"
        );

        handle.stop();

        // 停止后不得留下任何可转发的路由：否则请求会打到已死的 socket。
        assert!(
            yi_agent_store::ipc::plugin_socket_for("demo").is_none(),
            "the table must be empty after the supervisor stops"
        );
    }

    #[test]
    fn an_idle_handle_does_not_supervise() {
        let _guard = exclusive();
        clear_table();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("child.pid");
        armed_project(dir.path(), &marker);

        let handle = idle();
        assert!(
            !handle.is_supervising(),
            "an idle handle must report that it supervises nothing"
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !marker.exists(),
            "idle must not start anything: borrowing another daemon means not supervising"
        );
        handle.stop();
    }

    #[test]
    fn a_project_without_manifests_is_harmless() {
        let _guard = exclusive();
        clear_table();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();

        let handle = serve(dir.path());
        assert!(handle.is_supervising());
        std::thread::sleep(Duration::from_millis(150));
        handle.stop();
    }
}
