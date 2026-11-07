//! 维护屏障的 IPC 集成测试：真实 daemon、真实 socket、真实磁盘日志。
//!
//! # 这一层要证明什么
//!
//! `ipc::managed` 的单元测试钉的是判定函数本身；这里钉的是**daemon 真的
//! 走了它**，并且对端能拿到一个不会误导人的答复。两者不能互相代替：
//! 判定全对而 dispatcher 忘了接线，线上照样会放行。
//!
//! # 三条不许含糊的事
//!
//! 1. 旧协议对端要拿到**明确拒绝**，不能是"看起来没在维护"。
//! 2. 权威记录读不出来时要回答 `unknown`，而调用方必须把它当"停"。
//! 3. 判定只来自磁盘——把内存/通知当作事实，丢一次包就会放行。
//!
//! 第 3 点在这里的验证方式是：先让 daemon 回答一次 `open`，**不发送任何
//! 通知**，直接在磁盘上开一个事务，再问一次。答案必须变。若实现缓存了
//! 上一次结论，这条就会红——而那正是通知丢失时的形状。

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use tempfile::TempDir;
use yi_agent_plugins::{
    JournalStore, Lifecycle, LifecycleRequest, PluginId, ReleaseId, RuntimeControl,
};
use yi_agent_store::ipc::{Daemon, IpcErrorCode, IpcRequest, IpcResponse, send_request};

/// `paths::instance_home()` 读的是**进程级**环境变量，所以这些用例不能并行。
///
/// 不是懒：并行时两条用例会互相覆盖 `YI_AGENT_HOME`，于是"坏日志"那条
/// 可能读到另一条的干净目录，变成一条永远绿的假测试。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn serial_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Fixture {
    temp: TempDir,
    /// `YI_AGENT_HOME` 的值。按 `instance_home` 的归一化规则以 `.yi-agent` 结尾。
    home: std::path::PathBuf,
    _guard: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Fixture {
        let guard = serial_env();
        // `/private/tmp` 而非默认临时目录：socket 位于
        // `<instance_home>/sockets/<16 hex>/runtime.sock`，macOS 默认临时目录本身
        // 就 68 字节，加上 `.yi-agent` 与尾部 40 字节会超过 103 字节的 `sun_path`。
        let temp = tempfile::Builder::new()
            .prefix("mipc")
            .tempdir_in("/private/tmp")
            .unwrap();
        let home = temp.path().join(".yi-agent");
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: 环境变量在整个二进制内共享，但这里持有全局锁，本文件的
        // 其它用例不会同时读写它。别的测试二进制是独立进程，互不可见。
        unsafe {
            std::env::set_var("YI_AGENT_HOME", &home);
        }
        Fixture {
            temp,
            home,
            _guard: guard,
        }
    }

    fn daemon(&self) -> Daemon {
        Daemon::start(
            self.temp.path().join("runtime"),
            self.temp.path().join("runtime.sqlite"),
        )
        .expect("daemon starts")
    }

    fn plugin(&self) -> PluginId {
        PluginId::parse("example-counter").unwrap()
    }

    fn release(&self) -> ReleaseId {
        ReleaseId {
            plugin: self.plugin(),
            version: "1.4.0".into(),
            target: "darwin-aarch64".into(),
            sha256: "00".repeat(32),
        }
    }

    /// 在磁盘上开一个未收尾的事务。**不**通知 daemon。
    fn open_transaction(&self) -> String {
        let lifecycle = Lifecycle::open(&self.home).unwrap();
        let transaction = lifecycle
            .start(LifecycleRequest::activate(self.plugin(), self.release(), 2))
            .unwrap();
        lifecycle.advance(&transaction, &Noop).unwrap();
        transaction
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 恢复一个安全值，避免影响同进程里的其它测试二进制。
        unsafe {
            std::env::remove_var("YI_AGENT_HOME");
        }
    }
}

struct Noop;

