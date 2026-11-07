//! 记录 TCP 连接数与路径命中数的极简 HTTP/1.1 keep-alive 服务。
//!
//! `reqwest::Client` 不暴露内部连接池，无法比较两个 client 是否同源，所以
//! 「是否复用连接」只能靠服务端的 `accept()` 次数来断言。

// 共享测试辅助：每个测试 crate 只用得到其中一部分，
// 未用到的方法不该让 `clippy -D warnings` 失败。
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// 每个请求返回什么。`Empty` 用于只验证连接复用的测试（不消费流）；
/// `Sse` 用于必须走 `Provider::call_stream` 的测试。
#[derive(Clone)]
pub enum Body {
    Empty,
    Sse(String),
}

#[derive(Clone)]
struct State {
    accepts: usize,
    path_hits: HashMap<String, usize>,
    /// 命中该路径时返回的响应，未配置的路径回 404 空 body
    /// （正是 `looks_like_missing_route` 认的「路由不存在」）。
    routes: HashMap<String, (u16, Body)>,
}

/// 让每个实例的 `uri()` 带一个进程内唯一的路径段（`/t0`、`/t1`…）。
///
/// 共享 client 的缓存键是 `(base_url, timeout)`，而 `TcpListener::bind(0)` 拿到的
/// 端口在实例销毁后可能被后来的测试复用。端口撞上、timeout 又相同时，就会命中
/// 上一个测试留下的连接池。把唯一段放进**路径**里让缓存键天然不同；不用查询串，
/// 因为 `base_url` 会被直接拼成 `format!("{base}{path}")`。
static NEXT_TAG: AtomicUsize = AtomicUsize::new(0);

pub struct ConnCounter {
    uri: String,
    prefix: String,
    state: Arc<Mutex<State>>,
}

impl ConnCounter {
    /// 起服务。`routes` 的键是完整路径（如 `"/chat/completions"`）。
    pub async fn start(routes: Vec<(&str, u16, Body)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tag = NEXT_TAG.fetch_add(1, Ordering::Relaxed);
        let prefix = format!("/t{tag}");

        let state = Arc::new(Mutex::new(State {
            accepts: 0,
            path_hits: HashMap::new(),
            routes: routes
                .into_iter()
                .map(|(p, status, body)| (p.to_string(), (status, body)))
                .collect(),
        }));

        let accept_state = Arc::clone(&state);
        let match_prefix = prefix.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                accept_state.lock().unwrap().accepts += 1;

                let conn_state = Arc::clone(&accept_state);
                let match_prefix = match_prefix.clone();
                tokio::spawn(async move {
                    // 同一连接上按 keep-alive 语义连续处理多个请求。
                    // `carry` 保存上一轮已多读到的字节（下一个请求的开头），
                    // 否则 keep-alive 的第二个请求会被丢弃，表现为客户端超时。
                    let mut carry: Vec<u8> = Vec::new();
                    loop {
                        let Some(head) = read_request(&mut socket, &mut carry).await else {
                            return;
                        };

                        // 请求体必须读掉：socket 若残留未读入站数据就关闭，内核发 RST，
                        // 会把已写出的响应截断（integration.rs:592 记录过这个坑）。
                        // 去掉唯一前缀再匹配，测试里仍按裸路径（`/v1/chat/completions`）
                        // 声明路由，不必关心前缀。
                        let path = head
                            .path
                            .strip_prefix(match_prefix.as_str())
                            .unwrap_or(&head.path)
                            .to_string();
                        let (status, resp_body) = {
                            let mut guard = conn_state.lock().unwrap();
                            *guard.path_hits.entry(path.clone()).or_insert(0) += 1;
                            match guard.routes.get(&path).cloned() {
                                Some(route) => route,
                                // 未配置 = 路由不存在：404 + 空 body。
                                None => (404, Body::Empty),
                            }
                        };

                        let payload = match resp_body {
                            Body::Empty => Vec::new(),
                            Body::Sse(text) => text.into_bytes(),
                        };

                        let reason = match status {
                            200 => "OK",
                            404 => "Not Found",
                            500 => "Internal Server Error",
                            _ => "Unknown",
                        };
                        let head_text = format!(
                            "HTTP/1.1 {status} {reason}\r\n\
                             content-length: {}\r\n\
                             content-type: text/event-stream\r\n\
                             connection: keep-alive\r\n\r\n",
                            payload.len()
                        );
                        if socket.write_all(head_text.as_bytes()).await.is_err() {
                            return;
                        }
                        if socket.write_all(&payload).await.is_err() {
                            return;
                        }
                        if socket.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        Self {
            uri: format!("http://{addr}{prefix}"),
            prefix,
            state,
        }
    }

    pub fn uri(&self) -> String {
        self.uri.clone()
    }

    pub fn accepts(&self) -> usize {
        self.state.lock().unwrap().accepts
    }

    pub fn path_hits(&self, path: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .path_hits
            .get(path)
            .copied()
            .unwrap_or(0)
    }
}

struct RequestHead {
    path: String,
}

/// 读一个完整请求（头 + 按 `content-length` 的 body），返回它的路径。
///
/// `carry` 跨请求保留已多读的字节：`read` 可能一次读到两个请求的边界，
/// 丢掉它们会让 keep-alive 的第二个请求凭空消失。
async fn read_request(
    socket: &mut tokio::net::TcpStream,
    carry: &mut Vec<u8>,
) -> Option<RequestHead> {
    let mut buf = std::mem::take(carry);
    let mut chunk = [0u8; 1024];

    loop {
        let Some(head_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            continue;
        };

        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next()?;
        let mut parts = request_line.split_whitespace();
        let _method = parts.next()?;
        let path = parts.next()?.to_string();

        let body_len = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())?
            })
            .next()
            .unwrap_or(0);

        let body_start = head_end + 4;
        // 补齐本请求剩余的 body 字节（可能已全部读到，也可能还没读全）。
        while buf.len() < body_start + body_len {
            let n = socket.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
        }

        // 把超出本请求的字节留给下一轮（keep-alive 的下一个请求）。
        *carry = buf.split_off(body_start + body_len);
        return Some(RequestHead { path });
    }
}
