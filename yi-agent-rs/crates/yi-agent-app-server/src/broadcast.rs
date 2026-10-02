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

/// 一个客户端在 hub 里的登记项。
struct Client {
    tx: mpsc::Sender<Value>,
    /// 可靠客户端(仅 stdio 的 `local`):它的出站队列**永不**因背压被摘除。
    /// 见 [`Broadcaster::broadcast`]。
    reliable: bool,
}

/// 服务端 → 客户端的扇出中心。
///
/// 锁只在插入/摘除/取 sender 时短暂持有,绝不跨 `.await`,因此 `broadcast`
/// 与 `reply` 可以并发调用。
pub struct Broadcaster {
    clients: StdMutex<HashMap<ClientId, Client>>,
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
    ///
    /// 普通客户端:队列写满即被判定为慢消费者,`broadcast` 会摘除它(适合可
    /// 容忍断连的 ws 连接)。
    pub fn register(&self, id: ClientId) -> mpsc::Receiver<Value> {
        self.register_inner(id, false)
    }

    /// 注册一个**可靠**客户端(仅 stdio 的 `local` 使用)。
    ///
    /// 与 [`Broadcaster::register`] 唯一的不同在 [`Broadcaster::broadcast`]:它的
    /// 队列写满时**保留登记、丢弃当前帧**,而不是把客户端摘除。stdio 只有这一条
    /// 出站流,把它当慢消费者摘除会让主循环随即写响应失败、整个 sidecar 以错误
    /// 退出——桌面 host 一旦来不及读 stdout(WebKit/Tauri 事件循环繁忙)就会
    /// 触发。可靠客户端的真正断连只由它的出口泵(`pump_stdout`)在**写失败**时
    /// 摘除,即"对端确实没了",而不是"暂时读得慢"。
    pub fn register_reliable(&self, id: ClientId) -> mpsc::Receiver<Value> {
        self.register_inner(id, true)
    }

    fn register_inner(&self, id: ClientId, reliable: bool) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(CLIENT_QUEUE);
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, Client { tx, reliable });
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

    /// 当前已注册的客户端 id 快照。
    pub fn clients(&self) -> Vec<ClientId> {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    /// 该客户端是否仍注册着。
    pub fn is_connected(&self, id: &ClientId) -> bool {
        self.clients
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(id)
    }

    /// 广播给所有客户端。
    ///
    /// 失效或队列已满(慢消费者)的**普通**订阅者会被立即摘除——这是**背压**
    /// 而非无限缓冲:一个连不上的手机不能让主循环卡住。**可靠**客户端(stdio 的
    /// `local`)例外:它队列写满时只丢弃当前帧、保留登记,因为摘除它等于结束
    /// 整个 stdio 会话(见 [`Broadcaster::register_reliable`])。
    pub fn broadcast(&self, frame: Value) {
        let mut guard = self.clients.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|_, client| {
            if client.reliable {
                // 丢弃满队列的当前帧(背压),但保留登记;通道已关闭也保留,
                // 由出口泵在写失败时摘除——那才是对端真正消失。
                let _ = client.tx.try_send(frame.clone());
                true
            } else {
                client.tx.try_send(frame.clone()).is_ok()
            }
        });
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
            .map(|client| client.tx.clone());
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

    /// 回归:可靠客户端(stdio 的 `local`)的队列被灌满时**不得**被摘除。
    ///
    /// 摘除它会让主循环随后写响应得到 `Closed`、整个 sidecar 以错误退出——桌面
    /// host 一旦来不及读 stdout(`try_send` 在满队列上失败)就会触发。可靠客户端
    /// 只会被丢弃当前帧(背压),登记必须保留。
    #[tokio::test]
    async fn a_reliable_client_keeps_its_registration_when_its_queue_is_full() {
        let hub = Broadcaster::new();
        let local = ClientId::local();
        let mut rx = hub.register_reliable(local.clone()); // 从不 recv → 队列必满
        for i in 0..(CLIENT_QUEUE + 8) {
            hub.broadcast(serde_json::json!({ "n": i }));
        }
        assert!(
            hub.is_connected(&local),
            "the stdio local client must survive a full outbound queue"
        );
        // 队列里保留的是最早入队的那批帧(满后新帧被丢弃),而不是被清空。
        assert_eq!(rx.try_recv().unwrap()["n"], 0);
    }

    /// 可靠客户端的**出口泵**若真的停了(接收端被丢弃),`broadcast` 也不摘除它;
    /// 断连只由 [`Broadcaster::unregister`] 显式完成(pump_stdout 写失败时调用)。
    ///
    /// 这条钉死「满队列 ≠ 断连」与「接收端丢失 ≠ 断连」两个判据分离,避免再次
    /// 把「读得慢」误判成「对端没了」。
    #[tokio::test]
    async fn a_reliable_client_is_only_unregistered_explicitly() {
        let hub = Broadcaster::new();
        let local = ClientId::local();
        let rx = hub.register_reliable(local.clone());
        drop(rx); // 出口泵停了:接收端不复存在
        hub.broadcast(serde_json::json!({"method": "ping"}));
        assert!(
            hub.is_connected(&local),
            "a dropped receiver must not silently drop the local registration"
        );
        hub.unregister(&local);
        assert!(!hub.is_connected(&local));
    }

    #[tokio::test]
    async fn clients_lists_registered_ids() {
        let hub = Broadcaster::new();
        let id = ClientId::ws(uuid::Uuid::nil());
        let _a = hub.register(id.clone());
        let _b = hub.register(ClientId::local());
        let mut ids = hub.clients();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        assert_eq!(ids.len(), 2);
        assert!(hub.is_connected(&id));
        assert!(hub.is_connected(&ClientId::local()));
    }

    #[tokio::test]
    async fn is_connected_is_false_after_unregister() {
        let hub = Broadcaster::new();
        let id = ClientId::local();
        let _rx = hub.register(id.clone());
        hub.unregister(&id);
        assert!(!hub.is_connected(&id));
    }
}