impl RuntimeControl for Noop {
    fn pause_maintenance(&self, _: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn resume_maintenance(&self, _: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn stop_active(&self, _: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn start_candidate(
        &self,
        _: &PluginId,
        _: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn stop_candidate(&self, _: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn health_check(&self, _: &PluginId, _: &str) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn migrate_project(
        &self,
        _: &PluginId,
        _: &str,
        _: Option<&ReleaseId>,
        _: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn repair_projection(
        &self,
        _: &PluginId,
        _: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
}

fn maintenance_status(
    socket: &std::path::Path,
    plugin: &str,
    protocol: Option<u32>,
) -> IpcResponse {
    send_request(
        socket,
        IpcRequest::PluginMaintenance {
            plugin: plugin.to_string(),
            protocol,
        },
    )
    .expect("the daemon answers")
}

fn expect_status(response: IpcResponse) -> (String, Option<String>) {
    match response {
        IpcResponse::PluginMaintenanceStatus {
            state,
            transaction_id,
            ..
        } => (state, transaction_id),
        other => panic!("expected a maintenance status, got {other:?}"),
    }
}

fn expect_error(response: IpcResponse) -> (IpcErrorCode, String) {
    match response {
        IpcResponse::Error { code, message } => {
            (code, message.unwrap_or_else(|| "<no message>".into()))
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

/// 给 socket 留一点时间：`Daemon::start` 返回后监听线程可能还没进入 accept。
fn wait_ready(socket: &std::path::Path) {
    for _ in 0..200 {
        if send_request(socket, IpcRequest::Status).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the daemon never became ready at {}", socket.display());
}

#[test]
fn a_plugin_with_no_transaction_reports_open() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let (state, transaction) = expect_status(maintenance_status(
        daemon.socket_path(),
        "example-counter",
        Some(1),
    ));
    assert_eq!(state, "open");
    assert!(transaction.is_none(), "an open state has no transaction");
}

#[test]
fn a_transaction_on_disk_closes_the_barrier_without_any_notification() {
    // 这条是整个协议的正面证据：**没有**发过任何通知，只改磁盘，
    // daemon 的答复就必须改变。任何"缓存上一次结论"或"依赖通知"的
    // 实现都会在这里红。
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let (before, _) = expect_status(maintenance_status(
        daemon.socket_path(),
        "example-counter",
        Some(1),
    ));
    assert_eq!(before, "open");

    let transaction = fixture.open_transaction();

    let (after, reported) = expect_status(maintenance_status(
        daemon.socket_path(),
        "example-counter",
        Some(1),
    ));
    assert_eq!(
        after, "closed",
        "a transaction recorded on disk must close the barrier even though no \
         notification was ever sent"
    );
    assert_eq!(
        reported.as_deref(),
        Some(transaction.as_str()),
        "the answer must name the transaction so the caller can correlate"
    );
}

#[test]
fn an_unreadable_journal_answers_unknown_and_never_open() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let path = JournalStore::at(&fixture.home).path(&fixture.plugin());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();

    let (state, _) = expect_status(maintenance_status(
        daemon.socket_path(),
        "example-counter",
        Some(1),
    ));
    assert_eq!(
        state, "unknown",
        "an unreadable journal must not be reported as open"
    );
}

#[test]
fn an_old_client_is_refused_and_told_which_side_to_upgrade() {
    // 旧客户端不带 `protocol`。把它读成"没有维护"会让旧 daemon 在迁移期间
    // 照常启动后端，所以这里必须是明确错误，且错误里要有可执行的建议。
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let response = maintenance_status(daemon.socket_path(), "example-counter", None);
    let (_code, message) = expect_error(response);
    assert!(
        message.contains("not supported"),
        "the refusal must say it is unsupported, got: {message}"
    );
    assert!(
        message.contains("upgrade"),
        "the refusal must tell the user what to do, got: {message}"
    );
}

#[test]
fn a_request_naming_an_invalid_plugin_is_an_error_not_an_open_answer() {
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let response = maintenance_status(daemon.socket_path(), "../../etc/passwd", Some(1));
    let (_code, message) = expect_error(response);
    assert!(
        message.contains("not a valid plugin id"),
        "the refusal must name the reason, got: {message}"
    );
}

#[test]
fn one_plugins_broken_journal_does_not_change_another_plugins_answer() {
    // 一份无关的坏日志不能让健康插件也被判成 unknown：那会把"某个插件
    // 上次崩了"放大成"整台机器都不能启动插件"。
    let fixture = Fixture::new();
    let daemon = fixture.daemon();
    wait_ready(daemon.socket_path());

    let broken = JournalStore::at(&fixture.home).path(&fixture.plugin());
    std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
    std::fs::write(&broken, b"{ this is not json").unwrap();

    let (healthy, _) = expect_status(maintenance_status(
        daemon.socket_path(),
        "other-plugin",
        Some(1),
    ));
    assert_eq!(healthy, "open");
}
