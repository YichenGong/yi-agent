//! 服务端 → 客户端的扇出中心。
//!
//! 所有出站帧（通知、反向请求、响应）都经此转发。stdio 传输只注册一个
//! `local` 客户端，语义与改造前的单流写一致；WS 传输注册多个客户端并扇出。

use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

use serde_json::Value;
use tokio::sync::mpsc;

/// 每个客户端的出站队列容量。写满即视为慢消费者,`broadcast` 会摘除它
/// 而不是阻塞其它客户端。
pub const CLIENT_QUEUE: usize = 256;

/// 定向回复失败:客户端已注销或其出站队列已关闭。
#[derive(Debug, PartialEq, Eq)]
pub struct Closed;

/// 一个已连接客户端的身份。
///
/// stdio 传输固定为 `local`(全进程唯一);WS 传输每个连接一个 `ws-<uuid>`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(String);

impl ClientId {
    /// stdio 传输的唯一客户端。
    pub fn local() -> Self {
        Self("local".to_string())
    }

    /// 一个 WS 连接。
    pub fn ws(uuid: uuid::Uuid) -> Self {
        Self(format!("ws-{uuid}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 服务端 → 客户端的扇出中心。
///
/// 锁只在插入/摘除/取 sender 时短暂持有,绝不跨 `.await`,因此 `broadcast`
/// 与 `reply` 可以并发调用。
pub struct Broadcaster {
    clients: StdMutex<HashMap<ClientId, mpsc::Sender<Value>>>,
}

impl Default for Broadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl Broadcaster {
    pub fn new() -> Self {
        Self {
            clients: StdMutex::new(HashMap::new()),
        }
    }

    /// 注册一个客户端,返回它专属的出站接收端。
    pub fn register(&self, id: ClientId) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, tx);
        rx
    }

    /// 摘除一个客户端(断连时)。
    pub fn unregister(&self, id: &ClientId) {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(id);
    }

    /// 当前已注册客户端数(测试与单连接准入用)。
    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// 广播给所有客户端。
    ///
    /// 失效或队列已满(慢消费者)的订阅者会被立即摘除——这是**背压**而非无限
    /// 缓冲:一个连不上的手机不能让主循环卡住。
    pub fn broadcast(&self, frame: Value) {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, tx| tx.try_send(frame.clone()).is_ok());
    }

    /// 只发给指定客户端。
    ///
    /// 与 `broadcast` 不同,这里 `await` 到**入队**成功为止——即等到帧进入该
    /// 客户端的 mpsc 出站 channel,而不是等到对端从 socket 读走;真正的写由该
    /// 客户端的出口泵任务(`pump_stdout` / ws 出口泵)完成。队列有界,故入队
    /// 仍提供背压。客户端已注销或队列关闭时返回 `Err(Closed)`,调用方据此终止
    /// 会话。
    pub async fn reply(&self, id: &ClientId, frame: Value) -> Result<(), Closed> {
        let tx = self
            .clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .cloned();
        match tx {
            Some(tx) => tx.send(frame).await.map_err(|_| Closed),
            None => Err(Closed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn broadcast_reaches_every_registered_client() {
        let hub = Broadcaster::new();
        let mut a = hub.register(ClientId::ws(uuid::Uuid::nil()));
        let mut b = hub.register(ClientId::local());
        hub.broadcast(serde_json::json!({"method": "ping"}));
        assert_eq!(a.recv().await.unwrap()["method"], "ping");
        assert_eq!(b.recv().await.unwrap()["method"], "ping");
    }

    #[tokio::test]
    async fn reply_targets_only_the_addressed_client() {
        let hub = Broadcaster::new();
        let id_a = ClientId::ws(uuid::Uuid::nil());
        let mut a = hub.register(id_a.clone());
        let mut b = hub.register(ClientId::local());
        hub.reply(&id_a, serde_json::json!({"id": 7}))
            .await
            .unwrap();
        assert_eq!(a.recv().await.unwrap()["id"], 7);
        // b 不应收到任何东西。
        assert!(
            tokio::time::timeout(Duration::from_millis(50), b.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn reply_to_an_unknown_client_is_closed() {
        let hub = Broadcaster::new();
        assert!(
            hub.reply(&ClientId::local(), serde_json::json!({}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn unregister_stops_delivery() {
        let hub = Broadcaster::new();
        let id = ClientId::local();
        let mut rx = hub.register(id.clone());
        hub.unregister(&id);
        assert_eq!(hub.client_count(), 0);
        hub.broadcast(serde_json::json!({"method": "ping"}));
        // 注销即把 sender 移出注册表并丢弃,该客户端的出站队列随之关闭：
        // broadcast 无法再投递,recv 立即返回 None 而非阻塞。
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_slow_consumer_is_dropped_without_blocking_others() {
        let hub = Broadcaster::new();
        let slow_id = ClientId::ws(uuid::Uuid::nil());
        let _slow = hub.register(slow_id.clone()); // 从不 recv
        let mut fast = hub.register(ClientId::local());
        // 慢消费者永不 recv,每帧都堆进它的队列;其余消费者必须保持被排空。
        // 灌满并超过慢消费者的队列容量。
        for i in 0..(CLIENT_QUEUE + 8) {
            hub.broadcast(serde_json::json!({ "n": i }));
            // 除最后一帧外,fast 边收边丢,模拟它的写任务持续抽干队列——否则
            // 它自己也会因队列写满而被当作慢消费者摘除。
            if i + 1 < CLIENT_QUEUE + 8 {
                let _ = fast.try_recv();
            }
        }
        // 慢消费者被摘除,快消费者仍注册着。
        assert_eq!(hub.client_count(), 1);
        // 最后一帧已成功入队且发送从未阻塞,fast 取到的就是它。
        assert_eq!(fast.try_recv().unwrap()["n"], CLIENT_QUEUE + 7);
    }
}
