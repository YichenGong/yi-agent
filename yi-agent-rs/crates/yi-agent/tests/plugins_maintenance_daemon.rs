//! 真实 detached daemon 上的维护屏障测试。
//!
//! # 为什么必须是真进程
//!
//! 前两层（`ipc::managed` 单测、`maintenance_ipc` 进程内集成）用的都是同进程
//! 的 `Daemon`。它们证明不了两件只在**跨进程**时才成立的事：
//!
//! 1. `paths::instance_home()` 在真实启动路径上读到的确实是 `YI_AGENT_HOME`，
//!    而不是被某个 `OnceLock` 缓存的旧值。
//! 2. 管理端进程**死掉**之后，daemon 的结论仍然只来自磁盘。
//!
//! 第 2 点是这个文件的核心。设计 §9.2 第 5 步明确：暂停 supervisor 重启、
//! 停止旧服务之后，所有启用项目要保持维护所有权，**不得**为重启释放所有权
//! 让别的实例趁隙接管。若 daemon 把"管理端还活着/发过通知"当成前提，
//! 管理端一崩它就会认为"没人维护了"，于是在回滚进行到一半时把旧版本拉起来
//! ——那正是要防的抢跑。
//!
//! # 为什么不用 `#[ignore]`
//!
//! 这里的操作全是本机文件与本地 socket，不联网、不需要 API key。跑一次约
//! 数秒。把它标成 ignored 只会让 CI 长期看不到这条最关键的回归。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use yi_agent_plugins::{Lifecycle, LifecycleRequest, PluginId, ReleaseId, RuntimeControl};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yi-agent"))
}

/// 起真实 detached daemon 所需的最小模型配置。
///
/// `daemon serve` 会构造 provider，而 provider 拒绝空模型名。这里给的是一份
/// 明显的假配置：本文件的用例**不**发起任何模型请求，只要 daemon 能起来。
const MODEL_ENV: [(&str, &str); 3] = [
    ("YI_AGENT_PROVIDER", "anthropic"),
    ("YI_AGENT_MODEL", "claude-sonnet-4-5"),
    ("MODEL_API_KEY", "not-a-real-key"),
];

