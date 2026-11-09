//! 进程级测试的共享脚手架：短根布局、子进程守卫、Status 桩。
//!
//! 用 `path` 模块引用（`#[path = "support/daemon.rs"] mod support;`），不把
//! 这些辅助暴露成 crate 的公开 API——它们只是测试脚手架，不该进库。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// 短根布局：所有 socket 都在 `/tmp` 下，避免 sun_path 103 字节上限被
/// macOS 的 `/private/var/folders/...` 临时目录直接撑爆。
pub struct TestLayout {
    _root: tempfile::TempDir,
    pub root: PathBuf,
    pub home: PathBuf,
    pub runtime: PathBuf,
    pub project: PathBuf,
    pub state: PathBuf,
}

impl TestLayout {
    pub fn new() -> TestLayout {
        let root = tempfile::Builder::new()
            .prefix("kd")
            .tempdir_in("/tmp")
            .expect("short temp root");
        let base = root.path().canonicalize().expect("canonical root");
        let home = base.join("i");
        let runtime = base.join("r");
        let project = base.join("p");
        // 只预建 home/home.tmp/runtime/project。state 与其配置父目录由需要它的
        // 用例显式创建：缺 home / 超长路径用例必须能在 state 不存在时验证零副作用。
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(home.join("tmp")).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let state = project.join(".yi-agent").join("superpowers-kanban");
        TestLayout {
            _root: root,
            root: base,
            home,
            runtime,
            project,
            state,
        }
    }

    /// 创建 state 目录并打开项目层开关（默认用例需要循环真的在推进）。
    pub fn with_enabled_state(&self) {
        std::fs::create_dir_all(&self.state).unwrap();
        superpowers_kanban_core::switch::write_layer(
            &superpowers_kanban_core::layout::project_preferences_path(&self.state),
            superpowers_kanban_core::switch::SwitchValue::Enabled,
        )
        .unwrap();
    }

    pub fn with_disabled_state(&self) {
        std::fs::create_dir_all(&self.state).unwrap();
        superpowers_kanban_core::switch::write_layer(
            &superpowers_kanban_core::layout::project_preferences_path(&self.state),
            superpowers_kanban_core::switch::SwitchValue::Disabled,
        )
        .unwrap();
    }
}

/// 子进程守卫：日志落 tempfile（不用 pipe，避免读不到 EOF 或阻塞子进程），
/// 无论如何都 kill/wait。
pub struct ChildGuard {
    child: Option<Child>,
    /// 持有 tempfile 句柄，Drop 时才删除日志；内容通过 `path` 读。
    _log: tempfile::NamedTempFile,
    path: PathBuf,
}

