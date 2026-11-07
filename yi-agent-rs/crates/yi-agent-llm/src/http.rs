//! 进程级共享的 HTTP 连接池。
//!
//! 每个 provider 都各自 `reqwest::Client::builder().build()` 会得到一个独立连接池，
//! 于是**每个新会话的首条消息都要重付一次 TCP+TLS 握手**（本机实测 TCP 0.26s +
//! TLS 0.82s）。这里把 client 按 `(base_url, timeout)` 缓存到进程级，让所有会话
//! 复用同一份连接。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// `api_key` 刻意不在键里：密钥是每个请求的 header，与连接复用无关。
/// `timeout` 在键里：它被烤进 client，且探测（15s）与正式对话（300s）必须各用
/// 各的，绝不能让 15s 的探测缩短正式对话的读超时。
type ClientKey = (String, Duration);

static CLIENTS: OnceLock<Mutex<HashMap<ClientKey, reqwest::Client>>> = OnceLock::new();

fn clients() -> &'static Mutex<HashMap<ClientKey, reqwest::Client>> {
    CLIENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lookup(key: &ClientKey) -> Option<reqwest::Client> {
    clients()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()
}

/// 取（必要时构建）一个共享 client。
///
/// 构建失败**不写入缓存**：真实原因几乎只有 TLS provider 配置这类进程级问题，
/// 下次调用重试构建无害；把失败固化成永久哨兵会让一次瞬时问题变成必须重启才能
/// 恢复的故障。
pub fn shared_client(base_url: &str, timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
    let key = (base_url.to_string(), timeout);

    if let Some(client) = lookup(&key) {
        return Ok(client);
    }

    // 构建放在锁外：`build()` 会初始化 TLS 后端，不该让其它线程排队等它。
    // `build()` 是同步的，此处没有 await 点，不存在「持 std 锁跨 await」的问题。
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .build()?;

    // 二次检查（双检锁）：并发首次调用可能都构建出了 client。若先到者已写入，
    // 必须丢弃自己刚构建的那个并返回先到者——否则两个调用方各自持有独立连接池，
    // 永久失去复用，也永远无法满足「只建立一条连接」这个核心不变量。
    let mut guard = clients().lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(key).or_insert(client);
    Ok(entry.clone())
}

#[cfg(test)]
mod tests {
    use super::shared_client;
    use std::time::Duration;

    #[test]
    fn building_a_client_does_not_panic_for_a_normal_url() {
        // 只验证构建路径本身可用；连接复用由集成测试断言。
        assert!(shared_client("http://127.0.0.1:1", Duration::from_secs(1)).is_ok());
    }
}
