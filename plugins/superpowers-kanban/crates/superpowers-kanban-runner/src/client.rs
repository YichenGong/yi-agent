use std::path::{Path, PathBuf};

use superpowers_kanban_ipc::client::{self, ClientError};
use superpowers_kanban_ipc::wire::{Command, Reply};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedSession {
    pub session_id: String,
    pub root_task_id: String,
}

#[derive(Debug, Clone)]
pub struct BoardDaemon {
    socket: PathBuf,
}

impl BoardDaemon {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    /// 探测 daemon 是否还在应答，保留**失败的具体原因**。
    ///
    /// 返回值只表达「存活 / 失败」，但失败时调用方必须能打印 errno 级原因：
    /// 之前的 `bool` 把不可达、协议不符、超时压成同一句「daemon is gone」，
    /// 现场无法区分是 daemon 真没了还是插件探错了地址。
    ///
    /// 预算 2 秒：连接、写、读共享这一个截止时间（spec §7 A11）。
    pub fn probe(&self) -> Result<(), ClientError> {
        client::send_status_with_budget(&self.socket, std::time::Duration::from_secs(2))
            .map(|_reply| ())
    }

    /// 探测用的实际地址，供诊断行原样打印。
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Asks the daemon for a new autonomous session in `workdir`.
    pub fn create_session(
        &self,
        objective: &str,
        workdir: &Path,
    ) -> Result<CreatedSession, ClientError> {
        match client::send(
            &self.socket,
            Command::CreateAutonomousSession {
                objective: objective.to_owned(),
                workdir: workdir.to_string_lossy().to_string(),
            },
        )? {
            Reply::AutonomousSessionCreated {
                session_id,
                root_task_id,
            } => Ok(CreatedSession {
                session_id,
                root_task_id,
            }),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the session: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to CreateAutonomousSession: {other:?}"
            ))),
        }
    }

    /// The daemon's state string for `task_id`, or `None` when it is not listed.
    pub fn task_state(&self, task_id: &str) -> Result<Option<String>, ClientError> {
        match client::send(
            &self.socket,
            Command::ListTaskSummaries {
                session_id: None,
                active_only: false,
            },
        )? {
            Reply::TaskSummaries { tasks } => Ok(tasks
                .into_iter()
                .find(|task| task.task_id == task_id)
                .map(|task| task.state)),
            Reply::Error { code, message } => Err(ClientError::Malformed(format!(
                "daemon refused the task listing: {code}: {}",
                message.unwrap_or_default()
            ))),
            other => Err(ClientError::Malformed(format!(
                "unexpected reply to ListTaskSummaries: {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};
    use superpowers_kanban_ipc::wire::{PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope};

    fn short_root(prefix: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(prefix)
            .tempdir_in("/tmp")
            .expect("short temp root")
    }

    /// 业务错误也是合法应答：daemon 答了话就说明它还活着。
    #[test]
    fn probe_preserves_error_and_accepts_business_error() {
        let root = short_root("kr-probe");
        let socket = root.path().join("s.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();

        let server = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        stream
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let mut line = String::new();
                        BufReader::new(stream.try_clone().unwrap())
                            .read_line(&mut line)
                            .unwrap();
                        let request: RequestEnvelope =
                            serde_json::from_str(line.trim()).expect("request is JSON");
                        let reply = ResponseEnvelope {
                            protocol_version: PROTOCOL_VERSION,
                            request_id: request.request_id,
                            event_id: None,
                            event: None,
                            result: superpowers_kanban_ipc::wire::Reply::Error {
                                code: "TestOnly".into(),
                                message: None,
                            },
                        };
                        let mut bytes = serde_json::to_vec(&reply).unwrap();
                        bytes.push(b'\n');
                        stream.write_all(&bytes).unwrap();
                        stream.flush().unwrap();
                        return;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            }
        });

        let daemon = BoardDaemon::new(socket.clone());
        assert_eq!(
            daemon.socket(),
            socket.as_path(),
            "socket() is the probed address"
        );
        daemon
            .probe()
            .expect("a business error still proves the daemon is alive");
        server.join().expect("bounded accept thread");
    }

    #[test]
    fn probe_reports_the_underlying_connection_error() {
        let root = short_root("kr-missing");
        let daemon = BoardDaemon::new(root.path().join("absent.sock"));
        let error = daemon.probe().expect_err("no listener must fail");
        match error {
            ClientError::Io(error) => assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::ConnectionRefused
                        | std::io::ErrorKind::TimedOut
                ),
                "unexpected errno: {error:?}"
            ),
            other => panic!("expected the concrete io error, got {other:?}"),
        }
    }
}