impl ChildGuard {
    /// `instance_home = Some`：隔离 HOME + 指定 `YI_AGENT_INSTANCE_HOME`。
    /// `None`：同时不设 INSTANCE / override / HOME，用于缺 home 用例。
    pub fn spawn(
        layout: &TestLayout,
        instance_home: Option<&Path>,
        extra_env: &[(&str, &str)],
    ) -> ChildGuard {
        let log = tempfile::Builder::new()
            .prefix("kd-log")
            .tempfile_in("/tmp")
            .expect("log tempfile");
        let path = log.path().to_path_buf();
        let handle = log.reopen().expect("reopen log");

        let mut command = Command::new(env!("CARGO_BIN_EXE_superpowers-kanban"));
        command
            .args(["run", "--interval-secs", "1"])
            .arg("--runtime-dir")
            .arg(&layout.runtime)
            .arg("--state-dir")
            .arg(&layout.state)
            .arg("--project-root")
            .arg(&layout.project)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(handle));
        if let Some(home) = instance_home {
            command
                .env("HOME", &layout.home)
                .env("YI_AGENT_INSTANCE_HOME", home)
                .env("TMPDIR", layout.home.join("tmp"));
        }
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("spawn runner");
        ChildGuard {
            child: Some(child),
            _log: log,
            path,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("child is alive").id()
    }

    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child
            .as_mut()
            .and_then(|child| child.try_wait().unwrap())
    }

    /// 在 `deadline` 之前等待退出；超时返回 `None`，由调用方判失败。
    pub fn wait_before(&mut self, deadline: Instant) -> Option<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.path).unwrap_or_default()
    }

    /// 等到日志出现 `needle`；超时返回 `false`。
    pub fn wait_for_log(&self, needle: &str, deadline: Instant) -> bool {
        loop {
            if self.log().contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// 桩的应答类型。
#[derive(Clone, Copy, Debug)]
pub enum StubReply {
    /// 合法 Status。
    Status,
    /// 合法业务错误：仍证明 daemon 应答了（单测用不到，留给进程用例）。
    #[allow(dead_code)]
    BusinessError,
    /// 读到请求后直接关闭。
    Close,
    /// 读到请求后保持沉默（不写任何字节），直到客户端自己超时。
    Silent,
}

/// Status 桩：只数**解码成功的 Status 请求**，避免把半截连接算成一次探测。
pub struct StatusStub {
    count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handlers: Vec<std::thread::JoinHandle<()>>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl StatusStub {
    pub fn start(path: &Path, replies: Vec<StubReply>) -> StatusStub {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let listener = UnixListener::bind(path).expect("bind status stub");
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let keep = Arc::clone(&stop);
        let tally = Arc::clone(&count);
        let replies = Arc::new(replies);

        let accept = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            let mut served = 0_usize;
            let deadline = Instant::now() + Duration::from_secs(30);
            while !keep.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reply = if replies.is_empty() {
                            StubReply::Status
                        } else {
                            replies[served.min(replies.len() - 1)]
                        };
                        served += 1;
                        let tally = Arc::clone(&tally);
                        let keep = Arc::clone(&keep);
                        handlers.push(std::thread::spawn(move || {
                            serve_one(stream, reply, &tally, &keep);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
            for handler in handlers {
                handler.join().ok();
            }
        });

        StatusStub {
            count,
            stop,
            handlers: Vec::new(),
            accept: Some(accept),
        }
    }

    pub fn requests(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub fn wait_for_requests(&self, wanted: usize, deadline: Instant) -> bool {
        while Instant::now() < deadline {
            if self.requests() >= wanted {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

impl Drop for StatusStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(accept) = self.accept.take() {
            accept.join().ok();
        }
        for handler in self.handlers.drain(..) {
            handler.join().ok();
        }
    }
}

fn serve_one(mut stream: UnixStream, reply: StubReply, count: &AtomicUsize, stop: &AtomicBool) {
    stream.set_nonblocking(false).ok();
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(2))).ok();

    let mut line = String::new();
    let read = BufReader::new(stream.try_clone().unwrap()).read_line(&mut line);
    if read.is_err() || read.as_ref().map(|n| *n == 0).unwrap_or(true) {
        return;
    }
    // 只有解码出完整 Status 请求才算一次探测；否则半截连接会虚增计数。
    let Ok(envelope) =
        serde_json::from_str::<superpowers_kanban_ipc::wire::RequestEnvelope>(line.trim())
    else {
        return;
    };
    if !matches!(
        envelope.command,
        superpowers_kanban_ipc::wire::Command::Status
    ) {
        return;
    }
    count.fetch_add(1, Ordering::SeqCst);

    match reply {
        StubReply::Close => {}
        StubReply::Silent => {
            // 保持连接打开但一个字都不写：客户端必须靠自己的读预算超时。
            let deadline = Instant::now() + Duration::from_secs(10);
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        StubReply::Status | StubReply::BusinessError => {
            let result = match reply {
                StubReply::Status => superpowers_kanban_ipc::wire::Reply::Status {
                    high_water_event_id: 0,
                },
                _ => superpowers_kanban_ipc::wire::Reply::Error {
                    code: "TestOnly".into(),
                    message: None,
                },
            };
            let response = superpowers_kanban_ipc::wire::ResponseEnvelope {
                protocol_version: superpowers_kanban_ipc::wire::PROTOCOL_VERSION,
                request_id: envelope.request_id,
                event_id: None,
                event: None,
                result,
            };
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            stream.write_all(&bytes).ok();
            stream.flush().ok();
        }
    }
}
