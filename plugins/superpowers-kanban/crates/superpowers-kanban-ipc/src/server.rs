//! 插件侧的查询服务端：宿主问什么，插件答什么。
//!
//! 这一层**只做编解码**：把一行 JSON 解成 `method` + `params`，交给 `Dispatch`，
//! 再把结果或错误写回去。业务逻辑全在 `Dispatch` 的实现里（在 runner 中），
//! 因此那份逻辑可以脱离 socket 单独测试。
//!
//! 协议刻意简单，且与 daemon 侧的手写转发逐字对应：
//! 请求 `{"type":"plugin.query","method":..,"params":..}`，
//! 应答 `{"type":"plugin.result","value":..}` 或 `{"type":"plugin.error","error":".."}`。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::wire::MAX_FRAME_BYTES;

/// 查询 socket 的落点。与 supervisor 清单里声明的 `{state_dir}/superpowers-kanban.sock` 同源。
pub fn socket_path(state_dir: &Path) -> PathBuf {
    state_dir.join("superpowers-kanban.sock")
}

/// 一条查询的处理者。实现者在 runner 里，可脱离 socket 测试。
pub trait Dispatch: Send + Sync {
    fn dispatch(&self, method: &str, params: &Value) -> Result<Value, String>;
}

/// 单条连接的读超时：比 daemon 侧的 5s 转发超时更短，避免占用不必要的线程。
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(4);
/// accept 轮询间隔：让 `keep_going` 能被及时看到。
const ACCEPT_POLL: Duration = Duration::from_millis(200);

/// 处理一条请求行，回一行应答。纯函数，测试不必起 socket。
pub fn respond_line<D: Dispatch>(dispatch: &D, line: &[u8]) -> Vec<u8> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.len() > MAX_FRAME_BYTES {
        return error_frame("query frame is too large");
    }
    let request: Value = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return error_frame(&format!("malformed query frame: {error}")),
    };
    if request.get("type").and_then(Value::as_str) != Some("plugin.query") {
        return error_frame("expected a `plugin.query` frame");
    }
    let Some(method) = request.get("method").and_then(Value::as_str) else {
        return error_frame("query is missing its `method`");
    };
    let params = request.get("params").cloned().unwrap_or(Value::Null);
    match dispatch.dispatch(method, &params) {
        Ok(value) => frame(&json!({ "type": "plugin.result", "value": value })),
        Err(error) => error_frame(&error),
    }
}

fn frame(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(value).unwrap_or_else(|_| {
        br#"{"type":"plugin.error","error":"could not encode reply"}"#.to_vec()
    });
    bytes.push(b'\n');
    bytes
}

fn error_frame(message: &str) -> Vec<u8> {
    frame(&json!({ "type": "plugin.error", "error": message }))
}

/// 处理一条已接受的连接：读一行、回一行。
fn serve_connection<D: Dispatch>(dispatch: &D, stream: UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = Vec::new();
    let read = reader.read_until(b'\n', &mut line)?;
    if read == 0 {
        // 连上就断开：没有请求可答。
        return Ok(());
    }
    let mut stream = stream;
    stream.write_all(&respond_line(dispatch, &line))?;
    stream.flush()
}

/// 绑定并服务，直到 `keep_going` 返回 false。阻塞调用；调用方放到自己的线程里。
pub fn serve_with<D, F>(socket: &Path, dispatch: Arc<D>, keep_going: F) -> std::io::Result<()>
where
    D: Dispatch + 'static,
    F: Fn() -> bool,
{
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 进程被杀后 socket 文件会留在原地；EADDRINUSE 会挡住重启，所以先清掉。
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    listener.set_nonblocking(true)?;
    while keep_going() {
        match listener.accept() {
            Ok((stream, _)) => {
                // 单条连接的失败不得拖垮监听：记下就走，继续服务下一个。
                if let Err(error) = serve_connection(dispatch.as_ref(), stream) {
                    eprintln!("superpowers-kanban: query connection failed: {error}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(ACCEPT_POLL);
            }
            Err(error) => return Err(error),
        }
    }
    let _ = std::fs::remove_file(socket);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 用桩替代真实业务逻辑：这一层只该关心编解码，测试也只需证明这一点。
    struct Fake {
        seen: Mutex<Vec<(String, Value)>>,
    }

    impl Dispatch for Fake {
        fn dispatch(&self, method: &str, params: &Value) -> Result<Value, String> {
            self.seen
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
            match method {
                "list" => Ok(json!({"cards": [{"id": "a"}]})),
                other => Err(format!("unknown method: {other}")),
            }
        }
    }

    fn fake() -> Arc<Fake> {
        Arc::new(Fake {
            seen: Mutex::new(Vec::new()),
        })
    }

    fn round_trip(dispatch: &Fake, request: &str) -> Value {
        let line = format!("{request}\n");
        let reply = respond_line(dispatch, line.as_bytes());
        serde_json::from_slice(&reply).expect("every reply must be one JSON line")
    }

    #[test]
    fn the_socket_sits_where_the_manifest_says() {
        assert_eq!(
            socket_path(Path::new("/proj/.yi-agent/superpowers-kanban")),
            PathBuf::from("/proj/.yi-agent/superpowers-kanban/superpowers-kanban.sock")
        );
    }

    #[test]
    fn a_query_is_forwarded_with_its_params_and_answered_verbatim() {
        let dispatch = fake();
        let reply = round_trip(
            &dispatch,
            r#"{"type":"plugin.query","method":"list","params":{"x":1}}"#,
        );
        assert_eq!(reply["type"], "plugin.result");
        assert_eq!(reply["value"]["cards"][0]["id"], "a");
        let seen = dispatch.seen.lock().unwrap();
        assert_eq!(seen[0].0, "list");
        assert_eq!(seen[0].1, json!({"x": 1}));
    }

    #[test]
    fn a_failed_dispatch_becomes_an_error_frame() {
        let dispatch = fake();
        let reply = round_trip(
            &dispatch,
            r#"{"type":"plugin.query","method":"destroy","params":{}}"#,
        );
        assert_eq!(reply["type"], "plugin.error");
        assert!(
            reply["error"].as_str().unwrap().contains("destroy"),
            "{reply}"
        );
    }

    #[test]
    fn malformed_frames_are_refused_instead_of_panicking() {
        let dispatch = fake();
        for request in ["not json", r#"{"type":"plugin.query"}"#, r#"{"type":"other"}"#] {
            let reply = round_trip(&dispatch, request);
            assert_eq!(reply["type"], "plugin.error", "for {request}");
        }
    }

    #[test]
    fn a_client_that_connects_and_leaves_does_not_stop_serving() {
        let dir = tempfile::tempdir().unwrap();
        let socket = socket_path(dir.path());
        let dispatch = fake();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let path = socket.clone();
        let handle = std::thread::spawn(move || {
            serve_with(&path, dispatch, || !flag.load(std::sync::atomic::Ordering::SeqCst)).unwrap()
        });

        // 等 socket 出现，再连一个空连接（不发任何字节就断开）。
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        UnixStream::connect(&socket).unwrap(); // 立刻 drop

        // 紧接着的正常查询仍必须得到应答。
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .write_all(br#"{"type":"plugin.query","method":"list","params":{}}"#)
            .unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["type"], "plugin.result");

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        handle.join().unwrap();
    }
}