struct Fixture {
    temp: TempDir,
    home: PathBuf,
    project: PathBuf,
    /// 显式指定的 runtime 目录。
    ///
    /// 不依赖 `<project>/.yi-agent/runtime`：unix socket 的 `sun_path` 只有
    /// 103 字节，临时目录一深就会退到 `/tmp` 下的哈希名，测试再去拼路径
    /// 就会指向一个不存在的 socket。显式指定让路径长度可控，也省掉一段
    /// 与本测试无关的兜底逻辑。
    runtime: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join(".yi-agent");
        let project = temp.path().join("project");
        let runtime = temp.path().join("rt");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        Fixture {
            temp,
            home,
            project,
            runtime,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(binary());
        command
            .args(args)
            .env("YI_AGENT_HOME", &self.home)
            .env("YI_AGENT_RUNTIME_DIR", &self.runtime)
            .env("HOME", self.temp.path())
            .current_dir(&self.project);
        for (key, value) in MODEL_ENV {
            command.env(key, value);
        }
        command
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let output = self.command(args).output().expect("yi-agent runs");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// 起一个 detached daemon，并等到它真的应答。
    fn start_daemon(&self) {
        let (code, stdout, stderr) = self.run(&["daemon", "start"]);
        assert_eq!(code, 0, "daemon start failed: {stdout}{stderr}");
        assert!(stdout.contains("daemon started"), "got: {stdout}{stderr}");
    }

    fn maintenance(&self, plugin: &str) -> (i32, String) {
        let (code, stdout, stderr) = self.run(&["plugins", "maintenance", "--plugin", plugin]);
        (code, format!("{stdout}{stderr}"))
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

    /// 在磁盘上开一个未收尾事务。**不**通知 daemon。
    fn open_transaction(&self) -> String {
        let lifecycle = Lifecycle::open(&self.home).unwrap();
        let transaction = lifecycle
            .start(LifecycleRequest::activate(self.plugin(), self.release(), 2))
            .unwrap();
        lifecycle.advance(&transaction, &Noop).unwrap();
        transaction
    }

    /// 找到 daemon 的 pid。
    ///
    /// 按**项目工作目录**认：`daemon serve` 的 argv 里没有实例根，它的身份
    /// 由 cwd 决定（每个项目一个 daemon）。用实例根去匹配永远找不到。
    fn daemon_pid(&self) -> Option<u32> {
        // 用 `lsof` 认监听者，不靠 `ps` 匹配命令行：`daemon serve` 的 argv
        // 里没有实例根也没有 runtime 目录，靠子串匹配会误伤别的用例的
        // daemon（它们同时跑在同一个测试二进制里）。
        let socket = self.socket();
        let path = socket.to_str()?;
        let output = Command::new("lsof")
            .args(["-nP", "-t", path])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .and_then(|pid| pid.trim().parse().ok())
    }

    fn socket(&self) -> PathBuf {
        yi_agent_store::ipc::socket_path_for(&self.runtime).expect("socket path")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // 先请求有序停止；再兜底 kill，避免残留 daemon 占着 socket 目录。
        let _ = self.run(&["daemon", "stop"]);
        if let Some(pid) = self.daemon_pid() {
            let _ = Command::new("kill").arg(pid.to_string()).status();
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

fn wait_for(mut condition: impl FnMut() -> bool, what: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("timed out waiting for {what}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[test]
fn a_real_daemon_reports_the_barrier_from_disk() {
    let fixture = Fixture::new();
    fixture.start_daemon();

    let (code, output) = fixture.maintenance("example-counter");
    assert_eq!(code, 0, "no transaction means open: {output}");
    assert!(output.contains("state: open"), "got: {output}");
    assert!(
        output.contains("daemon: open"),
        "the daemon must answer too, got: {output}"
    );

    let transaction = fixture.open_transaction();

    let (code, output) = fixture.maintenance("example-counter");
    assert_eq!(code, 1, "an open transaction means the barrier is closed");
    assert!(output.contains("state: closed"), "got: {output}");
    assert!(
        output.contains(&transaction),
        "the answer must name the transaction, got: {output}"
    );
}

#[test]
fn killing_the_managing_side_does_not_open_the_barrier() {
    // 这是全文件最重要的一条。
    //
    // 步骤：开事务 → 确认 barrie 关闭 → 杀掉 daemon（"管理端崩溃"）→ 重新起
    // daemon → 结论必须**仍然**是 closed。
    //
    // 若 daemon 把"我在维护期间还活着/收到过通知"当成前提，重启后它会认为
    // "没人告诉我维护"，于是在回滚进行到一半时拉起旧版本。断言在重启后
    // 仍然 closed，就把这个可能性排除了。
    let fixture = Fixture::new();
    fixture.start_daemon();
    fixture.open_transaction();

    let (code, output) = fixture.maintenance("example-counter");
    assert_eq!(code, 1, "the barrier is closed before the crash: {output}");

    let pid = fixture.daemon_pid().expect("the daemon process is visible");
    let killed = Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "could not kill the daemon");
    assert!(
        wait_for(|| fixture.daemon_pid().is_none(), "the daemon to exit"),
        "the daemon must actually be gone before we restart it"
    );

    fixture.start_daemon();

    let (code, output) = fixture.maintenance("example-counter");
    assert_eq!(
        code, 1,
        "a restarted daemon must still refuse: the barrier lives on disk, not in the \
         process that happened to be watching it. Got: {output}"
    );
    assert!(output.contains("state: closed"), "got: {output}");
    assert!(
        output.contains("daemon: closed"),
        "the new daemon must reach the same conclusion from disk, got: {output}"
    );
}

#[test]
fn a_corrupt_journal_keeps_the_cli_from_reporting_open() {
    // 坏日志 = 不知道。CLI 的退出码必须是非零，否则 `plugins maintenance && ...`
    // 这条链会把"不知道"当成放行。
    let fixture = Fixture::new();

    let path = yi_agent_plugins::JournalStore::at(&fixture.home).path(&fixture.plugin());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ this is not json").unwrap();

    let (code, output) = fixture.maintenance("example-counter");
    assert_eq!(
        code, 1,
        "an unreadable journal must not exit successfully, got: {output}"
    );
    assert!(output.contains("state: unknown"), "got: {output}");
}

#[test]
fn the_daemon_refuses_an_old_maintenance_client() {
    // 直接对 socket 发一条**不带** `protocol` 的请求。旧客户端就是这个形状，
    // 它必须拿到明确拒绝，而不是一个看起来"没在维护"的回答。
    let fixture = Fixture::new();
    fixture.start_daemon();

    let socket = fixture.socket();
    assert!(
        socket.exists(),
        "the daemon must have bound its socket at {}",
        socket.display()
    );

    let request = serde_json::json!({
        "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
        "request_id": "old-client",
        "command": { "type": "PluginMaintenance", "plugin": "example-counter" }
    });
    let response = send_raw(&socket, &request);
    let message = response["result"]["message"]
        .as_str()
        .unwrap_or("<no message>");
    assert_eq!(
        response["result"]["type"], "Error",
        "an old client must get an error, got: {response}"
    );
    assert!(
        message.contains("not supported") && message.contains("upgrade"),
        "the refusal must be actionable, got: {message}"
    );
}

/// 发一帧原始 JSON，返回解析后的响应。
fn send_raw(socket: &Path, request: &serde_json::Value) -> serde_json::Value {
    use std::io::{BufRead, BufReader, Write};
    let mut stream = std::os::unix::net::UnixStream::connect(socket).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .write_all(format!("{request}\n").as_bytes())
        .expect("write");
    stream.flush().unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).expect("read");
    serde_json::from_str(&line).expect("the daemon must answer with JSON")
}
